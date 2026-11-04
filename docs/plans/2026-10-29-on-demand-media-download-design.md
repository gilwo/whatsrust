# On-demand media download — design and plan

**Status:** v5 (2026-11-04): the v4 follow-up cold review and a ureq timeout spike folded in, with the owner's
decisions of 2026-11-04 (§9). Next: the old-file test matrix (§11), then the build. History: v1 2026-10-29 (cold
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
- Schema **v9**: six new `media_refs` columns for history rows (document file name, plaintext hash, the raw chat
  and sender JIDs from the message key, the revoke time, the view-once flag).
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
| History rows store the chat under its phone JID even when the key used a LID, a full sender JID (`…@s.whatsapp.net`, or `…@lid` when unmapped) and the key's `from_me`; the raw `remote_jid` and `participant` are not kept. Live rows store a bare sender number and always `from_me = 0` (a bug, roadmap) | `src/bridge.rs` `persist_history_messages`, `resolve_sender`; `src/storage.rs` `insert_inbound` |
| Backfill pages strictly backward from the oldest stored message, so it does not revisit stored rows; a re-pair (pairing package) or a retried page can | `src/backfill.rs` (~77–91) |
| A document's original file name exists only inside the display label `[doc: NAME SIZE] caption` in `messages.body_text` | `src/bridge.rs` `MediaMode` |
| History extraction unwraps view-once wrappers into a normal media message; the view-once flag is set only on live events and is not stored | `src/bridge.rs` `unwrap_to_inner` (~3957), live flag (~3316) |
| `messages.timestamp` is epoch seconds; `messages.content_kind` is `image`/`audio`/`video`/`document`/`sticker` for media and maps 1:1 to the wa-rs `MediaType` | `src/storage.rs`; `src/bridge.rs` |
| `messages.message_id` is globally unique and sender-chosen; inserts are `OR IGNORE`; the `messages` indexes are `(chat_jid, timestamp)`, `message_id` and `embed_status` | `src/storage.rs` (~304–309) |
| `status@broadcast` and `@newsletter` messages are ignored, never stored, so there is no plaintext (newsletter) media | `src/bridge.rs` `should_ignore_jid` |
| A sender revoke is stored as its own row (`content_kind = 'revoke'`, label `[deleted <target id>]`); extraction keeps only the target's `key.id` and drops the key's `from_me`, `participant` and `remote_jid`; the revoker is the row's sender (live rows: `from_me = 0` always); the target row stays | `src/bridge.rs` `InboundContent::Revoke` (~527, ~4387-4394) |
| Local deletes happen only in the event handler, for deletes synced from the phone (REST and CLI deletes come back that way); each delete event runs in its own spawned task; `delete_inbound_*` removes `messages` and `media_refs` in one transaction; `delete_inbound_message` matches the message ID only, although the event also carries the chat | `src/bridge.rs` (~3674–3712); `src/storage.rs` (~1963–1979) |
| The public parts of a download: `client.refresh_media_conn(force)` (hosts and auth; `force = true` sends an IQ); `MediaConnection::from(&MediaConn)`; `DownloadUtils::prepare_download_requests(&dyn Downloadable, &MediaConnection)` (one URL per host); `DownloadUtils::decrypt_stream_to_writer(reader, media_key, media_type, writer)` (AES-CBC in 8 KiB chunks, the MAC checked at the end; errors are `anyhow` texts such as "MAC mismatch" and "Invalid final media size", or a wrapped `io::Error`; some plaintext is written before an error) | wa-rs `src/mediaconn.rs`, `src/download.rs`, `wacore/src/download.rs` (~350–467) **[wa-rs]** |
| wa-rs's own `download_to_writer` seeks the writer to 0 for every host attempt and treats any error, a writer error included, as "try the next host" | wa-rs `src/download.rs` (~228–237, 425) **[wa-rs]** |
| The shared HTTP client is `UreqHttpClient::new()` with ureq's default `http_status_as_error = true`: every 4xx/5xx becomes the error `http status: <n>`, so wa-rs's 404/410 ("not found/expired") and 401/403 branches never run, and its media-connection refresh retry neither | ureq 3.3.0 `config.rs:867`, `run.rs:126-129`; wa-rs `http_clients/ureq-client` `lib.rs:56-75,139` **[wa-rs]** |
| ureq 3.3 timeouts: for the phase being checked, `next_timeout()` starts from **now**, and the body loop checks `RecvBody` on every read, so **`timeout_recv_body` is an idle-read limit**, not a total. Every check also includes `Global` and `PerCall`, whose start times are recorded when the call is created, so **`timeout_global` is the total limit** of a download. Both are settable per request through `config()`. Redirects default to 10 (`max_redirects`; `max_redirects(0)` returns a 3xx response). A 2026-11-04 spike confirmed the idle behaviour over HTTP (a 10 s body in 1 s chunks passed a 5 s `timeout_recv_body`) | ureq 3.3.0 `timings.rs:63-68, 96-100, 152-172`, `run.rs` (~674, ~714), `config.rs:203, 669-748, 874`, `request.rs:349`; spike in `/tmp/ureq-idle-spike` |
| `direct_path` is sender-controlled and goes into the URL right after the host (`https://{host}{direct_path}…&auth=…`) with no check | `wacore/src/download.rs:291-294` |
| The re-upload request is a `server-error` receipt with an encrypted payload and an `<rmr jid from_me participant>` child; the phone answers with a `mediaretry` notification: `Success { direct_path }`, `NotFound` (code 2), `DecryptionError` (code 3), `GeneralError`; the server ACKs the receipt, with an `error` attribute when it rejects it | `wacore/src/media_retry.rs`; upstream `main` `src/features/media_reupload.rs` **[wa-rs]** |
| **The pin's `client.media_reupload()` sends the receipt `to` the account's phone number. On the owner's LID-migrated account the phone never answered (0 of 10). The same receipt `to` the account's own LID (`client.get_lid()` without the device part), with the chat and the participant in LID form, worked 3 of 3** (1:1, group from a member, the account's own group message); ACK in 0.2 s, answer in about 1 s; no size mismatch on 5 photos. Upstream `main` does the same and keeps a PN chat when no mapping is known. The parts are public at the pin: `encrypt_media_retry_receipt`, `build_media_retry_receipt`, `parse_media_retry_notification`, `client.send_node`, `client.wait_for_node(NodeFilter)` | phase 1 live test 2026-11-01 (HOWTO §3 "Media re-upload probe"; ADR 0050 Notes) **[wa-rs]** |
| Most history media is probably expired: CDN links live hours to days | ADR 0005 |
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
  - `revoked_at` is set (§5.5) → `revoked`;
  - `view_once` is true → `view_once` (a row stored before v9 has `view_once` NULL: unknown, allowed, and
    `status` says `view_once: unknown`).

---

## 4. Schema v9

An additive migration, `media_refs` only, through the existing staged-migration machinery (pre-migration backup,
validation, circuit breaker; ADR 0028–0032; the schema version goes from 8 to 9):

| New column | Type | Filled from | Used for |
|---|---|---|---|
| `file_name` | TEXT | the document's original file name | the document name suffix (§5.4) |
| `file_sha256` | BLOB | the proto's plaintext hash | a check of the decrypted file after the download (the MAC already authenticates the encrypted bytes; kept by the owner, 2026-11-01) |
| `remote_jid` | TEXT | the message key's `remote_jid` as the phone sent it (LID or phone) | the re-upload address of a 1:1 chat (§6.2) |
| `participant` | TEXT | the message key's `participant` as the phone sent it | the re-upload address of a group sender (§6.2) |
| `revoked_at` | INTEGER | the epoch seconds of a valid revoke (§5.5) | the final state `revoked` without a label lookup |
| `view_once` | INTEGER | 1 when history extraction unwrapped a view-once wrapper, else 0 | the final state `view_once` |

- Existing rows keep NULL in the new columns; nothing is backfilled from `body_text`. A NULL `view_once` means
  "unknown" and is allowed. Their re-upload address comes from the LID↔phone mapping (§6.2); `status` reports the
  source (`addressing: raw | mapped | stored | unresolved`).
- **The message and its reference are written in one transaction** (one storage call), so a crash or a failed
  insert can no longer leave a history media message without its reference.
- History ingest writes the new columns for new rows. For a **duplicate** (a message fetched again), and **only
  when the incoming message matches the stored row** (the same `chat_jid`, the same sender as the same user in
  phone or LID form, the same `from_me`, the same `content_kind`):
  - an existing `media_refs` row gets its NULL columns filled;
  - a missing `media_refs` row is **created when the stored message's `source` is not `live`** (`bootstrap` or
    `backfill`); a live row stays without a reference, as live media is out of scope (owner, 2026-11-03; this
    revises the "fill only" rule of 2026-11-01).
  - A mismatch changes nothing; it is counted (`duplicate_mismatch` in the persist counts) and logged once per
    batch. Message IDs are sender-chosen, so a colliding ID must never fill or create another message's reference.
- Because backfill does not revisit stored rows (§2), old history rows without a reference are repaired by a
  re-pair that keeps history, or by `clear-data` followed by a new fetch (HOWTO).
- No column for the transient states: `ready` is derived from `hydrated_path` (§6.1).
- **There is no supported way back after a healthy v8 → v9 migration.** A v8 binary refuses a v9 DB, and
  `--rollback` runs only after a failed migration (§2). The only revert is restoring the pre-migration backup by
  hand; it loses everything received since, including the Signal session state in the same file, so a re-pair may
  follow. Phase 2 delivers a HOWTO section for that manual revert, rehearsed on a copy of a real-shaped DB.
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
- **A valid revoke** is judged by identities, never by the live `from_me` flag:
  - the revoker is the revoke row's sender, compared as a user (phone or LID form, through the mapping and the
    account's own phone number and LID);
  - extraction keeps the revoke key's own `from_me` and `participant` (they name the target's author);
  - the revoke counts when the target is stored in the same chat, the revoke key's `from_me`/`participant` match the
    target row, and the revoker is the target's author: the owner's own account for an own target (whatever the
    stored flag says), else the target's sender.
  - An admin's delete in a group does not count (the media stays available; documented).
  - It is resolved **once, when the revoke row is stored** (live or history): its `media_refs.revoked_at` is set and
    its file is tagged. The schema migration also runs a **one-time pass** over the revoke rows already stored,
    joined to their targets by the exact label `[deleted <id>]` within the chat (the key fields are not stored
    there, so only the revoker-is-author test applies).
  - A revoke that arrives before its target is not applied (accepted: history usually delivers a revoked message as
    an empty stub without media). `revoked_at` is never cleared automatically.
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
| `<chat>/_deleted/<name>` | the message is stored again, has no `revoked_at`, and `get` is called | `<chat>/<name>` (un-tagged, adopted) |
| `<chat>/<name>.part` | the download finishes and the row still exists | `<chat>/<name>`, `hydrated_path` set |
| `<chat>/<name>.part` | the download finishes and the row is gone, or `revoked_at` is set | `<chat>/_deleted/<name>` (renamed straight there) |
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
| `ready` | `hydrated_path` set, outside `_deleted/`, and the file exists | yes (until deleted on disk) |
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
  because CDN URLs carry the media `auth` token.

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
6. Set `hydrated_path` with one statement that also requires the message row to exist and `revoked_at` to be NULL
   (`UPDATE … WHERE message_id = ? AND revoked_at IS NULL AND EXISTS (…)`). No row affected → the message was
   deleted or revoked meanwhile → `tag` the file (§5.5).
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
- **No LID form** for the 1:1 chat or the group sender: whether the stored phone JID is sent instead is decided by
  the phase 4 gate (§11, §13). Until the gate shows that the stored form works, the item fails as
  `address_unresolved` without contacting the phone.
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

