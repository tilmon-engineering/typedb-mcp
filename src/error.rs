//! Error classes returned to the agent. See DESIGN.md §5.
//!
//! This module is the single place where TypeDB's error-code stacks are
//! mapped onto the agent-facing classes defined in the design. Any change
//! to the table in DESIGN.md §5 must be reflected here, and vice versa.

use serde::Serialize;

/// Agent-facing error class. Each variant carries the lifecycle implication
/// directly — see [`ErrorClass::retriable_in_same_tx`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorClass {
    /// A tool other than `start_session` was called with a `session_id`
    /// the server does not recognize. Means the agent either fabricated
    /// an ID, used one from a different process lifetime, or hit a
    /// `SessionExpired` and dropped that signal. Recovery: call
    /// `start_session` for a fresh ID.
    SessionUnknown,
    /// A tool was called with a known `session_id` whose TTL has passed.
    /// The session has been purged (along with any tx it held). Recovery:
    /// call `start_session` for a fresh ID.
    SessionExpired,
    /// `open_*` / `read_once` called before `get_schema` for the database.
    SchemaNotRead,
    /// A second `open_*` issued while a transaction is already open.
    TxAlreadyOpen,
    /// `query` / `commit` issued with no transaction open.
    NoTxOpen,
    /// `commit` on a read tx (maps `TSV2`).
    TxIsRead,
    /// Wrong transaction kind for the query (maps `TSV9` / `TSV8`).
    /// **Recoverable**: tx is still open.
    WrongTxType,
    /// TypeQL syntax failure (maps `TQL03 → TSV7`).
    /// **Recoverable**: tx is still open.
    ParseError,
    /// Type inference / unknown label (maps `INF2 → QUA1 → QEX8 → TSV11`).
    /// **Recoverable**: tx is still open.
    TypeError,
    /// Write-pipeline failure (presence of `WEX1` / `PEX6` / `QEX14`).
    /// **Fatal**: tx is now gone.
    WriteFailed,
    /// Commit-time failure (`DCT3 → TSV5 → HSR18`).
    /// **Fatal**: tx is now gone.
    CommitFailed,
    /// Server-side result cap hit. **Recoverable**: tx is still open.
    ResultLimitExceeded,
    /// Server-side query timeout. **Fatal**: tx is gone.
    Timeout,
    /// Database not present on the server.
    UnknownDatabase,
    /// Idle reaper closed the tx before this call.
    IdleTimeout,
    /// TypeDB server unreachable / auth failure / generic upstream problem.
    UpstreamUnavailable,
    /// TypeDB returned an error this server does not know how to classify.
    /// Conservatively treated as fatal to any open transaction (we lack
    /// the information to claim otherwise). Most likely cause: a newer
    /// TypeDB release introduced an error code post-dating this
    /// classifier. The full driver message is preserved verbatim in the
    /// envelope's `error.message` and the bracketed codes in
    /// `error.typedb_codes` for human follow-up.
    Unclassified,
}

impl ErrorClass {
    /// Whether a transaction remains open and continuable after this error.
    ///
    /// `true` for both the recoverable-query family (where the query failed
    /// but the tx is intact — fix and retry) AND the no-op-on-existing-tx
    /// family (`TX_IS_READ`, `TX_ALREADY_OPEN` — the failing *call* cannot
    /// be retried as-is, but the underlying tx is alive and you can keep
    /// using it; see the response's `next_moves` for what to do instead).
    ///
    /// `false` when there is no live tx after this error: either none
    /// existed (`NO_TX_OPEN`, `SCHEMA_NOT_READ`, `UNKNOWN_DATABASE` etc.)
    /// or one was torn down by it (`WRITE_FAILED`, `COMMIT_FAILED`,
    /// `TIMEOUT`, `IDLE_TIMEOUT`).
    ///
    /// Maps the "Tx after" column in DESIGN.md §5 to a boolean:
    /// `unchanged` and `open` → `true`; `none`, `gone`, `session-level` → `false`.
    pub fn retriable_in_same_tx(self) -> bool {
        use ErrorClass::*;
        matches!(
            self,
            WrongTxType
                | ParseError
                | TypeError
                | ResultLimitExceeded
                | TxIsRead
                | TxAlreadyOpen
        )
        // SessionUnknown/SessionExpired deliberately false: there IS no
        // session, so by definition no tx is alive.
    }
}

