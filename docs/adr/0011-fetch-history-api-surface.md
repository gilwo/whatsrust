# 0011. Fetch history API surface: trigger, status, cancel, SSE progress

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Historical fetch needs API/MCP surface for:
- Trigger backfill (with mode: all/since/max)
- Check status (job state, progress)
- Cancel running job
- Stream progress (real-time updates)

MCP design pattern (from `src/mcp.rs`): one tool per high-level operation. HTTP API (from `src/api.rs`): RESTful endpoints + SSE streaming.

## Decision

**HTTP API (3 endpoints):**
```
POST   /backfill          {chat_jid, mode}           → {job_id, status, chat_jid, mode, resume_anchor?, more_remain}
GET    /backfill/:job_id                             → {job_id, status, messages_fetched, created_at, updated_at}
POST   /backfill/:job_id/cancel                      → {job_id, status: "cancelled"}
```

**MCP tool (1 tool):**
```
whatsrust_fetch_history(chat_jid, mode="all")        → {job_id, status, ...}
```

**SSE progress:** reuse existing `/events` SSE endpoint. Emit `BridgeEvent::BackfillProgress {job_id, chat_jid, messages_fetched, status}` on each page fetched + job state change (completed/failed/cancelled).

**No-op fast-path:** if `backfill_cursor.exhausted = true` for chat → return `{job_id: null, status: "already_exhausted", more_remain: false}` (no queue insertion).

## Consequences

**Positive:**
- Minimal API surface (3 HTTP endpoints, 1 MCP tool)
- SSE reuse = no new transport (existing `/events` infra)
- Fast-path avoids queue churn for exhausted chats
- MCP tool mirrors HTTP POST (consistent UX)

**Negative:**
- SSE is HTTP-only (MCP clients must poll GET `/backfill/:job_id` for progress)
- No batch trigger (must call POST `/backfill` per chat) — acceptable for v1, batch is future enhancement

**Future:**
- Batch trigger: POST `/backfill/batch` {chat_jids: [...], mode} → {job_ids: [...]}
- MCP progress callback (if JSON-RPC notification spec added)
- Webhook for completed jobs (external integrations)
