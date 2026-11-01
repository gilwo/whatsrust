# Cold design review: on-demand media download (ADR 0050 / design v2) — follow-up

**Date:** 2026-10-30 · **Review:** follow-up (v2) of [the 2026-10-29 review](2026-10-29-on-demand-media-download.md) · **Reviewer:** cold, independent `design-reviewer` subagent (it did not see the prior review)
**Artifact:** `docs/plans/2026-10-29-on-demand-media-download-design.md` (v2, cited as §n), `docs/adr/0050-on-demand-media-download.md` (Proposed v2), and the 2026-10-29 amendment (revised 2026-10-30) in `docs/adr/0005-lazy-media-hydration-with-persistent-refs.md`
**Verdict:** **Approve with changes.** The direction is sound. Two defects are Blocking and must be fixed in the document before Phase 2 (schema) and Phase 4 (engine). Neither needs a new architecture. Blocking 2, Major 7, Minor 9, Questions 7.

The coordinator's reconciliation against the v1 findings is at the end ("Reconciliation against v1").

---

I did not read `.env`, `runs/`, `vendor/` or any `whatsapp.db*`. wa-rs claims are checked against checkout `9e8c70e` and ureq 3.3.0 only.

## Steelman

The design answers the stated problem well. Bytes stay out of the DB. A single-flight background download streams through wa-rs in about 40 KB of memory. A short-lived capability URL on a separate listener lets a remote agent fetch the file without sending a custom header. A re-upload request to the owner's phone is treated as a main path, not an edge case. Several choices are good:

- `ready` is derived from `hydrated_path` plus a file check, so there is no state column to drift.
- Downloads go to `.part` and are renamed after verification.
- Download and re-upload run in separate lanes.
- The file extension comes from a MIME allowlist, and file lookup is by token. The URL path is only compared, never used to open a file, so path traversal is moot. Say that in the design.
- Responses use `attachment` plus a sandbox CSP.
- `get` is a POST because it can contact WhatsApp.

Facts I confirmed in the code:
- History rows are keyed by the phone JID (`bridge.rs:5171`).
- Local deletes come only from the two event handlers (`bridge.rs:3674-3696`).
- The `ApiResponse` buffering and the 40 s tool budget are as described.

## Blocking

### B1. Startup reconciliation treats "message not in the DB" as "message deleted"

**Problem.** §5.5 tags every file under `media/` whose message ID is absent from the DB as `.deleted` at startup. Several documented flows leave the DB without rows while keeping files:
- `clear-data` keeps `media/` (§10.1; `run-profile.sh:1172-1226` deletes the rows).
- `deploy.sh re-pair` fresh mode moves the DB aside and keeps `media/` (§10.1; `deploy.sh:227-228`).
- A restore of a pre-migration or pre-clear backup, or `--rollback`, gives the same state.

On the next start the whole library is renamed `*.deleted.*`. The pairing history that re-creates the same IDs arrives after reconciliation has run.

**Why it matters.**
- §5.4 says an existing final file is "adopted" (it names "files kept by `clear-data`"). Adoption looks for the untagged name, so it no longer finds the tagged files. The result is a re-download and doubled disk use, bounded only by the 20 GB reserve.
- The documented purge `find media -name '*.deleted.*' -delete` (§5.5, HOWTO) now deletes files of messages that were never deleted. This is data loss.
- The chat-delete path tags every file in the folder, and the folder comes from the event JID. `bridge.rs:3675` passes the raw `update.jid`, while rows live under resolved JIDs. The folder and the deleted rows can disagree.

**Direction.**
- Never infer deletion from absence. Tag only from an observed delete, and derive the file set from the rows actually deleted, not from the folder.
- To close the crash window between commit and rename, write the intent first (a list of paths in the `metadata` KV or a small journal) and replay it at startup. Let reconciliation only count and log orphans.
- State the rule for a message that comes back after tagging: probably un-tag and adopt.
- Add a test for the flow `clear-data`, then restart, then re-ingest.

### B2. Expiry detection rests on error strings the real HTTP client never produces

