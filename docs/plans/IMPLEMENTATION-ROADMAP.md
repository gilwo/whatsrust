# Implementation Roadmap — Historical Fetch + Semantic/Lexical Search

**Date:** 2026-06-25
**Status:** Roadmap skeleton (pre-GO). Detailed per-milestone plans are written at each milestone's start, not now.
**Design:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md` (3 cold reviews, converged, implementation-ready)
**Why this is a skeleton:** Phase 0 is a hard GO/NO-GO gate (the wa-rs rebase). We do not write detailed phase/task plans behind an unvalidated gate — a NO-GO or a fallback-triggering result reshapes everything. Only Phase 0 is planned in detail below; M1/M2 phase detail is deferred to their starts.

---

## Shape

```
  Phase 0 ── GATE (wa-rs rebase) ──┬── GO ──► Milestone 1 ──► Milestone 2
                                   └── NO-GO / fallback ──► pivot (see Phase 0)
```

- **Phase 0 (gate):** rebase wa-rs fork v0.2→v0.6.0; GO/NO-GO decision. Detailed below — it's the next action.
- **Milestone 1 (M1):** historical fetch + lexical (FTS5) search. **Ships independently, no sidecar.** A complete, useful feature on its own.
- **Milestone 2 (M2):** semantic search (embedder sidecar + vectors). Layers onto M1.

Cross-cutting (NOT separate phases — built into each phase as it's reached): **testing** (fake seams per ADR 0025, alongside each layer), **safety/config** (fail-closed config + pacer + daemon-side guards exist *as* the fetch worker is built, not bolted on after).

Tracking: this roadmap = the plan; `FEATURES.md` = live status; the design doc = what/how; ADRs = why. No duplication.

---

## Phase 0 — wa-rs rebase (HARD GO/NO-GO GATE)  [ADR 0002]

The entire feature is conditional on this. Throwaway/exploratory until GO; the productionized rebase only proceeds on GO. Work happens in `../whatsapp-rust` (only the cargo git checkout exists locally today); bump the pinned `rev` in `Cargo.toml` after.

**Steps:**
1. Branch the fork; rebase/rev onto upstream `jlucaso1` v0.6.0 (tag `56ed1b09`).
2. Point whatsrust at it (local path patch via `.cargo/config.toml`).
3. Fix the mechanical breakage the spike predicted (~15-20 sites): `Event::Message(Box,_)` → `(Arc,Arc)`, `.on_event` closure → `Fn(Arc<Event>,_)`, exhaustive match (+4 new variants, −JoinedGroup), `HistorySync` → `Box<LazyHistorySync>`.
4. Resolve the JoinedGroup handler (bridge.rs:2815): move the `group_cache.invalidate` to wherever group-join now surfaces in v0.6, or accept stale-until-refresh. **Verify group_cache on-demand population still works** (review I1).

**GO criteria (ALL must pass — fork R1 hardened):**
- **G1:** whatsrust compiles **and the 89 existing tests pass** against v0.6.
- **G2:** history `WebMessageInfo.message` is usable plaintext through the `extract_content_inner` adapter (ADR 0014).
- **G3:** an ON_DEMAND response is correlatable to its request (or single-flight matching suffices — ADR 0026).
- **LIVE SMOKE TEST (~15-30 min, real account):** connect → send → receive → **verify group-sender parsing** → one small history fetch. Runtime breakage (decryption, media, JID/LID) is exactly what compile+unit-tests miss.

**Decision checkpoint:**
- **GO** → write the M1 detailed plan, begin M1.
- **NO-GO / partial** → pivot (do NOT proceed to M1):
  - G1 deep-breakage beyond time-box → cherry-pick only the history-sync/PDO commits onto the current pin, **or** defer F1 and pick up audit quick-wins (CI, cargo-deny, etc.).
  - G2 encrypted → ADR 0014 fallback B (parallel extractor) — re-scope M1 with that cost.
  - G3 no correlation even single-flight → **F1 not viable on this protocol; STOP and reassess.**

**Exit:** a written GO/NO-GO verdict + (on GO) the pinned `rev` bumped and whatsrust green on v0.6.

---

## Milestone 1 — Historical fetch + lexical search  (detail deferred to M1 start)

**Goal:** explicitly trigger per-chat historical backfill; backfilled + live messages in one unified timeline; lexical (FTS5) search over it. **No sidecar, no embeddings** — fully usable on its own.

**Phases (skeleton — detailed at M1 start):**
- **M1.1 Storage + migration** — unified `messages` table (rename-in-place), sibling tables (`media_refs`, `backfill_cursor`, `backfill_jobs`, `metadata`), FTS5 external-content + triggers, staged migration-mode + circuit-breaker, single-connection model. [ADR 0009/0019/0027/0028-0032/0036] ⚠️ **prune age-DELETE removal lands HERE** (review I2) or first prune tick deletes backfilled history.
- **M1.2 Fetch worker** — `history-source` trait (test seam), durable backfill-job queue, single-worker FIFO pagination loop with 3-level abort, cursor, dedicated pacer, contained-C target model, enqueue-time atomic validation, stuck-anchor guard. [ADR 0003/0010/0020/0026/0033/0035] Safety/config (fail-closed, pacer, queue-depth) built in here, not deferred.
- **M1.3 Search (lexical)** — FTS5 query path; `search_inbound` updated for the unified table. [ADR 0019]
- **M1.4 API/MCP + watchdog** — `/api/history-fetch` trigger/status/cancel, `whatsrust_fetch_history` MCP tool, SSE progress (fuzzy/precise per target-kind); storage-growth watchdog (seed at migration completion). [ADR 0011/0034/0013]

**M1 exit criteria (milestone-level):**
- A user can trigger `all` / `since:T` / `count:N` backfill for a chat/group via API/MCP/CLI and watch SSE progress.
- Backfilled + live messages are unified, searchable by FTS5 (incl. Hebrew/Arabic), retained indefinitely (no age-pruning).
- Migration v7→v8 is safe: staged, backed-up, validated, rollback-able (`--rollback`/`--migrate`), circuit-breaker proven.
- Cancel + graceful-shutdown behave per the 3-level abort model (resume-on-restart for shutdown, terminal for cancel).
- Tests per ADR 0025 (history-source fake, storage temp-DB) green; manual E2E checklist passes.
- **Semantic layer absent by design** — `embed_status` column exists and accrues `pending`, but nothing consumes it yet.

---

## Milestone 2 — Semantic search  (detail deferred to M2 start; do NOT pre-plan phases)

**Goal:** optional semantic search via the embedder sidecar, layered on M1's stored messages.

**Scope (named only — phase breakdown written at M2 start, informed by what M1 teaches):** stateless stdio embedder sidecar + `Embedder` trait + JSON-RPC protocol (ADR 0024); embedding-drain worker — independent task, set-difference work derivation, multi-model retention + purge, decoupled-with-pathological-ceiling (ADR 0015/0017); embeddable-text classification (ADR 0016); FTS5-recall → cosine-rerank search path (ADR 0008); minimal fake-sidecar integration test (ADR 0025).

**M2 exit criteria (milestone-level):**
- With a sidecar configured, semantic search returns relevant results; without one, search cleanly falls back to M1 lexical (no errors, no blocking).
- Drain keeps up under normal backfill (the >100k pathological ceiling is the only coupling); model switch + per-model purge work.
- Multilingual model handles CJK semantically (the lexical gap M1 leaves).

---

## Notes
- Per Q3: phase-level granularity; task breakdown happens when each phase is reached.
- Per Q2: nothing past Phase 0 is detailed until GO.
- Reviews flagged implementation-time concerns (I1-I7) and residual risks (R1-R6) in `_reviewer/design/2026-06-24-...-v3.md` — fold the I-items into the relevant phase's detailed plan when written.
