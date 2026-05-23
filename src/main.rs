//! typedb-mcp entry point. See DESIGN.md.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService,
            session::local::LocalSessionManager,
        },
    },
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use typedb_mcp::{
    config::Config,
    handler::TypeDbMcp,
    session::{SessionStore, run_reaper},
    typedb::{TxKind, TypeDbClient},
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

    // Reaper: releases idle transactions and purges expired sessions.
    // Per-kind idle timeouts (read/write/schema) — see DESIGN.md §3 and
    // ServerConfig docs in src/config.rs. Reads are held longer because
    // they don't pin uncommitted state and shouldn't be reaped out from
    // under an agent's long LLM turn.
    {
        let s = sessions.clone();
        let read_idle = config.idle_timeout_for(TxKind::Read);
        let write_idle = config.idle_timeout_for(TxKind::Write);
        let schema_idle = config.idle_timeout_for(TxKind::Schema);
        let tick = config.min_idle_timeout();
        let session_ttl = config.session_ttl();
        tokio::spawn(async move {
            run_reaper(s, read_idle, write_idle, schema_idle, tick, session_ttl).await
        });
    }

    let shutdown = CancellationToken::new();

    // Streamable HTTP transport (optional, opt-in via config).
    let http_task = if let Some(addr) = &config.server.listen_http {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        tracing::info!(addr = %local_addr, "Streamable HTTP transport listening at /mcp");

        let config_h = config.clone();
        let typedb_h = typedb.clone();
        let sessions_h = sessions.clone();
        let ct_for_service = shutdown.child_token();

        // Per-MCP-session factory: each session gets its own handler
        // instance, but they all share the same TypeDB client and the
        // same SessionStore — so our schema-read gate and tx state are
        // tracked per agent session, not per process.
        let mut http_cfg =
            StreamableHttpServerConfig::default().with_cancellation_token(ct_for_service);
        if let Some(hosts) = &config.server.allowed_hosts {
            if hosts.is_empty() {
                tracing::warn!(
                    "server.allowed_hosts = []: Host-header check disabled; \
                     relying on network-level isolation"
                );
                http_cfg = http_cfg.disable_allowed_hosts();
            } else {
                tracing::info!(
                    allowed_hosts = ?hosts,
                    "Streamable HTTP Host-header allowlist overridden by config"
                );
                http_cfg = http_cfg.with_allowed_hosts(hosts.iter().cloned());
            }
        }
        let service = StreamableHttpService::new(
            move || Ok(TypeDbMcp::new(config_h.clone(), typedb_h.clone(), sessions_h.clone())),
            LocalSessionManager::default().into(),
            http_cfg,
        );
        let router = axum::Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn(log_mcp_request));

        let ct_for_axum = shutdown.clone();
        Some(tokio::spawn(async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move { ct_for_axum.cancelled().await })
                .await;
            if let Err(e) = result {
                tracing::error!(error = %e, "axum::serve exited with error");
            }
        }))
    } else {
        None
    };

    // Stdio transport (default).
    let stdio_task = if config.server.listen_stdio {
        let handler = TypeDbMcp::new(config.clone(), typedb.clone(), sessions.clone());
        let service = handler.serve(stdio()).await.map_err(|e| {
            tracing::error!("rmcp stdio service failed to start: {:?}", e);
            anyhow::anyhow!("rmcp stdio service: {e}")
        })?;
        Some(tokio::spawn(async move {
            if let Err(e) = service.waiting().await {
                tracing::error!(error = %e, "stdio service exited with error");
            }
        }))
    } else {
        None
    };

    if http_task.is_none() && stdio_task.is_none() {
        anyhow::bail!("neither stdio nor HTTP transport is enabled in config");
    }

    // Wait for either:
    //   1. stdio task to end naturally (client disconnected)
    //   2. Ctrl-C
    // Either triggers HTTP shutdown via the cancellation token.
    let stdio_done: futures::future::BoxFuture<'_, ()> = match stdio_task {
        Some(handle) => Box::pin(async move {
            let _ = handle.await;
        }),
        None => Box::pin(futures::future::pending()),
    };
    let ctrl_c: futures::future::BoxFuture<'_, ()> = Box::pin(async {
        let _ = tokio::signal::ctrl_c().await;
    });

    tokio::select! {
        _ = stdio_done => tracing::info!("stdio transport closed; shutting down"),
        _ = ctrl_c => tracing::info!("received ctrl-c; shutting down"),
    }
    shutdown.cancel();
    if let Some(h) = http_task {
        let _ = h.await;
    }
    typedb.force_close();
    Ok(())
}

/// Diagnostic middleware: log every incoming /mcp request with the
/// `Mcp-Session-Id` header value (or `<none>` if missing), method, path,
/// and user-agent; then log the response status. Lets us tell whether
/// clients are sending session IDs that we're failing to honour vs. not
/// sending them at all (one-shot-per-call gateway behaviour).
async fn log_mcp_request(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let session_id = req
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let user_agent = req
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    tracing::info!(
        target: "typedb_mcp::http",
        %method,
        path = %path,
        mcp_session_id = session_id.as_deref().unwrap_or("<none>"),
        user_agent = user_agent.as_deref().unwrap_or("<none>"),
        "incoming HTTP request",
    );

    let response = next.run(req).await;

    tracing::info!(
        target: "typedb_mcp::http",
        %method,
        path = %path,
        mcp_session_id = session_id.as_deref().unwrap_or("<none>"),
        status = response.status().as_u16(),
        "completed HTTP request",
    );

    response
}
