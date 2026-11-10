# Cold design review: on-demand media download, phase 2 (design v5.3, plan v5.3) — follow-up

**Date:** 2026-11-08 · **Review:** follow-up of [the v5.2 review](2026-11-08-on-demand-media-download-v5.md).
**Artifact:** `docs/plans/2026-11-08-media-phase2-plan.md` (v5.3), `docs/plans/2026-10-29-on-demand-media-download-design.md`
(v5.3; the phase 2 parts §2, §3, §4, §5.5, §6.2 step 6, §9, §13 row 2, §14), `docs/adr/0050-on-demand-media-download.md`
(decisions 2 and 9, Notes), the ADR 0005 amendment; committed in d1555aa (run 2a was in progress in the working tree
and was not reviewed).
**Verdict:** **Approve with changes.** Blocking 0, Major 4, Minor 8, Questions 6.

The coordinator checked two claims against the code before filing: M4's allowlist drop (`handle_event` returns before
storage for a sender not on `WHATSRUST_ALLOWED_NUMBERS`, `src/bridge.rs` ~3304–3315; `persist_history_messages` has no
allowlist) and M3's `mediaKeyTimestamp` (present on 15 media message types in the pinned `waproto/src/whatsapp.proto`).
Both hold. The reconciliation against v5.2 is at the end.

---

**Verdict: Approve with changes.** No blocking findings; 4 major. Run 2a can proceed once M1 and M2 are settled and M3
is decided consciously (a schema choice that becomes expensive after the v9 commit). M4 before 2b.

## Summary
Phase 2 is an additive v9 migration through the existing staged ceremony, plus a deferred, marker-guarded repair of
own-message attribution, a one-transaction history write with a duplicate rule, and a revoke judgement computed from
stored rows when read. The migration and ingest parts are sound and well fenced. The weak spots: the repair's
one-shot marker, its weakened justification and missing safety net; a fill-only duplicate rule that freezes the
media-key tuple with no freshness column; an unspecified precedence between a read-time `revoked` state and
event-time file tagging.

## What's solid
- The v9 step: five nullable columns plus one partial index, each guarded, inside the existing `BEGIN IMMEDIATE`
  transaction with the version bump last; no new-column index in `SCHEMA`.
- The in-place downgrade, traced against the committed v8 code: `schema_version == 8` means no migration; `SCHEMA`
  re-runs as a no-op; `schema_validated_version = '9'` differs, so v8 re-validates; `validate_migration_post_commit`
  checks only `messages` columns; `insert_media_ref` names its columns.
- The repair classifies distinct stored sender strings, then `UPDATE … IN (list)`; the paused update trigger is right
  for an external-content FTS table.
- The sender forms the plan lists are the forms the code produces.
- The shape report and the 30 s budget are real pre-flight evidence.
- Tombstone forgery is closed (a forged revoke needs `sender == target author` plus `same_chat`); cross-sender ID
  collisions cannot fill or create references.

## Major

**M1. The one-shot repair marker is not safe against later wrong rows.** Plan §2.3, §2.4, §2.1; design §4.
- The plan's own revert path breaks it: after the in-place downgrade, the v8 binary writes `from_me = 0` live rows and
  group-JID senders for own history rows; on re-upgrade "the repair marker stays, so the repair does not run twice",
  so every row written in the v8 window stays misattributed, silently.
- A partial identity at marker time: with `device.lid` empty and no mapping row, LID-form live rows are skipped, and
  the marker (number only) locks that in.
- `read_own_identity` swallows every error, and `persist_history_messages` (~5299) already falls back to the wrong
  sender on a lookup failure; once the marker exists those rows are never repaired.
- The repair is idempotent and cheap; the marker only skips a scan.
- *Direction:* `migrate_v9` deletes the marker; the marker records whether the LID was known and the repair re-runs
  when that changes; optionally treat a lookup failure during ingest as retryable.

