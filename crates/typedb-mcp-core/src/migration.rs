//! Safe, stdio-authorized database migration kernel.
use crate::{
    coordinator::{NameReservation, OperationCoordinator, ReservationError},
    session::SessionState,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{OwnedMutexGuard, Semaphore, oneshot};

pub const DEFAULT_SCHEMA_CAP: u64 = 16 * 1024 * 1024;
static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationKind {
    Export,
    Import,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStatus {
    Complete,
    UnknownOrPartial,
}
#[derive(Debug, Clone)]
pub struct Manifest {
    pub schema_size: u64,
    pub data_size: u64,
    pub schema_sha256: String,
    pub data_sha256: String,
}
#[derive(Debug)]
pub struct MigrationReport {
    pub kind: MigrationKind,
    pub manifest: Manifest,
    pub status: ImportStatus,
    pub published: Vec<PathBuf>,
}
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("migration busy: {0}")]
    Busy(String),
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
    #[error("migration backend error: {0}")]
    Backend(String),
    #[error("migration supervisor is shut down")]
    Shutdown,
}
impl From<io::Error> for MigrationError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

pub trait MigrationBackend: Send + Sync + 'static {
    fn export_database(&self, database: &str, schema: &Path, data: &Path) -> Result<(), String>;
    fn import_database(&self, database: &str, schema: &Path, data: &Path) -> Result<(), String>;
    fn database_exists(&self, database: &str) -> Result<bool, String>;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownResult {
    Completed,
    StillRunning,
}

#[derive(Clone)]
pub struct MigrationSupervisor {
    tx: std::sync::mpsc::Sender<Job>,
    admission: Arc<Semaphore>,
    accepting: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
    coordinator: OperationCoordinator,
    settings: MigrationSettings,
}
/// Builds the production backend ON the dedicated worker thread. This keeps
/// runtime/driver construction out of any Tokio runtime context (TypeDB's
/// gRPC channels are runtime-affine, so the backend must never wrap the
/// shared client driver).
pub type BackendFactory = Box<dyn FnOnce() -> Result<Arc<dyn MigrationBackend>, String> + Send>;
#[derive(Debug, Clone, Copy)]
pub struct MigrationSettings {
    pub schema_size_cap_bytes: u64,
    pub min_free_bytes: u64,
}

impl Default for MigrationSettings {
    fn default() -> Self {
        Self {
            schema_size_cap_bytes: DEFAULT_SCHEMA_CAP,
            min_free_bytes: 0,
        }
    }
}

