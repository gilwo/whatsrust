# Cold design review: on-demand media download (design v4, ADR 0050) — follow-up

**Date:** 2026-11-04 · **Review:** follow-up (v4) of [the 2026-11-01 review](2026-11-01-on-demand-media-download-v3.md) (which reconciles [v2](2026-10-30-on-demand-media-download-v2.md) and, through it, [v1](2026-10-29-on-demand-media-download.md)) · **Reviewer:** cold, independent `design-reviewer` subagent (it did not see the prior reviews)
**Artifact:** `docs/plans/2026-10-29-on-demand-media-download-design.md` (v4), `docs/adr/0050-on-demand-media-download.md` (Proposed v4), the ADR 0005 amendment (revised 2026-11-03); committed in a67d077
**Verdict:** **Approve with changes.** Blocking 0, Major 6, Minor 8, Questions 6, Notes 5.

The coordinator's reconciliation against the v3 findings is at the end ("Reconciliation against v3"). The
coordinator checked two majors against the code before filing: M1 (`src/main.rs:1484-1498` refuses `--rollback`
unless the migration pin is `failed`) and M2 (`src/mcp.rs:132` `call_tool` proxies stdio MCP calls to the loopback
REST port); both hold.

---

I found nothing that is certain to lose data or breach security as designed. Six Major findings should be settled before phase 2 (the schema) is merged anywhere that matters.

I checked the design against the code and the two source trees. These claims hold:
- The pin's `download_to_writer` seeks the writer to 0 on every host attempt.
- The shared ureq agent keeps `http_status_as_error = true`.
- `max_redirects(0)` returns a 3xx response instead of an error, because the error needs `max_redirects > 0` (ureq `config.rs:203`).
- `timeout_recv_body` is measured from the end of the response head.
- `CallContext.loopback_file_access` follows the REST bind.
- `insert_message_if_new` is `OR IGNORE` on `message_id` alone.
- `DeleteChatUpdate` and `DeleteMessageForMeUpdate` are the only delete sites.
- The revoke label is `[deleted <id>]`.

## Steelman

The design takes a storage model that deliberately holds no bytes (`media_refs`) and adds on-demand retrieval with an unusually strong threat model for sender-controlled input:
- bounded file names and IDs, an extension allowlist, and a `direct_path` check;
- a media-only HTTP agent with explicit status handling, a clamped timeout and a sticky-abort writer;
- a disk reserve, a separate re-upload lane with several caps and a breaker;
- a header-hardened file listener with token URLs;
- two small adapters, so the later wa-rs upgrade has a clear seam;
- an honest account of what is unproven. The old-file matrix gates the engine, and the live test already settled the own-LID addressing.

The owner's constraints are respected throughout. The write-ordering races (delete or revoke during a download) are handled correctly by the conditional `UPDATE` in step 6 plus `tag`.

## Blocking

None. The closest candidates (M1, M3) are Major because their blast radius is bounded and the fixes are cheap.

## Major

### M1. Phase 2 is a one-way door with a rollback story that does not exist, and it can merge before the feasibility gate
**Problem.**
- §4 L156–158 says a v8 binary refuses a v9 DB, "so a rollback after the merge uses `--rollback` (HOWTO §6a)".
- `do_rollback` (`src/main.rs:1484-1498`) refuses unless the migration pin is in state `failed`. It says "do not roll back a healthy database". ADR 0030 states the same.
- A successful v8→v9 migration leaves no pin, so the documented procedure is refused.
- The remaining path is hand-restoring the `.bak`. That drops everything received since, and also the Signal session state kept in the same DB. A stale session store is a re-pair risk.
- Meanwhile the stated benefit of merging phase 2 alone is only "new rows carry the raw JIDs earlier" (L158).
- The feature's value rests on re-upload of old expired files. The matrix that tests it (§11.1) gates only the merge of phase 4 (L743).

**Why it matters.** An operator reaching for the documented rollback at the worst moment will be refused. The one-way door is also taken before the cheapest experiment (the probe is ready) shows whether the main path works.

**Direction.**
- Run the matrix before phase 2 reaches any instance that matters.
- Either make additive v9 tolerable to the v8 binary, or correct §4, ADR 0050 and the HOWTO to say there is no supported way back, and rehearse a manual revert.
- Do not offer "merge phase 2 alone" until one of those is done.

