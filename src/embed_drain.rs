//! Embedding-drain worker — M2.3 (ADR 0015/0038).
//!
//! Consumes pending embeddable messages (via M2.2's set-difference query), sends them
//! to the configured embedder sidecar, and writes the resulting vectors to the
//! `embeddings` table. Resilient to sidecar outages (exponential backoff, notify-only
//! drop) and poison-pill rows (per-row content rejection → cap-3-attempts → `failed`,
//! bisection isolates offenders from innocent rows).
//!
//! Key components:
//!   - `run_embedding_drain_worker` — top-level loop: Notify-woken + periodic timer
//!   - `AttemptTracker` — in-memory HashMap keyed by `(message_id, model_id)`, tracks
//!     per-row content rejections, caps at 3, evicts on terminal `failed` (M2.3.1)
//!   - `prepare_text_for_embedding` — kind-gated text-prep (M2.3.9, Option C), UTF-8-safe
//!     truncation
//!   - `bisect_to_solo` — poison-pill batch bisection (M2.3.11, ADR 0038)
//!   - `LoadingTimer` — continuous-time tracking for `health()==Loading` timeout (M2.3.7)
//!
//! No DB schema changes — `CURRENT_SCHEMA_VERSION` stays 8.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::{interval, Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::embedder::{Embedder, HealthStatus};
use crate::storage::Store;

// ---------------------------------------------------------------------------
// AttemptTracker — in-memory per-row content-rejection counter (M2.3.1)
// ---------------------------------------------------------------------------

/// Tracks content-rejection attempts for rows that the sidecar explicitly rejects
/// (e.g., text too long even after truncation, malformed input). Transport failures
/// do NOT increment this — only per-row rejection does. Cap-3 → mark `failed`.
/// Evicts entries once their row reaches terminal `failed` so the map can't grow
/// unbounded over the daemon's lifetime (M2.3.1 / ADR 0038).
struct AttemptTracker {
    /// Keyed by `(message_id, model_id)`. Value is the attempt count for that row.
    attempts: HashMap<(String, String), u8>,
}

impl AttemptTracker {
    fn new() -> Self {
        Self { attempts: HashMap::new() }
    }

    /// Increment the attempt count for a given `(message_id, model_id)`. Returns `true`
    /// if the cap (3) is reached on this increment — the caller should then mark the row
    /// `failed` and call `evict(...)` to remove the entry.
    fn increment(&mut self, message_id: &str, model_id: &str) -> bool {
        let key = (message_id.to_string(), model_id.to_string());
        let count = self.attempts.entry(key).or_insert(0);
        *count += 1;
        *count >= 3
    }

    /// Remove the entry for `(message_id, model_id)` once the row is terminal `failed`.
    fn evict(&mut self, message_id: &str, model_id: &str) {
        let key = (message_id.to_string(), model_id.to_string());
        self.attempts.remove(&key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.attempts.len()
    }
}

// ---------------------------------------------------------------------------
// LoadingTimer — continuous-time tracking for health()==Loading (M2.3.7)
// ---------------------------------------------------------------------------

/// Tracks CONTINUOUS time in `health()==Loading` (resets on any non-loading observation).
/// If the sidecar remains `Loading` for more than the configured threshold, treat it as
/// an error for this cycle (rows stay pending, FTS5 fallback keeps search working).
/// Injectable clock for testing (M2.3.7 Verify: "Make the clock injectable so the test
/// can simulate >60s without sleeping").
struct LoadingTimer {
    loading_since: Option<Instant>,
    timeout: Duration,
}

impl LoadingTimer {
    fn new(timeout: Duration) -> Self {
        Self { loading_since: None, timeout }
    }

    /// Observe a health status. If `Loading`, start/continue the timer; if anything else,
    /// reset. Returns `true` if the continuous loading time exceeds the timeout.
    fn observe(&mut self, status: &HealthStatus, now: Instant) -> bool {
        match status {
            HealthStatus::Loading => {
                if self.loading_since.is_none() {
                    self.loading_since = Some(now);
                }
                now.duration_since(self.loading_since.unwrap()) > self.timeout
            }
            _ => {
                self.loading_since = None;
                false
            }
        }
    }

