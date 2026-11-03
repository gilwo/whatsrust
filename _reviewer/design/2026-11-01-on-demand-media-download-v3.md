# Cold design review: on-demand media download (design v3, ADR 0050) — follow-up

**Date:** 2026-11-01 · **Review:** follow-up (v3) of [the 2026-10-30 review](2026-10-30-on-demand-media-download-v2.md) (which reconciles [v1](2026-10-29-on-demand-media-download.md)) · **Reviewer:** cold, independent `design-reviewer` subagent (it did not see the prior reviews)
**Artifact:** `docs/plans/2026-10-29-on-demand-media-download-design.md` (v3), `docs/adr/0050-on-demand-media-download.md` (Proposed v3), the ADR 0005 amendment (revised 2026-11-01); committed in e5062f7
**Verdict:** **Approve with changes.** Blocking 0, Major 6, Minor 11, Questions 6.

The coordinator's reconciliation against the v2 findings is at the end ("Reconciliation against v2").

---

Paths are relative to `/Users/gila/prv/whatsrust` unless absolute. I checked the design against the code at HEAD, the pinned wa-rs `9e8c70e` and ureq 3.3.0. I did not use the `af2e018` upstream tree. I took the owner's constraints as given and only point at their consequences.

The design is careful and its facts about the pin hold up. I checked these and found them accurate:
- ureq's `http_status_as_error` defaults to true (`config.rs:867`), so wa-rs never sees a 404/410.
- `decrypt_stream_to_writer` checks the MAC only after writing plaintext (`wacore/src/download.rs:350-467`).
- wa-rs's `download_to_writer` seeks to 0 and retries on every host.
- `build_media_retry_receipt` and `parse_media_retry_notification` are public.
- The staged migration supports additive columns.
- MCP maps a status of 400 or more to `isError` (`src/mcp_registry.rs:477`).

The findings below are mostly interactions the prose does not close.

## Steelman

- The design takes the least trusting stance toward sender data that this codebase has taken so far: an extension allowlist, a `direct_path` check, hardened response headers, `.part` then rename, and quarantine.
- It keeps the wa-rs coupling behind two adapters, so the later upgrade stays contained.
- It replaced a library call with its own transfer and a status table once the pin's flaws were measured, rather than assumed.
- The re-upload addressing came from a live test, not from reading the library. The probe is kept as evidence.
- The 202-and-poll shape fits the 40 s MCP budget, and `get` is a POST because of the request gate.
- The separate token-only file listener isolates long streams from REST and MCP.
- State is derived rather than stored where it can be, so v9 stays small.

## Blocking

None. M1 and M2 come closest and should be fixed before phase 3, because both are cheap now and awkward after the pure functions and tests exist.

## Major

**M1. The `.deleted` tag marker is in the sender-controlled part of the file name (§5.3, §5.4, §5.5, §10.1).**
- **Problem:** §5.3 removes only `/ \ : * ? " < > |` and white space. A document name keeps its dots, and only its last extension is stripped. A sender can therefore produce `…_<id>_report.deleted.docx`.
- §5.5 treats a `.deleted.` substring as "tagged" in four places: tagging is idempotent, `ready` means "untagged", the serve path 404s a tagged file, and adoption/un-tag keys on it. The documented purge is `find media -name '*.deleted.*' -delete`.
- **Why it matters:**
  - A live file with such a name is never `ready` and is 404 on the listener.
  - A real delete or revoke never tags it, because "a `.deleted.` file is never tagged twice".
  - The documented purge deletes it. If the link is expired and the phone no longer has the file, that loss is permanent.
  - A benign name such as `invoice.deleted.pdf` triggers all of this too.
- **Direction:**
  - Keep the marker outside anything the sender influences. Either strip or escape the marker token in sanitised names (and test it), or tag structurally. Structural options are a reserved subdirectory such as `<chat>/_deleted/`, or a reserved prefix the sanitiser cannot emit.
  - Make "is tagged" a path or DB property, not a substring test. Make the purge command operate on that property.

