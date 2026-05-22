//! Per-MCP-session state and the idle reaper. See DESIGN.md §3 and §4.
//!
//! Each session owns its own async `Mutex` so a single session's calls
//! serialize naturally and the live driver `Transaction` can be held
//! across `await` points. The agent issues one tool call at a time per
//! session, so contention is theoretical, not real.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::Mutex;

use crate::typedb::{DriverTransaction, TxKind};

/// Opaque session identifier. For Streamable HTTP this is the
/// `Mcp-Session-Id` header; for stdio it is a process-singleton constant
/// (`"stdio"`).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn stdio() -> Self { Self("stdio".to_owned()) }
}

/// The live transaction held by a session.
#[derive(Debug)]
pub struct OpenTx {
    /// Short opaque ID for log correlation — not the driver's internal id.
    pub id: String,
    pub database: String,
    pub kind: TxKind,
    /// The live driver `Transaction` value. `commit` consumes it; `query`
    /// and `rollback` take `&self`.
    pub transaction: DriverTransaction,
    /// Last time this transaction was touched. Drives the idle reaper.
    pub last_activity: Instant,
}

#[derive(Debug, Default)]
pub struct SessionState {
    pub schema_seen: HashSet<String>,
    pub tx: Option<OpenTx>,
}

/// Shared store of all live sessions. Each session sits behind its own
/// `Mutex` so holding it across an `await` does not block the rest of
/// the server.
#[derive(Debug, Default)]
pub struct SessionStore {
    inner: Mutex<HashMap<SessionId, Arc<Mutex<SessionState>>>>,
}

impl SessionStore {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    /// Get-or-create a session and return an `Arc` to its lock. Callers
    /// then `.lock().await` the result for as long as they need.
    pub async fn get_or_create(&self, id: &SessionId) -> Arc<Mutex<SessionState>> {
        let mut guard = self.inner.lock().await;
        guard
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(SessionState::default())))
            .clone()
    }

    /// Snapshot the agent-visible session block. Acquires the session lock
    /// briefly. If no session exists, returns an empty snapshot.
    pub async fn snapshot(&self, id: &SessionId) -> SessionSnapshot {
        let session_arc = {
            let guard = self.inner.lock().await;
            guard.get(id).cloned()
        };
        match session_arc {
            None => SessionSnapshot::default(),
            Some(arc) => {
                let s = arc.lock().await;
                SessionSnapshot {
                    database: s.tx.as_ref().map(|t| t.database.clone()),
                    transaction: s.tx.as_ref().map(|t| OpenTxView {
                        id: t.id.clone(),
                        kind: t.kind,
                        state: TxState::Open,
                    }),
                    schema_seen_for: s.schema_seen.iter().cloned().collect(),
                }
            }
        }
    }

    /// Iterate all live sessions. The result is a vector of `(id, Arc)`
    /// pairs so the lock on `inner` is held only briefly.
    pub async fn all_sessions(&self) -> Vec<(SessionId, Arc<Mutex<SessionState>>)> {
        self.inner
            .lock()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Agent-facing session block — what gets serialized into every response.
/// See DESIGN.md §6.
#[derive(Debug, Default, Serialize)]
pub struct SessionSnapshot {
    pub database: Option<String>,
    pub transaction: Option<OpenTxView>,
    pub schema_seen_for: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct OpenTxView {
    pub id: String,
    pub kind: TxKind,
    pub state: TxState,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TxState {
    Open,
}

/// Background reaper that rolls back idle transactions. See DESIGN.md §3.
pub async fn run_idle_reaper(store: Arc<SessionStore>, idle_timeout: Duration) {
    let tick = (idle_timeout / 4).max(Duration::from_secs(5));
    loop {
        tokio::time::sleep(tick).await;
        let now = Instant::now();

        let sessions = store.all_sessions().await;
        for (id, arc) in sessions {
            let mut state = arc.lock().await;
            let should_reap = state
                .tx
                .as_ref()
                .is_some_and(|tx| now.duration_since(tx.last_activity) >= idle_timeout);
            if should_reap {
                if let Some(tx) = state.tx.take() {
                    // Best-effort rollback; if it fails, the driver will
                    // clean up server-side soon enough.
                    if let Err(e) = tx.transaction.rollback().await {
                        tracing::warn!(?id, error = %e, "reaper: rollback failed");
                    }
                    tracing::info!(session = ?id.0, "reaped idle transaction");
                }
            }
        }
    }
}
