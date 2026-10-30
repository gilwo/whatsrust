# On-demand media download — design and plan

**Status:** v2 (2026-10-30), owner-approved; next: a follow-up cold design review. v1: 2026-10-29 (cold review: needs rework).
**Decision record:** ADR 0050 (Proposed); ADR 0005 amendment of 2026-10-29.
**Roadmap item:** P1 "On-demand media download for stored history" (FEATURES F1 open item).
**Related:** ADR 0005 (lazy media hydration), ADR 0012 (indefinite retention), ADR 0022 (fail-closed config),
ADR 0028–0032 (staged schema migration), ADR 0044 (REST request gate), ADR 0045/0046 (MCP listener, tool
validation), ADR 0048 (stored pairing history), ADR 0049 (deployment as a service).

This document describes what is built and how. The owner's decisions are in §9; the open items in §11.

> **wa-rs version:** this work is built on the **current pin `9e8c70e`** (owner decision 2026-10-30). A compile
> spike measured the upgrade (v0.7.0: 165 compile errors, upstream `main`: 298, mainly the protobuf move from
> `prost` to `buffa`); it is a separate project after this one (roadmap Q-WARS). Items marked **[wa-rs]** are where
> a later upgrade changes this design; they sit behind the `MediaFetcher` adapter (§6.2).

---

## 1. Goal and scope

**Goal:** a client, usually an agent on **another computer**, asks for the media of one stored message by its
ID. whatsrust downloads it in the background into a per-chat folder; once it is ready, the client gets a
short-lived URL and fetches the file over the network.

**In scope:**
- Per-message `get` and `status` calls (REST on loopback, CLI, MCP — including the remote MCP listener).
- A **dedicated media file listener** (opt-in, own port and bind) that serves only short-lived file URLs, for
  local and remote clients, with TLS through the reverse proxy.
- The folder layout under `media/`, with one name file per chat folder.
- Background download: streamed into a temporary file, renamed when complete; a fixed free-disk reserve; no size
  cap; a stall watchdog; cancel.
- Recovery of an expired link through a re-upload request to the owner's own phone, in its own lane.
- Pacing, permanent-failure states, a circuit breaker and an hourly cap for re-upload requests; a kill switch.
- Schema **v9**: new `media_refs` columns for history rows (document file name, plaintext hash, static URL, the
  raw chat and sender JIDs as the phone sent them).
- Deleted content keeps its files, marked `.deleted`.
- Media counters in `/api/status`.
- `run-profile.sh clear-data-with-media` (new; `clear-data` keeps `media/`) and a media choice in the
  `deploy.sh uninstall` backup.

**Out of scope (later):**
- **Live media.** Live ingest is unchanged: it downloads eagerly for the inbound event (50 MiB limit) and writes
  no `media_refs` row. A media message without a reference reports `no_reference`.
- Bulk downloads ("all media of this chat"): a job system like `fetch-history`.
- Contact names in folders and name files before F7 stores them (phone numbers until then).
- Multi-range HTTP requests; SSE events for "ready"; a progress percentage (a byte count is given).
- MCP resources (`resources/read`); the MCP registry serves tools only.
- Selective purge commands (a documented `find` covers `.deleted` files).

---

## 2. Background (facts the design relies on)

