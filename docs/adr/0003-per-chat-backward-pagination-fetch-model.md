# 0003. Per-chat backward-pagination fetch model with resumable cursor

**Status:** Accepted  
**Date:** 2026-06-17  
**Updated:** 2026-06-22 — Composable-stop-conditions schema superseded by contained-C (ADR 0033)

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

**Note (2026-06-22):** Original composable schema (nullable `since_ts` + `max_messages`, "whichever first") superseded by **ADR 0033 contained-C model** (single `target_kind` + `target_value`, autonomy backstop). Frontier and resumable-continuation principles unchanged.

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

## Amendment (2026-10-20)

A fetch needs a usable anchor. The phone ignores a request with an empty anchor: it acknowledges the request and never answers (seen in a live run, where the job failed after 60 s on a fetch timeout). The earlier assumption that an empty anchor "returns the most-recent history chunk" was wrong and is removed.

The anchor is, in order: the cursor's own anchor (a non-empty `oldest_msg_id` and a timestamp), else the oldest stored message of the chat. With neither, `POST /api/history-fetch` returns `409` with `code: "no_anchor"` and creates no cursor and no job, and the worker fails a job that reaches that state at once with a `no_anchor:` reason, without calling the phone. The worker covers jobs queued before the upgrade and chats whose messages were deleted.

A cursor an older version left with an empty anchor (`oldest_msg_id = ''`, NULL timestamp) is re-seeded from the oldest stored message on the next trigger; it no longer blocks the chat for good.

Messages sent through whatsrust are not stored, so they never become anchors. A chat that has only such messages has no anchor: receive a message in it, or send one from the phone, then retry.
