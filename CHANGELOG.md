# Changelog

This file tracks notable changes to the whatsrust experimental fork. The project does not use release tags, so sections are dated by the commit date of the work on the main branch; a milestone date recorded in `docs/plans/FEATURES.md` or the roadmap can be a few days earlier (the day the work was done or validated). Design decisions are documented in `docs/adr/`, feature detail lives in `docs/plans/FEATURES.md`, and the order of open work lives in `docs/plans/IMPLEMENTATION-ROADMAP.md`. Every change adds a line under `[Unreleased]` when it lands (format: Keep a Changelog 1.1.0 categories, then commit hashes and ADR numbers in parentheses); the lines move to a dated section when the branch is merged to main.

## [Unreleased]

Work on branch `ops/supervisor`, not yet on `main`.

### Added
- `scripts/deploy.sh`: run the daemon as a service that restarts after a crash (launchd LaunchAgent on macOS, systemd user unit on Linux; ADR 0049). `install` is a step-by-step wizard (directory, binary, ports, API/MCP tokens, semantic model, device name, pairing history, autostart; every step also a flag, `--yes`, `--dry-run`) and starts nothing; `run` starts a paired instance in the background, or shows the QR code in the terminal until the device is linked; a start gate (`bin/start-if-paired`) keeps an unpaired instance from ever running unattended (login, crash restarts, upgrades); also `stop`, `re-pair` (fresh or keep-history), `autostart on|off`, `uninstall` (wizard: keep the data, or delete it after an optional verified backup), `status`, `logs`, `restart` (graceful), `upgrade`. Each instance lives outside the repo with its own copy of the binary and an ownership marker; restart on any non-zero exit (1 = startup refusal, 101 = panic) with a 10 s throttle; templates in `deploy/supervisor/`; HOWTO §3b; not live-tested yet
- `scripts/remap-doc-hashes.sh`: after a rebase-merge, finds commit hashes cited in the docs that are no longer in `main`'s history and maps each to its rebased twin (same subject and author date); report-only by default, `--apply` rewrites

### Fixed
- Commit hashes cited in the docs that the 2026-10-23 rebase-merge left pointing at branch-only commits now point at the `main` commits

## 2026-10-23 — Fetch-history anchors and pairing history, MCP HTTP listener, run profiles, planning docs

Merged from `feature/mcp-streamable-http` (rebase-merge; the hashes below are the `main` commits).

### Added
- Store messages from the pairing history sync package and late on-demand fetch answers, so chats have anchors from the start and stuck fetches unblock on the next attempt; `WHATSRUST_STORE_PAIRING_HISTORY=false` turns off the pairing part (b15aeec; ADR 0048)
- Seed fetch anchors from archive/unarchive and mark-read syncs that carry a chat's last message key; a chat with no stored message can now be fetched without sending anything (2768412; ADR 0003 amendment)
- `clear-data` command in `run-profile.sh`: empties a profile's message data and keeps the pairing; refuses while the daemon runs, prints the exact SQL, backs up first, asks y/N and then for the profile name (548d1d8)
- `mcp-on` / `mcp-off` / `mcp-client` commands in `run-profile.sh`: switch a profile's HTTP MCP listener by rewriting the `.env` with bash builtins only; token never printed or in argv; `mcp-client` prints a Claude Code config using `${WHATSRUST_MCP_TOKEN}` (e667b36)
- MCP Streamable HTTP listener (F3 Phases 2–5): `POST /mcp` on its own port, dual-era (modern 2026-07-28 + legacy 2025-11-25/2025-06-18), in-process tool dispatch via shared registry, permit lanes, 40 s tool budget from body completion, logging (four targets), metrics in `/api/status` (6ff509a, 03a5158, b64b458, 7768abf, 462d25a)
- Dev run profiles (F9): each profile in `runs/<profile>/` keeps its own `.env`, DB, logs and instance lock; `scripts/run-profile.sh` commands `new`, `list`, `daemon`, `attach`, `cli`, `mcp`, `env`, `clear-data`, and the MCP listener switches; `daemon` supports `--tmux` (opt-in, session `whatsrust-<profile>`) and `--detach`; `make new-profile` / `make run-profile` / `make attach-profile` (5cb92e5, 8633a84, 4a24426, e667b36, 548d1d8)
- Docker proxy playbook (`scripts/mcp-proxy-smoke.sh`): tests real nginx and Caddy in front of the MCP listener; ignored test `proxy_smoke_docker` (7768abf)
- `CHANGELOG.md` (this file): summarized fork history, then one line per change; the doc matrix (now in `AGENTS.md`) lists which docs to update for each kind of change

