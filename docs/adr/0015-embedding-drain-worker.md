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
