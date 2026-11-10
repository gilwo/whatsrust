# Cold design review: on-demand media download, phase 2 (design v5.2, plan 2026-11-08) — follow-up

**Date:** 2026-11-08 · **Review:** follow-up of [the 2026-11-04 review](2026-11-04-on-demand-media-download-v4.md) (v4).
**Artifact:** `docs/plans/2026-11-08-media-phase2-plan.md`, `docs/plans/2026-10-29-on-demand-media-download-design.md`
(v5.2; scope: the phase 2 parts §2, §4, §5.5, §9, §13 row 2, §14), `docs/adr/0050-on-demand-media-download.md`
(Proposed, decisions 2 and 9, Notes), the ADR 0005 amendment; committed in 36cc054 (the uncommitted LID-chat fix was
in the working tree).
**Verdict:** **Approve with changes.** Blocking 0, Major 6, Minor 6, Questions 7.

The coordinator checked three majors against the code before filing: M1 (`Store::clear_device`,
`src/storage.rs:1179-1186`, deletes the `device` row on logout), M2 (`messages_fts_update` fires `AFTER UPDATE ON
messages` for any column, `src/storage.rs:290-293`) and M3 (a v8 binary refuses a v9 DB only through
`schema_version > CURRENT_SCHEMA_VERSION`, `src/storage.rs:693`, and re-validates when `schema_validated_version` is
stale, `:868`); all three hold. The reconciliation against v4 is at the end.

---

**Verdict: Approve with changes.** No blocking defect; the migration mechanics and the identity-based revoke test are
sound. Resolve M1–M3 before 2a is built and M4–M6 before 2b starts.

**Summary.** The plan turns design §13 row 2 into two commits and reuses the staged-migration machinery correctly.
Its best decisions are the one-transaction write, the fail-closed duplicate match, and the identity-based revoke test.
Its main weaknesses: the v9 step bundles several irreversible data changes into a one-shot version bump whose cost and
revert story are unmeasured; the 2b revoke rule rests on a wire format inferred from a library's send side with no
real-client evidence; and revoke resolution depends on arrival order and on which build was running.

## What's solid
- The step-once trap (`if from_version < 9`) is caught, and the pass is in 2a.
- The SCHEMA-before-migration trap is caught (no new index in `SCHEMA`, R4; `src/storage.rs:812`).
- The pin and rollback paths of plan §2.4 match `open_with_mode` (`src/storage.rs:707-783`) and `do_rollback`
  (`src/main.rs:1520-1626`), including "pin inconsistent" after a validation failure and the `rolled_back` pin refusing
  the v9 binary.
- The duplicate rule (chat, sender, `from_me`, kind) fits the globally unique, sender-chosen `message_id`
  (`src/storage.rs:209`).
- The revoke pass SQL matches `InboundContent::Revoke` (label `[deleted {id}]`, `src/bridge.rs:527`): the 9-character
  prefix, the `length - 10` arithmetic, the self-reference exclusion, and the order after the repair.
- A fixture trigger instead of a production hook is the right way to test a failed migration.

## Major

**M1. The repair is a one-shot irreversible data change, silently skipped when there is no paired device row — and
that row is deleted on logout.** Plan §2.3b, §4; design §4. `src/bridge.rs:3463` calls `store.clear_device()` on
`Event::LoggedOut` (`DELETE FROM device WHERE id = 1`, `src/storage.rs:1182`); the start gate
(`deploy/supervisor/whatsrust.service.in:18`) starts the binary during a QR flow, i.e. unpaired. Upgrading a
logged-out or mid-pairing profile bumps `user_version` to 9 with no repair; re-pairing the same number never repairs,
because the step does not run again. Old own rows keep `from_me = 0` or the wrong sender, duplicates mismatch, own
revokes never apply, cursor flags stay stale. The owner accepted the no-device case only for revokes; the skip drops
the whole repair, traced only by one `eprintln`. *Direction:* treat "messages exist and no own identity" as a decision
point: refuse to migrate until paired, or make the idempotent repair re-runnable under a `metadata` marker on the first
paired start (guarded against a changed number). At minimum: "pair before upgrading" in HOWTO 6c and the CHANGELOG,
and a loud skip message.

