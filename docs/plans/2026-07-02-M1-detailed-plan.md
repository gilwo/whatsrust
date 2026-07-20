# M1 — Historical Fetch + Lexical Search — Detailed Phase/Task Plan

**Date:** 2026-07-02
**Status:** Planned (written at M1 start, per IMPLEMENTATION-ROADMAP gate policy)
**Milestone:** M1 of F1. Ships independently — **no sidecar, no embeddings.**
**Design (what/how):** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md`
**Why (ADRs):** 0003/0009/0010/0011/0012/0013/0014/0019/0020/0021/0022/0023/0026/0027/0028–0036
**Reviews folded in:** `_reviewer/design/2026-06-24-...-v3.md` (I1–I7, R1–R6)

> Written after reading the live code (`src/storage.rs`, `src/bridge.rs`, `src/api.rs`). Two
> real wrinkles surfaced that the design sketch glosses — see **§0 Findings**. They are the
> first things to settle at M1.1.

---

## 0. Findings from the current code (settle these first)

### F-A — SCHEMA-first vs rename-in-place collision  ⚠️ decision needed
`Store::new` (`storage.rs:291`) runs `execute_batch(SCHEMA)` — a blob of unconditional
`CREATE TABLE IF NOT EXISTS …` — **before** `run_schema_migrations` (`:295`). The design
(ADR 0009) says migrate `inbound_messages → messages` via `ALTER TABLE … RENAME`. These
conflict two ways:
- If `SCHEMA` still declares `inbound_messages`, a migrated DB gets an **empty
  `inbound_messages` re-created** on every open.
- If `SCHEMA` declares the v8 `messages` table, the migration's `RENAME inbound_messages →
  messages` **fails** because `messages` already exists (created by SCHEMA moments earlier).

**Recommended resolution (decide at M1.1):** make `SCHEMA` declare the **v8 target** (`messages`
+ siblings + FTS5 + triggers, all `IF NOT EXISTS`), and make the v7→v8 migration a
**copy-then-drop**, not a rename: create `messages` (SCHEMA already did), `INSERT INTO messages
(...) SELECT ... FROM inbound_messages`, `DROP TABLE inbound_messages`. Guard on
`inbound_messages` existing. This respects the SCHEMA-first ordering the codebase already
depends on and keeps fresh-DB and upgrade paths converging on one shape. (Alternative: reorder
`Store::new` to gate SCHEMA by version — larger blast radius, not recommended.)
→ This also means updating ADR 0009's "rename-in-place" wording to "copy-then-drop under
SCHEMA-first," or noting the deviation.

### F-B — ADR 0019 FTS5 DDL had two bugs (patched 2026-07-02)
1. **rowid:** DDL used `content_rowid='message_id'` (TEXT) — must be the INTEGER rowid `id`.
   The design doc (`:191`) was already correct. Fixed.
2. **triggers:** DELETE/UPDATE used plain `DELETE FROM messages_fts` / `UPDATE messages_fts`.
   For **external-content** FTS5 this silently **corrupts the index** — the index doesn't
   store the text, so it needs the *old* values to reverse entries. Must use the canonical
   `'delete'` command: `INSERT INTO messages_fts(messages_fts, rowid, body_text)
   VALUES('delete', old.id, old.body_text)`, then re-INSERT on UPDATE. Fixed in ADR 0019.

**M1.1.2 MUST use the corrected ADR 0019 trigger trio verbatim** (INSERT plain; UPDATE =
`'delete'` old + INSERT new; DELETE = `'delete'` old).

### F-C — the existing migration ceremony is minimal; M1.1 adds a lot
Today `run_schema_migrations` is a single `BEGIN IMMEDIATE` … incremental `if from_version < N`
chain … `user_version` bump last … `ROLLBACK` on error (`storage.rs:803–974`). There is **no**
pre-migration backup, **no** WAL-checkpoint, **no** validation, **no** circuit-breaker pin,
**no** `--rollback`/`--migrate` flags. ADR 0028–0032 require all of it for v7→v8. **This staged
migration mode is the single largest, riskiest task in M1.1** — plan it as its own sub-effort,
not a footnote to the DDL.

### F-D — access points (verified call sites)
Renaming the table + removing age-prune touches exactly these:
- `storage.rs`: `insert_inbound` (`:551`), `search_inbound` (`:579`), `delete_inbound_chat`
  (`:625`), `delete_inbound_message` (`:640`), `prune_old_data` (`:659`), `InboundRow` (`:256`),
  `PruneStats` (`:268`), `SCHEMA` const (`:203`), the v5→v6 migration block (`:897`).
- `bridge.rs`: prune caller (`:1980`, passes `30*86400` inbound retention — **the I2 bug**),
  `insert_inbound` caller (`:2519`), `delete_inbound_chat` (`:2821`), `delete_inbound_message`
  (`:2834`), `prune_interval_secs` config (`:709/:736`).
- `api.rs`: `search_inbound` callers (`:890`, `:906`).

---

## M1 exit criteria (from the roadmap — the definition of done)

- Trigger `all` / `since:T` / `count:N` backfill for a chat/group via API/MCP/CLI + watch SSE progress.
- Backfilled + live messages unified, FTS5-searchable (incl. Hebrew/Arabic), retained indefinitely (no age-pruning).
- Migration v7→v8 is staged, backed-up, validated, rollback-able (`--rollback`/`--migrate`), circuit-breaker proven.
- Cancel + graceful-shutdown obey the 3-level abort model (resume-on-restart for shutdown; terminal for cancel).
- Tests per ADR 0025 (history-source fake, storage temp-DB) green; manual E2E checklist passes.
- Semantic layer **absent by design**: `embed_status` accrues `pending`, nothing consumes it.

---

## M1.1 — Storage + migration  [ADR 0009/0019/0027/0028–0032/0036]

**Goal:** the v8 schema exists, existing history migrates safely, FTS5 indexes it, age-pruning
is gone, and all access points compile against the new shape.

| # | Task | Verify |
|---|---|---|
| 1.1.1 | Settle **F-A** (copy-then-drop under SCHEMA-first) + **F-B** (rowid=`id`); patch ADR 0019 and note ADR 0009 deviation. | ADRs updated; approach written into this plan. |
| 1.1.2 | Extend `SCHEMA` to the v8 target: `messages` (adds `from_me`, `source`, `embed_status`), `media_refs`, `embeddings`, `backfill_cursor`, `backfill_jobs`, `metadata`, `messages_fts` + INSERT/UPDATE/DELETE trigger trio. All `IF NOT EXISTS`. | Fresh DB (`user_version=0`) opens → all tables + triggers present; `SELECT 1 FROM messages_fts LIMIT 0` ok. |
| 1.1.3 | Bump `CURRENT_SCHEMA_VERSION` 7→8. Add the v7→v8 migration block: copy `inbound_messages`→`messages` (guarded on existence), backfill `body_text` into FTS via `rebuild`, `DROP inbound_messages`. | Temp-DB seeded at v7 with N inbound rows → after open, `messages` has N rows, FTS returns them, `inbound_messages` gone, `user_version=8`. |
| 1.1.4 | **Staged migration mode (ADR 0028):** on `version<CURRENT`: `wal_checkpoint(TRUNCATE)` → backup `whatsapp.db.pre-migration-v<from>-<ts>.bak` via Backup API as `.bak.tmp`+atomic-rename (fail-closed) → migrate in TX (`user_version` last) → validate → persist `schema_validated_version` in `metadata` only after validation → seed watchdog baseline as final step → start. | Backup file appears before migration; kill-after-backup leaves DB at v7 + valid backup; success path sets `schema_validated_version=8`. |
| 1.1.5 | **Migration validation (ADR 0029):** V1 structural (tables/columns/indexes present) + V3 smoke probes (FTS5 trigger sync, set-difference drain query parses, embeddings BLOB roundtrip). Skip full `integrity_check` as gate. Startup re-validates whenever `schema_validated_version != CURRENT`. | Deliberately corrupt a trigger → validation fails → halt with actionable message, DB untouched. |
| 1.1.6 | **Circuit-breaker (ADR 0030):** pin file `whatsapp.db.migration-pin` (atomic temp+rename) prevents crash-loop; startup validates pin vs DB consistency; `--rollback` (restore .bak + delete -wal/-shm + update pin + EXIT) and `--migrate` (clear pin + retry) flags in `main.rs`. | Simulated migration failure writes pin; restart halts with instruction; `--rollback` restores; `--migrate` retries. |
| 1.1.7 | **FTS5 probe (ADR 0032):** cheap startup probe `SELECT 1 FROM messages_fts LIMIT 0` after version check + one-time probe at v7→v8 boundary before FTS5 DDL. Absent → actionable error ("keep default `bundled` rusqlite feature"), leave DB at v7. | Probe present; (manual) a no-FTS5 build errors cleanly instead of corrupting. |
| 1.1.8 | **⚠️ I2 — remove age-prune.** In `prune_old_data` delete the `DELETE FROM inbound_messages WHERE created_at < ?1` block (`storage.rs:673–679`); keep outbound-queue cleanup. Drop `inbound_retention_secs` param + `PruneStats.inbound_deleted`; update caller `bridge.rs:1980`. | Grep shows no age-DELETE on messages; prune test asserts messages survive a prune tick; outbound cleanup still works. |
| 1.1.9 | Update access points (F-D) to the `messages` table: `insert_inbound` writes `from_me`/`source='live'`/`embed_status`; `search_inbound`, both deletes, `InboundRow`. (FTS5 query itself is **M1.3** — keep LIKE here so the build stays green.) | `cargo test` green; live insert/search/delete still work against `messages`. |
| 1.1.10 | **Watchdog baseline seed (I4/B2):** `INSERT INTO metadata('watchdog_last_alerted_size', <post-migration bytes>)` as the deterministic final migration step, before daemon accepts work. | Metadata row present post-migration; first watchdog tick does not false-alert. |
| 1.1.11 | Storage tests (ADR 0025, real temp DB): migration copy-then-drop, FTS5 trigger sync (insert/update/delete), FTS5 rebuild repair, prune-keeps-messages, embeddings BLOB roundtrip, `metadata` seed-on-absence. | New tests pass; total suite still green. |

**M1.1 exit:** fresh DB and a v7 upgrade both land on v8; history is FTS5-searchable; age-pruning
is gone; migration is backed-up + validated + rollback-able; suite green.

> ✅ **M1.1 DONE (2026-07-03).** All 11 tasks landed across two waves. 168 tests green (82 lib
> + 86 bin). Real-data dry-run on an actual Phase-0 v7 DB: 8 rows migrated, `inbound_messages`
> dropped, Hebrew + Arabic FTS5 MATCH verified, `.bak` confirmed pristine-v7 (reorder correct),
> `schema_validated_version=8`, watchdog baseline seeded. ADRs 0009/0019 corrected (copy-then-drop;
> FTS5 rowid=`id` + `'delete'`-command triggers). `search_inbound` still on `LIKE` (FTS MATCH is M1.3).

---

## M1.2 — Fetch worker + safety/config  [ADR 0003/0010/0020/0021/0022/0023/0026/0033/0035]

**Goal:** trigger per-chat backfill; a single paced worker paginates backward, persists into the
unified table via the same extraction path as live, with daemon-side safety built in (not bolted on).

| # | Task | Verify |
|---|---|---|
| 1.2.1 | **`history-source` trait** (test seam, ADR 0025): worker depends on the trait, not `Client`. Real impl wraps `Client.fetch_message_history`; fake injects canned `WebMessageInfo` batches + simulated more-remain/timeout. | Trait + real + fake compile; fake used in 1.2.x tests. |
| 1.2.2 | **WebMessageInfo adapter (ADR 0014):** map history `WebMessageInfo` → the inputs `extract_content_inner` already takes; backfilled rows parse identically to live (`source='backfill'`). | Canned history batch → rows land in `messages` with correct kind/body/from_me. |
| 1.2.3 | **Backfill-job queue (ADR 0010/0033):** `backfill_jobs` claim/complete mirroring `outbound_queue` claim pattern; contained-C target model (`since`/`all`/`count`); atomic enqueue-time validation (**I7/B5**) — one closure BEGIN→check cooldown+one-active→INSERT-or-reject→COMMIT (mirror `claim_next_job`). | Enqueue returns job_id; second concurrent enqueue for same chat → `already_active`; cooldown enforced. |
| 1.2.4 | **Single-worker FIFO pagination (ADR 0026):** sequential loop, anchor-based backward pagination, cursor persist + response in ONE TX (BATCH atomic). 3-level abort (batch atomic / job resumable / task cooperative); interruptible inter-batch sleep; **CASE-guarded** terminal status write (**I6** — do NOT copy outbound's unconditional write). | Fake-driven test: paginates, advances cursor, cancel at batch boundary → `cancelled` + resumable; shutdown → `queued`/resumable. |
| 1.2.5 | **Stuck-anchor guard (R2):** new-oldest-anchor == request anchor for K=2 consecutive batches → abort `failed` ("anchor not advancing"). **Exhausted/more_remain contract (I5):** verify phone signals "no more" explicitly, else empty-response heuristic. | Fake returning same anchor twice → job `failed`; fake signaling exhausted → cursor `exhausted`, next trigger no-ops. |
| 1.2.6 | **Dedicated backfill pacer (ADR 0020):** SEPARATE from outbound `SendPacer` (must not consume send budget). burst=1, ~4s/batch ± 40% jitter; occasional long pauses; response-timeout → backoff → pause (resumable). | Pacing test asserts min inter-batch spacing; timeout path pauses not fails. |
| 1.2.7 | **Daemon-side safety (ADR 0021/0022):** pacer + global concurrency cap + per-chat cooldown + `max_messages` clamp + **global queue-depth limit (R5, ~3–5; excess → 429)** live below the MCP layer. Fail-closed config (ADR 0022): ban-critical knobs validated at startup → refuse to start unless scoped `WHATSRUST_DANGEROUSLY_ALLOW_*`. | Over-cap enqueue → structured 429; bad ban-critical knob → refuses to start naming the override. |
| 1.2.8 | **Config (ADR 0023):** add `dotenvy` (1 crate); load `./.env` (or `WHATSRUST_ENV_FILE`) early in `main`; new `WHATSRUST_*` knobs + `BridgeConfig` defaults; `.env.example` documenting every knob. (**I3:** embedder config field may not exist yet — that's M2; don't spawn a drain worker in M1.) | `.env` overrides defaults; missing `.env` = silent no-op; `.env.example` covers all M1 knobs. |
| 1.2.9 | Worker tests (ADR 0025) via the history-source fake: cursor advance, stop-condition eval, anchor extraction, cancel/resume, pacing/backoff, enqueue validation + override gating. | All pass; suite green. |

**M1.2 exit:** a triggered backfill paginates a chat under the pacer, persists into `messages`,
respects the 3-level abort, and cannot be outrun by any client. Safety is in the daemon.

---

## M1.3 — Lexical search (FTS5)  [ADR 0019]

**Goal:** `search_inbound` (and its API callers) query FTS5/BM25 over the unified table.

| # | Task | Verify |
|---|---|---|
| 1.3.1 | Rewrite `search_inbound` to `… WHERE messages_fts MATCH ?1 … ORDER BY rank` (ascending; BM25 default — ADR 0019). Keep chat-scope + `before_ts` predicates; verify plan with `EXPLAIN QUERY PLAN`. | FTS query returns ranked hits; Hebrew/Arabic tokens match; `EXPLAIN` shows FTS index used. |
| 1.3.2 | **MATCH sanitization policy (ADR 0019 open item):** pick + implement — simplest is quote user input as a phrase, escaping `"`→`""`, so raw input can't trigger FTS syntax/parse errors. | Adversarial inputs (`"`, `AND`, `*`, `col:`) don't error or change semantics unexpectedly; test covers them. |
| 1.3.3 | Keep a non-FTS fallback path only if `EXPLAIN` shows a predicate that doesn't push down at scale (ADR 0019 note); otherwise single FTS path. | Decision recorded; if fallback kept, it's tested. |
| 1.3.4 | Update `api.rs:890/:906` callers; confirm CLI/MCP search still shaped the same to clients. | API search returns FTS-ranked results; MCP `whatsrust_search` unaffected in shape. |

**M1.3 exit:** search is FTS5/BM25 over live + backfilled messages, multilingual, injection-safe.

> ✅ **M1.3 DONE (2026-07-13).** `search_inbound` split into two branches: `query=None` keeps the
> chronological browse (`ORDER BY timestamp DESC`, byte-identical to before — `handle_history`);
> `query=Some` now runs FTS5 over the external-content index — `FROM messages_fts f JOIN messages m
> ON m.id = f.rowid WHERE f.body_text MATCH ?1 … ORDER BY rank` (ascending; BM25 negated score →
> most-relevant first; never `DESC`). **Sanitization (1.3.2):** quote-as-phrase — wrap input in
> `"…"`, escape `"`→`""`; ALL input is a literal phrase so FTS5 operators (`AND`/`OR`/`NOT`/`*`/`col:`)
> can't parse-error or alter semantics. **Fallback (1.3.3):** none — `EXPLAIN QUERY PLAN` shows
> `SCAN f VIRTUAL TABLE INDEX` + `SEARCH m USING INTEGER PRIMARY KEY (rowid=?)`, so the FTS index
> drives; no LIKE fallback kept. **Callers (1.3.4):** no code change needed in `api.rs`/`mcp.rs` —
> `/api/search` JSON shape unchanged (now relevance-ranked, not newest-first); `handle_history`
> unaffected. 9 new storage tests (English/Hebrew/Arabic hits, adversarial operators as literal
> phrases, whitespace/quote-only no-error, chat scoping, chronological browse, EXPLAIN). Suite green:
> 188 lib / 203 bin. No DDL/trigger changes. (ADR 0019)

