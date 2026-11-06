# 0050. On-demand media download: chat folders, background fetch, a media file listener

**Status:** Proposed (v5, 2026-11-04: the v4 follow-up cold review and a ureq timeout spike folded in; the old-file
test matrix passed 2026-11-05, GO 2026-11-06; v5.1 2026-11-06: the own-message attribution fix folded into phase 2;
next: phase 2)
**Date:** 2026-10-29 (v1), 2026-10-30 (v2), 2026-11-01 (v3), 2026-11-03 (v4), 2026-11-04 (v5), 2026-11-06 (v5.1)

## Context

Messages stored from history (the pairing package and on-demand fetches) keep no media bytes, only a
`media_refs` row (ADR 0005 and its amendments, ADR 0048). Nothing can download a stored message's media later.
The owner's agent runs **on another computer** than whatsrust, connected through the MCP HTTP listener.

Facts that shape the design (checked in the code, the wa-rs pin `9e8c70e` and a live test; detail in
`docs/plans/2026-10-29-on-demand-media-download-design.md` §2):

- wa-rs exposes the parts of a download (media connection, per-host URLs, streaming decryption with the MAC check
  at the end, about 40 KB of memory). Its own `download_to_writer` hides HTTP statuses at this pin: the shared ureq
  client turns every 4xx/5xx into `http status: <n>`. It also seeks the writer to 0 on each host and retries writer
  errors. The shared HTTP client has no body timeout. ureq's own `timeout_recv_body` restarts on every read (an
  idle limit), and `timeout_global` runs from the start of the call (a total limit).
- An expired CDN link can be recovered by a re-upload request to **the owner's own phone**; the phone must be
  online and still hold the file. Most history media is probably expired, so this is a main path. **The pin's
  `client.media_reupload()` sends the request to the account's phone number and got no answer on the owner's
  LID-migrated account; the same request sent to the account's own LID worked (live test 2026-11-01, Notes).**
- History rows keep neither the document's original file name nor the plaintext hash nor the raw chat/sender
  JIDs the phone used; live media gets no reference at all.
- A tool call has 40 s (MCP) and a CLI call 45 s; the REST response type buffers whole bodies.
- Media files are sender-controlled: their bytes, MIME type, file name and CDN path.
- The REST gate (ADR 0044) blocks cross-site POSTs, not cross-site GETs.

## Decision

1. **The media ID is the message ID.**
2. **Schema v9** (owner-authorised): `media_refs` gains `file_name`, `file_sha256`, `remote_jid`, `participant`,
   `revoked_at` and `view_once`, written by history ingest for new rows. The message and its reference are written
   in one transaction. A duplicate (a re-fetched message) whose chat, sender, `from_me` and kind match the stored
   row fills an existing reference, and creates a missing one when the stored message is not live; a mismatch
   changes nothing. Staged migration per ADR 0028–0032. No backfill of the new columns. Phase 2 first fixes the
   own-message attribution (live rows store the event's `from_me`; own history messages store the own phone JID as
   the sender), and the migration repairs the stored rows before the one-time revoke pass (owner-authorised
   2026-11-06). **There is no supported way back after a healthy
   migration** (`--rollback` serves only failed ones), so the schema phase is merged only after the old-file test
   matrix passes.
3. **Files on disk, one folder per chat,** under `media/` beside `whatsapp.db`: `chat_<number>/`,
   `group_<number>/`, `lid_<number>/`. One name file per folder (`_group_<name>.txt` / `_chat_<name>.txt`,
   sanitised by a std allow-list of letters, digits, `-` and `.`, at most 64 bytes; the group file lists the participants'
   phone numbers; a group name lookup never blocks a download). File names: `<sender phone>_<epoch>_<id>.<ext>` in
   a group, `<epoch>_<id>.<ext>` elsewhere (an ID that is too long or has other characters becomes a hash), a
   document's sanitised original name as a suffix. **The extension comes only from a MIME allowlist** (else `bin`);
   on macOS files get the quarantine attribute. `hydrated_path` is relative to `media/`. An existing final file is
   found by the ID part of its name and adopted when its hash or size matches. Each chat folder may hold one reserved subfolder, `_deleted/` (decision 9).
