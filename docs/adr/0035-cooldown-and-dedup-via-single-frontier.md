# 0035. Cooldown and dedup via single frontier model

**Status:** Accepted  
**Date:** 2026-06-22

## Context

Design review #6: "where is per-chat cooldown enforced?". ADR 0033 contained-C expanded it to "does request window OVERLAP an active/recent one?".

**Two concerns:**
- (a) **Enforcement location:** API-time vs worker-time rejection.
- (b) **Overlap reconciliation:** "did since:3mo, now want since:9mo" — is that a duplicate (already covered) or extension (need more)?

**Key insight:** ADR 0003's single contiguous frontier (oldest msg I have for this chat) IS the coverage boundary. Window subsumption logic is REDUNDANT with the frontier. Frontier = subsumption expressed as "how deep" not "which windows".

## Decision

**Enforcement location = ENQUEUE-TIME (API handler), before durable backfill_jobs write.** Validate + dedup + cooldown up front; return structured back-pressure (`{status: already_active, job_id}` / 429 `{retry_after}`). Don't admit doomed/duplicate jobs to the queue. Worker-time checks = backstop only (paranoia). Matches ADR 0021 daemon-side enforcement posture.

**Overlap reconciliation = ONE ACTIVE backfill per chat; reject overlapping with already_active.** At most one running/queued backfill per chat_jid. Second request (any target) while first running → `{status: already_active, job_id: <existing>}`.

**Single contiguous frontier (ADR 0003) dissolves window-tracking:** Cursor's "oldest msg I have for this chat" IS the coverage boundary. "did since:3mo, now want since:9mo" scenario:
1. First request (since:3mo) runs, frontier walks from newest back ~3mo, parks.
2. After completion, second request (since:9mo) resumes from frontier (~3mo back), walks older to 9mo.
3. NO overlap, NO re-fetch, NO window-math. Frontier = automatic subsumption.

**B's window-subsumption logic REJECTED:** redundant with the frontier (frontier already expresses "how deep we've fetched" without bookkeeping which time-windows are covered).

**Cooldown = configurable TIME GATE after a job COMPLETES before a new backfill for that chat is accepted** (rapid re-trigger within N sec → 429 `{retry_after}`). Anti-thrash guard from ADR 0020/0021. Stored: `backfill_cursor.last_backfill_at`; checked at enqueue.

**Rejected alternatives:**
- **B (window-subsumption bookkeeping):** track which time-ranges fetched per chat → compare incoming request window vs stored windows → allow/supersede/reject. Complexity that ADR 0003 deliberately avoided.
- **C (supersede/silent-cancel):** new request cancels old → surprising UX + cancel-races (ADR 0026).

## Consequences

**Positive:**
- One-active-per-chat is simple to enforce (single DB query: any job for chat_jid where status IN ('queued','running'))
- Frontier model dissolves window-overlap complexity (coverage = "how deep", not "which windows")
- Cooldown prevents rapid re-trigger thrashing
- Enqueue-time enforcement keeps queue clean (no doomed jobs)

**Negative:**
- Cannot run two different target kinds for same chat concurrently (e.g., count:100 exploratory + since:30d backfill) — acceptable trade-off for simplicity, rare use case

**Future:**
- If "parallel fetches for same chat" becomes a stated requirement (unlikely), add target-kind multiplexing — but confirm it's needed first.
