# whatsrust Feature Roadmap & Status

**Type:** Live document — kept up to date as features land. Update the **Status** of an
item when work starts/completes; add a dated note in its row's detail.
**Last updated:** 2026-10-19

This is the single tracking surface for in-flight and planned work on this experimental
fork. The *why* for each design lives in the ADRs (`docs/adr/`); the *what/how* blueprints
live in `docs/plans/*-design.md`; the *execution order* lives in
`docs/plans/IMPLEMENTATION-ROADMAP.md`. This file tracks *status*.

**Status legend:** 🟢 Done · 🟡 In progress · 🔵 Designed (not started) · ⚪ Planned (not designed) · ⏸️ Deferred

---

## Major Features

### F1 — Historical message fetch + semantic/lexical search 🟢 Done (M1 2026-07-20; M2 live-validated 2026-10-05)
Per-chat historical backfill (`all` / `since:T` / `count:N`) + local FTS5 (lexical) + optional vector (semantic) search.
- **Design:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md`; ADRs 0001–0036.
- **Execution plan:** `docs/plans/IMPLEMENTATION-ROADMAP.md` (Phase 0 gate → M1 → M2).
- **Reviews:** 3 cold passes (`_reviewer/design/`), 2026-06-18 / -23 / -24 → final verdict **implementation-ready, no blockers**.
- **First step:** Phase 0 — wa-rs adoption, a HARD GO/NO-GO gate (ADR 0002). **✅ PASSED (GO).**
- **Milestone tracking** (detailed phase/task plans written at each milestone's start, not before — gate-gated):
  - [x] **Phase 0 — GATE: GO** (2026-07-01, commit 615d185). Adopted wa-rs **main HEAD `9e8c70e2`** (not the v0.6.0 tag — the tag lacked the DM-send fix #731; HEAD is +315, all breaking changes handled). Verified: compile + **150 tests** + live smoke test (connect / receive / group send delivered+read / **1:1 send delivered** / history bootstrap). Live testing surfaced + fixed a `463 MissingTcToken` DM failure by **enabling history sync** (delivers trusted-contact tokens; ADR 0037). Known limitation: cold-outreach to non-contacts is WhatsApp-account-gated (`nct_salt`), not code-fixable. (ADR 0002/0037)
  - [x] **M1 — fetch + lexical search** — ✅ **COMPLETE 2026-07-20** (all exit criteria met; live E2E passed on a real account). Ships independently, no sidecar. Detailed plan: `docs/plans/2026-07-02-M1-detailed-plan.md`.
    - [x] **M1.1 storage + migration** — DONE 2026-07-03. v8 `messages` table + FTS5 (external-content, BM25) + `'delete'`-command sync triggers; sibling tables (`media_refs`, `embeddings`, `backfill_cursor`, `backfill_jobs`, `metadata`); copy-then-drop v7→v8 migration; staged ceremony (fail-closed backup → FTS5 probe → migrate-in-TX → post-commit validation → circuit-breaker pin + `--rollback`/`--migrate` → watchdog baseline seed); **I2 age-prune removed**. 168 tests + real-data dry-run (Hebrew/Arabic FTS verified). (ADR 0009/0019/0027/0028-0032/0036)
    - [x] **M1.2 fetch worker + safety/config** — DONE 2026-07-03. Single-FIFO paginating worker (two-phase async on-demand fetch via `HistoryCorrelator`), contained-C target model (`since`/`all`/`count`), 3-level abort, dedicated backfill pacer, daemon-side guards (queue-depth + `max_messages` clamp + cooldown), fail-closed safety config (`WHATSRUST_DANGEROUSLY_ALLOW_*`), dotenvy `.env`. Connection-gating (defer-not-fail when not connected) + LID/PN resolution fixes. **Live-validated end-to-end** (count fetch auto-paginated to exhaustion, 14 msgs stored under phone JID). (ADR 0003/0010/0020/0021/0022/0023/0026/0033/0035)
    - [x] **M1.3 FTS5 lexical search** — DONE 2026-07-13. `search_inbound` rewritten: `query=Some` → FTS5 `messages_fts MATCH` + BM25 `ORDER BY rank` (ascending); `query=None` history browse unchanged. Quote-as-phrase MATCH sanitization (`"`→`""`; operators neutralized). Single FTS path (EXPLAIN-verified: FTS index drives, PK rowid lookup) — no LIKE fallback. Multilingual (Hebrew/Arabic) + injection-safe. 9 tests; suite 188 lib / 203 bin green. API/MCP JSON shape unchanged (now relevance-ranked). (ADR 0019)
    - [x] **M1.4 API/MCP trigger + watchdog** — DONE 2026-07-16. `/api/history-fetch` (trigger/status/cancel) + MCP tools `whatsrust_fetch_history`/`_status`/`_cancel` (→33 MCP tools) + SSE `BackfillProgress` events (fuzzy/precise per target-kind); storage-growth watchdog wired into the periodic prune tick (`wal_checkpoint(PASSIVE)` + `db+wal+shm` stat, ≥50% → WARN + `BridgeEvent::StorageAlert` + baseline reset); temp `WHATSRUST_BACKFILL_TEST` hook retired. **Live E2E passed 2026-07-20** (real account: API/CLI/MCP trigger, SSE `backfill` + `storage_alert` events, FTS search, cancel→404) — **M1 COMPLETE.** (ADR 0011/0013/0034/0036)
    - [x] **fetch-history needs a usable anchor** — DONE 2026-10-20. The phone ignores an empty anchor, so a fetch of a chat with no stored message hung 60 s and left an empty cursor that blocked the chat for good (live run 2026-10-20). Now the anchor is the cursor's, else the oldest stored message; none → REST 409 `no_anchor` (also an MCP tool error) and the worker fails fast without calling the phone; empty cursors from older versions are re-seeded. (e140361; ADR 0003 amendment)
    - [x] **fetch-history: anchors from archive/read-state syncs, 180 s wait** — done 2026-10-22. An archive, unarchive or mark-read sync from the phone carries the chat's last message key; for a chat with no anchor (no usable cursor, no stored message, not exhausted) that key becomes the cursor's anchor, so such a chat can be fetched without sending anything (archive and unarchive it on the phone). The fetch now waits 180 s for the phone (was 60 s), because a sleeping phone answered minutes late; the timeout error says to open WhatsApp on the phone. Timestamp-only anchors do not work (tested 2026-10-21). `no_anchor` guidance updated. (ec3691f; ADR 0003 second amendment)
    - [x] **fetch-history: store the pairing history and late on-demand answers** — done 2026-10-22. The history package the phone sends at pairing is no longer ignored: every unmatched chunk with conversations is stored (`source=bootstrap`, keyed like live messages, duplicates skipped, embedding queued), so those chats have an anchor from the start; a late answer to an on-demand fetch is stored too (`source=backfill`; the job stays failed, the next fetch continues from the stored messages). A `@lid` DM that carries the phone JID (`pnJid`) is stored under it. Media is not downloaded for stored history: a row keeps its label (`[image 12.1KB] caption`) and a `media_refs` row for a later on-demand download (not built yet; ADR 0005 amendment). Storing runs in a spawned task and is best-effort; `WHATSRUST_STORE_PAIRING_HISTORY=false` turns the pairing part off. Only a fresh pairing delivers the package. (eed8e19; ADR 0048)
  - [x] **M2 — semantic search** — ✅ **DONE: code-complete 2026-09-30, live-validated 2026-10-05** (bge-m3 hybrid search on the real account: English queries found Hebrew messages with no shared words). All six M2 sub-milestones done (embedder sidecar contract, embeddable-text classification, drain worker, semantic search path, multi-model purge, config-wiring/observability), **plus ADR 0041 hybrid recall** (FTS5 ∪ bounded vector scan, so word-free cross-language/paraphrase queries can match, not just reorder) **and ADR 0039/0040's bge-m3 rollout** (native Rust/candle sidecar, `embedder-rs/`, now the default for `make run-semantic`; MiniLM stays selectable via `EMBED_MODEL=minilm`). Default `WHATSRUST_SEMANTIC_MIN_SCORE=0.55`, calibrated for bge-m3 against the real (read-only) `whatsapp.db` (`make calibrate`, ADR 0041). Semantic search is **opt-in** via `WHATSRUST_EMBEDDER_CMD` and **dormant** (degrades to FTS5 lexical) when sidecar absent/down. New surfaces: API `POST /api/embeddings/purge`, MCP `whatsrust_purge_embeddings` (34 tools total), CLI `purge-embeddings <model_id>`. (ADR 0008/0015/0016/0017/0024/0038/0039/0040/0041)

