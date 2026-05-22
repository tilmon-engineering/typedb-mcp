//! MCP tool handlers. See DESIGN.md §7.
//!
//! Sessions are server-issued (see DESIGN.md §3). The agent calls
//! `start_session` to mint a `session_id`, then passes it as an argument
//! to every other tool. The handler resolves the ID against
//! [`SessionStore`] on entry; on miss (`SESSION_UNKNOWN`) or expiry
//! (`SESSION_EXPIRED`) it returns a structured error envelope and tells
//! the agent to call `start_session` again.
//!
//! Per-session state is held behind a `tokio::sync::Mutex` so the live
//! driver `Transaction` can be borrowed across `await` points safely. The
//! agent issues one tool call at a time per session, so per-session
//! contention is theoretical.
//!
//! Agent-facing errors (e.g. `PARSE_ERROR`) are returned as `CallToolResult`
//! with `is_error: true` carrying the structured envelope — NOT as
//! protocol-level `McpError`. Protocol errors are reserved for things the
//! agent cannot reason about (transport breakage, malformed JSON requests).

use std::{sync::Arc, time::Instant};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    config::Config,
    error::{ErrorClass, InternalError},
    session::{
        OpenTx, SessionId, SessionResolveError, SessionSnapshot, SessionState, SessionStore,
    },
    typedb::{TxKind, TypeDbClient, query_answer_to_json},
};

