//! Bundled TypeQL language reference shipped to MCP clients.

/// Upstream source of the bundled reference.
pub const TYPEQL_LANGUAGE_REFERENCE_SOURCE: &str =
    "https://raw.githubusercontent.com/CaliLuke/skills/refs/heads/main/skills/typedb/SKILL.md";

/// SHA-256 of the vendored upstream bytes.
pub const TYPEQL_LANGUAGE_REFERENCE_SHA256: &str =
    "1923ebda17ab8c7f64810728612fe386d978b1839850f70b7ab0a139201d9722";

/// Verbatim upstream TypeQL language reference.
pub const TYPEQL_LANGUAGE_REFERENCE: &str = include_str!("../reference/typeql/SKILL.md");