4. **Background download, no size cap, transfer owned by whatsrust.** A download lane (one worker, bounded queue)
   runs the HTTP GET with a media-only ureq agent from wa-rs's public parts: statuses as values, no redirects, and
   ureq's own limits only (no custom transport): `timeout_recv_body` as the idle limit (60 s) and `timeout_global`
   as the total limit (scaled to the file size, clamped to 2 h by default). It streams the decryption into
   `<file>.part` through a position-based writer with a sticky abort (cancel, reserve). The response's
   `Content-Length` is the hard size cap; the sender's `file_length` is only a hint.
   One table maps each (phase, status) to an action or a reason; 404/410 (and 401/403 after a rate-limited
   media-connection refresh) mean expired; a decryption error is retried once on another host. `direct_path` is
   checked before every request. There is no resume. A **fixed free-space reserve (20 GB, configurable)** is checked before and during the write. A
   separate **re-upload lane** builds the re-upload request itself and sends it to the **account's own LID**, with
   the chat and the participant in LID form (the raw key JID, else the LID mapping). Whether the stored phone JID
   may be sent when no LID form exists is decided by the old-file test matrix, which gates the engine; until then
   such an item fails as `address_unresolved` without contacting the phone. An account without a LID falls back to
   the pin's own request. A 30 s wait never blocks downloads. Both reach WhatsApp only through two adapters, `MediaTransfer` and `MediaReuploader`.
5. **Bounded contact with the phone.** Re-upload pacing (1 per 30 s), one hourly window of 20 requests and 1 GB of
   requested files (a lone larger file is admitted into an empty window; the bytes are an honest-sender guard), a
   fresh re-upload path kept for 1 h across restarts, a per-item backoff, a circuit breaker after consecutive
   timeouts or expiries after a re-upload,
   `not_on_phone` remembered for 24 h (1 h with a stored-form address), and a kill switch that is **off by
   default**. These persist in the `metadata` table across restarts, one small key per entry. Every re-upload
   result and ACK error maps to a reason; a second expiry right after a re-upload is its own reason. Final states
   (`no_reference`, `incomplete_reference`, `revoked`, `view_once`, `integrity`) are never retried. Downloads have
   no token bucket: one worker limits the rate.
6. **Two calls plus cancel.** `POST /api/media/get {id}`: ready → a short-lived URL (and the local path for local
   callers only: direct callers of the loopback REST listener such as the CLI; the MCP HTTP listener and the stdio
   MCP server, which marks its REST requests, are remote); not ready → it queues the work and answers HTTP 202 with `ok: false`, `retryable`,
   `retry_after_ms`; every retryable failure is also a 202, every final one a 4xx, so an MCP agent polls instead of
   giving up. `GET /api/media/status?id=` reports the state with byte counts. `POST /api/media/cancel` (one item, or
   all). `get` and `cancel` are POSTs because they can make the daemon contact WhatsApp. Without the media file
   listener, a remote `get` answers `url_unavailable` and starts nothing.
7. **A dedicated media file listener** (opt-in; own port, bind, `ALLOW_REMOTE` and public base URL, required off
   loopback; startup rules like the MCP listener) serves only `GET /media-files/<token>/<file>`. Its purpose is
   resource isolation and a URL a remote agent can reach without the REST API. Its pipeline: a connection cap, a
   head deadline and size cap, one route (no `HEAD`, no `Range`), a token lookup, an exact name check, a stream
   cap, a minimum transfer rate. Tokens: 256 random bits, one live token per file (a fresh one when less than 60 s
   remain), 5 minutes, at most 4096, never logged. Every response is hardened: `attachment` unless the type is a safe image/audio/video type, a validated
   MIME type or `application/octet-stream`, `nosniff`, a sandboxing CSP, `no-referrer`, RFC 6266 file names. TLS
   comes from the reverse proxy, whose recipe gains the route without buffering or URI logs and with per-client
   connection and rate limits. The REST listener and
   its pipeline are unchanged.
8. **State without a state column.** `ready` = `hydrated_path` is set, outside `_deleted/`, and the file exists;
   `revoked` and `view_once` come from their columns; other states live in memory and reset on restart.
