# Media download phase 2 — implementation plan

This plan turns design §13 row 2 (`2026-10-29-on-demand-media-download-design.md`, v5.3) into two implementation runs,
2a and 2b, each one reviewable commit. It comes from a read-only planning pass over the code and the v5.2 cold review
(`_reviewer/design/2026-11-08-on-demand-media-download-v5.md`), with the owner's decisions of 2026-11-08 applied. The
design stays the specification; this file says where and in which order. Line numbers are approximate and move.

**Status:** v5.3, 2026-11-08. Done: the own-message attribution fix for new rows (d2f598c), the LID-chat fix
(a72b1f2). Next: **2a**. **2b** starts after the cold review of design v5.3 (owner).

## 1. The split

- **2a — schema v9, the repair, ingest:** the five `media_refs` columns and the revoke-label index in a re-runnable v9
  step; the deferred own-message repair (marker, set-wise, trigger paused); the in-place downgrade proven with the v8
  binary, and the HOWTO revert section; history ingest writes the new columns, with `view_once` from the extractor;
  the message and its reference in one transaction; the duplicate rule.
- **2b — revokes judged when read:** `is_revoked(message_id)` in storage, the key-shape debug log, tests. Callers
  (`get`, `status`, the engine) come in phases 4–6.

## 2. Run 2a

### 2.1 Identity helpers (`src/storage.rs`, sync on `&Connection`)
- `pub(crate) struct OwnIdentity { phone: Option<String>, lid: Option<String> }`: bare user parts (no server, no
  device). `fn phone_jid(&self) -> Option<String>`.
- `fn read_own_identity(conn) -> OwnIdentity`: `SELECT pn, lid FROM device WHERE id = 1`; reuses
  `phone_jid_without_device`; adds `lid_user_without_device`. **Any error** (no table, a missing column, no row, an
  empty `pn`) → no identity, never an abort. When `device.lid` is empty, take the LID from `lid_pn_mapping` by the
  own number. `own_phone_jid` (existing) delegates to it.
