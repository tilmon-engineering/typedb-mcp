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
}

impl ErrorClass {
    /// Whether the open transaction (if any) survives this error.
    /// Maps the "Tx survives?" column in DESIGN.md §5.
    pub fn retriable_in_same_tx(self) -> bool {
        use ErrorClass::*;
        matches!(
            self,
            WrongTxType | ParseError | TypeError | ResultLimitExceeded
        )
    }
}

/// Classify a TypeDB error-stack response into an [`ErrorClass`].
///
/// The TypeDB HTTP API returns errors as JSON `{ "code": "...", "message": "..." }`
/// where `message` contains a stack of bracketed codes — e.g.
/// `[CNT6] ... [WEX1] ... [TSV11] [HSR16]`.
pub fn classify_typedb_error(top_code: &str, message: &str) -> ErrorClass {
    // Order matters: write-pipeline before parse/compile, because a constraint
    // violation also carries `TSV11`, but its presence of `WEX1` / `PEX6` /
    // `QEX14` is the discriminator (see DESIGN.md §5).
    if message.contains("[WEX1]") || message.contains("[PEX6]") || message.contains("[QEX14]") {
        return ErrorClass::WriteFailed;
    }
    if message.contains("[DCT3]") || message.contains("[TSV5]") || message.contains("[HSR18]") {
        return ErrorClass::CommitFailed;
    }
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
    if message.contains("[TQL03]") || message.contains("[TSV7]") {
        return ErrorClass::ParseError;
    }
    if message.contains("[INF2]") || message.contains("[QUA1]") || message.contains("[QEX8]") {
        return ErrorClass::TypeError;
    }

    // Fallback by top code if message inspection missed something.
    match top_code {
        "CNT5" | "CNT6" | "CNT7" | "CNT8" => ErrorClass::WriteFailed,
        "TSV9" | "TSV8" => ErrorClass::WrongTxType,
        "TSV2" => ErrorClass::TxIsRead,
        "TSV12" => ErrorClass::IdleTimeout,
        "TQL0" => ErrorClass::ParseError,
        "INF2" => ErrorClass::TypeError,
        "AUT2" | "HSR9" => ErrorClass::UpstreamUnavailable,
        _ => ErrorClass::UpstreamUnavailable,
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

    #[test]
    fn tsv12_means_tx_already_gone() {
        let c = classify_typedb_error(
            "TSV12",
            "[TSV12] Operation failed: no open transaction.\n[HSR16] Transaction error.",
        );
        assert_eq!(c, ErrorClass::IdleTimeout);
    }
}
