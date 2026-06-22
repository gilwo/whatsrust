# 0034. Fuzzy progress for auto-continuing fetch targets

**Status:** Accepted  
**Date:** 2026-06-22

## Context

ADR 0033 established three target kinds: `since`, `all`, `count`. Progress reporting requirements differ:
- **count mode:** total known up front (requested N) → can show "fetched 1280 / 5000" (54%).
- **since/all modes:** completion condition is "phone says exhausted" or "oldest crossed T" — phone doesn't pre-report "you have 5342 messages in this window" before fetch starts. Exact total unknown until exhausted.

**Design review note:** User confirmed fuzzy progress acceptable ("N fetched, still going") for since/all, given async job model.

## Decision

**since/all modes:** SSE progress shows **"N fetched, more remain"** (fuzzy). NO ETA, NO percentage, NO fake "estimated total" (those are lies until phone says exhausted).

**count mode:** SSE shows **"N / target"** (percentage-capable). Total known from request.

**SSE event payload examples:**
```json
{"job_id": 123, "fetched": 1280, "target_kind": "since", "status": "running", "more_remain": true}
{"job_id": 124, "fetched": 3200, "target_kind": "count", "target_value": 5000, "status": "running"}
{"job_id": 123, "fetched": 5342, "status": "done", "more_remain": false}
```

**Completion states (all targets):**
- `done` — target reached (count: N fetched, since: crossed T, all: exhausted)
- `paused` — autonomy backstop hit (ADR 0033), requires re-trigger
- `cancelled` — explicit cancel
- `failed` — error (timeout, protocol error)

**Long-pause UX (from ADR 0020 backfill pacing contract):** SSE emits EXPLICIT `paused/cooldown` state + resume-hint during randomized long pauses so stalls read as deliberate caution NOT hangs. Separate from backstop-paused.

## Consequences

**Positive:**
- Honest progress reporting (fuzzy when total unknown, precise when known)
- since/all UX sets correct expectation (background marathon, not spinner)
- count mode preserves percentage for bounded fetches

**Negative:**
- Fuzzy progress can't show ETA for since/all — acceptable given async job + SSE visibility

**Future:**
- If phone protocol adds "total available in window" field to ON_DEMAND response, upgrade since/all to precise progress (check during wa-rs spike).
