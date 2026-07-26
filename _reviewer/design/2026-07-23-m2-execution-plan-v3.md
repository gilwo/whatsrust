# Design Review: M2 Detailed Execution Plan (Semantic Search) — v3 (follow-up)

**Date:** 2026-07-23
**Artifact:** `docs/plans/2026-07-23-M2-detailed-plan.md` (revised at commit `edf5530` — folded in the v2 cold review)
**Prior reviews (of this plan):** `2026-07-23-m2-execution-plan.md` (v1 — approve w/ changes, 1+12), `…-v2.md` (v2 — needs rework, 2+7)
**Prior reviews (of the underlying F1 *design*):** `2026-06-18`, `2026-06-23-v2`, `2026-06-24-v3` (final — implementation-ready)
**Reviewer:** Cold design-reviewer (independent, no prior context, no prior reviews fed)
**Verdict:** Approve with changes — 3 blocking, 7 non-blocking

> Coordinator note: the reviewer independently re-verified that the v1/v2 fixes are actually present in the
> current task text (not just referenced in fold-in tables) and that all ten §0 Findings still check out
> against live code. The three blockers below are NEW — deeper runtime-interaction gaps. Coordinator
> independently confirmed the load-bearing logic (batch-only protocol → livelock; None-model bind; single-
> construction wiring).

---

## Verdict: Approve with changes

Rigorous, code-grounded plan that has already survived two cold reviews with substantive fixes landed (independently re-verified that the B1/v2-B1/v2-B2 fixes are present in the current M2.3.8/M2.4.3/M2.1.9/M2.3.9 task text, not merely referenced in the fold-in tables). All ten §0 Findings check out exactly against the as-built M1 code, including line numbers. Three concrete, addressable gaps should be closed before implementation — none require restructuring the plan, but each is a real correctness/operability hole a competent implementer could fall into.

---

## Blocking issues

**B1 — Per-row content-rejection is unreachable at the production batch size; a single bad message can permanently stall the entire drain worker (M2.3.4/M2.3.6, interacting with ADR 0015/0024).**

ADR 0024's wire protocol (`docs/adr/0024-...:29-32`) is batch-only: `embed {texts[]} → {vectors[][], model_id, dim}` — one response for the whole call, no per-item error channel. Task 2.3.6 concedes this ("If the sidecar protocol has no native per-item rejection signal, treat whole-batch validation failure as a transport failure (2.3.5)... document this limitation rather than half-implementing per-item isolation"). But 2.3.6's Verify ("fake `Embedder` rejects one specific text 3 times running **solo-batches**") admits content-rejection accounting only fires at batch size 1 — while the production loop (2.3.4) always fetches `batch_size` (default 64) via `fetch_pending_embeddings` (`ORDER BY m.id LIMIT ?2`). If any one of those 64 rows errors the whole call (a caption exceeding an *unadvertised* `max_input_tokens`, an encoding edge case, an empty post-truncation string), 2.3.5 classifies it as a transport failure: rows stay `pending`, the attempt map is never touched, the loop backs off and retries — and because the fetch is deterministically oldest-first, the *same* poisoned 64-row batch is re-selected every cycle. No later row is ever reached. This is a livelock, not a transient outage: it defeats the M2.3 exit criterion and, via 2.3.8, eventually blocks *all* new backfill enqueues once the stalled backlog crosses 100k. Not rare for a years-long personal history.

**Direction:** add a bisection-on-repeated-whole-batch-failure task to M2.3 — after K consecutive whole-batch failures of the *same* message-id set, retry with the batch halved (pure whatsrust-side retry, no ADR 0024 change), converging to solo-batches so 2.3.6's per-row cap-3 can engage. Own Verify (e.g. "a batch containing one poisoned text still drains the other 63 within N cycles, and the poisoned row reaches `failed` within 3 solo attempts").

**B2 — The circuit-breaker query (M2.3.8) must explicitly no-op when there is no active model, or it silently regresses the exact bug it was designed to fix.**

2.3.8's anti-join binds `?1 = active_model_id`. The task never says what happens when no embedder is configured / `model_info()` never observed (`active_model_id` is `None`). If the impl supplies a placeholder/empty string for `?1`, the anti-join degenerates to "count everything with no vector for a model that doesn't exist" — i.e. it collapses back to counting raw lifetime-pending messages, precisely the v1-B1 bug, reached via a different path. Per the hard constraint ("semantic search is strictly additive... with no/unhealthy sidecar... no blocking"), this must not affect backfill enqueue when M2 is off. **Direction:** add an explicit branch — "if `active_model_id` is `None`, skip the breaker check entirely; enqueue proceeds exactly as in M1" — with a Verify case (e.g. "no `WHATSRUST_EMBEDDER_CMD` + 150k lifetime messages → enqueue never checks/trips the breaker").

**B3 — No task owns constructing the `StdioEmbedder` as a single shared instance; without one, M2.1.9's concurrency contract is moot and the sidecar could be spawned twice.**

M2.4.3 says the embedder "is held by the bridge / drain worker, 2.3.2-3" and M2.1.9 requires serializing concurrent calls from the drain worker *and* live search over "the SAME child." But no task explicitly says: construct one `Arc<dyn Embedder>` in `WhatsAppBridge::start()` (mirroring how `store`/`event_tx` are constructed once at `bridge.rs:874-896` and shared into both the reconnect loop and independent worker tasks), store it as a bridge field, and pass the *same* instance to both the drain-worker spawn (2.3.3) and the `search()` accessor (2.4.3). Without this, an implementer following M2.3 and M2.4 independently could construct two separate `StdioEmbedder`s, each spawning its own child — defeating 2.1.9's serialization and doubling sidecar/model memory. **Direction:** add one explicit task (e.g. M2.3.2 or a new M2.1.10) constructing the embedder once at `start()` under the same non-fatal-construction contract as 2.1.4, with a Verify asserting single-construction.