| Fact | Source |
|---|---|
| `media_refs(message_id PK, media_key, direct_path, file_enc_sha256, mimetype, file_length, width, height, hydrated_path)` (schema v8); written for media messages stored from history, only when the message row is new; `hydrated_path` is NULL | `src/storage.rs`; `src/bridge.rs` `persist_history_messages` |
| Messages stored before the 2026-10-22 amendment, and every live media message, have no `media_refs` row | `src/bridge.rs` (`MediaMode::Download` writes no ref; test `test_a_media_message_that_arrived_live_keeps_its_row_and_gets_no_media_ref`) |
| History rows store the chat under its phone JID even when the phone's key used a LID; the raw `remote_jid` and `participant` are not kept | `src/bridge.rs` `persist_history_messages` |
| A document's original file name exists only inside the display label `[doc: NAME SIZE] caption` in `messages.body_text` | `src/bridge.rs` `MediaMode` |
| `messages.timestamp` is epoch seconds; `messages.content_kind` is `image`/`audio`/`video`/`document`/`sticker` for media and maps 1:1 to the wa-rs `MediaType` | `src/storage.rs`; `src/bridge.rs` `content_kind()` |
| `messages.message_id` is globally unique; inserts are `OR IGNORE` | `src/storage.rs` |
| `status@broadcast` messages are ignored, never stored | `src/bridge.rs` `should_ignore_jid` |
| Local deletes happen only in the event handler, for deletes synced from the phone (REST and CLI deletes come back that way); `delete_inbound_*` removes `messages` and `media_refs` in one transaction | `src/bridge.rs` (~3674–3696); `src/storage.rs` |
| `download_from_params_to_writer(&DownloadParams, writer)` streams download, AES-CBC decryption and the HMAC check with about 40 KB of memory; the MAC is checked only after all plaintext was written; on an error some bytes may already be written; the streaming path has no byte cap. Encrypted media needs `media_key`, `direct_path`, `file_enc_sha256`; plaintext (newsletter) media needs `file_sha256` | wa-rs `src/download.rs`, `wacore/src/download.rs` **[wa-rs]** |
| Download errors are `anyhow!` strings on the streaming path (for example "MAC mismatch", "Download media not found/expired") | wa-rs `src/download.rs` **[wa-rs]** |
| The HTTP client is `UreqHttpClient::new()` (no timeouts; ureq 3 offers total timeouts only, no idle-read timeout); `UreqHttpClient::with_agent(agent)` accepts a configured agent | `src/bridge.rs:2977`; wa-rs `http_clients/ureq-client` |
| `client.media_reupload().request(...)` sends a "server-error" receipt **to the owner's own JID** (`rmr jid` = the chat JID, `participant` for groups) and waits up to a fixed `MEDIA_RETRY_TIMEOUT` = 30 s for a `mediaretry` notification; the result can be a new `direct_path` or `NotFound` (the phone no longer has the file) | wa-rs `src/features/media_reupload.rs`, `wacore/src/media_retry.rs` **[wa-rs]** |
| Most history media is probably expired: CDN links live hours to days | ADR 0005 |
| MCP tool budget 40 s; CLI client timeout 45 s | `src/mcp_http.rs` `TOOL_BUDGET`; `src/bridge.rs` `SYNC_CLIENT_TIMEOUT` |
| The REST response type buffers the whole body (`ApiResponse.body: Vec<u8>`); only SSE streams, with a write timeout | `src/api.rs` |
| Every REST route passes the Origin/Host/Content-Type gate, then the bearer-token check when a token is set; the gate blocks cross-site POSTs, not cross-site GETs | `src/api.rs` `reject_from_head`; ADR 0044 |
| The MCP HTTP listener is a separate listener (own port, bind, token), built for remote clients behind a TLS proxy; it serves only `POST /mcp` | `src/mcp_http.rs`; ADR 0045; `deploy/proxy/` |
| `GroupInfo { jid, subject, participants: [{ jid, phone, is_admin }] }` from `get_group_info` (network, cached 5 minutes) | `src/bridge.rs` |
| No contact names are stored | FEATURES F7 item 1 |
| `fs2` (free disk space) and `rand` (tokens) are dependencies | `Cargo.toml` |

---

## 3. Media identity and lookup

- The **media ID is the message ID**. IDs are random (16–32 hex or upper-case characters); a collision across
  chats is negligible, and the file path also contains the chat folder.
- A lookup joins `messages` (`chat_jid`, `sender_jid`, `from_me`, `timestamp`, `content_kind`) with `media_refs`.
- Outcomes before any download: no message → `unknown`; not a media kind → `no_media`; a media kind without a
  `media_refs` row → `no_reference`; a row that lacks the fields its kind needs (for example no `direct_path` or
  `file_enc_sha256`, or a plaintext item without `file_sha256`) → `incomplete_reference`. All four are final.

---

## 4. Schema v9

An additive migration, `media_refs` only, through the existing staged-migration machinery (pre-migration backup,
validation, circuit breaker; ADR 0028–0032; the schema version goes from 8 to 9):

