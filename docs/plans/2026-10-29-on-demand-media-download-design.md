# On-demand media download — design and plan

**Status:** v5.4 (2026-11-08); **phase 2 built** (2a 12204f9, 2b 2026-11-08; the allowlist variable is
`WHATSAPP_ALLOWED`, corrected from a wrong name used since v4). v5.4: the v5.3 cold review folded in with the owner's decisions: the repair runs at every
start that knows the owner's number, with a backup before it changes rows; a `media_key_timestamp` column and a
whole-tuple fill; "revoked wins" and revoke tombstones stored past the allowlist; phase 2 merges after 2b. v5.3
(2026-11-08): the v5.2 cold review folded in with the owner's decisions: revokes are judged when
read (no `revoked_at` column, no one-time pass), the own-message repair runs once at the first start that knows the
owner's number, the way back is an in-place downgrade, `view_once` comes from the extractor; real-DB facts in §2. A
cold review of v5.3 runs before phase 2b. v5.2 (2026-11-08): the revoke key rule corrected to the protocol, the decisions of the phase 2
implementation plan (`docs/plans/2026-11-08-media-phase2-plan.md`), and a fix for own messages in LID-addressed
1:1 chats; a cold review of v5.2 runs before phase 2b. v5.1 (2026-11-06): the own-message attribution fix and its
row repair folded into phase 2 (owner, 2026-11-06). v5 (2026-11-04): the v4 follow-up cold review and a ureq timeout spike folded in, with the owner's
decisions of 2026-11-04 (§9). The old-file test matrix passed (2026-11-05, GO 2026-11-06). Next: phase 2, and a
short retest of two cells (§11.1), done 2026-11-06. History: v1 2026-10-29 (cold
review: needs rework); v2 2026-10-30 (approve with changes, 2 blocking); v3 2026-11-01 (approve with changes,
0 blocking); v4 2026-11-03 (approve with changes, 0 blocking).
**Decision record:** ADR 0050 (Proposed); ADR 0005 amendment of 2026-10-29.
**Roadmap item:** P1 "On-demand media download for stored history" (FEATURES F1 open item).
**Related:** ADR 0005 (lazy media hydration), ADR 0012 (indefinite retention), ADR 0022 (fail-closed config),
ADR 0028–0032 (staged schema migration), ADR 0044 (REST request gate), ADR 0045/0046 (MCP listener, tool
validation), ADR 0048 (stored pairing history), ADR 0049 (deployment as a service).
**Reviews:** `_reviewer/design/2026-10-29-on-demand-media-download.md` (v1),
`_reviewer/design/2026-10-30-on-demand-media-download-v2.md` (v2, with the reconciliation of v1),
`_reviewer/design/2026-11-01-on-demand-media-download-v3.md` (v3, with the reconciliation of v2),
`_reviewer/design/2026-11-04-on-demand-media-download-v4.md` (v4, with the reconciliation of v3).

This document describes what is built and how. The owner's decisions are in §9; the open items in §11.

> **wa-rs version:** this work is built on the **current pin `9e8c70e`** (owner decision 2026-10-30). The upgrade
> is a separate project afterwards (roadmap Q-WARS; a compile spike measured 165 errors for v0.7.0 and 298 for
> upstream `main`). At this pin whatsrust runs the media HTTP transfer and builds the re-upload request itself, from
> wa-rs's public parts (§6.2). Items marked **[wa-rs]** are where the upgrade changes this design; they sit behind
> two adapters, `MediaTransfer` and `MediaReuploader`.

### Changes in v5.4

| Change | Why |
|---|---|
| **The repair runs at every start that knows the owner's number** (idempotent; it changes rows only when misattributed rows exist); the marker only records the number for the re-pair guard; a backup (`whatsapp.db.pre-repair-<ts>.bak`) before it changes rows outside the migrating start; an in-transaction post-condition; a persisted failure note; one warning for another number (§4; owner, 2026-11-08) | a one-shot marker locked in rows written later (a v8 window after a downgrade, an incomplete identity, a failed lookup), and a later run had no backup (v5.3 review M1, M2) |
| The repair's purpose restated: the duplicate and revoke rules are identity-based and work without it; it corrects stored data for `history`/`search` and the fetch cursor's anchor flag (§4) | v5.3 review M2 |
| A sixth column, `media_key_timestamp`; the encryption tuple (`media_key`, `direct_path`, `file_enc_sha256`, `media_key_timestamp`) is filled only as a whole, when all three key fields are empty (§4; owner, 2026-11-08) | per-column filling could pair a key from one encryption with a path from another (a false `integrity`); without the timestamp a newer copy could never be told apart without a v10 (v5.3 review M3) |
| **Revoked wins:** a message judged revoked is not `ready`; `get`/`status` answer `revoked`, and a downloaded file is tagged then (idempotent) (§5.5, §6.1; owner, 2026-11-08) | the read-time judgement and event-time tagging had no precedence (v5.3 review M4) |
| **Revoke tombstones are stored past the allowlist** (only the revoke row; no event, no receipt); the remaining false negatives fail open, as a stated policy; retention and prune features must keep revoke rows (§5.5; owner, 2026-11-08) | live ingest dropped revokes from senders not on `WHATSAPP_ALLOWED` while history stored their media (v5.3 review M4) |
| In a 1:1 chat a revoke is judged by `from_me` equality with the target (after the repair); groups by sender identity (§5.5) | removes the LID-mapping false negatives in 1:1 chats (v5.3 review m8) |
| `ensure_v9_columns` on the re-validation branch, so `--migrate` can fix a v9 DB with a missing column; precise recovery wording (§4) | v5.3 review m1 |
| Phase 2 merges to `main` after 2b (§9, §13; owner, 2026-11-08) | the attribution repair is user-visible on its own; the engine follows on the branch |

### Changes in v5.3

| Change | Why |
|---|---|
| **Revokes are judged when read** (`is_revoked`: `get`, `status`, the download start and its final write) from the stored revoke rows, through a partial index on the revoke label; v9 drops the `revoked_at` column, and there is no one-time pass (§3, §4, §5.5, §6.2; owner, 2026-11-08) | resolving at ingest was order-dependent (a revoke before its target, a build without the resolution, a non-atomic live path) and put a pass into the irreversible step; it drew a major finding in v3, v4 and v5.2 (v5.2 review M5) |
| The revoke key does not gate the judgement; its shape is only logged, without JIDs (§5.5) | the key format is inferred from wa-rs's send code only, and tests built on it would be circular (v5.2 review M4); the identity test already rejects a forged revoke |
| **The own-message repair runs once at the first start that knows the owner's number**, in its own transaction, under a metadata marker that records the number; a marker with another number skips it with a warning (§4; owner, 2026-11-08) | logout deletes the `device` row and the QR flow starts unpaired, so a repair inside the version step could be skipped for good (v5.2 review M1) |
| The repair updates set-wise with the full-text update trigger paused; a budget of 30 s for 500,000 messages, measured (§4) | the trigger re-indexed every repaired row's unchanged text (v5.2 review M2) |
| **The way back is an in-place downgrade** (`PRAGMA user_version = 8` on a stopped daemon), proven by a test with the v8 binary; restoring the backup is the fallback; the v9 step is re-runnable (§2, §4) | a v8 binary refuses v9 only by the version number, and v9 adds only nullable columns and an index (v5.2 review M3) |
| `view_once` is set inside the extractor's own recursion and never lowered by a duplicate (§4) | a separate unwrap walker could disagree and make view-once media downloadable (v5.2 review M6) |
| One owner-matching predicate for both repairs; the cursor flag also when NULL; a missing `device` column means "no identity"; §11.5's remedy corrected (only `clear-data` and a new fetch) | v5.2 review m1, m2 |

### Changes in v5.2