**M2. Migration cost is unmeasured, and the design multiplies it.** Plan §2.3c (an `UPDATE … WHERE id = ?` per hit),
R1. `messages_fts_update` (`src/storage.rs:290-293`) does an FTS5 delete plus re-insert for any column update;
`cache_size = -2000` (~2 MiB, `:679`); the validation probes (`:512-548`) exercise only the insert and delete
triggers. Every repaired row re-indexes an unchanged `body_text`; own rows can number in the hundreds of thousands;
with a 2 MiB cache, dirty pages spill into the WAL mid-transaction, which cannot be checkpointed before commit, on top
of the full `.bak` copy. ADR 0028 justified a dark daemon with "migrations are fast"; that premise no longer holds
(the daemon listens on nothing for the whole run, inside `Restart=on-failure` and the start limit). The update trigger
has so far run only for `embed_status='failed'` (`:1760`), never in bulk. R1 says "measure" with no acceptance bar.
*Direction:* update set-wise (`SELECT DISTINCT sender_jid`, compute the own forms, `UPDATE … WHERE sender_jid IN
(…)`); since `body_text` does not change, drop and recreate `messages_fts_update` inside the same transaction; state a
time and disk budget and measure time, WAL peak and disk peak on a synthetic DB sized like the largest real profile;
print elapsed time and row counts; add an update-trigger probe to `validate_migration_post_commit`.

**M3. "No supported way back" is overstated, and the planned rehearsal cannot test the real hazard.** Design §4,
plan §2.8 (HOWTO 6c), §4 ("Revert rehearsal"). Only the version number stops a v8 binary (`src/storage.rs:693`); v8
code names its `media_refs` columns (`:1537-1540`, `PROBE_COLUMNS` `:1005`); validation checks only the listed
`messages` columns. From reading, not running: v9 only adds nullable columns, so `PRAGMA user_version = 8` on a
stopped daemon should let the v8 build re-validate and run, losing nothing received since and no Signal state;
repaired rows are harmless to v8. The documented revert (restore the `.bak`) discards everything since the migration,
including the Signal sessions, and may force a re-pair. The rehearsal (a file copy on a synthetic DB) cannot show what
an older Signal state does against live servers. Plan §2.7 lists no revert test, although design §13 row 2 and plan
§4 require one. *Direction:* test the in-place downgrade with the real v8 build on a copy; if sound, make it the
primary documented revert, with restore-from-`.bak` as the fallback (or say in §4 why it is unsafe). Make `migrate_v9`
explicitly re-runnable, with an idempotency test (re-run after a downgrade). If restore stays primary, rehearse it once
live on the second account. Say in 6c to keep the v8 `.bak` until the engine is validated.

**M4. The 2b key rule rests on an inferred wire format, and its tests would be circular.** Design §2 ("revoke key is
written by the revoker"), §5.5, plan §3. wa-rs `src/send/actions.rs:33-63`, `src/send/mod.rs:409-428`; the stanza
also carries `edit = 7/8` (`SenderRevoke`/`AdminRevoke`, `wacore/src/types/message.rs:165-167`; parse tests
`src/message/tests.rs:4431`, `:4456`). The only evidence is another library's send code; real clients' keys have not
been observed on the receive side. If a real client sends another shape, every valid revoke by another person fails
closed (their deleted media stays downloadable) while the 2b tests, built from the same assumption, pass. Against a
forging member the identity test already rejects the revoke without the key; the key adds protection only against
identity-mapping errors or an admin delete with `same_user` wrongly true — real but narrow, for a fail-closed protocol
dependency. A LID-form `participant` with no mapping also fails closed. *Direction:* before 2b, capture real revokes on
the owner's accounts (a 1:1 delete by the other person, a group member's delete, an admin delete, the owner's own
delete from the phone), logging only the key shape and the `edit` attribute, never JIDs; use them as fixtures. Decide
whether the key check is a gate or telemetry until validated. Consider the stanza `edit` attribute as a second signal.

**M5. Revoke resolution depends on arrival order and on which build was running.** Plan §1, §3, R7; design §5.5.
- **2a-only window:** after a 2a-only build, revoke rows are stored but never resolved; the pass does not re-run, and
  2b resolves only later arrivals. Anything revoked while a 2a-only build runs on a live DB is missed for good.
- **Revoke before target:** never applied; history batches can be newest-first; no post-batch resolution is planned.
- **Live non-atomicity:** live resolution (plan §3 item 4) is a separate transaction from the revoke row's insert; a
  crash between them loses the revoke, while the history path is atomic.
- **Own revokes in misfiled LID chats** (design §11.5) stay false negatives: owner-revoked media stays fetchable; not
  listed as a known limit.