**Problem.** §2 and §6.2 step 6 route "not found / expired" to the re-upload lane. They assume wa-rs yields `Download media not found/expired` for 404/410, and the `MediaFetcher` pin test is meant to pin those values.
- ureq 3 defaults to `http_status_as_error = true` (`ureq-3.3.0 config.rs:867`, `run.rs:126-129`).
- wa-rs `build_agent()` does not override it (`ureq-client lib.rs:56-75`).
- `execute_streaming` does `req.call()?` (`lib.rs:139`), so any 4xx or 5xx becomes `Err("http status: 410")`.
- The caller maps that to `DownloadRequestError::other` (`src/download.rs:431-433`).

The 404/410 → `NotFound` and 401/403 → `Auth` branches (`download.rs:435-446`) are therefore unreachable. The media-connection refresh retry never runs either. An expired link tries every media host and ends as `network`.

Even if the status branches worked, 401/403 after the refresh yields "Download failed with status: 403". If the CDN answers 403 for expired signed URLs, that also falls to `network`. A fake fetcher cannot reveal any of this (§14: no live contact, fake fetcher).

**Why it matters.** The main path (§2: "most history media is probably expired") never reaches re-upload. Clients get retryable `network` forever, and every fake-driven test still passes. Phase 1 would probably expose it, but the document asserts the opposite and the test plan cannot catch it.

**Direction.**
- The design replaces the agent through `with_agent` anyway, so decide the status handling there. Set `http_status_as_error(false)` on the media agent, or give the fetcher a typed result of its own.
- Write an explicit table of (status, phase) → reason, including 403.
- Make Phase 1 record the raw status and error text for expired links of several ages.
- Pin the classification with a test that runs the real `UreqHttpClient` against a local server returning 403, 404, 410 and a truncated body.
- See M3 for the option of owning the transfer.

## Major

### M1. No loop guard between the two lanes, and re-upload results are mostly unmapped

**Problem.**
- §6.2 step 6 sends an expired item to re-upload. The re-upload lane puts it at the front of the download queue. If the download is "expired" again, nothing stops another round. This can happen when the CDN is slow to propagate or the link is bad.
- `MediaRetryResult` has four variants: `Success`, `GeneralError`, `NotFound`, `DecryptionError` (`wacore/src/media_retry.rs:29-36`). Errors also include `NotLoggedIn`, `InvalidRequest` (newsletters) and `Client` (`media_reupload.rs:81`). The design maps only `NotFound` and the timeout.
- `NotFound` also means "the phone cannot find the message under that key". That is an addressing error (see M2), and §6.1 remembers it for 24 h as "final".

**Why it matters.** A single item can burn the hourly re-upload budget and the phone's attention. An addressing bug looks like a permanent absence.

**Direction.**
- Add a per-item `reuploaded` flag. A second expiry fails with a distinct reason.
- Map every result and error variant to a reason.
- Record which addressing was used (raw or stored JIDs). Shorten the `not_on_phone` memory when the fallback addressing was used.

### M2. The existing population lacks all v9 fields, and "a re-fetch repairs older rows" is mostly illusory

**Problem.** Every `media_refs` row already in the DB (the pairing package) gets NULL for `remote_jid`, `participant`, `file_sha256` and `file_name` (§4). Re-upload for these rows uses the "stored JIDs" fallback (§6.2). The repair path (§4: an upsert on duplicates) only runs when the same message is ingested again. Backfill pages older than the stored anchor, so already-stored messages are not revisited. Persistence happens only for new rows (`bridge.rs:5218`).

**Why it matters.** The rows that most need re-upload, the old and expired ones, use the least-tested addressing. For LID chats, the fallback is exactly what §11 flags as possibly wrong. The design has no remedy beyond a re-pair.

**Direction.**
- Phase 1 must test the fallback addressing on old pairing-package rows, not only on fresh ones.
- Consider reconstructing the raw LID through `lid_pn_mapping`. The `phone_number` index exists (`storage.rs:300`).
- Show a `reference: degraded` flag in `status`.
- Make the upsert rule explicit (Q1).

### M3. The stall and timeout defence is incomplete, and it collides with "no size cap" and with the shared HTTP client

**Problem.**
- ureq 3.3 offers only total timeouts (`config.rs:669-748`). There is no idle-read timeout and no per-request timeout in wa-rs's `HttpRequest`.
- `with_agent(...)` replaces the one client used for all wa-rs HTTP, which `bridge.rs:2977` shows is a single client. That includes media upload and history-sync blob downloads. "Generous total timeouts" (§6.3) therefore apply to them too.
- Every retry restarts at offset 0 (`download.rs:425`), and there is no resume.
- With no size cap, any total timeout deterministically kills big files on slow links. The result is a livelock of `stalled` plus "retryable".
- Without a body timeout, a half-open socket blocks the transfer thread forever. After two such detached threads, "the lane pauses until one ends" (§6.3). That may never happen, so the feature is dead until restart, with no alarm.