**M2. `revoked` trusts any revoke row, and finds it by parsing a label (§3, §5.5, §6.1).**
- **Problem:** A revoke row carries its target only as `body_text` = `[deleted <id>]` (`src/bridge.rs:527`). Extraction takes just `key.id` from the protocol message (`src/bridge.rs:4387-4395`). The revoker is the row's `sender_jid`.
- The design makes "a revoke row exists" a persisted, final state. It blocks download, tags the file and makes it 404. Nothing checks that the revoker authored the target (or is a group admin).
- **Why it matters (authorization):** Any chat participant who knows a message ID can send a forged revoke. Every group member sees IDs. That permanently hides someone else's media from the agent, or the owner's own outgoing media in a DM. Today a revoke row is inert; this design gives it power.
- **Why it matters (cost):**
  - The lookup is a string equality on `body_text`. §12 rejects label parsing for document names but depends on it here.
  - The only `messages` indexes are `(chat_jid, timestamp)`, `message_id` and `embed_status` (`src/storage.rs:304-309`).
  - So each `get` or `status` poll range-scans the chat. It does so under the single `Mutex<Connection>` that also serves the Signal store and the send queue (`src/storage.rs:939-940`, `1147`).
  - A polling agent on a large group can stall the bridge.
- **Direction:**
  - Define a valid revoke. At minimum, the revoker's `sender_jid` equals the target's, or the target's `from_me` matches the revoker. Apply it when deriving `revoked` and when tagging.
  - Resolve the revoke-to-target link once at ingest, or add a partial index on revoke rows. `src/storage.rs:306` notes that a non-versioned index is not a schema bump.
  - Decide whether a mistaken `revoked` can ever be cleared.

**M3. "Every transfer thread has a bounded life" depends on a sender-controlled number (§6.2 step 4, §6.3).**
- **Problem:** ureq 3.3 has no idle timeout (`timeout_recv_body` is a total measured from the response head: `config.rs:742-748`, `timings.rs:55`). A blocked read cannot be interrupted. The design bounds it with `60 s + file_length ÷ 64 KiB/s`, or 4 h when the length is absent.
- `file_length` is sender-controlled and unclamped. A 2 GB file gives about 8.7 h. A claimed 1 TB gives about six months. After two lingering threads the lane stops until one ends.
- I checked ureq's `Instant + Duration` for overflow and it is safe on 64-bit Unix. The problem is the length of the bound, not a panic.
- **Why it matters:** One or two silent mid-body drops (a NAT timeout, a sleeping laptop, a Wi-Fi roam) can stop all downloads for hours. The only signal is a status field and one WARN line. Also, every stall retry restarts a multi-GB file from zero, because the writer truncates per host and there is no resume.
- **Direction:**
  - Make the abort able to unblock the read: shut the socket down on abort (a connector hook), or use an async fetch with a per-chunk timeout feeding the blocking decryptor through a bounded channel.
  - If you keep the current approach, clamp the `file_length` term (for example to 1–2 h). Also raise or remove the "2 threads" pause for the case where the abort has already fired.

**M4. The expired-link recovery is the main path, but it is untested for the data that exists, and the plan does not gate on it (§2 row 94, §4, §6.2, §11.1, §13).**
- **Problem:** The live test succeeded 3 of 3, on fresh messages with LID-form key JIDs. Every row already in the DB has NULL `remote_jid` and `participant`. Its address comes from `lid_pn_mapping`, or from the stored phone JID.
- ADR 0048 says contacts' mappings are normally unknown at pairing. The `stored` form is a phone JID in a LID-addressed world, and the design hedges with a 1 h `not_on_phone`.
- The duplicate fill will not heal old rows. Backfill pages strictly backward from the oldest stored message (`src/backfill.rs:77-91`). Already-stored rows come back only on a re-pair or a retried page.
- §11.1 leaves this to the owner and phase 9 tests one expired file. Phase 2 may merge to `main` before any of it.
- **Why it matters:** If `mapped` or `stored` addressing does not work, the feature for old data degenerates to `not_on_phone` and hourly retries. Each wrong-address try also spends a re-upload slot and a request to the phone.
- **Direction:**
  - Make §11.1 an exit gate for phase 4. Run the probe over rows of each class: 1:1 mapped, group member mapped, stored-only, own message. Record the matrix in ADR 0050.
  - If stored-only addressing fails, return a distinct reason (for example `address_unresolved`) without contacting the phone.

**M5. The re-upload budget counts requests, but the cost to the owner's phone is bytes (§6.5, §10.2).**
- **Problem:** Each request makes the phone upload the whole file. The pacing, the hourly cap and the breaker count requests, not bytes. There is no byte ceiling by owner decision. `file_length` is known before sending but is not in any control.
- **Why it matters:** At 20 per hour, a stream of requests for old videos can mean tens of GB of phone upload per hour, possibly on cellular. The caller is an AI agent that reads third-party text. A prompt injection can drive `get` over many old files. `status` does not show the cost before the request is sent.
- **Direction:** This does not reopen the download no-cap decision. Add a separate bytes-per-hour budget for re-upload requests only, with a configurable default. When it is exhausted, return `reupload_paused` with `retry_after_ms`. Show `file_length` in `waiting_reupload`.

