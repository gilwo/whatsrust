# whatsrust Feature Roadmap & Status

**Type:** Live document — kept up to date as features land. Update the **Status** of an
item when work starts/completes; add a dated note in its row's detail.
**Last updated:** 2026-06-17

This is the single tracking surface for in-flight and planned work on this experimental
fork. The *why* for each design lives in the ADRs (`docs/adr/`); the *what/how* blueprints
live in `docs/plans/*-design.md`. This file tracks *status*.

**Status legend:** 🟢 Done · 🟡 In progress · 🔵 Designed (not started) · ⚪ Planned (not designed) · ⏸️ Deferred

---

## Major Features

### F1 — Historical message fetch + semantic/lexical search 🔵 Designed
Per-chat historical backfill (`all` / `since` / `max_messages`) + local FTS5 + vector search.
- **Design:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md`; ADRs 0002–0025.
- **First step:** wa-rs rebase spike (see F-prereq below / ADR 0002).
- **Sub-tracking** (phases from the design doc §Implementation phasing):
  - [ ] 0. wa-rs rebase spike (ADR 0002) — also resolves WebMessageInfo-plaintext question (ADR 0014)
  - [ ] 1. Storage + migration: unified `messages` table, sibling tables, FTS5 + triggers (ADR 0009/0019)
  - [ ] 2. Fetch worker: history-source trait, backfill-job queue, pagination loop, cursor, pacer (ADR 0003/0010/0020)
  - [ ] 3. Search: FTS5 recall + BLOB cosine rerank (ADR 0008/0019)
  - [ ] 4. Embedding sidecar + drain worker (see F2)
  - [ ] 5. Safety + config: daemon-side guards, fail-closed config, `.env`/`dotenvy` (ADR 0021/0022/0023)
  - [ ] 6. API/MCP: trigger/status/cancel, `whatsrust_fetch_history`, SSE progress (ADR 0011)
  - [ ] 7. Storage watchdog (ADR 0012/0013)
  - [ ] 8. Tests (ADR 0025)

### F2 — Embedder sidecar (implementation) 🔵 Designed
Stateless separate binary, pure vectorizer; stdio JSON-RPC v1; transport-neutral
`Embedder` trait (HTTP/localhost as future sibling); multilingual model default.
- **Design:** ADRs 0006, 0015, 0024 (+ 0007/0008/0016/0017/0018). Protocol & validation fully specified.
- **Status note:** Design complete; implementation is phase 4 of F1 but tracked separately since the
  sidecar is its own binary/crate. Includes the minimal fake-sidecar test binary (ADR 0025).
- [ ] `Embedder` trait + stdio JSON-RPC transport (model_info / embed / health, trust-but-verify validation)
- [ ] Sidecar binary: external-API backend AND/OR local ONNX/GGUF backend (model choice = sidecar's concern)
- [ ] Drain worker integration (ADR 0015), multi-model store + purge (ADR 0017)

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

- **wa-rs fork rebase v0.2 → v0.6.0** (ADR 0002) — gates F1/F2. Tracked as F1 phase 0.

---

## Completed

- 🟢 Daemon Ctrl-C hang fix (stdin open terminal) — commit 36ae622
- 🟢 tokio worker stack → 8 MB (fixes inbound-message stack overflow) — commit 0c8228a (ADR 0001)
- 🟢 Dead-code warning silenced — commit b4b0500
- 🟢 Design + ADRs for F1/F2 (ADRs 0001–0025 + consolidated design doc) — commit 6817f52
- 🟢 Doc matrix in CLAUDE.md + `.gitignore` leak hardening — commit 6817f52
