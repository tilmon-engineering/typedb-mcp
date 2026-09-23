use std::{path::Path, sync::Arc, time::Duration};
use typedb_mcp_core::{
    coordinator::OperationCoordinator,
    migration::{MigrationBackend, MigrationSupervisor, ShutdownResult},
};
struct Blocked;
impl MigrationBackend for Blocked {
    fn export_database(&self, _: &str, _: &Path, _: &Path) -> Result<(), String> {
        std::thread::park();
        Ok(())
    }
    fn import_database(&self, _: &str, _: &Path, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn database_exists(&self, _: &str) -> Result<bool, String> {
        Ok(false)
    }
}
fn main() {
    let supervisor = MigrationSupervisor::new(Arc::new(Blocked), OperationCoordinator::new());
    let dir = std::env::temp_dir().join(format!("shutdown-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let store = typedb_mcp_core::session::SessionStore::new();
    let guard = rt.block_on(async {
        let id = store.start(Duration::from_secs(60)).await;
        store
            .resolve_and_touch(&id, Duration::from_secs(60))
            .await
            .unwrap()
            .lock_owned()
            .await
    });
    let _rx = rt
        .block_on(supervisor.submit(
            typedb_mcp_core::MigrationKind::Export,
            "probe_db".into(),
            dir.join("schema"),
            dir.join("data"),
            guard,
        ))
        .unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let result = supervisor.shutdown(Duration::from_secs(
        std::env::var("TYPEDB_MCP_SHUTDOWN_GRACE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
    ));
    if result == ShutdownResult::StillRunning {
        eprintln!("typedb-mcp: shutdown grace expired; migration state uncertain; forcing close");
        std::process::exit(1);
    }
}