**Why it matters.** Both failure directions are real, and the second is silent.

**Direction.**
- Bound a detached thread's lifetime: a per-request body timeout scaled from `file_length` at a minimum throughput, and socket keepalive if available.
- Make the lane-pause visible in `/api/status` and the logs.
- Evaluate letting `MediaFetcher` own the HTTP call. That means `DownloadUtils::prepare_download_requests` and `decrypt_stream_to_writer` (both public in wacore), its own agent, and media-connection access. Verify that media-connection access is public at this pin. This would also settle B2, M4 and M6.

### M4. The "counting writer" contract does not match how wa-rs uses writers

**Problem.** §6.3 describes an append counter that aborts on over-claim, cancel or reserve breach.
- wa-rs requires `W: Write + Seek + Send + 'static` (`download.rs:399-403`).
- It seeks to 0 at the start of every host attempt (`:425`).
- Any error from the transfer, including one raised by the writer, is logged as "Trying next host" and the loop restarts (`:228-237`).

**Why it matters.**
- Cancel, over-claim and `disk_reserve` aborts are retried N more times, each a new HTTP request.
- After a mid-stream failure on host 1, host 2 rewrites from 0. A cumulative byte counter then double-counts, and the over-claim check can fire falsely. The progress figure jumps back.
- A shorter second write leaves stale tail bytes in the file.

**Direction.**
- Make the writer position-based and truncating (`set_len` on success).
- Give it a sticky abort with an out-of-band reason, and classify from the writer's state first and the error text second.
- Verify the final size and hash.
- Add tests for host-failover and for abort-then-retry.

### M5. The media file listener is under-specified for an unauthenticated, remote-capable endpoint

**Problem.**
- §7.2 says "startup rules like the MCP listener", but the MCP rules centre on a mandatory token (`mcp_http.rs:202-209`). This listener has none.
- The MCP listener's bar (AGENTS.md:112) includes a 32-connection cap, a head deadline and a head-size cap. §7 specifies only the 8-stream cap and a 30 s per-write timeout.
- A per-write timeout is not a minimum rate. A token holder can pin all 8 slots indefinitely at a trickle.
- `ApiResponse` cannot be reused for this. It buffers, sends `Connection: close`, and has no 206 or 416 (`api.rs:143-182`). So this is a third hand-rolled HTTP server, with Range, HEAD, `If-Range` and 416 semantics unspecified.

**Why it matters.** This is the only unauthenticated, internet-reachable surface in the project.

**Direction.**
- Reuse `read_request_head` and the head deadline (`api.rs:599`).
- Write the pipeline order the way AGENTS.md does for the others.
- Add a connection cap separate from the stream cap, and a total or minimum-rate limit per stream.
- Specify the 416 and `If-Range` behaviour.
- Ask whether Range and HEAD are needed at all for an agent client (Q6).

### M6. Sender-controlled reference fields reach URL construction unvalidated

**Problem.** wacore builds `https://{host}{direct_path}?auth={media_conn.auth}&token=…` (`wacore/src/download.rs:291-294`) with no check on `direct_path`. The design persists the sender's `direct_path` (`media_refs`) and fetches it on an agent's request. Its threat model (§2, ADR) names bytes, MIME type and file name as sender-controlled but not the reference fields. By inspection, a value such as `@attacker.example/x` or `.attacker.example/x` changes the authority. The request, carrying the owner's media `auth` token, would then go to another host, including loopback or the LAN. This already exists for live ingest (`bridge.rs:4089`).

**Why it matters.** It is an SSRF and token-leak path that this design makes agent-triggerable.

**Direction.**
- Validate in the `MediaFetcher` adapter and at ingest. `direct_path` must start with a single `/`, use a restricted ASCII path charset, and contain no `@`, `\`, `#` or control characters.
- Confirm with a unit test over `prepare_download_requests`.
- Report it upstream.

### M7. Schema v9 includes columns and code paths with no reachable consumer

