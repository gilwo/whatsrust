# 0013. Storage growth watchdog with WAL checkpoint and baseline tracking

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Indefinite message retention (ADR-0012) means database grows unbounded. Users need visibility into storage growth to make informed deletion decisions.

Existing periodic task scaffolding: `bridge.rs:1969-1988` runs `prune_old_data()` every `prune_interval_secs` (default 3600s). Task is cancel-aware (stops on shutdown).

Measuring SQLite size is non-trivial:
- `PRAGMA page_count * page_size` counts pages in main db file, but **ignores WAL** (write-ahead log) which holds uncommitted changes
- WAL files (`whatsapp.db-wal`) can be 10-100 MB before auto-checkpoint
- Accurate measurement = sum of `.db` + `-wal` + `-shm` file sizes on disk

Alert threshold: notify when growth is significant (≥50% vs last baseline), not on every byte change (too noisy).

## Decision

**Reuse periodic task scaffolding** (same interval, same cancel-awareness). Swap `prune_old_data()` delete logic → storage observation.

**Metric:** total on-disk footprint = `stat(whatsapp.db).size + stat(whatsapp.db-wal).size + stat(whatsapp.db-shm).size`.

**Measurement steps:**
1. `PRAGMA wal_checkpoint(PASSIVE)` (flush WAL to main db, non-blocking, doesn't block readers)
2. Measure total footprint (fs::metadata on 3 files)
3. Compare to persisted `last_alerted_size` (stored in `metadata` table or bridge state)
4. If `current >= last_alerted * 1.5` (≥50% growth) → log warning + emit `BridgeEvent::StorageAlert {current_mb, baseline_mb, growth_pct}` (SSE-visible) → update `last_alerted_size = current`

**Baseline tracking:** persist `last_alerted_size` in SQLite `metadata` table (key-value store, reused pattern from existing schema).

## Consequences

**Positive:**
- Accurate measurement (includes WAL, not just page_count)
- Non-blocking (PASSIVE checkpoint, doesn't stall writers)
- Reuses existing periodic scaffolding (no new timer task)
- SSE visibility (UI/CLI can show alerts)
- Reduces noise (only alert on ≥50% growth vs last baseline)

**Negative:**
- PASSIVE checkpoint may not flush full WAL if writers are active (rare, acceptable — next interval will catch up)
- Adds fs::metadata syscalls (3 per interval, ~µs overhead, negligible)

**Future:**
- Configurable growth threshold (50% default, user can set 20% or 100%)
- Emit `BridgeEvent::StorageStats` on every interval (not just alerts) for monitoring dashboards
