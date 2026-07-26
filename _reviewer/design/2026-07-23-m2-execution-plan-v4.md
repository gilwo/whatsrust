# Design Review: M2 Detailed Execution Plan (Semantic Search) — v4 (follow-up)

**Date:** 2026-07-23
**Artifact:** `docs/plans/2026-07-23-M2-detailed-plan.md` (revised at commit `d1a694c` — folded in the v3 cold review + added ADR 0038)
**Prior reviews (of this plan):** `…-execution-plan.md` (v1 — approve w/ changes 1+12), `…-v2.md` (v2 — needs rework 2+7), `…-v3.md` (v3 — approve w/ changes 3+7)
**Reviewer:** Cold design-reviewer (independent, no prior context, no prior reviews fed)
**Verdict:** Approve with changes — 1 blocking, 8 non-blocking

> Coordinator note: the single blocker (B1, recall_width) is a **regression introduced by the v2-#2
> fold-in** — the review process catching a bug a prior fold-in created. Independently confirmed by the
> coordinator (`api.rs` clamps `limit` to ≤200, so `min(200, limit)` == `limit` → no widening).

---

## Verdict

**Approve with changes.** The plan is unusually well-grounded — nearly every code citation checked against the actual source was accurate down to the line number, and the sub-milestone sequencing, schema-migration-free scoping, and failure-mode handling (poison-pill bisection, breaker anti-join, shutdown-kill) are sound. But there is one specification defect that would ship a functionally-working but value-undermined semantic search for the common case, plus concrete gaps in sidecar-protocol conformance, shutdown correctness, and a purge feature that silently fails its own exit criterion on any pre-existing database (plausibly this project's own). None require redesign — all fixable in plan text before M2.1/M2.4/M2.5 coding.

---

## Blocking issues

### B1 — `recall_width` formula in M2.4.3 provides zero pool-widening for the common (small-`limit`) case

M2.4.3 step (2): *"recall … internally widened to `recall_width` candidates `>= min(200, requested_limit)`."* Since `api.rs`/`mcp.rs` clamp `limit` to ≤200 (`api.rs:949`, `.max(1).min(200)`), `requested_limit` is always ≤200, so `min(200, requested_limit)` **always equals `requested_limit`** — i.e. **no widening at all** for any normal request (MCP default `limit=20`, `mcp.rs:290`). This collapses "FTS5 recall ~50-200 candidates → cosine rerank → top-k" (`2026-06-17-design.md:65-66,244-245`) into "reorder the already-narrow BM25 top-N" — losing the entire point of a wider recall net (surfacing semantically-relevant messages BM25 ranked outside the top-N). A leftover of the v2-#2 fix, which targeted only the `limit=200` boundary. Verify test (c) only checks `limit=200`, so the common-case no-op would pass CI and ship silently.

**Direction:** decouple recall width from the final limit — `recall_width = max(200, requested_limit)` — then truncate to `requested_limit` at step (5). Add a Verify case at a *small* limit (e.g. 10) asserting the FTS recall requested a materially wider candidate set than 10.

---

## Non-blocking concerns / implementation-time cautions

1. **No task clamps `WHATSRUST_EMBEDDER_BATCH_SIZE` against `model_info().max_batch` (ADR 0024).** ADR 0024 says `max_batch` is advertised so "the bridge respects it," but no M2.3 task reads/clamps it. If a sidecar's `max_batch` < configured (default 64), every batch looks like a whole-batch failure to 2.3.5, and 2.3.11's bisection (designed for one bad row) repeatedly halves toward solo on every cycle — sustained thrash, no diagnostic distinguishing "batch too big" from "poison pill." **Direction:** clamp effective batch to `min(configured, model_info().max_batch)` once cached, with a fake-sidecar test advertising a small `max_batch`.

2. **M2.1.9's "~5s graceful window" is not a synchronization guarantee for the drain task.** `wait_stopped(5s)` polls `BridgeState::Stopped`, set unconditionally by the reconnect loop (`bridge.rs:2269`) without joining the separately-spawned prune/backfill/(new)drain tasks. No barrier ensures the drain task's cancel-triggered child-kill runs before `wait_stopped` returns and `main.rs` calls `std::process::exit(0)` (which skips destructors → `kill_on_drop` never fires, per the plan's own diagnosis). Likely-benign in practice (multi-thread runtime schedules the cancel branch promptly) but a race, not a guarantee; crash-loops could leak orphaned sidecar children (each holding a loaded model's memory). **Direction:** explicitly `join!` the drain task's `JoinHandle` (short-timeout-bounded) in the shutdown sequence before `process::exit`, or document the residual small-probability orphan risk as an accepted tradeoff (like F-D documents its own).

3. **`incremental_vacuum` is very likely a *permanent* no-op on every pre-existing whatsrust DB, undermining M2.5.2's "reclaims space" exit criterion.** `storage.rs:668` sets `PRAGMA auto_vacuum=INCREMENTAL` on open, but it only takes effect on a still-empty DB (SQLite constraint; a non-empty file needs a full `VACUUM`, which the project avoids per ADR 0017 R-prior5). Since the pragma predates M2, **any DB that already had tables when it shipped is stuck at `auto_vacuum=NONE` forever** — plausibly this project's own (git shows `whatsapp.db.pre-migration-v0-*.bak`). M2.5.2 correctly identifies the mechanism + adds a WARN, but two gaps: (a) the Verify's "allow 0 on a non-incremental temp DB" is misleading — a *fresh* temp DB gets INCREMENTAL correctly (pragma runs before any table), so the specified test won't exercise the real no-op path; (b) no remediation for a stuck real install (the only fix, one-time full `VACUUM`, is undocumented and contradicts the "never full VACUUM" stance). **Direction:** add a test fixture that pre-creates tables before opening via `Store` (simulating an old DB) to actually exercise the no-op path; and decide/document recourse — e.g. an opt-in one-time `--vacuum-once` maintenance path, or a prominent note that "reclaims space" holds only for DBs created after the pragma shipped.

4. **M2.3.9 truncation to `max_input_tokens` has no token-counting heuristic and risks a UTF-8 panic on this project's own data.** whatsrust has no tokenizer (that's the sidecar's job), so "truncate to the advertised limit" needs an approximation the task doesn't specify. A naive `&text[0..n]` byte-slice panics if `n` isn't a char boundary — a real risk given "user data is Hebrew/Arabic" (`2026-06-17-design.md:22`), where multi-byte chars are the norm. **Direction:** specify the unit (word-boundary up to N words, or `floor_char_boundary`-safe byte truncation), with a test on a long Hebrew/Arabic/CJK string truncated near the limit (no panic, no corrupted trailing bytes).

5. **M2.2.2 undercounts the `embed_status` wiring surface.** It says wire "both call sites" + add `embed_status` to `insert_message`, but the live-ingest site (`bridge.rs:2684`) calls `insert_inbound(...)` — a wrapper (`storage.rs:1291-1301`) hardcoding `from_me=false, source="live"` with **no** `embed_status` param, delegating to `insert_message`. `insert_inbound` has ~15 other call sites (all `storage.rs` tests). Changing `insert_message`'s signature doesn't wire the live path; `insert_inbound` must also change, cascading to those tests. **Direction:** call out that `insert_inbound` is the real live entry point and must gain the param (or be bypassed by a direct `insert_message` call), and that its test sites need mechanical updates.

6. **M2.3.8's "compose into the atomic enqueue closure" touches ~25 `enqueue_backfill_job` test call sites** (fixed 6-positional-arg signature, `storage.rs`/`backfill.rs`). Not a design flaw (the composition is sound — `enqueue_backfill_job` `:1672-1764` is the right shape, and `api.rs:1114-1121` has `bridge` in scope for `active_model_id`) — but note the mechanical cost so it isn't a surprise.

7. **Stale M1-era "deferred to M2" multi-worker-backfill comments are unreconciled.** `bridge.rs:727-729`, `main.rs:327-331` (a runtime `warn!`), `.env.example:82/117` promise a *different* "M2" (multi-worker backfill concurrency), but the roadmap scopes M2 to semantic search and ADR 0026 treats single-worker FIFO as deliberate. The plan's own "read the live code + surface discrepancies" methodology (§0 F-A..F-J) missed this naming collision. **Direction:** add a §0 finding noting the stale comments and either correct them (they don't mean *this* M2) or explicitly carry multi-worker backfill forward to a future milestone.

