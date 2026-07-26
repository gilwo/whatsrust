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

**Revised:** 2026-07-23 — folded in the v1 cold design review (`_reviewer/design/2026-07-23-m2-execution-plan.md`,
*approve with changes*): B1 (circuit-breaker metric) + concerns 1–12.
**Revised again:** 2026-07-23 — folded in the v2 cold review (`…-m2-execution-plan-v2.md`, *needs rework*):
a new B1 (query-embed + sidecar concurrency), B2 (per-kind text-prep — resolved via **Option C**,
migration-free) + 7 non-blocking items.
**Revised (v3):** 2026-07-23 — folded in the v3 cold review (`…-m2-execution-plan-v3.md`, *approve with
changes*): B1 poison-pill bisection, B2 breaker no-op when no active model, B3 single shared embedder,
+ 7 non-blocking. M2's plan-level decisions (in-memory tracking, Option-C text-prep, backlog tolerance)
are now formalized in **ADR 0038**.
**Revised (v4):** 2026-07-23 — folded in the v4 cold review (`…-m2-execution-plan-v4.md`, *approve with
changes*): B1 recall-width no-op fix (a regression from the v2-#2 fold-in) + 8 non-blocking (max_batch
clamp, shutdown join barrier, purge no-op on pre-existing DBs, UTF-8-safe truncation, call-site fan-out,
stale-comment §0 finding **F-K**). See the four **Review fold-in** tables near the end.

> Written after reading the live post-M1 code (`src/storage.rs`, `src/bridge.rs`, `src/backfill.rs`,
> `src/api.rs`, `src/mcp.rs`) — not just the design sketch. Eleven wrinkles surfaced where the design
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
starting in-memory. (Corner case, review v2-#6: a daemon crash-looping every few seconds could retry
a genuinely-rejected row 1-2× per restart without ever reaching terminal `failed` — benign: a handful
of wasted sidecar calls, no data loss, and dominated by whatever is causing the crash-loop.) → revisit
only if measured.

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

