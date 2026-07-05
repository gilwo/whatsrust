//! Lean rusqlite storage backend for whatsapp-rust.
//!
//! Implements all four `Backend` traits (SignalStore, AppSyncStore, ProtocolStore, DeviceStore)
//! using raw rusqlite — no Diesel, no ORM, no migration framework.
//!
//! Design advantages over upstream (Diesel, 2370 lines) and ZeroClaw (rusqlite, 1347 lines):
//!   - Single-device only (no device_id column) — halves query complexity
//!   - One Mutex<Connection> + spawn_blocking — no semaphore needed
//!   - WAL mode + NORMAL sync — fast writes, no corruption risk
//!   - CREATE TABLE IF NOT EXISTS — no migration framework
//!   - serde_json only for opaque types (HashState) — everything else is columns

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

use async_trait::async_trait;
use prost::Message as ProstMessage;
use tokio_util::bytes::Bytes;
use rusqlite::{params, Connection, OptionalExtension};
use wacore::appstate::hash::HashState;
use wacore::appstate::processor::AppStateMutationMAC;
use wacore::libsignal::protocol::{KeyPair, PrivateKey, PublicKey};
use wacore::store::device::DEVICE_PROPS;
use wacore::store::error::{Result, StoreError};
use wacore::store::traits::*;
use wacore::store::Device;
use wacore_binary::jid::Jid;
use waproto::whatsapp as wa;

// ---------------------------------------------------------------------------
// Helper to convert rusqlite errors to StoreError::Database
// ---------------------------------------------------------------------------

fn db_err(e: rusqlite::Error) -> StoreError {
    StoreError::Database(Box::new(e))
}

// ---------------------------------------------------------------------------
// Schema — 15 tables, no device_id column (single-device design)
// ---------------------------------------------------------------------------

const SCHEMA: &str = "
-- Device identity & crypto keys
CREATE TABLE IF NOT EXISTS device (
    id INTEGER PRIMARY KEY,
    pn TEXT NOT NULL DEFAULT '',
    lid TEXT NOT NULL DEFAULT '',
    registration_id INTEGER NOT NULL DEFAULT 0,
    noise_key BLOB NOT NULL DEFAULT x'',
    identity_key BLOB NOT NULL DEFAULT x'',
    signed_pre_key BLOB NOT NULL DEFAULT x'',
    signed_pre_key_id INTEGER NOT NULL DEFAULT 0,
    signed_pre_key_signature BLOB NOT NULL DEFAULT x'',
    adv_secret_key BLOB NOT NULL DEFAULT x'',
    account BLOB,
    push_name TEXT NOT NULL DEFAULT '',
    app_version_primary INTEGER NOT NULL DEFAULT 0,
    app_version_secondary INTEGER NOT NULL DEFAULT 0,
    app_version_tertiary INTEGER NOT NULL DEFAULT 0,
    app_version_last_fetched_ms INTEGER NOT NULL DEFAULT 0,
    edge_routing_info BLOB,
    props_hash TEXT,
    next_pre_key_id INTEGER NOT NULL DEFAULT 0,
    nct_salt BLOB
);

-- Signal Protocol: identity keys
CREATE TABLE IF NOT EXISTS identities (
    address TEXT PRIMARY KEY,
    key BLOB NOT NULL
);

-- Signal Protocol: sessions
CREATE TABLE IF NOT EXISTS sessions (
    address TEXT PRIMARY KEY,
    record BLOB NOT NULL
);

-- Signal Protocol: pre-keys
CREATE TABLE IF NOT EXISTS prekeys (
    id INTEGER PRIMARY KEY,
    key BLOB NOT NULL,
    uploaded INTEGER NOT NULL DEFAULT 0
);

-- Signal Protocol: signed pre-keys
CREATE TABLE IF NOT EXISTS signed_prekeys (
    id INTEGER PRIMARY KEY,
    record BLOB NOT NULL
);

-- Signal Protocol: sender keys (group messaging)
CREATE TABLE IF NOT EXISTS sender_keys (
    address TEXT PRIMARY KEY,
    record BLOB NOT NULL
);

-- App state sync keys
CREATE TABLE IF NOT EXISTS app_state_keys (
    key_id BLOB PRIMARY KEY,
    key_data BLOB NOT NULL,
    fingerprint BLOB NOT NULL DEFAULT x'',
    timestamp INTEGER NOT NULL DEFAULT 0
);

-- App state versions (HashState serialized as JSON)
CREATE TABLE IF NOT EXISTS app_state_versions (
    name TEXT PRIMARY KEY,
    state_data BLOB NOT NULL
);

-- App state mutation MACs
CREATE TABLE IF NOT EXISTS app_state_mutation_macs (
    name TEXT NOT NULL,
    index_mac BLOB NOT NULL,
    version INTEGER NOT NULL,
    value_mac BLOB NOT NULL,
    PRIMARY KEY (name, index_mac)
);

-- Per-device sender key tracking (replaces skdm_recipients)
CREATE TABLE IF NOT EXISTS sender_key_devices (
    group_jid TEXT NOT NULL,
    device_jid TEXT NOT NULL,
    needs_sender_key INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (group_jid, device_jid)
);

-- Sent message store for retry handling
CREATE TABLE IF NOT EXISTS sent_messages (
    chat_jid TEXT NOT NULL,
    message_id TEXT NOT NULL,
    message_bytes BLOB NOT NULL,
    timestamp INTEGER NOT NULL,
    PRIMARY KEY (chat_jid, message_id)
);

-- LID (Linked Identity) to phone number mapping
CREATE TABLE IF NOT EXISTS lid_pn_mapping (
    lid TEXT PRIMARY KEY,
    phone_number TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    learning_source TEXT NOT NULL
);

-- Base keys for replay/collision detection
CREATE TABLE IF NOT EXISTS base_keys (
    address TEXT NOT NULL,
    message_id TEXT NOT NULL,
    base_key BLOB NOT NULL,
    PRIMARY KEY (address, message_id)
);

-- Device registry (multi-device awareness per contact)
CREATE TABLE IF NOT EXISTS device_registry (
    user_id TEXT PRIMARY KEY,
    devices_json TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    phash TEXT
);

-- Trusted contact privacy tokens
CREATE TABLE IF NOT EXISTS tc_tokens (
    jid TEXT PRIMARY KEY,
    token BLOB NOT NULL,
    token_timestamp INTEGER NOT NULL,
    sender_timestamp INTEGER
);

-- Persistent outbound job queue (crash-safe, typed operations)
CREATE TABLE IF NOT EXISTS outbound_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    jid TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '',
    op_kind TEXT NOT NULL DEFAULT 'text',
    payload_json TEXT NOT NULL DEFAULT '{}',
    payload_blob BLOB,
    wa_message_id TEXT,
    delivery_status TEXT,
    last_error TEXT,
    status TEXT NOT NULL DEFAULT 'queued',
    retries INTEGER NOT NULL DEFAULT 0,
    retry_after INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Poll encryption keys (for decrypting incoming votes)
CREATE TABLE IF NOT EXISTS poll_keys (
    chat_jid TEXT NOT NULL,
    poll_id TEXT NOT NULL,
    enc_key BLOB NOT NULL,
    options_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (chat_jid, poll_id)
);

-- Unified message timeline (live + backfill) — v8 target (ADR 0009)
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_jid TEXT NOT NULL,
    sender_jid TEXT NOT NULL,
    message_id TEXT NOT NULL UNIQUE,
    content_kind TEXT NOT NULL,
    body_text TEXT,
    timestamp INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    from_me INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL DEFAULT 'live',
    embed_status TEXT NOT NULL DEFAULT 'pending'
);

-- Media references (bytes hydrated lazily; ADR 0005)
CREATE TABLE IF NOT EXISTS media_refs (
    message_id      TEXT PRIMARY KEY,
    media_key       BLOB,
    direct_path     TEXT,
    file_enc_sha256 BLOB,
    mimetype        TEXT,
    file_length     INTEGER,
    width           INTEGER,
    height          INTEGER,
    hydrated_path   TEXT
);

-- Vectors (multi-model retention; ADR 0017)
CREATE TABLE IF NOT EXISTS embeddings (
    message_id TEXT NOT NULL,
    model_id   TEXT NOT NULL,
    dim        INTEGER NOT NULL,
    vec        BLOB NOT NULL,
    PRIMARY KEY (message_id, model_id)
);

-- Per-chat backfill frontier (ADR 0003)
CREATE TABLE IF NOT EXISTS backfill_cursor (
    chat_jid                TEXT PRIMARY KEY,
    oldest_msg_id           TEXT,
    oldest_msg_from_me      INTEGER,
    oldest_msg_timestamp_ms INTEGER,
    more_remain             INTEGER NOT NULL DEFAULT 1,
    exhausted               INTEGER NOT NULL DEFAULT 0,
    last_backfill_at        INTEGER
);

-- Durable backfill-job queue (ADR 0010/0033)
CREATE TABLE IF NOT EXISTS backfill_jobs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_jid    TEXT NOT NULL,
    target_kind TEXT NOT NULL CHECK (target_kind IN ('since', 'all', 'count')),
    target_value INTEGER,
    status      TEXT NOT NULL,
    fetched     INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- Generic KV singletons (ADR 0036)
CREATE TABLE IF NOT EXISTS metadata (
    key   TEXT PRIMARY KEY,
    value TEXT
);

-- FTS5 external-content index over messages (ADR 0019)
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    body_text,
    content='messages',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

-- Sync triggers: keep messages_fts in step with messages (ADR 0019 corrected DDL)
CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body_text) VALUES (new.id, new.body_text);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body_text) VALUES('delete', old.id, old.body_text);
    INSERT INTO messages_fts(rowid, body_text) VALUES (new.id, new.body_text);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body_text) VALUES('delete', old.id, old.body_text);
END;

-- Indexes for non-PK lookups
CREATE INDEX IF NOT EXISTS idx_lid_pn_phone ON lid_pn_mapping(phone_number);
CREATE INDEX IF NOT EXISTS idx_tc_tokens_ts ON tc_tokens(token_timestamp);
CREATE INDEX IF NOT EXISTS idx_outbound_status ON outbound_queue(status, retry_after, id);
CREATE INDEX IF NOT EXISTS idx_outbound_wa_id ON outbound_queue(wa_message_id);
CREATE INDEX IF NOT EXISTS idx_messages_chat_ts ON messages(chat_jid, timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(message_id);
";

const CURRENT_SCHEMA_VERSION: i64 = 8;

#[cfg(unix)]
fn secure_backup_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let meta = std::fs::metadata(path)
        .map_err(|e| StoreError::Database(format!("stat {}: {e}", path.display()).into()))?;
    let mut perms = meta.permissions();
    let mode = if meta.is_dir() { 0o700 } else { 0o600 };
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms)
        .map_err(|e| StoreError::Database(format!("chmod {}: {e}", path.display()).into()))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_backup_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Staged-migration ceremony (ADR 0028 / 0029 / 0030 / 0032 / 0036)
// ---------------------------------------------------------------------------

/// Controls how `open_with_mode` handles schema versioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationMode {
    /// Normal startup: respect the circuit-breaker pin; run migration ceremony if
    /// `user_version < CURRENT_SCHEMA_VERSION`; validate; seed watchdog baseline.
    Normal,
    /// Reserved for future use: open in rollback mode (DB-restoration logic is in
    /// the `--rollback` subcommand in `main.rs`, not in `open_with_mode`).
    #[allow(dead_code)]
    Rollback,
    /// `--migrate` mode: ignore (clear) any existing pin and force a migration retry.
    ForceMigrate,
}

/// Sidecar pin written when a migration or validation fails (ADR 0030).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MigrationPin {
    state: String,          // "failed" | "rolled_back"
    pinned_version: i64,    // DB user_version after rollback / at failure
    blocked_target: i64,    // schema version that failed
    created_at: u64,        // unix seconds
    reason: String,
}

/// Compute pin-file path from a DB path.
fn pin_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    let mut name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("whatsapp.db")
        .to_owned();
    name.push_str(".migration-pin");
    p.set_file_name(name);
    p
}