## 9. Owner decisions (2026-10-28 → 11-04)

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
| Schema | **v9 migration authorised** for the new `media_refs` fields (2026-10-30); six columns: `file_name`, `file_sha256`, `remote_jid`, `participant` (2026-11-01), `revoked_at`, `view_once` (2026-11-03); `static_url` dropped (2026-11-01) |
| Re-upload | Live-tested against the owner's account (2026-11-01): only the request to the own LID works; whatsrust builds it at this pin. The old-file test matrix gates phase 4 (2026-11-03) |
| Re-upload byte budget | 1 GB per hour, configurable (2026-11-03); counted in one window with the request cap (2026-11-04) |
| Transport | whatsrust runs the media HTTP transfer with its own agent (2026-11-01) |
| Duplicates | A duplicate history message fills an existing reference, and creates a missing one when the stored row is not live; the message and its reference are written in one transaction (2026-11-03; revises "fill only" of 2026-11-01) |
| No `file_length` | No byte limit; the reserve and the transfer limit apply (2026-11-01) |
| Kill switch | `WHATSRUST_MEDIA_DOWNLOAD`, **off by default** (2026-11-03); off stops new downloads only; ready files are still served (2026-11-01) |
| Revoke | Only a revoke by the target's sender counts; resolved at ingest into `revoked_at`; the file moves to `_deleted/`; `get` answers `revoked`; no new download (2026-11-01, rule 2026-11-03) |
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
| Revoke identities | The owner's own revoke counts whatever the stored `from_me` says; existing revoke rows get a one-time pass; a revoke before its target is not applied (2026-11-04) |
| Duplicates | Fill or create only when chat, sender, `from_me` and kind match (2026-11-04) |
| Re-upload window | Requests and bytes in one window; a lone over-size file is admitted into an empty window; the fresh path persists 1 h (2026-11-04) |
| Sanitiser | A std allow-list; no Unicode crates (2026-11-04) |
| Off | With the feature off, only the two lanes stop; the delete and revoke hooks, the journal replay, the `.part` cleanup and the counts always run (2026-11-04) |
| Matrix threshold | Go when the mapped 1:1 or the mapped group addressing recovers files older than one week; a LID-less account falls back to the pin's phone-number request (2026-11-04) |
| Number allowlist | `WHATSRUST_ALLOWED_NUMBERS` does not scope media downloads, as for the history tools (2026-11-04) |

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

