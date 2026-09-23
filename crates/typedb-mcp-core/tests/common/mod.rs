//! Shared configuration and lifecycle helpers for live TypeDB tests.
//!
//! The matrix runner sets these variables for each disposable server. Defaults
//! preserve the historical local developer setup while allowing arbitrary
//! loopback ports and credentials:
//! `TYPEDB_MCP_TEST_ADDRESS`, `TYPEDB_MCP_TEST_USERNAME`,
//! `TYPEDB_MCP_TEST_PASSWORD`, and `TYPEDB_MCP_TEST_TLS`.

use typedb_mcp_core::typedb::TypeDbClient;

pub const ADDRESS_ENV: &str = "TYPEDB_MCP_TEST_ADDRESS";
pub const USERNAME_ENV: &str = "TYPEDB_MCP_TEST_USERNAME";
pub const PASSWORD_ENV: &str = "TYPEDB_MCP_TEST_PASSWORD";
pub const TLS_ENV: &str = "TYPEDB_MCP_TEST_TLS";

pub fn smoke_enabled() -> bool {
    std::env::var("TYPEDB_MCP_SMOKE").as_deref() == Ok("1")
}

pub fn address() -> String {
    std::env::var(ADDRESS_ENV).unwrap_or_else(|_| "127.0.0.1:1729".to_owned())
}

pub fn username() -> String {
    std::env::var(USERNAME_ENV).unwrap_or_else(|_| "admin".to_owned())
}

pub fn password() -> String {
    std::env::var(PASSWORD_ENV).unwrap_or_else(|_| "password".to_owned())
}

pub fn tls_enabled() -> bool {
    matches!(std::env::var(TLS_ENV).as_deref(), Ok("1" | "true" | "yes"))
}

pub async fn connect() -> anyhow::Result<TypeDbClient> {
    Ok(TypeDbClient::connect(&address(), &username(), &password(), tls_enabled()).await?)
}

#[allow(dead_code)]
pub fn unique_database(prefix: &str) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{}_{}", prefix, &suffix[..12])
}