| New column | Type | Filled from | Used for |
|---|---|---|---|
| `file_name` | TEXT | the document's original file name | the document name suffix (§5.4) |
| `file_sha256` | BLOB | the proto's plaintext hash | plaintext (newsletter) downloads; a size and integrity check after a download |
| `static_url` | TEXT | the proto's static URL, when present | plaintext (newsletter) downloads |
| `remote_jid` | TEXT | the message key's `remote_jid` as the phone sent it (LID or phone) | the re-upload request (`rmr jid`) |
| `participant` | TEXT | the message key's `participant` as the phone sent it | the re-upload request in groups |

- Existing rows keep NULL in the new columns; nothing is backfilled from `body_text`.
- History ingest writes the new columns for new rows, and **also for a duplicate** (a message fetched again):
  an upsert fills only columns that are still NULL, so a re-fetch repairs older rows.
- No column for state: `ready` is derived from `hydrated_path` (§6.1).

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
| Newsletter | `<id>@newsletter` | `media/newsletter_<id>/` |
| Any other | `<user>@<server>` | `media/<user>@<server>/` (sanitised) |

The prefix keeps a LID apart from a phone number and maps back to the JID exactly. A `lid_` folder exists only
for a chat that whatsrust could not map to a phone; merging it into the phone folder belongs to the `@lid` DM
split (roadmap P2), which then moves the files and rewrites `hydrated_path`.

### 5.3 The name file

- Exactly one per folder: `_group_<name>.txt` or `_chat_<name>.txt`.
- `<name>`: the group subject; for a 1:1 chat the contact name once F7 stores names, until then the international
  phone number.
- Sanitising (also used for document names, §5.4): Unicode normalisation (NFC); all control and format characters
  removed (Unicode categories Cc and Cf, which include the bidirectional overrides such as U+202E); white space →
  `_`; `/ \ : * ? " < > |` removed; runs of `_` collapsed; trimmed; lower case; at most 64 characters; an empty
  result falls back to the number. Letters of any script (for example Hebrew) are kept.
- Written when a download into the folder starts. The group metadata lookup is **non-fatal**: on any failure (the
  owner left the group, the group is gone, network) the last known name, else the number, is used. It uses the
  5-minute group cache, so a run of downloads in one group makes one lookup.
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
| Group | `<sender phone>_<epoch seconds>_<message id>.<ext>` | `972501234567_1761717600_3EB0A1B2C3D4.jpg` |
| 1:1, newsletter, others | `<epoch seconds>_<message id>.<ext>` | `1761717600_3EB0A1B2C3D4.jpg` |
| A document (either pattern) | `…_<message id>_<original name>.<ext>` | `972501234567_1761717600_3EB0A1B2C3D4_quarterly_report.pdf` |

- The epoch is the message's own time; 10 digits sort correctly as text.
- The sender phone is international; own messages use the owner's number; a `@lid` sender is mapped through
  `lid_pn_mapping`, else `lid<number>`.
- The message ID is reduced to `[A-Za-z0-9._-]`, so a name can never contain a path separator.
- **The extension comes only from an allowlist keyed by the MIME type** (type and subtype, parameters such as
  `; codecs=opus` stripped): for example `image/jpeg` → `jpg`, `image/png` → `png`, `image/webp` → `webp`,
  `audio/ogg` → `ogg`, `audio/mpeg` → `mp3`, `video/mp4` → `mp4`, `application/pdf` → `pdf`, common office
  formats, `text/plain` → `txt`. Anything else → `bin`. The sender's own extension is never used, so a file can
  never land as `.command`, `.app`, `.jar`, `.sh`, `.webloc` or similar.
- A document's original name is sanitised (§5.3) and its own extension removed before it becomes the suffix.
- **macOS:** every downloaded file gets the `com.apple.quarantine` extended attribute (best-effort), so opening it
  goes through Gatekeeper like a browser download.
- While downloading, the file is `<name>.part`; it is renamed when complete. A rename never overwrites: an
  existing final file of the expected size is **adopted** as `ready` (this covers a crash after the rename but
  before the DB update, and files kept by `clear-data`).

### 5.5 Deleted content (files are kept and marked)

Deleting a message or a chat (synced from the phone; REST and CLI deletes arrive the same way) deletes the DB rows,
including `media_refs`, but **never a media file**:

- **Message deleted:** its file gets `.deleted` before the extension (`…_3EB0A1B2C3D4.deleted.jpg`).
- **Chat deleted:** every file in the folder gets the tag, and the name file gets a line `deleted: <epoch>`. The
  folder keeps its name; new files of a chat that goes on land there untagged.