**M6. The duplicate-fills-only rule leaves most existing history permanently `no_reference`, and ref creation is not atomic (§2 row 80, §4; `src/bridge.rs:5248-5270`, `src/storage.rs:1513-1527`, ADR 0048 point 6).**
- **Problem:** A history message gets no reference in three cases:
  - It was stored before refs existed (before 2026-10-22). The earlier sink downloaded eagerly and wrote no ref.
  - Its `insert_media_ref` failed. The failure is only counted as `media_refs_failed`.
  - The process stopped between the message insert and the ref insert. These are two separate statements.
- By the owner's rule a re-fetch can never repair any of these. Together with "live is out of scope", the only downloadable media in the DB is media ingested from history after 2026-10-22. All other media answers `no_reference`, which is the same answer a live message gets.
- **Direction, inside the owner's rule:**
  - The stated reason for the rule is that live rows stay ref-less. `messages.source` already tells `live` from `backfill` and `bootstrap`. Create the missing ref on a duplicate only when the existing row's source is not `live`.
  - Whatever the owner decides, write the message and its ref in one transaction.
  - Make `no_reference` carry `source` and the stored date, so the agent and the owner can tell the cases apart.
  - Document in HOWTO that `clear-data` plus a re-fetch is the way to repopulate.

## Minor

**m1. The intent journal is a single metadata key, but deletes run as independent spawned tasks (§5.5; `src/bridge.rs:3694-3712`).**
- Each delete event runs in its own `tokio::spawn`. A chat delete can arrive together with many message deletes. "Writes the list under `media_tag_intent`, then clears the key" lets one task clear another's pending list.
- Result: a file of a deleted message stays untagged. The startup orphan count only counts it, and `find '*.deleted.*'` never sees it.
- **Direction:** Specify that the journal is append-only per task and each task removes only its own entries. A small table would do this. Note that `media_reupload_state` (up to 10 000 entries) and large intents are rewritten whole under the shared DB mutex.

**m2. Error classification relies on untyped `anyhow` text (§6.2 table).**
- `decrypt_stream_to_writer` reports truncation as "Invalid final media size", a bad MAC as "MAC mismatch", and I/O errors as wrapped `io::Error` (`wacore/src/download.rs:431-461`). The design maps these to `integrity`, with no retry.
- A short stream that ends cleanly, or a size mismatch after the MAC verified, becomes a final failure for a transient or harmless condition.
- **Direction:**
  - Compare bytes received with `Content-Length` before blaming integrity.
  - Retry once on the next host before declaring `integrity`.
  - Downcast to `io::Error` and the ureq error for the timeout and stall cases.
  - Accept a MAC-verified file whose size differs from `file_length`, and log the mismatch (see Q3).

**m3. The expired path has unpaced upstream calls and a blind spot in the breaker (§6.2 table, §6.5).**
- Each 401/403 item calls `refresh_media_conn(true)` and then makes a second pass over every host. The design itself says the CDN may answer 403 for an expired link. The download lane has no pacing by owner decision, so a batch of expired items produces a stream of forced IQs.
- The breaker counts only consecutive timeouts. A systemic fault where each re-upload "succeeds" but the new path also fails (`expired_after_reupload`) spends one phone upload per item and never trips it.
- The new agent also uses ureq's default of 10 redirects (`config.rs:874`), so a redirect off the named hosts would be followed.
- **Direction:**
  - Rate-limit forced refreshes, for example one per 60 s, and reuse the result.
  - Count `expired_after_reupload` toward the breaker.
  - Set `max_redirects(0)` and treat 3xx as "other status".

**m4. HTTP and MCP mapping for `failed` reasons is unspecified (§8.1, §8.3).**
- §8.1 maps `not_ready` to 202, finals to 404/409, and `disabled` to 503. It does not say what `get` returns for `failed` with `network`, `stalled`, `disk_reserve`, `phone_offline`, `reupload_paused`, `queue_full`, `url_busy` or `cancelled`. MCP turns any status of 400 or more into `isError`, so this decides whether an agent polls again or gives up.
- If the file listener is off, an MCP `get` still downloads the whole file and then answers `url_unavailable`, which is useless to a remote agent.
- **Direction:** Add the full status table to the golden-case list. Make MCP `get` fail fast with a clear code when no listener is configured.