*Direction:* ship 2a and 2b together to any DB that runs live before the engine, or give 2b a one-time catch-up over
stored revoke rows. Reconsider the rejected alternative "look up the label on `get`" (design §12): a partial index in
`SCHEMA` on existing columns (e.g. `ON messages(body_text) WHERE content_kind='revoke'`) removes the scan cost;
deriving "revoked" at read time from stored revoke rows, with `revoked_at` as a cache, makes judgement
order-independent and re-judgeable, so a rule correction becomes a code change, not another irreversible migration.
Either way, list the false negatives.

**M6. `view_once` is computed by yet another unwrap walker, so a mismatch silently allows view-once media.** Plan
§2.6 (`view_once_of`), design §4; the same pattern for 2b's `revoke_key_of`. `unwrap_to_inner` (`src/bridge.rs:3960`),
the extractor's recursion (`:4559-4580`) and the live detection (`:3326`) are already three walkers; R3 documents one
divergence. A false 0 makes a view-once file fetchable, against the owner's decision; "refuse more" holds only if the
flag is exact. The extractor reads `video_message` or `ptv_message`; the plan names only image, video, audio. The
`COALESCE` fill cannot raise a stored 0 to 1; for a safety flag, take the max. *Direction:* set the flag where the
extractor already recurses (`MediaMode::Reference` marks it on entering a `view_once_*` arm and reads the leaf flag in
the image, video/ptv and audio branches); hand out the revoke key the same way; table-test against the extractor's
wrapper set.

## Minor
- **m1.** Design §11.5's remedy fails under the duplicate rule: a re-pair that keeps history cannot move misfiled own
  LID-chat rows (`INSERT OR IGNORE` on `message_id`, `src/storage.rs:1497`, plus "mismatch changes nothing"); only
  `clear-data` works. These rows also inflate `duplicate_mismatch` for media.
- **m2.** The live repair (§2.3c, exact bare equality) and the history repair (§2.3d, `is_own` with mapping twins) use
  different predicates: use one. The fixture has no dotted LID (`100000000000001.1:75@lid`, documented by
  `resolve_sender`, `src/bridge.rs:4683`; wacore keeps the dot in a LID user); check how `device.lid` compares with
  stored forms. The cursor `UPDATE … WHERE oldest_msg_from_me = 0` misses NULL, which readers treat as false
  (`src/storage.rs:2672`). `read_own_identity(conn)?` aborts the migration on a missing column: treat it as "no
  identity".
- **m3.** `apply_revoke_conn(…, key: Option<&RevokeKey>)` uses `None` for both "the pass: no key stored" and "this
  revoke has no key"; a later caller could bypass the key rule. Use an explicit enum.
- **m4.** Public API: `src/lib.rs` makes everything public (habb). `MediaRefRow` has no `#[non_exhaustive]`, so new
  fields break consumers that build it by literal; `HistoryWrite` changes shape between 2a and 2b and
  `insert_history_message` gains a parameter. Make new types `pub(crate)` or `#[non_exhaustive]`; note the
  `MediaRefRow` change in the CHANGELOG.
- **m5.** `Filled` vs `Unchanged` needs real change detection (a `COALESCE` UPDATE reports a change even when no value
  changed); test `Unchanged`. A `LiveRow` duplicate stays without a reference even when history carries one (Q4).
- **m6.** Missing tests: a post-commit validation failure (the "pin inconsistent" start and `--migrate` recovery);
  re-running `migrate_v9`; the FTS update path at volume; a dotted-LID own row; the large synthetic DB as an
  acceptance gate; the revert rehearsal of design §13.

## Questions for the owner
- **Q1.** What do the real stored forms look like: is the own LID in `device.lid`, `lid_pn_mapping.lid` and
  `messages.sender_jid` identical in its user part (with or without a `.N` suffix)? Check a real DB copy.
- **Q2.** How should an upgrade of an unpaired profile behave: refuse, skip (as planned), or defer the repair? (M1)
- **Q3.** Will a 2a-only build ever run on a DB that receives live traffic (the F10 instance, the dev profiles of the
  phase 9 check)? (M5)
- **Q4.** The most recent media (the most likely to be on the CDN) is live-sourced and never gets a reference, even
  when a history re-fetch carries one (`LiveRow`). Intended, now that the repair and the attribution fix make the match
  reliable?
- **Q5.** How large is the biggest real `messages` table (rows, own rows), and what startup downtime is acceptable?
  (M2)
