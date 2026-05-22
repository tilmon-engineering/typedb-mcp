//! Operator configuration. See DESIGN.md §9.

use serde::Deserialize;
use std::{path::Path, time::Duration};


#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub typedb: TypeDbConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_idle_timeout_s")]
    pub idle_timeout_s: u64,
    #[serde(default = "default_result_cap")]
    pub result_cap: usize,
    #[serde(default = "default_true")]
    pub listen_stdio: bool,
    /// `host:port` to bind the Streamable HTTP transport, or `None` to disable.
    #[serde(default)]
    pub listen_http: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TypeDbConfig {
    /// `host:port` (e.g. `127.0.0.1:1729`). gRPC; no scheme.
    pub address: String,
    pub credentials: Credentials,
    /// Whether the driver should connect via TLS. Default false (local dev).
    #[serde(default)]
    pub tls_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum Credentials {
    /// Pull from environment variables.
    Env {
        username_var: String,
        password_var: String,
    },
    /// Literal — for local development only.
    Inline {
        username: String,
        password: String,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LoggingConfig {
    pub audit_log_path: Option<String>,
}

fn default_idle_timeout_s() -> u64 { 60 }
fn default_result_cap() -> usize { 500 }
fn default_true() -> bool { true }

impl Config {
    pub fn load_from_path(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text)?;
        Ok(cfg)
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.server.idle_timeout_s)
    }

    pub fn typedb_credentials(&self) -> anyhow::Result<(String, String)> {
        match &self.typedb.credentials {
            Credentials::Inline { username, password } => {
                Ok((username.clone(), password.clone()))
            }
            Credentials::Env { username_var, password_var } => {
                let u = std::env::var(username_var)
                    .map_err(|_| anyhow::anyhow!("env var {username_var} not set"))?;
                let p = std::env::var(password_var)
                    .map_err(|_| anyhow::anyhow!("env var {password_var} not set"))?;
                Ok((u, p))
            }
        }
    }
}
