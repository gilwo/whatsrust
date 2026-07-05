//! Historical backfill seam types, trait, fake, and target model.
//!
//! Wave A: storage + logic foundation — no live WhatsApp, no worker task.
//! All types here are unit-testable without a network connection.
//!
//! Key types:
//!   - `Anchor`         — per-chat backfill frontier (oldest known message)
//!   - `HistoryBatch`   — one page of fetched messages
//!   - `HistorySource`  — async trait seam; real impl (Wave B) wraps `Client`
//!   - `FakeHistorySource` — scripted canned responses for tests
//!   - `FetchTarget`    — contained-C target model (ADR 0033)
//!   - `BackfillStep`   — pure stop-condition output
//!   - `evaluate_target` — pure, total stop-condition function

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
}
