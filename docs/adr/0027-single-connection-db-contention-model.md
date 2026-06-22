# 0027. Single-connection DB contention model

**Status:** Accepted  
**Date:** 2026-06-22

## Context

Store = single `Arc<Mutex<Connection>>` (`storage.rs:242`). `run()` does `spawn_blocking(|| { guard=conn.lock(); f(&guard) })` (storage.rs:299) — lock held ONLY for one synchronous closure, NOT across `.await`. Expensive operations (network I/O, sidecar calls) release the lock between SQLite operations.

**Design review concern:** "embedding drain holds connection through whole batch" → feared multi-second lock holds starving other workers. This is IMPOSSIBLE under the actual pattern: drain batch = embed 64 via sidecar (network I/O, NO lock) → write 64 vectors (one short locked closure). Expensive part holds no lock.

**Real issue:** SQLite is single-writer by design. Multiple workers (outbound, backfill, embedding-drain, snapshot_db) serialize through the mutex. Heavy concurrent activity (backfill inserting batches + embedding drain writing vectors + search reads) queues closures, adding tens of ms latency. Acceptable at single-user scale, but worth measuring.

**ONE genuine multi-second lock-holder:** `snapshot_db` (`storage.rs:309`) runs `rusqlite::Backup::run_to_completion` WHILE holding the guard — pauses all DB operations for backup duration (seconds to minutes on large DB).

## Decision

**Keep SINGLE connection** (no architecture change). Matches existing "short closures, no semaphore needed" intent. Write serialization through one mutex is correct/desirable for SQLite (one writer) + millisecond hold. WAL lets readers proceed between writes.

**Actual fix: chunked transactions.** All bulk operations use per-batch TXs (backfill inserts per-batch, vector writes per-batch, NEVER one mega-TX). Limits lock hold to milliseconds per batch. ADR 0026 batch model already does this for backfill.

**Targeted hardening for snapshot_db:** Move backup to its OWN connection opened against the same file. SQLite Backup API supports a separate read handle:
```rust
let backup_conn = Connection::open(&db_path)?; // separate handle
let backup = rusqlite::backup::Backup::new(&self.conn.lock(), &backup_conn)?;
backup.run_to_completion(...)?;
```
Stops backup from being a global stall. Small targeted change, no pool/dep.

**Deferred (measured trigger, NOT speculative):** Reader-only connection pool + single writer (e.g., `r2d2` / `deadpool-sqlite`) ONLY if search latency under concurrent backfill is OBSERVED bad. Honest tension: heavy semantic search during active backfill adds ~tens of ms (read closures queue behind write closures). Acceptable at single-user scale. Avoids multi-connection complexity + dep until proven needed.

## Consequences

**Positive:**
- Preserves existing single-connection simplicity (no pool coordination, no deadlock risk from multi-connection TX)
- Chunked transactions bound lock hold per batch (milliseconds, not seconds)
- snapshot_db fix removes the ONE genuinely long lock-holder without pool complexity
- WAL mode already allows concurrent readers between write closures

**Negative:**
- Read latency can spike tens of ms during heavy concurrent write activity (backfill + embedding drain) — acceptable trade-off at single-user scale, measurable if it becomes a problem
- Backup still blocks new operations on the backup connection briefly (but not the main conn)

**Future:**
- If MEASURED search latency during backfill exceeds 100ms p95, revisit with read-only connection pool option (benchmark before committing to complexity).