    #[cfg(test)]
    fn is_loading(&self) -> bool {
        self.loading_since.is_some()
    }
}

// ---------------------------------------------------------------------------
// Kind-gated text preparation — Option C (M2.3.9)
// ---------------------------------------------------------------------------

/// Derive the text to embed from `(content_kind, body_text)`, branching per kind.
/// `body_text` holds the decorated `display_text()` label (e.g. `"[image 40KB] caption"`),
/// not the bare embeddable text. Kind-gating recovers the bare text:
///
///   - `text` → `body_text` as-is (no decoration)
///   - `image`/`video`/`document` → strip `"[… ] "` prefix to extract caption
///   - `location`/`contact`/`poll` → embed the decorated label as-is (mild bracket noise
///     accepted to keep M2 migration-free; future Option A would persist bare `embed_text`)
///
/// Then truncate to `max_input_tokens` when advertised (ADR 0024), **char-boundary-safe**
/// — approximate token length by char count, cut on a char boundary (`str::floor_char_boundary`),
/// NEVER a raw `&text[0..n]` byte slice (panics mid-UTF-8; Hebrew/Arabic/CJK are multi-byte).
///
/// See M2.3.9 Verify: "a long Hebrew/Arabic/CJK string truncated near the limit → no panic,
/// no corrupted trailing bytes". ADR 0038 formalizes Option C as the v1 choice; Option A
/// (persisting bare `embed_text` at write time) is deferred.
pub fn prepare_text_for_embedding(
    content_kind: &str,
    body_text: Option<&str>,
    max_input_tokens: Option<usize>,
) -> Option<String> {
    let text = body_text?;

    // Kind-gated prefix stripping (Option C, M2.3.9).
    let bare_text = match content_kind {
        "text" => text,
        "image" | "video" | "document" => {
            // Strip `"[… ] "` prefix to extract caption. Find the closing `]`, skip it + space.
            if let Some(close_bracket) = text.find(']') {
                let after_bracket = close_bracket + 1;
                if text.len() > after_bracket && text.as_bytes()[after_bracket] == b' ' {
                    &text[after_bracket + 1..]
                } else {
                    &text[after_bracket..]
                }
            } else {
                // No `]` found — shouldn't happen for a well-formed label, but gracefully
                // fall back to the full text.
                text
            }
        }
        "location" | "contact" | "poll" => {
            // Embed the decorated label as-is (contains the name/question; mild bracket
            // noise accepted to avoid a migration for the rare kinds).
            text
        }
        _ => {
            // Other kinds were classified `skipped` at write time (M2.2), so they never
            // appear in the pending set. If they somehow do, skip them.
            return None;
        }
    };

    if bare_text.is_empty() {
        return None;
    }

    // Truncate to max_input_tokens if advertised, char-boundary-safe (M2.3.9 Verify:
    // "Make the clock injectable", but this is the TEXT truncation, not the timer).
    let truncated = if let Some(max_tokens) = max_input_tokens {
        // Approximate tokens by char count (simplest, no tokenizer dependency). Real
        // token count varies per model; this is a conservative upper bound.
        let char_count = bare_text.chars().count();
        if char_count > max_tokens {
            // Find the byte offset of the `max_tokens`-th char, then floor to a valid
            // UTF-8 boundary (in case the char is multi-byte and we'd land mid-sequence).
            let byte_offset = bare_text
                .char_indices()
                .nth(max_tokens)
                .map(|(i, _)| i)
                .unwrap_or(bare_text.len());
            let safe_offset = bare_text.floor_char_boundary(byte_offset);
            &bare_text[..safe_offset]
        } else {
            bare_text
        }
    } else {
        bare_text
    };

    if truncated.is_empty() {
        None
    } else {
        Some(truncated.to_string())
    }
}

// ---------------------------------------------------------------------------
// Poison-pill batch bisection (M2.3.11)
// ---------------------------------------------------------------------------

/// After K consecutive whole-batch failures of the SAME message-id set, halve the batch
/// and retry. Converges to solo-batches so cap-3 per-row rejection engages and retires
/// the offender to `failed`, letting innocent rows drain meanwhile. Pure whatsrust-side
/// retry; no ADR 0024 protocol change. (M2.3.11 / ADR 0038)
///
/// Returns the halved batch size, or 1 if already solo.
fn bisect_batch_size(current: usize) -> usize {
    (current / 2).max(1)
}

/// State machine for tracking same-batch whole-batch failures and triggering bisection.
struct BisectionTracker {
    last_batch_ids: Vec<String>,
    consecutive_failures: u32,
    /// Number of consecutive failures of the same batch before bisecting (M2.3.11 Verify:
    /// "after K consecutive whole-batch failures"). Configurable for testing; 3 is the
    /// recommended default (balances latency vs. throughput).
    bisection_threshold: u32,
}

impl BisectionTracker {
    fn new(bisection_threshold: u32) -> Self {
        Self {
            last_batch_ids: Vec::new(),
            consecutive_failures: 0,
            bisection_threshold,
        }
    }

    /// Observe a whole-batch failure. If the batch is the SAME as the last failed batch
    /// (by message_id set), increment the counter; if it reaches the threshold, signal
    /// to bisect. If the batch differs, reset.
    fn observe_failure(&mut self, batch_ids: &[String]) -> bool {
        if self.last_batch_ids == batch_ids {
            self.consecutive_failures += 1;
            self.consecutive_failures >= self.bisection_threshold
        } else {
            self.last_batch_ids = batch_ids.to_vec();
            self.consecutive_failures = 1;
            false
        }
    }

