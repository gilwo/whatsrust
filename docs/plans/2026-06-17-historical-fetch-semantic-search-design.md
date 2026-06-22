# Historical Message Fetch + Semantic/Lexical Search Design

**Date:** 2026-06-17  
**Status:** Designed (not yet implemented)  
**Updated:** 2026-06-22 — Incorporates 2026-06-18 design review resolution (R1-R12, see session file)

Consolidated spec for explicitly-triggered, per-chat historical message backfill
with local lexical (FTS5) + semantic (vector) search. This document is the
*what/how* blueprint; the *why* lives in the ADRs (`docs/adr/0001-0036`), which
are cross-linked throughout (e.g. "see ADR 0008").

---

## Scope

**Goals**
- Explicitly trigger historical message backfill for a single chat or group.
- Fetch target kinds: `all`, `since:<ts>`, `count:<n>` — single target per request with autonomy backstop (ADR 0033, supersedes earlier composable-stop-conditions).
- Store backfilled + live messages in one unified timeline, retained indefinitely.
- Lexical search (FTS5, always on) + optional semantic search (vectors via a sidecar).
- Multilingual (user data is Hebrew/Arabic; design is language-neutral). See ADR 0018.

**Non-goals (v1)**
- Communities (umbrella over N groups, no single timeline) — rejected, reject community JIDs (ADR 0004).
- Media/audio *content* embedding, audio transcription — text-only embeddings (ADR 0016).
- Arbitrary mid-history time-window fetch — only "older than what I have" (single contiguous frontier, ADR 0003).
- ANN vector index / loadable SQLite extensions — preserves single-5MB-binary ethos (ADR 0008).
- Account-wide full history sync — out of scope; per-chat only.

---

## High-level data flow

```
                  trigger (API / MCP / CLI)
                          │  returns job_id immediately
                          ▼
                ┌──────────────────────┐
                │ backfill-job queue    │ (SQLite, durable, twin of outbound queue; ADR 0010)
                └──────────┬───────────┘
                           ▼
                ┌──────────────────────┐    paced (dedicated backfill pacer; ADR 0020)
                │  backfill worker      │    SINGLE worker, sequential FIFO (ADR 0026)
                └──────────┬───────────┘    3-level abort: batch atomic, job resumable, task cooperative
                           ▼
              history-source trait  ──────►  wa-rs Client.fetch_message_history()
              (test seam; ADR 0025)          PDO HistorySyncOnDemandRequest → primary phone
                           │                 response: HistorySyncNotification(ON_DEMAND)
                           ▼                          → Event::HistorySync / JoinedGroup
              WebMessageInfo adapter (ADR 0014)
                           ▼
              extract_content_inner  ◄──── SAME path as live ingest (bridge.rs)
                           ▼
        ┌───────────────────────────────────────────┐
        │  unified `messages` table (ADR 0009)        │
        │   + media_refs (ADR 0005)                   │
        └───────┬───────────────────────┬────────────┘
                │ trigger                │ embed_status='pending'
                ▼                        ▼
        FTS5 external-content     embedding-drain worker (ADR 0015)
        (ADR 0019)                  └─► Embedder (stdio sidecar; ADR 0024)
                                          └─► vectors → embeddings table (BLOB; ADR 0008/0017)

  SEARCH:  query ─► FTS5 lexical recall (~50-200 candidates) ─► fetch their vectors
                  ─► cosine rerank in Rust ─► top-k        (ADR 0008)
```

Live messages flow through the identical `extract_content_inner` → `messages` →
FTS5 + embed-pending path; backfill is enrichment, not a separate pipeline.

---

## Prerequisite: wa-rs rebase (implementation step 0)

The pinned fork (`199-biotechnologies/whatsapp-rust` @ `9fb13a7`) is ~v0.2-era.
Upstream (`jlucaso1`) is at **v0.6.0** with a heavily-reworked history-sync/PDO
subsystem (`pdo.rs` 501→870, `history_sync.rs` 281→1066) — exactly what we build on.

**Rebase spike is a HARD GO/NO-GO GATE** (ADR 0002). Must resolve criteria before any F1 implementation:

**Gate criteria:**
- **G1:** whatsrust compiles + 89 tests pass vs v0.6 (API-breakage blast radius tractable).
- **G2:** History `WebMessageInfo` already plaintext (feeds ADR 0014 single-extraction adapter).
- **G3:** ON_DEMAND response correlatable to its request (drives paginated loop). Single-worker (ADR 0026) lowers bar: one PDO in-flight → "match the only one" suffices.

