# 0022. Fail-closed config safety with scoped DANGEROUSLY overrides

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Ban-critical config knobs (backfill pacer interval, concurrency cap, max_messages ceiling) must have safe defaults. Naive users should be protected; advanced users need escape hatches.

Earlier plan (ADR 0021 context): document dangerous knobs with causal warnings. User pivot: warnings can be ignored; BLOCK startup instead unless explicit override.

whatsrust currently has an instance lock (ADR from first snapshot) that refuses to start if lock held → fail-closed pattern exists.

## Decision

**Ban-critical knobs validated at STARTUP against safe bounds:**
- Backfill pacer interval (floor: 3s — below risks ban)
- Backfill max concurrent jobs (ceiling: 3 — above risks ban)
- `max_messages` hard ceiling (ceiling: 50000 — above risks timeout/memory)

**Out-of-safe-range AND override-not-set → REFUSE TO START:**
- Exit non-zero
- Explained error message:
  - Which var is out of range
  - Actual value vs safe bound
  - Causal reason ("rapid PDO to own phone is detectable automation")
  - EXACT override flag verbatim (copy-pasteable)

Example error:
```
Error: WHATSRUST_BACKFILL_INTERVAL_SECS=1 is below safe floor (3).
Rapid PDO to own phone is detectable automation and risks account ban.

If you understand this risk, set:
  WHATSRUST_DANGEROUSLY_ALLOW_FAST_BACKFILL=1

Refusing to start.
```

**Override = SCOPED `WHATSRUST_DANGEROUSLY_ALLOW_*` per risk class:**
- `WHATSRUST_DANGEROUSLY_ALLOW_FAST_BACKFILL` (covers backfill interval floor)
- `WHATSRUST_DANGEROUSLY_ALLOW_HIGH_CONCURRENCY` (covers concurrency cap ceiling)
- `WHATSRUST_DANGEROUSLY_ALLOW_HUGE_FETCH` (covers max_messages ceiling)

**NOT one global `WHATSRUST_DANGEROUSLY_ALLOW_ALL`** — keeps blast radius tight, acknowledgment specific.

**When override set → daemon starts but:**
- Logs persistent WARN (every startup)
- Surfaces in status/SSE (`"warnings": ["fast_backfill_override_active"]`)
- NEVER silent (user must know override is active)

**ONLY a SMALL CURATED set of ban-critical knobs guarded:**
- Backfill pacer interval, backfill concurrency cap, `max_messages` ceiling
- Maybe send pacer interval if user-configurable (TBD)

**ALL benign tuning knobs UNGUARDED:**
- Embedder batch size (64)
- Drain backoff cap (60s)
- Watchdog interval + growth threshold %
- Per-chat backfill cooldown (no ban risk, just prevents churn)
- Backfill batch size (no ban risk, just network efficiency)
- FTS tokenizer (fixed `unicode61`, not configurable anyway)
- Backup/prune intervals (existing, no ban risk)

**Over-guarding breeds blanket-bypass fatigue** → curate the list carefully. Distinguish:
- **Hard floor** (block without override, e.g., backfill interval < 1s is never safe)
- **Soft range** (clamp + warn, e.g., batch size 1-1000 → clamp to 64-256 but allow)

Mirrors existing `instance_lock` fail-closed pattern (ADR from snapshot 1).

## Consequences

**Positive:**
- Naive users CANNOT accidentally configure dangerous values (protected by default)
- Advanced users have explicit, documented escape hatch (DANGEROUSLY flags)
- Scoped overrides keep acknowledgment specific (not blanket bypass)
- Persistent warnings + status surfacing ensure overrides never silent
- Fail-closed startup prevents "whoops I got banned" scenarios

**Negative:**
- Startup failure is user-hostile (vs silent clamp + warn) — mitigated by clear error message with exact fix
- Advanced users must set verbose `WHATSRUST_DANGEROUSLY_ALLOW_*` flags (by design — friction is feature)
- Over-guarding risk (too many flags → users `export WHATSRUST_DANGEROUSLY_ALLOW_*=1` in blanket) — mitigated by small curated set

**Pivoted from earlier Q19a plan:**
- **Earlier:** causal warnings in config comments (advisory)
- **Now:** block startup + require explicit scoped override (mandatory)
- User direction: "warnings can be ignored; block instead"

**Deferred:**
- Runtime config reload (currently requires restart) — fail-closed validation would need to reject hot-reload of dangerous changes