- **Order:** the hook reads the affected `hydrated_path` values first, commits the delete, then renames. Queued
  downloads of the deleted message or chat are dropped; a running one finishes, then sees the message is gone
  (§6.2) and tags its file at once. `.part` files of a deleted message are removed.
- Tagging is idempotent (a `.deleted.` file is never tagged twice). Renames are best-effort: a failure is logged
  and never blocks the delete.
- **Startup reconciliation:** files under `media/` whose message ID is no longer in the DB and that are not yet
  tagged get the tag; this covers a crash between the commit and the rename.
- Sender revoke ("delete for everyone") deletes no row today, so it marks nothing.
- **Reclaiming space:** `clear-data-with-media` (everything), or by hand: `find media -name '*.deleted.*' -delete`
  (documented in HOWTO).
- This **narrows ADR 0012** for media: a delete no longer removes everything; the owner chose keep-and-mark
  (ADR 0050 says so).

---

## 6. Download engine

### 6.1 States

| State | Meaning | Final? |
|---|---|---|
| `unknown`, `no_media`, `no_reference`, `incomplete_reference` | see §3 | yes |
| `not_downloaded` | a complete reference, no file | – |
| `queued` | waiting for the download lane (`position` given) | – |
| `waiting_reupload` | the link expired; waiting for, or in, a re-upload request | – |
| `downloading` | streaming; `bytes_written`, `bytes_expected` given | – |
| `ready` | `hydrated_path` set and the file exists | yes (until deleted on disk) |
| `failed` | `reason`, `retryable`, `retry_after_ms` | depends on the reason |

Failure reasons:

| Reason | Retryable | When |
|---|---|---|
| `not_on_phone` | no (remembered 24 h) | the re-upload answered `NotFound` |
| `integrity` | no | MAC or hash mismatch, or more bytes than claimed |
| `disk_reserve` | yes (after space is freed) | the free-space reserve would be broken (§6.4) |
| `phone_offline` | yes | no re-upload answer in 30 s |
| `reupload_paused` | yes (`retry_after_ms`) | the re-upload breaker or the hourly cap (§6.5) |
| `stalled` | yes | no progress for the watchdog period (§6.3) |
| `network` | yes | any other transfer error |
| `cancelled` | yes | `cancel` was called |
| `disabled` | yes | the kill switch is on (§8) |
| `internal` | yes | anything else (logged) |

- `ready` survives restarts. Every other non-final state lives in memory and resets to `not_downloaded` on restart
  (the queue is not persisted; a client calls `get` again). `not_on_phone` is remembered per message for 24 h in
  memory; a repeated `get` inside that time answers at once without contacting the phone.

### 6.2 Lanes

Two lanes, so a slow re-upload never blocks downloads:

- **Download lane:** one worker; a bounded FIFO queue (1000 entries; when full, `get` answers `queue_full`,
  retryable). A request for a message that is already queued or in progress joins it. Steps:
  1. Wait for the download pacer (§6.5).
  2. Free-disk check (§6.4).
  3. Write or refresh the name file (§5.3).
  4. Stream into `<name>.part` through a **counting writer** (§6.3); `fsync`; verify the size against
     `file_length` and, when stored, `file_sha256`; rename; set `hydrated_path`.
  5. Before setting `hydrated_path`, re-check that the message still exists; if it was deleted meanwhile, tag the
     file `.deleted` (§5.5).
  6. On "not found / expired" from the CDN: hand the item to the re-upload lane (`waiting_reupload`) and move on.
  7. On any other error: delete the `.part` file, set `failed(reason)`.
- **Re-upload lane:** one task. It waits for the re-upload pacer, sends the request with the raw `remote_jid` and
  `participant` (§4; for rows without them, the stored JIDs), waits up to 30 s, stores the new `direct_path` and
  puts the item at the **front** of the download queue; `NotFound` → `failed(not_on_phone)`; a timeout →
  `failed(phone_offline)` and a breaker count (§6.5).
- While the client is disconnected, both lanes wait; `status` shows `waiting: disconnected`.
- At startup, `*.part` files under `media/` are deleted, and the reconciliation of §5.5 runs.
- Error classification is isolated in one adapter (`MediaFetcher`) with a test that pins the real wa-rs error
  values **[wa-rs]**, so a library upgrade cannot silently turn "expired" into "internal".