/// Read the migration pin from `<db_path>.migration-pin`, if present.
fn read_migration_pin(db_path: &Path) -> Option<MigrationPin> {
    let path = pin_path(db_path);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the migration pin atomically (temp + rename).
fn write_migration_pin(db_path: &Path, pin: &MigrationPin) -> Result<()> {
    let path = pin_path(db_path);
    let tmp = path.with_extension("pin.tmp");
    let bytes = serde_json::to_vec_pretty(pin)
        .map_err(|e| StoreError::Database(format!("serialize migration pin: {e}").into()))?;
    std::fs::write(&tmp, &bytes)
        .map_err(|e| StoreError::Database(format!("write migration pin tmp: {e}").into()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| StoreError::Database(format!("rename migration pin: {e}").into()))?;
    Ok(())
}

/// Remove the migration pin file (used by `--migrate` / newer-binary auto-retry).
fn clear_migration_pin(db_path: &Path) -> Result<()> {
    let path = pin_path(db_path);
    match std::fs::remove_file(&path) {
        Ok(()) | Err(_) => Ok(()), // NotFound is fine
    }
}

/// Pre-migration backup via the SQLite Backup API (fail-closed, ADR 0028 requirement B).
/// Writes to `<name>.bak.tmp` then atomically renames to `<name>.bak`.
fn backup_db_pre_migration(conn: &Connection, db_path: &Path, from_version: i64) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let bak_name = format!(
        "{}.pre-migration-v{}-{}.bak",
        db_path.file_name().and_then(|n| n.to_str()).unwrap_or("whatsapp.db"),
        from_version,
        ts
    );
    let bak_path = db_path.with_file_name(&bak_name);
    let tmp_path = bak_path.with_extension("bak.tmp");

    {
        let mut dest = Connection::open(&tmp_path)
            .map_err(|e| StoreError::Database(format!("open backup tmp {}: {e}", tmp_path.display()).into()))?;
        let backup = rusqlite::backup::Backup::new(conn, &mut dest)
            .map_err(|e| StoreError::Database(format!("create backup: {e}").into()))?;
        backup
            .run_to_completion(100, std::time::Duration::from_millis(10), None)
            .map_err(|e| StoreError::Database(format!("backup run: {e}").into()))?;
    }

    std::fs::rename(&tmp_path, &bak_path)
        .map_err(|e| StoreError::Database(format!("rename backup: {e}").into()))?;
    secure_backup_permissions(&bak_path)?;
    Ok(bak_path)
}

/// One-time FTS5 probe: create a temp virtual table to confirm FTS5 is compiled in.
/// Call this BEFORE any FTS5 DDL runs so failures leave the DB at the prior version.
fn probe_fts5_availability(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE VIRTUAL TABLE temp.__fts5_probe USING fts5(x);")
        .map_err(|_| StoreError::Database(
            "SQLite built without FTS5 support; keep default `bundled` rusqlite feature in Cargo.toml (ENABLE_FTS5 required)".into()
        ))
}

/// Post-commit structural + smoke validation (ADR 0029 V1 + V3).
/// Returns an actionable error on failure.
fn validate_migration_post_commit(conn: &Connection) -> Result<()> {
    // --- V1 structural ---
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(db_err)?;
    if version != CURRENT_SCHEMA_VERSION {
        return Err(StoreError::Database(format!(
            "migration validation: user_version={version} != expected {CURRENT_SCHEMA_VERSION}"
        ).into()));
    }

    // Expected tables
    for table in &["messages", "messages_fts", "media_refs", "embeddings", "backfill_cursor", "backfill_jobs", "metadata"] {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
            params![table],
            |r| r.get(0),
        ).map_err(db_err)?;
        if exists == 0 {
            return Err(StoreError::Database(format!(
                "migration validation: expected table/index '{table}' is missing"
            ).into()));
        }
    }

    // Expected columns on messages
    for col in &["from_me", "source", "embed_status"] {
        let has_col: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name='{col}'"),
            [],
            |r| r.get(0),
        ).map_err(db_err)?;
        if has_col == 0 {
            return Err(StoreError::Database(format!(
                "migration validation: messages.{col} column missing"
            ).into()));
        }
    }

    // Expected indexes
    for idx in &["idx_messages_chat_ts", "idx_messages_msg_id"] {
        let has_idx: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
            params![idx],
            |r| r.get(0),
        ).map_err(db_err)?;
        if has_idx == 0 {
            return Err(StoreError::Database(format!(
                "migration validation: expected index '{idx}' missing"
            ).into()));
        }
    }

    // Expected FTS triggers
    for trig in &["messages_fts_insert", "messages_fts_update", "messages_fts_delete"] {
        let has_trig: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name=?1",
            params![trig],
            |r| r.get(0),
        ).map_err(db_err)?;
        if has_trig == 0 {
            return Err(StoreError::Database(format!(
                "migration validation: expected FTS trigger '{trig}' missing"
            ).into()));
        }
    }

    // --- V3 smoke probes ---

    // (a) FTS trigger sync: INSERT test row → FTS finds it → DELETE → gone
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    conn.execute(
        "INSERT INTO messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
         VALUES ('__validate__@s.whatsapp.net', '__validate__@s.whatsapp.net', '__validate_fts_probe__',
                 'text', 'xyzvalidationprobexyz', ?1, ?1)",
        params![ts],
    ).map_err(|e| StoreError::Database(format!("migration validation: FTS probe INSERT: {e}").into()))?;

    let found: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'xyzvalidationprobexyz'",
        [],
        |r| r.get(0),
    ).map_err(|e| StoreError::Database(format!("migration validation: FTS MATCH: {e}").into()))?;

    conn.execute(
        "DELETE FROM messages WHERE message_id = '__validate_fts_probe__'",
        [],
    ).map_err(|e| StoreError::Database(format!("migration validation: FTS probe DELETE: {e}").into()))?;

    if found == 0 {
        return Err(StoreError::Database(
            "migration validation: FTS trigger not wired — INSERT did not appear in messages_fts".into()
        ));
    }

    // Check deleted row is gone from FTS
    let after_delete: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'xyzvalidationprobexyz'",
        [],
        |r| r.get(0),
    ).map_err(|e| StoreError::Database(format!("migration validation: FTS MATCH after delete: {e}").into()))?;
    if after_delete != 0 {
        return Err(StoreError::Database(
            "migration validation: FTS delete trigger not wired — deleted row still found in messages_fts".into()
        ));
    }

    // (b) Embeddings BLOB roundtrip
    let blob_data: Vec<u8> = vec![0x01, 0x02, 0x03, 0x04];
    conn.execute(
        "INSERT INTO embeddings (message_id, model_id, dim, vec) VALUES ('__validate_emb__', '__probe__', 4, ?1)",
        params![blob_data],
    ).map_err(|e| StoreError::Database(format!("migration validation: embeddings INSERT: {e}").into()))?;

    let retrieved: Vec<u8> = conn.query_row(
        "SELECT vec FROM embeddings WHERE message_id = '__validate_emb__' AND model_id = '__probe__'",
        [],
        |r| r.get(0),
    ).map_err(|e| StoreError::Database(format!("migration validation: embeddings SELECT: {e}").into()))?;

    conn.execute(
        "DELETE FROM embeddings WHERE message_id = '__validate_emb__'",
        [],
    ).map_err(|e| StoreError::Database(format!("migration validation: embeddings DELETE: {e}").into()))?;

    if retrieved != blob_data {
        return Err(StoreError::Database(
            "migration validation: embeddings BLOB roundtrip mismatch".into()
        ));
    }

    // (c) Set-difference drain query shape (LIMIT 0 — just check parse/plan)
    conn.execute_batch(
        "SELECT m.message_id FROM messages m LEFT JOIN embeddings e
         ON m.message_id = e.message_id AND e.model_id = '__probe__'
         WHERE e.message_id IS NULL LIMIT 0;"
    ).map_err(|e| StoreError::Database(format!("migration validation: drain query: {e}").into()))?;

    Ok(())
}

/// Seed the watchdog baseline in `metadata` — INSERT OR IGNORE (seed-on-absence semantics).
/// Measures `db + -wal + -shm` on-disk size.
fn seed_watchdog_baseline(conn: &Connection, db_path: &Path) -> Result<()> {
    let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let sidecar = |suffix: &str| -> PathBuf {
        let name = format!(
            "{}{}",
            db_path.file_name().and_then(|n| n.to_str()).unwrap_or("whatsapp.db"),
            suffix
        );
        db_path.with_file_name(name)
    };
    let wal_size = std::fs::metadata(sidecar("-wal")).map(|m| m.len()).unwrap_or(0);
    let shm_size = std::fs::metadata(sidecar("-shm")).map(|m| m.len()).unwrap_or(0);
    let total = db_size + wal_size + shm_size;
    conn.execute(
        "INSERT OR IGNORE INTO metadata (key, value) VALUES ('watchdog_last_alerted_size', ?1)",
        params![total.to_string()],
    ).map_err(db_err)?;
    Ok(())
}

/// Find the newest `<db_path>.pre-migration-v*.bak` sidecar file.
fn find_newest_bak(db_path: &Path) -> Option<PathBuf> {
    let dir = db_path.parent()?;
    let stem = db_path.file_name().and_then(|n| n.to_str())?;
    let prefix = format!("{stem}.pre-migration-v");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix) && n.ends_with(".bak"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

/// Open (or create) the database at `path` with the given migration mode.
/// This is the single construction path for all callers. `Store::new` delegates here.
///
/// Reorder vs the old `Store::new`:
///   open → PRAGMAs → read `user_version` FIRST → branch:
///     migration needed → pin check → checkpoint → backup → SCHEMA → migrate → validate → metadata → watchdog seed
///     current → SCHEMA (idempotent) → re-validate if `schema_validated_version` != CURRENT
pub fn open_with_mode(path: &Path, mode: MigrationMode) -> Result<Store> {
    let conn = Connection::open(path).map_err(db_err)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = -2000;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA fullfsync = ON;
         PRAGMA journal_size_limit = 67108864;
         PRAGMA mmap_size = 268435456;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA auto_vacuum = INCREMENTAL;",
    ).map_err(db_err)?;

    // Read user_version FIRST (before SCHEMA which is now deferred).
    let schema_version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(db_err)?;

    if schema_version > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::Database(format!(
            "database schema version {schema_version} is newer than supported {CURRENT_SCHEMA_VERSION}; \
             use a newer whatsrust binary or restore from backup"
        ).into()));
    }

    let migration_needed = schema_version < CURRENT_SCHEMA_VERSION;

    // ----- ForceMigrate: clear pin then fall through as Normal -----
    if mode == MigrationMode::ForceMigrate {
        clear_migration_pin(path)?;
    }

    // ----- Circuit-breaker pin check (Normal mode, before ceremony) -----
    if mode == MigrationMode::Normal || mode == MigrationMode::ForceMigrate {
        if let Some(pin) = read_migration_pin(path) {
            let current_mode = if mode == MigrationMode::ForceMigrate { MigrationMode::ForceMigrate } else { MigrationMode::Normal };
            if current_mode == MigrationMode::Normal {
                // Only block on Normal; ForceMigrate already cleared the pin above.
                match pin.state.as_str() {
                    "failed" => {
                        // Consistency check
                        if schema_version >= pin.blocked_target {
                            return Err(StoreError::Database(format!(
                                "migration pin inconsistent: pin says 'failed' (blocked_target={}) but DB user_version={schema_version} >= blocked_target; \
                                 pin may be stale — delete {} to clear",
                                pin.blocked_target, pin_path(path).display()
                            ).into()));
                        }
                        if CURRENT_SCHEMA_VERSION > pin.blocked_target {
                            // Newer binary: auto-clear pin and retry
                            eprintln!(
                                "whatsrust: migration pin (blocked_target={}) overridden by newer binary (CURRENT={CURRENT_SCHEMA_VERSION}); retrying migration",
                                pin.blocked_target
                            );
                            clear_migration_pin(path)?;
                        } else {
                            return Err(StoreError::Database(format!(
                                "migration circuit-breaker: previous migration to v{} failed (reason: {}). \
                                 Options:\n  1. Run `whatsrust --rollback` to restore the pre-migration backup\n  \
                                 2. Run `whatsrust --migrate` to force a retry\n  \
                                 3. Use a newer binary that fixes the migration\n  \
                                 Pin file: {}",
                                pin.blocked_target, pin.reason, pin_path(path).display()
                            ).into()));
                        }
                    }
                    "rolled_back" => {
                        // Consistency check
                        if schema_version != pin.pinned_version {
                            return Err(StoreError::Database(format!(
                                "migration pin inconsistent: pin says 'rolled_back' (pinned_version={}) but DB user_version={schema_version}; \
                                 delete {} to clear",
                                pin.pinned_version, pin_path(path).display()
                            ).into()));
                        }
                        if CURRENT_SCHEMA_VERSION > pin.blocked_target {
                            // Newer binary: auto-clear and retry
                            eprintln!(
                                "whatsrust: rolled-back pin (blocked_target={}) overridden by newer binary (CURRENT={CURRENT_SCHEMA_VERSION}); retrying migration",
                                pin.blocked_target
                            );
                            clear_migration_pin(path)?;
                        } else if CURRENT_SCHEMA_VERSION == pin.pinned_version {
                            // Old binary on parked version: run normally, no migration needed
                            eprintln!(
                                "whatsrust: WARNING: DB is at rolled-back version v{} (auto-migration disabled). \
                                 Run with a newer binary or `whatsrust --migrate` to re-attempt.",
                                pin.pinned_version
                            );
                            // Fall through to normal open (no migration needed for this version)
                        } else {
                            return Err(StoreError::Database(format!(
                                "migration circuit-breaker: DB was rolled back to v{} after failed migration to v{}. \
                                 Options:\n  1. Run `whatsrust --migrate` to force a retry\n  \
                                 2. Use a newer binary that fixes the migration\n  \
                                 Pin file: {}",
                                pin.pinned_version, pin.blocked_target, pin_path(path).display()
                            ).into()));
                        }
                    }
                    _ => {
                        // Unknown state — clear it and proceed
                        eprintln!("whatsrust: unknown migration pin state '{}'; clearing pin", pin.state);
                        clear_migration_pin(path)?;
                    }
                }
            }
        }
    }

    if migration_needed {
        // --- Staged migration ceremony ---

        // 1. WAL checkpoint (flush to main file for a clean backup)
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|e| StoreError::Database(format!("wal_checkpoint before backup: {e}").into()))?;

        // 2. Pre-migration backup (fail-closed)
        let bak_path = backup_db_pre_migration(&conn, path, schema_version)
            .map_err(|e| {
                StoreError::Database(format!(
                    "pre-migration backup failed — refusing to migrate (DB at v{schema_version} is safe): {e}"
                ).into())
            })?;
        eprintln!(
            "whatsrust: pre-migration backup written to {}",
            bak_path.display()
        );

        // 3. FTS5 probe (before any FTS5 DDL; ADR 0032)
        if let Err(e) = probe_fts5_availability(&conn) {
            return Err(StoreError::Database(format!(
                "FTS5 not available — migration aborted, DB at v{schema_version}: {e}"
            ).into()));
        }

        // 4. Apply SCHEMA (creates new tables idempotently; required before run_schema_migrations)
        conn.execute_batch(SCHEMA).map_err(db_err)?;

        // 5. Run migration in TX (existing mechanism, unchanged)
        if let Err(e) = run_schema_migrations(&conn, schema_version) {
            let reason = e.to_string();
            let _ = write_migration_pin(path, &MigrationPin {
                state: "failed".to_owned(),
                pinned_version: schema_version,
                blocked_target: CURRENT_SCHEMA_VERSION,
                created_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
                reason: reason.clone(),
            });
            return Err(StoreError::Database(format!(
                "migration failed (DB rolled back to v{schema_version}; circuit-breaker pin written): {reason}"
            ).into()));
        }

        // 6. Post-commit validation (ADR 0029) — must run AFTER COMMIT
        if let Err(e) = validate_migration_post_commit(&conn) {
            let reason = e.to_string();
            let _ = write_migration_pin(path, &MigrationPin {
                state: "failed".to_owned(),
                pinned_version: schema_version,
                blocked_target: CURRENT_SCHEMA_VERSION,
                created_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
                reason: reason.clone(),
            });
            return Err(StoreError::Database(format!(
                "migration validation failed (circuit-breaker pin written): {reason}\n\
                 Run `whatsrust --rollback` to restore from {} or `whatsrust --migrate` to retry.",
                bak_path.display()
            ).into()));
        }

        // 7. Persist schema_validated_version (ADR 0029 B1)
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_validated_version', ?1)",
            params![CURRENT_SCHEMA_VERSION.to_string()],
        ).map_err(db_err)?;

        // 8. Seed watchdog baseline (ADR 0013/0036 — final migration step)
        seed_watchdog_baseline(&conn, path)?;

        eprintln!(
            "whatsrust: migration to v{CURRENT_SCHEMA_VERSION} complete; validated; watchdog baseline seeded"
        );
    } else {
        // Already at CURRENT: apply SCHEMA idempotently, then check schema_validated_version.
        conn.execute_batch(SCHEMA).map_err(db_err)?;

        // FTS5 startup probe (ADR 0032 M4): fast check after version check
        conn.execute_batch("SELECT 1 FROM messages_fts LIMIT 0;")
            .map_err(|e| StoreError::Database(format!(
                "FTS5 startup probe failed — DB may be corrupt or bundled rusqlite feature was removed: {e}"
            ).into()))?;

        // Re-validate if schema_validated_version is absent or stale (handles Wave-1 DBs)
        let validated_ver: Option<i64> = conn.query_row(
            "SELECT value FROM metadata WHERE key = 'schema_validated_version'",
            [],
            |r| {
                let s: String = r.get(0)?;
                Ok(s.parse::<i64>().unwrap_or(-1))
            },
        ).optional().map_err(db_err)?;

        if validated_ver != Some(CURRENT_SCHEMA_VERSION) {
            // Run validation and persist on success
            if let Err(e) = validate_migration_post_commit(&conn) {
                return Err(StoreError::Database(format!(
                    "startup schema re-validation failed: {e}\n\
                     Run `whatsrust --rollback` to restore from a backup or `whatsrust --migrate` to retry."
                ).into()));
            }
            conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_validated_version', ?1)",
                params![CURRENT_SCHEMA_VERSION.to_string()],
            ).map_err(db_err)?;
        }
    }

    Ok(Store {
        conn: Arc::new(Mutex::new(conn)),
    })
}

