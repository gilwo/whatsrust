//! Bridge event bus — unified event stream for inbound messages, outbound status,
//! and delivery receipts. Backed by `tokio::sync::broadcast`.
//!
//! Consumers:
//! - Library callers via `WhatsAppBridge::subscribe_events()`
//! - SSE endpoint via `/api/events`
//! - Internal waiters (e.g. `send_message_with_id` waiting for send completion)

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::bridge::WhatsAppInbound;

/// Capacity of the broadcast channel. Slow receivers that fall behind
/// this many events will get `Lagged` and should reconnect.
const EVENT_BUS_CAPACITY: usize = 2048;

/// Unified bridge event envelope.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(dead_code)] // Heartbeat constructed by SSE handler in api.rs, not bridge itself
pub enum BridgeEvent {
    /// An inbound WhatsApp message or status update.
    Inbound(Arc<WhatsAppInbound>),
    /// Status change for an outbound job (queued → sending → sent → delivered → read).
    OutboundStatus(OutboundStatusEvent),
    /// Progress update for a backfill job (per-batch running + terminal states).
    BackfillProgress(BackfillProgressEvent),
    /// Storage growth alert — DB footprint grew ≥50% vs the persisted baseline (ADR 0013).
    StorageAlert(StorageAlertEvent),
    /// Periodic heartbeat for keepalive (SSE clients).
    Heartbeat,
}

/// Progress update for a backfill job.
///
/// Emitted per-batch (status = "running") and once on terminal exit (status one of
/// "done" / "paused" / "failed" / "cancelled" / "deferred").
///
/// `target_value` is present for `count` jobs, enabling "N / target" display on the
/// client. `more_remain` drives the fuzzy "still going" indicator for `since`/`all`
/// targets (ADR 0034). Percentages are NOT computed server-side; raw numbers are shipped.
#[derive(Debug, Clone, Serialize)]
pub struct BackfillProgressEvent {
    /// The backfill_jobs row ID.
    pub job_id: i64,
    /// Chat JID the job is fetching for.
    pub chat_jid: String,
    /// Job target kind: "all", "since", or "count".
    pub target_kind: String,
    /// Target value (milliseconds for "since", message count for "count"; None for "all").
    pub target_value: Option<i64>,
    /// Total messages fetched so far (cumulative, including this batch).
    pub fetched: i64,
    /// Current status: "running", "done", "paused", "failed", "cancelled", "deferred".
    pub status: String,
    /// Whether the phone indicates more history is available (fuzzy indicator for since/all).
    pub more_remain: bool,
}

/// Storage growth alert emitted when the DB footprint grows ≥50% vs the persisted baseline.
///
/// Raw bytes are shipped; clients can format as MB. Emitted at most once per growth event
/// because the baseline is reset to `current_bytes` on each alert (ADR 0013).
#[derive(Debug, Clone, Serialize)]
pub struct StorageAlertEvent {
    /// Current total on-disk footprint in bytes (`db + -wal + -shm`).
    pub current_bytes: u64,
    /// Baseline footprint in bytes at the time of the last alert (or migration seed).
    pub baseline_bytes: u64,
    /// Growth percentage: `(current - baseline) * 100 / baseline`.
    pub growth_pct: u32,
}

/// Status update for an outbound job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundStatusEvent {
    /// The queue row ID returned by `enqueue_job()`.
    pub job_id: i64,
    /// Current state of the job.
    pub state: OutboundJobState,
    /// WhatsApp message ID (available after successful send).
    pub wa_message_id: Option<String>,
    /// Error message (if state is Failed).
    pub error: Option<String>,
}

/// Lifecycle states for an outbound job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(dead_code)] // Queued + Expired are valid states for SSE consumers / downstream crates
#[serde(rename_all = "snake_case")]
pub enum OutboundJobState {
    Queued,
    Sending,
    Sent,
    Delivered,
    Read,
    Played,
    Failed,
    Expired,
}

/// Delivery status for inbound receipt events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Sent,
    Delivered,
    Read,
    Played,
    Failed,
    Unknown,
}

impl std::fmt::Display for OutboundJobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => f.write_str("queued"),
            Self::Sending => f.write_str("sending"),
            Self::Sent => f.write_str("sent"),
            Self::Delivered => f.write_str("delivered"),
            Self::Read => f.write_str("read"),
            Self::Played => f.write_str("played"),
            Self::Failed => f.write_str("failed"),
            Self::Expired => f.write_str("expired"),
        }
    }
}

impl std::fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sent => f.write_str("sent"),
            Self::Delivered => f.write_str("delivered"),
            Self::Read => f.write_str("read"),
            Self::Played => f.write_str("played"),
            Self::Failed => f.write_str("failed"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// Create a new event bus (sender + initial receiver).
pub fn new_event_bus() -> (broadcast::Sender<Arc<BridgeEvent>>, broadcast::Receiver<Arc<BridgeEvent>>) {
    broadcast::channel(EVENT_BUS_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_bus_send_receive() {
        let (tx, mut rx) = new_event_bus();
        tx.send(Arc::new(BridgeEvent::Heartbeat)).unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(matches!(*evt, BridgeEvent::Heartbeat));
    }

    #[tokio::test]
    async fn test_outbound_status_serde() {
        let evt = OutboundStatusEvent {
            job_id: 42,
            state: OutboundJobState::Sent,
            wa_message_id: Some("ABC123".to_string()),
            error: None,
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"state\":\"sent\""));
        assert!(json.contains("\"job_id\":42"));
    }
}
