//! The library kernel. See DESIGN.md §11.
//!
//! [`TypeDbCore`] is a cheaply-cloneable `Arc` bundle of the TypeDB
//! driver client, the [`SessionStore`], and the operator config. One per
//! process. Construct it with [`TypeDbCore::connect`] and stash an
//! `Arc<TypeDbCore>` on whatever handler struct your MCP server uses.
//!
//! [`SessionHandle`] is the result of [`TypeDbCore::resolve`]. It carries
//! the resolved [`SessionId`] plus the per-session `Arc<Mutex<SessionState>>`.
//! All transaction work and envelope emission happens through methods on
//! this handle so the per-session lock window is correct every time.
//!
//! [`HasTypeDbCore`] is the trait the generic raw tool router uses to
//! reach the kernel from a consumer's handler struct.

use std::{sync::Arc, time::Instant};

use rmcp::model::CallToolResult;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::{
    config::Config,
    envelope::{
        NextMoves, envelope_err, envelope_ok, envelope_state_error,
        envelope_state_error_no_session, explain_query_error, next_moves,
    },
    error::{ErrorClass, InternalError},
    extensions::Extensions,
    session::{
        OpenTx, SessionId, SessionResolveError, SessionSnapshot, SessionState, SessionStore,
    },
    typedb::{DriverTransaction, TxKind, TypeDbClient, query_answer_to_json},
};

/// Trait by which the generic raw tool router (and consumer tools)
/// reach the kernel from whatever handler struct they live on.
///
/// Implementing this on a `Clone + Send + Sync + 'static` struct that
/// holds an `Arc<TypeDbCore>` is the entire integration story for
/// library consumers.
pub trait HasTypeDbCore: Clone + Send + Sync + 'static {
    fn typedb_core(&self) -> &Arc<TypeDbCore>;
}

/// Process-global kernel. Cheaply cloneable through `Arc`.
#[derive(Debug)]
pub struct TypeDbCore {
    pub config: Arc<Config>,
    pub typedb: Arc<TypeDbClient>,
    pub sessions: Arc<SessionStore>,
}