### F2 — Embedder sidecar (implementation) 🟢 Done (M2.1–M2.6 + ADR 0040/0041/0042 complete; live-validated 2026-10-05)
Stateless separate binary, pure vectorizer; stdio JSON-RPC v1; transport-neutral
`Embedder` trait (HTTP/localhost as future sibling); multilingual model default.
- **Design:** ADRs 0006, 0015, 0024 (+ 0007/0008/0016/0017/0018/0038/0039/0040/0041). Protocol & validation fully specified.
- **Status note:** M2.1–M2.6 code-complete (2026-09-02); native Rust sidecar (`embedder-rs/`, candle, ADR 0040) + bge-m3 support (ADR 0039) + hybrid recall (ADR 0041) landed 2026-09-30. The Python MVP sidecar is now the legacy/parity-check fallback.
- [x] `Embedder` trait + stdio JSON-RPC transport (model_info / embed / health, trust-but-verify validation) — DONE M2.1
- [x] Sidecar binary: MVP Python + sentence-transformers (paraphrase-multilingual-MiniLM-L12-v2, 384-dim) — DONE 2026-08-31; superseded as the deployment default by the native Rust sidecar (`embedder-rs/`, candle), serving MiniLM or **bge-m3** (default for `make run-semantic`) — DONE 2026-09-30 (ADR 0040/0039)
- [x] Drain worker integration (ADR 0015), multi-model store + purge (ADR 0017) — DONE M2.3/M2.5