### Changed
- Planning docs restructured: `docs/plans/FEATURES.md` holds each feature's breakdown, issues encountered and ADR links; `docs/plans/IMPLEMENTATION-ROADMAP.md` holds the feature map, one merged priority backlog (F8 README lowered to P2) and the completed list
- Project instructions split: the tool-neutral content of `CLAUDE.md` moved to `AGENTS.md` (read by any coding agent); `CLAUDE.md` now imports it with `@AGENTS.md` and holds only Claude Code-specific rules (none yet); the wa-rs section now states the real pin (`rev` 9e8c70e2, not a tag)
- Fetch-history now waits 180 s for the phone (was 60 s), because a sleeping phone answered minutes late (2768412; ADR 0003 amendment)
- Run profile daemons use tmux only when `--tmux` is passed (was always-on); `--detach` implies `--tmux`; a missing tmux is an error, not a silent fallback (4a24426)
- Stdio MCP transport now negotiates 2025-11-25 / 2025-06-18 protocol versions (was 2024-11-05 with no negotiation), returns `instructions`, and answers `ping` (d4cddad; ADR 0045 amendment)

### Fixed
- Fetch-history without a usable anchor (no cursor, no stored message, not exhausted) now fails fast with REST 409 `no_anchor` and an MCP tool error, instead of hanging 60 s and leaving an empty cursor that blocked the chat for good; empty cursors from older versions are re-seeded (9610d18; ADR 0003 amendment)

## 2026-10-19 — MCP HTTP listener, dev run profiles, crash hardening

### Added
- MCP Streamable HTTP transport (F3 Phases 0a–1): refactored REST server for in-process dispatch, shared tool registry with access classes, request gate and token check before body read, lingering drain, Expect: 100-continue support, config enforcement for unsafe REST and MCP binds (66e4852, 397c6db, e96f346, 3c72b68, 9685fe7; ADR 0045)
- Panic hook: logs one `ERROR whatsrust::panic` line with message and location before the release-mode abort, so a UTF-8 slicing panic or other crash leaves a trace in the log (219697d)
- Lint guard: `clippy::string_slice` warning in both crate roots prevents byte-index string slicing that panics on UTF-8 boundaries; all slices rewritten with non-panicking APIs (219697d)

### Fixed
- UTF-8 panic in REPL reply preview when a multibyte character was sliced at a byte boundary (a763838)

## 2026-10-16 — MCP tool validation and sync-send timeout

### Changed
- MCP tool argument validation (ADR 0046): `validated_args` in `src/mcp_registry.rs` is the single choke point; unknown tools and non-object arguments get JSON-RPC `-32602`; undeclared keys, wrong-type values and missing required keys are rejected as `isError: true` naming the fields, and nothing is sent; optional keys set to `null` are dropped; every tool schema sets `additionalProperties: false` (2b57527; ADR 0046)
- Sync send methods (`enqueue_and_wait`) now time out after 30 s and return `SendTimedOut { job_id }` so the REST handler can report 504 + job_id + "do not retry" without canceling the job; the job stays queued and usually delivers later (2b57527; ADR 0047)

## 2026-10-14 — Request gate