| Change | Why |
|---|---|
| A revoke counts only when its key says `from_me = true`, the key's participant is empty or the revoker, the target is stored in the same chat, and the revoker is the target's author; a key with `from_me = false` (an admin delete) or without the flag does not count (§2, §5.5) | the key is written from the revoker's view (wa-rs `src/send/actions.rs`: a sender revoke has `from_me = true` and no participant, an admin revoke `from_me = false` and the author), so v5's "the key matches the target row" rejected every valid revoke by another person |
| The one-time revoke pass is part of the v9 step in the same commit as the columns (phase 2a) (§5.5, §13) | each migration step runs once per DB (`if from_version < 9`); a pass added in a later commit would never run on a DB already at v9 |
| The repair: own history rows whose sender is **not one of the owner's identities** get the own phone JID; the live repair also matches full-JID forms and the own LID from the mapping; the fetch cursor's copy of the anchor flag is repaired too (§4; owner, 2026-11-08) | "sender = chat JID" missed own rows whose sender is an unmapped `@lid` form of the chat; a stale anchor flag makes a fetch ask the phone for a key that does not exist |
| Duplicates: a 1:1 chat matches as the same user, a group exactly; two own rows match on the sender; `duplicate_mismatch` counts media duplicates only (§4) | bootstrap and backfill can store one 1:1 chat under its LID and under its phone JID |
| `view_once` is also set by the media message's own view-once flag (§4) | the proto carries the flag on the image, video and audio message too; refusing more is the safe side |
| The revoke key is read from the raw message by a helper; the public `InboundContent` enum does not change (§5.5) | library consumers (habb) match on and build that enum |
| Own messages in a LID-addressed 1:1 chat are filed under the other person, not the account's own number: a fix commit before phase 2a (§2, §11) | live ingest resolved their chat with the account's own phone JID (wa-rs sets it as the sender's alternate ID), so they landed in the account's own chat, and the revoke rule's "same chat" test failed for them |

### Changes in v5.1

| Change | Why |
|---|---|
| Phase 2 starts with the own-message attribution fix: live rows store the event's `from_me`; a history message with `from_me` and no participant stores the account's own phone JID as the sender (§2, §4, §13) | the duplicate rule and the revoke rule compare the sender and `from_me`; history stored the key's `remote_jid` as the sender of own messages (the group in groups, the other person in 1:1 chats), so the owner's own revokes from history never counted |
| The v9 migration repairs the stored rows before the one-time revoke pass (§4, §5.5); owner-authorised 2026-11-06 | the old rows carry the same wrong values; a separate repair on `main` would need its own step or take the schema number 9 |

### Changes in v5

| Change | Why |
|---|---|
| The idle and total transfer limits are ureq's own `timeout_recv_body` (it restarts on every read) and `timeout_global` (from the start of the call); no watchdog thread, no lingering-thread pause (§6.2, §6.3) | v4 review M6, and a spike of 2026-11-04 that found `timeout_recv_body` is per read, not a total (v4 used it as the 2 h limit) |
| No supported rollback after a healthy v9 migration is stated; the old-file matrix now gates the merge of phase 2 (§4, §13) | v4 review M1: `--rollback` refuses a healthy DB |
| The stdio MCP server marks its REST calls, so the media routes treat it as remote (§8) | v4 review M2: stdio MCP is a REST client of the loopback listener |
| A revoke is judged by identities: the revoker's account against the target's author, plus the revoke key's own `from_me`/`participant`; a one-time pass over stored revoke rows (§5.5) | v4 review M3: the owner's own revokes failed on the live `from_me` bug |
| A duplicate fills or creates a reference only when chat, sender, `from_me` and kind match the stored row; the delete hook matches the chat too (§4, §5.5) | v4 review M4 and its note on deletes by ID |
| One re-upload window (requests and bytes); a lone over-size file is admitted into an empty window; the fresh path persists 1 h; a per-item backoff (§6.1, §6.5) | v4 review M5, Q4 |
| Smaller fixes: the ciphertext `Content-Length` is the hard cap and `file_length` only a hint; a std allow-list sanitiser (no new crates); adoption by the ID part of the name; chunked intents; `detail` from a closed vocabulary; a URL note for remote callers; what runs when off; a LID-less account falls back to the pin's request; `_deleted/` counters | v4 review m1–m8, Q2, Q5, notes |

### Changes in v4

| Change | Why |
|---|---|
| Deleted and revoked files move into a reserved `_deleted/` folder of their chat; "deleted" is a path property; one `tag` primitive and a transition table (§5.5) | v3 review M1 (a sender could put `.deleted.` in a document name) and the complexity note |
| A revoke counts only when the revoker is the target's sender; it is resolved once at ingest into a new column `revoked_at` (§4, §5.5) | v3 review M2: any member could hide anyone's media; a label lookup per poll |
| The transfer timeout is clamped (default 2 h, configurable); at most 4 ending threads (§6.2, §6.3) | v3 review M3: the bound followed a sender-controlled length |
| The old-file test matrix is an exit gate for phase 4; no LID form → `address_unresolved` without contacting the phone (§6.2, §11, §13) | v3 review M4 |
| A re-upload byte budget per hour (default 1 GB) (§6.5) | v3 review M5: the phone uploads whole files |
| A duplicate history message creates a missing reference when the stored row is not live; the message and its reference are written in one transaction; `no_reference` names the row's source (§4, §8.1) | v3 review M6 (revises the "fill only" rule of 2026-11-01) |
| View-once media is refused (a `view_once` column set at ingest) (§3, §4) | v3 review Q1 |
| `WHATSRUST_MEDIA_DOWNLOAD` is off by default (§10.2) | v3 review Q6 (fail-closed, ADR 0022) |
| Smaller fixes: per-entry journal and memory keys; decryption errors retried once and size mismatches accepted after a valid MAC; a rate-limited media-connection refresh; no redirects; the breaker counts `expired_after_reupload`; a full HTTP/MCP mapping; MCP `get` fails fast without a listener; bounded file-name IDs; the dependency list; a startup WARN and `cancel all`; fresh tokens near expiry; proxy connection limits; `waiting: no_lid` | v3 review m1–m11 |

---

## 1. Goal and scope

**Goal:** a client, usually an agent on **another computer**, asks for the media of one stored message by its
ID. whatsrust downloads it in the background into a per-chat folder; once it is ready, the client gets a
short-lived URL and fetches the file over the network.

**In scope:**
- Per-message `get`, `status` and `cancel` calls (REST on loopback, CLI, MCP — including the remote MCP listener).
- A **dedicated media file listener** (opt-in, own port and bind) that serves only short-lived file URLs, for
  local and remote clients, with TLS through the reverse proxy.
- The folder layout under `media/`, with one name file per chat folder.
- Background download: streamed into a temporary file, renamed when complete; a fixed free-disk reserve; no size
  cap; an idle limit and a total transfer limit; cancel. There is **no resume**: a retry starts from zero.
- Recovery of an expired link through a re-upload request to the owner's own phone, in its own lane.
- Permanent-failure states, a circuit breaker, pacing, and one hourly window (requests and bytes) for re-upload
  requests; a kill switch (off by default).
- Schema **v9**: five new `media_refs` columns for history rows (document file name, plaintext hash, the raw chat
  and sender JIDs from the message key, the view-once flag) and a partial index on the revoke label.
- Deleted and revoked content keeps its files, moved into the chat's `_deleted/` folder.
- Media counters and disk numbers in `/api/status`.
- `run-profile.sh clear-data-with-media` (new; `clear-data` keeps `media/`) and a media choice in the
  `deploy.sh uninstall` backup.

**Out of scope (later):**
- **Live media** (owner, 2026-10-30, confirmed 2026-11-01). Live ingest is unchanged: it downloads eagerly for the
  inbound event (50 MiB limit) and writes no `media_refs` row. A media message without a reference reports
  `no_reference` (with `source: live`). A follow-up can store live references, after the live `from_me` fix
  (roadmap).
- **View-once media** is refused (owner, 2026-11-03).
- Bulk downloads ("all media of this chat"): a job system like `fetch-history`.
- Contact names in folders and name files before F7 stores them (phone numbers until then).
- HTTP `Range` and `HEAD` on the file listener (owner, 2026-11-01); SSE events for "ready"; a progress percentage
  (a byte count is given); file bytes inside MCP results (owner, 2026-11-03: URL only).
- MCP resources (`resources/read`); the MCP registry serves tools only.
- Selective purge commands (a documented `rm` covers the `_deleted/` folders).

---

## 2. Background (facts the design relies on)

| Fact | Source |
|---|---|
| `media_refs(message_id PK, media_key, direct_path, file_enc_sha256, mimetype, file_length, width, height, hydrated_path)` (schema v8); written for media messages stored from history, only when the message row is new, by a second statement after the message insert (a failure is only counted); `hydrated_path` is NULL | `src/storage.rs`; `src/bridge.rs` `persist_history_messages` |
| Messages stored before the 2026-10-22 amendment, and every live media message, have no `media_refs` row; `messages.source` is `live`, `bootstrap` or `backfill` | `src/bridge.rs` (`MediaMode::Download` writes no ref); `src/storage.rs` |
| History rows store the chat under its phone JID even when the key used a LID, a full sender JID (`…@s.whatsapp.net`, or `…@lid` when unmapped) and the key's `from_me`; the raw `remote_jid` and `participant` are not kept. A key without a participant uses its `remote_jid` as the sender, so own history messages name the group (groups) or the other person (1:1 chats) as the sender (a bug, fixed in phase 2). Live rows store a bare sender number (the user part; an unmapped LID keeps its LID user part) and always `from_me = 0` (a bug, fixed in phase 2) | `src/bridge.rs` `persist_history_messages`, `resolve_sender`; `src/storage.rs` `insert_inbound` |
| Backfill pages strictly backward from the oldest stored message, so it does not revisit stored rows; a re-pair (pairing package) or a retried page can | `src/backfill.rs` (~77–91) |
| A document's original file name exists only inside the display label `[doc: NAME SIZE] caption` in `messages.body_text` | `src/bridge.rs` `MediaMode` |
| History extraction unwraps view-once wrappers into a normal media message; the view-once flag is set only on live events and is not stored | `src/bridge.rs` `unwrap_to_inner` (~3957), live flag (~3316) |
| `messages.timestamp` is epoch seconds; `messages.content_kind` is `image`/`audio`/`video`/`document`/`sticker` for media and maps 1:1 to the wa-rs `MediaType` | `src/storage.rs`; `src/bridge.rs` |
| `messages.message_id` is globally unique and sender-chosen; inserts are `OR IGNORE`; the `messages` indexes are `(chat_jid, timestamp)`, `message_id` and `embed_status` | `src/storage.rs` (~304–309) |
| `status@broadcast` and `@newsletter` messages are ignored, never stored, so there is no plaintext (newsletter) media | `src/bridge.rs` `should_ignore_jid` |
| A sender revoke is stored as its own row (`content_kind = 'revoke'`, label `[deleted <target id>]`); extraction keeps only the target's `key.id` and drops the key's `from_me`, `participant` and `remote_jid`; the revoker is the row's sender (live rows: `from_me = 0` always); the target row stays | `src/bridge.rs` `InboundContent::Revoke` (~527, ~4387-4394) |
| The revoke key is written by the revoker: a sender revoke has `from_me = true` and no participant in every chat, an admin revoke `from_me = false` and the author's JID as participant; in a 1:1 chat its `remote_jid` is the revoker's view of the chat | wa-rs `src/send/actions.rs` (~33-63) **[wa-rs]** |
| For an own message from a LID device, wa-rs sets the chat to the `recipient` (the other person, often `…@lid`) and `sender_alt` to the account's own phone JID; `recipient_alt` is never filled at this pin. Live ingest resolved such a chat with `sender_alt`, so own messages in LID-addressed 1:1 chats were stored under the account's own number (fixed before phase 2a; earlier rows keep the wrong chat) | wacore `src/messages.rs` (~578-597); `src/bridge.rs` `handle_event`, `resolve_chat_jid` **[wa-rs]** |
| A v8 binary refuses a newer DB only through `user_version > 8`, and at its own version it re-validates when `schema_validated_version` is stale; its validation checks the `messages` columns it knows, and its statements name their `media_refs` columns, so extra nullable columns and an extra index do not disturb it | `src/storage.rs` (~693, ~868, `validate_migration_post_commit`) |
| **The owner's largest profile (shape report, 2026-11-08):** 100 MB; 26,080 messages (23,318 from the pairing history, 2,762 live); 6,355 media references; 929 own history rows, **all** naming the group or the other person as sender; 34 own live rows, all with `from_me = 0`; 58 revoke rows, all live (none from history); no fetch cursor; no dotted LID in any form (0 of 230 mapping rows, 0 senders); the own LID in the `device` row equals its mapping row; 31 live rows sit in the own-number chat | `scripts/db-shape-report.sh` (counts and shapes only) |
| Local deletes happen only in the event handler, for deletes synced from the phone (REST and CLI deletes come back that way); each delete event runs in its own spawned task; `delete_inbound_*` removes `messages` and `media_refs` in one transaction; `delete_inbound_message` matches the message ID only, although the event also carries the chat | `src/bridge.rs` (~3674–3712); `src/storage.rs` (~1963–1979) |
| The public parts of a download: `client.refresh_media_conn(force)` (hosts and auth; `force = true` sends an IQ); `MediaConnection::from(&MediaConn)`; `DownloadUtils::prepare_download_requests(&dyn Downloadable, &MediaConnection)` (one URL per host); `DownloadUtils::decrypt_stream_to_writer(reader, media_key, media_type, writer)` (AES-CBC in 8 KiB chunks, the MAC checked at the end; errors are `anyhow` texts such as "MAC mismatch" and "Invalid final media size", or a wrapped `io::Error`; some plaintext is written before an error) | wa-rs `src/mediaconn.rs`, `src/download.rs`, `wacore/src/download.rs` (~350–467) **[wa-rs]** |
| wa-rs's own `download_to_writer` seeks the writer to 0 for every host attempt and treats any error, a writer error included, as "try the next host" | wa-rs `src/download.rs` (~228–237, 425) **[wa-rs]** |
| The shared HTTP client is `UreqHttpClient::new()` with ureq's default `http_status_as_error = true`: every 4xx/5xx becomes the error `http status: <n>`, so wa-rs's 404/410 ("not found/expired") and 401/403 branches never run, and its media-connection refresh retry neither | ureq 3.3.0 `config.rs:867`, `run.rs:126-129`; wa-rs `http_clients/ureq-client` `lib.rs:56-75,139` **[wa-rs]** |
| ureq 3.3 timeouts: for the phase being checked, `next_timeout()` starts from **now**, and the body loop checks `RecvBody` on every read, so **`timeout_recv_body` is an idle-read limit**, not a total. Every check also includes `Global` and `PerCall`, whose start times are recorded when the call is created, so **`timeout_global` is the total limit** of a download. Both are settable per request through `config()`. Redirects default to 10 (`max_redirects`; `max_redirects(0)` returns a 3xx response). A 2026-11-04 spike confirmed the idle behaviour over HTTP (a 10 s body in 1 s chunks passed a 5 s `timeout_recv_body`) | ureq 3.3.0 `timings.rs:63-68, 96-100, 152-172`, `run.rs` (~674, ~714), `config.rs:203, 669-748, 874`, `request.rs:349`; spike in `/tmp/ureq-idle-spike` |
| `direct_path` is sender-controlled and goes into the URL right after the host (`https://{host}{direct_path}…&auth=…`) with no check | `wacore/src/download.rs:291-294` |
| The re-upload request is a `server-error` receipt with an encrypted payload and an `<rmr jid from_me participant>` child; the phone answers with a `mediaretry` notification: `Success { direct_path }`, `NotFound` (code 2), `DecryptionError` (code 3), `GeneralError`; the server ACKs the receipt, with an `error` attribute when it rejects it | `wacore/src/media_retry.rs`; upstream `main` `src/features/media_reupload.rs` **[wa-rs]** |
| **The pin's `client.media_reupload()` sends the receipt `to` the account's phone number. On the owner's LID-migrated account the phone never answered (0 of 10). The same receipt `to` the account's own LID (`client.get_lid()` without the device part), with the chat and the participant in LID form, worked 3 of 3** (1:1, group from a member, the account's own group message); ACK in 0.2 s, answer in about 1 s; no size mismatch on 5 photos. Upstream `main` does the same and keeps a PN chat when no mapping is known. The parts are public at the pin: `encrypt_media_retry_receipt`, `build_media_retry_receipt`, `parse_media_retry_notification`, `client.send_node`, `client.wait_for_node(NodeFilter)` | phase 1 live test 2026-11-01 (HOWTO §3 "Media re-upload probe"; ADR 0050 Notes) **[wa-rs]** |
| Most history media is probably expired: CDN links live hours to days | ADR 0005 |
| **Old-file test matrix (2026-11-05, the owner's second account, which has a LID):** expired links answer **`http status: 403`** (4 of 4; not 404/410). The `web` request recovered files up to **4.8 years old** (1:1 chats and a group video, 4 of 4), with the chat and the group participant sent in **phone-JID form** (no LID mapping existed), so the stored form works when `to` is the own LID. Failures: an own group message, because history stores the **group JID as the sender of own group messages** (the key has no participant), and the server did not ACK a request naming the group as participant; and one fresh `@lid` chat, where the phone answered within 1 s but its answer did not decrypt with the media key that had just downloaded the file | `runs/dev-live/whatsrust-probe.log`; ADR 0050 Notes |
| **Retest (2026-11-06):** own group messages succeed with the own LID as the participant (3 of 3, up to 1606 d); `@lid` chats succeed (2 of 2 old, one an own message). Two fresh messages of one `@lid` chat failed: one answer did not decrypt, one re-uploaded file failed its MAC, so the phone used another media key than the one stored from history; their links were still valid. Across both runs, 13 of 15 `web` requests succeeded | ADR 0050 Notes |
| wa-rs logs every failed CDN download at WARN with the **full URL, including the media-connection `auth=` token** (60 of 60 such lines in the probe log; filtered from the daemon log since bd5f785, which also covers the DEBUG/TRACE stanza and HTTP traces that print it); the pin also appends `?auth=…` after a path that already has a query string (a double `?`), which the CDN accepts | `runs/dev-live/whatsrust-probe.log`; wa-rs `src/download.rs` (the "Trying next host" warning), `wacore/src/download.rs:291-293` |
| MCP tool budget 40 s; CLI client timeout 45 s; MCP turns an HTTP status ≥ 400 into `isError` | `src/mcp_http.rs` `TOOL_BUDGET`; `src/bridge.rs` `SYNC_CLIENT_TIMEOUT`; `src/mcp_registry.rs` (~477) |
| The REST response type buffers the whole body (`ApiResponse.body: Vec<u8>`); only SSE streams, with a write timeout | `src/api.rs` |
| Every REST route passes the Origin/Host/Content-Type gate, then the bearer-token check when a token is set; the gate blocks cross-site POSTs, not cross-site GETs. `CallContext.loopback_file_access` is the REST listener's bind (`cfg.is_loopback`), not the peer; the MCP HTTP runner sets false | `src/api.rs` `reject_from_head`, `CallContext` (~2126, ~2264); ADR 0044 |
| The MCP HTTP listener is a separate listener (own port, bind, token), built for remote clients behind a TLS proxy; it serves only `POST /mcp`; `read_request_head` and the head deadline are reusable | `src/mcp_http.rs`; `src/api.rs`; ADR 0045; `deploy/proxy/` |
| The stdio MCP server (`whatsrust mcp`) is a client of the loopback REST listener: every tool call is an HTTP request to it, so the daemon sees it as a loopback REST caller; `rest_tool_result` passes the body through | `src/mcp.rs` (~132–213); `src/mcp_registry.rs` (~477) |
| `--rollback` runs only after a **failed** migration (the migration pin must say `failed`); it refuses a healthy database | `src/main.rs` (~1484–1498); ADR 0030 |
| `GroupInfo { jid, subject, participants: [{ jid, phone, is_admin }] }` from `get_group_info` (a network call with no timeout, cached 5 minutes; failures are not cached) | `src/bridge.rs` (~1261) |
| A panic anywhere ends the process (`exit(101)`; release builds `panic = "abort"`) | `src/main.rs` (~234–257); `Cargo.toml` |
| No contact names are stored | FEATURES F7 item 1 |
| In `Cargo.lock` already: `ureq` 3.3.0 (wa-rs builds it with `default-features = false, features = ["rustls"]`), `fs2`, `rand`, `percent-encoding`, `sha2`, `libc` | `Cargo.lock` |

---

## 3. Media identity and lookup

- The **media ID is the message ID**. It is a sender-chosen string, usually 16–32 hex or upper-case characters; the
  file name uses a bounded form of it (§5.4).
- A lookup joins `messages` (`chat_jid`, `sender_jid`, `from_me`, `timestamp`, `content_kind`, `source`) with
  `media_refs`.
- Outcomes before any download, all final:
  - no message → `unknown`;
  - not a media kind → `no_media`;
  - a media kind without a `media_refs` row → `no_reference` (with the row's `source` and `timestamp`, so a live
    message is told apart from an old history row);
  - a row without `media_key`, `direct_path` or `file_enc_sha256`, or with a `direct_path` that fails the check of
    §6.2 → `incomplete_reference`;
  - a valid revoke of the message is stored (`is_revoked`, §5.5) → `revoked`;
  - `view_once` is true → `view_once` (a row stored before v9 has `view_once` NULL: unknown, allowed, and
    `status` says `view_once: unknown`).

---

## 4. Schema v9

An additive migration through the existing staged-migration machinery (pre-migration backup, validation, circuit
breaker; ADR 0028–0032; the schema version goes from 8 to 9): six `media_refs` columns and one partial index,
`CREATE INDEX … ON messages(body_text) WHERE content_kind = 'revoke'` (the revoke label lookup of §5.5). The step is
re-runnable: each column is added only when missing, and the index uses `IF NOT EXISTS`. The same guarded adds
(`ensure_v9_columns`) also run on the re-validation branch, so `--migrate` can fix a v9 DB that misses a column.

| New column | Type | Filled from | Used for |
|---|---|---|---|
| `file_name` | TEXT | the document's original file name | the document name suffix (§5.4) |
| `file_sha256` | BLOB | the proto's plaintext hash | a check of the decrypted file after the download (the MAC already authenticates the encrypted bytes; kept by the owner, 2026-11-01) |
| `remote_jid` | TEXT | the message key's `remote_jid` as the phone sent it (LID or phone) | the re-upload address of a 1:1 chat (§6.2) |
| `participant` | TEXT | the message key's `participant` as the phone sent it | the re-upload address of a group sender (§6.2) |
| `view_once` | INTEGER | 1 when history extraction unwrapped a view-once wrapper (any of the three, also inside an ephemeral wrapper) or the media message carries its own view-once flag, else 0 | the final state `view_once` |
| `media_key_timestamp` | INTEGER | the media message's `mediaKeyTimestamp` | tells a newer encryption of the same message apart (a later replacement rule needs no migration) |

- Existing rows keep NULL in the new columns; nothing is backfilled from `body_text`. A NULL `view_once` means
  "unknown" and is allowed. Their re-upload address comes from the LID↔phone mapping (§6.2); `status` reports the
  source (`addressing: raw | mapped | stored | unresolved`).
- **The message and its reference are written in one transaction** (one storage call), so a crash or a failed
  insert can no longer leave a history media message without its reference.
- History ingest writes the new columns for new rows. For a **duplicate** (a message fetched again), and **only
  when the incoming message matches the stored row** (the same chat: a 1:1 chat as the same user in phone or LID
  form, a group exactly; the same sender as the same user in phone or LID form, and two own rows always match; the
  same `from_me`; the same `content_kind`):
  - an existing `media_refs` row gets its NULL columns filled: the encryption tuple (`media_key`, `direct_path`,
    `file_enc_sha256`, `media_key_timestamp`) only as a whole, when the stored `media_key`, `direct_path` and
    `file_enc_sha256` are all NULL; the plaintext properties (`file_sha256`, `file_length`, `mimetype`, `width`,
    `height`, `file_name`, `remote_jid`, `participant`) one by one; `view_once` takes the larger value, so a stored 1
    is never lowered;
  - a missing `media_refs` row is **created when the stored message's `source` is not `live`** (`bootstrap` or
    `backfill`); a live row stays without a reference, as live media is out of scope (owner, 2026-11-03; this
    revises the "fill only" rule of 2026-11-01).
  - A mismatch changes nothing; for a media message it is counted (`duplicate_mismatch` in the persist counts)
    and logged once per batch. Message IDs are sender-chosen, so a colliding ID must never fill or create another message's reference.
- Because backfill does not revisit stored rows (§2), old history rows without a reference are repaired by a
  re-pair that keeps history, or by `clear-data` followed by a new fetch (HOWTO).
- No column for the transient states: `ready` is derived from `hydrated_path` (§6.1).
- **Own-message attribution** (the first commit of phase 2; owner, 2026-11-06). New rows store the right values:
  - live ingest stores the event's own `is_from_me` (today `insert_inbound` always writes 0);
  - history ingest: a message with `from_me` and no participant stores the account's own phone JID
    (`<number>@s.whatsapp.net`, device part removed) as the sender, not the key's `remote_jid`. Without a known
    own number (no paired device row), the current fallback stays.
- **The repair of stored rows** (owner-authorised 2026-11-06; scope and timing 2026-11-08) corrects stored data for
  `history`, `search` and the fetch cursor; the duplicate and revoke rules are identity-based and do not depend on
  it. It runs **at every start that knows the owner's number**, after the migration, in its own transaction, and
  changes rows only when misattributed rows exist (after a v8 window following a downgrade, a LID that became known,
  a failed lookup at ingest):
  - one predicate, "is the owner": the sender's bare user (before `@` and `:`; a `.` stays part of a LID user, as
    wacore keeps it) equals the owner's number or LID, or a LID the mapping gives for the owner's number. The owner's
    number and LID come from the `device` row; a missing row, an empty `pn` or a missing column means "no identity";
  - live rows with `from_me = 0` whose sender is the owner get `from_me = 1`;
  - history rows (`bootstrap`, `backfill`) with `from_me = 1` whose sender is not the owner (the old fallback named
    the group or the other person) get the own phone JID as the sender;
  - a fetch cursor whose anchor flag (`backfill_cursor.oldest_msg_from_me`) is 0 or NULL while its anchor message
    (`oldest_msg_id` in the same chat) now has `from_me = 1` gets 1;
  - the distinct senders are classified first; the updates are set-wise (`UPDATE … WHERE sender_jid IN (…)`), with
    the full-text update trigger dropped and re-created inside the transaction (the text does not change);
  - **before it changes rows outside the migrating start, it takes a backup** (`whatsapp.db.pre-repair-<ts>.bak`,
    mode 600; the migrating start is covered by the pre-migration backup); a failed backup skips the repair until
    the next start;
  - an in-transaction post-condition (no owner live row with `from_me = 0`, no own history row naming another
    sender) guards the commit; a failure rolls back and persists a failure note (time and reason, no JIDs), cleared
    by the next success;
  - the marker `own_attribution_repair` records the owner's bare number at the first run. A marker with **another**
    number (a DB kept across a re-pair to another number) skips the repair, with one warning at the first start that
    sees it (a persisted note keeps later starts quiet). A DB re-paired to another number before its first v9 start
    is repaired as if the current number had always been the owner's (a documented limit);
  - the counts and the elapsed time are logged (no JIDs); the watchdog baseline is seeded after the repair.
  - It is a startup data repair outside the version step: ADR 0028 records the pattern (idempotent, backed up,
    post-conditioned, guarded).
- **Budget:** the migration and the repair finish within 30 s on a synthetic DB of 500,000 messages, the backup
  included; the test reports the time, the WAL peak and the time of a no-op repair (the owner's largest profile:
  26,080 messages, 963 rows to repair; §2). First measurement (run 2a, debug build): 7.2–8.1 s, WAL peak 77 MiB.
- **The way back after a healthy v8 → v9 migration is an in-place downgrade:** stop the daemon, set
  `PRAGMA user_version = 8`, start the v8 build. A v8 binary refuses v9 only by the version number and re-validates
  at its own version (§2); v9 adds only nullable columns and an index, and the repaired rows are harmless to it, so
  nothing received since is lost. Phase 2a proves this with a test that runs the real v8 binary on a synthetic v9
  DB (run 2a: the v8 build's `--migrate` accepted it in 0.23 s, and a v9 build migrated it again); restoring the
  pre-migration backup is the fallback (it loses everything received since, including the Signal state, so a re-pair
  may follow). `--rollback` serves failed migrations only; after a post-commit validation failure the DB is already
  at v9, so `--migrate` re-validates (with `ensure_v9_columns`) and `--rollback` restores the backup. Restoring
  the backup is the documented fallback; keep the v8 backup until the engine is validated. A later v9 start
  migrates again (the step is re-runnable). HOWTO gets both procedures.
- Therefore phase 2 is merged to `main` **only after the old-file test matrix passes** (§11.1, §13). After that it
  may be merged before the engine; the owner decides. Its benefit is that new rows carry the raw JIDs and the flags
  earlier.

---

## 5. Folder layout

### 5.1 Where

`media/` beside `whatsapp.db`, resolved to an absolute path once at startup. Each dev profile (F9) and each
deployed instance (F10) has its own. Directory mode 700, files mode 600. `media/` is added to `.gitignore`.
`hydrated_path` is stored **relative to `media/`**, so a moved instance directory keeps working.

### 5.2 Chat folders

| Chat kind | JID | Folder |
|---|---|---|
| 1:1 | `972544321321@s.whatsapp.net` | `media/chat_972544321321/` |
| Group | `120363012345678901@g.us` | `media/group_120363012345678901/` |
| Chat known only by its LID | `123456789012345@lid` | `media/lid_123456789012345/` |
| Any other | `<user>@<server>` | `media/<user>@<server>/` (sanitised) |

- The prefix keeps a LID apart from a phone number and maps back to the JID exactly. A `lid_` folder exists only
  for a chat that whatsrust could not map to a phone; merging it into the phone folder belongs to the `@lid` DM
  split (roadmap P2), which then moves the files and rewrites `hydrated_path`. Newsletters are never stored (§2),
  so they have no folder.
- Every chat folder may hold one reserved subfolder, **`_deleted/`** (§5.5). No downloaded file can have that name:
  file names are flat and always start with a digit or a phone number (§5.4).

### 5.3 The name file

- Exactly one per folder: `_group_<name>.txt` or `_chat_<name>.txt`.
- `<name>`: the group subject; for a 1:1 chat the contact name once F7 stores names, until then the international
  phone number.
- **Sanitising** (also used for document names, §5.4) is an **allow-list** built from the standard library, with
  no extra crates:
  - a character is kept only when `char::is_alphanumeric()` is true (letters and digits of any script, for example
    Hebrew), or it is `-` or `.`; every other character becomes `_`. Controls, format characters, the bidirectional
    overrides (such as U+202E), punctuation, symbols and emoji are therefore removed by construction;
  - runs of `_` collapsed; trimmed of `_` and `.`; lower case;
  - **at most 64 bytes of UTF-8, cut at a character boundary**; an empty result falls back to the number.
  - There is no Unicode normalisation: a name written in two normalisation forms gives two spellings (the name file
    is then renamed once). Some combining marks (for example Hebrew niqqud) may be lost.
  - The code uses no byte indexing and no unchecked arithmetic (a panic ends the whole daemon, §2), and it is
    property-tested with hostile Unicode.
- **Never blocks a download.** The download writes the name file with the last known name (else the number). A
  group name refresh runs beside it: one lookup per group per 10 minutes at most, time-boxed to 10 s, a failure
  remembered for 10 minutes (the owner left the group, the group is gone, network). It uses the 5-minute group
  cache.
- When the name changed, the old name file is renamed; its `deleted:` lines are kept (§5.5).
- Content (UTF-8):

```
group: Family
jid: 120363012345678901@g.us
updated: 1761717600
participants:
972501234567 admin
972509876543
```

For a 1:1 chat: `chat:`, `jid:`, `updated:`. Participant names are added once F7 stores them.

### 5.4 File names

| Chat | Pattern | Example |
|---|---|---|
| Group | `<sender phone>_<epoch seconds>_<id>.<ext>` | `972501234567_1761717600_3EB0A1B2C3D4.jpg` |
| 1:1 and others | `<epoch seconds>_<id>.<ext>` | `1761717600_3EB0A1B2C3D4.jpg` |
| A document (either pattern) | `…_<id>_<original name>.<ext>` | `972501234567_1761717600_3EB0A1B2C3D4_quarterly_report.pdf` |

- The epoch is the message's own time; 10 digits sort correctly as text.
- The sender phone is international; own messages (the key's `from_me`) use the owner's number; a `@lid` sender is
  mapped through `lid_pn_mapping`, else `lid<number>`.
- **`<id>`** is the message ID when it already consists of 1–64 characters of `[A-Za-z0-9._-]`. Any other ID (too
  long, or with other characters) becomes `h` + the first 32 hex characters of its SHA-256. So a name can never
  contain a path separator, has a bounded length, and two different IDs never share a name.
- **The extension comes only from an allowlist keyed by the MIME type** (type and subtype, parameters such as
  `; codecs=opus` stripped): for example `image/jpeg` → `jpg`, `image/png` → `png`, `image/webp` → `webp`,
  `audio/ogg` → `ogg`, `audio/mpeg` → `mp3`, `video/mp4` → `mp4`, `application/pdf` → `pdf`, common office
  formats, `text/plain` → `txt`. Anything else → `bin`. The sender's own extension is never used, so a file can
  never land as `.command`, `.app`, `.jar`, `.sh`, `.webloc` or similar.
- A document's original name is sanitised (§5.3, at most 64 bytes) and its own extension removed before it becomes
  the suffix. A whole file name stays well under the 255-byte limit. Whatever the original name contains, it has
  no meaning to whatsrust: "deleted" is a folder, not a name (§5.5).
- **macOS:** every downloaded file gets the `com.apple.quarantine` extended attribute (best-effort, `libc::setxattr`),
  so opening it goes through Gatekeeper like a browser download (kept by the owner, 2026-11-01).
- While downloading, the file is `<name>.part`; it is renamed when complete. A rename never overwrites.
- **Adoption:** a final file of the message in the chat folder is adopted as `ready` without a download. The file
  is found by the **ID part of its name** (`…_<id>.<ext>` or `…_<id>_<name>.<ext>`; the ID part is unique), not by
  recomputing the whole name, because the sender part and the folder can change with the LID mapping. It is adopted
  when its SHA-256 equals `file_sha256` (when stored), else when its size equals `file_length`; with neither value,
  it is not adopted and the file is downloaded again. This covers a crash after the rename and before the DB
  update, and files kept by `clear-data` or a fresh re-pair.

### 5.5 Deleted and revoked content (files are kept and moved)

Deleting a message or a chat (synced from the phone; REST and CLI deletes arrive the same way) deletes the DB rows,
including `media_refs`, but **never a media file**. A valid sender revoke keeps the rows (§2) and moves the file.

**One primitive: `tag(path)`** moves a file from `<chat folder>/<name>` to `<chat folder>/_deleted/<name>` (it creates
the folder; mode 700). It is idempotent: a file already in `_deleted/` stays there, and a missing file is a no-op.
"Is tagged" means "lives in `_deleted/`"; nothing reads the file name for it. Its inverse, `untag(path)`, moves a
file back when its message is valid again.

**Only an observed event tags a file:**
- **A message delete or a chat delete:** the file set is the `hydrated_path` of exactly the rows the delete removes,
  read in the same transaction (not the files of a folder, so a chat delete whose event JID differs from the stored
  chat JID still finds the right files). A message delete matches the message ID **and** the event's chat (resolved
  to the stored chat JID), so a colliding ID in another chat is never touched.
- **A valid revoke** is judged by identities, **when it is read** (owner, 2026-11-08):
  - a revoke counts when a stored revoke row in the same chat (a 1:1 chat as the same user, a group exactly)
    carries the exact label `[deleted <id>]` and its revoker is the target's author. **In a 1:1 chat** the author
    test is `from_me` equality between the revoke row and the target (the repaired flag; no LID mapping needed).
    **In a group** it is the owner for an own target (the repaired `from_me` or the owner's identities), else the
    target's sender as the same user (phone or LID form, through the mapping; the mapping comes from server stanza
    attributes and own-phone sync, one hop only, so a reassigned phone number would merge users). An admin's delete
    in a group does not count: the admin is not the author (the media stays available; documented);
  - **revoke rows are stored past the allowlist:** a revoke from a sender not on `WHATSAPP_ALLOWED` is
    stored as its tombstone row only (no event, no read receipt), because history stores that sender's media
    regardless of the allowlist (owner, 2026-11-08). A tombstone is stored only when the target ID and the stanza ID
    look like message IDs (1–128 bytes of `[A-Za-z0-9._=+-]`), so an excluded sender cannot put free text into
    searchable history; it is stored under `message_id = tombstone:<stanza id>`, so it can never take the ID of a real
    message (the fetch anchor skips such rows); an identical tombstone (chat, sender, label) is not stored twice;
    tombstones naming distinct targets are not capped per sender (a residual risk) (2b code review, 2026-11-08);
  - **revoked wins:** a message judged revoked is never `ready`; `get` and `status` answer `revoked`, and a
    downloaded file of it is tagged at that moment (idempotent; tagging a file of a message judged revoked is safe,
    unlike tagging by absence);
  - **policy:** every remaining false negative fails open (the media stays available) rather than hiding media by
    a guess; retention and prune features must keep `content_kind = 'revoke'` rows, which the judgement reads;
  - `is_revoked(message_id)` runs this check through the partial index of §4 wherever the state matters: `get`,
    `status`, the download start and its final `hydrated_path` write (§6.2). No column stores the result, so the
    order of arrival does not matter (a revoke stored before its target applies once the target is stored), no pass
    over old rows is needed, and a rule correction is a code change;
  - when a live revoke row is stored, the delete-and-revoke hook (phase 4) runs the same check and tags a
    downloaded file;
  - the revoke key (written from the revoker's view, §2) does not gate the check; its shape is logged at debug
    without JIDs, until real revokes confirm it (v5.2 review M4);
  - known false negatives (the media stays available): own revokes in LID 1:1 chats stored before the LID-chat fix
    (the revoke and its target may sit in different chats); own revokes before the repair has run (an unpaired
    profile), which count once it has run; a revoker in unmapped LID form against a target sender in phone form.
- **A message missing from the DB is never treated as deleted**: `clear-data`, a fresh re-pair and a DB restore
  leave the files in place, and adoption (§5.4) picks them up again.

**Intent journal (per event):** in the same transaction as the delete or the revoke, the event writes its own
metadata keys `media_tag_intent:<random id>:<n>`, each with at most 500 paths. After the commit it tags them and
deletes its own keys. Startup replays every leftover `media_tag_intent:*` key. Concurrent delete tasks never touch
each other's entries, and no single value grows with the size of a chat.

**Transitions of a downloaded file:**

| From | Event | To |
|---|---|---|
| `<chat>/<name>` (ready) | message or chat delete; valid revoke | `<chat>/_deleted/<name>` |
| `<chat>/_deleted/<name>` | the message is stored again, is not revoked, and `get` is called | `<chat>/<name>` (un-tagged, adopted) |
| `<chat>/<name>.part` | the download finishes and the row still exists | `<chat>/<name>`, `hydrated_path` set |
| `<chat>/<name>.part` | the download finishes and the row is gone, or the message is revoked | `<chat>/_deleted/<name>` (renamed straight there) |
| `<chat>/<name>.part` | the download fails, or startup | removed |

- **Chat deleted:** when the delete removed every row of the chat, the name file also gets a line
  `deleted: <epoch>`. New files of a chat that goes on land in the chat folder as usual.
- **Running downloads:** a queued download of a deleted or revoked message is dropped. A running one finishes; its
  DB update (§6.2 step 6) decides the last two rows of the table.
- **Startup** removes `*.part` files, replays the intents, and **counts** files outside `_deleted/` whose message
  ID is not in the DB (`orphan_files` in `/api/status`, one log line). It moves nothing else.
- **Reclaiming space:** `clear-data-with-media` (everything), or by hand: `rm -r media/*/_deleted` (documented in
  HOWTO). Only observed deletes and valid revokes put files there.
- This **narrows ADR 0012** for media: a delete no longer removes everything; the owner chose keep-and-mark
  (ADR 0050 says so).

---

## 6. Download engine

### 6.1 States

| State | Meaning | Final? |
|---|---|---|
| `unknown`, `no_media`, `no_reference`, `incomplete_reference`, `revoked`, `view_once` | see §3 | yes |
| `not_downloaded` | a complete reference, no file | – |
| `queued` | waiting for the download lane (`position` given) | – |
| `waiting_reupload` | the link expired; waiting for, or in, a re-upload request (`bytes_expected` given, so the cost to the phone is visible) | – |
| `downloading` | streaming; `bytes_written`, `bytes_expected` given | – |
| `ready` | `hydrated_path` set, outside `_deleted/`, the file exists, and the message is not revoked (revoked wins, §5.5) | yes (until deleted on disk) |
| `failed` | `reason`, `retryable`, `retry_after_ms` | depends on the reason |

A non-final item may show `waiting: disconnected` (the client is not connected) or `waiting: no_lid` (connected,
but the account has no LID yet). Neither is a failure.

Failure reasons:

| Reason | Retryable | When |
|---|---|---|
| `not_on_phone` | no, for 24 h (persisted, §6.5) | the phone answered `NotFound` to a request with a raw or mapped LID address |
| `not_on_phone` (stored address) | no, for 1 h | the same answer when the stored phone-number form was used (only if the phase 4 gate allows that form, §6.2) |
| `address_unresolved` | yes, on the next `get` (the mapping may be learned meanwhile) | no LID form exists for the chat (1:1) or the sender (group), and the stored form is not allowed (§6.2); the phone is not contacted |
| `reupload_rejected` | after 1 h | the server's ACK carried an `error` code (given in `detail`) |
| `reupload_failed` | after 1 h; never for `DecryptionError` | the phone answered `GeneralError` or `DecryptionError` |
| `expired_after_reupload` | after 1 h | the new path from a re-upload also answered as expired (no second re-upload in a row; counts toward the breaker) |
| `integrity` | no | a MAC or padding error on two hosts, more bytes than the response's `Content-Length`, a `file_sha256` mismatch |
| `disk_reserve` | yes (after space is freed) | the free-space reserve would be broken (§6.4) |
| `phone_offline` | yes | no re-upload answer in 30 s |
| `reupload_paused` | yes (`retry_after_ms`) | the re-upload breaker or the hourly window (§6.5) |
| `stalled` | yes | no byte for the idle limit (`detail: idle`), or the total transfer limit (`detail: transfer_limit`) (§6.3) |
| `network` | yes | connection errors, short bodies, 429 and 5xx on every host, other unexpected statuses (`detail` names the status) |
| `cancelled` | yes | `cancel` was called |
| `internal` | yes | anything else (logged) |

- `ready`, `revoked` and `view_once` survive restarts (derived from the DB). The re-upload controls and the
  `not_on_phone` memory persist (§6.5). Every other non-final state lives in memory and resets to `not_downloaded`
  on restart (the queue is not persisted; a client calls `get` again).
- **Per-item backoff:** a retryable failure sets a `not_before` time for that message (its `retry_after_ms`). A
  `get` inside that time answers the stored failure again and does not restart the download. Without it, a
  persistent stall on a large file would become an endless full re-download loop (there is no resume).
- `detail` comes from a closed vocabulary (status numbers, reason words); it is never built from raw error text,
  because CDN URLs carry the media `auth` token. The engine runs its own transfer and logs only that vocabulary;
  wa-rs's own download warnings (which print full URLs, §2) are filtered out of the log (roadmap P1).

### 6.2 Lanes

Two lanes, so a slow re-upload never blocks downloads. Both lanes reach WhatsApp only through two adapters with
fakes for tests: **`MediaTransfer`** (the HTTP download) and **`MediaReuploader`** (the re-upload request). **[wa-rs]**

**Download lane:** one worker; a bounded FIFO queue (1000 entries; when full, `get` answers `queue_full`,
retryable). A request for a message that is already queued or in progress joins it (single flight). Steps:

1. Free-disk check (§6.4).
2. Write or refresh the name file (§5.3; never waits for the network).
3. Check `direct_path`: it must start with exactly one `/`, contain only printable ASCII, and contain none of `@`,
   `\`, `#`. A failing path → `incomplete_reference` (final). The same check applies to a path from a re-upload
   answer.
4. Transfer (`MediaTransfer`, in `spawn_blocking`):
   - Hosts and auth from `client.refresh_media_conn(false)`; URLs from `DownloadUtils::prepare_download_requests`.
     Every URL host must be one the media connection named.
   - A **media-only ureq agent** (the shared wa-rs client is not changed) with ureq's own, versioned settings:
     `http_status_as_error(false)`, `max_redirects(0)` (a 3xx is "any other status"), `timeout_connect` 15 s,
     `timeout_recv_response` 30 s;
     - **idle limit:** `timeout_recv_body` = the stall period (`WHATSRUST_MEDIA_STALL_SECS`, default 60 s); it
       restarts on every read, so a dead connection fails within the stall period of its last byte;
     - **total limit:** per request `timeout_global` = 60 s + `file_length` ÷ 64 KiB/s, clamped to the transfer limit
       (`WHATSRUST_MEDIA_TRANSFER_MAX_SECS`, default 7200 s); a missing `file_length` uses the limit. At 2 h a 2 GB
       file needs about 290 KB/s; a slower link needs a higher limit.
     - The error tells them apart: `Timeout(RecvBody)` = idle, `Timeout(Global)` = total. No custom transport is
       needed (§12).
   - The response body goes through `DownloadUtils::decrypt_stream_to_writer` into `<name>.part`.
   - **The writer is position-based:** before each host attempt it truncates the `.part` file (`set_len(0)`) and
     resets its counter. It counts the plaintext bytes. A **sticky abort** (cancel, reserve breach) stops the
     transfer at its next write with its reason; an aborted transfer is never retried on another host.
   - **The hard cap is the response's `Content-Length`** (the ciphertext size the CDN declares). `file_length` is
     only a hint (for the reserve check, the total limit, the byte charge and `size_note`), because the sender
     declares it.
   - Statuses and errors map to actions per the table below.
5. `fsync`; check the plaintext hash against `file_sha256` when stored. A size that differs from `file_length`
   after a valid MAC is accepted and logged, larger or smaller (the MAC authenticates the bytes; `status` shows a
   `size_note`). Rename
   `<name>.part` → `<name>`; on macOS set the quarantine attribute.
6. In one transaction, check that the message row exists and is not revoked (`is_revoked`, §5.5), then set
   `hydrated_path`. Otherwise the message was deleted or revoked meanwhile → `tag` the file (§5.5).
7. On any failure: delete the `.part` file and set `failed(reason)`.

| Phase | Result | Action |
|---|---|---|
| connect, DNS, TLS, reset | error | next host; all hosts failed → `network` |
| response | 200 | stream (step 4) |
| response | 404, 410 | expired → the re-upload lane (`waiting_reupload`); after a re-upload already → `expired_after_reupload` |
| response | 401, 403 | refresh the media connection (`refresh_media_conn(true)`, **at most once per 60 s** for the whole lane; items in that window reuse the last result) and try every host once more; again 401/403 → treat as expired (the CDN may answer 403 for an expired signed link) |
| response | 429, 5xx | next host; all hosts → `network` (`retry_after_ms` 60 s) |
| response | 3xx, any other status | `network` with the status in `detail` |
| body | read error; the body ended before `Content-Length` | next host (the `.part` file is truncated); all hosts → `network` |
| body | `Timeout(RecvBody)` (idle) | `stalled` (`detail: idle`) |
| body | `Timeout(Global)` (total) | `stalled` (`detail: transfer_limit`) |
| body | more bytes than `Content-Length` | `integrity`, no retry |
| decrypt | "MAC mismatch", padding or "Invalid final media size" errors (checked after the byte count above) | next host once; again → `integrity` |
| decrypt | a wrapped `io::Error` (timeout, reset) | as a body read error |
| writer | sticky abort | its reason (`cancelled`, `disk_reserve`), no retry |

The decryption errors are matched by their `anyhow` text **[wa-rs]**; a pin test fixes those texts, so a library
upgrade cannot silently move them into another reason.

**Re-upload lane:** one task, behind the pacing, the hourly window (requests and bytes) and the breaker
(§6.5). `MediaReuploader` builds and sends the request itself, as the phase 1 live test proved works (§2): **[wa-rs]**

- `to`: the account's own LID without the device part (`client.get_lid()`). **An account without a LID** falls back
  to the pin's own request (`client.media_reupload()`, which addresses the phone number), recorded as
  `addressing: pn_account`; it is not proven on such an account (§11.2). While connected without a LID yet, the item
  shows `waiting: no_lid`.
- `rmr jid`: a group → the group JID; a 1:1 chat → its LID: the raw `remote_jid` when it is a LID, else the mapped
  LID (`lid_pn_mapping`).
- `participant` (groups only): the sender's LID: the raw `participant` when it is a LID, else the mapped LID;
  without the device part.
- **No LID form** for the 1:1 chat or the group sender: the stored phone JID is sent; the old-file matrix showed it
  works (§2). Only an item with neither form (for example a sender stored as a bare LID user with no mapping and no
  raw key) fails as `address_unresolved` without contacting the phone.
- **Own messages in a group:** the participant is the account's own LID without the device part, never the stored
  sender (history stores the group JID there, §2), as upstream `main` does.
- `from_me`: the stored key value (history rows carry the correct one).
- It waits up to 10 s for the server's ACK (an `error` attribute → `reupload_rejected`), then up to 30 s in total
  for the `mediaretry` notification.
- `Success` → store the new `direct_path` for this item (persisted for 1 h as `media_fresh_path:<id>`, so a restart
  does not cost the phone a second upload), mark it `reuploaded`, and put it at the **front** of the download queue.
  It still waits for a running transfer to end (one worker); the persisted path covers that wait. `NotFound` → `not_on_phone`. `GeneralError`, `DecryptionError` →
  `reupload_failed`. A timeout → `phone_offline` and a breaker count. A send error → `network`.
- `status` reports the address source (`raw`, `mapped`, `stored`, `unresolved` or `pn_account`).
- After the wa-rs upgrade, upstream `main`'s `client.media_reupload()` does the same; the adapter can switch to it.

### 6.3 Stalls and cancel

- **No watchdog thread.** ureq's `timeout_recv_body` restarts on every read (§2), so a read that gets no byte for
  the stall period fails by itself, and `timeout_global` ends any transfer at the transfer limit. Every transfer
  thread therefore ends within the stall period of its last byte, or at the transfer limit.
- `cancel` (REST, CLI, MCP) removes a queued item, or sets the sticky abort `cancelled` on the running transfer. The
  item answers `cancelled` at once; the transfer stops at its next write, or within the stall period when no byte
  arrives. The lane waits for that thread to end before it starts the next item, so at most one transfer thread
  exists. `cancel` with `all` empties the queue and cancels the running transfer.

### 6.4 Disk reserve

- A **fixed free-space reserve: 20 GB by default**, configurable (`WHATSRUST_MEDIA_MIN_FREE_GB`).
- Before a download: free space (`fs2::available_space` on the `media/` volume) minus `file_length` must stay at or
  above the reserve; else `failed(disk_reserve)`. A missing `file_length` counts as 0: there is **no byte limit**
  then, but the reserve check during the write and the transfer limit still apply (owner, 2026-11-01).
- While writing: the free space is checked again every 64 MiB; below the reserve the sticky abort `disk_reserve`
  stops the transfer.
- At startup, when the feature is on and the free space is already below the reserve, one WARN line says so (for
  example on a small VPS disk; lower the reserve to use the feature there).
- The reserve protects `whatsapp.db` and the Signal keys, which share the volume. `/api/status` shows the free
  bytes and the reserve (§8.4).

### 6.5 Re-upload controls

| Control | Default | Why |
|---|---|---|
| Re-upload pacer | at most 1 every 30 s | each request makes the owner's phone upload a file |
| **Re-upload window** | **20 requests and 1 GB per hour**, counted together in one sliding hour (the bytes are the `file_length` of each requested file; a missing length counts as 64 MiB) | each request makes the phone upload the whole file, possibly over a mobile network; the request count is the hard bound, the bytes an honest-sender guard (the sender declares `file_length`) |
| Re-upload breaker | after 3 consecutive timeouts or `expired_after_reupload` results: pause re-uploads 30 min, doubling to 4 h; reset by the first full success (re-upload and download) | an offline phone, or a systemic fault, must not receive a stream of requests |
| `not_on_phone` memory | 24 h (1 h with a stored-form address) | a repeated `get` answers at once without contacting the phone |

- A request that would exceed the window waits: the item fails as `reupload_paused`, with `retry_after_ms` computed
  exactly from the window's entries (the time until enough of them leave the hour).
- **A lone over-size file:** a file whose `file_length` alone is larger than the byte budget is admitted when the
  window is empty, and its bytes then fill the window. So no file is blocked forever while the answer says "retry".
- These **persist** in the `metadata` table: one small key `media_reupload_window` (the hour window's entries, the
  breaker state and its end time), one key per `not_on_phone` entry (`media_not_on_phone:<id>` = its expiry; at most
  10 000, the oldest dropped first), and one key per fresh re-upload path (`media_fresh_path:<id>`, 1 h). A restart, for example a crash
  restart by the F10 service supervisor, keeps them, and no write rewrites a large value.
- Downloads have **no token bucket** (owner, 2026-11-01): one worker already limits the rate, and the CDN is not
  the anti-ban surface. The pacer, the window and the breaker bound what reaches the phone. The media-connection
  refresh is rate-limited (§6.2).

---

## 7. Serving files

### 7.1 Who fetches the file

The main client is an **agent on another computer**, connected through the MCP HTTP listener (usually behind the
TLS proxy). A local agent or the owner's browser may also use the URL. A local file path is returned as well, but
only to callers of the loopback REST listener (§8.1).

### 7.2 The media file listener

- **A separate listener, off by default:** `WHATSRUST_MEDIA_PORT`, `WHATSRUST_MEDIA_BIND` (default `127.0.0.1`),
  `WHATSRUST_MEDIA_ALLOW_REMOTE` for a non-loopback bind (logs a warning), and `WHATSRUST_MEDIA_PUBLIC_URL`, the
  base URL that clients use (for example `https://wa.example.com`, through the proxy). On loopback the default is
  `http://127.0.0.1:<port>`; **a non-loopback bind requires `PUBLIC_URL`** (a wildcard bind has no usable default).
- **Startup rules** like the MCP listener: validated before the instance lock and the bridge (ADR 0022); an unsafe
  config refuses to start; the port is bound before the bridge starts.
- **Why a separate listener:** resource isolation (long file streams can never use up REST or MCP connections) and
  a URL a remote agent can reach without exposing the REST API. A separate port is a separate browser origin only
  when the client reaches it directly; behind the single-host proxy recipe the origin is shared with `/mcp`. The
  defences that hold in both cases are the response headers (§7.3) and the MCP listener's header-based auth (it
  uses no cookies).
- **Pipeline (per connection):**
  1. accept: at most 16 connections (more → 503 + `Retry-After: 10`);
  2. head: `read_request_head`, deadline 10 s from accept, at most 8 KiB;
  3. route: only `GET /media-files/<token>/<file name>`; other paths → 404; other methods → 405 (`HEAD` included);
     a request body (`Content-Length` > 0 or chunked) → 400;
  4. token: looked up in memory; unknown or expired → 404;
  5. name: the percent-decoded name must equal the issued name exactly; else 404;
  6. file: opened through the token's stored path; missing or moved into `_deleted/` → 404 (the URL path is never
     used to open a file);
  7. stream permit: at most 8 streams (more → 503 + `Retry-After: 10`);
  8. headers (§7.3), then the body (§7.4).
- **No bearer token:** the URL token is the credential. The REST listener and its pipeline are unchanged. Anyone
  who can reach the port can hold connections until the head deadline; the connection cap bounds that, and the proxy
  recipe adds per-client limits. Directly exposed (`ALLOW_REMOTE` without the proxy), the caps are global, so one
  client can keep others at 503; HOWTO recommends the proxy for any remote use.
- **Logging:** one line per request with the message ID, the status and the bytes sent; never the token or the
  URL.
- **The proxy recipe** (`deploy/proxy/` nginx and Caddy) gains a `/media-files/` location for this listener: nginx
  `proxy_buffering off`, `proxy_request_buffering off`, `access_log off` for the location (the token is in the URI;
  a buffering proxy would spool large files outside the reserve), and `limit_conn` plus `limit_req` per client
  address; Caddy without buffering, with the route's access log skipped and an equivalent rate limit. The proxy
  smoke playbook covers the route.

### 7.3 Response headers (every file response)

- `Content-Type`: for a type in the §5.4 allowlist, its validated value (`type/subtype` token grammar, parameters
  dropped); everything else `application/octet-stream`. A value that fails validation is never echoed.
- `Content-Disposition`: **`attachment` by default**; `inline` only for allowlisted `image/png`, `image/jpeg`,
  `image/gif`, `image/webp`, `audio/*` and `video/*` types. The file name is encoded per RFC 6266/5987
  (`filename*=UTF-8''…`).
- Always: `X-Content-Type-Options: nosniff`, `Content-Security-Policy: sandbox; default-src 'none'`,
  `Referrer-Policy: no-referrer`, `Cache-Control: private, no-store`, `Content-Length`.
- No header value is built from unvalidated data; CR and LF can never reach a header.

### 7.4 Streaming

- The file is copied in 64 KiB chunks (no whole-file buffer), with a per-write timeout of 30 s and a **minimum
  rate**: a stream that averages less than 16 KiB/s over a 60 s window is closed. A slow client cannot hold a
  stream permit for long.
- No `Range` support (owner, 2026-11-01): a `Range` header is ignored, and the whole file is sent with 200.
- A file that disappeared or was moved into `_deleted/` after the token was issued → 404. A stream that already
  started finishes; the token's expiry is checked once, at step 4 of the pipeline.

### 7.5 Tokens

- 32 random bytes from the OS RNG, base64url (43 characters).
- **One live token per file:** a repeated `get` on a ready file returns the same URL while more than 60 s of it
  remain; otherwise it mints a fresh token (the old one stays valid until it expires). At most 4096 live tokens (an
  expired one is dropped first; a full table answers `url_busy`, retryable).
- Valid 5 minutes (configurable), reusable within that time; kept in memory; lost on restart (a client calls `get`
  again).
- The file-name part of the URL is percent-encoded when issued, and decoded and compared when served.
- Tokens never appear in logs; a log line names the message ID instead.

---

## 8. API, CLI, MCP

### 8.1 REST (loopback REST listener)

| Method and path | Behaviour |
|---|---|
| `POST /api/media/get` body `{"id"}` | Starts or joins a download when needed and answers per the table below. A ready answer: `{ok: true, id, state: "ready", url, expires_at, file_name, size, mimetype}`, plus `path` for callers of the loopback REST listener. |
| `GET /api/media/status?id=` | 200 with the state and its fields (`bytes_written`, `bytes_expected`, `position`, `waiting`, `reason`, `detail`, `addressing`, `size_note`, `source` for `no_reference`, and `path` for loopback callers); never starts anything. |
| `POST /api/media/cancel` body `{"id"}` or `{"all": true}` | removes a queued item or stops the running download; `all` empties the queue and stops the running download. |

**`get` answers** (MCP turns a status ≥ 400 into `isError`, so every retryable answer is a 202 and the agent keeps
polling; every final answer is a 4xx):

| Situation | HTTP | Body |
|---|---|---|
| ready, URL available | 200 | `ok: true`, `url`, … |
| ready, the media listener is off (REST caller) | 200 | `ok: true`, `path`, `code: "url_unavailable"` |
| the media listener is off and the caller is remote (the MCP HTTP listener, or a request marked by the stdio MCP server) | 409 | `code: "url_unavailable"`; **nothing is started** (the file would be of no use to a remote agent) |
| not ready: `queued`, `downloading`, `waiting_reupload`, `not_downloaded` | 202 | `ok: false`, `code: "not_ready"`, `state`, `retryable: true`, `retry_after_ms` |
| a retryable failure (`network`, `stalled`, `disk_reserve`, `phone_offline`, `reupload_paused`, `reupload_rejected`, `reupload_failed`, `expired_after_reupload`, `address_unresolved`, `cancelled`, `internal`), `queue_full`, `url_busy` | 202 | `ok: false`, `code: <reason>`, `retryable: true`, `retry_after_ms` |
| `unknown`, `no_media`, `no_reference`, `incomplete_reference`, `view_once` | 404 | `code` |
| `revoked`, `not_on_phone` (inside its memory), `integrity` | 409 | `code` |
| the kill switch is off and the file is not ready | 503 | `code: "disabled"` |

- `get` and `cancel` are POSTs: they can make the daemon contact WhatsApp, and the request gate blocks cross-site
  POSTs but not cross-site GETs.
- **Caller class.** A caller is *local* (it gets `path`) only when it calls the loopback REST listener directly,
  like the CLI. It is *remote* when it comes through the MCP HTTP listener (`loopback_file_access = false`), or
  through the stdio MCP server, which adds the header `X-Whatsrust-Caller: mcp` to every REST request it makes. The
  media routes treat a marked request as remote. The marker can only remove capabilities, so a forged marker gains
  nothing. This covers an agent that runs `ssh <host> whatsrust mcp` from another computer.
- The REST API must not be published through a proxy (HOWTO says so): every unmarked caller of the loopback REST
  listener is treated as local.
- **URL base for remote callers:** when `WHATSRUST_MEDIA_PUBLIC_URL` is not set and the listener uses the loopback
  default, a remote caller's answer carries `url_note: "loopback URL; set WHATSRUST_MEDIA_PUBLIC_URL for a remote
  agent"`. At startup, one WARN line says the same when the MCP HTTP listener has a non-loopback bind, the media
  listener is on, and `PUBLIC_URL` is unset.
- Every row of the answer table is a REST golden case.

### 8.2 CLI

`whatsrust media-get <id>`, `whatsrust media-status <id>`, `whatsrust media-cancel <id>|--all` (JSON to stdout; a
`not_ready` or retryable answer exits 1). The CLI talks to the loopback REST listener, so it gets `path`.

### 8.3 MCP

| Tool | Access class | Annotations |
|---|---|---|
| `whatsrust_media_get` | Write | `readOnlyHint: false`, `destructiveHint: false`, `idempotentHint: true`, `openWorldHint: true` |
| `whatsrust_media_status` | Read | as the other read tools |
| `whatsrust_media_cancel` | Write | `destructiveHint: false`, `idempotentHint: true` |

- The existing Write class is used, as for `whatsrust_fetch_history` (no new access class; owner, 2026-11-01).
- Strict schemas (`id` string, required; `cancel` takes `id` or `all`; ADR 0046). Read-only MCP mode offers only
  `media_status` (the registry test that counts read tools is updated).
- The tools are always listed; with the feature off (the default) they answer `disabled` and change nothing.
- The MCP tools never return `path`, over either transport (§8.1 caller class). The URL comes from
  `WHATSRUST_MEDIA_PUBLIC_URL`, so a remote agent can use it. Results carry the URL only, never file bytes (owner,
  2026-11-03).
- The tool descriptions say that `file_name`, `mimetype` and the file content are third-party content, as the
  history tools say for message text.

### 8.4 `/api/status`

A `media` snapshot (`{enabled: false}` when the feature is off):
- counts: `queued`, `downloading`, `waiting_reupload`, `ready_total`, `failed` per reason, `orphan_files`
  (from startup), `deleted_files` and `deleted_bytes` (the `_deleted/` folders: counted at startup, updated by
  `tag` and `untag`), `duplicate_mismatch`;
- re-uploads: `reuploads_requested`, `reuploads_succeeded`, `reupload_hour_used` (requests and bytes),
  `reupload_breaker` (`closed`/`open` with the time left), `addressing` counts (`raw`, `mapped`, `stored`,
  `unresolved`, `pn_account`);
- transfer: `bytes_downloaded`, `media_conn_refreshes`, `stalls` (idle and transfer limit);
- disk: `free_bytes`, `reserve_bytes`;
- serving: `url_tokens_live`, `file_streams_active`, `file_connections_active`.

---

## 9. Owner decisions (2026-10-28 → 11-08)

| Topic | Decision |
|---|---|
| Folder | Named by chat ID with a type prefix, not by name; the name lives in `_group_<name>.txt` / `_chat_<name>.txt` |
| Group files | Prefixed with the sender's international phone, then the epoch seconds, then the message ID |
| Group name file | The participants' phone numbers (names after F7) |
| Names before F7 | Phone numbers |
| API | `get` returns a short-lived URL when ready; otherwise starts the fetch and fails (HTTP 202, `ok: false`); a separate `status` |
| URL | **Remote clients in scope**: the agent runs on another computer; a dedicated media file listener (2026-10-30) |
| HTTP method | `get` is a POST |
| Size | No size cap; a fixed free-space reserve of 20 GB, configurable (2026-10-30); a transfer limit of 2 h, configurable (2026-11-03) |
| Scope | One message per call; bulk later |
| Live media | **Not supported now** (2026-10-30; confirmed 2026-11-01); `no_reference` for such messages |
| View-once media | **Refused** (`view_once`); media from disappearing-message chats is allowed (2026-11-03) |
| Schema | **v9 migration authorised** for the new `media_refs` fields (2026-10-30); six columns: `file_name`, `file_sha256`, `remote_jid`, `participant` (2026-11-01), `revoked_at`, `view_once` (2026-11-03); `static_url` dropped (2026-11-01); **five** since 2026-11-08 (`revoked_at` dropped: revokes are judged when read) |
| Re-upload | Live-tested against the owner's account (2026-11-01): only the request to the own LID works; whatsrust builds it at this pin. The old-file test matrix gates phase 4 (2026-11-03) |
| Re-upload byte budget | 1 GB per hour, configurable (2026-11-03); counted in one window with the request cap (2026-11-04) |
| Transport | whatsrust runs the media HTTP transfer with its own agent (2026-11-01) |
| Duplicates | A duplicate history message fills an existing reference, and creates a missing one when the stored row is not live; the message and its reference are written in one transaction (2026-11-03; revises "fill only" of 2026-11-01) |
| No `file_length` | No byte limit; the reserve and the transfer limit apply (2026-11-01) |
| Kill switch | `WHATSRUST_MEDIA_DOWNLOAD`, **off by default** (2026-11-03); off stops new downloads only; ready files are still served (2026-11-01) |
| Revoke | Only a revoke by the target's sender counts; judged when read (2026-11-08; was: resolved at ingest into `revoked_at`); the file moves to `_deleted/`; `get` answers `revoked`; no new download (2026-11-01, rule 2026-11-03) |
| Deleted content | Files kept, moved into the chat's `_deleted/` folder (2026-11-03; was a `.deleted` name tag); a deleted chat also gets a `deleted:` line |
| A message that comes back | Its file is moved back and adopted (2026-11-01) |
| MCP results | URL only, no file bytes (2026-11-03) |
| Dropped | A new MCP access class, `Range`/`HEAD`, the download token bucket (2026-11-01) |
| Kept | The macOS quarantine attribute (2026-11-01) |
| `clear-data` | Keeps `media/`; a new `clear-data-with-media` deletes it |
| Deploy uninstall | Include `media/` in the backup by default (`[Y/n]`, 2026-10-30) |
| Document names | Keep the original file name as a suffix (sanitised; the extension from the allowlist) |
| Counters | A `media` snapshot in `/api/status` |
| wa-rs | Build on the current pin; the upgrade is a separate project afterwards (2026-10-30) |
| Rollout | Phase 2 (schema and ingest) is merged only after the old-file matrix passes, and may then be merged before the engine; the owner decides (2026-11-01, gate 2026-11-04) |
| Transfer limits | ureq's own `timeout_recv_body` (idle) and `timeout_global` (total); no custom transport (2026-11-04, after the spike) |
| stdio MCP | An agent using `ssh <host> whatsrust mcp` is supported; the stdio server marks its requests as remote (2026-11-04) |
| Revoke identities | The owner's own revoke counts whatever the stored `from_me` says; existing revoke rows get a one-time pass; a revoke before its target is not applied (2026-11-04) Superseded 2026-11-08: no pass; revokes are judged when read |
| Duplicates | Fill or create only when chat, sender, `from_me` and kind match (2026-11-04) |
| Re-upload window | Requests and bytes in one window; a lone over-size file is admitted into an empty window; the fresh path persists 1 h (2026-11-04) |
| Sanitiser | A std allow-list; no Unicode crates (2026-11-04) |
| Off | With the feature off, only the two lanes stop; the delete and revoke hooks, the journal replay, the `.part` cleanup and the counts always run (2026-11-04) |
| Matrix threshold | Go when the mapped 1:1 or the mapped group addressing recovers files older than one week; a LID-less account falls back to the pin's phone-number request (2026-11-04) |
| Number allowlist | `WHATSAPP_ALLOWED` does not scope media downloads, as for the history tools (2026-11-04) |
| Attribution | The own-message attribution fix is the first commit of phase 2, not its own branch; the v9 migration repairs the stored rows (live `from_me`, the own sender of history messages) before the one-time revoke pass (2026-11-06) Revised 2026-11-08: the repair runs once at the first start that knows the owner's number; no revoke pass |
| Repair scope | Own history rows whose sender is not one of the owner's identities; the live repair matches every form of the owner's number and LID; the fetch cursor's anchor flag is repaired too (2026-11-08) |
| LID 1:1 chats | Own messages in LID-addressed 1:1 chats are filed under the other person: a separate fix commit on the media branch before phase 2a; earlier rows keep the wrong chat (2026-11-08) |
| Revoke key | The corrected rule of §5.5 goes to a cold review (v5.2) before phase 2b; phase 2a may start meanwhile (2026-11-08) |
| Revoke model | Judged when read from the stored revoke rows; no `revoked_at`, no one-time pass; the key shape is only logged (2026-11-08, after the v5.2 cold review) |
| Unpaired upgrade | The repair waits for the first start that knows the owner's number; another number → skipped with a warning (2026-11-08; it runs at every such start since the v5.3 review) |
| Budget | 30 s for the migration and the repair on a synthetic 500,000-message DB (set by the coordinator; the owner gave no limit, 2026-11-08) |
| Repair runs | At every start that knows the owner's number, with a backup before it changes rows outside the migrating start; the marker is the re-pair guard (2026-11-08, after the v5.3 cold review) |
| Media key | A `media_key_timestamp` column in v9; the encryption tuple filled only as a whole (2026-11-08) |
| Revoke policy | Revoked wins (tag lazily); revoke tombstones stored past the allowlist; remaining false negatives fail open (2026-11-08) |
| Merge timing | Phase 2 merges to `main` after 2b; the engine follows on the branch (2026-11-08) |

---

## 10. Scripts and configuration

### 10.1 Scripts

- **`run-profile.sh clear-data`** is unchanged and keeps `media/` (the files stay, without DB rows, outside
  `_deleted/`; a later history ingest and `get` adopt them).
- **`run-profile.sh clear-data-with-media <profile> [--all] [--dry-run] [--yes] [--backup-media <file>]`** (new):
  the `clear-data` flow (refuses while the daemon runs, prints the exact SQL, backs up the DB, asks y/N and then
  the profile name), plus the size of `media/`, then deletes it. No media backup by default; `--backup-media
  <file>` writes an uncompressed `tar` (media is already compressed) after a free-space check, and verifies it.
- **`deploy.sh uninstall`**, when the data is backed up before deletion: shows the size of `media/` and asks
  "Include media/ (N GB) in the backup? [Y/n]" (default: include; flag `--backup-media yes|no`); a "no" leaves
  `media/` out; the script checks free space first and does not gzip the media part.
- **`deploy.sh re-pair` (fresh mode)** keeps `media/` in place (the same account re-links; nothing moves);
  HOWTO says to move it aside first when linking a different account.

### 10.2 Configuration

| Variable | Default | Floor / ceiling (override) |
|---|---|---|
| `WHATSRUST_MEDIA_DOWNLOAD` | **`off`** | `on` enables the two lanes; `off` = no download and no re-upload starts, a ready file is still served, a not-ready `get` answers `disabled`. The delete and revoke hooks, the journal replay, the `.part` cleanup, adoption and the counts run either way, so deleted files are still moved and no intent is left behind |
| `WHATSRUST_MEDIA_MIN_FREE_GB` | 20 | ≥ 1 |
| `WHATSRUST_MEDIA_TRANSFER_MAX_SECS` | 7200 | 600–86400 |
| `WHATSRUST_MEDIA_REUPLOAD_INTERVAL_SECS` | 30 | ≥ 10, unless `WHATSRUST_DANGEROUSLY_ALLOW_FAST_MEDIA_REUPLOAD=1` |
| `WHATSRUST_MEDIA_REUPLOAD_PER_HOUR` | 20 | ≤ 60, unless `WHATSRUST_DANGEROUSLY_ALLOW_FAST_MEDIA_REUPLOAD=1` |
| `WHATSRUST_MEDIA_REUPLOAD_MB_PER_HOUR` | 1024 | 50–10240, unless `WHATSRUST_DANGEROUSLY_ALLOW_FAST_MEDIA_REUPLOAD=1` |
| `WHATSRUST_MEDIA_STALL_SECS` | 60 (the idle limit, ureq `timeout_recv_body`) | 10–600 |
| `WHATSRUST_MEDIA_URL_TTL_SECS` | 300 | 90–3600 (above the 60 s fresh-token margin) |
| `WHATSRUST_MEDIA_PORT` / `_BIND` / `_ALLOW_REMOTE` / `_PUBLIC_URL` | off / `127.0.0.1` / off / derived on loopback | startup rules as the MCP listener; `_PUBLIC_URL` required off loopback |

All in `.env.example`; validated at startup (ADR 0022). The other timeouts (15 s connect, 30 s response, 10 s head,
30 s per write), the 64 KiB/s floor of the total limit, the 16 KiB/s serving floor, the caps (16 connections, 8
streams, 4096 tokens, 1000 queued, 500 paths per intent key) and the 60 s refresh interval are named constants, not
settings.

### 10.3 New dependencies

| Crate | Why | Note |
|---|---|---|
| `ureq` 3.3 (direct) | the media-only agent (§6.2); only its versioned API is used | already in `Cargo.lock` through wa-rs; the same build: `default-features = false, features = ["rustls"]` |

No other new crate: the sanitiser uses the standard library (§5.3), and the quarantine attribute uses
`libc::setxattr` (already a dependency). The release profile stays
`opt-level = "z"`, `panic = "abort"`.

---

## 11. Open items

1. **The old-file test matrix: done 2026-11-05, decision GO** (owner, 2026-11-06); **retest 2026-11-06 done.**
   Results in §2 and ADR 0050 Notes: expired links answer 403; the phone re-uploaded files up to 4.8 years old with
   phone-JID addressing; own group messages work with the own LID as the participant (3 of 3); `@lid` chats work
   (2 of 2 old). **Open, not blocking:** fresh messages of one `@lid` chat, where the phone used another media key
   than the stored one (a re-upload is attempted only after a 403, and a MAC mismatch ends as `integrity`). A mapped
   1:1 chat or group sender was not exercised; the unmapped form, the harder case, works.
2. **A second account:** it has a LID (checked 2026-11-05), and the matrix ran on it. An account without a LID
   still uses the pin's phone-number request (§6.2); no such account is available to test it.
3. **The HTTPS check of the native limits:** the spike proved the idle limit over HTTP only. Phase 4's tests run
   both limits over HTTPS against a local TLS server (§14).
4. **Related, resolved on `main` 2026-11-06 (bd5f785):** live ingest checks `direct_path` with
   `direct_path_is_safe` (`src/bridge.rs`, the rule of §6.2 step 3; phase 4 reuses it) and refuses a
   sender-supplied `static_url`, which wa-rs fetches as-is; the engine never stores one and checks every URL host
   (§6.2 step 4). The daemon log filter keeps the media token out of the log. The own-message attribution row is now
   part of phase 2 (§4, owner 2026-11-06).
5. **Own messages stored in the wrong chat:** live own messages in LID-addressed 1:1 chats stored before the fix
   sit under the account's own number. They cannot be repaired (the other person's ID was not kept). Only `clear-data`
   and a new fetch bring them back under the right chat; a re-pair that keeps history does not (the stored row keeps
   its `message_id`, and a mismatch changes nothing). With media they also count as `duplicate_mismatch`.