**Pivot paths pre-attached:**
- G1 deep-breakage → time-box rebase ≤N days, else cherry-pick only history-sync/PDO OR defer F1.
- G2 encrypted → ADR 0014 fallback B (parallel extractor).
- G3 no-correlation → single-flight matching; if impossible → F1 not viable, STOP.

**Minimal spike result (2026-06-22, static inspection of upstream v0.6.0):** Overall lean **GO, MEDIUM effort (~1-2 days mechanical)**. G1 = LIKELY-FAIL-but-mechanical (~15-20 call sites: Event::Message Arc wrappers, .on_event closure signature, exhaustive match adds 4 variants/removes JoinedGroup, LazyHistorySync); G2 = LIKELY-PASS (WebMessageInfo.message populated plaintext); G3 = LIKELY-PASS (peer_data_request_session_id field 12 exposed). JoinedGroup landmine defused (low-stakes handler, stale cache acceptable). No architectural blockers.

Per CLAUDE.md: work in `../whatsapp-rust`, push, bump the pinned `rev`.

---

## The fetch model (ADR 0003, refined by ADR 0033)

- **Anchor-based backward pagination.** `HistorySyncOnDemandRequest` takes an anchor
  (`oldest_msg_id`, `oldest_msg_from_me`, `oldest_msg_timestamp_ms`) + `on_demand_msg_count`
  → returns messages older than the anchor. Each batch's new oldest message becomes the next anchor.
- **Single contiguous backward frontier per chat.** We only ever fetch *older than the current oldest contiguous anchor* → no mid-history gaps, no arbitrary windows.

**Target model (contained-C, ADR 0033):** Single `target` kind per request (clean discriminator, NOT ambiguous composition):
- **`since:T`** → done when oldest crosses T OR phone exhausted. AUTO-CONTINUES across paced segments.
- **`all`** → done when phone exhausted. AUTO-CONTINUES.
- **`count:N`** → done when N fetched. Does NOT auto-continue ("fetch up to N then stop").

**Autonomy backstop (ADR 0033):** CONFIG-level guarded knob (ban-critical, ADR 0022/0030) bounds how far auto-continuing (since/all) may run in ONE trigger before STOP + require re-trigger. e.g. backstop=20k msgs. PARKS-and-requires-retrigger, does NOT auto-enqueue child jobs (flat queue, ADR 0026).

**Example:** target=since:9mo, backstop=20k. 10k chat → completes in one trigger (~25-40min auto-continuing). 50k chat → runs to 20k → parks → "re-trigger to continue".

- **Resume = re-trigger.** The persisted `backfill_cursor` holds the frontier; re-triggering the same chat continues from it.
- **No-op fast path:** if the cursor says `exhausted`, the job completes immediately.

---

## Storage / schema (ADR 0009, migration via ADR 0028-0032)

Migrate existing `inbound_messages` → unified `messages` **in place**
(`ALTER TABLE ... RENAME` + `ADD COLUMN`), bump schema version v7→v8 (ADR 0031 single-integer invariant). **Staged migration mode** (ADR 0028): open→read version→wal_checkpoint(TRUNCATE)→backup `.pre-migration-v7-<ts>.bak` (fail-closed)→TX migrate→validate (ADR 0029: V1 structural + V3 smoke probes)→start. Circuit-breaker (ADR 0030): pin file prevents re-migration crash loop, `--rollback` / `--migrate` flags. FTS5 availability probed at migration boundary (ADR 0032).

Live ingest becomes a full writer of the new columns. Sketches (illustrative, finalize at implementation):

