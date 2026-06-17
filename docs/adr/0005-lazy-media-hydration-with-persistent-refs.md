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