### M2. The stdio MCP server is a REST client, so the "caller class" model of §7–§8 does not hold for it
**Problem.**
- §8.1 L559–562 and §8.3 L581 say MCP never gets `path`. L549–550 say that with the listener off, an MCP `get` answers 409 and starts nothing.
- That is true only for the MCP HTTP runner (`src/mcp_http.rs:1380`, `loopback_file_access: false`).
- `whatsrust mcp` (stdio) proxies every tool call to the loopback REST port over HTTP (`src/mcp.rs:132-155, 158-213`). The daemon sees `CallContext { loopback_file_access: cfg.is_loopback }` (`src/api.rs:2264`).
- `rest_tool_result` (`src/mcp_registry.rs:477`) passes the body through unchanged.
- So over stdio, `get` returns `path`. With the listener off it returns 200 `url_unavailable` plus `path` and does start the download. That is the "of no use to a remote agent" case the design wants to prevent.
- An agent on another computer running `ssh host whatsrust mcp` is a natural deployment for the stated use case.
- §13 phase 6 lists both "stdio/HTTP parity" and "`path` absent over MCP". They contradict each other for stdio.

**Why it matters.** The rule "`path` and local-file behaviour only for local callers" is enforced by transport, but stdio MCP and the CLI share a transport and differ in who they serve. The result is a contract that tests will flag, or an unusable answer to a remote agent.

**Direction.**
- Make the caller class explicit. For example, the stdio proxy sends a marker that the media routes honour, or the media tools' results are filtered in the registry layer.
- Or declare stdio MCP "local" and say so, and drop the `path`-absent claim for it.
- Decide what the URL base is for a stdio-over-ssh agent (see m8).

### M3. The revoke rule fails open in three realistic cases, against the owner's "a revoked message is never downloaded"
**Problem.** The rule is in §5.5 L261–267. A revoke counts only when "the revoker is the target's sender", and it is resolved once when the revoke row is stored.
- **(a) The owner's own revokes.** For a `from_me` target the revoke must be `from_me` too. Live ingest hard-codes `from_me = 0` (`src/storage.rs:1498-1509`, called from `src/bridge.rs:3390`). The design lists this bug as out of scope (L695). Every "delete for everyone" the owner does on the phone arrives live, with `from_me = 0`, and is rejected as invalid.
- **(b) Sender comparison across forms.** Comparison "through the mapping" (history rows hold full JIDs, live rows bare numbers). An unmapped LID makes the revoke not count.
- **(c) Existing data.** Revoke rows already in a v8 DB, whose targets are stored, are never evaluated, because resolution happens only at revoke-store time. L143 says "nothing is backfilled from `body_text`". For revokes the label is generated as `format!("[deleted {target_id}]")` (`src/bridge.rs:527`), so an exact-equality join is reliable, unlike the document-name case.
- Extraction also drops the revoke's own `key.from_me`, `participant` and `remote_jid` (`src/bridge.rs:4389-4394`). Those are the fields that identify the target without guessing sender forms.

**Why it matters.** The failure is silent. A revoked media file stays fetchable.

**Direction.**
- Keep the revoke key's `from_me` and `participant` at extraction and compare them to the target row.
- For own targets, accept a revoke whose sender is the owner's own account, instead of depending on the buggy flag. Or fix the live `from_me` as a prerequisite.
- Add a one-time pass at migration or startup over existing `content_kind='revoke'` rows, using exact-equality on the label.
- Treat the revoke-before-target case with a bounded pending key (the `metadata` pattern already used elsewhere), or document that it is accepted.

### M4. The duplicate fill and create rule trusts `message_id` alone
**Problem.**
- §4 L148–152: a duplicate history message fills NULL columns of an existing reference and creates a missing one when the stored row is not live.
- Duplicates are detected by `message_id` alone. The column is globally `UNIQUE` and sender-chosen (`src/storage.rs:209`, `1458-1490`; `src/bridge.rs:5249-5270`).
- A group member who has seen a victim message's ID can send a message with the same ID. If it later reaches history ingest (re-pair, retried page, fetch), it counts as a "duplicate" of the victim's row.
- Every pre-v9 reference has NULL `file_name`, `file_sha256`, `remote_jid` and `participant`, so all are fillable from the colliding message.
- A pre-amendment media row with no reference gets a complete reference, including `media_key` and `direct_path`, from the colliding message.

**Why it matters.**
- An attacker's file can appear as the victim's attachment, in the victim's chat folder, under the victim's sender name.
- A poisoned `file_sha256` makes a final `integrity` failure.
- A poisoned `remote_jid` or `participant` gives a wrong re-upload address and a 24 h `not_on_phone` memory.