#[derive(Clone)]
pub struct TypeDbMcp {
    pub config: Arc<Config>,
    pub typedb: Arc<TypeDbClient>,
    pub sessions: Arc<SessionStore>,
    pub tool_router: ToolRouter<TypeDbMcp>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionOnlyParam {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionAndDatabaseParam {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
    /// Name of the TypeDB database.
    pub database: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionAndQueryParam {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
    /// TypeQL 3.x query. The transaction kind required is determined by the
    /// currently-open transaction in this session.
    pub query: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadOnceParam {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
    pub database: String,
    pub query: String,
}

#[tool_router]
impl TypeDbMcp {
    pub fn new(
        config: Arc<Config>,
        typedb: Arc<TypeDbClient>,
        sessions: Arc<SessionStore>,
    ) -> Self {
        Self {
            config,
            typedb,
            sessions,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "\
Mint a new server-side session and return its `session_id`. Every other \
tool in this server requires `session_id` as an argument — call this \
first. The response also includes the list of databases on the server \
(same content as `list_databases`) so you can pick one without a \
follow-up call. Sessions expire after a configured period of inactivity \
(default 60 minutes); on `SESSION_EXPIRED` or `SESSION_UNKNOWN` from any \
other tool, call `start_session` again for a fresh ID.")]
    async fn start_session(&self) -> Result<CallToolResult, McpError> {
        let ttl = self.config.session_ttl();
        let sid = self.sessions.start(ttl).await;
        let expires_in_seconds = ttl.as_secs();

        // Fetch the database list for convenience. If TypeDB is
        // unreachable, return the session_id anyway with an error
        // envelope — the agent can retry the listing via list_databases
        // without re-minting.
        let arc = match self
            .sessions
            .resolve_and_touch(&sid, ttl)
            .await
        {
            Ok(a) => a,
            Err(_) => {
                // Should be impossible: we just minted it. Defensive.
                return Ok(envelope_state_error_no_session(
                    ErrorClass::SessionUnknown,
                    "Internal error: just-minted session was not resolvable.",
                    next_moves::on_session_unknown(),
                ));
            }
        };
        let snap = SessionStore::snapshot_arc(&sid, &arc).await;

        match self.typedb.list_databases().await {
            Ok(names) => {
                let result = serde_json::json!({
                    "session_id": sid.0,
                    "expires_in_seconds": expires_in_seconds,
                    "databases": names
                        .iter()
                        .map(|n| serde_json::json!({"name": n}))
                        .collect::<Vec<_>>(),
                });
                Ok(envelope_ok(snap, result, next_moves::after_start_session()))
            }
            Err(e) => Ok(envelope_err(
                snap,
                e,
                "Session was created, but the database list could not be \
                 fetched. Retry via `list_databases` once the upstream is \
                 back; your session_id is still valid.",
                next_moves::after_start_session_partial(&sid.0),
            )),
        }
    }

    #[tool(description = "\
List all databases available on the TypeDB server. Read-only; safe to call \
without restriction (other than needing a valid `session_id`).")]
    async fn list_databases(
        &self,
        Parameters(p): Parameters<SessionOnlyParam>,
    ) -> Result<CallToolResult, McpError> {
        let (sid, arc) = match self.resolve(&p.session_id).await {
            Ok(r) => r,
            Err(env) => return Ok(env),
        };
        let snap = SessionStore::snapshot_arc(&sid, &arc).await;
        match self.typedb.list_databases().await {
            Ok(names) => {
                let result = serde_json::json!({
                    "databases": names.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>()
                });
                Ok(envelope_ok(snap, result, next_moves::after_list_databases()))
            }
            Err(e) => Ok(envelope_err(
                snap,
                e,
                "Could not list databases.",
                next_moves::on_upstream_unavailable(),
            )),
        }
    }

    #[tool(description = "\
Return the complete TypeQL `define` source for a database. You MUST call \
this before opening any transaction on the database; the safety layer blocks \
`open_read`, `open_write`, `open_schema`, and `read_once` until you do. \
TypeQL 3.x differs materially from 2.x — do not write queries from prior \
assumptions; treat this schema as ground truth.")]
    async fn get_schema(
        &self,
        Parameters(p): Parameters<SessionAndDatabaseParam>,
    ) -> Result<CallToolResult, McpError> {
        let (sid, arc) = match self.resolve(&p.session_id).await {
            Ok(r) => r,
            Err(env) => return Ok(env),
        };
        match self.typedb.get_schema(&p.database).await {
            Ok(schema) => {
                {
                    let mut state = arc.lock().await;
                    state.schema_seen.insert(p.database.clone());
                }
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                Ok(envelope_ok(
                    snap,
                    serde_json::json!({
                        "database": p.database,
                        "schema": schema,
                    }),
                    next_moves::after_get_schema(&p.database),
                ))
            }
            Err(e) => {
                let class = e.to_class();
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                Ok(envelope_err(
                    snap,
                    e,
                    "Could not fetch schema.",
                    next_moves::on_error(class, Some(&p.database)),
                ))
            }
        }
    }

    #[tool(description = "\
Open a READ transaction on the named database. Read-only; commits are not \
permitted (use `rollback` to close, or just leave it for the idle reaper). \
Requires that `get_schema(database)` was called earlier in this session.")]
    async fn open_read(
        &self,
        Parameters(p): Parameters<SessionAndDatabaseParam>,
    ) -> Result<CallToolResult, McpError> {
        self.open_any(p.session_id, p.database, TxKind::Read).await
    }

    #[tool(description = "\
Open a WRITE transaction on the named database. Writes only persist on \
`commit`; use `rollback` to discard. Requires prior `get_schema(database)`. \
Constraint violations that reach TypeDB's write pipeline ABORT the \
transaction — you must then open a new one to continue.")]
    async fn open_write(
        &self,
        Parameters(p): Parameters<SessionAndDatabaseParam>,
    ) -> Result<CallToolResult, McpError> {
        self.open_any(p.session_id, p.database, TxKind::Write).await
    }

    #[tool(description = "\
Open a SCHEMA transaction on the named database. SCHEMA changes are \
DESTRUCTIVE and only persist on `commit`. Requires prior \
`get_schema(database)`. On successful commit of a schema transaction, the \
schema-read gate is reset for this database — you must call `get_schema` \
again before opening further transactions.")]
    async fn open_schema(
        &self,
        Parameters(p): Parameters<SessionAndDatabaseParam>,
    ) -> Result<CallToolResult, McpError> {
        self.open_any(p.session_id, p.database, TxKind::Schema).await
    }

    #[tool(description = "\
Execute a TypeQL query against the currently-open transaction. The required \
transaction kind is determined by the open transaction (set at `open_*` \
time). Parse, type, and wrong-tx-type errors leave the transaction OPEN — \
fix the query and retry. Write-pipeline errors close the transaction; you \
must open a new one. Results are capped (see config); paginate with \
`sort $k; offset N; limit M;` — `offset` MUST come before `limit`.")]
    async fn query(
        &self,
        Parameters(p): Parameters<SessionAndQueryParam>,
    ) -> Result<CallToolResult, McpError> {
        let (sid, arc) = match self.resolve(&p.session_id).await {
            Ok(r) => r,
            Err(env) => return Ok(env),
        };
        let mut state = arc.lock().await;

        let Some(tx) = state.tx.as_mut() else {
            drop(state);
            let snap = SessionStore::snapshot_arc(&sid, &arc).await;
            return Ok(envelope_state_error(
                snap,
                ErrorClass::NoTxOpen,
                "No transaction is open in this session. Use `open_read` for queries, \
                 `open_write` for mutations, `open_schema` for schema changes, or \
                 `read_once` for a one-shot read.",
                next_moves::on_error(ErrorClass::NoTxOpen, None),
            ));
        };
        let kind = tx.kind;
        let database = tx.database.clone();
        tx.last_activity = Instant::now();

        let answer_result = tx.transaction.query(&p.query).await;
        let result_cap = self.config.server.result_cap;

        match answer_result {
            Ok(answer) => {
                match query_answer_to_json(answer, result_cap).await {
                    Ok(json) => {
                        let truncated = json.truncated;
                        let value = json.into_value();
                        if truncated {
                            drop(state);
                            let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                            return Ok(envelope_state_error(
                                snap,
                                ErrorClass::ResultLimitExceeded,
                                &format!(
                                    "Query returned more than {result_cap} answers (server cap). \
                                     The result has been discarded — TypeDB does not paginate \
                                     after the fact. Re-issue with `sort $k; offset N; limit M;` \
                                     (`offset` MUST come before `limit`)."
                                ),
                                next_moves::on_error(ErrorClass::ResultLimitExceeded, None),
                            ));
                        }
                        drop(state);
                        let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                        Ok(envelope_ok(snap, value, next_moves::after_query_ok(kind, &database)))
                    }
                    Err(e) => {
                        let class = e.to_class();
                        if !class.retriable_in_same_tx() {
                            state.tx = None;
                        }
                        drop(state);
                        let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                        let moves = next_moves::on_error(class, None);
                        Ok(envelope_err(snap, e, &explain_query_error(class), moves))
                    }
                }
            }
            Err(e) => {
                let internal = InternalError::Driver(e);
                let class = internal.to_class();
                if !class.retriable_in_same_tx() {
                    state.tx = None;
                }
                drop(state);
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                let moves = next_moves::on_error(class, None);
                Ok(envelope_err(snap, internal, &explain_query_error(class), moves))
            }
        }
    }

    #[tool(description = "\
Commit the currently-open transaction. Only valid for WRITE and SCHEMA \
transactions (READ transactions cannot be committed; use `rollback`). On \
successful commit of a SCHEMA transaction, the schema-read gate is reset \
for the affected database.")]
    async fn commit(
        &self,
        Parameters(p): Parameters<SessionOnlyParam>,
    ) -> Result<CallToolResult, McpError> {
        let (sid, arc) = match self.resolve(&p.session_id).await {
            Ok(r) => r,
            Err(env) => return Ok(env),
        };
        let mut state = arc.lock().await;

        let Some(tx) = state.tx.as_ref() else {
            drop(state);
            let snap = SessionStore::snapshot_arc(&sid, &arc).await;
            return Ok(envelope_state_error(
                snap,
                ErrorClass::NoTxOpen,
                "No transaction is open in this session. There is nothing to commit.",
                next_moves::on_error(ErrorClass::NoTxOpen, None),
            ));
        };

        if matches!(tx.kind, TxKind::Read) {
            drop(state);
            let snap = SessionStore::snapshot_arc(&sid, &arc).await;
            return Ok(envelope_state_error(
                snap,
                ErrorClass::TxIsRead,
                "READ transactions cannot be committed (TypeDB will reject this — \
                 TSV2). Use `rollback` to close the transaction.",
                next_moves::on_error(ErrorClass::TxIsRead, None),
            ));
        }

        let owned = state.tx.take().expect("checked just above");
        let database = owned.database.clone();
        let kind = owned.kind;
        let commit_result = owned.transaction.commit().await;

        match commit_result {
            Ok(()) => {
                if matches!(kind, TxKind::Schema) {
                    state.schema_seen.remove(&database);
                }
                drop(state);
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                Ok(envelope_ok(
                    snap,
                    serde_json::json!({"committed": true}),
                    next_moves::after_commit_ok(kind, &database),
                ))
            }
            Err(e) => {
                let internal = InternalError::Driver(e);
                let class = internal.to_class();
                drop(state);
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                Ok(envelope_err(
                    snap,
                    internal,
                    "Commit failed. The transaction has been closed and NO changes were \
                     persisted — including any uncommitted inserts/updates from earlier \
                     queries in this tx. Any concept IIDs in the upstream details below \
                     refer to never-committed state and cannot be looked up in a fresh tx.",
                    next_moves::on_error(class, Some(&database)),
                ))
            }
        }
    }

    #[tool(description = "\
Roll back (discard) the currently-open transaction. Forgiving: if no \
transaction is open, this is a no-op success.")]
    async fn rollback(
        &self,
        Parameters(p): Parameters<SessionOnlyParam>,
    ) -> Result<CallToolResult, McpError> {
        let (sid, arc) = match self.resolve(&p.session_id).await {
            Ok(r) => r,
            Err(env) => return Ok(env),
        };
        let mut state = arc.lock().await;

        let Some(tx) = state.tx.take() else {
            drop(state);
            let snap = SessionStore::snapshot_arc(&sid, &arc).await;
            return Ok(envelope_ok(
                snap,
                serde_json::json!({
                    "rolled_back": false,
                    "message": "No transaction was open; nothing to roll back."
                }),
                next_moves::after_rollback_ok(),
            ));
        };

        if let Err(e) = tx.transaction.rollback().await {
            tracing::warn!(error = %e, "driver rollback returned an error");
        }
        drop(state);
        let snap = SessionStore::snapshot_arc(&sid, &arc).await;
        Ok(envelope_ok(
            snap,
            serde_json::json!({"rolled_back": true}),
            next_moves::after_rollback_ok(),
        ))
    }

    #[tool(description = "\
Run a single read query against the named database in a managed \
read-and-close transaction. Convenience for one-shot reads — the transaction \
is opened, the query is run, and the transaction is closed atomically. \
Requires prior `get_schema(database)`. Result is capped (see config); \
paginate with `sort $k; offset N; limit M;`.")]
    async fn read_once(
        &self,
        Parameters(p): Parameters<ReadOnceParam>,
    ) -> Result<CallToolResult, McpError> {
        let (sid, arc) = match self.resolve(&p.session_id).await {
            Ok(r) => r,
            Err(env) => return Ok(env),
        };
        let state = arc.lock().await;

        if state.tx.is_some() {
            drop(state);
            let snap = SessionStore::snapshot_arc(&sid, &arc).await;
            return Ok(envelope_state_error(
                snap,
                ErrorClass::TxAlreadyOpen,
                "A transaction is already open in this session. `read_once` cannot \
                 be used while another transaction is open. Commit or rollback first.",
                next_moves::on_error(ErrorClass::TxAlreadyOpen, None),
            ));
        }
        if !state.schema_seen.contains(&p.database) {
            drop(state);
            let snap = SessionStore::snapshot_arc(&sid, &arc).await;
            return Ok(envelope_state_error(
                snap,
                ErrorClass::SchemaNotRead,
                "Cannot read this database: schema has not been read this session. \
                 Call `get_schema(database)` first.",
                next_moves::on_error(ErrorClass::SchemaNotRead, Some(&p.database)),
            ));
        }
        drop(state);

        let tx = match self.typedb.open_transaction(&p.database, TxKind::Read).await {
            Ok(t) => t,
            Err(e) => {
                let class = e.to_class();
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                return Ok(envelope_err(
                    snap,
                    e,
                    "Could not open read transaction.",
                    next_moves::on_error(class, Some(&p.database)),
                ));
            }
        };

        let result_cap = self.config.server.result_cap;
        let answer_result = tx.query(&p.query).await;
        let _ = tx.rollback().await;

        let snap = SessionStore::snapshot_arc(&sid, &arc).await;
        match answer_result {
            Ok(answer) => match query_answer_to_json(answer, result_cap).await {
                Ok(json) => {
                    if json.truncated {
                        Ok(envelope_state_error(
                            snap,
                            ErrorClass::ResultLimitExceeded,
                            &format!(
                                "Query returned more than {result_cap} answers (server cap). \
                                 Re-issue via `read_once` (or an explicit `open_read`) with \
                                 `sort $k; offset N; limit M;` (`offset` MUST come before `limit`)."
                            ),
                            next_moves::on_error(ErrorClass::ResultLimitExceeded, None),
                        ))
                    } else {
                        Ok(envelope_ok(snap, json.into_value(), next_moves::after_read_once_ok()))
                    }
                }
                Err(e) => {
                    let class = e.to_class();
                    let moves = next_moves::on_error(class, Some(&p.database));
                    Ok(envelope_err(snap, e, &explain_query_error(class), moves))
                }
            },
            Err(e) => {
                let internal = InternalError::Driver(e);
                let class = internal.to_class();
                let moves = next_moves::on_error(class, Some(&p.database));
                Ok(envelope_err(snap, internal, &explain_query_error(class), moves))
            }
        }
    }

    async fn open_any(
        &self,
        session_id: String,
        database: String,
        kind: TxKind,
    ) -> Result<CallToolResult, McpError> {
        let (sid, arc) = match self.resolve(&session_id).await {
            Ok(r) => r,
            Err(env) => return Ok(env),
        };
        {
            let state = arc.lock().await;
            if state.tx.is_some() {
                drop(state);
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                return Ok(envelope_state_error(
                    snap,
                    ErrorClass::TxAlreadyOpen,
                    "A transaction is already open in this session. Commit or rollback \
                     it before opening another.",
                    next_moves::on_error(ErrorClass::TxAlreadyOpen, None),
                ));
            }
            if !state.schema_seen.contains(&database) {
                drop(state);
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                return Ok(envelope_state_error(
                    snap,
                    ErrorClass::SchemaNotRead,
                    "Cannot open a transaction on this database: schema has not been \
                     read this session. Call `get_schema(database)` first.",
                    next_moves::on_error(ErrorClass::SchemaNotRead, Some(&database)),
                ));
            }
        }

        let transaction = match self.typedb.open_transaction(&database, kind).await {
            Ok(t) => t,
            Err(e) => {
                let class = e.to_class();
                let snap = SessionStore::snapshot_arc(&sid, &arc).await;
                return Ok(envelope_err(
                    snap,
                    e,
                    "Could not open transaction.",
                    next_moves::on_error(class, Some(&database)),
                ));
            }
        };

        let short_id = format!("tx_{}", &uuid::Uuid::new_v4().to_string()[..8]);
        {
            let mut state = arc.lock().await;
            state.tx = Some(OpenTx {
                id: short_id,
                database: database.clone(),
                kind,
                transaction,
                last_activity: Instant::now(),
            });
        }
        let snap = SessionStore::snapshot_arc(&sid, &arc).await;
        let moves = match kind {
            TxKind::Read => next_moves::after_open_read(&database),
            TxKind::Write => next_moves::after_open_write(&database),
            TxKind::Schema => next_moves::after_open_schema(&database),
        };
        Ok(envelope_ok(snap, serde_json::json!({"opened": true}), moves))
    }

    /// Resolve an agent-supplied session_id string against the
    /// SessionStore. On success returns the (SessionId, Arc) pair. On
    /// failure returns a ready-to-return CallToolResult error envelope.
    async fn resolve(
        &self,
        session_id: &str,
    ) -> Result<(SessionId, Arc<Mutex<SessionState>>), CallToolResult> {
        let sid = SessionId(session_id.to_owned());
        let ttl = self.config.session_ttl();
        match self.sessions.resolve_and_touch(&sid, ttl).await {
            Ok(arc) => Ok((sid, arc)),
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

#[tool_handler]
impl ServerHandler for TypeDbMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "TypeDB safety-gated query server. Call `start_session` first to \
             obtain a `session_id`; every other tool requires it as an \
             argument. Always call `get_schema` for a database before \
             opening any transaction on it. Errors carry an \
             `error.retriable_in_same_tx` boolean indicating whether the \
             open transaction (if any) survived. On `SESSION_EXPIRED` or \
             `SESSION_UNKNOWN`, call `start_session` again. See DESIGN.md \
             for the full state machine."
                .to_owned(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

// -- next_moves catalogue -------------------------------------------------

mod next_moves {
    use crate::error::ErrorClass;
    use crate::typedb::TxKind;

    pub fn after_start_session() -> Vec<String> {
        vec![
            "Pass `session_id` to EVERY subsequent tool call.".into(),
            "The `databases` list above is the same content `list_databases` \
             returns; pick one and call `get_schema(session_id=..., \
             database=<name>)` before opening any transaction on it.".into(),
            "Sessions expire after a configured period of inactivity. On \
             `SESSION_EXPIRED` or `SESSION_UNKNOWN` from any tool, call \
             `start_session` again for a fresh ID.".into(),
        ]
    }

    pub fn after_start_session_partial(session_id: &str) -> Vec<String> {
        vec![
            format!(
                "Your session_id ({session_id}) is valid; the database list \
                 fetch failed. Retry it via `list_databases(session_id=\"{session_id}\")`."
            ),
            "Pass `session_id` to EVERY subsequent tool call.".into(),
        ]
    }

    pub fn on_session_unknown() -> Vec<String> {
        vec![
            "Call `start_session` to mint a fresh `session_id`, then reissue \
             the call you just made with the new ID.".into(),
        ]
    }

    pub fn on_session_expired() -> Vec<String> {
        vec![
            "Your previous session is gone (idle TTL). Call `start_session` \
             to mint a fresh `session_id`; you will need to re-call \
             `get_schema` on any database before opening transactions.".into(),
        ]
    }

    pub fn after_list_databases() -> Vec<String> {
        vec![
            "Call `get_schema(session_id=..., database=<name>)` for any \
             database before opening a transaction on it or calling \
             `read_once`.".into(),
        ]
    }

    pub fn after_get_schema(db: &str) -> Vec<String> {
        vec![
            format!("Open a transaction on `{db}`: \
                     `open_read(session_id=..., database=\"{db}\")` for queries, \
                     `open_write(session_id=..., database=\"{db}\")` for mutations, \
                     or `open_schema(session_id=..., database=\"{db}\")` for \
                     schema changes."),
            format!("Or run a one-shot read with \
                     `read_once(session_id=..., database=\"{db}\", query=...)`."),
        ]
    }

    pub fn after_open_read(db: &str) -> Vec<String> {
        vec![
            "Submit TypeQL with `query(session_id=..., query=\"match ...; \
             fetch { ... };\")`. Repeat for as many reads as you need.".into(),
            "Close the transaction with `rollback(session_id=...)` when done. \
             READ transactions cannot be committed (TypeDB will reject this \
             with TSV2).".into(),
            format!("Open transaction is on database `{db}`."),
        ]
    }

    pub fn after_open_write(db: &str) -> Vec<String> {
        vec![
            "Submit TypeQL with `query(session_id=..., query=\"...\")`. Mix \
             `match`/`fetch` reads with `insert`/`delete`/`update` writes as \
             needed.".into(),
            "End with `commit(session_id=...)` to persist, or \
             `rollback(session_id=...)` to discard. A write that reaches \
             TypeDB's write pipeline and fails will ABORT the transaction \
             automatically; you'll then need to open a new one.".into(),
            format!("Open transaction is on database `{db}`."),
        ]
    }

    pub fn after_open_schema(db: &str) -> Vec<String> {
        vec![
            "Submit TypeQL `define` / `undefine` / `redefine` with \
             `query(session_id=..., query=\"...\")`.".into(),
            format!("End with `commit(session_id=...)` to persist (this will \
                     CLEAR the schema-read gate for `{db}` — you'll need to \
                     call `get_schema(session_id=..., database=\"{db}\")` \
                     again before opening further transactions on it), or \
                     `rollback(session_id=...)` to discard."),
        ]
    }

    pub fn after_query_ok(kind: TxKind, db: &str) -> Vec<String> {
        let close: String = match kind {
            TxKind::Read => "Close with `rollback(session_id=...)` when done (READ \
                             transactions cannot be committed).".into(),
            TxKind::Write => "End with `commit(session_id=...)` to persist, or \
                              `rollback(session_id=...)` to discard.".into(),
            TxKind::Schema => format!(
                "End with `commit(session_id=...)` to persist the schema \
                 change (which will CLEAR the schema-read gate for `{db}` — \
                 you'll need to call `get_schema(session_id=..., \
                 database=\"{db}\")` again before opening further \
                 transactions on it), or `rollback(session_id=...)` to discard."
            ),
        };
        vec![
            "Continue with more `query(session_id=..., ...)` calls on the same \
             transaction if needed.".into(),
            close,
        ]
    }

    pub fn after_commit_ok(kind: TxKind, db: &str) -> Vec<String> {
        let mut v = vec![
            "Open another transaction with `open_read` / `open_write` / \
             `open_schema` (each takes `session_id` + `database`), or run a \
             one-shot read with `read_once`.".into(),
            "Or call `get_schema(session_id=..., database=<other>)` if you \
             want to work on a different database.".into(),
        ];
        if matches!(kind, TxKind::Schema) {
            v.insert(0, format!(
                "The schema-read gate for `{db}` was cleared by this commit. \
                 Call `get_schema(session_id=..., database=\"{db}\")` again \
                 before opening any transaction on it."
            ));
        }
        v
    }

    pub fn after_rollback_ok() -> Vec<String> {
        vec![
            "Open another transaction with `open_read` / `open_write` / \
             `open_schema` (each takes `session_id` + `database`), or run a \
             one-shot read with `read_once`.".into(),
            "Or call `get_schema(session_id=..., database=<other>)` to work \
             on a different database.".into(),
        ]
    }

    pub fn after_read_once_ok() -> Vec<String> {
        vec![
            "Run another `read_once(session_id=..., ...)`, or call \
             `get_schema(session_id=..., database=<other>)` for a different \
             database.".into(),
            "If you need multiple reads or any writes, open an explicit \
             transaction with `open_read` / `open_write` / `open_schema`.".into(),
        ]
    }

    pub fn on_upstream_unavailable() -> Vec<String> {
        vec![
            "Retry the call once — the TypeDB upstream was unreachable. If the error \
             persists, the server is down or misconfigured; surface this to the human \
             operator rather than looping.".into(),
        ]
    }

    pub fn on_error(class: ErrorClass, db_hint: Option<&str>) -> Vec<String> {
        use ErrorClass::*;
        match class {
            SessionUnknown => on_session_unknown(),
            SessionExpired => on_session_expired(),
            SchemaNotRead => {
                let db = db_hint.unwrap_or("<name>");
                vec![format!(
                    "Call `get_schema(session_id=..., database=\"{db}\")` first, \
                     then retry this call."
                )]
            }
            TxAlreadyOpen => vec![
                "A transaction is already open in this session. Either continue \
                 using it with `query(session_id=..., ...)`, then \
                 `commit(session_id=...)`/`rollback(session_id=...)`, or \
                 `rollback` it now and retry this call.".into(),
            ],
            NoTxOpen => vec![
                "Open a transaction first with `open_read` / `open_write` / \
                 `open_schema` (each takes `session_id` + `database`), or use \
                 `read_once` for a one-shot read.".into(),
            ],
            TxIsRead => vec![
                "READ transactions are closed with `rollback(session_id=...)`, \
                 not `commit`. Call `rollback` instead.".into(),
            ],
            WrongTxType => vec![
                "Your transaction is still open. Either issue a different query, or \
                 `rollback(session_id=...)` and open a transaction of the correct kind.".into(),
            ],
            ParseError | TypeError => vec![
                "Your transaction is still open. Fix the query and call \
                 `query(session_id=..., ...)` again, or \
                 `rollback(session_id=...)` to start over.".into(),
            ],
            WriteFailed => vec![
                "The transaction has been ABORTED by TypeDB. Open a new transaction \
                 (typically `open_write(session_id=..., database=...)`) to continue. \
                 The schema-read gate is still valid (no schema change happened), so \
                 you do NOT need to call `get_schema` again before the next \
                 `open_*`.".into(),
                "Before retrying the failing write, consider re-reading the relevant \
                 *data* (the constraint that fired tells you something about reality \
                 you may not have known). Re-reading the schema is optional — useful \
                 only if you suspect you misremembered a constraint shape.".into(),
            ],
            CommitFailed => vec![
                "The transaction is closed and no changes were persisted. Open a new \
                 transaction with `open_write(session_id=..., database=...)` (or \
                 `open_schema`) to retry.".into(),
                "Before retrying, read back the existing data — a commit-time \
                 cardinality violation often means another instance is required, or \
                 you missed an attribute.".into(),
            ],
            ResultLimitExceeded => vec![
                "Re-issue the query with `sort $k; offset N; limit M;` — `offset` MUST \
                 come before `limit` (pipeline stages run in textual order). Your \
                 transaction is still open.".into(),
            ],
            Timeout | IdleTimeout => vec![
                "The transaction is gone. Open a new one with `open_read` / \
                 `open_write` / `open_schema` (each takes `session_id` + \
                 `database`) to continue.".into(),
            ],
            UnknownDatabase => vec![
                "Call `list_databases(session_id=...)` to see the available \
                 database names.".into(),
            ],
            UpstreamUnavailable => on_upstream_unavailable(),
            Unclassified => vec![
                "Assume the transaction (if any) is gone. Open a new one with \
                 `open_read` / `open_write` / `open_schema` if you intend to continue.".into(),
                "The full TypeDB error is preserved verbatim in `error.message` (after \
                 `(details: ...)`), and every bracketed upstream code is in \
                 `error.typedb_codes` — quote them when reporting to the human operator.".into(),
                "If this class recurs, escalate: it likely means TypeDB introduced a new \
                 error code post-dating this server's classifier, and someone needs to \
                 update `classify_typedb_error` in `src/error.rs` to recognize it.".into(),
            ],
        }
    }
}

// -- envelope -------------------------------------------------------------

#[derive(Serialize)]
struct AgentEnvelope<'a> {
    session: &'a SessionSnapshot,
    next_moves: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorPayload>,
}

#[derive(Serialize)]
struct ErrorPayload {
    class: ErrorClass,
    message: String,
    retriable_in_same_tx: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    typedb_codes: Option<Vec<String>>,
}

fn envelope_ok(
    session: SessionSnapshot,
    result: serde_json::Value,
    next_moves: Vec<String>,
) -> CallToolResult {
    let env = AgentEnvelope {
        session: &session,
        next_moves,
        result: Some(result),
        error: None,
    };
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&env).expect("envelope serialization"),
    )])
}

fn envelope_err(
    session: SessionSnapshot,
    err: InternalError,
    explanation: &str,
    next_moves: Vec<String>,
) -> CallToolResult {
    let class = err.to_class();
    let typedb_codes = match &err {
        InternalError::Driver(d) => Some(extract_codes(&d.to_string())),
        _ => None,
    };
    let message = format!("{explanation} (details: {err})");
    let payload = ErrorPayload {
        class,
        message,
        retriable_in_same_tx: class.retriable_in_same_tx(),
        typedb_codes,
    };
    let env = AgentEnvelope {
        session: &session,
        next_moves,
        result: None,
        error: Some(payload),
    };
    CallToolResult::error(vec![Content::text(
        serde_json::to_string_pretty(&env).expect("envelope serialization"),
    )])
}

fn envelope_state_error(
    session: SessionSnapshot,
    class: ErrorClass,
    message: &str,
    next_moves: Vec<String>,
) -> CallToolResult {
    let payload = ErrorPayload {
        class,
        message: message.to_owned(),
        retriable_in_same_tx: class.retriable_in_same_tx(),
        typedb_codes: None,
    };
    let env = AgentEnvelope {
        session: &session,
        next_moves,
        result: None,
        error: Some(payload),
    };
    CallToolResult::error(vec![Content::text(
        serde_json::to_string_pretty(&env).expect("envelope serialization"),
    )])
}

/// Variant for SESSION_UNKNOWN / SESSION_EXPIRED where we have no
/// resolved session to snapshot.
fn envelope_state_error_no_session(
    class: ErrorClass,
    message: &str,
    next_moves: Vec<String>,
) -> CallToolResult {
    envelope_state_error(SessionSnapshot::default(), class, message, next_moves)
}

fn explain_query_error(class: ErrorClass) -> String {
    match class {
        ErrorClass::ParseError =>
            "TypeQL parse error. Your transaction is still open — fix the query and retry.".into(),
        ErrorClass::TypeError =>
            "TypeQL type-inference error (e.g. unknown type label). Your transaction is still open — fix the query and retry.".into(),
        ErrorClass::WrongTxType =>
            "Wrong transaction kind for this query. Your transaction is still open; rollback and open the correct kind, or issue a different query.".into(),
        ErrorClass::WriteFailed =>
            "Write failed and the transaction has been aborted by TypeDB. Open a new transaction to continue.".into(),
        ErrorClass::CommitFailed =>
            "Commit-time validation failed; the transaction is closed and no changes were \
             persisted (any concept IIDs in the upstream details refer to never-committed state).".into(),
        ErrorClass::ResultLimitExceeded =>
            "Result set exceeded the server-side cap. Re-issue with `sort $k; offset N; limit M;` (offset MUST come before limit).".into(),
        ErrorClass::Timeout =>
            "Server-side query timeout; the transaction is closed.".into(),
        ErrorClass::IdleTimeout =>
            "Your transaction was closed (idle reaper or prior write error). Open a new one to continue.".into(),
        ErrorClass::Unclassified =>
            "TypeDB returned an error this MCP server does not recognize — most likely a code \
             introduced in a newer TypeDB release than this server was built against. The \
             VERBATIM driver message is included in the details below, and every bracketed \
             code from the stack is in `error.typedb_codes`. Treat any open transaction as \
             gone; surface this to the human operator if it recurs (it may indicate the \
             classifier needs updating).".into(),
        _ => "Query failed; see error details.".into(),
    }
}

fn extract_codes(message: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = message.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(end_rel) = message[i + 1..].find(']') {
                let code = &message[i + 1..i + 1 + end_rel];
                if !code.is_empty()
                    && code.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                {
                    out.push(code.to_owned());
                }
                i += 1 + end_rel + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}
