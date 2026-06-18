//! Generic raw tool router — exposes the ten default tools enumerated in
//! DESIGN.md §7, plus optional database-admin tools when explicitly enabled,
//! against any handler type `H: HasTypeDbCore`.
//!
//! This module is the library equivalent of the macro-generated tool
//! router in [`crate::handler`]: it is written without `#[tool_router]`
//! so the routes can be generic over the consumer's handler type.
//!
//! Typical use (consumer crate):
//!
//! ```ignore
//! use typedb_mcp_core::tools;
//!
//! let mut router = tools::raw_tools_router::<MyHandler>(Default::default());
//! router.merge(MyHandler::semantic_tools_router()); // consumer's own #[tool_router]
//! ```
//!
//! Knobs:
//! - [`RawToolsConfig::with_prefix`] — prepend a string to every tool name (e.g. `"tdb_"`).
//! - [`RawToolsConfig::without`] — omit individual tools by their default name.
//!
//! Implementation note: tool route handlers are written as free generic
//! `fn`s (not closures) so that `rmcp`'s `CallToolHandler` HRTB bound
//! (`for<'a> FnOnce(&'a S, P) -> Pin<Box<dyn Future + Send + 'a>>`)
//! resolves cleanly — closures can't express that HRTB on their input
//! borrows.

use std::{borrow::Cow, collections::HashSet, future::Future, pin::Pin, time::Instant};

use rmcp::{
    ErrorData as McpError,
    handler::server::{
        common::schema_for_type, router::tool::ToolRouter, tool::ToolRoute, wrapper::Parameters,
    },
    model::{CallToolResult, Tool},
    schemars,
};
use serde::Deserialize;

use crate::{
    core::{HasTypeDbCore, TypeDbCore, stash_open_tx},
    envelope::{
        NextMoves, envelope_err, envelope_ok, envelope_state_error, explain_query_error, next_moves,
    },
    error::{ErrorClass, InternalError},
    language_reference::{
        TYPEQL_LANGUAGE_REFERENCE, TYPEQL_LANGUAGE_REFERENCE_SHA256,
        TYPEQL_LANGUAGE_REFERENCE_SOURCE,
    },
    session::SessionStore,
    typedb::{TxKind, query_answer_to_json},
};

