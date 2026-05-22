//! Per-server-session state and the idle reaper. See DESIGN.md §3 and §4.
//!
//! Sessions are minted by the `start_session` tool and identified by a
//! UUID v4. The agent passes the `session_id` to every other tool. This
//! is deliberately decoupled from the MCP transport's own session: some
//! clients (LiteLLM's MCP gateway observed on edge-01 2026-05-22) tear
//! the transport session down after every tool call, but our state has
//! to live across calls.
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

/// Opaque server-issued session identifier (UUID v4 string).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new_random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
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
    /// Last time this transaction was touched. Drives the tx-idle reaper.
    pub last_activity: Instant,
}

#[derive(Debug)]
pub struct SessionState {
    pub schema_seen: HashSet<String>,
    pub tx: Option<OpenTx>,
    /// Absolute deadline for this session. Set on `start_session`,
    /// refreshed to `now + session_ttl` on every successful tool call.
    pub expires_at: Instant,
}

/// Result of resolving a `session_id` parameter against the store.
#[derive(Debug)]
pub enum SessionResolveError {
    /// No session with that ID exists. Either fabricated, from a prior
    /// process lifetime, or already purged.
    Unknown,
    /// The session existed but its TTL has passed (and the store has now
    /// purged it). The agent must call `start_session` again.
    Expired,
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

    /// Mint a brand-new session and return its ID. The session is
    /// initialized with no schema reads, no open tx, and a fresh TTL.
    pub async fn start(&self, ttl: Duration) -> SessionId {
        let id = SessionId::new_random();
        let state = SessionState {
            schema_seen: HashSet::new(),
            tx: None,
            expires_at: Instant::now() + ttl,
        };
        let mut guard = self.inner.lock().await;
        guard.insert(id.clone(), Arc::new(Mutex::new(state)));
        let total = guard.len();
        tracing::info!(
            target: "typedb_mcp::session",
            session_id = %id.0,
            total_sessions = total,
            ttl_s = ttl.as_secs(),
            "minted new session",
        );
        id
    }

    /// Resolve a session ID against the store, refreshing its TTL if it
    /// exists and has not expired. Expired sessions are purged here.
    ///
    /// On success, returns the `Arc<Mutex<SessionState>>` for use by
    /// the caller. On failure, returns the reason — the handler turns
    /// this into a `SESSION_UNKNOWN` / `SESSION_EXPIRED` envelope.
    pub async fn resolve_and_touch(
        &self,
        id: &SessionId,
        ttl: Duration,
    ) -> Result<Arc<Mutex<SessionState>>, SessionResolveError> {
        let mut guard = self.inner.lock().await;
        let Some(arc) = guard.get(id).cloned() else {
            tracing::debug!(
                target: "typedb_mcp::session",
                session_id = %id.0,
                "session lookup miss (SESSION_UNKNOWN)",
            );
            return Err(SessionResolveError::Unknown);
        };
        // Check expiry without holding the inner-store lock during the
        // tx rollback (which can take some time).
        let now = Instant::now();
        let expired = {
            let state = arc.lock().await;
            state.expires_at <= now
        };
        if expired {
            // Purge and report. Hold the store lock through the remove,
            // then release before rolling back any open tx.
            guard.remove(id);
            drop(guard);
            // Best-effort: roll back any tx the dying session held.
            if let Some(tx) = arc.lock().await.tx.take() {
                if let Err(e) = tx.transaction.rollback().await {
                    tracing::warn!(
                        target: "typedb_mcp::session",
                        session_id = %id.0,
                        error = %e,
                        "rollback on expired session failed",
                    );
                }
            }
            tracing::info!(
                target: "typedb_mcp::session",
                session_id = %id.0,
                "session expired and purged on resolve",
            );
            return Err(SessionResolveError::Expired);
        }
        // Refresh TTL.
        {
            let mut state = arc.lock().await;
            state.expires_at = now + ttl;
        }
        Ok(arc)
    }

    /// Snapshot the agent-visible session block for a known-good session
    /// `Arc`. Pre-resolved so the caller controls the lifetime semantics.
    pub async fn snapshot_arc(id: &SessionId, arc: &Arc<Mutex<SessionState>>) -> SessionSnapshot {
        let s = arc.lock().await;
        SessionSnapshot {
            session_id: Some(id.0.clone()),
            database: s.tx.as_ref().map(|t| t.database.clone()),
            transaction: s.tx.as_ref().map(|t| OpenTxView {
                id: t.id.clone(),
                kind: t.kind,
                state: TxState::Open,
            }),
            schema_seen_for: s.schema_seen.iter().cloned().collect(),
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

    /// Remove a specific session from the store, returning its Arc if it
    /// existed. Used by the reaper after deciding a session is expired.
    pub async fn remove(&self, id: &SessionId) -> Option<Arc<Mutex<SessionState>>> {
        self.inner.lock().await.remove(id)
    }
}

/// Agent-facing session block — what gets serialized into every response.
/// See DESIGN.md §6.
#[derive(Debug, Default, Serialize)]
pub struct SessionSnapshot {
    /// The session_id this snapshot belongs to. `None` only when no
    /// session resolved (e.g. on `SESSION_UNKNOWN`/`SESSION_EXPIRED`
    /// errors, which still need to render a snapshot).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
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

/// Background reaper for both:
/// 1. Open transactions whose `last_activity` exceeds `tx_idle_timeout`
///    (the tx is rolled back; the session keeps existing).
/// 2. Whole sessions whose `expires_at` has passed (the session is
///    purged; any tx it held is rolled back as part of the purge).
///
/// Runs every `tx_idle_timeout / 4` (min 5s).
pub async fn run_reaper(
    store: Arc<SessionStore>,
    tx_idle_timeout: Duration,
    session_ttl: Duration,
) {
    let tick = (tx_idle_timeout / 4).max(Duration::from_secs(5));
    loop {
        tokio::time::sleep(tick).await;
        let now = Instant::now();

        let sessions = store.all_sessions().await;
        for (id, arc) in sessions {
            // First check: whole-session expiry. Hold the per-session
            // lock briefly to peek at expires_at, then drop it before
            // doing the purge dance (which may involve await on
            // rollback).
            let purge = {
                let state = arc.lock().await;
                state.expires_at <= now
            };
            if purge {
                if let Some(removed) = store.remove(&id).await {
                    if let Some(tx) = removed.lock().await.tx.take() {
                        if let Err(e) = tx.transaction.rollback().await {
                            tracing::warn!(
                                target: "typedb_mcp::session",
                                session_id = %id.0,
                                error = %e,
                                "reaper: rollback on expired session failed",
                            );
                        }
                    }
                    tracing::info!(
                        target: "typedb_mcp::session",
                        session_id = %id.0,
                        "reaper purged expired session",
                    );
                }
                continue;
            }

            // Second check: tx-idle reaping for sessions that are still
            // alive. We use `_session_ttl` only above; here we use the
            // tx_idle_timeout.
            let mut state = arc.lock().await;
            let should_reap_tx = state
                .tx
                .as_ref()
                .is_some_and(|tx| now.duration_since(tx.last_activity) >= tx_idle_timeout);
            if should_reap_tx {
                if let Some(tx) = state.tx.take() {
                    if let Err(e) = tx.transaction.rollback().await {
                        tracing::warn!(?id, error = %e, "reaper: tx rollback failed");
                    }
                    tracing::info!(session = ?id.0, "reaper: rolled back idle transaction");
                }
            }
            // session_ttl reference kept to silence unused-param warnings
            // in case the body ever loses its use:
            let _ = session_ttl;
        }
    }
}
