//! typedb-mcp-core — library kernel. See DESIGN.md §11.
//!
//! Two kinds of consumer:
//!
//! 1. **The reference binary** (`typedb-mcp`): mounts the eleven default raw
//!    tools via the [`tools`] router, plus optional database-admin tools
//!    only when explicitly enabled by operator config.
//! 2. **Other MCP servers** that want to embed TypeDB safety semantics
//!    alongside semantic tools of their own. They construct a
//!    [`TypeDbCore`], implement [`HasTypeDbCore`] on their handler
//!    struct, and either merge the raw [`tools::raw_tools_router`]
//!    into their own [`rmcp::handler::server::router::tool::ToolRouter`]
//!    or omit it entirely.
//!
//! Either way, every TypeDB-backed tool — raw or semantic — must go
//! through the kernel's transaction helpers ([`SessionHandle::with_read_tx`]
//! etc.) so the safety invariants in DESIGN.md §3 hold uniformly.

pub mod config;
pub mod core;
pub mod envelope;
pub mod error;
pub mod extensions;
pub mod handler;
pub mod language_reference;
pub mod session;
pub mod tools;
pub mod typedb;

pub use crate::core::{HasTypeDbCore, SessionHandle, TxOutcome, TypeDbCore};
pub use crate::envelope::{
    AgentEnvelope, ENVELOPE_VERSION, ErrorPayload, NextMoves, envelope_err, envelope_ok,
    envelope_state_error, envelope_state_error_no_session, explain_query_error, extract_codes,
    next_moves,
};
pub use crate::error::{ErrorClass, InternalError, classify_driver_error, classify_typedb_error};
pub use crate::extensions::Extensions;
pub use crate::language_reference::{
    TYPEQL_LANGUAGE_REFERENCE, TYPEQL_LANGUAGE_REFERENCE_SHA256, TYPEQL_LANGUAGE_REFERENCE_SOURCE,
};
pub use crate::session::{
    OpenTx, OpenTxView, SessionId, SessionResolveError, SessionSnapshot, SessionState,
    SessionStore, TxState, run_reaper,
};
pub use crate::typedb::{
    DriverTransaction, QueryAnswerJson, TxKind, TypeDbClient, query_answer_to_json,
};