8. **`messages.embed_status='pending'` is permanently stale by design** (2.3.4) — reasonable and justified, but worth a schema/doc comment at the column definition itself so a future reader doesn't assume it reflects current embedding state without cross-referencing `embeddings`.

---

## Risks

| Risk | Severity | Likelihood | Note |
|---|---|---|---|
| Recall-width bug (B1) ships as literally specified | High (undermines core value prop) | Medium-High if not fixed in plan text first | Cheap fix, high value |
| Orphaned sidecar child on shutdown race (#2) | Medium (leak, not data loss) | Low normal, higher under crash-loop | Mitigation exists but unguaranteed ordering |
| Purge `bytes_reclaimed` permanently 0 on real DBs (#3) | Medium (feature partially non-functional) | High for any pre-pragma DB, plausibly this project's | WARN exists; no remediation documented |
| Truncation panic on multibyte text (#4) | Medium (worker crash, recoverable) | Low-Medium — near-limit Hebrew/Arabic/CJK | Easily avoided with the right idiom |
| Batch-size/`max_batch` mismatch thrash (#1) | Medium (throughput) | Low-Medium — depends on chosen sidecar | No diagnostic vs a genuine poison pill |
| Call-site fan-out (#5, #6) | Low | High (will occur) | Implementation friction, not design risk |

---

## Strengths

- **Exceptional code-grounding.** Independently verified essentially every §0 `file:line` anchor (F-A..F-J) against source — `embeddings` schema (`storage.rs:233-239`), FTS join (`:1385-1421`), `display_text()` decoration (`bridge.rs:433-492`), the anti-join already smoke-tested in `validate_migration_post_commit` (`:563-568`), `mcp.rs` server-vs-client framing (`:13-67`), missing `tokio` `process` feature (`Cargo.toml:37`), `handle_search` (`api.rs:950`) — all matched precisely.
- Correctly scopes M2 as schema-migration-free (F-D) — the biggest, well-justified, reversible risk reduction.
- Exhaustive, correct classification (7 embeddable / 8 skipped covers all 15 `InboundContent` variants).
- Real, substantive bug-catching across rounds (raw-`pending` breaker, poison-pill livelock, missing query-embed, single-shared-embedder) grounded in the batch-only protocol (ADR 0024) and single-connection model (ADR 0027).
- Disciplined shape-preservation (F-H): `api.rs` the only call-path change, `mcp.rs` none (HTTP proxy) — verified.
- No new one-way doors — in-memory tracking, Option-C text-prep, backlog tolerance all reversible/promotable.