9. **Deleted and revoked content keeps its files.** A delete still removes the DB rows, but each downloaded file
   is moved into its chat's reserved `_deleted/` folder by one idempotent `tag` primitive; "deleted" is a path
   property, never a part of a file name (a chat whose rows are all deleted gets a `deleted: <epoch>` line in its
   name file). **Only an observed delete or a valid revoke tags a file.** A revoke is judged by identities, not by
   the stored `from_me`: the revoker must be the target's author (the owner's own account for an own message), and
   the revoke key's `from_me`/`participant` must match the target (an admin's delete does not count). It is
   resolved once at ingest into `revoked_at`, and the migration runs a one-time pass over stored revoke rows. The
   file set comes from the rows actually deleted (a message delete also matches the chat), through per-event,
   chunked intent keys in `metadata` that startup replays. A message missing from the DB is never treated as deleted. A revoked message is never downloaded. A
   message that comes back is moved back and adopted. **This narrows ADR 0012 for media:** a delete no longer
   removes the bytes.
10. **Out of scope:** live media (no reference: `no_reference`), view-once media (refused: `view_once`), bulk
    downloads, contact names before F7, MCP resources, HTTP `Range`/`HEAD` on the file listener, file bytes inside
    MCP results.
11. **Surfaces.** CLI `media-get`, `media-status`, `media-cancel`; MCP `whatsrust_media_get` and
    `whatsrust_media_cancel` (the existing Write class, non-destructive annotations) and `whatsrust_media_status`
    (Read); MCP results carry the URL, never the local path or file bytes, and the tool descriptions mark file
    names, MIME types and contents as third-party content. The tools are always listed; with the feature off they
    answer `disabled`. `/api/status` gains a `media` snapshot with disk numbers.
    `run-profile.sh clear-data-with-media` is new (`clear-data` keeps `media/`); the `deploy.sh uninstall` backup
    asks whether to include `media/` (default: include).

## Consequences

- Any stored history media can be fetched later, as long as the CDN still has it or the owner's phone can
  re-upload it; a remote agent gets it through one short-lived URL.
- Memory stays flat for any file size; disk use grows with what clients request, bounded by the free-space
  reserve, not by a quota. Every transfer thread ends within the idle limit of its last byte or at the total limit.
- The v8 → v9 migration is one-way: after a healthy migration the only revert is restoring the pre-migration backup
  by hand, which loses newer data and the Signal session state (HOWTO documents it).
- whatsrust owns two pieces of protocol work at this pin: the media HTTP transfer and the re-upload request. Both
  sit behind one adapter each. The wa-rs upgrade (roadmap Q-WARS) can hand them back to the library: upstream
  `main` has typed download errors and sends the re-upload to the own LID. A 2026-10-30 compile spike measured 165
  errors for v0.7.0 and 298 for upstream `main`.
- One more opt-in listener to configure and to put behind the proxy.
- Files of deleted and revoked messages stay on disk, in `_deleted/` folders, until `clear-data-with-media` or a
  manual `rm -r media/*/_deleted`. An admin's delete in a group leaves the media available.
- Old history rows without a reference are repaired only by a re-pair that keeps history, or by `clear-data` and a
  new fetch (backfill does not revisit stored rows).
- Until F7, name files show phone numbers only; live media cannot be downloaded on demand.
- Large files on slow links need a higher transfer limit (2 h covers 2 GB at about 290 KB/s).

## Notes

**2026-11-01: phase 1 live test (re-upload addressing).** At the wa-rs pin, `client.media_reupload()` sends
the request (a `server-error` receipt) `to` the account's **phone number**. On this LID-migrated account the
phone never answered: 0 of 10 requests, across the stored and the phone-number↔LID alternate addresses. The
same receipt, built from wacore's public `media_retry` functions and sent `to` the account's own **LID**
(`client.get_lid()` without the device part), worked 3 of 3: a 1:1 photo, a group photo from a member, and the
account's own group photo. The chat and the participant were in LID form, as WhatsApp Web and upstream wa-rs
`main` send them. In groups only `to` differed between the failing and the working request.
**Consequence for decision 4:** at this pin the re-upload lane builds and sends the request itself, behind the
one wa-rs adapter. The wa-rs upgrade (roadmap Q-WARS) can replace it with the library call. Old, expired history
files are not tested yet (design §11). Folded into v3 (the probe: HOWTO §3 "Media re-upload probe").

