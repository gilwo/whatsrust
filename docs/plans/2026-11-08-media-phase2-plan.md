# Media download phase 2 — implementation plan

This plan turns design §13 row 2 (`2026-10-29-on-demand-media-download-design.md`, v5.4) into two implementation runs,
2a and 2b, each one reviewable commit. It comes from a read-only planning pass over the code and two cold reviews
(`_reviewer/design/2026-11-08-on-demand-media-download-v5.md`, `…-v5.3.md`), with the owner's decisions of 2026-11-08
applied. The design stays the specification; this file says where and in which order.

**Status:** v5.4, 2026-11-08. Done: the attribution fix (d2f598c), the LID-chat fix (a72b1f2). Run 2a: done (ae89d33),
with the v5.3 review follow-up (§2.9) and a code review round (§2.10). Run 2b: done (2026-11-08; the allowlist
variable is `WHATSAPP_ALLOWED`). Next: the merge to `main` (owner), then phase 3.

## 1. The split

- **2a — schema v9, the repair, ingest:** six `media_refs` columns and the revoke-label index in a re-runnable v9
  step (`ensure_v9_columns` on the re-validation branch); the own-message repair at every start that knows the
  owner's number; the in-place downgrade proven with a v8 binary, and the HOWTO revert section; history ingest
  writes the new columns, with `view_once` from the extractor; the message and its reference in one transaction;
  the duplicate rule with a whole-tuple fill.
- **2b — revokes judged when read:** `is_revoked`, revoke tombstones stored past the allowlist, the key-shape log.
  `get`, `status` and the engine (phases 4–6) call it; "revoked wins" lives there.

## 2. Run 2a

### 2.1–2.8 As built (first pass)
- **Identity helpers** (`src/storage.rs`): `OwnIdentity`, `read_own_identity` (never aborts; the LID falls back to the
  mapping), `bare_user` (keeps a `.`; moved from `bridge`), `user_forms`, `same_user`, `is_owner` (the one predicate),
  `same_chat`.
- **Schema v9:** the columns in `SCHEMA` and a guarded `migrate_v9`; `CURRENT_SCHEMA_VERSION = 9`; the index in
  `SCHEMA` and the step (ADR 0031: an optional performance index may be created at startup, like
  `idx_messages_embed_status`); validation of the columns, the index and an update-trigger probe (which restores
  its text before the FTS delete). The FTS update trigger text is one shared constant.
- **Ingest:** `MediaRefRow` new fields; `insert_media_ref` writes all columns; `insert_history_message` and its types
  `pub(crate)`, one transaction; the duplicate rule with exact `Filled` / `Unchanged`; `view_once` set in the
  extractor's recursion (`mark_view_once` in the wrapper arms, the leaf flags on image, video/ptv, audio);
  `PersistCounts` with `media_refs_filled`, `media_refs_created`, `duplicate_mismatch`.
- **Errors:** `store_error_text` keeps the cause in migration and validation messages and in the pin `reason`
  (`StoreError::Database` displayed only "database operation error").
- **Tests:** `src/storage/v9_tests.rs` (34 default, 2 ignored, before the code review round) and bridge tests; the failure injected inside the v9
  step makes `media_refs` a view, so `ALTER TABLE` fails inside the transaction and the pin path is exercised.
- **Budget** (`cargo test --lib budget_500k -- --ignored --nocapture`): 500,000 messages, 50,000 rows to repair;
  migration, backup, validation and repair 7.2–8.1 s (debug build); the repair 1.6–1.9 s; WAL peak 77 MiB.
- **Downgrade** (`WHATSRUST_V8_BIN=<v8 build> cargo test --lib downgrade_to_v8 -- --ignored --nocapture`): migrate a
  synthetic v8 DB to v9, set `user_version = 8`, run the v8 build's `--migrate` in an empty temp directory with a
  cleared environment: exit 0 in 0.23 s, `schema_validated_version` back to 8; a v9 reopen migrates again.

### 2.9 Follow-up for the v5.3 review (owner, 2026-11-08)
1. **The repair runs at every start that knows the owner's number** (design §4): the marker only records the number
   (re-pair guard; another number → skip, one warning, a persisted note keeps later starts quiet); classification
   first; a backup `whatsapp.db.pre-repair-<ts>.bak` before it changes rows outside the migrating start (a failed
   backup skips the repair); an in-transaction post-condition; a persisted failure note, cleared on success; the
   watchdog baseline seeded after the repair; the time of a no-op run reported.
2. **`media_key_timestamp`** (sixth column, after `view_once`; from `mediaKeyTimestamp`); the encryption tuple
   (`media_key`, `direct_path`, `file_enc_sha256`, `media_key_timestamp`) filled only as a whole, when the stored
   three key fields are all NULL; the plaintext properties per column; `view_once` max.
3. **`ensure_v9_columns`** on the re-validation branch; a test where `ForceMigrate` adds a missing column.
4. `view_once` merge tests: NULL + 0 → 0, NULL + 1 → 1, 1 + 0 → 1, 0 + None → 0.
5. The budget fixture with about 5,000 distinct non-owner history senders.
6. Docs: CHANGELOG, HOWTO (the pre-repair backup, the notes, how to build a v8 binary for the downgrade test),
   ARCHITECTURE, AGENTS.md.

