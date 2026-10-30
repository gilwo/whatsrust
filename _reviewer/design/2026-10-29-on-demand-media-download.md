# Cold design review: on-demand media download (ADR 0050, design doc, ADR 0005 amendment)

**Date:** 2026-10-29 · **Review:** first (no prior review of this design) · **Reviewer:** cold, independent `design-reviewer` subagent
**Artifact:** `docs/plans/2026-10-29-on-demand-media-download-design.md`, `docs/adr/0050-on-demand-media-download.md` (Proposed), the 2026-10-29 amendment of `docs/adr/0005-lazy-media-hydration-with-persistent-refs.md`
**Verdict:** Needs rework (revise the design document; the approach itself stands). Blocking 1, Major 10, Minor 11, Questions 6.

---

Reviewed: `docs/plans/2026-10-29-on-demand-media-download-design.md` (cited as §n), `docs/adr/0050-on-demand-media-download.md`, and the 2026-10-29 amendment in `docs/adr/0005-lazy-media-hydration-with-persistent-refs.md`. Checked against `src/api.rs`, `src/bridge.rs`, `src/storage.rs`, `src/main.rs`, `scripts/deploy.sh`, ADRs 0012/0022/0044/0049, and the pinned wa-rs checkout (`9e8c70e`) plus the ureq 3.3.0 source in the cargo registry. Nothing was run.

**Steelman.** The core shape is right: refs only, stream decrypted bytes into a `.part` file (wa-rs really does decrypt in ~8 KB chunks), rename on success, and a job-style API (202 then poll) that fits the 40 s / 45 s call budgets. A capability URL for a same-host client is a reasonable pattern. The feature is additive, has no destructive migration, and stays inside the ADR 0022 fail-closed config style. The problems are in the serving half, in liveness, in a few wrong "facts" in §2, and in the unproven recovery path.

---

## Blocking (1)