6. **Resolved:** the wa-rs version and the uninstall media prompt (2026-10-30); the re-upload addressing for fresh
   messages (2026-11-01, §2); the transfer limits (2026-11-04 spike, §2).

---

## 12. Alternatives considered

| Alternative | Why not (now) |
|---|---|
| A fixed size cap | streaming keeps memory flat; the owner chose no cap; the free-space reserve and the transfer limit cover the real risks |
| Bytes in the API response (base64) | 33 % bloat; large files exceed the 40 s / 45 s call budgets; for small files the owner chose URL only |
| Serving files on the REST listener | a remote agent would need the whole REST API exposed, writes included; long streams would share the REST connection limit |
| Serving files on the MCP listener | breaks ADR 0045 ("POST /mcp only"); long streams could use up MCP connections |
| A local path only | the agent runs on another computer; the path is still returned to loopback REST callers |
| MCP resources | the registry serves tools only; a later option |
| Folders named by chat name | names change and repeat; the name file carries the name instead |
| Folder by bare number | a LID and a phone number can collide |
| A GET that starts the fetch | cross-site GETs pass the request gate |
| Trusting the sender's extension | executable extensions could land on disk under a harmless name |
| One worker for downloads and re-uploads | a 30 s re-upload wait would block every download behind it |
| A small-file lane | one queue is simpler; a large file blocks small ones only while it streams; a later option |
| A folder quota with eviction | complex, and it guesses which files to delete; the fixed reserve is simpler |
| Backfilling document names from `body_text` | the label is not reliably parseable |
| wa-rs `download_to_writer` | at this pin it hides HTTP statuses (`http status: <n>`), seeks the writer to 0 on each host, retries writer errors, and the shared client has no body timeout |
| An async HTTP client with cancellable reads | adds a second HTTP stack beside wa-rs's ureq; ureq's own idle limit (`timeout_recv_body`) already ends a stalled read |
| A custom ureq `Transport` that clamps each read's timeout | works (the 2026-11-04 spike), but uses ureq's `unversioned` API, which may break in minor releases; ureq's own `timeout_recv_body` and `timeout_global` give the same two limits |
| A stall watchdog thread with lingering-thread limits | unnecessary once the read itself has an idle limit |
| The pin's `client.media_reupload()` | it sends the request to the account's phone number; the live test got no answer (0 of 10) on a LID account |
| Tagging at startup by absence | `clear-data`, a fresh re-pair and a restore would mark the whole library, and the documented purge would delete live files |
| A `.deleted` marker in the file name | a sender-chosen document name can contain it; a reserved folder cannot be named by a sender |
| Trusting any revoke row | any group member could hide anyone's media |
| Resolving a revoke once at ingest into a `revoked_at` column | order-dependent (a revoke before its target, a build without the resolution, a non-atomic live path), it needs a one-time pass inside the irreversible step, and each rule correction needs another migration (v5.2 review M5); the label lookup on read costs one indexed query |
| `Range` and `HEAD` | an agent fetches whole files; can be added for browser video playback later |
| A deny-list sanitiser (NFC plus the Cc/Cf categories) | needs two Unicode crates; a std allow-list removes the same dangerous characters by construction |
| Matching duplicates by message ID alone | IDs are sender-chosen; a colliding ID could fill or create another message's reference |
| Judging a revoke by the stored `from_me` | live rows store 0 for own messages, so the owner's own revokes would never count |
| A download token bucket | one download worker already limits the rate; the phone is protected by the re-upload controls |
| HMAC-signed stateless URLs | survive restarts, but add a secret to manage; the in-memory token table is small and a client calls `get` again |
| A bounded wait inside `get` (`wait_ms`) | the 202-and-poll pattern fits the call budgets; a later option |
| Storing live media references now | out of scope by owner decision; the live `from_me` fix it needs comes with phase 2 |
| The attribution fix on its own branch from `main` | it changes the same history function as phase 2, and its row repair would need its own step or take the schema number 9 (owner, 2026-11-06) |

