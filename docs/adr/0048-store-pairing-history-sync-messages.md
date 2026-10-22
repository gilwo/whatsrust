# 0048. Store the messages of the pairing history sync, and late on-demand answers

**Status:** Accepted
**Date:** 2026-10-22

## Context

A history fetch needs an **anchor**: a real message key (id, from-me flag, timestamp) from the chat's cursor or, failing that, its oldest stored message (`backfill::resolve_anchor`, ADR 0003 amendments). The phone ignores a request without one: it acknowledges the request and never answers. Live tests on 2026-10-21 tried an empty anchor, an empty id with a timestamp, and a made-up id with a timestamp, five times in all; none was answered, while a real anchor was answered about 1 s after the receipt.

A freshly paired profile has no stored message, so no chat could be fetched. The only ways to get an anchor were to receive a message in the chat, or to archive and unarchive it on the phone (anchor seeding from the archive sync, ADR 0003 second amendment).

Meanwhile the phone already sends messages at pairing. When a device is linked it pushes a one-time history package, and each chunk reaches whatsrust as `Event::HistorySync` with a type: `INITIAL_BOOTSTRAP`, `RECENT`, `FULL`, `PUSH_NAME`, `NON_BLOCKING_DATA` or `INITIAL_STATUS_V3`. wa-rs asks for a full sync (30 days), but the phone decides what to send: on the test account two pairings delivered about 9 conversations in all (5, then 1 and 3), and the `FULL` chunk was empty. ADR 0037 turned the sync on for the DM trusted-contact tokens it carries, and left the handler a no-op so that pairing could never break. Every chunk that no pending fetch claimed was ignored ("unsolicited bootstrap"), so those messages, which are exactly the anchors a fresh profile lacks, were thrown away.

A second kind of chunk was dropped the same way. An on-demand fetch waits 180 s for the phone's answer (`HISTORY_FETCH_WAIT`). An answer that arrives later finds no waiting correlator entry and was ignored, although it holds the page the job asked for.

## Decision

1. **Store the messages of the pairing package.** Every unmatched history-sync chunk that holds conversations is stored, with `messages.source = "bootstrap"`.
2. **Store a late on-demand answer.** An unmatched chunk of type `ON_DEMAND` is stored with `source = "backfill"`. The job that timed out stays `failed`; nothing revives it. The next fetch starts from the chat's cursor if that has an anchor, else from the oldest stored message, which the late answer has moved back (`resolve_anchor`).
3. **A flag, default on.** `WHATSRUST_STORE_PAIRING_HISTORY` (`BridgeConfig.store_pairing_history`, default `true`) is read like the other boolean variables (`true`/`1`/`yes`, `false`/`0`/`no`, any case; unset or blank keeps the default, an unrecognised value warns and keeps it). When it is `false`, unmatched chunks other than `ON_DEMAND` are ignored as before. Late on-demand answers are stored whatever the flag says: the owner asked for that data.
4. **wa-rs limits unchanged.** What the phone is asked to send (`HistorySyncConfig`) is not touched. Whatever it sends is stored; nothing more is requested.
5. **No cursor rows.** Storing writes only `messages` rows (and `media_refs`, point 6). The oldest stored message of a chat becomes its anchor through `resolve_anchor`, as for any chat with messages.
6. **No media downloads for stored history; the reference is kept.** Neither the pairing package nor an on-demand fetch downloads any media bytes. A media message is stored with the same label a download would have produced (`[image 12.1KB] caption`, `[doc: report.pdf 2.0KB]`), with the size taken from the proto's `file_length`, which is the length of the decrypted file that a download measures. Its reference (`media_key`, `direct_path`, `file_enc_sha256`, `mimetype`, `file_length`, `width`, `height`) is written to `media_refs` (ADR 0005, "always store media refs"; `hydrated_path` stays NULL). A later change can download on demand from that row, best-effort, because CDN links expire; that download is not built here. Live ingest is unchanged: it downloads eagerly, because the bytes feed the inbound event, and writes no `media_refs` row.