---

## M1.4 — API/MCP trigger + watchdog  [ADR 0011/0013/0034/0036]

**Goal:** expose the trigger/status/cancel surface and the storage-growth watchdog.

| # | Task | Verify |
|---|---|---|
| 1.4.1 | **Endpoints (ADR 0011):** `POST /api/history-fetch` (enqueue→job_id), `GET /api/history-fetch` (status/list), `POST /api/history-fetch/cancel`. Immediate return `{job_id, chat_jid, target_kind, target_value?, resume_anchor, more_remain, status}`; no-op fast path when cursor `exhausted`; echo `{requested, accepted}` for clamps. | curl each endpoint against a live-ish daemon (fake source ok); no-op path returns immediately. |
| 1.4.2 | **MCP tool `whatsrust_fetch_history`** mirroring the trigger; description documents pacing/limits (advisory, ADR 0021). | Tool appears in MCP list; invocation enqueues; description present. |
| 1.4.3 | **SSE progress (ADR 0034):** since/all → fuzzy "N fetched, more remain"; count → "N / target"; explicit `paused/cooldown` states so pauses don't read as hangs. | SSE stream shows progress + paused state during a long pause. |
| 1.4.4 | **Watchdog (ADR 0013/0036):** reuse the periodic prune task; each tick `wal_checkpoint(PASSIVE)` then measure `db + -wal + -shm` via filesystem `stat`; compare to `metadata` baseline; ≥50% growth → WARN + `BridgeEvent` → reset baseline. Baseline seeded at migration (1.1.10). | Simulated growth crosses threshold → one alert + baseline reset; no alert on first tick. |
| 1.4.5 | CLI commands wired to the endpoints (trigger/status/cancel) for parity across CLI/REST/MCP. | CLI trigger/status/cancel work against the daemon. |

