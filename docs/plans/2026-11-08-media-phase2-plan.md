# Media download phase 2 — implementation plan

This plan turns design §13 row 2 (`2026-10-29-on-demand-media-download-design.md`, v5.2) into two implementation runs,
2a and 2b, each one reviewable commit. It comes from a read-only planning pass over the code (2026-11-08), with the
owner's decisions applied. The design stays the specification; this file says where and in which order. Line numbers
are approximate (branch `feature/media-download` at 1fd9b05) and move as code changes.

**Status:** 2026-11-08. Done before 2a: the own-message attribution fix for new rows (d2f598c). Next: the LID-chat
fix commit (design §2, §11), then **2a**. **2b** starts after the cold review of design v5.2 (owner, 2026-11-08).

## 1. The split

- **2a — the whole v9 step and the ingest columns:** the six `media_refs` columns; the attribution repair (incl. the
  fetch cursor's anchor flag); the one-time revoke pass (revoker-is-author test only); validation; the HOWTO manual
  revert; history ingest writes the new columns and `view_once`; the message and its reference in one transaction;
  the duplicate rule.
- **2b — the revoke key at ingest:** a helper reads the revoke key from the raw message; resolution into
  `revoked_at` when live and history revoke rows are stored. It reuses the 2a helpers and adds the key check.

**Why the pass is in 2a:** `run_schema_migrations` (`src/storage.rs` ~2812) runs each step once per DB
(`if from_version < N`). A pass added to the v9 step in a later commit would never run on a DB a 2a build already
migrated (the owner's dev profiles). Fallback if 2a is too large to review: 2a = the migration only (columns, repair,
pass, HOWTO), 2b = all ingest work; the whole v9 step still lands in one commit.

## 2. Run 2a

### 2.1 Identity helpers (`src/storage.rs`, sync on `&Connection`, usable inside the migration)
- `pub struct OwnIdentity { pub phone: Option<String>, pub lid: Option<String> }` — bare user parts (no server, no
  device); `fn phone_jid(&self) -> Option<String>`.
- `fn read_own_identity(conn) -> rusqlite::Result<OwnIdentity>`: `SELECT pn, lid FROM device WHERE id = 1`; reuses
  `phone_jid_without_device`; adds `lid_user_without_device` (`device.lid` is `Jid::to_string()`,
  `<lid>[:<dev>]@lid`). **When `device.lid` is empty**, take the own LID from `lid_pn_mapping` by the own number.
  `pub async fn own_identity(&self)`; `own_phone_jid` delegates to it.
- `pub(crate) fn bare_user(s) -> &str` strips `@server` and `:device`; move `bridge::bare_lid_user` here.
- `fn user_forms(conn, s) -> Vec<String>`: the bare form plus its twins from `lid_pn_mapping` (LID → phone like
  `get_lid_mapping`; phone → LID like `get_pn_mapping`, all rows); a value without a server (live rows) is looked up
  both ways.
- `fn same_user(conn, a, b) -> bool` (the form sets intersect); `fn is_own(conn, own, s) -> bool` (the forms of `s`
  include `own.phone` or `own.lid`); `fn same_chat(conn, a, b) -> bool`: groups (`@g.us`) exact, 1:1 chats
  `same_user`.

### 2.2 Schema
- `SCHEMA` (`media_refs`, ~227): append `file_name TEXT, file_sha256 BLOB, remote_jid TEXT, participant TEXT,
  revoked_at INTEGER, view_once INTEGER` in this order (a fresh DB matches a migrated one). **No index on the new
  columns in `SCHEMA`:** it runs before the migration step, so a v8 DB would fail there, and a failure at that point
  writes no breaker pin (pre-existing gap, risk R4).
- `CURRENT_SCHEMA_VERSION = 9`. In `run_schema_migrations`, `if from_version < 9 { report.v9 = migrate_v9(conn)?; }`
  after the `< 8` block (a v7 DB passes through it), inside the existing `BEGIN IMMEDIATE` transaction. Return a
  `MigrationReport` with `V9Report { repair: Option<RepairCounts>, revokes_applied, revokes_not_valid }`.
- `open_with_mode` prints the counts with `eprintln!` after the commit (tracing is not up yet when `Store::new`
  runs). Counts only, never JIDs.
- `validate_migration_post_commit`: check the six columns through `pragma_table_info('media_refs')`, like the
  `messages` loop; optionally a probe that inserts, reads and deletes a `media_refs` row with the v9 columns.

### 2.3 `migrate_v9(conn)` — in one transaction, after the pre-migration backup
a. **Columns**, each guarded by `SELECT COUNT(*) FROM pragma_table_info('media_refs') WHERE name = ?1` (the v4→v5
   `add_col` pattern; a fresh DB already has them from `SCHEMA`):
   `ALTER TABLE media_refs ADD COLUMN file_name TEXT;` … `file_sha256 BLOB`, `remote_jid TEXT`, `participant TEXT`,
   `revoked_at INTEGER`, `view_once INTEGER`.
b. `let own = read_own_identity(conn)?`. No own number (no row, empty `pn`) → skip the repair; print the skip only
   when `messages` has rows.
c. **Live repair:** for `source = 'live' AND from_me = 0` rows, set `from_me = 1` when `bare_user(sender_jid)` is
   `own.phone` or `own.lid` (covers bare, `:<device>`, and full-JID forms). Implement as a candidate scan in Rust
   with an `UPDATE … WHERE id = ?` per hit (or one `UPDATE` with the computed form list); no `LIKE`.
d. **History repair:** for `source IN ('bootstrap', 'backfill') AND from_me = 1` rows whose sender is **not**
   `is_own` (the old fallback named the group or the other person, in phone or LID form), set
   `sender_jid = own.phone_jid()`. Rows already naming the owner (a participant) are untouched; a self-chat row whose
   sender is already the own JID is not counted.
e. **Cursor flag:**
   ```sql
   UPDATE backfill_cursor SET oldest_msg_from_me = 1
    WHERE oldest_msg_from_me = 0
      AND EXISTS (SELECT 1 FROM messages m
                   WHERE m.message_id = backfill_cursor.oldest_msg_id
                     AND m.chat_jid = backfill_cursor.chat_jid AND m.from_me = 1);
   ```
f. **One-time revoke pass** (after c–e). Candidates (the label `[deleted ` is 9 characters):
   ```sql
   SELECT r.chat_jid, r.sender_jid, r.from_me, r.timestamp, t.chat_jid, t.message_id, t.sender_jid, t.from_me
     FROM messages r
     JOIN messages t ON t.message_id = substr(r.body_text, 10, length(r.body_text) - 10)
                    AND t.message_id <> r.message_id
     JOIN media_refs mr ON mr.message_id = t.message_id AND mr.revoked_at IS NULL
    WHERE r.content_kind = 'revoke' AND length(r.body_text) > 10
      AND substr(r.body_text, 1, 9) = '[deleted ' AND substr(r.body_text, -1) = ']'
    ORDER BY r.timestamp, r.id;
   ```
   For each: `same_chat(r.chat, t.chat)`, then `revoker_is_author`: the revoker is the owner when `r.from_me = 1` or
   `is_own(r.sender)`; an own target (`t.from_me = 1`) is valid only when the revoker is the owner; any other target
   is valid when the revoker is not the owner and `same_user(r.sender, t.sender)` (so an admin delete by the owner
   does not count). Valid → `UPDATE media_refs SET revoked_at = ?1 WHERE message_id = ?2 AND revoked_at IS NULL`
   (`?1` = the revoke's timestamp; the earliest valid revoke wins). Count applied and not valid.
   Build it as `fn apply_revoke_conn(conn, own, revoke: &RevokeSide, target_id, key: Option<&RevokeKey>) ->
   RevokeOutcome`; the pass passes `key = None`, 2b passes `Some`.

### 2.4 How it fits the staged migration (ADR 0028–0032)
- v8 DB: pin check → WAL checkpoint → **pre-migration backup** `whatsapp.db.pre-migration-v8-<ts>.bak` (mode 600) →
  FTS5 probe → `SCHEMA` → transaction { columns → repair → cursor → pass → `user_version = 9` } → commit → validation
  → `schema_validated_version = 9` → watchdog seed.
- A failure inside the transaction rolls all of it back (SQLite DDL is transactional): the DB stays v8, unrepaired,
  with a pin `{failed, pinned 8, blocked 9}`. A normal start halts with the breaker text; `--rollback` (needs pin
  `failed`) restores the newest `*.pre-migration-v*.bak` and writes `{rolled_back, 8}`; the v9 binary then refuses
  until `--migrate`.
- A validation failure after the commit: the DB is v9 and repaired with a `failed` pin; `--rollback` restores the v8
  backup (which also undoes the repair).
- After a healthy migration there is no pin, so `--rollback` refuses: only the manual revert (§2.8) goes back.

### 2.5 Ingest API (`src/storage.rs`)
- `MediaRefRow` gains `file_name: Option<String>, file_sha256: Option<Vec<u8>>, remote_jid: Option<String>,
  participant: Option<String>, view_once: Option<bool>` (not `revoked_at`; ingest never writes it). Fix every struct
  literal (incl. `probe_row_from` under feature `media-probe`, test helpers).
- `insert_media_ref` keeps its signature (public API) and writes all columns.
- New, one storage call in one transaction (`unchecked_transaction`):
  ```rust
  pub struct HistoryMessageRow { chat_jid, sender_jid, message_id, content_kind, body_text: Option<String>,
                                 timestamp: i64, from_me: bool, source, embed_status }
  pub enum DuplicateMedia { NoMedia, Unchanged, Filled, Created, LiveRow, Mismatch }
  pub enum HistoryWrite { Stored { media_ref: bool }, Duplicate(DuplicateMedia) }
  pub async fn insert_history_message(&self, row: HistoryMessageRow, media: Option<MediaRefRow>) -> Result<HistoryWrite>
  ```
- New row: `INSERT OR IGNORE INTO messages`, then `INSERT OR IGNORE INTO media_refs` with all columns; a failed
  reference insert rolls the message back too (risk R6).
- Duplicate with media: read the stored `chat_jid, sender_jid, from_me, content_kind, source`. Match = `same_chat`,
  equal kind, equal `from_me`, and the sender `same_user` (two own rows always match). Mismatch → nothing changes,
  `Mismatch`. Match and a reference exists → fill NULL columns with `COALESCE` (all reference columns incl. the new
  ones; never `hydrated_path` or `revoked_at`) → `Filled` or `Unchanged`. Match and no reference → create it when
  `source <> 'live'` (`Created`), else `LiveRow`.

### 2.6 History ingest (`src/bridge.rs`)
- `media_ref_row` gains `file_name: Option<&str>` and sets `file_sha256` from `Downloadable::file_sha256()`.
- `MediaMode::bytes` gains `file_name: Option<&str>`; the document branches pass the raw `doc.file_name` (not the
  `"file"` display fallback); other kinds pass None. `cached_media_ref` likewise.
- `fn view_once_of(msg: &wa::Message) -> bool`: true for `view_once_message`, `_v2`, `_v2_extension` (also inside
  `ephemeral_message`), **or** the inner image, video or audio message's own `view_once` flag.
- `persist_history_messages`: after `into_media_ref()` set `remote_jid` and `participant` from the raw key and
  `view_once = Some(view_once_of(msg))`; replace `insert_message_if_new` + `insert_media_ref` with
  `insert_history_message`; after the loop log `duplicate_mismatch` once when above zero (a count only).
- `PersistCounts`: remove `media_refs_failed`; add `media_refs_filled`, `media_refs_created`, `duplicate_mismatch`
  (media duplicates only; sub-counts of `duplicates`, so `stored + duplicates` stays as the backfill sink expects).
  Update the log lines in `store_unmatched_history`.

### 2.7 Tests (2a)
- **Fixture:** a real-shaped v8 DB built in the storage tests: the v8 `media_refs` DDL first, then the current
  `SCHEMA` (its `CREATE TABLE IF NOT EXISTS` keeps the v8 table); `user_version = 8`, `schema_validated_version =
  '8'`; a device row (`pn '15550001111:12@s.whatsapp.net'`, `lid '100000000000009:12@lid'`) and `lid_pn_mapping`
  rows; live received rows; live own rows as bare number, with `:12`, as full JID, as LID user, LID with `:12`;
  history own rows in a group and a 1:1 chat (sender = chat, and an unmapped `@lid` form); an own row with a
  participant; received history rows; v8 `media_refs` rows; revoke rows; a cursor anchored on an own live row.
- **Fresh DB:** v9, the six columns, `schema_validated_version = '9'`, the same `pragma_table_info('media_refs')` as
  a migrated DB.
- **v8 → v9 and v7 → v9:** the columns; the `.bak` is the unrepaired v8 file; the counts.
- **Repair:** every live own form → 1; received unchanged; own history rows not naming the owner → own phone JID;
  the own row with a participant unchanged; the cursor flag; no device row and an empty `pn` → nothing changes and the
  migration completes; an empty `device.lid` → the LID from the mapping.
- **Failed migration (no production hook):** a fixture trigger `BEFORE UPDATE OF from_me ON messages BEGIN SELECT
  RAISE(ABORT,'injected'); END;` → open fails; pin `failed` 8→9; still v8, no columns, no repair; the `.bak` exists;
  a normal reopen halts; simulate `--rollback`'s file steps (as the existing test does) with
  `write_migration_pin_pub("rolled_back", 8)`; drop the trigger; `ForceMigrate` succeeds.
- **One-time pass:** an own revoke stored from history (sender was the group JID) counts after the repair; a live
  own revoke stored with `from_me = 0` counts after the repair; a revoke by another member of someone else's message,
  an admin delete by the owner, the same label in another chat, and a target without a `media_refs` row do not
  count; `revoked_at` = the revoke's timestamp; the earliest valid revoke wins.
- **Ingest:** the columns written; the raw `file_name`, NULL for a document without one; a raw LID `remote_jid` kept
  while the chat resolves to a phone; `view_once = 1` for each wrapper, one nested in ephemeral, and the leaf flag;
  0 for plain media.
- **One transaction:** rewrite `test_a_media_ref_error_is_counted_and_the_message_row_is_kept`: with `media_refs`
  dropped, `failed: 1` and no message row.
- **Duplicates:** fill the NULL columns of a v8-shaped reference; create for `bootstrap` and `backfill`, not for
  `live`; each mismatch (chat, sender, `from_me`, kind) changes nothing and counts; phone vs LID sender and 1:1 chat
  through the mapping; a live bare sender matches a history full JID.
- **Existing tests:** replace hard-coded 8 with `CURRENT_SCHEMA_VERSION`; pins written with `blocked_target: 8` must
  become `pinned 8 / blocked CURRENT` on a v8 DB (else `open_with_mode` auto-clears them).
- Build `cargo clippy --features media-probe` too (`make lint` does not build that feature).

### 2.8 Docs (2a)
- `CHANGELOG.md` `[Unreleased]`: Added (schema v9, the six columns, `view_once`); Changed (one transaction, the
  duplicate rule, `duplicate_mismatch`); Fixed (extend the attribution line: stored rows and the cursor flag are
  repaired by v9).
- `docs/HOWTO.md` §6: `--rollback` serves failed migrations only. New **6c "No way back after a healthy v8 → v9
  migration: the manual revert"**: stop the daemon; keep the v9 `whatsapp.db`, `-wal`, `-shm` aside together; copy the
  newest `whatsapp.db.pre-migration-v8-<ts>.bak` to `whatsapp.db` with no `-wal`/`-shm` left; remove a stray
  `.migration-pin`; check offline (`sqlite3 whatsapp.db 'PRAGMA user_version'` → 8; the **v8** build's `--migrate`
  validates and exits); start the v8 build (a versioned `whatsrust-<ver>-<commit>`; a v9 build would migrate again);
  the losses (messages since, the Signal state; a re-pair may follow). After a v9 *validation* failure the start
  says "pin inconsistent": use `--rollback` or `--migrate`, do not delete the pin. **Never let a copied paired DB
  connect to WhatsApp** (same linked device). The re-pair-to-another-number limit of the repair.
- `ARCHITECTURE.md`: schema v9 (columns, repair, pass); the one-transaction write and the duplicate rule.
- `AGENTS.md`: "currently v8" → v9; the `src/storage.rs` key file gets the v9 `media_refs` columns.
- `README.md`: schema v8 → v9 where stated.
- `FEATURES.md` F1 and the roadmap: status; issue rows for what the run finds.

## 3. Run 2b (after the v5.2 cold review)

1. **Revoke key helper** (`src/bridge.rs`): `pub(crate) fn revoke_key_of(msg: &wa::Message) -> Option<RevokeKey>`
   reads the protocol message's key (through the ephemeral wrapper, like the revoke extraction) and returns
   `RevokeKey { target_id, from_me: Option<bool>, participant: Option<String> }`. **The public `InboundContent` enum
   does not change** (habb matches and builds it; the SSE bytes stay the same).
2. **Storage:** `pub enum RevokeOutcome { Applied, AlreadyRevoked, NoTarget, NoReference, NotValid }`.
   `apply_revoke_conn(…, Some(key))` adds the key check on top of the 2a rule: valid only when `key.from_me ==
   Some(true)` and the key's participant is None or `same_user` with the revoker; `false` (an admin delete) or None
   does not count. `pub async fn apply_revoke(&self, chat_jid, revoker_sender, revoker_from_me, revoked_at, key) ->
   Result<RevokeOutcome>` runs it in its own transaction (phase 4 adds the intent keys to it).
   `insert_history_message` gains `revoke: Option<RevokeKey>` and resolves it in the same transaction when the revoke
   row is new: `HistoryWrite::Stored { media_ref, revoke: Option<RevokeOutcome> }`.
3. **History:** `persist_history_messages` passes `revoke_key_of(msg)`; `PersistCounts` gains `revokes_applied` and
   `revokes_not_applied`.
4. **Live:** move the persist block of `handle_event` into `async fn persist_live_inbound(store, embed_notify,
   &WhatsAppInbound, revoke_key: Option<RevokeKey>)` so it can be tested without a `Client`; after `insert_inbound`,
   call `store.apply_revoke(…)` when there is a key; log the outcome at debug, without JIDs.

**Tests (2b):** the key is read, also inside an ephemeral wrapper; a revoke's SSE JSON is unchanged. Through
`persist_live_inbound` and through history: the owner's own revoke counts (live and history targets, group and 1:1);
a valid revoke by another person counts (key `from_me = true`, no participant; phone vs LID sender through the
mapping); an admin revoke (`from_me = false`), a key without the flag, a forged revoke (another member, `from_me =
true`), and a key whose participant is not the revoker change nothing; the same target ID in another chat changes
nothing; a revoke before its target is not applied later; a second revoke keeps the first `revoked_at`; a history
revoke row is resolved in the same transaction as its insert.

**Docs (2b):** CHANGELOG Added (revokes resolved at ingest into `revoked_at`); ARCHITECTURE (revoke resolution);
AGENTS.md Patterns (revokes judged by identities, resolved once at ingest); FEATURES and roadmap status; ADR 0050
and the design: a dated note if the cold review changes the rule.

## 4. Decisions applied (owner, 2026-11-08, unless marked)

| Topic | Decision |
|---|---|
| The revoke pass | In 2a, inside the v9 step (technical: steps run once per DB) |
| Revoke key | Read from the revoker's view (design v5.2 §5.5); cold review of v5.2 before 2b |
| LID 1:1 chats | Own messages filed under the other person: a separate fix commit before 2a; earlier rows keep the wrong chat |
| History repair | Own rows whose sender is not one of the owner's identities |
| Live repair | Every form of the owner's number and LID; the LID from the mapping when `device.lid` is empty |
| Cursor flag | Repaired in the same v9 step |
| Duplicates | 1:1 chats as the same user, groups exact; two own rows match; `duplicate_mismatch` for media only; fill NULL columns only |
| `view_once` | A wrapper or the media message's own flag |
| `InboundContent` | Unchanged; a separate revoke key helper |
| The owner as revoker | The repaired `from_me = 1` or the owner's identities |
| No paired device row | Own revokes stay unapplied (accepted) |
| Re-pair to another number | The repair credits old rows to the current number (a documented limit) |
| Revert rehearsal | A test on a synthetic v8 DB now; the owner repeats it on an offline copy of a real DB |

## 5. Remaining risks (accepted or to measure)

- **R1 Migration time:** the FTS update trigger fires on any `UPDATE` of `messages`, so every repaired row is
  re-indexed inside the transaction. Measure on the fixture; report the time for a large synthetic DB.
- **R2 Bare-number collisions:** comparing bare live values could confuse a LID user with a phone number; negligible
  (documented).
- **R3 Live `view_once` flag:** the live path misses `_v2_extension` and nested wrappers; live media is out of scope,
  so it is not changed here.
- **R4 A failure in `SCHEMA` writes no breaker pin:** pre-existing; the reason for no new index in `SCHEMA`.
- **R5 Fill-only duplicates:** a re-fetch never refreshes a stale `direct_path` or `media_key` (by design; an expired
  link goes through the re-upload).
- **R6 One transaction can drop a message:** a failed reference insert now drops its message too; backfill does not
  revisit rows, so only a re-pair brings it back (accepted by the design).
- **R7 Merge timing:** phase 2 may merge before the engine; the owner decides.
