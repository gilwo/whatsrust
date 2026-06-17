# Historical Message Fetch + Semantic/Lexical Search Design

**Date:** 2026-06-17
**Status:** Designed (not yet implemented)

Consolidated spec for explicitly-triggered, per-chat historical message backfill
with local lexical (FTS5) + semantic (vector) search. This document is the
*what/how* blueprint; the *why* lives in the ADRs (`docs/adr/0001-0025`), which
are cross-linked throughout (e.g. "see ADR 0008").

---

## Scope

**Goals**
- Explicitly trigger historical message backfill for a single chat or group.
- Fetch modes: `all`, `since:<ts>`, bounded by `max_messages` — composable stop conditions on one backward-pagination loop (ADR 0003).
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
                │  backfill worker      │    sequential await-response loop
                └──────────┬───────────┘
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
**Rebase the fork onto upstream v0.6.0 first** (ADR 0002), as a spike that must resolve:

1. Whether history-sync `WebMessageInfo` is already plaintext (suspected yes) or needs separate Signal decryption (ADR 0014 fallback B).
2. Magnitude of whatsrust API breakage v0.2→v0.6 (event variants, client signatures; the recent LID work may overlap).
3. That the ON_DEMAND `HistorySyncNotification` response path is wired to an event whatsrust can consume (today it is only logged).

Per CLAUDE.md: work in `../whatsapp-rust`, push, bump the pinned `rev`.

---

## The fetch model (ADR 0003)

- **Anchor-based backward pagination.** `HistorySyncOnDemandRequest` takes an anchor
  (`oldest_msg_id`, `oldest_msg_from_me`, `oldest_msg_timestamp_ms`) + `on_demand_msg_count`
  → returns messages older than the anchor. Each batch's new oldest message becomes the next anchor.
- **Single contiguous backward frontier per chat.** We only ever fetch *older than the current oldest contiguous anchor* → no mid-history gaps, no arbitrary windows.
- **Stop conditions (composable):** `all` = until phone reports exhausted; `since:<ts>` = until oldest crosses T; `max_messages` = until N pulled. They combine (e.g. `since:90d` + `max:5000`).
- **Resume = re-trigger.** The persisted `backfill_cursor` holds the frontier; re-triggering the same chat continues from it. "Fetch 5000, then continue" is the same call twice.
- **No-op fast path:** if the cursor says `exhausted`, the job completes immediately.

---

## Storage / schema (ADR 0009)

Migrate existing `inbound_messages` → unified `messages` **in place**
(`ALTER TABLE ... RENAME` + `ADD COLUMN`), bump schema version. Live ingest becomes
a full writer of the new columns. Sketches (illustrative, finalize at implementation):

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

-- Durable backfill-job queue (twin of outbound_queue; ADR 0010)
CREATE TABLE backfill_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_jid TEXT NOT NULL, mode TEXT NOT NULL,        -- 'all' | 'since' | ...
    since_ts INTEGER, max_messages INTEGER,
    status TEXT NOT NULL,                              -- queued|running|paused|done|cancelled|failed
    fetched INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

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

## Retention + storage watchdog (ADR 0012, 0013)

- **No time-based deletion of message history** — kept indefinitely; removal only by
  explicit user action. Remove the age-based `DELETE FROM ...` from `prune_old_data`
  (keep outbound-queue cleanup, which is transient operational data).
- **Watchdog:** reuse the existing periodic prune task scaffolding (`bridge.rs`, interval
  `prune_interval_secs`). Each tick: `PRAGMA wal_checkpoint(PASSIVE)` then measure total
  on-disk footprint = `whatsapp.db` + `-wal` + `-shm` (filesystem `stat`, WAL-accurate —
  NOT the `page_count` pragma). Compare to a **persisted last-alerted baseline**; on
  ≥50% growth → log warning + emit a `BridgeEvent` (SSE-visible) → reset baseline.

---

## Anti-ban + safety

- **Backfill pacing (ADR 0020):** dedicated pacer, SEPARATE from the outbound `SendPacer`
  (must not consume send budget). burst=1, base ~4s/batch with ±40% jitter (always on).
  Strictly **sequential** (await each response → extract anchor → pace → next), which the
  anchor-based protocol requires anyway. Occasional randomized long pauses (every ~5-15
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

## API / MCP surface (ADR 0011)

- **Endpoints:** `POST /api/history-fetch` (enqueue → job_id), `GET /api/history-fetch`
  (status / list active), `POST /api/history-fetch/cancel`. Reuse the existing SSE
  stream for live progress.
- **MCP tool:** one — `whatsrust_fetch_history` (mirrors the trigger). Its description
  documents the pacing/limits for agent expectation-setting (ADR 0021).
- **Immediate trigger return:** `{job_id, chat_jid, mode, resume_anchor, more_remain, status}`;
  no-op fast path when the cursor is `exhausted`.
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

0. **wa-rs rebase spike** (ADR 0002) — resolves the open risks below before anything else.
1. **Storage + migration** — unified `messages`, sibling tables, FTS5 + triggers, access-point updates.
2. **Fetch worker** — history-source trait, backfill-job queue, pagination loop, cursor, pacer (ADR 0003/0010/0020).
3. **Search** — FTS5 query + BLOB cosine rerank (ADR 0008/0019).
4. **Embedding sidecar + drain** — Embedder trait, stdio JSON-RPC, drain worker, multi-model store (ADR 0015/0017/0024).
5. **Safety + config** — daemon-side guards, fail-closed config, `.env` + `.env.example` (ADR 0021/0022/0023).
6. **API / MCP** — trigger/status/cancel, `whatsrust_fetch_history`, SSE progress (ADR 0011).
7. **Watchdog** — repurpose periodic task (ADR 0012/0013).
8. **Tests** — per ADR 0025 alongside each layer.

---

## Open risks (spike must resolve)

- Is history `WebMessageInfo` plaintext, or does it need separate Signal decryption? (ADR 0014)
- v0.2→v0.6 wa-rs API breakage surface in `bridge.rs` (events, client signatures, LID overlap).
- Does the ON_DEMAND `HistorySyncNotification` response reliably route to a consumable event, and how is a per-chat request correlated to its response? (upstream added `stanza_id`/device-0 validation in `pdo.rs`).
- Does media `directPath` from history reliably resolve for lazy hydration, or expire too fast to be useful? (ADR 0005 — best-effort assumed).
- CJK-without-embedder lexical quality (trigger for deferred trigram option C; ADR 0018).