**M1.4 exit:** all M1 exit criteria met; manual E2E checklist (real phone, small fetch) passes.

> ✅ **M1.4 DONE (2026-07-16).** All 5 tasks landed across 4 waves + review fixes. Wave 1: `/api/history-fetch` trigger/status/cancel REST endpoints. Wave 2: MCP tools `whatsrust_fetch_history`/`_status`/`_cancel` (total 33 MCP tools). Wave 3: SSE `BackfillProgress` events (fuzzy "N fetched, more remain" for `since`/`all`; precise "N / target" for `count`; explicit `paused`/`cooldown` states). Wave 4 (this wave): storage-growth watchdog wired into the existing periodic prune tick — `storage_footprint()` (PASSIVE WAL checkpoint + 3-file stat), `get_metadata`/`set_metadata` for the `watchdog_last_alerted_size` baseline, `watchdog_should_alert()` pure decision fn (≥50% → WARN + `BridgeEvent::StorageAlert` SSE event + baseline reset; zero baseline → silent re-seed); `StorageAlertEvent{current_bytes, baseline_bytes, growth_pct}` + `format_sse_event` arm; temp `WHATSRUST_BACKFILL_TEST` block retired; `.env.example` confirmed clean. 7 new tests (metadata roundtrip + 6 boundary tests for `watchdog_should_alert`). Suite green: 212 lib / 225 bin (213/226 after the Wave-4 review fixes). **Live E2E smoke test PASSED 2026-07-20** (real account: API trigger → OnDemand PDO fetch → `done`; SSE `backfill` + `storage_alert` events observed; CLI + MCP parity; FTS/BM25 search; cancel→404; watchdog alert forced via a lowered baseline + short interval, then reverted) — **all M1 exit criteria met; M1 COMPLETE.**