### 6.3 Stall watchdog and cancel

- The counting writer records `bytes_written` and the time of the last progress, and **aborts** when the bytes
  exceed `file_length` + 64 KiB (the sender's claim is not trusted).
- A watchdog fails the download as `stalled` when no byte arrived for 60 s (configurable). The blocking transfer
  thread cannot be interrupted; it ends with its socket, and the worker moves on. At most 2 such detached threads
  may exist; beyond that the download lane pauses until one ends. The HTTP agent also gets generous total
  timeouts through `UreqHttpClient::with_agent`.
- `cancel` (REST, CLI, MCP) removes a queued item, or aborts the running download through the counting writer
  (`failed(cancelled)`).

### 6.4 Disk reserve

- A **fixed free-space reserve: 20 GB by default**, configurable (`WHATSRUST_MEDIA_MIN_FREE_GB`).
- Before a download: free space (`fs2::available_space` on the `media/` volume) minus `file_length` must stay at or
  above the reserve; else `failed(disk_reserve)`. A missing `file_length` counts as 0.
- While writing: the free space is checked again every 64 MiB; below the reserve the download aborts
  (`disk_reserve`).
- The reserve protects `whatsapp.db` and the Signal keys, which share the volume.

### 6.5 Pacing, breaker, hourly cap

| Control | Default | Why |
|---|---|---|
| Download pacer | token bucket: burst 5 (at most 20), then 1 every 2 s | a normal client downloads media now and then; bounds the request rate |
| Re-upload pacer | at most 1 every 30 s | each request makes the owner's phone upload a file |
| Re-upload hourly cap | 20 per hour | bounds the volume of automated receipts to the phone |
| Re-upload breaker | after 3 consecutive timeouts: pause re-uploads 30 min, doubling to 4 h; reset by the first success | an offline phone must not receive a stream of requests |

Pacing bounds the rate; the hourly cap and the breaker bound the volume.

---

## 7. Serving files

### 7.1 Who fetches the file

The main client is an **agent on another computer**, connected through the MCP HTTP listener (usually behind the
TLS proxy). A local agent or the owner's browser may also use the URL. A local file path is returned as well (in
`status` and `get`) for same-machine clients that prefer it.

### 7.2 The media file listener

- **A separate listener, off by default:** `WHATSRUST_MEDIA_PORT`, `WHATSRUST_MEDIA_BIND` (default `127.0.0.1`),
  `WHATSRUST_MEDIA_ALLOW_REMOTE` for a non-loopback bind (logs a warning), and `WHATSRUST_MEDIA_PUBLIC_URL`, the
  base URL that clients use (for example `https://wa.example.com`, through the proxy; default
  `http://<bind>:<port>`). Startup rules like the MCP listener (validated before the bridge starts; an unsafe
  config refuses to start; the port is bound before the bridge).
- **It serves exactly one route:** `GET` (and `HEAD`) `/media-files/<token>/<file name>`. Everything else → 404;
  other methods → 405. No bearer token: the URL token is the credential. The REST listener and its pipeline are
  unchanged.
- **A separate port is a separate browser origin**, so a file opened from it cannot read the REST or MCP APIs.
- **The proxy recipe** (`deploy/proxy/` nginx and Caddy) gains a `/media-files/` location for this listener, and
  the proxy smoke playbook covers it.

### 7.3 Response headers (every file response)

- `Content-Type`: the validated MIME type (`type/subtype` token grammar, parameters dropped) for allowlisted
  types, else `application/octet-stream`. A value that fails validation is never echoed.
- `Content-Disposition`: **`attachment` by default**; `inline` only for `image/png`, `image/jpeg`, `image/gif`,
  `image/webp`, `audio/*` and `video/*`. The file name is encoded per RFC 6266/5987 (`filename*=UTF-8''…`).
- Always: `X-Content-Type-Options: nosniff`, `Content-Security-Policy: sandbox; default-src 'none'`,
  `Referrer-Policy: no-referrer`, `Cache-Control: private, no-store`.
- No header value is built from unvalidated data; CR and LF can never reach a header.

### 7.4 Streaming

- The file is copied in chunks (no whole-file buffer), with a per-write timeout (30 s) and a concurrency cap
  (8 file streams; more → 503 with `Retry-After`).