**m5. Sender-controlled strings and the process-wide panic hook (§3, §5.3, §5.4; `src/main.rs:234-238`).**
- §3 assumes IDs are "16–32 hex or upper-case characters". The ID is a sender-chosen string. After reduction to `[A-Za-z0-9._-]` it has no length bound, and two distinct IDs can collapse to one name.
- Any panic anywhere calls `exit(101)`. So a slicing or indexing bug in the many pure string functions (byte-limit cuts, name handling) takes down the whole bridge, not one lane.
- **Direction:** Bound or hash the ID part of the file name. Write the sanitiser and the timeout and size arithmetic without indexing or unchecked maths. Fuzz or property-test them with hostile Unicode.

**m6. Single worker, a fixed 64 KiB/s floor and no resume (§6.2, §6.3).**
- One worker means a 2 GB video blocks a 50 KB image behind it. A link slower than about 0.5 Mbit/s fails every large file as `stalled`, and each retry starts from zero.
- **Direction:** Consider a small-file lane, or a size-aware order. Make the floor a configurable constant. State in the design that there is no resume.

**m7. New dependencies are not listed (§2 last row, §5.3).**
- `Cargo.lock` has `ureq`, `fs2` and `percent-encoding`, but no crate for Unicode NFC or the Cc/Cf categories. A direct `ureq` dependency is also needed, with the same TLS feature as wa-rs. The quarantine attribute needs an `xattr` call or a shell-out.
- **Direction:** List them in the plan with the reasons. The release profile is `opt-level = "z"` and `panic = "abort"`.

**m8. Operability gaps (§10.2, §6.4).**
- The kill switch is an environment variable, so stopping new downloads means a restart and a WhatsApp reconnect. There is no pause and no cancel-all. A runaway client can fill a 1000-entry queue that only per-ID cancel can drain.
- A 20 GB default reserve makes the feature fail with `disk_reserve` on a small VPS disk.
- **Direction:** Add a runtime pause or `cancel-all` and a startup WARN when free space is below the reserve.

**m9. Token and listener edges (§7.2, §7.5).**
- "One live token per file, same URL until it expires" can hand out a URL with seconds left. A client that then starts a multi-GB fetch gets a 404.
- The listener is unauthenticated. 16 connections with a 10 s head deadline can be held by anyone who can reach the port.
- **Direction:** Mint a fresh token when under about 60 s remain. Add `limit_conn` and a request-rate limit to the proxy recipe.

**m10. `path` is gated by the bind address, not by the peer (§8.1; `src/api.rs:2126`, `2264`).**
- `loopback_file_access = cfg.is_loopback` is true for any caller of a loopback-bound REST listener. A REST API placed behind a local proxy would send `path` to remote callers.
- **Direction:** Say so in the doc, or return `path` only when the proxy headers are absent.

**m11. `waiting: disconnected` is also used for "no LID yet" (§6.1, §6.2).**
- On an account that has no LID, the item waits forever under a misleading label. **Direction:** Use a distinct `waiting: no_lid`.

## Questions for the owner

- **Q1. View-once and disappearing media.**
  - The view-once flag is a live-event flag (`src/bridge.rs:3316`) and is not stored. History extraction unwraps view-once into a normal `media_refs` row (`unwrap_to_inner`, `src/bridge.rs:3957-3971`).
  - Should an agent be able to pull view-once content to disk and onto another computer? Refusing it needs a stored flag (a fifth column), or an "unknown means refuse" rule.
  - Is persisting media from disappearing-message chats acceptable?
- **Q2. The live exclusion.** Everything received since pairing, the freshest and CDN-live media, can never be downloaded. §1 ties a live ref to the `from_me` fix, but a fresh link needs no re-upload. Does the fix really block storing a live ref?
- **Q3. Is `file_length` always the plaintext length?** The probe prints a note when sizes differ (`src/media_probe.rs:280`). Were any seen? The design makes a mismatch a final `integrity` failure.
- **Q4. Can the remote agent fetch an arbitrary URL and show the pixels to the model?** MCP tool results are one text block (`src/mcp_registry.rs:462-470`). Small files (images, voice notes, PDFs) could instead be returned inline, with the URL kept for large ones. §12 rejects bytes in the response only for large files.
- **Q5. Does the owner have other accounts or LID-less accounts?** The recovery path was validated on one account.
- **Q6. Is default-on right for already deployed instances?** An upgrade makes `media_get` appear on existing MCP tokens, which means new disk writes and automatic phone contact.

