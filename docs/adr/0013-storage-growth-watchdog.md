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
4. If `current >= last_alerted * 1.5` (≥50% growth) → log warning → **update `last_alerted_size = current` FIRST, then** emit `BridgeEvent::StorageAlert {current_bytes, baseline_bytes, growth_pct}` (SSE-visible) **only if the reset write succeeded**.

> **Implementation note (2026-07-16, M1.4):** the reset is persisted *before* the SSE emit. If the `set_metadata` reset fails (e.g. broken connection), the alert is **not** emitted and the baseline stays stale — the alert then defers to the next tick where the write succeeds, rather than re-emitting an identical alert every interval. The event carries **bytes** (`current_bytes`/`baseline_bytes`), not MB; clients format. The `growth_pct` comparison is a pure `watchdog_should_alert(current, baseline)` (integer-only, overflow-safe; returns `None` when `baseline == 0` or `current <= baseline`).

**Baseline tracking:** persist `last_alerted_size` in SQLite `metadata` table (key-value store, created in F1 migration, see ADR 0036). **Behavioral note (revised 2026-06-24, v2 review B2):** Seed baseline DETERMINISTICALLY as final step of migration (synchronous, before daemon accepts work), NOT lazily on first tick (which races the hourly interval + captures migration bloat). Ensures baseline reflects clean post-migration state. **Runtime fallback (M1.4):** as belt-and-suspenders, if the baseline key is ever absent/zero at a watchdog tick it is seeded silently from the current footprint (no alert) — the migration seed remains the primary path; this only guards the theoretical case of a v8 DB that never went through the seeding migration step.

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
- **Baseline resets only on alert, not on shrink (M1.4):** after `prune_old_data`'s `incremental_vacuum` reclaims space, the baseline stays at the last-alerted (higher) value, so a subsequent regrowth is measured against the pre-shrink baseline. A large regrowth relative to the *shrunken* size may not re-alert until it exceeds 1.5× the old baseline. Acceptable for a growth-*awareness* signal (not a quota); revisit (e.g. also lower the baseline on significant shrink) only if false-negatives prove to matter.

**Future:**
- Configurable growth threshold (50% default, user can set 20% or 100%)
- Emit `BridgeEvent::StorageStats` on every interval (not just alerts) for monitoring dashboards