### Added
- Origin/Host/Content-Type gate for the REST API: rejects cross-site and DNS-rebinding requests; mitigates CSRF and DNS rebinding holes (1fda7ee; ADR 0044)

## 2026-10-13 — Device name at pairing

### Changed
- Device name is now fixed at pairing time: `WHATSRUST_DEVICE_NAME` (default `whatsrust`) is used only while unpaired, then saved as `metadata.device_name`; once paired the saved name wins; `clear_device()` on logout deletes it so re-pairing picks up the current value (71025e3, 2d84c25; ADR 0043)

### Added
- `make release-all` and `make dist` targets for cross-platform builds (71025e3)

## 2026-10-06 — Sidecar client hardening; M2 live-validated

**M2 (semantic search) live-validated on the real account (2026-10-05):** bge-m3 hybrid search found Hebrew messages from English queries with no shared words. The live runs found the two bugs fixed below.

### Added
- Bounded query-embed wait with cool-down (ADR 0042): `search` waits for the same in-flight embed within one budget (`WHATSRUST_SEMANTIC_QUERY_TIMEOUT_SECS`, default 20 s); exhausting it falls back to lexical AND trips a shared cool-down (30 s, doubling to 10 min cap, reset by the first successful embed) (31b985e; ADR 0042)
- Cancellation-safe sidecar client: embed calls no longer leave orphaned in-flight requests when the search task is dropped (9c8252c; ADR 0042)

### Fixed
- Query strings now decode `+` as a space (63f5662)

## 2026-10-01 — Native Rust embedder and hybrid recall (M2 code-complete)

**M2 code-complete (recorded 2026-09-30):** the M2.1–M2.6 work below plus hybrid recall and the bge-m3 default.

### Added
- Native Rust embedder sidecar (`embedder-rs/`): candle-based, no Python dependency, serves bge-m3 (default, 1024-dim) or MiniLM (`--model minilm`); now the default for `make run-semantic` (7a04575, afeaa5b; ADR 0040)
- Hybrid recall (ADR 0041): semantic search is FTS5 lexical candidates ∪ a bounded brute-force vector scan (skipped above `WHATSRUST_SEMANTIC_SCAN_CAP`, default 50000); lexical matches are always kept; vector-only matches need `cosine ≥ WHATSRUST_SEMANTIC_MIN_SCORE` (default 0.55, calibrated for bge-m3); no embedder degrades to FTS5 lexical byte-identically (e46809e; ADR 0041)
- bge-m3 model support in the Rust embedder sidecar; fixed cold-cache model downloads (46b070a)

### Changed
- Python embedder sidecar is now the legacy/parity-check fallback; Rust sidecar is the default (afeaa5b)

## 2026-09-10 — M2.1–M2.6 and the MVP sidecar

**All six M2 sub-milestones done** (embedder sidecar contract, embeddable-text classification, drain worker, semantic search path, multi-model purge, config-wiring/observability; M2.6 recorded done 2026-09-02). These commits first marked M2 code-complete; hybrid recall and the bge-m3 default followed (2026-10-01).

### Added
- M2.1: `Embedder` trait + stdio JSON-RPC transport (`model_info` / `embed` / `health`, trust-but-verify validation) (69f386a)
- M2.2: embeddable-text classification (`embeddable_text()` distinct from `display_text()`), set-difference drain query (f4c96c7, c727a28)
- M2.3: embedding-drain worker with pathological-pending circuit breaker (05f8afc, d54f603)
- M2.4: semantic search path (query-embed → rerank → lexical fallback) (a8919e0)
- M2.5: multi-model retention with explicit per-model embedding purge; `POST /api/embeddings/purge`, MCP `whatsrust_purge_embeddings` (34 tools total), CLI `purge-embeddings <model_id>` (2759fa3)
- M2.6: embedder config wiring tests, misbehave integration test, observability (6b49f58)
- MVP Python embedder sidecar: sentence-transformers, paraphrase-multilingual-MiniLM-L12-v2 (384-dim); JSON-RPC stdio protocol (4ed3a58, ee6b329; ADR 0039)
- Makefile targets: `run-semantic`, `sidecar-setup`, `sidecar-smoke` (72e42d6)
- Fake embedder binary for round-trip integration tests (behind `fake-embedder` Cargo feature) (69f386a)

