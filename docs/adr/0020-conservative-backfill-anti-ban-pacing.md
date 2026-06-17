# 0020. Conservative backfill anti-ban pacing

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Repeated on-demand history requests (PDO, Protocol Data Object) to own phone are an automation fingerprint. WhatsApp bans accounts for aggressive scraping behavior.

PDO is anchor-based (oldest_msg_id + count per batch) → structurally sequential (batch N+1 needs N's response to get next anchor).

whatsrust ships with an outbound `SendPacer` (token-bucket burst + passive refill + jitter). Backfill must NOT consume send budget (different operation type, separate rate limit).

## Decision

**Dedicated backfill pacer, SEPARATE from outbound `SendPacer`:**
- `burst = 1` (no burst, strictly one-at-a-time)
- Base interval ~4 seconds/batch with ±40% jitter (ALWAYS jittered; periodic timing = automation fingerprint)
- **Strictly sequential:** await each batch response → extract new oldest anchor → pace → next request (structurally required by anchor-based protocol anyway)

**Response timeout → exponential backoff → PAUSE job** (resumable via cursor). Never hammer on timeout.

**Occasional randomized LONG pause** (secondary insurance):
- Every N batches (N randomized ~5-15)
- Pause duration randomized ~20-90 seconds
- Mimics human distraction (user puts phone down, gets interrupted)

**PRIMARY defense = conservative AVERAGE rate** (4s/batch ≈ 15 batches/min ≈ 960 msgs/min at 64/batch). Jitter + pauses are SECONDARY insurance.

**All parameters configurable** (interval, jitter %, long-pause cadence/range) but defaults are deliberately conservative.

**UX impact (accepted contract):**
- ~4s/batch × 64msg/batch → 1k msgs ≈ 1-2 min, 5k ≈ 6-10 min, 20k ≈ 30-45 min
- Backfill is a BACKGROUND MARATHON not interactive
- Async job + immediate `job_id` return (no spinner)
- SSE progress stream: `"1280/~5000, more remain"`
- Resumable via cursor (impatient users get fast partials via `max_messages` cap)
- FTS5 + live messages work DURING backfill (enrichment, not blocking)

**REQUIRED UX features:**
1. SSE emits EXPLICIT `paused` / `cooldown` state + resume-hint during long pauses (stalls read as deliberate caution NOT hangs)
2. Trigger endpoint returns rough ETA
3. Documentation notes semantic coverage lags fetch (FTS5 immediate, embeddings drain behind)

## Consequences

**Positive:**
- Conservative average rate is primary ban defense (mimics patient human review)
- Jitter + randomized pauses add secondary anti-fingerprint noise
- Sequential protocol enforcement is natural (anchor-based)
- Explicit paused/cooldown UX prevents "is it hung?" confusion
- Resumable cursor absorbs interruptions gracefully

**Negative:**
- Slow throughput (20k msgs ≈ 30-45 min) — by design, not a bug
- Long pauses feel laggy (mitigated by explicit SSE `paused` state)
- No fast path for "trusted" users (uniform pacing reduces ban risk for all)

**Rejected:**
- **Elaborate human-simulation** (e.g., typing indicators, mouse movement) — diminishing returns vs opaque detector, added complexity
- **Shared pacer with outbound send** — conflates different operation types, risks send starvation during backfill

**Deferred:**
- Adaptive pacing based on response latency (option B) — complex, conservative fixed rate is sufficient for v1
