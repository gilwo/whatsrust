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

## Amendment (2026-10-22)

**Archive seeding.** An archive, unarchive or mark-read/unread sync that the owner does on the phone reaches whatsrust as `Event::ArchiveUpdate` / `Event::MarkChatAsReadUpdate`. Its `message_range` holds the chat's last message: a `MessageKey` (`id`, `from_me`) and a timestamp in seconds (seen live on 2026-10-21 on a profile with 0 stored messages). whatsrust uses that key as the anchor of a chat that has none: when the chat has no usable cursor anchor and no stored message, and its cursor is not exhausted, it writes a cursor with that key and the timestamp in milliseconds (`more_remain = true`, `last_backfill_at` untouched). A usable cursor, a stored message or an exhausted cursor is never changed, and nothing is written when the range has no key with an id and a timestamp. The write is best-effort: an error is logged as a warning and never fails event handling. One INFO line is logged when it seeds (chat, action and anchor timestamp, not the message id). The chat is keyed like live inbound messages (`resolve_chat_jid`): an `@lid` DM becomes the phone JID when the mapping is known, a group JID passes through, and an `@lid` without a mapping stays as it is.

This is how to fetch a chat with no stored message without sending anything: archive and then unarchive the chat on the phone (the other side sees nothing), then trigger the fetch. The `no_anchor` guidance says so.

**Negative result (do not retry).** The phone ignores an anchor that is not a real message key. On 2026-10-21 three variants were tested five times in all: an empty anchor; an empty id with the timestamp set to now; a made-up id with the timestamp set to now. Each time the phone acknowledged the request (PeerMsg receipt) and never answered, while a real anchor was answered about 1 s after the receipt. A timestamp alone is therefore not an anchor.

**The wait is 180 s, not 60 s.** A phone that is asleep can receive the request minutes late: one receipt came 2.5 min after the request, while the fetch waited only 60 s; an answer arriving that late would have been dropped as an unmatched history sync. `LiveHistorySource::fetch_older` now waits `HISTORY_FETCH_WAIT` (180 s). If the phone still does not answer, the job fails with "the phone did not answer within 180 s; it may be asleep or offline: open WhatsApp on the phone, then retry". Cost: the single FIFO worker is busy for up to 180 s on one unanswered batch, so queued jobs for other chats wait behind it (a batch is never interrupted once started, ADR 0026). An answer that arrives after the wait is still dropped; a retry asks again. (Changed 2026-10-22: such a late answer is now stored, and the job stays failed; see ADR 0048.)
