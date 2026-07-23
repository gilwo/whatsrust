# Design Review: M2 Detailed Execution Plan (Semantic Search)

**Date:** 2026-07-23
**Artifact:** `docs/plans/2026-07-23-M2-detailed-plan.md` (M2 execution plan — semantic/embedding search)
**Prior reviews (of the underlying F1 *design*, not this plan):** `2026-06-18` (7 blocking), `2026-06-23-v2` (5 blocking / 5 major), `2026-06-24-v3` (final — implementation-ready, no blockers)
**Reviewer:** Cold design-reviewer (independent, no prior context, no prior reviews fed)
**Verdict:** Approve with changes (1 blocking, 12 non-blocking, several risks)

> This is the first review of the M2 *execution plan* specifically. The underlying design blueprint
> was reviewed three times (above) and converged to "implementation-ready." The reviewer inspected
> the live post-M1 source to verify the plan's §0 Findings and line-anchors.

---

# Cold Design Review — M2 Detailed Execution Plan (Semantic Search)

**Artifact reviewed:** `docs/plans/2026-07-23-M2-detailed-plan.md`
**Grounding checked against:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md`, `docs/plans/IMPLEMENTATION-ROADMAP.md`, `docs/plans/2026-07-02-M1-detailed-plan.md`, ADRs 0006/0008/0015/0016/0017/0018/0022/0024/0025/0027/0031, and the live source: `src/storage.rs`, `src/bridge.rs`, `src/api.rs`, `src/mcp.rs`, `Cargo.toml`.

---

## 1. Verdict

**Approve with changes.** This is a well-constructed, unusually well-grounded execution plan — every `file:line` anchor checked in §0 Findings (F-A through F-J) matched the live code exactly, which is rare and a strong signal of rigor. But there is one concrete logic bug in the plan's own task text that, if implemented as literally written, defeats a stated M2 exit criterion (the pathological-pending circuit breaker), plus several real gaps in failure-mode coverage and sequencing that should be resolved before M2.3/M2.5 coding starts. None require redesign — all are fixable within the current phase structure.

---

## 2. Blocking issues

### B1 — M2.3.8's circuit-breaker metric will (not "might") trip permanently for any account with a substantial message history, defeating the stated exit criterion

- **M2.3.4** decides: `messages.embed_status` **stays `'pending'` forever** once a row is classified embeddable — "the `embeddings` table itself is the source of truth for 'done' ... no `'done'` value needed/used." Sound reading of ADR 0017 on its own.
- **M2.3.8** then specifies the pathological-pending circuit breaker as: *"periodically count `messages` rows with `embed_status='pending'`; if `>100_000` ... reject new backfill job enqueues."*

Read literally (and this is a task list handed to an implementing subagent, so it will be read literally), this counts **all classified-embeddable messages ever**, not "messages lacking a vector for the active model." Per M2.3.4, that count **never decreases** as the drain worker catches up — it only grows. So once an account's lifetime embeddable-message count crosses 100k (very plausible: exactly the scenario M1's `all`-target backfill exists to produce, across years of one user's chats), the circuit breaker becomes a **permanent, always-on block on new backfill enqueues**, even when the drain worker is fully caught up with zero outstanding work for the active model. That directly contradicts the M2 exit criterion: *"the only coupling to backfill is the >100k pathological-pending circuit breaker ... never throttles the backfill worker itself."*

This also sits in tension with F-C's justification for the "tolerate the backlog" decision ("backlog is small ... not internet-scale") — the plan's own R3 circuit breaker anticipates exactly the six-figure scale F-C waves away.

**Direction:** M2.3.8's count must be the **active-model anti-join set-difference** — the same shape as `fetch_pending_embeddings` (M2.2.4) but as a `COUNT(*)`:
```sql
SELECT COUNT(*) FROM messages m LEFT JOIN embeddings e
 ON m.message_id = e.message_id AND e.model_id = ?1
 WHERE e.message_id IS NULL AND m.embed_status = 'pending'
