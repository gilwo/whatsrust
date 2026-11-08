//! Tests of schema v9, the deferred own-message repair, the identity helpers and the history
//! ingest (media download phase 2a; plan `docs/plans/2026-11-08-media-phase2-plan.md` §2.7).
//!
//! Every database here is synthetic, built in a fresh directory under the system temp dir.

use super::*;

use std::sync::atomic::{AtomicU32, Ordering};

const OWN_PHONE: &str = "15550001111";
const OWN_LID: &str = "100000000000009";
const OWN_JID: &str = "15550001111@s.whatsapp.net";
const PEER_PHONE: &str = "15551234567";
const PEER_LID: &str = "100000000000001";
const PEER_JID: &str = "15551234567@s.whatsapp.net";
const GROUP: &str = "120363000000000001@g.us";
const UNMAPPED_LID_JID: &str = "100000000000002@lid";

/// The `media_refs` table as schema v8 had it (nine columns), which the fixture creates before the
/// current `SCHEMA` runs, as a real v8 database looked.
const V8_MEDIA_REFS_DDL: &str = "
CREATE TABLE media_refs (
    message_id      TEXT PRIMARY KEY,
    media_key       BLOB,
    direct_path     TEXT,
    file_enc_sha256 BLOB,
    mimetype        TEXT,
    file_length     INTEGER,
    width           INTEGER,
    height          INTEGER,
    hydrated_path   TEXT
);";

static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

fn test_dir(name: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("whatsrust-v9-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn raw(db: &Path) -> Connection {
    Connection::open(db).unwrap()
}

fn user_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

fn metadata(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM metadata WHERE key = ?1", [key], |r| r.get(0))
        .optional()
        .unwrap()
}

fn trigger_sql(conn: &Connection, name: &str) -> Option<String> {
    conn.query_row("SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = ?1", [name], |r| r.get(0))
        .optional()
        .unwrap()
}

fn index_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1", [name], |r| r.get::<_, i64>(0))
        .unwrap()
        > 0
}