**Direction.** Fill or create only when the incoming `(chat_jid, resolved sender, from_me, content_kind)` equals the stored row's. Count and log mismatches. A one-line rule in §4 and one test row cover it.

### M5. The re-upload byte budget can make a file unrecoverable while the API says "retry"
**Problem.**
- §6.5 L445 says a request that would exceed the budget waits and fails as `reupload_paused` with `retry_after_ms` "until the hour window has room".
- A file whose `file_length` exceeds the whole budget never fits. The default is 1 GB, the floor 50 MB (L663), and the owner set no per-file size cap. WhatsApp documents can reach 2 GB.
- §8.1 L552 maps this reason to a retryable 202, so the agent polls forever.
- The charge is the sender-declared `file_length`, or 64 MiB when it is absent (L441). The phone uploads the whole file either way. Against a hostile or sloppy sender the byte budget therefore limits nothing. The 20 per hour count is the real bound.
- The new `direct_path` is held only in memory (L406). A restart after a successful re-upload costs a second phone upload and a second budget charge.

**Why it matters.** There is a permanent-failure state disguised as a transient one, and a protection that is weaker than it reads.

**Direction.**
- Admit a lone oversize request when the window is empty, or give a distinct final reason that names the setting.
- Compute `retry_after_ms` exactly.
- Say in §6.5 that the budget is an honest-sender guard.
- Persist the fresh path briefly (a bounded `metadata` key, as for `not_on_phone`).

### M6. The lane-liveness design rests on "a blocked read cannot be interrupted", which is not true for ureq 3.3.0
**Problem.**
- §6.3 L414–418 and §12 L721 reject an interruptible client. They rely on `timeout_recv_body` to end stalled threads, and pause the lane when 4 are lingering.
- ureq 3.3.0 exposes `Agent::with_parts(config, connector, resolver)` (`agent.rs:130`) and the `Connector` and `Transport` traits (`unversioned/transport/mod.rs:120, 266`).
- The TCP transport applies `NextTimeout.after` with `set_read_timeout` (`tcp.rs:217-224`), and `after` is a public field (`timings.rs:187-192`).
- A thin transport wrapper that clamps `after` to the stall period turns every read into an idle timeout.
- As designed, a dead connection (a laptop wake-up, a NAT drop) leaves its thread until `timeout_recv_body`: up to 7200 s for large or unknown-length files. Four such stalls pause the only download worker for up to 2 h after the network is back.

**Why it matters.** The bounded-life argument is correct but too weak for an always-on daemon. The sticky-abort, lingering-thread and `download_lane: paused` machinery exists to work around a limit the chosen library does not have.

**Direction.**
- Prototype the idle-timeout wrapper. It is an `unversioned` API, so pin ureq exactly (the lock already does) and test it with the real agent against a stalling local server.
- If it works, drop the 4-thread pause and most of the lingering-thread text.
- If not, keep the design but say the pause can last up to the transfer limit.

## Minor

**m1. The over-claim abort contradicts the "accept after a valid MAC" rule (§6.2 L363, L366–368, L384).**
- Streaming aborts at `bytes > file_length` as a final `integrity` failure, before the MAC is known.
- The rule that accepts a size mismatch after a valid MAC can then only cover files smaller than claimed.
- `file_length` is sender-supplied. A slightly wrong value makes a good file permanently undownloadable.
- *Direction:* use the ciphertext `Content-Length` as the hard cap, and treat `file_length` as a hint for the budget and timeout.

**m2. The sanitiser is a deny-list that needs two new crates (§5.3, §10.3).**
- A deny-list of Cc and Cf plus NFC pulls in `unicode-normalization` and `unicode-properties`, in a release profile tuned for size.
- A plain allow-list built from std (`char::is_alphanumeric` plus `_ - .`) keeps Hebrew and any script and removes control, format and bidi characters by construction.
- The cost is losing combining marks (for example niqqud).
- *Direction:* weigh two dependencies against that cost. The 64-byte cut and the property tests remain either way.

**m3. Adoption and un-tag recompute the file name from mutable inputs (§5.4 L243–245, §5.5 L281).**
- Inputs: the LID↔phone mapping, the `lid_` versus phone folder choice, and the sender-phone fallback `lid<number>`.
- The adoption test is "expected size", while `size_note` accepts a different size after a valid MAC (§6.2 L366).
- Adoption silently fails and the file is downloaded again. A returning message can miss its `_deleted/` copy for the same reason.
- *Direction:* find the file by its ID component in `<chat>/` and `<chat>/_deleted/`. Verify `file_sha256` when stored. Define adoption when `file_length` is absent.

