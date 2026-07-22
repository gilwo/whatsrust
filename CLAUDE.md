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
| **Architecture Decision Records (the "why" ledger)** | `docs/adr/` — start at `docs/adr/0000-index.md` |
| **Feature roadmap & status (LIVE — what's done/in-flight/planned)** | `docs/plans/FEATURES.md` |
| **Implementation execution plan (phases, gates, milestone exit criteria)** | `docs/plans/IMPLEMENTATION-ROADMAP.md` |
| Design specs / plans (the "what/how" blueprints) | `docs/plans/*.md` |
| Design review reports (cold reviews, reconciliation) | `_reviewer/design/` |
| Historical fetch + lexical search (M1 — **DONE** 2026-07-20) / semantic search (M2 — planned) | `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md` (+ ADRs 0001–0037) |

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
- `src/api.rs` — REST API server (58 endpoints) + SSE streaming + CLI HTTP client
- `src/mcp.rs` — MCP server (33 tools, JSON-RPC over stdio, proxies to HTTP daemon)
- `src/storage.rs` — rusqlite Signal Protocol store + typed outbound queue + v8 unified `messages` table (live + backfill) + FTS5 search + backfill job queue/cursor + `metadata` KV
- `src/backfill.rs` — historical backfill worker: two-phase async on-demand fetch (`client.fetch_message_history` → session-id → later `Event::HistorySync` correlated via `HistoryCorrelator`), single-worker FIFO pagination, contained-C target model (`since`/`all`/`count`), dedicated `BackfillPacer`, 3-level abort (batch/job/task), behind `HistorySource`/`BatchSink` trait seams
- `src/polls.rs` — poll crypto (HKDF-SHA256 + AES-256-GCM)
- `src/dedup.rs` — generation-tracked DashMap dedup
- `src/read_receipts.rs` — batched receipt scheduler
- `src/qr.rs` — QR rendering (terminal/PNG/HTML/SVG)
- `src/instance_lock.rs` — single-instance file lock
- `src/lib.rs` — library crate entry: all modules pub (consumed by habb)
- `src/main.rs` — binary: daemon mode (REPL + API) + CLI client (54 commands) + MCP mode

## Patterns
- SQLite-first sends: all outbound ops enqueue to SQLite via `enqueue_job()`, worker executes via `execute_job()`
- `enqueue_and_wait()` subscribes to broadcast BEFORE enqueue for sync send methods
- `parse_jid()` for JID normalization (phone → @s.whatsapp.net, group → @g.us)
- `parking_lot::Mutex<Connection>` + `spawn_blocking` for SQLite
- `extract_content_inner` recursive descent for inbound message parsing
- Schema migrations via version check in `Store::new()` (currently v8)
- Token-bucket rate limiter (burst + sustained rate) for anti-ban pacing
- Chat management ops (pin, mute, archive, mark-read, delete, star) use direct client calls, not the outbound queue
- Status/story sending (text, image, video, revoke) goes through the outbound queue like regular messages
- FTS5/BM25 lexical search: `query=Some(q)` → `messages_fts MATCH` + `ORDER BY f.rank` (relevance-ranked); `query=None` → chronological browse
- SQLite-first backfill: durable `backfill_jobs` queue + single FIFO worker + connection-gating (defer, not fail, when disconnected)
