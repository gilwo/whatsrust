# 0030. Migration circuit-breaker with rollback and migrate flags

**Status:** Accepted  
**Date:** 2026-06-22

## Context

Failed migration → rollback → restart → auto-migrate → fail again → ∞ crash loop. Need a way to:
1. **Prevent re-migration after a failed attempt** (circuit-breaker)
2. **Support safe manual rollback** (restore .bak + old binary)
3. **Allow retry** (force past breaker if user fixes root cause)

**Model choice:**
- **M1:** rollback pairs DB-downgrade WITH running the OLDER binary (old binary on its-CURRENT DB → no migration → runs). The breaker (pin) stops the NEW binary from auto-re-migrating.
- **M2 (rejected):** new binary runs in degraded mode on old schema. Complex (every codepath checks version), fragile (old schema may lack required fields).

## Decision

**Model M1.** Rollback = restore DB + run old binary. Breaker = sidecar pin file.

**Pin file:** `whatsapp.db.migration-pin` (NOT a DB row — after rollback the metadata table may not exist; control-plane flag can't live in the data it gates; consistent w/ `.instance.lock` / `.bak` conventions).

**Pin fields:**
- `state`: "failed" | "rolled_back"
- `pinned_version`: schema version after rollback
- `blocked_target`: schema version that failed
- `created_at`: timestamp
- `reason`: error message

**Pin lifecycle:**
1. Written ONLY by a FAILED migration (state=failed) or validation (ADR 0029).
2. `--rollback` updates it to state=rolled_back after restoring.
3. `--migrate` clears/ignores it (force retry).
4. Startup reads it and applies decision table (below).

**`--rollback` flag:** Valid ONLY when pin present (i.e., only after a failed migration). No pin → refuse ("--rollback only valid after a failed migration; none pending"). Closes footgun of rolling back a healthy DB.

**Rollback steps (maintenance subcommand):**
1. Verify pin exists (state=failed)
2. Find most-recent `.pre-migration-v*.bak` (NO chaining — "last known working" only)
3. Copy .bak → whatsapp.db
4. DELETE whatsapp.db-wal AND -shm (CRITICAL: stale post-migration WAL would corrupt restored file)
5. Update pin: state=rolled_back
6. EXIT (does NOT start daemon) + loud point-in-time-loss warning (messages received after upgrade lost)

**`--migrate` flag:** Clear/ignore pin + normal staged migration (ADR 0028) + start. Force past breaker.

**Startup decision table:**

| Pin state | Binary CURRENT (C) vs DB version (M) vs blocked_target | Action |
|-----------|-------------------------------------------------------|--------|
| No pin | M == C | Normal start |
| No pin | M < C | Migration mode (ADR 0028) |
| Pin (failed/rolled_back) | C == blocked_target && M < C | HALT (breaker; instruct: use old binary / --migrate / wait for newer binary) |
| Pin (rolled_back) | C == M (old binary on parked version) | Run normally + STICKY-ROLLBACK WARNING (auto-migration disabled) |
| Pin | C > blocked_target (genuinely newer binary) | CLEAR pin + normal migration (warn prior pin overridden) |
| No pin | M > C | Existing guard halts (storage.rs:795) |

**Newer binary (C > blocked_target) AUTO-RETRIES** migration (re-trips breaker + re-pins if it also fails) rather than always requiring `--migrate`. Allows iterative fix-and-deploy workflow.

## Consequences

**Positive:**
- Prevents infinite re-migration crash loop (pin blocks new binary after failure)
- Rollback is safe (no running-on-old-schema degraded mode to maintain)
- `--rollback` footgun closed (only valid after failure)
- Newer binary auto-retries (simplifies fix-and-deploy iteration)
- Sticky-rollback warning reminds user auto-migration disabled until they act

**Negative:**
- Rollback requires BOTH restoring DB AND running old binary (two steps, not one) — acceptable, matches the M1 model
- User must keep old binary available for rollback (document in release notes)
- Pin as sidecar file adds another filesystem state file (but consistent w/ .instance.lock)

**Future:**
- `--rollback` could auto-fetch old binary from GitHub releases (optional convenience).

**Hardening (2026-06-24, v2 review):**
- **Pin consistency check (R4):** Startup validates pin vs DB (rolled_back: pinned_version==user_version; failed: user_version<blocked_target); mismatch → halt "pin inconsistent". Pin written atomically (temp+rename).
