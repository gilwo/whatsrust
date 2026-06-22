# Design Review: Historical Message Fetch + Semantic/Lexical Search

**Date:** 2026-06-18
**Artifact:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md`
**Reviewer:** Cold design-reviewer (independent, no prior context)
**Verdict:** Approve with changes (7 blocking issues)

---

## 1. Verdict

**Approve with changes.** The design is structurally sound and well-reasoned, but has several blocking issues that must be resolved before implementation: critical open questions about wa-rs compatibility lack fallback plans, the schema migration has a destructive operation without rollback story, the backfill worker concurrency model is underspecified, and several safety mechanisms have implementation gaps that could lead to production failures.

---

## 2. Structural soundness

### Load-bearing assumptions

**wa-rs spike is informational, not a gate.** The design hangs heavily on ADR 0002's rebase spike resolving three critical unknowns (WebMessageInfo decryption, event correlation, API breakage), but treats the spike as "step 0" rather than a GO/NO-GO decision point. If the spike reveals incompatibilities, multiple downstream decisions collapse:
- ADR 0014 single-extraction-path collapses into fallback B (500+ LOC duplication)
- The backfill worker can't correlate ON_DEMAND responses to per-chat pending jobs
- No contingency if breakage exceeds a threshold

**Recommendation:** Define spike acceptance criteria (max 3 days to resolve breakage, adapter < 200 LOC, correlation API exists) and an explicit escalation path if any criterion fails.

### Concurrent backfill concurrency cap is contradictory

"Sequential await-response loop" (one worker) conflicts with "concurrency cap 1-2 active jobs." If one worker processes sequentially, the cap is meaningless. If multiple workers exist, the design doesn't specify how they coordinate (semaphore? claim-with-lock?).

**Recommendation:** Clarify the worker topology (1 worker, sequential? N workers, parallel up to cap?) and where the cap is enforced.

### Backfill pacing is ad-hoc

Outbound uses `SendPacer` (token-bucket). Backfill pacing is described only as behavior (4s, jitter, pauses) with no component abstraction. Risk of duplicating/diverging rate-limit logic.

---

## 3. Completeness gaps

1. **Schema migration rollback story incomplete.** Once `ALTER TABLE RENAME` commits, rolling back to v7 code requires backup-restore (up to 6h data loss). No forward-migration escape hatch specified. **Recommendation:** Document one-way nature + require fresh backup (<1h stale) before migrating.

2. **Metadata table missing.** The watchdog requires a `last_alerted_size` stored persistently, but no `metadata` table exists in the schema. Add to v7→v8 migration.

3. **Per-chat cooldown enforcement location unspecified.** API-time (immediate rejection) vs worker-time (delayed no-op) is architecturally different. Specify which + what the API returns.

4. **`max_messages` schema inconsistency.** The `backfill_jobs` table has both `mode TEXT` and separate `since_ts INTEGER, max_messages INTEGER` columns. Unclear whether these compose or conflict. Two implementers would produce incompatible schemas.

5. **Media hydration mechanism undefined.** `media_refs` table exists, but no API endpoint for "hydrate media for message X" is specified. An implementer wouldn't know what to build.

6. **Embedding drain worker wakeup on config change is missing.** User fixes broken sidecar without restarting daemon → drain worker stuck in 60s backoff → semantic coverage never catches up (hours MTTR for a typo). **Recommendation:** Add a manual kick endpoint or make health() success reset the backoff.

7. **HistorySource trait buried in testing ADR only.** It's part of the design, not just a test seam — move to the main design doc's fetch-model section.

---

## 4. Risk assessment (ordered by severity × likelihood)

| # | Risk | Severity | Likelihood | Blast radius |
|---|------|----------|------------|-------------|
| 1 | wa-rs spike reveals insurmountable incompatibility | HIGH | Medium | Entire feature blocked, no pivot path |
| 2 | FTS5 unavailable on system SQLite bricks migration | HIGH | Low | User locked out, backup-restore only |
| 3 | Backfill cancel race → partial backfill marked complete | HIGH | Medium | Cursor corrupted, manual DB surgery |
| 4 | Embedding drain starves outbound sends (SQLite connection contention) | MEDIUM | Medium | Send latency spikes during backfill |
| 5 | Per-model purge without proper vacuum leaves "ghost" storage | MEDIUM | High | User confusion (disk doesn't shrink) |
| 6 | Long-pause states indistinguishable in SSE (stuck vs cooldown) | MEDIUM | High | Support burden, user confusion |
| 7 | Community JID rejection point unspecified (job fails vs enqueue rejects) | MEDIUM | Low | Cryptic errors |
| 8 | Dotenvy silent failure on malformed .env | LOW | Low | Config silently ignored |

---

## 5. Coupling & evolution

**One-way doors (hard to reverse):**
- `inbound_messages → messages` rename: irreversible without backup-restore
- Multi-model embeddings PK `(message_id, model_id)`: purge is destructive (re-drain rebuilds, but hours)

**Tight coupling:**
- FTS5 external-content relies on triggers → bulk imports/manual DELETEs bypass triggers → silent FTS5 drift. **Recommendation:** Add a health-check query surfacing FTS5 row-count vs messages row-count.

**Good abstraction boundaries:**
- `HistorySource` trait (two-way door, can add impls)
- `Embedder` trait (transport-neutral, can swap stdio→HTTP without touching storage)
- Config evolution (env vars, can add knobs without breaking existing deploys)

---

## 6. Minor observations

- JID normalization at enqueue time (prevent duplicate backfill jobs via variant JIDs)
- ETA calculation unspecified (phone doesn't report total in advance → return null, not a fake ETA)
- `dim INTEGER` per embeddings row is redundant (`model_id` implies dim via sidecar's `model_info`) — minor, future optimization
- `remove_diacritics 2` is a magic number — add a comment explaining modes (0=off, 1=legacy, 2=improved)
- Backfill should probably ignore the `allowed_numbers` inbound filter (it's a processing filter, not a storage filter) — clarify
- `from_me` derivation for backfilled messages: specify that `WebMessageInfo.key.from_me` field provides this (verify during spike)
- Watchdog alert should hint "check for long-running transactions" if WAL grows unbounded
- Backfill job claim priority (FIFO? user-triggered > auto-resume?) unspecified

---

## Summary of blocking issues

1. **Spike is not a gate:** Define acceptance criteria and escalation path.
2. **Backfill worker topology underspecified:** 1-worker-sequential vs N-workers-parallel.
3. **FTS5 availability unchecked:** Pre-migration probe or document system-sqlite unsupported.
4. **Schema migration rollback story incomplete:** Document one-way + require fresh backup.
5. **Metadata table missing:** Add to v7→v8 migration.
6. **Per-chat cooldown enforcement location unspecified:** API-time vs worker-time.
7. **`max_messages` schema inconsistency:** Reconcile mode vs separate columns.