/// Classify a TypeDB error-stack response into an [`ErrorClass`].
///
/// The TypeDB HTTP API returns errors as JSON `{ "code": "...", "message": "..." }`
/// where `message` contains a stack of bracketed codes — e.g.
/// `[CNT6] ... [WEX1] ... [TSV11] [HSR16]`. The gRPC driver renders the
/// same stack via the `Server` error's `Display`.
///
/// **Strategy: structural markers, then specific codes, then family prefixes,
/// then a conservative fallback.** TypeDB has 70+ code prefixes and 300+
/// individual codes (see `DESIGN.md` §5 for the source of truth). We do
/// not enumerate them all — that list drifts every release. Instead we
/// match on the small set of *structural* codes that decide lifecycle
/// outcome (`WEX1`/`PEX6`/`QEX14` mean "write pipeline torn down,"
/// `DCT3`/`TSV5`/`HSR18`/`SRV19` mean "commit failed," etc.) and let
/// whole code families collapse to the same agent-facing class
/// (`[INF*` and `[QUA*` are all type/inference issues → `TypeError`;
/// `[TQL*` is all TypeQL parse → `ParseError`; `[CNT*` and `[DVL*` are
/// constraint/validation failures → `WriteFailed`).
///
/// Anything we cannot map ends in `Unclassified` — *not*
/// `UpstreamUnavailable`. The latter promises "retry once, TypeDB was
/// unreachable," which would be a lie for unrecognized codes from a new
/// TypeDB release. `Unclassified` carries the verbatim driver message
/// to the agent and tells it the transaction is gone.
pub fn classify_typedb_error(top_code: &str, message: &str) -> ErrorClass {
    // 1. Structural markers — these decide lifecycle.
    //    Order matters: a constraint violation (CNT*) ALSO carries WEX1 in
    //    the stack on a write-pipeline failure, so the write-pipeline marker
    //    must win first.
    if message.contains("[WEX1]") || message.contains("[PEX6]") || message.contains("[QEX14]") {
        return ErrorClass::WriteFailed;
    }
    if message.contains("[DCT3]")
        || message.contains("[TSV5]")
        || message.contains("[HSR18]")
        || message.contains("[SRV19]")
    {
        return ErrorClass::CommitFailed;
    }

    // 2. Specific TSV codes (heterogeneous family — must be matched
    //    individually, not as `[TSV*` prefix).
    if message.contains("[TSV12]") {
        // "Operation failed: no open transaction." Means the tx was reaped
        // or aborted between calls; the *session* still exists.
        return ErrorClass::IdleTimeout;
    }
    if message.contains("[TSV9]") || message.contains("[TSV8]") {
        return ErrorClass::WrongTxType;
    }
    if message.contains("[TSV2]") {
        return ErrorClass::TxIsRead;
    }
    if message.contains("[TSV7]") {
        return ErrorClass::ParseError;
    }

    // 3. Family prefixes — whole code families with one agent-facing class.
    //    Must come after specific codes above, before the fallback below.
    if message.contains("[TQL") {
        return ErrorClass::ParseError;
    }
    if message.contains("[INF") || message.contains("[QUA") {
        return ErrorClass::TypeError;
    }
    if message.contains("[CNT") || message.contains("[DVL") {
        // Constraint/validation failure that didn't fire the WEX/DCT markers
        // above. Treat as fatal: a constraint or validator returned a no on
        // some write/define action.
        return ErrorClass::WriteFailed;
    }

    // 4. Fallback by top code for things that don't show in stack markers.
    match top_code {
        "TXN1" | "TXN2" => ErrorClass::Timeout,
        "SRV3" => ErrorClass::UnknownDatabase,
        "AUT2" | "HSR9" => ErrorClass::UpstreamUnavailable,
        _ => ErrorClass::Unclassified,
    }
}