- A single `Range: bytes=a-b` request is honoured (`206`, `Accept-Ranges: bytes`), so video players can seek;
  a multi-range request gets the whole file.
- A file that disappeared or was tagged `.deleted` after the token was issued → 404.

### 7.5 Tokens

- 32 random bytes from the OS RNG, base64url (43 characters).
- **One live token per file:** a repeated `get` on a ready file returns the same URL until it expires; at most 256
  live tokens (an expired one is dropped first; a full table answers `url_busy`, retryable).
- Valid 5 minutes (configurable), reusable within that time; kept in memory; lost on restart.
- The file-name part of the URL is percent-encoded when issued, and decoded and compared when served.
- Tokens never appear in logs; a log line names the message ID instead.

---

## 8. API, CLI, MCP

### 8.1 REST (loopback REST listener)

| Method and path | Behaviour |
|---|---|
| `POST /api/media/get` body `{"id"}` | **ready** → 200 `{ok: true, id, state: "ready", url, expires_at, file_name, path, size, mimetype}` (`url` is absent and `code: "url_unavailable"` is set when the media listener is off). **Not ready** → queues it when needed and returns **202** `{ok: false, code: "not_ready", id, state, retryable: true, retry_after_ms, …}`. A final state → 404 with its code (`unknown`, `no_media`, `no_reference`, `incomplete_reference`) or 409 (`not_on_phone`, `integrity`). |
| `GET /api/media/status?id=` | 200 with the state and its fields (`bytes_written`, `bytes_expected`, `position`, `waiting`, `reason`, `path`); never starts anything. |
| `POST /api/media/cancel` body `{"id"}` | removes a queued item or stops the running download. |

`get` and `cancel` are POSTs: they can make the daemon contact WhatsApp, and the request gate blocks cross-site
POSTs but not cross-site GETs.

### 8.2 CLI

`whatsrust media-get <id>`, `whatsrust media-status <id>`, `whatsrust media-cancel <id>` (JSON to stdout; a
`not_ready` answer exits 1).

### 8.3 MCP

| Tool | Access class | Annotations |
|---|---|---|
| `whatsrust_media_get` | a new class for "writes local files, contacts WhatsApp, not destructive" | `readOnlyHint: false`, `destructiveHint: false`, `idempotentHint: true`, `openWorldHint: true` |
| `whatsrust_media_status` | Read | as the other read tools |
| `whatsrust_media_cancel` | the same new class | `idempotentHint: true` |

Strict schemas (`id` string, required; ADR 0046). Read-only MCP mode offers only `media_status` (the registry test
that counts read tools is updated). The URL comes from `WHATSRUST_MEDIA_PUBLIC_URL`, so a remote agent can use it.

### 8.4 `/api/status`

A `media` snapshot: `queued`, `downloading`, `waiting_reupload`, `ready_total`, `failed` per reason,
`reuploads_requested`, `reuploads_succeeded`, `reupload_breaker` (`closed`/`open` with the time left),
`bytes_downloaded`, `url_tokens_live`, `file_streams_active`; `null` when the feature is off.

---

## 9. Owner decisions (2026-10-28 → 30)

| Topic | Decision |
|---|---|
| Folder | Named by chat ID with a type prefix, not by name; the name lives in `_group_<name>.txt` / `_chat_<name>.txt` |
| Group files | Prefixed with the sender's international phone, then the epoch seconds, then the message ID |
| Group name file | The participants' phone numbers (names after F7) |
| Names before F7 | Phone numbers |
| API | `get` returns a short-lived URL when ready; otherwise starts the fetch and fails (HTTP 202, `ok: false`); a separate `status` |
| URL | **Remote clients in scope**: the agent runs on another computer; a dedicated media file listener (2026-10-30) |
| HTTP method | `get` is a POST |
| Size | No size cap; a fixed free-space reserve of 20 GB, configurable (2026-10-30) |
| Scope | One message per call; bulk later |
| Live media | **Not supported now** (2026-10-30); `no_reference` for such messages |
| Schema | **v9 migration authorised** for the new `media_refs` fields (2026-10-30) |
| Re-upload | A live test against the owner's account before the engine is built; the owner prepares it (2026-10-30) |
| Deleted content | Files kept and marked `.deleted` before the extension; a deleted chat also gets a `deleted:` line |
| `clear-data` | Keeps `media/`; a new `clear-data-with-media` deletes it |
| Deploy uninstall | A choice to leave `media/` out of the backup |
| Document names | Keep the original file name as a suffix (sanitised; the extension from the allowlist) |
| Counters | A `media` snapshot in `/api/status` |
| wa-rs | Build on the current pin; the upgrade is a separate project afterwards (2026-10-30) |
| Uninstall media prompt | Include `media/` in the backup by default (`[Y/n]`, 2026-10-30) |
| Design | v2 approved by the owner (2026-10-30) |

