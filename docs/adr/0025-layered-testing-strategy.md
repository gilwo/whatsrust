# 0025. Layered testing strategy with two fake seams

**Status:** Accepted  
**Date:** 2026-06-17

## Context

whatsrust testing culture (89 inline unit tests, NO `tests/` dir, NO mocks for wa-rs client):
- Test deterministic logic AROUND the protocol, NOT the protocol itself
- Storage tests use real temp-file DB (`Store::new(&db_path)`)
- Deliberately NEVER test live WhatsApp (E2E is manual only)

Historical fetch + semantic search adds:
- Complex worker logic (backfill loop: pacing, backoff, cursor, cancel, resume)
- Multi-model embedding drain with failure handling
- New migration path (rename-in-place, FTS5 triggers)
- Cosine rerank, per-model purge, search ranking

Need test strategy that verifies risky logic without live WhatsApp, matches existing culture.

## Decision

**Layered testing:**

### 1. Pure logic (unit tests, no fakes)
- Frontier-cursor advance (`oldest_anchor` extraction from `WebMessageInfo`)
- Stop-condition eval (`all` / `since:<ts>` / `max_messages`)
- Anchor extraction from history batch
- Community JID reject (parse + error)
- Config validation + `DANGEROUSLY_*` override gating (ADR 0022)
- Cosine similarity math (dot product, magnitude, edge cases)

### 2. Storage (real temp-DB tests)
- Messages rename-in-place migration (v5→v6→v7) — create old schema, migrate, verify columns
- FTS5 trigger sync (insert/update/delete on `messages` → verify FTS5 reflects change)
- Set-difference drain query (ADR 0017: embeddable messages lacking `(message_id, model_id)` row)
- Embeddings BLOB roundtrip (write float array, read back, verify bitwise identical)
- Search ranking (FTS5 recall → cosine rerank → verify top-k order)
- Per-model purge (delete embeddings for model X, verify model Y untouched, measure bytes reclaimed)
- Watchdog size calc (create .db + -wal + -shm, measure sum, verify checkpoint effect)

### 3. Two fake seams (key new design)

**Seam 1: `Embedder` trait (already exists)**
- Trivial fake impl returns canned vectors (e.g., `vec![0.1, 0.2, ..., 0.384]` deterministic)
- Worker tests: drain loop, batch fetch, write vec + flip status, backoff on transport failure, per-row reject cap 3
- Search tests: FTS5 recall → fake vectors → cosine rerank (verify ranking logic, not real semantic similarity)

**Seam 2: NEW history-source trait** (backfill worker depends on this, NOT `Client` directly)
```rust
trait HistorySource {
    async fn fetch_history_batch(&self, chat_jid: &str, anchor: Anchor, count: u32)
        -> Result<HistoryBatch, HistoryError>;
}

struct HistoryBatch {
    messages: Vec<WebMessageInfo>,
    new_anchor: Option<Anchor>,
    more_remain: bool,
}
```

Fake impl injects:
- Canned `WebMessageInfo` batches (deterministic message sequences)
- Simulated `more_remain` = false (exhaustion)
- Simulated timeout (returns error after N calls)
- Simulated duplicate anchor (no progress)

Unit-test backfill worker:
- Pacing (verify ~4s/batch with jitter, no tight loop)
- Backoff on timeout (exponential, cap 60s)
- Cursor advance (verify frontier moves backward)
- Cancel signal (verify clean shutdown mid-fetch)
- Resume (start with persisted cursor, continue from same anchor)
- Long pause injection (verify occasional 20-90s pauses)

**This seam is ALSO what the rebase spike validates:** real `WebMessageInfo` from wa-rs v0.6.0 feeds same `HistorySource` trait → adapter is narrow, testable.

### 4. Minimal fake-sidecar binary
Tiny separate binary (10-20 lines):
- Reads JSON-RPC `model_info` / `embed` / `health` requests
- Returns deterministic vectors (`vec![0.1 * i as f32; 384]` per text)
- For 1-2 TRUE stdio-transport integration tests (exercises framing + ADR 0024 validation end-to-end)

Rest of sidecar logic tested via in-process fake `Embedder`.

### 5. E2E (real phone → real PDO → real storage)
**Documented MANUAL checklist, NEVER in CI:**
- Pair test phone
- Trigger backfill for known chat
- Verify messages inserted, FTS5 indexed, embeddings drained
- Verify search returns expected results
- Verify cancel/resume works

Consistent with existing "never test live WhatsApp" culture.

## Consequences

**Positive:**
- Matches existing test culture (inline tests, real temp-DB, no live WA)
- Two fake seams (Embedder + HistorySource) enable unit-testing riskiest worker logic (pacing, backoff, cursor, cancel)
- History-source seam is ALSO the rebase validation boundary (good design, not just testability)
- Minimal fake-sidecar binary verifies stdio+JSON-RPC plumbing without heavy dependency
- Storage tests use real SQLite (catches FTS5 trigger bugs, migration errors)
- Pure logic tests are fast (no I/O, no fakes)

**Negative:**
- Two fake traits add abstraction cost (real code must implement traits, not just `Client` directly)
- Manual E2E checklist is maintenance burden (must update when features change)
- No coverage of wa-rs protocol changes (but deliberate — not testing the protocol, testing our logic around it)
- Fake-sidecar binary must be kept in sync with real sidecar protocol (small, but duplicated)

**Rejected:**
- **Unit-only (B)** — leaves riskiest worker/pacing/validation logic unverified (too many "trust me" paths)
- **Mock WhatsApp server (C)** — huge effort (protocol complexity, encryption, state machine), out of proportion to test culture, high maintenance burden

**Deferred:**
- Property-based testing (e.g., cosine similarity commutativity, FTS5 trigger sync invariants) — quickcheck-style; overkill for v1 but nice future enhancement
- Benchmarking (embeddings drain throughput, cosine rerank latency) — no perf requirements yet, premature
