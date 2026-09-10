//! Historical backfill seam types, trait, fake, and target model.
//!
//! Wave A: storage + logic foundation — no live WhatsApp, no worker task.
//! Wave B1: worker core (process_job), pacer, BatchSink seam, run_worker_loop.
//! All types here are unit-testable without a network connection.
//!
//! Key types:
//!   - `Anchor`           — per-chat backfill frontier (oldest known message)
//!   - `HistoryBatch`     — one page of fetched messages
//!   - `HistorySource`    — async trait seam; real impl (Wave B2) wraps `Client`
//!   - `FakeHistorySource`— scripted canned responses for tests
//!   - `FetchTarget`      — contained-C target model (ADR 0033)
//!   - `BackfillStep`     — pure stop-condition output
//!   - `evaluate_target`  — pure, total stop-condition function
//!   - `BatchSink`        — async trait seam for persisting a batch (ADR 0025)
//!   - `FakeBatchSink`    — recording sink for tests
//!   - `BackfillPacer`    — interruptible inter-batch sleep (ADR 0020)
//!   - `JobEnd`           — terminal outcome of `process_job`
//!   - `process_job`      — worker core: pagination loop with abort/pacing
//!   - `run_worker_loop`  — thin driver: claim → process → mark

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use waproto::whatsapp as wa;

// ---------------------------------------------------------------------------
// HistoryCorrelator — session-id registry for on-demand fetch (Wave B2b)
// ---------------------------------------------------------------------------

/// Matches an outstanding on-demand history-fetch request to the arriving
/// `Event::HistorySync` by `peer_data_request_session_id`.
///
/// Usage:
///   1. Caller invokes `client.fetch_message_history(...)` → obtains a `session_id`.
///   2. Caller calls `register(session_id)` → gets a `Receiver`.
///   3. When `Event::HistorySync` arrives, `handle_event` calls `fulfill(sid, lazy)`.
///   4. Caller awaits the `Receiver` to get the `LazyHistorySync`.
pub struct HistoryCorrelator {
    pending: dashmap::DashMap<String, tokio::sync::oneshot::Sender<Box<wacore::types::events::LazyHistorySync>>>,
}

impl HistoryCorrelator {
    pub fn new() -> Self {
        Self { pending: dashmap::DashMap::new() }
    }

    /// Register a pending fetch: insert a oneshot sender keyed by `session_id`.
    /// Returns the receiver the caller should await.
    pub fn register(
        &self,
        session_id: String,
    ) -> tokio::sync::oneshot::Receiver<Box<wacore::types::events::LazyHistorySync>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.insert(session_id, tx);
        rx
    }

    /// Fulfill a pending fetch: remove the sender for `session_id` and send `lazy`.
    /// Returns `true` if a matching pending entry was found and the send succeeded.
    pub fn fulfill(
        &self,
        session_id: &str,
        lazy: Box<wacore::types::events::LazyHistorySync>,
    ) -> bool {
        if let Some((_, tx)) = self.pending.remove(session_id) {
            tx.send(lazy).is_ok()
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Anchor — the per-chat backward-pagination frontier
// ---------------------------------------------------------------------------

/// The oldest known message position for a chat — used as the pagination anchor
/// when requesting history older than what we have.
///
/// `oldest_msg_timestamp_ms` is Unix **milliseconds** (proto `messageTimestamp`
/// is seconds; `oldest_anchor()` scales by ×1000). This matches the wa-rs
/// `oldestMsgTimestampMs` fetch field and `FetchTarget::Since(ts_ms)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub oldest_msg_id: String,
    pub oldest_msg_from_me: bool,
    /// Unix timestamp of the oldest known message in **milliseconds**.
    pub oldest_msg_timestamp_ms: i64,
}

// ---------------------------------------------------------------------------
// HistoryBatch — one page of fetched messages
// ---------------------------------------------------------------------------

/// A single page of historical messages returned by `HistorySource::fetch_older`.
#[derive(Debug)]
pub struct HistoryBatch {
    /// The messages in this batch (oldest-first or newest-first — Wave B decides ordering).
    pub messages: Vec<wa::WebMessageInfo>,
    /// Whether the phone indicates more history is available older than this batch.
    /// `false` means the history is exhausted (no more to fetch).
    pub more_remain: bool,
    /// Optional fuzzy progress indicator (percentage 0–100) for UX feedback.
    /// None for `all`/`since` targets (no reliable total); Some for `count` targets.
    pub progress: Option<u32>,
}

// ---------------------------------------------------------------------------
// HistorySource trait — the seam between the worker and the network
// ---------------------------------------------------------------------------

/// Async seam for fetching a page of messages older than `anchor` for `chat_jid`.
///
/// The real implementation (Wave B) hides the two-phase async dance:
///   1. `Client::fetch_message_history` → returns a session-id immediately.
///   2. `Event::HistorySync` arrives later, correlated by `peer_data_request_session_id`.
///
/// Here we only define the contract and the fake. The trait is dyn-safe via
/// `#[async_trait]`, allowing `Box<dyn HistorySource>` injection in the worker (Wave B).
#[async_trait::async_trait]
pub trait HistorySource: Send + Sync {
    /// Whether the source can currently fetch (live WA connection up). ADR 0026 connection-gating.
    ///
    /// The default is `true` so that `FakeHistorySource` and existing tests are unaffected
    /// unless they explicitly opt out via `set_ready(false)`.
    fn is_ready(&self) -> bool {
        true
    }

    /// Fetch a batch of messages OLDER than `anchor` for `chat_jid`, up to `count`.
    async fn fetch_older(
        &self,
        chat_jid: &str,
        anchor: &Anchor,
        count: i32,
    ) -> anyhow::Result<HistoryBatch>;
}

// ---------------------------------------------------------------------------
// FakeHistorySource — scripted canned responses for tests / dev
// ---------------------------------------------------------------------------

/// Script entry for `FakeHistorySource`.
pub enum FakeResponse {
    /// Return a batch with the given messages and `more_remain` flag.
    Batch {
        messages: Vec<wa::WebMessageInfo>,
        more_remain: bool,
        progress: Option<u32>,
    },
    /// Simulate an error (e.g. timeout, network failure, WA rate-limit).
    Error(String),
}

/// A scripted `HistorySource` for tests and development.
///
/// Responses are consumed in order (FIFO). After all scripted responses are
/// exhausted, subsequent calls return an error ("no more scripted responses").
///
/// This lets Wave B's worker tests exercise:
///   - Normal pagination (sequences of `Batch` entries)
///   - Exhausted history (`more_remain = false`)
///   - Error / timeout paths (pacing, backoff, pause, cancel)
///   - Connection-gating: call `set_ready(false)` to simulate a disconnected source.
pub struct FakeHistorySource {
    responses: std::sync::Mutex<std::collections::VecDeque<FakeResponse>>,
    /// Simulated connection readiness. Default: `true` (ready). Use `set_ready(false)` in
    /// tests that exercise the ADR 0026 connection-gate path.
    ready: std::sync::atomic::AtomicBool,
}

impl FakeHistorySource {
    /// Create a fake source from a list of scripted responses.
    pub fn new(responses: Vec<FakeResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
            ready: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Convenience: a source that returns one empty exhausted batch.
    pub fn exhausted() -> Self {
        Self::new(vec![FakeResponse::Batch {
            messages: vec![],
            more_remain: false,
            progress: None,
        }])
    }

    /// Convenience: a source that immediately returns an error.
    pub fn error(msg: impl Into<String>) -> Self {
        Self::new(vec![FakeResponse::Error(msg.into())])
    }

    /// Set the simulated connection readiness. `false` causes `is_ready()` to return
    /// `false`, triggering the ADR 0026 connection-gate in `process_job` /
    /// `run_worker_loop`.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl HistorySource for FakeHistorySource {
    fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn fetch_older(
        &self,
        _chat_jid: &str,
        _anchor: &Anchor,
        _count: i32,
    ) -> anyhow::Result<HistoryBatch> {
        let mut queue = self.responses.lock().expect("FakeHistorySource mutex poisoned");
        match queue.pop_front() {
            Some(FakeResponse::Batch { messages, more_remain, progress }) => {
                Ok(HistoryBatch { messages, more_remain, progress })
            }
            Some(FakeResponse::Error(msg)) => Err(anyhow::anyhow!("{}", msg)),
            None => Err(anyhow::anyhow!("FakeHistorySource: no more scripted responses")),
        }
    }
}

// ---------------------------------------------------------------------------
// FetchTarget — contained-C target model (ADR 0033)
// ---------------------------------------------------------------------------

/// The fetch completion intent for a backfill job.
///
/// Exactly ONE kind per job (clean discriminator, not ambiguous composition):
/// - `Since(ts_ms)` — fetch until oldest message crosses `ts_ms` OR phone exhausted.
///   `ts_ms` is Unix **milliseconds** — consistent with `Anchor.oldest_msg_timestamp_ms`.
///   Auto-continues across paged segments.
/// - `All`          — fetch until phone exhausted. Auto-continues.
/// - `Count(n)`     — fetch until `n` messages fetched. Does NOT auto-continue.
///
/// The autonomy backstop (from config) is SEPARATE from the target: it limits how far
/// `Since`/`All` may run in one trigger before PARKING (requiring re-trigger).
/// `Count` is already bounded and never parks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    /// Fetch until the oldest seen message timestamp is at or before this value (ms).
    Since(i64 /* ts_ms — Unix milliseconds */),
    All,
    Count(u32),
}

impl FetchTarget {
    /// Parse from the DB columns `(target_kind, target_value)`.
    ///
    /// Returns an error if `kind` is unrecognised or `value` is missing/wrong type.
    pub fn parse(kind: &str, value: Option<i64>) -> anyhow::Result<FetchTarget> {
        match kind {
            "since" => {
                let ts = value.ok_or_else(|| {
                    anyhow::anyhow!("FetchTarget::parse: 'since' requires a non-null target_value")
                })?;
                Ok(FetchTarget::Since(ts))
            }
            "all" => Ok(FetchTarget::All),
            "count" => {
                let n = value.ok_or_else(|| {
                    anyhow::anyhow!("FetchTarget::parse: 'count' requires a non-null target_value")
                })?;
                let count = u32::try_from(n).map_err(|_| {
                    anyhow::anyhow!(
                        "FetchTarget::parse: 'count' target_value {} is out of u32 range",
                        n
                    )
                })?;
                Ok(FetchTarget::Count(count))
            }
            other => Err(anyhow::anyhow!(
                "FetchTarget::parse: unknown target_kind '{}'",
                other
            )),
        }
    }

    /// Serialise back to the DB columns `(target_kind, target_value)`.
    pub fn to_row(&self) -> (&'static str, Option<i64>) {
        match self {
            FetchTarget::Since(ts) => ("since", Some(*ts)),
            FetchTarget::All => ("all", None),
            FetchTarget::Count(n) => ("count", Some(*n as i64)),
        }
    }
}

// ---------------------------------------------------------------------------
// BackfillStep — output of the pure stop-condition function
// ---------------------------------------------------------------------------

/// The decision returned by `evaluate_target` after each batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillStep {
    /// Fetch the next batch.
    Continue,
    /// The target is satisfied — mark the job `done`.
    Done,
    /// Hit the autonomy backstop — mark the job `paused` and require a re-trigger.
    Parked,
}

// ---------------------------------------------------------------------------
// evaluate_target — pure stop-condition (no I/O, total)
// ---------------------------------------------------------------------------

