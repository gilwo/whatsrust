# 0050. On-demand media download: chat folders, background fetch, a media file listener

**Status:** Proposed (v2, 2026-10-30; owner-approved; next: a follow-up cold design review)
**Date:** 2026-10-29 (v1), 2026-10-30 (v2)

## Context

Messages stored from history (the pairing package and on-demand fetches) keep no media bytes, only a
`media_refs` row (ADR 0005 and its amendments, ADR 0048). Nothing can download a stored message's media later.
The owner's agent runs **on another computer** than whatsrust, connected through the MCP HTTP listener.

Facts that shape the design (checked in the code and the wa-rs pin `9e8c70e`; detail in
`docs/plans/2026-10-29-on-demand-media-download-design.md` §2):

- wa-rs streams download, decryption and the MAC check into any writer with about 40 KB of memory; the MAC is
  checked only at the end; the streaming path has no byte cap; the HTTP client has no timeouts by default.
- An expired CDN link can be recovered: `client.media_reupload()` asks **the owner's own phone** to upload the
  file again (30 s fixed timeout); the phone must be online and still hold it. Most history media is probably
  expired, so this is a main path, not an edge case.
- History rows keep neither the document's original file name nor the plaintext hash nor the raw chat/sender
  JIDs the phone used; live media gets no reference at all.
- A tool call has 40 s (MCP) and a CLI call 45 s; the REST response type buffers whole bodies.
- Media files are sender-controlled: their bytes, MIME type and file name.
- The REST gate (ADR 0044) blocks cross-site POSTs, not cross-site GETs.

## Decision

1. **The media ID is the message ID.**
2. **Schema v9** (owner-authorised): `media_refs` gains `file_name`, `file_sha256`, `static_url`, `remote_jid` and
   `participant`, written by history ingest for new rows and filled in for re-fetched duplicates. Staged
   migration per ADR 0028–0032. No backfill.
3. **Files on disk, one folder per chat,** under `media/` beside `whatsapp.db`: `chat_<number>/`,
   `group_<number>/`, `lid_<number>/`, `newsletter_<id>/`. One name file per folder (`_group_<name>.txt` /
   `_chat_<name>.txt`, sanitised incl. Unicode control/format characters; the group file lists the participants'
   phone numbers). File names: `<sender phone>_<epoch>_<id>.<ext>` in a group, `<epoch>_<id>.<ext>` elsewhere, a
   document's sanitised original name as a suffix. **The extension comes only from a MIME allowlist** (else
   `bin`); on macOS files get the quarantine attribute. `hydrated_path` is relative to `media/`.
4. **Background download, no size cap.** A download lane (one worker, bounded queue) streams into `<file>.part`
   through a counting writer that aborts past the claimed size, with a stall watchdog and cancel, then verifies
   and renames. A **fixed free-space reserve (20 GB, configurable)** is checked before and during the write.
   A separate **re-upload lane** handles expired links, so a 30 s wait never blocks downloads.
5. **Bounded contact with the phone.** Download pacing (a token bucket), re-upload pacing (1 per 30 s), an hourly
   re-upload cap (20), a circuit breaker after consecutive timeouts, `not_on_phone` remembered for 24 h, and a kill
   switch. Final states (`no_reference`, `incomplete_reference`, `not_on_phone`, `integrity`) are never retried
   automatically.
6. **Two calls plus cancel.** `POST /api/media/get {id}`: ready → a short-lived URL (and the local path); not ready
   → it queues the work and answers HTTP 202 with `ok: false`, `retryable`, `retry_after_ms`.
   `GET /api/media/status?id=` reports the state with byte counts. `POST /api/media/cancel`. `get` and `cancel`
   are POSTs because they can make the daemon contact WhatsApp.
7. **A dedicated media file listener** (opt-in; own port, bind, `ALLOW_REMOTE` and public base URL; startup rules
   like the MCP listener) serves only `GET`/`HEAD /media-files/<token>/<file>`, so files never share an origin with
   the REST or MCP APIs. Tokens: 256 random bits, one live token per file, 5 minutes, at most 256, never logged.
   Every response is hardened: `attachment` unless the type is a safe image/audio/video type, a validated MIME
   type or `application/octet-stream`, `nosniff`, a sandboxing CSP, `no-referrer`, RFC 6266 file names; chunked
   streaming with a write timeout, a stream cap and single-range support. TLS comes from the reverse proxy, whose
   recipe gains the route. The REST listener and its pipeline are unchanged.
8. **State without a state column.** `ready` = `hydrated_path` is set and the file exists; other states live in
   memory and reset on restart.
9. **Deleted content keeps its files.** A delete still removes the DB rows, but each downloaded file is marked
   `.deleted` before its extension (a deleted chat's name file gets a `deleted: <epoch>` line), in a defined order
   with a startup reconciliation. **This narrows ADR 0012 for media:** a delete no longer removes the bytes.
10. **Out of scope:** live media (no reference: `no_reference`), bulk downloads, contact names before F7, MCP
    resources.
11. **Surfaces.** CLI `media-get`, `media-status`, `media-cancel`; MCP `whatsrust_media_get` and
    `whatsrust_media_cancel` (a new access class: writes local files, contacts WhatsApp, not destructive) and
    `whatsrust_media_status` (Read). `/api/status` gains a `media` snapshot. `run-profile.sh clear-data-with-media`
    is new (`clear-data` keeps `media/`); the `deploy.sh uninstall` backup asks whether to include `media/` (default: include).

## Consequences

- Any stored history media can be fetched later, as long as the CDN still has it or the owner's phone can
  re-upload it; a remote agent gets it through one short-lived URL.
- Memory stays flat for any file size; disk use grows with what clients request, bounded by the free-space
  reserve, not by a quota.
- One more opt-in listener to configure and to put behind the proxy.
- Files of deleted messages stay on disk until `clear-data-with-media` or a manual `find … -delete`.
- Until F7, name files show phone numbers only; live media cannot be downloaded on demand.
- Built on the current wa-rs pin (owner decision 2026-10-30). A later upgrade changes the download error types
  and the re-upload addressing; both sit behind one adapter. The upgrade itself is a separate project (roadmap
  Q-WARS): a 2026-10-30 compile spike measured 165 errors for v0.7.0 and 298 for upstream `main`.

## Alternatives considered

See the design doc §12: a size cap, bytes in the API response, serving on the REST or MCP listener, a path only,
MCP resources, folders by name or bare number, a GET that starts the fetch, trusting the sender's extension, one
lane for downloads and re-uploads, a folder quota with eviction, backfilling document names from `body_text`.
