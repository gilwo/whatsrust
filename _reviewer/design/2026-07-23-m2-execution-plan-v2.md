# Design Review: M2 Detailed Execution Plan (Semantic Search) — v2 (follow-up)

**Date:** 2026-07-23
**Artifact:** `docs/plans/2026-07-23-M2-detailed-plan.md` (revised at commit `d05e226` — folded in the v1 cold review)
**Prior review (of this plan):** `2026-07-23-m2-execution-plan.md` (v1 — approve with changes, 1 blocking + 12 non-blocking)
**Prior reviews (of the underlying F1 *design*):** `2026-06-18`, `2026-06-23-v2`, `2026-06-24-v3` (final — implementation-ready)
**Reviewer:** Cold design-reviewer (independent, no prior context, no prior reviews fed — explicitly did NOT read the v1 review)
**Verdict:** Needs rework (narrowly) — 2 blocking, 7 non-blocking

> Coordinator note: the reviewer evaluated the plan's own "Review fold-in" section on its merits rather than
> treating it as pre-validated. The two blocking issues below are NEW — neither the v1 review nor the thrice-
> reviewed design blueprint caught them. Both code claims independently re-verified by the coordinator
> (`bridge.rs:433-492` for B2; the plan's own M2.4 task text for B1).

---

## Method note

Read the plan in full, the design blueprint, the roadmap, the M1 execution plan (for format/precedent), ADRs 0006/0007/0008/0009/0015/0016/0017/0018/0022/0023/0024/0025/0027/0031, and the as-built code the plan cites (`src/storage.rs`, `src/bridge.rs`, `src/api.rs`, `src/mcp.rs`, `Cargo.toml`). Did NOT read the prior cold review — evaluated the plan's "Review fold-in" independently.

Every one of the plan's §0 Findings (F-A through F-J) checked out exactly against the live code. Findings below are things the plan (and the review it folded in) did not catch.

---

## 1. Verdict

**Needs rework** — but narrowly. The overall shape (sub-milestone sequencing, the F-A–F-J findings, the 13-item review fold-in) is sound and unusually well fact-checked against the real codebase. However, two concrete, load-bearing mechanisms of the milestone's core deliverable (M2.4 semantic search, and M2.2/M2.3's text-preparation for 3 of the 5 embeddable content kinds) are underspecified to the point of not being implementable as written. Neither requires re-architecting the plan; both need new tasks/decisions added before M2.4 (and part of M2.2/M2.3) coding starts. M2.1, M2.5, M2.6, and most of M2.3 are implementation-ready as written.

---

## 2. Blocking issues

### B1 — No task produces a query vector; "cosine rerank" in M2.4.3 has nothing to rerank against

M2.4.3 says: FTS5 recall candidates → "fetch their vectors from `embeddings`" → "cosine rerank in Rust." This fetches only the *candidates'* vectors. Cosine similarity requires two vectors — nowhere does the plan (or the design doc §Search) call `Embedder::embed(&[query_text])` to get a vector for the search **query** itself. M2.4.1 defines cosine as a pure `(Vec<f32>, Vec<f32>) -> f32` function; nothing supplies its second argument at search time. Without it, "semantic rerank" is not a defined operation — and it's the entire payoff of M2.

Downstream consequences the plan doesn't address once you try to fix it:
- **Where does the query-embed call live?** `Store` (storage.rs) holds no reference to `Embedder` (owned by `WhatsAppBridge`/the drain worker per M2.3.2-3). `api.rs::handle_search` currently calls `bridge.store().search_inbound(jid, Some(q), limit, None)` directly (api.rs:950). Composing FTS-recall (Store) + query-embed (Embedder, elsewhere) + vector-fetch (Store) + cosine-rerank (pure fn) requires a new orchestration point — most naturally a new `WhatsAppBridge::search(...)` method — which means **`api.rs::handle_search`'s call site DOES change**, contradicting M2.4.7's claim that "api.rs::handle_search / mcp.rs need no changes." (The *response shape* is unaffected; the *call path* is not.)
- **New failure mode, unlisted in M2.4.4's fallback triggers:** a live, synchronous `embed()` on the query at request time can time out / fail even when cached `health()` says "ok" (sidecar just crashed, or mid-batch on a large drain). M2.4.4 enumerates {no embedder, unhealthy, zero vectors, all candidate vectors missing} — "the live query-embed call itself failed" isn't one and needs to be.
- **Concurrency over the single child process:** the drain worker (M2.3.4, batches of 64) and a live search now both call the *same* `StdioEmbedder` child. The plan directs implementers to mirror `mcp.rs`'s wire conventions (F-G/M2.1.3), and `mcp.rs::run_mcp_server` (mcp.rs:13-46) is a strictly sequential, one-request-in-flight blocking loop. Built the same way, a search's query-embed will either corrupt stdin if it races the drain worker's write, or block behind an in-progress 64-text batch — turning "negligible added latency" (M2.4.4) into a multi-hundred-ms stall during active drain. No task addresses `Embedder`/`StdioEmbedder`'s concurrent-access semantics.

