//! Public agent-facing response envelope. See DESIGN.md §6 and §6.1.
//!
//! Every tool response — raw or consumer-defined — must carry the same
//! outer shape: a `session` block of observed state, a `next_moves` list of
//! one-line usage hints, and exactly one of `result` (success) or `error`
//! (failure). The constructors in this module are the only sanctioned way
//! to emit that shape; library consumers should not hand-build
//! [`CallToolResult`] envelopes.
//!
//! The envelope's JSON shape is tagged with [`ENVELOPE_VERSION`]. Breaking
//! changes require a version bump and a DESIGN.md edit.

use rmcp::model::{CallToolResult, Content};
use serde::Serialize;

use crate::error::{ErrorClass, InternalError};
use crate::session::SessionSnapshot;

/// Envelope JSON-shape version. See DESIGN.md §6.1.
pub const ENVELOPE_VERSION: u32 = 1;

/// Builder for the `next_moves` block — the in-band lifecycle teacher.
///
/// Build one with [`NextMoves::default_for`] (canonical lines for a state
/// or error class) and extend with tool-specific hints. The catalogue of
/// canonical lines lives in [`next_moves`]; consumers should prefer to
/// extend canonical lines rather than write freeform prose, so the
/// agent's reading register stays uniform across raw and semantic tools.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub struct NextMoves(Vec<String>);

impl NextMoves {
    pub fn empty() -> Self { Self::default() }

    pub fn new<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(lines.into_iter().map(Into::into).collect())
    }

    pub fn push(mut self, line: impl Into<String>) -> Self {
        self.0.push(line.into());
        self
    }

    pub fn extended<I, S>(mut self, lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.0.extend(lines.into_iter().map(Into::into));
        self
    }

    /// Canonical lines for an [`ErrorClass`]. Mirrors the per-class
    /// guidance in DESIGN.md §5 and the project's CLAUDE.md catalogue.
    pub fn default_for(class: ErrorClass, db_hint: Option<&str>) -> Self {
        Self::new(next_moves::on_error(class, db_hint))
    }

    pub fn as_slice(&self) -> &[String] { &self.0 }
    pub fn into_inner(self) -> Vec<String> { self.0 }
}

impl From<NextMoves> for Vec<String> {
    fn from(m: NextMoves) -> Self { m.0 }
}

impl From<Vec<String>> for NextMoves {
    fn from(v: Vec<String>) -> Self { Self(v) }
}

/// Top-level envelope serialized into every tool response.
#[derive(Debug, Serialize)]
pub struct AgentEnvelope<'a> {
    pub session: &'a SessionSnapshot,
    pub next_moves: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

/// Error block; populated when `result` is absent.
#[derive(Debug, Serialize)]
pub struct ErrorPayload {
    pub class: ErrorClass,
    pub message: String,
    pub retriable_in_same_tx: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typedb_codes: Option<Vec<String>>,
}

/// Build a success envelope. The canonical constructor for "tool ran
/// cleanly; here's the agent-facing result."
pub fn envelope_ok(
    session: SessionSnapshot,
    result: serde_json::Value,
    next_moves: impl Into<Vec<String>>,
) -> CallToolResult {
    let env = AgentEnvelope {
        session: &session,
        next_moves: next_moves.into(),
        result: Some(result),
        error: None,
    };
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&env).expect("envelope serialization"),
    )])
}

/// Build an error envelope from an [`InternalError`]. Used when the
/// failure came from the TypeDB driver or the kernel itself; the verbatim
/// driver message is preserved in `error.message` and the bracketed codes
/// are extracted into `error.typedb_codes`.
pub fn envelope_err(
    session: SessionSnapshot,
    err: InternalError,
    explanation: &str,
    next_moves: impl Into<Vec<String>>,
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
        next_moves: next_moves.into(),
        result: None,
        error: Some(payload),
    };
    CallToolResult::error(vec![Content::text(
        serde_json::to_string_pretty(&env).expect("envelope serialization"),
    )])
}