- `pub(crate) fn bare_user(s) -> &str`: the part before `@`, then before `:`. **A `.` stays** (wacore keeps it in a LID
  user; the owner's DB has no dotted form, 2026-11-08). Move `bridge::bare_lid_user` here.
- `fn user_forms(conn, s) -> Vec<String>`: the bare form plus its twins from `lid_pn_mapping` (LID → phone like
  `get_lid_mapping`, phone → LID all rows; a value without a server is looked up both ways).
- `fn same_user(conn, a, b) -> bool` (the form sets intersect); `fn is_owner(conn, own, s) -> bool` (the forms of `s`
  include `own.phone` or `own.lid`) — **the one predicate for both repairs and for revokes**; `fn same_chat(conn, a,
  b) -> bool`: groups (`@g.us`) exact, 1:1 chats `same_user`.

### 2.2 Schema v9
- `SCHEMA` (`media_refs`): append `file_name TEXT, file_sha256 BLOB, remote_jid TEXT, participant TEXT, view_once
  INTEGER` in this order. **No `revoked_at`.** No index on a new column in `SCHEMA` (it runs before the migration; a
  failure there writes no breaker pin, R4).
- The revoke-label index uses existing columns:
  `CREATE INDEX IF NOT EXISTS idx_messages_revoke_label ON messages(body_text) WHERE content_kind = 'revoke';`
  Create it in the v9 step and in `SCHEMA` (safe on a v8 DB: the columns exist). Follow ADR 0031; if it forbids a
  `SCHEMA` object without a version bump, create it in the v9 step only and say so.
- `CURRENT_SCHEMA_VERSION = 9`; in `run_schema_migrations`, `if from_version < 9 { migrate_v9(conn)?; }` after the
  `< 8` block (a v7 DB passes through it), inside the existing `BEGIN IMMEDIATE` transaction.
- `migrate_v9(conn)`: each column added only when missing (`SELECT COUNT(*) FROM pragma_table_info('media_refs')
  WHERE name = ?1`, the v4→v5 `add_col` pattern), then the index (`IF NOT EXISTS`). **Nothing else**: no repair, no
  pass. Re-runnable by construction.
- `validate_migration_post_commit`: the five columns, the index, and an **update-trigger probe** (insert a row,
  update its `body_text`, check the FTS match, delete it).

### 2.3 The deferred repair (`src/storage.rs`)
- `fn repair_own_attribution(conn) -> Result<Option<RepairReport>>`, called from `open_with_mode` after a successful
  open at v9 (the migrating start, and every later start until it has run). It is a no-op when the marker
  `own_attribution_repair` exists with the same number, and when there is no identity.
- Marker: `metadata` key `own_attribution_repair`, value `<bare number> <epoch seconds>`. A marker with **another**
  number → skip and warn loudly (stderr and `warn!`): the DB was kept across a re-pair to another number.
- In one transaction:
  1. `DROP TRIGGER messages_fts_update;` (the text does not change).
  2. Classify the distinct senders once: `SELECT DISTINCT sender_jid FROM messages WHERE (source = 'live' AND
     from_me = 0) OR (source IN ('bootstrap','backfill') AND from_me = 1)`; `is_owner` each.
  3. Live: `UPDATE messages SET from_me = 1 WHERE source = 'live' AND from_me = 0 AND sender_jid IN (<owner
     senders>)` (bind the list in chunks of ≤ 500).
  4. History: `UPDATE messages SET sender_jid = ?own_phone_jid WHERE source IN ('bootstrap','backfill') AND
     from_me = 1 AND sender_jid IN (<non-owner senders>)` (chunks).
  5. Cursor:
     ```sql
     UPDATE backfill_cursor SET oldest_msg_from_me = 1
      WHERE (oldest_msg_from_me = 0 OR oldest_msg_from_me IS NULL)
        AND EXISTS (SELECT 1 FROM messages m WHERE m.message_id = backfill_cursor.oldest_msg_id
                    AND m.chat_jid = backfill_cursor.chat_jid AND m.from_me = 1);
     ```
  6. Re-create `messages_fts_update` with the exact `SCHEMA` text (share one constant).
  7. Write the marker.
- Log the three counts and the elapsed time (no JIDs). A failure rolls the transaction back (the trigger comes back
  with it) and logs; the start continues, and the next start retries.

### 2.4 How it fits the staged migration (ADR 0028–0032)
- v8 DB: pin check → WAL checkpoint → pre-migration backup (mode 600) → FTS5 probe → `SCHEMA` → transaction { v9
  columns → index → `user_version = 9` } → commit → validation → `schema_validated_version = 9` → watchdog seed →
  **the repair** (if an identity is known).
- A failure inside the migration transaction rolls it back; the pin `{failed, pinned 8, blocked 9}`; `--rollback`
  restores the backup. A validation failure after the commit: "pin inconsistent" on the next start; `--rollback` or
  `--migrate` recover.
- **The in-place downgrade** (primary revert after a healthy migration): stop the daemon; `PRAGMA user_version = 8`;
  start the v8 build, which re-validates at its own version. A later v9 start migrates again (re-runnable step; the
  repair marker stays, so the repair does not run twice).

### 2.5 Ingest API (`src/storage.rs`)
- `MediaRefRow` (public) gains `file_name: Option<String>, file_sha256: Option<Vec<u8>>, remote_jid: Option<String>,
  participant: Option<String>, view_once: Option<bool>`; fix every literal (incl. `probe_row_from` under
  `media-probe`). The CHANGELOG notes the public field change.
- `insert_media_ref` keeps its signature and writes all columns.
- New, in one transaction (`unchecked_transaction`); the new types are `pub(crate)` where the call sites allow:
  ```rust
  pub(crate) struct HistoryMessageRow { chat_jid, sender_jid, message_id, content_kind, body_text: Option<String>,
                                        timestamp: i64, from_me: bool, source, embed_status }
  pub(crate) enum DuplicateMedia { NoMedia, Unchanged, Filled, Created, LiveRow, Mismatch }
  pub(crate) enum HistoryWrite { Stored { media_ref: bool }, Duplicate(DuplicateMedia) }
  pub(crate) async fn insert_history_message(&self, row: HistoryMessageRow, media: Option<MediaRefRow>)
      -> Result<HistoryWrite>
  ```
- New row: `INSERT OR IGNORE INTO messages`, then the reference with all columns; a failed reference insert rolls
  the message back (R6).
- Duplicate with media: read the stored `chat_jid, sender_jid, from_me, content_kind, source`. Match = `same_chat`,
  equal kind, equal `from_me`, sender `same_user` (two own rows always match). Mismatch → `Mismatch`, nothing
  changes. Match and a reference exists → fill: `COALESCE` for every reference column, `view_once = max(stored,
  new)`; never `hydrated_path`. **Real change detection** (compare before/after, or an `UPDATE … WHERE` that only
  matches when a value would change) → `Filled` or `Unchanged`. Match and no reference → create when `source <>
  'live'` (`Created`), else `LiveRow`.

### 2.6 History ingest (`src/bridge.rs`)
- `media_ref_row` gains `file_name: Option<&str>` and sets `file_sha256` from `Downloadable::file_sha256()`.
- `MediaMode::bytes` gains `file_name`; the document branches pass the raw `doc.file_name` (not the `"file"`
  display fallback); `cached_media_ref` likewise.
- **`view_once` inside the extractor:** `MediaMode::Reference` gets a `view_once: bool` that the recursion sets when
  it enters a `view_once_message`, `_v2` or `_v2_extension` arm (also below an ephemeral wrapper), and that the
  image, video/ptv and audio branches set from the message's own `view_once` field. No separate walker.
- `persist_history_messages`: set `remote_jid` and `participant` from the raw key and `view_once` from the mode;
  call `insert_history_message`; log `duplicate_mismatch` once per batch when above zero (media duplicates only).
- `PersistCounts`: remove `media_refs_failed`; add `media_refs_filled`, `media_refs_created`, `duplicate_mismatch`
  (sub-counts of `duplicates`); update the log lines in `store_unmatched_history`.

### 2.7 Tests (2a)
- **Fixture:** a real-shaped v8 DB (the v8 `media_refs` DDL first, then the current `SCHEMA`; `user_version = 8`,
  `schema_validated_version = '8'`), with a device row (`pn '15550001111:12@s.whatsapp.net'`, `lid
  '100000000000009:12@lid'`), `lid_pn_mapping` rows (one dotted LID too), live received and own rows (bare, `:12`,
  full JID, LID user), history own rows naming the group, the peer (phone and unmapped `@lid`) and the owner, received
  history rows, v8 `media_refs` rows, revoke rows, cursors with flag 0 and NULL.
- **Migration:** fresh DB (v9, five columns, the index, the same `pragma_table_info` as a migrated DB); v8 → v9 and
  v7 → v9 (the `.bak` is the v8 file); a re-run of `migrate_v9` on a v9 DB changes nothing; the failed-migration path
  (a failure injected inside the v9 step from the fixture, for example an object that blocks the index name; no
  production hook; then the pin, `--rollback`'s file steps, `ForceMigrate`); a post-commit validation failure → "pin inconsistent" → `--migrate` recovers; the
  update-trigger probe.
- **Repair:** paired at migration (counts; the marker); unpaired at migration, then a device row, then a reopen
  (repairs then); the marker with the same number (no second run) and with another number (skipped, warned); a
  missing `device` column (no identity, no abort); `device.lid` empty (the LID from the mapping); a dotted LID; the
  cursor flag 0 and NULL; the trigger exists after the repair and FTS still matches the repaired rows.
- **Budget:** a synthetic DB of 500,000 messages with 50,000 rows to repair: migration plus repair under 30 s;
  print the time and the WAL peak (mark the test `#[ignore]` if it is slow in the default suite, and report its
  numbers).
- **Downgrade:** with the v8 binary (`target/debug/whatsrust-0.3.0-55cbe70-dirty`, built before v9; or a build of
  `main`), in a temp directory with no `.env`: migrate a synthetic v8 DB to v9 with the current code, set
  `PRAGMA user_version = 8`, run the v8 binary's `--migrate` (offline: it opens, validates and exits; it must never
  connect), check it accepts the DB; then reopen with the v9 code (it migrates again, no second repair). An
  `#[ignore]` integration test or a script under `scripts/`; never touch a real DB.
- **Ingest:** the columns; the raw `file_name`, NULL for a document without one; a raw LID `remote_jid` while the
  chat resolves to a phone; `view_once` table test against the extractor's wrapper set (each wrapper, nested in
  ephemeral, the leaf flag on image, video, ptv, audio; 0 for plain media).
