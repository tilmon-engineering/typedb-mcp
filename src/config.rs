//! Operator configuration. See DESIGN.md §9.

use serde::Deserialize;
use std::{path::Path, time::Duration};

use crate::typedb::TxKind;


#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub typedb: TypeDbConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Idle timeout for READ transactions. Reads don't hold uncommitted
    /// state and don't block other transactions, so the cost of letting an
    /// agent hold one across a long LLM turn is just snapshot memory.
    /// Default 600s.
    #[serde(default = "default_idle_timeout_read_s")]
    pub idle_timeout_read_s: u64,
    /// Idle timeout for WRITE transactions. Writes hold uncommitted state,
    /// so abandoned writes have real cost. Default 60s.
    #[serde(default = "default_idle_timeout_write_s")]
    pub idle_timeout_write_s: u64,
    /// Idle timeout for SCHEMA transactions. Schema operations can block
    /// other transactions, so we keep this aggressive. Default 60s.
    #[serde(default = "default_idle_timeout_schema_s")]
    pub idle_timeout_schema_s: u64,
    /// How long a server-issued session may sit idle before its
    /// `SessionStore` entry is purged (and any tx it held rolled back).
    /// Refreshed on every tool call that resolves the session. Default
    /// 3600s (60 minutes) per DESIGN.md §3.
    #[serde(default = "default_session_ttl_s")]
    pub session_ttl_s: u64,
    #[serde(default = "default_result_cap")]
    pub result_cap: usize,
    #[serde(default = "default_true")]
    pub listen_stdio: bool,
    /// `host:port` to bind the Streamable HTTP transport, or `None` to disable.
    #[serde(default)]
    pub listen_http: Option<String>,
    /// Host-header allowlist for the Streamable HTTP transport. Defaults to
    /// `["localhost", "127.0.0.1", "::1"]` (rmcp's loopback-only default,
    /// which mitigates DNS rebinding against local stdio-style deployments).
    /// Operators deploying behind a Service / Ingress / reverse proxy must
    /// extend this with the externally-visible authority (e.g.
    /// `typedb-mcp.example.svc:8001`). An empty list (`[]`) disables the
    /// check entirely — only safe when network-level isolation is enforced
    /// elsewhere.
    #[serde(default)]
    pub allowed_hosts: Option<Vec<String>>,
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

fn default_idle_timeout_read_s() -> u64 { 600 }
fn default_idle_timeout_write_s() -> u64 { 60 }
fn default_idle_timeout_schema_s() -> u64 { 60 }
fn default_session_ttl_s() -> u64 { 3600 }
fn default_result_cap() -> usize { 500 }
fn default_true() -> bool { true }

impl Config {
    pub fn load_from_path(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text)?;
        Ok(cfg)
    }

    /// Idle timeout for a transaction of the given kind. See per-field docs
    /// on `ServerConfig` for the per-kind rationale.
    pub fn idle_timeout_for(&self, kind: TxKind) -> Duration {
        let s = match kind {
            TxKind::Read => self.server.idle_timeout_read_s,
            TxKind::Write => self.server.idle_timeout_write_s,
            TxKind::Schema => self.server.idle_timeout_schema_s,
        };
        Duration::from_secs(s)
    }

    /// Shortest of the three idle timeouts — used to set the reaper's tick
    /// cadence so it can react promptly to whichever kind times out first.
    pub fn min_idle_timeout(&self) -> Duration {
        let s = self
            .server
            .idle_timeout_read_s
            .min(self.server.idle_timeout_write_s)
            .min(self.server.idle_timeout_schema_s);
        Duration::from_secs(s)
    }

    pub fn session_ttl(&self) -> Duration {
        Duration::from_secs(self.server.session_ttl_s)
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