// ---------- public params -------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct StartSessionParams {}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SessionOnlyParams {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SessionAndDatabaseParams {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
    /// Name of the TypeDB database to use for this operation.
    pub database: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SessionAndQueryParams {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
    /// TypeQL query to execute against the currently-open transaction.
    pub query: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ReadOnceParams {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
    /// Name of the TypeDB database to read. You must call `get_schema` for it first.
    pub database: String,
    /// TypeQL read query to run in a managed read transaction.
    pub query: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CreateDatabaseParams {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
    /// Name of the TypeDB database to create.
    pub database: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DeleteDatabaseParams {
    /// Server-issued session identifier. Obtain one from `start_session`.
    pub session_id: String,
    /// Name of the TypeDB database to permanently delete.
    pub database: String,
    /// Must exactly equal `database` to confirm permanent deletion.
    pub confirm_database: String,
}

// ---------- canonical tool descriptions ----------------------------------

const DESC_START_SESSION: &str = "\
Mint a new server-side session and return its `session_id`. Every other \
tool in this server requires `session_id` as an argument — call this \
first. The response also includes the list of databases on the server \
(same content as `list_databases`) so you can pick one without a \
follow-up call, plus a verbatim bundled TypeQL language reference. The \
live database schema and this server's transaction lifecycle instructions \
take precedence over the general reference. Sessions expire after a \
configured period of inactivity (default 60 minutes); on `SESSION_EXPIRED` \
or `SESSION_UNKNOWN` from any other tool, call `start_session` again for a \
fresh ID.";

const DESC_LIST_DATABASES: &str = "\
List all databases available on the TypeDB server. Read-only; safe to call \
without restriction (other than needing a valid `session_id`).";

const DESC_GET_SCHEMA: &str = "\
Return the complete TypeQL `define` source for a database. You MUST call \
this before opening any transaction on the database; the safety layer blocks \
`open_read`, `open_write`, `open_schema`, and `read_once` until you do. \
TypeQL 3.x differs materially from 2.x — do not write queries from prior \
assumptions; treat this schema as ground truth.";

const DESC_OPEN_READ: &str = "\
Open a READ transaction on the named database. Read-only; commits are not \
permitted (use `rollback` to close, or just leave it for the idle reaper). \
Requires that `get_schema(database)` was called earlier in this session.";

const DESC_OPEN_WRITE: &str = "\
Open a WRITE transaction on the named database. Writes only persist on \
`commit`; use `rollback` to discard. Requires prior `get_schema(database)`. \
Constraint violations that reach TypeDB's write pipeline ABORT the \
transaction — you must then open a new one to continue.";

const DESC_OPEN_SCHEMA: &str = "\
Open a SCHEMA transaction on the named database. SCHEMA changes are \
DESTRUCTIVE and only persist on `commit`. Requires prior \
`get_schema(database)`. On successful commit of a schema transaction, the \
schema-read gate is reset for this database — you must call `get_schema` \
again before opening further transactions.";

const DESC_QUERY: &str = "\
Execute a TypeQL query against the currently-open transaction. The required \
transaction kind is determined by the open transaction (set at `open_*` \
time). Parse, type, and wrong-tx-type errors leave the transaction OPEN — \
fix the query and retry. Write-pipeline errors close the transaction; you \
must open a new one. Results are capped (see config); paginate with \
`sort $k; offset N; limit M;` — `offset` MUST come before `limit`.";

const DESC_COMMIT: &str = "\
Commit the currently-open transaction. Only valid for WRITE and SCHEMA \
transactions (READ transactions cannot be committed; use `rollback`). On \
successful commit of a SCHEMA transaction, the schema-read gate is reset \
for the affected database.";

const DESC_ROLLBACK: &str = "\
Roll back (discard) the currently-open transaction. Forgiving: if no \
transaction is open, this is a no-op success.";

const DESC_READ_ONCE: &str = "\
Run a single read query against the named database in a managed \
read-and-close transaction. Convenience for one-shot reads — the transaction \
is opened, the query is run, and the transaction is closed atomically. \
Requires prior `get_schema(database)`. Result is capped (see config); \
paginate with `sort $k; offset N; limit M;`.";

const DESC_CREATE_DATABASE: &str = "\
Create a database on the TypeDB server. This admin tool is disabled unless \
the operator explicitly enables database admin tools in server config. \
Requires a valid `session_id` and fails if this session has an open \
transaction. After creation, call `list_databases`, then `get_schema`, and \
use `open_schema` if you intend to define schema.";

const DESC_DELETE_DATABASE: &str = "\
Permanently delete a TypeDB database, including all schema and data. This \
destructive admin tool is disabled by default and only appears when the \
operator explicitly enables database admin tools. Requires a valid \
`session_id`, rejects while transactions are open on the target database, \
and requires `confirm_database` to exactly equal `database`.";

// ---------- canonical names ----------------------------------------------

pub mod names {
    pub const START_SESSION: &str = "start_session";
    pub const LIST_DATABASES: &str = "list_databases";
    pub const GET_SCHEMA: &str = "get_schema";
    pub const OPEN_READ: &str = "open_read";
    pub const OPEN_WRITE: &str = "open_write";
    pub const OPEN_SCHEMA: &str = "open_schema";
    pub const QUERY: &str = "query";
    pub const COMMIT: &str = "commit";
    pub const ROLLBACK: &str = "rollback";
    pub const READ_ONCE: &str = "read_once";
    pub const CREATE_DATABASE: &str = "create_database";
    pub const DELETE_DATABASE: &str = "delete_database";

    pub const ADMIN_ALL: &[&str] = &[CREATE_DATABASE, DELETE_DATABASE];

    pub const ALL: &[&str] = &[
        START_SESSION,
        LIST_DATABASES,
        GET_SCHEMA,
        OPEN_READ,
        OPEN_WRITE,
        OPEN_SCHEMA,
        QUERY,
        COMMIT,
        ROLLBACK,
        READ_ONCE,
    ];
}

// ---------- config -------------------------------------------------------

/// Composition knobs for the raw tool router.
#[derive(Debug, Clone, Default)]
pub struct RawToolsConfig {
    prefix: Option<String>,
    omit: HashSet<String>,
    include_database_admin: bool,
}

impl RawToolsConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prepend `prefix` to every raw tool name (e.g. `"tdb_"` → `tdb_query`).
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Include optional database-admin tools. These are omitted by default
    /// because `delete_database` is destructive.
    pub fn with_database_admin_tools(mut self, enabled: bool) -> Self {
        self.include_database_admin = enabled;
        self
    }

    /// Omit the named tool(s) from the router. Names use the canonical
    /// form from [`names`] — omits apply *before* prefixing.
    pub fn without<I, S>(mut self, names_to_omit: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.omit.extend(names_to_omit.into_iter().map(Into::into));
        self
    }

    fn resolve_name(&self, canonical: &'static str) -> Option<Cow<'static, str>> {
        if self.omit.contains(canonical) {
            return None;
        }
        Some(match &self.prefix {
            None => Cow::Borrowed(canonical),
            Some(p) => Cow::Owned(format!("{p}{canonical}")),
        })
    }
}

// ---------- the router builder -------------------------------------------

/// Build a [`ToolRouter`] carrying the default ten raw TypeDB tools, plus
/// optional database-admin tools when explicitly enabled, generic over any
/// handler type `H: HasTypeDbCore`.
pub fn raw_tools_router<H>(config: RawToolsConfig) -> ToolRouter<H>
where
    H: HasTypeDbCore,
{
    let mut router = ToolRouter::<H>::new();
    if let Some(name) = config.resolve_name(names::START_SESSION) {
        router.add_route(ToolRoute::new(
            tool_with_empty_input(name, DESC_START_SESSION),
            handler_start_session::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::LIST_DATABASES) {
        router.add_route(ToolRoute::new(
            tool_with::<SessionOnlyParams>(name, DESC_LIST_DATABASES),
            handler_list_databases::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::GET_SCHEMA) {
        router.add_route(ToolRoute::new(
            tool_with::<SessionAndDatabaseParams>(name, DESC_GET_SCHEMA),
            handler_get_schema::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::OPEN_READ) {
        router.add_route(ToolRoute::new(
            tool_with::<SessionAndDatabaseParams>(name, DESC_OPEN_READ),
            handler_open_read::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::OPEN_WRITE) {
        router.add_route(ToolRoute::new(
            tool_with::<SessionAndDatabaseParams>(name, DESC_OPEN_WRITE),
            handler_open_write::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::OPEN_SCHEMA) {
        router.add_route(ToolRoute::new(
            tool_with::<SessionAndDatabaseParams>(name, DESC_OPEN_SCHEMA),
            handler_open_schema::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::QUERY) {
        router.add_route(ToolRoute::new(
            tool_with::<SessionAndQueryParams>(name, DESC_QUERY),
            handler_query::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::COMMIT) {
        router.add_route(ToolRoute::new(
            tool_with::<SessionOnlyParams>(name, DESC_COMMIT),
            handler_commit::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::ROLLBACK) {
        router.add_route(ToolRoute::new(
            tool_with::<SessionOnlyParams>(name, DESC_ROLLBACK),
            handler_rollback::<H>,
        ));
    }
    if let Some(name) = config.resolve_name(names::READ_ONCE) {
        router.add_route(ToolRoute::new(
            tool_with::<ReadOnceParams>(name, DESC_READ_ONCE),
            handler_read_once::<H>,
        ));
    }
    if config.include_database_admin {
        if let Some(name) = config.resolve_name(names::CREATE_DATABASE) {
            router.add_route(ToolRoute::new(
                tool_with::<CreateDatabaseParams>(name, DESC_CREATE_DATABASE),
                handler_create_database::<H>,
            ));
        }
        if let Some(name) = config.resolve_name(names::DELETE_DATABASE) {
            router.add_route(ToolRoute::new(
                tool_with::<DeleteDatabaseParams>(name, DESC_DELETE_DATABASE),
                handler_delete_database::<H>,
            ));
        }
    }
    router
}

// ---------- Tool attribute helpers ----------------------------------------

fn tool_with_empty_input(name: Cow<'static, str>, description: &'static str) -> Tool {
    Tool::new(
        name,
        description,
        rmcp::handler::server::common::schema_for_empty_input(),
    )
}

fn tool_with<P>(name: Cow<'static, str>, description: &'static str) -> Tool
where
    P: schemars::JsonSchema + std::any::Any,
{
    Tool::new(name, description, schema_for_type::<Parameters<P>>())
}

// ---------- generic route handlers (free fns, not closures) --------------
//
// rmcp's CallToolHandler<S, AsyncMethodAdapter<...>> requires
// `for<'a> FnOnce(&'a S, P) -> Pin<Box<dyn Future + Send + 'a>>` — an HRTB
// that closures cannot express in their inferred trait impls. Function
// items can, because they're naturally lifetime-polymorphic.

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type ToolFut<'a> = BoxFut<'a, Result<CallToolResult, McpError>>;

fn handler_start_session<H: HasTypeDbCore>(service: &H) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_start_session(&core).await) })
}

fn handler_list_databases<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<SessionOnlyParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_list_databases(&core, p).await) })
}

fn handler_get_schema<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<SessionAndDatabaseParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_get_schema(&core, p).await) })
}

fn handler_open_read<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<SessionAndDatabaseParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_open_any(&core, p, TxKind::Read).await) })
}

fn handler_open_write<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<SessionAndDatabaseParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_open_any(&core, p, TxKind::Write).await) })
}

