# 0031. Single-integer schema version invariant

**Status:** Accepted  
**Date:** 2026-06-22

## Context

Schema versioning trade-offs:
- **Single integer:** Simple (migration-required ⟺ version bump), but forces full migration ceremony for any change.
- **MAJOR.MINOR tiers:** Allows additive changes (MINOR bump, no migration) vs breaking (MAJOR bump, full ceremony). Cost: runtime code must branch on "does column X exist?" forever, combinatorial test matrix.

**Fence case:** Optional indexes for performance. If tied to version, deploying a performance patch requires full migration ceremony. If NOT tied to version, how do we track/apply them?

## Decision

**INVARIANT: ANY schema change requiring a migration step (breaking OR mandatory) ⇒ version bump ⇒ full ceremony (ADR 0028/0029/0030).**

**NO MINOR schema tier in v1.** Resolved the additive-optional fence by REMOVING the ambiguous middle:
- If code DEPENDS on an additive field/table → it's migration-required → full version bump. (Columns/tables are schema STATE, never "sneaked in.")
- If code does NOT depend on it → it's not a versioned change at all.

**Optional PERFORMANCE structures (indexes ONLY):** `CREATE INDEX IF NOT EXISTS` at startup, OUTSIDE the version system (no version bump, no migration TX, no backup). Idempotent. This is the "ship a fast patch that runs on a less-migrated DB" outlet — scoped to non-state-bearing optimizations. Base schema already uses `IF NOT EXISTS` throughout.

**Single integer `CURRENT_SCHEMA_VERSION` retained** (like today's =7).

**MAJOR.MINOR split + runtime code-flow branching DEFERRED** behind a measured trigger (frequent tiny column adds making ceremony painful) — NOT built speculatively. Avoids combinatorial test matrix + permanent "does column X exist?" branches (tar pit).

## Consequences

**Positive:**
- Simplest possible versioning (one number, one rule: migration ⟺ bump)
- No runtime branching on version tiers (every codepath assumes current schema)
- Indexes-as-idempotent-startup-DDL provides performance outlet without versioning overhead
- Test matrix stays linear (one version active, not N×M MAJOR×MINOR combos)

**Negative:**
- Additive state-bearing changes (new nullable column) require full ceremony even if backward-compatible — acceptable trade-off for simplicity until ceremony becomes painful (not yet)
- Indexes outside versioning can drift (one DB has index, another doesn't) — acceptable, indexes don't affect correctness

**Future:**
- If MEASURED pain from frequent additive changes (>3 per quarter), revisit MAJOR.MINOR split. Document the transition criteria: runtime branches contained to a migration-shim layer, test matrix expanded deliberately.
