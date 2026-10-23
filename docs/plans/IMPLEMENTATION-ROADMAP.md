# whatsrust Roadmap

**Type:** Live document: what is planned, in what order, and what is done. Update it as work
advances (see [Keeping it current](#keeping-it-current)).
**Last updated:** 2026-10-23

Where else to look:
- **Feature detail (breakdown, issues encountered, ADR links):** `docs/plans/FEATURES.md`.
- **What changed, and when:** `CHANGELOG.md`.
- **Why (decisions):** `docs/adr/` (start at `0000-index.md`). **Blueprints:** `docs/plans/*-design.md`.

**Status legend:** 🟢 Done · 🟡 In progress or partly done · 🔵 Designed (not started) · ⚪ Planned / open · ⏸️ Deferred · 👤 Owner action

## Contents

- [Keeping it current](#keeping-it-current)
- [Feature map](#feature-map)
- [Priority backlog](#priority-backlog)
- [Project health (not ranked)](#project-health)
- [Completed](#completed)

<a id="keeping-it-current"></a>
## Keeping it current

- **Work starts or lands:** update the Status cell of its backlog row.
- **A feature's status changes:** update its row in the feature map (and its heading in FEATURES.md).
- **Work is finished:** move the backlog row to [Completed](#completed) with the date and the commit,
  and add a line to `CHANGELOG.md`.
- **New open work:** add one backlog row (each open item lives in one row only; its detail lives in
  FEATURES.md or the source it names). Re-rank with the owner.

<a id="feature-map"></a>
## Feature map

| ID | Feature | Status | In one line |
|---|---|---|---|
| [F1](FEATURES.md#f1) | Historical message fetch + search | 🟢 Done; open items | Per-chat history backfill plus local lexical (FTS5) and opt-in semantic search. |
| [F2](FEATURES.md#f2) | Embedder sidecar | 🟢 Done; follow-ups open | Stateless vectorizer process (Rust/candle; bge-m3 or MiniLM) that feeds semantic search. |
| [F3](FEATURES.md#f3) | MCP Streamable HTTP transport | 🟢 Done; follow-ups open | Opt-in `/mcp` HTTP listener beside stdio: dual-era, own port and token. |
| [F4](FEATURES.md#f4) | Multi-account support | ⚪ Planned | Run several WhatsApp accounts from one daemon or from coordinated daemons. |
| [F5](FEATURES.md#f5) | Access profiles | ⚪ Planned | Read-only and full-access profiles, so an agent can read without sending or changing anything. |
| [F6](FEATURES.md#f6) | Database encryption at rest | ⚪ Planned | Encrypt `whatsapp.db` and its backups; the passphrase is entered at daemon start. |
| [F7](FEATURES.md#f7) | Richer read tools | ⚪ Planned | Sender names, counts, contacts, groups and search pagination in the read surfaces. |
| [F8](FEATURES.md#f8) | Documentation overhaul | 🟡 Started (planning docs + CHANGELOG) | README = the what, an operations guide = the how, ARCHITECTURE with diagrams. |
| [F9](FEATURES.md#f9) | Dev run profiles | 🟢 Done; owner step open | Each debug run keeps its DB, logs and `.env` in `runs/<profile>/`. |

<a id="priority-backlog"></a>
## Priority backlog

The one ordered list of open work. It was set with the owner on 2026-10-19 and merged on
2026-10-23 with the suggested feature order and the open follow-up lists. **Row order within a
priority is the pick-up order.** Detail lives where the "Source" column points.

**Priority:** P0 = stability of live runs · P1 = next up · P2 = planned · P3 = later / when needed.
**Effort:** S / M / L (relative size, not a time estimate).

| P | Item | Effort | Source | Depends on | Why this rank | Status |
|---|---|---|---|---|---|---|
| P0 | Rebuild and restart the live daemon on the fixed binary | – | ops | – | a running pre-fix binary can still abort on a long multi-byte reply | 👤 open |
| P0 | Supervisor restart (launchd/systemd) documented | S | Q-PANIC | – | nothing restarted the daemon after the 2026-10-19 crash | ⚪ (planned with the F8 operations guide) |
| P1 | On-demand media download for stored history (REST/CLI/MCP, from `media_refs`; an expired link can be recovered through a re-upload by the phone) | M | F1 open items; ADR 0005 | – | history rows keep media refs but no bytes; agreed as next | ⚪ |
| P1 | Re-pair bug: logout + re-pair keeps the old device's app-state and Signal data (`clear_device` deletes only the device row and name), so the new device's app-state sync looks broken (no push name, no archive/read-state events). Confirm with a pairing from an empty DB, then wipe the device-scoped tables on logout (keep messages, `lid_pn_mapping`, `tc_tokens`) | M | F1 issues; HOWTO §3a | 👤 confirm first | the documented re-pair path breaks archive seeding, the push name, presence and (later) address-book names | ⚪ |
| P1 | Live check of archive-key seeding | – | F1 open items | re-pair bug | the code is done (ec3691f); the check needs a device whose app-state sync works | 👤 open |
| P1 | Rate limiting for network-touching reads (`groups`, `group_info` cache misses) and chat-management via MCP | M | F3 open items | – | an LLM loop can hammer WhatsApp; anti-ban is a core promise | ⚪ |
| P1 | wa-rs upgrade check | M | Q-WARS | – | the pin is from 2026-07-01 and upstream has moved | ⚪ |
| P1 | Move the root run state into `runs/main/` | – | F9 | daemon stopped | protects the main DB while experimenting | 👤 open |
| P2 | `@lid` DM split: re-key pairing-package DM rows stored under `@lid` once the LID mapping is known (lead: HistorySync field 15) | M | F1 open items | – | one DM split on the 2026-10-22 re-pair; builds the LID→phone resolution that sender names need | ⚪ |
| P2 | Sender information on messages (F7 item 1): display name and phone number in `history` / `search` | M | F7 item 1 | `@lid` split; re-pair bug for address-book names | owner request 2026-10-22; push names can ship first | ⚪ |
| P2 | The other F7 items: search pagination, counts, groups, contacts | L | F7 items 2–5 | sender information (for names) | user value; viable item by item; pagination is ready to build | ⚪ |
| P2 | README (the what) + fork notice + operational guide (the how) | M | F8 | after the items above | lowered from P1 on 2026-10-23 (owner): the docs change once, after the fetch and names work | ⚪ |
| P2 | ARCHITECTURE update with Mermaid diagrams | M | F8 | README split | follows the README split | ⚪ |
| P2 | CONTRIBUTING update | S | F8 | – | align with Makefile targets, ignored tests, commit and ADR rules, and the doc-update checklist | ⚪ |
| P2 | Sidecar auto-respawn | M | F2 open items | – | semantic search silently stays off after a sidecar crash | ⚪ |
| P2 | M2 review: bounded sidecar line read (reviewer M6) | S | F2 issues | – | a runaway sidecar could exhaust memory before the 16 MiB cap fires; Med → P2 (2026-10-23) | ⚪ |
| P2 | M2 review: timeout on `fetch_embeddings_for_candidates` (reviewer H2); apply to the whole single-connection storage layer, not only the semantic path | M | F1 open items | – | the query embed is bounded, the candidate fetch is not; Med → P2 (2026-10-23) | ⚪ |
| P2 | ADR 0047 job_id cell (a spawned enqueue publishes the `job_id`) | M | F3 open items | – | rare now that the tool budget starts after the body | ⚪ |
| P2 | `enqueue_and_wait` skips `Lagged` broadcast errors (re-check the job row on timeout) | S | ARCHITECTURE (event bus) | – | can report a timeout for a job that was sent | ⚪ |
| P2 | `/api/contact` keeps a leading `+` and adds its own, producing `TEL:++…` (strip it in `handle_contact`) | S | REST | – | wrong vCard phone format | ⚪ |
| P2 | One parser for boolean env vars (`WHATSRUST_SKIP_HISTORY_SYNC` and the `WHATSRUST_DANGEROUSLY_*` flags accept `1`/`true` but not `yes`; switch them to `bool_env_var`, which accepts `1 true TRUE yes YES`) | S | config | – | `yes` works for some flags only | ⚪ |
| P2 | Flaky `test_drain_worker_max_batch_clamp` → tokio paused clock | S | F2 issues | – | load-dependent test failure | ⚪ |
| P3 | fail2ban helper script | S | Q-F2B | – | only for direct exposure; the proxy recipe covers most setups | ⚪ |
| P3 | `whatsrust_send_async` MCP tool | S | F3 open items | – | only if an agent needs scheduling or link previews | ⚪ |
| P3 | Two-token rotation for the MCP listener | M | F3 open items | – | needs a restart today; low frequency | ⚪ |
| P3 | Native TLS for the MCP listener | L | F3 open items | – | the tested reverse-proxy recipe covers it; until then a non-loopback bind sends the token in cleartext | ⚪ |
| P3 | F5 per-token access profiles | L | F5 | – | builds on AccessClass; not needed for a single owner | ⚪ |
| P3 | HEAD `/mcp` returns 405 with a body | S | F3 open items | – | cosmetic; the connection closes | ⚪ |
| P3 | Test hygiene: `#[cfg(test)]` capture hooks in `log_request`/`accept_loop`; a storage test that scans source text | S | F3 open items | – | test-only code inside production functions; brittle text scan | ⚪ |
| P3 | M2 review, low: cosine epsilon `< 1e-9` (M4); `max_input_tokens` floor (M2); purge non-INCREMENTAL warning in the response (M1); poison-pill-at-tail log (M3); nits L1–L4 | S | F1, F2 open items | – | defense in depth; Low/Nit → P3 (2026-10-23) | ⚪ |
| P3 | `MsgSecretStore` for history-delivered edits, reactions and poll votes | M | F1 issues | – | added 2026-10-23 (a Phase 0 deferral no list tracked); not ranked by the owner | ⚪ |
| P3 | F4 multi-account and F6 encryption at rest: investigation + design | L | F4, F6 | – | not ranked by the owner; each needs a design first | ⚪ |

<a id="project-health"></a>
## Project health (not ranked)

From the 2026-06-17 read-only project audit. These are tracked but **not in the backlog** until they
are ranked with the owner (the audit's own priority is shown). Items already ranked point to the
backlog.

| ID | Item | Why | Effort | Audit priority | Status |
|----|------|-----|--------|----------|--------|
| Q-CI | GitHub Actions CI (fmt + clippy + test + build matrix; nightly pin; wa-rs git-dep cache) | No CI exists; the nightly toolchain is fragile | M | High | ⚪ |
| Q-IT | Integration test harness (`tests/` dir) using the F1/F2 fake seams (Embedder + history source) | Only inline unit tests; no `tests/` dir yet (checked 2026-10-23) | L | High | ⚪ |
| Q-DENY | `cargo-deny` (`deny.toml`) + add to CI | No supply-chain audit for the crates.io and git-pinned wa-rs crates | S | Med-High | ⚪ |
| Q-REL | Release pipeline: cross-compiled binaries + SHA256 + CHANGELOG + tags | README claims a 5 MB binary but there are no published releases or tags; `CHANGELOG.md` exists since 2026-10-23 | M | Med | ⚪ |
| Q-SEC | `SECURITY.md` (disclosure policy, scope) | Used in production agent software; raw SQL via `params!` | S | Med | ⚪ |
| Q-DOCS | `docs/INDEX.md` / onboarding guide | Many .md files; discoverability gap (partly addressed by the doc matrix, now in `AGENTS.md`) | S–M | Med | 🟡 Partial (CLAUDE.md matrix 2026-06-17, change checklist 2026-10-23) |
| Q-TRACE | Structured `#[instrument]` spans on `handle_event` / `execute_job` | Debugging "message didn't send / backfill stuck" means grepping flat logs | M | Med | ⚪ |
| Q-SPLIT | Split the 900-line `extract_content_inner` / 700-line `handle_event` | Maintainability | L | Low | ⏸️ Deferred (stack-overflow history — risky) |
| Q-PANIC | Panic visibility + recovery: a panic hook that logs the message and location before the release-mode abort, and a documented supervisor (launchd/systemd restart) | A UTF-8 slicing panic (fixed in a763838) killed a 9 h live session on 2026-10-19; `panic = "abort"` gives no log line of its own | S | High | 🟡 Hook done (219697d); supervisor docs → backlog P0 |
| Q-F2B | fail2ban reference + optional helper script (install filter/jail, plain-text log via `NO_COLOR=1`) | HOWTO §5a documents a filter and jail; a script makes direct exposure safer | S | Low | → backlog P3 |
| Q-WARS | wa-rs dependency upgrade check | Pin `9e8c70e2` (2026-07-01); upstream HEAD `7c4021d4` seen 2026-10-18. Follow AGENTS.md "To upgrade wa-rs". The untracked `wa-rs-upgrade-impact-analysis.txt` (2026-06-29) pre-dates the pin and is advisory only | M | Med | → backlog P1 |

<a id="completed"></a>
## Completed

Newest first. Detail: FEATURES.md (features) and `CHANGELOG.md` (every change). Dates are the
recorded milestone dates; `CHANGELOG.md` sections use commit dates, which can be a few days later.

| Date | What | Feature / source | Reference |
|---|---|---|---|
| 2026-10-23 | Planning docs restructured (FEATURES = detail, this roadmap = map/order/completed), `CHANGELOG.md` started, doc-update checklist; project instructions split into `AGENTS.md` (tool-neutral) + `CLAUDE.md` (imports it) | F8 | – |
| 2026-10-22 | fetch-history: store the pairing history and late on-demand answers; live-verified 2026-10-22 | F1 | eed8e19; ADR 0048 |
| 2026-10-22 | fetch-history: anchors from archive/read-state syncs; 180 s wait for a sleeping phone | F1 | ec3691f; ADR 0003 amendment |
| 2026-10-22 | `run-profile.sh clear-data`: empty a profile's message data, keep the pairing | F9 | b9c7dd2 |
| 2026-10-21 | fetch-history: fail fast without an anchor; re-seed empty cursors | F1 | e140361; ADR 0003 amendment |
| 2026-10-20 | Stdio MCP `initialize` negotiates 2025-11-25/2025-06-18, returns instructions, answers `ping` | F3 | 858de51 |
| 2026-10-20 | Run profiles: tmux made opt-in; `mcp-on` / `mcp-off` / `mcp-client` | F9 | – |
| 2026-10-19 | F3 MCP Streamable HTTP listener complete (Phases 0a–5) | F3 | ADR 0045–0047 |
| 2026-10-19 | Dev run profiles (`runs/<profile>/`) | F9 | f953948 |
| 2026-10-19 | Panic hook (one `ERROR whatsrust::panic` line before the abort) and the byte-index string-slicing lint (Q-SLICE: `#![warn(clippy::string_slice)]` in both crate roots, 0 warnings in all targets); the UTF-8 slicing crash fixed on main | Q-PANIC, Q-SLICE | 219697d, a763838 |
| 2026-10-15 | MCP argument validation (declared arguments only); sync-send timeout returns `job_id`; `whatsrust_send` MCP schema fixed | F3 | ADR 0046, 0047 |
| 2026-10-14 | REST request gate (Origin/Host/Content-Type) | security | 1fda7ee; ADR 0044 |
| 2026-10-13 | Device name fixed at pairing (`WHATSRUST_DEVICE_NAME`); `make release-all` / `make dist` | – | ADR 0043 |
| 2026-10-05 | M2 semantic search live-validated (bge-m3 hybrid search on the real account) | F1, F2 | ADR 0042 |
| 2026-09-30 | M2 code-complete: native Rust sidecar with bge-m3 default, hybrid recall | F1, F2 | ADR 0039–0041 |
| 2026-09-02 | M2.1–M2.6 (sidecar contract, embeddable text, drain worker, semantic path, purge, config wiring); `.env.example` completed (Q-ENV, with `dotenvy` from M1.2) | F1, F2 | ADR 0015–0017, 0038 |
| 2026-07-20 | M1 complete: fetch + FTS5 lexical search, live E2E | F1 | ADR 0003–0036 |
| 2026-07-01 | Phase 0 gate GO: wa-rs upstream HEAD `9e8c70e2` adopted; history sync on for 1:1-DM tokens | F1 | 3df4ce9; ADR 0002, 0037 |
| 2026-06-17 | Design + ADRs for F1/F2 (ADRs 0001–0025 + the consolidated design doc); doc matrix in CLAUDE.md; `.gitignore` leak hardening (`*.log`, `.mcp.json`, `.env`, session files; Q-GI) | F1, F2 | 7c4c403 |
| 2026-06-15 | Daemon Ctrl-C hang fix (stdin an open terminal); tokio worker stack → 8 MB (inbound stack overflow) | – | 36ae622, 0c8228a; ADR 0001 |
| 2026-06-14 | Dead-code warning silenced | – | b4b0500 |