/// `(name, declared type)` of every column of `table`, in table order.
fn table_info(conn: &Connection, table: &str) -> Vec<(String, String)> {
    let mut stmt = conn.prepare("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid").unwrap();
    stmt.query_map([table], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn put(conn: &Connection, id: &str, chat: &str, sender: &str, from_me: i64, source: &str, kind: &str) {
    let body = if kind == "revoke" { format!("[deleted {id}]") } else { format!("fixture {}", id.to_lowercase()) };
    conn.execute(
        "INSERT INTO messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at, from_me, source, embed_status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, 'skipped')",
        params![chat, sender, id, kind, body, 1_000_i64, from_me, source],
    )
    .unwrap();
}

/// `(message id, sender_jid, from_me)` of the fixture's rows as a v8 build stored them, and as the
/// repair leaves them.
fn fixture_rows(repaired: bool) -> Vec<(&'static str, String, i64)> {
    let live_own_flag = i64::from(repaired);
    let own = |s: &str| s.to_owned();
    let history_own_sender = |old: &str| if repaired { OWN_JID.to_owned() } else { old.to_owned() };
    vec![
        // Live rows: the owner in every form (all stored with from_me 0 by the old code) ...
        ("L1", own("15550001111"), live_own_flag),
        ("L2", own("15550001111:12"), live_own_flag),
        ("L3", own("15550001111@s.whatsapp.net"), live_own_flag),
        ("L4", own("100000000000009"), live_own_flag),
        ("L5", own("100000000000009:12@lid"), live_own_flag),
        // ... other people (a phone number, a LID user, a dotted LID user), and an own row that is right.
        ("L6", own("15551234567"), 0),
        ("L7", own("100000000000001"), 0),
        ("L8", own("15550001111"), 1),
        ("L9", own("100000000000003.1"), 0),
        // History own rows naming the group, the peer, an unmapped LID: repaired; the owner's phone
        // JID and the owner's LID form: already right.
        ("H1", history_own_sender(GROUP), 1),
        ("H2", history_own_sender(PEER_JID), 1),
        ("H3", history_own_sender(UNMAPPED_LID_JID), 1),
        ("H4", own(OWN_JID), 1),
        ("H5", own("100000000000009@lid"), 1),
        // History rows received (from_me 0) are never touched, whoever the sender is.
        ("H6", own(PEER_JID), 0),
        ("H7", own(OWN_JID), 0),
    ]
}

/// A real-shaped v8 database: the v8 `media_refs` DDL first, then the current `SCHEMA`, the
/// revoke-label index dropped again (v8 had none), `user_version = 8` and
/// `schema_validated_version = '8'`. With `paired`, the `device` row of the owner is there.
fn v8_fixture(db: &Path, paired: bool) {
    let conn = raw(db);
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; PRAGMA journal_mode = WAL;").unwrap();
    conn.execute_batch(V8_MEDIA_REFS_DDL).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute_batch("DROP INDEX idx_messages_revoke_label;").unwrap();

    if paired {
        conn.execute(
            "INSERT INTO device (id, pn, lid) VALUES (1, '15550001111:12@s.whatsapp.net', '100000000000009:12@lid')",
            [],
        )
        .unwrap();
    }
    for (lid, phone) in [(OWN_LID, OWN_PHONE), (PEER_LID, PEER_PHONE), ("100000000000003.1", "15553333333")] {
        conn.execute(
            "INSERT INTO lid_pn_mapping (lid, phone_number, created_at, updated_at, learning_source) VALUES (?1, ?2, 1, 1, 'test')",
            [lid, phone],
        )
        .unwrap();
    }

    // Live rows (the old code stored from_me = 0 for the owner's too).
    put(&conn, "L1", PEER_JID, "15550001111", 0, "live", "text");
    put(&conn, "L2", PEER_JID, "15550001111:12", 0, "live", "text");
    put(&conn, "L3", PEER_JID, "15550001111@s.whatsapp.net", 0, "live", "text");
    put(&conn, "L4", PEER_JID, "100000000000009", 0, "live", "text");
    put(&conn, "L5", GROUP, "100000000000009:12@lid", 0, "live", "text");
    put(&conn, "L6", PEER_JID, "15551234567", 0, "live", "text");
    put(&conn, "L7", GROUP, "100000000000001", 0, "live", "text");
    put(&conn, "L8", PEER_JID, "15550001111", 1, "live", "text");
    put(&conn, "L9", GROUP, "100000000000003.1", 0, "live", "text");
    // History rows.
    put(&conn, "H1", GROUP, GROUP, 1, "bootstrap", "text");
    put(&conn, "H2", PEER_JID, PEER_JID, 1, "backfill", "text");
    put(&conn, "H3", UNMAPPED_LID_JID, UNMAPPED_LID_JID, 1, "bootstrap", "text");
    put(&conn, "H4", PEER_JID, OWN_JID, 1, "bootstrap", "text");
    put(&conn, "H5", PEER_JID, "100000000000009@lid", 1, "backfill", "text");
    put(&conn, "H6", PEER_JID, PEER_JID, 0, "bootstrap", "text");
    put(&conn, "H7", GROUP, OWN_JID, 0, "bootstrap", "text");
    // Media rows with v8 references (the v9 columns do not exist yet in their DDL), and revokes.
    put(&conn, "M1", PEER_JID, PEER_JID, 0, "bootstrap", "image");
    put(&conn, "M2", GROUP, "15559998888@s.whatsapp.net", 0, "backfill", "document");
    for id in ["M1", "M2"] {
        conn.execute(
            "INSERT INTO media_refs (message_id, media_key, direct_path, file_enc_sha256, mimetype, file_length)
             VALUES (?1, x'0102', '/v/t62/x', x'0304', 'image/jpeg', 99)",
            [id],
        )
        .unwrap();
    }
    put(&conn, "R1", PEER_JID, OWN_JID, 0, "live", "revoke");
    put(&conn, "R2", GROUP, "15559998888", 0, "live", "revoke");
    // Fetch cursors: flag 0 and NULL whose anchor is an own message, a received anchor, a right
    // flag, and an anchor in another chat.
    for (chat, anchor, flag) in [
        (PEER_JID, "H2", Some(0)),
        (GROUP, "H1", None),
        ("15557654321@s.whatsapp.net", "H6", Some(0)),
        ("15558887777@s.whatsapp.net", "H4", Some(1)),
        ("15556665555@s.whatsapp.net", "H1", Some(0)),
    ] {
        conn.execute(
            "INSERT INTO backfill_cursor (chat_jid, oldest_msg_id, oldest_msg_from_me, oldest_msg_timestamp_ms)
             VALUES (?1, ?2, ?3, 1000)",
            params![chat, anchor, flag],
        )
        .unwrap();
    }

    conn.pragma_update(None, "user_version", 8_i64).unwrap();
    conn.execute("INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_validated_version', '8')", [])
        .unwrap();
}

fn assert_fixture_rows(conn: &Connection, repaired: bool) {
    for (id, sender, from_me) in fixture_rows(repaired) {
        let found: (String, i64) = conn
            .query_row("SELECT sender_jid, from_me FROM messages WHERE message_id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(found, (sender, from_me), "row {id} (repaired = {repaired})");
    }
    // The owner's own live revoke (R1) is repaired like any own live row; another person's stays.
    let revoke_flag = |id: &str| -> i64 { conn.query_row("SELECT from_me FROM messages WHERE message_id = ?1", [id], |r| r.get(0)).unwrap() };
    assert_eq!((revoke_flag("R1"), revoke_flag("R2")), (i64::from(repaired), 0), "revoke rows (repaired = {repaired})");
    let flag = |chat: &str| -> Option<i64> {
        conn.query_row("SELECT oldest_msg_from_me FROM backfill_cursor WHERE chat_jid = ?1", [chat], |r| r.get(0)).unwrap()
    };
    assert_eq!(flag(PEER_JID), if repaired { Some(1) } else { Some(0) }, "cursor on an own anchor, flag 0");
    assert_eq!(flag(GROUP), if repaired { Some(1) } else { None }, "cursor on an own anchor, flag NULL");
    assert_eq!(flag("15557654321@s.whatsapp.net"), Some(0), "a received anchor keeps flag 0");
    assert_eq!(flag("15558887777@s.whatsapp.net"), Some(1), "a right flag stays");
    assert_eq!(flag("15556665555@s.whatsapp.net"), Some(0), "an anchor of another chat does not count");
}

/// The full-text index still finds every fixture row by its own word, and the text is unchanged.
fn assert_fts_matches_fixture(conn: &Connection) {
    for (id, _, _) in fixture_rows(true) {
        let word = id.to_lowercase();
        let hits: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1", [&word], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 1, "FTS finds {id}");
        let body: String = conn.query_row("SELECT body_text FROM messages WHERE message_id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(body, format!("fixture {word}"));
    }
}

// ---------------------------------------------------------------------------
// Identity helpers
// ---------------------------------------------------------------------------

#[test]
fn bare_user_strips_server_and_device_but_keeps_a_dot() {
    assert_eq!(bare_user("15550001111"), "15550001111");
    assert_eq!(bare_user("15550001111:12"), "15550001111");
    assert_eq!(bare_user("15550001111@s.whatsapp.net"), "15550001111");
    assert_eq!(bare_user("15550001111:12@s.whatsapp.net"), "15550001111");
    assert_eq!(bare_user("100000000000009:12@lid"), "100000000000009");
    assert_eq!(bare_user("100000000000001.1:75@lid"), "100000000000001.1", "a dot stays part of a LID user");
    assert_eq!(bare_user(GROUP), "120363000000000001");
    assert_eq!(bare_user(""), "");
}

fn identity_conn(device: Option<(&str, &str)>, mappings: &[(&str, &str)]) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    if let Some((pn, lid)) = device {
        conn.execute("INSERT INTO device (id, pn, lid) VALUES (1, ?1, ?2)", [pn, lid]).unwrap();
    }
    for (lid, phone) in mappings {
        conn.execute(
            "INSERT INTO lid_pn_mapping (lid, phone_number, created_at, updated_at, learning_source) VALUES (?1, ?2, 1, 1, 'test')",
            [lid, phone],
        )
        .unwrap();
    }
    conn
}

#[test]
fn read_own_identity_never_aborts() {
    // No row, an empty pn, a pn that is not a phone JID: no identity.
    assert_eq!(read_own_identity(&identity_conn(None, &[])), OwnIdentity::default());
    assert_eq!(read_own_identity(&identity_conn(Some(("", "")), &[])), OwnIdentity::default());
    assert_eq!(read_own_identity(&identity_conn(Some(("100000000000009:12@lid", "")), &[])), OwnIdentity::default());
    // pn and lid with their device parts.
    let own = read_own_identity(&identity_conn(Some(("15550001111:12@s.whatsapp.net", "100000000000009:12@lid")), &[]));
    assert_eq!((own.phone.as_deref(), own.lid.as_deref()), (Some(OWN_PHONE), Some(OWN_LID)));
    assert_eq!(own.phone_jid().as_deref(), Some(OWN_JID));
    // An empty device.lid: the LID comes from the mapping by the own number.
    let own = read_own_identity(&identity_conn(Some(("15550001111@s.whatsapp.net", "")), &[(OWN_LID, OWN_PHONE)]));
    assert_eq!((own.phone.as_deref(), own.lid.as_deref()), (Some(OWN_PHONE), Some(OWN_LID)));
    // ... and none when the mapping has none.
    let own = read_own_identity(&identity_conn(Some(("15550001111@s.whatsapp.net", "")), &[(PEER_LID, PEER_PHONE)]));
    assert_eq!((own.phone.as_deref(), own.lid), (Some(OWN_PHONE), None));
    // A missing column, a missing table: no identity, no panic.
    let no_lid_column = identity_conn(Some(("15550001111@s.whatsapp.net", "")), &[]);
    no_lid_column.execute_batch("ALTER TABLE device DROP COLUMN lid;").unwrap();
    assert_eq!(read_own_identity(&no_lid_column), OwnIdentity::default());
    let no_table = Connection::open_in_memory().unwrap();
    assert_eq!(read_own_identity(&no_table), OwnIdentity::default());
}

#[test]
fn user_forms_same_user_and_same_chat_use_the_mapping() {
    let conn = identity_conn(None, &[(PEER_LID, PEER_PHONE), ("100000000000003.1", "15553333333"), ("100000000000004", PEER_PHONE)]);

    // The forms come back sorted and without duplicates.
    assert_eq!(user_forms(&conn, PEER_JID), vec!["100000000000001", "100000000000004", "15551234567"]);
    assert_eq!(user_forms(&conn, "100000000000001@lid"), vec!["100000000000001", "15551234567"]);
    // A value without a server is looked up both ways.
    assert!(user_forms(&conn, PEER_LID).contains(&PEER_PHONE.to_owned()));
    assert!(user_forms(&conn, PEER_PHONE).contains(&PEER_LID.to_owned()));
    // A group has no twins; an unknown user only itself; an empty value nothing.
    assert_eq!(user_forms(&conn, GROUP), vec!["120363000000000001".to_owned()]);
    assert_eq!(user_forms(&conn, "15559999999@s.whatsapp.net"), vec!["15559999999".to_owned()]);
    assert!(user_forms(&conn, "").is_empty());

    assert!(same_user(&conn, "15551234567", "15551234567@s.whatsapp.net"));
    assert!(same_user(&conn, "15551234567:12@s.whatsapp.net", "15551234567"));
    assert!(same_user(&conn, PEER_JID, "100000000000001@lid"), "phone vs LID through the mapping");
    assert!(same_user(&conn, "100000000000001", PEER_JID), "a bare LID user vs a phone JID");
    assert!(same_user(&conn, "100000000000004@lid", "100000000000001@lid"), "two LIDs of one number");
    assert!(!same_user(&conn, PEER_JID, "15559999999@s.whatsapp.net"));
    assert!(!same_user(&conn, "", ""), "empty values never match");
    // The dot is part of the LID user.
    assert!(same_user(&conn, "100000000000003.1:75@lid", "15553333333@s.whatsapp.net"));
    assert!(!same_user(&conn, "100000000000003@lid", "15553333333@s.whatsapp.net"));

    assert!(same_chat(&conn, GROUP, GROUP));
    assert!(!same_chat(&conn, GROUP, "120363000000000002@g.us"), "groups are compared exactly");
    assert!(!same_chat(&conn, GROUP, "120363000000000001"), "a group never matches a bare value");
    assert!(same_chat(&conn, PEER_JID, "100000000000001@lid"), "a 1:1 chat is the same user in either form");
    assert!(!same_chat(&conn, PEER_JID, "15559999999@s.whatsapp.net"));
}

#[test]
fn is_owner_is_the_one_predicate_for_every_form() {
    let conn = identity_conn(Some(("15550001111:12@s.whatsapp.net", "100000000000009:12@lid")), &[(OWN_LID, OWN_PHONE), ("100000000000010", OWN_PHONE)]);
    let own = read_own_identity(&conn);
    for form in [
        "15550001111",
        "15550001111:12",
        "15550001111@s.whatsapp.net",
        "15550001111:12@s.whatsapp.net",
        "100000000000009",
        "100000000000009:12@lid",
        "100000000000010@lid", // another LID the mapping gives for the own number
    ] {
        assert!(is_owner(&conn, &own, form), "{form} is the owner");
    }
    for form in ["15551234567", "100000000000001@lid", GROUP, "", "15550001112"] {
        assert!(!is_owner(&conn, &own, form), "{form:?} is not the owner");
    }
    assert!(!is_owner(&conn, &OwnIdentity::default(), "15550001111"), "no identity: nobody is the owner");
}

#[test]
fn is_owner_keeps_the_dot_of_a_dotted_own_lid() {
    let conn = identity_conn(Some(("15550001111@s.whatsapp.net", "200000000000007.2:12@lid")), &[]);
    let own = read_own_identity(&conn);
    assert_eq!(own.lid.as_deref(), Some("200000000000007.2"));
    assert!(is_owner(&conn, &own, "200000000000007.2:5@lid"));
    assert!(is_owner(&conn, &own, "200000000000007.2"));
    assert!(!is_owner(&conn, &own, "200000000000007"), "without the dot it is another user");
}

// ---------------------------------------------------------------------------
// Schema v9: fresh DB, v8 -> v9, v7 -> v9, a re-run
// ---------------------------------------------------------------------------

const V9_COLUMNS: [&str; 15] = [
    "message_id", "media_key", "direct_path", "file_enc_sha256", "mimetype", "file_length", "width", "height",
    "hydrated_path", "file_name", "file_sha256", "remote_jid", "participant", "view_once", "media_key_timestamp",
];

#[test]
fn fresh_db_is_v9_and_has_the_same_media_refs_shape_as_a_migrated_one() {
    let dir = test_dir("fresh");
    let fresh_db = dir.join("fresh.db");
    let migrated_db = dir.join("migrated.db");
    v8_fixture(&migrated_db, false);

    let fresh = Store::new(&fresh_db).unwrap();
    let migrated = Store::new(&migrated_db).unwrap();

    let (fresh_conn, migrated_conn) = (fresh.conn.lock(), migrated.conn.lock());
    assert_eq!(user_version(&fresh_conn), 9);
    assert_eq!(user_version(&migrated_conn), 9);
    let shape = table_info(&fresh_conn, "media_refs");
    assert_eq!(shape.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), V9_COLUMNS);
    assert_eq!(shape, table_info(&migrated_conn, "media_refs"), "a migrated DB has the fresh DB's media_refs columns");
    for conn in [&fresh_conn, &migrated_conn] {
        assert!(index_exists(conn, "idx_messages_revoke_label"));
        let sql: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'idx_messages_revoke_label'", [], |r| r.get(0))
            .unwrap();
        assert!(sql.contains("ON messages(body_text) WHERE content_kind = 'revoke'"), "{sql}");
        assert_eq!(metadata(conn, "schema_validated_version").as_deref(), Some("9"));
    }
    drop((fresh_conn, migrated_conn));
    drop((fresh, migrated));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v8_to_v9_migrates_validates_and_keeps_every_row() {
    let dir = test_dir("v8-to-v9");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    let (messages_before, refs_before): (i64, i64) = {
        let c = raw(&db);
        (
            c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap(),
            c.query_row("SELECT COUNT(*) FROM media_refs", [], |r| r.get(0)).unwrap(),
        )
    };

    let store = open_with_mode(&db, MigrationMode::Normal).unwrap();

    {
        let conn = store.conn.lock();
        assert_eq!(user_version(&conn), 9);
        assert_eq!(metadata(&conn, "schema_validated_version").as_deref(), Some("9"));
        assert!(metadata(&conn, "watchdog_last_alerted_size").is_some());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0)).unwrap(),
            messages_before
        );
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM media_refs", [], |r| r.get::<_, i64>(0)).unwrap(), refs_before);
        // The v8 references keep their values; the six new columns are NULL.
        let (key, path): (Vec<u8>, String) = conn
            .query_row("SELECT media_key, direct_path FROM media_refs WHERE message_id = 'M1'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((key, path.as_str()), (vec![1, 2], "/v/t62/x"));
        let null_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_refs WHERE message_id = 'M1' AND file_name IS NULL AND file_sha256 IS NULL
                   AND remote_jid IS NULL AND participant IS NULL AND view_once IS NULL AND media_key_timestamp IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_columns, 1, "the six new columns of an existing reference are NULL");
        assert!(index_exists(&conn, "idx_messages_revoke_label"));
        assert!(!pin_path(&db).exists(), "no pin after a successful migration");
    }
    // The pre-migration backup is the v8 file.
    let bak = find_newest_bak(&db).expect("a pre-migration backup");
    assert!(bak.file_name().unwrap().to_string_lossy().contains("pre-migration-v8-"), "{bak:?}");
    let bak_conn = raw(&bak);
    assert_eq!(user_version(&bak_conn), 8);
    assert_eq!(table_info(&bak_conn, "media_refs").len(), 9, "the backup has the v8 media_refs");
    drop(bak_conn);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v7_to_v9_passes_through_the_v9_step() {
    let dir = test_dir("v7-to-v9");
    let db = dir.join("whatsapp.db");
    {
        let conn = raw(&db);
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; PRAGMA journal_mode = WAL;").unwrap();
        // A v7 DB: inbound_messages, and a media_refs table in the old shape to exercise ALTER.
        conn.execute_batch(
            "CREATE TABLE inbound_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT, chat_jid TEXT NOT NULL, sender_jid TEXT NOT NULL,
                message_id TEXT NOT NULL UNIQUE, content_kind TEXT NOT NULL, body_text TEXT,
                timestamp INTEGER NOT NULL, created_at INTEGER NOT NULL);
             INSERT INTO inbound_messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
                 VALUES ('c@s.whatsapp.net', 's@s.whatsapp.net', 'old-1', 'text', 'seven hello', 10, 10);",
        )
        .unwrap();
        conn.execute_batch(V8_MEDIA_REFS_DDL).unwrap();
        conn.pragma_update(None, "user_version", 7_i64).unwrap();
    }

    let store = open_with_mode(&db, MigrationMode::Normal).unwrap();

    {
        let conn = store.conn.lock();
        assert_eq!(user_version(&conn), 9);
        assert_eq!(table_info(&conn, "media_refs").iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), V9_COLUMNS);
        assert!(index_exists(&conn, "idx_messages_revoke_label"));
        let hits: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'seven'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 1);
    }
    let bak = find_newest_bak(&db).expect("a pre-migration backup");
    assert!(bak.file_name().unwrap().to_string_lossy().contains("pre-migration-v7-"), "{bak:?}");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `sqlite_master` plus the rows, as text: what must not change when the step runs again.
fn snapshot(conn: &Connection) -> String {
    let mut out = String::new();
    let mut stmt = conn.prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name").unwrap();
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))).unwrap() {
        out.push_str(&format!("{:?}\n", row.unwrap()));
    }
    for table in ["messages", "media_refs", "backfill_cursor", "metadata"] {
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2")).unwrap();
        let cols = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..cols).map(|i| r.get_ref(i).map(|v| format!("{v:?}"))).collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap();
        for row in rows {
            out.push_str(&format!("{table}: {:?}\n", row.unwrap()));
        }
    }
    out
}