**2026-11-05: the old-file test matrix (gate for phase 2).** Run with the dev probe on the owner's second account,
which has a LID. All requests below are the `web` form (to the own LID); the pin's request to the phone number timed
out 6 of 6 again.

| Cell | Age | Stored link | Re-upload | New link |
|---|---|---|---|---|
| Control: 1:1, fresh, chat as phone JID | 4 d | 200 | success | 200, same SHA-256 |
| 1:1, chat as phone JID (no mapping) | 1764 d | 403 | success | 200 |
| 1:1, chat as phone JID (no mapping) | 1761 d | 403 | success | 200 |
| Group video, participant as phone JID (no mapping) | 1466 d | 403 | success | 200 |
| Group, own message, participant = the group JID (history stores it as the sender) | 1219 d | 403 | no ACK, timeout | – |
| `@lid` chat, fresh | 2 d | 200 | the answer did not decrypt with the media key | – |

Findings: expired links answer 403; the phone still holds files 4.8 years old; the stored phone-JID form works when
`to` is the own LID, so `address_unresolved` is needed only when no form exists. **Decision: GO for phase 2** (owner,
2026-11-06). Follow-ups for a short retest (design §11.1): own group messages with the own LID as the participant,
and 2–3 more `@lid` rows.

**2026-11-06: the retest** (same account, `media-probe --web`, only the `web` request):

| Cell | Age | Stored link | Re-upload | New link |
|---|---|---|---|---|
| Group, own message, participant = own LID (the cell that failed) | 1220 d | 403 | success | 200 |
| Group, own message (PDF), participant = own LID | 1606 d | 403 | success | 200 |
| Group, own message (PDF), participant = own LID | 781 d | 403 | success | 200 |
| `@lid` chat A, fresh (the cell that failed) | 2 d | 200 | the answer (from the own account, naming chat A) did not decrypt | – |
| `@lid` chat A, fresh | 2 d | 200 | success | MAC mismatch |
| `@lid` chat B | 80 d | 403 | success | 200 |
| `@lid` chat B, own message | 80 d | 403 | success | 200 |

Findings: the own-LID participant for own group messages works (3 of 3). `@lid` chats work (2 of 2 old). The two
failures are fresh messages of one chat, where the phone handled the media with another key than the one stored
from history; their links are still valid, and the engine re-uploads only after a 403, so it does not block the
build (an open item; a MAC mismatch after a re-upload ends as `integrity`). Across both runs, 13 of 15 `web` requests
succeeded, for files up to 4.8 years old.

**2026-11-06: the own-message attribution fix moves into phase 2** (owner). History ingest uses the key's
`remote_jid` as the sender of every message without a participant, so own history messages name the group (in
groups) or the other person (in 1:1 chats) as the sender, and live rows store `from_me = 0` for own messages. The
duplicate rule and the revoke rule (decision 9) compare these values; an own revoke stored from history would
never count. Phase 2 therefore starts with the fix, and the v9 migration repairs the stored rows (live rows whose
sender is the owner's number or LID get `from_me = 1`; history rows with `from_me = 1` whose sender is the chat
JID get the own phone JID),
before the one-time revoke pass. The owner authorised this repair; it is undone only by the manual revert. The
"live media security" fix stays on its own branch from `main` (design §11).

## Alternatives considered

See the design doc §12, among others: a size cap, bytes in the API response, serving on the REST or MCP listener,
a path only, MCP resources, folders by name or bare number, a GET that starts the fetch, trusting the sender's
extension, one lane for downloads and re-uploads, a folder quota with eviction, wa-rs `download_to_writer`, the
pin's `client.media_reupload()`, tagging at startup by absence, `Range`/`HEAD`, a download token bucket,
HMAC-signed stateless URLs, a bounded wait inside `get`, storing live references now, an async HTTP client, a
`.deleted` marker in the file name, trusting any revoke row, a small-file lane, a custom ureq transport (it works,
but uses ureq's `unversioned` API), a deny-list sanitiser with two Unicode crates, matching duplicates by message
ID alone, judging a revoke by the stored `from_me`.