```
not a raw `embed_status='pending'` count. Update the M2.3.8 task text explicitly and add a test asserting the breaker does **not** trip when the active model is fully drained regardless of total historical message count, and **does** trip on genuine active-model backlog. Also note that switching models on a large history (M2.5.1) will legitimately re-trip this breaker until the new model's re-drain completes — expected once the metric is fixed, not a bug.

---

## 3. Non-blocking concerns / implementation-time cautions

1. **No index on `messages.embed_status`; both the corrected circuit-breaker count and the M2.2.4 drain query will full-scan a growing, indefinitely-retained table.** `storage.rs:292-298` shows the only indexes are `idx_messages_chat_ts`, `idx_messages_msg_id`, `idx_outbound_*` — none on `embed_status`. `fetch_pending_embeddings`'s `ORDER BY m.id LIMIT ?2` (M2.2.4) and the fixed circuit-breaker count (B1) must scan from the start of the table. Given ADR 0012's indefinite retention, this grows unboundedly with account age. ADR 0031 sanctions idempotent non-versioned `CREATE INDEX IF NOT EXISTS` — add `idx_messages_embed_status` (or a composite covering the anti-join) and require `EXPLAIN QUERY PLAN` checks for both queries, mirroring M1.3's FTS5 discipline (`test_fts_explain_query_plan`). Neither M2.2.4 nor M2.3.8's Verify column asks for this.

2. **M2.4.3 doesn't specify what happens when only *some* FTS-recalled candidates have a vector for the active model.** Covers the all-or-nothing case (M2.4.4) but not the common partial case during drain catch-up — a recently-arrived, still-`pending` message that FTS recalled could silently vanish from results purely because the drain worker hasn't reached it. **Direction:** specify semantic rerank as additive-only — candidates lacking a vector keep their FTS-rank position (appended after the reranked vectorized subset, or interleaved with a defined tie-break) so the semantic path is never *worse* than pure lexical for any FTS-recalled candidate.

3. **Real `StdioEmbedder` (child-process JSON-RPC) verification is deferred to M2.6, the last sub-milestone.** M2.1.4's only sub-process validation happens four sub-milestones after M2.2–M2.5 are built on it (tested only against the in-process fake). A framing/buffering/lifecycle bug could surface at M2.6 forcing rework. **Direction:** pull the minimal fake-sidecar binary (2.6.3) + one real-subprocess round-trip into M2.1's exit gate; leave only the "misbehave"-mode test in M2.6.

4. **Startup-time failure of `StdioEmbedder` construction is untested and unspecified.** M2.3.3 covers "no CMD → not spawned" and "CMD → spawned," not "CMD set but broken" (typo'd path, binary that exits immediately, `model_info()` that never returns). Per ADR 0022 (unguarded/benign) and the "no errors, no blocking" exit criterion, a *misconfigured* embedder must not block daemon startup or panic the spawn task. Add a task/test: construction failure → WARN, treat as absent/unhealthy, daemon starts (e.g. point CMD at `/bin/false` or a nonexistent path).

5. **M2.3.7's "cumulative time in loading" contradicts the "continuous" wording in the ADRs it cites.** ADR 0015 and ADR 0024 both say *">60s continuous loading"* (resets on any non-loading observation); M2.3.7 says *"cumulative ... across polls."* A sidecar that briefly reports `loading` on every restart (5s each, many times) would eventually cross a cumulative 60s and get permanently flagged `error` despite each episode being harmless. Align with "continuous," or justify the deviation.

6. **Interaction between loading-timeout (M2.3.7) and backoff/notify-only (M2.3.5) is unspecified.** Does a loading-timeout "treat as error" count toward "N consecutive backoff cycles → Notify-only"? If not, a sidecar stuck in `loading` polls at full cadence forever. Fold loading-timeout events into the same backoff/notify-only accounting.

7. **`PRAGMA incremental_vacuum` (M2.5.2) is a silent no-op unless `auto_vacuum=INCREMENTAL` was set before any tables existed** — a SQLite constraint, not a whatsrust bug, but it threatens the honesty of the purge API's `{model_id, rows_deleted, bytes_reclaimed}` contract. `prune_old_data` already carries the caveat (`storage.rs:1479`: "no-op if auto_vacuum != INCREMENTAL") used best-effort/silently. M2.5.2 is the first feature making a real accounting *promise*. The fresh-temp-DB test always has `auto_vacuum=INCREMENTAL` so won't catch a real pre-existing DB. **Direction:** (a) compute `bytes_reclaimed` via actual before/after file-size measurement; (b) add a one-time `PRAGMA auto_vacuum` check/log (should read `2`) so a stuck DB warns instead of silently reclaiming 0.

8. **F-C's "tolerate the backlog" means the *entire* pre-M2 history (not just NL rows) is sent to the sidecar on first drain.** ADR 0016 estimated ~40-60% load reduction from skipping non-NL; ADR 0015's "2-8× headroom" is implicitly built on that. Since every M1-era row is `pending`, the one-time backlog drain processes ~2× the sized-for volume. One-time cost, not a correctness bug — state it as an operational expectation (initial catch-up slower than steady-state math).

9. **M2.2.4's parenthetical "consider stripping the bracket in the drain worker's text preparation step, M2.3" has no corresponding task in M2.3 (2.3.1–2.3.8).** Loose thread the plan is otherwise careful to close (cf. M2.4.6's explicit deferral). Either add an explicit M2.3 task + Verify for bracket-stripping, or record it as an explicit decision-to-defer.

10. **`WHATSRUST_EMBEDDER_DRAIN_INTERVAL_SECS` has no stated default** (M2.3.4 and M2.6.1 reference it; unlike batch=64, backoff=60s, loading=60s). Pin it down before `.env.example`.

11. **No truncation logic for `max_input_tokens`.** ADR 0024's `model_info` includes optional `max_input_tokens` "so bridge truncates," but no M2.1–M2.3 task implements/tests truncating `embeddable_text()` before sending. Minor given typical message lengths; add a task if `model_info` reports the field.

12. **ADR 0015's top-level text ("No embedder configured → worker IDLES entirely") is stale vs its own 2026-06-24 hardening ("DON'T spawn drain worker") that M2.3.3 correctly follows.** Not a plan defect — patch ADR 0015's body to match, as the plan already patches ADR 0008/0009 elsewhere.

---

## 4. Risks

| Risk | Severity | Likelihood | Note |
|---|---|---|---|
| Circuit-breaker metric bug (B1) permanently blocks new backfill past ~100k lifetime embeddable messages | High | High (near-certain over time for any long-lived multi-chat account) | Must fix before M2.3 coding |
| Missing `embed_status` index → drain-query + circuit-breaker-count cost grows unboundedly with (indefinitely-retained) history | Medium | High over account lifetime | Cheap, additive, ADR-0031-sanctioned |
| Partial vector coverage during rerank silently drops recently-arrived, lexically-relevant candidates | Medium | High (whenever drain lag exists) | Needs explicit merge/fallback rule in M2.4.3 |
| Real `StdioEmbedder` transport bugs surface only at M2.6, after 4 sub-milestones built on it | Medium | Medium | Schedule/rework risk |
| `auto_vacuum`/`incremental_vacuum` no-op on non-originally-incremental DB → purge under-reports bytes | Medium | Unknown/Low-Medium | Verify live DB `PRAGMA auto_vacuum` at impl time |
| Sidecar embeds decorated non-NL labels from backlog rather than rejecting → cap-3 backstop doesn't bound the noise | Low | Medium | Mitigated: rerank only touches FTS-recalled candidates, can't surface new irrelevant hits |
| Loading-timeout "cumulative" vs ADR "continuous" → over-aggressive fallback for oscillating sidecar | Low-Medium | Low | Cheap to align now |
| Startup construction failure of a misconfigured (not absent) embedder unhandled/untested | Medium | Medium (any CMD typo) | Must not block daemon startup (ADR 0022) |

---

## 5. Strengths

- **§0 Findings are accurate.** Every load-bearing code claim (F-A–F-J) independently verified against live source and matched, often to the exact line range: `embeddings` schema (`storage.rs:233-239`), FTS5 DDL + trigger trio (`:271-290`), `insert_message` (`:1258-1288`), `search_inbound`'s two branches (`:1335-1426`, FTS branch `1385-1421`), `InboundRow` (`:927-936`), `handle_search` (`api.rs:943-957`), `display_text()` decorated-label behavior (`bridge.rs:433-492`), the two write sites (`:2682`, `:4263`), drain-spawn precedent (`:2030-2153`, reconnect loop `2162`), `BridgeConfig`/`main.rs` env plumbing (`:676/746`, `main.rs:229-335`), missing tokio `process` feature (`Cargo.toml:37`), `mcp.rs` server-only framing (`:13-67`), anti-join probe (`storage.rs:563-568`). A genuinely well-grounded plan.
- **No-schema-migration scoping (F-D) is the right call** — reuses ADR 0017's sanctioned "in-memory" option, keeps `CURRENT_SCHEMA_VERSION` at 8, honestly framed as reversible/lossless.
- **Fallback discipline (M2.4.4, F-H) well specified for the all-or-nothing case** and protects the M1.3 shape/behavior.
- **Classifier per-variant mapping (M2.2.1) is complete** — all 15 `InboundContent` variants against ADR 0016 (7 embeddable, 8 skipped), with `Edit` correctly flagged as a deliberate exception.
- **Honest about its own open decisions** (F-C, F-G, M2.4.2, M2.4.6, M2.6.5) — listed explicitly, not buried.
- **Sequencing (M2.1→…→M2.6) directionally sound** and isolates the highest-risk logic (drain resilience, search fallback) as the areas needing most scrutiny.

## Assumptions / open questions

- Assumed the live `whatsapp.db` is plausible to exceed 100k lifetime embeddable messages given multi-year multi-chat Hebrew/Arabic usage + M1 `all`-target backfill; could not verify actual row count — B1's likelihood is "near-certain over time," and the bug is real regardless of current DB size.
- Could not determine whether the live DB's `auto_vacuum` was ever set on a non-empty database — verify `PRAGMA auto_vacuum;` before relying on `incremental_vacuum` byte-accounting (§3.7/§4).
- Did not evaluate whether a real embedding model rejects decorated-label backlog text at the rate M2.3.6's cap-3 assumes — depends on the eventual real sidecar; flagged low-severity, mitigated by FTS5-recall-first rerank.
