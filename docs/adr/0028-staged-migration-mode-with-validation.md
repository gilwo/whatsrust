# 0028. Staged migration mode with pre-migration backup and validation

**Status:** Accepted  
**Date:** 2026-06-22

## Context

Today `run_schema_migrations` runs IMPLICITLY in `Store::new` (`storage.rs:286`), called via `Store::new(...).expect()` (`bridge.rs:829`) → migration failure = PANIC. No backup, no WAL handling, no validation. Destructive operations like `ALTER TABLE ... RENAME` are irreversible without backup-restore (up to hours of received messages lost on rollback).

**Design review blocking issue:** "one-way migration door" — once committed, rolling back to v7 code requires manual backup-restore with potential data loss.

## Decision

**Staged startup when version < CURRENT:**

1. **Open DB (PRAGMAs only, NO migration) → read user_version.**  
   - `version == CURRENT` → normal fast start (no backup).  
   - `version < CURRENT` → ENTER MIGRATION MODE (app NOT fully started).

2. **PRAGMA wal_checkpoint(TRUNCATE)** — flush WAL into main file (page-level, schema-agnostic). Ensures backup captures a single consistent file, not db+wal fragments.

3. **Backup → `whatsapp.db.pre-migration-v<from>-<ts>.bak`** via existing `snapshot_db` (SQLite Backup API → single consistent .db, WAL folded in). **FAIL-CLOSED:** backup fails ⇒ ABORT migration (exit non-zero, explained error).

4. **Run migration in TX.** `user_version` bump is the LAST statement (atomic — crash mid-migration → TX rollback → DB at v_old → clean re-migrate on next start).

5. **Validate** (see ADR 0029 for criteria).

6. **Pass → full start; fail → halt + instruct** (see ADR 0030 for circuit-breaker + rollback procedure).

**Migration mode visibility:** Daemon stays DARK in v1 (clients connection-refused; migrations are fast; don't build partial-API speculatively). Minimal 503 `{"status":"migrating"}` health endpoint deferred to future.

**Validation/any-stage failure handling:** Failed migration TX already auto-rolls-back (DB stays v_old). Only "committed but bad" reaches validation step → human decides, not daemon doing automated file surgery that could itself fail. Fail-closed (daemon down-but-recoverable > up-on-corrupt-schema), matches instance-lock ethos.

**Rollback procedure:** (documented, or `--rollback` helper — see ADR 0030):  
1. Stop daemon  
2. Copy `.bak` over `whatsapp.db`  
3. DELETE `whatsapp.db-wal` AND `-shm` (CRITICAL: stale post-migration WAL would replay onto restored old file → corruption)  
4. Start old binary on restored DB

Point-in-time: messages received after upgrade are LOST on override (logged + documented).

## Consequences

**Positive:**
- Pre-migration backup provides safe rollback path (no multi-hour data loss)
- wal_checkpoint(TRUNCATE) ensures backup is consistent single file
- Atomic TX commit (version bump last) → crash mid-migration leaves DB clean at prior version
- Fail-closed validation prevents running on corrupt schema
- Staged mode separates concerns: backup → migrate → validate → run

**Negative:**
- Adds startup latency for migration (TRUNCATE + backup can be seconds on large DB) — acceptable once per schema version
- Backup consumes disk (old .bak files accumulate) — document retention policy (keep most recent, delete older after confirming new version stable)
- Manual rollback procedure required (no auto-rollback) — prevents daemon from corrupting DB further during automated recovery attempts

**Future:**
- Auto-prune old .bak files (keep N most recent, configurable)
- 503 health endpoint during migration mode (low priority — migrations are fast)