---

## 10. Scripts and configuration

### 10.1 Scripts

- **`run-profile.sh clear-data`** is unchanged and keeps `media/` (the files stay, without DB rows).
- **`run-profile.sh clear-data-with-media <profile> [--all] [--dry-run] [--yes] [--backup-media <file>]`** (new):
  the `clear-data` flow (refuses while the daemon runs, prints the exact SQL, backs up the DB, asks y/N and then
  the profile name), plus the size of `media/`, then deletes it. No media backup by default; `--backup-media
  <file>` writes an uncompressed `tar` (media is already compressed) after a free-space check, and verifies it.
- **`deploy.sh uninstall`**, when the data is backed up before deletion: shows the size of `media/` and asks
  "Include media/ (N GB) in the backup? [Y/n]" (default: include; flag `--backup-media yes|no`); a "no" leaves
  `media/` out; the script checks free space first and does not gzip the media part.
- **`deploy.sh re-pair` (fresh mode)** keeps `media/` in place (the same account re-links); HOWTO says to move it
  aside first when linking a different account.

### 10.2 Configuration

| Variable | Default | Floor / ceiling (override) |
|---|---|---|
| `WHATSRUST_MEDIA_DOWNLOAD` | `on` | – (`off` = kill switch: `get` answers `disabled`, nothing is contacted) |
| `WHATSRUST_MEDIA_MIN_FREE_GB` | 20 | ≥ 1 |
| `WHATSRUST_MEDIA_DOWNLOAD_BURST` | 5 | 1–20 |
| `WHATSRUST_MEDIA_DOWNLOAD_INTERVAL_MS` | 2000 | ≥ 200 |
| `WHATSRUST_MEDIA_REUPLOAD_INTERVAL_SECS` | 30 | ≥ 10, unless `WHATSRUST_DANGEROUSLY_ALLOW_FAST_MEDIA_REUPLOAD=1` |
| `WHATSRUST_MEDIA_REUPLOAD_PER_HOUR` | 20 | ≤ 60, unless `WHATSRUST_DANGEROUSLY_ALLOW_FAST_MEDIA_REUPLOAD=1` |
| `WHATSRUST_MEDIA_STALL_SECS` | 60 | 10–600 |
| `WHATSRUST_MEDIA_URL_TTL_SECS` | 300 | 30–3600 |
| `WHATSRUST_MEDIA_PORT` / `_BIND` / `_ALLOW_REMOTE` / `_PUBLIC_URL` | off / `127.0.0.1` / off / derived | startup rules as the MCP listener |

All in `.env.example`; validated at startup (ADR 0022).

---

## 11. Open items

1. **The live re-upload test** (owner, phase 1): a 1:1 chat, a chat whatsrust keeps under `@lid`, and a group;
   each with a recent and an old file. It decides whether the raw `remote_jid`/`participant` (§4) or the stored
   JIDs address the request correctly, and how many old files the phone still holds.
2. **Resolved 2026-10-30:** the wa-rs version (build on the current pin; upgrade later, roadmap Q-WARS) and the
   default of the uninstall media prompt (include, `[Y/n]`).

---

## 12. Alternatives considered

| Alternative | Why not (now) |
|---|---|
| A fixed size cap | streaming keeps memory flat; the owner chose no cap; the free-space reserve covers the real risk |
| Bytes in the API response (base64) | 33 % bloat; large files exceed the 40 s / 45 s call budgets |
| Serving files on the REST listener | a remote agent would need the whole REST API exposed, writes included; same-origin risk |
| Serving files on the MCP listener | breaks ADR 0045 ("POST /mcp only") and mixes concerns |
| A local path only | the agent runs on another computer; the path is still returned for local clients |
| MCP resources | the registry serves tools only; a later option |
| Folders named by chat name | names change and repeat; the name file carries the name instead |
| Folder by bare number | a LID and a phone number can collide |
| A GET that starts the fetch | cross-site GETs pass the request gate |
| Trusting the sender's extension | executable extensions could land on disk under a harmless name |
| One worker for downloads and re-uploads | a 30 s re-upload wait would block every download behind it |
| A folder quota with eviction | complex, and it guesses which files to delete; the fixed reserve is simpler |
| Backfilling document names from `body_text` | the label is not reliably parseable |