### F-K — stale M1-era "deferred to M2" comments refer to a DIFFERENT "M2" (multi-worker backfill)
`bridge.rs:727-729`, `main.rs:327-331` (a runtime `warn!`), and `.env.example:82/117` promise that
**multi-worker backfill concurrency** lands in "M2" — but the roadmap scopes M2 exclusively to
semantic search, and ADR 0026 treats single-worker FIFO backfill as a deliberate architectural
decision, not a placeholder. This is a naming collision (review v4-#7) the §0 pass first missed.

**Recommended resolution:** this M2 (semantic search) does **not** deliver multi-worker backfill.
Reword those three comments to drop the "M2" promise (single-worker FIFO is the standing decision per
ADR 0026 unless a future milestone revisits it), or carry "multi-worker backfill" forward as an
explicitly-named future item. A one-line doc/comment fix — no code behavior change. → lands in M2.6.6.

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
| 2.1.4 | `StdioEmbedder`: spawn `WHATSRUST_EMBEDDER_CMD` (+ `WHATSRUST_EMBEDDER_ARGS`) via `tokio::process::Command`, write newline-delimited JSON-RPC requests to child stdin, read responses from child stdout via a buffered async line reader; call `model_info` once at construction and cache the result. **Construction is fallible + non-fatal (review #4):** a bad/missing command, an immediate child exit, or a `model_info` that errors/times out → return an `Err`/unhealthy `Embedder`, NEVER panic and NEVER block startup (embedder is unguarded/benign, ADR 0022; the 2.3.3 caller treats construction failure as "absent"). Bound the construction-time `model_info` call with a timeout. **Bound each stdout line read (review v3-#7):** a max-line-length guard → an oversized unbroken response is treated as a transport failure (cheap insurance; ADR 0024's sidecar is trusted, not adversarial, so low priority). | Real-subprocess round-trip test lands in **2.1.8** (not deferred to M2.6); plus a test pointing `WHATSRUST_EMBEDDER_CMD` at `/bin/false` (or a nonexistent path) → construction yields an unhealthy/absent embedder, no panic. |
| 2.1.5 | Trust-but-verify validation (ADR 0024): after every `embed` response, check `model_id` + `dim` match the cached `model_info()`, `vectors.len() == texts.len()`, each vector's length `== dim`. Any mismatch → typed transport-failure error (never stored). | Unit test feeds a fake malformed response (wrong dim / wrong count / wrong model_id) → `embed()` returns an error, nothing written. |
| 2.1.6 | `health()` semantics: `ok` / `loading` / `error(detail)` relayed faithfully by `StdioEmbedder` — timeout-tracking for "stuck loading" lives in the **drain worker** (M2.3), not the trait impl, per ADR 0015 B4. | Unit test: `StdioEmbedder::health()` against a fake sidecar reporting each of the 3 states relays each unchanged. |
| 2.1.7 | Fake `Embedder` (ADR 0025 seam 1): deterministic canned vectors (e.g. `vec![0.1 * i; dim]` per input text), in-process (no child spawn) — used by M2.3/M2.4 tests. | Trivial test: fake's `embed()` output shape matches input count and configured `dim`. |

| 2.1.8 | **Pull the real-subprocess smoke test into M2.1 (review #3):** land the minimal fake-sidecar binary (the `[[bin]]` of 2.6.3) now, plus one CI-safe real-subprocess round-trip through `StdioEmbedder` (spawn child → `model_info`/`embed`/`health`), so transport framing/buffering/lifecycle bugs surface at M2.1 — not four sub-milestones later at M2.6, after M2.2–M2.5 are built on it. M2.6 then only adds the "misbehave"-mode validation test. The fake-embedder `[[bin]]` is **test/dev-only** — release packaging ships only the primary `whatsrust` binary (single-binary ethos, review v2-#7); give it a `required-features`/test gate (or document `--bin whatsrust`) so the exclusion has a **mechanical backstop** once a release workflow exists (review v3-#6; none exists in-repo yet). | `cargo test` spawns the fake-sidecar child and round-trips all three methods over a real process boundary; green; the extra `[[bin]]` is gated out of the release artifact. |
| 2.1.9 | **`StdioEmbedder` concurrency + lifecycle contract (review v2-B1, #3):** the SAME child is called by the drain worker (batches of 64) and, after M2.4, by live search — over one stdin/stdout pipe pair. Serialize `embed`/`model_info`/`health` calls (internal `tokio::sync::Mutex` or a request-mailbox actor) so a search-time call can NEVER interleave/corrupt an in-flight drain batch's framing; document whether a search call **queues behind** an in-flight batch (simplest, may add latency during active drain) or preempts (more complex) — queue-behind is the recommended v1. Spawn the child with `kill_on_drop(true)` and terminate it when the `CancellationToken` fires (review v2-#3) so restarts don't leak orphaned sidecars. **Shutdown ordering (review v3-#1):** `main.rs` ends via `std::process::exit(0)` (`main.rs:1215-1227`, which skips destructors to dodge the blocking-stdin hang), so `kill_on_drop` alone is insufficient — the drain/embedder task MUST explicitly kill the child on `cancel`. Because `wait_stopped` only watches `BridgeState::Stopped` (set by the reconnect loop, `bridge.rs:2269`) and does NOT join the spawned worker tasks, add an explicit `join!` of the drain task's `JoinHandle` (short-timeout-bounded) to the shutdown sequence before `process::exit` — a barrier, not a scheduling race (review v3-#1/v4-#2). | Concurrency test: a drain batch and a `health()`/`embed()` call issued concurrently → responses correct and uncorrupted (serialized), neither times out; a dropped `StdioEmbedder` / fired cancel reaps the child (no orphan); an integration test triggers shutdown and asserts the child is reaped within the graceful window (via the join barrier). |

**M2.1 exit:** `Embedder` trait + real `StdioEmbedder` (child-process JSON-RPC, trust-but-verify,
faithful health relay, non-fatal construction, **serialized concurrent access + `kill_on_drop`**) + fake
`Embedder` seam all compile and pass unit tests, **including one real-subprocess round-trip (2.1.8)**.
No drain worker, no schema changes yet.

---

## M2.2 — Embeddable-text classification + set-difference work derivation  [ADR 0016/0017]

**Goal:** write-time classification at both ingest paths (NL → `pending`, else `skipped`), the
existing-backlog decision recorded, and the set-difference drain query implemented.

| # | Task | Verify |
|---|---|---|
| 2.2.1 | Settle **F-C** (part 1): add `InboundContent::embeddable_text(&self) -> Option<String>`, distinct from `display_text()` — bare NL only per ADR 0016 (`Text.body`; `Image`/`Video`/`Document` caption *only if present*, else `None`; `PollCreated` question+options joined; `Contact.display_name`; `Location.name` if present else `None`); `None` for every other kind (`Audio`, `Sticker`, `ReactionAdded`/`Removed`, `Edit`, `Revoke`, `PollVote`, `DeliveryReceipt` — `Edit` carries real text but ADR 0016 explicitly places it in "skipped"; don't relitigate). | Per-variant unit tests: image-with-caption → `Some(caption)` (not the `"[image ...]"` label); image-without-caption → `None`; sticker → `None` always; poll → `Some("question | opt1 | opt2")`-shaped text; edit → `None`. |
| 2.2.2 | Derive `embed_status` at write time (`"pending"` if `embeddable_text().is_some()`, else `"skipped"`); add it as an explicit bound param to `insert_message` (currently relies on the schema `DEFAULT`, `storage.rs:1258-1288`); wire both call sites: live ingest (`bridge.rs:2681-2686`) and `LiveBatchSink::persist_batch` (`bridge.rs:4263-4277`). **Real blast radius (review v4-#5):** the live path (`bridge.rs:2684`) goes through the `insert_inbound` wrapper (`storage.rs:1291-1301`, hardcodes `from_me=false, source='live'`, no `embed_status` param) — so `insert_inbound` must ALSO gain the param (or `bridge.rs:2684` bypasses it with a direct `insert_message`); its ~15 test call sites (`storage.rs`) then need a mechanical update. Add a **schema/code comment at the `embed_status` column def** noting it stays `'pending'` by design (the `embeddings` table is the source of truth for "done") so future readers don't misread it (review v4-#8). | Storage test: insert one row per `content_kind`, assert `messages.embed_status` matches the ADR 0016 table exactly (image+caption→pending, sticker→skipped, poll→pending, reaction→skipped, edit→skipped, receipt→skipped, captionless image→skipped). |
| 2.2.3 | Settle **F-C** (part 2, existing-backlog decision): **tolerate** — no reclassify migration; document the reasoning as a code comment at the classification call site (raw `wa::Message` isn't retained, so a perfect backlog reclassify isn't possible; backlog volume is small; ADR 0016's failure mode for stray non-NL text is bounded by the 3-attempt cap in M2.3). **Operational note (review #8):** because the backlog is untriaged, the one-time first drain sends the *whole* pre-M2 history to the sidecar — ~2× the NL-only volume ADR 0015's throughput math assumed (which counted on the ~40-60% non-NL skip) — so initial catch-up is slower than steady-state; not a correctness issue. | Decision recorded in this doc + the code comment; confirm no migration/backfill script is added for this. → **formalized in ADR 0038** (backlog tolerance). |
| 2.2.4 | Set-difference drain query (**F-J**): reuse the exact anti-join shape already smoke-tested in `validate_migration_post_commit` (`storage.rs:563-568`) as a new `Store::fetch_pending_embeddings(active_model_id, batch_size)`: `SELECT m.message_id, m.content_kind, m.body_text FROM messages m LEFT JOIN embeddings e ON m.message_id = e.message_id AND e.model_id = ?1 WHERE e.message_id IS NULL AND m.embed_status = 'pending' ORDER BY m.id LIMIT ?2`. **Selects `content_kind` (review v2-B2)** so the drain worker's text-prep (**2.3.9**) can branch per kind — `body_text` is the *decorated* `display_text()` label (F-C), and for the payload-inside-bracket kinds (location/contact/poll) a blind prefix-strip yields empty/noise, so 2.3.9 must know the kind. `body_text` itself stays the FTS-indexed label (untouched). | Storage test seeds mixed `embed_status` + partial `embeddings` rows for model A; query returns exactly the pending-and-unvectored-for-A set (with `content_kind`), excludes `skipped`/`failed` and already-vectored rows. |

| 2.2.5 | **Index for the anti-join (review #1, ADR 0031 non-versioned):** add `CREATE INDEX IF NOT EXISTS idx_messages_embed_status ON messages(embed_status, id)` at startup (idempotent, OUTSIDE the schema-version system per ADR 0031 — no version bump). Both `fetch_pending_embeddings` (2.2.4) and the circuit-breaker count (2.3.8) filter `embed_status='pending'` over an indefinitely-retained (ADR 0012) table; without an index they full-scan, cost growing unboundedly with account age. | `EXPLAIN QUERY PLAN` for both the drain query and the breaker count shows the index used (SEARCH, not SCAN) — mirrors M1.3's `test_fts_explain_query_plan` discipline. |

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
| 2.3.1 | **F-D (decided: in-memory — formalized in ADR 0038).** Add a process-local attempt tracker owned by the drain worker — `HashMap<(String /*message_id*/, String /*model_id*/), u8>` (or `DashMap` if touched off-task) keyed by `(message_id, active_model_id)`. **No schema change, no migration** (`CURRENT_SCHEMA_VERSION` stays 8). Attempts reset on restart by design (F-D tradeoff). **Evict an entry once its row reaches terminal `failed`** (review v3-#4) so the map can't grow unbounded over the daemon's life. | Unit test: incrementing the same key 3× trips the cap on the 3rd; a distinct key is independent; a key is removed after its row hits `failed`; state is scoped to the worker (grep confirms no new table / no `CURRENT_SCHEMA_VERSION` bump / no DB write for attempts). |
| 2.3.2 | Add `embed_notify: Arc<tokio::sync::Notify>` to `WhatsAppBridge` (alongside `backfill_notify`, `bridge.rs:828/863`), exposed via an accessor mirroring `backfill_notify()` (`:1284-1286`). Call `.notify_one()` directly from the two M2.2 write-time classification call sites (`:2681`, `:4263`) whenever a row lands `pending` (**F-E**). | Test/inspection confirms `Notify` fires on a pending-classified insert; no change to `outbound_notify`/`backfill_notify` behavior. |
| 2.3.3 | Spawn the drain worker as an **independent long-lived task** in `WhatsAppBridge::start()`'s `run_bridge`, alongside the prune/watchdog block (`bridge.rs:2030-2115`) and the backfill-worker block (`:2117-2153`) — **not** inside `run_bot_session`/the reconnect loop (`:2162+`) — sharing `cancel: CancellationToken` (**F-E**, ADR 0015 B3). **No embedder configured → don't spawn the task at all** (ADR 0015 M2); **configured-but-broken → spawn, but the worker treats a failed/unhealthy embedder as "absent" and idles** (review #4 — never blocks or crashes startup). Also patch ADR 0015's stale top-level text ("worker IDLES entirely") to match its own B3 hardening ("don't spawn") — doc-consistency, mirrors the F-A ADR-0008 patch (review #12). | With no `WHATSRUST_EMBEDDER_CMD` → never spawned; with a valid CMD → spawns alongside prune/backfill; with a **broken** CMD (e.g. `/bin/false`) → daemon starts normally, worker idles, no panic. |
| 2.3.4 | Drain loop core: `Notify`-woken + periodic timer (`WHATSRUST_EMBEDDER_DRAIN_INTERVAL_SECS`, **default 60**), fetch a batch via `fetch_pending_embeddings` (M2.2.4); the batch size = `min(WHATSRUST_EMBEDDER_BATCH_SIZE` (default 64)`, model_info().max_batch)` when the sidecar advertises `max_batch` — ADR 0024 says "the bridge respects it"; review v4-#1, else an over-large batch reads as a whole-batch failure and thrashes 2.3.11's bisection. Run each row's `(content_kind, body_text)` through the kind-gated text-prep step (**2.3.9**) → `Embedder::embed(texts)`, on success write the batch's vectors in **one chunked transaction** (`INSERT OR REPLACE INTO embeddings (message_id, model_id, dim, vec)` for all rows in the batch, per ADR 0027's chunked-write pattern — one lock acquisition, not 64; a partial-batch crash is harmless since set-difference re-derives — review v2-#4). `messages.embed_status` stays `'pending'` — the `embeddings` table itself is the source of truth for "done" (ADR 0017; no `'done'` value needed/used). | Worker test (fake `Embedder`): a batch of N pending rows → N `embeddings` rows appear with correct `model_id`/`dim`; re-running the set-difference query for the same model returns empty; a fake sidecar advertising a small `max_batch` → the effective batch is clamped (no spurious whole-batch failures). |
| 2.3.5 | Sidecar-down resilience: transport failure → exponential backoff (cap 60s via `WHATSRUST_EMBEDDER_BACKOFF_CAP_SECS`), rows STAY `pending`, **no** attempt-counter increment; after N consecutive backoff cycles, drop to Notify-only (stop periodic polling until a new pending row wakes it) — ADR 0015 M2 hardening. | Worker test: fake `Embedder` configured to always transport-fail → rows remain `pending` after several loop iterations; backoff grows then caps; the in-memory attempt map stays empty (transport failures don't count against the content-rejection cap). **Repeated failure of the *same* batch triggers bisection (2.3.11), not endless same-batch retry.** |
| 2.3.6 | Per-row content rejection: a well-formed response that fails **per-row** (not whole-batch) validation increments that row's in-memory attempt count (F-D); cap 3 → `messages.embed_status='failed'` (terminal, drops out of the set-difference query). The sidecar protocol is batch-only (ADR 0024, no per-item rejection signal), so a whole-batch failure is first treated as a transport failure (2.3.5); **isolating the single bad row is done by the bisection loop (2.3.11)** — which shrinks the batch until the offender is alone and this per-row cap-3 can fire (this replaces the earlier "document the limitation" hand-wave — review v3-B1). | Worker test: fake `Embedder` rejects one specific text 3 times in solo-batches → row flips to `failed` on the 3rd attempt and disappears from subsequent set-difference results (end-to-end poison-pill isolation is 2.3.11's test). |
| 2.3.7 | Loading-timeout (ADR 0015 B4): track **continuous** time in `health()==loading` — reset the timer on any non-`loading` observation (review #5: the ADRs say "continuous", NOT cumulative-across-restarts, so a sidecar that briefly reloads many times is never falsely condemned); `>60s` continuous (`WHATSRUST_EMBEDDER_LOADING_TIMEOUT_SECS`) → treat as `error` for this cycle → skip embedding this tick (rows stay `pending`; FTS5 fallback keeps serving search uninterrupted). A loading-timeout "treat-as-error" **counts toward the same backoff / Notify-only accounting as 2.3.5** (review #6) so a permanently-`loading` sidecar quiesces to Notify-only instead of polling at full cadence forever. | Worker test: (a) fake reports `loading` continuously past 60s (injected clock) → treat-as-error, worker does not block; (b) fake alternates `loading`/`ok` → timer resets, never condemned; (c) repeated loading-timeouts drive it to Notify-only like other persistent failures. |
| 2.3.8 | Pathological-pending circuit breaker (fork R3) — **must count the active-model set-difference, NOT raw `embed_status='pending'`** (review B1). Because `embed_status` stays `'pending'` for life (2.3.4), a raw `pending` count never decreases and would permanently block enqueue once lifetime embeddable messages cross 100k (exactly what M1 `all`-backfill produces). Count the anti-join instead — **bounded** so it stays ms-short in the enqueue closure (ADR 0027; review v3-#3): `SELECT COUNT(*) FROM (SELECT 1 FROM messages m LEFT JOIN embeddings e ON m.message_id = e.message_id AND e.model_id = ?1 WHERE e.message_id IS NULL AND m.embed_status = 'pending' LIMIT 100001)` (same anti-join as 2.2.4, uses the 2.2.5 index; the `LIMIT` caps the scan at threshold+1 rows regardless of true backlog). **No active model → NO-OP (review v3-B2):** if `active_model_id` is `None` (no embedder configured, or `model_info()` never resolved), SKIP the breaker entirely — enqueue proceeds exactly as in M1. NEVER bind an empty/placeholder `?1`: that would collapse the anti-join to a raw lifetime-pending count (the very v1-B1 bug) and block backfill when M2 is *off*, violating the additive/no-blocking contract. If `> 100_000` (test-overridable), surface a flag the backfill **enqueue** path checks (`api.rs`/`storage.rs`) → reject new backfill jobs with a structured error until the *active-model backlog* drains back under threshold. Must not throttle the backfill *worker* (running jobs continue) or lexical search. Note: switching to a fresh model on a large history (2.5.1) **or purging the active model (2.5.2)** legitimately re-trips this until re-drain — expected, not a bug (review v2-#5). **Compose the check INTO the existing atomic enqueue closure (review v2-#1):** `enqueue_backfill_job` (`storage.rs:1672-1764`) already runs one `unchecked_transaction` checking active-job/cooldown/queue-depth/clamp (ADR 0035 B5); thread `active_model_id` in and add the anti-join count as one more atomic step + a new `EnqueueOutcome` variant, rather than a separate race-prone check beside it. (Mechanical: this changes `enqueue_backfill_job`'s signature → ~25 existing test call sites in `storage.rs`/`backfill.rs` need updating — friction, not risk; review v4-#6.) | Tests: (a) seed 100k+ `pending` rows that ALL already have active-model vectors → breaker does **NOT** trip (proves it's not a raw count); (b) seed >threshold pending-and-unvectored rows → breaker trips, enqueue rejected, running job unaffected, and once drained enqueue succeeds; (c) `EXPLAIN QUERY PLAN` shows the count uses `idx_messages_embed_status` (2.2.5), not a full scan; (d) **no `WHATSRUST_EMBEDDER_CMD` set + 150k lifetime `pending` rows → enqueue never checks/trips the breaker** (B2 no-op). |

| 2.3.9 | **Drain-worker text preparation — kind-gated, Option C (review v2-B2, #9, #11):** derive the text to embed from `(content_kind, body_text)` (both from 2.2.4), branching on kind — never a blind strip-after-`]`, which yields **empty strings** for location/contact and **noise** for poll (payload is *inside* the brackets — `bridge.rs:473-484`): **`text`** → `body_text` as-is; **`image`/`video`/`document`** (caption trails the `]`) → strip the `"[… ] "` prefix to the caption; **`location`/`contact`/`poll`** → embed the decorated `body_text` **as-is** (the label still carries the name/question; mild bracket/coord/`(pick N)` noise accepted on these rare kinds — migration-free, honors F-D). Then truncate to `model_info().max_input_tokens` when advertised (ADR 0024), **char-boundary-safe** — approximate token length by word/char count and cut on a `char` boundary (`str::floor_char_boundary` or `char_indices()`), NEVER a raw `&text[0..n]` byte slice (panics mid-UTF-8; Hebrew/Arabic/CJK are multi-byte and are this project's actual data — review v4-#4). Kind-gating also protects a genuine `text` starting with `"["` (e.g. `"[URGENT] call me"`) from being wrongly stripped. **Future (deferred, noted per user):** *Option A* — persist a bare `embed_text` column at write time from `embeddable_text()` (exact for all kinds, no format-coupling) — a clean v8→v9 additive migration if embedding quality on location/contact/poll ever warrants it; → formalized as the deferred alternative in **ADR 0038**. | Unit tests, one fixture **per kind**: `text` unchanged (incl. leading-`[` text); `image`+caption → bare caption; **`location` → non-empty (name present), not empty**; **`contact` → non-empty**; **`poll` → contains question + options**; over-long → truncated at the advertised limit; **a long Hebrew/Arabic/CJK string truncated near the limit → no panic, no corrupted trailing bytes**; no `max_input_tokens` → no truncation. |

| 2.3.10 | **Single shared embedder construction + wiring (review v3-B3):** construct exactly ONE `Arc<dyn Embedder>` in `WhatsAppBridge::start()` (mirroring how `store`/`event_tx` are built once at `bridge.rs:874-896` and shared into the reconnect loop + independent worker tasks), store it as a bridge field, and hand the SAME instance to both the drain-worker spawn (2.3.3) and the `search()` accessor (2.4.3). NEVER construct a second `StdioEmbedder` per call site — that spawns a second child and defeats 2.1.9's serialization + doubles model memory. Non-fatal construction per 2.1.4 (broken/absent → `None` → drain worker not spawned per 2.3.3, search falls back to lexical per 2.4.4). | Test/grep: exactly one `Arc<dyn Embedder>` bridge field and one construction site (not one per call site); a broken CMD yields `None` and the daemon still starts. |
| 2.3.11 | **Poison-pill batch bisection (review v3-B1):** `embed` is batch-only (ADR 0024, no per-item error), so one un-embeddable row in a batch of 64 errors the whole call → 2.3.5 treats it as transport-failure → the deterministic oldest-first `fetch_pending_embeddings` (`ORDER BY m.id`) re-selects the SAME batch every cycle = **livelock** (the offender never reaches `failed`; no later row ever drains; 2.3.8 eventually trips account-wide). Fix: after **K consecutive whole-batch failures of the same message-id set**, halve the batch and retry, converging to solo-batches so 2.3.6's per-row cap-3 engages and retires the offender to `failed` — the innocent rows drain meanwhile. Pure whatsrust-side retry; no ADR 0024 change. (Formalized in ADR 0038.) | Worker test: a batch of 64 with ONE poisoned text → the other 63 drain within N cycles AND the poisoned row reaches `failed` within 3 solo attempts (proves no livelock). |

**M2.3 exit:** the drain worker runs independently of WA connection state, never blocks FTS5
search, never leaves a row `failed` without 3 genuine content-rejection attempts (transport
outages never count against that cap), **a single poison-pill message can't livelock the drain**
(bisection, 2.3.11), and the pathological-pending case throttles only new backfill *enqueues*
(and only when an embedder is active — 2.3.8), never in-flight jobs.

---

## M2.4 — Semantic search path  [ADR 0007/0008/0017/0018]

**Goal:** layer query-embed → FTS5-recall → active-model vector fetch → Rust cosine rerank,
orchestrated above `Store` (a new bridge-level `search()`), preserving the exact M1.3 lexical path
as the no-sidecar/no-vectors fallback.

| # | Task | Verify |
|---|---|---|
| 2.4.1 | Cosine similarity as a pure function (dot product / magnitudes; error on mismatched `dim`; defined behavior for a zero-magnitude vector). | Unit tests (ADR 0025 pure-logic layer): orthogonal → 0, identical → 1, mismatched dim → `Err`, zero vector → defined (not NaN/panic). |
| 2.4.2 | Decide + document **active-model resolution**: the active model is whatever the currently-configured `Embedder`'s `model_info().model_id` reports at startup — no separate persisted "switch" state in v1 (switching models = restarting with a different `WHATSRUST_EMBEDDER_CMD`). This resolves ADR 0017's "one model active at a time" into a concrete mechanism. | Decision recorded in this doc; a config/wiring test confirms the active-model id is read from `model_info()`, not a separate config knob. |
| 2.4.3 | **Orchestrate the semantic path in a new bridge-level `WhatsAppBridge::search()` (review v2-B1)** — NOT in `Store`, which owns no `Embedder` (it's held by the bridge / drain worker, 2.3.2-3). Composition, when `query=Some(q)` AND the embedder is configured+healthy AND ≥1 vector exists for the active model: **(1) embed the query** — `Embedder::embed(&[q])` with its own timeout (this is the missing second cosine operand — without it there is nothing to rerank against); **(2)** run the M1.3 FTS5 MATCH+rank recall (`Store`), internally widened to `recall_width = max(200, requested_limit)` candidates — a fixed ~200-candidate recall floor **decoupled from** the final `limit` (review v4-B1; the earlier `min(200, limit)` was a no-op since `api.rs`/`mcp.rs` clamp `limit` to ≤200, so it never actually widened the common small-`limit` query — MCP default is 20 — collapsing rerank into "reorder BM25's top-20"), so semantic rerank can surface messages BM25 ranked *outside* the caller's top-N; **(3)** fetch candidate vectors `(message_id IN (…), model_id=active)` (`Store`); **(4)** cosine-rerank in Rust against the query vector; **(5)** truncate to `limit` → `InboundRow`-shaped rows (**F-H**: order changes, shape doesn't). **Additive-only (review #2):** candidates lacking an active-model vector (routine during drain catch-up) are NOT dropped — they keep their FTS-rank position, appended after the reranked vectorized subset. | Storage/bridge tests (seeded vectors + fake `Embedder`): (a) all vectored → cosine order differs from BM25; (b) **partial coverage** → unvectored candidates survive after the reranked subset; (c) **at `limit=10` the FTS recall requested a materially wider candidate set (≥200) than 10** — proving real widening, not just reorder; (d) rows use the exact M1.3 fields. |
| 2.4.4 | No-sidecar / degraded fallback: any of {no embedder configured, sidecar unhealthy, **the live query-embed call (2.4.3 step 1) itself times out or fails even though cached `health()`==ok** — review v2-B1, zero vectors for active model, all candidate vectors missing} → return the **verbatim** M1.3 FTS5/BM25 path, unmodified, no error, negligible added latency. | Regression tests: (a) zero rows in `embeddings` → M1.3 assertions identical against the M2 path; (b) a fake `Embedder` whose query-embed call fails → search still returns the lexical result, no error surfaced. |
| 2.4.5 | Multilingual default (ADR 0018): document the recommended `WHATSRUST_EMBEDDER_CMD`/model choice as a multilingual model (e.g. an e5/bge-m3-class multilingual model) in `.env.example`, with a one-line rationale (CJK/Thai semantic coverage where `unicode61` FTS5 degrades). whatsrust itself stays language-neutral — no code branches on language. | `.env.example` entry present recommending a multilingual model + the rationale. |
| 2.4.6 | Pure-semantic brute-force fallback (FTS5 zero-hit, embeddings exist) — ADR 0007/0008 "Future" item: **defer** past M2 (record explicitly, do not half-implement). | Decision recorded in this doc; no code path added for this in M2. |
| 2.4.7 | **Correction (review v2-B1, refined v3-#2):** the response *shape* is preserved, but the *call path* changes in **`api.rs` only** — `api.rs::handle_search` (`api.rs:950`) currently calls `bridge.store().search_inbound(...)` directly; it must switch to the new `bridge.search(...)` (2.4.3). **`mcp.rs` needs ZERO change:** its `whatsrust_search` tool (`mcp.rs:287-291`) is a blocking-HTTP proxy to `/api/search`, so it inherits the new behavior over HTTP automatically — don't hunt for an mcp.rs call site. Any optional debug field (e.g. `matched_by: "lexical"\|"semantic"`) must be additive so old clients ignore it. | `api.rs` search-*shape* tests pass unmodified with the call site now targeting `bridge.search`; `mcp.rs` unchanged; if an optional field is added, a new test asserts its presence/absence per mode. |

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
| 2.5.2 | Explicit per-model purge: `DELETE FROM embeddings WHERE model_id = ?` followed by a bounded `PRAGMA incremental_vacuum` loop (**not** full `VACUUM`, per ADR 0017 R-prior5 — mirrors the existing prune/watchdog's non-blocking ethos). Expose via a new API endpoint (e.g. `POST /api/embeddings/purge`), an MCP tool, and a CLI subcommand; returns `{model_id, rows_deleted, bytes_reclaimed}`. **`bytes_reclaimed` honesty (review #7):** compute it from an actual on-disk file-size stat *before/after* (the same 3-file footprint the watchdog uses), NEVER assume the vacuum freed anything — `incremental_vacuum` is a silent no-op unless `PRAGMA auto_vacuum` is `INCREMENTAL` (`=2`), only true if set before any table existed on the file (cf. `storage.rs:1479`'s best-effort caveat). At purge time also read `PRAGMA auto_vacuum` once and WARN if it isn't `2`, so a stuck non-incremental DB produces a diagnosable log instead of an unexplained `0 bytes reclaimed`. Purging the *active* model re-trips the M2.3.8 breaker until re-drain — expected (review v2-#5). **Remediation for a stuck DB (review v4-#3):** `auto_vacuum=INCREMENTAL` takes effect only on a still-empty file, so any DB that had tables before that pragma shipped (plausibly this project's own, lineage v0) is permanently `NONE` → `incremental_vacuum` is a *permanent* no-op and "reclaims space" silently fails. Either document that space-reclaim holds only for post-pragma DBs, or add an explicit opt-in one-time `--vacuum-once` maintenance flag (full `VACUUM`, clearly labeled as locking — the one sanctioned exception to ADR 0017 R-prior5). | Storage test: purge model A → A rows gone, B rows untouched, `rows_deleted` matches; a **pre-created-tables fixture** (tables made before `Store` opens → `auto_vacuum` stuck `NONE`) exercises the real no-op path and asserts the WARN fires + `bytes_reclaimed=0` reported honestly (measured, not assumed); a fresh temp DB (INCREMENTAL) reclaims a measured before/after delta; API/MCP/CLI round-trip test (fake bridge) confirms the same triple across all three surfaces. |
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
| 2.6.1 | New `BridgeConfig` fields (`bridge.rs:676`) + `main.rs` env reads (mirroring the M1.2.8 pattern at `main.rs:295-302`): `WHATSRUST_EMBEDDER_CMD` (optional; absence = feature off, no drain worker spawned per 2.3.3), `WHATSRUST_EMBEDDER_ARGS`, `WHATSRUST_EMBEDDER_BATCH_SIZE` (default 64), `WHATSRUST_EMBEDDER_BACKOFF_CAP_SECS` (default 60), `WHATSRUST_EMBEDDER_LOADING_TIMEOUT_SECS` (default 60), `WHATSRUST_EMBEDDER_DRAIN_INTERVAL_SECS` (default 60). All **unguarded/benign** (ADR 0022/0023) — no `DANGEROUSLY_ALLOW_*` gate, no fail-closed startup validation beyond basic type parsing. | `.env` override test for at least one new knob (mirrors existing config tests in `main.rs`); absence of `WHATSRUST_EMBEDDER_CMD` leaves the feature off with no startup error. |
| 2.6.2 | `.env.example` additions: every new knob documented with default + one-line rationale, plus the multilingual-model recommendation (M2.4.5). | `.env.example` diff reviewed; every new `WHATSRUST_EMBEDDER_*` var has an entry. |
| 2.6.3 | Extend the fake-sidecar binary (landed in **2.1.8** as `src/bin/fake-embedder.rs`) with an env-toggled "misbehave" mode (wrong dim / wrong count / wrong model_id) for validation testing. (The base binary + happy-path round-trip already exist from 2.1.8 — review #3.) | Misbehave mode produces the expected malformed response on demand. |
| 2.6.4 | Add the "misbehave"-mode integration test: spawn the fake-sidecar (misbehave on) as a real child via `StdioEmbedder` → prove trust-but-verify (2.1.5) rejects the malformed batch over a real process boundary (the happy-path real-subprocess round-trip already lands in 2.1.8). | `cargo test` runs it as a real subprocess test; CI-safe (no live network, no live WA, no flaky timing). |
| 2.6.5 | Watchdog/observability cross-check (ADR 0013): the existing storage-growth watchdog (`bridge.rs:2030-2115`) measures whole-file footprint, so `embeddings`-table growth is already implicitly covered (there is no `embed_failures` table — F-D is in-memory) — confirm this holds. Separately decide whether a `pending`-count-specific surfacing (e.g. a periodic log line or a `/api/status` field) is in scope for M2 or deferred, since ADR 0013 measures bytes, not embed-status backlog. | Decision recorded; if implemented, a test/log-format assertion; if deferred, noted here as a follow-up (non-blocking for M2 exit). |
| 2.6.6 | **Reword the stale "deferred to M2" multi-worker-backfill comments (F-K, review v4-#7):** `bridge.rs:727-729`, `main.rs:327-331` (runtime `warn!`), `.env.example:82/117` — drop the "M2" promise (semantic-search M2 does NOT deliver multi-worker backfill; single-worker FIFO stands per ADR 0026) or name it as a future item. Doc/comment-only, no behavior change. | Grep shows no remaining comment implying multi-worker backfill lands in *this* M2. |

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

## Review fold-in (2026-07-23 cold design review → where each finding lands)

Cold review verdict: **approve with changes** (`_reviewer/design/2026-07-23-m2-execution-plan.md`). All §0
Findings independently code-verified. One blocking issue + 12 non-blocking, folded in as follows:

| Review finding | Lands in |
|---|---|
| **B1** — circuit-breaker counted raw `pending` (never decreases) → permanent enqueue block past 100k lifetime msgs | **M2.3.8** — count the active-model anti-join instead; +tests (a)/(b)/(c) |
| #1 — no `embed_status` index → full scans of an indefinitely-retained table | **M2.2.5** — `idx_messages_embed_status`, non-versioned (ADR 0031); +EXPLAIN checks in 2.2.5/2.3.8 |
| #2 — partial vector coverage silently drops recent lexical hits | **M2.4.3** — additive-only rerank; unvectored candidates keep FTS position; +partial-coverage test |
| #3 — real `StdioEmbedder` only tested at M2.6 | **M2.1.8** — pull fake-sidecar binary + real round-trip into M2.1; M2.6.3/6.4 keep only misbehave-mode |
| #4 — misconfigured (not absent) embedder unhandled at startup | **M2.1.4** (non-fatal construction) + **M2.3.3** (broken-CMD test) |
| #5 — loading-timeout "cumulative" contradicts ADRs' "continuous" | **M2.3.7** — continuous (resets on non-loading); +alternating test |
| #6 — loading-timeout vs backoff/notify-only interaction unspecified | **M2.3.7** — folds into the same backoff/notify-only accounting |
| #7 — `incremental_vacuum` no-op ⇒ dishonest `bytes_reclaimed` | **M2.5.2** — real before/after file-size stat + `auto_vacuum` WARN |
| #8 — F-C backlog ⇒ ~2× first-drain volume vs throughput math | **M2.2.3** — operational note (one-time, not a correctness bug) |
| #9 — M2.2.4's bracket-strip parenthetical had no task | **M2.3.9** — explicit text-preparation task + Verify |
| #10 — `DRAIN_INTERVAL_SECS` had no default | **M2.3.4 / M2.6.1** — default 60 |
| #11 — no `max_input_tokens` truncation | **M2.3.9** — truncate in text-prep when advertised |
| #12 — ADR 0015 stale top-level text | **M2.3.3** — patch ADR 0015 (doc-consistency, like the F-A ADR-0008 patch) |

### v2 cold review (`…-m2-execution-plan-v2.md`, *needs rework* → resolved) — where each finding lands

| Review finding | Lands in |
|---|---|
| **v2-B1** — no query-embed (cosine had no 2nd operand); orchestration owner; live-embed failure; sidecar concurrency | **M2.4.3** (query-embed + bridge-level `search()` + recall floor), **M2.4.4** (live-embed-fail fallback), **M2.4.7** (call-path correction), **M2.1.9** (serialize + `kill_on_drop`) |
| **v2-B2** — bracket-strip empty/noise for location/contact/poll; SQL lacked `content_kind` | **M2.2.4** (SELECT `content_kind`) + **M2.3.9** (kind-gated, **Option C** as-is; Option A column noted deferred) |
| v2-#1 — breaker check race beside the atomic enqueue closure | **M2.3.8** — compose into `enqueue_backfill_job`'s TX + new `EnqueueOutcome` variant |
| v2-#2 — recall width could undercut lexical for large `limit` | **M2.4.3** — `recall_width >= min(200, limit)` |
| v2-#3 — sidecar child orphan on shutdown | **M2.1.9** — `kill_on_drop` + cancel-token termination |
| v2-#4 — drain batch write not one TX | **M2.3.4** — one chunked transaction per batch (ADR 0027) |
| v2-#5 — active-model purge re-trips breaker | **M2.3.8 / M2.5.2** — noted as expected |
| v2-#6 — in-memory attempts crash-loop corner | **F-D** — noted benign |
| v2-#7 — fake-embedder second `[[bin]]` vs single-binary ethos | **M2.1.8** — test/dev-only, not in the release artifact |

### v3 cold review (`…-m2-execution-plan-v3.md`, *approve with changes* → resolved) — where each finding lands

| Review finding | Lands in |
|---|---|
| **v3-B1** — poison-pill livelock (batch-only protocol + deterministic re-select) | **M2.3.11** — bisect-to-solo so cap-3 engages; M2.3.5/2.3.6 updated |
| **v3-B2** — breaker collapses to raw count when no active model | **M2.3.8** — explicit no-op when `active_model_id` is `None` + Verify (d) |
| **v3-B3** — no task builds a single shared embedder | **M2.3.10** — one `Arc<dyn Embedder>` in `start()`, shared into drain + search |
| v3-#1 — shutdown race (`process::exit` skips `kill_on_drop`) | **M2.1.9** — explicit child-kill on cancel before the graceful window + test |
| v3-#2 — M2.4.7 wrongly said `mcp.rs` changes | **M2.4.7** — corrected: `api.rs` only; `mcp.rs` inherits over HTTP |
| v3-#3 — unbounded breaker `COUNT(*)` | **M2.3.8** — `LIMIT threshold+1` bounded scan |
| v3-#4 — in-memory attempt map never evicts | **M2.3.1** — evict on terminal `failed` |
| v3-#5 — M2 decisions recorded only in the plan | **ADR 0038** (new) — in-memory tracking, Option-C text-prep, backlog tolerance |
| v3-#6 — fake-embedder release exclusion has no backstop | **M2.1.8** — `required-features`/test gate |
| v3-#7 — unbounded sidecar stdout line read | **M2.1.4** — max-line-length guard → transport failure |

### v4 cold review (`…-m2-execution-plan-v4.md`, *approve with changes* → resolved) — where each finding lands

| Review finding | Lands in |
|---|---|
| **v4-B1** — `recall_width = min(200,limit)` was a no-op (regression from v2-#2) → no pool-widening | **M2.4.3** — `max(200, limit)`; Verify (c) now tests at `limit=10` |
| v4-#1 — no clamp to `model_info().max_batch` → bisection thrash | **M2.3.4** — `min(batch_size, max_batch)` + fake-sidecar test |
| v4-#2 — shutdown kill isn't a barrier (`wait_stopped` doesn't join the task) | **M2.1.9** — explicit `join!` of the drain `JoinHandle` before `process::exit` |
| v4-#3 — `incremental_vacuum` permanent no-op on pre-existing DBs | **M2.5.2** — real no-op test fixture + documented remediation (`--vacuum-once`) |
| v4-#4 — UTF-8 truncation panic risk on Hebrew/Arabic/CJK | **M2.3.9** — char-boundary-safe truncation + multibyte test |
| v4-#5 — `insert_inbound` wrapper hides the real live call site (~15 tests) | **M2.2.2** — call out `insert_inbound` blast radius |
| v4-#6 — `enqueue_backfill_job` signature change touches ~25 tests | **M2.3.8** — noted as mechanical friction |
| v4-#7 — stale M1 "deferred to M2" multi-worker-backfill comments | **§0 F-K** + **M2.6.6** — reword; single-worker FIFO stands (ADR 0026) |
| v4-#8 — `embed_status='pending'` is permanently stale by design | **M2.2.2** — schema/code comment at the column def |

---

## Sequencing & gating

M2.1 → M2.2 → M2.3 → M2.4 → M2.5 → M2.6, each with tests alongside (ADR 0025). **Gate to the user
at each sub-milestone boundary** per session convention; implementation delegated to subagents
(orchestrate, don't implement).

**Callout:** unlike M1, M2 has **no schema migration** — F-D resolved to in-memory attempt
tracking, so `CURRENT_SCHEMA_VERSION` stays 8 and the staged-migration ceremony is entirely out of
scope. The riskiest work is therefore concentrated in the drain-worker resilience/concurrency
logic (M2.3.4–M2.3.8: backoff, per-row failure cap, loading-timeout, pathological-pending circuit
breaker), the **sidecar concurrency contract (M2.1.9) + query-embed orchestration (M2.4.3)** surfaced
by the v2 review, and the search-path fallback discipline (M2.4.3–M2.4.4). No single task carries
M1.1-caliber migration risk.

**Open decisions to confirm before coding M2.1:**
- **F-D** — ✅ DECIDED (2026-07-23): in-memory attempt tracking, no `embed_failures` table, no
  v8→v9 migration. (ADR 0017's "in-memory OR a table" resolves to in-memory for v1 — **formalized in
  ADR 0038**.)
- **B2 (v2)** — ✅ DECIDED (2026-07-23): drain text-prep is **kind-gated, Option C** (embed the
  decorated label as-is for location/contact/poll) — migration-free (M2.3.9). *Option A* (persist a
  bare `embed_text` column, v8→v9) is the deferred future improvement (→ **ADR 0038**).
- **F-C** — ✅ DECIDED (2026-07-23): tolerate the pre-M2 backlog (no reclassify pass) — **formalized
  in ADR 0038**.
- **F-G** — JSON-RPC framing module boundary (fresh structs in the new sidecar module vs. any
  shared abstraction with `mcp.rs`) — confirm before M2.1.3.
- **M2.4.2** — active-model resolution mechanism (derive from configured `Embedder`'s
  `model_info()`, no separate admin "set active model" state in v1) — confirm before M2.4 coding
  starts.
- **M2.4.6** — pure-semantic brute-force fallback: confirm deferral (recommended) rather than
  partial implementation.
- **M2.6.5** — `pending`-count observability: confirm in-scope-for-M2 vs. deferred before closing
  out M2.6.
