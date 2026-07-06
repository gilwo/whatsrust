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
// Anchor — the per-chat backward-pagination frontier
// ---------------------------------------------------------------------------

/// The oldest known message position for a chat — used as the pagination anchor
/// when requesting history older than what we have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub oldest_msg_id: String,
    pub oldest_msg_from_me: bool,
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
pub struct FakeHistorySource {
    responses: std::sync::Mutex<std::collections::VecDeque<FakeResponse>>,
}

impl FakeHistorySource {
    /// Create a fake source from a list of scripted responses.
    pub fn new(responses: Vec<FakeResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
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
}

#[async_trait::async_trait]
impl HistorySource for FakeHistorySource {
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
///   Auto-continues across paged segments.
/// - `All`          — fetch until phone exhausted. Auto-continues.
/// - `Count(n)`     — fetch until `n` messages fetched. Does NOT auto-continue.
///
/// The autonomy backstop (from config) is SEPARATE from the target: it limits how far
/// `Since`/`All` may run in one trigger before PARKING (requiring re-trigger).
/// `Count` is already bounded and never parks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    Since(i64 /* ts_ms */),
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
/// - `batch_oldest_ts_ms`— timestamp of the oldest message in the batch, if any
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
/// Returns `None` if the batch has no messages or none have a key.
fn oldest_anchor(batch: &HistoryBatch) -> Option<Anchor> {
    batch
        .messages
        .iter()
        .filter_map(|m| {
            let ts = m.message_timestamp? as i64;
            let id = m.key.id.clone()?;
            let from_me = m.key.from_me.unwrap_or(false);
            Some((ts, id, from_me))
        })
        .min_by_key(|(ts, id, _)| (*ts, id.clone()))
        .map(|(ts, id, from_me)| Anchor {
            oldest_msg_id: id,
            oldest_msg_from_me: from_me,
            oldest_msg_timestamp_ms: ts,
        })
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
/// `cancel` here is a per-job cancellation token (API-cancel). The outer
/// shutdown token is handled by `run_worker_loop` at the job-dispatch level.
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
) -> anyhow::Result<JobEnd> {
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

        // 3b. Fetch the next batch
        let batch = match source.fetch_older(&job.chat_jid, &anchor, batch_size).await {
            Ok(b) => b,
            Err(e) => return Ok(JobEnd::Failed(e.to_string())),
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

        // 3e. Compute new anchor from the oldest message in this batch
        let new_anchor = oldest_anchor(&batch);

        // 3f. Stuck-anchor guard (R2, ADR 0026)
        let anchor_id_for_check = new_anchor.as_ref().map(|a| a.oldest_msg_id.as_str()).unwrap_or("");
        if anchor_id_for_check == last_anchor_id.as_str() && !anchor_id_for_check.is_empty() {
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

        // 3h. Persist progress: cursor + fetched counter in one logical step
        {
            let _ = store
                .upsert_backfill_cursor(
                    &job.chat_jid,
                    Some(&anchor.oldest_msg_id),
                    Some(anchor.oldest_msg_from_me),
                    Some(anchor.oldest_msg_timestamp_ms),
                    more_remain,
                    exhausted,
                    None,
                )
                .await;
            let _ = store.update_backfill_fetched(job.id, fetched).await;
        }

        // 3i. Evaluate the target
        let batch_oldest_ts = new_anchor.as_ref().map(|a| a.oldest_msg_timestamp_ms);
        let step = evaluate_target(&target, fetched, batch_oldest_ts, more_remain, backstop);

        match step {
            BackfillStep::Done => {
                // Mark exhausted if history is fully consumed
                if !more_remain {
                    let _ = store
                        .upsert_backfill_cursor(
                            &job.chat_jid,
                            Some(&anchor.oldest_msg_id),
                            Some(anchor.oldest_msg_from_me),
                            Some(anchor.oldest_msg_timestamp_ms),
                            false,
                            true, // exhausted
                            Some(now_unix_secs()),
                        )
                        .await;
                }
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
        )
        .await;

        let (new_status, log_msg): (&str, &str) = match &outcome {
            Ok(JobEnd::Done) => ("done", "done"),
            Ok(JobEnd::Parked) => ("paused", "parked (backstop hit)"),
            Ok(JobEnd::Cancelled) => {
                // Shutdown cancel → requeue so it resumes after restart
                ("queued", "cancelled (shutdown) → requeued")
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
        let outcome = store.enqueue_backfill_job(chat_jid, kind, value, 0).await.unwrap();
        let _job_id = match outcome {
            EnqueueOutcome::Accepted { job_id } => job_id,
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
        assert_eq!(a.oldest_msg_timestamp_ms, 1_000);
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel)
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 2, 0, &cancel)
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
        // Target: fetch until oldest message is at or before ts=1000
        // Batch 1 oldest ts=3000 (> 1000) → Continue
        // Batch 2 oldest ts=800 (< 1000) → Done
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
        let job = enqueue_and_claim(&store, "since-chat@s.whatsapp.net", "since", Some(1000)).await;
        let cancel = CancellationToken::new();

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel)
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 3, 4, &cancel)
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel)
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel)
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel)
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel)
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
        let end = process_job(&job, &source, &sink, &store, &pacer, 10, 0, &cancel)
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
        store.enqueue_backfill_job("driver-chat@s.whatsapp.net", "all", None, 0)
            .await.unwrap();

        let store2 = store.clone();
        let notify2 = notify.clone();
        let cancel2 = cancel.clone();
        let source2 = source.clone();
        let sink2 = sink.clone();

        // Run the loop in the background
        let loop_handle = tokio::spawn(async move {
            run_worker_loop(source2, sink2, store2, no_pacer(), 10, 0, notify2, cancel2).await;
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

        store.enqueue_backfill_job("shutdown-chat@g.us", "all", None, 0).await.unwrap();

        let store2 = store.clone();
        let notify2 = notify.clone();
        let source2 = source.clone();
        let sink2 = sink.clone();

        let loop_handle = tokio::spawn(async move {
            run_worker_loop(source2, sink2, store2, no_pacer(), 10, 0, notify2, cancel2).await;
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel)
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

        let end = process_job(&job, &source, &sink, &store, &no_pacer(), 10, 0, &cancel)
            .await
            .unwrap();

        assert_eq!(end, JobEnd::Done);
        assert!(sink.recorded_calls().is_empty(), "no batch must be persisted");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