- **One transaction:** with `media_refs` dropped, `failed: 1` and no message row.
- **Duplicates:** fill the NULL columns of a v8 reference (`Filled`), the same data again (`Unchanged`), a 1 never
  lowered to 0; create for `bootstrap`/`backfill`, not `live`; each mismatch (chat, sender, `from_me`, kind) counts;
  phone vs LID sender and 1:1 chat through the mapping; a live bare sender matches a history full JID.
- **Existing tests:** hard-coded 8 → `CURRENT_SCHEMA_VERSION`; pins written with `blocked_target: 8` become `pinned 8
  / blocked CURRENT` on a v8 DB.
- Also build `cargo clippy --features media-probe` and `cargo clippy --tests`.

### 2.8 Docs (2a)
- `CHANGELOG.md` `[Unreleased]`: Added (schema v9: five columns, the revoke-label index, `view_once`); Changed (one
  transaction, the duplicate rule and `duplicate_mismatch`, the public `MediaRefRow` fields); Fixed (extend the
  attribution line: stored rows and the cursor flag are repaired once at the first start that knows the number).
- `docs/HOWTO.md` §6: `--rollback` serves failed migrations only. New **6c "Going back after a healthy v8 → v9
  migration"**: (1) the in-place downgrade (stop the daemon; `sqlite3 whatsapp.db 'PRAGMA user_version = 8'`;
  start a v8 build, a versioned `whatsrust-<ver>-<commit>`; a v9 build migrates again); (2) the fallback, restoring
  the newest `whatsapp.db.pre-migration-v8-<ts>.bak` (stop; keep the v9 files aside; no `-wal`/`-shm` left; remove a
  stray `.migration-pin`; the losses: everything since, the Signal state, maybe a re-pair). Keep the v8 `.bak` until
  the engine is validated. After a validation failure the start says "pin inconsistent": use `--rollback` or
  `--migrate`, do not delete the pin. Never let a copied paired DB connect to WhatsApp. Pair before upgrading when
  possible (the repair waits for the number); the re-pair-to-another-number limit.