**m4. Head-of-line blocking interacts badly with re-upload (§6.2 L344, L406, §12 L717).**
- One worker means a 2 h transfer delays every other `get`.
- A re-upload success is placed "at the front of the queue" but cannot pass the running transfer. The phone already paid for the upload, and the new link may idle or expire.
- *Direction:* either raise the priority of the small-file lane ("a later option" today), or let a re-upload success preempt the queue wait rather than only the queue order.

**m5. For pre-v9 rows three guarantees cannot be met, and the doc should say so (§4 L143–145).**
- `view_once` NULL means "allowed", so a view-once item stored before v9 is downloadable despite the owner's "refused".
- The raw `remote_jid` and `participant` are absent, which sends those rows to the `mapped` or `address_unresolved` addressing.
- Repair is only a re-pair or a `clear-data` plus a new fetch (L153–154), a heavy remedy for the entire existing corpus.
- *Direction:* state this in the HOWTO and in `get`'s answer for NULL rows. Quantify `address_unresolved` in the matrix (see Q6).

**m6. The intent journal can hold one very large value (§5.5 L271–274 versus §6.5 L449–450).**
- A chat delete writes one `media_tag_intent:<id>` listing every path of that chat. Elsewhere the design takes care that "no write rewrites a large value".
- *Direction:* chunk the list, or write one key per path batch.

**m7. Logging and `detail` hygiene.**
- The media-connection `auth` token is appended to every CDN URL (`wacore/src/download.rs:292`). Neither the design nor the test plan says that URLs and ureq debug output stay out of logs, `status.detail` and MCP results (which a remote agent and its model see).
- *Direction:* build `detail` from a closed vocabulary, never from raw error text, and add a test that greps for `auth=`.

**m8. The URL base for a remote MCP client (§7.2 L469–471).**
- The default `PUBLIC_URL` on loopback is `http://127.0.0.1:<port>`, which is useless to a remote agent.
- When the MCP listener is non-loopback and the media listener is on, but `PUBLIC_URL` was not set explicitly, startup should refuse or warn. Apply the same check to stdio-over-ssh (see M2).

## Questions for the owner

**Q1.** Is an agent on another computer using the stdio MCP server over ssh a supported deployment? The answer sets the fix for M2 (explicit caller marker versus declaring stdio "local").

**Q2.** What runs when `WHATSRUST_MEDIA_DOWNLOAD=off`? §10.2 L658 says only "no download and no re-upload starts". Please state that the delete hook, the revoke tag, the journal replay, the startup `.part` cleanup, adoption and the orphan count keep running. If they are gated by the flag instead, files of deleted messages stay in place and journal entries accumulate.

**Q3.** Has a fresh history-stored file, with a `direct_path` that carries a query string, been downloaded with 200 through the pin's URL assembly?
- The pin builds `https://{host}{direct_path}?auth=…&token=…` unconditionally (`wacore/src/download.rs:291-293`).
- Your own test fixtures use paths like `/v/t62/abc?ccb=11` (`src/bridge.rs:7735`) and `direct_path_is_safe` accepts `?`.
- A path that already has a query would then get a second `?`.
- The matrix records what the CDN says for expired links, but not a known-good control, and 403 is treated as expired (L379).

**Q4.** Inside `retry_after_ms` of a retryable failure (`network`, `stalled`, `internal`, `disk_reserve`), does `get` restart the download?
- There is no resume, and the agent is told to keep polling.
- If `get` always restarts, a persistent stall on a big file becomes an endless full re-download loop. That would need a server-enforced `not_before` per message. That is not a download token bucket; it is a per-item backoff.

**Q5.** Should the matrix have a go/no-go threshold? Phases 5–8 (listener, scripts, docs) do not depend on its result, so they will be built regardless.
- What share of old files must be recoverable for phases 2–4 to go ahead?
- What if the stored-form addressing never works and many rows end up `address_unresolved`?
- Does a second account without a LID get `waiting: no_lid` forever (L313), or a fallback to the pin's phone-number request?

**Q6.** Is `WHATSRUST_ALLOWED_NUMBERS` meant to scope media downloads? The allowlist is applied only in live ingest (`src/bridge.rs:3294-3305`). History tools and this feature do not apply it. This is consistent with today, but please confirm it is intended.