### How the handler decides

`handle_event`, `Event::HistorySync`:

- The chunk has a session id and `correlator.fulfill` accepts it: it is routed to the backfill worker, exactly as before (**route**).
- Otherwise it is unmatched: no session id (the pairing package), or a session id nobody is waiting for any more (a late answer). `unmatched_history_source(sync_type, store_pairing_history)` picks its label: `ON_DEMAND` gives `backfill`; any other type gives `bootstrap` if the flag is on, and nothing if it is off (**store** or **ignore**).

`fulfill` consumes the chunk, so the handler keeps a spare copy first (the compressed payload is shared and the decode cache is not carried over, so a copy is cheap) and hands it to the unmatched path only when `fulfill` fails.

### How a chunk becomes rows

- **A spawned task, never inline.** A chunk may hold hundreds of conversations; storing it does one or two database writes per message, so none of it runs on the event loop. `spawn_store_unmatched_history` spawns a detached task. The task inflates and decodes the chunk in `spawn_blocking` (tens of milliseconds for a multi-MB chunk), then stores. Every failure is a WARN; nothing panics, returns early from event handling or fails the event.
- **One conversion, shared.** `backfill::drain_all_conversations` calls `drain_history_sync` once per distinct conversation id, so an on-demand answer and a chunk nobody asked for become messages the same way. It leaves out conversations with an empty id or one that `should_ignore_jid` rejects (status, broadcast, newsletters, as the live path does), messages without a payload, a key id or a positive timestamp, and conversations left with no message.
- **One persistence path, shared.** `persist_history_messages` is the loop the backfill sink always ran, lifted out so both use it: content from `extract_content_inner` (the single extraction path, ADR 0014), `embed_status` classified like live ingest (ADR 0016), `INSERT OR IGNORE` by message id, a `media_refs` row for each new media message, and a wake of the embedding-drain worker for each new row left `pending` (M2.3.2). The `source` label is the only difference. `Store::insert_message_if_new` reports whether a row was written, which gives the stored and duplicate counts; `Store::insert_media_ref` is insert-or-ignore on `message_id`. A duplicate message gets no reference (a live row already has its bytes).
- **One extraction path, two media modes.** `extract_content_inner` takes a `MediaMode`. `Download` (live ingest) checks the size limit, downloads under the semaphore and puts the bytes in the content. `Reference` (stored history) downloads nothing, puts empty bytes in the content, and hands back the media reference, read through wa-rs's `Downloadable` trait, so the per-type parsing is not repeated. The per-type media branches share one helper (`MediaMode::bytes`) for both modes. The label's size comes from `InboundContent::display_text_with_media_len`, which replaces the byte count of a media variant with `file_length`; `display_text()` is unchanged. The public `InboundContent` type is not touched. A message that carries no `file_length` is labelled `0B`. No test needs a WhatsApp client any more, and the history download slots and the backfill sink's client handle are gone.
- **Size limit.** `MAX_MEDIA_BYTES` (50 MiB) is a download limit and still applies to live ingest. History rows do not download, so a larger file is stored too, with its true size in the label and in `media_refs`.
- **Chat keys.** Each conversation id becomes the stored `chat_jid` through `resolve_chat_jid`, as for live messages: a `@lid` DM becomes the phone JID when the conversation's `pnJid` gives it (used first, the way live messages use `sender_alt`) or `lid_pn_mapping` knows it; groups and phone JIDs pass through; an unmapped `@lid` without a `pnJid` stays as it is. The sender of a DM message is resolved with the same `pnJid`.
- **Duplicates.** The same message may arrive live, in the package and in a fetch. The first writer's row is kept; later ones are counted as duplicates.
- **Logging.** One INFO line per stored chunk: `sync_type`, `source`, `conversations`, `stored`, `duplicates`, `unstorable`, `media_refs`. No message text, no ids. A WARN follows if message or reference writes failed.
- **Deleting.** `delete_inbound_chat` and `delete_inbound_message` also delete the `media_refs` rows of the messages they remove, in the same transaction.
- **No schema change.** `messages.source` is free text (`live`, `backfill`, now `bootstrap`); nothing reads it. `media_refs` already existed (ADR 0005) and nothing wrote to it until now.