### F3 — MCP streamable HTTP transport (on top of stdio) 🟢 Done (2026-10-19)
Add MCP Streamable HTTP transport alongside the existing stdio transport — so MCP clients
can connect over HTTP (remote/multiplexed) in addition to spawn-as-child stdio.
- **Status:** ✅ **DONE 2026-10-19** (Phases 0a through 5 complete; dual-era listener feature-complete; ADR 0045 accepted + amended).
- **Design:** A dedicated `/mcp` listener runs inside the daemon on its own port (`WHATSRUST_MCP_PORT`, off by default), serving only the `/mcp` route. Supports dual-era clients: modern (2026-07-28) and legacy (2025-11-25, 2025-06-18) from one stateless endpoint. In-process dispatch to existing REST handlers via a shared tool registry (`src/mcp_registry.rs`). Separate configuration: `WHATSRUST_MCP_TOKEN` (required, ≥22 characters, must differ from `WHATSRUST_API_TOKEN`), `WHATSRUST_MCP_BIND` (default `127.0.0.1`, independent of `WHATSRUST_BIND`), `WHATSRUST_MCP_ALLOW_REMOTE` (opt-in for non-loopback bind, logs loud warning), and `WHATSRUST_MCP_READ_ONLY` (server-side read-only switch). The stdio proxy keeps its transport but now shares the registry (annotations in `tools/list`, the same argument validation).
- **References:** Design doc `docs/plans/2026-10-15-mcp-http-transport-design.md`, ADR 0045.
- [x] Design pass + ADR
- [x] Phase 0a: Pure refactors (head/body split, ApiResponse, RestRequest, CallContext) — done 2026-10-17 (behaviour-neutral; byte-for-byte REST golden test)
- [x] Behaviour-change step after 0a: gate + token before body read, lingering drain, new reason phrases, 415 code, 411 chunked, Expect: 100-continue supported after auth (other expectations 417), read deadlines — done 2026-10-17
- [x] Phase 0b: Config enforcement (REST rules; MCP-listener rules moved to Phase 2) — done 2026-10-18
- [x] Phase 1: Shared registry (access_class, integral float normalisation, ADR 0046 amendment) — done 2026-10-18
- [x] Phase 2: `/mcp` skeleton (opt-in, separate bind, routing, era detection, read-only switch) — done 2026-10-18 (initialize, ping, tools/list live; tools/call stubbed)
- [x] Phase 3: Legacy request handling (tools/call live, lanes, golden test, official SDK test) — done 2026-10-18 (tools/call in-process via registry, permit lanes split by stage, 40 s tool budget from body completion; SDK gate passed; proxy test moved to Phase 5)
- [x] Phase 4: Modern request handling (dual-era staging) — done 2026-10-19 (dual-era live; rmcp 3.5.1 modern gate passed)
- [x] Phase 5: Polish, logging, metrics, documentation — done 2026-10-19 (four log targets, `/api/status` mcp_http snapshot, Docker proxy playbook, docs/HOWTO.md §5a, ADR 0045 amendment)
- [x] Known nit: stdio `initialize` advertised `2024-11-05` with no `instructions` and had no `ping` — done 2026-10-20 (stdio negotiates 2025-11-25 / 2025-06-18 through the shared helpers in `src/mcp_registry.rs`, returns `instructions`, answers `ping`; 858de51; ADR 0045 amendment)
- **Follow-ups (planned, not in MVP):**
  - ADR 0047 job_id cell (a spawned enqueue publishes job_id, so the rare MCP tool-budget timeout that stalls before the bridge's own 30 s wait can still report it)
  - Native TLS for the MCP listener (first release uses a reverse proxy for HTTPS)
  - Two-token rotation with overlap (requires daemon restart today)
  - `whatsrust_send_async` tool (non-blocking send, returns job_id immediately)
  - F5 per-token access profiles, built on the `AccessClass` infrastructure
  - Rate limiting for read operations and non-queued network-touching tools (no owner yet; separate effort from F5)

### F4 — Multi-account support ⚪ Planned (needs design)
Run/route multiple WhatsApp accounts from one daemon (or coordinated daemons).
- **Current state:** `bridge_id` field already exists ("for multi-number routing"), BUT the bridge is
  **single-device only** (ARCHITECTURE.md: "No `device_id` column"; single-instance file lock per db_path;
  one `WhatsAppBridge` per process). So multi-account is a substantial new capability, not a config tweak.
- **Open design questions (significant — grill before building):** one daemon hosting N bridges vs N daemons;
  per-account SQLite DB vs shared DB with account scoping; how the instance-lock model changes; how API/MCP/CLI
  select the target account (path prefix? header? tool param?); event-bus routing per account; QR/pairing per
  account; resource/memory implications vs the lean ethos. → Needs its own design doc + ADR(s).
- **Investigation scope (2026-10-15):**
  - **Storage:** one DB holding all accounts (account-scoped tables) vs one DB per account.
  - **Process model:** one process per account vs one process hosting N bridges.
  - **Every existing feature must work per account:** backfill and its pacing (anti-ban limits are
    per account), FTS search, embeddings + the drain worker + per-model purge, the pairing-time
    device name (ADR 0043), outbound queue and rate limiting, SSE events, the storage watchdog.
  - **MCP and API addressing (open):** a tool/route parameter selecting the account vs a
    per-account endpoint (e.g. `/mcp/<account>`, `/api/<account>/...`) vs one MCP server per
    account process. Interacts with F3 (HTTP MCP) and F5 (profiles: per-account access?).
  - **Cross-feature:** F6 (encryption): one key for all accounts or one per account.
- [ ] Investigation + design doc + ADR(s)
- [ ] Implementation

### F5 — Access profiles: read-only vs full access ⚪ Planned (needs investigation)
Run whatsrust with a **read-only** profile (list, read, search, status, history, events, and so
on) as well as the current **full-access** behaviour, so an agent or remote client can be given
read access without the ability to send, delete or change anything.
- **Open questions (investigate before design):**
  - **Where a profile applies:** per process (a startup setting), per token (e.g. a separate
    read-only token alongside the full one), per transport (REST / MCP stdio / MCP HTTP), or a mix.
  - **Classifying every operation:** some are clearly read-only (status, groups, group-info,
    history, search, fetch-status, events, QR?). Others change state on WhatsApp without sending
    a message (mark-read, typing/presence, subscribe, pin/mute/archive, the first
    `group-invite-link` call), and `fetch-history` triggers network fetches with ban-risk pacing.
    Each needs a deliberate classification.
  - **Enforcement point:** the REST router (covers the CLI and the stdio MCP proxy) plus the MCP tool
    registry. F3 adds a server-side access class (Read/Write) in the tool registry, plus the
    `WHATSRUST_MCP_READ_ONLY` switch. Profiles build on the access class, not on the advisory hints.
  - REPL behaviour under a read-only profile, and how this composes with the ADR 0044 request gate
    and tokens.
- **Related:** F3 (tool metadata hook), F4 (per-account profiles?).
- [ ] Investigation + design + ADR
- [ ] Implementation

### F6 — Database encryption at rest ⚪ Planned (needs investigation)
Encrypt `whatsapp.db`, which holds the Signal keys, the message history, the search index and
embeddings.
- **Unlock process (user's initial lean):** the passphrase is **entered when the daemon starts** —
  not from an env var and not from a command-line argument (both leak through process listings,
  shell history and `.env` files).
- **Open questions (investigate before design):**
  - **Mechanism:** SQLCipher via rusqlite's `bundled-sqlcipher` feature (page-level encryption,
    covers the WAL too) vs alternatives. Impact on build, binary size and query speed, and FTS5 /
    `incremental_vacuum` compatibility.
  - **Prompting:** read from the terminal without echo, and before the REPL starts (the REPL owns
    stdin). What happens with no terminal (`make run` from a script, launchd/systemd): fail closed
    with a clear message? A later optional unlock source (e.g. the macOS Keychain)?
  - **Migrating an existing plaintext DB**, wrong-passphrase handling, and passphrase change.
  - **Everything else that holds a copy of the data:** startup and `--rollback` backups
    (`whatsapp.db.backups/`, pre-migration `.bak` files) must be encrypted too, or they defeat the
    point. `--rollback` / `--migrate` need the passphrase. The CLI and the stdio MCP process don't
    open the DB (they talk to the daemon), so they need no passphrase.
  - Key handling in memory, and the threat model (lost/stolen disk vs a live compromised machine).
- **Related:** F4 (one key for all accounts or one per account).
- [ ] Investigation + design + ADR
- [ ] Implementation

### F7 — Richer read tools (names, counts, contacts, groups, pagination) ⚪ Planned (needs viability check)
Enhance read-only surfaces (search, history, status) with contact/sender names, message counts,
contact and group listing/search, and pagination for search results.
- **Cross-feature impact:** New/extended MCP tools (ADR 0046 strict schemas apply); F3 registry (access class: Read); F5 read-only profiles benefit.
- **Investigation scope (2026-10-17):**
  1. **Contact/sender names in search results.** Display contact name (1:1) or group name + sender's contact name (groups) in search/history results.
     - **Current state:** `InboundRow` (storage.rs:945) has `sender_jid` (phone@s.whatsapp.net or LID), not display names. `device.push_name` (storage.rs:61) stores only OUR device's name. `GroupInfo` (bridge.rs:298) has `subject` but no participant names.
     - **What's needed:** Persist push names received in inbound messages, OR fetch on-demand via wa-rs `UserInfo` (wacore/iq/usync.rs:323, has `status` field for about text, but no push_name getter). Group context adds the group subject; sender name resolution is the same problem.
     - **Verdict:** Viable with work — persist names from inbound `Event::Message` (likely carries push_name in protobuf, needs wa-rs source check), or fetch via contacts API.
  2. **Message counts (overall, per-chat, unread); attachment counts.**
     - **Current state:** `messages` table (storage.rs:205) supports `COUNT(*)` for overall and `WHERE chat_jid=?` for per-chat. Attachments countable via `media_refs` (storage.rs:226) or `content_kind` filter. Unread state: bridge receives `Event::ChatRead`/`Mute`/`Archive` (bridge.rs:3717) from app-state sync, but does NOT persist it locally.
     - **What's needed:** Counts are local queries (ready). Unread needs a chat-state table (mute/archive/unread bool, last-read timestamp?) and an insert/update path from the app-state events.
     - **Verdict:** Viable now for counts; viable with work for unread (persist app-state chat status).
  3. **List contacts; search contact names and descriptions (about text).**
     - **Current state:** NO contacts table. Only JIDs seen in `messages` exist. No names or status/about stored.
     - **What's needed:** Define the contact list (app-state contacts sync vs seen-JIDs heuristic). Persist `UserInfo` (wacore/iq/usync.rs:323: `status` = about text). wa-rs `Contacts` feature (contacts.rs:67) has `is_on_whatsapp()` check; unclear if a "get all contacts" API exists (may need app-state sync investigation).
     - **Verdict:** Viable with work — define contact-list source, persist UserInfo, build search index.
  4. **Search groups and group descriptions; resolve member names.**
     - **Current state:** `GroupCache` (bridge.rs:97) stores `GroupInfo` with `subject` (line 299), NO description. wa-rs `GroupMetadata` (groups.rs:95) HAS `description: Option<String>` (line 109), fetched via `get_metadata()` (bridge.rs:1242). Member names: same issue as #1.
     - **What's needed:** Add `description` field to `GroupInfo`, populate from wa-rs. Persist groups in SQLite (currently 5-min in-memory cache). Member name resolution identical to #1.
     - **Verdict:** Viable with work — extend GroupInfo, persist groups, resolve member names (same as #1).
  5. **Pagination for search results (in addition to history, which already has it).**
     - **Current state:** `/api/history` (api.rs:1285) and MCP `whatsrust_history` (mcp.rs:150) both support `before` timestamp pagination. `/api/search` (api.rs:1298) and MCP `whatsrust_search` (mcp.rs:155) do NOT expose `before` in their schemas, BUT the underlying `bridge.search()` (bridge.rs:1607) and `store.search_inbound()` (storage.rs:1415) already accept `before_ts: Option<i64>`.
     - **What's needed:** Add `before` parameter to `/api/search` query string and MCP `whatsrust_search` tool schema (ADR 0046: `additionalProperties: false`, so a schema change is required).
     - **Verdict:** Viable now — implementation ready, needs schema update (REST + MCP).
- [ ] Investigation + design
- [ ] Implementation

### F8 — Documentation overhaul (README = what, operations guide = how, diagrams) ⚪ Planned
Restructure the user-facing docs so each answers one question (added 2026-10-19).
- **README (the what):** say up front that this is an experimental, feature-rich fork (link the
  upstream project) and what it adds; keep the overview, features and pointers; drop the build
  section (not relevant to readers of the README); move CLI/API/config/MCP detail out.
- **Operational guide (the how):** one place for build, configure, run, CLI, REST, MCP (stdio and
  HTTP), embedder, proxies, fail2ban, and every helper script (`scripts/*`, `tools/*`,
  `deploy/proxy/`) with when and how to use it. Decide: extend `docs/HOWTO.md` or split it.
- **CONTRIBUTING:** review and update (Makefile targets, ignored SDK/proxy tests, commit style,
  docs matrix, ADR rule, "ask before changing behaviour" golden-test rule).
- **ARCHITECTURE:** update to the current system (MCP HTTP listener, registry, request pipelines,
  workers) and add diagrams (Mermaid: component map, inbound/outbound data flow, REST and MCP
  request pipelines, backfill/embedding workers).
- [ ] Plan (target structure, what moves where)
- [ ] README + operational guide
- [ ] CONTRIBUTING
- [ ] ARCHITECTURE + diagrams

### F9 — Dev run profiles (debug runs in their own folders) 🟢 Done (2026-10-19; owner step open)
Keep each debug/live run's state out of the project root (added 2026-10-19).
- **Why:** the daemon keeps everything relative to its working directory (`.env` via
  `WHATSRUST_ENV_FILE` or `./.env`, `whatsapp.db*`, `whatsapp.db.backups/`, the instance lock,
  pre-migration `.bak` files) and runs are started from the repo root, so the root collects DBs,
  backups and logs from every run.
- **As built:** one gitignored folder per profile, `runs/<profile>/`, with its own `.env`, DB and
  `logs/`. `scripts/run-profile.sh` (`new [--semantic] [--mcp]`, `list [--tmux]`, `daemon`, `attach`, `cli`,
  `mcp`, `env`, `mcp-on`, `mcp-off`, `mcp-client`) and `make new-profile` (`SEMANTIC=1`, `MCP=1`) /
  `make run-profile` / `make attach-profile`; no Rust change. `mcp-on` / `mcp-off` switch the profile's HTTP MCP
  listener (§5a) in its `.env`: port = the reserved one, else REST+1, else the next free; token = kept, else
  the exported `WHATSRUST_MCP_TOKEN` (validated), else `openssl rand -base64 32`; the `.env` is rewritten with
  bash builtins only (token never printed, never in argv, file stays mode 600); `mcp-off` keeps the token line;
  `mcp-client` prints the URL and a Claude Code config using `${WHATSRUST_MCP_TOKEN}` (2026-10-20). `daemon` runs in the
  current terminal by default; `--tmux` (`USE_TMUX=1` for make) opts in to session `whatsrust-<profile>`
  (attach when interactive; an exit note keeps the pane open until Enter) and `--detach` implies `--tmux`;
  with tmux missing, `--tmux` is an error, never a silent fallback. `list` shows every profile with a TMUX
  column, `list --tmux` only those running in a session (2026-10-19, tmux made opt-in 2026-10-20).
  Docs: HOWTO §3 "Run profiles" (incl. a demo playbook).
- [x] Approach decision — A: profile folders + launcher script (owner, 2026-10-19)
- [x] Script + Makefile target + docs — done 2026-10-19
- [ ] Move existing root run state into `runs/main/` (owner, daemon stopped; HOWTO checklist)

---

## Quick Wins (project health — tracked, NOT in active focus)

From the 2026-06-17 read-only project audit. Listed here so they aren't lost; **not being worked
now** to preserve focus on F1/F2. Pick up opportunistically or schedule deliberately.

| ID | Item | Why | Effort | Priority | Status |
|----|------|-----|--------|----------|--------|
| Q-CI | GitHub Actions CI (fmt + clippy + test + build matrix; nightly pin; wa-rs git-dep cache) | No CI exists; nightly toolchain is fragile (bit us this session) | M | High | ⚪ |
| Q-IT | Integration test harness (`tests/` dir) using the F1/F2 fake seams (Embedder + history-source) | 89 inline unit tests, no integration tests; riskiest worker/pacing/cursor logic untested | L | High | ⚪ (lands with F1/F2) |
| Q-GI | `.gitignore` leak fix (`*.log`, `.mcp.json`, `.env`, session files) | Active leaks: debug log + machine-path `.mcp.json` were untracked | S | High | 🟢 Done 2026-06-17 (commit 6817f52) |
| Q-ENV | `.env.example` + `dotenvy` wiring | Documents config knobs + the fail-closed danger warnings (ADR 0023) | S | High | ⚪ (lands with F1 phase 5) |
| Q-DENY | `cargo-deny` (`deny.toml`) + add to CI | No supply-chain audit for 22 crates.io + 6 git-pinned wa-rs crates | S | Med-High | ⚪ |
| Q-REL | Release pipeline: cross-compiled binaries + SHA256 + CHANGELOG + tags | README claims a 5MB binary but there are no published releases/tags | M | Med | ⚪ |
| Q-SEC | `SECURITY.md` (disclosure policy, scope) | Used in production agent software; raw SQL via `params!` | S | Med | ⚪ |
| Q-DOCS | `docs/INDEX.md` / onboarding guide | 34 .md files; discoverability gap (partly addressed by CLAUDE.md doc matrix) | S–M | Med | 🟡 Partial (CLAUDE.md matrix added 2026-06-17) |
| Q-TRACE | Structured `#[instrument]` spans on `handle_event` / `execute_job` | Debugging "message didn't send / backfill stuck" means grepping flat logs | M | Med | ⚪ |
| Q-SPLIT | Split 900-line `extract_content_inner` / 700-line `handle_event` | Maintainability; both are large | L | Low | ⏸️ Deferred (stack-overflow history — risky; post-F1) |
| Q-PANIC | Panic visibility + recovery: a panic hook that logs the message and location before the release-mode abort, and a documented supervisor (launchd/systemd restart) | A UTF-8 slicing panic (fixed in a763838 on main) killed a 9 h live session on 2026-10-19; `panic = "abort"` gives no log line of its own | S | High | 🟡 Hook done 2026-10-19 (219697d: one `ERROR whatsrust::panic` line, then the default hook); supervisor docs open (F8 operational guide) |
| Q-SLICE | Lint guard against byte-index string slicing (`clippy::string_slice` in `make lint` / CI) | Prevents a repeat of the 2026-10-19 crash; the audit found no other instance | S | High | 🟢 Done 2026-10-19 (219697d: `#![warn(clippy::string_slice)]` in both crate roots; all slices rewritten with non-panicking APIs; 0 warnings in all targets) |
| Q-F2B | fail2ban reference + optional helper script (install filter/jail, plain-text log via `NO_COLOR=1`) | HOWTO §5a documents a filter and jail; a script makes direct (no-proxy) exposure safer | S | Low | ⚪ |
| Q-WARS | wa-rs dependency upgrade check | Pinned at `oxidezap/whatsapp-rust` rev `9e8c70e2` (2026-07-01); upstream HEAD `7c4021d4` seen 2026-10-18 (newer than the pin). Check for new upstream commits/tags, assess breaking changes, run tests + live smoke test per CLAUDE.md "To upgrade wa-rs". Untracked `wa-rs-upgrade-impact-analysis.txt` (2026-06-29, v0.6.0→main) is dated (pre-dates current pin) and advisory only. | M | Med | ⚪ |

---

## Prerequisites

- ✅ **wa-rs adoption (v0.2 → upstream main HEAD `9e8c70e2`)** (ADR 0002/0037) — gated F1/F2; **DONE 2026-07-01 (commit 615d185)**. History sync enabled by default for 1:1-DM tctokens.

---

## Completed

- 🟢 Daemon Ctrl-C hang fix (stdin open terminal) — commit 36ae622
- 🟢 tokio worker stack → 8 MB (fixes inbound-message stack overflow) — commit 0c8228a (ADR 0001)
- 🟢 Dead-code warning silenced — commit b4b0500
- 🟢 Design + ADRs for F1/F2 (ADRs 0001–0025 + consolidated design doc) — commit 6817f52
- 🟢 Doc matrix in CLAUDE.md + `.gitignore` leak hardening — commit 6817f52
- 🟢 Configurable device name (`WHATSRUST_DEVICE_NAME`, default `whatsrust`), fixed at pairing and stored with the session; re-pairing keeps history (ADR 0043). Plus `make release-all` / `make dist`.
