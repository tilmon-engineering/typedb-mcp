//! typedb-mcp entry point. See DESIGN.md.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

use typedb_mcp::{
    config::Config,
    handler::TypeDbMcp,
    session::{SessionStore, run_idle_reaper},
    typedb::TypeDbClient,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config_path = std::env::var("TYPEDB_MCP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));
    let config = Arc::new(Config::load_from_path(&config_path)?);

    let (user, pass) = config.typedb_credentials()?;
    let typedb = Arc::new(
        TypeDbClient::connect(
            &config.typedb.address,
            &user,
            &pass,
            config.typedb.tls_enabled,
        )
        .await?,
    );

    let sessions = SessionStore::new();

    // Idle reaper
    {
        let s = sessions.clone();
        let idle = config.idle_timeout();
        tokio::spawn(async move { run_idle_reaper(s, idle).await });
    }

    if let Some(addr) = &config.server.listen_http {
        tracing::warn!(
            addr = %addr,
            "Streamable HTTP transport is configured but not yet wired; running stdio only"
        );
    }

    if !config.server.listen_stdio {
        anyhow::bail!("listen_stdio is false and HTTP transport is not yet wired");
    }

    let handler = TypeDbMcp::new(config, typedb.clone(), sessions);
    let service = handler.serve(stdio()).await.map_err(|e| {
        tracing::error!("rmcp service error: {:?}", e);
        anyhow::anyhow!("rmcp service error: {e}")
    })?;

    service.waiting().await?;
    typedb.force_close();
    Ok(())
}