### LID mappings at pairing time

Read from the wa-rs source at the pinned revision (`9e8c70e`); not yet seen on a live pairing. The only mapping wa-rs writes while pairing is the account's own (`LearningSource::Pairing`). Its history-sync handling stores tctokens, the nct salt, the push name and message secrets, and does not read the conversations' `pnJid` / `lidJid` fields or the chunk's `phoneNumberToLidMappings`. Contacts' mappings are learned later, from usync, group lookups and live messages, and are persisted by a detached task. So when the chunk is processed, `lid_pn_mapping` normally does not yet know a contact's LID.

That is why the conversation's own `pnJid` is passed to `resolve_chat_jid` as the inline alternative: a `@lid` DM that carries a `pnJid` lands under the phone JID, the key later live messages use, with no mapping needed. A `@lid` DM without a `pnJid` and without a stored mapping is stored under the `@lid` JID, while later live messages from the same contact are stored under the phone JID; no re-keying job is built for that remainder. Whether the phone fills `pnJid` in these chunks has to be checked on a live pairing.

## Consequences

**Positive:**
- A freshly paired profile has anchors for the chats in the package: `fetch-history` works on them at once, with no archive and unarchive step.
- Those messages are searchable (lexical, and semantic when an embedder is on) from the first minute.
- A late answer to a fetch is no longer lost.
- The matched on-demand path is unchanged, and so are the REST wire bytes (golden fixtures).

**Negative / limitations:**
- **Only at pairing.** The phone sends the package when the device is linked. An already-paired profile gets nothing from it until it is re-paired (unlink on the phone, restart; local content is kept, HOWTO §3a). Turning the flag on later does not fetch anything.
- **Coverage depends on the phone.** It chooses which chats and how many messages to send (about 9 conversations on the test account). Chats outside the package still need an anchor from the archive and unarchive step or from a new message.
- **Embedding work.** Stored text rows are classified `pending`, so with an embedder on, the drain worker embeds the whole package once, shortly after pairing.
- **No media bytes.** Stored history keeps a media message's label and reference, not its bytes. The bytes can be fetched later from `media_refs`, best-effort: CDN links expire, and that download is not built yet (ROADMAP). Media whose link has already expired is still stored, which a download-at-store design would have dropped.
- **The sender allowlist is not applied.** `WHATSAPP_ALLOWED` filters live inbound messages only. Neither a fetch nor the pairing package is filtered by it (decided 2026-10-22; unchanged). A deployment that needs the allowlist to bound what is stored should set `WHATSRUST_STORE_PAIRING_HISTORY=false`.
- **Media keys are stored.** `media_refs` holds each stored media message's key and CDN path (ADR 0005). It sits in the same database as the Signal sessions and the messages themselves, and is deleted with its message.
- **A late answer does not revive its job.** The job stays `failed`; the user retries. If the cursor already has an anchor, the retry asks for the same page again and the duplicates are skipped.
- **No re-keying.** See the LID section above.
- **Detached task.** It is not cancelled at shutdown; a chunk cut off half way leaves some rows stored, which is harmless (`INSERT OR IGNORE`, and a later fetch continues from them).

**Pairing safety:** the handler returns at once after spawning; storing is best-effort; with the flag off, unmatched chunks are ignored as before ADR 0048. ADR 0037's rule that the history-sync handler must never break pairing still holds.

**Related:** ADR 0037 (history sync enabled; amended), ADR 0003 (anchors; amendments), ADR 0005 (media references; amended), ADR 0014 (single extraction path), ADR 0016 and 0038 (embed classification), ADR 0043 (re-pairing keeps local content).