### Changed
- `cargo run` no longer fails with binary ambiguity; `default-run = "whatsrust"` in `Cargo.toml` (8de6be9)

## 2026-07-28 — M2 starts, Makefile and HOWTO

### Added
- Makefile with targets `build`, `test`, `lint`, `release`, `run`, `run-semantic`, `sidecar-setup`, `sidecar-smoke` (9c544a4)
- `docs/HOWTO.md`: operational playbook for build, configure, run, CLI/MCP usage, DB migration recovery, test/lint (9c544a4)
- M2 detailed execution plan with four cold design reviews folded in (f3f1a0b, 81c5226, 0c63111, 9fd8042, cb22f06, b65d3cb; ADR 0038)

## 2026-07-20 — M1 complete

**M1 (historical fetch + lexical search) complete:** live E2E passed on a real account (API/CLI/MCP trigger, SSE events, FTS5 search, cancel).

### Added
- M1.1: v8 unified `messages` table + FTS5 (external-content, BM25) + `'delete'`-command sync triggers; sibling tables (`media_refs`, `embeddings`, `backfill_cursor`, `backfill_jobs`, `metadata`); v7→v8 migration (copy-then-drop, fail-closed backup, FTS5 probe, post-commit validation, circuit-breaker pin, `--rollback`/`--migrate`, watchdog baseline seed); 168 tests + real-data dry-run (d5490e0, 49396c2, 0eefef8; ADR 0009/0019/0027/0028/0029/0030/0031/0032/0036)
- M1.2: single-FIFO paginating backfill worker (two-phase async on-demand fetch via `HistoryCorrelator`), contained-C target model (`since`/`all`/`count`), 3-level abort, dedicated backfill pacer, daemon-side guards (queue-depth + `max_messages` clamp + cooldown), fail-closed safety config (`WHATSRUST_DANGEROUSLY_ALLOW_*`), dotenvy `.env` (b5ab1bc, 34ec69e, c97716a, 7e29377, 7fac6ec, d38ad1f, 3261f4d; ADR 0003/0010/0020/0021/0022/0023/0026/0033/0035)
- M1.3: FTS5/BM25 lexical search (`query=Some` → `messages_fts MATCH` + `ORDER BY rank` relevance-ranked; `query=None` chronological browse unchanged); quote-as-phrase MATCH sanitization; single FTS path, no LIKE fallback; multilingual (Hebrew/Arabic) and injection-safe (6f81f89, d9a08ad; ADR 0019)
- M1.4: `/api/history-fetch` (trigger/status/cancel) + MCP tools `whatsrust_fetch_history` / `_status` / `_cancel` (→ 33 MCP tools) + CLI commands; SSE `BackfillProgress` events (fuzzy/precise per target-kind); storage-growth watchdog (periodic `wal_checkpoint(PASSIVE)` + `db+wal+shm` stat, ≥50% → WARN + `BridgeEvent::StorageAlert` + baseline reset) (74f2d7c, a990bc7, 384e518, ddf9bd1, ad8ea30, 6c0f11a; ADR 0011/0013/0034/0036)

### Fixed
- Connection-gating: backfill worker defers (not fails) when disconnected (03a0ed7)
- LID/PN resolution: match LID conversations, store under phone JID (a5c6220)
- Percent-encode user values in MCP/CLI query URLs; decode server-side (d56d808)
- Daemon hang on Ctrl+C when run in a pipeline (4318f29)

## 2026-07-02 — Phase 0 gate passed (wa-rs adoption)

