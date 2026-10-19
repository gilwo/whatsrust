# whatsrust

Pure Rust WhatsApp bridge. Single binary, no Node.js. (Experimental, feature-rich fork.)

## Documentation Matrix
Where to look for what — read the relevant doc before changing related code.

| Topic | Doc |
|---|---|
| Project overview, conventions, key files | `CLAUDE.md` (this file) |
| System architecture, data flow, design decisions | `ARCHITECTURE.md` |
| User-facing features, install, API/MCP overview | `README.md` |
| Contributing workflow | `CONTRIBUTING.md` |
| Operational playbook (build, configure, run, CLI/MCP usage, DB migration recovery, test/lint) | `docs/HOWTO.md` |
| **Architecture Decision Records (the "why" ledger)** | `docs/adr/` — start at `docs/adr/0000-index.md` |
| **Feature roadmap & status (LIVE — what's done/in-flight/planned)** | `docs/plans/FEATURES.md` |
| **Implementation execution plan (phases, gates, milestone exit criteria)** | `docs/plans/IMPLEMENTATION-ROADMAP.md` |
| Design specs / plans (the "what/how" blueprints) | `docs/plans/*.md` |
| Design review reports (cold reviews, reconciliation) | `_reviewer/design/` |
| Historical fetch + lexical search (M1 — **DONE** 2026-07-20) / semantic search (M2 — **DONE**: code-complete 2026-09-30, live-validated 2026-10-05) | `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md` (+ ADRs 0001–0047) |
| MCP Streamable HTTP listener (F3 — **DONE** 2026-10-19; dual-era listener, opt-in; follow-ups in FEATURES.md) | `docs/plans/2026-10-15-mcp-http-transport-design.md` (+ ADRs 0045–0047) |

When making an architectural decision, add an ADR (`docs/adr/NNNN-kebab-title.md`, MADR format) and link it from `docs/adr/0000-index.md`.

## wa-rs Dependency (Separate Library, consumed upstream)
- **Upstream of record:** `oxidezap/whatsapp-rust` (byte-identical to `jlucaso1/whatsapp-rust`; the project's active home). The WhatsApp protocol library whatsrust builds on.
- **Cargo.toml** points the 6 wa-rs crates (whatsapp-rust, wacore, wacore-binary, waproto, whatsapp-rust-tokio-transport, whatsapp-rust-ureq-http-client) at upstream by git tag.
- **It is a plain pinned dependency, NOT a fork we maintain.** The old `199-biotechnologies` "fork" was just upstream frozen at the v0.2 era with zero custom commits (see ADR 0002, 2026-06-25 correction). There is no sibling clone, no local `rev` to push, no `.cargo/config.toml` path-patch by default.
- **To upgrade wa-rs:** bump the tag in `Cargo.toml`, `cargo update`, fix any API breakage, run tests + a live smoke test. (As of the F1 effort: adopting `oxidezap` v0.6.0 — Phase 0 of `docs/plans/IMPLEMENTATION-ROADMAP.md`.)
- If a feature ever genuinely needs wa-rs *source* changes, fork `oxidezap`, point Cargo.toml at your fork, and upstream the change — but the default posture is consume-upstream-as-is.

## Key Files
- `src/bridge.rs` — core bridge: events, all message types, typing, groups, polls, presence, delivery receipts, group cache
- `src/outbound.rs` — typed outbound ops (21 OpKinds), payload structs, execute_job() builds wa::Message + uploads media
- `src/bridge_events.rs` — broadcast event bus: BridgeEvent, OutboundStatusEvent, OutboundJobState, DeliveryStatus
- `src/api.rs` — REST API server (59 endpoints) + SSE streaming + CLI HTTP client; `serve_connection` per connection, handlers return `ApiResponse`
- `src/api/golden_tests.rs` — byte-for-byte REST wire golden tests (fixtures change only in a deliberate, labelled behaviour change; a failing case prints the new bytes as a Rust literal)
- `src/mcp.rs` — MCP stdio server (34 tools, JSON-RPC, proxies to HTTP daemon; tools come from shared registry)
- `src/mcp_registry.rs` — shared MCP tool registry: tool metadata (name, description, input schema, annotations, access class), argument validation (`validated_args`), tool→REST mapping; shared by stdio and the HTTP listener (`mcp_http.rs`)
- `src/mcp_http.rs` — opt-in MCP Streamable HTTP listener: `POST /mcp` only, own port/bind/token, startup rules (bind after REST, before bridge), era detection, dual-era (modern `server/discover` protocol 2026-07-28, legacy `initialize` 2025-11-25/2025-06-18), request-metadata header validation (`-32020`), `tools/call` live (in-process via registry, permit lanes, 40 s tool budget from body completion), `resultType`/cache hints in modern era
- `scripts/mcp_sdk_smoke.py` — official Python MCP SDK smoke test against the MCP listener (ignored test `sdk_smoke_python` in `cargo test`; needs `uv`, downloads `mcp<2`, no WhatsApp contact; token from `WHATSRUST_MCP_TOKEN`)
- `tools/mcp-smoke-rs/` — standalone Rust MCP SDK smoke client: rmcp 3.5.1 (client + transport-streamable-http-client-reqwest); ignored test `sdk_smoke_rmcp` (`cargo test sdk_smoke_rmcp -- --ignored --nocapture`); three modes (modern, auto, legacy); first build downloads crates; no WhatsApp contact
- `deploy/proxy/` — tested nginx and Caddy reverse proxy configs for the MCP listener (TLS, only `/mcp` forwarded, other paths 404, Host rewritten to 127.0.0.1, 10 MiB body limit)
- `scripts/mcp-proxy-smoke.sh` — Docker proxy playbook: runs real nginx and Caddy in Docker in front of a running listener; ignored test `proxy_smoke_docker` (`cargo test proxy_smoke_docker -- --ignored --nocapture`); needs docker, openssl, curl
- `src/storage.rs` — rusqlite Signal Protocol store + typed outbound queue + v8 unified `messages` table (live + backfill, with `embed_status` column) + `embeddings` table (multi-model BLOB vectors) + FTS5 search + backfill job queue/cursor + `metadata` KV
- `src/backfill.rs` — historical backfill worker: two-phase async on-demand fetch (`client.fetch_message_history` → session-id → later `Event::HistorySync` correlated via `HistoryCorrelator`), single-worker FIFO pagination, contained-C target model (`since`/`all`/`count`), dedicated `BackfillPacer`, 3-level abort (batch/job/task), behind `HistorySource`/`BatchSink` trait seams
- `src/embedder.rs` — Embedder trait (sidecar contract), StdioEmbedder (JSON-RPC stdio transport), FakeEmbedder (test seam), cosine_similarity (Rust-side vector rerank)
- `src/embed_drain.rs` — embedding-drain worker: set-difference query (pending messages), batch sidecar calls, failure tracking (in-memory, 3-attempt cap), circuit-breaker (pathological-pending ceiling)
- `src/bin/fake-embedder.rs` — minimal fake sidecar for round-trip integration tests (behind `fake-embedder` Cargo feature)
- `embedder-rs/` — native Rust embedder sidecar (candle, separate crate; ADR 0040 — primary, Python-free)
- `scripts/embedder-sidecar.py` — legacy Python sidecar: sentence-transformers, paraphrase-multilingual-MiniLM-L12-v2 (384-dim), JSON-RPC stdio protocol (reference/parity checks)
- `src/polls.rs` — poll crypto (HKDF-SHA256 + AES-256-GCM)
- `src/dedup.rs` — generation-tracked DashMap dedup
- `src/read_receipts.rs` — batched receipt scheduler
- `src/qr.rs` — QR rendering (terminal/PNG/HTML/SVG)
- `src/instance_lock.rs` — single-instance file lock
- `src/lib.rs` — library crate entry: all modules pub (consumed by habb)
- `src/main.rs` — binary: daemon mode (REPL + API) + CLI client (50 commands) + MCP mode

## Patterns
- SQLite-first sends: all outbound ops enqueue to SQLite via `enqueue_job()`, worker executes via `execute_job()`
- `enqueue_and_wait()` subscribes to broadcast BEFORE enqueue for sync send methods; times out after 30s (ADR 0047), returning `SendTimedOut { job_id }` so the caller (REST → 504 + job_id + "do not retry") can report it without the job being cancelled (it stays queued and is usually delivered later)
- `parse_jid()` for JID normalization (phone → @s.whatsapp.net, group → @g.us)
- `parking_lot::Mutex<Connection>` + `spawn_blocking` for SQLite
- `extract_content_inner` recursive descent for inbound message parsing
- Schema migrations via version check in `Store::new()` (currently v8)
- Token-bucket rate limiter (burst + sustained rate) for anti-ban pacing
- Chat management ops (pin, mute, archive, mark-read, delete, star) use direct client calls, not the outbound queue
- Status/story sending (text, image, video, revoke) goes through the outbound queue like regular messages
- FTS5/BM25 lexical search: `query=Some(q)` → `messages_fts MATCH` + `ORDER BY f.rank` (relevance-ranked); `query=None` → chronological browse
- Hybrid recall search (opt-in via `WHATSRUST_EMBEDDER_CMD`, ADR 0041): FTS5 lexical candidates ∪ a bounded brute-force vector scan of the chat's stored vectors (skipped above `WHATSRUST_SEMANTIC_SCAN_CAP`, default 50000). Lexical matches are always kept; vector-only matches need `cosine ≥ WHATSRUST_SEMANTIC_MIN_SCORE` (default 0.55). No embedder → byte-identical to M1 lexical.
- Bounded query-embed wait + cool-down (ADR 0042): `search` waits for the SAME in-flight embed (never re-sent) within one budget (`WHATSRUST_SEMANTIC_QUERY_TIMEOUT_SECS`, default 20s, covering health-poll + embed); exhausting it falls back to lexical AND trips a shared `SemanticCooldown` (30s, doubling to 10min cap, reset by the first successful embed from search or the drain worker) during which later searches skip the embed entirely.
- Embedder sidecar: stateless stdio JSON-RPC process (primary: Rust + candle, ADR 0040 — bge-m3 default, MiniLM selectable via `--model`/`WHATSRUST_EMBEDDER_MODEL`/`make run-semantic EMBED_MODEL=minilm`; legacy: Python + sentence-transformers); embedding-drain worker (independent task, set-difference, multi-model, circuit-breaker at pathological-pending ceiling); per-model purge via `purge_embeddings(model_id)` (incremental_vacuum, exposed in API/MCP/CLI)
- SQLite-first backfill: durable `backfill_jobs` queue + single FIFO worker + connection-gating (defer, not fail, when disconnected)
- Device name fixed at pairing (ADR 0043): `WHATSRUST_DEVICE_NAME` (default `whatsrust`) is used only while the device is unpaired, then saved as `metadata.device_name`; once paired the saved name wins and the env var is ignored. `clear_device()` (on logout) deletes it with the credentials, so a re-pair picks up the current value. Library consumers set `BridgeConfig.device_name` directly.
- MCP argument validation (ADR 0046): `validated_args` in `src/mcp_registry.rs` is the single choke point; unknown tool or non-object arguments → JSON-RPC `-32602`; undeclared keys → `isError: true` (reported alone, nothing sent); optional keys set to `null` are dropped; every remaining value must match its top-level `type` (integer must fit i64; integral floats normalised); every `required` key must be present and non-null (type and required problems reported together). Every tool schema sets `additionalProperties: false`. Undeclared keys (notably REST-only `path` for local-file media reads) are unreachable via MCP
- REST handlers return `ApiResponse` (`into_bytes()` is the only serializer), and the per-connection pipeline in `serve_connection` is head → gate → token → framing → 100 Continue (if awaited) → body → route. Early rejections (gate, token, framing, SSE capacity, unreadable body) write the response and drain ≤64 KiB or ≤1 s of incoming data before closing.
- API startup refusal (F3 Phase 0b): `validate_api_startup_from_env` runs before the instance lock; the REST listener is pre-bound before `WhatsAppBridge::start` (`serve_with_listener`); the library `serve()` shares the validator but logs and returns
- MCP listener (F3 Phases 2–5 done): startup refusal like REST (`validate_mcp_startup_from_env` → exit on unsafe config); per-connection pipeline accept (32-connection cap → 503 + Retry-After: 10) → head (arrival + 20 s) → route (`POST /mcp` only, other paths → 404, other methods → 405) → gate → token → framing → Content-Length > 1 MiB? → large-body permit (any method) → 100 Continue → body (arrival + min(60 s, 20 s + 4 s per MiB), 10 MiB cap; body complete = tool budget start) → JSON-RPC → notifications 202 → era → header validation (`-32020`) → dispatch (legacy: initialize/ping/tools/list/tools/call; modern: server/discover/tools/list/tools/call with `resultType`/cache hints, initialize/ping → 404 -32601; initialize/ping/tools/list take no permit; tools/call runs in-process after validation → read/write permit from `tool.access_class`, large-body permit moved into task → 40 s tool budget from body completion); never mints `Mcp-Session-Id`. Logging: four targets (`whatsrust::mcp::call`, `whatsrust::mcp::infra`, `whatsrust::mcp::reject`, `whatsrust::mcp::auth`), one line per request, never arguments/bodies/tokens/JSON-RPC id/raw headers, unknown method/tool/version masked, peer IP on auth failures and initialize/server/discover. Metrics: `/api/status` `mcp_http` snapshot (`null` when off; otherwise counters + gauges for requests, tools, errors, rejects, timeouts, 503s, outlived tasks, in-flight and available permits)
