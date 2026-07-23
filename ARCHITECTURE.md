# Architecture

whatsrust is a pure Rust WhatsApp Web bridge. ~19,600 lines across 15 files.

## Module Map

```
src/
  bridge.rs          Core: event loop, messaging, groups, delivery receipts, group cache, presence, chat management, status/stories
  outbound.rs        Typed outbound ops (21 OpKinds), payload structs, execute_job()
  media_utils.rs     Media enrichment: image dims + JPEG thumbnails, audio waveform data
  bridge_events.rs   Broadcast event bus: BridgeEvent, OutboundStatusEvent, DeliveryStatus
  api.rs             REST API (58 endpoints), SSE streaming, CLI HTTP client
  mcp.rs             MCP server (33 tools, JSON-RPC over stdio)
  storage.rs         rusqlite Signal Protocol store + typed job queue + v8 unified messages table (FTS5 search) + backfill queue/cursor
  backfill.rs        Historical backfill worker: two-phase on-demand fetch (HistoryCorrelator), FIFO pagination, contained-C targets, pacer, 3-level abort
  dedup.rs           Generation-tracked DashMap dedup (concurrent-safe, bounded)
  read_receipts.rs   Batched receipt scheduler with flush-before-reply
  polls.rs           Poll crypto (HKDF-SHA256 + AES-256-GCM)
  qr.rs              QR rendering (terminal/PNG/HTML/SVG)
  instance_lock.rs   flock-based single-instance guard (prevents StreamReplaced loops)
  main.rs            Daemon (REPL + API), CLI client (49 commands), MCP mode
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

## Historical fetch & lexical search (M1)

M1 (per-chat historical backfill + FTS5 lexical search) is **complete** — all four sub-milestones shipped and live-E2E-validated. It builds a local, searchable message archive alongside the live timeline. M2 (semantic/embedding-based search, sidecar-backed) layers on top of this schema but is **not built yet** (ADR 0006/0008/0015-0017/0024).

### Schema (v8)

`messages` is a single unified table for both live and backfilled history: `chat_jid`, `sender_jid`, `message_id` (unique), `content_kind`, `body_text`, `timestamp`, `from_me`, `source` (`'live'` | `'backfill'`), `embed_status` (reserved for M2) (ADR 0009). Sibling tables: `media_refs` (lazily-hydrated media pointers, ADR 0005), `embeddings` (multi-model vector storage, ADR 0017 — unused until M2), `backfill_cursor` (per-chat frontier: oldest known message + `more_remain`/`exhausted` flags, ADR 0003), `backfill_jobs` (durable job queue, ADR 0010/0033), and `metadata` (generic KV singletons, e.g. the storage-watchdog baseline, ADR 0036).

`messages_fts` is an external-content FTS5 index (`content='messages'`, `content_rowid='id'`, `unicode61 remove_diacritics 2` tokenizer for Hebrew/Arabic/multilingual support) kept in sync by `'delete'`-command AFTER INSERT/UPDATE/DELETE triggers on `messages` (ADR 0019). The v7→v8 upgrade is a staged copy-then-drop migration: pristine pre-migration backup → FTS5 availability probe → migrate inside one transaction → post-commit validation → circuit-breaker pin (blocks retry-looping a broken migration) with `--rollback`/`--migrate` recovery flags (ADR 0028-0032). Everything still runs through the single `Arc<parking_lot::Mutex<Connection>>`.

### Backfill worker (`src/backfill.rs`)

Fetching history is a **two-phase async, on-demand** flow, since WhatsApp doesn't answer history requests synchronously:

1. The worker calls `client.fetch_message_history(...)`, which returns a `session_id` immediately (no messages yet).
2. The worker registers that `session_id` with `HistoryCorrelator` and awaits a oneshot receiver.
3. Sometime later, the batch arrives out-of-band as `Event::HistorySync`; `handle_event` looks up the pending `session_id` in the correlator and fulfills it.
4. The worker drains the batch, persists new rows under the **phone JID** (`extract_content_inner`, same path as live messages — ADR 0014) even when the response is keyed by the recipient's LID, and advances the chat's `backfill_cursor`.

This whole exchange sits behind two trait seams — `HistorySource` (fetch) and `BatchSink` (persist) — so the pagination/target/abort logic is unit-testable without a live WhatsApp connection or a real client (ADR 0025).

Jobs are durable: `backfill_jobs` rows track `chat_jid`, `target_kind`/`target_value`, `status`, and `fetched`, survive daemon restarts, and are drained by a **single sequential FIFO worker** (twin of the outbound worker) — no concurrency coordination needed since the anti-ban pacer budget is global anyway (ADR 0026). Targets follow the **contained-C model** — exactly one of `Since(ts_ms)`, `All`, or `Count(n)` per job (ADR 0033) — evaluated by a pure `evaluate_target` stop-condition function after each batch, with an autonomy **backstop** that parks (not fails) `since`/`all` jobs past a configured message ceiling for re-trigger (`count` jobs are already bounded and never park). A **stuck-anchor guard** aborts a job as `failed` if the oldest-message anchor stops advancing for two consecutive batches.

Abort is **three-level** (ADR 0026): a **BATCH** (send request → await response → persist + advance cursor in one TX) is never interrupted once started; a **JOB** is abortable only at batch boundaries (terminal `cancelled`, resumable via the cursor); the **TASK** (worker loop) is never hard-aborted, only cooperatively stopped via a `CancellationToken`. Cancel vs. daemon-shutdown diverge in terminal state: explicit cancel → `'cancelled'` (dead); shutdown → back to `'queued'` (resumable, since a large backfill can't drain in the shutdown window). A dedicated `BackfillPacer` (separate token bucket from the send pacer) paces inter-batch fetches, with an interruptible sleep so cancel stays responsive.

**Connection-gating:** if the client isn't connected when a batch would fetch, the job defers (`queued`, not failed) and resumes once reconnected — same posture as the outbound worker.

**Daemon-side safety (fail-closed):** `WHATSRUST_BACKFILL_*` env knobs (interval, max concurrent, max messages, batch size, jitter, cooldown, queue depth) are validated at startup against hardcoded safety floors/ceilings; violations refuse to start unless explicitly overridden via scoped `WHATSRUST_DANGEROUSLY_ALLOW_*` flags (ADR 0021/0022/0023). At request time, the daemon also enforces a global backfill queue-depth limit, clamps `max_messages` per request, and applies a per-chat cooldown — uniformly, regardless of which client (API/CLI/MCP) triggered the job.

### Lexical search

`search_inbound` branches on whether a query is given: with `query=Some(q)`, it joins `messages_fts MATCH` (quote-as-phrase sanitized — embedded `"` doubled, no raw FTS5 operator injection) to `messages` by rowid and orders by `f.rank` ascending (BM25, most-relevant first); with `query=None`, it's a plain chronological browse. One query path, EXPLAIN-verified to hit the FTS5 index (no LIKE fallback) — multilingual by construction via the `unicode61 remove_diacritics 2` tokenizer (ADR 0019/0018).

### Progress & observability

Each backfill batch — and the terminal outcome — emits a `BridgeEvent::BackfillProgress` on the broadcast bus, surfaced over SSE (`GET /api/events`): fuzzy "N fetched, more remaining" for `since`/`all` jobs, precise "N / target" for `count` jobs (percentages are computed client-side — ADR 0034). A **storage-growth watchdog** rides the existing hourly prune tick: it issues a `PASSIVE` WAL checkpoint, stats `db + -wal + -shm` on disk, and compares against a `metadata`-persisted baseline; ≥50% growth logs a WARN, emits `BridgeEvent::StorageAlert`, and resets the baseline so growth is measured incrementally rather than alerting once and going silent forever (ADR 0013/0036).

## Dependencies

The bridge depends on `whatsapp-rust` (wa-rs) by jlucaso1, git-pinned to a specific commit. wa-rs handles: WebSocket transport, Noise Protocol handshake, Signal Protocol encryption/decryption, protobuf encoding, keepalive pings, media upload/download.

The bridge layer handles: reconnection, state management, dedup, queueing, pacing, receipts, metrics, QR rendering, and the consumer API.