struct Job {
    kind: MigrationKind,
    database: String,
    schema: PathBuf,
    data: PathBuf,
    _permit: tokio::sync::OwnedSemaphorePermit,
    _reservation: NameReservation,
    _session: OwnedMutexGuard<SessionState>,
    reply: oneshot::Sender<Result<MigrationReport, MigrationError>>,
    settings: MigrationSettings,
}
impl MigrationSupervisor {
    /// Test convenience: supervisor with an already-constructed backend.
    /// Production must use [`MigrationSupervisor::with_factory`], which builds
    /// the backend ON the worker thread so its runtime/driver pair is never
    /// created inside a Tokio runtime context.
    pub fn new(backend: Arc<dyn MigrationBackend>, coordinator: OperationCoordinator) -> Self {
        Self::with_factory_and_settings(
            Box::new(move || Ok(backend)),
            coordinator,
            MigrationSettings::default(),
        )
    }
    pub fn with_factory(factory: BackendFactory, coordinator: OperationCoordinator) -> Self {
        Self::with_factory_and_settings(factory, coordinator, MigrationSettings::default())
    }
    pub fn with_factory_and_settings(
        factory: BackendFactory,
        coordinator: OperationCoordinator,
        settings: MigrationSettings,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let admission = Arc::new(Semaphore::new(1));
        let accepting = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_completed = completed.clone();
        thread::Builder::new()
            .name("typedb-migration".into())
            .spawn(move || {
                // Factory errors must not kill the worker: it stays alive and
                // answers every job with a lifecycle-honest backend error.
                let (backend, factory_error) = match factory() {
                    Ok(b) => (Some(b), None),
                    Err(e) => (None, Some(e)),
                };
                // The worker loop is deliberately FULLY SYNCHRONOUS: the
                // backend owns the only Tokio runtime here, and calling
                // Runtime::block_on from inside another runtime's async
                // context panics ("Cannot start a runtime from within a
                // runtime", observed live 2026-09-10). Plain-thread blocking
                // is exactly the dedicated-worker design contract.
                while !worker_stop.load(Ordering::Acquire) {
                    match rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(mut j) => {
                            let result = match (backend.as_deref(), factory_error.as_ref()) {
                                (Some(b), _) => run_job(&mut j, b),
                                (None, Some(e)) => Err(MigrationError::Backend(e.clone())),
                                (None, None) => {
                                    unreachable!("factory produced neither backend nor error")
                                }
                            };
                            let _ = j.reply.send(result);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                worker_completed.store(true, Ordering::Release);
                drop(backend); // backend runtime drops on this plain thread
            })
            .expect("migration worker");
        Self {
            tx,
            admission,
            accepting,
            stop,
            completed,
            coordinator,
            settings,
        }
    }
    pub fn stop_admission(&self) {
        self.accepting.store(false, Ordering::Release);
    }
    pub fn shutdown(&self, grace: Duration) -> ShutdownResult {
        self.stop_admission();
        self.stop.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + grace;
        while !self.completed.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                return ShutdownResult::StillRunning;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        ShutdownResult::Completed
    }
    pub async fn submit(
        &self,
        kind: MigrationKind,
        database: String,
        schema: PathBuf,
        data: PathBuf,
        session: OwnedMutexGuard<SessionState>,
    ) -> Result<oneshot::Receiver<Result<MigrationReport, MigrationError>>, MigrationError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(MigrationError::Busy(
                "migration shutdown has started".into(),
            ));
        }
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| MigrationError::Busy("migration already running".into()))?;
        let reservation = self
            .coordinator
            .try_reserve(&database)
            .map_err(|e| match e {
                ReservationError::Busy(n) => MigrationError::Busy(n),
                ReservationError::InvalidName => {
                    MigrationError::InvalidPath("invalid database name".into())
                }
            })?;
        let (schema, data) = if kind == MigrationKind::Export {
            (
                normalize_destination(&schema)?,
                normalize_destination(&data)?,
            )
        } else {
            (schema, data)
        };
        let (reply, out) = oneshot::channel();
        let job = Job {
            kind,
            database,
            schema,
            data,
            _permit: permit,
            _reservation: reservation,
            _session: session,
            reply,
            settings: self.settings,
        };
        // Unbounded channel; capacity-1 admission is enforced by the
        // semaphore above, so send cannot meaningfully block on backlog.
        self.tx
            .send(job)
            .map_err(|_| MigrationError::Busy("migration worker is gone".into()))?;
        Ok(out)
    }
}
fn run_job(j: &mut Job, backend: &dyn MigrationBackend) -> Result<MigrationReport, MigrationError> {
    match j.kind {
        MigrationKind::Export => {
            validate_export_inputs(&j.schema, &j.data)?;
            export(j, backend)
        }
        MigrationKind::Import => {
            validate_pair(&j.schema, &j.data, j.settings.schema_size_cap_bytes)?;
            import(j, backend)
        }
    }
}
fn export(j: &Job, backend: &dyn MigrationBackend) -> Result<MigrationReport, MigrationError> {
    ensure_filesystem_headroom(
        j.schema.parent().unwrap_or(Path::new("/")),
        j.settings.min_free_bytes,
    )?;
    ensure_filesystem_headroom(
        j.data.parent().unwrap_or(Path::new("/")),
        j.settings.min_free_bytes,
    )?;
    // Export staging reserves paths only; the driver creates its files
    // exclusively, so pre-created placeholders would abort the export.
    let s = stage_path_only(&j.schema)?;
    let d = stage_path_only(&j.data)?;
    let mut owned = StagingGuard::new(vec![s.clone(), d.clone()]);
    if let Err(e) = backend.export_database(&j.database, &s, &d) {
        cleanup(&[&s, &d]);
        return Err(MigrationError::Backend(e));
    }
    let sm = hash_file(&s)?;
    let dm = hash_file(&d)?;
    publish_noclobber(&s, &j.schema)?;
    if let Err(e) = publish_noclobber(&d, &j.data) {
        return Err(MigrationError::Io(format!(
            "second publication failed; surviving published paths: {}; {}",
            j.schema.display(),
            e
        )));
    }
    owned.disarm();
    Ok(MigrationReport {
        kind: MigrationKind::Export,
        manifest: Manifest {
            schema_size: sm.0,
            data_size: dm.0,
            schema_sha256: sm.1,
            data_sha256: dm.1,
        },
        status: ImportStatus::Complete,
        published: vec![j.schema.clone(), j.data.clone()],
    })
}
fn import(j: &Job, backend: &dyn MigrationBackend) -> Result<MigrationReport, MigrationError> {
    let s = stage(&j.schema)?;
    let d = stage(&j.data)?;
    ensure_filesystem_headroom(
        j.schema.parent().unwrap_or(Path::new("/")),
        j.settings.min_free_bytes,
    )?;
    ensure_filesystem_headroom(
        j.data.parent().unwrap_or(Path::new("/")),
        j.settings.min_free_bytes,
    )?;
    let mut owned = StagingGuard::new(vec![s.clone(), d.clone()]);
    fs::remove_file(&s)?;
    fs::remove_file(&d)?;
    fs::copy(&j.schema, &s)?;
    fs::copy(&j.data, &d)?;
    validate_schema(&s, j.settings.schema_size_cap_bytes)?;
    validate_frames(&d)?;
    let sm = hash_file(&s)?;
    let dm = hash_file(&d)?;
    if backend
        .database_exists(&j.database)
        .map_err(MigrationError::Backend)?
    {
        return Err(MigrationError::Busy(
            "target database exists; target status: exists".into(),
        ));
    }
    let result = backend.import_database(&j.database, &s, &d);
    match result {
        Ok(()) => {
            owned.disarm();
            Ok(MigrationReport {
                kind: MigrationKind::Import,
                manifest: Manifest {
                    schema_size: sm.0,
                    data_size: dm.0,
                    schema_sha256: sm.1,
                    data_sha256: dm.1,
                },
                status: ImportStatus::Complete,
                published: vec![],
            })
        }
        Err(e) => {
            let status = backend
                .database_exists(&j.database)
                .map(|exists| if exists { "exists" } else { "absent" })
                .unwrap_or("unknown");
            Err(MigrationError::Backend(format!(
                "import failed; target status: {status}; {e}"
            )))
        }
    }
}
fn ensure_free_space(available: Option<u64>, required: u64) -> Result<(), MigrationError> {
    match available {
        Some(bytes) if bytes >= required => Ok(()),
        Some(bytes) => Err(MigrationError::InvalidPath(format!(
            "filesystem headroom insufficient (available {bytes} bytes, required reserve {required} bytes; headroom-only check, not proof)"
        ))),
        None => Ok(()),
    }
}
fn filesystem_available(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        let mut s = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::statvfs(c.as_ptr(), s.as_mut_ptr()) };
        if rc == 0 {
            let s = unsafe { s.assume_init() };
            return (s.f_bavail as u128)
                .checked_mul(s.f_frsize as u128)
                .and_then(|n| u64::try_from(n).ok());
        }
        None
    }
    #[cfg(not(unix))]
    {
        None
    }
}
fn ensure_filesystem_headroom(path: &Path, required: u64) -> Result<(), MigrationError> {
    ensure_free_space(filesystem_available(path), required).map_err(|e| match e {
        MigrationError::InvalidPath(m) => {
            MigrationError::InvalidPath(format!("{}: {m}", path.display()))
        }
        other => other,
    })
}
fn validate_pair(s: &Path, d: &Path, cap: u64) -> Result<(), MigrationError> {
    let a = validate_schema(s, cap)?;
    let b = validate_input(d)?;
    if a == b {
        Err(MigrationError::InvalidPath(
            "schema and data paths alias".into(),
        ))
    } else {
        Ok(())
    }
}
fn validate_export_inputs(s: &Path, d: &Path) -> Result<(), MigrationError> {
    let s = normalize_destination(s)?;
    let d = normalize_destination(d)?;
    for p in [&s, &d] {
        if !p.is_absolute() || p.as_os_str().is_empty() {
            return Err(MigrationError::InvalidPath(
                "destination must be absolute and nonempty".into(),
            ));
        }
        if p.to_string_lossy().contains('~')
            || p.to_string_lossy().contains("${")
            || p.to_string_lossy().starts_with("file:")
        {
            return Err(MigrationError::InvalidPath(
                "path expansion/URL rejected".into(),
            ));
        }
        if p.exists() || fs::symlink_metadata(p).is_ok() {
            return Err(MigrationError::InvalidPath(
                "export destination already exists".into(),
            ));
        }
        if !fs::metadata(
            p.parent()
                .ok_or_else(|| MigrationError::InvalidPath("missing destination parent".into()))?,
        )?
        .is_dir()
        {
            return Err(MigrationError::InvalidPath(
                "destination parent is not a directory".into(),
            ));
        }
    }
    if s == d {
        Err(MigrationError::InvalidPath(
            "schema and data paths alias".into(),
        ))
    } else {
        Ok(())
    }
}
fn normalize_destination(p: &Path) -> Result<PathBuf, MigrationError> {
    if !p.is_absolute() || p.as_os_str().is_empty() {
        return Err(MigrationError::InvalidPath(
            "destination must be absolute and nonempty".into(),
        ));
    }
    let parent = p
        .parent()
        .ok_or_else(|| MigrationError::InvalidPath("missing destination parent".into()))?;
    let parent = fs::canonicalize(parent)
        .map_err(|e| MigrationError::Io(format!("destination parent: {e}")))?;
    let name = p
        .file_name()
        .ok_or_else(|| MigrationError::InvalidPath("missing destination filename".into()))?;
    Ok(parent.join(name))
}
fn validate_schema(p: &Path, cap: u64) -> Result<PathBuf, MigrationError> {
    let q = validate_input(p)?;
    if fs::metadata(&q)?.len() > cap {
        return Err(MigrationError::InvalidPath(
            "schema exceeds size cap".into(),
        ));
    }
    let bytes = fs::read(&q)?;
    std::str::from_utf8(&bytes)
        .map_err(|_| MigrationError::InvalidPath("schema is not UTF-8".into()))?;
    Ok(q)
}
fn validate_input(p: &Path) -> Result<PathBuf, MigrationError> {
    if !p.is_absolute() || p.as_os_str().is_empty() {
        return Err(MigrationError::InvalidPath(
            "path must be absolute and nonempty".into(),
        ));
    }
    if p.to_string_lossy().contains('~')
        || p.to_string_lossy().contains("${")
        || p.to_string_lossy().starts_with("file:")
    {
        return Err(MigrationError::InvalidPath(
            "path expansion/URL rejected".into(),
        ));
    }
    let q = fs::canonicalize(p)?;
    if !fs::metadata(&q)?.is_file() {
        return Err(MigrationError::InvalidPath(
            "path is not a regular file".into(),
        ));
    }
    Ok(q)
}
fn validate_frames(p: &Path) -> Result<(), MigrationError> {
    let mut f = File::open(p)?;
    let n = f.metadata()?.len();
    let mut used = 0;
    while used < n {
        let mut shift = 0;
        let mut len = 0;
        loop {
            if used >= n {
                return Err(MigrationError::InvalidFrame("truncated varint".into()));
            }
            let mut b = [0];
            f.read_exact(&mut b)?;
            used += 1;
            if shift >= 64 || (shift == 63 && b[0] > 1) {
                return Err(MigrationError::InvalidFrame("overflowing varint".into()));
            }
            len |= ((b[0] & 127) as u64) << shift;
            if b[0] & 128 == 0 {
                break;
            }
            shift += 7
        }
        if len > n - used {
            return Err(MigrationError::InvalidFrame(
                "payload exceeds remaining bytes".into(),
            ));
        }
        io::copy(&mut f.by_ref().take(len), &mut io::sink())?;
        used += len
    }
    Ok(())
}
fn stage(p: &Path) -> Result<PathBuf, MigrationError> {
    let parent = p
        .parent()
        .ok_or_else(|| MigrationError::InvalidPath("missing parent".into()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| MigrationError::Io(e.to_string()))?
        .as_nanos();
    let id = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let q = parent.join(format!(
        ".typedb-mcp-stage-{}-{nanos}-{id}",
        std::process::id()
    ));
    OpenOptions::new().write(true).create_new(true).open(&q)?;
    Ok(q)
}
/// Export staging: reserve a unique path on the destination filesystem but
/// do NOT create the file — the driver's export creates its target files
/// exclusively (try_create_export_file fails on "File exists", observed live
/// 2026-09-10). Only the parent directory is validated/created here.
fn stage_path_only(p: &Path) -> Result<PathBuf, MigrationError> {
    let parent = p
        .parent()
        .ok_or_else(|| MigrationError::InvalidPath("missing parent".into()))?;
    fs::create_dir_all(parent)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| MigrationError::Io(e.to_string()))?
        .as_nanos();
    let id = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".typedb-mcp-stage-{}-{nanos}-{id}",
        std::process::id()
    )))
}
fn publish_noclobber(from: &Path, to: &Path) -> Result<(), MigrationError> {
    let mut out = OpenOptions::new().write(true).create_new(true).open(to)?;
    let mut input = File::open(from)?;
    if let Err(e) = io::copy(&mut input, &mut out).and_then(|_| out.sync_all()) {
        let cleanup = fs::remove_file(to).err();
        return Err(MigrationError::Io(match cleanup {
            None => format!(
                "publication failed for {}; partial destination removed: {e}",
                to.display()
            ),
            Some(remove) => format!(
                "publication failed for {}; surviving partial destination {} (removal failed: {remove})",
                to.display(),
                to.display()
            ),
        }));
    }
    fs::remove_file(from)?;
    Ok(())
}
struct StagingGuard {
    paths: Vec<PathBuf>,
    armed: bool,
}
impl StagingGuard {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.armed {
            cleanup(&self.paths.iter().map(PathBuf::as_path).collect::<Vec<_>>());
        }
    }
}
fn cleanup(ps: &[&Path]) {
    for p in ps {
        let _ = fs::remove_file(p);
    }
}
fn hash_file(p: &Path) -> Result<(u64, String), MigrationError> {
    let mut f = File::open(p)?;
    let mut h = Sha256::new();
    let mut b = [0u8; 65536];
    let mut n = 0;
    loop {
        let k = f.read(&mut b)?;
        if k == 0 {
            break;
        }
        n += k as u64;
        h.update(&b[..k])
    }
    let digest = h.finalize();
    Ok((n, digest.iter().map(|x| format!("{x:02x}")).collect()))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{coordinator::OperationCoordinator, session::SessionStore};
    use std::sync::{Barrier, atomic::AtomicBool};

    struct BlockingBackend {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
        completed: Arc<AtomicBool>,
    }
    impl MigrationBackend for BlockingBackend {
        fn export_database(&self, _: &str, schema: &Path, data: &Path) -> Result<(), String> {
            fs::write(schema, b"define entity thing;").unwrap();
            fs::write(data, [0u8]).unwrap();
            self.entered.wait();
            self.release.wait();
            self.completed.store(true, Ordering::Release);
            Ok(())
        }
        fn import_database(&self, _: &str, _: &Path, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn database_exists(&self, _: &str) -> Result<bool, String> {
            Ok(false)
        }
    }
    fn owned_session() -> (
        Arc<tokio::sync::Mutex<SessionState>>,
        tokio::sync::OwnedMutexGuard<SessionState>,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = SessionStore::new();
            let id = store.start(Duration::from_secs(60)).await;
            let state = store
                .resolve_and_touch(&id, Duration::from_secs(60))
                .await
                .unwrap();
            let guard = state.clone().lock_owned().await;
            (state, guard)
        })
    }
    fn submit_blocked(
        supervisor: &MigrationSupervisor,
        name: &str,
    ) -> (
        Arc<tokio::sync::Mutex<SessionState>>,
        tokio::sync::oneshot::Receiver<Result<MigrationReport, MigrationError>>,
    ) {
        let (state, guard) = owned_session();
        let dir = std::env::temp_dir().join(format!("migration-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rx = rt
            .block_on(supervisor.submit(
                MigrationKind::Export,
                name.into(),
                dir.join("schema"),
                dir.join("data"),
                guard,
            ))
            .unwrap();
        (state, rx)
    }
    #[test]
    fn blocked_worker_retains_all_ownership_after_receiver_drop() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let completed = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(BlockingBackend {
            entered: entered.clone(),
            release: release.clone(),
            completed: completed.clone(),
        });
        let coordinator = OperationCoordinator::new();
        let supervisor = MigrationSupervisor::new(backend.clone(), coordinator.clone());
        let (state, rx) = submit_blocked(&supervisor, "retain");
        entered.wait();
        assert!(coordinator.try_reserve("retain").is_err());
        assert!(state.try_lock().is_err());
        let (_, second_guard) = owned_session();
        let second = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(supervisor.submit(
                MigrationKind::Export,
                "other".into(),
                "/tmp/other-s".into(),
                "/tmp/other-d".into(),
                second_guard,
            ));
        assert!(matches!(second, Err(MigrationError::Busy(_))));
        drop(rx);
        release.wait();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !completed.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(completed.load(Ordering::Acquire));
        assert_eq!(
            supervisor.shutdown(Duration::from_secs(2)),
            ShutdownResult::Completed
        );
        assert!(!coordinator.is_reserved("retain"));
        assert!(coordinator.try_reserve("retain").is_ok());
        assert!(state.try_lock().is_ok());
    }
    struct TestBackend;
    impl MigrationBackend for TestBackend {
        fn export_database(&self, _: &str, _: &Path, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn import_database(&self, _: &str, _: &Path, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn database_exists(&self, _: &str) -> Result<bool, String> {
            Ok(false)
        }
    }
    #[test]
    fn supervisor_stop_admission_and_idle_shutdown() {
        let s = MigrationSupervisor::new(Arc::new(TestBackend), OperationCoordinator::new());
        s.stop_admission();
        let result = tokio::runtime::Runtime::new().unwrap().block_on(s.submit(
            MigrationKind::Export,
            "db".into(),
            "/tmp/a".into(),
            "/tmp/b".into(),
            panic_guard(),
        ));
        assert!(matches!(result, Err(MigrationError::Busy(_))));
        assert_eq!(
            s.shutdown(Duration::from_secs(1)),
            ShutdownResult::Completed
        );
    }
    fn panic_guard() -> tokio::sync::OwnedMutexGuard<SessionState> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = crate::session::SessionStore::new();
            let id = store.start(Duration::from_secs(60)).await;
            store
                .resolve_and_touch(&id, Duration::from_secs(60))
                .await
                .unwrap()
                .lock_owned()
                .await
        })
    }

    #[test]
    fn frame_rejects_truncation() {
        let p = std::env::temp_dir().join(format!("mig-{}", std::process::id()));
        fs::write(&p, [0x80]).unwrap();
        assert!(matches!(
            validate_frames(&p),
            Err(MigrationError::InvalidFrame(_))
        ));
        let _ = fs::remove_file(p);
    }
    #[test]
    fn free_space_seam() {
        assert!(ensure_free_space(None, u64::MAX).is_ok());
        assert!(ensure_free_space(Some(10), 10).is_ok());
        let err = ensure_free_space(Some(9), 10).unwrap_err().to_string();
        assert!(err.contains("required reserve 10") && err.contains("headroom-only"));
    }
    #[test]
    fn sha_known() {
        let p = std::env::temp_dir().join(format!("mig-h-{}", std::process::id()));

        fs::write(&p, b"abc").unwrap();
        assert_eq!(
            hash_file(&p).unwrap().1,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = fs::remove_file(p);
    }
}