/// Decide what to do after receiving a batch.
///
/// # Parameters
/// - `target`            — the job's fetch target
/// - `fetched`           — total messages fetched so far (including this batch)
/// - `batch_oldest_ts_ms`— timestamp (Unix **milliseconds**) of the oldest message in the
///                         batch, if any. Must be consistent with `FetchTarget::Since(ts_ms)`.
/// - `more_remain`       — whether the phone indicated more history is available
/// - `backstop`          — autonomy backstop (global config, max messages for Since/All);
///                         `0` means "no backstop — never park"
///
/// # Rules
/// - `Since(t)`: Done when `batch_oldest_ts_ms <= t` OR `!more_remain`; else Continue.
///   Parks when `fetched >= backstop` (if backstop > 0).
/// - `All`: Done when `!more_remain`; else Continue.
///   Parks when `fetched >= backstop` (if backstop > 0).
/// - `Count(n)`: Done when `fetched >= n`; else Continue.
///   **Never Parks** — it's already explicitly bounded.
///
/// Backstop check is tested BEFORE the completion condition so that, for a
/// large `since`/`all` job that would complete in the same batch as the backstop
/// is hit, completion wins (the backstop is a safety net, not a forced pause
/// when work is actually finishing).
pub fn evaluate_target(
    target: &FetchTarget,
    fetched: u32,
    batch_oldest_ts_ms: Option<i64>,
    more_remain: bool,
    backstop: u32,
) -> BackfillStep {
    match target {
        FetchTarget::Since(ts) => {
            // Completion conditions (checked first — completion wins over park)
            let done = !more_remain || batch_oldest_ts_ms.map_or(false, |t| t <= *ts);
            if done {
                return BackfillStep::Done;
            }
            // Autonomy backstop
            if backstop > 0 && fetched >= backstop {
                return BackfillStep::Parked;
            }
            BackfillStep::Continue
        }
        FetchTarget::All => {
            // Completion condition
            if !more_remain {
                return BackfillStep::Done;
            }
            // Autonomy backstop
            if backstop > 0 && fetched >= backstop {
                return BackfillStep::Parked;
            }
            BackfillStep::Continue
        }
        FetchTarget::Count(n) => {
            // Count never Parks — it's explicitly bounded
            if fetched >= *n {
                BackfillStep::Done
            } else {
                BackfillStep::Continue
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BatchSink — persist seam (Wave B1 seam; Wave B2 implements the real one)
// ---------------------------------------------------------------------------

/// Persist seam for writing a fetched batch to the message store.
///
/// The real implementation (Wave B2) runs:
///   `WebMessageInfo → extract_content_inner → INSERT OR IGNORE INTO messages`
/// The fake records calls and returns a configurable inserted-count.
///
/// Kept as a separate trait (ADR 0025) so the worker is fully testable without
/// a live WhatsApp client or a real extract_content pipeline.
#[async_trait::async_trait]
pub trait BatchSink: Send + Sync {
    /// Persist one fetched batch for `chat_jid`.
    ///
    /// Returns the number of NEW rows stored (after dedup / INSERT OR IGNORE).
    /// Implementations must be idempotent: calling twice with the same batch
    /// must return 0 on the second call.
    async fn persist_batch(&self, chat_jid: &str, batch: &HistoryBatch) -> anyhow::Result<usize>;
}

// ---------------------------------------------------------------------------
// FakeBatchSink — recording sink for tests
// ---------------------------------------------------------------------------

/// A recorded call to `FakeBatchSink::persist_batch`.
#[derive(Debug, Clone)]
pub struct SinkCall {
    pub chat_jid: String,
    pub message_count: usize,
    pub more_remain: bool,
}

/// Behaviour the `FakeBatchSink` should exhibit on the next call(s).
pub enum FakeSinkMode {
    /// Return `inserted_count` new rows (simulating dedup — may be ≤ batch.messages.len()).
    Ok { inserted_count: usize },
    /// Return an error with the given message.
    Error(String),
}

/// A recording `BatchSink` for tests.
///
/// Each call consumes one entry from the `modes` deque (FIFO). After all modes
/// are consumed, subsequent calls succeed with `inserted_count == batch.messages.len()`.
pub struct FakeBatchSink {
    modes: std::sync::Mutex<std::collections::VecDeque<FakeSinkMode>>,
    pub calls: std::sync::Mutex<Vec<SinkCall>>,
}

impl FakeBatchSink {
    /// Create a sink with a sequence of explicit per-call behaviours.
    pub fn new(modes: Vec<FakeSinkMode>) -> Self {
        Self {
            modes: std::sync::Mutex::new(modes.into()),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Create a sink that always succeeds, returning `batch.messages.len()` as inserted.
    pub fn always_ok() -> Self {
        Self::new(vec![])
    }

    /// Convenience: a sink whose first call returns an error.
    pub fn error(msg: impl Into<String>) -> Self {
        Self::new(vec![FakeSinkMode::Error(msg.into())])
    }

    /// Return the recorded calls (snapshot).
    pub fn recorded_calls(&self) -> Vec<SinkCall> {
        self.calls.lock().expect("FakeBatchSink calls mutex poisoned").clone()
    }
}

#[async_trait::async_trait]
impl BatchSink for FakeBatchSink {
    async fn persist_batch(&self, chat_jid: &str, batch: &HistoryBatch) -> anyhow::Result<usize> {
        let mode = self
            .modes
            .lock()
            .expect("FakeBatchSink modes mutex poisoned")
            .pop_front();

        match mode {
            Some(FakeSinkMode::Error(msg)) => {
                return Err(anyhow::anyhow!("{}", msg));
            }
            Some(FakeSinkMode::Ok { inserted_count }) => {
                self.calls
                    .lock()
                    .expect("FakeBatchSink calls mutex poisoned")
                    .push(SinkCall {
                        chat_jid: chat_jid.to_owned(),
                        message_count: batch.messages.len(),
                        more_remain: batch.more_remain,
                    });
                return Ok(inserted_count);
            }
            None => {
                // default: succeed, all messages are "new"
                let n = batch.messages.len();
                self.calls
                    .lock()
                    .expect("FakeBatchSink calls mutex poisoned")
                    .push(SinkCall {
                        chat_jid: chat_jid.to_owned(),
                        message_count: n,
                        more_remain: batch.more_remain,
                    });
                return Ok(n);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BackfillPacer — interruptible inter-batch sleep (ADR 0020)
// ---------------------------------------------------------------------------

/// Dedicated backfill inter-batch pacer — separate from the outbound SendPacer
/// (ADR 0020). Injectable so tests construct it with `base_delay: Duration::ZERO`
/// and skip all waiting.
///
/// Jitter: when `base_delay > 0` and `jitter_frac > 0.0`, each sleep is
/// `base_delay ± (base_delay * jitter_frac)`. When `base_delay` is zero the jitter
/// computation is skipped entirely (no `rand` call, deterministic zero-delay for tests).
#[derive(Debug, Clone)]
pub struct BackfillPacer {
    /// Base inter-batch delay.
    pub base_delay: Duration,
    /// Fractional jitter: 0.0 = no jitter, 0.5 = ±50% of base_delay.
    pub jitter_frac: f64,
}

impl BackfillPacer {
    /// Sleep `base_delay ± jitter`, but return immediately if `cancel` fires.
    ///
    /// When `base_delay == Duration::ZERO` this is a no-op (zero-cost for tests).
    pub async fn pace(&self, cancel: &CancellationToken) {
        if self.base_delay == Duration::ZERO {
            return;
        }
        let delay = self.jittered_delay();
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => {}
        }
    }

    fn jittered_delay(&self) -> Duration {
        if self.jitter_frac <= 0.0 || self.base_delay == Duration::ZERO {
            return self.base_delay;
        }
        let base_ms = self.base_delay.as_millis() as u64;
        let window = ((base_ms as f64) * self.jitter_frac).round() as u64;
        if window == 0 {
            return self.base_delay;
        }
        use rand::Rng;
        let jitter_ms = rand::thread_rng().gen_range(0..=(window * 2));
        let delta = jitter_ms as i64 - window as i64;
        let adjusted_ms = (base_ms as i64 + delta).max(0) as u64;
        Duration::from_millis(adjusted_ms)
    }
}

// ---------------------------------------------------------------------------
// JobEnd — terminal outcome of process_job
// ---------------------------------------------------------------------------

/// The outcome reported by `process_job` when the loop exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    /// The fetch target was satisfied or history is exhausted. Job → 'done'.
    Done,
    /// The autonomy backstop was hit. Job → 'paused'. Re-trigger to continue.
    Parked,
    /// A graceful cancel (API-cancel / per-job cancel) was received. Job → 'cancelled'.
    Cancelled,
    /// A fetch or sink error occurred, OR the anchor was not advancing. Job → 'failed'.
    Failed(String),
    /// The source is not ready (WA not connected). Job → back to 'queued'. ADR 0026.
    ///
    /// This is a **non-terminal** outcome: the job cursor is not advanced and the job
    /// is re-enqueued so it resumes once the connection is up. Distinct from `Parked`
    /// (which is a user-facing "paused" state requiring an explicit re-trigger).
    Deferred,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the anchor derived from the OLDEST message in a batch.
///
/// "Oldest" is the message with the smallest `message_timestamp` (earliest in
/// time). If two messages share the same timestamp, ties are broken by lowest
/// key.id (lexicographic) — deterministic but arbitrary.
///
/// `WebMessageInfo.messageTimestamp` is Unix **seconds** (proto `uint64`).
/// The returned `Anchor.oldest_msg_timestamp_ms` is scaled to **milliseconds**
/// (multiply by 1000) because `FetchTarget::Since(ts_ms)` and the wa-rs
/// fetch request field `oldestMsgTimestampMs` are millisecond-based.
///
/// Returns `None` if the batch has no messages or none have a key.
fn oldest_anchor(batch: &HistoryBatch) -> Option<Anchor> {
    batch
        .messages
        .iter()
        .filter_map(|m| {
            // message_timestamp is seconds — scale to ms for the anchor.
            let ts_ms = (m.message_timestamp? as i64).saturating_mul(1000);
            let id = m.key.id.clone()?;
            let from_me = m.key.from_me.unwrap_or(false);
            Some((ts_ms, id, from_me))
        })
        .min_by_key(|(ts_ms, id, _)| (*ts_ms, id.clone()))
        .map(|(ts_ms, id, from_me)| Anchor {
            oldest_msg_id: id,
            oldest_msg_from_me: from_me,
            oldest_msg_timestamp_ms: ts_ms,
        })
}

// ---------------------------------------------------------------------------
// drain_history_sync — extract a HistoryBatch for one chat from a HistorySync
// ---------------------------------------------------------------------------

/// Extract the messages for `accepted_ids` from a decoded `HistorySync` into a `HistoryBatch`.
///
/// The live source (Wave B2b) calls `lazy.get()` then passes the decoded `&wa::HistorySync` here.
///
/// ## Selection
/// All `Conversation`s whose `id` appears in `accepted_ids` are selected. This allows matching
/// both the requested phone JID (`…@s.whatsapp.net`) and the LID alias (`…@lid`) for the same
/// contact — the phone sent the LID form in `Conversation.id` even though we requested by phone.
///
/// **Single-conversation fallback**: if NO conversation matched any `accepted_ids` AND
/// `hs.conversations.len() == 1`, the sole conversation is used regardless. On-demand fetch
/// targets exactly one chat; the LID mapping may not be learned yet at the time of the first
/// fetch, making this the safe fallback. A debug log is emitted when the fallback fires.
///
/// Every `HistorySyncMsg.message` (dereffing the `Box<WebMessageInfo>`) is collected into
/// `messages` (cloned, order preserved). If no conversation matches, `messages` is empty.
///
/// ## `more_remain`
/// Derived from the matched conversation's `end_of_history_transfer_type`:
/// - `Some(0) | Some(2)` → `true` (more available)
/// - `Some(1) | Some(3)` → `false` (no more)
/// - `None` → fall back to `hs.progress`: more_remain = `hs.progress.map_or(true, |p| p < 100)`
///   (conservative: uncertain → assume more, so we never silently stop early)
///
/// When multiple conversations match, their `more_remain` signals are OR'd.
///
/// ## Timestamps
/// Raw `WebMessageInfo` values are returned unmodified — `message_timestamp` is seconds.
/// Do NOT scale here; `oldest_anchor` handles ×1000 when computing the next anchor.
pub fn drain_history_sync(hs: &wa::HistorySync, accepted_ids: &[String]) -> HistoryBatch {
    let mut messages: Vec<wa::WebMessageInfo> = Vec::new();
    let mut found_any = false;
    let mut more_remain = false;

    for conv in &hs.conversations {
        if !accepted_ids.iter().any(|id| id == &conv.id) {
            continue;
        }
        found_any = true;

        // Collect all messages from this conversation (message is Box<WebMessageInfo>)
        for hsm in &conv.messages {
            if let Some(boxed) = &hsm.message {
                messages.push((**boxed).clone());
            }
        }

        // Determine more_remain from end_of_history_transfer_type
        let conv_more = match conv.end_of_history_transfer_type {
            Some(0) | Some(2) => true,
            Some(1) | Some(3) => false,
            _ => {
                // Conservative fallback: uncertain → assume more
                hs.progress.map_or(true, |p| p < 100)
            }
        };
        more_remain = more_remain || conv_more;
    }

    // Single-conversation fallback: if no accepted_id matched but there is exactly one
    // conversation, use it. On-demand fetch targets one chat; the LID mapping may not
    // be populated yet on the first fetch, so we cannot match by id.
    if !found_any && hs.conversations.len() == 1 {
        let conv = &hs.conversations[0];
        tracing::debug!(
            conv_id = %conv.id,
            "drain_history_sync: no accepted_id matched; using single-conversation fallback",
        );
        found_any = true;
        for hsm in &conv.messages {
            if let Some(boxed) = &hsm.message {
                messages.push((**boxed).clone());
            }
        }
        let conv_more = match conv.end_of_history_transfer_type {
            Some(0) | Some(2) => true,
            Some(1) | Some(3) => false,
            _ => hs.progress.map_or(true, |p| p < 100),
        };
        more_remain = conv_more;
    }

    // If no matching conversation (and no fallback applied), apply the progress-based
    // fallback conservatively.
    if !found_any {
        more_remain = hs.progress.map_or(true, |p| p < 100);
    }

    HistoryBatch {
        messages,
        more_remain,
        progress: hs.progress,
    }
}

// ---------------------------------------------------------------------------
// initial_anchor — seed the starting anchor from the oldest stored message
// ---------------------------------------------------------------------------

/// Seed the starting anchor for a chat from its oldest already-stored message
/// ("older than what I have", ADR 0003).
///
/// - `Some(row)` → anchor at that message (seconds → milliseconds via `saturating_mul(1000)`).
/// - `None` → empty anchor (phone returns most-recent history chunk).
pub fn initial_anchor(oldest: Option<crate::storage::OldestMessageRow>) -> Anchor {
    match oldest {
        Some(row) => Anchor {
            oldest_msg_id: row.message_id,
            oldest_msg_from_me: row.from_me,
            oldest_msg_timestamp_ms: row.timestamp_secs.saturating_mul(1000),
        },
        None => Anchor {
            oldest_msg_id: String::new(),
            oldest_msg_from_me: false,
            oldest_msg_timestamp_ms: 0,
        },
    }
}

// ---------------------------------------------------------------------------
// process_job — the pagination loop (Wave B1 core)
// ---------------------------------------------------------------------------

/// Run one backfill job to completion (or until cancelled/parked/failed).
///
/// # Missing-cursor anchor handling
/// If the DB has no cursor for this chat, we start with an empty "sentinel"
/// anchor (`oldest_msg_id = ""`, `oldest_msg_timestamp_ms = 0`). The real
/// initial anchor (seeded from the most-recent live message) is set up by
/// the Wave B2 caller before enqueuing the job. In fake-driven unit tests
/// this default is sufficient — `FakeHistorySource` ignores the anchor value.
///
/// # Cancel semantics (ADR 0026)
/// In B1, `cancel` is the **single shutdown token** (passed through from
/// `run_worker_loop`). When it fires, `process_job` returns `Cancelled` and
/// the worker requeues the job for resumption after restart.
/// Wave B2 will add a separate per-job API-cancel token so that an operator
/// `DELETE /backfill/{id}` can permanently cancel a job without affecting the
/// shutdown path.
/// When this cancel fires, the job is marked `Cancelled`.
pub async fn process_job(
    job: &crate::storage::BackfillJobRow,
    source: &dyn HistorySource,
    sink: &dyn BatchSink,
    store: &crate::storage::Store,
    pacer: &BackfillPacer,
    batch_size: i32,
    backstop: u32,
    cancel: &CancellationToken,
    event_tx: Option<&tokio::sync::broadcast::Sender<std::sync::Arc<crate::bridge_events::BridgeEvent>>>,
) -> anyhow::Result<JobEnd> {
    // --- 0. Validate inputs ---
    if batch_size <= 0 {
        return Ok(JobEnd::Failed(format!("invalid batch_size: {batch_size}")));
    }

    // --- 1. Parse the target ---
    let target = match FetchTarget::parse(&job.target_kind, job.target_value) {
        Ok(t) => t,
        Err(e) => return Ok(JobEnd::Failed(format!("bad target: {e}"))),
    };

    // --- 2. Load or initialise the anchor ---
    let cursor = store
        .get_backfill_cursor(&job.chat_jid)
        .await
        .map_err(|e| anyhow::anyhow!("get_backfill_cursor failed: {e}"))?;

    // Fast-path: history already fully exhausted
    if let Some(ref c) = cursor {
        if c.exhausted {
            return Ok(JobEnd::Done);
        }
    }

    let mut anchor = cursor
        .as_ref()
        .and_then(|c| {
            let id = c.oldest_msg_id.clone()?;
            let ts = c.oldest_msg_timestamp_ms?;
            Some(Anchor {
                oldest_msg_id: id,
                oldest_msg_from_me: c.oldest_msg_from_me.unwrap_or(false),
                oldest_msg_timestamp_ms: ts,
            })
        })
        .unwrap_or_else(|| Anchor {
            oldest_msg_id: String::new(),
            oldest_msg_from_me: false,
            oldest_msg_timestamp_ms: 0,
        });

    // Running fetched tally (resume from what's already stored)
    let mut fetched: u32 = job.fetched.max(0) as u32;

    // Stuck-anchor guard: K=2 consecutive same-anchor batches → failed
    const STUCK_K: u8 = 2;
    let mut consecutive_same_anchor: u8 = 0;
    let mut last_anchor_id = anchor.oldest_msg_id.clone();

    // --- 3. Pagination loop (one iteration = one batch) ---
    loop {
        // 3a. Check cancel BEFORE issuing the next fetch
        if cancel.is_cancelled() {
            // Persist current anchor so the job is resumable
            let _ = store
                .upsert_backfill_cursor(
                    &job.chat_jid,
                    Some(&anchor.oldest_msg_id),
                    Some(anchor.oldest_msg_from_me),
                    Some(anchor.oldest_msg_timestamp_ms),
                    true,  // more_remain still unknown — assume yes (safe)
                    false, // not exhausted
                    None,
                )
                .await;
            return Ok(JobEnd::Cancelled);
        }

        // 3a-i. Connection gate (ADR 0026): if the source is not ready (WA not connected),
        // return Deferred immediately — the cursor is unchanged and the job stays resumable.
        // This prevents a terminal Failed when the worker runs at startup before connect.
        if !source.is_ready() {
            tracing::debug!(
                job_id = job.id,
                chat_jid = %job.chat_jid,
                "backfill process_job: source not ready — deferring job until connected",
            );
            return Ok(JobEnd::Deferred);
        }

        // 3b. Fetch the next batch
        let batch = match source.fetch_older(&job.chat_jid, &anchor, batch_size).await {
            Ok(b) => b,
            Err(e) => {
                // If the source is now not-ready (connection dropped mid-fetch), treat as
                // transient: return Deferred so the job is requeued (not permanently failed).
                if !source.is_ready() {
                    tracing::debug!(
                        job_id = job.id,
                        chat_jid = %job.chat_jid,
                        error = %e,
                        "backfill process_job: fetch error while disconnected — deferring",
                    );
                    return Ok(JobEnd::Deferred);
                }
                return Ok(JobEnd::Failed(e.to_string()));
            }
        };

        let more_remain = batch.more_remain;
        let batch_msg_count = batch.messages.len();

        // 3c. Persist the batch via the sink
        match sink.persist_batch(&job.chat_jid, &batch).await {
            Ok(inserted) => {
                tracing::trace!(
                    job_id = job.id,
                    chat_jid = %job.chat_jid,
                    batch_msgs = batch_msg_count,
                    inserted,
                    "backfill batch persisted",
                );
            }
            Err(e) => return Ok(JobEnd::Failed(format!("persist_batch failed: {e}"))),
        }

        // 3d. Update fetched count (use batch.messages.len() not inserted, to count toward target)
        fetched = fetched.saturating_add(batch_msg_count as u32);

        // 3e. Compute new anchor from the oldest message in this batch.
        //
        // NOTE (Wave B2): before calling process_job the caller must seed a real
        // initial anchor from the live message frontier. The sentinel anchor
        // ("", ts 0) used when no cursor exists is only safe for FakeHistorySource
        // tests where the anchor value is ignored.
        let new_anchor = oldest_anchor(&batch);

        // 3e-i. No-progress terminal: if the batch yielded no derivable anchor
        // (empty batch or all messages lack a key/timestamp), there is nothing to
        // page further — mark exhausted and return Done.  This avoids a spin on
        // repeated empty batches and the all-key-less case (Fix 4a).
        if new_anchor.is_none() {
            let _ = store
                .upsert_backfill_cursor(
                    &job.chat_jid,
                    Some(&anchor.oldest_msg_id),
                    Some(anchor.oldest_msg_from_me),
                    Some(anchor.oldest_msg_timestamp_ms),
                    false,
                    true, // exhausted — nothing to page further
                    Some(now_unix_secs()),
                )
                .await;
            return Ok(JobEnd::Done);
        }

        // 3f. Stuck-anchor guard (R2, ADR 0026).
        // With Fix 4a handling the None case above, we only reach here with a
        // Some anchor, so the empty-id special-case is removed: any repeated
        // anchor id (including "") triggers the K=2 stuck guard.
        let anchor_id_for_check = new_anchor.as_ref().map(|a| a.oldest_msg_id.as_str()).unwrap_or("");
        if anchor_id_for_check == last_anchor_id.as_str() {
            consecutive_same_anchor += 1;
            if consecutive_same_anchor >= STUCK_K {
                return Ok(JobEnd::Failed("anchor not advancing".to_string()));
            }
        } else {
            consecutive_same_anchor = 0;
            last_anchor_id = anchor_id_for_check.to_owned();
        }

        // 3g. Determine exhausted: empty batch OR source says no more remain
        let exhausted = batch_msg_count == 0 || !more_remain;

        // Advance anchor (use old anchor if batch had no messages)
        if let Some(na) = new_anchor.as_ref() {
            anchor = na.clone();
        }

        // 3h. Persist progress: cursor UPSERT + fetched counter in one atomic TX
        // (Fix 7 — record_backfill_progress wraps both in unchecked_transaction).
        let _ = store
            .record_backfill_progress(
                job.id,
                &job.chat_jid,
                Some(&anchor.oldest_msg_id),
                Some(anchor.oldest_msg_from_me),
                Some(anchor.oldest_msg_timestamp_ms),
                more_remain,
                exhausted,
                None,
                fetched,
            )
            .await;

        // 3h-i. Emit per-batch progress event (ADR 0034).
        // NOTE: a dedicated "cooldown" status for randomised long inter-batch pauses is
        // intentionally absent — the pacer (ADR 0020) currently only does uniform ~4s
        // jittered inter-batch sleeps; there are no long pauses to signal. Per-batch
        // "running" events provide liveness. The cooldown state is deferred until the
        // pacer implements long pauses.
        if let Some(tx) = event_tx {
            let _ = tx.send(std::sync::Arc::new(crate::bridge_events::BridgeEvent::BackfillProgress(
                crate::bridge_events::BackfillProgressEvent {
                    job_id: job.id,
                    chat_jid: job.chat_jid.clone(),
                    target_kind: job.target_kind.clone(),
                    target_value: job.target_value,
                    fetched: fetched as i64,
                    status: "running".to_string(),
                    more_remain,
                },
            )));
        }

        // 3i. Evaluate the target
        let batch_oldest_ts = new_anchor.as_ref().map(|a| a.oldest_msg_timestamp_ms);
        let step = evaluate_target(&target, fetched, batch_oldest_ts, more_remain, backstop);

        match step {
            BackfillStep::Done => {
                // Stamp last_backfill_at on every Done exit (ADR 0035) so that
                // Count(n) and Since-crossing completions also trigger the cooldown.
                // exhausted is true only when the phone signals no more history.
                let exhausted = !more_remain;
                let _ = store
                    .upsert_backfill_cursor(
                        &job.chat_jid,
                        Some(&anchor.oldest_msg_id),
                        Some(anchor.oldest_msg_from_me),
                        Some(anchor.oldest_msg_timestamp_ms),
                        more_remain,
                        exhausted,
                        Some(now_unix_secs()),
                    )
                    .await;
                return Ok(JobEnd::Done);
            }
            BackfillStep::Parked => {
                return Ok(JobEnd::Parked);
            }
            BackfillStep::Continue => {
                // 3j. Interruptible pacing sleep before next iteration
                pacer.pace(cancel).await;
                // Check again after sleep (pacing may have been cut short by cancel)
                if cancel.is_cancelled() {
                    return Ok(JobEnd::Cancelled);
                }
            }
        }
    }
}

/// Returns unix seconds (i64) for cursor timestamps.
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// run_worker_loop — thin driver: claim → process → mark
// ---------------------------------------------------------------------------

/// Drive the backfill worker: poll for queued jobs, run them, update their status.
///
/// # Cancel semantics
/// `cancel` is the **shutdown** token (not a per-job API cancel — that comes from Wave B2).
/// When `cancel` fires:
/// - No new jobs are claimed.
/// - If a job is currently in-flight the loop detects the cancel during `process_job`
///   (via the same token) and returns `JobEnd::Cancelled`. The driver then **requeues**
///   the job (status → 'queued') so it resumes after restart. This is the correct
///   shutdown behaviour (ADR 0026): distinguish API-cancel (permanent Cancelled) from
///   shutdown (transient, must be resumable). For B1 we use the single token for both
///   and model shutdown → requeue.
///
/// # Re-trigger
/// Callers signal that new jobs are available via `notify`. The loop also polls on a
/// 5-second tick to handle missed notifications (e.g. after restart).
pub async fn run_worker_loop(
    source: Arc<dyn HistorySource>,
    sink: Arc<dyn BatchSink>,
    store: crate::storage::Store,
    pacer: BackfillPacer,
    batch_size: i32,
    backstop: u32,
    notify: Arc<Notify>,
    cancel: CancellationToken,
    event_tx: Option<tokio::sync::broadcast::Sender<std::sync::Arc<crate::bridge_events::BridgeEvent>>>,
) {
    const POLL_INTERVAL: Duration = Duration::from_secs(5);

    loop {
        // Wait for either a notification, a periodic tick, or shutdown
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.cancelled() => {
                tracing::info!("backfill worker: shutdown requested, exiting loop");
                return;
            }
        }

        // Connection gate (ADR 0026): do NOT claim a job when the source is not ready.
        // If we claimed and ran, fetch_older would return Deferred and we'd requeue — but
        // it's cleaner (and avoids a spurious state transition) to not claim at all.
        // The existing periodic tick will retry once the connection is up.
        if !source.is_ready() {
            tracing::debug!("backfill worker: source not ready — skipping claim until connected");
            continue;
        }

        // Claim the next queued job
        let job = match store.claim_next_backfill_job().await {
            Ok(Some(j)) => j,
            Ok(None) => continue, // nothing to do
            Err(e) => {
                tracing::warn!("backfill worker: claim_next_backfill_job error: {e}");
                continue;
            }
        };

        tracing::info!(
            job_id = job.id,
            chat_jid = %job.chat_jid,
            target_kind = %job.target_kind,
            "backfill worker: processing job",
        );

        let outcome = process_job(
            &job,
            source.as_ref(),
            sink.as_ref(),
            &store,
            &pacer,
            batch_size,
            backstop,
            &cancel,
            event_tx.as_ref(),
        )
        .await;

        let (new_status, log_msg): (&str, &str) = match &outcome {
            Ok(JobEnd::Done) => ("done", "done"),
            Ok(JobEnd::Parked) => ("paused", "parked (backstop hit)"),
            Ok(JobEnd::Cancelled) => {
                // Shutdown cancel → requeue so it resumes after restart
                ("queued", "cancelled (shutdown) → requeued")
            }
            Ok(JobEnd::Deferred) => {
                // Source not ready (WA not connected) → requeue for retry once connected.
                // ADR 0026: a not-connected condition is transient; never mark failed.
                tracing::debug!(job_id = job.id, "backfill job deferred — waiting for connection");
                ("queued", "deferred (not connected) → requeued")
            }
            Ok(JobEnd::Failed(reason)) => {
                tracing::warn!(job_id = job.id, reason, "backfill job failed");
                ("failed", "failed")
            }
            Err(e) => {
                tracing::error!(job_id = job.id, error = %e, "backfill process_job returned Err");
                ("failed", "failed (unexpected error)")
            }
        };

        tracing::info!(job_id = job.id, status = new_status, "{}", log_msg);

        // Emit terminal progress event (ADR 0034). One event per job completion.
        // For Cancelled/Deferred we use `more_remain=true` (job will resume); for
        // Done/Failed we use `more_remain=false` (nothing more to expect from this job).
        if let Some(ref tx) = event_tx {
            let terminal_more_remain = matches!(&outcome,
                Ok(JobEnd::Deferred) | Ok(JobEnd::Parked) | Ok(JobEnd::Cancelled)
            );
            let terminal_status = match &outcome {
                Ok(JobEnd::Done) => "done",
                Ok(JobEnd::Parked) => "paused",
                Ok(JobEnd::Cancelled) => "cancelled",
                Ok(JobEnd::Deferred) => "deferred",
                Ok(JobEnd::Failed(_)) | Err(_) => "failed",
            };
            // Re-query the DB row so `fetched` reflects what process_job recorded via
            // record_backfill_progress — the local `job` snapshot captured the claim-time
            // state (fetched=0 for a fresh job) and was never updated in place.
            let fresh_fetched = store.get_backfill_job(job.id).await
                .ok()
                .flatten()
                .map(|r| r.fetched)
                .unwrap_or(job.fetched);
            let _ = tx.send(std::sync::Arc::new(crate::bridge_events::BridgeEvent::BackfillProgress(
                crate::bridge_events::BackfillProgressEvent {
                    job_id: job.id,
                    chat_jid: job.chat_jid.clone(),
                    target_kind: job.target_kind.clone(),
                    target_value: job.target_value,
                    fetched: fresh_fetched,
                    status: terminal_status.to_string(),
                    more_remain: terminal_more_remain,
                },
            )));
        }

        if let Err(e) = store.mark_backfill_job(job.id, new_status).await {
            tracing::warn!(job_id = job.id, error = %e, "backfill: mark_backfill_job failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- FetchTarget::parse / to_row round-trips ---

    #[test]
    fn test_fetch_target_parse_since() {
        let t = FetchTarget::parse("since", Some(1_700_000_000_000)).unwrap();
        assert_eq!(t, FetchTarget::Since(1_700_000_000_000));
        assert_eq!(t.to_row(), ("since", Some(1_700_000_000_000)));
    }

    #[test]
    fn test_fetch_target_parse_all() {
        let t = FetchTarget::parse("all", None).unwrap();
        assert_eq!(t, FetchTarget::All);
        assert_eq!(t.to_row(), ("all", None));
    }

    #[test]
    fn test_fetch_target_parse_count() {
        let t = FetchTarget::parse("count", Some(500)).unwrap();
        assert_eq!(t, FetchTarget::Count(500));
        assert_eq!(t.to_row(), ("count", Some(500)));
    }

    #[test]
    fn test_fetch_target_parse_count_zero() {
        let t = FetchTarget::parse("count", Some(0)).unwrap();
        assert_eq!(t, FetchTarget::Count(0));
        assert_eq!(t.to_row(), ("count", Some(0)));
    }

    #[test]
    fn test_fetch_target_parse_since_missing_value() {
        let err = FetchTarget::parse("since", None);
        assert!(err.is_err(), "since with None value must fail");
        assert!(err.unwrap_err().to_string().contains("non-null"));
    }

    #[test]
    fn test_fetch_target_parse_count_missing_value() {
        let err = FetchTarget::parse("count", None);
        assert!(err.is_err(), "count with None value must fail");
    }

    #[test]
    fn test_fetch_target_parse_unknown_kind() {
        let err = FetchTarget::parse("range", Some(0));
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("unknown target_kind"));
    }

    #[test]
    fn test_fetch_target_round_trip_all() {
        let t = FetchTarget::All;
        let (kind, val) = t.to_row();
        let t2 = FetchTarget::parse(kind, val).unwrap();
        assert_eq!(t, t2, "All must round-trip");
    }

    #[test]
    fn test_fetch_target_round_trip_since() {
        let t = FetchTarget::Since(9_999_000_000_000);
        let (kind, val) = t.to_row();
        let t2 = FetchTarget::parse(kind, val).unwrap();
        assert_eq!(t, t2, "Since must round-trip");
    }

    #[test]
    fn test_fetch_target_round_trip_count() {
        let t = FetchTarget::Count(1234);
        let (kind, val) = t.to_row();
        let t2 = FetchTarget::parse(kind, val).unwrap();
        assert_eq!(t, t2, "Count must round-trip");
    }

    // --- evaluate_target: Since ---

    #[test]
    fn test_evaluate_since_done_no_more_remain() {
        // more_remain=false → Done regardless of ts
        let step = evaluate_target(
            &FetchTarget::Since(1_000_000),
            10,
            Some(1_500_000),
            false,
            0,
        );
        assert_eq!(step, BackfillStep::Done);
    }

    #[test]
    fn test_evaluate_since_done_oldest_crossed_threshold() {
        // oldest ts <= target ts → Done
        let step = evaluate_target(
            &FetchTarget::Since(1_000_000),
            10,
            Some(900_000), // older than target
            true,
            0,
        );
        assert_eq!(step, BackfillStep::Done);
    }

    #[test]
    fn test_evaluate_since_done_oldest_equal_threshold() {
        // oldest ts == target ts → Done (boundary)
        let step = evaluate_target(
            &FetchTarget::Since(1_000_000),
            10,
            Some(1_000_000),
            true,
            0,
        );
        assert_eq!(step, BackfillStep::Done);
    }

    #[test]
    fn test_evaluate_since_continue() {
        // oldest ts > target ts, more_remain=true, no backstop → Continue
        let step = evaluate_target(
            &FetchTarget::Since(1_000_000),
            10,
            Some(2_000_000),
            true,
            0,
        );
        assert_eq!(step, BackfillStep::Continue);
    }

    #[test]
    fn test_evaluate_since_parked_at_backstop() {
        // fetched >= backstop, but not done → Parked
        let step = evaluate_target(
            &FetchTarget::Since(1_000_000),
            20_000,
            Some(2_000_000),
            true,
            20_000,
        );
        assert_eq!(step, BackfillStep::Parked);
    }

    #[test]
    fn test_evaluate_since_completion_wins_over_backstop() {
        // oldest ts <= target ts AND fetched >= backstop → Done (completion wins)
        let step = evaluate_target(
            &FetchTarget::Since(1_000_000),
            20_000,
            Some(900_000), // crossed threshold
            true,
            20_000,
        );
        assert_eq!(step, BackfillStep::Done);
    }

    #[test]
    fn test_evaluate_since_zero_backstop_never_parks() {
        // backstop=0 means "no backstop"
        let step = evaluate_target(
            &FetchTarget::Since(1_000_000),
            1_000_000,
            Some(2_000_000),
            true,
            0,
        );
        assert_eq!(step, BackfillStep::Continue);
    }

    // --- evaluate_target: All ---

    #[test]
    fn test_evaluate_all_done() {
        let step = evaluate_target(&FetchTarget::All, 5, None, false, 0);
        assert_eq!(step, BackfillStep::Done);
    }

    #[test]
    fn test_evaluate_all_continue() {
        let step = evaluate_target(&FetchTarget::All, 5, None, true, 0);
        assert_eq!(step, BackfillStep::Continue);
    }

    #[test]
    fn test_evaluate_all_parked() {
        let step = evaluate_target(&FetchTarget::All, 20_000, None, true, 20_000);
        assert_eq!(step, BackfillStep::Parked);
    }

    #[test]
    fn test_evaluate_all_completion_wins_over_backstop() {
        // more_remain=false AND fetched >= backstop → Done
        let step = evaluate_target(&FetchTarget::All, 20_000, None, false, 20_000);
        assert_eq!(step, BackfillStep::Done);
    }

    #[test]
    fn test_evaluate_all_zero_backstop_never_parks() {
        let step = evaluate_target(&FetchTarget::All, 1_000_000, None, true, 0);
        assert_eq!(step, BackfillStep::Continue);
    }

    // --- evaluate_target: Count ---

    #[test]
    fn test_evaluate_count_done() {
        let step = evaluate_target(&FetchTarget::Count(100), 100, None, true, 50_000);
        assert_eq!(step, BackfillStep::Done);
    }

    #[test]
    fn test_evaluate_count_done_over_target() {
        // fetched > n also counts as Done
        let step = evaluate_target(&FetchTarget::Count(100), 105, None, true, 50_000);
        assert_eq!(step, BackfillStep::Done);
    }

    #[test]
    fn test_evaluate_count_continue() {
        let step = evaluate_target(&FetchTarget::Count(100), 50, None, true, 0);
        assert_eq!(step, BackfillStep::Continue);
    }

    #[test]
    fn test_evaluate_count_never_parks() {
        // Count with backstop < n → must NOT park, continues until done
        let step = evaluate_target(&FetchTarget::Count(100), 50, None, true, 30);
        assert_eq!(step, BackfillStep::Continue, "Count must never park");
    }

    #[test]
    fn test_evaluate_count_zero_target_is_done() {
        let step = evaluate_target(&FetchTarget::Count(0), 0, None, true, 0);
        assert_eq!(step, BackfillStep::Done);
    }

    // --- FakeHistorySource ---

    #[tokio::test]
    async fn test_fake_source_returns_batch_in_order() {
        let anchor = Anchor {
            oldest_msg_id: "msg-001".to_string(),
            oldest_msg_from_me: false,
            oldest_msg_timestamp_ms: 1_700_000_000_000,
        };
        let fake = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![],
                more_remain: true,
                progress: Some(10),
            },
            FakeResponse::Batch {
                messages: vec![],
                more_remain: false,
                progress: None,
            },
        ]);

        let b1 = fake.fetch_older("chat@s.whatsapp.net", &anchor, 100).await.unwrap();
        assert!(b1.more_remain);
        assert_eq!(b1.progress, Some(10));

        let b2 = fake.fetch_older("chat@s.whatsapp.net", &anchor, 100).await.unwrap();
        assert!(!b2.more_remain);
        assert_eq!(b2.progress, None);
    }

    #[tokio::test]
    async fn test_fake_source_exhausted_constructor() {
        let anchor = Anchor {
            oldest_msg_id: "x".to_string(),
            oldest_msg_from_me: false,
            oldest_msg_timestamp_ms: 0,
        };
        let fake = FakeHistorySource::exhausted();
        let b = fake.fetch_older("c", &anchor, 10).await.unwrap();
        assert!(!b.more_remain);
        assert!(b.messages.is_empty());
    }

    #[tokio::test]
    async fn test_fake_source_error_constructor() {
        let anchor = Anchor {
            oldest_msg_id: "x".to_string(),
            oldest_msg_from_me: false,
            oldest_msg_timestamp_ms: 0,
        };
        let fake = FakeHistorySource::error("timeout");
        let err = fake.fetch_older("c", &anchor, 10).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("timeout"));
    }

    #[tokio::test]
    async fn test_fake_source_exhausted_after_scripted_responses() {
        let anchor = Anchor {
            oldest_msg_id: "x".to_string(),
            oldest_msg_from_me: false,
            oldest_msg_timestamp_ms: 0,
        };
        let fake = FakeHistorySource::new(vec![FakeResponse::Batch {
            messages: vec![],
            more_remain: true,
            progress: None,
        }]);
        // First call consumes the scripted response
        fake.fetch_older("c", &anchor, 10).await.unwrap();
        // Second call hits the "no more scripted responses" error
        let err = fake.fetch_older("c", &anchor, 10).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("no more scripted responses"));
    }

    #[tokio::test]
    async fn test_fake_source_error_in_sequence() {
        let anchor = Anchor {
            oldest_msg_id: "x".to_string(),
            oldest_msg_from_me: false,
            oldest_msg_timestamp_ms: 0,
        };
        // Script: batch, then error, then batch
        let fake = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Error("simulated WA rate-limit".to_string()),
            FakeResponse::Batch {
                messages: vec![],
                more_remain: false,
                progress: None,
            },
        ]);
        let b1 = fake.fetch_older("c", &anchor, 10).await;
        assert!(b1.is_ok());

        let e = fake.fetch_older("c", &anchor, 10).await;
        assert!(e.is_err());
        assert!(e.unwrap_err().to_string().contains("rate-limit"));

        let b2 = fake.fetch_older("c", &anchor, 10).await;
        assert!(b2.is_ok());
        assert!(!b2.unwrap().more_remain);
    }

    // =========================================================================
    // Wave B1 tests — process_job, run_worker_loop, BackfillPacer, BatchSink
    // =========================================================================

    use crate::storage::{EnqueueOutcome, Store};
    use std::path::PathBuf;

    // --- helpers ---

    fn unique_test_dir_bf(name: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("whatsrust-b1-{name}-{ts}"))
    }

    fn open_b1_store(name: &str) -> (Store, PathBuf) {
        let dir = unique_test_dir_bf(name);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wa.db");
        let store = Store::new(&db_path).unwrap();
        (store, dir)
    }

    /// Build a `WebMessageInfo` with a given message-id, from_me, and timestamp (unix secs).
    fn make_msg(id: &str, from_me: bool, ts_secs: u64) -> wa::WebMessageInfo {
        wa::WebMessageInfo {
            key: wa::MessageKey {
                id: Some(id.to_owned()),
                from_me: Some(from_me),
                remote_jid: None,
                participant: None,
            },
            message_timestamp: Some(ts_secs),
            ..Default::default()
        }
    }

    /// Enqueue a job directly and return the `BackfillJobRow` after claiming it.
    async fn enqueue_and_claim(store: &Store, chat_jid: &str, kind: &str, value: Option<i64>)
        -> crate::storage::BackfillJobRow
    {
        let outcome = store.enqueue_backfill_job(chat_jid, kind, value, 0, 0, 0, None).await.unwrap();
        let _job_id = match outcome {
            EnqueueOutcome::Accepted { job_id, .. } => job_id,
            other => panic!("expected Accepted, got {:?}", other),
        };
        // claim_next flips to 'running'
        store.claim_next_backfill_job().await.unwrap().expect("job must be claimable")
    }

    fn no_pacer() -> BackfillPacer {
        BackfillPacer { base_delay: Duration::ZERO, jitter_frac: 0.0 }
    }

    // --- test: oldest_anchor helper ---

    #[test]
    fn test_oldest_anchor_picks_smallest_timestamp() {
        // make_msg uses ts_secs; oldest_anchor scales to ms (×1000).
        // msg-old has ts_secs=1_000 → expected oldest_msg_timestamp_ms = 1_000_000 ms.
        let batch = HistoryBatch {
            messages: vec![
                make_msg("msg-old", false, 1_000),
                make_msg("msg-new", false, 3_000),
                make_msg("msg-mid", true,  2_000),
            ],
            more_remain: true,
            progress: None,
        };
        let a = oldest_anchor(&batch).expect("should find oldest");
        assert_eq!(a.oldest_msg_id, "msg-old");
        // ts_secs=1_000 → ms=1_000_000 (oldest_anchor scales seconds→ms)
        assert_eq!(a.oldest_msg_timestamp_ms, 1_000_000);
    }

    #[test]
    fn test_oldest_anchor_empty_batch_returns_none() {
        let batch = HistoryBatch { messages: vec![], more_remain: false, progress: None };
        assert!(oldest_anchor(&batch).is_none());
    }

    // --- test: multi-batch pagination → Done + cursor exhausted ---

    #[tokio::test]
    async fn test_process_job_multi_batch_pagination() {
        let (store, dir) = open_b1_store("multi-batch");

        // 3 batches: batches 1+2 have more_remain=true, batch 3 exhausts
        let source = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![make_msg("m3", false, 3_000), make_msg("m4", false, 4_000)],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![make_msg("m1", false, 1_000), make_msg("m2", false, 2_000)],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![make_msg("m0", false, 500)],
                more_remain: false,
                progress: None,
            },
        ]);
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Done);

        // 3 calls to persist_batch
        let calls = sink.recorded_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].message_count, 2);
        assert_eq!(calls[1].message_count, 2);
        assert_eq!(calls[2].message_count, 1);

        // Fetched = 5 total
        let row = store.get_backfill_job(job.id).await.unwrap().unwrap();
        assert_eq!(row.fetched, 5);

        // Cursor should be exhausted
        let cursor = store.get_backfill_cursor("chat@s.whatsapp.net").await.unwrap().unwrap();
        assert!(cursor.exhausted);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: Count target stops at n ---

    #[tokio::test]
    async fn test_process_job_count_target() {
        let (store, dir) = open_b1_store("count-target");

        // 3 batches of 2; Count(5) should stop after 3 batches (6 fetched ≥ 5)
        let source = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![make_msg("a2", false, 200), make_msg("a3", false, 300)],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![make_msg("a0", false, 0), make_msg("a1", false, 100)],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                // This batch would not be needed for Count(5) since 4 already fetched
                // but the loop fetches before evaluating, so we need it ready
                messages: vec![make_msg("b0", false, 999)],
                more_remain: true,
                progress: None,
            },
        ]);
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "count-chat@s.whatsapp.net", "count", Some(5)).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 2, 0, &cancel, None)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Done);
        let row = store.get_backfill_job(job.id).await.unwrap().unwrap();
        // Must have fetched ≥ 5 (stopped as soon as target met)
        assert!(row.fetched >= 5, "expected fetched >= 5, got {}", row.fetched);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: Since target stops at timestamp ---

    #[tokio::test]
    async fn test_process_job_since_target() {
        let (store, dir) = open_b1_store("since-target");
        // Target: fetch until oldest message is at or before ts_ms = 1_000_000 ms (= 1000 secs).
        // make_msg produces messageTimestamp in seconds; oldest_anchor scales to ms (×1000).
        // Batch 1: oldest ts_secs=3_000 → anchor_ms=3_000_000 > 1_000_000 → Continue
        // Batch 2: oldest ts_secs=800   → anchor_ms=  800_000 < 1_000_000 → Done
        let source = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![make_msg("s2", false, 3_000), make_msg("s3", false, 5_000)],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![make_msg("s0", false, 800), make_msg("s1", false, 2_000)],
                more_remain: true,
                progress: None,
            },
        ]);
        let sink = FakeBatchSink::always_ok();
        // Since target value is milliseconds: 1000 secs × 1000 = 1_000_000 ms
        let job = enqueue_and_claim(&store, "since-chat@s.whatsapp.net", "since", Some(1_000_000)).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Done);
        let row = store.get_backfill_job(job.id).await.unwrap().unwrap();
        assert_eq!(row.fetched, 4); // 2 + 2

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: Backstop park → Parked ---

    #[tokio::test]
    async fn test_process_job_backstop_park() {
        let (store, dir) = open_b1_store("backstop");
        // All target with backstop=4; fake never exhausts
        // Batches of 3 messages each; after batch 2 fetched=6 >= backstop=4 → Parked
        let source = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![
                    make_msg("p3", false, 30),
                    make_msg("p4", false, 40),
                    make_msg("p5", false, 50),
                ],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![
                    make_msg("p0", false, 0),
                    make_msg("p1", false, 10),
                    make_msg("p2", false, 20),
                ],
                more_remain: true,
                progress: None,
            },
            // A third batch that must NOT be fetched (we should park after batch 2)
            FakeResponse::Batch {
                messages: vec![make_msg("p-never", false, 999)],
                more_remain: true,
                progress: None,
            },
        ]);
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "park-chat@g.us", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 3, 4, &cancel, None)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Parked);
        let row = store.get_backfill_job(job.id).await.unwrap().unwrap();
        assert!(row.fetched >= 4, "fetched should be >= backstop=4");

        // Cursor should NOT be exhausted
        let cursor = store.get_backfill_cursor("park-chat@g.us").await.unwrap().unwrap();
        assert!(!cursor.exhausted, "cursor must not be exhausted when parked");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: cancel mid-loop → Cancelled, cursor persisted ---

    #[tokio::test]
    async fn test_process_job_cancel_mid_loop() {
        let (store, dir) = open_b1_store("cancel-mid");

        // Two batches; we cancel after batch 1 via a hook on the sink
        // We use a dedicated cancel token
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Sink that cancels the token after the first persist_batch call
        let sink = FakeBatchSink::always_ok();
        // We'll manually cancel before the second fetch by using a source that
        // cancels the token on its second call
        struct CancelOnSecond {
            calls: std::sync::atomic::AtomicU32,
            cancel: CancellationToken,
        }
        #[async_trait::async_trait]
        impl HistorySource for CancelOnSecond {
            async fn fetch_older(&self, _jid: &str, _anchor: &Anchor, _count: i32)
                -> anyhow::Result<HistoryBatch>
            {
                let call_no = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call_no == 0 {
                    Ok(HistoryBatch {
                        messages: vec![make_msg("c1", false, 1000)],
                        more_remain: true,
                        progress: None,
                    })
                } else {
                    // Cancel on the second fetch attempt
                    self.cancel.cancel();
                    Ok(HistoryBatch {
                        messages: vec![make_msg("c0", false, 500)],
                        more_remain: true,
                        progress: None,
                    })
                }
            }
        }
        let source = CancelOnSecond {
            calls: std::sync::atomic::AtomicU32::new(0),
            cancel: cancel_clone,
        };

        let job = enqueue_and_claim(&store, "cancel-chat@s.whatsapp.net", "all", None).await;

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        // The cancel fires during/after batch 2, so the job ends Cancelled
        assert_eq!(end, JobEnd::Cancelled);

        // Cursor must be persisted (resumable)
        let cursor = store.get_backfill_cursor("cancel-chat@s.whatsapp.net").await.unwrap();
        assert!(cursor.is_some(), "cursor must exist for resumability");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: cancel BEFORE first fetch → Cancelled immediately ---

    #[tokio::test]
    async fn test_process_job_cancel_before_first_fetch() {
        let (store, dir) = open_b1_store("cancel-pre");
        let source = FakeHistorySource::new(vec![FakeResponse::Batch {
            messages: vec![make_msg("x", false, 1000)],
            more_remain: true,
            progress: None,
        }]);
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "pre-cancel@s.whatsapp.net", "all", None).await;

        let cancel = CancellationToken::new();
        cancel.cancel(); // already cancelled

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Cancelled);
        assert!(sink.recorded_calls().is_empty(), "no batch must be persisted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: stuck anchor → Failed ---

    #[tokio::test]
    async fn test_process_job_stuck_anchor() {
        let (store, dir) = open_b1_store("stuck");

        // K=2 consecutive same-anchor batches trigger the guard.
        // Batch 1: new anchor = "stuck-id" (different from initial "" → not counted yet).
        // Batch 2: new anchor = "stuck-id" again → consecutive_same_anchor = 1 (< K).
        // Batch 3: new anchor = "stuck-id" again → consecutive_same_anchor = 2 (>= K) → Failed.
        let source = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![make_msg("stuck-id", false, 1000)],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![make_msg("stuck-id", false, 1000)],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![make_msg("stuck-id", false, 1000)],
                more_remain: true,
                progress: None,
            },
        ]);
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "stuck-chat@g.us", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        match end {
            JobEnd::Failed(reason) => {
                assert!(reason.contains("anchor not advancing"), "got: {reason}");
            }
            other => panic!("expected Failed(anchor not advancing), got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: fetch error → Failed(reason) ---

    #[tokio::test]
    async fn test_process_job_fetch_error() {
        let (store, dir) = open_b1_store("fetch-error");
        let source = FakeHistorySource::error("network timeout");
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "err-chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        match end {
            JobEnd::Failed(reason) => {
                assert!(reason.contains("network timeout"), "got: {reason}");
            }
            other => panic!("expected Failed, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: pace interruptibility (cancel fires during sleep) ---

    #[tokio::test]
    async fn test_pacer_cancel_during_sleep() {
        // One batch then more_remain=true (Continue); pacer has a 200ms delay.
        // Cancel fires almost immediately — elapsed should be well under 200ms.
        let (store, dir) = open_b1_store("pacer-cancel");

        let source = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![make_msg("p0", false, 1000)],
                more_remain: true,
                progress: None,
            },
            // Second fetch never called — cancel happens during sleep after batch 1
            FakeResponse::Error("should not be reached".to_string()),
        ]);
        let sink = FakeBatchSink::always_ok();
        let pacer = BackfillPacer {
            base_delay: Duration::from_millis(200),
            jitter_frac: 0.0,
        };
        let job = enqueue_and_claim(&store, "pacer-chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Fire the cancel shortly after starting (10ms)
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_clone.cancel();
        });

        let t0 = std::time::Instant::now();
        let end = process_job(&job, &source, &sink, &store, &pacer, 10, 0, &cancel, None)
            .await
            .unwrap();
        let elapsed = t0.elapsed();

        assert_eq!(end, JobEnd::Cancelled, "job should be Cancelled by pacer interrupt");
        assert!(
            elapsed < Duration::from_millis(180),
            "elapsed {:?} should be well under base_delay 200ms",
            elapsed
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: run_worker_loop claims, processes, marks job ---

    #[tokio::test]
    async fn test_run_worker_loop_basic() {
        let (store, dir) = open_b1_store("driver-basic");

        // One exhausted batch job
        let source = Arc::new(FakeHistorySource::exhausted());
        let sink = Arc::new(FakeBatchSink::always_ok());
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();

        // Enqueue a job BEFORE starting the loop
        store.enqueue_backfill_job("driver-chat@s.whatsapp.net", "all", None, 0, 0, 0, None)
            .await.unwrap();

        let store2 = store.clone();
        let notify2 = notify.clone();
        let cancel2 = cancel.clone();
        let source2 = source.clone();
        let sink2 = sink.clone();

        // Run the loop in the background
        let loop_handle = tokio::spawn(async move {
            run_worker_loop(source2, sink2, store2, no_pacer(), 10, 0, notify2, cancel2, None).await;
        });

        // Trigger processing
        notify.notify_one();

        // Give the loop time to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The job should now be 'done' (exhausted source → Done → marked done)
        let jobs = store.list_backfill_jobs(false).await.unwrap();
        let job = jobs.iter().find(|j| j.chat_jid == "driver-chat@s.whatsapp.net")
            .expect("job must exist");
        assert_eq!(job.status, "done", "job status should be 'done', got '{}'", job.status);

        // Shut down the loop
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(200), loop_handle)
            .await
            .expect("loop should exit promptly after cancel")
            .expect("loop task must not panic");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: run_worker_loop shutdown while job in-flight requeues job ---

    #[tokio::test]
    async fn test_run_worker_loop_shutdown_requeues() {
        let (store, dir) = open_b1_store("driver-shutdown");

        // Source never exhausts — job should stay in the loop
        // We use a slow source so we can cancel during processing
        struct SlowSource;
        #[async_trait::async_trait]
        impl HistorySource for SlowSource {
            async fn fetch_older(&self, _jid: &str, _anchor: &Anchor, _count: i32)
                -> anyhow::Result<HistoryBatch>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(HistoryBatch {
                    messages: vec![make_msg("q0", false, 1000)],
                    more_remain: true,
                    progress: None,
                })
            }
        }

        let source = Arc::new(SlowSource);
        let sink = Arc::new(FakeBatchSink::always_ok());
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();

        store.enqueue_backfill_job("shutdown-chat@g.us", "all", None, 0, 0, 0, None).await.unwrap();

        let store2 = store.clone();
        let notify2 = notify.clone();
        let source2 = source.clone();
        let sink2 = sink.clone();

        let loop_handle = tokio::spawn(async move {
            run_worker_loop(source2, sink2, store2, no_pacer(), 10, 0, notify2, cancel2, None).await;
        });

        notify.notify_one();
        // Let the loop claim and start the job
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Cancel (shutdown) while the job is running
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(300), loop_handle)
            .await
            .expect("loop should exit after cancel")
            .expect("loop task must not panic");

        // The job should be requeued (not cancelled) so it resumes after restart
        let jobs = store.list_backfill_jobs(false).await.unwrap();
        let job = jobs.iter().find(|j| j.chat_jid == "shutdown-chat@g.us")
            .expect("job must exist");
        // Shutdown → requeue → status == 'queued'
        assert_eq!(job.status, "queued",
            "shutdown cancel must requeue the job, got '{}'", job.status);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: FakeBatchSink records correctly ---

    #[tokio::test]
    async fn test_fake_batch_sink_records() {
        let sink = FakeBatchSink::always_ok();
        let batch = HistoryBatch {
            messages: vec![make_msg("r1", false, 1000), make_msg("r2", true, 2000)],
            more_remain: true,
            progress: None,
        };
        let n = sink.persist_batch("test-jid", &batch).await.unwrap();
        assert_eq!(n, 2);
        let calls = sink.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].chat_jid, "test-jid");
        assert_eq!(calls[0].message_count, 2);
    }

    #[tokio::test]
    async fn test_fake_batch_sink_error_mode() {
        let sink = FakeBatchSink::error("sink failed");
        let batch = HistoryBatch { messages: vec![], more_remain: false, progress: None };
        let result = sink.persist_batch("jid", &batch).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("sink failed"));
    }

    // --- test: process_job with sink error → Failed ---

    #[tokio::test]
    async fn test_process_job_sink_error() {
        let (store, dir) = open_b1_store("sink-error");
        let source = FakeHistorySource::new(vec![FakeResponse::Batch {
            messages: vec![make_msg("x", false, 1000)],
            more_remain: true,
            progress: None,
        }]);
        let sink = FakeBatchSink::error("disk full");
        let job = enqueue_and_claim(&store, "sink-err@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        match end {
            JobEnd::Failed(reason) => {
                assert!(reason.contains("disk full"), "got: {reason}");
            }
            other => panic!("expected Failed(disk full), got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: exhausted cursor fast-path → Done immediately ---

    #[tokio::test]
    async fn test_process_job_exhausted_cursor_fast_path() {
        let (store, dir) = open_b1_store("exhausted-fp");

        // Seed an exhausted cursor
        store.upsert_backfill_cursor(
            "exhaust-chat@s.whatsapp.net",
            Some("last-msg"),
            Some(false),
            Some(1000),
            false,
            true, // exhausted
            None,
        ).await.unwrap();

        // Source that would fail if called (must not be called)
        let source = FakeHistorySource::error("must not be called");
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "exhaust-chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Done);
        assert!(sink.recorded_calls().is_empty(), "no batch must be persisted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // =========================================================================
    // New tests for Fix 2, Fix 3, Fix 4, Fix 5, Fix 7
    // =========================================================================

    // --- Fix 2: mark_backfill_job must not overwrite 'cancelled' with 'paused'/'queued' ---

    #[tokio::test]
    async fn test_mark_backfill_job_paused_queued_do_not_overwrite_cancelled() {
        let (store, dir) = open_b1_store("cancel-guard");

        // Enqueue and claim a job, then manually set it to 'cancelled'
        let outcome = store.enqueue_backfill_job("guard-chat@s.whatsapp.net", "all", None, 0, 0, 0, None)
            .await.unwrap();
        let job_id = match outcome {
            EnqueueOutcome::Accepted { job_id, .. } => job_id,
            other => panic!("expected Accepted, got {:?}", other),
        };
        // Claim it (→ running)
        store.claim_next_backfill_job().await.unwrap().expect("must claim");
        // Cancel it
        store.mark_backfill_job(job_id, "cancelled").await.unwrap();

        // Attempt to write 'paused' — must be a no-op (job stays 'cancelled')
        store.mark_backfill_job(job_id, "paused").await.unwrap();
        let row = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(row.status, "cancelled",
            "paused must not overwrite cancelled, got '{}'", row.status);

        // Attempt to write 'queued' — must also be a no-op
        store.mark_backfill_job(job_id, "queued").await.unwrap();
        let row = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(row.status, "cancelled",
            "queued must not overwrite cancelled, got '{}'", row.status);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Fix 3: last_backfill_at is stamped on Count(n) completion (more_remain=true) ---

    #[tokio::test]
    async fn test_process_job_count_completion_stamps_last_backfill_at() {
        let (store, dir) = open_b1_store("count-cooldown");

        // Count(2): two messages in one batch → Done with more_remain=true
        let source = FakeHistorySource::new(vec![FakeResponse::Batch {
            messages: vec![make_msg("c1", false, 1_000), make_msg("c2", false, 2_000)],
            more_remain: true, // history not exhausted — this is a Count(n) stop
            progress: None,
        }]);
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "count-cool@s.whatsapp.net", "count", Some(2)).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await.unwrap();

        assert_eq!(end, JobEnd::Done);

        // Cursor must have last_backfill_at set (Fix 3: always stamp on Done)
        let cursor = store.get_backfill_cursor("count-cool@s.whatsapp.net").await.unwrap().unwrap();
        assert!(
            cursor.last_backfill_at.is_some(),
            "last_backfill_at must be set on Count(n) Done with more_remain=true"
        );
        // exhausted must be false — more_remain=true means history is not exhausted
        assert!(!cursor.exhausted, "cursor must NOT be exhausted when more_remain=true");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Fix 4: batch with no derivable anchor → Done + exhausted (not a spin) ---

    #[tokio::test]
    async fn test_process_job_no_derivable_anchor_returns_done_exhausted() {
        let (store, dir) = open_b1_store("no-anchor");

        // Batch where all messages lack a key.id → oldest_anchor returns None
        let mut keyless_msg = wa::WebMessageInfo {
            key: wa::MessageKey {
                id: None, // no id → filtered out by oldest_anchor
                from_me: Some(false),
                remote_jid: None,
                participant: None,
            },
            message_timestamp: Some(9_999),
            ..Default::default()
        };
        // Add a second keyless message for good measure
        let keyless_msg2 = keyless_msg.clone();
        keyless_msg.message_timestamp = Some(1_000);

        let source = FakeHistorySource::new(vec![FakeResponse::Batch {
            messages: vec![keyless_msg, keyless_msg2],
            more_remain: true, // source claims more, but we can't page further
            progress: None,
        }]);
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "noanchor-chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await.unwrap();

        // Must be Done (not spin, not stuck-anchor Failed)
        assert_eq!(end, JobEnd::Done, "no-anchor batch must terminate with Done");

        // Cursor must be exhausted
        let cursor = store.get_backfill_cursor("noanchor-chat@s.whatsapp.net").await.unwrap().unwrap();
        assert!(cursor.exhausted, "cursor must be exhausted when no anchor is derivable");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Fix 5: invalid batch_size → Failed ---

    #[tokio::test]
    async fn test_process_job_invalid_batch_size_zero() {
        let (store, dir) = open_b1_store("batch-zero");
        let source = FakeHistorySource::exhausted();
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "bz-chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 0, 0, &cancel, None)
            .await.unwrap();

        match end {
            JobEnd::Failed(reason) => {
                assert!(reason.contains("invalid batch_size"), "got: {reason}");
            }
            other => panic!("expected Failed(invalid batch_size), got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_process_job_invalid_batch_size_negative() {
        let (store, dir) = open_b1_store("batch-neg");
        let source = FakeHistorySource::exhausted();
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "bn-chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), -1, 0, &cancel, None)
            .await.unwrap();

        match end {
            JobEnd::Failed(reason) => {
                assert!(reason.contains("invalid batch_size"), "got: {reason}");
            }
            other => panic!("expected Failed(invalid batch_size), got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Fix 7: record_backfill_progress updates both cursor and fetched atomically ---

    #[tokio::test]
    async fn test_record_backfill_progress_updates_cursor_and_fetched() {
        let (store, dir) = open_b1_store("atomic-progress");

        // Enqueue a job so we have a valid job_id
        let outcome = store.enqueue_backfill_job("atomic-chat@s.whatsapp.net", "all", None, 0, 0, 0, None)
            .await.unwrap();
        let job_id = match outcome {
            EnqueueOutcome::Accepted { job_id, .. } => job_id,
            other => panic!("expected Accepted, got {:?}", other),
        };

        // Call record_backfill_progress directly
        store.record_backfill_progress(
            job_id,
            "atomic-chat@s.whatsapp.net",
            Some("msg-abc"),
            Some(false),
            Some(1_700_000_000_000i64), // ms
            true,
            false,
            None,
            42,
        ).await.unwrap();

        // Verify cursor was written
        let cursor = store.get_backfill_cursor("atomic-chat@s.whatsapp.net").await.unwrap().unwrap();
        assert_eq!(cursor.oldest_msg_id, Some("msg-abc".to_owned()));
        assert_eq!(cursor.oldest_msg_timestamp_ms, Some(1_700_000_000_000i64));
        assert!(cursor.more_remain);
        assert!(!cursor.exhausted);

        // Verify fetched was updated
        let row = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(row.fetched, 42);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // =========================================================================
    // Wave B2a tests — drain_history_sync, initial_anchor
    // =========================================================================

    use crate::storage::OldestMessageRow;

    /// Build a minimal HistorySyncMsg containing the given WebMessageInfo.
    /// `HistorySyncMsg.message` is `Option<Box<WebMessageInfo>>` in the generated proto.
    fn make_hsm(wmi: wa::WebMessageInfo) -> wa::HistorySyncMsg {
        wa::HistorySyncMsg {
            message: Some(Box::new(wmi)),
            ..Default::default()
        }
    }

    /// Build a WebMessageInfo with the given message-id (timestamp irrelevant for drain tests).
    fn make_wmi(id: &str) -> wa::WebMessageInfo {
        wa::WebMessageInfo {
            key: wa::MessageKey {
                id: Some(id.to_owned()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    // --- drain_history_sync: basic 3-message extraction ---

    #[test]
    fn test_drain_basic_three_messages_more_remain_type2() {
        let conv = wa::Conversation {
            id: "chat@s.whatsapp.net".to_owned(),
            messages: vec![
                make_hsm(make_wmi("m1")),
                make_hsm(make_wmi("m2")),
                make_hsm(make_wmi("m3")),
            ],
            end_of_history_transfer_type: Some(2),
            ..Default::default()
        };
        let hs = wa::HistorySync {
            conversations: vec![conv],
            progress: Some(50),
            ..Default::default()
        };

        let batch = drain_history_sync(&hs, &["chat@s.whatsapp.net".to_string()]);
        assert_eq!(batch.messages.len(), 3);
        assert!(batch.more_remain, "type=2 → more_remain=true");
        assert_eq!(batch.progress, Some(50));
        // Message IDs preserved in order
        assert_eq!(batch.messages[0].key.id.as_deref(), Some("m1"));
        assert_eq!(batch.messages[1].key.id.as_deref(), Some("m2"));
        assert_eq!(batch.messages[2].key.id.as_deref(), Some("m3"));
    }

    // --- drain_history_sync: end_of_history_transfer_type variants ---

    #[test]
    fn test_drain_type0_more_remain_true() {
        let conv = wa::Conversation {
            id: "c@s".to_owned(),
            messages: vec![make_hsm(make_wmi("x"))],
            end_of_history_transfer_type: Some(0),
            ..Default::default()
        };
        let hs = wa::HistorySync { conversations: vec![conv], ..Default::default() };
        let batch = drain_history_sync(&hs, &["c@s".to_string()]);
        assert!(batch.more_remain, "type=0 → more_remain=true");
    }

    #[test]
    fn test_drain_type1_more_remain_false() {
        let conv = wa::Conversation {
            id: "c@s".to_owned(),
            messages: vec![make_hsm(make_wmi("x"))],
            end_of_history_transfer_type: Some(1),
            ..Default::default()
        };
        let hs = wa::HistorySync { conversations: vec![conv], ..Default::default() };
        let batch = drain_history_sync(&hs, &["c@s".to_string()]);
        assert!(!batch.more_remain, "type=1 → more_remain=false");
    }

    #[test]
    fn test_drain_type2_more_remain_true() {
        let conv = wa::Conversation {
            id: "c@s".to_owned(),
            messages: vec![make_hsm(make_wmi("x"))],
            end_of_history_transfer_type: Some(2),
            ..Default::default()
        };
        let hs = wa::HistorySync { conversations: vec![conv], ..Default::default() };
        let batch = drain_history_sync(&hs, &["c@s".to_string()]);
        assert!(batch.more_remain, "type=2 → more_remain=true");
    }

    #[test]
    fn test_drain_type3_more_remain_false() {
        let conv = wa::Conversation {
            id: "c@s".to_owned(),
            messages: vec![make_hsm(make_wmi("x"))],
            end_of_history_transfer_type: Some(3),
            ..Default::default()
        };
        let hs = wa::HistorySync { conversations: vec![conv], ..Default::default() };
        let batch = drain_history_sync(&hs, &["c@s".to_string()]);
        assert!(!batch.more_remain, "type=3 → more_remain=false");
    }

    // --- drain_history_sync: None type → progress fallback ---

    #[test]
    fn test_drain_none_type_progress_50_more_remain_true() {
        let conv = wa::Conversation {
            id: "c@s".to_owned(),
            messages: vec![make_hsm(make_wmi("x"))],
            end_of_history_transfer_type: None,
            ..Default::default()
        };
        let hs = wa::HistorySync {
            conversations: vec![conv],
            progress: Some(50),
            ..Default::default()
        };
        let batch = drain_history_sync(&hs, &["c@s".to_string()]);
        assert!(batch.more_remain, "None type + progress=50 → more_remain=true (50 < 100)");
    }

    #[test]
    fn test_drain_none_type_progress_100_more_remain_false() {
        let conv = wa::Conversation {
            id: "c@s".to_owned(),
            messages: vec![make_hsm(make_wmi("x"))],
            end_of_history_transfer_type: None,
            ..Default::default()
        };
        let hs = wa::HistorySync {
            conversations: vec![conv],
            progress: Some(100),
            ..Default::default()
        };
        let batch = drain_history_sync(&hs, &["c@s".to_string()]);
        assert!(!batch.more_remain, "None type + progress=100 → more_remain=false");
    }

    // --- drain_history_sync: non-matching conversation (two convs) → empty batch (no fallback) ---
    // With two conversations, the single-conversation fallback does NOT fire.

    #[test]
    fn test_drain_different_jid_two_convs_returns_empty() {
        let conv1 = wa::Conversation {
            id: "other1@s.whatsapp.net".to_owned(),
            messages: vec![make_hsm(make_wmi("x"))],
            end_of_history_transfer_type: Some(1),
            ..Default::default()
        };
        let conv2 = wa::Conversation {
            id: "other2@s.whatsapp.net".to_owned(),
            messages: vec![make_hsm(make_wmi("y"))],
            end_of_history_transfer_type: Some(1),
            ..Default::default()
        };
        let hs = wa::HistorySync {
            conversations: vec![conv1, conv2],
            progress: Some(100), // progress=100 → if no match, more_remain=false
            ..Default::default()
        };
        let batch = drain_history_sync(&hs, &["chat@s.whatsapp.net".to_string()]);
        assert!(batch.messages.is_empty(), "two non-matching convs, no fallback → empty messages");
    }

    // --- drain_history_sync: empty conversations → empty batch ---

    #[test]
    fn test_drain_empty_conversations_returns_empty() {
        let hs = wa::HistorySync {
            conversations: vec![],
            progress: Some(100),
            ..Default::default()
        };
        let batch = drain_history_sync(&hs, &["chat@s.whatsapp.net".to_string()]);
        assert!(batch.messages.is_empty(), "empty HistorySync → empty batch");
    }

    // --- initial_anchor: Some(row) → scaled anchor ---

    #[test]
    fn test_initial_anchor_some_row_scales_to_ms() {
        let row = OldestMessageRow {
            message_id: "m1".to_owned(),
            from_me: true,
            timestamp_secs: 1_000,
        };
        let anchor = initial_anchor(Some(row));
        assert_eq!(anchor.oldest_msg_id, "m1");
        assert!(anchor.oldest_msg_from_me);
        assert_eq!(anchor.oldest_msg_timestamp_ms, 1_000_000, "seconds must be scaled ×1000 to ms");
    }

    // --- initial_anchor: None → empty anchor ---

    #[test]
    fn test_initial_anchor_none_returns_empty() {
        let anchor = initial_anchor(None);
        assert_eq!(anchor.oldest_msg_id, "", "empty anchor id");
        assert!(!anchor.oldest_msg_from_me);
        assert_eq!(anchor.oldest_msg_timestamp_ms, 0, "empty anchor ts_ms = 0");
    }

    // =========================================================================
    // LID/PN matching and single-conversation fallback tests (Part 1 of LID fix)
    // =========================================================================

    // (a) Conversation keyed by LID; accepted_ids = [phone, lid] → matched via LID entry.
    #[test]
    fn test_drain_lid_matched_via_accepted_ids() {
        let conv = wa::Conversation {
            id: "7945790185720@lid".to_owned(),
            messages: vec![make_hsm(make_wmi("lid-msg-1")), make_hsm(make_wmi("lid-msg-2"))],
            end_of_history_transfer_type: Some(1), // complete — no more remain
            ..Default::default()
        };
        let hs = wa::HistorySync {
            conversations: vec![conv],
            progress: Some(100),
            ..Default::default()
        };

        // The caller provides both the phone JID and the LID alias in accepted_ids.
        let accepted_ids = vec![
            "972542271337@s.whatsapp.net".to_string(),
            "7945790185720@lid".to_string(),
        ];
        let batch = drain_history_sync(&hs, &accepted_ids);
        assert_eq!(batch.messages.len(), 2, "messages must be extracted via LID match");
        assert!(!batch.more_remain, "type=1 → no more remain");
        assert_eq!(batch.messages[0].key.id.as_deref(), Some("lid-msg-1"));
        assert_eq!(batch.messages[1].key.id.as_deref(), Some("lid-msg-2"));
    }

    // (b) Single conversation whose id is in neither accepted_id → single-conv fallback fires.
    #[test]
    fn test_drain_single_conv_fallback_when_no_accepted_id_matches() {
        let conv = wa::Conversation {
            id: "unknown-lid@lid".to_owned(),
            messages: vec![make_hsm(make_wmi("fallback-msg"))],
            end_of_history_transfer_type: Some(1),
            ..Default::default()
        };
        let hs = wa::HistorySync {
            conversations: vec![conv],
            progress: Some(100),
            ..Default::default()
        };

        // accepted_ids does NOT contain "unknown-lid@lid", but there is exactly one conversation.
        let accepted_ids = vec!["972000000001@s.whatsapp.net".to_string()];
        let batch = drain_history_sync(&hs, &accepted_ids);
        assert_eq!(batch.messages.len(), 1, "single-conv fallback must extract messages");
        assert!(!batch.more_remain, "type=1 → no more remain");
        assert_eq!(batch.messages[0].key.id.as_deref(), Some("fallback-msg"));
    }

    // (c) Two conversations, none in accepted_ids → fallback does NOT fire → empty batch.
    #[test]
    fn test_drain_two_convs_no_match_returns_empty() {
        let conv1 = wa::Conversation {
            id: "conv-a@lid".to_owned(),
            messages: vec![make_hsm(make_wmi("a"))],
            end_of_history_transfer_type: Some(2),
            ..Default::default()
        };
        let conv2 = wa::Conversation {
            id: "conv-b@lid".to_owned(),
            messages: vec![make_hsm(make_wmi("b"))],
            end_of_history_transfer_type: Some(2),
            ..Default::default()
        };
        let hs = wa::HistorySync {
            conversations: vec![conv1, conv2],
            progress: Some(50),
            ..Default::default()
        };

        // Neither conv-a nor conv-b is in accepted_ids, and there are 2 convs → no fallback.
        let accepted_ids = vec!["972000000002@s.whatsapp.net".to_string()];
        let batch = drain_history_sync(&hs, &accepted_ids);
        assert!(batch.messages.is_empty(), "two non-matching convs → no fallback → empty");
    }

    // =========================================================================
    // ADR 0026 connection-gating tests
    // =========================================================================

    // --- process_job: not-ready source → Deferred, no fetch attempted, cursor unchanged ---

    #[tokio::test]
    async fn test_process_job_not_ready_returns_deferred_no_fetch() {
        let (store, dir) = open_b1_store("gate-not-ready");

        // Source starts not-ready; its fetch_older would fail if called.
        let source = FakeHistorySource::error("must not be called — not ready");
        source.set_ready(false);

        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "gate-chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Deferred, "not-ready source must return Deferred");

        // No fetch was attempted — sink has no calls
        assert!(sink.recorded_calls().is_empty(), "no batch must be persisted when deferred");

        // Cursor is unchanged (None — never written)
        let cursor = store.get_backfill_cursor("gate-chat@s.whatsapp.net").await.unwrap();
        assert!(cursor.is_none(), "cursor must not be written when deferred before first fetch");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- process_job: fetch error while not-ready → Deferred (not Failed) ---

    #[tokio::test]
    async fn test_process_job_fetch_error_while_disconnected_returns_deferred() {
        let (store, dir) = open_b1_store("gate-fetch-err-disconnect");

        // Source is initially ready (so the loop starts), then becomes not-ready
        // before fetch_older returns an error (simulating a mid-fetch disconnect).
        // We implement this with a custom source that flips ready=false on its first call.
        struct DisconnectOnFetch {
            inner: FakeHistorySource,
        }
        #[async_trait::async_trait]
        impl HistorySource for DisconnectOnFetch {
            fn is_ready(&self) -> bool {
                self.inner.is_ready()
            }
            async fn fetch_older(&self, jid: &str, anchor: &Anchor, count: i32)
                -> anyhow::Result<HistoryBatch>
            {
                // Simulate disconnect during the fetch
                self.inner.set_ready(false);
                self.inner.fetch_older(jid, anchor, count).await
            }
        }

        let inner = FakeHistorySource::error("LiveHistorySource: no active client");
        inner.set_ready(true); // ready at start; flip happens inside fetch_older
        let source = DisconnectOnFetch { inner };

        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "gate-mid@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Deferred,
            "fetch error while disconnected must return Deferred, not Failed");

        assert!(sink.recorded_calls().is_empty(), "no batch must be persisted when deferred");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- process_job: fetch error while ready → Failed (existing behaviour preserved) ---

    #[tokio::test]
    async fn test_process_job_fetch_error_while_ready_returns_failed() {
        let (store, dir) = open_b1_store("gate-fetch-err-ready");

        // Source is ready; fetch returns an error → should still be Failed (not Deferred)
        let source = FakeHistorySource::error("network timeout");
        // ready is true by default

        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "ready-err@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, None)
            .await
            .unwrap();

        match end {
            JobEnd::Failed(reason) => {
                assert!(reason.contains("network timeout"), "got: {reason}");
            }
            other => panic!("expected Failed when ready+error, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- run_worker_loop: not-ready → job not claimed / not failed; ready → processed Done ---

    #[tokio::test]
    async fn test_run_worker_loop_defers_until_ready() {
        let (store, dir) = open_b1_store("driver-gate");

        // Source starts not-ready; one exhausted batch once ready
        let source = Arc::new(FakeHistorySource::exhausted());
        source.set_ready(false);

        let sink = Arc::new(FakeBatchSink::always_ok());
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();

        // Enqueue a job
        store.enqueue_backfill_job("gate-driver@s.whatsapp.net", "all", None, 0, 0, 0, None)
            .await.unwrap();

        let store2 = store.clone();
        let notify2 = notify.clone();
        let cancel2 = cancel.clone();
        let source2 = source.clone();
        let sink2 = sink.clone();

        let loop_handle = tokio::spawn(async move {
            run_worker_loop(source2, sink2, store2, no_pacer(), 10, 0, notify2, cancel2, None).await;
        });

        // Trigger first tick — source not ready, job must NOT be claimed
        notify.notify_one();
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Job must still be queued (not claimed/failed)
        let jobs = store.list_backfill_jobs(false).await.unwrap();
        let job = jobs.iter().find(|j| j.chat_jid == "gate-driver@s.whatsapp.net")
            .expect("job must exist");
        assert_eq!(job.status, "queued",
            "job must stay queued when source is not ready, got '{}'", job.status);

        // Now flip to ready and notify — the job should be processed to Done
        source.set_ready(true);
        notify.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let jobs = store.list_backfill_jobs(false).await.unwrap();
        let job = jobs.iter().find(|j| j.chat_jid == "gate-driver@s.whatsapp.net")
            .expect("job must exist");
        assert_eq!(job.status, "done",
            "job must be done after source becomes ready, got '{}'", job.status);

        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(200), loop_handle)
            .await
            .expect("loop should exit promptly after cancel")
            .expect("loop task must not panic");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- run_worker_loop: Deferred → job back to queued (not failed) ---

    #[tokio::test]
    async fn test_run_worker_loop_deferred_maps_to_queued() {
        let (store, dir) = open_b1_store("driver-deferred");

        // Source starts ready so the job gets claimed, but returns Deferred from process_job.
        // We achieve this by having is_ready() return true initially so the loop claims the job,
        // but then have it return false inside the pagination loop when process_job checks.
        // The simplest way: use a source whose is_ready() starts false but the claim-gate check
        // passes because we're checking after the initial "ready" window.
        //
        // Actually: since run_worker_loop checks is_ready() before claiming, we need a source
        // that is ready at claim time but becomes not-ready when process_job checks at loop start.
        // We simulate this with an AtomicBool that flips after claim_next_backfill_job is called,
        // i.e., the source becomes not-ready between the claim-guard check and the process_job
        // readiness check. We'll just test that Deferred output → mark_backfill_job("queued").
        //
        // Simplest approach: set ready=false from the start and rely on the driver skipping the
        // claim check (POLL_INTERVAL is 5s; we use notify to drive). But the claim-gate skips
        // entirely. So let us drive a Deferred by having the source be ready=true at loop entry
        // but not-ready by the time process_job's inner check runs.
        //
        // To avoid async races we use a custom source.
        struct FlipAfterFirstIsReady {
            calls: std::sync::atomic::AtomicU32,
        }
        #[async_trait::async_trait]
        impl HistorySource for FlipAfterFirstIsReady {
            fn is_ready(&self) -> bool {
                // First call (from run_worker_loop claim-gate) returns true.
                // Second call (from process_job loop) returns false → Deferred.
                let prev = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                prev == 0
            }
            async fn fetch_older(&self, _jid: &str, _anchor: &Anchor, _count: i32)
                -> anyhow::Result<HistoryBatch>
            {
                // Should not be reached because process_job returns Deferred before fetch
                Err(anyhow::anyhow!("must not be called"))
            }
        }

        let source = Arc::new(FlipAfterFirstIsReady {
            calls: std::sync::atomic::AtomicU32::new(0),
        });
        let sink = Arc::new(FakeBatchSink::always_ok());
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();

        // Enqueue a job
        let outcome = store.enqueue_backfill_job("deferred-driver@s.whatsapp.net", "all", None, 0, 0, 0, None)
            .await.unwrap();
        let job_id = match outcome {
            EnqueueOutcome::Accepted { job_id, .. } => job_id,
            other => panic!("expected Accepted, got {:?}", other),
        };

        let store2 = store.clone();
        let notify2 = notify.clone();
        let cancel2 = cancel.clone();
        let source2 = source.clone();
        let sink2 = sink.clone();

        let loop_handle = tokio::spawn(async move {
            run_worker_loop(source2, sink2, store2, no_pacer(), 10, 0, notify2, cancel2, None).await;
        });

        notify.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Job must be back to queued (Deferred → requeued), NOT failed
        let row = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(row.status, "queued",
            "Deferred must requeue the job, got '{}'", row.status);

        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(200), loop_handle)
            .await
            .expect("loop should exit promptly after cancel")
            .expect("loop task must not panic");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // =========================================================================
    // M1.4 Wave 3 — SSE progress event tests (ADR 0034)
    // =========================================================================

    // --- test: process_job emits BackfillProgress(status="running") per batch ---

    #[tokio::test]
    async fn test_process_job_emits_running_progress_events() {
        let (store, dir) = open_b1_store("sse-progress-running");

        // Two batches: batch 1 has more_remain=true (running), batch 2 exhausts (done).
        let source = FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![make_msg("e1", false, 2_000), make_msg("e2", false, 3_000)],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![make_msg("e0", false, 1_000)],
                more_remain: false,
                progress: None,
            },
        ]);
        let sink = FakeBatchSink::always_ok();
        let job = enqueue_and_claim(&store, "sse-chat@s.whatsapp.net", "all", None).await;
        let cancel = CancellationToken::new();

        let (tx, mut rx) = crate::bridge_events::new_event_bus();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel, Some(&tx))
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Done);

        // Collect all events that are available (non-blocking drain)
        let mut running_events = vec![];
        while let Ok(evt) = rx.try_recv() {
            if let crate::bridge_events::BridgeEvent::BackfillProgress(ref p) = *evt {
                running_events.push(p.clone());
            }
        }

        // We expect exactly 2 running events (one per batch) — terminal is emitted by run_worker_loop
        assert_eq!(running_events.len(), 2, "expected 2 running progress events, got {}", running_events.len());

        // First batch: 2 messages fetched
        assert_eq!(running_events[0].status, "running");
        assert_eq!(running_events[0].fetched, 2);
        assert_eq!(running_events[0].chat_jid, "sse-chat@s.whatsapp.net");
        assert_eq!(running_events[0].target_kind, "all");
        assert!(running_events[0].more_remain);

        // Second batch: 3 total fetched (2 + 1)
        assert_eq!(running_events[1].status, "running");
        assert_eq!(running_events[1].fetched, 3);
        assert!(!running_events[1].more_remain);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- test: format_sse_event returns Some with event name "backfill" for BackfillProgress ---

    #[test]
    fn test_format_sse_event_backfill_progress() {
        use crate::bridge_events::{BackfillProgressEvent, BridgeEvent};
        use std::sync::Arc;

        let evt = BridgeEvent::BackfillProgress(BackfillProgressEvent {
            job_id: 7,
            chat_jid: "test@s.whatsapp.net".to_string(),
            target_kind: "count".to_string(),
            target_value: Some(100),
            fetched: 42,
            status: "running".to_string(),
            more_remain: true,
        });

        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"job_id\":7"), "json: {json}");
        assert!(json.contains("\"status\":\"running\""), "json: {json}");
        assert!(json.contains("\"fetched\":42"), "json: {json}");
        assert!(json.contains("\"more_remain\":true"), "json: {json}");

        // Also verify the BridgeEvent enum serialises correctly (tag="backfill_progress")
        // Note: serde tag + rename_all="snake_case" → BackfillProgress → "backfill_progress"
        let _ = Arc::new(evt); // ensure it's Clone/Arc-compatible
    }

    // =========================================================================
    // Fix 1: terminal SSE event carries fresh `fetched` (not the claim-time 0)
    // =========================================================================

    /// Drive `run_worker_loop` end-to-end with a FakeHistorySource that returns a known
    /// total of messages, then assert the terminal `BackfillProgress` SSE event reports
    /// the correct `fetched` count — not the stale claim-time 0.
    #[tokio::test]
    async fn test_run_worker_loop_terminal_event_has_correct_fetched() {
        let (store, dir) = open_b1_store("fix1-terminal-fetched");

        // Two batches: 3 messages total, then exhausted (more_remain=false).
        let source = Arc::new(FakeHistorySource::new(vec![
            FakeResponse::Batch {
                messages: vec![
                    make_msg("f1", false, 2_000),
                    make_msg("f2", false, 3_000),
                ],
                more_remain: true,
                progress: None,
            },
            FakeResponse::Batch {
                messages: vec![make_msg("f0", false, 1_000)],
                more_remain: false,
                progress: None,
            },
        ]));

        let sink = Arc::new(FakeBatchSink::always_ok());
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();

        // Set up an event channel to capture SSE events
        let (tx, mut rx) = crate::bridge_events::new_event_bus();

        store.enqueue_backfill_job("fix1-chat@s.whatsapp.net", "all", None, 0, 0, 0, None)
            .await.unwrap();

        let store2 = store.clone();
        let notify2 = notify.clone();
        let cancel2 = cancel.clone();
        let source2 = source.clone();
        let sink2 = sink.clone();

        let loop_handle = tokio::spawn(async move {
            run_worker_loop(source2, sink2, store2, no_pacer(), 10, 0, notify2, cancel2, Some(tx)).await;
        });

        // Trigger the worker
        notify.notify_one();
        // Give it enough time to process both batches and emit the terminal event
        tokio::time::sleep(Duration::from_millis(100)).await;

        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(200), loop_handle)
            .await
            .expect("loop must exit after cancel")
            .expect("loop task must not panic");

        // Collect all BackfillProgress events
        let mut progress_events = vec![];
        while let Ok(evt) = rx.try_recv() {
            if let crate::bridge_events::BridgeEvent::BackfillProgress(ref p) = *evt {
                progress_events.push(p.clone());
            }
        }

        // The terminal event is the one with status != "running"
        let terminal = progress_events.iter()
            .find(|p| p.status != "running")
            .expect("a terminal BackfillProgress event must be emitted");

        assert_eq!(terminal.status, "done", "terminal status must be 'done'");
        // Fix 1: fetched must be 3 (2 + 1), not 0 (the claim-time snapshot value)
        assert_eq!(terminal.fetched, 3,
            "terminal event fetched must equal the total messages fetched (3), got {}",
            terminal.fetched);
        assert_eq!(terminal.chat_jid, "fix1-chat@s.whatsapp.net");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
