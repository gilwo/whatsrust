# 0010. Durable backfill-job queue with async progress tracking

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Historical backfill is long-running (minutes to hours for large chats) and must survive:
- Daemon restart (network hiccup, OOM, deploy)
- User cancellation (explicit stop request)
- WhatsApp rate-limiting (transient 429 errors → retry with backoff)

In-memory task tracking loses state on restart. Sync API calls block caller for full fetch duration (bad UX). Polling for status adds API complexity.

Existing pattern: outbound message queue in SQLite (`outbound_queue` table) + worker + broadcast events via `tokio::sync::broadcast`.

## Decision

**Durable backfill-job queue** (twin of outbound queue) in SQLite:
```sql
CREATE TABLE backfill_jobs (
    job_id INTEGER PRIMARY KEY,
    chat_jid TEXT NOT NULL,
    mode TEXT NOT NULL, -- 'all' | 'since:<ts>' | 'max:<n>'
    status TEXT NOT NULL, -- 'pending' | 'running' | 'paused' | 'completed' | 'failed' | 'cancelled'
    messages_fetched INTEGER DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
```

**Async trigger:** POST `/backfill` → insert job → return `{job_id, status: "pending"}` immediately. Dedicated backfill worker (separate tokio task) polls queue + drives fetch loop.

**Progress tracking:** worker emits `BridgeEvent::BackfillProgress {job_id, chat_jid, fetched, status}` to broadcast bus. API/MCP consumers subscribe via SSE.

**Restart-safe:** on daemon start, resume `status='running'` jobs (or mark paused + require user re-trigger).

**Cancellable:** POST `/backfill/:job_id/cancel` → set `status='cancelled'`, worker checks flag per page.

## Consequences

**Positive:**
- Async trigger = responsive API (no long-poll, no timeout)
- Survives restart (jobs in SQLite, not memory)
- Progress visible via SSE (real-time updates to UI/CLI)
- Cancellation = graceful stop (not kill -9)
- Reuses existing broadcast event pattern (same infra as outbound status)

**Negative:**
- Adds backfill_jobs table + worker task (complexity vs in-memory task map)
- Worker must handle job starvation (prioritize user-triggered over auto-resume)

**Future:**
- Prioritization: interactive user-triggered > background auto-resume
- Rate-limit per-account total backfill bandwidth (across all jobs)