---

## 13. Plan

| Phase | Work | Exit criteria |
|---|---|---|
| 0 | Branch from `main`; the "after a merge" step | committed; 0 stale doc hashes |
| 1 | **The live re-upload test** with a small probe command in a dev build (download → re-upload → download, reporting each step), on the current wa-rs pin | the test result recorded in FEATURES F1; §4/§6.2 updated if the addressing differs |
| 2 | Schema v9 (staged migration, tests per ADR 0028–0032); history ingest writes the new columns and upserts duplicates | migration tests (fresh DB, v8 → v9, rollback path); ingest tests |
| 3 | Layout: pure functions for folders, file names, sanitising, the MIME allowlist, the name file; the macOS quarantine attribute | unit tests for every row of §5.2 and §5.4, Cf/bidi stripping, Hebrew, empty names, executable extensions → `bin` |
| 4 | The engine: `MediaFetcher` seam (wa-rs implementation + fake), the two lanes, the counting writer, the watchdog, the reserve, pacers, the breaker, the hourly cap, the states, cancel, startup cleanup and reconciliation, delete marking | fake-driven tests for every state and reason, the race "deleted while downloading", queue full, disconnected wait, adoption of an existing file |
| 5 | The media file listener (startup rules, one route, headers, streaming, Range, tokens); REST `get`/`status`/`cancel`; the proxy recipe update | gate and header tests (an HTML document is served as `attachment` + `octet-stream` with the security headers; CRLF in a MIME value never reaches a header; expired token 404; Range 206); REST golden cases (labelled); the proxy smoke playbook |
| 6 | CLI and MCP (new access class, read-only mode), `/api/status` counters, config validation | registry and validation tests; stdio/HTTP parity |
| 7 | Scripts: `clear-data-with-media`, the `deploy.sh uninstall` media choice; `.gitignore` | shellcheck, dry runs, the deploy harness re-run |
| 8 | Docs (AGENTS.md checklist) and a review pass | `make test`, `make test-all`, `make lint` (no new warnings) |
| 9 | Live check (owner): an image and a document from history, one expired file, through the remote URL | the files open; `status` shows `ready` |

---

## 14. Test plan

- **Unit:** naming (all chat kinds, LID fallback, sanitising incl. Cf/bidi, Hebrew, empty names), the MIME
  allowlist and `bin` fallback, the state machine, pacers and the breaker (paused tokio clock), the reserve
  arithmetic, the counting writer (over-claim abort), tokens (expiry, reuse, cap), RFC 6266 encoding.
- **Engine with a fake fetcher:** every reason of §6.1; expired → re-upload → success; re-upload `NotFound`
  remembered; breaker open and reset; stall; cancel; deleted while downloading; startup reconciliation.
- **Migration:** v8 → v9 on a copy of a real-shaped DB, fresh v9, the circuit-breaker path.
- **Listener:** routes, methods, headers per type, Range, streaming cap, write timeout, 404 hygiene.
- **REST and MCP:** golden cases, schemas, access classes, read-only listing.
- **No live WhatsApp contact in tests.** The live checks are the owner's (phases 1 and 9).

---

## 15. Docs to update (AGENTS.md checklist)

CHANGELOG; roadmap (P1 row, then Completed); FEATURES F1 (open item → done, issues, the live-test result);
ADR 0050 → Accepted; README (counts: 59 → 62 REST endpoints, 34 → 37 MCP tools, 50 → 53 CLI commands; the media
listener); HOWTO §4 (usage, the `find` purge), §5a-like section for the media listener, §3 (`clear-data-with-media`),
§3b (the uninstall choice); ARCHITECTURE (media lanes, listener, schema v9); AGENTS.md (key files, patterns,
schema v9); `.env.example`; `deploy/proxy/` and its README.