/// Build a state-error envelope where the failure is a kernel-level
/// state invariant (no live transaction, schema not read, session
/// unknown/expired, etc.) and there is no upstream message to embed.
pub fn envelope_state_error(
    session: SessionSnapshot,
    class: ErrorClass,
    message: &str,
    next_moves: impl Into<Vec<String>>,
) -> CallToolResult {
    let payload = ErrorPayload {
        class,
        message: message.to_owned(),
        retriable_in_same_tx: class.retriable_in_same_tx(),
        typedb_codes: None,
    };
    let env = AgentEnvelope {
        session: &session,
        next_moves: next_moves.into(),
        result: None,
        error: Some(payload),
    };
    CallToolResult::error(vec![Content::text(
        serde_json::to_string_pretty(&env).expect("envelope serialization"),
    )])
}

/// Variant for SESSION_UNKNOWN / SESSION_EXPIRED where no session
/// was resolved and therefore no snapshot exists.
pub fn envelope_state_error_no_session(
    class: ErrorClass,
    message: &str,
    next_moves: impl Into<Vec<String>>,
) -> CallToolResult {
    envelope_state_error(SessionSnapshot::default(), class, message, next_moves)
}

/// Render a short lifecycle-aware explanation for a query/commit error
/// class. Used by the raw `query` and `read_once` tools and re-exported
/// here so consumer tools that build query envelopes themselves can
/// reuse the canonical wording.
pub fn explain_query_error(class: ErrorClass) -> String {
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

/// Pull the bracketed `[CODE]` markers out of a TypeDB driver message
/// stack. The codes are uppercase alphanumerics in square brackets;
/// anything else in brackets is ignored.
pub fn extract_codes(message: &str) -> Vec<String> {
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

// =========================================================================
// next_moves catalogue
//
// One-line lifecycle reminders, keyed by tool/state. See CLAUDE.md and
// DESIGN.md §6 for the rationale. Public so library consumers can reuse
// the canonical phrasing in their own semantic tools — recommended over
// hand-written prose, to keep the agent's reading register uniform.
// =========================================================================

pub mod next_moves {
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
                "The transaction has been ABORTED by TypeDB. Every write you had \
                 already submitted in it is also DISCARDED — including any prior \
                 `insert` / `delete` / `update` calls that returned success in that \
                 same transaction. None of them reached durable storage. Open a new \
                 transaction (typically `open_write(session_id=..., database=...)`) \
                 and re-apply every one of those prior writes in it before issuing \
                 your replacement for the failing write.".into(),
                "The schema-read gate is still valid (no schema change happened), so \
                 you do NOT need to call `get_schema` again before the next \
                 `open_*`. This is unrelated to your *data* writes, which are gone.".into(),
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
            TransientConflict => vec![
                "TypeDB aborted this call because of a concurrent transaction \
                 rollback (TSV13). This is a benign transient — most commonly seen \
                 on the first call issued right after a successful `commit`. No \
                 state was damaged; simply retry the failing call as-is. If the \
                 same call fails this way more than a couple of times in a row, \
                 escalate (it is no longer a transient).".into(),
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn write_failed_next_moves_warn_prior_writes_are_discarded() {
            let moves = on_error(ErrorClass::WriteFailed, None);
            let joined = moves.join(" ").to_lowercase();
            assert!(
                joined.contains("discarded") || joined.contains("not reached"),
                "WriteFailed next_moves must explicitly state prior writes are \
                 discarded; got {moves:?}"
            );
            assert!(
                joined.contains("re-apply") || joined.contains("reapply"),
                "WriteFailed next_moves must direct the agent to re-apply prior \
                 writes; got {moves:?}"
            );
        }

        #[test]
        fn transient_conflict_next_moves_direct_retry() {
            let moves = on_error(ErrorClass::TransientConflict, None);
            let joined = moves.join(" ").to_lowercase();
            assert!(
                joined.contains("retry"),
                "TransientConflict next_moves must tell the agent to retry; \
                 got {moves:?}"
            );
            assert!(
                joined.contains("tsv13") || joined.contains("transient"),
                "TransientConflict next_moves should name the cause so the \
                 operator can recognize it on recurrence; got {moves:?}"
            );
        }
    }
}