---

## 13. Plan

| Phase | Work | Exit criteria |
|---|---|---|
| 0 | Branch from `main`; the "after a merge" step | ✅ committed; 0 stale doc hashes |
| 1 | **The live re-upload test** with the dev probe (`media-probe`, HOWTO §3) | ✅ 2026-11-01: addressing for fresh messages settled (own LID), recorded in FEATURES F1 and ADR 0050 |
| 1b | **The old-file test matrix** (§11.1; owner, with the probe) | ✅ 2026-11-05: recorded in ADR 0050 and §2; **GO** (2026-11-06); retest of two cells done 2026-11-06 (§11.1) |
| 2 | Detail: `docs/plans/2026-11-08-media-phase2-plan.md` (v5.4). Done first: the own-message attribution fix (18739df) and the LID-chat fix (304e8f4). **2a:** schema v9 (6 columns and the revoke-label index; a re-runnable step and `ensure_v9_columns`; staged migration, tests per ADR 0028–0032); the repair at every start that knows the owner's number (backup before changes, post-condition, set-wise, trigger paused); the message and its reference in one transaction; history ingest writes the new columns, with `view_once` from the extractor; the duplicate rule; the in-place downgrade proven with the v8 binary and the HOWTO revert section. **2b:** `is_revoked` (the read-time judgement; 1:1 by `from_me`), revoke tombstones stored past the allowlist, and the key-shape log. **Merged to `main` after 2b** (phase 1b passed) | migration tests (fresh DB, v8 → v9, v7 → v9, a re-run, the failed-migration path, a post-commit validation failure and `--migrate`, the update-trigger probe); repair tests (paired at migration, unpaired then paired, the marker with the same and another number, a missing `device` column, a dotted LID, the cursor flag 0 and NULL); the budget test (500,000 messages, time and WAL peak); the downgrade test with the v8 binary and a re-upgrade; ingest, `view_once` (each wrapper, the leaf flag, never lowered), duplicate (match, each mismatch, `Filled` vs `Unchanged`) tests; `is_revoked` tests (own, by another person, admin, forged, another chat, a revoke stored before its target, before and after the repair) |
| 3 | Layout: pure functions for folders, file names (bounded IDs), sanitising (byte limits), the MIME allowlist, the name file; the `tag`/`untag` primitive; the macOS quarantine attribute | unit and property tests for every row of §5.2 and §5.4, Cc/Cf/bidi stripping, Hebrew, empty names, 64-byte cuts, long and hostile IDs, executable extensions → `bin`, the §5.5 transition table |
| 4 | The engine: `MediaTransfer` (ureq's own idle and total limits) and `MediaReuploader` (own-LID request, the pin's request for a LID-less account) with fakes; the two lanes; the reserve; the re-upload window, breaker and memories (persisted per key); the states and the per-item backoff; cancel and cancel all; the delete hook (matched by chat) with the chunked intent journal; startup cleanup and the counts; adoption by ID and un-tag | fake-driven tests for every state and reason; the real agent against local HTTP **and HTTPS** servers (both limits); journal replay with concurrent events; the races "deleted while downloading" and "revoked while downloading"; queue full; disconnected and no-LID waits |
| 5 | The media file listener (startup rules, the pipeline of §7.2, headers, streaming with the minimum rate, tokens with the fresh-token margin); REST `get`/`status`/`cancel`; the proxy recipe update | gate, pipeline and header tests; REST golden cases for every row of the §8.1 answer table (labelled); the proxy smoke playbook |
| 6 | CLI and MCP (Write class, read-only mode, the stdio caller marker, fail fast without a listener, the URL note), `/api/status` snapshot, config validation | registry and validation tests; `path` absent over both MCP transports; the CLI keeps `path` |
| 7 | Scripts: `clear-data-with-media`, the `deploy.sh uninstall` media choice; `.gitignore` | shellcheck, dry runs, the deploy harness re-run |
| 8 | Docs (AGENTS.md checklist); the probe is removed or kept as a dev tool (owner); a review pass | `make test`, `make test-all`, `make lint` (no new warnings) |
| 9 | Live check (owner): an image and a document from history, one expired file, through the remote URL | the files open; `status` shows `ready`; a re-upload succeeds |