**Direction:** add an explicit task before M2.4.3 that (a) decides the orchestration owner (bridge-level `search()`, not `Store`), (b) adds the query-embed call with its own timeout and an explicit fallback-to-lexical trigger on its failure, and (c) specifies `StdioEmbedder`'s concurrency contract (serialize embed calls safely across drain-worker and search callers; state whether a search-time call preempts or queues behind an in-flight drain batch) — with a test exercising a search request concurrent with an in-flight drain batch.

### B2 — Text-preparation (M2.3.9) only works for 2 of the 5 embeddable content kinds; 2.2.4's SQL doesn't even fetch the field needed to fix it

M2.3.9 describes stripping `display_text()`'s decoration generically as `"[image 40KB] caption"` → `"caption"`. The actual per-kind formats (`bridge.rs:436-492`):

- Image/Video/Document: caption trails the `]` (`"[image {size}] {caption}"`, `"[doc: {filename} {size}] {caption}"`) — prefix-strip works.
- **Location** (`bridge.rs:473-475`): `"[location: {name} ({lat},{lon})]"` — the name is *inside* the brackets, coordinate junk trailing inside the same brackets, nothing after `]`. "Keep everything after the first `]`" → **empty string**, not the name.
- **Contact** (`bridge.rs:477`): `"[contact: {display_name}]"` — entire payload inside the brackets, nothing trails. Naive strip → empty.
- **Poll** (`bridge.rs:482-484`): `"[poll: {question} (pick {n}) — {opt1 | opt2}]"` — question inside the brackets, no leading `]` to chop to, `"(pick N) — "` noise before the options.

Per ADR 0016 / M2.2.1, Location name, Contact name, and Poll question+options are three of the five embeddable-text categories — not an edge case, but most of the non-Text content this milestone exists to make searchable. As specified, the drain worker sends empty strings (Location/Contact) or noise-contaminated text (Poll) to the sidecar — silently, since M2.3.9's Verify only tests the image example plus already-bare Text and truncation.

Compounding: **M2.2.4's `fetch_pending_embeddings` SELECT is only `(message_id, body_text)`** — no `content_kind`, which 2.3.9 needs to (a) pick the per-kind un-decoration rule and (b) know when to skip stripping entirely (a genuine `Text` starting with a literal `[` — e.g. "[URGENT] call me" — must never be bracket-stripped; "Text kind is already bare" implicitly requires kind-gating the SQL doesn't supply).

**Direction:** add `content_kind` to `fetch_pending_embeddings` (2.2.4) and thread it into 2.3.9; make text-prep branch on `content_kind` with a distinct, tested extraction rule per kind (consider having 2.3.9 reuse the per-variant logic `embeddable_text()` in 2.2.1 already writes, rather than re-deriving it via string surgery on the decorated label — note the drain worker only has `body_text`, not the original enum, so this may argue for persisting the bare embeddable text at write time). Expand 2.3.9's Verify to include Location/Contact/Poll before/after fixtures.

---

## 3. Non-blocking concerns / implementation-time cautions

1. **M2.3.8's circuit-breaker check should compose with the existing atomic enqueue closure, not sit beside it.** `enqueue_backfill_job` (storage.rs:1672-1764) already does an atomic `unchecked_transaction` closure checking active-job / cooldown / queue-depth / clamp (ADR 0035 B5 TOCTOU fix). M2.3.8 reads as a separate check bolted around it. Thread `active_model_id: Option<&str>` into `enqueue_backfill_job` and add the anti-join count as one more atomic step (a new `EnqueueOutcome` variant). Low severity (soft 100k ceiling), but be deliberate.

2. **M2.4.3's "internally-widened recall (~50-200), independent of the caller's `limit`"** can silently return fewer results than the M1 lexical path when the widened-recall constant is below the caller's `limit`. `api.rs`/`mcp.rs` clamp `limit` to `min(200)` (api.rs:932, :949) — pin recall width `>= min(caller_limit_max, 200)` so a `limit=200` request never gets truncated below pure lexical.