**M2. The deferred repair is the riskiest unit, with the weakest safety net, and its premise no longer holds.**
Design §4 ("both values must be right first"), plan §2.3, §5 R6, §4.
- The 2b rules are identity-based (a target is own when `from_me` is set or `is_owner(sender)`; "two own rows always
  match"), so an own revoke counts without the repair on a paired DB. The repair is now mostly data hygiene plus the
  cursor flag (`src/backfill.rs` ~822 reads it, NULL → false). Say so.
- Its costs: it irreversibly overwrites `sender_jid`; it has no backup on the deferred path (R6) — the very case of a
  profile unpaired at migration; it guesses that the current `device` row owns the stored history; it is a hidden data
  mutation inside `Store::new` / `open_with_mode`, which library consumers also call; it sets a precedent for
  startup-time data repairs outside ADR 0028–0031, with no ADR note.
- *Direction:* reuse `backup_db_pre_migration` before any non-migrating repair that will change rows (the counts are
  known after classification); or offer the repair as an explicit command with dry-run counts and keep the automatic
  run for the migrating start only; record the pattern in an ADR 0028/0031 amendment; consider three commits for 2a
  (schema, repair, ingest).

**M3. Fill-only duplicates freeze the download tuple, and v9 adds no freshness discriminator.** Design §4, §11.1 and
the ADR 0050 retest table; plan §2.5, §5 R4.
- The one unexplained live failure is a fresh `@lid` message where the phone used another media key than the one
  stored from history (it ends as the final `integrity`). A history re-fetch is the one thing that could carry the
  phone's current key and path, and the duplicate rule forbids using it.
- Per-column `COALESCE` can assemble a tuple from two encryptions of one message (a NULL `direct_path` or `media_key`
  filled from the new row): a key that does not match the path → a spurious `integrity`.
- `mediaKeyTimestamp` exists on every media message; v9 does not store it; adding it later is a v10 with the full
  ceremony.
- *Direction (choose now):* a nullable `media_key_timestamp` in v9, with a newer duplicate replacing the whole tuple
  atomically; or accept fill-only and fill the download tuple all-or-nothing (only when `media_key`, `direct_path` and
  `file_enc_sha256` are all NULL).

**M4. Revoke judgement (2b): read-time state versus event-time side effects, and where revoke rows come from.**
Design §3, §5.5, §6.1, §6.2 step 6; plan §3, R7.
- Precedence is unspecified: a downloaded, untagged file whose message becomes judged revoked at read time (after a v8
  window, after the repair flips `from_me`, after rows stored by a build without the hook, after a mapping is learned)
  is both `ready` and `revoked`. Prefer revoked wins, with a lazy idempotent `tag` on read or a startup reconcile
  (tagging the files of messages judged revoked is safe, unlike tagging by absence).
- Revoke rows reach `messages` only through live ingest, which drops a message from a sender not on
  `WHATSRUST_ALLOWED_NUMBERS` (`src/bridge.rs` ~3304–3315); `persist_history_messages` has no allowlist. With an
  allowlist, a non-allowed sender's retraction of a history-stored media message is never recorded. Likewise
  `should_ignore_jid`, dedup and extraction failures. R7's "a 2a-only build loses no revoke" holds only for rows that
  reach `messages`.
- All documented false negatives, plus this one, fail open; state it as the policy.
- Revoke rows are now load-bearing for a privacy decision: note that retention and prune features must keep
  `content_kind = 'revoke'` rows.

## Minor
- **m1.** Recovery overstates `--migrate`: after a post-commit validation failure `user_version` is 9, so the v9 step
  never re-runs and `--migrate` only re-validates after `SCHEMA`; a structural failure stays stuck and `--rollback` is
  the way out (safe: the daemon never started on v9). "Re-runnable" is reachable only through the downgrade; the same
  for a dev DB opened by an interim 2a build at version 9 with another column set. Document it; optionally an
  idempotent `ensure_v9_columns` on the re-validation branch.
- **m2.** The index in both `SCHEMA` and the v9 step: ADR 0031 treats such an index as an unversioned startup outlet,
  so `SCHEMA` alone does. With the index in `SCHEMA`, the failure injection "an object that blocks the index name"
  fails in `open_with_mode` step 4, which writes no pin, so the pin / `--rollback` / `ForceMigrate` path is never
  exercised; use a failure inside the step (for example a view named `media_refs`, so `ALTER TABLE` fails). Plan §2.4's
  order line ("index" inside the transaction) is inaccurate; R4/R6 cross-references are off by one (R3, R5).
- **m3.** The downgrade proof is thin: `…-16a22d2-dirty` has unknown provenance (build v8 from a clean `main` into a
  scratch directory); `--migrate` only opens and validates, not v8 storage behaviour on v9 data (also run the v8
  storage tests or a smoke script on the migrated fixture); the HOWTO procedure needs an external `sqlite3`.
- **m4.** SQL pitfalls: scalar `max(NULL, x)` is NULL in SQLite (use `MAX(COALESCE(stored, 0), new)` or Rust; test
  NULL→0, NULL→1, 1 vs an incoming 0, an incoming `None`); `INSERT OR IGNORE … changed == 0` also covers other ignores —
  define the outcome when the duplicate read finds no stored row (a failure, not a duplicate).
- **m5.** Existing references bypass the view-once refusal: all pre-v9 references have `view_once` NULL ("unknown,
  allowed"); the largest profile has 6,355; only a re-fetch fills the flag. Quantify the exposure in status and HOWTO.
- **m6.** Public surface: `MediaRefRow` (all-pub fields) — consider `#[non_exhaustive]` now, since phases 3–4 may add
  more; keep 2b's `is_revoked` `pub(crate)` until phase 4; `insert_message_if_new` and `insert_media_ref` stay public
  and bypass the one-transaction and duplicate rules.
- **m7.** Budget and operability: the 500,000-row test must include thousands of distinct non-owner history senders
  (each ≤ 500-bind chunk of `sender_jid IN (…)` is a full scan; `sender_jid` is not indexed); say whether the 30 s
  includes the backup and checkpoint; the watchdog baseline is seeded before the repair (seed after it); a failing
  repair is retried every start with only a log line (a persisted failure note or a backoff); an in-transaction
  post-condition (no owner-sender live rows with `from_me = 0` left) before the marker.
- **m8.** Alternatives not weighed: for 1:1 chats, `same_chat` plus `from_me` equality is now enough and removes the
  LID-mapping false negatives (groups still need sender identity). `lid_pn_mapping` is the trust anchor of
  `same_user` (in wa-rs: server stanza attributes and own-phone sync, fine); `user_forms` is one hop only, and a
  reassigned phone number merges users. Say so.

## Questions for the owner
- **Q1.** Is shipping v9 plus the repair to `main` before the engine (R7) acceptable, given every consumer then runs a
  migration with no user-visible benefit yet?
- **Q2.** M3: `media_key_timestamp` in v9, or fill-only and a v10 later?
- **Q3.** M2: a fully automatic repair with no backup on the deferred path, or a pre-repair backup or an explicit
  command?
- **Q4.** M4: when a downloaded file's message is judged revoked at read time, should `get` answer `revoked` and tag
  the file lazily? Is "fail open" the intended policy for admin deletes and allowlist-dropped revokes?
- **Q5.** m5: accept that all pre-v9 references bypass the view-once refusal, or a default-deny switch for NULL?
- **Q6.** Does habb construct `MediaRefRow` literals? If so, `#[non_exhaustive]` plus a minor version bump.

## Assumptions
- The device row, the mapping rows and the stored forms behave as in the shape report; the mapping holds bare LIDs; a
  dotted LID is possible in other deployments.
- One connection behind a mutex serialises the one-transaction write (`Store::run`).
- The 2a code in the working tree was not reviewed.

---

## Reconciliation against v5.2 (coordinator)

| v5.2 finding | Status in v5.3 | Evidence |
|---|---|---|
| M1 repair skipped for good when unpaired | **Resolved**, with a **new residual** (v5.3 M1: the one-shot marker after a downgrade, a partial identity, lookup failures) | design §4 (deferred repair, marker) |
| M2 migration cost | **Resolved in the design** (set-wise, trigger paused, budget); **indeterminate** until the 2a budget test runs; refined by v5.3 m7 | design §4, plan §2.3, §2.7 |
| M3 "no way back" overstated | **Resolved** (in-place downgrade primary, backup fallback); the proof is planned; v5.3 m1/m3 ask for precision and a cleaner v8 build | design §4, plan §2.4, §2.7 |
| M4 inferred revoke key | **Resolved** (the key does not gate; only logged) | design §5.5 |
| M5 order and build dependence | **Resolved** by the read-time judgement; **superseded** in part by v5.3 M4 (precedence with event-time tagging; revokes that never reach `messages`) | design §5.5, plan §3 |
| M6 separate `view_once` walker | **Resolved in the design** (set in the extractor); implementation in 2a | plan §2.6 |
| m1 §11.5 remedy | **Resolved** | design §11 item 5 |
| m2 predicates, dotted LID, NULL cursor flag, missing column | **Resolved** | plan §2.1, §2.3 |
| m3 ambiguous `None` key | **Superseded** (no key in the judgement) | design §5.5 |
| m4 public API | **Partly resolved** (new types `pub(crate)`, CHANGELOG note); `#[non_exhaustive]` re-raised (v5.3 m6) | plan §2.5 |
| m5 `Filled` vs `Unchanged`; `LiveRow` | **Resolved** (real change detection); `LiveRow` **accepted** (live media out of scope) | plan §2.5, §4 |
| m6 missing tests | **Resolved in the plan** | plan §2.7 |
| Q1 real forms | **Answered** (shape report, design §2) | — |
| Q2 unpaired upgrade | **Answered** (defer) | design §9 |
| Q3 2a-only builds on live DBs | **Moot** (read-time judgement) | — |
| Q4 live references | **Answered** (out of scope) | plan §4 |
| Q5 size and downtime | **Answered** for size; the budget set by the coordinator (no owner limit) | design §9 |
| Q6 habb and `MediaRefRow` | **Open** (re-asked as v5.3 Q6) | — |
| Q7 capture real revokes | **Moot** (the key is not used) | — |

**New in v5.3:** M1 (marker), M2 (the repair's safety net and premise), M3 (media-key freshness), M4 (precedence,
revokes that never reach `messages`, fail-open policy, retention); m1–m8; Q1–Q6.

**Trajectory: converging.** The revoke mechanism no longer churns: v5.3's revoke finding is about precedence and
policy, not about the judgement itself. The remaining majors are narrower: the repair's safety net (cheap fixes: a
re-runnable repair, a pre-repair backup) and one new axis, media-key freshness, tied to the one unexplained live
failure. The design is not accumulating complexity to patch a weak base; two of the fixes (dropping the marker's
one-shot role, the all-or-nothing fill) make it simpler.
