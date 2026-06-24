# 0015. Embedding-drain worker with sidecar-down resilience

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Messages need embeddings for semantic search, but embedding generation is slow and requires the optional sidecar. The drain worker must handle sidecar failures without leaving permanent semantic-coverage holes.

Two failure modes exist:
- **Transport failures** (sidecar down, timeout, protocol error): temporary, recoverable
- **Content rejection** (sidecar refuses specific text, e.g., too long, malformed): persistent per-row

## Decision

**Dedicated embedding-drain worker** (third worker alongside outbound + backfill):
- Woken by `Notify` on new embeddable messages + periodic timer
- Batches `embed_status='pending'` rows (batch size fixed at 64, configurable)
- Continuous drain with inter-batch yield (don't monopolize SQLite connection)

**No embedder configured → worker IDLES entirely** (rows stay `pending` forever, FTS5 fallback always works)

**Sidecar failing (down/timeout/protocol error):**
- Exponential backoff, cap 60s, reset on success
- Rows STAY `pending` (NOT `failed`) — temporary outage must not leave permanent semantic holes
- Transport error does NOT increment row attempt counter
- If persistent sidecar failure, `health()` returns "absent" → worker idles

**Per-row content rejection** (sidecar rejects specific text):
- Increment that row's attempt counter
- Cap 3 attempts → `embed_status='failed'` (terminal state)

## Consequences

**Positive:**
- Transient sidecar outages (restarts, network hiccups) don't poison semantic coverage
- FTS5 baseline keeps search working while embeddings backfill
- Exponential backoff prevents hammering a failing sidecar
- Per-row failure cap stops retry loops on genuinely unembeddable text

**Negative:**
- Persistent sidecar misconfiguration leaves messages in `pending` limbo indefinitely
- No automatic retry after manual sidecar fix (must restart daemon or wait for next embeddable message to wake worker)
- Idle worker during sidecar-down still holds memory/task slot

**Deferred:**
- Storage watchdog (ADR 0013) surfaces growth from unbounded `pending` accumulation; user investigates and fixes sidecar

**Hardening (2026-06-24, v2 review):**
- **Spawn location (B3):** Drain worker spawns as INDEPENDENT long-lived task in `WhatsAppBridge::start` alongside prune/backup tasks (bridge.rs:1956/1965), shares cancel token. NOT in `run_bot_session` (which restarts per reconnect). Rationale: connection-agnostic (DB+sidecar only, never WA).
- **No-embedder idle (M2):** No embedder configured → DON'T spawn drain worker. Configured-but-persistently-failing → after N backoffs drop to Notify-only (no periodic poll).
- **Loading timeout (B4):** Drain tracks time-in-loading; >60s (configurable) continuous loading → treat as error → FTS5 fallback (rows stay pending). >60s model load = misconfig.
- **Decoupled from backfill (R3):** Embedding drain stays FULLY DECOUPLED from backfill in normal range. Set-difference (ADR 0017) remains durable source of truth; any "queue" is IN-MEMORY flow-control only (restart → re-derive, nothing lost). ONE bound: if pending-embedding count grows PATHOLOGICAL (>100k runaway) → pause backfill ENQUEUE until drained (circuit-breaker on runaway, NOT lockstep sync). Rationale: anti-ban pacing makes backfill ~16 msg/s vs embedding ~32-128 msg/s (2-8× headroom) → lag is a tail case.