**Problem.**
- `should_ignore_jid` drops `@newsletter` entirely (`bridge.rs:4892`; test `:7858`). So `static_url`, the plaintext `file_sha256` branch, the `newsletter_` folder, and the plaintext `incomplete_reference` path are unreachable.
- Independently, `DownloadParams` does not override `static_url` (`download.rs:57-76`), so §2's `download_from_params_to_writer(&DownloadParams)` could not use it anyway.
- Re-upload rejects newsletters (`media_reupload.rs:81`).

**Why it matters.** Schema columns are a one-way door. The authorisation is for new `media_refs` fields, not necessarily for these.

**Direction.**
- Drop `static_url` and the newsletter paths.
- Re-justify `file_sha256`: the MAC already authenticates encrypted media, so it adds little.
- Weigh the other optional parts against the owner-facing value: name-file participants and rename logic, the macOS quarantine xattr (it only helps a local double-click), and the download pacer (the CDN is not the anti-ban surface).

## Minor

- **m1. Proxy recipe and origin claim.**
  - `nginx-mcp.conf` has default `proxy_buffering on`. Multi-GB files would spool into nginx temp storage outside the 20 GB reserve. The access log would record the token URL, which is valid for 5 minutes. Add `proxy_buffering off` and no URI logging for the location, and the Caddy equivalent.
  - §7.2's "separate port = separate origin" does not hold behind the single-host recipe (`wa.example.com` serves both `/mcp` and `/media-files/`). The defences that apply are `attachment`, the sandbox CSP, `nosniff` and header-based MCP auth. Say so.
- **m2. Delete race.** §6.2 step 5 does "re-check, then set". Use the `rows-affected` result of the `hydrated_path` UPDATE after the rename, or one transaction. The file-set derivation is covered in B1.
- **m3. Local path and third-party content.**
  - Return `path` only when `loopback_file_access` (`api.rs:1645`). The MCP runner sets it to false (`mcp_http.rs:1380`). Otherwise a remote agent gets the absolute server path.
  - The tool descriptions must warn that `file_name`, `mimetype` and file contents are third-party content (compare `mcp_registry.rs:313`).
- **m4. New MCP access class.** `AccessClass` is {Read, Write} (`mcp_registry.rs:15`). `whatsrust_fetch_history` already uses Write with non-destructive annotations (`:326-335`). A third variant touches lanes (`mcp_http.rs:1360`) and tests for no gain.
- **m5. Group-name lookup in the download lane.** `get_group_info` awaits `get_metadata` with no timeout (`bridge.rs:1261`). It is also a WhatsApp contact outside the pacing controls, and failures are not cached. Time-box it, cache the failure, and do not block the download.
- **m6. In-memory controls reset on restart.** The hourly cap, the breaker and `not_on_phone` are in memory. F10 restarts on any non-zero exit within 10 s, which is exactly when these should hold. Persist them in the existing `metadata` KV.
- **m7. Rollout.** A v8 binary refuses a v9 DB (`storage.rs:693-698`), so an engine bug found later forces `--rollback` and loses newer data. Ship Phase 2 (schema plus ingest) alone first and let it soak.
- **m8. Spec nits.**
  - The 64-character limit should be a byte limit: 64 astral characters is 256 bytes, over the 255-byte filename limit.
  - §7.3 says `inline` for `audio/*` and `video/*` but `octet-stream` for non-allowlisted types.
  - `PUBLIC_URL` defaults to `http://0.0.0.0:port` for a wildcard bind. Require it for non-loopback binds.
  - The `url_busy` cap of 256 throttles a bulk reader for no resource reason.
  - ADR decision 5 says `not_on_phone` is never retried; §6.1 forgets it after 24 h or a restart.
- **m9. Disk observability.** `/api/status` has no free bytes, reserve or media-directory size. Once the reserve is reached the feature fails permanently as `disk_reserve`, with no signal.

## Questions for the owner