fn handler_open_schema<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<SessionAndDatabaseParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_open_any(&core, p, TxKind::Schema).await) })
}

fn handler_query<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<SessionAndQueryParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_query(&core, p).await) })
}

fn handler_commit<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<SessionOnlyParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_commit(&core, p).await) })
}

fn handler_rollback<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<SessionOnlyParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_rollback(&core, p).await) })
}

fn handler_read_once<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<ReadOnceParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_read_once(&core, p).await) })
}

fn handler_create_database<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<CreateDatabaseParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_create_database(&core, p).await) })
}

fn handler_delete_database<H: HasTypeDbCore>(
    service: &H,
    Parameters(p): Parameters<DeleteDatabaseParams>,
) -> ToolFut<'_> {
    let core = service.typedb_core().clone();
    Box::pin(async move { Ok(do_delete_database(&core, p).await) })
}

// ---------- tool implementations -----------------------------------------
//
// Plain async functions, callable from the generic route handlers above
// or any other code path that owns an Arc<TypeDbCore>.

async fn do_start_session(core: &TypeDbCore) -> CallToolResult {
    let ttl = core.config.session_ttl();
    let sid = core.sessions.start(ttl).await;
    let expires_in_seconds = ttl.as_secs();
    let arc = match core.sessions.resolve_and_touch(&sid, ttl).await {
        Ok(a) => a,
        Err(_) => {
            return crate::envelope::envelope_state_error_no_session(
                ErrorClass::SessionUnknown,
                "Internal error: just-minted session was not resolvable.",
                next_moves::on_session_unknown(),
            );
        }
    };
    let snap = SessionStore::snapshot_arc(&sid, &arc).await;
    match core.typedb.list_databases().await {
        Ok(names) => {
            let result = serde_json::json!({
                "session_id": sid.0,
                "expires_in_seconds": expires_in_seconds,
                "databases": names.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>(),
                "language_reference": {
                    "content": TYPEQL_LANGUAGE_REFERENCE,
                    "source": TYPEQL_LANGUAGE_REFERENCE_SOURCE,
                    "sha256": TYPEQL_LANGUAGE_REFERENCE_SHA256,
                    "precedence": "The live database schema and typedb-mcp lifecycle instructions take precedence over this general reference.",
                },
            });
            envelope_ok(snap, result, next_moves::after_start_session())
        }
        Err(e) => envelope_err(
            snap,
            e,
            "Session was created, but the database list could not be fetched. \
             Retry via `list_databases` once the upstream is back; your \
             session_id is still valid.",
            next_moves::after_start_session_partial(&sid.0),
        ),
    }
}