#[test]
fn the_v9_step_is_rerunnable_and_changes_nothing_the_second_time() {
    // The step alone, on a v8-shaped DB (no SCHEMA run): it adds the columns and the index.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(V8_MEDIA_REFS_DDL).unwrap();
    conn.execute_batch("CREATE TABLE messages (id INTEGER PRIMARY KEY, body_text TEXT, content_kind TEXT);").unwrap();
    ensure_v9_columns(&conn).unwrap();
    assert_eq!(table_info(&conn, "media_refs").iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), V9_COLUMNS);
    assert!(index_exists(&conn, "idx_messages_revoke_label"));
    let once = snapshot_schema(&conn);
    ensure_v9_columns(&conn).unwrap();
    assert_eq!(snapshot_schema(&conn), once, "a second run changes nothing");

    // A v9 DB re-entering the step (a downgraded DB reopened): nothing changes, the marker stays.
    let dir = test_dir("rerun");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    drop(open_with_mode(&db, MigrationMode::Normal).unwrap());
    let conn = raw(&db);
    let marker = metadata(&conn, "own_attribution_repair").expect("the repair ran");
    let before = snapshot(&conn);
    conn.pragma_update(None, "user_version", 8_i64).unwrap();
    run_schema_migrations(&conn, 8).unwrap();
    assert_eq!(user_version(&conn), 9);
    assert_eq!(snapshot(&conn), before, "re-running the v9 step changes nothing");
    assert_eq!(metadata(&conn, "own_attribution_repair"), Some(marker));
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