3. **Embedder subprocess lifecycle on shutdown is unaddressed.** No task mentions `kill_on_drop(true)` or explicit child termination when the `CancellationToken` fires — a long-running deployment could leak orphaned sidecar children across restarts. Cheap; worth a one-line task/verify.

4. **Drain-worker vector writes: batch discipline not explicit.** M2.3.4 says "`INSERT OR REPLACE ... per row`" without stating the 64 writes are one chunked transaction (ADR 0027's stated pattern). Harmless either way (set-difference re-derivation), but explicit-one-TX matches the ADR precedent and avoids 64 lock acquisitions/batch.

5. **Purge of the *active* model isn't called out as re-tripping the M2.3.8 breaker**, though the mechanism is identical to the accepted model-switch case. One-line note next to the existing caveat.

6. **F-D's in-memory attempt-tracker reset-on-restart** doesn't mention the crash-loop corner case: a daemon restarting every few seconds retries a rejected row 1-2×/restart indefinitely, never reaching terminal `failed`. Low likelihood, already substantially addressed ("a handful of wasted sidecar calls"). Flagged for completeness.

7. **`src/bin/fake-embedder.rs` (2.1.8) adds a second `[[bin]]`** — one-line confirmation that release packaging ships only the primary `whatsrust` binary (single-binary ethos).

---

## 4. Risks

| Risk | Severity | Likelihood | Note |
|---|---|---|---|
| B1 (missing query-embed + sidecar concurrency contract) | High | Certain to surface | Blocks M2.4 as specified; discovered the moment M2.4.3 impl begins |
| B2 (content-kind-blind text stripping) | Medium-High | Certain for Location/Contact, likely for Poll | Silent embedding-quality degradation, not a crash — could ship undetected (specified tests don't cover these kinds) |
| First-drain volume ~2× steady-state (M2.2.3) | Low | Certain | Already documented accepted one-time cost |
| Circuit-breaker TOCTOU if not composed into the atomic enqueue closure (§3.1) | Low | Occasional under concurrent enqueue | Soft threshold |
| `auto_vacuum` silent no-op on pre-existing DBs (M2.5.2) | Low (mitigated) | Realistic — the working tree carries pre-M2-era `.bak` files suggesting the DB predates `auto_vacuum=INCREMENTAL` | WARN-based honesty check is the right mitigation; gotcha confirmed via `storage.rs:668` + `:1479-1480` |
| Sidecar process orphaned across restarts (§3.3) | Low | Low-medium over long uptime | Cheap `kill_on_drop` fix, unaddressed |

---

## 5. Strengths

- Every §0 Finding (F-A–F-J) independently verified true against live code: `embeddings.message_id` TEXT PK (storage.rs:233-239), FTS `rowid`/`embeddings.message_id` dual-key issue (:271-276, :1385-1421), `embed_status` default `'pending'` + both write sites storing `display_text()`'s decorated label (`bridge.rs:2682`, `:4263`), no `embed_failures` table, drain-spawn precedent (`bridge.rs:2117-2153`, before reconnect at `:2162`), `BridgeConfig`/`main.rs` env plumbing (`:676/746`, `main.rs:229-335`), `mcp.rs` server-vs-client framing (`:13-46`, `:48-67`), missing `tokio` `"process"` feature. A plan that read the code, not one transcribing the design.
- The "review fold-in" table (B1 + 12) is well-reasoned; each item lands in a specific testable task. The raw-vs-anti-join circuit-breaker fix and the `bytes_reclaimed` honesty fix (correctly citing the real `auto_vacuum`/`incremental_vacuum` gotcha) are substantively correct.
- Deciding F-D (attempt tracking) in-memory to avoid any schema migration is a good, explicitly-justified scope reduction, internally consistent with `CURRENT_SCHEMA_VERSION` staying 8.
- M2.2.5 index correctly respects ADR 0031's non-versioned-index carve-out.
- Deferring pure-semantic brute-force (M2.4.6) is right, and bounds the blast radius of the F-C "tolerate the backlog" decision (garbage vectors from decorated labels only surface via FTS-gated recall, never an all-vectors scan).
- Sub-milestone sequencing (M2.1 → M2.6) has no dependency-ordering hazards found — transport before dependents (with a real subprocess test), classification before drain, drain before search, retention/purge/config after the core mechanism.