## Outside the requested structure

- **Complexity.** The delete tag, the revoke tag, the startup replay, adoption and un-tag form one state machine across three actors: the download finisher, the delete and revoke handlers, and startup. The invariants are argued in prose. A single `tag(path)` primitive and a short transition table would make the races (finisher against revoke) checkable. Parts that carry real cost for modest value are the participant list with its group-metadata lookup from the download path, the `lid_` folders, and the 10 000-entry persisted map.
- **Rollout.** Merging phase 2 alone forces a one-way DB migration on every deployed instance for no user benefit. The benefit it does give is that new rows carry raw JIDs earlier. That is a legitimate reason, but it should be a conscious choice.
- **Alternative worth a line in §12.** An async client with cancellable reads, instead of ureq in `spawn_blocking`, removes the lingering-thread class (M3) and the 2-thread rule.
- **Checked and fine.**
  - `direct_path` check parity with the probe (`src/media_probe.rs:534-538`).
  - Hard `process::exit`, so a blocked transfer thread cannot hang shutdown (`src/main.rs:246-257`).
  - The migration machinery supports `ADD COLUMN` (`src/storage.rs:2794-2961`).
  - MCP lane sizes make `get` and `status` cheap (`src/mcp_http.rs:54-58`).
  - `get` returning 202 is not an MCP error.
  - Uniform 404 on the listener leaks nothing.

Files the reviewer consulted include `src/bridge.rs`, `src/storage.rs`, `src/api.rs`, `src/mcp_registry.rs`, `src/mcp_http.rs`, `src/main.rs`, `src/media_probe.rs`, `deploy/proxy/*`, ADRs 0012 and 0048, and wa-rs `9e8c70e` `src/download.rs`, `wacore/src/download.rs`, `wacore/src/media_retry.rs`, `http_clients/ureq-client/src/lib.rs`.

---

## Reconciliation against v2

The coordinator checked each v2 finding against the v3 text (design v3, ADR 0050 v3, the ADR 0005 amendment), not
against this report. The cold reviewer did not see v2, so its silence on a v2 item means nothing. The v1 items that
v2's reconciliation left open all map to v2 items (v1 M1–M7 → v2 M1–M5/M7, m5; v1 M8 → v2 B1; v1 m10 → v2 B2; v1 Q2
→ the live test), so this table covers them too. "Partly → v3 Xn" means: v3 addresses the v2 item, but a v3 finding
shows that something is still missing.

