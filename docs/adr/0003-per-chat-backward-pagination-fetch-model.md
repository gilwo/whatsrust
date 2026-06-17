# 0003. Per-chat backward-pagination fetch model with resumable cursor

**Status:** Accepted  
**Date:** 2026-06-17

## Context

WhatsApp history sync offers multiple strategies: automatic full sync on pairing, periodic background sync, and on-demand fetch. For a pure Rust bridge with user control, explicitly-triggered backfill per chat is the cleanest model.

`HistorySyncOnDemandRequest` supports pagination: anchor (oldest_msg_id / timestamp) + count. Challenge: how to expose control without creating a complex windowing API.

## Decision

Per-chat backward-pagination loop over `HistorySyncOnDemandRequest`. Single contiguous backward frontier per chat (no mid-history gaps, no arbitrary time windows).

**Stop conditions (composable):**
- `all`: fetch until history exhausted
- `since:<timestamp>`: stop when reaching timestamp
- `max_messages:<n>`: stop after n messages

Runtime cap enforced via parameter. Resumable continuation via persisted per-chat frontier cursor in `backfill_cursor` table (chat_jid, oldest_anchor, more_remain, exhausted, last_backfill_at). Re-trigger = resume from cursor.

## Consequences

**Positive:**
- Simple user model: "fetch more history for this chat"
- Single backward frontier avoids gap complexity
- Composable stop conditions cover common use cases (recent history, full backfill, bounded fetch)
- Resumable across restarts (cursor persisted in SQLite)

**Negative:**
- Cannot fetch arbitrary historical windows (e.g., "messages from 2024-03" without preceding months)
- Backward-only (cannot fetch forward from a gap, though wa-rs may not support that anyway)

**Alternatives rejected:**
- Full auto-sync on connect: unpredictable data usage, no user control
- Arbitrary time-range windows: complex state (multiple gaps), unclear UX
