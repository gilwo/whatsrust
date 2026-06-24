# Design Review: Historical Message Fetch + Semantic/Lexical Search (v2 — follow-up)

**Date:** 2026-06-23
**Artifact:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md` (updated 2026-06-22)
**Prior review:** `2026-06-18-historical-fetch-semantic-search.md`
**Reviewer:** Cold design-reviewer (independent, no prior context, no prior review fed)
**Verdict:** Approve with changes (5 blocking issues, 5 major risks, 5 minor observations)

---

## 1. Verdict

Approve with changes. The design is structurally sound with thorough consideration of failure modes and extensive ADR coverage, but has 5 blocking issues (shutdown race in migration, watchdog false-alert timing, missing drain-worker spawn, sidecar health state contract ambiguity, and backfill cooldown enforcement gap) and several major risks around the wa-rs rebase gate, embedding drain lifecycle, and migration testing burden.

---

## 2. Structural soundness

Load-bearing assumptions that hold:
- Single SQLite connection + WAL + short-closure locking: correct for single-user scale.
- Anchor-based pagination requires sequential fetch: single-worker + dedicated pacer is the structurally correct solution.
- Reusing extract_content_inner for both live and backfill: viable if spike confirms WebMessageInfo compatibility.
- Fail-closed config + daemon-side enforcement: matches existing instance-lock pattern.

Critical assumption at risk:
- G2 (WebMessageInfo plaintext): spike says LIKELY-PASS on static inspection, but semantic compatibility (LID vs phone-number sender-JIDs in history, timestamp formats) is unverified until runtime. The parallel-extractor fallback is expensive (~500 LOC duplication).

Shutdown-resume model tension:
- ADR 0026 says "shutdown → requeue" but auto-continuing since/all targets (ADR 0033) are durable long-lived intents that must resume "keep going until T, parked at segment K". Works via cursor + re-claim on restart, but shutdown must not clear the target or touch the cursor — only park the job. Not explicitly stated.

---

## 3. Completeness gaps

### Blocking (B1-B5)

**B1. Migration shutdown-race window (ADR 0028).** Ctrl-C during backup phase or validation phase leaves ambiguous state. Backup-in-progress → incomplete .bak file (next start retries, but stale .bak exists). Validation-interrupted → DB at v8 with no pin, validation incomplete, next start skips re-validation → subtle corruption undetected. Direction: signal-mask during migration, or extend pin-write to cover interruption.

**B2. Watchdog false-alert timing (ADR 0013/0036).** Seed-on-absence happens post-migration, capturing the inflated post-migration DB size. Migration overhead + early backfill could sum to 50% growth triggering a false-positive alert. Direction: seed baseline pre-migration (in the migration step itself) or document as expected.

**B3. Embedding-drain worker spawn location.** Design describes the worker (ADR 0015) but never specifies where it spawns (run_bot_session? WhatsAppBridge::start? standalone task?). It's connection-agnostic (unlike backfill) so it should be an independent task. Missing from implementation phasing.

**B4. Sidecar health `loading` state timeout ambiguous (ADR 0024).** How long does the bridge wait on `loading` before treating it as `error`? A stuck sidecar returning `loading` forever blocks drain progress silently. Direction: add a loading-timeout (suggest 60s → treat as error).

**B5. Cooldown enforcement TOCTOU race (ADR 0035).** Two concurrent requests for the same chat both pass the one-active check before either writes its job → two jobs for one chat. Direction: wrap enqueue check+insert in one TX, or add a defensive double-check in the worker's claim step.

### Major (M1-M5)

**M1.** wa-rs spike is static-only; "1-2 days mechanical" is unvalidated by compilation.
**M2.** Drain worker "idles" when sidecar absent but still wakes every 60s (useless churn).
**M3.** Autonomy backstop is global config, not per-request; rigid for power users with mixed chat sizes.
**M4.** FTS5 probe only at migration, not at startup — system-sqlite swap post-migration would crash.
**M5.** Migration validation is structural+smoke, not semantic (accepted risk; TX rollback should prevent partial DDL).

---

## 4. Risk assessment

| # | Risk | Severity | Likelihood |
|---|------|----------|------------|
| R1 | wa-rs rebase breaks more than spike predicts (runtime, not compile) | CRITICAL | MEDIUM |
| R2 | Backfill loop stuck on malformed anchor (phone returns same cursor) | HIGH | LOW |
| R3 | Embedding drain never catches up on large backfill (slow sidecar) | MEDIUM | HIGH |
| R4 | Circuit-breaker pin file corruption/inconsistency | MEDIUM | LOW |
| R5 | Agent triggers N-chat backfill marathon (per-chat cooldown doesn't bound total) | LOW | MEDIUM |

---

## 5. Coupling & evolution

One-way doors (documented, justified):
- Schema v7→v8 rename (lossy rollback via backup)
- FTS5 tokenizer choice (rebuild to switch)

Good two-way doors:
- Embedder model switch (multi-model retention)
- Config as env vars (can add knobs without breaking deploys)

Hard to reverse:
- Single contiguous backward frontier (no arbitrary time windows; rearchitecture if gap-fill needed)
- Config as env-vars only (no runtime reconfiguration; restart required for any change)

---

## 6. Minor observations

1. CJK-without-embedder should be tested early (trigram FTS5 benchmark) before relying on semantic-only.
2. Media hydration: mark expired directPaths as `expired` to avoid re-attempting doomed fetches.
3. The prune task (bridge.rs:1976) still deletes inbound_messages at 30 days — MUST be removed in F1 or it will delete backfilled history.
4. `.env.example` mentioned but never itemized — include as explicit subtask.
5. Watchdog `-shm` measurement is harmless but almost always negligible — a code comment would clarify why it's measured.
6. group_cache initial population after JoinedGroup removal — clarify whether it's on-demand or needs an alternative event.
7. Re-trigger continuation semantics: does re-triggering same chat automatically resume from cursor, or need a flag? Clarify in API spec.