- **Q1.** For a duplicate (already-stored) message, should the upsert create a `media_refs` row when none exists? That would make live-ingested media downloadable after a history fetch and contradict "live → `no_reference`". Or may it only fill existing rows?
- **Q2.** `file_length` NULL: §6.4 "counts as 0". Does the over-claim abort then fire at 64 KiB, or is the stream unbounded (reserve checks only)?
- **Q3.** With the kill switch off, are `ready` files still served and tokens still minted?
- **Q4.** Is "un-tag and adopt when a message returns to the DB" the intended semantic (B1)?
- **Q5.** Will the Phase 1 probe record the raw CDN failure (status and text) for links of several ages, and exercise the fallback addressing on old pairing rows?
- **Q6.** Does the agent's fetch tool need Range or HEAD, or will it always GET the whole file?
- **Q7.** Sender revoke removes no row, so revoked media stays gettable. Is that intended?

## Outside the requested structure

- **Alternatives not engaged.**
  - A bounded `wait_ms` on `get`, capped under the 40 s budget, would spare the agent a poll loop (compare `?sync=true` on sends).
  - HMAC-signed stateless URLs would survive restarts and need no token table.
  - Serving a token-gated file route on the MCP listener would reuse its hardening, but at the cost of ADR 0045's "POST /mcp only". Separate resource isolation is the valid reason for a separate listener (long streams cannot exhaust MCP connections); say that, not "origin".
- **Document consistency.** ADR 0050 decision 5 and design §6.1 disagree on `not_on_phone` (see m8).
- **Test plan gaps.** No crash-injection test between rename and DB update. No test for the real ureq client against a local server (B2). No listener tests for slow clients, oversized heads, malformed Range, or encoded separators in the URL name.

Files the reviewer cited: the design doc and ADR 0050; `src/bridge.rs`, `src/storage.rs`, `src/api.rs`, `src/mcp_http.rs`, `src/mcp_registry.rs`; `deploy/proxy/nginx-mcp.conf`; `scripts/deploy.sh`, `scripts/run-profile.sh`; wa-rs `9e8c70e` `src/download.rs`, `src/features/media_reupload.rs`, `wacore/src/download.rs`, `wacore/src/media_retry.rs`, `http_clients/ureq-client/src/lib.rs`; ureq 3.3.0 `src/config.rs`, `src/run.rs`.

---

## Reconciliation against v1

The coordinator checked each v1 finding against the current design text (design v2, ADR 0050 v2, the ADR 0005
amendment), not against the v2 report. The cold reviewer did not see v1, so its silence on a v1 item means
nothing. "Partly resolved → v2 Xn" means: the v2 text addresses the v1 item, but a v2 finding shows that the
fix is incomplete or rests on a wrong fact.

