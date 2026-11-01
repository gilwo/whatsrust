# 0005. Store media refs always, hydrate bytes lazily

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Historical message fetch includes media-bearing messages (images, videos, audio, documents). Each has:
- Metadata: `mediaKey`, `directPath`, `fileSha256`, `mime`, `size`, dimensions (images/videos)
- Bytes: downloaded from WhatsApp CDN via directPath + decrypted via mediaKey

Challenge: storing gigabytes of media inline bloats the database. Fetching media synchronously during backfill slows ingestion and risks rate-limiting.

WhatsApp's `directPath` URLs expire after an unknown TTL (hours to days). Once expired, media bytes are unrecoverable unless WhatsApp provides a re-fetch API (unclear).

## Decision

**Always** store media refs (mediaKey, directPath, mime, size, dimensions) in `media_refs` table.

**Lazily** hydrate media bytes on explicit request (API endpoint / MCP tool). Decryption: `aes-256-cbc` with mediaKey-derived key + IV.

Old directPaths that expire → **best-effort**. If fetch fails, return error to caller. No proactive re-validation or expiry tracking (unpredictable TTL).

## Consequences

**Positive:**
- Database stays compact (refs = ~200 bytes vs full media = KB-MB per message)
- Fast backfill ingestion (no CDN download bottleneck)
- On-demand bandwidth usage (only fetch media user views)

**Negative:**
- Media bytes not guaranteed available after directPath expiry (user sees "media unavailable" on old messages)
- No proactive expiry detection (can't pre-warn user)
- Requires separate fetch API endpoint / error handling for expired media

**Future options:**
- If wa-rs exposes a media re-request API (via mediaKey alone), add fallback path
- Store decrypted bytes in separate on-disk blob store if full archival needed (out of scope v1)

## Amendment (2026-10-22)

The decision is now carried out for history rows. Messages stored from history (the package the phone sends at pairing, and on-demand fetches) download no media: each media message is stored with a label built from the proto's `file_length` (for example `[image 12.1KB] caption`) and a `media_refs` row (`media_key`, `direct_path`, `file_enc_sha256`, `mimetype`, `file_length`, `width`, `height`; `hydrated_path` stays NULL). Media whose link has expired is therefore stored too. Live ingest is unchanged: it still downloads eagerly, because the bytes feed the inbound event, and writes no `media_refs` row. The on-demand download from `media_refs` (REST, CLI and MCP) is not built yet; it is on the ROADMAP backlog. Deleting a message or a chat deletes its `media_refs` rows. See ADR 0048.

## Amendment (2026-10-29, proposed; revised 2026-10-30 and 2026-11-01)

The on-demand download is designed in ADR 0050 (status Proposed; detail in
`docs/plans/2026-10-29-on-demand-media-download-design.md`). Three points of this ADR change with it:

- **"Future options" is now concrete.** An expired file can be re-requested: a re-upload request asks the
  owner's own phone to upload it again and returns a new `direct_path`. The phone must be online and still hold
  the file. At the current wa-rs pin the library call (`client.media_reupload()`) sends the request to the
  account's phone number and got no answer; whatsrust sends it to the account's own LID, which works (live test
  2026-11-01, ADR 0050 Notes).
- **Bytes are stored on disk after all**, on request only: one file per message under
  `media/<prefix>_<number>/` beside `whatsapp.db`, recorded in `media_refs.hydrated_path` (relative to
  `media/`). The DB still holds no media bytes, and nothing is downloaded unless a client asks for that message.
- **The reference gains fields** (schema v9): `file_name`, `file_sha256`, `remote_jid`, `participant`, for new
  history rows (`static_url` was dropped on 2026-11-01: newsletters are never stored). Live media still gets no
  reference (out of scope for now).
