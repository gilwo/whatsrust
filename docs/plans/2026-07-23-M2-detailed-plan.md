# M2 — Semantic Search — Detailed Phase/Task Plan

**Date:** 2026-07-23
**Status:** Planned (written at M2 start, per IMPLEMENTATION-ROADMAP gate policy)
**Milestone:** M2 of F1. Layers onto M1 (historical fetch + FTS5 lexical search — DONE 2026-07-20).
**Design (what/how):** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md` — §High-level
data flow (:33), §Search (:238), §Embedding subsystem (:254), §Config (:352), §Testing strategy
(:370), §Implementation phasing steps 4-8 (:386).
**Why (ADRs):** 0006 (stateless sidecar), 0007 (FTS5 baseline + optional rerank), 0008 (BLOB
vectors + Rust cosine), 0013 (watchdog), 0015 (drain worker + resilience), 0016 (embeddable-text
definition), 0017 (multi-model retention + purge), 0018 (multilingual FTS/vector strategy), 0022
(fail-closed config), 0023 (env/dotenv config), 0024 (sidecar JSON-RPC protocol), 0025 (layered
testing).

> Written after reading the live post-M1 code (`src/storage.rs`, `src/bridge.rs`, `src/backfill.rs`,
> `src/api.rs`, `src/mcp.rs`) — not just the design sketch. Ten wrinkles surfaced where the design
> doc / ADRs diverge from, or under-specify, what M1 actually shipped. See **§0 Findings**; they
> are the first things to settle, mostly at M2.1/M2.2/M2.3 boundaries.

---

## 0. Findings from the current code (settle these first)

### F-A — `embeddings.message_id` is TEXT, not the ADR 0008 sketch's `INTEGER PRIMARY KEY`
`storage.rs:233-239`:
```sql
CREATE TABLE IF NOT EXISTS embeddings (
    message_id TEXT NOT NULL,
    model_id   TEXT NOT NULL,
    dim        INTEGER NOT NULL,
    vec        BLOB NOT NULL,
    PRIMARY KEY (message_id, model_id)
);
```
ADR 0008's sketch used `message_id INTEGER PRIMARY KEY` (single-column PK, no `model_id`). M1.1
already deviated (correctly) to match ADR 0017's multi-model composite-key design, and used the
TEXT `message_id` (WhatsApp's stable string ID) rather than the integer rowid — a good call,
since rowids get reassigned across the copy-then-drop v7→v8 migration (ADR 0009 correction) while
`message_id` is stable across any future re-migration.

**Recommended resolution:** accept the as-built TEXT-keyed schema; patch ADR 0008 with a dated
correction note (mirroring how ADR 0009's rename→copy-then-drop correction was handled in M1.1.1),
not a schema change. → note in M2.1/M2.2, no code change.

### F-B — search bridges two key spaces: FTS `rowid` and `embeddings.message_id`
`messages` has both `id` (INTEGER AUTOINCREMENT rowid) and `message_id` (TEXT UNIQUE).
`messages_fts` is `content_rowid='id'` (confirmed `storage.rs:271-276`). The M1.3 lexical query
(`storage.rs:1385-1421`) already does `FROM messages_fts f JOIN messages m ON m.id = f.rowid`.
Semantic rerank must extend this: FTS MATCH → `f.rowid` → join `messages` for `message_id` →
**separately** join/fetch `embeddings` keyed by `(m.message_id, active_model_id)` — a second join
on a different key than the first. Make this explicit in the M2.4 query rather than assuming a
single join chain.

**Recommended resolution:** two-step fetch (FTS candidates → collect `message_id`s → batch-fetch
their vectors by `message_id` list against the active model) rather than one giant 3-way SQL join;
simpler to reason about and test in isolation (recall query vs. vector fetch vs. rerank are three
independently testable pure/storage layers per ADR 0025).

### F-C — `embed_status` is ungoverned: every M1 row is `'pending'`, and `body_text` is NOT
NULL-for-non-content as the design assumed  ⚠️ decision needed
Two compounding facts:
1. `insert_message` (`storage.rs:1258-1288`) never writes `embed_status` explicitly — it relies on
   the schema `DEFAULT 'pending'` (`storage.rs:216`). **Every** live and backfilled row, regardless
   of `content_kind`, lands `'pending'`. ADR 0016 requires write-time classification (NL → pending,
   everything else → skipped); this has never run.
2. **New finding beyond the task brief:** both write call sites — live ingest (`bridge.rs:2681`,
   `body_text = inbound.content.display_text()`) and backfill's `LiveBatchSink::persist_batch`
   (`bridge.rs:4263`, `body_str = c.display_text()`) — store **`display_text()`**, not `None`, for
   non-content kinds. `display_text()` (`bridge.rs:433-492`) produces synthetic bracketed labels for
   *every* variant except empty text (`"[image 40KB]"`, `"[sticker 40KB]"`, `"[react 👍 on
   abc123]"`, `"[deleted xyz]"`, `"[receipt: delivered for 2 message(s)]"`, etc.). So `body_text IS
   NOT NULL` is true for nearly every row — it is **not** a usable embeddability proxy, and it is
   already what M1.3's FTS5 index searches over (an existing M1 lexical-search wrinkle: searching
   "image" will hit every captionless image's synthetic label — out of scope to fix in M2, noted
   for awareness only, **do not touch `body_text`/FTS in M2**).

   Consequence for M2: the classifier cannot reuse `body_text`. It needs a **new** function,
   distinct from `display_text()`, that returns the *bare* NL string ADR 0016 actually wants to
   embed (e.g. just the caption, not `"[image 40KB] <caption>"`) — call it
   `InboundContent::embeddable_text() -> Option<String>`. `embed_status` is then derived from
   `embeddable_text().is_some()`, and `embeddable_text()` is what gets sent to the sidecar (never
   `body_text`, never `display_text()`).

**The existing-backlog decision:** every already-migrated M1 row (live + backfilled, pre-M2) is
stuck `'pending'` with no way to recover the bare embeddable text (`body_text` holds the decorated
label, not the original caption/body — for `Text` kind they're identical, but not for
caption-bearing media). Two options:
- (a) **One-time reclassify pass** — re-derive `embed_status` for existing rows from
  `content_kind` alone (best-effort: `content_kind='text'` → keep pending if `body_text`
  non-empty; media/poll/contact/location kinds → best-effort strip the known bracket prefix to
  recover the caption, or just accept degraded embeddable-text quality for the backlog).
- (b) **Tolerate** — leave the backlog `pending`; the set-difference drain (F-D/M2.2) will send
  those rows' *stored* `body_text` (decorated labels, since that's all that remains) to the
  sidecar once. Non-NL kinds among them either produce weak-but-harmless vectors or get rejected
  and capped at 3 attempts → `failed` (ADR 0016's "negative": some vector-space noise, bounded).

**Recommended resolution: (b) tolerate.** The backlog is small (single-user, per-chat backfill,
not internet-scale), the raw `wa::Message` needed to re-extract clean captions is not retained
(only the rendered label is), and building a reclassify pass to parse brackets back out of
`display_text()` output is fragile and one-way (never needs to run again once M2.2 ships
write-time classification). Document this as a code comment at the classification call site; do
**not** write a migration for it. → **needs ADR 0038+** if this project wants it as a permanent
policy rather than an implementation note (flagged, not written here).

### F-D — no `embed_failures` table yet; ADR 0017 leaves it "in-memory OR a table" → DECIDED: in-memory
Confirmed: no `embed_failures` table exists anywhere in `storage.rs`. Per-row rejection-attempt
tracking (ADR 0015's "cap 3 attempts → `failed`") has nowhere to live today.

**Decision (2026-07-23): in-memory, NOT a table.** Track attempts in a process-local map
(`HashMap<(message_id, model_id), u8>`, or a `DashMap` if it ends up touched off the worker task)
owned by the drain worker. This keeps M2 **schema-migration-free** — no v8→v9 bump, no staged-
migration ceremony — which is the single biggest scope/risk reduction available to M2. Attempt-
counting is transient operational state and genuine content-rejections are rare; per ADR 0017
in-memory is an explicitly sanctioned option, so **no new ADR is needed** (ADR 0017's "in-memory
OR a table" open item resolves to "in-memory" for v1 — optionally note it in ADR 0017, but no
supersession).

**Accepted tradeoff:** attempt counts reset to zero on daemon restart. Worst case, a genuinely-
unembeddable row is retried up to 3 times again after a restart before re-retiring to `failed` (a
handful of wasted sidecar calls — never data loss, never a search regression). The terminal
`messages.embed_status = 'failed'` write IS durable once reached, so only rows still mid-budget at
restart are affected. If this ever proves to matter, promoting to a durable table is a clean
additive change (a future v-bump), losslessly — message text is retained, so nothing is lost by
starting in-memory. → revisit only if measured.

### F-E — drain worker spawn site: confirmed pattern to mirror
`WhatsAppBridge::start()` (`bridge.rs:855-921`) creates `outbound_notify`/`backfill_notify` as
`Arc<tokio::sync::Notify>` (`:862-863`), passes clones into `run_bridge`, which spawns the prune +
storage-watchdog task (`:2030-2115`) and the backfill worker (`:2117-2153`) as independent
`tokio::spawn` tasks sharing `cancel: CancellationToken`, all **before** the reconnect loop
(`:2162+`) that calls `run_bot_session` (connection-scoped, restarts per reconnect). ADR 0015 B3
requires the drain worker to spawn exactly like prune/backfill — connection-agnostic, alongside
them, NOT inside `run_bot_session`.

**Confirmed plumbing to mirror:** a new `embed_notify: Arc<tokio::sync::Notify>` field on
`WhatsAppBridge` (alongside `backfill_notify` at `:828`), created at `:863`, exposed via an
accessor mirroring `backfill_notify()` (`:1284-1286`). Unlike `backfill_notify` (which is notified
from `api.rs:1130` — the *external* enqueue endpoint), `embed_notify` should be notified directly
from *inside* `bridge.rs` at the two write-time classification call sites (`:2681` live ingest,
`:4263` `LiveBatchSink`) whenever a row lands `pending` — those call sites already live in
`bridge.rs`, so no cross-module notify plumbing is needed.

### F-F — config plumbing confirmed: `BridgeConfig` (`bridge.rs:676`) + `main.rs:229-335`
`BridgeConfig` struct at `bridge.rs:676`, `impl Default` at `:746`. `main.rs` loads `dotenvy`
early (`:229-241`, before tracing init so `RUST_LOG` in `.env` takes effect) then reads
`WHATSRUST_*` vars via a `parse_env_or` helper (`:295-302` for the M1.2.8 backfill knobs) before
constructing `BridgeConfig` (`:335`). M2 adds embedder knobs (command/args, batch size default 64,
drain backoff cap, loading-timeout secs, drain periodic interval) as new fields on this same
struct, read the same way — all **unguarded/benign** per ADR 0022/0023 (no `DANGEROUSLY_ALLOW_*`
gate; they don't affect ban risk, only local compute/latency).

### F-G — `mcp.rs`'s JSON-RPC framing is a *server*; the sidecar needs a *client* — reuse means
the wire-format conventions, not the function
`src/mcp.rs::run_mcp_server` (`:13-46`) reads JSON-RPC requests from **its own stdin** and writes
responses to **its own stdout** — it's the server half, proxying to the HTTP API (`http_get`/
`http_post` at `:348-395` are plain blocking `TcpStream` calls, unrelated to the stdio framing).
The embedder sidecar needs the **mirror image**: whatsrust spawns a **child process** and must
write JSON-RPC requests to the *child's* stdin and read responses from the *child's* stdout — the
client half. ADR 0024's "reuse `mcp.rs`'s exact JSON-RPC 2.0 over stdio, newline-delimited" should
be read as: reuse the wire-format conventions already proven in `mcp.rs` — `{jsonrpc, id, method,
params}` request / `{jsonrpc, id, result|error}` response (`JsonRpcRequest`/`JsonRpcResponse` at
`mcp.rs:48-67`), one JSON object per line, flush after write — not a literal call into
`run_mcp_server`.

**Recommended resolution (decide at M2.1):** define the request/response struct shapes fresh in
the new sidecar-client module (small, ~20 lines, not worth cross-crate/cross-module sharing for
two structs used in opposite directions), with a doc comment cross-referencing ADR 0024 and
`mcp.rs` as the sibling convention. Do not attempt to literally share code between the server and
client halves — the I/O direction differs too much to make a shared abstraction worth it at this
size.

### F-H — `search_inbound`'s M1.3 dual-branch and its response shape must survive untouched
`storage.rs:1335-1426` — `query=None` → chronological (`ORDER BY timestamp DESC`, byte-identical
to pre-M1.3 `handle_history`); `query=Some` → FTS5 `MATCH` + `ORDER BY f.rank` ascending
(BM25-negated, most-relevant first), quote-as-phrase sanitized. `InboundRow` (`storage.rs:927-936`:
`id, chat_jid, sender_jid, message_id, content_kind, body_text, timestamp`) is what `api.rs:950-956`
(`handle_search`) serializes as `{"messages": rows, "count": count}`. M1.3 needed **zero** `api.rs`/
`mcp.rs` changes because the shape didn't change — only ranking did. M2 semantic rerank must hold
to the same discipline: **re-order** rows, don't reshape them; the `query=None` chronological
branch is completely out of scope for M2 (semantic search only applies when `query=Some`).

### F-I — `tokio`'s `process` feature is not enabled
`Cargo.toml` tokio features: `["macros", "rt-multi-thread", "signal", "sync", "time", "io-util",
"io-std", "fs"]` — no `"process"`. `StdioEmbedder` needs `tokio::process::Command` (async child
spawn + piped stdin/stdout) — this feature must be added before M2.1 code compiles.

### F-J — the set-difference query shape is already smoke-tested; reuse it verbatim
`validate_migration_post_commit` (`storage.rs:563-568`) already probes:
```sql
SELECT m.message_id FROM messages m LEFT JOIN embeddings e
 ON m.message_id = e.message_id AND e.model_id = '__probe__'
 WHERE e.message_id IS NULL LIMIT 0;