```sql
-- Unified message timeline (live + backfill)
-- (rename of inbound_messages + added columns)
ALTER TABLE inbound_messages RENAME TO messages;
ALTER TABLE messages ADD COLUMN from_me      INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN source       TEXT    NOT NULL DEFAULT 'live';   -- 'live' | 'backfill'
ALTER TABLE messages ADD COLUMN embed_status TEXT    NOT NULL DEFAULT 'pending';-- pending|done|failed|skipped (ADR 0015/0016)
-- body_text holds genuine NL text only; NULL for non-content kinds (ADR 0019)

-- Media references (bytes hydrated lazily; ADR 0005)
CREATE TABLE media_refs (
    message_id      TEXT PRIMARY KEY,
    media_key       BLOB, direct_path TEXT, file_enc_sha256 BLOB,
    mimetype        TEXT, file_length INTEGER, width INTEGER, height INTEGER,
    hydrated_path   TEXT          -- set once bytes downloaded on demand
);

-- Vectors (multi-model retention; ADR 0017). Search filters active model_id.
CREATE TABLE embeddings (
    message_id TEXT NOT NULL,
    model_id   TEXT NOT NULL,
    dim        INTEGER NOT NULL,
    vec        BLOB NOT NULL,
    PRIMARY KEY (message_id, model_id)
);

-- Per-chat backfill frontier (ADR 0003)
CREATE TABLE backfill_cursor (
    chat_jid             TEXT PRIMARY KEY,
    oldest_msg_id        TEXT, oldest_msg_from_me INTEGER, oldest_msg_timestamp_ms INTEGER,
    more_remain          INTEGER NOT NULL DEFAULT 1,   -- phone said older history exists
    exhausted            INTEGER NOT NULL DEFAULT 0,
    last_backfill_at     INTEGER
);

-- Durable backfill-job queue (twin of outbound_queue; ADR 0010, schema revised by ADR 0033)
CREATE TABLE backfill_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_jid TEXT NOT NULL,
    target_kind TEXT NOT NULL CHECK (target_kind IN ('since', 'all', 'count')),  -- ADR 0033 contained-C
    target_value INTEGER,                              -- ts for since, count for count, NULL for all
    status TEXT NOT NULL,                              -- queued|running|paused|done|cancelled|failed
    fetched INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

-- Generic KV table for singletons (ADR 0036, created in F1 migration)
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY,
    value TEXT
);
-- watchdog baseline: ('watchdog_last_alerted_size', '<bytes>'); seed-on-absence (no false-alert on first tick)

-- FTS5 external-content over messages (ADR 0019)
CREATE VIRTUAL TABLE messages_fts USING fts5(
    body_text,
    content='messages', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'          -- never porter; ICU rejected (ADR 0018)
);
-- + AFTER INSERT/UPDATE/DELETE sync-trigger trio; `rebuild` for repair.
```

Per-model failure backoff (sidecar rejecting specific text) tracked separately
(lightweight `embed_failures(message_id, model_id, attempts)` or in-mem), NOT via
the `done` flag — drain work is derived by set-difference (ADR 0017).

**Existing access points to update:** `insert_inbound`, `search_inbound`,
the two delete paths, `prune_old_data` (remove its age-based DELETE — see Retention),
and the schema migration block in `storage.rs`.

**Single-connection model (ADR 0027):** Keep single `Arc<Mutex<Connection>>` (short closures, WAL allows readers between writes). Chunked transactions (backfill/embedding batches). Targeted hardening: `snapshot_db` gets its own backup connection (stops multi-second global stall). Reader pool deferred until measured latency problem.

---

## Migration & rollback (ADR 0028-0032)

**Staged migration mode (ADR 0028):** version < CURRENT → enter migration mode BEFORE full start:
1. `PRAGMA wal_checkpoint(TRUNCATE)` (flush WAL into main file, ensures backup captures single consistent file)
2. Backup → `whatsapp.db.pre-migration-v<from>-<ts>.bak` via SQLite Backup API (fail-closed: backup fails → abort migration)
3. Run migration in TX (`user_version` bump LAST → crash mid-migration auto-rolls-back, DB stays v_old)
4. Validate (ADR 0029: V1 structural checks + V3 smoke probes — FTS5 triggers, set-difference drain query, vector roundtrip; skip V2 full integrity_check as gate)
5. Pass → full start; fail → halt + instruct

**Circuit-breaker (ADR 0030):** Pin file `whatsapp.db.migration-pin` prevents re-migration crash loop. Startup decision table (pin presence × binary CURRENT vs DB version vs blocked_target) → normal / migration / halt-instruct / run-with-warning. `--rollback` (valid only when pin present, no footgun): restore .bak + DELETE -wal/-shm + update pin + EXIT (point-in-time-loss warning). `--migrate` clears pin + force retry. Newer binary (CURRENT > blocked_target) auto-retries.

**Schema version invariant (ADR 0031):** Single integer `CURRENT_SCHEMA_VERSION`. ANY migration-required change ⟺ version bump ⟺ full ceremony. Optional performance indexes = idempotent `CREATE INDEX IF NOT EXISTS` at startup, OUTSIDE version system. No MAJOR.MINOR split in v1 (deferred behind measured trigger).