1. **The old-file test matrix: the gate for merging phase 2** (and so for everything after it; owner; the probe is
   ready, HOWTO §3 "Media re-upload probe"). Fetch chats with old media, then probe stored rows of each class:
   - a 1:1 chat with a mapped LID; a group sender with a mapped LID;
   - a row with no LID form (it decides whether the stored phone JID may be sent; until then `address_unresolved`);
   - an own message;
   - **a known-good control:** a fresh history file (not yet expired) downloaded with 200 through the pin's URL
     assembly, so that a 403 or 404 on old files is not a URL-building fault (some paths carry a query string, and
     the pin appends `?auth=…` unconditionally).
   Record what the CDN answers for expired links of several ages (the expected `http status: 404`/`410`, or 403)
   and how long the phone keeps files. The matrix goes into ADR 0050.
   **Go/no-go:** go when the mapped 1:1 or the mapped group addressing recovers files older than one week. Otherwise
   the scope shrinks to "history files whose link has not expired yet", and the re-upload lane is reconsidered.
2. **A second account** (owner: there is one): check whether it has a LID (HOWTO §3 probe: `media-probe x` prints
   the `own account:` line), and include it in the matrix when it is paired. An account without a LID uses the pin's
   phone-number request (§6.2), which the matrix then tests.
