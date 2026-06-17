# 0021. Daemon-side uniform safety enforcement vs misbehaving agents

**Status:** Accepted  
**Date:** 2026-06-17

## Context

MCP exposes whatsrust operations to external agents. A misbehaving agent (buggy logic, hallucinated retry loop, malicious prompt injection) could fire hundreds of rapid send/backfill calls, risking account ban.

whatsrust MCP server (`mcp.rs`) is a THIN PROXY over the HTTP daemon (stdio JSON-RPC → localhost HTTP). Pacers (ADR 0020 backfill, existing `SendPacer` for outbound) live in the DAEMON, below the MCP layer.

Three access paths: CLI (direct), REST API, MCP (proxies to REST). Safety must be uniform across all paths.

## Decision

**MCP is a thin proxy.** Pacers + safety limits live in the DAEMON, BELOW the MCP layer (REST API server handles all enforcement).

**Agent firing 100 send calls → 100 queued jobs draining at same human rate** (enforced at execution time, not call time). Agent CANNOT outrun pacer.

**Enforcement is DAEMON-SIDE, UNIFORM across CLI/REST/MCP:**
- NOT MCP-specific
- NOT bypassable by switching client (CLI vs REST vs MCP all hit same daemon guards)

**Guards beyond per-message pacer:**
1. **Global backfill concurrency cap** (default 1-2 active jobs; excess requests queue or reject)
2. **Per-chat backfill cooldown** (rapid re-trigger within N seconds → no-op / "already recent")
3. **Server-side `max_messages` CLAMP** (requested value clamped to hard max, return accepted value in response)
4. **Outbound queue-depth limit** (over threshold → reject enqueue with back-pressure error)

**All guards return STRUCTURED back-pressure errors** (agent can self-correct vs retry-storm):
- `429`-style `{error: "rate_limited", retry_after_secs: 60}`
- `{requested: 50000, accepted: 10000}` (clamped `max_messages`)
- `{status: "already_active", job_id: "abc123"}` (duplicate backfill trigger)

**MCP tool DESCRIPTIONS document pacing/limits per-tool** (expectation-setting so agent doesn't misread slowness as failure):
> "Backfill is rate-limited to ~4s/batch. Expect 5k messages to take 6-10 minutes. Check progress via SSE."

**Tool descriptions are NEVER the enforcement** — "seatbelt sign not seatbelt". Descriptions are advisory; daemon enforcement is mandatory.

## Consequences

**Positive:**
- Misbehaving agent structurally CANNOT cause ban (pacers enforce at execution, not call time)
- Uniform safety across all access paths (CLI/REST/MCP all hit same guards)
- Structured errors enable agent self-correction (retry with backoff, abort loop, adjust request size)
- Tool descriptions set expectations (agent interprets slow progress as normal, not failure)

**Negative:**
- Agent with legitimate high-volume use case (e.g., multi-chat backfill) still rate-limited (by design)
- No "trusted agent" fast path (uniform enforcement reduces ban risk for all users)
- Structured errors require agent to parse and handle them (dumb retry loop still possible if agent ignores errors)

**Rejected:**
- **MCP-layer enforcement** — bypassable by switching to CLI/REST, non-uniform safety
- **Tool-param safety overrides** (e.g., `force=true`) — agent can defeat safety by changing request params
- **Per-agent trust levels** — complex, trust model unclear, uniform enforcement is simpler and safer

**Deferred:**
- Daemon-level agent identification (track which MCP client is making calls) for better error messages / per-agent quotas — requires MCP context propagation through HTTP layer