**FTS5 probe (ADR 0032):** One-time probe at v7→v8 boundary before FTS5 DDL (`CREATE VIRTUAL TABLE temp.__fts5_probe...` or `sqlite_compileoption_used`). Absent → actionable error ("keep default `bundled` rusqlite feature") + leave DB at v7. Deliberate-misbuild guard, not degraded mode.

---

## Content extraction (ADR 0014)

Single extraction path. A thin adapter maps history-sync `WebMessageInfo` into the
inputs `extract_content_inner` already consumes (`wa::Message`, sender, chat,
timestamp, from_me). Live and backfilled messages parse identically (same kinds,
caption/body logic, media-ref derivation). Fallback (separate extractor) only if
the spike proves history `WebMessageInfo` is incompatible.

---

## Search

- **Lexical (always on):** FTS5 external-content, `unicode61 remove_diacritics 2`.
  Correct for space-delimited scripts (Hebrew/Arabic/Latin/Cyrillic). CJK/Thai
  degrade to whole-message token under unicode61 → lean on the vector layer; a
  `trigram` index is a deferred additive option (ADR 0018).
- **Semantic (optional):** FTS5 recalls ~50-200 candidates → fetch their BLOB vectors
  → **cosine rerank in Rust** → top-k (ADR 0008). No ANN index, no loadable extension.
  Pure-semantic queries (lexical miss) → optional bounded brute-force cosine over a
  recency/chat-scoped subset.
- **Vectors stamped `(model_id, dim)`; search filters the active model only**; never
  compare across models (ADR 0017). Embedder defaults to a **multilingual** model →
  CJK semantic search works even when FTS5 can't tokenize, plus free cross-lingual recall.

---

## Embedding subsystem

- **Drain worker (ADR 0015):** dedicated (3rd worker, alongside outbound + backfill),
  Notify-woken + periodic timer. Batches `embed_status='pending'` rows (batch=64,
  configurable) → sidecar → write vectors + flip `done`. No embedder configured →
  worker idles. Configured-but-failing → exponential backoff (cap 60s), rows STAY
  `pending` (transient outage must not leave permanent semantic holes). Per-row
  rejection → attempts cap 3 → `failed`.
- **Embeddable text (ADR 0016):** genuine natural language only (text body, image/video/doc
  captions, poll question+options, contact name, location name). Everything else →
  `skipped` at write time (never sent to sidecar). Text-only in v1.
- **Multi-model retention + explicit purge (ADR 0017):** keep `(message_id, model_id)`
  vectors; one model active at a time; non-active vectors are cold storage for cheap
  switch-back. Model switch is free; **no auto re-embed** (supersedes the rejected
  auto-stale idea). Drain work = set-difference: embeddable messages lacking an
  `(message_id, active_model)` vector. Explicit per-model purge
  (`DELETE WHERE model_id=?` + vacuum) is destructive but losslessly reapplicable
  (message text is the retained source; re-drain rebuilds).