/// Internal error type used inside this crate. Distinct from the
/// agent-facing [`ErrorClass`]: operational failures from the driver bubble
/// up here, and we map them to [`ErrorClass`] at the agent boundary.
#[derive(Debug, thiserror::Error)]
pub enum InternalError {
    /// Any error from the TypeDB driver — server-returned errors, connection
    /// errors, etc. The classifier in [`classify_driver_error`] inspects
    /// the structure to decide on agent-facing class.
    #[error("TypeDB driver error: {0}")]
    Driver(#[from] typedb_driver::Error),

    #[error("config error: {0}")]
    Config(String),
}

impl InternalError {
    /// Map this internal error to an agent-facing error class.
    pub fn to_class(&self) -> ErrorClass {
        match self {
            InternalError::Driver(e) => classify_driver_error(e),
            InternalError::Config(_) => ErrorClass::UpstreamUnavailable,
        }
    }
}

/// Classify a driver error. We rely on:
///
/// 1. `Error::Connection(ConnectionError::TransactionIsClosed | …)` →
///    [`ErrorClass::IdleTimeout`] (the tx is gone, agent must reopen).
/// 2. `Error::Server(ServerError)` whose `Display` joins the bracketed
///    code stack — we run the existing substring classifier over that
///    string.
/// 3. Everything else → [`ErrorClass::UpstreamUnavailable`].
pub fn classify_driver_error(e: &typedb_driver::Error) -> ErrorClass {
    use typedb_driver::Error::*;
    use typedb_driver::error::ConnectionError;

    match e {
        Connection(
            ConnectionError::TransactionIsClosed
            | ConnectionError::TransactionIsClosedWithErrors { .. },
        ) => ErrorClass::IdleTimeout,
        Connection(_) => ErrorClass::UpstreamUnavailable,
        Server(_) => {
            // ServerError::Display renders the stack as bracketed codes
            // joined by "\nCaused: " — exactly what our classifier eats.
            let top = e.code();
            let msg = e.message();
            classify_typedb_error(&top, &msg)
        }
        // Concept/Migration/Analyze/Internal/Other → treat as upstream.
        _ => ErrorClass::UpstreamUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_pipeline_is_fatal() {
        let c = classify_typedb_error(
            "CNT6",
            "[CNT6] Constraint @regex violated.\n[DVL7] ...\n[COW5] ...\n[WEX1] ...\n[PEX6] ...\n[QEX14] ...\n[TSV11] Query failed.\n[HSR16] Transaction error.",
        );
        assert_eq!(c, ErrorClass::WriteFailed);
        assert!(!c.retriable_in_same_tx());
    }

    #[test]
    fn commit_time_is_fatal() {
        let c = classify_typedb_error(
            "CNT5",
            "[CNT5] Constraint @card violated.\n[DVL10] ...\n[COW5] ...\n[DCT3] Data commit error.\n[TSV5] Data transaction commit failed.\n[HSR18] Error while committing single-query transaction.",
        );
        assert_eq!(c, ErrorClass::CommitFailed);
        assert!(!c.retriable_in_same_tx());
    }

    #[test]
    fn parse_error_is_recoverable() {
        let c = classify_typedb_error(
            "TQL0",
            "[TQL0] [TQL03] TypeQL Error: syntax error.\n[TSV7] Query parsing failed.\n[HSR16] Transaction error.",
        );
        assert_eq!(c, ErrorClass::ParseError);
        assert!(c.retriable_in_same_tx());
    }

    #[test]
    fn type_error_is_recoverable() {
        let c = classify_typedb_error(
            "INF2",
            "[INF2] Type label 'nonexistent' not found.\n[QUA1] ...\n[QEX8] ...\n[TSV11] Query failed.\n[HSR16] Transaction error.",
        );
        assert_eq!(c, ErrorClass::TypeError);
        assert!(c.retriable_in_same_tx());
    }

    #[test]
    fn wrong_tx_type_is_recoverable() {
        let c = classify_typedb_error(
            "TSV9",
            "[TSV9] Data modification queries require either write or schema transactions.\n[HSR16] Transaction error.",
        );
        assert_eq!(c, ErrorClass::WrongTxType);
        assert!(c.retriable_in_same_tx());
    }

    // --- Family-prefix tests ---------------------------------------------

    #[test]
    fn any_inf_code_is_type_error() {
        // INF7 isn't in our specific list; the family prefix must still catch it.
        let c = classify_typedb_error(
            "INF7",
            "[INF7] Some new inference failure introduced in TypeDB 3.12.",
        );
        assert_eq!(c, ErrorClass::TypeError);
        assert!(c.retriable_in_same_tx());
    }

    #[test]
    fn any_qua_code_is_type_error() {
        let c = classify_typedb_error(
            "QUA12",
            "[QUA12] Annotation error: some new annotation failure.",
        );
        assert_eq!(c, ErrorClass::TypeError);
        assert!(c.retriable_in_same_tx());
    }

    #[test]
    fn any_tql_code_is_parse_error() {
        // Even if TypeDB introduces a TQL99 we've never seen, ParseError is right.
        let c = classify_typedb_error(
            "TQL99",
            "[TQL99] Some hypothetical new TypeQL parse failure.",
        );
        assert_eq!(c, ErrorClass::ParseError);
        assert!(c.retriable_in_same_tx());
    }

    #[test]
    fn cnt_alone_is_write_failed() {
        // CNT* outside the WEX/DCT contexts (e.g. caught by a validator before
        // the write pipeline runs). Still fatal-shaped.
        let c = classify_typedb_error(
            "CNT9",
            "[CNT9] Constraint '@distinct' violated (hypothetical).",
        );
        assert_eq!(c, ErrorClass::WriteFailed);
        assert!(!c.retriable_in_same_tx());
    }

    #[test]
    fn dvl_alone_is_write_failed() {
        let c = classify_typedb_error(
            "DVL3",
            "[DVL3] Data validation failed for some new reason.",
        );
        assert_eq!(c, ErrorClass::WriteFailed);
        assert!(!c.retriable_in_same_tx());
    }

    // --- Specific code additions ----------------------------------------

    #[test]
    fn txn1_is_timeout() {
        let c = classify_typedb_error(
            "TXN1",
            "[TXN1] Transaction exceeded its configured timeout.",
        );
        assert_eq!(c, ErrorClass::Timeout);
        assert!(!c.retriable_in_same_tx());
    }

    #[test]
    fn txn2_is_timeout() {
        let c = classify_typedb_error(
            "TXN2",
            "[TXN2] Write exclusivity acquisition timed out.",
        );
        assert_eq!(c, ErrorClass::Timeout);
        assert!(!c.retriable_in_same_tx());
    }

    // --- Conservative fallback ------------------------------------------

    #[test]
    fn unrecognized_code_is_unclassified_not_upstream() {
        // A code from some future TypeDB release we've never seen.
        // Must NOT classify as UpstreamUnavailable (which would tell the
        // agent to retry once — wrong for a structural error).
        let c = classify_typedb_error(
            "XYZ99",
            "[XYZ99] Some entirely new error category in TypeDB 4.0.",
        );
        assert_eq!(c, ErrorClass::Unclassified);
        assert!(!c.retriable_in_same_tx());
    }

    #[test]
    fn unrecognized_code_with_no_markers_in_message_is_unclassified() {
        // No structural marker, no family prefix, no fallback top-code match.
        let c = classify_typedb_error("???", "Some bare error string with no [BRACKETED] codes.");
        assert_eq!(c, ErrorClass::Unclassified);
    }

    // --- Order-sensitivity regression -----------------------------------

    #[test]
    fn cnt_inside_write_pipeline_stack_is_write_failed_not_just_cnt() {
        // WEX1 in the stack must win over the bare [CNT family check;
        // the agent advice is the same (WriteFailed both ways) but the
        // classifier is making the correct discrimination.
        let c = classify_typedb_error(
            "CNT6",
            "[CNT6] Constraint @regex violated.\n[WEX1] write pipeline failed.",
        );
        assert_eq!(c, ErrorClass::WriteFailed);
    }

    #[test]
    fn cnt_inside_commit_stack_is_commit_failed_not_write_failed() {
        // DCT3/SRV19 in the stack must win over the bare [CNT family check.
        // The discrimination matters: WriteFailed says "your insert failed
        // mid-transaction," CommitFailed says "your insert was fine, the
        // commit caught a violation across the whole tx state."
        let c = classify_typedb_error(
            "CNT5",
            "[CNT5] Constraint @card violated.\n[DCT3] data commit error.\n[SRV19] commit failed.",
        );
        assert_eq!(c, ErrorClass::CommitFailed);
    }

    #[test]
    fn srv19_alone_is_commit_failed() {
        // Defensive: today TypeDB always emits SRV19 alongside DCT3/TSV5.
        // If a future version ever drops the trio but keeps SRV19, we
        // still want to classify it as a commit-time failure rather than
        // falling through to UPSTREAM_UNAVAILABLE.
        let c = classify_typedb_error(
            "CNT5",
            "[CNT5] Constraint violated.\n[SRV19] Data commit failed.",
        );
        assert_eq!(c, ErrorClass::CommitFailed);
        assert!(!c.retriable_in_same_tx());
    }

    #[test]
    fn tx_is_read_keeps_tx_alive() {
        // `commit` on a read tx is caught at the handler before reaching
        // TypeDB, but `retriable_in_same_tx` still has to reflect reality:
        // the tx is open and reusable (for more reads, or rollback).
        assert!(ErrorClass::TxIsRead.retriable_in_same_tx());
    }

    #[test]
    fn tx_already_open_means_original_tx_survives() {
        // The whole point of TX_ALREADY_OPEN is that the existing tx was
        // untouched — the failing `open_*` never replaced it.
        assert!(ErrorClass::TxAlreadyOpen.retriable_in_same_tx());
    }

    #[test]
    fn srv3_means_unknown_database() {
        let c = classify_typedb_error(
            "SRV3",
            "[SRV3] Database 'definitely_not_a_real_db_lol' not found.",
        );
        assert_eq!(c, ErrorClass::UnknownDatabase);
        assert!(!c.retriable_in_same_tx());
    }

    #[test]
    fn tsv12_means_tx_already_gone() {
        let c = classify_typedb_error(
            "TSV12",
            "[TSV12] Operation failed: no open transaction.\n[HSR16] Transaction error.",
        );
        assert_eq!(c, ErrorClass::IdleTimeout);
    }
}
