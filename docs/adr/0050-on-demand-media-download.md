# 0050. On-demand media download: chat folders, background fetch, a media file listener

**Status:** Proposed (v3, 2026-11-01: the v2 follow-up cold review and the phase 1 live test folded in; next: owner
approval of v3, then the build)
**Date:** 2026-10-29 (v1), 2026-10-30 (v2), 2026-11-01 (v3)

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
2. **Schema v9** (owner-authorised): `media_refs` gains `file_name`, `file_sha256`, `remote_jid` and
   `participant`, written by history ingest for new rows. A duplicate (a re-fetched message) fills an existing
   row only and never creates one. Staged migration per ADR 0028–0032. No backfill. The schema phase may be
   merged on its own first.
3. **Files on disk, one folder per chat,** under `media/` beside `whatsapp.db`: `chat_<number>/`,
   `group_<number>/`, `lid_<number>/`. One name file per folder (`_group_<name>.txt` / `_chat_<name>.txt`,
   sanitised incl. Unicode control/format characters, at most 64 bytes; the group file lists the participants'
   phone numbers; a group name lookup never blocks a download). File names: `<sender phone>_<epoch>_<id>.<ext>` in
   a group, `<epoch>_<id>.<ext>` elsewhere, a document's sanitised original name as a suffix. **The extension
   comes only from a MIME allowlist** (else `bin`); on macOS files get the quarantine attribute. `hydrated_path`
   is relative to `media/`. An existing final file of the expected size is adopted.
4. **Background download, no size cap, transfer owned by whatsrust.** A download lane (one worker, bounded queue)
   runs the HTTP GET with a media-only ureq agent (statuses as values, a body timeout scaled to the file size)
   from wa-rs's public parts, and streams the decryption into `<file>.part` through a position-based writer with
   a sticky abort (cancel, reserve, over-claim, stall). One table maps each (phase, status) to an action or a
   reason; 404/410 (and 401/403 after a media-connection refresh) mean expired. `direct_path` is checked before
   every request. A **fixed free-space reserve (20 GB, configurable)** is checked before and during the write. A
   separate **re-upload lane** builds the re-upload request itself and sends it to the **account's own LID**, with
   the chat and the participant in LID form (raw key JID, else the LID mapping, else the stored JID); a 30 s wait
   never blocks downloads. Both reach WhatsApp only through two adapters, `MediaTransfer` and `MediaReuploader`.
5. **Bounded contact with the phone.** Re-upload pacing (1 per 30 s), an hourly cap (20), a circuit breaker after
   consecutive timeouts, `not_on_phone` remembered for 24 h (1 h when only the stored phone-number address was
   available), and a kill switch. These persist in the `metadata` table across restarts. Every re-upload result
   and ACK error maps to a reason; a second expiry right after a re-upload is its own reason. Final states
   (`no_reference`, `incomplete_reference`, `revoked`, `integrity`) are never retried. Downloads have no token
   bucket: one worker limits the rate.
6. **Two calls plus cancel.** `POST /api/media/get {id}`: ready → a short-lived URL (and the local path for
   loopback REST callers); not ready → it queues the work and answers HTTP 202 with `ok: false`, `retryable`,
   `retry_after_ms`. `GET /api/media/status?id=` reports the state with byte counts. `POST /api/media/cancel`. `get`
   and `cancel` are POSTs because they can make the daemon contact WhatsApp.
7. **A dedicated media file listener** (opt-in; own port, bind, `ALLOW_REMOTE` and public base URL, required off
   loopback; startup rules like the MCP listener) serves only `GET /media-files/<token>/<file>`. Its purpose is
   resource isolation and a URL a remote agent can reach without the REST API. Its pipeline: a connection cap, a
   head deadline and size cap, one route (no `HEAD`, no `Range`), a token lookup, an exact name check, a stream
   cap, a minimum transfer rate. Tokens: 256 random bits, one live token per file, 5 minutes, at most 4096, never
   logged. Every response is hardened: `attachment` unless the type is a safe image/audio/video type, a validated
   MIME type or `application/octet-stream`, `nosniff`, a sandboxing CSP, `no-referrer`, RFC 6266 file names. TLS
   comes from the reverse proxy, whose recipe gains the route without buffering or URI logs. The REST listener and
   its pipeline are unchanged.
8. **State without a state column.** `ready` = `hydrated_path` is set, untagged, and the file exists; `revoked` =
   a revoke row exists; other states live in memory and reset on restart.
9. **Deleted and revoked content keeps its files.** A delete still removes the DB rows, but each downloaded file
   is marked `.deleted` before its extension (a chat whose rows are all deleted gets a `deleted: <epoch>` line in
   its name file). **Only an observed delete or revoke tags a file**, and the file set comes from the rows actually
   deleted, through an intent journal in `metadata` that startup replays. A message missing from the DB is never
   treated as deleted. A revoked message is never downloaded. A message that comes back is un-tagged and adopted.
   **This narrows ADR 0012 for media:** a delete no longer removes the bytes.
10. **Out of scope:** live media (no reference: `no_reference`), bulk downloads, contact names before F7, MCP
    resources, HTTP `Range`/`HEAD` on the file listener.
11. **Surfaces.** CLI `media-get`, `media-status`, `media-cancel`; MCP `whatsrust_media_get` and
    `whatsrust_media_cancel` (the existing Write class, non-destructive annotations) and `whatsrust_media_status`
    (Read); MCP results never include the local path, and the tool descriptions mark file names, MIME types and
    contents as third-party content. `/api/status` gains a `media` snapshot with disk numbers.
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
- Files of deleted and revoked messages stay on disk until `clear-data-with-media` or a manual `find … -delete`.
- Until F7, name files show phone numbers only; live media cannot be downloaded on demand.

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
HMAC-signed stateless URLs, a bounded wait inside `get`, storing live references now.