---

## I-item fold-in (v3 review → where it lands)

| Item | Lands in |
|---|---|
| I1 group_cache on-demand post-rebase | ✅ verified in Phase 0 (GO); re-confirm during M1.2 live test |
| **I2 prune age-DELETE removal** | **M1.1.8 (must)** |
| I3 drain-worker spawn vs config | M2 (no drain worker in M1); note dependency in M1.2.8 |
| I4 watchdog baseline final migration step | M1.1.10 |
| I5 exhausted vs more_remain contract | M1.2.5 |
| I6 cancel-race CASE guard (not outbound's pattern) | M1.2.4 |
| I7 atomic enqueue-time validation (one closure) | M1.2.3 |

Residual risks R1–R6: R1 (rebase) retired by Phase 0 GO; R2 → M1.2.5; R3 (CJK-without-embedder)
documented, mitigated by M2; R4 (search latency) → add metrics in M1.3, reader-pool deferred (ADR 0027);
R5 → M1.2.7; R6 (set-difference scale) → M2.

---

## Sequencing & gating

M1.1 → M1.2 → M1.3 → M1.4, each with tests alongside (ADR 0025). **Gate to the user at each
sub-milestone boundary** per session convention; implementation delegated to subagents
(orchestrate, don't implement). `MsgSecretStore` stays a stubbed no-op through M1 (affects
decrypting history-delivered edits/reactions/poll-votes — an M1-era TODO, tracked, not blocking
lexical search).

**Open decisions to confirm before coding M1.1:** F-A resolution (copy-then-drop vs reorder),
and that ADR 0019/0009 doc patches are in-scope for M1.1.1.
