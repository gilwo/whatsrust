# Design Review: Historical Message Fetch + Semantic/Lexical Search (v3 — final)

**Date:** 2026-06-24
**Artifact:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md` (updated 2026-06-23)
**Prior reviews:** `2026-06-18` (7 blocking), `2026-06-23` (5 blocking)
**Reviewer:** Cold design-reviewer (independent, no prior context, no prior reviews fed)
**Verdict:** Approve — implementation-ready with no blocking issues

---

## 1. Verdict

Approve with minor implementation-time cautions. This design is implementation-ready with no blocking issues. The two rounds of hardening resolved all blocking concerns, and the three fork decisions are well-justified. The design builds conservatively on proven patterns from the existing codebase (single-worker claim model, atomic TX operations, SQLite-backed durable queues).

---

## 2. Remaining blockers

**None.**

---

## 3. Implementation-time concerns (I1-I7)

- **I1.** Rebase live-test fidelity: budget time for real account testing (connect/send/receive/group-sender), not just compile+unit. Verify group_cache on-demand population post-rebase.
- **I2.** Prune age-DELETE removal: MUST land in phase 1 (storage.rs:665-669 deletion). Flag in F1 checklist.
- **I3.** Drain-worker spawn timing vs config: embedder config field doesn't exist in BridgeConfig yet. Either spawn unconditionally (safe, idles) or wait for F5 (config) to land first. Document the dependency.
- **I4.** Watchdog baseline seed must be final migration step: INSERT INTO metadata after all DDL, measure post-migration size, before user_version bump.
- **I5.** Backfill cursor "exhausted" vs "more_remain" contract: verify phone signals "no more" explicitly or implement empty-response heuristic.
- **I6.** Cancel-race CASE guard is backfill-specific: do NOT copy-paste outbound's unconditional status-write pattern into backfill.
- **I7.** Atomic enqueue-time validation: implement as one run() closure (BEGIN→SELECT→INSERT-or-reject→COMMIT), mirroring claim_next_job. Don't split check and insert into separate async calls.

---

## 4. Residual risks (R1-R6)

- **R1.** Rebase is still a risk gate despite the spike (pivot paths pre-attached; budget 2-3 days).
- **R2.** Media directPath CDN longevity unknown (best-effort, expired flag mitigates).
- **R3.** CJK lexical search broken without embedder (document embedder as strongly recommended for non-Latin).
- **R4.** Search latency under concurrent backfill (add metrics early; reader-pool is the clean fix if >100ms).
- **R5.** Autonomy backstop tuning is deployment-specific (can't optimize at design time; fail-closed error messages guide users).
- **R6.** Set-difference drain query scales with table size (acceptable at current scale; monitor post-launch).

---

## 5. Strengths (S1-S6)

- **S1.** Systematic failure-mode coverage (sidecar down, PDO timeout, ban risk, migration rollback, data-loss windows — all documented with mitigations).
- **S2.** Builds on proven patterns, doesn't reinvent (twin of outbound worker, reuses prune/backup scaffolding, extends instance-lock ethos).
- **S3.** Operability first-class (--rollback flag, circuit-breaker, watchdog seed-on-absence, structured back-pressure errors, SSE paused/cooldown states).
- **S4.** Conservative scope with explicit non-goals and clear extension points.
- **S5.** Two-pass hardening with fork decisions tracked and justified, cross-linked to ADRs.
- **S6.** Grounded in real constraints (actual code line references, measured spike, community ban knowledge).