```
This is a `LEFT JOIN ... WHERE ... IS NULL` anti-join, not the `NOT EXISTS` subquery form sketched
in the design doc / ADR 0017 — functionally equivalent, already proven to parse and plan against
the live v8 schema. M2.2's real drain-work query should reuse this exact anti-join shape (adding
`AND m.embed_status = 'pending'` and binding the real active `model_id`), not reinvent the `NOT
EXISTS` form.

---

## M2 exit criteria (from IMPLEMENTATION-ROADMAP.md + the design doc — the definition of done)

- With a sidecar configured, semantic search returns relevant, cosine-reranked results; without
  one configured (or if it's unhealthy / has no vectors for the active model), search cleanly
  falls back to the exact M1 lexical (FTS5/BM25) path — no errors, no blocking, no shape change.
- The embedding-drain worker keeps up under normal backfill load; the only coupling to backfill is
  the >100k pathological-pending circuit breaker (pauses new backfill *enqueue*, never throttles
  the backfill worker itself or blocks lexical search).
- Model switch is free (no auto re-embed) and per-model purge works (deletes + reclaims space,
  returns a byte count), exposed via API/MCP/CLI.
- The default/recommended embedder model is multilingual, so CJK/Thai — the scripts M1's
  `unicode61` FTS5 tokenizer can't usefully tokenize (ADR 0018) — get real semantic search
  coverage.
- Tests per ADR 0025 (fake `Embedder` seam, real temp-DB storage tests, 1-2 true stdio-transport
  integration tests via a minimal fake-sidecar binary) all green; no live-WA test required (the
  sidecar and drain worker are WA-connection-agnostic).
- No `embed_status`/`embeddings` row is ever silently mislabeled: trust-but-verify validation
  (ADR 0024) rejects malformed sidecar responses as transport failures, never stores them.

---

## M2.1 — Embedder sidecar contract & transport  [ADR 0006/0024/0025]

**Goal:** a transport-neutral `Embedder` trait, a real `StdioEmbedder` (spawns a child process,
speaks newline-delimited JSON-RPC 2.0), trust-but-verify validation, `health()` semantics, and a
fake `Embedder` test seam — with no drain worker wired yet.

| # | Task | Verify |
|---|---|---|
| 2.1.1 | Add `tokio` `process` feature to `Cargo.toml` (**F-I**). | `cargo build` succeeds with `tokio::process::Command` referenced. |
| 2.1.2 | Define the `Embedder` trait (`model_info() -> ModelInfo`, `embed(&[String]) -> Result<Vec<Vec<f32>>>`, `health() -> HealthStatus{Ok,Loading,Error}`) in a new module (e.g. `src/embedder.rs`), transport-neutral per ADR 0006. | Trait compiles; a trivial in-test struct implements it. |
| 2.1.3 | Settle **F-G**: define request/response JSON-RPC structs fresh in the new module (mirroring `mcp.rs`'s `{jsonrpc,id,method,params}`/`{jsonrpc,id,result,error}` shapes, doc-commented cross-reference to ADR 0024 + `mcp.rs`), not a shared abstraction with `run_mcp_server`. | Struct definitions present with the cross-reference comment; unit test round-trips a request/response pair through serde. |
| 2.1.4 | `StdioEmbedder`: spawn `WHATSRUST_EMBEDDER_CMD` (+ `WHATSRUST_EMBEDDER_ARGS`) via `tokio::process::Command`, write newline-delimited JSON-RPC requests to child stdin, read responses from child stdout via a buffered async line reader; call `model_info` once at construction and cache the result. | Integration test (built in M2.6 against the fake-sidecar binary) spawns a real child, round-trips `model_info`/`embed`/`health`. |
| 2.1.5 | Trust-but-verify validation (ADR 0024): after every `embed` response, check `model_id` + `dim` match the cached `model_info()`, `vectors.len() == texts.len()`, each vector's length `== dim`. Any mismatch → typed transport-failure error (never stored). | Unit test feeds a fake malformed response (wrong dim / wrong count / wrong model_id) → `embed()` returns an error, nothing written. |
| 2.1.6 | `health()` semantics: `ok` / `loading` / `error(detail)` relayed faithfully by `StdioEmbedder` — timeout-tracking for "stuck loading" lives in the **drain worker** (M2.3), not the trait impl, per ADR 0015 B4. | Unit test: `StdioEmbedder::health()` against a fake sidecar reporting each of the 3 states relays each unchanged. |
| 2.1.7 | Fake `Embedder` (ADR 0025 seam 1): deterministic canned vectors (e.g. `vec![0.1 * i; dim]` per input text), in-process (no child spawn) — used by M2.3/M2.4 tests. | Trivial test: fake's `embed()` output shape matches input count and configured `dim`. |

**M2.1 exit:** `Embedder` trait + real `StdioEmbedder` (child-process JSON-RPC, trust-but-verify,
faithful health relay) + fake `Embedder` seam all compile and pass unit tests. No drain worker,
no schema changes yet.

---

## M2.2 — Embeddable-text classification + set-difference work derivation  [ADR 0016/0017]

**Goal:** write-time classification at both ingest paths (NL → `pending`, else `skipped`), the
existing-backlog decision recorded, and the set-difference drain query implemented.

| # | Task | Verify |
|---|---|---|
| 2.2.1 | Settle **F-C** (part 1): add `InboundContent::embeddable_text(&self) -> Option<String>`, distinct from `display_text()` — bare NL only per ADR 0016 (`Text.body`; `Image`/`Video`/`Document` caption *only if present*, else `None`; `PollCreated` question+options joined; `Contact.display_name`; `Location.name` if present else `None`); `None` for every other kind (`Audio`, `Sticker`, `ReactionAdded`/`Removed`, `Edit`, `Revoke`, `PollVote`, `DeliveryReceipt` — `Edit` carries real text but ADR 0016 explicitly places it in "skipped"; don't relitigate). | Per-variant unit tests: image-with-caption → `Some(caption)` (not the `"[image ...]"` label); image-without-caption → `None`; sticker → `None` always; poll → `Some("question | opt1 | opt2")`-shaped text; edit → `None`. |
| 2.2.2 | Derive `embed_status` at write time (`"pending"` if `embeddable_text().is_some()`, else `"skipped"`); add it as an explicit bound param to `insert_message` (currently relies on the schema `DEFAULT`, `storage.rs:1258-1288`); wire both call sites: live ingest (`bridge.rs:2681-2686`) and `LiveBatchSink::persist_batch` (`bridge.rs:4263-4277`). | Storage test: insert one row per `content_kind`, assert `messages.embed_status` matches the ADR 0016 table exactly (image+caption→pending, sticker→skipped, poll→pending, reaction→skipped, edit→skipped, receipt→skipped, captionless image→skipped). |
| 2.2.3 | Settle **F-C** (part 2, existing-backlog decision): **tolerate** — no reclassify migration; document the reasoning as a code comment at the classification call site (raw `wa::Message` isn't retained, so a perfect backlog reclassify isn't possible; backlog volume is small; ADR 0016's failure mode for stray non-NL text is bounded by the 3-attempt cap in M2.3). | Decision recorded in this doc + the code comment; confirm no migration/backfill script is added for this. → flagged: **needs ADR 0038+** if promoted to permanent policy. |
| 2.2.4 | Set-difference drain query (**F-J**): reuse the exact anti-join shape already smoke-tested in `validate_migration_post_commit` (`storage.rs:563-568`) as a new `Store::fetch_pending_embeddings(active_model_id, batch_size)`: `SELECT m.message_id, m.body_text FROM messages m LEFT JOIN embeddings e ON m.message_id = e.message_id AND e.model_id = ?1 WHERE e.message_id IS NULL AND m.embed_status = 'pending' ORDER BY m.id LIMIT ?2`. (Uses `body_text` as the embeddable source for now — for `Text` kind it's identical to `embeddable_text()`; for caption-bearing kinds the stored `body_text` is the decorated label, per **F-C** — accept for backlog rows, see 2.2.3; going forward all newly-classified `pending` rows are only ever `Text`/caption/poll/contact/location kinds by construction of 2.2.2, so `body_text`'s decoration only ever wraps genuinely-embeddable text plus a short bracket prefix — note this residual wrinkle and consider stripping the bracket in the drain worker's text preparation step, M2.3). | Storage test seeds mixed `embed_status` + partial `embeddings` rows for model A; query returns exactly the pending-and-unvectored-for-A set, excludes `skipped`/`failed` rows and already-vectored rows. |

**M2.2 exit:** every newly-ingested message (live + backfill) is classified `pending`/`skipped` at
write time per ADR 0016; the set-difference query returns exactly the correct drain work-set for a
given active model; the existing-backlog handling is a documented decision, not silent behavior.

---

## M2.3 — Embedding-drain worker  [ADR 0015/0017]

**Goal:** an independent long-lived task, resilient to sidecar outages, bounded by a
pathological-pending circuit breaker. **No schema migration** (F-D decided in-memory) — this
sub-milestone is pure worker/async logic; `CURRENT_SCHEMA_VERSION` stays 8.

| # | Task | Verify |
|---|---|---|
| 2.3.1 | **F-D (decided: in-memory).** Add a process-local attempt tracker owned by the drain worker — `HashMap<(String /*message_id*/, String /*model_id*/), u8>` (or `DashMap` if touched off-task) keyed by `(message_id, active_model_id)`. **No schema change, no migration** (`CURRENT_SCHEMA_VERSION` stays 8). Attempts reset on restart by design (F-D tradeoff). | Unit test: incrementing the same key 3× trips the cap on the 3rd; a distinct key is independent; state is scoped to the worker (grep confirms no new table / no `CURRENT_SCHEMA_VERSION` bump / no DB write for attempts). |
| 2.3.2 | Add `embed_notify: Arc<tokio::sync::Notify>` to `WhatsAppBridge` (alongside `backfill_notify`, `bridge.rs:828/863`), exposed via an accessor mirroring `backfill_notify()` (`:1284-1286`). Call `.notify_one()` directly from the two M2.2 write-time classification call sites (`:2681`, `:4263`) whenever a row lands `pending` (**F-E**). | Test/inspection confirms `Notify` fires on a pending-classified insert; no change to `outbound_notify`/`backfill_notify` behavior. |
| 2.3.3 | Spawn the drain worker as an **independent long-lived task** in `WhatsAppBridge::start()`'s `run_bridge`, alongside the prune/watchdog block (`bridge.rs:2030-2115`) and the backfill-worker block (`:2117-2153`) — **not** inside `run_bot_session`/the reconnect loop (`:2162+`) — sharing `cancel: CancellationToken` (**F-E**, ADR 0015 B3). **No embedder configured → don't spawn the task at all** (ADR 0015 M2). | With no `WHATSRUST_EMBEDDER_CMD` set, the task is never spawned (log/inspection confirms); with it set, the task spawns alongside prune/backfill. |
| 2.3.4 | Drain loop core: `Notify`-woken + periodic timer (`WHATSRUST_EMBEDDER_DRAIN_INTERVAL_SECS`), fetch a batch via `fetch_pending_embeddings` (M2.2.4, default 64 via `WHATSRUST_EMBEDDER_BATCH_SIZE`), call `Embedder::embed(texts)`, on success `INSERT OR REPLACE INTO embeddings (message_id, model_id, dim, vec)` per row. `messages.embed_status` stays `'pending'` — the `embeddings` table itself is the source of truth for "done" (ADR 0017; no `'done'` value needed/used). | Worker test (fake `Embedder`): a batch of N pending rows → N `embeddings` rows appear with correct `model_id`/`dim`; re-running the set-difference query for the same model returns empty. |
| 2.3.5 | Sidecar-down resilience: transport failure → exponential backoff (cap 60s via `WHATSRUST_EMBEDDER_BACKOFF_CAP_SECS`), rows STAY `pending`, **no** attempt-counter increment; after N consecutive backoff cycles, drop to Notify-only (stop periodic polling until a new pending row wakes it) — ADR 0015 M2 hardening. | Worker test: fake `Embedder` configured to always transport-fail → rows remain `pending` after several loop iterations; backoff grows then caps; the in-memory attempt map stays empty (transport failures don't count against the content-rejection cap). |
| 2.3.6 | Per-row content rejection: a well-formed response that fails **per-row** (not whole-batch) validation increments that row's in-memory attempt count (F-D); cap 3 → `messages.embed_status='failed'` (terminal, drops out of the set-difference query). If the sidecar protocol has no native per-item rejection signal, treat whole-batch validation failure as a transport failure (2.3.5) and note that isolating a single bad row may require shrinking the batch — document this limitation rather than half-implementing per-item isolation. | Worker test: fake `Embedder` rejects one specific text 3 times running solo-batches → row flips to `failed` on the 3rd attempt and disappears from subsequent set-difference results. |
| 2.3.7 | Loading-timeout (ADR 0015 B4): track cumulative time in `health()==loading` across polls; `>60s` (`WHATSRUST_EMBEDDER_LOADING_TIMEOUT_SECS`) → treat as `error` for this cycle → skip embedding this tick (rows stay `pending`; FTS5 fallback keeps serving search uninterrupted). | Worker test: fake `Embedder` reports `loading` continuously with a simulated/injected clock past 60s → worker logs treat-as-error and does not block. |
| 2.3.8 | Pathological-pending circuit breaker (fork R3): periodically count `messages` rows with `embed_status='pending'`; if `>100_000` (test-overridable threshold), surface a flag the backfill **enqueue** path checks (`api.rs`/`storage.rs`) and rejects new backfill job enqueues with a structured error until the count drops back under threshold. Must not throttle the backfill *worker* (already-running jobs continue) or lexical search. | Test seeds >threshold pending rows (lowered threshold via config in test) → enqueue attempt is rejected with the circuit-breaker error; an already-running job is unaffected; once pending drops under threshold, enqueue succeeds again. |

**M2.3 exit:** the drain worker runs independently of WA connection state, never blocks FTS5
search, never leaves a row `failed` without 3 genuine content-rejection attempts (transport
outages never count against that cap), and the pathological-pending case throttles only new
backfill *enqueues*, never in-flight jobs.

---

## M2.4 — Semantic search path  [ADR 0007/0008/0017/0018]

**Goal:** layer FTS5-recall → active-model vector fetch → Rust cosine rerank onto
`search_inbound`, preserving the exact M1.3 lexical path as the no-sidecar/no-vectors fallback.

| # | Task | Verify |
|---|---|---|
| 2.4.1 | Cosine similarity as a pure function (dot product / magnitudes; error on mismatched `dim`; defined behavior for a zero-magnitude vector). | Unit tests (ADR 0025 pure-logic layer): orthogonal → 0, identical → 1, mismatched dim → `Err`, zero vector → defined (not NaN/panic). |
| 2.4.2 | Decide + document **active-model resolution**: the active model is whatever the currently-configured `Embedder`'s `model_info().model_id` reports at startup — no separate persisted "switch" state in v1 (switching models = restarting with a different `WHATSRUST_EMBEDDER_CMD`). This resolves ADR 0017's "one model active at a time" into a concrete mechanism. | Decision recorded in this doc; a config/wiring test confirms the active-model id is read from `model_info()`, not a separate config knob. |
| 2.4.3 | Extend `search_inbound` (**F-B**): when `query=Some(q)` AND an embedder is configured+healthy AND at least one embedding exists for the active model → run the existing FTS5 MATCH+rank query with an internally-widened recall (~50-200 candidates, independent of the caller's requested `limit`) → collect candidate `message_id`s → fetch their vectors from `embeddings` filtered to `(message_id IN (...), model_id = active)` → cosine rerank in Rust → truncate to the caller's `limit` → return `InboundRow`-shaped rows (**F-H**: order changes, shape doesn't). | Storage test (seeded vectors): FTS recall of a handful of candidates reranked into a predictable cosine order different from pure BM25 order; response rows use the exact same fields as the M1.3 tests assert. |
| 2.4.4 | No-sidecar / no-active-model / zero-embeddings-for-active-model fallback: any of {no embedder configured, sidecar unhealthy, zero vectors for active model, all candidate vectors missing} → return the **verbatim** M1.3 FTS5/BM25 path, unmodified, no error, negligible added latency (one cheap existence check). | Regression test: with zero rows in `embeddings`, re-run the exact M1.3 `search_inbound` test assertions against the M2 code path — results identical. |
| 2.4.5 | Multilingual default (ADR 0018): document the recommended `WHATSRUST_EMBEDDER_CMD`/model choice as a multilingual model (e.g. an e5/bge-m3-class multilingual model) in `.env.example`, with a one-line rationale (CJK/Thai semantic coverage where `unicode61` FTS5 degrades). whatsrust itself stays language-neutral — no code branches on language. | `.env.example` entry present recommending a multilingual model + the rationale. |
| 2.4.6 | Pure-semantic brute-force fallback (FTS5 zero-hit, embeddings exist) — ADR 0007/0008 "Future" item: **defer** past M2 (record explicitly, do not half-implement). | Decision recorded in this doc; no code path added for this in M2. |
| 2.4.7 | Confirm `api.rs::handle_search` / `mcp.rs` need **no changes** given the shape is preserved (M1.3 precedent: zero `api.rs` changes were needed when FTS5 landed). If an optional debug field (e.g. `matched_by: "lexical"\|"semantic"`) is added, it must be additive so old clients ignore it. | Existing `api.rs`/`mcp.rs` search-shape tests pass unmodified; if an optional field is added, a new test asserts its presence/absence in each mode. |

**M2.4 exit:** search returns semantically-reranked results whenever a healthy embedder + active-
model vectors exist, and falls back byte-for-byte to the M1.3 lexical path otherwise — zero
API/MCP shape breakage either way.

---

## M2.5 — Multi-model retention & explicit purge  [ADR 0017]

**Goal:** model switch stays free (no auto re-embed); explicit per-model purge exposed to the user
with a byte-count receipt.

| # | Task | Verify |
|---|---|---|
| 2.5.1 | Confirm/exercise the model-switch-is-free invariant: switching the configured embedder to a different `model_id` requires no migration; old vectors remain queryable (cold storage) the instant you switch back; the set-difference query (M2.2.4) automatically starts draining the new model's gaps on its own. | Storage test: seed vectors for model A, "switch" active model to B (test double config) → drain fills B's gaps → "switch back" to A → set-difference for A is immediately empty (zero drain work), A vectors already present and correct. |
| 2.5.2 | Explicit per-model purge: `DELETE FROM embeddings WHERE model_id = ?` followed by a bounded `PRAGMA incremental_vacuum` loop (**not** full `VACUUM`, per ADR 0017 R-prior5 — mirrors the existing prune/watchdog's non-blocking ethos). Expose via a new API endpoint (e.g. `POST /api/embeddings/purge`), an MCP tool, and a CLI subcommand; returns `{model_id, rows_deleted, bytes_reclaimed}`. | Storage test: purge model A → A rows gone, B rows untouched, returned `rows_deleted` matches; API/MCP/CLI round-trip test (fake bridge) confirms the same triple across all three surfaces. |
| 2.5.3 | Purge is **never automatic** — no code path calls it except the explicit admin surface from 2.5.2; document loudly (comment + this plan) since an accidental automatic purge would force full sidecar re-drain even though it's "lossless" in principle (source text is retained). | Code-review check: `DELETE FROM embeddings` appears exactly once, in the purge handler. |

**M2.5 exit:** users can freely switch embedder/model without losing old vectors, and can
explicitly reclaim space per-model via API/MCP/CLI with a byte-count receipt; purge is never
triggered automatically.

---

## M2.6 — Config, integration test, wiring  [ADR 0023/0024/0025]

**Goal:** embedder knobs fully plumbed through `.env`/`BridgeConfig`/`.env.example` (unguarded per
ADR 0022); a minimal fake-sidecar binary backs 1-2 true stdio-transport integration tests; confirm
the M1 watchdog story still holds (or explicitly extend it).

| # | Task | Verify |
|---|---|---|
| 2.6.1 | New `BridgeConfig` fields (`bridge.rs:676`) + `main.rs` env reads (mirroring the M1.2.8 pattern at `main.rs:295-302`): `WHATSRUST_EMBEDDER_CMD` (optional; absence = feature off, no drain worker spawned per 2.3.3), `WHATSRUST_EMBEDDER_ARGS`, `WHATSRUST_EMBEDDER_BATCH_SIZE` (default 64), `WHATSRUST_EMBEDDER_BACKOFF_CAP_SECS` (default 60), `WHATSRUST_EMBEDDER_LOADING_TIMEOUT_SECS` (default 60), `WHATSRUST_EMBEDDER_DRAIN_INTERVAL_SECS`. All **unguarded/benign** (ADR 0022/0023) — no `DANGEROUSLY_ALLOW_*` gate, no fail-closed startup validation beyond basic type parsing. | `.env` override test for at least one new knob (mirrors existing config tests in `main.rs`); absence of `WHATSRUST_EMBEDDER_CMD` leaves the feature off with no startup error. |
| 2.6.2 | `.env.example` additions: every new knob documented with default + one-line rationale, plus the multilingual-model recommendation (M2.4.5). | `.env.example` diff reviewed; every new `WHATSRUST_EMBEDDER_*` var has an entry. |
| 2.6.3 | Minimal fake-sidecar binary (ADR 0025 §4): a tiny separate `[[bin]]` (e.g. `src/bin/fake-embedder.rs`, ~20-40 lines) speaking `model_info`/`embed`/`health` over stdio with deterministic vectors, plus an env-toggled "misbehave" mode (wrong dim / wrong count / wrong model_id) for validation testing. | Binary builds; manual invocation echoes a canned `embed` response; misbehave mode produces the expected malformed response. |
| 2.6.4 | 1-2 true stdio-transport integration tests: spawn the fake-sidecar binary as a real child process via `StdioEmbedder`, exercise `model_info`/`embed`/`health` end-to-end, including one test that triggers "misbehave" mode to prove trust-but-verify validation (2.1.5) rejects it over a real process boundary (not just an in-process fake). | `cargo test` runs these as real subprocess tests; CI-safe (no live network, no live WA, no flaky timing dependence). |
| 2.6.5 | Watchdog/observability cross-check (ADR 0013): the existing storage-growth watchdog (`bridge.rs:2030-2115`) measures whole-file footprint, so `embeddings`-table growth is already implicitly covered (there is no `embed_failures` table — F-D is in-memory) — confirm this holds. Separately decide whether a `pending`-count-specific surfacing (e.g. a periodic log line or a `/api/status` field) is in scope for M2 or deferred, since ADR 0013 measures bytes, not embed-status backlog. | Decision recorded; if implemented, a test/log-format assertion; if deferred, noted here as a follow-up (non-blocking for M2 exit). |

**M2.6 exit:** embedder config is fully plumbed through `.env`/`BridgeConfig`/`.env.example`; a
real (non-fake, real-subprocess) stdio JSON-RPC round-trip is covered by CI-safe tests; the
watchdog/observability story for M2 is either extended or explicitly deferred with a recorded
reason.

---

## I-item / risk fold-in (design doc residual risks → where they land)

| Item | Lands in |
|---|---|
| R3 (set-difference / pathological-pending scale) | **M2.3.8** — circuit-breaker on backfill enqueue, not lockstep coupling |
| R4 (search latency / reader-pool) | **M2.4.3** — cosine rerank over ~50-200 candidates targets <5ms (ADR 0008); if measured latency under concurrent backfill exceeds target, ADR 0027's reader-pool option is the escape hatch (still deferred — only revisit if actually measured, not preemptively) |
| CJK-without-embedder lexical quality (M1's noted residual risk, design doc "Open risks") | **M2.4.5** — this is the problem M2's multilingual-model default exists to fix; no FTS5/trigram change needed if the embedder covers it |
| Media `directPath` lazy-hydration reliability (design doc "Open risks", unrelated to embeddings) | out of scope for M2 — an M1/ADR 0005 concern, not touched here |
| Search latency under concurrent backfill (design doc "Open risks", still open) | becomes a **measurement task** in M2.4 (informal — add a log/timing check when the semantic path lands; formalize only if it proves to matter) |

---

## Sequencing & gating

M2.1 → M2.2 → M2.3 → M2.4 → M2.5 → M2.6, each with tests alongside (ADR 0025). **Gate to the user
at each sub-milestone boundary** per session convention; implementation delegated to subagents
(orchestrate, don't implement).

**Callout:** unlike M1, M2 has **no schema migration** — F-D resolved to in-memory attempt
tracking, so `CURRENT_SCHEMA_VERSION` stays 8 and the staged-migration ceremony is entirely out of
scope. The riskiest work is therefore concentrated in the drain-worker resilience/concurrency
logic (M2.3.4–M2.3.8: backoff, per-row failure cap, loading-timeout, pathological-pending circuit
breaker) and the search-path fallback discipline (M2.4.3–M2.4.4). No single task carries M1.1-
caliber migration risk.

**Open decisions to confirm before coding M2.1:**
- **F-D** — ✅ DECIDED (2026-07-23): in-memory attempt tracking, no `embed_failures` table, no
  v8→v9 migration. (ADR 0017's "in-memory OR a table" resolves to in-memory for v1; no new ADR.)
- **F-C** — existing-backlog handling: tolerate (recommended) vs. one-time reclassify pass —
  confirm before M2.2.3 (→ needs ADR 0038+ to formalize).
- **F-G** — JSON-RPC framing module boundary (fresh structs in the new sidecar module vs. any
  shared abstraction with `mcp.rs`) — confirm before M2.1.3.
- **M2.4.2** — active-model resolution mechanism (derive from configured `Embedder`'s
  `model_info()`, no separate admin "set active model" state in v1) — confirm before M2.4 coding
  starts.
- **M2.4.6** — pure-semantic brute-force fallback: confirm deferral (recommended) rather than
  partial implementation.
- **M2.6.5** — `pending`-count observability: confirm in-scope-for-M2 vs. deferred before closing
  out M2.6.