---

## Non-blocking concerns / implementation-time cautions

1. **Shutdown race for the sidecar child.** `main.rs:1215-1227` calls `bridge.stop()` → `cancel.cancel()`, waits ≤5s via `wait_stopped()` (which only watches `BridgeState::Stopped`, a connection-state signal, not "all aux tasks finished"), then `std::process::exit(0)` unconditionally (to avoid Rust drop glue hanging on the blocking stdin reader). `std::process::exit` skips destructors, so `kill_on_drop` (2.1.9) only protects the sidecar if the owning task's cancellation branch has already killed the child before that 5s window — nothing guarantees this ordering. Recommend an explicit testable teardown (integration test: start fake-sidecar, trigger shutdown, assert child reaped within the graceful window).

2. **M2.4.7's mcp.rs claim is inaccurate.** The task says `api.rs` *and* "the analogous `mcp.rs` search proxy" both get "a one-line call-site change." `mcp.rs`'s `whatsrust_search` (`mcp.rs:287-291`) is a pure blocking-HTTP proxy to `/api/search` — it never calls `Store`/`Bridge`. Once `api.rs::handle_search` switches to `bridge.search(...)`, `mcp.rs` needs **zero** change — it inherits over HTTP. Fix the doc so an implementer doesn't hunt for a nonexistent mcp.rs call site.

3. **The breaker `COUNT(*)` (M2.3.8) is unbounded**, unlike M2.2.4's `LIMIT`-bounded fetch. It runs in the short single-mutex `enqueue_backfill_job` closure (ADR 0027 wants it ms-short); an exact count scans up to ~100k rows on every enqueue. Bound it: `SELECT COUNT(*) FROM (SELECT 1 FROM ... LIMIT 100001)` so the check costs at most `threshold+1` scans.

4. **In-memory attempt map (F-D/2.3.1) has no eviction policy** for entries reaching terminal `failed`. Likely negligible; a one-line bound/note closes "does it grow forever."

5. **ADR record-keeping is inconsistent.** F-A patches ADR 0008 and M2.3.3 patches ADR 0015 — good. But F-D (in-memory vs table — an item ADR 0017 explicitly left open), the Option-C text-prep decision, and the "tolerate backlog" decision are recorded only in this plan with "→ needs ADR 0038+" hedges. Per CLAUDE.md ("add an ADR"), and since ADR 0017 posed exactly this question, a short dated correction to ADR 0017 (like the F-A/ADR-0008 patch) is cheaper now than reconstructing later.

6. **The fake-embedder `[[bin]]` (2.1.8) has no enforced release exclusion.** No CI/release workflow exists yet (`.github/workflows` absent). Add a `required-features` gate or explicit `--bin whatsrust` note when packaging is set up, so the single-binary claim has a mechanical backstop.

7. **No bound on a single stdout line read from the sidecar** (2.1.4's "buffered async line reader"). Low priority given ADR 0024's trusted-sidecar posture, but a misbehaving sidecar emitting an unbroken oversized line grows the buffer unbounded. A cheap max-line-length guard (→ transport failure) is inexpensive insurance.

---

## Risks

- **B1 (poison-pill livelock)** — High severity (defeats the milestone's core resilience goal); High likelihood over a multi-year history. Blocking.
- **First-activation breaker trip for a large pre-existing backlog** — enabling the embedder on an account with >100k backfilled messages immediately trips M2.3.8 (all unvectored for the new model on day one) and blocks new enqueues until drain. Plan anticipates/accepts/tests this (2.3.8 note + v2-#5 + test (b)). Low severity (self-resolving, tested), Medium-High likelihood for power users. Visibility only.
- **Read latency under concurrent drain + search** — inherited ADR 0027 tension, correctly deferred behind a measured trigger (M2.4.3 R4). Low as scoped.
- **Sidecar child orphan on shutdown** (Non-blocking #1) — Low-Medium (resource leak, not data loss), Low-Medium likelihood depending on scheduler timing in the 5s window.

---

## Strengths

- All ten §0 Findings independently re-verified against live code (`storage.rs:204-294, 540-568, 927-936, 1258-1426, 1672-1764`; `bridge.rs:420-492, 676, 746, 820-921, 1275-1291, 2020-2153, 2660-2689, 4240-4288`; `mcp.rs:1-67, 287-291, 348-395`; `main.rs:220-365, 1190-1227`; `Cargo.toml`) — every citation exact, down to line numbers.
- The B1 fix (anti-join vs raw `pending` count) is sound and well-tested (2.3.8's three-part test proves the count is not raw pending).
- Reusing the anti-join shape already smoke-tested in `validate_migration_post_commit` (F-J) rather than inventing `NOT EXISTS` is good low-risk continuity.
- Composing the breaker into the existing atomic `enqueue_backfill_job` transaction (2.3.8, v2-#1) is correct for the single-connection/single-mutex model (ADR 0027).
- Consistent additive-only, shape-preserving search discipline (F-H, M2.4.3, M2.4.7).
- Honest tradeoff analysis at F-C/F-D — real alternatives, chosen one, stated downside.
- Good scope discipline: defers pure-semantic brute-force (2.4.6), MAJOR.MINOR tiers, multi-active-model search.
- `bytes_reclaimed` honesty check (M2.5.2) is a genuinely good catch, verified against `storage.rs:1479-1480` and `:668`.