- **Sidecar (ADR 0024):** stateless separate binary, pure vectorizer (owns no storage/search).
  Transport-neutral, batch + model-aware `Embedder` trait (`model_info()`,
  `embed(&[String])`, `health()`); v1 transport = stdio child, JSON-RPC 2.0
  newline-delimited (reuses `mcp.rs` framing). HTTP/localhost is a future sibling impl.
  - `model_info` → `{model_id, dim, max_batch?, max_input_tokens?}`
  - `embed {texts[]}` → `{vectors[][], model_id, dim}` (echo model+dim per response)
  - `health` → `{status: ok|loading|error, detail?}` (`loading` = wait, don't fall back)
  - **Trust-but-verify:** bridge validates model_id/dim/count; mismatch → reject batch
    as transport failure (rows stay `pending`), never store mislabeled/corrupt vectors.

---

## Retention + storage watchdog (ADR 0012, 0013, 0036)

- **No time-based deletion of message history** — kept indefinitely; removal only by
  explicit user action. Remove the age-based `DELETE FROM ...` from `prune_old_data`
  (keep outbound-queue cleanup, which is transient operational data).
- **Watchdog (ADR 0013/0036):** reuse the existing periodic prune task scaffolding (`bridge.rs`, interval
  `prune_interval_secs`). Each tick: `PRAGMA wal_checkpoint(PASSIVE)` then measure total
  on-disk footprint = `whatsapp.db` + `-wal` + `-shm` (filesystem `stat`, WAL-accurate —
  NOT the `page_count` pragma). Compare to **persisted last-alerted baseline** (stored in `metadata` table, ADR 0036); on
  ≥50% growth → log warning + emit a `BridgeEvent` (SSE-visible) → reset baseline. **Seed-on-absence:** first tick after table created → seed current size silently, no false-alert.

---

## Worker topology + anti-ban + safety

- **Backfill worker (ADR 0026):** SINGLE worker, sequential FIFO (genuine twin of outbound worker). "Concurrency cap" = 1 by construction; excess enqueues. Connection-gated (PDO needs live WA). Three-level abort: **BATCH** (send PDO → response → persist+cursor in ONE TX) = atomic, never interrupted; **JOB** = abortable at batch boundaries → 'cancelled', resumable; **TASK** = cooperative stop. Inter-batch sleep INTERRUPTIBLE (cancel responsive). CASE-guarded terminal status write prevents cancel-race. Shutdown unification: SIGINT/shutdown stops at batch boundary, terminal state = function of stop-reason (cancel-API → 'cancelled', shutdown → 'queued'/resumable). Embedding-drain is SEPARATE always-on task (talks only to sidecar).

- **Backfill pacing (ADR 0020):** dedicated pacer, SEPARATE from the outbound `SendPacer`
  (must not consume send budget). burst=1, base ~4s/batch with ±40% jitter (always on).
  Strictly **sequential** (await each response → extract anchor → pace → next), which the
  anchor-based protocol + single-worker model require. Occasional randomized long pauses (every ~5-15
  batches, ~20-90s) as secondary insurance; conservative *average rate* is the primary
  defense. Response timeout → exponential backoff → pause job (resumable). No elaborate
  human-simulation in v1.
  - **UX contract:** async job + `job_id` (no spinner); SSE progress with explicit
    `paused/cooldown` + resume-hint states so pauses don't read as hangs; trigger returns
    a rough ETA; document that semantic coverage lags fetch (FTS5 immediate, embeddings drain behind).
    Throughput reference: ~4s/batch × 64 → 5k ≈ 6-10 min (background marathon).
- **Daemon-side uniform enforcement (ADR 0021):** MCP is a thin proxy; pacers +
  global backfill concurrency cap + per-chat cooldown + `max_messages` clamp +
  outbound queue-depth limit live in the daemon BELOW the MCP layer → uniform across
  CLI/REST/MCP, an agent cannot outrun the pacer or pick a client that skips safety.
  All guards return structured back-pressure errors (429-style `{error, retry_after_secs}`,
  `{requested, accepted}`, `{status: already_active, job_id}`) so agents self-correct.
  Tool descriptions document pacing (advisory) but never enforce.
- **Fail-closed config (ADR 0022):** ban-critical knobs validated at startup against
  safe bounds → **refuse to start** (exit non-zero, explained error naming the exact
  override flag) unless a SCOPED `WHATSRUST_DANGEROUSLY_ALLOW_*` (per risk class, never
  one global bypass) is set → then start with a persistent WARN surfaced in status/SSE.
  Only a small curated set is guarded (backfill interval, concurrency cap, max_messages
  ceiling); benign knobs unguarded to avoid bypass-fatigue.

---

## API / MCP surface (ADR 0011, refined by ADR 0033-0035)

- **Endpoints:** `POST /api/history-fetch` (enqueue → job_id), `GET /api/history-fetch`
  (status / list active), `POST /api/history-fetch/cancel`. Reuse the existing SSE
  stream for live progress.
- **MCP tool:** one — `whatsrust_fetch_history` (mirrors the trigger). Its description
  documents the pacing/limits for agent expectation-setting (ADR 0021).
- **Immediate trigger return:** `{job_id, chat_jid, target_kind, target_value?, resume_anchor, more_remain, status}`;
  no-op fast path when the cursor is `exhausted`. Response echoes `{requested, accepted}` for clamped values (autonomy backstop, ADR 0033).
- **Enqueue-time validation (ADR 0035):** per-chat cooldown + one-active-per-chat enforced BEFORE durable write. Return structured back-pressure (`{status: already_active, job_id}` / 429 `{retry_after}`).
- **Progress (ADR 0034):** since/all modes show fuzzy "N fetched, more remain" (no ETA/total); count mode shows "N / target" (precise). SSE emits explicit `paused/cooldown` state during long pauses (ADR 0020 UX contract).
- "Continue/resume" needs no separate endpoint — re-trigger resumes from the cursor.

---

## Config (ADR 0023)

- **Mechanism:** env vars + `.env` file via **`dotenvy`** (1 crate, 0 transitive deps).
  No TOML, no parsed-config struct. New knobs = `WHATSRUST_*` read in `main.rs` + `BridgeConfig` defaults.
- **Precedence:** real env vars override `.env` (dotenv convention); `.env` fills unset
  vars only. Load `./.env` (or `WHATSRUST_ENV_FILE`) once, early in `main`, before any
  var reads; absent file = silent no-op (zero-config still works).
- **Files:** committed `.env.example` documents every `WHATSRUST_*` var (default + causal
  warning + exact override flag for guarded knobs); real `.env` gitignored.

**Guarded knobs (block + `DANGEROUSLY` override):** backfill min interval secs (hard floor),
backfill max concurrent jobs, `max_messages` ceiling.
**Unguarded knobs (free):** embedder endpoint/cmd, embedder batch size (64), drain backoff
cap, watchdog interval + growth-threshold %, per-chat cooldown, backfill batch size,
long-pause cadence/duration ranges, queue-depth limit, backup/prune intervals.

---

## Testing strategy (ADR 0025)

Extends the project's culture (inline unit tests, real temp-file DB for storage, no live-WA tests).
- **Unit (no fakes):** frontier-cursor advance, stop-condition eval, anchor extraction,
  community-reject, config validation + override gating, cosine math.
- **Storage (real temp DB):** rename-in-place migration, FTS5 trigger sync, set-difference
  drain query, embeddings BLOB roundtrip, search ranking, purge, watchdog size calc.
- **Two fake seams:** (1) `Embedder` trait → fake returning canned vectors; (2) NEW
  **history-source trait** the worker depends on (not `Client`) → inject canned
  `WebMessageInfo` batches + simulated more-remain/timeout to test pacing/backoff/cursor/cancel/resume.
- **Minimal fake-sidecar binary:** 1-2 true stdio-transport integration tests (exercise
  JSON-RPC framing + validation end-to-end).
- **E2E (real phone):** documented manual checklist, never CI.

---

## Implementation phasing (suggested order)

0. **wa-rs rebase spike** (ADR 0002) — HARD GO/NO-GO GATE, resolves G1/G2/G3 before anything else. Spike result: GO-leaning, MEDIUM ~1-2 days.
1. **Storage + migration** — unified `messages`, sibling tables (`metadata`, `media_refs`, `embeddings`, `backfill_cursor`, `backfill_jobs` with revised schema), FTS5 + triggers, access-point updates. Staged migration mode (ADR 0028-0032).
2. **Fetch worker** — history-source trait, backfill-job queue, single-worker FIFO pagination loop (ADR 0026), cursor, pacer (ADR 0020), contained-C target model (ADR 0033), enqueue-time validation (ADR 0035).
3. **Search** — FTS5 query + BLOB cosine rerank (ADR 0008/0019).
4. **Embedding sidecar + drain** — Embedder trait, stdio JSON-RPC (ADR 0024), drain worker (ADR 0015), multi-model store (ADR 0017), embeddable-text classification (ADR 0016).
5. **Safety + config** — daemon-side guards (ADR 0021), fail-closed config (ADR 0022), `.env` + `.env.example` (ADR 0023), autonomy backstop (ADR 0033).
6. **API / MCP** — trigger/status/cancel, `whatsrust_fetch_history`, SSE progress with fuzzy/precise per target-kind (ADR 0011/0034).
7. **Watchdog** — repurpose periodic task (ADR 0012/0013/0036), metadata table seed-on-absence.
8. **Tests** — per ADR 0025 alongside each layer (history-source fake, Embedder fake, storage temp-DB, minimal fake-sidecar binary).

---

## Open risks (spike resolved G1-G3, remaining runtime unknowns)

**Resolved by spike (2026-06-22):**
- **G1 (API breakage):** ~15-20 call sites, MEDIUM ~1-2 days mechanical. Event::Message Arc wrappers, .on_event closure, exhaustive match adds 4 / removes JoinedGroup (landmine defused — low-stakes handler).
- **G2 (WebMessageInfo plaintext):** LIKELY-PASS. WebMessageInfo.message populated plaintext (phone decrypts before packing into blob).
- **G3 (correlation):** LIKELY-PASS. peer_data_request_session_id field 12 exposed + single-worker fallback.

**Runtime unknowns (measure during implementation):**
- Does media `directPath` from history reliably resolve for lazy hydration, or expire too fast to be useful? (ADR 0005 — best-effort assumed).
- CJK-without-embedder lexical quality (trigger for deferred trigram option C; ADR 0018).
- Search latency under concurrent backfill (triggers ADR 0027 reader-pool option if >100ms p95).