impl TypeDbCore {
    /// Construct a kernel from an already-built TypeDB client, session
    /// store, and config. The binary uses this directly; consumers who
    /// want a fully managed connection should use [`TypeDbCore::connect`].
    pub fn new(
        config: Arc<Config>,
        typedb: Arc<TypeDbClient>,
        sessions: Arc<SessionStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            typedb,
            sessions,
        })
    }

    /// Connect to TypeDB using the credentials in `config`, allocate a
    /// fresh in-memory `SessionStore`, and return a ready-to-use kernel.
    pub async fn connect(config: Arc<Config>) -> Result<Arc<Self>, InternalError> {
        let (user, pass) = config
            .typedb_credentials()
            .map_err(|e| InternalError::Config(format!("typedb credentials: {e}")))?;
        let typedb = Arc::new(
            TypeDbClient::connect(
                &config.typedb.address,
                &user,
                &pass,
                config.typedb.tls_enabled,
            )
            .await?,
        );
        Ok(Self::new(config, typedb, SessionStore::new()))
    }

    /// Spawn the background reaper task. Returns the JoinHandle.
    /// Per-kind idle timeouts and TTL are read from the kernel's
    /// [`Config`]. The binary spawns this; library consumers who run
    /// their own runtime can call this once at startup.
    pub fn spawn_reaper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let sessions = self.sessions.clone();
        let read_idle = self.config.idle_timeout_for(TxKind::Read);
        let write_idle = self.config.idle_timeout_for(TxKind::Write);
        let schema_idle = self.config.idle_timeout_for(TxKind::Schema);
        let tick = self.config.min_idle_timeout();
        let session_ttl = self.config.session_ttl();
        tokio::spawn(async move {
            crate::session::run_reaper(
                sessions,
                read_idle,
                write_idle,
                schema_idle,
                tick,
                session_ttl,
            )
            .await
        })
    }

    /// Mint a new session and return its ID. Equivalent to the
    /// agent-facing `start_session` tool's first step (the tool also
    /// fetches the database list).
    pub async fn start_session(&self) -> SessionId {
        self.sessions.start(self.config.session_ttl()).await
    }

    /// Resolve an agent-supplied `session_id` against the store.
    ///
    /// On success returns a [`SessionHandle`] for all further work.
    /// On miss/expiry returns a ready-to-return [`CallToolResult`]
    /// carrying the canonical `SESSION_UNKNOWN` / `SESSION_EXPIRED`
    /// envelope, so a consumer's tool body is a one-liner:
    ///
    /// ```ignore
    /// let session = match self.core.resolve(&p.session_id).await {
    ///     Ok(s)   => s,
    ///     Err(env) => return Ok(env),
    /// };
    /// ```
    pub async fn resolve<'a>(
        &'a self,
        session_id: &str,
    ) -> Result<SessionHandle<'a>, CallToolResult> {
        let sid = SessionId(session_id.to_owned());
        let ttl = self.config.session_ttl();
        match self.sessions.resolve_and_touch(&sid, ttl).await {
            Ok(arc) => Ok(SessionHandle {
                core: self,
                id: sid,
                arc,
            }),
            Err(SessionResolveError::Unknown) => Err(envelope_state_error_no_session(
                ErrorClass::SessionUnknown,
                "Unknown `session_id`. The server has no session with this ID. \
                 Either it never existed, it was minted in a prior process \
                 lifetime, or it has already been purged. Call `start_session` \
                 to get a fresh ID, then reissue.",
                next_moves::on_session_unknown(),
            )),
            Err(SessionResolveError::Expired) => Err(envelope_state_error_no_session(
                ErrorClass::SessionExpired,
                "Your `session_id` was valid but its TTL has passed and the \
                 server has purged it (rolling back any open transaction). \
                 Call `start_session` for a fresh ID and reissue.",
                next_moves::on_session_expired(),
            )),
        }
    }
}

/// Outcome of a write/schema tx closure: tells the kernel whether to
/// commit or release. The kernel takes responsibility for the wire-op
/// choice (per DESIGN.md §5.0.1) and for envelope construction.
#[derive(Debug)]
pub enum TxOutcome<T> {
    /// Commit the transaction and surface `T` in `result`. The kernel
    /// emits `COMMIT_FAILED` if the commit itself fails.
    Commit(T),
    /// Release (rollback) the transaction and surface `T` in `result`.
    Rollback(T),
}

/// Borrowed, lock-not-yet-acquired handle to a resolved session.
///
/// Construct one via [`TypeDbCore::resolve`]. All methods take the
/// per-session lock for exactly the right window; consumers cannot hold
/// it themselves.
pub struct SessionHandle<'a> {
    core: &'a TypeDbCore,
    id: SessionId,
    arc: Arc<Mutex<SessionState>>,
}

