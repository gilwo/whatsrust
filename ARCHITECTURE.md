# Architecture

whatsrust is a pure Rust WhatsApp Web bridge. ~19,600 lines across 15 files.

## Module Map

```
src/
  bridge.rs          Core: event loop, messaging, groups, delivery receipts, group cache, presence, chat management, status/stories
  outbound.rs        Typed outbound ops (21 OpKinds), payload structs, execute_job()
  media_utils.rs     Media enrichment: image dims + JPEG thumbnails, audio waveform data
  bridge_events.rs   Broadcast event bus: BridgeEvent, OutboundStatusEvent, DeliveryStatus
  api.rs             REST API (59 endpoints), SSE streaming, CLI HTTP client
  mcp.rs             MCP stdio server (34 tools, JSON-RPC)
  mcp_registry.rs    Shared MCP tool registry (metadata, annotations, validation, REST mapping)
  mcp_http.rs        Opt-in MCP Streamable HTTP listener (POST /mcp)
  storage.rs         rusqlite Signal Protocol store + typed job queue + v9 unified messages table (FTS5 search, embed_status) + media_refs (v9 columns) + embeddings (multi-model BLOB vectors) + backfill queue/cursor
  backfill.rs        Historical backfill worker: two-phase on-demand fetch (HistoryCorrelator), FIFO pagination, contained-C targets, pacer, 3-level abort
  embedder.rs        Embedder trait (sidecar contract), StdioEmbedder (JSON-RPC stdio), FakeEmbedder (test seam), cosine_similarity
  embed_drain.rs     Embedding-drain worker: set-difference query, batch sidecar calls, in-memory failure tracking, circuit-breaker
  dedup.rs           Generation-tracked DashMap dedup (concurrent-safe, bounded)
  read_receipts.rs   Batched receipt scheduler with flush-before-reply
  polls.rs           Poll crypto (HKDF-SHA256 + AES-256-GCM)
  qr.rs              QR rendering (terminal/PNG/HTML/SVG)
  instance_lock.rs   flock-based single-instance guard (prevents StreamReplaced loops)
  main.rs            Daemon (REPL + API), CLI client (50 commands), MCP mode
  lib.rs             Library crate exports (all modules pub, consumed by habb)
```

## Data Flow

```
                          ┌──────────────┐
                          │  WhatsApp    │
                          │  Servers     │
                          └──────┬───────┘
                                 │ WebSocket (Noise Protocol + Signal E2EE)
                          ┌──────┴───────┐
                          │  wa-rs       │  ← whatsapp-rust library (git-pinned)
                          │  (Client)    │
                          └──────┬───────┘
                                 │ Event callbacks (tokio::spawn per event)
                          ┌──────┴───────┐
                          │  bridge.rs   │
                          │  handle_event│
                          └──┬───────┬───┘
                             │       │
              ┌──────────────┘       └──────────────┐
              ▼                                      ▼
    ┌──────────────┐                      ┌──────────────────┐
    │ inbound_tx   │ mpsc channel         │ outbound queue   │ SQLite-backed
    │ (to consumer)│                      │ (from consumer)  │
    └──────────────┘                      └──────────────────┘
```

**Inbound path:** wa-rs event → `handle_event` → dedup check → content extraction (media download if needed) → `WhatsAppInbound` on mpsc channel → consumer.

