//! Reference [`ServerHandler`] used by the binary in `crates/typedb-mcp`.
//!
//! This is now a thin wrapper around [`TypeDbCore`] and the generic raw
//! tool router from [`crate::tools`]. It exists to:
//!
//! 1. Keep the binary's wiring stable (same `TypeDbMcp::new(config,
//!    typedb, sessions)` constructor the binary and the in-process
//!    tests use).
//! 2. Serve as a worked example of a `HasTypeDbCore` implementation
//!    that library consumers can crib from.
//!
//! All ten raw tools live in [`crate::tools`] — see DESIGN.md §11 for
//! the library extension API.

use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{ServerCapabilities, ServerInfo},
    tool_handler,
};

use crate::{
    config::Config,
    core::{HasTypeDbCore, TypeDbCore},
    session::SessionStore,
    tools::{RawToolsConfig, raw_tools_router},
    typedb::TypeDbClient,
};

#[derive(Clone)]
pub struct TypeDbMcp {
    pub core: Arc<TypeDbCore>,
    pub tool_router: ToolRouter<Self>,
}

impl TypeDbMcp {
    pub fn new(
        config: Arc<Config>,
        typedb: Arc<TypeDbClient>,
        sessions: Arc<SessionStore>,
    ) -> Self {
        let core = TypeDbCore::new(config, typedb, sessions);
        Self::from_core(core)
    }

    /// Construct a `TypeDbMcp` from an already-built kernel. Library
    /// consumers who want the reference handler but already have a
    /// kernel use this; consumers writing their own handler implement
    /// [`HasTypeDbCore`] on it directly.
    pub fn from_core(core: Arc<TypeDbCore>) -> Self {
        let tool_router = raw_tools_router::<Self>(RawToolsConfig::default());
        Self { core, tool_router }
    }
}

impl HasTypeDbCore for TypeDbMcp {
    fn typedb_core(&self) -> &Arc<TypeDbCore> {
        &self.core
    }
}

#[tool_handler(router = self.tool_router)]
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
