# 0036. Generic metadata KV table for singleton scalars

**Status:** Accepted  
**Date:** 2026-06-22

## Context

ADR 0013 storage watchdog needs to persist `last_alerted_size` baseline. Claimed to "reuse existing `metadata` table pattern" — but no such table exists in `storage.rs` today. Must be created.

**Options:**
1. **Generic KV table** `metadata(key TEXT PRIMARY KEY, value TEXT)` — idiomatic SQLite, extensible without migrations for future scalars.
2. **Purpose-specific table/column** `watchdog_state(last_alerted_size)` — over-fitted for one scalar.

## Decision

**Generic `metadata(key TEXT PRIMARY KEY, value TEXT)` KV table** — idiomatic SQLite, home for watchdog baseline + future small singletons (config overrides, feature flags, last-cleanup timestamps). Extensible without per-scalar schema changes.

**Created in the F1 MAJOR migration** (v7→v8, unified-messages, ADR 0009/0028). Rides the migration ceremony, no separate version bump per ADR 0031 invariant.

```sql
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY,
    value TEXT
);
```

**NOTE:** Migration-pin (ADR 0030) stays a SIDECAR FILE, NOT this table — the pin must work when this table doesn't yet exist (after rollback to a pre-metadata version). Control-plane flag can't live in the data it gates.

**Watchdog behavioral note (ADR 0013 revision):** Missing baseline row (first run after table created) → SEED current size silently, NO alert (else first tick false-alerts ∞% growth). Baseline present → compare. Query:
```sql
SELECT value FROM metadata WHERE key='watchdog_last_alerted_size'
-- NULL → seed current, INSERT INTO metadata VALUES ('watchdog_last_alerted_size', ?current)
-- present → compare
```

## Consequences

**Positive:**
- Extensible for future singleton scalars (no per-scalar migration)
- Idiomatic SQLite pattern (many projects use KV table for app state)
- Created once in F1 migration (no separate ceremony per ADR 0031)

**Negative:**
- TEXT values require app-level parsing (serialize int/bool/timestamp as string) — acceptable, keeps schema simple

**Future:**
- If many structured singletons accumulate (>10 keys), consider purpose-specific tables — but defer until proven needed.
