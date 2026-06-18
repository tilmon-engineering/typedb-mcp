//! Worked example: an MCP server that combines the typedb-mcp raw tool
//! set with semantic tools of its own, sharing one [`TypeDbCore`].
//!
//! This is the reference DESIGN.md §11.2 ("What a consumer's semantic
//! tool body looks like") in compilable form. It does three things a
//! real consumer would do:
//!
//! 1. Constructs a [`TypeDbCore`] once and stashes it on a handler
//!    struct. Implements [`HasTypeDbCore`] on that struct.
//! 2. Defines its own `#[tool_router]` with two semantic tools:
//!    - `count_entities` — uses [`SessionHandle::with_read_tx`] to count
//!      instances of a named entity type. Demonstrates the safety
//!      helper that gates on `schema_seen`, single-tx invariant,
//!      kind-correct release, and envelope construction.
//!    - `current_focus` / `set_focus` — uses
//!      [`SessionHandle::extensions_mut`] to stash a per-session "focus
//!      entity id" string. Demonstrates the typemap (DESIGN.md §3a).
//! 3. Merges [`tools::raw_tools_router`] into its own
//!    [`rmcp::handler::server::router::tool::ToolRouter`] so the agent
//!    sees both the ten raw tools and the two semantic tools through a
//!    single handler.
//!
//! Run with the same `TYPEDB_MCP_CONFIG` env var the binary uses, e.g.
//! `TYPEDB_MCP_CONFIG=config.local.toml cargo run -p example-semantic-mcp`.
//! Speaks stdio; pair with any MCP client.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;
use typedb_mcp_core::{
    HasTypeDbCore, NextMoves, TypeDbCore, config::Config, error::InternalError, tools,
};

// ---------- per-session extension state ----------------------------------

/// Tiny per-session state the example uses to demonstrate the typemap.
/// In a real consumer this might be "current focus customer", a query
/// cache, accepted-disclaimer flags, etc.
#[derive(Debug, Clone, Default)]
struct FocusState {
    entity_id: Option<String>,
}

// ---------- handler -------------------------------------------------------

#[derive(Clone)]
struct ExampleMcp {
    core: Arc<TypeDbCore>,
    tool_router: ToolRouter<Self>,
}

impl ExampleMcp {
    fn new(core: Arc<TypeDbCore>) -> Self {
        // Mount the ten raw tools generic over `Self`.
        let mut tool_router = tools::raw_tools_router::<Self>(tools::RawToolsConfig::default());
        // Merge our own semantic tools onto the same router. Both halves
        // dispatch into the same `Self` so they can share state.
        tool_router.merge(Self::semantic_router());
        Self { core, tool_router }
    }
}

impl HasTypeDbCore for ExampleMcp {
    fn typedb_core(&self) -> &Arc<TypeDbCore> {
        &self.core
    }
}

// ---------- semantic tool params -----------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CountEntitiesParams {
    session_id: String,
    database: String,
    /// TypeQL entity type label (e.g. `customer`).
    entity_type: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionIdParam {
    session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SetFocusParams {
    session_id: String,
    /// IID or external identifier of the focus entity. Pass `null` /
    /// omit to clear.
    entity_id: Option<String>,
}

// ---------- semantic tools -----------------------------------------------

#[tool_router(router = semantic_router, vis = "pub")]
impl ExampleMcp {
    /// Count instances of an entity type in a database. One-shot read.
    /// Demonstrates `with_read_tx` — the kernel handles the schema-read
    /// gate, single-tx invariant, kind-correct release, error
    /// classification, and envelope construction.
    #[tool(
        description = "Count instances of an entity type in the given database. \
                          Read-only; requires prior `get_schema(database)`."
    )]
    async fn count_entities(
        &self,
        Parameters(p): Parameters<CountEntitiesParams>,
    ) -> Result<CallToolResult, McpError> {
        let session = match self.core.resolve(&p.session_id).await {
            Ok(s) => s,
            Err(env) => return Ok(env),
        };
        // Defend the input minimally — a TypeQL label is a sane subset of
        // ASCII identifiers. Real consumers would do schema-aware checks.
        if !p
            .entity_type
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Ok(session
                .state_err(
                    typedb_mcp_core::ErrorClass::ParseError,
                    "entity_type must be a TypeQL label (ASCII alphanumerics, '_' or '-').",
                    NextMoves::new(["Reissue with a valid entity type label."]),
                )
                .await);
        }
        let typeql = format!("match $x isa {}; reduce $count = count;", p.entity_type);
        let database = p.database.clone();
        let entity_type = p.entity_type.clone();
        let hints = NextMoves::new([
            "Reissue with a different entity_type to count another set.".to_string(),
            "Or call `set_focus(session_id=..., entity_id=...)` to pin a focus entity \
             for this session, then `current_focus(session_id=...)` to read it back."
                .to_string(),
        ]);
        Ok(session
            .with_read_tx(&database, hints, async |tx| {
                let answer = tx.query(&typeql).await.map_err(InternalError::Driver)?;
                let json = typedb_mcp_core::query_answer_to_json(answer, 10).await?;
                Ok(serde_json::json!({
                    "entity_type": entity_type,
                    "database": database,
                    "raw_answer": json.into_value(),
                }))
            })
            .await)
    }

    /// Read the current per-session focus entity (or `null`).
    /// Demonstrates `extensions()` reading the typemap.
    #[tool(description = "Return the focus entity stashed in this session, if any.")]
    async fn current_focus(
        &self,
        Parameters(p): Parameters<SessionIdParam>,
    ) -> Result<CallToolResult, McpError> {
        let session = match self.core.resolve(&p.session_id).await {
            Ok(s) => s,
            Err(env) => return Ok(env),
        };
        let focus = session
            .extensions(|ext| ext.get::<FocusState>().cloned().unwrap_or_default())
            .await;
        Ok(session
            .ok(
                serde_json::json!({ "entity_id": focus.entity_id }),
                NextMoves::new([
                    "Update with `set_focus(session_id=..., entity_id=...)`.",
                    "Use `count_entities(...)` to learn what's in a database.",
                ]),
            )
            .await)
    }

    /// Set or clear the per-session focus entity. Demonstrates
    /// `extensions_mut()` writing the typemap.
    #[tool(description = "Set the focus entity for this session, or clear it by passing null.")]
    async fn set_focus(
        &self,
        Parameters(p): Parameters<SetFocusParams>,
    ) -> Result<CallToolResult, McpError> {
        let session = match self.core.resolve(&p.session_id).await {
            Ok(s) => s,
            Err(env) => return Ok(env),
        };
        let entity_id = p.entity_id.clone();
        session
            .extensions_mut(|ext| {
                let slot = ext.get_or_insert_with(FocusState::default);
                slot.entity_id = entity_id;
            })
            .await;
        Ok(session
            .ok(
                serde_json::json!({ "entity_id": p.entity_id }),
                NextMoves::new(["Read back with `current_focus(session_id=...)`."]),
            )
            .await)
    }
}

// ---------- ServerHandler ------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ExampleMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "Example MCP server demonstrating typedb-mcp-core library use. \
             Exposes the standard ten typedb tools (start_session, get_schema, \
             open_*, query, commit, rollback, read_once, list_databases) plus \
             three semantic tools: count_entities, current_focus, set_focus. \
             Call `start_session` first."
                .to_owned(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

// ---------- entry point --------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config_path = std::env::var("TYPEDB_MCP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));
    let config = Arc::new(Config::load_from_path(&config_path)?);

    let core = TypeDbCore::connect(config).await?;
    let _reaper = core.spawn_reaper();

    let handler = ExampleMcp::new(core);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