### 2.10 Code review round (2026-11-08)
An independent code review of 2a (0 critical, 3 medium, 7 low) led to:
- a 24 h backoff after a failed repair, which reuses the failed attempt's backup; only the newest 3 pre-repair
  backups kept; no leaked `.bak.tmp`;
- `ensure_v9_columns` at every open at v9; a `warn!` with the `failed` count when a history write drops a message;
- `--migrate` prints the cause (`store_error_text`), and a failed forced re-validation writes the breaker pin, so
  `--rollback` stays available;
- the duplicate fill as a Rust merge in the same transaction (the tuple fills only from a complete incoming tuple,
  with its own timestamp);
- the validation probe inside a rolled-back transaction; backup files created with mode 0600 before they are
  written; a "pin inconsistent" message that points at `--rollback` / `--migrate`; tighter tests.

## 3. Run 2b

1. **`is_revoked`** (`src/storage.rs`): `pub(crate) fn is_revoked_conn(conn, own: &OwnIdentity, message_id) ->
   Result<bool>` and a `pub(crate) async fn is_revoked(&self, message_id)` (crate-private until phase 4 calls it).
   Load the target (`chat_jid, sender_jid, from_me`); find revoke rows through the index (`content_kind = 'revoke'
   AND body_text = '[deleted ' || ?1 || ']'`); a revoke counts when `same_chat` and its revoker is the target's
   author:
   - **1:1 chat:** the revoke row's `from_me` equals the target's `from_me`;
   - **group:** an own target (`from_me = 1`) needs the revoker to be the owner (`from_me = 1` or `is_owner`); any
     other target needs a revoker that is not the owner and `same_user(revoker, target sender)`.
   No target → false.
2. **Tombstones past the allowlist** (`src/bridge.rs` `handle_event`): when the allowlist would drop a message and it
   is a sender revoke (`InboundContent::Revoke`), store its revoke row with `insert_inbound` and stop there: no
   event, no inbound dispatch, no read receipt, no dedup change beyond the existing one. Every other dropped message
   stays dropped.
3. **The key-shape log:** where the revoke is extracted, one `debug!` with the key's `from_me` (true / false /
   absent) and whether a participant is present; no JIDs, no IDs. `InboundContent` does not change.
4. **Tests:** an own revoke counts (after the repair; on an unpaired DB, not yet); a revoke by the author in a 1:1
   chat (by `from_me`) and in a group (phone vs LID form through the mapping); an admin's delete and a forged revoke
   do not count; another chat does not count; a revoke stored before its target counts once the target is stored; a
   label that is only a prefix does not match; the query uses the index (`EXPLAIN QUERY PLAN`); a revoke from a
   non-allowed sender is stored as a tombstone (and nothing else is stored, dispatched or receipted); a non-revoke
   message from that sender stays dropped.
5. **Docs:** CHANGELOG Added (`is_revoked`; tombstones past the allowlist); ARCHITECTURE (revokes judged when read);
   AGENTS.md Patterns (the judgement, the tombstones, "revoke rows must be kept by retention"); HOWTO where the
   allowlist is described; FEATURES and roadmap status.

## 4. Decisions applied (owner, 2026-11-08, unless marked)

| Topic | Decision |
|---|---|
| Revoke model | Judged when read from the stored revoke rows; no `revoked_at`, no one-time pass; the key shape only logged |
| Revoke policy | Revoked wins (a downloaded file is tagged lazily); tombstones stored past the allowlist; remaining false negatives fail open |
| 1:1 revokes | Judged by `from_me` equality (coordinator, from the v5.3 review m8) |
| Repair | At every start that knows the owner's number; a backup before it changes rows outside the migrating start; the marker is the re-pair guard |
| Repair scope | Live rows whose sender is the owner (every form, the mapping LID too); history own rows whose sender is not the owner; the cursor flag (0 or NULL) |
| Media key | `media_key_timestamp` in v9; the encryption tuple filled only as a whole |
| Revert | The in-place downgrade (proven); restoring the backup is the fallback |
| Budget | 30 s for 500,000 messages, backup included (coordinator; the owner gave no limit); measured 7.2–8.1 s |
| Duplicates | 1:1 chats as the same user, groups exact; two own rows match; `duplicate_mismatch` for media only; `view_once` never lowered |
| Pre-v9 `view_once` | NULL stays "unknown, allowed" (6,355 references in the largest profile); `status` says so (coordinator) |
| `MediaRefRow` | Public, no `#[non_exhaustive]` (a downstream `..Default::default()` keeps working); the field change noted in the CHANGELOG (coordinator) |
| Live references | Out of scope (owner, 2026-11-03) |
| Merge | Phase 2 to `main` after 2b |

## 5. Remaining risks

- **R1 Bare-number collisions:** a LID user equal to a phone number would confuse bare live values; negligible.
- **R2 Live `view_once` flag:** the live path misses some wrappers; live media is out of scope.
- **R3 A failure in `SCHEMA` writes no breaker pin:** pre-existing.
- **R4 Fill-only duplicates:** a newer encryption is stored (`media_key_timestamp`) but not yet used to replace an
  older tuple; a later rule needs no migration.
- **R5 One transaction can drop a message:** a failed reference insert drops its message too (accepted).
- **R6 Backups of the same second share a name:** two backups within one second overwrite each other (rare).
- **R7 A reassigned phone number** would merge two users through the mapping (one hop; documented).