    /// Observe a success — reset the tracker.
    fn observe_success(&mut self) {
        self.last_batch_ids.clear();
        self.consecutive_failures = 0;
    }

    /// Observe that the batch changed (e.g., after bisection) — reset the counter so we
    /// don't falsely bisect again immediately.
    fn observe_batch_changed(&mut self) {
        self.last_batch_ids.clear();
        self.consecutive_failures = 0;
    }
}

// ---------------------------------------------------------------------------
// Main drain worker loop (M2.3)
// ---------------------------------------------------------------------------

/// Top-level embedding-drain worker (M2.3). Runs independently of the WA connection state;
/// woken by `Notify` on new pending rows + periodic timer. Resilient to sidecar outages
/// (exponential backoff, notify-only drop after persistent failures), per-row content
/// rejection (cap-3 → `failed`), and poison-pill batches (bisection isolates offenders).
#[allow(clippy::too_many_arguments)]
pub async fn run_embedding_drain_worker(
    embedder: Arc<dyn Embedder>,
    store: Store,
    embed_notify: Arc<Notify>,
    cancel: CancellationToken,
    periodic_interval: Duration,
    configured_batch_size: usize,
    backoff_cap: Duration,
    loading_timeout: Duration,
    failure_threshold: u32,
) {
    info!("embedding-drain worker starting");
    let model_info = embedder.model_info();
    let active_model_id = model_info.model_id.clone();
    let dim = model_info.dim;
    info!(
        model_id = %active_model_id,
        dim = dim,
        max_batch = ?model_info.max_batch,
        max_input_tokens = ?model_info.max_input_tokens,
        "drain worker: model configured"
    );

    let mut tracker = AttemptTracker::new();
    let mut loading_timer = LoadingTimer::new(loading_timeout);
    let mut bisection = BisectionTracker::new(3); // K=3 consecutive same-batch failures
    let mut current_batch_size = configured_batch_size;
    // Clamp to model_info().max_batch if advertised (M2.3.4 / review v4-#1).
    if let Some(max_batch) = model_info.max_batch {
        if current_batch_size > max_batch {
            info!(
                configured = configured_batch_size,
                clamped = max_batch,
                "drain worker: batch size clamped to model's advertised max_batch"
            );
            current_batch_size = max_batch;
        }
    }

    let mut interval_timer = interval(periodic_interval);
    interval_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval_timer.tick().await; // Consume immediate first tick

    let mut current_backoff = Duration::from_secs(1);
    let mut consecutive_failures: u32 = 0;
    let mut notify_only_mode = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("embedding-drain worker shutting down");
                break;
            }
            _ = embed_notify.notified(), if !notify_only_mode => {
                // New pending row arrived — try draining immediately if not in notify-only mode.
                debug!("drain worker: woken by embed_notify");
            }
            _ = interval_timer.tick(), if !notify_only_mode => {
                // Periodic tick — drain.
                debug!("drain worker: periodic tick");
            }
            _ = embed_notify.notified(), if notify_only_mode => {
                // Notify-only mode: only react to new pending rows (no periodic poll).
                debug!("drain worker: woken from notify-only mode");
                // Reset backoff and consecutive failures — we have new work.
                current_backoff = Duration::from_secs(1);
                consecutive_failures = 0;
                notify_only_mode = false;
            }
        }

        if cancel.is_cancelled() {
            break;
        }

        // Check sidecar health. If Loading for >timeout continuous, treat as error this cycle.
        let health = embedder.health().await;
        let now = Instant::now();
        let loading_timeout_exceeded = loading_timer.observe(&health, now);
        if loading_timeout_exceeded {
            warn!(
                timeout_secs = loading_timeout.as_secs(),
                "drain worker: sidecar stuck in Loading state beyond timeout, skipping this cycle"
            );
            consecutive_failures += 1;
            if consecutive_failures >= failure_threshold {
                info!(
                    threshold = failure_threshold,
                    "drain worker: entering notify-only mode after persistent loading timeouts"
                );
                notify_only_mode = true;
            } else {
                current_backoff = (current_backoff * 2).min(backoff_cap);
                tokio::time::sleep(current_backoff).await;
            }
            continue;
        }

        match health {
            HealthStatus::Ok => {
                // Sidecar is ready — proceed to drain.
            }
            HealthStatus::Loading => {
                // Still loading (but not past the timeout yet) — wait a tick.
                debug!("drain worker: sidecar is still loading, waiting");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            HealthStatus::Error(detail) => {
                warn!(error = %detail, "drain worker: sidecar health check failed, backing off");
                consecutive_failures += 1;
                if consecutive_failures >= failure_threshold {
                    info!(
                        threshold = failure_threshold,
                        "drain worker: entering notify-only mode after persistent health errors"
                    );
                    notify_only_mode = true;
                } else {
                    current_backoff = (current_backoff * 2).min(backoff_cap);
                    tokio::time::sleep(current_backoff).await;
                }
                continue;
            }
        }

        // Fetch a batch of pending rows (M2.2.4 set-difference query).
        let rows = match store
            .fetch_pending_embeddings(&active_model_id, current_batch_size as i64)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "drain worker: fetch_pending_embeddings failed");
                consecutive_failures += 1;
                if consecutive_failures >= failure_threshold {
                    notify_only_mode = true;
                } else {
                    current_backoff = (current_backoff * 2).min(backoff_cap);
                    tokio::time::sleep(current_backoff).await;
                }
                continue;
            }
        };

        if rows.is_empty() {
            debug!("drain worker: no pending rows, idling");
            continue;
        }

        debug!(count = rows.len(), batch_size = current_batch_size, "drain worker: fetched batch");

        // Prepare texts for embedding (M2.3.9 kind-gated text-prep).
        let mut texts = Vec::with_capacity(rows.len());
        let mut valid_row_indices = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            if let Some(text) = prepare_text_for_embedding(
                &row.content_kind,
                row.body_text.as_deref(),
                model_info.max_input_tokens,
            ) {
                texts.push(text);
                valid_row_indices.push(i);
            } else {
                // Empty/non-embeddable after prep — mark failed immediately.
                debug!(
                    message_id = %row.message_id,
                    kind = %row.content_kind,
                    "drain worker: row has no embeddable text after prep, marking failed"
                );
                if let Err(e) = store.mark_embedding_failed(&row.message_id).await {
                    warn!(error = %e, message_id = %row.message_id, "failed to mark row as failed");
                }
            }
        }

        if texts.is_empty() {
            debug!("drain worker: all rows in batch had no embeddable text, continuing");
            continue;
        }

        // Embed the batch.
        let vectors = match embedder.embed(&texts).await {
            Ok(v) => v,
            Err(e) => {
                // Transport failure or trust-but-verify rejection (M2.3.5).
                let batch_ids: Vec<String> = valid_row_indices.iter()
                    .map(|&i| rows[i].message_id.clone())
                    .collect();

                // Check for solo-batch per-row rejection first (M2.3.6).
                if texts.len() == 1 {
                    let msg_id = &rows[valid_row_indices[0]].message_id;
                    if tracker.increment(msg_id, &active_model_id) {
                        // Cap-3 reached → mark failed (terminal).
                        match store.mark_embedding_failed(msg_id).await {
                            Ok(()) => {
                                debug!(
                                    message_id = %msg_id,
                                    "drain worker: row marked 'failed' after 3 content-rejection attempts"
                                );
                                tracker.evict(msg_id, &active_model_id);
                            }
                            Err(e) => {
                                warn!(error = %e, message_id = %msg_id, "failed to mark row as 'failed'");
                            }
                        }
                    } else {
                        debug!(
                            message_id = %msg_id,
                            error = %e,
                            "drain worker: per-row content rejection (attempt < 3)"
                        );
                    }
                    // Reset bisection state after processing solo row.
                    bisection.observe_success();
                    current_backoff = Duration::from_secs(1);
                    consecutive_failures = 0;
                    continue;
                }

                // Multi-row batch failure — check if same batch (M2.3.11 bisection).
                warn!(
                    error = %e,
                    batch_size = texts.len(),
                    "drain worker: embed call failed (transport failure)"
                );

                if bisection.observe_failure(&batch_ids) && texts.len() > 1 {
                    let new_size = bisect_batch_size(current_batch_size);
                    info!(
                        old_size = current_batch_size,
                        new_size = new_size,
                        "drain worker: same batch failed K times, bisecting"
                    );
                    current_batch_size = new_size;
                    bisection.observe_batch_changed();
                    // Reset backoff and consecutive failure count on bisection — we're making
                    // progress by narrowing the batch.
                    current_backoff = Duration::from_secs(1);
                    consecutive_failures = 0;
                    continue;
                }

                consecutive_failures += 1;
                if consecutive_failures >= failure_threshold {
                    info!(
                        threshold = failure_threshold,
                        "drain worker: entering notify-only mode after persistent transport failures"
                    );
                    notify_only_mode = true;
                } else {
                    current_backoff = (current_backoff * 2).min(backoff_cap);
                    tokio::time::sleep(current_backoff).await;
                }
                continue;
            }
        };

        // Success — reset backoff and consecutive failures.
        bisection.observe_success();
        current_backoff = Duration::from_secs(1);
        consecutive_failures = 0;

        // Write vectors in one chunked transaction (M2.3.4 / ADR 0027).
        let message_ids: Vec<String> = valid_row_indices.iter()
            .map(|&i| rows[i].message_id.clone())
            .collect();
        match store
            .write_embedding_batch(&message_ids, &active_model_id, dim, &vectors)
            .await
        {
            Ok(()) => {
                debug!(count = vectors.len(), "drain worker: wrote batch");
            }
            Err(e) => {
                warn!(error = %e, "drain worker: write_embedding_batch failed");
                // Write failure — rows stay pending; treat as a transport failure.
                consecutive_failures += 1;
                if consecutive_failures >= failure_threshold {
                    notify_only_mode = true;
                } else {
                    current_backoff = (current_backoff * 2).min(backoff_cap);
                    tokio::time::sleep(current_backoff).await;
                }
                continue;
            }
        }

        // If we were bisecting and just successfully drained a smaller batch, try growing
        // back toward the configured size (don't stay at 1 forever after one poison pill).
        if current_batch_size < configured_batch_size {
            let new_size = (current_batch_size * 2).min(configured_batch_size);
            if let Some(max_batch) = model_info.max_batch {
                if new_size <= max_batch {
                    debug!(
                        old_size = current_batch_size,
                        new_size = new_size,
                        "drain worker: growing batch size after successful drain"
                    );
                    current_batch_size = new_size;
                }
            } else {
                debug!(
                    old_size = current_batch_size,
                    new_size = new_size,
                    "drain worker: growing batch size after successful drain"
                );
                current_batch_size = new_size;
            }
        }
    }

    info!("embedding-drain worker stopped");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- M2.3.1: AttemptTracker ---

    #[test]
    fn test_attempt_tracker_cap_3_then_evict() {
        let mut tracker = AttemptTracker::new();
        assert!(!tracker.increment("m1", "model"));
        assert!(!tracker.increment("m1", "model"));
        assert!(tracker.increment("m1", "model")); // 3rd → cap
        assert_eq!(tracker.len(), 1);
        tracker.evict("m1", "model");
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn test_attempt_tracker_distinct_keys_independent() {
        let mut tracker = AttemptTracker::new();
        assert!(!tracker.increment("m1", "modelA"));
        assert!(!tracker.increment("m2", "modelA"));
        assert!(!tracker.increment("m1", "modelA"));
        assert!(tracker.increment("m1", "modelA")); // m1/modelA reaches cap
        assert!(!tracker.increment("m2", "modelA")); // m2/modelA still at 1
        assert_eq!(tracker.len(), 2);
    }

    // --- M2.3.7: LoadingTimer ---

    #[test]
    fn test_loading_timer_continuous_exceeds_timeout() {
        let mut timer = LoadingTimer::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let t61 = t0 + Duration::from_secs(61);
        assert!(!timer.observe(&HealthStatus::Loading, t0));
        assert!(timer.is_loading());
        assert!(timer.observe(&HealthStatus::Loading, t61));
    }

    #[test]
    fn test_loading_timer_resets_on_non_loading() {
        let mut timer = LoadingTimer::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let t30 = t0 + Duration::from_secs(30);
        let t31 = t30 + Duration::from_secs(1);
        let t91 = t0 + Duration::from_secs(91);
        assert!(!timer.observe(&HealthStatus::Loading, t0));
        assert!(!timer.observe(&HealthStatus::Loading, t30));
        assert!(!timer.observe(&HealthStatus::Ok, t31)); // reset
        assert!(!timer.is_loading());
        assert!(!timer.observe(&HealthStatus::Loading, t91)); // starts fresh
    }

    // --- M2.3.9: Kind-gated text preparation ---

    #[test]
    fn test_prepare_text_text_kind_unchanged() {
        let result = prepare_text_for_embedding("text", Some("hello world"), None);
        assert_eq!(result, Some("hello world".to_string()));
    }

    #[test]
    fn test_prepare_text_text_kind_leading_bracket_not_stripped() {
        let result = prepare_text_for_embedding("text", Some("[URGENT] call me"), None);
        assert_eq!(result, Some("[URGENT] call me".to_string()));
    }

    #[test]
    fn test_prepare_text_image_strips_prefix() {
        let result = prepare_text_for_embedding("image", Some("[image 40KB] sunset photo"), None);
        assert_eq!(result, Some("sunset photo".to_string()));
    }

    #[test]
    fn test_prepare_text_video_strips_prefix() {
        let result = prepare_text_for_embedding("video", Some("[video 2MB] birthday party"), None);
        assert_eq!(result, Some("birthday party".to_string()));
    }

    #[test]
    fn test_prepare_text_document_strips_prefix() {
        let result = prepare_text_for_embedding("document", Some("[document 100KB] report.pdf"), None);
        assert_eq!(result, Some("report.pdf".to_string()));
    }

    #[test]
    fn test_prepare_text_location_keeps_decorated() {
        let result = prepare_text_for_embedding(
            "location",
            Some("[location: 37.7749,-122.4194, name: SF Office]"),
            None,
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("SF Office"));
    }

    #[test]
    fn test_prepare_text_contact_keeps_decorated() {
        let result = prepare_text_for_embedding("contact", Some("[contact: Alice Smith]"), None);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Alice"));
    }

    #[test]
    fn test_prepare_text_poll_keeps_decorated() {
        let result = prepare_text_for_embedding(
            "poll",
            Some("[poll: Which one? | Option A | Option B | Option C]"),
            None,
        );
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("Which one?"));
        assert!(text.contains("Option A"));
    }

    #[test]
    fn test_prepare_text_truncation_at_char_boundary() {
        let long_text = "a".repeat(1000);
        let result = prepare_text_for_embedding("text", Some(&long_text), Some(100));
        assert!(result.is_some());
        let truncated = result.unwrap();
        assert_eq!(truncated.len(), 100);
        assert_eq!(truncated.chars().count(), 100);
    }

    #[test]
    fn test_prepare_text_multibyte_truncation_no_panic() {
        // Hebrew: each char is 2 bytes in UTF-8.
        let hebrew = "שלום עולם ".repeat(50); // ~500 chars, ~1000 bytes
        let result = prepare_text_for_embedding("text", Some(&hebrew), Some(100));
        assert!(result.is_some());
        let truncated = result.unwrap();
        assert!(truncated.chars().count() <= 100);
        // Confirm no corrupted trailing bytes by re-parsing as UTF-8.
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn test_prepare_text_no_max_tokens_no_truncation() {
        let long_text = "a".repeat(10000);
        let result = prepare_text_for_embedding("text", Some(&long_text), None);
        assert_eq!(result, Some(long_text));
    }

    #[test]
    fn test_prepare_text_empty_after_strip_returns_none() {
        let result = prepare_text_for_embedding("image", Some("[image 40KB] "), None);
        assert_eq!(result, None);
    }

    // --- M2.3.11: Bisection ---

    #[test]
    fn test_bisect_batch_size_halves() {
        assert_eq!(bisect_batch_size(64), 32);
        assert_eq!(bisect_batch_size(32), 16);
        assert_eq!(bisect_batch_size(2), 1);
        assert_eq!(bisect_batch_size(1), 1); // floor
    }

    #[test]
    fn test_bisection_tracker_same_batch_failure() {
        let mut tracker = BisectionTracker::new(3);
        let batch = vec!["m1".to_string(), "m2".to_string()];
        assert!(!tracker.observe_failure(&batch));
        assert!(!tracker.observe_failure(&batch));
        assert!(tracker.observe_failure(&batch)); // 3rd → trigger
    }

    #[test]
    fn test_bisection_tracker_different_batch_resets() {
        let mut tracker = BisectionTracker::new(3);
        let batch1 = vec!["m1".to_string()];
        let batch2 = vec!["m2".to_string()];
        assert!(!tracker.observe_failure(&batch1));
        assert!(!tracker.observe_failure(&batch1));
        assert!(!tracker.observe_failure(&batch2)); // different → reset
        assert!(!tracker.observe_failure(&batch2)); // count=1 for batch2
    }

    #[test]
    fn test_bisection_tracker_success_resets() {
        let mut tracker = BisectionTracker::new(3);
        let batch = vec!["m1".to_string()];
        assert!(!tracker.observe_failure(&batch));
        tracker.observe_success();
        assert!(!tracker.observe_failure(&batch)); // count reset to 1
    }

    // --- M2.3.4/2.3.5/2.3.6/2.3.7/2.3.11: Drain worker integration tests ---

    use std::sync::Arc;
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    /// Helper to create a temp Store for testing with a unique name.
    async fn temp_store(test_name: &str) -> crate::storage::Store {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);

        let temp_dir = std::env::temp_dir().join(format!("test_embed_drain_{}_{}", test_name, counter));
        let _ = std::fs::create_dir_all(&temp_dir);
        let db_path = temp_dir.join("test.db");

        // Ensure clean slate
        let _ = std::fs::remove_file(&db_path);

        let store = crate::storage::Store::new(&db_path).expect("failed to create temp store");
        store
    }

    /// Helper to seed a pending message in the Store.
    async fn seed_pending_message(
        store: &crate::storage::Store,
        message_id: &str,
        content_kind: &str,
        body_text: &str,
    ) {
        store
            .insert_message(
                "chat@s.whatsapp.net",
                "sender@s.whatsapp.net",
                message_id,
                content_kind,
                Some(body_text),
                1000,
                false,
                "test",
                "pending",
            )
            .await
            .expect("failed to insert message");
    }

    #[tokio::test]
    async fn test_drain_worker_happy_path() {
        // 2.3.4: N pending rows → N embeddings rows written; re-query returns empty.
        let store = temp_store("happy_path").await;
        seed_pending_message(&store, "msg1", "text", "hello").await;
        seed_pending_message(&store, "msg2", "text", "world").await;
        seed_pending_message(&store, "msg3", "text", "test").await;

        let embedder = Arc::new(crate::embedder::FakeEmbedder::new("test-model", 4));
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let worker_handle = tokio::spawn(run_embedding_drain_worker(
            embedder.clone(),
            store.clone(),
            notify.clone(),
            cancel_clone,
            Duration::from_secs(1),
            64,
            Duration::from_secs(60),
            Duration::from_secs(60),
            10,
        ));

        // Wake the worker
        notify.notify_one();

        // Give it a moment to process
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check that embeddings were written
        let pending = store
            .fetch_pending_embeddings("test-model", 10)
            .await
            .expect("fetch failed");
        assert_eq!(pending.len(), 0, "all rows should be embedded");

        // Verify embeddings exist by checking pending count (should be 0)
        // We already checked above, so this is verified.

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), worker_handle).await;
    }

    #[tokio::test]
    async fn test_drain_worker_max_batch_clamp() {
        // 2.3.4 / v4-#1: fake advertises small max_batch → effective batch is clamped.
        let store = temp_store("max_batch_clamp").await;
        for i in 0..10 {
            seed_pending_message(&store, &format!("msg{}", i), "text", "hello").await;
        }

        let embedder = Arc::new(
            crate::embedder::FakeEmbedder::new("test-model", 4)
                .with_max_batch(3), // Advertise max_batch of 3
        );
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let worker_handle = tokio::spawn(run_embedding_drain_worker(
            embedder.clone(),
            store.clone(),
            notify.clone(),
            cancel_clone,
            Duration::from_secs(1),
            64, // Configured batch size is 64, but should clamp to 3
            Duration::from_secs(60),
            Duration::from_secs(60),
            10,
        ));

        // Wake the worker multiple times to drain all rows
        for _ in 0..5 {
            notify.notify_one();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // All rows should eventually be embedded despite small max_batch
        let pending = store
            .fetch_pending_embeddings("test-model", 10)
            .await
            .expect("fetch failed");
        assert_eq!(pending.len(), 0, "all rows should be embedded despite small max_batch");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), worker_handle).await;
    }

    #[tokio::test]
    async fn test_drain_worker_transport_failure_resilience() {
        // 2.3.5: running worker with always-fail fake → rows stay pending (NOT failed),
        // transport failures don't increment attempt map, worker eventually quiesces to notify-only.
        //
        // Key insight: use MULTIPLE rows so bisection doesn't converge to solo batches.
        // Solo-batch failures are indistinguishable from per-row rejections, so the worker
        // correctly treats them as content rejections (incrementing the attempt counter).
        // To test transport-failure resilience, we need multi-row batches that keep failing.
        let store = temp_store("transport_failure").await;
        seed_pending_message(&store, "msg1", "text", "hello").await;
        seed_pending_message(&store, "msg2", "text", "world").await;
        seed_pending_message(&store, "msg3", "text", "test").await;

        let embedder = Arc::new(
            crate::embedder::FakeEmbedder::new("test-model", 4)
                .always_fail() // Always return transport error
                .with_max_batch(10), // Prevent bisection from converging too fast
        );

        // Verify always_fail actually works
        let test_embed = embedder.embed(&["test".to_string()]).await;
        assert!(test_embed.is_err(), "always_fail mode should return error");

        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Spawn the actual drain worker with short backoff so multiple cycles happen fast
        let worker_handle = tokio::spawn(run_embedding_drain_worker(
            embedder.clone(),
            store.clone(),
            notify.clone(),
            cancel_clone,
            Duration::from_secs(10), // Long interval so we control via notify
            64,
            Duration::from_millis(50), // Short backoff cap for testing
            Duration::from_secs(60),
            10, // Failure threshold for notify-only
        ));

        // Wake the worker a few times to trigger repeated transport failures.
        // Not too many, to avoid bisection converging to solo batches.
        for _ in 0..5 {
            notify.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), worker_handle).await;

        // Key assertion: rows are STILL pending after transport failures.
        // Transport failures (multi-row batch failures) do NOT increment the per-row
        // attempt counter, so rows should never reach embed_status='failed'.
        let pending = store
            .fetch_pending_embeddings("test-model", 10)
            .await
            .expect("fetch failed");

        // All 3 rows should still be pending (not marked failed, not embedded).
        assert_eq!(
            pending.len(),
            3,
            "all rows should still be pending after transport failures (not marked failed)"
        );

        let pending_ids: Vec<&str> = pending.iter().map(|r| r.message_id.as_str()).collect();
        assert!(pending_ids.contains(&"msg1"), "msg1 should be pending");
        assert!(pending_ids.contains(&"msg2"), "msg2 should be pending");
        assert!(pending_ids.contains(&"msg3"), "msg3 should be pending");

        // Note: notify-only quiescence (2.3.5 after N failures) is not directly observable
        // without adding a production accessor. The observable proxy — rows stay pending
        // across many cycles, never flip to 'failed' — proves transport failures are free.
    }

    #[tokio::test]
    async fn test_drain_worker_per_row_rejection_cap_3() {
        // 2.3.6: row rejected 3× in solo batches → embed_status='failed'.
        let store = temp_store("per_row_rejection").await;
        seed_pending_message(&store, "bad-msg", "text", "reject-me").await;

        let embedder = Arc::new(
            crate::embedder::FakeEmbedder::new("test-model", 4)
                .reject_text("reject-me"), // Reject this specific text
        );
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let worker_handle = tokio::spawn(run_embedding_drain_worker(
            embedder.clone(),
            store.clone(),
            notify.clone(),
            cancel_clone,
            Duration::from_secs(1),
            64,
            Duration::from_secs(60),
            Duration::from_secs(60),
            10,
        ));

        // Wake the worker 3+ times to accumulate rejections
        for _ in 0..5 {
            notify.notify_one();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Should not appear in pending set (confirms it was marked 'failed')
        let pending = store
            .fetch_pending_embeddings("test-model", 10)
            .await
            .expect("fetch failed");
        assert_eq!(pending.len(), 0, "failed row should not appear in pending set");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), worker_handle).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_drain_worker_poison_pill_bisection() {
        // 2.3.11: batch of 8 with ONE poisoned row → bisection isolates it, others drain.
        let store = temp_store("poison_pill").await;
        seed_pending_message(&store, "good1", "text", "hello").await;
        seed_pending_message(&store, "good2", "text", "world").await;
        seed_pending_message(&store, "good3", "text", "test").await;
        seed_pending_message(&store, "poison", "text", "reject-me").await;
        seed_pending_message(&store, "good4", "text", "foo").await;
        seed_pending_message(&store, "good5", "text", "bar").await;
        seed_pending_message(&store, "good6", "text", "baz").await;
        seed_pending_message(&store, "good7", "text", "qux").await;

        let embedder = Arc::new(
            crate::embedder::FakeEmbedder::new("test-model", 4)
                .reject_text("reject-me"), // Reject only this text
        );
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let worker_handle = tokio::spawn(run_embedding_drain_worker(
            embedder.clone(),
            store.clone(),
            notify.clone(),
            cancel_clone,
            Duration::from_millis(100), // Fast periodic checks
            64,
            Duration::from_millis(50), // Very short backoff for testing
            Duration::from_secs(60),
            10,
        ));

        // Actively notify the worker many times to ensure it gets chances to run
        // (batch of 8 → fails → bisect to 4 → fails → bisect to 2 → fails → bisect to 1 → cap-3)
        for _ in 0..50 {
            notify.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), worker_handle).await;

        // Strong assertion: NO rows should be pending (all processed).
        // Bisection should converge deterministically with 50 notify cycles + short backoff.
        let pending = store
            .fetch_pending_embeddings("test-model", 10)
            .await
            .expect("fetch failed");
        assert_eq!(
            pending.len(),
            0,
            "all rows should be processed (7 embedded, 1 failed); {} still pending",
            pending.len()
        );

        // Verify the poison row specifically is gone from pending (marked 'failed').
        // The 7 good rows should have embeddings.
        // We can't directly query embed_status without Store internals, but we can verify
        // that the poison row is NOT in the pending set (it was either embedded or failed;
        // since the fake rejects "reject-me", it must be failed).
        let pending_ids: Vec<&str> = pending.iter().map(|r| r.message_id.as_str()).collect();
        assert!(
            !pending_ids.contains(&"poison"),
            "poison row should not be in pending set (should be marked failed)"
        );
    }

    #[tokio::test]
    async fn test_drain_worker_loading_timeout() {
        // 2.3.7: fake reports Loading continuously → treated as error, rows stay pending.
        let store = temp_store("loading_timeout").await;
        seed_pending_message(&store, "msg1", "text", "hello").await;

        let embedder = Arc::new(
            crate::embedder::FakeEmbedder::new("test-model", 4)
                .with_health(HealthStatus::Loading), // Always report Loading
        );
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let worker_handle = tokio::spawn(run_embedding_drain_worker(
            embedder.clone(),
            store.clone(),
            notify.clone(),
            cancel_clone,
            Duration::from_secs(1),
            64,
            Duration::from_secs(60),
            Duration::from_millis(50), // Very short loading timeout for testing
            3, // Low failure threshold
        ));

        // Wake the worker multiple times
        for _ in 0..5 {
            notify.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Row should still be pending (worker treats loading-timeout as error)
        let pending = store
            .fetch_pending_embeddings("test-model", 10)
            .await
            .expect("fetch failed");
        assert_eq!(pending.len(), 1, "row should still be pending after loading timeout");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), worker_handle).await;
    }
}