- `ARCHITECTURE.md`: schema v9 (columns, index), the repair at startup, the one-transaction write, the duplicate rule.
- `AGENTS.md`: "currently v8" → v9; the `src/storage.rs` key file gets the v9 columns and the repair.
- `README.md`: schema v8 → v9 where stated.
- `FEATURES.md` F1 and the roadmap: status; issue rows for what the run finds.

## 3. Run 2b (after the v5.3 cold review)

- `pub(crate) fn is_revoked_conn(conn, own: &OwnIdentity, message_id) -> Result<bool>` and `pub async fn
  is_revoked(&self, message_id) -> Result<bool>`: load the target (`chat_jid, sender_jid, from_me`); find revoke rows
  through the index (`content_kind = 'revoke' AND body_text = '[deleted ' || ?1 || ']'`); a revoke counts when
  `same_chat` and its revoker is the target's author (an own target: the revoker's `from_me = 1` or `is_owner`; else
  not the owner and `same_user(revoker, target sender)`). No target → false.
- The key-shape log: where the revoke is extracted (`InboundContent::Revoke`), one `debug!` with the key's
  `from_me` (true / false / absent) and whether a participant is present; no JIDs, no IDs. `InboundContent` does
  not change.
- **Tests:** an own revoke counts (after the repair; and before it, not, on an unpaired DB); a revoke by the author in
  a 1:1 chat and a group, phone vs LID form through the mapping; an admin's delete and a forged revoke do not count;
  another chat does not count; a revoke stored before its target counts once the target is stored; a label that is
  only a prefix does not match; the query uses the index (`EXPLAIN QUERY PLAN`).
- **Docs:** CHANGELOG Added (`is_revoked`); ARCHITECTURE (revokes judged when read); AGENTS.md Patterns line;
  FEATURES and roadmap status; a dated design/ADR note if the cold review changes the rule.

## 4. Decisions applied (owner, 2026-11-08, unless marked)

| Topic | Decision |
|---|---|
| Revoke model | Judged when read from the stored revoke rows; no `revoked_at`, no one-time pass; the key shape only logged |
| Repair timing | Once at the first start that knows the owner's number; a marker records it; another number → skipped with a warning |
| Repair scope | Live rows whose sender is the owner (every form, the mapping LID too); history own rows whose sender is not the owner; the cursor flag (0 or NULL) |
| Revert | The in-place downgrade, once a test with the v8 binary proves it; restoring the backup is the fallback |
| Budget | 30 s for 500,000 messages (coordinator; the owner gave no limit). The owner's largest profile: 26,080 messages, 963 rows to repair |
| Duplicates | 1:1 chats as the same user, groups exact; two own rows match; `duplicate_mismatch` for media only; fill NULL columns; `view_once` never lowered |
| `view_once` | Set in the extractor's recursion: a wrapper or the media message's own flag |
| LID 1:1 chats | Fixed for new rows (a72b1f2); earlier rows keep the wrong chat; only `clear-data` and a new fetch bring them back |
| Public API | New types `pub(crate)` where possible; the `MediaRefRow` field change noted in the CHANGELOG (no answer yet on whether habb builds it) |
| Live references | Still out of scope (owner, 2026-11-03): a `LiveRow` duplicate gets no reference |

## 5. Remaining risks

- **R1 Bare-number collisions:** a LID user equal to a phone number would confuse bare live values; negligible.
- **R2 Live `view_once` flag:** the live path misses some wrappers; live media is out of scope.
- **R3 A failure in `SCHEMA` writes no breaker pin:** pre-existing; the reason for no new-column index in `SCHEMA`.
- **R4 Fill-only duplicates:** a re-fetch never refreshes a stale `direct_path` or `media_key`; an expired link goes
  through the re-upload.
- **R5 One transaction can drop a message:** a failed reference insert drops its message too (accepted).
- **R6 The deferred repair has no backup of its own** when it runs after the migrating start (narrow, idempotent,
  harmless to v8).
- **R7 Merge timing:** phase 2 may merge before the engine; the owner decides. With revokes judged when read, a
  2a-only build loses no revoke.