**Sender allowlist and revoke rows:** after the dedup check, a message from a sender not on the allowlist (`WHATSAPP_ALLOWED`; `allowlist_drops`) is dropped before extraction, with one exception: a sender revoke (a protocol message of type REVOKE, found in the raw message through the ephemeral and view-once wrappers to the extractor's depth by `sender_revoke_target`; no extraction, no download) is stored as a revoke row (`store_revoke_tombstone`). Nothing else happens to it: no `BridgeEvent`, no inbound dispatch, no read receipt, no reply. Stored history has no allowlist and keeps that sender's media, so the revoke row is what lets the read-time judgement below see the revoke. Both paths write the row through `persist_inbound` (kind, label and embed classification come from the `InboundContent`), so the row is the same by structure; three guards keep an excluded sender from using the table: the row is stored only when the revoke's target ID and the message's own ID look like message IDs (non-empty, at most 128 bytes, `A-Za-z0-9._=+-`, because the ID goes into the searchable label); its `message_id` is `tombstone:<stanza id>` (`Store::insert_revoke_tombstone`), so it cannot take the ID of a message that arrives later and `get_oldest_message` never offers it as a fetch anchor; and it is skipped when an identical revoke row (chat, sender, `from_me`, label) exists (one `INSERT … WHERE NOT EXISTS`, through `idx_messages_revoke_label`). The ID is marked done in the dedup cache like any dropped message; after a failed write it is removed instead, so a redelivery retries.

**Outbound path:** Consumer calls `send_message()` → SQLite queue → `handle_outbound` loop → anti-ban pacing → `client.send_message()` → retry on failure.

**Chat management path:** `pin_chat()`, `mute_chat()`, `archive_chat()`, `mark_read()`, `delete_chat()`, `star_message()` use direct `client` calls (not the outbound queue). These are app-state mutations, not messages — they don't need queueing, retry, or pacing.

**Status/story path:** `send_status_text()`, `send_status_image()`, `send_status_video()`, `revoke_status()` go through the standard outbound queue. Status messages are regular messages sent to `status@broadcast`.

## Key Design Decisions

**parking_lot::Mutex + spawn_blocking for SQLite.** No async SQLite driver needed. WAL mode + `synchronous=NORMAL` gives fast writes without corruption risk.

**DashMap with generation counter for dedup.** Lock-free concurrent access. Generation tracking prevents eviction corruption after remove+re-admit cycles.

**SQLite-first sends.** All 17 outbound op types write to SQLite before returning success. Outbound worker wakes via `tokio::sync::Notify`, claims jobs with `claim_next_job()`, executes via `execute_job()` (media upload + send). Survives crashes — inflight jobs are requeued on restart.

**Broadcast event bus.** `tokio::sync::broadcast` (cap 256) carries `BridgeEvent::Inbound`, `OutboundStatus`, `Heartbeat`. Feeds SSE endpoint, in-process subscribers, and sync waiters (`enqueue_and_wait` subscribes BEFORE enqueue to avoid race).

**Token-bucket rate limiter.** Allows short bursts (default 5) while enforcing sustained rate (400ms/msg + jitter). Passive refill, no background task.

**Channel architecture.** `mpsc` for inbound to consumer, `broadcast` for event bus, `watch` for state + QR. `Notify` for outbound worker wakeup.

**Single-device only.** No `device_id` column in the protocol store. Cuts query complexity in half vs multi-device implementations.

**Read receipt batching.** Groups message IDs by (chat, participant) on a 200ms coalesce timer. Matches WhatsApp Web's native batching pattern. Flush-before-reply ensures read receipts go out before bot responses.

**Atomic BridgeMetrics.** All counters use `AtomicU64` — no locks, no DB reads for health checks. API server is raw TCP (no HTTP framework dependency) with connection semaphore (64) + dedicated SSE semaphore (8).

## Historical fetch & lexical/semantic search (M1 + M2)

**M1** (per-chat historical backfill + FTS5 lexical search) is **complete** — all four sub-milestones shipped and live-E2E-validated. It builds a local, searchable message archive alongside the live timeline.

**M2** (semantic/embedding-based search, sidecar-backed) is **complete** (embedder sidecar contract + drain worker + hybrid recall, ADR 0041, + multi-model purge; code-complete 2026-09-30, live-validated 2026-10-05). Semantic search is **opt-in** via `WHATSRUST_EMBEDDER_CMD` and **dormant** (degrades gracefully to FTS5 lexical) when the sidecar is absent or down. (ADR 0006/0008/0015-0017/0024/0038/0039/0041)

### Schema (v9)

`messages` is a single unified table for both live and backfilled history: `chat_jid`, `sender_jid`, `message_id` (unique), `content_kind`, `body_text`, `timestamp`, `from_me`, `source` (`'live'` | `'backfill'` | `'bootstrap'`), `embed_status` (`'pending'`/`'embedded'`/`'failed'`/`'skipped'` — tracks embedding-drain worker progress) (ADR 0009/0016). Sibling tables: `media_refs` (media pointers, written for media messages stored from history; bytes hydrated lazily, ADR 0005; the v9 columns below), `embeddings` (multi-model BLOB vector storage with composite PK `(message_id, model_id)`, ADR 0017 — M2 complete), `backfill_cursor` (per-chat frontier: oldest known message + `more_remain`/`exhausted` flags, ADR 0003), `backfill_jobs` (durable job queue, ADR 0010/0033), and `metadata` (generic KV singletons, e.g. the storage-watchdog baseline, ADR 0036, and the `device_name` fixed at pairing, ADR 0043).

`messages_fts` is an external-content FTS5 index (`content='messages'`, `content_rowid='id'`, `unicode61 remove_diacritics 2` tokenizer for Hebrew/Arabic/multilingual support) kept in sync by `'delete'`-command AFTER INSERT/UPDATE/DELETE triggers on `messages` (ADR 0019). The v7→v8 upgrade is a staged copy-then-drop migration: pristine pre-migration backup → FTS5 availability probe → migrate inside one transaction → post-commit validation → circuit-breaker pin (blocks retry-looping a broken migration) with `--rollback`/`--migrate` recovery flags (ADR 0028-0032). Everything still runs through the single `Arc<parking_lot::Mutex<Connection>>`.

**Schema v9 (media download phase 2a; design `docs/plans/2026-10-29-on-demand-media-download-design.md` §4).** `media_refs` gains six nullable columns, written for history media only (rows stored before v9 keep NULL): `file_name` (a document's raw name), `file_sha256`, `remote_jid` and `participant` (the message key's raw JIDs, LID or phone form, as the phone sent them), `view_once` (1 for view-once media, 0 for other media, NULL = unknown) and `media_key_timestamp` (the message's `mediaKeyTimestamp`). `messages` gets the partial index `idx_messages_revoke_label ON messages(body_text) WHERE content_kind = 'revoke'` (the lookup of a revoke row by its label `[deleted <id>]`; ADR 0031 optional index, created by `SCHEMA` and by the v9 step). The step is `ensure_v9_columns`: idempotent (a column only when missing, the index `IF NOT EXISTS`, no repair and no pass over rows), so the in-place downgrade (`PRAGMA user_version = 8`, start a v8 build; HOWTO §6c) can migrate again later, and it runs at every other start at v9 too (six column lookups when all is there), so a DB one column short never fails a media insert; `--migrate` always re-validates, and a failure there writes the same `failed` pin as a failed migration so `--rollback` stays available. Post-commit validation checks the columns, the index and an update-trigger probe (an UPDATE of the text must reach `messages_fts`); the probes run in a transaction that is rolled back. `view_once` is set inside `extract_content_inner`'s own recursion in `MediaMode::Reference` (each view-once wrapper arm, also below an ephemeral wrapper, and the media message's own flag), not by a separate walk.

**The own-message repair** (`repair_own_attribution`, called from `open_with_mode`) is not part of the version step: it runs after a successful open at v9, at **every** start that knows the owner's number (the `device` row; logout deletes it, so an unpaired profile waits). It is idempotent, so it also corrects rows misattributed later (a v8 window, a LID that became known). The marker `metadata.own_attribution_repair` (`<bare number> <first-run epoch>`) only guards against a re-pair: another number skips the repair, with a warning once (note `own_attribution_repair_skipped`), debug level afterwards. Order: classify first, reading only (the distinct live senders with `from_me = 0` and the distinct history-own senders, each classified once by `is_owner`; stale cursor flags counted); nothing to change means no backup and no write (but the marker on the first run). If rows will change and the open did not just take the pre-migration backup, it writes `<db>.pre-repair-<ts>.bak` (mode 600, newest 3 kept) first, and skips this start if that fails; a failed attempt's backup is reused by the retry. Then one transaction, with `messages_fts_update` dropped and re-created from the `SCHEMA` text (the text does not change): live rows whose sender `is_owner` get `from_me = 1`; history rows (`bootstrap`, `backfill`) with `from_me = 1` whose sender is not the owner get the own phone JID; a fetch cursor whose anchor is now an own message gets its flag set; `IN (…)` lists of at most 500 values. Before the commit a post-condition is read (no live owner row with `from_me = 0`, no history own row with a foreign sender); a failure (the scan too) rolls back and writes `metadata.own_attribution_repair_failed` (`<epoch> <backup name> <reason>`, no JIDs; a success clears it); for 24 hours after it the repair is not tried again, then it is retried once per day. On a migrating start the watchdog baseline is seeded after the repair. The identity helpers (`OwnIdentity`, `read_own_identity`, `bare_user`, `user_forms`, `same_user`, `is_owner`, `same_chat`) work on a `&Connection` and use `lid_pn_mapping` for the phone/LID twins; `is_owner` is the one predicate for both repairs and for the revoke judgement below.

**History media is written in one transaction** (`Store::insert_history_message`): the message row and its `media_refs` row, so a failed reference drops its message (counted as `failed`). A message id that is already stored follows the duplicate rule: only when the incoming message matches the stored one (chat, kind, `from_me`, sender; 1:1 chats and senders as the same user through the mapping, groups exactly, two own rows always) it fills the stored reference or, for a `bootstrap`/`backfill` row without one, creates it (`live` rows get none). The fill is a pure merge function (`merge_media_ref`) applied in the same transaction: read the stored row, merge, compare, `UPDATE` only when it differs (an exact `Filled` / `Unchanged`). It keeps the encryption tuple (`media_key`, `direct_path`, `file_enc_sha256`, `media_key_timestamp`) consistent: filled only all together, only when the stored `media_key`, `direct_path` and `file_enc_sha256` are all NULL and the incoming three are all non-NULL, with the incoming timestamp. The plaintext properties (`file_sha256`, `file_length`, `mimetype`, `width`, `height`, `file_name`, `remote_jid`, `participant`) fill per column, `view_once` takes the larger value. A mismatch changes nothing and is counted (`duplicate_mismatch`).

**Revokes are judged when read** (media download phase 2b; design §5.5). `Store::is_revoked(message_id)` (`is_revoked_conn` inside a transaction; crate-private, called by the media engine of phases 4-6) stores no result: a revoke row is an ordinary `content_kind = 'revoke'` row with the label `[deleted <target id>]`, and the judgement reads the stored rows each time, through the partial index `idx_messages_revoke_label` (`body_text` equals the label; no `LIKE`). The target is loaded by `message_id` (none stored means not revoked); a revoke row's own `message_id` is never read, only its chat, sender, `from_me` and label (the candidates are `SELECT DISTINCT`, so repeated identical rows are one). A revoke row counts when `same_chat` holds and its revoker is the target's author: in a 1:1 chat the revoke row's `from_me` equals the target's (the repaired flag; no LID mapping needed); in a group an own target (`from_me = 1`) needs the owner as the revoker (the row's `from_me`, or `is_owner` of its sender), and any other target needs a revoker who is not the owner and is the target's sender as `same_user` (phone or LID form through `lid_pn_mapping`). An admin's delete is the revoke of someone who is not the author, so it does not count. The owner's identity is read once per call (`read_own_identity`). Because nothing is resolved at ingest, the order of arrival does not matter (a revoke stored before its target counts once the target is stored), a rule correction is a code change, and the key of the revoke message does not gate the result (its shape is only logged at debug level, without JIDs or IDs). Remaining false negatives fail open (the media stays available): an own revoke before the repair has run, an own revoke in a LID 1:1 chat stored before the LID-chat fix, a revoker in an unmapped LID form. Retention and prune features must keep the `content_kind = 'revoke'` rows.

### Backfill worker (`src/backfill.rs`)

Fetching history is a **two-phase async, on-demand** flow, since WhatsApp doesn't answer history requests synchronously.

Every request starts from an **anchor**: a real message key that the phone pages back from (ADR 0003 and its amendments). `backfill::resolve_anchor` takes the cursor's anchor, else the oldest stored message. The phone ignores a request without a real key, so a chat with neither is refused at once (REST 409 `no_anchor`, and the worker fails fast). Anchors also come from two other places: an archive or read-state sync from the phone, which carries the chat's last message key and seeds the cursor of a chat that has none, and the stored pairing package (below). The worker waits up to 180 s (`HISTORY_FETCH_WAIT`) for the phone, because a sleeping phone can receive the request minutes late. The flow:

1. The worker calls `client.fetch_message_history(...)`, which returns a `session_id` immediately (no messages yet).
2. The worker registers that `session_id` with `HistoryCorrelator` and awaits a oneshot receiver.
3. Sometime later, the batch arrives out-of-band as `Event::HistorySync`; `handle_event` looks up the pending `session_id` in the correlator and fulfills it.
4. The worker drains the batch, persists new rows under the **phone JID** (`extract_content_inner`, same path as live messages but without downloading media — ADR 0014, ADR 0048) even when the response is keyed by the recipient's LID, and advances the chat's `backfill_cursor`.

A history-sync chunk that no pending fetch claims is stored, not dropped (ADR 0048): the package the phone sends at pairing as `source='bootstrap'` (unless `WHATSRUST_STORE_PAIRING_HISTORY=false`), and a late on-demand answer as `'backfill'`. It goes through the same persistence path (`persist_history_messages`) in a spawned, best-effort task, so pairing never waits on it; the stored messages give those chats a fetch anchor. History rows download no media: `extract_content_inner` runs in `MediaMode::Reference`, so a media message is stored with a label built from `file_length` and a `media_refs` row, while live ingest (`MediaMode::Download`) still downloads eagerly.

This whole exchange sits behind two trait seams — `HistorySource` (fetch) and `BatchSink` (persist) — so the pagination/target/abort logic is unit-testable without a live WhatsApp connection or a real client (ADR 0025).

Jobs are durable: `backfill_jobs` rows track `chat_jid`, `target_kind`/`target_value`, `status`, and `fetched`, survive daemon restarts, and are drained by a **single sequential FIFO worker** (twin of the outbound worker) — no concurrency coordination needed since the anti-ban pacer budget is global anyway (ADR 0026). Targets follow the **contained-C model** — exactly one of `Since(ts_ms)`, `All`, or `Count(n)` per job (ADR 0033) — evaluated by a pure `evaluate_target` stop-condition function after each batch, with an autonomy **backstop** that parks (not fails) `since`/`all` jobs past a configured message ceiling for re-trigger (`count` jobs are already bounded and never park). A **stuck-anchor guard** aborts a job as `failed` if the oldest-message anchor stops advancing for two consecutive batches.

Abort is **three-level** (ADR 0026): a **BATCH** (send request → await response → persist + advance cursor in one TX) is never interrupted once started; a **JOB** is abortable only at batch boundaries (terminal `cancelled`, resumable via the cursor); the **TASK** (worker loop) is never hard-aborted, only cooperatively stopped via a `CancellationToken`. Cancel vs. daemon-shutdown diverge in terminal state: explicit cancel → `'cancelled'` (dead); shutdown → back to `'queued'` (resumable, since a large backfill can't drain in the shutdown window). A dedicated `BackfillPacer` (separate token bucket from the send pacer) paces inter-batch fetches, with an interruptible sleep so cancel stays responsive.

**Connection-gating:** if the client isn't connected when a batch would fetch, the job defers (`queued`, not failed) and resumes once reconnected — same posture as the outbound worker.

**Daemon-side safety (fail-closed):** `WHATSRUST_BACKFILL_*` env knobs (interval, max concurrent, max messages, batch size, jitter, cooldown, queue depth) are validated at startup against hardcoded safety floors/ceilings; violations refuse to start unless explicitly overridden via scoped `WHATSRUST_DANGEROUSLY_ALLOW_*` flags (ADR 0021/0022/0023). At request time, the daemon also enforces a global backfill queue-depth limit, clamps `max_messages` per request, and applies a per-chat cooldown — uniformly, regardless of which client (API/CLI/MCP) triggered the job.

### Lexical search

`search_inbound` branches on whether a query is given: with `query=Some(q)`, it joins `messages_fts MATCH` (quote-as-phrase sanitized — embedded `"` doubled, no raw FTS5 operator injection) to `messages` by rowid and orders by `f.rank` ascending (BM25, most-relevant first); with `query=None`, it's a plain chronological browse. One query path, EXPLAIN-verified to hit the FTS5 index (no LIKE fallback) — multilingual by construction via the `unicode61 remove_diacritics 2` tokenizer (ADR 0019/0018).

### Semantic search: hybrid recall (ADR 0041; M2.1–M2.6 code-complete, live-validated 2026-10-05)

**Opt-in via `WHATSRUST_EMBEDDER_CMD`**: when configured and the query embeds successfully,
`WhatsAppBridge::search` unions two candidate sets — **lexical** (FTS5, recall width = max of 200
and `limit`, unchanged from M1) and **vector** (a bounded brute-force cosine scan of every stored
vector for the active model in the search scope, same top-N width). Lexical candidates are always
kept (a semantic score never drops a word-matched hit). Vector-only candidates (no lexical match —
the case cross-language/paraphrase queries need) are admitted only when
`cosine ≥ WHATSRUST_SEMANTIC_MIN_SCORE` (default 0.55, calibrated for bge-m3 — ADR 0041); the vector
scan itself is skipped (falling back to the M2.4 lexical-recall-plus-rerank path) when the scope holds
more than `WHATSRUST_SEMANTIC_SCAN_CAP` vectors (default 50000). When no embedder is active, or the
query-embed call fails/times out, search is byte-identical to M1 lexical. (ADR 0007/0008/0041)

**Embedder sidecar**: stateless stdio JSON-RPC child process (ADR 0006/0024). Primary: `embedder-rs/`
(native Rust, candle-based; ADR 0040 — Python-free deploy), selectable via `--model minilm|bge-m3`
(or `WHATSRUST_EMBEDDER_MODEL`) — **`BAAI/bge-m3`** (1024-dim) is the default for `make run-semantic`
(stronger multilingual/cross-language recall, `max_batch: 8`); `paraphrase-multilingual-MiniLM-L12-v2`
(384-dim, lighter/faster) stays selectable (`make run-semantic EMBED_MODEL=minilm`). Both are
prefix-free and load in the background at startup (`model_info` answers instantly; `health` reports
`loading` → `ok`). Legacy reference: `scripts/embedder-sidecar.py` (Python + sentence-transformers,
MiniLM only, parity-verified). The `Embedder` trait (`src/embedder.rs`) abstracts transport;
`StdioEmbedder` handles the stdio framing and request/response lifecycle (load timeout, health
checks, trust-but-verify validation). A `FakeEmbedder` test seam (`src/embedder.rs`) plus a minimal
fake sidecar (`src/bin/fake-embedder.rs`, behind the `fake-embedder` Cargo feature) support round-trip
integration tests. (ADR 0025/0039)

**Embedding-drain worker**: independent background task (spawned alongside backfill/prune in `run_bridge`, before the reconnect loop — ADR 0015). Wakes periodically (default 60s), derives work via a set-difference query (`embed_status='pending'` minus already-embedded for the active model), batches text to the sidecar (default 64 msgs/batch), writes successful vectors to the `embeddings` table, and on sidecar/transport failure leaves rows `pending` for retry (never mislabels them). Per-row content-rejection tracking (in-memory `(message_id, model_id)` attempt counter, cap-3 → terminal `embed_status='failed'`) is scaffolded but **currently dormant** — the batch `embed` protocol can't distinguish a content rejection from a transport failure, so the cap-3→`failed` path is deferred until the protocol grows a positive rejection signal (M2.6.7 / ADR 0038). A pathological-pending circuit-breaker back-pressures **new backfill enqueues** (`enqueue_backfill_job` returns `EmbedderBacklogFull`) when the active-model pending backlog exceeds a ceiling (default 100k), so the drain can catch up without the backlog growing unbounded — the drain itself keeps running. Multi-model: `embeddings` PK is `(message_id, model_id)`, so switching models starts a fresh drain for the new model without purging old vectors (ADR 0017).

**Per-model purge**: `Store::purge_embeddings(model_id)` deletes all rows for a given model and runs SQLite `incremental_vacuum` to reclaim space (ADR 0017). Exposed via API `POST /api/embeddings/purge`, MCP tool `whatsrust_purge_embeddings`, and CLI `purge-embeddings <model_id>`. Returns `{model_id, rows_deleted, bytes_reclaimed}`.

### Progress & observability

Each backfill batch — and the terminal outcome — emits a `BridgeEvent::BackfillProgress` on the broadcast bus, surfaced over SSE (`GET /api/events`): fuzzy "N fetched, more remaining" for `since`/`all` jobs, precise "N / target" for `count` jobs (percentages are computed client-side — ADR 0034). A **storage-growth watchdog** rides the existing hourly prune tick: it issues a `PASSIVE` WAL checkpoint, stats `db + -wal + -shm` on disk, and compares against a `metadata`-persisted baseline; ≥50% growth logs a WARN, emits `BridgeEvent::StorageAlert`, and resets the baseline so growth is measured incrementally rather than alerting once and going silent forever (ADR 0013/0036).

## Dependencies

The bridge depends on `whatsapp-rust` (wa-rs) by jlucaso1, git-pinned to a specific commit. wa-rs handles: WebSocket transport, Noise Protocol handshake, Signal Protocol encryption/decryption, protobuf encoding, keepalive pings, media upload/download.

The bridge layer handles: reconnection, state management, dedup, queueing, pacing, receipts, metrics, QR rendering, and the consumer API.