impl<'a> SessionHandle<'a> {
    pub fn id(&self) -> &SessionId {
        &self.id
    }
    pub fn core(&self) -> &'a TypeDbCore {
        self.core
    }

    /// Direct access to the per-session `Arc<Mutex<SessionState>>`.
    /// Library consumers should prefer the higher-level helpers
    /// ([`with_read_tx`], [`extensions_mut`], etc.) which acquire the
    /// lock with the right window. This accessor is provided for
    /// kernel-internal raw tool implementations and for the rare
    /// consumer that needs to compose multiple state inspections under
    /// a single lock acquisition.
    pub fn arc(&self) -> &Arc<Mutex<SessionState>> {
        &self.arc
    }

    /// Snapshot the agent-visible session block for use in an envelope.
    pub async fn snapshot(&self) -> SessionSnapshot {
        SessionStore::snapshot_arc(&self.id, &self.arc).await
    }

    /// Read-only borrow of the per-session extension typemap.
    pub async fn extensions<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Extensions) -> R,
    {
        let state = self.arc.lock().await;
        f(&state.extensions)
    }

    /// Mutable borrow of the per-session extension typemap.
    pub async fn extensions_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Extensions) -> R,
    {
        let mut state = self.arc.lock().await;
        f(&mut state.extensions)
    }

    // ---- envelope helpers (canonical §6 shape) -------------------------

    /// Build a success envelope using a fresh snapshot of this session.
    pub async fn ok(&self, result: serde_json::Value, hints: NextMoves) -> CallToolResult {
        let snap = self.snapshot().await;
        envelope_ok(snap, result, hints)
    }

    /// Build an error envelope from an upstream [`InternalError`].
    pub async fn err(
        &self,
        err: InternalError,
        explanation: &str,
        hints: NextMoves,
    ) -> CallToolResult {
        let snap = self.snapshot().await;
        envelope_err(snap, err, explanation, hints)
    }

    /// Build a state-error envelope (kernel-level invariant violation,
    /// no upstream message).
    pub async fn state_err(
        &self,
        class: ErrorClass,
        message: &str,
        hints: NextMoves,
    ) -> CallToolResult {
        let snap = self.snapshot().await;
        envelope_state_error(snap, class, message, hints)
    }

    // ---- the load-bearing transaction helpers (§3 + §5 invariants) ----

    /// Run a closure inside a one-shot READ transaction.
    ///
    /// Enforces the schema-read gate, the single-tx invariant, and the
    /// per-session lock held across the network work (see DESIGN.md §3
    /// reasoning about TSV13). Releases the transaction with
    /// `Transaction::close()` per §5.0.1. The closure's `Ok` value is
    /// wrapped into a canonical success envelope; an `Err` is classified
    /// and rendered through the canonical error envelope with
    /// `hints_on_err`-derived `next_moves`.
    pub async fn with_read_tx<F, T>(
        &self,
        database: &str,
        hints_on_ok: NextMoves,
        f: F,
    ) -> CallToolResult
    where
        F: AsyncFnOnce(&DriverTransaction) -> Result<T, InternalError>,
        T: Serialize,
    {
        // Hold the per-session lock across precondition checks → open →
        // closure → release. See `read_once` in src/handler.rs for the
        // historical reasoning (preventing TSV13 races).
        let state = self.arc.lock().await;
        if state.tx.is_some() {
            drop(state);
            return self
                .state_err(
                    ErrorClass::TxAlreadyOpen,
                    "A transaction is already open in this session. `with_read_tx` \
                     cannot be used while another transaction is open. Commit or \
                     rollback first.",
                    NextMoves::default_for(ErrorClass::TxAlreadyOpen, None),
                )
                .await;
        }
        if !state.schema_seen.contains(database) {
            drop(state);
            return self
                .state_err(
                    ErrorClass::SchemaNotRead,
                    "Cannot read this database: schema has not been read this session. \
                     Call `get_schema(database)` first.",
                    NextMoves::default_for(ErrorClass::SchemaNotRead, Some(database)),
                )
                .await;
        }

        let tx = match self
            .core
            .typedb
            .open_transaction(database, TxKind::Read)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                drop(state);
                let class = e.to_class();
                return self
                    .err(
                        e,
                        "Could not open read transaction.",
                        NextMoves::default_for(class, Some(database)),
                    )
                    .await;
            }
        };

        let result = f(&tx).await;
        // Always close, regardless of outcome. close() per §5.0.1 — never
        // Rollback on a read tx (TSV3) and never leave the stream lingering.
        if let Err(e) = tx.close().await {
            tracing::warn!(error = %e, "with_read_tx close returned an error");
        }
        drop(state);

        match result {
            Ok(value) => {
                let json = serde_json::to_value(&value).unwrap_or(serde_json::Value::Null);
                self.ok(json, hints_on_ok).await
            }
            Err(e) => {
                let class = e.to_class();
                self.err(
                    e,
                    &explain_query_error(class),
                    NextMoves::default_for(class, Some(database)),
                )
                .await
            }
        }
    }

    /// Run a closure inside a WRITE transaction. The closure decides
    /// whether to commit (via [`TxOutcome::Commit`]) or release (via
    /// [`TxOutcome::Rollback`]); the kernel issues the correct wire op.
    /// On commit failure, the kernel emits the canonical `COMMIT_FAILED`
    /// envelope.
    pub async fn with_write_tx<F, T>(
        &self,
        database: &str,
        hints_on_ok: NextMoves,
        f: F,
    ) -> CallToolResult
    where
        F: AsyncFnOnce(&DriverTransaction) -> Result<TxOutcome<T>, InternalError>,
        T: Serialize,
    {
        self.with_owned_tx(database, TxKind::Write, hints_on_ok, f)
            .await
    }

    /// Run a closure inside a SCHEMA transaction. Same semantics as
    /// [`with_write_tx`]. On successful commit of a schema tx, the
    /// schema-read gate for the affected database is cleared per
    /// invariant §3.6.
    pub async fn with_schema_tx<F, T>(
        &self,
        database: &str,
        hints_on_ok: NextMoves,
        f: F,
    ) -> CallToolResult
    where
        F: AsyncFnOnce(&DriverTransaction) -> Result<TxOutcome<T>, InternalError>,
        T: Serialize,
    {
        self.with_owned_tx(database, TxKind::Schema, hints_on_ok, f)
            .await
    }

    /// Borrow the session's currently-open transaction (whatever kind),
    /// for tools that operate on whatever the agent already opened
    /// (e.g. a semantic `query`-style tool). Emits `NO_TX_OPEN` if no
    /// transaction is open. Does NOT commit, rollback, or close — the
    /// agent retains ownership of the transaction lifecycle.
    pub async fn with_current_tx<F, T>(&self, hints_on_ok: NextMoves, f: F) -> CallToolResult
    where
        F: AsyncFnOnce(&DriverTransaction, TxKind, &str) -> Result<T, InternalError>,
        T: Serialize,
    {
        let mut state = self.arc.lock().await;
        let Some(tx) = state.tx.as_mut() else {
            drop(state);
            return self
                .state_err(
                    ErrorClass::NoTxOpen,
                    "No transaction is open in this session. Use `open_read` for \
                     queries, `open_write` for mutations, `open_schema` for \
                     schema changes, or `read_once` for a one-shot read.",
                    NextMoves::default_for(ErrorClass::NoTxOpen, None),
                )
                .await;
        };
        let kind = tx.kind;
        let database = tx.database.clone();
        tx.last_activity = Instant::now();
        let result = f(&tx.transaction, kind, &database).await;
        // Lifecycle: only clear `state.tx` for fatal classes (mirrors the
        // raw `query` handler logic).
        match result {
            Ok(value) => {
                drop(state);
                let json = serde_json::to_value(&value).unwrap_or(serde_json::Value::Null);
                self.ok(json, hints_on_ok).await
            }
            Err(e) => {
                let class = e.to_class();
                if !class.retriable_in_same_tx() {
                    state.tx = None;
                }
                drop(state);
                self.err(
                    e,
                    &explain_query_error(class),
                    NextMoves::default_for(class, Some(&database)),
                )
                .await
            }
        }
    }

    // ---- internals -----------------------------------------------------

    async fn with_owned_tx<F, T>(
        &self,
        database: &str,
        kind: TxKind,
        hints_on_ok: NextMoves,
        f: F,
    ) -> CallToolResult
    where
        F: AsyncFnOnce(&DriverTransaction) -> Result<TxOutcome<T>, InternalError>,
        T: Serialize,
    {
        debug_assert!(matches!(kind, TxKind::Write | TxKind::Schema));
        let state = self.arc.lock().await;
        if state.tx.is_some() {
            drop(state);
            return self
                .state_err(
                    ErrorClass::TxAlreadyOpen,
                    "A transaction is already open in this session. Commit or \
                     rollback it before opening another.",
                    NextMoves::default_for(ErrorClass::TxAlreadyOpen, None),
                )
                .await;
        }
        if !state.schema_seen.contains(database) {
            drop(state);
            return self
                .state_err(
                    ErrorClass::SchemaNotRead,
                    "Cannot open a transaction on this database: schema has not been \
                     read this session. Call `get_schema(database)` first.",
                    NextMoves::default_for(ErrorClass::SchemaNotRead, Some(database)),
                )
                .await;
        }

        let tx = match self.core.typedb.open_transaction(database, kind).await {
            Ok(t) => t,
            Err(e) => {
                drop(state);
                let class = e.to_class();
                return self
                    .err(
                        e,
                        "Could not open transaction.",
                        NextMoves::default_for(class, Some(database)),
                    )
                    .await;
            }
        };

        let outcome = f(&tx).await;
        // Release the per-session lock now: tx is local to this function.
        drop(state);

        match outcome {
            Ok(TxOutcome::Commit(value)) => {
                match tx.commit().await {
                    Ok(()) => {
                        // Schema commit clears the gate for this db (§3.6).
                        if matches!(kind, TxKind::Schema) {
                            let mut s = self.arc.lock().await;
                            s.schema_seen.remove(database);
                        }
                        let json = serde_json::to_value(&value).unwrap_or(serde_json::Value::Null);
                        self.ok(json, hints_on_ok).await
                    }
                    Err(e) => {
                        let internal = InternalError::Driver(e);
                        let class = internal.to_class();
                        self.err(
                            internal,
                            "Commit failed. The transaction has been closed and NO \
                             changes were persisted — including any uncommitted \
                             inserts/updates from earlier queries in this tx.",
                            NextMoves::default_for(class, Some(database)),
                        )
                        .await
                    }
                }
            }
            Ok(TxOutcome::Rollback(value)) => {
                if let Err(e) = tx.rollback().await {
                    tracing::warn!(error = %e, ?kind, "with_*_tx rollback returned an error");
                }
                let json = serde_json::to_value(&value).unwrap_or(serde_json::Value::Null);
                self.ok(json, hints_on_ok).await
            }
            Err(e) => {
                // Closure errored. Roll back to discard partial work,
                // then surface the error.
                if let Err(re) = tx.rollback().await {
                    tracing::warn!(error = %re, ?kind, "with_*_tx rollback on error path failed");
                }
                let class = e.to_class();
                self.err(
                    e,
                    &explain_query_error(class),
                    NextMoves::default_for(class, Some(database)),
                )
                .await
            }
        }
    }
}