**B1. §6.1/§6.2: the file route serves sender-controlled bytes and sender-controlled `Content-Type`, inline, from the API origin.**
- **Problem.** §6.1 sets `Content-Type` from the MIME type and `Content-Disposition: inline`. That MIME comes from `media_refs.mimetype`, copied from the sender's proto (`MediaMode::bytes` in `src/bridge.rs`). A contact can send a document with mimetype `text/html`, or an SVG or PDF carrying script.
- **Attack.** The owner or an agent opens the returned `http://127.0.0.1:<port>/media-files/...` URL in a browser. The script now runs on the same origin as `/api/history` and `/api/events`. Same-origin GETs carry no `Origin` and a valid `Host`, so they pass `gate_request`. With no token, which is the default on loopback, the script reads all history and the live event stream and sends them out. POSTs stay blocked, but read access is exactly what ADR 0044 names as the threat.
- **Second hole.** A mimetype containing CRLF becomes header injection: `ApiResponse.headers` writes `name: value` unescaped, and today `content_type` is a `&'static str`, so the new dynamic path has no existing guard.
- **Third hole.** If an HTML or SVG file loads external resources, the Referer leaks the token URL.
- **Direction.**
  - Serve `attachment` by default. Allow `inline` only for a small safe allowlist (png/jpeg/gif/webp, audio/*, video/*, possibly pdf). Send everything else as `application/octet-stream`.
  - Always send `X-Content-Type-Options: nosniff`, `Content-Security-Policy: sandbox; default-src 'none'` and `Referrer-Policy: no-referrer`.
  - Validate the MIME against the `type/subtype` token grammar before emitting it.
  - Encode the filename per RFC 6266/5987.
  - As defence in depth, put the file URL on a different origin by spelling it `localhost` while the API is addressed as `127.0.0.1`. The Host gate accepts both, and a cross-origin script then triggers the Origin check.
  - Add gate and header tests for these cases to Phase 3.

## Major (10)

**M1. §6.1/§6.2: no design for streaming a file through the REST pipeline.**
- **Problem.** `ApiResponse.body` is a `Vec<u8>` and `into_bytes()` copies it into a second buffer. Following the existing pattern for a 2 GB document means about 4 GB of RAM, which breaks the "flat memory for any size" claim. Connection permits (64) are held until the handler ends. Only SSE has a write timeout (`SSE_WRITE_TIMEOUT`), so a stalled reader holds a permit and a file handle forever. `Connection: close` plus no Range means Safari cannot play video.
- **Why it matters.** Large files either exhaust memory or are not servable. Slow readers can take the REST listener down.
- **Direction.** Specify a streaming response path like the SSE bypass (headers, then a chunked file copy). Give it its own semaphore and a per-write timeout. Cap concurrent file streams. Grant the bearer exemption inside that handler, not in the shared `reject_from_head`.

**M2. §5.2 and the §2 row on the 2 GiB limit: no deadline or stall detection on a single serial worker, and the 2 GiB "fact" is misapplied.**
- **Problem.**
  - `src/bridge.rs:2977` uses `UreqHttpClient::new()` with the default ureq agent. ureq 3.3.0's default `Timeouts` are all `None`, including `recv_body`, and ureq offers only total timeouts, no idle-read timeout.
  - The wa-rs streaming path is explicitly "unbounded — the caller owns the sink". The 2 GiB cap applies only to `execute()` and the buffered fallback.
  - One stalled CDN connection therefore wedges the only worker until restart, and status stays `downloading` forever.
  - A 2 GB file also blocks every small request behind it. There is no cancel call.
- **Direction.**
  - Wrap the writer in a counting writer that records `bytes_written` and `last_progress_at`.
  - Run a progress-based watchdog that fails the job and moves the worker on. The blocked thread cannot be cancelled and dies with its socket, so bound the number of detached threads.
  - Expose the byte count in `status` (cheap, and more useful than a percentage).
  - Add a `media-cancel`, or at least a small-file lane.

**M3. §5.2 step 2: the disk guard trusts the sender's `file_length`, is a pre-flight check only, and shares a volume with the Signal store.**
- **Problem.** `file_length` is sender-supplied and stored as-is. wa-rs does not enforce it (`decrypt_stream_to_writer` has no cap). A sender who understates it passes the check, and nothing bounds the write. `media/` sits beside `whatsapp.db`, so ENOSPC hits SQLite and the Signal key store. The 100 MiB reserve is thin for that, and there is no total quota.
- **Why it matters.** A full disk risks message loss and session-state damage, not just a failed download.
- **Direction.**
  - Have the counting writer abort at claimed length plus one block of slack.
  - Re-check free space periodically while writing, against a configurable absolute reserve.
  - Compare the final size to `file_length`.
  - Consider a plain "refuse above N GB total" cap. That is not the eviction scheme §9 rejected.

**M4. §5.2 step 5, §5.3, §11: the re-upload path is probably the main path for history media, yet it is unverified, and it serialises the worker.**
- **Problem.**
  - ADR 0005 says link TTL is "hours to days", so most pairing-package media is likely expired. The design treats re-upload as a recovery edge case.
  - §5.3 flags the `participant` question only for LID groups. `persist_history_messages` (`src/bridge.rs` ~5171) stores the phone JID as `chat_jid` even when the phone's key used a LID, and the raw `remote_jid` is not kept. The `rmr jid=` attribute could mismatch for every LID chat, DMs included.
  - wa-rs fixes `MEDIA_RETRY_TIMEOUT` at 30 s (`media_reupload.rs`), so the design's "60 s" is unreachable.
  - The pacer wait (30 s) happens inside the single worker. Each expired item costs up to 30 s idle plus up to 30 s waiting, and non-expired downloads queue behind it.
  - Verification is deferred to Phase 2 with fake-only exit criteria, then to Phase 7.
- **Direction.** Run a live spike before building the engine. Define what happens if addressing cannot be made to work: expired becomes a terminal state. Give re-upload its own lane or timer so the download worker keeps moving.

**M5. §5.1/§5.4: no terminal states, negative caching, or breaker.**
- **Problem.** `failed` lives in memory and `get` on `failed` re-queues. An agent polling `get` re-sends a phone-upload request every 30 s forever, even when the phone answered `NotFound` (`MediaRetryResult::NotFound` is permanent in practice). If the phone is offline, every queued item burns a 30 s timeout. Unattended, automated receipts to the owner's phone are the pattern ADR 0022 calls ban-relevant. The 30 s default has no evidence behind it. §7 says "a scoped `WHATSRUST_DANGEROUSLY_ALLOW_*` flag" but never names one.
- **Direction.**
  - Split failure reasons into retryable and terminal.
  - Remember `NotFound` per message for a TTL.
  - Add a circuit breaker after N consecutive re-upload timeouts, similar to the existing `SemanticCooldown` and embed-drain breaker.
  - Cap re-upload requests per hour.
  - Name the override flag.

**M6. §4.4, §5.1, ADR 0050 point 6: "no schema change" is false for two requirements.**
- **Document names.** §4.4 keeps the original document file name. `MediaRefRow` has no name column. The name exists only inside the display label `[doc: NAME SIZE] caption` in `body_text`, which is not reliably parseable, can change on edit, and is unsanitised. Existing rows cannot be backfilled.
- **Newsletter media.** §4.2 lists `newsletter_<id>/`, but the media is plaintext. `DownloadParams` has no `static_url` and requires a `file_sha256` that `media_refs` doesn't store. `is_encrypted()` is false when `media_key` is None, so the download fails.
- **Incomplete refs.** A ref missing `direct_path` or `file_enc_sha256` fails inside wa-rs with a "Missing …" error, which the design classes as `internal`.
- **Direction.** Decide explicitly between a v9 migration (`file_name`, `file_sha256`, maybe `static_url`, populated for new rows only) and dropping the name suffix and newsletter support. Add an up-front completeness check with its own reason code.

**M7. §3 and ADR 0050 point 7: `no_media` conflates "not media" with "media but no reference", and large parts of the stored population have no reference.**
- **Problem.**
  - All live media stored before this change has no ref. The test `test_a_media_message_that_arrived_live_keeps_its_row_and_gets_no_media_ref` pins that behaviour.
  - `persist_history_messages` writes a ref only when the message insert returns `Ok(true)`, so a re-fetch (`duplicates`) never repairs the gap.
  - History rows stored before the 2026-10-22 amendment are in the same state.
  - Live media over 50 MiB returns `Unhandled`, and only the `Content` branch inserts a row, so no message row exists at all. A failed live download is also stored nowhere.
  - These are the largest files, which is exactly the on-demand use case. "Changes to live ingest beyond writing the ref row" being out of scope hides this.
- **Direction.**
  - Add a distinct `no_reference` state for media-kind messages without a ref.
  - Write the ref for duplicate rows too. `INSERT OR IGNORE` is already idempotent.
  - Decide explicitly whether over-limit or failed live media should be stored as a row plus ref, without bytes, instead of being dropped.

**M8. §4.5 and ADR 0050 point 8: the delete-marking is racy and has holes, and it cuts against ADR 0012.**
- **Hook.** Both deletes run in a fire-and-forget `tokio::spawn` in the event handler (`src/bridge.rs:3674-3696`). `delete_inbound_*` removes `media_refs` and `messages` in one transaction, so `hydrated_path` must be read first, which the design doesn't say. A crash between commit and rename leaves an unmarked file and no record of it.
- **Download race.** A download that finishes after the delete does `UPDATE hydrated_path` on 0 rows, silently. The file lands under its final name, unmarked, for a deleted message. Queued ids for that chat are not cancelled, and `.part` files are not touched.
- **Smaller gaps.** `.deleted.deleted` idempotency is unspecified. Step 3 ("refresh the name file") can erase the `deleted:` line.
- **Policy.** ADR 0012 rests on "explicit user control over deletion (privacy, compliance)". After this change, delete-for-me, delete-chat and sender revoke leave the bytes on disk. The only reclaim is `clear-data-with-media` (all of it) or by hand. The owner chose keep-and-mark, so this is not disputed. ADR 0050 should still say it narrows 0012 for media.
- **Direction.** The worker re-checks the message and ref after a download and marks or removes accordingly. A startup reconciliation scan parses message ids from file names against the DB. Add a selective purge: a documented `find … '*.deleted.*'` at minimum.

**M9. §4.4: sender-chosen names and extensions land on disk unfiltered.**
- **Problem.**
  - Documents keep "its own extension" from the sender, so `.command`, `.terminal`, `.jar`, `.webloc` and similar can be written under a normal-looking name. A daemon-written file carries no quarantine attribute on macOS, so a double-click runs it without a Gatekeeper prompt.
  - §4.3 keeps "letters of any script" and strips only control characters. Bidi overrides (U+202E and other Cf characters) survive and can disguise an extension.
  - `audio/ogg; codecs=opus` needs parameter stripping before the extension map.
- **Direction.** Derive the extension only from an allowlisted MIME map. Neutralise unknown or executable extensions, for example `.bin` plus the original name kept as metadata. Strip Cf and bidi characters. Consider setting the quarantine xattr on macOS.

**M10. §1, §6.2, §9: the largest-risk component (token store, bearer exemption, streaming file server) has no stated beneficiary, and "return a local path" was not considered.**
- **Problem.** The design says the URL "works for MCP clients on the same machine", but a same-machine client can read a mode-600 file by path. The repo already treats loopback-local file access as acceptable (`loopback_file_access`).
- **Remote MCP clients.** The shared registry also serves the MCP HTTP listener, which is built for remote use (`deploy/proxy`). A remote client would get a loopback URL for the server's machine, which is useless and possibly confusing.
- **Direction.**
  - Name the consumer that needs HTTP rather than a path.
  - If there is none, ship the path-only version first. It removes B1, M1 and most of the token-store surface.
  - Otherwise, restrict the URL to loopback callers and return `url_unavailable` (or a path) for remote MCP callers.

## Minor (11)

1. **§5.2 step 3: group metadata fetch inside the worker.** Failure handling is unspecified (left group, group gone). It must be non-fatal with a fallback name. It is also an unpaced network call. The name file writes the full current roster of phone numbers next to files the user may share.
2. **`.gitignore`.** It has `*.db` but not `media/`. The repo root is already used as a data dir in dev (untracked `whatsapp.db.pre-migration-*.bak` files exist, and `*.bak` is not ignored). `db_path` is `PathBuf::from("whatsapp.db")`, relative to cwd. Add `media/` to `.gitignore` and resolve the media root once at startup.
3. **§6.2 token store.**
   - Does each `get` mint a new token or reuse the live one? A loop of `get` calls on a ready file can evict others' tokens under "oldest dropped". The cap value is unspecified.
   - Logging the first 6 characters leaks 36 bits for little gain.
   - `head.path` is not percent-decoded. The file-name part (Hebrew etc.) must be encoded in the URL and decoded and compared in the handler.
   - A rename to `.deleted` while a token is live leaves the token stale.
4. **`hydrated_path` should be stored relative to `media/`.** Instance directories move (deploy, profile copies, restore). Also adopt an existing final-named file of the right size as `ready`. That covers a crash after rename but before the DB update, and files kept by `clear-data`.
5. **One-way doors in §4.2/§4.4.**
   - Folder and file naming will be scripted against.
   - `status@broadcast` has many senders but uses the sender-less pattern.
   - The `lid_` to phone folder "merge" after the P2 DM-split fix is an unplanned file-move plus `hydrated_path` rewrite.
6. **§7 config.**
   - Name the override flags.
   - Burst has no ceiling.
   - A download interval floor of 500 ms (a CDN fetch, not a chat action) is arguably the over-guarding ADR 0022 warns about. The re-upload floor is the ban-relevant one.
   - There is no kill switch (`WHATSRUST_MEDIA_DOWNLOAD=off`) for a feature that can trigger phone uploads.
7. **§6.1/§6.4 wire shape.**
   - `202 + ok:false` for an in-progress state can't be distinguished from failure by `ok`. Add `retryable` and `retry_after_ms`.
   - The existing annotation classes don't fit `media_get`: `WRITE_REMOTE` sets `destructiveHint: true`. Add a class for "writes local files, idempotent, non-destructive".
   - The registry test "exactly six read tools" must be updated.
8. **§6.6 scripts.**
   - `write_backup` tars the whole instance dir (`tar -czf … -C "$DIR" .`), so `media/` goes in by default unless an exclude is added.
   - It then re-reads the whole archive to verify, and gzips incompressible media.
   - There is no free-space preflight for a multi-GB archive. A default of "Y" on a prompt about N GB is questionable.
   - `re-pair fresh` leaves old-account `media/` in place, mixing accounts if the new account differs.
9. **§5.2 queue edges.** Behaviour when the 1000-entry queue is full is unspecified. While disconnected, `get` returns `queued` indefinitely with no `waiting: disconnected` hint. A restart silently drops the queue.
10. **§5.2 step 5/6: error classification relies on wa-rs message strings.** `DownloadRequestError` is private. The 404/410 case is `anyhow!("Download media not found/expired…")` and the MAC failure is `anyhow!("MAC mismatch")`, with no typed variants on the streaming path. A wa-rs bump (roadmap item Q-WARS) could silently turn `expired` into `internal` and stop re-upload from ever firing. Isolate this in the `MediaFetcher` adapter and pin it with a test on the real strings.
11. **Plan and ADR hygiene.**
    - Put the live spike (M4) and a streaming-response prototype (M1) ahead of the engine work.
    - ADR 0050 should explicitly amend ADR 0044/AGENTS.md ("every route also checks the bearer token", the head → gate → token pipeline) for the `/media-files/` exemption.
    - ADR 0050's "pacing bounds what a loop can do" holds for rate, not for volume.

## Questions (6)

1. **Q1.** Who is the client that needs a URL instead of a path, and is it ever remote?
2. **Q2.** What share of pairing-package media is actually expired, and does the phone hold the files? Has any live `media_reupload` been run against this account, including a LID-addressed 1:1 chat?
3. **Q3.** Is newsletter or channel media in scope? If yes, which columns will carry `file_sha256` and `static_url`?
4. **Q4.** In deployed instances (`~/Library/Application Support/...`), is `media/` on the same volume as the DB and Signal store, and is that acceptable without a quota?
5. **Q5.** Does sender revoke ("delete for everyone") mark files too? Today no code deletes the row for revoke. Do REST `delete-chat` and `delete-for-me` reach the marking hook only through the phone-synced events at `bridge.rs:3674-3696`?
6. **Q6.** For a message-id collision across chats (`messages.message_id` is globally UNIQUE and inserts are `OR IGNORE`), is the global-uniqueness assumption acceptable now that the ID is also the media ID and part of a file name?

---

## Outside the requested structure

**Checked and satisfied.**
- **Streaming memory.** `decrypt_stream_to_writer` uses an 8 KB buffer plus a small tail. The MAC is verified only after all plaintext has been written, so the `.part` isolation in §5.2 is correct and necessary.
- **Shutdown.** `main.rs` calls `process::exit` without dropping the runtime, so a blocking download never delays shutdown. The startup `.part` cleanup covers the leftover.
- **Cross-site POST.** `get` as a JSON-only POST is consistent with ADR 0044.
- **Host gate.** It also covers `/media-files/`, so DNS rebinding is blocked.
- **Dependencies.** `fs2` and `rand` are present. The README counts add up (59 to 62 endpoints, 34 to 36 tools, 50 to 52 commands).
- **Token strength.** 256 bits from the OS RNG is fine.

**One-way doors (scrutinise before shipping).** The folder and file naming, the `.deleted` convention, the 202 + `ok:false` wire shape and tool names (golden-pinned), and the bearer-exemption pattern.

**Assumptions.**
- The ureq version in the lockfile is the 3.3.0 read.
- The WhatsApp figures in §2 (2 GB documents, link TTL) are the author's unofficial numbers.
- Browsers send no `Origin` on same-origin GETs.
- FEATURES.md and the roadmap were not read.

**Verdict: Needs rework (revise the design document; the approach itself stands). Blocking 1, Major 10, Minor 11, Questions 6.**