| v2 finding | Short title | Status | Where settled / what remains |
|---|---|---|---|
| B1 | Startup tags by absence | **Resolved** | §5.5: only observed deletes and revokes tag; the file set comes from the deleted rows; an intent journal; startup only counts orphans. New sub-issue: the journal key race (v3 m1) |
| B2 | Expiry by error texts that never occur | **Resolved** | §2 rows on ureq and wa-rs; §6.2: own agent with `http_status_as_error(false)` and a (phase, status) table. The decryption errors are still text (v3 m2) |
| M1 | No loop guard; re-upload results unmapped | **Resolved** | §6.1 `expired_after_reupload`, every result and ACK error mapped, a 1 h memory for stored-only addresses; §6.2 `reuploaded` flag. Follow-up: the breaker ignores `expired_after_reupload` (v3 m3) |
| M2 | Existing rows lack the v9 fields | **Partly → v3 M4, M6** | §4 and §6.2: the LID mapping as fallback, `addressing` in `status`. Still missing: a live test of mapped and stored-only addresses on old rows (a gate), and a way to repair refs-less history rows |
| M3 | Stall and timeout defence incomplete | **Partly → v3 M3, m6** | §6.2: a media-only agent (the shared client is untouched), a body timeout scaled to `file_length`, a visible lane pause. Still missing: the bound depends on an unclamped, sender-controlled length; no resume |
| M4 | Counting writer vs wa-rs writer use | **Resolved** | §6.2 step 4: own transfer; a position-based writer that truncates per host; a sticky abort that is never retried on another host |
| M5 | File listener under-specified | **Resolved** | §7.2 pipeline: connection cap, head deadline and size cap, GET only, stream cap; §7.4 minimum rate. Residual edges: near-expiry tokens and unauthenticated connection holds (v3 m9) |
| M6 | `direct_path` unvalidated | **Resolved** (for this feature) | §6.2 step 3 (stored and re-upload paths); live ingest → roadmap P1 row. New residual: redirects are followed (v3 m3) |
| M7 | Columns and parts with no consumer | **Resolved** | `static_url` and newsletter paths dropped; the new access class and the download token bucket dropped; `file_sha256` and the quarantine attribute kept by the owner (§9) |
| m1 | Proxy buffering and logs; origin claim | **Resolved** | §7.2 proxy recipe and the corrected origin text |
| m2 | Delete race "re-check, then set" | **Resolved** | §6.2 step 6: one `UPDATE … WHERE … AND EXISTS` |
| m3 | Local path to remote callers; third-party content | **Partly → v3 m10** | §8.1/§8.3: `path` only for loopback REST callers, never over MCP; tool descriptions warn. Remaining: the loopback flag is bind-based, not peer-based |
| m4 | New MCP access class | **Resolved** | §8.3: the existing Write class |
| m5 | Group-name lookup in the download lane | **Resolved** | §5.3: never blocks; time-boxed 10 s; failures remembered |
| m6 | Controls reset on restart | **Resolved** | §6.5: persisted in `metadata`. Cost note: rewritten whole under the DB mutex (v3 m1) |
| m7 | Rollout | **Resolved** (as a stated choice) | §4, §9: phase 2 may merge alone; the owner decides |
| m8 | Spec nits | **Resolved** | §5.3 byte limits; §7.3 allowlist wording; §7.2 `PUBLIC_URL` required off loopback; §7.5 4096 tokens; ADR decision 5 now matches §6.1 |
| m9 | Disk observability | **Resolved** | §8.4 `free_bytes`, `reserve_bytes`. New: a startup WARN and a runtime pause (v3 m8) |
| Q1 | May the upsert create a missing ref? | **Answered** (fill only) → v3 M6 | the consequence (no repair of refs-less history) is challenged in v3 M6 |
| Q2 | `file_length` missing | **Answered** | §6.4: no byte limit; reserve and timeouts apply |
| Q3 | Kill switch scope | **Answered** | §10.2: new downloads only. v3 m8: no runtime switch |
| Q4 | Un-tag on return | **Answered** | §5.4 adoption and un-tag |
| Q5 | Raw CDN failures and fallback addressing on old rows | **Partly → v3 M4** | the probe records both; the old-file run is still open (§11.1) |
| Q6 | Range and HEAD | **Answered** | dropped (§7.2, §7.4) |
| Q7 | Revoked media | **Answered** (mark, no download) → v3 M2 | the rule as written trusts any revoke row (authorization) and needs a label lookup per poll |
| Outside | `wait_ms`, HMAC URLs, listener rationale, test gaps | **Resolved** | §12 rows; §7.2 rationale; §14 covers the crash replay, the real agent, the listener edge cases |

**Counts:** resolved 15, partly resolved 4 (M2, M3, m3, Q5), answered by the owner 7 (Q1–Q4, Q6, Q7 and the outside
items counted with "resolved" above), regressed 0, still present 0, indeterminate 0. Two answers (Q1, Q7) are
challenged by new v3 findings.

### New in v3

- **Introduced by v3 changes:** M2 (revoke authorization and the label lookup; the revoke rule is new in v3), m1
  (the single-key journal; new in v3), m3 part (the forced media-connection refresh per 401/403 item; new in v3's
  table).
- **Latent since v1, found now:** M1 (the `.deleted` marker inside a sender-controlled name), M5 (no byte budget for
  phone uploads), m5 (unbounded IDs and name collisions; the panic hook), m6 (one worker, no resume), m7 (the
  dependency list), m11 (`no_lid`).
- **Spec gaps:** m4 (HTTP/MCP mapping of `failed` reasons; MCP `get` without a listener), m8, m9, m10.
- **Questions:** view-once and disappearing media (Q1), the live exclusion rationale (Q2), `file_length` as the
  plaintext length (Q3; the probe showed no size mismatch on the 5 live photos of 2026-10-31 and 11-01), inline small
  files (Q4), other accounts (Q5), default-on for deployed instances (Q6).

### Trajectory

The design is converging. Blocking findings went 1 → 2 → 0, and the v3 majors are narrower: edge interactions,
authorization and budgets, not structure. The core (own transfer, own re-upload request, the listener, the
adapters) held up. The weak area is the delete/revoke/adoption subsystem: it grew the most in v3 and produced three
of the new findings (M1, M2, m1). The next revision should simplify it, for example by tagging structurally in a
reserved folder and resolving revokes once at ingest, rather than add more rules.