// =========================================================================
// Convenience: re-use kernel query materialization (mirrors raw `query`).
// =========================================================================

/// Materialize a TypeDB query inside a `with_*_tx` closure. Returns the
/// JSON-shaped [`QueryAnswerJson`] (the same shape the raw `query` tool
/// produces). On truncation the caller is responsible for surfacing
/// `RESULT_LIMIT_EXCEEDED` — typically by returning an `InternalError`
/// that classifies to it, or by inspecting `json.truncated` and emitting
/// the canonical envelope directly.
pub async fn run_typeql(
    tx: &DriverTransaction,
    query: &str,
    cap: usize,
) -> Result<crate::typedb::QueryAnswerJson, InternalError> {
    let answer = tx.query(query).await.map_err(InternalError::Driver)?;
    query_answer_to_json(answer, cap).await
}

// Helper to manufacture an OpenTx record from the raw tool path (used by
// the raw open_* tools in src/tools.rs). Not part of the public API.
pub(crate) async fn stash_open_tx(
    state: &mut SessionState,
    database: String,
    kind: TxKind,
    transaction: DriverTransaction,
) {
    let short_id = format!("tx_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    state.tx = Some(OpenTx {
        id: short_id,
        database,
        kind,
        transaction,
        last_activity: Instant::now(),
    });
}
