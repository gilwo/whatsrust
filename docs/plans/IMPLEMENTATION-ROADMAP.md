# Implementation Roadmap — Historical Fetch + Semantic/Lexical Search

**Date:** 2026-06-25 (updated 2026-09-01)
**Status:** **Phase 0 GATE PASSED — GO** (2026-07-01, commit 615d185). **M1 COMPLETE (2026-07-20)** — M1.1/M1.2/M1.3/M1.4 all DONE and the live E2E smoke test passed on a real account (API/CLI/MCP trigger, SSE `backfill` + `storage_alert`, FTS search, cancel→404). **M2 (semantic search) M2.1–M2.6 CODE-COMPLETE (2026-09-02)** — embedder sidecar contract, drain worker, cosine rerank, multi-model purge, config-wiring/misbehave tests + observability. MVP Python sidecar built. **Only remaining M2 item: live end-to-end validation against a running daemon (hands-on, not automatable).**
**Design:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md` (3 cold reviews, converged, implementation-ready)
**Why this is a skeleton:** Phase 0 is a hard GO/NO-GO gate (the wa-rs v0.6.0 dependency adoption). We do not write detailed phase/task plans behind an unvalidated gate — a NO-GO or a fallback-triggering result reshapes everything. Only Phase 0 is planned in detail below; M1/M2 phase detail is deferred to their starts.

---

## Shape

```
  Phase 0 ── GATE (wa-rs v0.6.0 adopt) ──┬── GO ──► Milestone 1 ──► Milestone 2
                                   └── NO-GO / fallback ──► pivot (see Phase 0)