- **Q6.** Do habb or other consumers construct `MediaRefRow` or match exhaustively on the new public enums? (m4)
- **Q7.** Can you capture real revokes (other person, group member, admin, own from the phone) before 2b? (M4)

## Assumptions and limits
- The tree at 36cc054 plus the uncommitted working tree (the LID-chat fix in `src/bridge.rs`). Plan line numbers are
  approximate.
- The v8 binary accepting extra columns, WAL growth under a 2 MiB cache, and real clients' revoke keys come from
  reading code and library sources; nothing was run.
- The earlier cold reviews were not read, per instruction.

---

## Reconciliation against v4 (coordinator)

Each v4 finding checked against the v5.2 artifact itself (not against this report's silence).

| v4 finding | Status in v5.2 | Evidence |
|---|---|---|
| M1 one-way door; phase 2 could merge before the gate | **Resolved**, then **superseded** by v5.2 M3 | §4 states the no-rollback rule and the manual revert; the matrix gate passed (§13 1b). v5.2 M3 finds the door less one-way than stated (an in-place downgrade looks possible) |
| M2 stdio MCP is a REST client | **Resolved** | §8 caller marker (`X-Whatsrust-Caller`); §9 stdio MCP row |
| M3 revoke rule fails open: (a) own revokes, (b) sender forms, (c) existing data, key fields dropped, revoke-before-target | **Resolved** for (a)–(c) (attribution fix 18739df, the repair, identity comparison through the mapping, the one-time pass); key fields: a helper reads them (v5.2); revoke-before-target: **accepted and documented**. Residuals **re-raised** as v5.2 M4 (key shape unproven) and M5 (order and build dependence) | §4, §5.5, §9 |
| M4 duplicates trust `message_id` alone | **Resolved** | §4 match rule (chat, sender, `from_me`, kind) |
| M5 byte budget can make a file unrecoverable | **Resolved** | §6.5 one window; a lone over-size file admitted into an empty window |
| M6 "a blocked read cannot be interrupted" | **Resolved** (design); **indeterminate** over HTTPS until the phase 4 tests | §6.2 ureq's own `timeout_recv_body` / `timeout_global`; §11 item 3 |
| m1 over-claim abort before the MAC | **Resolved** | the ciphertext `Content-Length` is the hard cap |
| m2 deny-list sanitiser with two crates | **Resolved** | std allow-list (§5.3, §9) |
| m3 adoption recomputes names from mutable inputs | **Resolved** | adoption by the ID part of the name (§5.4) |
| m4 head-of-line blocking vs re-upload | **Still present, accepted** | two lanes keep re-uploads off the download lane; a large file still blocks small ones while it streams; "a small-file lane" stays a later option (§12) |
| m5 pre-v9 rows cannot meet three guarantees | **Resolved in the design**; the HOWTO text is phase 8 work | §4 (NULL `view_once` = unknown, `status` says so; addressing source reported) |
| m6 one large intent value | **Resolved** | chunked keys `media_tag_intent:<id>:<n>` (§5.5) |
| m7 logging and `detail` hygiene | **Resolved** | closed `detail` vocabulary; the daemon log filter (bd5f785) |
| m8 URL base for a remote MCP client | **Resolved** | `url_note` (§7.2, §8) |
| Q1–Q6 (stdio over ssh; off behaviour; query-string paths; `retry_after_ms`; matrix threshold; allowlist) | **Answered** | §9 rows; the matrix control cell (200) and the double-`?` fact (§2); the per-item backoff `not_before` |

**New in v5.2:** M1 (repair skipped when unpaired), M2 (migration cost and the FTS update trigger), M3 (revert
story), M4 (unproven revoke key shape), M5 (order and build dependence of revoke resolution), M6 (a separate
`view_once` walker); m1–m6; Q1–Q7.

**Trajectory:** the storage and transfer parts converge: no blocking finding, and the majors are now about migration
operations (cost, the unpaired case, the revert), which are measurable and fixable. **The revoke subsystem does not
converge**: it drew a major in v3 (M2), v4 (M3) and v5.2 (M4, M5), each answered by another patch to resolve-at-ingest
(identity rules, a one-time pass, a key rule, ordering caveats). The reviewer's alternative — derive "revoked" at read
time from the stored revoke rows, with a partial index — would make the judgement re-runnable and order-independent and
remove the pass from the irreversible migration. That is the owner's call.
