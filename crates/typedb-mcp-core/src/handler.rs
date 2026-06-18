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
//! The default ten raw tools, plus explicitly enabled optional admin
//! tools, live in [`crate::tools`] — see DESIGN.md §11 for the library
//! extension API.

use std::sync::Arc;

use rmcp::{
    RoleServer, ServerHandler,
    handler::server::router::{prompt::PromptRouter, tool::ToolRouter},
    model::{
        GetPromptRequestParams, GetPromptResult, ListPromptsResult, PaginatedRequestParams,
        PromptMessage, PromptMessageRole, ServerCapabilities, ServerInfo,
    },
    prompt, prompt_handler, prompt_router,
    service::RequestContext,
    tool_handler,
};

use crate::{
    config::Config,
    core::{HasTypeDbCore, TypeDbCore},
    language_reference::TYPEQL_LANGUAGE_REFERENCE,
    session::SessionStore,
    tools::{RawToolsConfig, raw_tools_router},
    typedb::TypeDbClient,
};

#[derive(Clone)]
pub struct TypeDbMcp {
    pub core: Arc<TypeDbCore>,
    pub tool_router: ToolRouter<Self>,
    pub prompt_router: PromptRouter<Self>,
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
        let raw_cfg = RawToolsConfig::default()
            .with_database_admin_tools(core.config.server.enable_database_admin_tools);
        let tool_router = raw_tools_router::<Self>(raw_cfg);
        let prompt_router = Self::prompt_router();
        Self {
            core,
            tool_router,
            prompt_router,
        }
    }
}

#[prompt_router]
impl TypeDbMcp {
    /// Return the bundled TypeQL language reference verbatim.
    #[prompt(
        name = "typeql-language-reference",
        description = "Return the bundled TypeQL language reference verbatim. The live database schema and typedb-mcp lifecycle instructions take precedence."
    )]
    async fn typeql_language_reference(&self) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            TYPEQL_LANGUAGE_REFERENCE,
        )]
    }
}

impl HasTypeDbCore for TypeDbMcp {
    fn typedb_core(&self) -> &Arc<TypeDbCore> {
        &self.core
    }
}

#[prompt_handler(router = self.prompt_router)]
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
             for the full state machine. `start_session` returns a verbatim \
             bundled TypeQL language reference; prompt-capable clients can \
             also request `typeql-language-reference`. The live schema and \
             these lifecycle instructions take precedence over the general \
             language reference."
                .to_owned(),
        );
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .build();
        info
    }
}