---

## 14. Test plan

- **Unit and property:** naming (all chat kinds, LID fallback, bounded and hashed IDs, sanitising incl. Cc/Cf/bidi,
  Hebrew, empty names, 64-byte cuts, hostile Unicode with no panics), the MIME allowlist and `bin` fallback, the
  `direct_path` check, the state machine, the re-upload controls, the byte budget and the breaker (paused tokio
  clock, persistence round trip), the timeout arithmetic (clamp, missing length, no overflow), the reserve
  arithmetic, the writer (truncate per attempt, the `Content-Length` cap, sticky abort), tokens (expiry, reuse, the fresh-token
  margin, cap), RFC 6266 encoding, the `tag`/`untag` transitions.
- **Transfer with a real ureq agent against local HTTP and HTTPS servers** (a self-signed certificate trusted
  through ureq's TLS root certificates): 200; 403 then 200 after the refresh, and the refresh rate limit; 404 and
  410 → expired; a 302 → `network`; 500 on one host then 200 on the next (the `.part` file truncated); a body shorter
  than `Content-Length`; **a stalled body → `stalled`/`idle` within the stall period, and a slow but steady body
  that completes; a body longer than the total limit → `stalled`/`transfer_limit`** (both over HTTPS too); more bytes
  than `Content-Length` → `integrity`; a file larger or smaller than `file_length` with a valid MAC → `ready` with a
  `size_note`; a MAC error on one host then a good host; cancel and reserve aborts are not retried on the next host.
- **Re-upload request builder:** `to` = own LID without the device; the LID conversion of the chat and the
  participant from raw and mapped sources; no LID form → `address_unresolved` without a send; a LID-less account →
  the pin's request; an ACK with `error` → `reupload_rejected`; each notification code → its reason; the window
  counts requests and `file_length`; a lone over-size file in an empty window; an exact `retry_after_ms`; the fresh
  path survives a restart.
- **Engine with fakes:** every reason of §6.1; expired → re-upload → success; a second expiry →
  `expired_after_reupload` and a breaker count; `not_on_phone` remembered (24 h and 1 h) across a restart; breaker
  open and reset; stall; cancel and cancel all; deleted while downloading; revoked while downloading.
- **Delete and revoke:** the file set comes from the deleted rows; per-event intents replay after a simulated crash
  and with concurrent deletes; a chat delete whose event JID differs from the stored JID; a forged revoke (another
  member) changes nothing; a revoke before its target is not applied; `clear-data`, then a restart, then re-ingest →
  nothing moved, files adopted; a returning message is un-tagged; a document named `x.deleted.pdf` is an ordinary
  file.
- **Migration and ingest:** v8 → v9 on a copy of a real-shaped DB, fresh v9, the circuit-breaker path; the message
  and its reference in one transaction; a duplicate creates a reference for a `bootstrap` row and not for a `live`
  row; view-once history → `view_once`.
- **Listener:** the pipeline order; the connection and stream caps; the head deadline and size cap; `HEAD` → 405; a
  `Range` header ignored; a slow client closed by the minimum rate; the per-write timeout; encoded separators in the
  file name; 404 hygiene; headers per type; a file moved into `_deleted/` → 404.
- **REST and MCP:** golden cases for every row of the §8.1 answer table, schemas, access classes, read-only listing,
  `path` only for unmarked loopback callers, the stdio marker, MCP `get` without a listener starts nothing (both
  transports), the URL note, the tools with the feature off.
- **Hygiene:** a test runs a failing download and greps the logs, `status` and the MCP result for `auth=` and
  `token=`; none may appear.
- **Delete and revoke (v5, v5.3):** the owner's own revoke from a live row counts (after the repair); a revoke by
  the target's author counts in a 1:1 chat and a group, phone vs LID form through the mapping; an admin's delete and
  a forged revoke change nothing; a revoke stored before its target applies once the target is stored; a message delete with a colliding ID in another chat moves nothing; adoption finds a file
  by its ID part after the LID mapping changed the sender part.
- **No live WhatsApp contact in tests.** The live checks are the owner's (phases 1, the phase 4 gate, and 9).

---

## 15. Docs to update (AGENTS.md checklist)

CHANGELOG; roadmap (P1 row, then Completed); FEATURES F1 (open item → done, issues, the live-test results);
ADR 0050 → Accepted; README (counts: 59 → 62 REST endpoints, 34 → 37 MCP tools, 50 → 53 CLI commands; the media
listener; the feature is off by default); HOWTO §4 (usage, the `rm -r media/*/_deleted` purge, repairing old rows by
re-pair or `clear-data` and a new fetch), a §5a-like section for the media listener (the REST API is never proxied;
use the proxy for any remote use; `PUBLIC_URL` for remote agents), §3 (`clear-data-with-media`; the probe section),
§3b (the uninstall choice), §6 (no supported rollback after a healthy v9 migration; the manual revert); ARCHITECTURE (media lanes,
adapters, listener, schema v9); AGENTS.md (key files, patterns, schema v9); `.env.example`; `deploy/proxy/` and its
README.
