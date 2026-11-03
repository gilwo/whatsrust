# 0050. On-demand media download: chat folders, background fetch, a media file listener

**Status:** Proposed (v4, 2026-11-03: the v3 follow-up cold review folded in; next: owner approval of v4, then the
build)
**Date:** 2026-10-29 (v1), 2026-10-30 (v2), 2026-11-01 (v3), 2026-11-03 (v4)

## Context

Messages stored from history (the pairing package and on-demand fetches) keep no media bytes, only a
`media_refs` row (ADR 0005 and its amendments, ADR 0048). Nothing can download a stored message's media later.
The owner's agent runs **on another computer** than whatsrust, connected through the MCP HTTP listener.

Facts that shape the design (checked in the code, the wa-rs pin `9e8c70e` and a live test; detail in
`docs/plans/2026-10-29-on-demand-media-download-design.md` §2):

- wa-rs exposes the parts of a download (media connection, per-host URLs, streaming decryption with the MAC check
  at the end, about 40 KB of memory). Its own `download_to_writer` hides HTTP statuses at this pin: the shared ureq
  client turns every 4xx/5xx into `http status: <n>`. It also seeks the writer to 0 on each host and retries writer
  errors. The HTTP client has no body timeout.
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
   in one transaction. A duplicate (a re-fetched message) fills an existing row, and creates a missing one when the
   stored message is not live. Staged migration per ADR 0028–0032. No backfill. The schema phase may be merged on
   its own first.
3. **Files on disk, one folder per chat,** under `media/` beside `whatsapp.db`: `chat_<number>/`,
   `group_<number>/`, `lid_<number>/`. One name file per folder (`_group_<name>.txt` / `_chat_<name>.txt`,
   sanitised incl. Unicode control/format characters, at most 64 bytes; the group file lists the participants'
   phone numbers; a group name lookup never blocks a download). File names: `<sender phone>_<epoch>_<id>.<ext>` in
   a group, `<epoch>_<id>.<ext>` elsewhere (an ID that is too long or has other characters becomes a hash), a
   document's sanitised original name as a suffix. **The extension comes only from a MIME allowlist** (else `bin`);
   on macOS files get the quarantine attribute. `hydrated_path` is relative to `media/`. An existing final file of
   the expected size is adopted. Each chat folder may hold one reserved subfolder, `_deleted/` (decision 9).
4. **Background download, no size cap, transfer owned by whatsrust.** A download lane (one worker, bounded queue)
   runs the HTTP GET with a media-only ureq agent (statuses as values, no redirects, a body timeout scaled to the
   file size and clamped to a transfer limit, default 2 h) from wa-rs's public parts, and streams the decryption
   into `<file>.part` through a position-based writer with a sticky abort (cancel, reserve, over-claim, stall).
   One table maps each (phase, status) to an action or a reason; 404/410 (and 401/403 after a rate-limited
   media-connection refresh) mean expired; a decryption error is retried once on another host. `direct_path` is
   checked before every request. There is no resume. A **fixed free-space reserve (20 GB, configurable)** is checked before and during the write. A
   separate **re-upload lane** builds the re-upload request itself and sends it to the **account's own LID**, with
   the chat and the participant in LID form (the raw key JID, else the LID mapping). Whether the stored phone JID
   may be sent when no LID form exists is decided by the old-file test matrix, which gates the engine; until then
   such an item fails as `address_unresolved` without contacting the phone. A 30 s wait never blocks downloads. Both reach WhatsApp only through two adapters, `MediaTransfer` and `MediaReuploader`.
5. **Bounded contact with the phone.** Re-upload pacing (1 per 30 s), an hourly request cap (20), an hourly byte
   budget (1 GB of requested files), a circuit breaker after consecutive timeouts or expiries after a re-upload,
   `not_on_phone` remembered for 24 h (1 h with a stored-form address), and a kill switch that is **off by
   default**. These persist in the `metadata` table across restarts, one small key per entry. Every re-upload
   result and ACK error maps to a reason; a second expiry right after a re-upload is its own reason. Final states
   (`no_reference`, `incomplete_reference`, `revoked`, `view_once`, `integrity`) are never retried. Downloads have
   no token bucket: one worker limits the rate.
6. **Two calls plus cancel.** `POST /api/media/get {id}`: ready → a short-lived URL (and the local path for
   loopback REST callers); not ready → it queues the work and answers HTTP 202 with `ok: false`, `retryable`,
   `retry_after_ms`; every retryable failure is also a 202, every final one a 4xx, so an MCP agent polls instead of
   giving up. `GET /api/media/status?id=` reports the state with byte counts. `POST /api/media/cancel` (one item, or
   all). `get` and `cancel` are POSTs because they can make the daemon contact WhatsApp. Without the media file
   listener, an MCP `get` answers `url_unavailable` and starts nothing.
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
   name file). **Only an observed delete or a valid revoke tags a file.** A revoke is valid only when the revoker
   is the target's sender (an admin's delete does not count); it is resolved once at ingest into `revoked_at`. The
   file set comes from the rows actually deleted, through a per-event intent journal in `metadata` that startup
   replays. A message missing from the DB is never treated as deleted. A revoked message is never downloaded. A
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
  reserve, not by a quota. Every transfer thread has a bounded life.
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

## Alternatives considered

See the design doc §12, among others: a size cap, bytes in the API response, serving on the REST or MCP listener,
a path only, MCP resources, folders by name or bare number, a GET that starts the fetch, trusting the sender's
extension, one lane for downloads and re-uploads, a folder quota with eviction, wa-rs `download_to_writer`, the
pin's `client.media_reupload()`, tagging at startup by absence, `Range`/`HEAD`, a download token bucket,
HMAC-signed stateless URLs, a bounded wait inside `get`, storing live references now, an async HTTP client, a
`.deleted` marker in the file name, trusting any revoke row, a small-file lane.
