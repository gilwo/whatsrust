# 0033. Fetch target model: contained-C (single target kind, autonomy backstop)

**Status:** Accepted  
**Date:** 2026-06-22  
**Supersedes:** Earlier composable-stop-conditions schema (nullable `since_ts` + `max_messages`, "whichever first"). Mentioned in design doc and relates to ADR 0003.

## Context

ADR 0003 established backward-pagination fetch with composable stop conditions (all/since/max). Original schema: nullable columns (`since_ts INTEGER`, `max_messages INTEGER`) where presence = active condition, "whichever first" semantics.

**Design review tension:** User wants explicit "fetch until done" (since/all auto-continue) vs safety concern "auto-continue could become unbounded multi-hour marathon" (hundreds of PDOs without re-authorization).

**Earlier R8 (nullable composable columns):** Intent + safety bound overload one field (since_ts = both "stop at T" AND implicit bound "don't fetch more than window size"). Ambiguous.

## Decision

**"Contained-C" model:** Intent and safety bound are SEPARATED.

**`target` = completion INTENT, exactly ONE kind** (clean discriminator, NOT ambiguous composition):
- **`since:T`** → done when oldest crosses T OR phone exhausted. AUTO-CONTINUES across paced segments (may run 10+ batches to completion).
- **`all`** → done when phone exhausted. AUTO-CONTINUES.
- **`count:N`** → done when N fetched. Does NOT auto-continue ("fetch up to N then stop" intent).

**Autonomy backstop = CONFIG-level guarded knob** (ban-critical per ADR 0022 fail-closed), NOT a per-request field. Bounds how far an auto-continuing (since/all) request may run in ONE trigger before it STOPS + requires re-trigger (= degrades to manual continuation past the limit). e.g., `backstop=20000`.

**NO child-job spawning** (preserves flat queue from ADR 0026 — we cut parent/child when we dropped community fan-out). Backstop PARKS-and-requires-retrigger, does NOT auto-enqueue continuation jobs.

**backfill_jobs schema (revised):**
```sql
target_kind TEXT CHECK (target_kind IN ('since', 'all', 'count')),
target_value INTEGER -- ts for since, count for count, NULL for all
```
NO composable since+max combo. The clamp ceiling for count + the autonomy backstop for since/all are CONFIG, applied at enqueue; response echoes `{requested, accepted}`.

**Example:** 9mo/>10k scenario — target=since:9mo, backstop=20k. 10k < 20k → COMPLETES IN ONE TRIGGER (~25-40min, auto-continuing across paced segments, no manual re-trigger). A 50k chat → runs to 20k backstop → parks → "re-trigger to continue" → behaves like manual thereafter.

**Progress (ADR 0034):** since/all completion has NO reliable ETA/total (phone doesn't pre-report window size) → SSE shows "N fetched, still going" (fuzzy), NOT "N/total %". count mode CAN show N/target.

**RESIDUAL COSTS accepted** (inherent to auto-continue-to-completion):
1. Completion requests are durable long-lived intents not single jobs → shutdown-resume (ADR 0026) must restore "keep going until T, parked at segment K" (more restart paths to test).
2. Fuzzy progress for since/all (above).
3. Cooldown/dedup (ADR 0035) must understand the frontier model (window overlap dissolved by single frontier — see ADR 0035).

## Consequences

**Positive:**
- Clear intent separation (target = what user wants; backstop = safety valve)
- Backstop as config → agent/API cannot bypass (structural safety, matches ADR 0021 daemon-side enforcement)
- since/all auto-continue → UX win for "fetch my history" (no manual pagination)
- count preserves bounded-fetch use case (exploratory "show me 100 old msgs")

**Negative:**
- Auto-continue adds complexity (long-lived intents, fuzzy progress, shutdown-resume paths)
- Backstop hit requires re-trigger (acceptable UX for rare >20k fetches)

**Future:**
- If "fetch window X to Y" becomes a stated requirement, add `range` target kind (two timestamps) — but confirm it's needed before building (arbitrary windows rejected in ADR 0003 for good reasons).
