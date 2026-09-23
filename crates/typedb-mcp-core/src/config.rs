//! Operator configuration and validated TypeDB connection settings.
use crate::typedb::TxKind;
use serde::Deserialize;
use std::{collections::HashMap, path::Path, time::Duration};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub typedb: TypeDbConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_idle_timeout_read_s")]
    pub idle_timeout_read_s: u64,
    #[serde(default = "default_idle_timeout_write_s")]
    pub idle_timeout_write_s: u64,
    #[serde(default = "default_idle_timeout_schema_s")]
    pub idle_timeout_schema_s: u64,
    #[serde(default = "default_session_ttl_s")]
    pub session_ttl_s: u64,
    #[serde(default = "default_result_cap")]
    pub result_cap: usize,
    #[serde(default = "default_true")]
    pub listen_stdio: bool,
    #[serde(default)]
    pub listen_http: Option<String>,
    #[serde(default)]
    pub enable_database_admin_tools: bool,
    #[serde(default)]
    pub enable_database_migration_tools: bool,
    #[serde(default)]
    pub expose_connection_details: bool,
    /// Maximum accepted UTF-8 schema size for migration imports (default 16 MiB).
    #[serde(default = "default_schema_size_cap_bytes")]
    pub migration_schema_size_cap_bytes: u64,
    /// Minimum free bytes required on each migration destination filesystem.
    #[serde(default = "default_migration_min_free_bytes")]
    pub migration_min_free_bytes: u64,
    /// Grace period for migration shutdown (default 30 seconds).
    #[serde(default = "default_migration_shutdown_grace_s")]
    pub migration_shutdown_grace_s: u64,
    #[serde(default)]
    pub allowed_hosts: Option<Vec<String>>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct TypeDbConfig {
    /// Legacy single `host:port` form. Exactly one address form is required.
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub addresses: Option<Vec<String>>,
    #[serde(default)]
    pub address_translation: Option<HashMap<String, String>>,
    pub credentials: Credentials,
    #[serde(default)]
    pub tls_enabled: bool,
    #[serde(default)]
    pub tls_root_ca_path: Option<String>,
    #[serde(default)]
    pub request_timeout_s: Option<u64>,
    #[serde(default)]
    pub primary_failover_retries: Option<usize>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum Credentials {
    Env {
        username_var: String,
        password_var: String,
    },
    Inline {
        username: String,
        password: String,
    },
}
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LoggingConfig {
    pub audit_log_path: Option<String>,
}
fn default_idle_timeout_read_s() -> u64 {
    600
}
fn default_idle_timeout_write_s() -> u64 {
    60
}
fn default_idle_timeout_schema_s() -> u64 {
    60
}
fn default_session_ttl_s() -> u64 {
    3600
}
fn default_result_cap() -> usize {
    500
}
fn default_true() -> bool {
    true
}
fn default_schema_size_cap_bytes() -> u64 {
    16 * 1024 * 1024
}
fn default_migration_min_free_bytes() -> u64 {
    256 * 1024 * 1024
}
fn default_migration_shutdown_grace_s() -> u64 {
    30
}
impl Config {
    pub fn load_from_path(path: &Path) -> anyhow::Result<Self> {
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }
    pub fn idle_timeout_for(&self, kind: TxKind) -> Duration {
        Duration::from_secs(match kind {
            TxKind::Read => self.server.idle_timeout_read_s,
            TxKind::Write => self.server.idle_timeout_write_s,
            TxKind::Schema => self.server.idle_timeout_schema_s,
        })
    }
    pub fn min_idle_timeout(&self) -> Duration {
        Duration::from_secs(
            self.server
                .idle_timeout_read_s
                .min(self.server.idle_timeout_write_s)
                .min(self.server.idle_timeout_schema_s),
        )
    }
    pub fn session_ttl(&self) -> Duration {
        Duration::from_secs(self.server.session_ttl_s)
    }
    pub fn typedb_credentials(&self) -> anyhow::Result<(String, String)> {
        match &self.typedb.credentials {
            Credentials::Inline { username, password } => Ok((username.clone(), password.clone())),
            Credentials::Env {
                username_var,
                password_var,
            } => Ok((
                std::env::var(username_var)
                    .map_err(|_| anyhow::anyhow!("env var {username_var} not set"))?,
                std::env::var(password_var)
                    .map_err(|_| anyhow::anyhow!("env var {password_var} not set"))?,
            )),
        }
    }
    pub fn connection_settings(&self) -> anyhow::Result<ConnectionSettings> {
        ConnectionSettings::from_config(&self.typedb)
    }
}
#[derive(Debug, Clone)]
pub struct ConnectionSettings {
    pub address: AddressSpec,
    pub tls_enabled: bool,
    pub tls_root_ca_path: Option<String>,
    pub request_timeout: Option<Duration>,
    pub primary_failover_retries: Option<usize>,
}
#[derive(Debug, Clone)]
pub enum AddressSpec {
    Legacy(String),
    Addresses(Vec<String>),
    Translation(HashMap<String, String>),
}
impl ConnectionSettings {
    pub fn from_config(c: &TypeDbConfig) -> anyhow::Result<Self> {
        let present = [
            !c.address.trim().is_empty(),
            c.addresses.as_ref().is_some_and(|v| !v.is_empty()),
            c.address_translation
                .as_ref()
                .is_some_and(|m| !m.is_empty()),
        ]
        .into_iter()
        .filter(|x| *x)
        .count();
        if present != 1 {
            anyhow::bail!(
                "exactly one non-empty TypeDB address form (address, addresses, address_translation) is required"
            )
        }
        if c.tls_root_ca_path.is_some() && !c.tls_enabled {
            anyhow::bail!("tls_root_ca_path requires tls_enabled=true")
        }
        if c.request_timeout_s == Some(0) {
            anyhow::bail!("request_timeout_s must be positive")
        }
        Ok(Self {
            address: (!c.address.trim().is_empty())
                .then(|| AddressSpec::Legacy(c.address.clone()))
                .or_else(|| {
                    c.addresses
                        .as_ref()
                        .map(|v| AddressSpec::Addresses(v.clone()))
                })
                .or_else(|| {
                    c.address_translation
                        .as_ref()
                        .map(|v| AddressSpec::Translation(v.clone()))
                })
                .unwrap(),
            tls_enabled: c.tls_enabled,
            tls_root_ca_path: c.tls_root_ca_path.clone(),
            request_timeout: c.request_timeout_s.map(Duration::from_secs),
            primary_failover_retries: c.primary_failover_retries,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_conflicting_forms() {
        let c = TypeDbConfig {
            address: "a:1".into(),
            addresses: Some(vec!["b:2".into()]),
            address_translation: None,
            credentials: Credentials::Inline {
                username: "u".into(),
                password: "p".into(),
            },
            tls_enabled: false,
            tls_root_ca_path: None,
            request_timeout_s: None,
            primary_failover_retries: None,
        };
        assert!(ConnectionSettings::from_config(&c).is_err());
    }
    #[test]
    fn rejects_ca_without_tls() {
        let c = TypeDbConfig {
            address: "a:1".into(),
            addresses: None,
            address_translation: None,
            credentials: Credentials::Inline {
                username: "u".into(),
                password: "p".into(),
            },
            tls_enabled: false,
            tls_root_ca_path: Some("/x".into()),
            request_timeout_s: None,
            primary_failover_retries: None,
        };
        assert!(ConnectionSettings::from_config(&c).is_err());
    }
}
