# whatsrust Feature Roadmap & Status

**Type:** Live document — kept up to date as features land. Update the **Status** of an
item when work starts/completes; add a dated note in its row's detail.
**Last updated:** 2026-09-01

This is the single tracking surface for in-flight and planned work on this experimental
fork. The *why* for each design lives in the ADRs (`docs/adr/`); the *what/how* blueprints
live in `docs/plans/*-design.md`; the *execution order* lives in
`docs/plans/IMPLEMENTATION-ROADMAP.md`. This file tracks *status*.

**Status legend:** 🟢 Done · 🟡 In progress · 🔵 Designed (not started) · ⚪ Planned (not designed) · ⏸️ Deferred

---

## Major Features

### F1 — Historical message fetch + semantic/lexical search 🟡 In progress (Phase 0 GATE passed — GO)
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
  - [x] **M2 — semantic search** — 🟡 **M2.1–M2.5 DONE, M2.6 + E2E validation PENDING** (2026-08-31). Code-complete for sub-milestones M2.1 (embedder sidecar contract), M2.2 (embeddable-text classification), M2.3 (drain worker), M2.4 (semantic search path), M2.5 (multi-model purge). **MVP sidecar BUILT** (`scripts/embedder-sidecar.py`: Python + sentence-transformers MiniLM, 384-dim, multilingual). **M2.6 (config wiring + integration tests) and live E2E validation PENDING.** Semantic search is **opt-in** via `WHATSRUST_EMBEDDER_CMD` and **dormant** (degrades to FTS5 lexical) when sidecar absent/down. New surfaces: API `POST /api/embeddings/purge`, MCP `whatsrust_purge_embeddings` (35 tools total), CLI `purge-embeddings <model_id>`. (ADR 0008/0015/0016/0017/0024/0038/0039)

### F2 — Embedder sidecar (implementation) 🟢 Done (M2.1–M2.5 complete, wiring pending)
Stateless separate binary, pure vectorizer; stdio JSON-RPC v1; transport-neutral
`Embedder` trait (HTTP/localhost as future sibling); multilingual model default.
- **Design:** ADRs 0006, 0015, 0024 (+ 0007/0008/0016/0017/0018/0038/0039). Protocol & validation fully specified.
- **Status note:** M2.1–M2.5 code-complete (2026-08-31). MVP Python sidecar built. M2.6 (full wiring + integration tests) and E2E validation pending.
- [x] `Embedder` trait + stdio JSON-RPC transport (model_info / embed / health, trust-but-verify validation) — DONE M2.1
- [x] Sidecar binary: MVP Python + sentence-transformers (paraphrase-multilingual-MiniLM-L12-v2, 384-dim) — DONE 2026-08-31
- [x] Drain worker integration (ADR 0015), multi-model store + purge (ADR 0017) — DONE M2.3/M2.5

### F3 — MCP streamable HTTP transport (on top of stdio) 🔵 Designed-lite / ⚪ needs design pass
Add MCP Streamable HTTP transport alongside the existing stdio transport — so MCP clients
can connect over HTTP (remote/multiplexed) in addition to spawn-as-child stdio.
- **Current:** `src/mcp.rs` is **stdio-only** (JSON-RPC over stdin/stdout, proxies to the HTTP daemon).
- **Goal:** support the MCP **Streamable HTTP** transport (single endpoint, POST + optional SSE stream)
  as an opt-in alongside stdio; reuse the existing tool dispatch.
- **Open design questions (to grill before building):** auth/token model for the HTTP endpoint;
  bind/port (reuse api.rs raw-TCP server vs separate listener); session management & SSE streaming;
  whether it shares the API server's connection semaphore. → Write an ADR + design pass before impl.
- [ ] Design pass + ADR
- [ ] Implementation

### F4 — Multi-account support ⚪ Planned (needs design)
Run/route multiple WhatsApp accounts from one daemon (or coordinated daemons).
- **Current state:** `bridge_id` field already exists ("for multi-number routing"), BUT the bridge is
  **single-device only** (ARCHITECTURE.md: "No `device_id` column"; single-instance file lock per db_path;
  one `WhatsAppBridge` per process). So multi-account is a substantial new capability, not a config tweak.
- **Open design questions (significant — grill before building):** one daemon hosting N bridges vs N daemons;
  per-account SQLite DB vs shared DB with account scoping; how the instance-lock model changes; how API/MCP/CLI
  select the target account (path prefix? header? tool param?); event-bus routing per account; QR/pairing per
  account; resource/memory implications vs the lean ethos. → Needs its own design doc + ADR(s).
- [ ] Design doc + ADR(s)
- [ ] Implementation

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