| v1 finding | Short title | Status | Where settled / what remains |
|---|---|---|---|
| B1 | Sender-controlled bytes and MIME served inline from the API origin | resolved | §7.2 separate listener (REST unchanged); §7.3 `attachment` by default, inline allowlist, MIME validation, `nosniff`, sandbox CSP, `no-referrer`, RFC 6266, no CR/LF in headers; §13 phase 5 tests. v2 m1: behind the single-host proxy recipe the "separate origin" claim does not hold; the header defences still apply. Correct the claim. |
| M1 | No streaming path through the REST pipeline | partly resolved → v2 M5 | §7.4 chunked copy, 30 s per-write timeout, 8-stream cap, single Range; the separate listener removes the REST coupling and the bearer exemption. Remains: a connection cap apart from the stream cap, a head deadline and head-size cap, a minimum rate per stream, the 206/416/`If-Range`/HEAD rules, the pipeline order. |
| M2 | No deadline or stall detection; 2 GiB fact misapplied | partly resolved → v2 M3, M4 | §2 drops the 2 GiB claim; §6.3 counting writer, 60 s watchdog, at most 2 detached threads, cancel; §6.1 byte counts. Remains: no body timeout, so a detached thread can live forever and the lane then pauses silently; `with_agent` timeouts also hit uploads and history blobs; the counter must survive wa-rs's seek-to-0 host failover. One download worker still lets a big file delay small ones (cancel exists). |
| M3 | Disk guard trusts `file_length`, pre-flight only | partly resolved → v2 M4, Q2 | §6.3 over-claim abort (claim + 64 KiB); §6.4 20 GB reserve, re-checked every 64 MiB; §6.2 size check. No total quota (owner decision). Remains: a cumulative counter fires falsely after a host failover, and wa-rs retries the writer's abort; a NULL `file_length` makes the over-claim rule undefined (v2 Q2). |
| M4 | Re-upload is the main path, unverified, and serialises the worker | partly resolved → v2 B2, M2 | §2 calls re-upload a main path; §2 states the fixed 30 s; §4 keeps the raw `remote_jid`/`participant`; §6.2 separate re-upload lane; §13 phase 1 live test before the engine. Remains: the live test is still to run; no rule says what happens if neither addressing works; the expiry trigger itself is unreachable at this pin (v2 B2); existing rows have no raw JIDs (v2 M2). |
| M5 | No terminal states, negative caching or breaker | partly resolved → v2 M1, m6 | §6.1 reason table (retryable or final), `not_on_phone` for 24 h; §6.5 breaker and hourly cap; §10.2 names `WHATSRUST_DANGEROUSLY_ALLOW_FAST_MEDIA_REUPLOAD`. Remains: no loop guard for "expired again after a re-upload"; `GeneralError`, `DecryptionError`, `NotLoggedIn`, `InvalidRequest` and `Client` are unmapped; `NotFound` can be an addressing error; the controls reset on a restart; ADR 0050 decision 5 and §6.1 disagree on `not_on_phone`. |
| M6 | "No schema change" was false (names, newsletter, incomplete refs) | partly resolved → v2 M7 | §4 schema v9 (`file_name`, `file_sha256`, `static_url`); §3 `incomplete_reference`. Remains: newsletter messages are never stored, and `DownloadParams` cannot use `static_url`, so `static_url`, the plaintext `file_sha256` path and the `newsletter_` folder have no consumer. |
| M7 | `no_media` conflates; many rows have no reference | partly resolved → v2 M2, Q1 | §3 `no_reference` state; §1/§9 live media out of scope (owner decision); §4 upsert for duplicates. Remains: the re-fetch repair rarely runs (backfill pages older than the anchor); whether the upsert may create a missing `media_refs` row is undecided (v2 Q1). |
| M8 | Delete marking racy, with holes; ADR 0012 | regressed → v2 B1, m2 | §5.5 order (read paths, commit, rename), queued items dropped, `.part` removed, idempotent tags, the `deleted:` line kept on a rename (§5.3); ADR 0050 decision 9 narrows ADR 0012; the `find` purge. The added startup reconciliation treats "not in the DB" as "deleted": after `clear-data`, a fresh re-pair or a restore it tags the whole library, and the documented purge then deletes files of live messages. Also: "re-check, then set" still races (v2 m2); the chat-delete folder comes from the raw event JID. |
| M9 | Sender names and extensions unfiltered | resolved | §5.4 extension only from the MIME allowlist (parameters stripped, else `bin`), quarantine attribute; §5.3 NFC plus Cc/Cf removal. v2 m8 nit: 64 characters should be a byte limit. |
| M10 | URL has no stated beneficiary; path not considered | resolved | §7.1 and §9: a remote agent on another computer; §12 "A local path only" considered; the path is also returned. v2 m3: return `path` only to loopback callers. |
| m1 | Group metadata fetch in the worker | partly resolved → v2 m5 | §5.3 non-fatal lookup, fallback name, 5-minute cache. Remains: no timeout, outside the pacing controls, failures not cached. The roster in the name file is an owner decision. |
| m2 | `.gitignore` and the media root | resolved | §5.1: `media/` in `.gitignore`; the root resolved once at startup. |
| m3 | Token store details | resolved | §7.5 one live token per file, cap 256 with `url_busy`, never logged, percent-encoded file name; §7.4 404 after a `.deleted` tag. v2 m8: the 256 cap may throttle a bulk reader. |
| m4 | Relative `hydrated_path`; adopt a final file | resolved (interacts with v2 B1) | §5.1 relative path; §5.4 adoption. The reconciliation of §5.5 tags kept files before adoption can find them (v2 B1). |
| m5 | One-way doors in naming | resolved | §2 `status@broadcast` is never stored (moot); §5.2 assigns the `lid_` merge to the P2 DM split. The naming stays a one-way door; the doc does not mark it as one. |
| m6 | Config: flags, burst ceiling, floors, kill switch | resolved | §10.2 named override flag, burst 1–20, download floor 200 ms, `WHATSRUST_MEDIA_DOWNLOAD=off`. v2 Q3: what the kill switch does to `ready` files and tokens is not stated. |
| m7 | Wire shape and annotation class | resolved (over-applied → v2 m4) | §8.1 `retryable`, `retry_after_ms`; §8.3 non-destructive annotations, registry test updated. v1 asked for an annotation set; the design adds a new access class, which v2 m4 calls unneeded (`Write` plus the annotations is enough). |
| m8 | Scripts: backup, free space, re-pair | resolved | §10.1 media choice in the uninstall backup, free-space check, no gzip for media, the HOWTO note for re-pair. The `[Y/n]` default is the owner's decision. |
| m9 | Queue edges | resolved | §6.2 `queue_full`, `waiting: disconnected`; §6.1 the queue is lost on restart (stated). |
| m10 | Error classification relies on wa-rs strings | still-present (escalated → v2 B2) | §6.2 isolates it in `MediaFetcher` with a pin test, but §2 still names "Download media not found/expired" as the expiry error. At this pin the ureq client returns `http status: <n>` for every 4xx/5xx, so the strings to pin are wrong. The pin test must run the real `UreqHttpClient` against a local server. |
| m11 | Plan and ADR hygiene | resolved / superseded | §13 phase 1 runs the live test first; §6.5 separates rate from volume. The bearer-exemption amendment is superseded: the separate listener leaves the REST pipeline unchanged (ADR 0050 decision 7). No streaming prototype comes before the engine; the risk is lower now that the listener is separate. |
| Q1 | Who needs a URL; remote? | resolved | §7.1, §9: a remote agent. |
| Q2 | Expired share; has re-upload run live (incl. a LID 1:1)? | still-present (scheduled: phase 1) | §11 item 1 and §13 phase 1; the probe exists, and the owner runs it now. v2 Q5 widens it: record the raw CDN status and text, and test the fallback addressing on old pairing rows. |
| Q3 | Newsletter media in scope? | superseded → v2 M7 | The design answered "yes" (§4, §5.2), but newsletter messages are never stored (`should_ignore_jid`), so the answer has no consumer. |
| Q4 | Same volume as the DB; quota? | resolved | §6.4: same volume, protected by the fixed reserve; no quota (owner decision). |
| Q5 | Sender revoke; do REST deletes reach the hook? | resolved | §5.5: revoke deletes no row and marks nothing; §2: REST and CLI deletes come back as phone-synced events. v2 Q7 asks whether revoked media should stay gettable. |
| Q6 | Global message-id uniqueness | resolved | §3: accepted as negligible; the chat folder is also in the path. |
| v1 "outside": one-way doors | Naming, `.deleted`, wire shape, bearer exemption | partly superseded | The bearer exemption is gone (separate listener). The naming, the `.deleted` convention and the 202 + `ok:false` shape remain one-way doors; v2 B1 shows the `.deleted` convention needs the strongest care. |