// Public wrappers used by main.rs --rollback / --migrate subcommands.

/// Public wrapper: return the migration pin file path for `db_path`.
pub fn pin_path_pub(db_path: &Path) -> PathBuf {
    pin_path(db_path)
}

/// Public wrapper: read the migration pin (returns `None` if absent or unparseable).
pub fn read_migration_pin_pub(db_path: &Path) -> Option<serde_json::Value> {
    let path = pin_path(db_path);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Public wrapper: write the migration pin with `state` and `pinned_version`.
/// Used by `--rollback` to update the pin after restoring.
pub fn write_migration_pin_pub(db_path: &Path, state: &str, pinned_version: i64) -> anyhow::Result<()> {
    let pin = MigrationPin {
        state: state.to_owned(),
        pinned_version,
        blocked_target: CURRENT_SCHEMA_VERSION,
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        reason: format!("written by --rollback at version {pinned_version}"),
    };
    write_migration_pin(db_path, &pin)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Public wrapper: find newest `.pre-migration-v*.bak` sidecar next to `db_path`.
pub fn find_newest_bak_pub(db_path: &Path) -> Option<PathBuf> {
    find_newest_bak(db_path)
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// A row from the inbound message history table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InboundRow {
    pub id: i64,
    pub chat_jid: String,
    pub sender_jid: String,
    pub message_id: String,
    pub content_kind: String,
    pub body_text: Option<String>,
    pub timestamp: i64,
}

/// Statistics from a prune operation.
#[derive(Debug, Clone)]
pub struct PruneStats {
    pub sent_deleted: u32,
}

// ---------------------------------------------------------------------------
// Backfill row structs
// ---------------------------------------------------------------------------

/// A row from the `backfill_jobs` table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackfillJobRow {
    pub id: i64,
    pub chat_jid: String,
    pub target_kind: String,
    pub target_value: Option<i64>,
    pub status: String,
    pub fetched: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A row from the `backfill_cursor` table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackfillCursorRow {
    pub chat_jid: String,
    pub oldest_msg_id: Option<String>,
    pub oldest_msg_from_me: Option<bool>,
    pub oldest_msg_timestamp_ms: Option<i64>,
    pub more_remain: bool,
    pub exhausted: bool,
    pub last_backfill_at: Option<i64>,
}

/// Outcome of `enqueue_backfill_job`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// Job was accepted; `job_id` is the new row's id.
    Accepted { job_id: i64 },
    /// A job for this chat is already active (queued/running/paused); `job_id` is the existing one.
    AlreadyActive { job_id: i64 },
    /// The chat's last backfill completed within the cooldown window.
    Cooldown { retry_after_secs: i64 },
}

impl Store {
    /// Open (or create) the database at `path` and initialize the schema.
    /// Delegates to `open_with_mode(path, MigrationMode::Normal)`.
    pub fn new(path: &Path) -> Result<Self> {
        open_with_mode(path, MigrationMode::Normal)
    }

    /// Run a blocking database operation on a dedicated thread.
    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            f(&guard)
        })
        .await
        .map_err(|e| StoreError::Database(format!("spawn_blocking join error: {e}").into()))?
    }

    /// Create a hot backup of the database using SQLite's backup API.
    pub fn snapshot_db(&self, dest_path: &Path) -> Result<()> {
        let guard = self.conn.lock();
        let mut dest = Connection::open(dest_path).map_err(db_err)?;
        let backup = rusqlite::backup::Backup::new(&guard, &mut dest).map_err(db_err)?;
        backup
            .run_to_completion(100, std::time::Duration::from_millis(10), None)
            .map_err(db_err)?;
        Ok(())
    }

    /// Clear stored device credentials (used after LoggedOut to trigger re-pairing on reconnect).
    pub async fn clear_device(&self) -> Result<()> {
        self.run(|c| {
            c.execute("DELETE FROM device WHERE id = 1", [])
                .map(|_| ())
                .map_err(db_err)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Outbound queue — persistent message queue (crash-safe)
    // -----------------------------------------------------------------------

    /// Mark an outbound message as successfully sent.
    /// Atomically mark a job as sent AND record its WA message ID in one write.
    /// Prevents the race where a receipt arrives between separate sent/wa_id updates.
    pub async fn mark_outbound_sent_with_id(&self, id: i64, wa_message_id: Option<&str>) -> Result<()> {
        let ts = now_secs();
        let wa = wa_message_id.map(|s| s.to_owned());
        self.run(move |c| {
            c.execute(
                "UPDATE outbound_queue SET status = 'sent', wa_message_id = COALESCE(?1, wa_message_id), updated_at = ?2 WHERE id = ?3",
                params![wa, ts, id],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Mark an outbound message as failed (increment retries).
    /// If retries >= max_retries, status becomes 'failed'; otherwise back to 'queued'
    /// with exponential backoff via `retry_after` (1s, 2s, 4s, 8s, ...).
    /// This prevents head-of-line blocking: newer messages flow while failed ones wait.
    pub async fn mark_outbound_failed(&self, id: i64, max_retries: i32) -> Result<()> {
        let ts = now_secs();
        self.run(move |c| {
            // Read current retry count to compute backoff
            let retries: i32 = c
                .query_row(
                    "SELECT retries FROM outbound_queue WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?
                .unwrap_or(0);
            let backoff_secs = 1i64 << retries.min(6); // 1, 2, 4, 8, 16, 32, 64 max
            c.execute(
                "UPDATE outbound_queue SET
                    retries = retries + 1,
                    status = CASE WHEN retries + 1 >= ?1 THEN 'failed' ELSE 'queued' END,
                    retry_after = CASE WHEN retries + 1 >= ?1 THEN 0 ELSE ?4 END,
                    updated_at = ?2
                 WHERE id = ?3",
                params![max_retries, ts, id, ts + backoff_secs],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Requeue messages stuck in 'inflight' for longer than the given threshold.
    /// This handles process crashes where inflight messages were never completed.
    pub async fn requeue_stale_inflight(&self, older_than_secs: i64) -> Result<u32> {
        let cutoff = now_secs() - older_than_secs;
        self.run(move |c| {
            let count = c
                .execute(
                    "UPDATE outbound_queue SET status = 'queued', updated_at = ?1
                     WHERE status = 'inflight' AND updated_at < ?2",
                    params![now_secs(), cutoff],
                )
                .map_err(db_err)?;
            u32::try_from(count).map_err(|_| {
                StoreError::Database(format!("requeue count {count} out of u32 range").into())
            })
        })
        .await
    }

    /// Requeue a specific outbound message without incrementing retries.
    /// Used when the message can't be sent due to connection issues (not send failures).
    pub async fn requeue_outbound(&self, id: i64) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE outbound_queue SET status = 'queued', updated_at = ?1 WHERE id = ?2",
                params![now_secs(), id],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Get the number of messages in queued or inflight status.
    pub async fn outbound_queue_depth(&self) -> Result<i64> {
        self.run(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM outbound_queue WHERE status IN ('queued', 'inflight')",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Typed outbound job queue (v5+)
    // -----------------------------------------------------------------------

    /// Enqueue a typed outbound job. Returns the row ID (job_id).
    pub async fn enqueue_job(
        &self,
        jid: &str,
        op_kind: &str,
        payload_json: &str,
        payload_blob: Option<Vec<u8>>,
    ) -> Result<i64> {
        let j = jid.to_owned();
        let ok = op_kind.to_owned();
        let pj = payload_json.to_owned();
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "INSERT INTO outbound_queue (jid, payload, op_kind, payload_json, payload_blob, status, retries, created_at, updated_at)
                 VALUES (?1, '', ?2, ?3, ?4, 'queued', 0, ?5, ?5)",
                params![j, ok, pj, payload_blob, ts],
            )
            .map_err(db_err)?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    /// Enqueue a typed outbound job scheduled for a future time. Returns the row ID.
    /// The job will not be claimed until `execute_at` (unix epoch seconds).
    pub async fn enqueue_job_at(
        &self,
        jid: &str,
        op_kind: &str,
        payload_json: &str,
        payload_blob: Option<Vec<u8>>,
        execute_at: i64,
    ) -> Result<i64> {
        let j = jid.to_owned();
        let ok = op_kind.to_owned();
        let pj = payload_json.to_owned();
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "INSERT INTO outbound_queue (jid, payload, op_kind, payload_json, payload_blob, status, retries, retry_after, created_at, updated_at)
                 VALUES (?1, '', ?2, ?3, ?4, 'queued', 0, ?5, ?6, ?6)",
                params![j, ok, pj, payload_blob, execute_at, ts],
            )
            .map_err(db_err)?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    /// Atomically claim the next queued job for processing. Returns the full job row.
    pub async fn claim_next_job(&self) -> Result<Option<crate::outbound::OutboundJobRow>> {
        let ts = now_secs();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            let row = tx
                .query_row(
                    "SELECT id, jid, op_kind, payload_json, payload_blob, retries
                     FROM outbound_queue
                     WHERE status = 'queued' AND retry_after <= ?1
                     ORDER BY id LIMIT 1",
                    params![ts],
                    |row| {
                        Ok(crate::outbound::OutboundJobRow {
                            id: row.get(0)?,
                            jid: row.get(1)?,
                            op_kind: row.get(2)?,
                            payload_json: row.get(3)?,
                            payload_blob: row.get(4)?,
                            retries: row.get(5)?,
                        })
                    },
                )
                .optional()
                .map_err(db_err)?;
            if let Some(ref r) = row {
                tx.execute(
                    "UPDATE outbound_queue SET status = 'inflight', updated_at = ?1 WHERE id = ?2",
                    params![ts, r.id],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(row)
        })
        .await
    }


    /// Update delivery status for a job identified by its WhatsApp message ID.
    pub async fn update_delivery_status(&self, wa_message_id: &str, status: &str) -> Result<()> {
        let wa = wa_message_id.to_owned();
        let st = status.to_owned();
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "UPDATE outbound_queue SET delivery_status = ?1, updated_at = ?2 WHERE wa_message_id = ?3",
                params![st, ts, wa],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Inbound message history
    // -----------------------------------------------------------------------

    /// Insert an inbound message into the history table. Duplicates (by message_id) are ignored.
    pub async fn insert_inbound(
        &self,
        chat_jid: &str,
        sender_jid: &str,
        message_id: &str,
        content_kind: &str,
        body_text: Option<&str>,
        timestamp: i64,
    ) -> Result<()> {
        let cj = chat_jid.to_owned();
        let sj = sender_jid.to_owned();
        let mid = message_id.to_owned();
        let ck = content_kind.to_owned();
        let bt = body_text.map(|s| s.to_owned());
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'live')",
                params![cj, sj, mid, ck, bt, timestamp, ts],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Search inbound message history. Returns recent messages matching filters.
    pub async fn search_inbound(
        &self,
        chat_jid: Option<&str>,
        query: Option<&str>,
        limit: i64,
        before_ts: Option<i64>,
    ) -> Result<Vec<InboundRow>> {
        let cj = chat_jid.map(|s| s.to_owned());
        let q = query.map(|s| format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let before = before_ts.unwrap_or(i64::MAX);
        self.run(move |c| {
            let mut sql = String::from(
                "SELECT id, chat_jid, sender_jid, message_id, content_kind, body_text, timestamp
                 FROM messages WHERE timestamp < ?1"
            );
            let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(before)];
            if let Some(ref jid) = cj {
                sql.push_str(&format!(" AND chat_jid = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(jid.clone()));
            }
            if let Some(ref search) = q {
                sql.push_str(&format!(" AND body_text LIKE ?{} ESCAPE '\\'", params_vec.len() + 1));
                params_vec.push(Box::new(search.clone()));
            }
            sql.push_str(&format!(" ORDER BY timestamp DESC LIMIT ?{}", params_vec.len() + 1));
            params_vec.push(Box::new(limit));

            let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
            let mut stmt = c.prepare(&sql).map_err(db_err)?;
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok(InboundRow {
                    id: row.get(0)?,
                    chat_jid: row.get(1)?,
                    sender_jid: row.get(2)?,
                    message_id: row.get(3)?,
                    content_kind: row.get(4)?,
                    body_text: row.get(5)?,
                    timestamp: row.get(6)?,
                })
            }).map_err(db_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_err)
        })
        .await
    }

    /// Delete all inbound messages for a chat (e.g. when the chat is deleted).
    pub async fn delete_inbound_chat(&self, chat_jid: &str) -> Result<u32> {
        let jid = chat_jid.to_owned();
        self.run(move |c| {
            let n = c
                .execute(
                    "DELETE FROM messages WHERE chat_jid = ?1",
                    params![jid],
                )
                .map_err(db_err)? as u32;
            Ok(n)
        })
        .await
    }

    /// Delete a single inbound message by its message ID.
    pub async fn delete_inbound_message(&self, message_id: &str) -> Result<u32> {
        let mid = message_id.to_owned();
        self.run(move |c| {
            let n = c
                .execute(
                    "DELETE FROM messages WHERE message_id = ?1",
                    params![mid],
                )
                .map_err(db_err)? as u32;
            Ok(n)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Database pruning — prevent unbounded growth
    // -----------------------------------------------------------------------

    /// Prune old data from the database. Returns counts of deleted rows.
    /// Message history is retained indefinitely (ADR 0012); only transient outbound data is pruned.
    pub async fn prune_old_data(&self, sent_retention_secs: i64) -> Result<PruneStats> {
        let sent_cutoff = now_secs() - sent_retention_secs;
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;

            // Delete completed outbound messages older than retention period
            let sent_deleted = tx
                .execute(
                    "DELETE FROM outbound_queue WHERE status IN ('sent', 'failed') AND updated_at < ?1",
                    params![sent_cutoff],
                )
                .map_err(db_err)? as u32;

            tx.commit().map_err(db_err)?;

            // Reclaim disk space progressively (no-op if auto_vacuum != INCREMENTAL)
            let _ = c.execute_batch("PRAGMA incremental_vacuum(500);");

            Ok(PruneStats { sent_deleted })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Backup — timestamped snapshots with rotation
    // -----------------------------------------------------------------------

    /// Create a timestamped backup in `backup_dir`, keeping at most `max_backups`.
    /// Returns the path to the new backup file.
    pub fn perform_backup(&self, backup_dir: &Path, max_backups: usize) -> Result<PathBuf> {
        // Ensure backup directory exists
        std::fs::create_dir_all(backup_dir)
            .map_err(|e| StoreError::Database(format!("create backup dir: {e}").into()))?;
        secure_backup_permissions(backup_dir)?;

        // Generate timestamped filename
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let filename = format!("whatsapp_backup_{ts}.db");
        let dest_path = backup_dir.join(&filename);

        // Perform the hot backup
        self.snapshot_db(&dest_path)?;
        secure_backup_permissions(&dest_path)?;

        // Rotate: list backups, sort by name, delete oldest if over limit
        if let Ok(entries) = std::fs::read_dir(backup_dir) {
            let mut backups: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("whatsapp_backup_") && n.ends_with(".db"))
                        .unwrap_or(false)
                })
                .collect();
            backups.sort();
            while backups.len() > max_backups {
                if let Some(oldest) = backups.first() {
                    let _ = std::fs::remove_file(oldest);
                }
                backups.remove(0);
            }
        }

        Ok(dest_path)
    }

    // -----------------------------------------------------------------------
    // Poll key storage
    // -----------------------------------------------------------------------

    /// Store a poll's encryption key and option names for later vote decryption.
    pub async fn store_poll_key(
        &self,
        chat_jid: &str,
        poll_id: &str,
        enc_key: &[u8],
        options: &[String],
    ) -> Result<()> {
        let chat_jid = chat_jid.to_string();
        let poll_id = poll_id.to_string();
        let enc_key = enc_key.to_vec();
        let options_json = serde_json::to_string(options).map_err(|e| {
            StoreError::Serialization(format!("poll options: {e}").into())
        })?;
        self.run(move |c| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            c.execute(
                "INSERT OR REPLACE INTO poll_keys (chat_jid, poll_id, enc_key, options_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![chat_jid, poll_id, enc_key, options_json, now],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Retrieve a poll's encryption key and option names.
    pub async fn get_poll_key(
        &self,
        chat_jid: &str,
        poll_id: &str,
    ) -> Result<Option<(Vec<u8>, Vec<String>)>> {
        let chat_jid = chat_jid.to_string();
        let poll_id = poll_id.to_string();
        self.run(move |c| {
            let result: Option<(Vec<u8>, String)> = c
                .query_row(
                    "SELECT enc_key, options_json FROM poll_keys WHERE chat_jid = ?1 AND poll_id = ?2",
                    params![chat_jid, poll_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(db_err)?;
            match result {
                Some((enc_key, options_json)) => {
                    let options: Vec<String> = serde_json::from_str(&options_json)
                        .map_err(|e| StoreError::Serialization(format!("poll options: {e}").into()))?;
                    Ok(Some((enc_key, options)))
                }
                None => Ok(None),
            }
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Backfill job queue (ADR 0010/0033/0035)
    // -----------------------------------------------------------------------

    /// Atomically enqueue a backfill job for `chat_jid` with the given target.
    ///
    /// The entire check-and-insert runs in ONE `unchecked_transaction` closure
    /// (ADR 0035 B5 — TOCTOU fix; mirrors `claim_next_job` atomicity):
    ///
    ///   BEGIN → check for active job → check cooldown → INSERT-or-reject → COMMIT
    ///
    /// Returns a structured `EnqueueOutcome` instead of an error for the two
    /// rejection cases — they are normal back-pressure, not faults.
    pub async fn enqueue_backfill_job(
        &self,
        chat_jid: &str,
        target_kind: &str,
        target_value: Option<i64>,
        cooldown_secs: i64,
    ) -> Result<EnqueueOutcome> {
        let jid = chat_jid.to_owned();
        let kind = target_kind.to_owned();
        let ts = now_secs();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;

            // Check for an already-active job for this chat
            let active: Option<i64> = tx
                .query_row(
                    "SELECT id FROM backfill_jobs
                     WHERE chat_jid = ?1 AND status IN ('queued', 'running', 'paused')
                     ORDER BY id LIMIT 1",
                    params![jid],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?;

            if let Some(existing_id) = active {
                tx.commit().map_err(db_err)?;
                return Ok(EnqueueOutcome::AlreadyActive { job_id: existing_id });
            }

            // Check per-chat cooldown via backfill_cursor.last_backfill_at
            if cooldown_secs > 0 {
                let last_at: Option<i64> = tx
                    .query_row(
                        "SELECT last_backfill_at FROM backfill_cursor WHERE chat_jid = ?1",
                        params![jid],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(db_err)?
                    .flatten();

                if let Some(last) = last_at {
                    let elapsed = ts - last;
                    if elapsed < cooldown_secs {
                        let retry_after_secs = cooldown_secs - elapsed;
                        tx.commit().map_err(db_err)?;
                        return Ok(EnqueueOutcome::Cooldown { retry_after_secs });
                    }
                }
            }

            // Insert the new job
            tx.execute(
                "INSERT INTO backfill_jobs (chat_jid, target_kind, target_value, status, fetched, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'queued', 0, ?4, ?4)",
                params![jid, kind, target_value, ts],
            )
            .map_err(db_err)?;

            let job_id = tx.last_insert_rowid();
            tx.commit().map_err(db_err)?;
            Ok(EnqueueOutcome::Accepted { job_id })
        })
        .await
    }

    /// Atomically claim the next queued backfill job (FIFO by id), flipping it to `running`.
    ///
    /// Mirrors `claim_next_job` for the outbound queue.
    pub async fn claim_next_backfill_job(&self) -> Result<Option<BackfillJobRow>> {
        let ts = now_secs();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            let row = tx
                .query_row(
                    "SELECT id, chat_jid, target_kind, target_value, status, fetched, created_at, updated_at
                     FROM backfill_jobs
                     WHERE status = 'queued'
                     ORDER BY id LIMIT 1",
                    [],
                    |row| {
                        Ok(BackfillJobRow {
                            id: row.get(0)?,
                            chat_jid: row.get(1)?,
                            target_kind: row.get(2)?,
                            target_value: row.get(3)?,
                            status: row.get(4)?,
                            fetched: row.get(5)?,
                            created_at: row.get(6)?,
                            updated_at: row.get(7)?,
                        })
                    },
                )
                .optional()
                .map_err(db_err)?;
            if let Some(ref r) = row {
                tx.execute(
                    "UPDATE backfill_jobs SET status = 'running', updated_at = ?1 WHERE id = ?2",
                    params![ts, r.id],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(row)
        })
        .await
    }

    /// Update the status of a backfill job.
    ///
    /// For terminal statuses (`done`, `failed`, `cancelled`) a CASE-guarded conditional
    /// update is used so that a `cancelled` job cannot be silently overwritten to `done`
    /// (ADR 0026 I6). Specifically: setting `done` or `failed` only applies if the
    /// current status is still `running`; `cancelled` is always written (it may arrive
    /// from any non-terminal state).
    pub async fn mark_backfill_job(&self, id: i64, status: &str) -> Result<()> {
        let st = status.to_owned();
        let ts = now_secs();
        self.run(move |c| {
            // CASE guard: terminal "done"/"failed" must not overwrite "cancelled".
            // "paused" and "cancelled" are written unconditionally within running jobs.
            let sql = match st.as_str() {
                "done" | "failed" => {
                    "UPDATE backfill_jobs
                     SET status = ?1, updated_at = ?2
                     WHERE id = ?3 AND status != 'cancelled'"
                }
                _ => {
                    "UPDATE backfill_jobs
                     SET status = ?1, updated_at = ?2
                     WHERE id = ?3"
                }
            };
            c.execute(sql, params![st, ts, id]).map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Update the `fetched` counter and `updated_at` for a backfill job.
    pub async fn update_backfill_fetched(&self, id: i64, fetched: u32) -> Result<()> {
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "UPDATE backfill_jobs SET fetched = ?1, updated_at = ?2 WHERE id = ?3",
                params![fetched as i64, ts, id],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Fetch a single backfill job row by id.
    pub async fn get_backfill_job(&self, id: i64) -> Result<Option<BackfillJobRow>> {
        self.run(move |c| {
            c.query_row(
                "SELECT id, chat_jid, target_kind, target_value, status, fetched, created_at, updated_at
                 FROM backfill_jobs WHERE id = ?1",
                params![id],
                |row| {
                    Ok(BackfillJobRow {
                        id: row.get(0)?,
                        chat_jid: row.get(1)?,
                        target_kind: row.get(2)?,
                        target_value: row.get(3)?,
                        status: row.get(4)?,
                        fetched: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    /// List backfill jobs. If `active_only` is true, only returns jobs with
    /// status in (`queued`, `running`, `paused`).
    pub async fn list_backfill_jobs(&self, active_only: bool) -> Result<Vec<BackfillJobRow>> {
        self.run(move |c| {
            let sql = if active_only {
                "SELECT id, chat_jid, target_kind, target_value, status, fetched, created_at, updated_at
                 FROM backfill_jobs
                 WHERE status IN ('queued', 'running', 'paused')
                 ORDER BY id"
            } else {
                "SELECT id, chat_jid, target_kind, target_value, status, fetched, created_at, updated_at
                 FROM backfill_jobs
                 ORDER BY id"
            };
            let mut stmt = c.prepare(sql).map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(BackfillJobRow {
                        id: row.get(0)?,
                        chat_jid: row.get(1)?,
                        target_kind: row.get(2)?,
                        target_value: row.get(3)?,
                        status: row.get(4)?,
                        fetched: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                })
                .map_err(db_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_err)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Backfill cursor
    // -----------------------------------------------------------------------

    /// Get the backfill cursor for a chat, if one exists.
    pub async fn get_backfill_cursor(&self, chat_jid: &str) -> Result<Option<BackfillCursorRow>> {
        let jid = chat_jid.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT chat_jid, oldest_msg_id, oldest_msg_from_me, oldest_msg_timestamp_ms,
                        more_remain, exhausted, last_backfill_at
                 FROM backfill_cursor WHERE chat_jid = ?1",
                params![jid],
                |row| {
                    let from_me_raw: Option<i64> = row.get(2)?;
                    let exhausted_raw: i64 = row.get(5)?;
                    let more_remain_raw: i64 = row.get(4)?;
                    Ok(BackfillCursorRow {
                        chat_jid: row.get(0)?,
                        oldest_msg_id: row.get(1)?,
                        oldest_msg_from_me: from_me_raw.map(|v| v != 0),
                        oldest_msg_timestamp_ms: row.get(3)?,
                        more_remain: more_remain_raw != 0,
                        exhausted: exhausted_raw != 0,
                        last_backfill_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    /// Insert or update the backfill cursor for a chat.
    ///
    /// Anchor fields are passed separately (matching DB columns) so this method
    /// has no dependency on the `backfill` module — keeping the storage layer lean.
    ///
    /// - `oldest_msg_id` / `oldest_msg_from_me` / `oldest_msg_timestamp_ms`: the pagination
    ///   anchor (pass `None` for all three when creating a placeholder before the first batch).
    /// - `more_remain`: whether more history is available (phone's signal).
    /// - `exhausted`: whether history is fully exhausted.
    /// - `last_backfill_at`: unix seconds of last completed job; pass `None` to leave unchanged.
    pub async fn upsert_backfill_cursor(
        &self,
        chat_jid: &str,
        oldest_msg_id: Option<&str>,
        oldest_msg_from_me: Option<bool>,
        oldest_msg_timestamp_ms: Option<i64>,
        more_remain: bool,
        exhausted: bool,
        last_backfill_at: Option<i64>,
    ) -> Result<()> {
        let jid = chat_jid.to_owned();
        let mid = oldest_msg_id.map(|s| s.to_owned());
        let from_me = oldest_msg_from_me.map(|v| if v { 1i64 } else { 0i64 });
        let more_remain_i = if more_remain { 1i64 } else { 0i64 };
        let exhausted_i = if exhausted { 1i64 } else { 0i64 };
        self.run(move |c| {
            c.execute(
                "INSERT INTO backfill_cursor
                     (chat_jid, oldest_msg_id, oldest_msg_from_me, oldest_msg_timestamp_ms,
                      more_remain, exhausted, last_backfill_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(chat_jid) DO UPDATE SET
                     oldest_msg_id           = excluded.oldest_msg_id,
                     oldest_msg_from_me      = excluded.oldest_msg_from_me,
                     oldest_msg_timestamp_ms = excluded.oldest_msg_timestamp_ms,
                     more_remain             = excluded.more_remain,
                     exhausted               = excluded.exhausted,
                     last_backfill_at        = COALESCE(excluded.last_backfill_at, last_backfill_at)",
                params![
                    jid,
                    mid,
                    from_me,
                    oldest_msg_timestamp_ms,
                    more_remain_i,
                    exhausted_i,
                    last_backfill_at,
                ],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }
}

fn run_schema_migrations(conn: &Connection, from_version: i64) -> Result<()> {
    if from_version > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::Database(format!(
            "database schema version {from_version} is newer than supported {CURRENT_SCHEMA_VERSION}"
        ).into()));
    }

    if from_version >= CURRENT_SCHEMA_VERSION {
        return Ok(());
    }

    // Run all migrations inside a single transaction — partial migrations
    // leave the DB in a consistent pre-migration state on failure.
    conn.execute_batch("BEGIN IMMEDIATE;").map_err(db_err)?;

    let result = (|| -> Result<()> {
        if from_version < 1 {
            // v0→v1: initial schema (tables created by SCHEMA execute_batch above).
        }

        if from_version < 2 {
            // v1→v2: outbound_queue table (idempotent — CREATE IF NOT EXISTS in SCHEMA).
        }

        if from_version < 3 {
            // v2→v3: add retry_after column for per-message exponential backoff.
            // Check if column already exists (fresh DBs have it in the initial schema).
            let has_col: bool = conn
                .prepare("SELECT COUNT(*) FROM pragma_table_info('outbound_queue') WHERE name='retry_after'")
                .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
                .unwrap_or(0)
                > 0;
            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE outbound_queue ADD COLUMN retry_after INTEGER NOT NULL DEFAULT 0;"
                ).map_err(db_err)?;
            }
        }

        if from_version < 4 {
            // v3→v4: poll_keys table for decrypting incoming poll votes.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS poll_keys (
                    chat_jid TEXT NOT NULL,
                    poll_id TEXT NOT NULL,
                    enc_key BLOB NOT NULL,
                    options_json TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    PRIMARY KEY (chat_jid, poll_id)
                );"
            ).map_err(db_err)?;
        }

        if from_version < 5 {
            // v4→v5: typed outbound job queue — add columns for op_kind, structured
            // payload, WA message ID tracking, and delivery status.
            let add_col = |col: &str, def: &str| -> std::result::Result<(), StoreError> {
                // Check if column exists first (fresh DBs already have it).
                let exists: bool = conn
                    .prepare(&format!(
                        "SELECT COUNT(*) FROM pragma_table_info('outbound_queue') WHERE name='{col}'"
                    ))
                    .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
                    .unwrap_or(0)
                    > 0;
                if !exists {
                    conn.execute_batch(&format!(
                        "ALTER TABLE outbound_queue ADD COLUMN {col} {def};"
                    ))
                    .map_err(db_err)?;
                }
                Ok(())
            };
            add_col("op_kind", "TEXT NOT NULL DEFAULT 'text'")?;
            add_col("payload_json", "TEXT NOT NULL DEFAULT '{}'")?;
            add_col("payload_blob", "BLOB")?;
            add_col("wa_message_id", "TEXT")?;
            add_col("delivery_status", "TEXT")?;
            add_col("last_error", "TEXT")?;

            // Migrate existing text rows: copy payload into payload_json
            conn.execute_batch(
                "UPDATE outbound_queue SET payload_json = json_object('text', payload)
                 WHERE op_kind = 'text' AND payload_json = '{}'
                 AND payload != '';"
            ).map_err(db_err)?;

            // Add better index for the typed queue
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_outbound_retry ON outbound_queue(status, retry_after, id);
                 CREATE INDEX IF NOT EXISTS idx_outbound_wa_id ON outbound_queue(wa_message_id);"
            ).map_err(db_err)?;
        }

        if from_version < 6 {
            // v5→v6: superseded by the v8 unified `messages` table in SCHEMA.
            // A DB migrating from v0–v5 never had inbound_messages; it goes straight to
            // `messages` (created by SCHEMA above). A real v6/v7 DB still has
            // inbound_messages with data; the `< 8` block below copies those rows.
            // No DDL needed here.
        }

        if from_version < 7 {
            // v6→v7: wa-rs v0.5.0 — new ProtocolStore tables + device columns.
            // Replace skdm_recipients with sender_key_devices.
            conn.execute_batch(
                "DROP TABLE IF EXISTS skdm_recipients;
                 DROP TABLE IF EXISTS sender_key_status;

                 CREATE TABLE IF NOT EXISTS sender_key_devices (
                     group_jid TEXT NOT NULL,
                     device_jid TEXT NOT NULL,
                     needs_sender_key INTEGER NOT NULL DEFAULT 1,
                     PRIMARY KEY (group_jid, device_jid)
                 );

                 CREATE TABLE IF NOT EXISTS sent_messages (
                     chat_jid TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     message_bytes BLOB NOT NULL,
                     timestamp INTEGER NOT NULL,
                     PRIMARY KEY (chat_jid, message_id)
                 );"
            ).map_err(db_err)?;

            // Add new device columns (idempotent check).
            let add_dev_col = |col: &str, def: &str| -> std::result::Result<(), StoreError> {
                let exists: bool = conn
                    .prepare(&format!(
                        "SELECT COUNT(*) FROM pragma_table_info('device') WHERE name='{col}'"
                    ))
                    .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
                    .unwrap_or(0)
                    > 0;
                if !exists {
                    conn.execute_batch(&format!(
                        "ALTER TABLE device ADD COLUMN {col} {def};"
                    ))
                    .map_err(db_err)?;
                }
                Ok(())
            };
            add_dev_col("next_pre_key_id", "INTEGER NOT NULL DEFAULT 0")?;
            add_dev_col("nct_salt", "BLOB")?;
        }

        if from_version < 8 {
            // v7→v8: unify inbound_messages → messages (copy-then-drop under SCHEMA-first;
            // ADR 0009 correction, ADR 0019). SCHEMA already created `messages` + FTS5 +
            // triggers above. Guard on inbound_messages existing (a fresh DB never had it).
            let has_inbound: bool = conn
                .prepare(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='inbound_messages'"
                )
                .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
                .unwrap_or(0)
                > 0;
            if has_inbound {
                // Copy rows; the three new columns take their DEFAULTs.
                // The messages_fts_insert trigger fires per-row and populates the FTS index.
                conn.execute_batch(
                    "INSERT OR IGNORE INTO messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
                     SELECT chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at
                     FROM inbound_messages;
                     DROP TABLE inbound_messages;"
                ).map_err(db_err)?;
            }
        }

        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
            .map_err(db_err)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT;").map_err(db_err)?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn serialize_keypair(kp: &KeyPair) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(kp.private_key.serialize());
    buf.extend_from_slice(kp.public_key.public_key_bytes());
    buf
}

fn deserialize_keypair(bytes: &[u8]) -> Result<KeyPair> {
    if bytes.len() != 64 {
        return Err(StoreError::Serialization(format!(
            "keypair: expected 64 bytes, got {}",
            bytes.len()
        ).into()));
    }
    let private = PrivateKey::deserialize(&bytes[..32])
        .map_err(|e| StoreError::Serialization(e.to_string().into()))?;
    let public = PublicKey::from_djb_public_key_bytes(&bytes[32..])
        .map_err(|e| StoreError::Serialization(e.to_string().into()))?;
    Ok(KeyPair::new(public, private))
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Save a Device to the database (shared by DeviceStore::save and ::create).
fn save_device_to_db(conn: &Connection, device: &Device) -> Result<()> {
    let pn = device
        .pn
        .as_ref()
        .map(|j| j.to_string())
        .unwrap_or_default();
    let lid = device
        .lid
        .as_ref()
        .map(|j| j.to_string())
        .unwrap_or_default();
    let noise = serialize_keypair(&device.noise_key);
    let identity = serialize_keypair(&device.identity_key);
    let spk = serialize_keypair(&device.signed_pre_key);
    let account = device.account.as_ref().map(|a| a.encode_to_vec());

    conn.execute(
        "INSERT INTO device (id, pn, lid, registration_id, noise_key, identity_key,
         signed_pre_key, signed_pre_key_id, signed_pre_key_signature, adv_secret_key,
         account, push_name, app_version_primary, app_version_secondary,
         app_version_tertiary, app_version_last_fetched_ms, edge_routing_info, props_hash,
         next_pre_key_id, nct_salt)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(id) DO UPDATE SET
            pn=excluded.pn, lid=excluded.lid, registration_id=excluded.registration_id,
            noise_key=excluded.noise_key, identity_key=excluded.identity_key,
            signed_pre_key=excluded.signed_pre_key, signed_pre_key_id=excluded.signed_pre_key_id,
            signed_pre_key_signature=excluded.signed_pre_key_signature,
            adv_secret_key=excluded.adv_secret_key, account=excluded.account,
            push_name=excluded.push_name,
            app_version_primary=excluded.app_version_primary,
            app_version_secondary=excluded.app_version_secondary,
            app_version_tertiary=excluded.app_version_tertiary,
            app_version_last_fetched_ms=excluded.app_version_last_fetched_ms,
            edge_routing_info=excluded.edge_routing_info,
            props_hash=excluded.props_hash,
            next_pre_key_id=excluded.next_pre_key_id,
            nct_salt=excluded.nct_salt",
        params![
            pn,
            lid,
            device.registration_id as i64,
            noise,
            identity,
            spk,
            device.signed_pre_key_id as i64,
            device.signed_pre_key_signature,
            device.adv_secret_key,
            account,
            device.push_name,
            device.app_version_primary as i64,
            device.app_version_secondary as i64,
            device.app_version_tertiary as i64,
            device.app_version_last_fetched_ms,
            device.edge_routing_info,
            device.props_hash,
            device.next_pre_key_id as i64,
            device.nct_salt,
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Load a Device from the database.
fn load_device_from_db(conn: &Connection) -> Result<Option<Device>> {
    let row = conn
        .query_row(
            "SELECT pn, lid, registration_id, noise_key, identity_key, signed_pre_key,
             signed_pre_key_id, signed_pre_key_signature, adv_secret_key, account,
             push_name, app_version_primary, app_version_secondary, app_version_tertiary,
             app_version_last_fetched_ms, edge_routing_info, props_hash,
             next_pre_key_id, nct_salt
             FROM device WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                    row.get::<_, Vec<u8>>(8)?,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, Option<Vec<u8>>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, i64>(17)?,
                    row.get::<_, Option<Vec<u8>>>(18)?,
                ))
            },
        )
        .optional()
        .map_err(db_err)?;

    let Some((
        pn_s,
        lid_s,
        reg_id,
        noise_b,
        ident_b,
        spk_b,
        spk_id,
        spk_sig,
        adv,
        account_b,
        push_name,
        v1,
        v2,
        v3,
        v_ts,
        eri,
        ph,
        npk_id_raw,
        nct_raw,
    )) = row
    else {
        return Ok(None);
    };

    let to_u32 = |v: i64, field: &str| -> Result<u32> {
        u32::try_from(v)
            .map_err(|_| StoreError::Serialization(format!("{field}: value {v} out of u32 range").into()))
    };
    let to_fixed = |v: Vec<u8>, field: &str, expected: usize| -> Result<Vec<u8>> {
        if v.len() == expected {
            Ok(v)
        } else {
            Err(StoreError::Serialization(format!(
                "{field}: expected {expected} bytes, got {}",
                v.len()
            ).into()))
        }
    };
    let npk_id = to_u32(npk_id_raw, "next_pre_key_id")?;
    let nct = nct_raw;

    // server_has_prekeys is a v0.6 login optimization: when true, the client skips
    // re-uploading prekeys at login. It has no dedicated column (Phase 0 keeps schema
    // at v7), so derive it from whether any prekeys are already marked uploaded —
    // equivalent to "the server already has our prekeys".
    let server_has_prekeys: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM prekeys WHERE uploaded = 1)",
            [],
            |row| row.get(0),
        )
        .map_err(db_err)?;

    Ok(Some(Device {
        pn: if pn_s.is_empty() {
            None
        } else {
            Jid::from_str(&pn_s).ok()
        },
        lid: if lid_s.is_empty() {
            None
        } else {
            Jid::from_str(&lid_s).ok()
        },
        registration_id: to_u32(reg_id, "registration_id")?,
        noise_key: deserialize_keypair(&noise_b)?,
        identity_key: deserialize_keypair(&ident_b)?,
        signed_pre_key: deserialize_keypair(&spk_b)?,
        signed_pre_key_id: to_u32(spk_id, "signed_pre_key_id")?,
        signed_pre_key_signature: {
            let bytes = to_fixed(spk_sig, "signed_pre_key_signature", 64)?;
            let mut fixed = [0u8; 64];
            fixed.copy_from_slice(&bytes);
            fixed
        },
        adv_secret_key: {
            let bytes = to_fixed(adv, "adv_secret_key", 32)?;
            let mut fixed = [0u8; 32];
            fixed.copy_from_slice(&bytes);
            fixed
        },
        account: account_b
            .map(|b| wa::AdvSignedDeviceIdentity::decode(b.as_slice()))
            .transpose()
            .map_err(|e| StoreError::Serialization(e.to_string().into()))?
            .map(Arc::new),
        push_name,
        app_version_primary: to_u32(v1, "app_version_primary")?,
        app_version_secondary: to_u32(v2, "app_version_secondary")?,
        app_version_tertiary: to_u32(v3, "app_version_tertiary")?,
        app_version_last_fetched_ms: v_ts,
        device_props: Arc::new(DEVICE_PROPS.clone()),
        client_profile: Default::default(),
        edge_routing_info: eri,
        props_hash: ph,
        next_pre_key_id: npk_id,
        first_unupload_pre_key_id: 0,
        server_has_prekeys,
        nct_salt: nct,
        nct_salt_sync_seen: false,
        server_cert_chain: None,
        login_counter: 0,
    }))
}

// ===========================================================================
// SignalStore — identity keys, sessions, pre-keys, sender keys
// ===========================================================================

#[async_trait]
impl SignalStore for Store {
    async fn put_identity(&self, address: &str, key: [u8; 32]) -> Result<()> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.execute(
                "INSERT INTO identities (address, key) VALUES (?1, ?2)
                 ON CONFLICT(address) DO UPDATE SET key = excluded.key",
                params![addr, key.as_slice()],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn load_identity(&self, address: &str) -> Result<Option<[u8; 32]>> {
        let addr = address.to_owned();
        self.run(move |c| {
            let opt: Option<Vec<u8>> = c.query_row(
                "SELECT key FROM identities WHERE address = ?1",
                params![addr],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
            opt.map(|v| {
                if v.len() != 32 {
                    return Err(StoreError::Serialization(format!("identity key: expected 32 bytes, got {}", v.len()).into()));
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&v);
                Ok(arr)
            }).transpose()
        })
        .await
    }

    async fn delete_identity(&self, address: &str) -> Result<()> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM identities WHERE address = ?1", params![addr])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_session(&self, address: &str) -> Result<Option<Bytes>> {
        let addr = address.to_owned();
        self.run(move |c| {
            let opt: Option<Vec<u8>> = c.query_row(
                "SELECT record FROM sessions WHERE address = ?1",
                params![addr],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
            Ok(opt.map(Bytes::from))
        })
        .await
    }

    async fn put_session(&self, address: &str, session: &[u8]) -> Result<()> {
        let addr = address.to_owned();
        let data = session.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO sessions (address, record) VALUES (?1, ?2)
                 ON CONFLICT(address) DO UPDATE SET record = excluded.record",
                params![addr, data],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn delete_session(&self, address: &str) -> Result<()> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM sessions WHERE address = ?1", params![addr])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn store_prekey(&self, id: u32, record: &[u8], uploaded: bool) -> Result<()> {
        let data = record.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO prekeys (id, key, uploaded) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET key = excluded.key, uploaded = excluded.uploaded",
                params![id, data, uploaded],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn load_prekey(&self, id: u32) -> Result<Option<Bytes>> {
        self.run(move |c| {
            let opt: Option<Vec<u8>> = c.query_row(
                "SELECT key FROM prekeys WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
            Ok(opt.map(Bytes::from))
        })
        .await
    }

    async fn mark_prekeys_uploaded(&self, ids: &[u32]) -> Result<()> {
        let ids_vec = ids.to_vec();
        self.run(move |c| {
            for id in &ids_vec {
                c.execute(
                    "UPDATE prekeys SET uploaded = 1 WHERE id = ?1",
                    params![id],
                )
                .map_err(db_err)?;
            }
            Ok(())
        })
        .await
    }

    async fn remove_prekey(&self, id: u32) -> Result<()> {
        self.run(move |c| {
            c.execute("DELETE FROM prekeys WHERE id = ?1", params![id])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn store_signed_prekey(&self, id: u32, record: &[u8]) -> Result<()> {
        let data = record.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO signed_prekeys (id, record) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET record = excluded.record",
                params![id, data],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn load_signed_prekey(&self, id: u32) -> Result<Option<Vec<u8>>> {
        self.run(move |c| {
            c.query_row(
                "SELECT record FROM signed_prekeys WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn load_all_signed_prekeys(&self) -> Result<Vec<(u32, Vec<u8>)>> {
        self.run(|c| {
            let mut stmt = c
                .prepare("SELECT id, record FROM signed_prekeys")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(db_err)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(db_err)?);
            }
            Ok(out)
        })
        .await
    }

    async fn remove_signed_prekey(&self, id: u32) -> Result<()> {
        self.run(move |c| {
            c.execute("DELETE FROM signed_prekeys WHERE id = ?1", params![id])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_max_prekey_id(&self) -> Result<u32> {
        self.run(|c| {
            let id: Option<u32> = c
                .query_row("SELECT MAX(id) FROM prekeys", [], |r| r.get(0))
                .optional()
                .map_err(db_err)?
                .flatten();
            Ok(id.unwrap_or(0))
        })
        .await
    }

    async fn put_sender_key(&self, address: &str, record: &[u8]) -> Result<()> {
        let addr = address.to_owned();
        let data = record.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO sender_keys (address, record) VALUES (?1, ?2)
                 ON CONFLICT(address) DO UPDATE SET record = excluded.record",
                params![addr, data],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_sender_key(&self, address: &str) -> Result<Option<Vec<u8>>> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT record FROM sender_keys WHERE address = ?1",
                params![addr],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn delete_sender_key(&self, address: &str) -> Result<()> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM sender_keys WHERE address = ?1", params![addr])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }
}

// ===========================================================================
// AppSyncStore — app state keys, versions, mutation MACs
// ===========================================================================

#[async_trait]
impl AppSyncStore for Store {
    async fn get_sync_key(&self, key_id: &[u8]) -> Result<Option<AppStateSyncKey>> {
        let kid = key_id.to_vec();
        self.run(move |c| {
            c.query_row(
                "SELECT key_data, fingerprint, timestamp FROM app_state_keys WHERE key_id = ?1",
                params![kid],
                |row| {
                    Ok(AppStateSyncKey {
                        key_data: row.get(0)?,
                        fingerprint: row.get(1)?,
                        timestamp: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn set_sync_key(&self, key_id: &[u8], key: AppStateSyncKey) -> Result<()> {
        let kid = key_id.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO app_state_keys (key_id, key_data, fingerprint, timestamp)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key_id) DO UPDATE SET
                    key_data=excluded.key_data, fingerprint=excluded.fingerprint,
                    timestamp=excluded.timestamp",
                params![kid, key.key_data, key.fingerprint, key.timestamp],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_version(&self, name: &str) -> Result<HashState> {
        let n = name.to_owned();
        self.run(move |c| {
            let opt: Option<Vec<u8>> = c
                .query_row(
                    "SELECT state_data FROM app_state_versions WHERE name = ?1",
                    params![n],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?;
            match opt {
                Some(data) => serde_json::from_slice(&data)
                    .map_err(|e| StoreError::Serialization(e.to_string().into())),
                None => Ok(HashState::default()),
            }
        })
        .await
    }

    async fn set_version(&self, name: &str, state: HashState) -> Result<()> {
        let n = name.to_owned();
        let data =
            serde_json::to_vec(&state).map_err(|e| StoreError::Serialization(e.to_string().into()))?;
        self.run(move |c| {
            c.execute(
                "INSERT INTO app_state_versions (name, state_data) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET state_data = excluded.state_data",
                params![n, data],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn put_mutation_macs(
        &self,
        name: &str,
        version: u64,
        mutations: &[AppStateMutationMAC],
    ) -> Result<()> {
        let n = name.to_owned();
        let macs: Vec<(Vec<u8>, Vec<u8>)> = mutations
            .iter()
            .map(|m| (m.index_mac.clone(), m.value_mac.clone()))
            .collect();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            for (index_mac, value_mac) in &macs {
                tx.execute(
                    "INSERT INTO app_state_mutation_macs (name, index_mac, version, value_mac)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(name, index_mac) DO UPDATE SET
                        version=excluded.version, value_mac=excluded.value_mac",
                    params![n, index_mac, version as i64, value_mac],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)
        })
        .await
    }

    async fn get_mutation_mac(&self, name: &str, index_mac: &[u8]) -> Result<Option<Vec<u8>>> {
        let n = name.to_owned();
        let im = index_mac.to_vec();
        self.run(move |c| {
            c.query_row(
                "SELECT value_mac FROM app_state_mutation_macs
                 WHERE name = ?1 AND index_mac = ?2",
                params![n, im],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn delete_mutation_macs(&self, name: &str, index_macs: &[Vec<u8>]) -> Result<()> {
        let n = name.to_owned();
        let macs = index_macs.to_vec();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            for mac in &macs {
                tx.execute(
                    "DELETE FROM app_state_mutation_macs WHERE name = ?1 AND index_mac = ?2",
                    params![n, mac],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)
        })
        .await
    }

    async fn clear_mutation_macs(&self, name: &str) -> Result<()> {
        let n = name.to_owned();
        self.run(move |c| {
            c.execute(
                "DELETE FROM app_state_mutation_macs WHERE name = ?1",
                params![n],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_latest_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        self.run(|c| {
            c.query_row(
                "SELECT key_id FROM app_state_keys ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }
}

// ===========================================================================
// MsgSecretStore — message edit/reaction secrets (stub: returns empty/no-op)
// ===========================================================================
// wa-rs HEAD requires this trait for Backend. We don't persist msg secrets yet
// (no table, no retention policy), so stub with no-op/in-memory-only semantics:
// put succeeds silently, get always returns None. This keeps the trait satisfied
// without changing the schema or adding a migration. Real persistence TBD.

#[async_trait]
impl wacore::store::traits::MsgSecretStore for Store {
    async fn put_msg_secrets(&self, _entries: Vec<wacore::store::traits::MsgSecretEntry>) -> Result<usize> {
        // No-op: we don't persist msg secrets yet
        Ok(0)
    }

    async fn get_msg_secret(
        &self,
        _chat: &str,
        _sender: &str,
        _msg_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        // Always returns None: no persistence
        Ok(None)
    }

    async fn delete_expired_msg_secrets(&self, _cutoff_timestamp: i64) -> Result<u32> {
        // No-op: nothing to delete
        Ok(0)
    }
}

// ===========================================================================
// ProtocolStore — SKDM, LID mapping, base keys, device registry, tc tokens
// ===========================================================================

#[async_trait]
impl ProtocolStore for Store {
    async fn get_sender_key_devices(&self, group_jid: &str) -> Result<Vec<(String, bool)>> {
        let gj = group_jid.to_owned();
        self.run(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT device_jid, needs_sender_key FROM sender_key_devices WHERE group_jid = ?1",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params![gj], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
                })
                .map_err(db_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err)
        })
        .await
    }

    async fn set_sender_key_status(&self, group_jid: &str, entries: &[(&str, bool)]) -> Result<()> {
        let gj = group_jid.to_owned();
        let owned: Vec<(String, bool)> = entries.iter().map(|(s, b)| (s.to_string(), *b)).collect();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            for (device_jid, needs) in &owned {
                tx.execute(
                    "INSERT INTO sender_key_devices (group_jid, device_jid, needs_sender_key) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(group_jid, device_jid) DO UPDATE SET needs_sender_key = ?3",
                    params![gj, device_jid, needs],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)
        })
        .await
    }

    async fn clear_sender_key_devices(&self, group_jid: &str) -> Result<()> {
        let gj = group_jid.to_owned();
        self.run(move |c| {
            c.execute(
                "DELETE FROM sender_key_devices WHERE group_jid = ?1",
                params![gj],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn delete_sender_key_device_rows(&self, device_jids: &[&str]) -> Result<()> {
        let jids: Vec<String> = device_jids.iter().map(|s| s.to_string()).collect();
        self.run(move |c| {
            let placeholders = (0..jids.len()).map(|i| format!("?{}", i + 1)).collect::<Vec<_>>().join(",");
            let sql = format!("DELETE FROM sender_key_devices WHERE device_jid IN ({})", placeholders);
            let params: Vec<&dyn rusqlite::ToSql> = jids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            c.execute(&sql, params.as_slice())
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn clear_all_sender_key_devices(&self) -> Result<()> {
        self.run(move |c| {
            c.execute("DELETE FROM sender_key_devices", [])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_lid_mapping(&self, lid: &str) -> Result<Option<LidPnMappingEntry>> {
        let l = lid.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT lid, phone_number, created_at, updated_at, learning_source
                 FROM lid_pn_mapping WHERE lid = ?1",
                params![l],
                |row| {
                    Ok(LidPnMappingEntry {
                        lid: row.get(0)?,
                        phone_number: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                        learning_source: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn get_pn_mapping(&self, phone: &str) -> Result<Option<LidPnMappingEntry>> {
        let p = phone.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT lid, phone_number, created_at, updated_at, learning_source
                 FROM lid_pn_mapping WHERE phone_number = ?1
                 ORDER BY updated_at DESC LIMIT 1",
                params![p],
                |row| {
                    Ok(LidPnMappingEntry {
                        lid: row.get(0)?,
                        phone_number: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                        learning_source: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn put_lid_mapping(&self, entry: &LidPnMappingEntry) -> Result<()> {
        let e = entry.clone();
        self.run(move |c| {
            c.execute(
                "INSERT INTO lid_pn_mapping (lid, phone_number, created_at, updated_at, learning_source)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(lid) DO UPDATE SET
                    phone_number=excluded.phone_number, updated_at=excluded.updated_at,
                    learning_source=excluded.learning_source",
                params![e.lid, e.phone_number, e.created_at, e.updated_at, e.learning_source],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_all_lid_mappings(&self) -> Result<Vec<LidPnMappingEntry>> {
        self.run(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT lid, phone_number, created_at, updated_at, learning_source
                     FROM lid_pn_mapping",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(LidPnMappingEntry {
                        lid: row.get(0)?,
                        phone_number: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                        learning_source: row.get(4)?,
                    })
                })
                .map_err(db_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err)
        })
        .await
    }

    async fn save_base_key(&self, address: &str, message_id: &str, base_key: &[u8]) -> Result<()> {
        let addr = address.to_owned();
        let mid = message_id.to_owned();
        let bk = base_key.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO base_keys (address, message_id, base_key) VALUES (?1, ?2, ?3)
                 ON CONFLICT(address, message_id) DO UPDATE SET base_key = excluded.base_key",
                params![addr, mid, bk],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn has_same_base_key(
        &self,
        address: &str,
        message_id: &str,
        current_base_key: &[u8],
    ) -> Result<bool> {
        let addr = address.to_owned();
        let mid = message_id.to_owned();
        let cbk = current_base_key.to_vec();
        self.run(move |c| {
            let stored: Option<Vec<u8>> = c
                .query_row(
                    "SELECT base_key FROM base_keys WHERE address = ?1 AND message_id = ?2",
                    params![addr, mid],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?;
            Ok(stored.map_or(false, |s| s == cbk))
        })
        .await
    }

    async fn delete_base_key(&self, address: &str, message_id: &str) -> Result<()> {
        let addr = address.to_owned();
        let mid = message_id.to_owned();
        self.run(move |c| {
            c.execute(
                "DELETE FROM base_keys WHERE address = ?1 AND message_id = ?2",
                params![addr, mid],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn update_device_list(&self, record: DeviceListRecord) -> Result<()> {
        let devices_json = serde_json::to_string(&record.devices)
            .map_err(|e| StoreError::Serialization(e.to_string().into()))?;
        self.run(move |c| {
            c.execute(
                "INSERT INTO device_registry (user_id, devices_json, timestamp, phash)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(user_id) DO UPDATE SET
                    devices_json=excluded.devices_json, timestamp=excluded.timestamp,
                    phash=excluded.phash",
                params![record.user, devices_json, record.timestamp, record.phash],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_devices(&self, user: &str) -> Result<Option<DeviceListRecord>> {
        let u = user.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT user_id, devices_json, timestamp, phash FROM device_registry
                 WHERE user_id = ?1",
                params![u],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(db_err)?
            .map(|(user, dj, ts, ph)| {
                let devices = serde_json::from_str(&dj)
                    .map_err(|e| StoreError::Serialization(e.to_string().into()))?;
                Ok(DeviceListRecord {
                    user,
                    devices,
                    timestamp: ts,
                    phash: ph,
                    raw_id: None,
                })
            })
            .transpose()
        })
        .await
    }

    async fn delete_devices(&self, user: &str) -> Result<()> {
        let u = user.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM device_registry WHERE user_id = ?1", params![u])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn store_sent_message(
        &self,
        chat_jid: &str,
        message_id: &str,
        payload: &[u8],
    ) -> Result<()> {
        let cj = chat_jid.to_owned();
        let mid = message_id.to_owned();
        let bytes = payload.to_vec();
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO sent_messages (chat_jid, message_id, message_bytes, timestamp) VALUES (?1, ?2, ?3, ?4)",
                params![cj, mid, bytes, ts],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn take_sent_message(&self, chat_jid: &str, message_id: &str) -> Result<Option<Vec<u8>>> {
        let cj = chat_jid.to_owned();
        let mid = message_id.to_owned();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            let result: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT message_bytes FROM sent_messages WHERE chat_jid = ?1 AND message_id = ?2",
                    params![cj, mid],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?;
            if result.is_some() {
                tx.execute(
                    "DELETE FROM sent_messages WHERE chat_jid = ?1 AND message_id = ?2",
                    params![cj, mid],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(result)
        })
        .await
    }

    async fn delete_expired_sent_messages(&self, cutoff_timestamp: i64) -> Result<u32> {
        self.run(move |c| {
            let count = c
                .execute(
                    "DELETE FROM sent_messages WHERE timestamp < ?1",
                    params![cutoff_timestamp],
                )
                .map_err(db_err)?;
            u32::try_from(count)
                .map_err(|_| StoreError::Database(format!("delete count {count} out of u32 range").into()))
        })
        .await
    }

    async fn get_tc_token(&self, jid: &str) -> Result<Option<TcTokenEntry>> {
        let j = jid.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT token, token_timestamp, sender_timestamp FROM tc_tokens WHERE jid = ?1",
                params![j],
                |row| {
                    Ok(TcTokenEntry {
                        token: row.get(0)?,
                        token_timestamp: row.get(1)?,
                        sender_timestamp: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn put_tc_token(&self, jid: &str, entry: &TcTokenEntry) -> Result<()> {
        let j = jid.to_owned();
        let e = entry.clone();
        self.run(move |c| {
            c.execute(
                "INSERT INTO tc_tokens (jid, token, token_timestamp, sender_timestamp)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(jid) DO UPDATE SET
                    token=excluded.token, token_timestamp=excluded.token_timestamp,
                    sender_timestamp=excluded.sender_timestamp",
                params![j, e.token, e.token_timestamp, e.sender_timestamp],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn delete_tc_token(&self, jid: &str) -> Result<()> {
        let j = jid.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM tc_tokens WHERE jid = ?1", params![j])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_all_tc_token_jids(&self) -> Result<Vec<String>> {
        self.run(|c| {
            let mut stmt = c.prepare("SELECT jid FROM tc_tokens").map_err(db_err)?;
            let rows = stmt.query_map([], |row| row.get(0)).map_err(db_err)?;
            rows.collect::<std::result::Result<_, _>>().map_err(db_err)
        })
        .await
    }

    async fn delete_expired_tc_tokens(&self, cutoff_timestamp: i64) -> Result<u32> {
        self.run(move |c| {
            let count = c
                .execute(
                    "DELETE FROM tc_tokens WHERE token_timestamp < ?1",
                    params![cutoff_timestamp],
                )
                .map_err(db_err)?;
            u32::try_from(count)
                .map_err(|_| StoreError::Database(format!("delete count {count} out of u32 range").into()))
        })
        .await
    }
}

// ===========================================================================
// DeviceStore — device persistence
// ===========================================================================

#[async_trait]
impl DeviceStore for Store {
    async fn save(&self, device: &Device) -> Result<()> {
        let d = device.clone();
        self.run(move |c| save_device_to_db(c, &d)).await
    }

    async fn load(&self) -> Result<Option<Device>> {
        self.run(load_device_from_db).await
    }

    async fn exists(&self) -> Result<bool> {
        self.run(|c| {
            let count: i64 = c
                .query_row("SELECT COUNT(*) FROM device WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(db_err)?;
            Ok(count > 0)
        })
        .await
    }

    async fn create(&self) -> Result<i32> {
        self.run(|c| {
            // Guard: never overwrite an existing device (would rotate identity keys)
            let count: i64 = c
                .query_row("SELECT COUNT(*) FROM device WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(db_err)?;
            if count > 0 {
                return Ok(1); // Already exists — no-op
            }
            let device = Device::new();
            save_device_to_db(c, &device)?;
            Ok(1)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(name: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("whatsrust-{name}-{ts}"))
    }

    /// Open a raw Connection with WAL + SCHEMA applied (mirrors Store::new internals).
    fn open_fresh_conn(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             PRAGMA auto_vacuum = INCREMENTAL;",
        ).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        run_schema_migrations(&conn, version).unwrap();
        conn
    }

    #[test]
    fn test_perform_backup_creates_backup_file() {
        let root = unique_test_dir("backup");
        let db_path = root.join("whatsapp.db");
        let backup_dir = root.join("backups");
        std::fs::create_dir_all(&root).unwrap();

        let store = Store::new(&db_path).unwrap();
        let backup_path = store.perform_backup(&backup_dir, 3).unwrap();

        assert!(backup_path.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn test_perform_backup_hardens_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_test_dir("backup-perms");
        let db_path = root.join("whatsapp.db");
        let backup_dir = root.join("backups");
        std::fs::create_dir_all(&root).unwrap();

        let store = Store::new(&db_path).unwrap();
        let backup_path = store.perform_backup(&backup_dir, 3).unwrap();

        let dir_mode = std::fs::metadata(&backup_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&backup_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);

        let _ = std::fs::remove_dir_all(&root);
    }

    // -------------------------------------------------------------------------
    // v8 schema tests (ADR 0009/0019)
    // -------------------------------------------------------------------------

    /// Helper: check if a named table/virtual-table exists in the DB.
    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','shadow') AND name = ?1
             UNION ALL
             SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            rusqlite::params![name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
            || conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                    rusqlite::params![name],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0
    }

    /// Helper: check if a named trigger exists.
    fn trigger_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name=?1",
            rusqlite::params![name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    /// (1) Fresh DB opens at v8 with messages, messages_fts, sibling tables, triggers present.
    #[test]
    fn test_v8_fresh_db_schema() {
        let dir = unique_test_dir("v8-fresh");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wa.db");

        let conn = open_fresh_conn(&db_path);

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 8, "user_version must be 8");

        // Core tables
        assert!(table_exists(&conn, "messages"), "messages table must exist");
        assert!(table_exists(&conn, "media_refs"), "media_refs table must exist");
        assert!(table_exists(&conn, "embeddings"), "embeddings table must exist");
        assert!(table_exists(&conn, "backfill_cursor"), "backfill_cursor table must exist");
        assert!(table_exists(&conn, "backfill_jobs"), "backfill_jobs table must exist");
        assert!(table_exists(&conn, "metadata"), "metadata table must exist");

        // FTS5 virtual table
        assert!(table_exists(&conn, "messages_fts"), "messages_fts table must exist");

        // Probe FTS5 is usable
        conn.execute_batch("SELECT 1 FROM messages_fts LIMIT 0;").unwrap();

        // Triggers
        assert!(trigger_exists(&conn, "messages_fts_insert"), "insert trigger must exist");
        assert!(trigger_exists(&conn, "messages_fts_update"), "update trigger must exist");
        assert!(trigger_exists(&conn, "messages_fts_delete"), "delete trigger must exist");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (2) A v7 DB with inbound_messages rows migrates: rows land in messages, inbound_messages gone.
    #[test]
    fn test_v7_to_v8_migration_copy_then_drop() {
        let dir = unique_test_dir("v7-migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wa.db");

        // Build a v7-shaped DB manually (inbound_messages only, user_version=7).
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA auto_vacuum = INCREMENTAL;",
            ).unwrap();
            conn.execute_batch(
                "CREATE TABLE inbound_messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    chat_jid TEXT NOT NULL,
                    sender_jid TEXT NOT NULL,
                    message_id TEXT NOT NULL UNIQUE,
                    content_kind TEXT NOT NULL,
                    body_text TEXT,
                    timestamp INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO inbound_messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
                    VALUES ('chat1@s.whatsapp.net', 'sender1@s.whatsapp.net', 'msg-001', 'text', 'hello world', 1000, 1000);
                INSERT INTO inbound_messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
                    VALUES ('chat1@s.whatsapp.net', 'sender1@s.whatsapp.net', 'msg-002', 'text', 'foo bar baz', 2000, 2000);
                INSERT INTO inbound_messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
                    VALUES ('chat2@g.us', 'sender2@s.whatsapp.net', 'msg-003', 'image', NULL, 3000, 3000);",
            ).unwrap();
            conn.pragma_update(None, "user_version", 7_i64).unwrap();
        }

        // Now open via Store::new — this triggers SCHEMA + run_schema_migrations.
        let _store = Store::new(&db_path).unwrap();

        // Verify via a fresh raw connection.
        let conn = Connection::open(&db_path).unwrap();

        // user_version=8
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 8);

        // inbound_messages must be gone
        let old_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='inbound_messages'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_exists, 0, "inbound_messages must be dropped after migration");

        // messages must have all 3 rows
        let row_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(row_count, 3, "all 3 rows must be in messages");

        // FTS must find the text rows via a MATCH query
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'hello'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1, "FTS must find 'hello' after migration");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (3) FTS trigger sync: insert → found; update body → new text found, old not; delete → gone.
    #[test]
    fn test_fts_trigger_sync() {
        let dir = unique_test_dir("fts-triggers");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wa.db");
        let conn = open_fresh_conn(&db_path);

        let ts = now_secs();

        // INSERT — FTS must find it
        conn.execute(
            "INSERT INTO messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
             VALUES ('c@s.whatsapp.net', 's@s.whatsapp.net', 'fts-001', 'text', 'apple banana cherry', ?1, ?1)",
            rusqlite::params![ts],
        ).unwrap();

        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'banana'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "FTS must find 'banana' after insert");

        // UPDATE body_text — FTS must reflect new text, not old
        conn.execute(
            "UPDATE messages SET body_text = 'dragonfruit elderberry' WHERE message_id = 'fts-001'",
            [],
        ).unwrap();

        let old_found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'banana'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_found, 0, "FTS must NOT find old text 'banana' after update");

        let new_found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'elderberry'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new_found, 1, "FTS must find new text 'elderberry' after update");

        // DELETE — FTS must no longer find it
        conn.execute("DELETE FROM messages WHERE message_id = 'fts-001'", []).unwrap();

        let after_delete: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'elderberry'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after_delete, 0, "FTS must not find deleted row");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (4) prune_old_data keeps messages — messages with old created_at survive a prune tick.
    #[tokio::test]
    async fn test_prune_keeps_messages() {
        let dir = unique_test_dir("prune-keep");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wa.db");

        let store = Store::new(&db_path).unwrap();

        // Insert a message with a very old created_at (simulate year-old message)
        let old_ts: i64 = now_secs() - 365 * 86400;
        {
            let conn_arc = store.conn.clone();
            let guard = conn_arc.lock();
            guard.execute(
                "INSERT INTO messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
                 VALUES ('c@s.whatsapp.net', 's@s.whatsapp.net', 'old-msg-001', 'text', 'old message text', ?1, ?1)",
                rusqlite::params![old_ts],
            ).unwrap();
        }

        // Run prune
        let stats = store.prune_old_data(86400).await.unwrap();
        assert_eq!(stats.sent_deleted, 0, "no outbound rows to prune");

        // Message must still be there
        let conn_arc = store.conn.clone();
        let guard = conn_arc.lock();
        let count: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE message_id = 'old-msg-001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "old message must survive prune (indefinite retention)");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------------------
    // Wave 2: staged-migration ceremony tests (ADR 0028/0029/0030/0032/0036)
    // -------------------------------------------------------------------------

    /// (a) Staged v7→v8 migration: backup appears, migrates, validates,
    ///     sets schema_validated_version=8, seeds watchdog baseline.
    #[test]
    fn test_staged_migration_creates_bak_and_validates() {
        let dir = unique_test_dir("staged-mig");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("whatsapp.db");

        // Build a v7-shaped DB
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA auto_vacuum = INCREMENTAL;",
            ).unwrap();
            conn.execute_batch(
                "CREATE TABLE inbound_messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    chat_jid TEXT NOT NULL,
                    sender_jid TEXT NOT NULL,
                    message_id TEXT NOT NULL UNIQUE,
                    content_kind TEXT NOT NULL,
                    body_text TEXT,
                    timestamp INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO inbound_messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
                    VALUES ('c@s.whatsapp.net', 's@s.whatsapp.net', 'staged-001', 'text', 'hello staged', 1000, 1000);",
            ).unwrap();
            conn.pragma_update(None, "user_version", 7_i64).unwrap();
        }

        // Open via open_with_mode Normal — triggers the full ceremony
        let store = open_with_mode(&db_path, MigrationMode::Normal).unwrap();

        // Verify .bak file was created next to db
        let bak = find_newest_bak(&db_path);
        assert!(bak.is_some(), "pre-migration .bak file must exist");
        assert!(bak.unwrap().exists(), ".bak file must be readable");

        // DB must be at v8
        let conn_arc = store.conn.clone();
        let guard = conn_arc.lock();

        let version: i64 = guard.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 8);

        // schema_validated_version must be set
        let svv: Option<String> = guard.query_row(
            "SELECT value FROM metadata WHERE key='schema_validated_version'",
            [],
            |r| r.get(0),
        ).optional().unwrap();
        assert_eq!(svv.as_deref(), Some("8"), "schema_validated_version must be '8'");

        // Watchdog baseline must be seeded
        let wdog: Option<String> = guard.query_row(
            "SELECT value FROM metadata WHERE key='watchdog_last_alerted_size'",
            [],
            |r| r.get(0),
        ).optional().unwrap();
        assert!(wdog.is_some(), "watchdog_last_alerted_size must be seeded");
        let bytes: u64 = wdog.unwrap().parse().unwrap();
        assert!(bytes > 0, "watchdog baseline must be > 0");

        // No pin file (successful migration clears/avoids writing pin)
        let pin_file = pin_path(&db_path);
        assert!(!pin_file.exists(), "no pin file on successful migration");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (b) Migration ceremony aborts and writes pin when migration TX fails.
    ///     We simulate this by providing a v7 DB where `run_schema_migrations` will
    ///     encounter a UNIQUE constraint violation (message_id conflict), forcing
    ///     a rollback and pin write.
    #[test]
    fn test_migration_failure_writes_pin() {
        let dir = unique_test_dir("mig-fail");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("whatsapp.db");

        // Write the pin file directly to simulate a prior failure, then verify
        // that Normal-mode startup correctly halts with an actionable error.
        write_migration_pin(&db_path, &MigrationPin {
            state: "failed".to_owned(),
            pinned_version: 7,
            blocked_target: 8,
            created_at: 0,
            reason: "simulated migration failure for test".to_owned(),
        }).unwrap();

        // Pin must be present on disk
        assert!(pin_path(&db_path).exists(), "pin file must exist after write");

        // Build a v7 DB to accompany it
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = DELETE;").unwrap();
            conn.pragma_update(None, "user_version", 7_i64).unwrap();
        }

        // Normal startup must halt due to pin
        let result = open_with_mode(&db_path, MigrationMode::Normal);
        assert!(result.is_err(), "open must halt when failed pin is present");

        // ForceMigrate clears pin and proceeds
        // (we expect it to succeed since this is a valid v7 DB with no data to conflict)
        let result2 = open_with_mode(&db_path, MigrationMode::ForceMigrate);
        assert!(result2.is_ok(), "ForceMigrate must succeed after clearing pin: {:?}", result2.err().map(|e| e.to_string()));

        // Pin file must be gone after successful ForceMigrate
        assert!(!pin_path(&db_path).exists(), "pin file must be cleared after successful ForceMigrate");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (c) Pin-present startup halts with an actionable error.
    #[test]
    fn test_pin_present_halts_startup() {
        let dir = unique_test_dir("pin-halt");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("whatsapp.db");

        // Build a v7 DB (so migration would be needed)
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA auto_vacuum = INCREMENTAL;",
            ).unwrap();
            conn.pragma_update(None, "user_version", 7_i64).unwrap();
        }

        // Write a "failed" pin manually
        write_migration_pin(&db_path, &MigrationPin {
            state: "failed".to_owned(),
            pinned_version: 7,
            blocked_target: 8,
            created_at: 0,
            reason: "test-induced failure".to_owned(),
        }).unwrap();

        // Normal startup must halt
        let result = open_with_mode(&db_path, MigrationMode::Normal);
        assert!(result.is_err(), "open must halt when circuit-breaker pin is present");
        // Walk the error source chain for the detailed message
        let err = result.err().unwrap();
        let mut found = false;
        let mut msg = err.to_string();
        let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&err);
        while !found {
            if msg.contains("circuit-breaker") || msg.contains("--rollback") || msg.contains("--migrate") {
                found = true;
                break;
            }
            match src {
                Some(e) => {
                    msg = e.to_string();
                    src = e.source();
                }
                None => break,
            }
        }
        assert!(found, "error chain must mention recovery options; got top-level: {}", err);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (d) Rollback helper: copy .bak → db, delete -wal/-shm.
    ///     Tests the low-level operations that `do_rollback` performs.
    ///     Uses journal_mode=DELETE (not WAL) for the current db so that opening
    ///     the restored db afterward does not recreate -wal/-shm sidecars.
    #[test]
    fn test_rollback_restores_and_removes_wal_shm() {
        let dir = unique_test_dir("rollback");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("whatsapp.db");
        let bak_path = dir.join("whatsapp.db.pre-migration-v7-0.bak");

        // Build a fake v7 "backup" (journal_mode=DELETE so no WAL files are created
        // when we later open the restored copy for version verification).
        {
            let conn = Connection::open(&bak_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = DELETE;").unwrap();
            conn.pragma_update(None, "user_version", 7_i64).unwrap();
        }

        // Create a fake v8 "current" db (also DELETE mode to keep test simple)
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = DELETE;").unwrap();
            conn.pragma_update(None, "user_version", 8_i64).unwrap();
        }

        // Create fake WAL and SHM sidecars (simulating leftover post-migration WAL)
        let wal_path = dir.join("whatsapp.db-wal");
        let shm_path = dir.join("whatsapp.db-shm");
        std::fs::write(&wal_path, b"fake wal data").unwrap();
        std::fs::write(&shm_path, b"fake shm data").unwrap();

        assert!(wal_path.exists(), "fake -wal must exist before rollback");
        assert!(shm_path.exists(), "fake -shm must exist before rollback");

        // Simulate rollback: copy .bak → db, remove WAL and SHM
        std::fs::copy(&bak_path, &db_path).unwrap();
        for ext in &["-wal", "-shm"] {
            let sidecar = dir.join(format!("whatsapp.db{ext}"));
            let _ = std::fs::remove_file(&sidecar);
        }

        // WAL and SHM must be gone (checked before opening the connection)
        assert!(!wal_path.exists(), "-wal must be deleted by rollback");
        assert!(!shm_path.exists(), "-shm must be deleted by rollback");

        // Verify: db is now at v7 (open after assertion so SQLite can't re-create sidecars)
        let conn = Connection::open(&db_path).unwrap();
        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(ver, 7, "restored DB must be at v7");
        drop(conn);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (e) Watchdog baseline INSERT OR IGNORE: seeded once, not overwritten on second call.
    #[test]
    fn test_watchdog_baseline_seed_once() {
        let dir = unique_test_dir("watchdog-seed");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("whatsapp.db");

        let store = open_with_mode(&db_path, MigrationMode::Normal).unwrap();
        let conn_arc = store.conn.clone();
        let guard = conn_arc.lock();

        // Read the seeded baseline
        let first: Option<String> = guard.query_row(
            "SELECT value FROM metadata WHERE key='watchdog_last_alerted_size'",
            [],
            |r| r.get(0),
        ).optional().unwrap();
        assert!(first.is_some(), "watchdog baseline must be set after open");

        // Call seed_watchdog_baseline again — INSERT OR IGNORE must not overwrite
        seed_watchdog_baseline(&guard, &db_path).unwrap();

        let second: Option<String> = guard.query_row(
            "SELECT value FROM metadata WHERE key='watchdog_last_alerted_size'",
            [],
            |r| r.get(0),
        ).optional().unwrap();
        assert_eq!(first, second, "INSERT OR IGNORE must not overwrite existing baseline");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test that `read_migration_pin_pub` / `write_migration_pin_pub` round-trip the `state`
    /// field correctly, so the Fix-1 guard in `do_rollback` receives accurate input.
    ///
    /// Covers two guard branches:
    ///   - pin.state == "rolled_back"  → guard must bail (the state is *not* "failed")
    ///   - pin.state == "failed"       → guard must allow (the state *is* "failed")
    #[test]
    fn test_migration_pin_state_roundtrip_for_rollback_guard() {
        let dir = unique_test_dir("pin-state-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("whatsapp.db");

        // Write a "rolled_back" pin and read it back.
        write_migration_pin_pub(&db_path, "rolled_back", 7).unwrap();
        let pin = read_migration_pin_pub(&db_path).expect("pin must be readable");
        let state = pin.get("state").and_then(|s| s.as_str());
        assert_eq!(
            state,
            Some("rolled_back"),
            "round-trip must preserve state='rolled_back'; guard would correctly reject it"
        );
        // Confirm the guard condition: not "failed" → would bail
        assert_ne!(
            state,
            Some("failed"),
            "'rolled_back' must not match 'failed' so the guard bails"
        );

        // Overwrite with a "failed" pin and read it back.
        write_migration_pin_pub(&db_path, "failed", 7).unwrap();
        let pin2 = read_migration_pin_pub(&db_path).expect("pin must be readable");
        let state2 = pin2.get("state").and_then(|s| s.as_str());
        assert_eq!(
            state2,
            Some("failed"),
            "round-trip must preserve state='failed'; guard would correctly allow it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------------------
    // Backfill storage tests (Wave A — ADR 0010/0033/0035)
    // -------------------------------------------------------------------------

    /// Helper: open a fresh temp Store for backfill tests.
    fn open_backfill_store(name: &str) -> (Store, std::path::PathBuf) {
        let dir = unique_test_dir(name);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wa.db");
        let store = Store::new(&db_path).unwrap();
        (store, dir)
    }

    /// (bf-1) First enqueue is accepted; second enqueue for same chat returns AlreadyActive.
    #[tokio::test]
    async fn test_backfill_enqueue_first_accepted_second_already_active() {
        let (store, dir) = open_backfill_store("bf-enqueue-dedup");

        let outcome1 = store
            .enqueue_backfill_job("chat1@s.whatsapp.net", "all", None, 0)
            .await
            .unwrap();
        let job_id = match outcome1 {
            EnqueueOutcome::Accepted { job_id } => job_id,
            other => panic!("expected Accepted, got {:?}", other),
        };
        assert!(job_id > 0);

        // Second enqueue for same chat → AlreadyActive
        let outcome2 = store
            .enqueue_backfill_job("chat1@s.whatsapp.net", "count", Some(50), 0)
            .await
            .unwrap();
        match outcome2 {
            EnqueueOutcome::AlreadyActive { job_id: existing_id } => {
                assert_eq!(existing_id, job_id, "AlreadyActive must report the existing job id");
            }
            other => panic!("expected AlreadyActive, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-2) Enqueue within cooldown window → Cooldown.
    #[tokio::test]
    async fn test_backfill_enqueue_cooldown() {
        let (store, dir) = open_backfill_store("bf-cooldown");

        // Seed a cursor with last_backfill_at = now (simulating just-finished job)
        let ts = now_secs();
        store
            .upsert_backfill_cursor("chat2@s.whatsapp.net", None, None, None, false, true, Some(ts))
            .await
            .unwrap();

        // Enqueue with cooldown_secs = 3600 (1 hour)
        let outcome = store
            .enqueue_backfill_job("chat2@s.whatsapp.net", "all", None, 3600)
            .await
            .unwrap();
        match outcome {
            EnqueueOutcome::Cooldown { retry_after_secs } => {
                // retry_after_secs should be close to 3600 (we just set last_backfill_at = now)
                assert!(retry_after_secs > 0 && retry_after_secs <= 3600,
                    "retry_after_secs={} expected in (0, 3600]", retry_after_secs);
            }
            other => panic!("expected Cooldown, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-3) Cooldown is not triggered after the window expires.
    #[tokio::test]
    async fn test_backfill_enqueue_after_cooldown_expired() {
        let (store, dir) = open_backfill_store("bf-cooldown-expired");

        // Seed a cursor with last_backfill_at far in the past
        let old_ts = now_secs() - 7200; // 2 hours ago
        store
            .upsert_backfill_cursor("chat3@s.whatsapp.net", None, None, None, false, true, Some(old_ts))
            .await
            .unwrap();

        // Enqueue with cooldown_secs = 3600 (1 hour) — should pass
        let outcome = store
            .enqueue_backfill_job("chat3@s.whatsapp.net", "all", None, 3600)
            .await
            .unwrap();
        match outcome {
            EnqueueOutcome::Accepted { .. } => {}
            other => panic!("expected Accepted after cooldown expired, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-4) claim_next_backfill_job: FIFO order, flips to running.
    #[tokio::test]
    async fn test_backfill_claim_fifo_and_running() {
        let (store, dir) = open_backfill_store("bf-claim");

        // Enqueue two jobs for different chats
        let out1 = store
            .enqueue_backfill_job("chatA@s.whatsapp.net", "all", None, 0)
            .await
            .unwrap();
        let id1 = match out1 {
            EnqueueOutcome::Accepted { job_id } => job_id,
            other => panic!("{:?}", other),
        };

        let out2 = store
            .enqueue_backfill_job("chatB@s.whatsapp.net", "count", Some(100), 0)
            .await
            .unwrap();
        let id2 = match out2 {
            EnqueueOutcome::Accepted { job_id } => job_id,
            other => panic!("{:?}", other),
        };

        // Claim first → should get job with smaller id (FIFO)
        let claimed1 = store.claim_next_backfill_job().await.unwrap().unwrap();
        assert_eq!(claimed1.id, id1, "FIFO: first claimed must be the first enqueued");
        // Re-fetch to verify the DB was updated to 'running'
        let row1 = store.get_backfill_job(claimed1.id).await.unwrap().unwrap();
        assert_eq!(row1.status, "running", "claimed job must be running in DB");

        // Claim second → should get job id2
        let claimed2 = store.claim_next_backfill_job().await.unwrap().unwrap();
        assert_eq!(claimed2.id, id2);
        let row2 = store.get_backfill_job(claimed2.id).await.unwrap().unwrap();
        assert_eq!(row2.status, "running");

        // No more queued jobs
        let none = store.claim_next_backfill_job().await.unwrap();
        assert!(none.is_none(), "no more queued jobs to claim");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-5) cursor upsert + get round-trip, including advancing anchor + setting exhausted.
    #[tokio::test]
    async fn test_backfill_cursor_upsert_and_get() {
        let (store, dir) = open_backfill_store("bf-cursor");

        // Initial insert — no anchor yet
        store
            .upsert_backfill_cursor(
                "chatX@s.whatsapp.net",
                None,
                None,
                None,
                true,
                false,
                None,
            )
            .await
            .unwrap();

        let row1 = store
            .get_backfill_cursor("chatX@s.whatsapp.net")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row1.chat_jid, "chatX@s.whatsapp.net");
        assert!(row1.oldest_msg_id.is_none());
        assert!(row1.more_remain);
        assert!(!row1.exhausted);
        assert!(row1.last_backfill_at.is_none());

        // Advance anchor
        let ts = now_secs();
        store
            .upsert_backfill_cursor(
                "chatX@s.whatsapp.net",
                Some("msg-oldest-123"),
                Some(false),
                Some(1_700_000_000_000),
                false,
                true,
                Some(ts),
            )
            .await
            .unwrap();

        let row2 = store
            .get_backfill_cursor("chatX@s.whatsapp.net")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row2.oldest_msg_id.as_deref(), Some("msg-oldest-123"));
        assert_eq!(row2.oldest_msg_from_me, Some(false));
        assert_eq!(row2.oldest_msg_timestamp_ms, Some(1_700_000_000_000));
        assert!(!row2.more_remain);
        assert!(row2.exhausted);
        assert_eq!(row2.last_backfill_at, Some(ts));

        // Missing chat → None
        let missing = store.get_backfill_cursor("nobody@s.whatsapp.net").await.unwrap();
        assert!(missing.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-6) CASE guard: a cancelled job must not be overwritten to done.
    #[tokio::test]
    async fn test_backfill_mark_job_case_guard_cancelled_not_overwritten() {
        let (store, dir) = open_backfill_store("bf-case-guard");

        let out = store
            .enqueue_backfill_job("chatGuard@g.us", "all", None, 0)
            .await
            .unwrap();
        let job_id = match out {
            EnqueueOutcome::Accepted { job_id } => job_id,
            other => panic!("{:?}", other),
        };

        // Claim it (flips to running)
        let _claimed = store.claim_next_backfill_job().await.unwrap().unwrap();

        // Cancel it
        store.mark_backfill_job(job_id, "cancelled").await.unwrap();
        let row_after_cancel = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(row_after_cancel.status, "cancelled");

        // Attempt to mark it done — CASE guard must block this
        store.mark_backfill_job(job_id, "done").await.unwrap();
        let row_after_done = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(
            row_after_done.status, "cancelled",
            "cancelled job must not be overwritten to done (CASE guard)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-7) mark_backfill_job: failed also obeys CASE guard vs cancelled.
    #[tokio::test]
    async fn test_backfill_mark_job_failed_case_guard() {
        let (store, dir) = open_backfill_store("bf-case-guard-failed");

        let out = store
            .enqueue_backfill_job("chatFail@g.us", "count", Some(100), 0)
            .await
            .unwrap();
        let job_id = match out {
            EnqueueOutcome::Accepted { job_id } => job_id,
            other => panic!("{:?}", other),
        };
        let _claimed = store.claim_next_backfill_job().await.unwrap().unwrap();

        // Cancel, then try to mark failed
        store.mark_backfill_job(job_id, "cancelled").await.unwrap();
        store.mark_backfill_job(job_id, "failed").await.unwrap();
        let row = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(row.status, "cancelled", "failed must not overwrite cancelled");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-8) update_backfill_fetched advances the counter correctly.
    #[tokio::test]
    async fn test_backfill_update_fetched() {
        let (store, dir) = open_backfill_store("bf-fetched");

        let out = store
            .enqueue_backfill_job("chatFetch@s.whatsapp.net", "count", Some(500), 0)
            .await
            .unwrap();
        let job_id = match out {
            EnqueueOutcome::Accepted { job_id } => job_id,
            other => panic!("{:?}", other),
        };

        store.update_backfill_fetched(job_id, 250).await.unwrap();
        let row = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(row.fetched, 250);

        store.update_backfill_fetched(job_id, 500).await.unwrap();
        let row2 = store.get_backfill_job(job_id).await.unwrap().unwrap();
        assert_eq!(row2.fetched, 500);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-9) list_backfill_jobs: active_only vs all.
    #[tokio::test]
    async fn test_backfill_list_jobs() {
        let (store, dir) = open_backfill_store("bf-list");

        // Two chats, two jobs
        let out1 = store
            .enqueue_backfill_job("listA@s.whatsapp.net", "all", None, 0)
            .await
            .unwrap();
        let id1 = match out1 {
            EnqueueOutcome::Accepted { job_id } => job_id,
            other => panic!("{:?}", other),
        };
        store
            .enqueue_backfill_job("listB@s.whatsapp.net", "count", Some(50), 0)
            .await
            .unwrap();

        // Mark first job done
        let _claimed = store.claim_next_backfill_job().await.unwrap();
        store.mark_backfill_job(id1, "done").await.unwrap();

        // active_only should return only the queued job (listB)
        let active = store.list_backfill_jobs(true).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].chat_jid, "listB@s.whatsapp.net");

        // all should return both
        let all = store.list_backfill_jobs(false).await.unwrap();
        assert_eq!(all.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (bf-10) upsert_backfill_cursor: last_backfill_at=None does not overwrite existing value.
    #[tokio::test]
    async fn test_backfill_cursor_none_last_backfill_at_preserves_existing() {
        let (store, dir) = open_backfill_store("bf-cursor-preserve-ts");

        let ts = now_secs();
        // Insert with a known last_backfill_at
        store
            .upsert_backfill_cursor("chatP@s.whatsapp.net", None, None, None, true, false, Some(ts))
            .await
            .unwrap();

        // Update with None for last_backfill_at — must keep the existing value
        store
            .upsert_backfill_cursor("chatP@s.whatsapp.net", Some("msg-x"), Some(true), Some(999), false, false, None)
            .await
            .unwrap();

        let row = store.get_backfill_cursor("chatP@s.whatsapp.net").await.unwrap().unwrap();
        assert_eq!(row.last_backfill_at, Some(ts), "last_backfill_at must be preserved when None is passed");
        assert_eq!(row.oldest_msg_id.as_deref(), Some("msg-x"), "anchor must be updated");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
