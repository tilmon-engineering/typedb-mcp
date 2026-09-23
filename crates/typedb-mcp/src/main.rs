//! typedb-mcp entry point. See DESIGN.md.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use typedb_mcp_core::{
    config::Config, core::TypeDbCore, handler::TypeDbMcp, session::SessionStore,
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
    let settings = config.connection_settings()?;
    let (user, pass) = config.typedb_credentials()?;
    let typedb = Arc::new(TypeDbClient::connect_with_settings(&settings, &user, &pass).await?);
    let core = TypeDbCore::new(config.clone(), typedb.clone(), SessionStore::new());
    let reaper = core.spawn_reaper();
    let shutdown = CancellationToken::new();

    let http_task = if let Some(addr) = &config.server.listen_http {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(addr = %listener.local_addr()?, "Streamable HTTP transport listening at /mcp");
        let mut http_cfg =
            StreamableHttpServerConfig::default().with_cancellation_token(shutdown.child_token());
        if let Some(hosts) = &config.server.allowed_hosts {
            http_cfg = if hosts.is_empty() {
                tracing::warn!("server.allowed_hosts = []: Host-header check disabled");
                http_cfg.disable_allowed_hosts()
            } else {
                tracing::info!(allowed_hosts = ?hosts, "HTTP Host-header allowlist enabled");
                http_cfg.with_allowed_hosts(hosts.iter().cloned())
            };
        }
        let core_for_service = core.clone();
        let service = StreamableHttpService::new(
            move || Ok(TypeDbMcp::for_http(core_for_service.clone())),
            LocalSessionManager::default().into(),
            http_cfg,
        );
        let router = axum::Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn(log_mcp_request));
        let cancel = shutdown.clone();
        Some(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
        }))
    } else {
        None
    };

    let stdio_task = if config.server.listen_stdio {
        let service = TypeDbMcp::for_stdio(core.clone()).serve(stdio()).await?;
        Some(tokio::spawn(async move { service.waiting().await }))
    } else {
        None
    };
    if http_task.is_none() && stdio_task.is_none() {
        anyhow::bail!("neither stdio nor HTTP transport is enabled in config");
    }

    let mut http_task = http_task;
    let mut stdio_task = stdio_task;
    // The HTTP JoinHandle must be awaited AT MOST once: record the select's
    // result and reuse it during shutdown instead of re-polling a completed
    // handle (a second poll panics: "JoinHandle polled after completion").
    let mut http_result: Option<Result<Result<(), std::io::Error>, tokio::task::JoinError>> = None;
    tokio::select! {
        result = async { match stdio_task.as_mut() { Some(task) => Some(task.await), None => None } }, if stdio_task.is_some() => {
            if let Some(result) = result {
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::error!(%error, "stdio transport failed"),
                    Err(error) => tracing::error!(%error, "stdio task failed"),
                }
            }
            tracing::info!("stdio transport closed; shutting down");
            stdio_task.take(); // consumed here; do not re-await below
        }
        _ = tokio::signal::ctrl_c() => tracing::info!("received ctrl-c; shutting down"),
        result = async { match http_task.as_mut() { Some(task) => Some(task.await), None => None } }, if http_task.is_some() => {
            if let Some(result) = result {
                match result {
                    Ok(Ok(())) => {
                        tracing::info!("HTTP transport closed; shutting down");
                        http_result = Some(Ok(Ok(())));
                    }
                    Ok(Err(error)) => {
                        eprintln!("typedb-mcp: HTTP transport failed: {error}");
                        shutdown.cancel();
                        reaper.abort();
                        typedb.force_close();
                        std::process::exit(1);
                    }
                    Err(error) => {
                        eprintln!("typedb-mcp: HTTP task failed: {error}");
                        shutdown.cancel();
                        reaper.abort();
                        typedb.force_close();
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    core.migration_supervisor.stop_admission();
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(config.server.migration_shutdown_grace_s);
    let supervisor = core.migration_supervisor.clone();
    let grace = Duration::from_secs(config.server.migration_shutdown_grace_s);
    let migration_shutdown = tokio::task::spawn_blocking(move || supervisor.shutdown(grace));
    shutdown.cancel();
    shutdown.cancel();
    let http_future = async {
        match http_result.take() {
            Some(result) => Ok(result),
            None => match http_task.take() {
                Some(task) => tokio::time::timeout_at(deadline, task)
                    .await
                    .map_err(|_| "deadline"),
                None => Ok(Ok(Ok(()))),
            },
        }
    };
    let migration_future = tokio::time::timeout_at(deadline, migration_shutdown);
    let (http_outcome, migration_outcome) = tokio::join!(http_future, migration_future);
    match http_outcome {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => {
            eprintln!("typedb-mcp: HTTP transport failed during shutdown: {error}");
            reaper.abort();
            typedb.force_close();
            std::process::exit(1);
        }
        Ok(Err(error)) => {
            eprintln!("typedb-mcp: HTTP shutdown task failed: {error}");
            reaper.abort();
            typedb.force_close();
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!(
                "typedb-mcp: shutdown grace expired; forcing close (migration state uncertain)"
            );
            reaper.abort();
            typedb.force_close();
            std::process::exit(1);
        }
    }
    match migration_outcome {
        Ok(Ok(typedb_mcp_core::ShutdownResult::Completed)) => {}
        Ok(Ok(typedb_mcp_core::ShutdownResult::StillRunning)) | Err(_) => {
            eprintln!(
                "typedb-mcp: shutdown grace expired; migration state uncertain; forcing close"
            );
            reaper.abort();
            typedb.force_close();
            std::process::exit(1);
        }
        Ok(Err(error)) => {
            eprintln!("typedb-mcp: migration shutdown task failed: {error}");
            reaper.abort();
            typedb.force_close();
            std::process::exit(1);
        }
    }
    if let Some(task) = stdio_task.take() {
        let _ = task.await;
    }
    reaper.abort();
    typedb.force_close();
    Ok(())
}

async fn log_mcp_request(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let session_id = req
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>");
    tracing::info!(target: "typedb_mcp::http", %method, %path, mcp_session_id = session_id, "incoming HTTP request");
    let response = next.run(req).await;
    tracing::info!(target: "typedb_mcp::http", %method, %path, status = response.status().as_u16(), "completed HTTP request");
    response
}