```

- **Phase 0 (gate):** adopt wa-rs upstream v0.6.0 (dependency bump, NOT a rebase — fork has zero custom commits); GO/NO-GO decision. Detailed below — it's the next action.
- **Milestone 1 (M1):** historical fetch + lexical (FTS5) search. **Ships independently, no sidecar.** A complete, useful feature on its own.
- **Milestone 2 (M2):** semantic search (embedder sidecar + vectors). Layers onto M1.

Cross-cutting (NOT separate phases — built into each phase as it's reached): **testing** (fake seams per ADR 0025, alongside each layer), **safety/config** (fail-closed config + pacer + daemon-side guards exist *as* the fetch worker is built, not bolted on after).

Tracking: this roadmap = the plan; `FEATURES.md` = live status; the design doc = what/how; ADRs = why. No duplication.

---

## Phase 0 — adopt wa-rs upstream v0.6.0 (HARD GO/NO-GO GATE)  [ADR 0002]

The entire feature is conditional on this. **No rebase, no fork, no clone** — the `199-biotechnologies` pin has zero custom commits (it's just upstream frozen at v0.2). This is a **dependency bump**: point `Cargo.toml` at upstream and fix the breakage. **Upstream of record = `oxidezap/whatsapp-rust`** (byte-identical to `jlucaso1`; the project's active home). Done on a branch in THIS repo; the result is a normal committed dep change (no `rev` to push anywhere).

**Steps:**
1. On a branch, edit `Cargo.toml`: repoint all 6 wa-rs deps to `git = "https://github.com/oxidezap/whatsapp-rust", tag = "v0.6.0"` (drop the `199-biotechnologies` rev). `cargo update`. (Verified: all 6 crates + all 7 requested features exist at v0.6.0.)
2. Fix the mechanical breakage the spike predicted (~15-20 sites): `Event::Message(Box,_)` → `(Arc,Arc)`, `.on_event` closure → `Fn(Arc<Event>,_)`, exhaustive match (+4 new variants, −JoinedGroup), `HistorySync` → `Box<LazyHistorySync>`.
3. Resolve the JoinedGroup handler (bridge.rs:2815): move the `group_cache.invalidate` to wherever group-join now surfaces in v0.6, or accept stale-until-refresh. **Verify group_cache on-demand population still works** (review I1).

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

**Exit:** a written GO/NO-GO verdict + (on GO) `Cargo.toml` pointed at upstream and whatsrust green (build + tests + live smoke test).

---

### ✅ VERDICT: GO — 2026-07-01 (commit 615d185)

- **Pinned to wa-rs `oxidezap` main HEAD `9e8c70e2`, NOT the v0.6.0 tag.** The tag (pinned first) compiled + passed 150 tests, but live testing showed **1:1 DM sends failed** with the server nack `463 MissingTcToken`. Root cause traced to upstream bug #730/#731, whose fix (`09a3b0c1`) landed **after** the v0.6.0 tag. Scope-analyzed HEAD (+315, incl. #852 bot API / #866 Message boxing / #893 typed errors — all handled; #911 bincode→prost doesn't affect us; #853 history-sync verified F1-safe) and pinned there.
- **G1 ✅** compile + **150 tests**. **G2 ✅** history `WebMessageInfo` plaintext (bootstrap decoded). **G3 ✅** `peer_data_request_session_id` + `fetch_message_history` intact at HEAD.
- **Live smoke test ✅** connect / receive / **group send (delivered+read)** / **1:1 send (delivered)** / history bootstrap processed.
- **DM fix:** the `463` was resolved by **enabling history sync** (delivers trusted-contact tokens) — see **ADR 0037**. `skip_history_sync` default flipped to `false`; opt-out via `WHATSRUST_SKIP_HISTORY_SYNC=1`.
- **Known limitation (not a blocker):** cold-outreach to non-contacts may still `463` — `nct_salt` (the cstoken path) is WhatsApp-account-gated, not code-fixable.
- **Deferred to M1:** `MsgSecretStore` currently stubbed no-op (can't decrypt history-delivered edits/reactions/poll-votes).

**Next:** M1 — write its detailed phase/task plan, then begin M1.1 (storage + migration). ⚠️ Fold in the reviews' I-items (esp. removing the prune age-DELETE, review I2).

---

## Milestone 1 — Historical fetch + lexical search  (detail deferred to M1 start)

**Goal:** explicitly trigger per-chat historical backfill; backfilled + live messages in one unified timeline; lexical (FTS5) search over it. **No sidecar, no embeddings** — fully usable on its own.

**Phases (detailed in `docs/plans/2026-07-02-M1-detailed-plan.md`):**
- ✅ **M1.1 Storage + migration — DONE 2026-07-03.** Unified `messages` table via **copy-then-drop** (not rename — SCHEMA-first ordering; ADR 0009 correction), sibling tables (`media_refs`, `embeddings`, `backfill_cursor`, `backfill_jobs`, `metadata`), FTS5 external-content + `'delete'`-command triggers (ADR 0019 correction), staged migration-mode + validation + circuit-breaker (`--rollback`/`--migrate`), FTS5 probe, watchdog baseline seed, single-connection model. [ADR 0009/0019/0027/0028-0032/0036] **prune age-DELETE removed (review I2).** Verified: 168 tests + real-data v7→v8 dry-run (Hebrew/Arabic FTS, pristine-v7 backup).
- ✅ **M1.2 Fetch worker — DONE 2026-07-03 (live-validated).** `history-source` trait (test seam), durable backfill-job queue, single-worker FIFO pagination loop with 3-level abort, cursor, dedicated pacer, contained-C target model, enqueue-time atomic validation, stuck-anchor guard. [ADR 0003/0010/0020/0026/0033/0035] Safety/config (fail-closed, pacer, queue-depth) built in here, not deferred. Two-phase async on-demand fetch (`HistoryCorrelator`); connection-gating + LID/PN resolution fixes surfaced live; end-to-end verified (count fetch auto-paginated to exhaustion).
- ✅ **M1.3 Search (lexical) — DONE 2026-07-13.** FTS5 query path; `search_inbound` rewritten (`MATCH` + BM25 `ORDER BY rank` ascending; quote-as-phrase sanitization; single FTS path, EXPLAIN-verified; multilingual + injection-safe). API/MCP shape unchanged. 188 lib / 203 bin green. [ADR 0019]
- ✅ **M1.4 API/MCP + watchdog — DONE 2026-07-16.** `/api/history-fetch` trigger/status/cancel; MCP tools `whatsrust_fetch_history`/`_status`/`_cancel` (33 total); SSE `BackfillProgress` (fuzzy/precise per target-kind); storage-growth watchdog in the periodic prune tick (PASSIVE checkpoint + 3-file stat, ≥50% → WARN + `BridgeEvent::StorageAlert` + baseline reset); temp `WHATSRUST_BACKFILL_TEST` hook retired. **Live E2E passed 2026-07-20 (real account) — M1 COMPLETE.** [ADR 0011/0034/0013]

**M1 exit criteria (milestone-level):**
- A user can trigger `all` / `since:T` / `count:N` backfill for a chat/group via API/MCP/CLI and watch SSE progress.
- Backfilled + live messages are unified, searchable by FTS5 (incl. Hebrew/Arabic), retained indefinitely (no age-pruning).
- Migration v7→v8 is safe: staged, backed-up, validated, rollback-able (`--rollback`/`--migrate`), circuit-breaker proven.
- Cancel + graceful-shutdown behave per the 3-level abort model (resume-on-restart for shutdown, terminal for cancel).
- Tests per ADR 0025 (history-source fake, storage temp-DB) green; manual E2E checklist passes.
- **Semantic layer absent by design** — `embed_status` column exists and accrues `pending`, but nothing consumes it yet.

---

## Milestone 2 — Semantic search  (M2.1–M2.6 CODE-COMPLETE 2026-09-02; live E2E validation PENDING)

**Goal:** optional semantic search via the embedder sidecar, layered on M1's stored messages.

**Phases (detailed in `docs/plans/2026-07-23-M2-detailed-plan.md`):**
- ✅ **M2.1 Embedder sidecar contract & transport — DONE.** `Embedder` trait (`model_info`/`embed`/`health`), `StdioEmbedder` (JSON-RPC stdio client), `FakeEmbedder` test seam, trust-but-verify validation (dim, model_id, timeout). [ADR 0006/0024/0025]
- ✅ **M2.2 Embeddable-text classification — DONE.** `InboundContent::embeddable_text()` (NL-only, distinct from `display_text()`), write-time classification (`pending`/`skipped`), set-difference drain query (`embed_status='pending'` minus already-embedded). Backlog tolerance: pre-M2 rows remain `pending` with `display_text()` stored; tolerated per ADR 0038 (small single-user backlog, raw captions unrecoverable). [ADR 0016/0017/0038]
- ✅ **M2.3 Embedding-drain worker — DONE.** Independent task (spawned alongside backfill/prune, before reconnect loop), periodic wake (default 60s), batch sidecar calls (default 64), transport-failure retry (rows stay `pending`); the in-memory 3-attempt-cap → terminal `failed` path is scaffolded but **dormant** (batch protocol can't distinguish content rejection from transport failure — deferred to M2.6.7); pathological-pending circuit-breaker back-pressures **new backfill enqueues** (`EmbedderBacklogFull`) at a ceiling (default 100k), drain itself keeps running. [ADR 0015/0017/0038]
- ✅ **M2.4 Semantic search path — DONE.** `WhatsAppBridge::search`: embed query → FTS recall (width = max of 200 and limit) → fetch vectors by `(message_id, active_model_id)` → cosine rerank (Rust-side `cosine_similarity()`) → additive lexical fallback (byte-identical to M1 when sidecar absent/down). [ADR 0007/0008/0017/0018]
- ✅ **M2.5 Multi-model retention & explicit purge — DONE.** `embeddings` PK `(message_id, model_id)` supports model switch without purge; `Store::purge_embeddings(model_id)` deletes one model's rows + `incremental_vacuum`; surfaces: API `POST /api/embeddings/purge`, MCP `whatsrust_purge_embeddings` (35 tools total), CLI `purge-embeddings <model_id>`. Returns `{model_id, rows_deleted, bytes_reclaimed}`. [ADR 0017]
- ✅ **MVP sidecar BUILT (2026-08-31).** `scripts/embedder-sidecar.py` (Python + sentence-transformers, `paraphrase-multilingual-MiniLM-L12-v2`, 384-dim, prefix-free, multilingual) + `scripts/run-embedder.sh` convenience wrapper. [ADR 0039]
- ✅ **M2.6 Config, integration test, wiring — DONE (2026-09-02).** Embedder knobs plumbed through `.env`/`BridgeConfig` with config tests (override + absent-is-benign); `.env.example` completed (FAILURE_THRESHOLD); fake-embedder misbehave mode + real-subprocess trust-but-verify integration tests (wrong dim/count/model_id rejected over the process boundary, CI-safe); additive `/api/status` embed-status counts (watchdog already covers footprint); stale multi-worker-backfill comments reworded; inert per-row-rejection semantics reconciled in ADR 0015/0038 (path a — cap-3 dormant pending a protocol signal). **Remaining M2 item: live end-to-end validation against a running daemon with real data (hands-on, not automatable).** [ADR 0023/0024/0025]

**M2 exit criteria (milestone-level):**
- ✅ With a sidecar configured, semantic search returns relevant results; without one, search cleanly falls back to M1 lexical (no errors, no blocking). — **CODE-COMPLETE, E2E validation pending.**
- ✅ Drain keeps up under normal backfill (the >100k pathological ceiling is the only coupling); model switch + per-model purge work. — **CODE-COMPLETE, E2E validation pending.**
- ⏸️ Multilingual model handles CJK semantically (the lexical gap M1 leaves). — **Deferred to live testing.**

---

## Notes
- Per Q3: phase-level granularity; task breakdown happens when each phase is reached.
- Per Q2: nothing past Phase 0 is detailed until GO.
- Reviews flagged implementation-time concerns (I1-I7) and residual risks (R1-R6) in `_reviewer/design/2026-06-24-...-v3.md` — fold the I-items into the relevant phase's detailed plan when written.