## Outside the requested structure

- **Note: overlapping re-upload controls.** The pacer, the hourly count, the byte budget, the breaker and the `not_on_phone` memory all protect the phone and all persist (§6.5). That is many mechanisms for one purpose. Several of them (count and bytes) could share one window. Not a gate.
- **Note: unauthenticated connection cap.**
  - The file listener's 16-connection and 8-stream caps (§7.2) are global. Directly exposed, a single unauthenticated client can keep the real agent at 503 by opening connections.
  - The proxy recipe's per-client limits fix this, but only behind the proxy. Say so in the HOWTO for `ALLOW_REMOTE` without a proxy.
- **Note: `HEAD` returns 405.** Some download tools and browsers probe with `HEAD`. The owner decided this; consequence only.
- **Note: `_deleted/` has no visibility.** Nothing reports its size and nothing reclaims it except a manual `rm`. `/api/status` could carry `deleted_bytes` so the free-space reserve's decline has a visible cause.
- **Note: delete by ID only.** `delete_inbound_message` deletes by `message_id` alone (`src/storage.rs:1963-1979`). With this feature a colliding ID (see M4) moves the file of whichever chat owns the stored row. The delete event also carries `chat_jid`; match on it too.

## Assumptions

- Only the pin `9e8c70e` was used for claims about wa-rs. The upstream `main` statements in the design (typed errors, own-LID `media_reupload`) were not verified.
- I did not read `_reviewer/`, `.env`, `runs/`, `vendor/`, or any `whatsapp.db*` file.
- I assumed `WHATSRUST_ALLOWED_NUMBERS` is a single global allowlist.

---

## Reconciliation against v3

The coordinator checked each v3 finding against the v4 text (design v4, ADR 0050 v4, the ADR 0005 amendment), not
against this report. The cold reviewer did not see v3, so its silence on a v3 item means nothing. v1 and v2 items
were all resolved or carried into v3 items by the earlier reconciliations, so this table covers them too.
"Partly → v4 Xn" means: v4 addresses the v3 item, but a v4 finding shows that something is still missing.