async fn do_list_databases(core: &TypeDbCore, p: SessionOnlyParams) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    let snap = session.snapshot().await;
    match core.typedb.list_databases().await {
        Ok(names) => envelope_ok(
            snap,
            serde_json::json!({
                "databases": names.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>(),
            }),
            next_moves::after_list_databases(),
        ),
        Err(e) => envelope_err(
            snap,
            e,
            "Could not list databases.",
            next_moves::on_upstream_unavailable(),
        ),
    }
}

async fn do_create_database(core: &TypeDbCore, p: CreateDatabaseParams) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    if let Err(message) = validate_database_name(&p.database) {
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::Unclassified,
            &message,
            vec![
                "Use a non-empty database name containing only ASCII letters, digits, `_`, or `-`."
                    .into(),
            ],
        );
    }
    let arc = session.arc().clone();
    let state = arc.lock().await;
    if state.tx.is_some() {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::TxAlreadyOpen,
            "A transaction is already open in this session. Close it with `commit` or `rollback` before creating a database.",
            NextMoves::default_for(ErrorClass::TxAlreadyOpen, None).into_inner(),
        );
    }
    drop(state);
    match core.typedb.create_database(&p.database).await {
        Ok(()) => envelope_ok(
            session.snapshot().await,
            serde_json::json!({ "created": true, "database": p.database }),
            next_moves::after_create_database(&p.database),
        ),
        Err(e) => {
            let class = e.to_class();
            envelope_err(
                session.snapshot().await,
                e,
                "Could not create database.",
                next_moves::on_error(class, Some(&p.database)),
            )
        }
    }
}

