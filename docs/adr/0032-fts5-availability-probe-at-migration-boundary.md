# 0032. FTS5 availability probe at migration boundary

**Status:** Accepted  
**Date:** 2026-06-22

## Context

rusqlite `features=["bundled","backup"]` is UNCONDITIONAL (`Cargo.toml:32`) — no pkg-config/system-sqlite path exposed. Bundled SQLite compiles amalgamation w/ `SQLITE_ENABLE_FTS5` on by default. Empirically probed earlier (CREATE VIRTUAL TABLE...fts5 succeeded).

**Only missing-FTS5 path:** maintainer deliberately editing Cargo.toml to disable bundled feature + system SQLite lacks FTS5 (rare, self-inflicted).

**Design review concern:** "FTS5 unavailable → cryptic mid-migration crash."

## Decision

**Cheap one-time FTS5 probe at the v7→v8 migration boundary** (before any FTS5 DDL):

```rust
// Probe FTS5 availability before migration
conn.execute_batch("CREATE VIRTUAL TABLE temp.__fts5_probe USING fts5(x);")?;
// Temp table auto-drops on disconnect, no cleanup needed
```

OR use `sqlite_compileoption_used('ENABLE_FTS5')` pragma.

**Absent → actionable error:** "SQLite built without FTS5 support; keep default `bundled` rusqlite feature in Cargo.toml" + leave DB atomically at prior version (no partial migrate). Migration TX rolls back, version unchanged.

**Treated as deliberate-misbuild GUARD, NOT a supported degraded mode.** Default build always has FTS5; probe catches rare misconfiguration before corrupting schema.

**Rejected alternatives:**
- **Do nothing (C):** Cryptic "no such module: fts5" mid-migration → unclear what to fix.
- **Vectors-only fallback (D):** Over-engineered for self-inflicted misconfig; violates FTS5-always-on baseline.

## Consequences

**Positive:**
- Clear error message on rare misconfiguration (points at fix: keep `bundled` feature)
- Probe is subsecond (CREATE temp table trivial)
- Fails before corrupting schema (migration TX rolls back)

**Negative:**
- Adds one probe per migration (negligible cost)
- Only catches FTS5 absence, not other compile-time options (acceptable — FTS5 is the only hard dependency)

**Future:**
- If other compile-time features become hard deps (e.g., JSON1), add similar probes.

**Hardening (2026-06-24, v2 review):**
- **Startup probe (M4):** Add cheap FTS5 probe at STARTUP (after version check) via `SELECT 1 FROM messages_fts LIMIT 0`, complements migration-boundary probe. Catches later bundled→system-sqlite swap. Probe order: startup check if migrated DB → fast-path normal; migration-boundary check if schema change needed.