| v3 finding | Short title | Status | Where settled / what remains |
|---|---|---|---|
| M1 | `.deleted` marker in a sender-controlled name | **Resolved** | §5.2 reserved `_deleted/` folder; §5.5 one `tag` primitive and a transition table; §14 test "`x.deleted.pdf` is an ordinary file" |
| M2 | Any revoke row counts; label lookup per poll | **Partly → v4 M3** | §5.5 validity rule and `revoked_at` resolved at ingest (the per-poll cost is gone). Still missing: own revokes fail because of the live `from_me` bug; unmapped LIDs; existing revoke rows are never evaluated |
| M3 | Transfer bound follows a sender-controlled length | **Resolved** | §6.2 clamp (`WHATSRUST_MEDIA_TRANSFER_MAX_SECS`, 2 h); §6.3 at most 4 ending threads; §1 "no resume". Improvement offered: a ureq idle-read hook (v4 M6) |
| M4 | Recovery for old rows untested and not gated | **Partly → v4 M1, Q5** | §11.1 and §13: the matrix gates the merge of phase 4; `address_unresolved`. Still missing: phase 2 may merge before the matrix; no go/no-go threshold |
| M5 | No byte budget for phone uploads | **Partly → v4 M5** | §6.5 1 GB/h budget; size shown in `waiting_reupload`. Still missing: a file larger than the budget never fits but answers "retry"; the budget trusts the sender's length; the fresh path is lost on restart |
| M6 | Refs-less history can never be repaired | **Resolved** (new risk → v4 M4) | §4: create when the stored row is not live; one transaction; `no_reference` with `source`. The create rule trusts `message_id` alone (v4 M4) |
| m1 | Single-key intent journal | **Partly → v4 m6** | §5.5 per-event keys. A chat delete can still write one very large key |
| m2 | Decryption errors as text | **Partly → v4 m1** | §6.2: `Content-Length` check, one retry on another host, a pin test, accept a size mismatch after a valid MAC. The over-claim abort fires before the MAC is known |
| m3 | Unpaced refresh; breaker blind spot; redirects | **Resolved** | §6.2 refresh at most once per 60 s; `max_redirects(0)` (the v4 reviewer confirmed a 3xx is returned, not an error); §6.5 the breaker counts `expired_after_reupload` |
| m4 | HTTP/MCP mapping; MCP `get` without a listener | **Partly → v4 M2** | §8.1 full answer table; MCP fails fast without a listener. Does not hold for the stdio MCP server, which is a REST client |
| m5 | Unbounded IDs; panic hook | **Resolved** | §5.4 bounded or hashed IDs; §5.3 no indexing, property tests |
| m6 | One worker, floor, no resume | **Partly → v4 m4** | stated choices (§12 small-file lane row, §1 no resume, named constants). New interaction: a re-upload success waits behind a long transfer |
| m7 | Dependency list | **Resolved** | §10.3. Alternative offered: a std allow-list instead of two crates (v4 m2) |
| m8 | Operability | **Resolved** | §6.4 startup WARN; §6.3 and §8.1 `cancel all` |
| m9 | Near-expiry tokens; proxy limits | **Resolved** | §7.5 fresh token under 60 s; §7.2 `limit_conn`/`limit_req`. Note: a direct exposure without the proxy has only the global caps |
| m10 | `path` by bind, not peer | **Partly → v4 M2** | §8.1 states the rule (the REST API is never proxied). The stdio MCP server gets `path` through the loopback REST listener |
| m11 | `waiting: no_lid` | **Resolved** | §6.1 |
| Q1 | View-once and disappearing media | **Answered** (→ v4 m5) | view-once refused (`view_once` column); disappearing chats allowed. Pre-v9 rows are unknown and allowed |
| Q2 | The live exclusion | **Answered** | out of scope, confirmed (§1, §9) |
| Q3 | `file_length` as the plaintext length | **Answered** (→ v4 m1) | no mismatch on 5 photos; accept after a valid MAC; contradicted by the over-claim abort |
| Q4 | Inline small files | **Answered** | URL only (§8.3, §9) |
| Q5 | Other accounts | **Answered** (→ v4 Q5) | one other account; check its LID with the probe (§11.2). Open: what a LID-less account gets |
| Q6 | Default on for deployed instances | **Answered** (→ v4 Q2) | off by default (§10.2). Open: which hooks still run when off |
| Outside | Complexity of the delete/revoke subsystem | **Partly** | §5.5 `tag` primitive and transition table. Note: five overlapping re-upload controls (v4 note) |
| Outside | Rollout of phase 2 alone | **Partly → v4 M1** | stated as the owner's choice, but the rollback sentence in §4 is wrong (latent since v3) |
| Outside | Async client alternative | **Resolved** | §12 row; v4 M6 offers a ureq-native idle timeout instead |

**Counts:** resolved 12 (M1, M3, M6, m3, m5, m7, m8, m9, m11, Q2, Q4, async alternative); partly resolved 11 (M2,
M4, M5, m1, m2, m4, m6, m10, complexity, rollout, and Q1/Q3/Q5/Q6 counted as answered with follow-ups);
regressed 0; still present 0; indeterminate 0.

### New in v4

- **Introduced by v4 changes:** M3 (the revoke validity rule meets the live `from_me` bug), M4 (the create-on-duplicate
  rule trusts `message_id`), M5 (the byte budget can never admit a large file), m1 (the accept-after-MAC rule
  contradicts the over-claim abort), m6 (the per-event intent of a chat delete).
- **Latent since earlier versions, found now:** M1 (the rollback sentence, since v3), M2 (stdio MCP is a REST client,
  since v2's `path` rule), M6 (ureq can time out idle reads), m3 (adoption recomputes the name from mutable inputs),
  m4, m5, m7 (the media `auth` token in logs and `detail`), m8 (the URL base for remote agents).
- **Alternatives offered:** m2 (a std allow-list sanitiser without two crates).
- **Questions:** stdio over ssh (Q1), what runs when off (Q2), a known-good control for query-string paths (Q3),
  per-item backoff (Q4), a go/no-go threshold for the matrix and LID-less accounts (Q5), the number allowlist (Q6).

### Trajectory

Converging, with diminishing returns. Blocking findings went 1 → 2 → 0 → 0, and majors 10 → 7 → 6 → 6. The core
architecture (own transfer, own-LID re-upload, the listener, the adapters, `_deleted/`) has held through two
reviews. But each revision's new rules produced most of the next round's majors: four of the five "introduced by
v4" items come from rules added in v4. Several v4 fixes are simplifications (M6 removes the lingering-thread
machinery; the overlapping re-upload controls can share one window; m2 removes two crates). The next step should
favour removing rules over adding them, make the old-file matrix the first gate (before phase 2), and move the
review effort from the design to the code of phases 2–3.