async fn do_delete_database(core: &TypeDbCore, p: DeleteDatabaseParams) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    if let Err(message) = validate_database_name(&p.database) {
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::Unclassified,
            &message,
            vec![
                "Use a non-empty database name containing only ASCII letters, digits, `_`, or `-`."
                    .into(),
            ],
        );
    }
    if p.confirm_database != p.database {
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::ConfirmationRequired,
            "Database was NOT deleted: `confirm_database` must exactly equal `database`.",
            next_moves::on_error(ErrorClass::ConfirmationRequired, Some(&p.database)),
        );
    }
    let arc = session.arc().clone();
    let state = arc.lock().await;
    if state.tx.is_some() {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::TxAlreadyOpen,
            "A transaction is already open in this session. Close it with `commit` or `rollback` before deleting a database.",
            NextMoves::default_for(ErrorClass::TxAlreadyOpen, None).into_inner(),
        );
    }
    drop(state);
    for (sid, arc) in core.sessions.all_sessions().await {
        let state = arc.lock().await;
        if state
            .tx
            .as_ref()
            .is_some_and(|tx| tx.database == p.database)
        {
            return envelope_state_error(
                session.snapshot().await,
                ErrorClass::TxAlreadyOpen,
                &format!(
                    "Database was NOT deleted: session {} has an open transaction on `{}`. Close that transaction before retrying.",
                    sid.0, p.database
                ),
                NextMoves::default_for(ErrorClass::TxAlreadyOpen, None).into_inner(),
            );
        }
    }
    match core.typedb.delete_database(&p.database).await {
        Ok(()) => {
            let sessions = core.sessions.all_sessions().await;
            for (_sid, arc) in sessions {
                arc.lock().await.schema_seen.remove(&p.database);
            }
            envelope_ok(
                session.snapshot().await,
                serde_json::json!({ "deleted": true, "database": p.database }),
                next_moves::after_delete_database(),
            )
        }
        Err(e) => {
            let class = e.to_class();
            envelope_err(
                session.snapshot().await,
                e,
                "Could not delete database.",
                next_moves::on_error(class, Some(&p.database)),
            )
        }
    }
}

fn validate_database_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Invalid database name: name must not be empty.".into());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("Invalid database name: use only ASCII letters, digits, `_`, or `-`.".into());
    }
    Ok(())
}

async fn do_get_schema(core: &TypeDbCore, p: SessionAndDatabaseParams) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    match core.typedb.get_schema(&p.database).await {
        Ok(schema) => {
            {
                let mut state = session.arc().lock().await;
                state.schema_seen.insert(p.database.clone());
            }
            let snap = session.snapshot().await;
            envelope_ok(
                snap,
                serde_json::json!({ "database": p.database, "schema": schema }),
                next_moves::after_get_schema(&p.database),
            )
        }
        Err(e) => {
            let class = e.to_class();
            let snap = session.snapshot().await;
            envelope_err(
                snap,
                e,
                "Could not fetch schema.",
                next_moves::on_error(class, Some(&p.database)),
            )
        }
    }
}