fn snapshot_schema(conn: &Connection) -> String {
    let mut stmt = conn.prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name").unwrap();
    stmt.query_map([], |r| Ok(format!("{:?}", (r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Failure paths: the v9 step fails; validation fails after the commit
// ---------------------------------------------------------------------------

fn read_pin(db: &Path) -> (String, i64, i64) {
    let pin = read_migration_pin_pub(db).expect("a pin");
    (
        pin["state"].as_str().unwrap().to_owned(),
        pin["pinned_version"].as_i64().unwrap(),
        pin["blocked_target"].as_i64().unwrap(),
    )
}

#[test]
fn a_failure_inside_the_v9_step_rolls_back_pins_and_recovers_with_rollback_and_force_migrate() {
    let dir = test_dir("v9-fails");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    {
        // Inject the failure from the fixture: `media_refs` is a view, which cannot take a column.
        let conn = raw(&db);
        conn.execute_batch(
            "DROP TABLE media_refs;
             CREATE VIEW media_refs AS SELECT 'x' AS message_id, NULL AS media_key, NULL AS direct_path,
                 NULL AS file_enc_sha256, NULL AS mimetype, NULL AS file_length, NULL AS width,
                 NULL AS height, NULL AS hydrated_path;",
        )
        .unwrap();
    }

    let err = open_with_mode(&db, MigrationMode::Normal).err().expect("the v9 step fails");
    assert!(store_error_text(&err).contains("migration failed (DB rolled back to v8"), "{}", store_error_text(&err));
    {
        let conn = raw(&db);
        assert_eq!(user_version(&conn), 8, "the transaction rolled back");
        assert!(table_info(&conn, "media_refs").iter().all(|c| c.0 != "file_name"));
    }
    assert_eq!(read_pin(&db), ("failed".to_owned(), 8, 9), "pinned 8, blocked CURRENT");
    let bak = find_newest_bak(&db).expect("the pre-migration backup was written first");
    assert!(bak.file_name().unwrap().to_string_lossy().contains("pre-migration-v8-"));

    // The circuit breaker halts the next start.
    let halted = open_with_mode(&db, MigrationMode::Normal).err().expect("halted by the pin");
    assert!(store_error_text(&halted).contains("circuit-breaker"), "{}", store_error_text(&halted));

    // `--rollback`'s file steps: restore the backup, remove -wal/-shm, write the rolled_back pin.
    std::fs::copy(&bak, &db).unwrap();
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("whatsapp.db{ext}")));
    }
    write_migration_pin_pub(&db, "rolled_back", 8).unwrap();
    let still = open_with_mode(&db, MigrationMode::Normal).err().expect("a rolled-back DB waits for --migrate");
    assert!(store_error_text(&still).contains("rolled back to v8"), "{}", store_error_text(&still));

    // The cause is fixed (a real `media_refs` table again); `--migrate` (ForceMigrate) recovers.
    {
        let conn = raw(&db);
        conn.execute_batch("DROP VIEW media_refs;").unwrap();
        conn.execute_batch(V8_MEDIA_REFS_DDL).unwrap();
    }
    let store = open_with_mode(&db, MigrationMode::ForceMigrate).unwrap();
    {
        let conn = store.conn.lock();
        assert_eq!(user_version(&conn), 9);
        assert!(metadata(&conn, "own_attribution_repair").is_some(), "the repair ran after the open");
    }
    assert!(!pin_path(&db).exists(), "ForceMigrate cleared the pin");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_validation_failure_after_the_commit_says_pin_inconsistent_and_migrate_recovers() {
    let dir = test_dir("validation-fails");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    {
        // A text-sync trigger that does nothing: the migration and the structure pass, the
        // update-trigger probe fails.
        let conn = raw(&db);
        conn.execute_batch(
            "DROP TRIGGER messages_fts_update;
             CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages BEGIN SELECT 1; END;",
        )
        .unwrap();
    }

    let err = open_with_mode(&db, MigrationMode::Normal).err().expect("validation fails");
    assert!(store_error_text(&err).contains("migration validation failed"), "{}", store_error_text(&err));
    assert!(store_error_text(&err).contains("update trigger"), "{}", store_error_text(&err));
    {
        let conn = raw(&db);
        assert_eq!(user_version(&conn), 9, "the migration itself committed");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM messages WHERE message_id LIKE '__validate%'", [], |r| r.get::<_, i64>(0)).unwrap(),
            0,
            "the probe row is gone"
        );
        assert!(metadata(&conn, "own_attribution_repair").is_none(), "no repair after a failed open");
    }
    assert_eq!(read_pin(&db), ("failed".to_owned(), 8, 9));

    let inconsistent = open_with_mode(&db, MigrationMode::Normal).err().expect("pin inconsistent");
    let text = store_error_text(&inconsistent);
    assert!(text.contains("migration pin inconsistent"), "{text}");
    assert!(text.contains("whatsrust --rollback") && text.contains("whatsrust --migrate"), "names both recoveries: {text}");
    assert!(text.contains("do not delete the pin"), "{text}");

    // The cause is removed (SCHEMA re-creates the missing trigger); `--migrate` recovers.
    raw(&db).execute_batch("DROP TRIGGER messages_fts_update;").unwrap();
    let store = open_with_mode(&db, MigrationMode::ForceMigrate).unwrap();
    {
        let conn = store.conn.lock();
        assert_eq!(metadata(&conn, "schema_validated_version").as_deref(), Some("9"));
        assert_eq!(trigger_sql(&conn, "messages_fts_update").as_deref().map(|s| s.contains("INSERT INTO messages_fts")), Some(true));
        assert!(metadata(&conn, "own_attribution_repair").is_some());
    }
    assert!(!pin_path(&db).exists());
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_update_trigger_probe_passes_on_a_good_db_and_fails_on_a_broken_trigger() {
    let dir = test_dir("probe");
    let store = Store::new(&dir.join("whatsapp.db")).unwrap();
    let conn = store.conn.lock();
    validate_migration_post_commit(&conn).unwrap();
    let leftovers: i64 = conn.query_row("SELECT COUNT(*) FROM messages WHERE message_id LIKE '__validate%'", [], |r| r.get(0)).unwrap();
    assert_eq!(leftovers, 0);

    conn.execute_batch(
        "DROP TRIGGER messages_fts_update;
         CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages BEGIN SELECT 1; END;",
    )
    .unwrap();
    let err = validate_migration_post_commit(&conn).expect_err("a no-op update trigger fails the probe");
    assert!(store_error_text(&err).contains("update trigger"), "{}", store_error_text(&err));
    let leftovers: i64 = conn.query_row("SELECT COUNT(*) FROM messages WHERE message_id LIKE '__validate%'", [], |r| r.get(0)).unwrap();
    assert_eq!(leftovers, 0, "the probe row is removed on failure too");
    // And the FTS index is still usable (the probe left it consistent).
    conn.execute_batch("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'anything';").unwrap();
    drop(conn);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The own-message repair (every start that knows the owner's number)
// ---------------------------------------------------------------------------

/// Make the failure note look 25 hours old, so the next start retries.
fn age_failure_note(conn: &Connection) {
    conn.execute(
        "UPDATE metadata SET value = (CAST(strftime('%s', 'now') AS INTEGER) - 90000) || substr(value, instr(value, ' '))
         WHERE key = 'own_attribution_repair_failed'",
        [],
    )
    .unwrap();
}

/// Files in `dir` with the given ending (for example `.tmp`).
fn files_ending(dir: &Path, ending: &str) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(ending))
        .collect()
}

fn from_me_of(conn: &Connection, id: &str) -> i64 {
    conn.query_row("SELECT from_me FROM messages WHERE message_id = ?1", [id], |r| r.get(0)).unwrap()
}

#[test]
fn the_repair_counts_and_repairs_every_form_paired_at_migration() {
    let dir = test_dir("repair-counts");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    let conn = raw(&db);
    assert_fixture_rows(&conn, false);

    // The repair on its own (the migration is not needed for it); `true`: a backup was just taken.
    let report = repair_own_attribution(&conn, &db, true).unwrap().expect("a paired DB is repaired");
    assert_eq!((report.live_from_me, report.history_sender, report.cursor_flag), (6, 3, 2), "L1-L5 and the own revoke R1; H1-H3; two cursors");
    assert_fixture_rows(&conn, true);
    assert_fts_matches_fixture(&conn);
    assert!(pre_repair_backups(&db).is_empty(), "no extra backup when the open just took one");

    let marker = metadata(&conn, "own_attribution_repair").expect("the marker");
    let (number, epoch) = marker.split_once(' ').expect("`<bare number> <epoch>`");
    assert_eq!(number, OWN_PHONE);
    assert!(epoch.parse::<i64>().unwrap() > 1_700_000_000);
    // The trigger is back, with the SCHEMA text.
    let fresh = Connection::open_in_memory().unwrap();
    fresh.execute_batch(SCHEMA).unwrap();
    assert_eq!(trigger_sql(&conn, "messages_fts_update"), trigger_sql(&fresh, "messages_fts_update"));
    // ... and works: an UPDATE of the text reaches the index.
    conn.execute("UPDATE messages SET body_text = 'brandnew text' WHERE message_id = 'L1'", []).unwrap();
    let hits: i64 = conn.query_row("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'brandnew'", [], |r| r.get(0)).unwrap();
    assert_eq!(hits, 1);

    // A second call finds nothing to change.
    assert!(repair_own_attribution(&conn, &db, false).unwrap().is_none());
    assert!(pre_repair_backups(&db).is_empty(), "and takes no backup");
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_migrating_start_needs_no_pre_repair_backup_and_a_later_start_repairs_again_after_taking_one() {
    let dir = test_dir("repair-every-start");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);

    drop(Store::new(&db).unwrap());

    let conn = raw(&db);
    assert_eq!(user_version(&conn), 9);
    assert_fixture_rows(&conn, true);
    assert_fts_matches_fixture(&conn);
    assert!(pre_repair_backups(&db).is_empty(), "the pre-migration backup covers the migrating start");
    let marker = metadata(&conn, "own_attribution_repair").expect("the marker");

    // Rows become misattributed again (a v8 window, a LID that became known): the next start repairs them.
    conn.execute("UPDATE messages SET from_me = 0 WHERE message_id IN ('L1', 'L4')", []).unwrap();
    drop(conn);
    drop(Store::new(&db).unwrap());

    let conn = raw(&db);
    assert_eq!((from_me_of(&conn, "L1"), from_me_of(&conn, "L4")), (1, 1), "repaired again at a later start");
    assert_eq!(metadata(&conn, "own_attribution_repair"), Some(marker), "the marker keeps the first-run time");
    let backups = pre_repair_backups(&db);
    assert_eq!(backups.len(), 1, "a start that changes rows takes a pre-repair backup first");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&backups[0]).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let backup = raw(&backups[0]);
    assert_eq!(from_me_of(&backup, "L1"), 0, "the backup holds the state before the repair");
    drop(backup);

    // A start with nothing to repair takes no backup and writes nothing.
    std::fs::remove_file(&backups[0]).unwrap();
    let before = snapshot(&conn);
    drop(conn);
    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert!(pre_repair_backups(&db).is_empty());
    assert_eq!(snapshot(&conn), before, "a clean start changes nothing");
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unpaired_profile_is_repaired_at_the_first_start_that_knows_the_number() {
    let dir = test_dir("repair-unpaired-then-paired");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, false);

    drop(Store::new(&db).unwrap());
    {
        let conn = raw(&db);
        assert_eq!(user_version(&conn), 9, "migrated");
        assert_fixture_rows(&conn, false);
        assert!(metadata(&conn, "own_attribution_repair").is_none(), "no number, no marker");
        assert!(pre_repair_backups(&db).is_empty());
        // Pairing happens: the device row appears.
        conn.execute("INSERT INTO device (id, pn, lid) VALUES (1, '15550001111:12@s.whatsapp.net', '100000000000009:12@lid')", []).unwrap();
    }

    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert_fixture_rows(&conn, true);
    assert_fts_matches_fixture(&conn);
    assert!(metadata(&conn, "own_attribution_repair").unwrap().starts_with("15550001111 "));
    assert_eq!(pre_repair_backups(&db).len(), 1, "this start was not a migrating one, so it took its own backup");
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_marker_with_another_number_skips_the_repair_and_warns_once_per_number() {
    let dir = test_dir("repair-other-number");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    raw(&db)
        .execute("INSERT INTO metadata (key, value) VALUES ('own_attribution_repair', '15559990000 1700000000')", [])
        .unwrap();

    drop(Store::new(&db).unwrap());

    let conn = raw(&db);
    assert_eq!(user_version(&conn), 9, "the start itself goes on");
    assert_fixture_rows(&conn, false);
    assert_eq!(metadata(&conn, "own_attribution_repair").as_deref(), Some("15559990000 1700000000"), "the marker is not touched");
    let note = metadata(&conn, "own_attribution_repair_skipped").expect("the loud warning is noted");
    assert!(note.starts_with("15550001111 "), "the note names the number it was given for: {note}");
    assert!(pre_repair_backups(&db).is_empty());

    // A later start logs at debug only: the note is not rewritten.
    conn.execute("UPDATE metadata SET value = '15550001111 1700000001' WHERE key = 'own_attribution_repair_skipped'", []).unwrap();
    drop(conn);
    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert_eq!(metadata(&conn, "own_attribution_repair_skipped").as_deref(), Some("15550001111 1700000001"));
    assert_fixture_rows(&conn, false);

    // Another current number is a new situation: the warning is given again.
    conn.execute("UPDATE device SET pn = '15557776666:3@s.whatsapp.net'", []).unwrap();
    drop(conn);
    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert!(metadata(&conn, "own_attribution_repair_skipped").unwrap().starts_with("15557776666 "));
    assert!(repair_own_attribution(&conn, &db, false).unwrap().is_none());
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_device_column_means_no_identity_and_no_abort() {
    let dir = test_dir("repair-no-device-column");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    raw(&db).execute_batch("ALTER TABLE device DROP COLUMN lid;").unwrap();

    let store = Store::new(&db).expect("the start is not aborted");
    assert_eq!(user_version(&store.conn.lock()), 9);
    drop(store);

    let conn = raw(&db);
    assert_fixture_rows(&conn, false);
    assert!(metadata(&conn, "own_attribution_repair").is_none());
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_device_lid_takes_the_own_lid_from_the_mapping() {
    // With the mapping: the own LID user (L4) and the LID with a device (L5) are the owner's.
    let dir = test_dir("repair-empty-lid");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    raw(&db).execute("UPDATE device SET lid = ''", []).unwrap();
    drop(Store::new(&db).unwrap());
    assert_fixture_rows(&raw(&db), true);

    // Without any mapping for the own number the LID forms are not recognised; the phone forms are.
    let db2 = dir.join("second.db");
    v8_fixture(&db2, true);
    raw(&db2).execute_batch("UPDATE device SET lid = ''; DELETE FROM lid_pn_mapping WHERE phone_number = '15550001111';").unwrap();
    drop(Store::new(&db2).unwrap());
    let conn = raw(&db2);
    assert_eq!((from_me_of(&conn, "L1"), from_me_of(&conn, "L2"), from_me_of(&conn, "L3")), (1, 1, 1));
    assert_eq!((from_me_of(&conn, "L4"), from_me_of(&conn, "L5")), (0, 0), "no own LID known");

    // The LID becomes known later (the mapping is learned): the next start repairs those rows.
    conn.execute(
        "INSERT INTO lid_pn_mapping (lid, phone_number, created_at, updated_at, learning_source) VALUES ('100000000000009', '15550001111', 2, 2, 'test')",
        [],
    )
    .unwrap();
    drop(conn);
    drop(Store::new(&db2).unwrap());
    let conn = raw(&db2);
    assert_eq!((from_me_of(&conn, "L4"), from_me_of(&conn, "L5")), (1, 1), "repaired once the mapping is known");
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dotted_own_lid_matches_with_its_dot() {
    let dir = test_dir("repair-dotted-lid");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    {
        let conn = raw(&db);
        conn.execute_batch(
            "UPDATE device SET lid = '200000000000007.2:12@lid';
             DELETE FROM lid_pn_mapping WHERE phone_number = '15550001111';",
        )
        .unwrap();
        put(&conn, "D1", PEER_JID, "200000000000007.2", 0, "live", "text");
        put(&conn, "D2", PEER_JID, "200000000000007.2:5@lid", 0, "live", "text");
        put(&conn, "D3", PEER_JID, "200000000000007", 0, "live", "text");
    }
    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert_eq!((from_me_of(&conn, "D1"), from_me_of(&conn, "D2"), from_me_of(&conn, "D3")), (1, 1, 0), "the dot is part of the LID user");
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_repair_rolls_back_notes_the_failure_and_the_next_start_retries() {
    let dir = test_dir("repair-fails");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, false);
    drop(Store::new(&db).unwrap()); // migrate while unpaired (the validation probe updates a row)
    {
        let conn = raw(&db);
        conn.execute("INSERT INTO device (id, pn, lid) VALUES (1, '15550001111:12@s.whatsapp.net', '100000000000009:12@lid')", []).unwrap();
        conn.execute_batch("CREATE TRIGGER poison BEFORE UPDATE ON messages BEGIN SELECT RAISE(ABORT, 'poison'); END;").unwrap();
    }

    let store = Store::new(&db).expect("a failed repair never stops the start");
    drop(store);
    let backup_name = {
        let conn = raw(&db);
        assert_fixture_rows(&conn, false);
        assert!(trigger_sql(&conn, "messages_fts_update").is_some(), "the dropped trigger came back with the rollback");
        assert!(metadata(&conn, "own_attribution_repair").unwrap().starts_with("15550001111 "), "the marker records the number");
        let note = metadata(&conn, "own_attribution_repair_failed").expect("the failure is noted");
        let mut parts = note.splitn(3, ' ');
        let (epoch, backup, reason) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
        assert!(epoch.parse::<i64>().unwrap() > 1_700_000_000);
        assert!(reason.contains("poison"), "{note}");
        assert!(!note.contains('@') && !note.contains("15550001111"), "no JID or number in the note: {note}");
        assert_eq!(pre_repair_backups(&db).len(), 1, "the backup was taken before the transaction");
        assert_eq!(pre_repair_backups(&db)[0].file_name().unwrap().to_str().unwrap(), backup, "the note names the backup");
        backup.to_owned()
    };

    // A start within 24 hours leaves the repair alone: no backup, no change, the note as it was.
    let note_before = metadata(&raw(&db), "own_attribution_repair_failed");
    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert_eq!(pre_repair_backups(&db).len(), 1, "no second backup for the second failing start");
    assert!(files_ending(&dir, ".tmp").is_empty());
    assert_eq!(metadata(&conn, "own_attribution_repair_failed"), note_before, "not tried, so not rewritten");
    assert_fixture_rows(&conn, false);

    // After 24 hours it is tried again, and fails again; it reuses the first attempt's backup.
    age_failure_note(&conn);
    drop(conn);
    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert_eq!(pre_repair_backups(&db).len(), 1, "the retry reuses the backup of the failed attempt");
    assert!(files_ending(&dir, ".tmp").is_empty());
    let note = metadata(&conn, "own_attribution_repair_failed").expect("noted again");
    assert!(note.split(' ').nth(1) == Some(backup_name.as_str()), "the note keeps the backup name: {note}");
    assert!(note.split(' ').next().unwrap().parse::<i64>().unwrap() > now_secs() - 100, "with a fresh time");
    assert_fixture_rows(&conn, false);

    // The cause is gone and the next day has come: it repairs, still without a new backup.
    conn.execute_batch("DROP TRIGGER poison;").unwrap();
    age_failure_note(&conn);
    drop(conn);
    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert_fixture_rows(&conn, true);
    assert!(metadata(&conn, "own_attribution_repair_failed").is_none(), "success clears the note");
    assert_eq!(pre_repair_backups(&db).len(), 1);
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_post_condition_rolls_back_a_repair_that_leaves_a_misattributed_row() {
    let dir = test_dir("repair-postcondition");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, false);
    drop(Store::new(&db).unwrap());
    {
        let conn = raw(&db);
        conn.execute("INSERT INTO device (id, pn, lid) VALUES (1, '15550001111:12@s.whatsapp.net', '100000000000009:12@lid')", []).unwrap();
        // A trigger that undoes the repair's update: every statement succeeds, the result is wrong.
        conn.execute_batch(
            "CREATE TRIGGER undo AFTER UPDATE OF from_me ON messages WHEN new.from_me = 1
             BEGIN UPDATE messages SET from_me = 0 WHERE id = new.id; END;",
        )
        .unwrap();
    }

    drop(Store::new(&db).expect("the start goes on"));
    {
        let conn = raw(&db);
        assert_fixture_rows(&conn, false);
        assert!(trigger_sql(&conn, "messages_fts_update").is_some(), "the trigger came back with the rollback");
        let note = metadata(&conn, "own_attribution_repair_failed").expect("the failure is noted");
        assert!(note.contains("post-condition"), "{note}");
        conn.execute_batch("DROP TRIGGER undo;").unwrap();
        age_failure_note(&conn);
    }

    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert_fixture_rows(&conn, true);
    assert!(metadata(&conn, "own_attribution_repair_failed").is_none());
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_repair_at_the_migrating_start_still_seeds_the_watchdog_baseline() {
    let dir = test_dir("repair-fails-at-migration");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    // Fails only for an update of from_me; the validation probe updates the text, which passes.
    raw(&db)
        .execute_batch("CREATE TRIGGER poison BEFORE UPDATE OF from_me ON messages BEGIN SELECT RAISE(ABORT, 'poison'); END;")
        .unwrap();

    let store = Store::new(&db).expect("the migration succeeds and the start goes on");
    {
        let conn = store.conn.lock();
        assert_eq!(user_version(&conn), 9);
        assert!(metadata(&conn, "watchdog_last_alerted_size").is_some(), "seeded after the repair, failed or not");
        assert!(metadata(&conn, "own_attribution_repair_failed").is_some());
    }
    assert!(pre_repair_backups(&db).is_empty(), "the pre-migration backup covers the migrating start");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_repair_is_skipped_when_its_backup_cannot_be_written_and_runs_at_the_next_start() {
    let dir = test_dir("repair-backup-fails");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    drop(open_with_mode(&db, MigrationMode::Normal).unwrap());
    let conn = raw(&db);
    conn.execute("UPDATE messages SET from_me = 0 WHERE message_id = 'L1'", []).unwrap();

    // The backup goes next to a database path whose directory does not exist: it fails, nothing is written.
    let nowhere = dir.join("no-such-directory").join("whatsapp.db");
    assert!(repair_own_attribution(&conn, &nowhere, false).unwrap().is_none());
    assert_eq!(from_me_of(&conn, "L1"), 0, "skipped this start");
    assert!(metadata(&conn, "own_attribution_repair_failed").is_none(), "a backup failure is not a repair failure");

    // The next start (a working backup) repairs.
    let report = repair_own_attribution(&conn, &db, false).unwrap().expect("repaired");
    assert_eq!(report.live_from_me, 1);
    assert_eq!(from_me_of(&conn, "L1"), 1);
    assert_eq!(pre_repair_backups(&db).len(), 1);
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_repair_binds_in_chunks_of_at_most_500_values() {
    // 1,200 distinct history senders (not the owner): three chunks, all repaired.
    let dir = test_dir("repair-chunks");
    let db = dir.join("whatsapp.db");
    let conn = raw(&db);
    conn.execute_batch(V8_MEDIA_REFS_DDL).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute("INSERT INTO device (id, pn, lid) VALUES (1, '15550001111@s.whatsapp.net', '')", []).unwrap();
    for i in 0..1_200 {
        put(&conn, &format!("C{i}"), &format!("{i}@g.us"), &format!("{i}@g.us"), 1, "backfill", "text");
    }
    // Lower SQLite's bind limit on this connection: a statement with one `IN (…)` of 1,200 values
    // (a regression to no chunking) fails to prepare, while chunks of 500 (+ the one bound sender) fit.
    let previous = unsafe { rusqlite::ffi::sqlite3_limit(conn.handle(), rusqlite::ffi::SQLITE_LIMIT_VARIABLE_NUMBER, 600) };
    assert!(previous >= 600);
    let report = repair_own_attribution(&conn, &db, true).unwrap().unwrap();
    assert_eq!(report.history_sender, 1_200);
    let wrong: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages WHERE sender_jid <> '15550001111@s.whatsapp.net'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(wrong, 0);
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_v9_db_one_column_short_gets_it_at_a_normal_start_and_at_migrate() {
    let dir = test_dir("ensure-columns");
    let make = |name: &str, validated: &str| -> PathBuf {
        let db = dir.join(name);
        v8_fixture(&db, false);
        drop(Store::new(&db).unwrap());
        let conn = raw(&db);
        // A DB from a build in between: version 9, one column short.
        conn.execute_batch("ALTER TABLE media_refs DROP COLUMN media_key_timestamp;").unwrap();
        conn.execute("UPDATE metadata SET value = ?1 WHERE key = 'schema_validated_version'", [validated]).unwrap();
        assert!(!table_info(&conn, "media_refs").iter().any(|c| c.0 == "media_key_timestamp"));
        db
    };
    let columns = |db: &Path| -> Vec<String> { table_info(&raw(db), "media_refs").into_iter().map(|c| c.0).collect() };

    // A stale validated version: `--migrate` (ForceMigrate) repairs it, and validation passes.
    let stale = make("stale.db", "8");
    drop(open_with_mode(&stale, MigrationMode::ForceMigrate).unwrap());
    assert!(columns(&stale).contains(&"media_key_timestamp".to_owned()));
    assert_eq!(metadata(&raw(&stale), "schema_validated_version").as_deref(), Some("9"));

    // The same on a normal start: the re-validation branch adds it too.
    let normal = make("normal.db", "8");
    drop(open_with_mode(&normal, MigrationMode::Normal).unwrap());
    assert!(columns(&normal).contains(&"media_key_timestamp".to_owned()));

    // A validated version that says 9 while the structure fell behind (a build in between wrote it):
    // a normal start adds the column too, since it checks the v9 columns at every start, so no later
    // media insert fails on it; `--migrate` also re-validates.
    let current = make("current.db", "9");
    drop(open_with_mode(&current, MigrationMode::Normal).unwrap());
    assert!(columns(&current).contains(&"media_key_timestamp".to_owned()), "added at a normal start");
    let forced = make("forced.db", "9");
    drop(open_with_mode(&forced, MigrationMode::ForceMigrate).unwrap());
    assert!(columns(&forced).contains(&"media_key_timestamp".to_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// History ingest: one transaction, the columns, the duplicate rule
// ---------------------------------------------------------------------------

async fn ingest_store(name: &str) -> (Store, PathBuf) {
    let dir = test_dir(name);
    let store = Store::new(&dir.join("whatsapp.db")).unwrap();
    {
        let conn = store.conn.lock();
        for (lid, phone) in [(PEER_LID, PEER_PHONE), ("100000000000004", PEER_PHONE)] {
            conn.execute(
                "INSERT INTO lid_pn_mapping (lid, phone_number, created_at, updated_at, learning_source) VALUES (?1, ?2, 1, 1, 'test')",
                [lid, phone],
            )
            .unwrap();
        }
    }
    (store, dir)
}

fn hist(id: &str, chat: &str, sender: &str, kind: &str, from_me: bool, source: &str) -> HistoryMessageRow {
    HistoryMessageRow {
        chat_jid: chat.to_owned(),
        sender_jid: sender.to_owned(),
        message_id: id.to_owned(),
        content_kind: kind.to_owned(),
        body_text: Some(format!("[{kind}]")),
        timestamp: 1_000,
        from_me,
        source: source.to_owned(),
        embed_status: "skipped".to_owned(),
    }
}

fn full_ref() -> MediaRefRow {
    MediaRefRow {
        media_key: Some(vec![1, 2]),
        direct_path: Some("/v/t62/new".to_owned()),
        file_enc_sha256: Some(vec![3]),
        mimetype: Some("image/jpeg".to_owned()),
        file_length: Some(100),
        width: Some(10),
        height: Some(20),
        file_name: Some("a.pdf".to_owned()),
        file_sha256: Some(vec![9, 9]),
        remote_jid: Some("100000000000001@lid".to_owned()),
        participant: None,
        view_once: Some(false),
        media_key_timestamp: Some(1_700_000_000),
    }
}

/// A v8-shaped reference: the encryption tuple and the plaintext properties of v8 are there, the six
/// v9 columns are NULL.
fn v8_ref() -> MediaRefRow {
    MediaRefRow {
        file_name: None,
        file_sha256: None,
        remote_jid: None,
        participant: None,
        view_once: None,
        media_key_timestamp: None,
        ..full_ref()
    }
}

/// What a v8-shaped reference holds after a duplicate filled it from [`full_ref`]: the plaintext
/// v9 columns are filled, the complete encryption tuple is left alone (so no `media_key_timestamp`).
fn v8_ref_filled() -> MediaRefRow {
    MediaRefRow { media_key_timestamp: None, ..full_ref() }
}

fn stored_ref(dir: &Path, id: &str) -> Option<(MediaRefRow, Option<String>)> {
    let conn = raw(&dir.join("whatsapp.db"));
    conn.query_row(
        "SELECT media_key, direct_path, file_enc_sha256, mimetype, file_length, width, height,
                file_name, file_sha256, remote_jid, participant, view_once, media_key_timestamp, hydrated_path
         FROM media_refs WHERE message_id = ?1",
        [id],
        |r| {
            Ok((
                MediaRefRow {
                    media_key: r.get(0)?,
                    direct_path: r.get(1)?,
                    file_enc_sha256: r.get(2)?,
                    mimetype: r.get(3)?,
                    file_length: r.get(4)?,
                    width: r.get(5)?,
                    height: r.get(6)?,
                    file_name: r.get(7)?,
                    file_sha256: r.get(8)?,
                    remote_jid: r.get(9)?,
                    participant: r.get(10)?,
                    view_once: r.get(11)?,
                    media_key_timestamp: r.get(12)?,
                },
                r.get(13)?,
            ))
        },
    )
    .optional()
    .unwrap()
}

fn message_count(dir: &Path) -> i64 {
    raw(&dir.join("whatsapp.db")).query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap()
}

#[tokio::test]
async fn a_new_history_message_and_its_reference_are_written_with_every_column() {
    let (store, dir) = ingest_store("ingest-new").await;

    let with_ref = store.insert_history_message(hist("A", PEER_JID, PEER_JID, "image", false, "bootstrap"), Some(full_ref())).await.unwrap();
    let without = store.insert_history_message(hist("B", PEER_JID, PEER_JID, "text", false, "bootstrap"), None).await.unwrap();

    assert_eq!(with_ref, HistoryWrite::Stored { media_ref: true });
    assert_eq!(without, HistoryWrite::Stored { media_ref: false });
    assert_eq!(stored_ref(&dir, "A"), Some((full_ref(), None)), "all fourteen columns, hydrated_path NULL");
    assert!(stored_ref(&dir, "B").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_message_and_its_reference_are_one_transaction() {
    let (store, dir) = ingest_store("ingest-one-tx").await;
    store.conn.lock().execute_batch("DROP TABLE media_refs").unwrap();

    let result = store.insert_history_message(hist("A", PEER_JID, PEER_JID, "image", false, "bootstrap"), Some(full_ref())).await;

    assert!(result.is_err(), "the failed reference fails the write");
    assert_eq!(message_count(&dir), 0, "and the message row is rolled back with it");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_duplicate_fills_a_v8_reference_then_reports_unchanged_and_never_touches_hydrated_path() {
    let (store, dir) = ingest_store("dup-fill").await;
    let row = || hist("A", PEER_JID, PEER_JID, "image", false, "bootstrap");
    store.insert_history_message(row(), Some(v8_ref())).await.unwrap();
    store.conn.lock().execute("UPDATE media_refs SET hydrated_path = 'chat/file.jpg' WHERE message_id = 'A'", []).unwrap();

    let filled = store.insert_history_message(row(), Some(full_ref())).await.unwrap();
    let again = store.insert_history_message(row(), Some(full_ref())).await.unwrap();

    assert_eq!(filled, HistoryWrite::Duplicate(DuplicateMedia::Filled));
    assert_eq!(again, HistoryWrite::Duplicate(DuplicateMedia::Unchanged), "the same data again changes nothing");
    assert_eq!(stored_ref(&dir, "A"), Some((v8_ref_filled(), Some("chat/file.jpg".to_owned()))));
    assert_eq!(message_count(&dir), 1);

    // A stale non-NULL value is kept (fill only): a re-fetch never refreshes a link.
    let newer = MediaRefRow { direct_path: Some("/v/t62/other".to_owned()), ..full_ref() };
    assert_eq!(
        store.insert_history_message(row(), Some(newer)).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Unchanged)
    );
    assert_eq!(stored_ref(&dir, "A").unwrap().0.direct_path.as_deref(), Some("/v/t62/new"));
    // A duplicate without a reference compares nothing.
    assert_eq!(store.insert_history_message(row(), None).await.unwrap(), HistoryWrite::Duplicate(DuplicateMedia::NoMedia));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn view_once_takes_the_larger_value_and_is_never_lowered() {
    let (store, dir) = ingest_store("dup-view-once").await;
    use DuplicateMedia::{Filled, Unchanged};
    // (stored, incoming, stored afterwards, result): everything else in the two references is equal.
    let cases = [
        (None, Some(false), Some(false), Filled),
        (None, Some(true), Some(true), Filled),
        (Some(true), Some(false), Some(true), Unchanged),
        (Some(false), None, Some(false), Unchanged),
        (Some(false), Some(true), Some(true), Filled),
        (Some(true), None, Some(true), Unchanged),
        (Some(true), Some(true), Some(true), Unchanged),
        (None, None, None, Unchanged),
    ];
    for (i, (stored, incoming, expected, result)) in cases.into_iter().enumerate() {
        let id = format!("V{i}");
        let row = || hist(&id, PEER_JID, PEER_JID, "image", false, "bootstrap");
        store.insert_history_message(row(), Some(MediaRefRow { view_once: stored, ..full_ref() })).await.unwrap();
        let written = store.insert_history_message(row(), Some(MediaRefRow { view_once: incoming, ..full_ref() })).await.unwrap();
        assert_eq!(written, HistoryWrite::Duplicate(result), "stored {stored:?} + incoming {incoming:?}");
        assert_eq!(stored_ref(&dir, &id).unwrap().0.view_once, expected, "stored {stored:?} + incoming {incoming:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The encryption tuple (`media_key`, `direct_path`, `file_enc_sha256`, `media_key_timestamp`) is
/// filled only all together: into a stored tuple whose three required parts are all NULL, from an
/// incoming tuple whose three required parts are all non-NULL, with the incoming timestamp.
#[tokio::test]
async fn the_encryption_tuple_is_filled_only_all_together_and_only_into_an_empty_one() {
    let (store, dir) = ingest_store("dup-tuple").await;
    let row = |id: &str| hist(id, PEER_JID, PEER_JID, "image", false, "bootstrap");
    let incoming = MediaRefRow { media_key: Some(vec![7, 7]), direct_path: Some("/v/t62/incoming".to_owned()), file_enc_sha256: Some(vec![8]), ..full_ref() };
    let no_tuple = |r: MediaRefRow| MediaRefRow { media_key: None, direct_path: None, file_enc_sha256: None, media_key_timestamp: None, ..r };

    // Only `direct_path` NULL: no mixed tuple. The row has every plaintext property, so nothing changes.
    let partial = MediaRefRow { direct_path: None, media_key_timestamp: None, ..full_ref() };
    store.insert_history_message(row("PARTIAL"), Some(partial.clone())).await.unwrap();
    assert_eq!(
        store.insert_history_message(row("PARTIAL"), Some(incoming.clone())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Unchanged)
    );
    assert_eq!(stored_ref(&dir, "PARTIAL"), Some((partial, None)), "neither the missing path nor the timestamp was filled");

    // The same with only `media_key` NULL, and only `file_enc_sha256` NULL.
    for (id, gap) in [
        ("NOKEY", MediaRefRow { media_key: None, media_key_timestamp: None, ..full_ref() }),
        ("NOENC", MediaRefRow { file_enc_sha256: None, media_key_timestamp: None, ..full_ref() }),
    ] {
        store.insert_history_message(row(id), Some(gap.clone())).await.unwrap();
        assert_eq!(
            store.insert_history_message(row(id), Some(incoming.clone())).await.unwrap(),
            HistoryWrite::Duplicate(DuplicateMedia::Unchanged),
            "{id}"
        );
        assert_eq!(stored_ref(&dir, id), Some((gap, None)), "{id}");
    }

    // All three NULL and a complete incoming tuple: the whole tuple is filled, the timestamp with it.
    let empty = no_tuple(full_ref());
    store.insert_history_message(row("EMPTY"), Some(empty.clone())).await.unwrap();
    assert_eq!(
        store.insert_history_message(row("EMPTY"), Some(incoming.clone())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Filled)
    );
    assert_eq!(stored_ref(&dir, "EMPTY"), Some((incoming.clone(), None)));
    // ... and once filled, the same data again is exact: Unchanged.
    assert_eq!(
        store.insert_history_message(row("EMPTY"), Some(incoming.clone())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Unchanged)
    );

    // A partial incoming tuple into an all-NULL stored tuple: no fill, not even of the parts it has.
    for (id, part) in [
        ("INKEY", MediaRefRow { media_key: Some(vec![7, 7]), ..no_tuple(incoming.clone()) }),
        ("INPATH", MediaRefRow { direct_path: Some("/v/t62/incoming".to_owned()), ..no_tuple(incoming.clone()) }),
        ("INENC", MediaRefRow { file_enc_sha256: Some(vec![8]), ..no_tuple(incoming.clone()) }),
        ("INKEY2", MediaRefRow { media_key: Some(vec![7, 7]), direct_path: Some("/v/t62/incoming".to_owned()), ..no_tuple(incoming.clone()) }),
    ] {
        store.insert_history_message(row(id), Some(empty.clone())).await.unwrap();
        assert_eq!(
            store.insert_history_message(row(id), Some(part)).await.unwrap(),
            HistoryWrite::Duplicate(DuplicateMedia::Unchanged),
            "{id}"
        );
        assert_eq!(stored_ref(&dir, id), Some((empty.clone(), None)), "{id}: still an empty tuple");
    }
    // An incoming timestamp alone changes nothing, whatever is stored.
    let stamp_only = MediaRefRow { media_key_timestamp: Some(1_800_000_000), ..no_tuple(incoming.clone()) };
    store.insert_history_message(row("STAMP1"), Some(empty.clone())).await.unwrap();
    assert_eq!(store.insert_history_message(row("STAMP1"), Some(stamp_only.clone())).await.unwrap(), HistoryWrite::Duplicate(DuplicateMedia::Unchanged));
    assert_eq!(stored_ref(&dir, "STAMP1"), Some((empty.clone(), None)));
    store.insert_history_message(row("STAMP2"), Some(v8_ref())).await.unwrap();
    assert_eq!(
        store.insert_history_message(row("STAMP2"), Some(MediaRefRow { media_key_timestamp: Some(1_800_000_000), ..v8_ref() })).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Unchanged),
        "a complete stored tuple without a timestamp does not get one alone"
    );
    assert_eq!(stored_ref(&dir, "STAMP2").unwrap().0.media_key_timestamp, None);

    // A stored timestamp with an all-NULL tuple, and a complete incoming tuple: the incoming
    // timestamp wins (the stored one belonged to no tuple), even when it is the older number.
    let stamped_empty = MediaRefRow { media_key_timestamp: Some(1_900_000_000), ..no_tuple(full_ref()) };
    store.insert_history_message(row("STALE"), Some(stamped_empty.clone())).await.unwrap();
    assert_eq!(stored_ref(&dir, "STALE").unwrap().0.media_key_timestamp, Some(1_900_000_000));
    assert_eq!(
        store.insert_history_message(row("STALE"), Some(incoming.clone())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Filled)
    );
    assert_eq!(stored_ref(&dir, "STALE"), Some((incoming.clone(), None)), "the incoming timestamp came with the tuple");
    // ... also when the incoming tuple has no timestamp: the stale one does not stay with a foreign tuple.
    store.insert_history_message(row("STALE2"), Some(stamped_empty)).await.unwrap();
    let incoming_unstamped = MediaRefRow { media_key_timestamp: None, ..incoming.clone() };
    assert_eq!(
        store.insert_history_message(row("STALE2"), Some(incoming_unstamped.clone())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Filled)
    );
    assert_eq!(stored_ref(&dir, "STALE2"), Some((incoming_unstamped, None)));

    // A complete tuple without a timestamp (a v8 row) gets the plaintext v9 columns, not the timestamp.
    store.insert_history_message(row("V8"), Some(v8_ref())).await.unwrap();
    assert_eq!(
        store.insert_history_message(row("V8"), Some(full_ref())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Filled),
        "the plaintext v9 columns are filled"
    );
    assert_eq!(stored_ref(&dir, "V8"), Some((v8_ref_filled(), None)));
    // Plaintext properties still fill one column at a time, whatever the tuple.
    let plain_gap = MediaRefRow { width: None, ..full_ref() };
    store.insert_history_message(row("PLAIN"), Some(plain_gap)).await.unwrap();
    assert_eq!(
        store.insert_history_message(row("PLAIN"), Some(full_ref())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Filled)
    );
    assert_eq!(stored_ref(&dir, "PLAIN"), Some((full_ref(), None)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The merge itself is a pure function; its rules in one table.
#[test]
fn merge_media_ref_is_the_one_place_for_the_fill_rules() {
    let stored = v8_ref();
    // Nothing new: the merge is the stored reference.
    assert_eq!(merge_media_ref(&stored, &MediaRefRow::default()), stored);
    assert_eq!(merge_media_ref(&full_ref(), &full_ref()), full_ref());
    // A NULL plaintext column takes the incoming value; a set one keeps its own.
    let merged = merge_media_ref(
        &MediaRefRow { mimetype: Some("a/b".to_owned()), width: None, ..v8_ref() },
        &MediaRefRow { mimetype: Some("c/d".to_owned()), width: Some(5), ..full_ref() },
    );
    assert_eq!((merged.mimetype.as_deref(), merged.width), (Some("a/b"), Some(5)));
    // view_once: NULL takes any value, 0 is raised to 1, 1 stays.
    let vo = |stored: Option<bool>, incoming: Option<bool>| {
        merge_media_ref(&MediaRefRow { view_once: stored, ..v8_ref() }, &MediaRefRow { view_once: incoming, ..v8_ref() }).view_once
    };
    assert_eq!((vo(None, Some(false)), vo(None, Some(true)), vo(Some(false), Some(true))), (Some(false), Some(true), Some(true)));
    assert_eq!((vo(Some(true), Some(false)), vo(Some(true), None), vo(Some(false), None), vo(None, None)), (Some(true), Some(true), Some(false), None));
}

#[tokio::test]
async fn a_duplicate_creates_a_missing_reference_for_history_rows_and_not_for_live_rows() {
    let (store, dir) = ingest_store("dup-create").await;
    for (id, source) in [("BOOT", "bootstrap"), ("BACK", "backfill"), ("LIVE", "live")] {
        store
            .insert_message(PEER_JID, PEER_PHONE, id, "image", Some("[image]"), 1_000, false, source, "skipped")
            .await
            .unwrap();
    }
    // The live row stores a bare sender; the incoming history message names the full JID.
    for (id, expected) in [
        ("BOOT", DuplicateMedia::Created),
        ("BACK", DuplicateMedia::Created),
        ("LIVE", DuplicateMedia::LiveRow),
    ] {
        let result = store.insert_history_message(hist(id, PEER_JID, PEER_JID, "image", false, "bootstrap"), Some(full_ref())).await.unwrap();
        assert_eq!(result, HistoryWrite::Duplicate(expected), "{id}");
    }
    assert_eq!(stored_ref(&dir, "BOOT"), Some((full_ref(), None)));
    assert_eq!(stored_ref(&dir, "BACK"), Some((full_ref(), None)));
    assert!(stored_ref(&dir, "LIVE").is_none(), "live media stays without a reference");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_mismatch_changes_nothing_and_is_reported() {
    let (store, dir) = ingest_store("dup-mismatch").await;
    store.insert_history_message(hist("A", PEER_JID, PEER_JID, "image", false, "bootstrap"), Some(v8_ref())).await.unwrap();
    store.insert_history_message(hist("G", GROUP, "15559998888@s.whatsapp.net", "image", false, "bootstrap"), Some(v8_ref())).await.unwrap();

    let other_person = "15559990000@s.whatsapp.net";
    let cases = [
        ("another chat, the same sender", hist("A", other_person, PEER_JID, "image", false, "bootstrap")),
        ("another sender, the same chat", hist("A", PEER_JID, other_person, "image", false, "bootstrap")),
        ("another chat and sender", hist("A", other_person, other_person, "image", false, "bootstrap")),
        ("another from_me", hist("A", PEER_JID, PEER_JID, "image", true, "bootstrap")),
        ("another kind", hist("A", PEER_JID, PEER_JID, "video", false, "bootstrap")),
        ("group vs 1:1 chat", hist("G", PEER_JID, "15559998888@s.whatsapp.net", "image", false, "bootstrap")),
        ("another group", hist("G", "120363000000000002@g.us", "15559998888@s.whatsapp.net", "image", false, "bootstrap")),
    ];
    for (what, row) in cases {
        let result = store.insert_history_message(row, Some(full_ref())).await.unwrap();
        assert_eq!(result, HistoryWrite::Duplicate(DuplicateMedia::Mismatch), "{what}");
    }
    assert_eq!(stored_ref(&dir, "A"), Some((v8_ref(), None)), "nothing was filled");
    assert_eq!(stored_ref(&dir, "G"), Some((v8_ref(), None)));
    // A mismatching duplicate with no stored reference creates none either.
    store.insert_message(PEER_JID, PEER_JID, "NOREF", "image", None, 1, false, "bootstrap", "skipped").await.unwrap();
    let result = store.insert_history_message(hist("NOREF", other_person, other_person, "image", false, "bootstrap"), Some(full_ref())).await.unwrap();
    assert_eq!(result, HistoryWrite::Duplicate(DuplicateMedia::Mismatch));
    assert!(stored_ref(&dir, "NOREF").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn phone_and_lid_forms_of_the_chat_and_the_sender_match_through_the_mapping() {
    let (store, dir) = ingest_store("dup-forms").await;
    store.insert_history_message(hist("A", PEER_JID, PEER_JID, "image", false, "bootstrap"), Some(v8_ref())).await.unwrap();

    // The same message fetched again in LID form: chat and sender are the mapped user.
    let lid_form = hist("A", "100000000000001@lid", "100000000000001@lid", "image", false, "backfill");
    assert_eq!(
        store.insert_history_message(lid_form, Some(full_ref())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Filled)
    );
    // A sender with a device part, and a second LID of the same number.
    let device_form = hist("A", "100000000000004@lid", "15551234567:75@s.whatsapp.net", "image", false, "backfill");
    assert_eq!(
        store.insert_history_message(device_form, Some(full_ref())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Unchanged)
    );
    // An unmapped LID is another user.
    let unmapped = hist("A", UNMAPPED_LID_JID, UNMAPPED_LID_JID, "image", false, "backfill");
    assert_eq!(
        store.insert_history_message(unmapped, Some(full_ref())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Mismatch)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_own_rows_match_whatever_sender_each_names_and_a_live_bare_sender_matches_a_full_jid() {
    let (store, dir) = ingest_store("dup-own").await;
    // A stored own row from before the fallback was fixed names the group; the incoming one the account.
    store.insert_history_message(hist("OWN", GROUP, GROUP, "image", true, "bootstrap"), Some(v8_ref())).await.unwrap();
    assert_eq!(
        store.insert_history_message(hist("OWN", GROUP, OWN_JID, "image", true, "backfill"), Some(full_ref())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::Filled)
    );
    // A stored live row with a bare sender matches the history message's full JID.
    store.insert_message(PEER_JID, PEER_PHONE, "LIVEBARE", "image", None, 1, false, "live", "skipped").await.unwrap();
    assert_eq!(
        store.insert_history_message(hist("LIVEBARE", PEER_JID, PEER_JID, "image", false, "bootstrap"), Some(full_ref())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::LiveRow)
    );
    // A bare LID user on a live row matches the phone form through the mapping.
    store.insert_message(PEER_JID, PEER_LID, "LIVELID", "image", None, 1, false, "live", "skipped").await.unwrap();
    assert_eq!(
        store.insert_history_message(hist("LIVELID", PEER_JID, PEER_JID, "image", false, "bootstrap"), Some(full_ref())).await.unwrap(),
        HistoryWrite::Duplicate(DuplicateMedia::LiveRow)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Backups, the failed re-validation pin, the validation probe, smaller guards
// ---------------------------------------------------------------------------

#[test]
fn db_dir_of_a_bare_file_name_is_the_current_directory() {
    assert_eq!(db_dir(Path::new("whatsapp.db")), Path::new("."));
    assert_eq!(db_dir(Path::new("runs/dev/whatsapp.db")), Path::new("runs/dev"));
    assert_eq!(db_dir(Path::new("/var/data/whatsapp.db")), Path::new("/var/data"));
}

#[test]
fn backups_are_private_and_a_failed_one_leaves_no_temporary_file() {
    let dir = test_dir("backups");
    let db = dir.join("whatsapp.db");
    let store = Store::new(&db).unwrap();
    let conn = store.conn.lock();

    for label in ["pre-repair", "pre-migration-v8"] {
        let bak = backup_db(&conn, &db, label).unwrap();
        assert!(bak.file_name().unwrap().to_string_lossy().contains(label), "{bak:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777, 0o600, "{label}");
        }
        assert_eq!(user_version(&raw(&bak)), 9, "a complete copy");
    }
    assert!(files_ending(&dir, ".tmp").is_empty());

    // The rename fails when the target name is a non-empty directory (this second, or the next):
    // the error path removes the temporary file and its journal.
    let now = now_secs();
    for ts in now..=now + 2 {
        let blocker = dir.join(format!("whatsapp.db.blocked-{ts}.bak"));
        std::fs::create_dir_all(&blocker).unwrap();
        std::fs::write(blocker.join("keep"), b"x").unwrap();
    }
    assert!(backup_db(&conn, &db, "blocked").is_err());
    assert!(files_ending(&dir, ".tmp").is_empty(), "{:?}", files_ending(&dir, ".tmp"));
    assert!(files_ending(&dir, "-journal").is_empty());
    drop(conn);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_the_newest_three_pre_repair_backups_are_kept() {
    let dir = test_dir("prune");
    let db = dir.join("whatsapp.db");
    for ts in 1_700_000_001_u64..=1_700_000_005 {
        std::fs::write(dir.join(format!("whatsapp.db.pre-repair-{ts}.bak")), b"x").unwrap();
    }
    // Files that are not this database's pre-repair backups stay.
    std::fs::write(dir.join("whatsapp.db.pre-migration-v8-1700000001.bak"), b"x").unwrap();
    std::fs::write(dir.join("other.db.pre-repair-1700000001.bak"), b"x").unwrap();

    prune_pre_repair_backups(&db, 10);
    assert_eq!(pre_repair_backups(&db).len(), 5, "nothing to prune below the limit");
    prune_pre_repair_backups(&db, PRE_REPAIR_BACKUPS_KEPT);

    let names: Vec<String> = pre_repair_backups(&db).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(
        names,
        ["whatsapp.db.pre-repair-1700000003.bak", "whatsapp.db.pre-repair-1700000004.bak", "whatsapp.db.pre-repair-1700000005.bak"]
    );
    assert!(dir.join("whatsapp.db.pre-migration-v8-1700000001.bak").exists());
    assert!(dir.join("other.db.pre-repair-1700000001.bak").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_backup_prunes_the_old_ones_after_it_succeeds() {
    let dir = test_dir("prune-after-backup");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    drop(open_with_mode(&db, MigrationMode::Normal).unwrap());
    for ts in 1_700_000_001_u64..=1_700_000_004 {
        std::fs::write(dir.join(format!("whatsapp.db.pre-repair-{ts}.bak")), b"x").unwrap();
    }
    let conn = raw(&db);
    conn.execute("UPDATE messages SET from_me = 0 WHERE message_id = 'L1'", []).unwrap();

    repair_own_attribution(&conn, &db, false).unwrap().expect("repaired");

    let kept = pre_repair_backups(&db);
    assert_eq!(kept.len(), 3, "the new backup and the two newest old ones: {kept:?}");
    assert!(kept[0].to_string_lossy().ends_with("1700000003.bak") && kept[1].to_string_lossy().ends_with("1700000004.bak"));
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_scan_is_noted_like_a_failed_transaction() {
    let dir = test_dir("repair-scan-fails");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, true);
    let conn = raw(&db);
    conn.execute_batch("DROP TABLE backfill_cursor;").unwrap(); // the scan counts stale cursors

    assert!(repair_own_attribution(&conn, &db, true).is_err());

    let note = metadata(&conn, "own_attribution_repair_failed").expect("the failure is noted");
    let mut parts = note.splitn(3, ' ');
    assert!(parts.next().unwrap().parse::<i64>().unwrap() > 1_700_000_000);
    assert_eq!(parts.next(), Some("-"), "no backup was taken before the scan");
    assert!(pre_repair_backups(&db).is_empty());
    assert_eq!((from_me_of(&conn, "L1"), from_me_of(&conn, "L4")), (0, 0), "nothing was changed");
    // Within 24 hours the next call leaves it alone.
    assert!(repair_own_attribution(&conn, &db, true).unwrap().is_none());
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_forced_revalidation_writes_the_pin_so_rollback_stays_available() {
    let dir = test_dir("migrate-fails-again");
    let db = dir.join("whatsapp.db");
    v8_fixture(&db, false);
    drop(Store::new(&db).unwrap());
    assert!(find_newest_bak(&db).is_some(), "the migration took its backup");
    raw(&db)
        .execute_batch(
            "DROP TRIGGER messages_fts_update;
             CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages BEGIN SELECT 1; END;",
        )
        .unwrap();

    // `whatsrust --migrate` (ForceMigrate) clears any pin, re-validates, fails again ...
    let err = open_with_mode(&db, MigrationMode::ForceMigrate).err().expect("fails again");
    let text = store_error_text(&err);
    assert!(text.contains("update trigger"), "the cause is in the message: {text}");
    assert!(text.contains("circuit-breaker pin written"), "{text}");
    // ... and leaves the same pin as a failed migration: `--rollback` accepts a `failed` pin and
    // finds the backup.
    assert_eq!(read_pin(&db), ("failed".to_owned(), 8, 9));
    let pin = read_migration_pin_pub(&db).expect("a pin");
    assert_eq!(pin["state"].as_str(), Some("failed"), "the guard of do_rollback");
    assert!(pin["reason"].as_str().unwrap().contains("update trigger"));
    let bak = find_newest_bak_pub(&db).expect("do_rollback finds the backup");

    // A normal start tells what to do.
    let halted = open_with_mode(&db, MigrationMode::Normal).err().expect("pin inconsistent");
    assert!(store_error_text(&halted).contains("whatsrust --rollback"));

    // The rollback's file steps work, and the restored v8 DB migrates again.
    std::fs::copy(&bak, &db).unwrap();
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("whatsapp.db{ext}")));
    }
    write_migration_pin_pub(&db, "rolled_back", 8).unwrap();
    drop(open_with_mode(&db, MigrationMode::ForceMigrate).unwrap());
    assert_eq!(user_version(&raw(&db)), 9);

    // The same when the forced step cannot add a column (`media_refs` is a view).
    let db2 = dir.join("second.db");
    v8_fixture(&db2, false);
    drop(Store::new(&db2).unwrap());
    raw(&db2)
        .execute_batch(
            "DROP TABLE media_refs;
             CREATE VIEW media_refs AS SELECT 'x' AS message_id, NULL AS media_key, NULL AS direct_path,
                 NULL AS file_enc_sha256, NULL AS mimetype, NULL AS file_length, NULL AS width,
                 NULL AS height, NULL AS hydrated_path;",
        )
        .unwrap();
    assert!(open_with_mode(&db2, MigrationMode::ForceMigrate).is_err());
    assert_eq!(read_pin(&db2), ("failed".to_owned(), 8, 9));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_validation_probe_leaves_nothing_behind() {
    let dir = test_dir("probe-clean");
    let store = Store::new(&dir.join("whatsapp.db")).unwrap();
    let conn = store.conn.lock();
    conn.execute("INSERT INTO messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at) VALUES ('c', 's', 'real', 'text', 'real words', 1, 1)", []).unwrap();
    let state = |conn: &Connection| -> (String, Vec<(String, i64)>) {
        let mut stmt = conn.prepare("SELECT name, seq FROM sqlite_sequence ORDER BY name").unwrap();
        let sequences = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<std::result::Result<Vec<_>, _>>().unwrap();
        (snapshot(conn), sequences)
    };
    let before = state(&conn);

    validate_migration_post_commit(&conn).unwrap();

    assert_eq!(state(&conn), before, "no row, vector, index entry or sequence number is left");
    conn.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES('integrity-check');").unwrap();
    drop(conn);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_duplicate_whose_stored_row_cannot_be_read_fails_the_write() {
    let (store, dir) = ingest_store("dup-no-row").await;
    // An insert that is ignored without a stored row (here: a trigger), then the duplicate read
    // finds no row: that is an error, not a silent duplicate.
    store.conn.lock().execute_batch("CREATE TRIGGER skip BEFORE INSERT ON messages BEGIN SELECT RAISE(IGNORE); END;").unwrap();

    let result = store.insert_history_message(hist("A", PEER_JID, PEER_JID, "image", false, "bootstrap"), Some(full_ref())).await;

    assert!(result.is_err(), "QueryReturnedNoRows is reported");
    assert_eq!(message_count(&dir), 0);
    assert!(stored_ref(&dir, "A").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Budget: 500,000 messages, 50,000 rows to repair, migration plus repair under 30 s
// ---------------------------------------------------------------------------

const BUDGET_MESSAGES: i64 = 500_000;
const BUDGET_SECS: u64 = 30;

/// A synthetic v8 database of `BUDGET_MESSAGES` rows (per 20 rows: ten live received from 3,000
/// distinct senders, one live own with `from_me = 0` in five forms, one history own naming one of
/// 5,000 distinct groups as its sender, eight history received), so 50,000 rows need the repair.
/// The full-text triggers are off while it is filled and the index is rebuilt in one go.
fn build_budget_db(db: &Path) {
    let conn = raw(db);
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; PRAGMA journal_mode = WAL; PRAGMA synchronous = OFF;").unwrap();
    conn.execute_batch(V8_MEDIA_REFS_DDL).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute_batch(
        "DROP INDEX idx_messages_revoke_label;
         DROP TRIGGER messages_fts_insert; DROP TRIGGER messages_fts_update; DROP TRIGGER messages_fts_delete;",
    )
    .unwrap();
    conn.execute("INSERT INTO device (id, pn, lid) VALUES (1, '15550001111:12@s.whatsapp.net', '100000000000009:12@lid')", []).unwrap();
    conn.execute(
        "INSERT INTO lid_pn_mapping (lid, phone_number, created_at, updated_at, learning_source) VALUES ('100000000000009', '15550001111', 1, 1, 'test')",
        [],
    )
    .unwrap();
    conn.execute(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
         INSERT INTO messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at, from_me, source, embed_status)
         SELECT
            CASE WHEN i % 20 = 11 THEN printf('1203630000%05d@g.us', (i / 20) % 5000)
                 ELSE printf('1555%07d@s.whatsapp.net', i % 2500) END,
            CASE WHEN i % 20 < 10 THEN printf('1777%07d', (i / 20) % 3000)
                 WHEN i % 20 = 10 THEN CASE i % 5 WHEN 0 THEN '15550001111' WHEN 1 THEN '15550001111:12'
                                                  WHEN 2 THEN '15550001111@s.whatsapp.net' WHEN 3 THEN '100000000000009'
                                                  ELSE '100000000000009:12@lid' END
                 WHEN i % 20 = 11 THEN printf('1203630000%05d@g.us', (i / 20) % 5000)
                 ELSE printf('1556%07d@s.whatsapp.net', i % 4000) END,
            'M' || i,
            'text',
            'budget message ' || i || ' lorem ipsum dolor sit amet consectetur',
            1700000000 + i,
            1700000000 + i,
            CASE WHEN i % 20 = 11 THEN 1 ELSE 0 END,
            CASE WHEN i % 20 < 11 THEN 'live' WHEN i % 2 = 0 THEN 'bootstrap' ELSE 'backfill' END,
            'skipped'
         FROM n",
        [BUDGET_MESSAGES],
    )
    .unwrap();
    conn.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES('rebuild');").unwrap();
    conn.execute_batch(MESSAGES_FTS_UPDATE_TRIGGER).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts(rowid, body_text) VALUES (new.id, new.body_text);
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
             INSERT INTO messages_fts(messages_fts, rowid, body_text) VALUES('delete', old.id, old.body_text);
         END;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 8_i64).unwrap();
    conn.execute("INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_validated_version', '8')", []).unwrap();
}

/// `cargo test budget -- --ignored --nocapture`. Ignored in the default suite: building the
/// 500,000-row database takes a while.
#[test]
#[ignore = "builds a 500,000-message database; run explicitly with --ignored --nocapture"]
fn budget_500k_messages_50k_to_repair_migrate_and_repair_under_30_seconds() {
    let dir = test_dir("budget");
    let db = dir.join("whatsapp.db");
    let built = std::time::Instant::now();
    build_budget_db(&db);
    let db_bytes = std::fs::metadata(&db).unwrap().len();
    println!("budget: built {BUDGET_MESSAGES} messages in {:.1} s; v8 database file {:.1} MiB", built.elapsed().as_secs_f64(), db_bytes as f64 / 1_048_576.0);

    // Watch the write-ahead log while the migration and the repair run.
    let wal = dir.join("whatsapp.db-wal");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let watcher = {
        let (stop, peak, wal) = (stop.clone(), peak.clone(), wal.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if let Ok(meta) = std::fs::metadata(&wal) {
                    peak.fetch_max(meta.len(), Ordering::SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };

    let started = std::time::Instant::now();
    let store = open_with_mode(&db, MigrationMode::Normal).unwrap();
    let elapsed = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    watcher.join().unwrap();

    let wal_peak = peak.load(Ordering::SeqCst);
    let (live_fixed, history_fixed, total): (i64, i64, i64) = {
        let conn = store.conn.lock();
        (
            conn.query_row("SELECT COUNT(*) FROM messages WHERE source = 'live' AND from_me = 1", [], |r| r.get(0)).unwrap(),
            conn.query_row("SELECT COUNT(*) FROM messages WHERE source <> 'live' AND sender_jid = '15550001111@s.whatsapp.net'", [], |r| r.get(0)).unwrap(),
            conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap(),
        )
    };
    println!(
        "budget: migration + backup + validation + repair of {total} messages took {:.2} s (limit {BUDGET_SECS} s); WAL peak {:.1} MiB; database file now {:.1} MiB",
        elapsed.as_secs_f64(),
        wal_peak as f64 / 1_048_576.0,
        std::fs::metadata(&db).unwrap().len() as f64 / 1_048_576.0,
    );
    assert_eq!(total, BUDGET_MESSAGES);
    assert_eq!(live_fixed, 25_000, "live own rows now from_me");
    assert_eq!(history_fixed, 25_000, "history own rows now sent by the owner");
    assert!(elapsed.as_secs() < BUDGET_SECS, "took {elapsed:?}");

    // The steady state: the repair runs at every start, so a start that finds nothing to change
    // (the classification scan only: two DISTINCT scans and the senders' classification) must be cheap.
    let noop = {
        let conn = store.conn.lock();
        let started = std::time::Instant::now();
        let outcome = repair_own_attribution(&conn, &db, false).unwrap();
        assert!(outcome.is_none(), "nothing left to repair");
        started.elapsed()
    };
    drop(store);
    let started = std::time::Instant::now();
    let store = open_with_mode(&db, MigrationMode::Normal).unwrap();
    let second_open = started.elapsed();
    println!(
        "budget: a no-op repair (scan only) took {:.0} ms; a whole second start (schema, FTS probe, no-op repair) {:.0} ms",
        noop.as_secs_f64() * 1000.0,
        second_open.as_secs_f64() * 1000.0,
    );
    assert!(noop.as_secs() < BUDGET_SECS && second_open.as_secs() < BUDGET_SECS);
    assert!(pre_repair_backups(&db).is_empty(), "a no-op start takes no backup");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The in-place downgrade, proven with a real v8 binary
// ---------------------------------------------------------------------------

/// The way back after a healthy v8 -> v9 migration (design v5.3 §4, plan §2.4): set
/// `PRAGMA user_version = 8`, start the v8 build, which accepts the DB and re-validates at its own
/// version; the v9 build then migrates again (the step is re-runnable), keeps the owner marker (its
/// first-run time), and its idempotent repair puts right the row that was misattributed meanwhile.
///
/// Needs a real v8 build of whatsrust, named by `WHATSRUST_V8_BIN` (only that variable; there is no
/// default path). Build one from `main`, or from a commit before the v9 work, with a versioned
/// `make build` in a checkout of that commit (a separate `git worktree` keeps this tree as it is;
/// give it its own `CARGO_TARGET_DIR` or expect a full debug build there). The binary is
/// `target/debug/whatsrust-<version>-<commit>[-dirty]`; `whatsrust --version` prints the same string.
///
/// The test runs only the offline maintenance command `--migrate` (open, validate, exit; it never
/// connects) inside a fresh temporary directory without a `.env` and with an empty environment, on a
/// synthetic database, and kills it after 60 s.
///
/// `WHATSRUST_V8_BIN=/path/to/whatsrust-<version>-<commit> cargo test --lib downgrade_to_v8 -- --ignored --nocapture`
#[test]
#[ignore = "needs a real v8 binary: set WHATSRUST_V8_BIN"]
fn downgrade_to_v8_with_the_real_v8_binary_then_upgrade_again() {
    let bin = PathBuf::from(
        std::env::var("WHATSRUST_V8_BIN").expect("set WHATSRUST_V8_BIN to a v8 whatsrust binary (see the test's doc comment)"),
    );
    assert!(bin.is_file(), "no such binary: {}", bin.display());
    let dir = test_dir("downgrade");
    let db = dir.join("whatsapp.db");
    assert!(!dir.join(".env").exists(), "the directory must have no .env");

    // 1. A synthetic v8 DB, migrated and repaired by the current code.
    v8_fixture(&db, true);
    drop(Store::new(&db).unwrap());
    let marker = {
        let conn = raw(&db);
        assert_eq!(user_version(&conn), 9);
        let marker = metadata(&conn, "own_attribution_repair").expect("the repair ran");
        // Undo one repaired row, as a v8 window would leave it.
        conn.execute("UPDATE messages SET from_me = 0 WHERE message_id = 'L1'", []).unwrap();
        // 2. The in-place downgrade.
        conn.pragma_update(None, "user_version", 8_i64).unwrap();
        marker
    };

    // 3. The v8 binary: `--migrate` opens the DB, validates it at its own version and exits.
    let version = std::process::Command::new(&bin).arg("--version").env_clear().output().expect("run --version");
    println!("downgrade: v8 binary says: {}", String::from_utf8_lossy(&version.stdout).trim());
    let log = dir.join("v8-stderr.txt");
    let mut child = std::process::Command::new(&bin)
        .arg("--migrate")
        .current_dir(&dir)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(std::fs::File::create(dir.join("v8-stdout.txt")).unwrap()))
        .stderr(std::process::Stdio::from(std::fs::File::create(&log).unwrap()))
        .spawn()
        .expect("start the v8 binary");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the v8 binary did not exit within 60 s and was killed: it must only open, validate and exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let stderr = std::fs::read_to_string(&log).unwrap();
    println!("downgrade: v8 --migrate exit {status}; stderr:\n{stderr}");
    assert!(status.success(), "the v8 binary refused the downgraded DB: {stderr}");
    assert!(stderr.contains("migration succeeded"), "{stderr}");
    assert!(!dir.join(".env").exists());

    {
        let conn = raw(&db);
        assert_eq!(user_version(&conn), 8, "still the v8 version");
        assert_eq!(metadata(&conn, "schema_validated_version").as_deref(), Some("8"), "the v8 build re-validated at its own version");
        assert!(!pin_path(&db).exists(), "no pin");
        assert_eq!(table_info(&conn, "media_refs").len(), 15, "the extra columns stay; the v8 build does not mind them");
        assert_eq!(metadata(&conn, "own_attribution_repair"), Some(marker.clone()));
    }

    // 4. The current build opens it again: it migrates again (the pre-migration backup covers that
    // start), keeps the marker, and the idempotent repair corrects the row undone above.
    drop(Store::new(&db).unwrap());
    let conn = raw(&db);
    assert_eq!(user_version(&conn), 9);
    assert_eq!(metadata(&conn, "schema_validated_version").as_deref(), Some("9"));
    assert_eq!(metadata(&conn, "own_attribution_repair"), Some(marker), "the marker is unchanged");
    assert_eq!(from_me_of(&conn, "L1"), 1, "the repair runs at every start and put the row right again");
    assert!(pre_repair_backups(&db).is_empty(), "the migrating start took the pre-migration backup instead");
    assert!(index_exists(&conn, "idx_messages_revoke_label"));
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}