3. **The HTTPS check of the native limits:** the spike proved the idle limit over HTTP only. Phase 4's tests run
   both limits over HTTPS against a local TLS server (§14).
4. **Related, outside this feature** (roadmap rows): live ingest downloads from the same unchecked `direct_path`
   (`src/bridge.rs` ~4089); live rows store `from_me = 0` for own messages.
5. **Resolved:** the wa-rs version and the uninstall media prompt (2026-10-30); the re-upload addressing for fresh
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
| Looking up the revoke label on every `get` | a chat range scan under the shared DB lock per poll; `revoked_at` is resolved once instead |
| `Range` and `HEAD` | an agent fetches whole files; can be added for browser video playback later |
| A deny-list sanitiser (NFC plus the Cc/Cf categories) | needs two Unicode crates; a std allow-list removes the same dangerous characters by construction |
| Matching duplicates by message ID alone | IDs are sender-chosen; a colliding ID could fill or create another message's reference |
| Judging a revoke by the stored `from_me` | live rows store 0 for own messages, so the owner's own revokes would never count |
| A download token bucket | one download worker already limits the rate; the phone is protected by the re-upload controls |
| HMAC-signed stateless URLs | survive restarts, but add a secret to manage; the in-memory token table is small and a client calls `get` again |
| A bounded wait inside `get` (`wait_ms`) | the 202-and-poll pattern fits the call budgets; a later option |
| Storing live media references now | out of scope by owner decision; needs the live `from_me` fix first |

---

## 13. Plan

| Phase | Work | Exit criteria |
|---|---|---|
| 0 | Branch from `main`; the "after a merge" step | ✅ committed; 0 stale doc hashes |
| 1 | **The live re-upload test** with the dev probe (`media-probe`, HOWTO §3) | ✅ 2026-11-01: addressing for fresh messages settled (own LID), recorded in FEATURES F1 and ADR 0050 |
| 1b | **The old-file test matrix** (§11.1; owner, with the probe) | the matrix recorded in ADR 0050; the go/no-go decision taken |
| 2 | Schema v9 (6 columns; staged migration, tests per ADR 0028–0032); the message and its reference in one transaction; history ingest writes the new columns and the `view_once` flag; the duplicate rule (match, then fill, or create when not live); the revoke extraction (key `from_me`/`participant`), its resolution at ingest (`revoked_at`) and the one-time pass; the HOWTO manual-revert section. **Merged only after phase 1b** | migration tests (fresh DB, v8 → v9, the failed-migration path, the one-time revoke pass); the manual revert rehearsed on a copy; ingest, duplicate (match and mismatch) and revoke tests (own, valid, forged, before the target) |
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
- **Delete and revoke (v5):** the owner's own revoke from a live row counts; a revoke whose key does not match the
  target changes nothing; a message delete with a colliding ID in another chat moves nothing; adoption finds a file
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
