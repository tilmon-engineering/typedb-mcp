//! Bounded, sanitized connection diagnostics.
use crate::{connection::ServerVersionInfo, error::InternalError};
use serde::Serialize;
use std::time::{Duration, Instant};
use typedb_driver::{Replica, Server, TypeDBDriver};
#[derive(Debug, Clone, Serialize)]
pub struct TopologyServer {
    pub address: Option<String>,
    pub id: Option<u64>,
    pub role: Option<String>,
    pub term: Option<u64>,
    pub available: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    pub version: ServerVersionInfo,
    pub connectivity: ConnectivityObservation,
    pub topology: Option<Vec<TopologyServer>>,
    pub topology_error: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct ConnectivityObservation {
    pub status: String,
    pub elapsed_ms: u128,
    pub read_only: bool,
}
impl DiagnosticReport {
    pub fn sanitized_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("diagnostic DTO serializes")
    }
}
#[allow(clippy::result_large_err)]
pub async fn collect(
    driver: &TypeDBDriver,
    version: ServerVersionInfo,
    deadline: Duration,
    expose_topology: bool,
) -> Result<DiagnosticReport, InternalError> {
    let started = Instant::now();
    if started.elapsed() >= deadline {
        return Err(InternalError::Config(
            "diagnostic deadline exceeded (underlying RPC cancellation is not guaranteed)".into(),
        ));
    }
    let remaining = deadline.saturating_sub(started.elapsed());
    let connectivity_result = tokio::time::timeout(remaining, driver.databases().all()).await;
    let connectivity = match connectivity_result {
        Ok(Ok(_)) => ConnectivityObservation {
            status: "connected".into(),
            elapsed_ms: started.elapsed().as_millis(),
            read_only: true,
        },
        Ok(Err(e)) => ConnectivityObservation {
            status: format!("unavailable: {e}"),
            elapsed_ms: started.elapsed().as_millis(),
            read_only: true,
        },
        Err(_) => {
            return Err(InternalError::Config(
                "diagnostic deadline exceeded (underlying RPC cancellation is not guaranteed)"
                    .into(),
            ));
        }
    };
    let (topology, topology_error) = if expose_topology {
        match tokio::time::timeout(deadline.saturating_sub(started.elapsed()), driver.servers())
            .await
        {
            Ok(Ok(servers)) => (Some(servers.into_iter().map(server_dto).collect()), None),
            Ok(Err(e)) => (None, Some(e.to_string())),
            Err(_) => (
                None,
                Some(
                    "diagnostic deadline exceeded; underlying RPC cancellation is not guaranteed"
                        .into(),
                ),
            ),
        }
    } else {
        (None, None)
    };
    Ok(DiagnosticReport {
        version,
        connectivity,
        topology,
        topology_error,
    })
}
fn server_dto(s: Server) -> TopologyServer {
    TopologyServer {
        address: s.address().map(ToString::to_string),
        id: Some(s.id()),
        role: s.role().map(|r| format!("{r:?}")),
        term: s.term(),
        available: matches!(s, Server::Available(_)),
    }
}