async fn do_open_any(
    core: &TypeDbCore,
    p: SessionAndDatabaseParams,
    kind: TxKind,
) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    let arc = session.arc().clone();
    let mut state = arc.lock().await;
    if state.tx.is_some() {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::TxAlreadyOpen,
            "A transaction is already open in this session. Commit or rollback \
             it before opening another.",
            NextMoves::default_for(ErrorClass::TxAlreadyOpen, None).into_inner(),
        );
    }
    if !state.schema_seen.contains(&p.database) {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::SchemaNotRead,
            "Cannot open a transaction on this database: schema has not been \
             read this session. Call `get_schema(database)` first.",
            NextMoves::default_for(ErrorClass::SchemaNotRead, Some(&p.database)).into_inner(),
        );
    }
    let transaction = match core.typedb.open_transaction(&p.database, kind).await {
        Ok(t) => t,
        Err(e) => {
            drop(state);
            let class = e.to_class();
            return envelope_err(
                session.snapshot().await,
                e,
                "Could not open transaction.",
                next_moves::on_error(class, Some(&p.database)),
            );
        }
    };
    stash_open_tx(&mut state, p.database.clone(), kind, transaction).await;
    drop(state);
    let snap = session.snapshot().await;
    let moves = match kind {
        TxKind::Read => next_moves::after_open_read(&p.database),
        TxKind::Write => next_moves::after_open_write(&p.database),
        TxKind::Schema => next_moves::after_open_schema(&p.database),
    };
    envelope_ok(snap, serde_json::json!({"opened": true}), moves)
}

async fn do_query(core: &TypeDbCore, p: SessionAndQueryParams) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    let arc = session.arc().clone();
    let mut state = arc.lock().await;
    let Some(tx) = state.tx.as_mut() else {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::NoTxOpen,
            "No transaction is open in this session. Use `open_read` for queries, \
             `open_write` for mutations, `open_schema` for schema changes, or \
             `read_once` for a one-shot read.",
            NextMoves::default_for(ErrorClass::NoTxOpen, None).into_inner(),
        );
    };
    let kind = tx.kind;
    let database = tx.database.clone();
    tx.last_activity = Instant::now();
    let answer_result = tx.transaction.query(&p.query).await;
    let result_cap = core.config.server.result_cap;
    match answer_result {
        Ok(answer) => match query_answer_to_json(answer, result_cap).await {
            Ok(json) => {
                let truncated = json.truncated;
                let value = json.into_value();
                if truncated {
                    drop(state);
                    return envelope_state_error(
                        session.snapshot().await,
                        ErrorClass::ResultLimitExceeded,
                        &format!(
                            "Query returned more than {result_cap} answers (server cap). \
                             The result has been discarded — TypeDB does not paginate \
                             after the fact. Re-issue with `sort $k; offset N; limit M;` \
                             (`offset` MUST come before `limit`)."
                        ),
                        NextMoves::default_for(ErrorClass::ResultLimitExceeded, None).into_inner(),
                    );
                }
                drop(state);
                envelope_ok(
                    session.snapshot().await,
                    value,
                    next_moves::after_query_ok(kind, &database),
                )
            }
            Err(e) => {
                let class = e.to_class();
                if !class.retriable_in_same_tx() {
                    state.tx = None;
                }
                drop(state);
                envelope_err(
                    session.snapshot().await,
                    e,
                    &explain_query_error(class),
                    next_moves::on_error(class, None),
                )
            }
        },
        Err(e) => {
            let internal = InternalError::Driver(e);
            let class = internal.to_class();
            if !class.retriable_in_same_tx() {
                state.tx = None;
            }
            drop(state);
            envelope_err(
                session.snapshot().await,
                internal,
                &explain_query_error(class),
                next_moves::on_error(class, None),
            )
        }
    }
}