**Phase 0 GO/NO-GO gate: PASSED (GO recorded 2026-07-01).** Adopted wa-rs upstream HEAD `9e8c70e2` (not the v0.6.0 tag; HEAD is +315, all breaking changes handled). Live smoke test passed: connect, receive, group send delivered+read, 1:1 send delivered, history bootstrap. Fixed `463 MissingTcToken` DM failure by enabling history sync (delivers trusted-contact tokens). Known limitation: cold-outreach to non-contacts is WhatsApp-account-gated (`nct_salt`), not code-fixable.

### Added
- History sync enabled by default to obtain 1:1-DM trusted-contact tokens (3df4ce9, 8852eed; ADR 0037)

### Changed
- Adopted wa-rs upstream (`oxidezap/whatsapp-rust`) HEAD `9e8c70e2` (was v0.2 era); 150 tests + live smoke test (3df4ce9; ADR 0002)

## 2026-06-25 — Design reviews, ADRs and roadmaps

### Added
- Doc matrix in `CLAUDE.md` (where to look for what) and `.gitignore` leak hardening (7c4c403)
- Three cold design reviews of F1/F2 spec, resolved into 36 ADRs (0001–0036) and a consolidated design doc (`docs/plans/2026-06-17-historical-fetch-semantic-search-design.md`); final verdict: implementation-ready, no blockers (7c4c403, e0be8b3, 9fa8356, c60dc5e, 3010442, b9ae01c)
- Implementation roadmap: Phase 0 gate → M1 → M2 (`docs/plans/IMPLEMENTATION-ROADMAP.md`) (37d7256)
- Live feature roadmap (`docs/plans/FEATURES.md`) (82c148d)

## 2026-06-15 — Early fixes

### Changed
- Tokio worker thread stack increased to 8 MB (was default 2 MB); fixes inbound-message stack overflow (0c8228a; ADR 0001)

### Fixed
- Daemon hang on Ctrl-C when stdin is an open terminal (36ae622)
- Dead-code warning silenced (b4b0500)

## Upstream baseline

This experimental fork started 2026-06-14 (commit b4b0500) from the upstream project by Boris Djordjevic. The upstream baseline (last upstream commit 3dc3015, 2026-04-17) already included:

- **Core bridge:** WhatsApp protocol via wa-rs (v0.2 era), event handling, message send/receive, delivery receipts, read receipts (adaptive batching), typing and presence indicators, groups (query, create, rename, leave, add/remove/promote/demote participants, invite links, reactions), polls (send, vote), view-once messages, forwarding, revoke, edit, quote/reply, mentions, link preview, media (image, audio, video, document, sticker) with base64 upload, scheduled sends, outbound media enrichment (thumbnails, dimensions, waveforms).
- **Chat management:** pin, mute, archive, mark-read, delete, star.
- **Status/story:** send text/image/video, revoke.
- **Reliability:** SQLite-first outbound queue (durable send path, per-message exponential backoff), bridge event bus, SSE event stream, rate limiter (token-bucket), connection recovery (graceful `StreamReplaced` recovery with loop detection), LID handling (device suffix stripping, poll key fallback, sender_alt fallback, receipt/presence normalization).
- **Storage:** rusqlite Signal Protocol store (v7 schema), inbound message history + search API (v6 schema; no FTS, no backfill), group metadata cache (5-min TTL, mutation-invalidated).
- **API surfaces:** REST API server (50+ endpoints, token auth), CLI mode (50+ commands), REPL, MCP stdio server (30 tools, JSON-RPC).
- **QR pairing:** modular QR rendering (terminal/PNG/HTML/SVG), in-place refresh, auto-refreshing HTML, configurable device name, automatic re-pairing on `LoggedOut`.
- **Hardening:** single-instance file lock, atomic dedup (DashMap), scheduled/delayed sends, anti-ban pacing (auto typing indicator, device name, wider jitter), production audit fixes (Codex, GPT Pro).
- **Library crate:** exposed as `lib.rs` with `subscribe_state` API for embedding in other Rust projects.