Counts (29 items): resolved 16 (B1, M9, M10, m2–m9 incl. m4 and m7 with notes, m11, Q1, Q4–Q6); partly resolved 9
(M1–M7, m1, and the "outside" item); regressed 1 (M8); still-present 2 (m10 escalated, Q2 scheduled);
superseded 1 (Q3); indeterminate 0.

### New in v2 (no v1 ancestor)

- **M6** — sender-controlled `direct_path` reaches URL construction unchecked (SSRF and a leak of the media
  `auth` token); already true for live ingest.
- **m7** — rollout: a v8 binary refuses a v9 DB; ship phase 2 alone first and let it soak.
- **m9** — disk observability: no free bytes, reserve or media size in `/api/status`.
- **B2** in its blocking form (its ancestor v1 m10 was a fragility note, not a wrong fact).
- The "outside" notes: a bounded `wait_ms` on `get`, HMAC-signed stateless URLs, the real reason for a separate
  listener (resource isolation, not origin), and the test-plan gaps (crash injection, the real ureq client,
  slow-client and malformed-Range listener tests).

### Trajectory

The design converges. The verdict rose from "Needs rework" to "Approve with changes", and the v1 blocking issue
(same-origin file serving) is closed by a structural change, not a patch. Both new blocking issues are local to
the engine: one comes from a wrong library fact (B2), and one from a v1 fix that went too far (B1, the startup
reconciliation). Neither needs a new architecture. The risk is growth: each review adds controls (lanes,
breakers, caps, tags, reconciliation), and v2 M7 asks to remove parts with no consumer. The next revision should
remove as much as it adds: drop the newsletter paths, `static_url` and the new access class, and replace
"absence means deleted" with tags from observed deletes only.