async fn do_commit(core: &TypeDbCore, p: SessionOnlyParams) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    let arc = session.arc().clone();
    let mut state = arc.lock().await;
    let Some(tx) = state.tx.as_ref() else {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::NoTxOpen,
            "No transaction is open in this session. There is nothing to commit.",
            NextMoves::default_for(ErrorClass::NoTxOpen, None).into_inner(),
        );
    };
    if matches!(tx.kind, TxKind::Read) {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::TxIsRead,
            "READ transactions cannot be committed (TypeDB will reject this — TSV2). \
             Use `rollback` to close the transaction.",
            NextMoves::default_for(ErrorClass::TxIsRead, None).into_inner(),
        );
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
            envelope_ok(
                session.snapshot().await,
                serde_json::json!({"committed": true}),
                next_moves::after_commit_ok(kind, &database),
            )
        }
        Err(e) => {
            let internal = InternalError::Driver(e);
            let class = internal.to_class();
            drop(state);
            envelope_err(
                session.snapshot().await,
                internal,
                "Commit failed. The transaction has been closed and NO changes were \
                 persisted — including any uncommitted inserts/updates from earlier \
                 queries in this tx. Any concept IIDs in the upstream details below \
                 refer to never-committed state and cannot be looked up in a fresh tx.",
                next_moves::on_error(class, Some(&database)),
            )
        }
    }
}

async fn do_rollback(core: &TypeDbCore, p: SessionOnlyParams) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    let arc = session.arc().clone();
    let mut state = arc.lock().await;
    let Some(tx) = state.tx.take() else {
        drop(state);
        return envelope_ok(
            session.snapshot().await,
            serde_json::json!({
                "rolled_back": false,
                "message": "No transaction was open; nothing to roll back."
            }),
            next_moves::after_rollback_ok(),
        );
    };
    if let Err(e) = tx.release().await {
        tracing::warn!(error = %e, kind = ?tx.kind, "driver tx release returned an error");
    }
    drop(state);
    envelope_ok(
        session.snapshot().await,
        serde_json::json!({"rolled_back": true}),
        next_moves::after_rollback_ok(),
    )
}

async fn do_read_once(core: &TypeDbCore, p: ReadOnceParams) -> CallToolResult {
    let session = match core.resolve(&p.session_id).await {
        Ok(s) => s,
        Err(env) => return env,
    };
    let arc = session.arc().clone();
    let state = arc.lock().await;
    if state.tx.is_some() {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::TxAlreadyOpen,
            "A transaction is already open in this session. `read_once` cannot \
             be used while another transaction is open. Commit or rollback first.",
            NextMoves::default_for(ErrorClass::TxAlreadyOpen, None).into_inner(),
        );
    }
    if !state.schema_seen.contains(&p.database) {
        drop(state);
        return envelope_state_error(
            session.snapshot().await,
            ErrorClass::SchemaNotRead,
            "Cannot read this database: schema has not been read this session. \
             Call `get_schema(database)` first.",
            NextMoves::default_for(ErrorClass::SchemaNotRead, Some(&p.database)).into_inner(),
        );
    }
    let tx = match core
        .typedb
        .open_transaction(&p.database, TxKind::Read)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            drop(state);
            let class = e.to_class();
            return envelope_err(
                session.snapshot().await,
                e,
                "Could not open read transaction.",
                next_moves::on_error(class, Some(&p.database)),
            );
        }
    };
    let result_cap = core.config.server.result_cap;
    // Materialize the answer stream BEFORE closing the transaction. The
    // driver's QueryAnswer is a lazy gRPC stream tied to the live tx;
    // close() tears that stream down synchronously, so draining after
    // close aborts with TSV13 (self-inflicted, not a concurrent conflict).
    let json_result = match tx.query(&p.query).await {
        Ok(answer) => query_answer_to_json(answer, result_cap).await,
        Err(e) => Err(InternalError::Driver(e)),
    };
    if let Err(e) = tx.close().await {
        tracing::warn!(error = %e, "read_once close returned an error");
    }
    drop(state);
    let snap = session.snapshot().await;
    match json_result {
        Ok(json) => {
            if json.truncated {
                envelope_state_error(
                    snap,
                    ErrorClass::ResultLimitExceeded,
                    &format!(
                        "Query returned more than {result_cap} answers (server cap). \
                         Re-issue via `read_once` (or an explicit `open_read`) with \
                         `sort $k; offset N; limit M;` (`offset` MUST come before `limit`)."
                    ),
                    NextMoves::default_for(ErrorClass::ResultLimitExceeded, None).into_inner(),
                )
            } else {
                envelope_ok(snap, json.into_value(), next_moves::after_read_once_ok())
            }
        }
        Err(e) => {
            let class = e.to_class();
            let moves = next_moves::on_error(class, Some(&p.database));
            envelope_err(snap, e, &explain_query_error(class), moves)
        }
    }
}
