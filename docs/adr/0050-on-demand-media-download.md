# 0050. On-demand media download: chat folders, background fetch, a media file listener

**Status:** Proposed (v5, 2026-11-04: the v4 follow-up cold review and a ureq timeout spike folded in; the old-file
test matrix passed 2026-11-05, GO 2026-11-06; v5.1 2026-11-06: the own-message attribution fix folded into phase 2;
v5.2 2026-11-08: the revoke key rule corrected; v5.3 2026-11-08: the v5.2 cold review folded in (revokes judged
when read, a deferred repair, an in-place downgrade); v5.4 2026-11-08: the v5.3 cold review folded in; next: phase
2a commit, then 2b; phase 2 merges to `main` after 2b)
**Date:** 2026-10-29 (v1), 2026-10-30 (v2), 2026-11-01 (v3), 2026-11-03 (v4), 2026-11-04 (v5), 2026-11-06 (v5.1), 2026-11-08 (v5.2, v5.3, v5.4)

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
2. **Schema v9** (owner-authorised): `media_refs` gains `file_name`, `file_sha256`, `remote_jid`, `participant`
   `view_once` and `media_key_timestamp` (`revoked_at` dropped 2026-11-08), written by history ingest for new rows;
   `messages` gains a partial index on the revoke label. The step is re-runnable (`ensure_v9_columns` also runs on the
   re-validation branch). A duplicate fills the encryption tuple (`media_key`, `direct_path`, `file_enc_sha256`,
   `media_key_timestamp`) only as a whole. The message and its reference are written
   in one transaction. A duplicate (a re-fetched message) whose chat, sender, `from_me` and kind match the stored
   row fills an existing reference, and creates a missing one when the stored message is not live; a mismatch
   changes nothing. Staged migration per ADR 0028–0032. No backfill of the new columns. Phase 2 first fixes the
   own-message attribution (live rows store the event's `from_me`; own history messages store the own phone JID as
   the sender). The stored rows and the fetch cursor's anchor flag are repaired at every start that knows the
   owner's number (idempotent; a backup before it changes rows outside the migrating start; a post-condition; the
   marker records the number as a re-pair guard; ADR 0028 amendment) (owner-authorised 2026-11-06; timing
   2026-11-08). **The way back after a healthy migration is an in-place downgrade** (`PRAGMA user_version = 8` on
   a stopped daemon; proven by a test with the v8 binary); restoring the pre-migration backup is the fallback, and
   `--rollback` serves failed migrations only. The schema phase is merged only after the old-file test matrix
   passes (done 2026-11-05).
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
   the stored `from_me`: a stored revoke row in the same chat with the exact label `[deleted <id>]` counts when
   its revoker is the target's author (the owner's own account for an own message); an admin's delete does not
   count. **It is judged when read** (`get`, `status`, the download start and its final write), through a partial
   index on the label, so arrival order and the running build do not matter; no column stores the result and no
   pass runs over old rows (2026-11-08). In a 1:1 chat the author test is `from_me` equality; in a group, sender
   identity. **Revoked wins** (a message judged revoked is never `ready`; its file is tagged then), and revoke
   tombstones are stored even from senders outside `WHATSRUST_ALLOWED_NUMBERS`. Remaining false negatives fail open,
   by policy; retention must keep revoke rows. The revoke key does not gate the judgement; its shape is only logged. The
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

**2026-11-08: the phase 2 implementation plan (design v5.2).** A read-only planning pass over the code found:
- **The revoke key is written by the revoker** (wa-rs `src/send/actions.rs`): a sender revoke carries
  `from_me = true` and no participant in every chat, an admin revoke `from_me = false` and the author. v5's rule
  "the key's `from_me`/`participant` match the target row" would have rejected every valid revoke by another person.
  Decision 9 now reads the key from the revoker's view. A cold review of v5.2 runs before phase 2b (owner).
- **A migration step runs once per DB** (`if from_version < 9`), so the one-time revoke pass ships in the same commit
  as the v9 columns (phase 2a).
- **Own messages in LID-addressed 1:1 chats were stored under the account's own number:** wa-rs sets `sender_alt` to
  the own phone JID for them, and live ingest resolved the chat with it. A separate fix commit precedes phase 2a;
  earlier rows cannot be repaired.
- The owner widened the repair (2026-11-08): own history rows whose sender is not one of the owner's identities,
  every form of the owner's number and LID for live rows, and the fetch cursor's anchor flag.

**2026-11-08: the v5.2 cold review** (`_reviewer/design/2026-11-08-on-demand-media-download-v5.md`; approve with
changes, 0 blocking, 6 major) and the owner's decisions (design v5.3):
- the revoke subsystem drew a major finding in three reviews (v3, v4, v5.2), each answered by another patch to
  "resolve at ingest". Revokes are now **judged when read**, from the stored revoke rows; `revoked_at` and the
  one-time pass are dropped. The revoke key's shape (inferred from wa-rs's send code only) is logged, not used;
- logout deletes the `device` row and the QR flow starts unpaired, so the repair runs once at the first start
  that knows the owner's number, under a marker, not inside the version step;
- a v8 binary refuses v9 only by the version number, so an in-place downgrade becomes the primary revert, once a
  test with the v8 binary proves it;
- the owner's largest profile (shape report): 26,080 messages, 963 rows to repair, no dotted LID; the budget is
  30 s for the migration and the repair on a synthetic 500,000-message DB.

**2026-11-08: the v5.3 cold review** (`_reviewer/design/2026-11-08-on-demand-media-download-v5.3.md`; approve with
changes, 0 blocking, 4 major; trajectory: converging) and the owner's decisions (design v5.4):
- the one-shot repair marker would lock in rows written later; the repair now runs at every start that knows the
  number, with a backup before it changes rows, a post-condition and a failure note (ADR 0028 amendment);
- per-column filling of duplicates could pair a key with another encryption's path; v9 adds `media_key_timestamp`
  and fills the encryption tuple only as a whole;
- "revoked wins" over a downloaded file, revoke tombstones stored past the allowlist, fail-open as policy;
- phase 2 merges to `main` after 2b.
Run 2a measured the budget (7.2–8.1 s for 500,000 messages, debug build) and proved the in-place downgrade with a
v8 build (`--migrate` accepted the DB in 0.23 s).

## Alternatives considered

See the design doc §12, among others: a size cap, bytes in the API response, serving on the REST or MCP listener,
a path only, MCP resources, folders by name or bare number, a GET that starts the fetch, trusting the sender's
extension, one lane for downloads and re-uploads, a folder quota with eviction, wa-rs `download_to_writer`, the
pin's `client.media_reupload()`, tagging at startup by absence, `Range`/`HEAD`, a download token bucket,
HMAC-signed stateless URLs, a bounded wait inside `get`, storing live references now, an async HTTP client, a
`.deleted` marker in the file name, trusting any revoke row, a small-file lane, a custom ureq transport (it works,
but uses ureq's `unversioned` API), a deny-list sanitiser with two Unicode crates, matching duplicates by message
ID alone, judging a revoke by the stored `from_me`.
