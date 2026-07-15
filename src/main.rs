//! whatsrust — Pure Rust WhatsApp bridge.
//!
//! Lean replacement for the Baileys (Node.js) sidecar.
//! Uses whatsapp-rust (wa-rs) for the WhatsApp Web protocol
//! and our own rusqlite backend for Signal Protocol storage.
//!
//! Two modes:
//!   whatsrust              — daemon mode (bridge + REPL + API server)
//!   whatsrust <command>    — CLI mode (sends HTTP to running daemon, prints JSON)

mod api;
mod backfill;
mod bridge;
mod bridge_events;
mod dedup;
mod instance_lock;
mod media_utils;
mod outbound;
mod polls;
pub mod qr;
mod read_receipts;
mod storage;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use bridge::{BridgeConfig, WhatsAppBridge};
use storage::{MigrationMode, open_with_mode};
use whatsrust::mcp;

const MAX_LOCAL_MEDIA_READ_BYTES: u64 = 50 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Backfill safety validation (ADR 0022) — pure, testable, no I/O
// ---------------------------------------------------------------------------

/// The three ban-critical backfill knobs that are validated at startup.
pub struct BackfillSafety {
    pub interval_secs: u64,
    pub max_concurrent: u32,
    pub max_messages: i64,
}

/// Per-knob override flags (set via `WHATSRUST_DANGEROUSLY_ALLOW_*` env vars).
pub struct SafetyOverrides {
    pub allow_fast: bool,
    pub allow_high_concurrency: bool,
    pub allow_huge_fetch: bool,
}

/// Validate the three ban-critical backfill knobs against their safety bounds.
///
/// Returns `Ok(warnings)` when all violations are overridden (warnings are non-empty if any
/// override flag is active). Returns `Err(message)` for the first unoverridable violation;
/// the caller must print the message and exit non-zero BEFORE constructing the bridge.
///
/// Rules (ADR 0022):
/// - `interval_secs < 3`  → violation; override: `WHATSRUST_DANGEROUSLY_ALLOW_FAST_BACKFILL=1`
/// - `max_concurrent > 3` → violation; override: `WHATSRUST_DANGEROUSLY_ALLOW_HIGH_CONCURRENCY=1`
/// - `max_messages > 50000` → violation; override: `WHATSRUST_DANGEROUSLY_ALLOW_HUGE_FETCH=1`
///
/// Multiple violations: the first violation encountered (in the order above) is reported as `Err`
/// if unoverridden; overridden violations each contribute a warning string to the `Ok` return.
pub fn validate_backfill_safety(
    s: &BackfillSafety,
    ov: &SafetyOverrides,
) -> std::result::Result<Vec<String>, String> {
    let mut warnings: Vec<String> = Vec::new();

    // 1. interval_secs floor: 3 s (too-fast polling is a ban risk)
    if s.interval_secs < 3 {
        if ov.allow_fast {
            warnings.push("fast_backfill_override_active".to_string());
        } else {
            return Err(format!(
                "REFUSE TO START: WHATSRUST_BACKFILL_INTERVAL_SECS={} is below the safety floor of 3 seconds.\n\
                 Reason: polling faster than 3 s risks WhatsApp rate-limiting or account ban.\n\
                 To override (dangerous): set WHATSRUST_DANGEROUSLY_ALLOW_FAST_BACKFILL=1",
                s.interval_secs
            ));
        }
    }

    // 2. max_concurrent ceiling: 3 (parallel backfill streams amplify ban risk)
    if s.max_concurrent > 3 {
        if ov.allow_high_concurrency {
            warnings.push("high_concurrency_override_active".to_string());
        } else {
            return Err(format!(
                "REFUSE TO START: WHATSRUST_BACKFILL_MAX_CONCURRENT={} exceeds the safety ceiling of 3.\n\
                 Reason: running more than 3 concurrent backfill workers amplifies WhatsApp ban risk.\n\
                 To override (dangerous): set WHATSRUST_DANGEROUSLY_ALLOW_HIGH_CONCURRENCY=1",
                s.max_concurrent
            ));
        }
    }

    // 3. max_messages ceiling: 50000 (huge single fetches attract server-side scrutiny)
    if s.max_messages > 50_000 {
        if ov.allow_huge_fetch {
            warnings.push("huge_fetch_override_active".to_string());
        } else {
            return Err(format!(
                "REFUSE TO START: WHATSRUST_BACKFILL_MAX_MESSAGES={} exceeds the safety ceiling of 50000.\n\
                 Reason: requesting more than 50 000 messages in a single backfill job attracts \
                 server-side scrutiny and risks account suspension.\n\
                 To override (dangerous): set WHATSRUST_DANGEROUSLY_ALLOW_HUGE_FETCH=1",
                s.max_messages
            ));
        }
    }

    Ok(warnings)
}

/// Parse a DANGEROUSLY_ALLOW_* flag: truthy = "1" or "true" (case-insensitive).
/// If the var is set but not a recognised truthy value, warns and returns false (fail-safe).
/// Requires tracing to be initialised before use.
fn parse_dangerously_flag(key: &str) -> bool {
    match std::env::var(key) {
        Err(_) => false,
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => true,
        Ok(v) if v.is_empty() => false,
        Ok(v) => {
            tracing::warn!(
                var = key,
                value = %v,
                "unrecognised value for override flag — treating as false (use =1 to enable)"
            );
            false
        }
    }
}

/// Parse an env var into T, warning via tracing if the var is present but unparseable.
/// Requires tracing to be initialised before use.
fn parse_env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    match std::env::var(key) {
        Ok(v) => v.parse().unwrap_or_else(|_| {
            tracing::warn!(var = key, value = %v, "unparseable env value; using default");
            default
        }),
        Err(_) => default,
    }
}

/// Read the API port from env, with fallback chain.
fn get_port() -> u16 {
    std::env::var("WHATSRUST_PORT")
        .or_else(|_| std::env::var("HEALTH_PORT"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7270)
}

fn read_local_media_file(path: &Path) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        anyhow::bail!("path is not a regular file: {}", path.display());
    }
    if meta.len() > MAX_LOCAL_MEDIA_READ_BYTES {
        anyhow::bail!(
            "file exceeds size limit ({} bytes > {} bytes): {}",
            meta.len(),
            MAX_LOCAL_MEDIA_READ_BYTES,
            path.display()
        );
    }
    Ok(std::fs::read(path)?)
}

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // --rollback and --migrate are maintenance subcommands that operate on the DB
    // directly (they must run BEFORE the daemon's instance lock and before cli_main,
    // which would forward the command to a running daemon via HTTP).
    if args.get(1).map(|s| s.as_str()) == Some("--rollback") {
        return do_rollback(&args).await;
    }
    if args.get(1).map(|s| s.as_str()) == Some("--migrate") {
        return do_force_migrate().await;
    }

    // CLI mode: fire request to running daemon
    if args.len() > 1 {
        return cli_main(&args[1..]).await;
    }

    // --- dotenvy: load .env file BEFORE tracing init so RUST_LOG in .env takes effect (ADR 0023) ---
    // Absent file → silent no-op. Malformed entry → eprintln! (tracing not yet initialised).
    // Real env vars take precedence (dotenvy fills unset vars only).
    {
        let env_path = std::env::var("WHATSRUST_ENV_FILE")
            .unwrap_or_else(|_| ".env".to_string());
        match dotenvy::from_path(&env_path) {
            Ok(_) => {}
            Err(dotenvy::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
                // absent file is a no-op
            }
            Err(e) => {
                eprintln!("whatsrust: dotenvy: malformed .env file at {env_path}: {e} — continuing with environment only");
            }
        }
    }

    // Daemon mode
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "whatsrust=info,whatsapp_rust=info".parse().unwrap()),
        )
        .init();

    info!("whatsrust v{}", env!("CARGO_PKG_VERSION"));

    let (inbound_tx, mut inbound_rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();

    // Allowed numbers: only bridge messages from these senders (empty = all)
    let allowed: Vec<String> = std::env::var("WHATSAPP_ALLOWED")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.chars().filter(|c| c.is_ascii_digit()).collect::<String>())
        .filter(|s| !s.is_empty())
        .collect();

    if !allowed.is_empty() {
        info!(allowed = ?allowed, "sender allowlist active");
    }

    let api_port = get_port();

    let backup_dir = std::env::var("BACKUP_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("whatsapp.db.backups")));

    let send_burst: u32 = std::env::var("WHATSRUST_SEND_BURST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    // History sync is ENABLED by default: it delivers the trusted-contact tokens
    // (tctokens) that WhatsApp requires to send 1:1 DMs (server nacks token-less
    // DMs with `463 MissingTcToken`), plus pushname/nct_salt. The old "deaf client"
    // rationale for skipping it is obsolete on the current wa-rs (history is processed
    // on a dedicated worker off the live path). Set WHATSRUST_SKIP_HISTORY_SYNC=1 to
    // opt out (leaner initial pairing; but 1:1 DMs will fail with 463). See ADR 0037.
    let skip_history_sync = std::env::var("WHATSRUST_SKIP_HISTORY_SYNC")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // --- Backfill knobs (ADR 0021/0022/0023) ---
    let bf_default = bridge::BridgeConfig::default();

    let backfill_interval_secs: u64 = parse_env_or("WHATSRUST_BACKFILL_INTERVAL_SECS", bf_default.backfill_interval_secs);
    let backfill_max_concurrent: u32 = parse_env_or("WHATSRUST_BACKFILL_MAX_CONCURRENT", bf_default.backfill_max_concurrent);
    let backfill_max_messages: i64 = parse_env_or("WHATSRUST_BACKFILL_MAX_MESSAGES", bf_default.backfill_max_messages);
    let backfill_batch_size: i32 = parse_env_or("WHATSRUST_BACKFILL_BATCH_SIZE", bf_default.backfill_batch_size);
    let backfill_backstop: u32 = parse_env_or("WHATSRUST_BACKFILL_BACKSTOP", bf_default.backfill_backstop);
    let backfill_jitter_frac: f64 = parse_env_or("WHATSRUST_BACKFILL_JITTER_FRAC", bf_default.backfill_jitter_frac);
    let backfill_cooldown_secs: i64 = parse_env_or("WHATSRUST_BACKFILL_COOLDOWN_SECS", bf_default.backfill_cooldown_secs);
    let backfill_queue_depth: i64 = parse_env_or("WHATSRUST_BACKFILL_QUEUE_DEPTH", bf_default.backfill_queue_depth);

    // --- Fail-closed safety validation (ADR 0022) — MUST run before bridge construction ---
    let safety = BackfillSafety {
        interval_secs: backfill_interval_secs,
        max_concurrent: backfill_max_concurrent,
        max_messages: backfill_max_messages,
    };
    let overrides = SafetyOverrides {
        allow_fast: parse_dangerously_flag("WHATSRUST_DANGEROUSLY_ALLOW_FAST_BACKFILL"),
        allow_high_concurrency: parse_dangerously_flag("WHATSRUST_DANGEROUSLY_ALLOW_HIGH_CONCURRENCY"),
        allow_huge_fetch: parse_dangerously_flag("WHATSRUST_DANGEROUSLY_ALLOW_HUGE_FETCH"),
    };
    match validate_backfill_safety(&safety, &overrides) {
        Ok(warnings) => {
            for w in warnings {
                // TODO(ADR 0022): surface in status/SSE
                warn!(warning = %w, "backfill safety override active");
            }
        }
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    }
    // M1 runs a single-FIFO backfill worker; multi-worker is deferred to M2.
    if backfill_max_concurrent > 1 {
        warn!(
            n = backfill_max_concurrent,
            "WHATSRUST_BACKFILL_MAX_CONCURRENT > 1 has no effect yet (M1 single-FIFO worker; multi-worker deferred)"
        );
    }

    let config = BridgeConfig {
        db_path: PathBuf::from("whatsapp.db"),
        pair_phone: std::env::var("WHATSAPP_PAIR_PHONE").ok(),
        allowed_numbers: allowed,
        health_port: api_port,
        backup_dir,
        send_burst,
        skip_history_sync,
        backfill_interval_secs,
        backfill_max_concurrent,
        backfill_max_messages,
        backfill_batch_size,
        backfill_backstop,
        backfill_jitter_frac,
        backfill_cooldown_secs,
        backfill_queue_depth,
        ..Default::default()
    };

    // Single-instance guard: prevent two bridges from using the same session
    let _instance_lock = match instance_lock::InstanceLock::acquire(
        &config.db_path,
        Duration::from_secs(2),
    ) {
        Ok(lock) => {
            info!(lock = %lock.path().display(), "session lock acquired");
            lock
        }
        Err(e) => {
            error!(error = %e, "another bridge instance is already running — refusing to start");
            std::process::exit(1);
        }
    };

    let bridge = Arc::new(WhatsAppBridge::start(config, inbound_tx, cancel.clone()));

    info!("bridge started, state: {:?}", bridge.state());
    info!("waiting for WhatsApp connection (scan QR code or enter pair code)...");

    // Spawn API server
    if api_port > 0 {
        let api_bridge = bridge.clone();
        let api_cancel = cancel.clone();
        tokio::spawn(async move {
            api::serve(api_bridge, api_port, api_cancel).await;
        });
    }

    // Print inbound messages
    let bridge_for_rx = bridge.clone();
    tokio::spawn(async move {
        while let Some(msg) = inbound_rx.recv().await {
            let reply_tag = msg
                .reply_to
                .as_ref()
                .map(|r| {
                    let qt = r.quoted_text.as_deref().unwrap_or("");
                    if qt.is_empty() {
                        format!(" (reply to {})", r.stanza_id)
                    } else {
                        let preview = if qt.len() > 40 { &qt[..40] } else { qt };
                        format!(" (reply to {}: \"{}\")", r.stanza_id, preview)
                    }
                })
                .unwrap_or_default();
            let flags_tag = {
                let mut parts = Vec::new();
                if msg.flags.is_forwarded {
                    parts.push(format!("fwd:{}", msg.flags.forwarding_score));
                }
                if msg.flags.is_view_once {
                    parts.push("view-once".to_string());
                }
                if parts.is_empty() { String::new() } else { format!(" [{}]", parts.join(",")) }
            };
            let name_tag = if msg.push_name.is_empty() { String::new() } else { format!(" ~{}", msg.push_name) };
            println!(
                "\n<< [{}] {}{} ({}){}{}: {}",
                msg.jid,
                msg.sender,
                name_tag,
                msg.content.kind(),
                reply_tag,
                flags_tag,
                msg.content.display_text()
            );
            if bridge_for_rx.is_connected() {
                print!("> ");
            }
        }
    });

    // Interactive REPL for testing (stdin)
    let bridge_for_repl = bridge.clone();
    let cancel_for_repl = cancel.clone();
    tokio::spawn(async move {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();

        println!("Commands:");
        println!("  send <jid> <message>           — send text (prints msg ID)");
        println!("  reply <jid> <id> <sender> <msg>— reply quoting a message");
        println!("  edit <jid> <id> <new text>     — edit a sent message");
        println!("  react <jid> <id> <emoji> [from_me] [sender_jid] — react to a message");
        println!("  unreact <jid> <id> [from_me] [sender_jid] — remove a reaction");
        println!("  revoke <jid> <id>              — delete a message for everyone");
        println!("  image <jid> <path>             — send an image file");
        println!("  audio <jid> <path>             — send audio as voice note");
        println!("  video <jid> <path>             — send a video file");
        println!("  doc <jid> <path>               — send a document/file");
        println!("  sticker <jid> <path>           — send a WebP sticker");
        println!("  location <jid> <lat> <lon>     — send a location pin");
        println!("  contact <jid> <name> <phone>   — send a contact card");
        println!("  forward <jid> <msg_id>         — forward a cached message (fwd)");
        println!("  vo-image <jid> <path> [cap]    — send view-once image");
        println!("  vo-video <jid> <path> [cap]    — send view-once video");
        println!("  poll <jid> <N> <Q> | opt | ... — create a poll (N = selectable count)");
        println!("  subscribe <jid>                — subscribe to contact presence");
        println!("  typing <jid>                   — show typing indicator");
        println!("  stop-typing <jid>              — cancel typing indicator");
        println!("  recording <jid>                — show recording indicator");
        println!("  stop-recording <jid>           — cancel recording indicator");
        println!("  status                         — show bridge state");
        println!("  quit                           — shut down");
        println!();

        loop {
            let next = tokio::select! {
                _ = cancel_for_repl.cancelled() => break,
                next = lines.next_line() => next,
            };
            match next {
                Ok(Some(line)) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }

                    let parts: Vec<&str> = line.splitn(3, ' ').collect();
                    match parts[0] {
                        "send" | "s" => {
                            if parts.len() < 3 {
                                println!("usage: send <jid> <message>");
                                continue;
                            }
                            match bridge_for_repl
                                .send_message_with_id(parts[1], parts[2])
                                .await
                            {
                                Ok(id) => println!(">> sent to {} (id: {})", parts[1], id),
                                Err(e) => println!("!! send failed: {e}"),
                            }
                        }
                        "edit" | "e" => {
                            let edit_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if edit_parts.len() < 4 {
                                println!("usage: edit <jid> <msg_id> <new text>");
                                continue;
                            }
                            match bridge_for_repl
                                .edit_message(edit_parts[1], edit_parts[2], edit_parts[3])
                                .await
                            {
                                Ok(()) => println!(">> edited {}", edit_parts[2]),
                                Err(e) => println!("!! edit failed: {e}"),
                            }
                        }
                        "image" | "img" => {
                            if parts.len() < 3 {
                                println!("usage: image <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("png") => "image/png",
                                        Some("gif") => "image/gif",
                                        Some("webp") => "image/webp",
                                        _ => "image/jpeg",
                                    };
                                    match bridge_for_repl
                                        .send_image(parts[1], data, mime, None)
                                        .await
                                    {
                                        Ok(()) => println!(">> image sent to {}", parts[1]),
                                        Err(e) => println!("!! image send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "audio" | "voice" => {
                            if parts.len() < 3 {
                                println!("usage: audio <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("ogg") | Some("opus") => "audio/ogg; codecs=opus",
                                        Some("mp3") => "audio/mpeg",
                                        Some("m4a") | Some("aac") => "audio/mp4",
                                        _ => "audio/ogg; codecs=opus",
                                    };
                                    match bridge_for_repl
                                        .send_audio(parts[1], data, mime, None, true)
                                        .await
                                    {
                                        Ok(()) => println!(">> audio sent to {}", parts[1]),
                                        Err(e) => println!("!! audio send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "video" | "vid" => {
                            if parts.len() < 3 {
                                println!("usage: video <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("webm") => "video/webm",
                                        Some("mov") => "video/quicktime",
                                        Some("3gp") => "video/3gpp",
                                        _ => "video/mp4",
                                    };
                                    match bridge_for_repl
                                        .send_video(parts[1], data, mime, None)
                                        .await
                                    {
                                        Ok(()) => println!(">> video sent to {}", parts[1]),
                                        Err(e) => println!("!! video send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "doc" | "document" => {
                            if parts.len() < 3 {
                                println!("usage: doc <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            let filename = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("file");
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("pdf") => "application/pdf",
                                        Some("zip") => "application/zip",
                                        Some("txt") => "text/plain",
                                        _ => "application/octet-stream",
                                    };
                                    match bridge_for_repl
                                        .send_document(parts[1], data, mime, filename, None)
                                        .await
                                    {
                                        Ok(()) => println!(">> doc sent to {}", parts[1]),
                                        Err(e) => println!("!! doc send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "reply" | "r" => {
                            let reply_parts: Vec<&str> = line.splitn(5, ' ').collect();
                            if reply_parts.len() < 5 {
                                println!("usage: reply <jid> <msg_id> <sender_jid> <message>");
                                continue;
                            }
                            match bridge_for_repl
                                .send_reply(
                                    reply_parts[1],
                                    reply_parts[2],
                                    reply_parts[3],
                                    reply_parts[4],
                                )
                                .await
                            {
                                Ok(id) => println!(">> replied (id: {})", id),
                                Err(e) => println!("!! reply failed: {e}"),
                            }
                        }
                        "react" => {
                            // react <jid> <msg_id> <emoji> [from_me|sender_jid] [sender_jid]
                            let react_parts: Vec<&str> = line.splitn(6, ' ').collect();
                            if react_parts.len() < 4 {
                                println!("usage: react <jid> <msg_id> <emoji> [from_me|sender_jid] [sender_jid]");
                                continue;
                            }
                            let (from_me, sender_jid) = parse_react_target(
                                react_parts.get(4).copied(),
                                react_parts.get(5).copied(),
                            );
                            match bridge_for_repl
                                .send_reaction(
                                    react_parts[1],
                                    react_parts[2],
                                    sender_jid.as_deref(),
                                    react_parts[3],
                                    from_me,
                                )
                                .await
                            {
                                Ok(()) => println!(">> reacted {} (from_me={})", react_parts[3], from_me),
                                Err(e) => println!("!! react failed: {e}"),
                            }
                        }
                        "unreact" => {
                            // unreact <jid> <msg_id> [from_me|sender_jid] [sender_jid]
                            let react_parts: Vec<&str> = line.splitn(5, ' ').collect();
                            if react_parts.len() < 3 {
                                println!("usage: unreact <jid> <msg_id> [from_me|sender_jid] [sender_jid]");
                                continue;
                            }
                            let (from_me, sender_jid) = parse_react_target(
                                react_parts.get(3).copied(),
                                react_parts.get(4).copied(),
                            );
                            match bridge_for_repl
                                .remove_reaction(react_parts[1], react_parts[2], sender_jid.as_deref(), from_me)
                                .await
                            {
                                Ok(()) => println!(">> reaction removed (from_me={})", from_me),
                                Err(e) => println!("!! unreact failed: {e}"),
                            }
                        }
                        "sticker" | "stk" => {
                            if parts.len() < 3 {
                                println!("usage: sticker <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    match bridge_for_repl
                                        .send_sticker(parts[1], data, "image/webp", false)
                                        .await
                                    {
                                        Ok(()) => println!(">> sticker sent to {}", parts[1]),
                                        Err(e) => println!("!! sticker send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "vo-image" => {
                            let vo_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if vo_parts.len() < 3 {
                                println!("usage: vo-image <jid> <path> [caption]");
                                continue;
                            }
                            let path = std::path::Path::new(vo_parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("png") => "image/png",
                                        Some("gif") => "image/gif",
                                        Some("webp") => "image/webp",
                                        _ => "image/jpeg",
                                    };
                                    let caption = vo_parts.get(3).copied();
                                    match bridge_for_repl
                                        .send_view_once_image(vo_parts[1], data, mime, caption)
                                        .await
                                    {
                                        Ok(()) => println!(">> view-once image sent to {}", vo_parts[1]),
                                        Err(e) => println!("!! vo-image failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "vo-video" => {
                            let vo_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if vo_parts.len() < 3 {
                                println!("usage: vo-video <jid> <path> [caption]");
                                continue;
                            }
                            let path = std::path::Path::new(vo_parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("webm") => "video/webm",
                                        Some("mov") => "video/quicktime",
                                        Some("3gp") => "video/3gpp",
                                        _ => "video/mp4",
                                    };
                                    let caption = vo_parts.get(3).copied();
                                    match bridge_for_repl
                                        .send_view_once_video(vo_parts[1], data, mime, caption)
                                        .await
                                    {
                                        Ok(()) => println!(">> view-once video sent to {}", vo_parts[1]),
                                        Err(e) => println!("!! vo-video failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "poll" => {
                            // poll <jid> <count> <question> | <opt1> | <opt2> ...
                            let poll_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if poll_parts.len() < 4 {
                                println!("usage: poll <jid> <count> <question> | opt1 | opt2 ...");
                                continue;
                            }
                            let jid = poll_parts[1];
                            let count: u32 = match poll_parts[2].parse() {
                                Ok(v) => v,
                                Err(_) => {
                                    println!("!! invalid selectable count");
                                    continue;
                                }
                            };
                            let rest = poll_parts[3];
                            let segments: Vec<&str> = rest.split('|').collect();
                            if segments.len() < 2 {
                                println!("!! need at least a question and one option separated by |");
                                continue;
                            }
                            let question = segments[0].trim();
                            let options: Vec<String> = segments[1..]
                                .iter()
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            if options.is_empty() {
                                println!("!! need at least one option");
                                continue;
                            }
                            match bridge_for_repl.send_poll(jid, question, &options, count).await {
                                Ok(id) => println!(">> poll sent to {} (id: {})", jid, id),
                                Err(e) => println!("!! poll failed: {e}"),
                            }
                        }
                        "subscribe" | "sub" => {
                            if parts.len() < 2 {
                                println!("usage: subscribe <jid>");
                                continue;
                            }
                            match bridge_for_repl.subscribe_presence(parts[1]).await {
                                Ok(()) => println!(">> subscribed to presence of {}", parts[1]),
                                Err(e) => println!("!! subscribe failed: {e}"),
                            }
                        }
                        "forward" | "fwd" => {
                            if parts.len() < 3 {
                                println!("usage: forward <jid> <msg_id>");
                                continue;
                            }
                            match bridge_for_repl.forward_message(parts[1], parts[2]).await {
                                Ok(id) => println!(">> forwarded to {} (id: {})", parts[1], id),
                                Err(e) => println!("!! forward failed: {e}"),
                            }
                        }
                        "location" | "loc" => {
                            let loc_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if loc_parts.len() < 4 {
                                println!("usage: location <jid> <lat> <lon>");
                                continue;
                            }
                            let lat: f64 = match loc_parts[2].parse() {
                                Ok(v) => v,
                                Err(_) => {
                                    println!("!! invalid latitude");
                                    continue;
                                }
                            };
                            let lon: f64 = match loc_parts[3].parse() {
                                Ok(v) => v,
                                Err(_) => {
                                    println!("!! invalid longitude");
                                    continue;
                                }
                            };
                            match bridge_for_repl
                                .send_location(loc_parts[1], lat, lon, None, None)
                                .await
                            {
                                Ok(()) => println!(">> location sent to {}", loc_parts[1]),
                                Err(e) => println!("!! location send failed: {e}"),
                            }
                        }
                        "contact" => {
                            let contact_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if contact_parts.len() < 4 {
                                println!("usage: contact <jid> <name> <phone>");
                                continue;
                            }
                            let name = contact_parts[2];
                            let phone = contact_parts[3];
                            let vcard = format!(
                                "BEGIN:VCARD\nVERSION:3.0\nFN:{name}\nTEL;type=CELL:+{phone}\nEND:VCARD"
                            );
                            match bridge_for_repl
                                .send_contact(contact_parts[1], name, &vcard)
                                .await
                            {
                                Ok(()) => println!(">> contact sent to {}", contact_parts[1]),
                                Err(e) => println!("!! contact send failed: {e}"),
                            }
                        }
                        "typing" | "t" => {
                            if parts.len() < 2 {
                                println!("usage: typing <jid>");
                                continue;
                            }
                            if let Err(e) = bridge_for_repl.start_typing(parts[1]).await {
                                println!("!! typing failed: {e}");
                            }
                        }
                        "stop-typing" | "st" => {
                            if parts.len() < 2 {
                                println!("usage: stop-typing <jid>");
                                continue;
                            }
                            if let Err(e) = bridge_for_repl.stop_typing(parts[1]).await {
                                println!("!! stop-typing failed: {e}");
                            }
                        }
                        "recording" | "rec" => {
                            if parts.len() < 2 {
                                println!("usage: recording <jid>");
                                continue;
                            }
                            if let Err(e) = bridge_for_repl.start_recording(parts[1]).await {
                                println!("!! recording failed: {e}");
                            }
                        }
                        "stop-recording" | "sr" => {
                            if parts.len() < 2 {
                                println!("usage: stop-recording <jid>");
                                continue;
                            }
                            if let Err(e) = bridge_for_repl.stop_recording(parts[1]).await {
                                println!("!! stop-recording failed: {e}");
                            }
                        }
                        "edit-test" | "et" => {
                            if parts.len() < 2 {
                                println!("usage: edit-test <jid>");
                                continue;
                            }
                            let jid = parts[1].to_string();
                            println!(">> sending original message...");
                            match bridge_for_repl
                                .send_message_with_id(&jid, "EDIT-TEST: This will change in 3 seconds...")
                                .await
                            {
                                Ok(id) => {
                                    println!(">> sent (id: {}), waiting 3s then editing...", id);
                                    tokio::time::sleep(Duration::from_secs(3)).await;
                                    match bridge_for_repl
                                        .edit_message(&jid, &id, "EDITED: whatsrust edited this!")
                                        .await
                                    {
                                        Ok(()) => println!(">> edit sent for {}", id),
                                        Err(e) => println!("!! edit failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! send failed: {e}"),
                            }
                        }
                        "revoke" | "del" => {
                            if parts.len() < 3 {
                                println!("usage: revoke <jid> <msg_id>");
                                continue;
                            }
                            match bridge_for_repl.revoke_message(parts[1], parts[2]).await {
                                Ok(()) => println!(">> message revoked: {}", parts[2]),
                                Err(e) => println!("!! revoke failed: {e}"),
                            }
                        }
                        "revoke-test" | "rt" => {
                            if parts.len() < 2 {
                                println!("usage: revoke-test <jid>");
                                continue;
                            }
                            let jid = parts[1].to_string();
                            println!(">> sending message to delete in 5s...");
                            match bridge_for_repl
                                .send_message_with_id(&jid, "DELETE-TEST: This will be deleted in 5 seconds...")
                                .await
                            {
                                Ok(id) => {
                                    println!(">> sent (id: {}), waiting 5s then revoking...", id);
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                    match bridge_for_repl.revoke_message(&jid, &id).await {
                                        Ok(()) => println!(">> revoke sent for {}", id),
                                        Err(e) => println!("!! revoke failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! send failed: {e}"),
                            }
                        }
                        "status" => {
                            println!(
                                "state: {:?}, connected: {}",
                                bridge_for_repl.state(),
                                bridge_for_repl.is_connected()
                            );
                        }
                        "groups" => {
                            match bridge_for_repl.get_joined_groups().await {
                                Ok(groups) => {
                                    println!("joined {} groups:", groups.len());
                                    for g in &groups {
                                        println!("  {} — {} ({} members)", g.jid, g.subject, g.participants.len());
                                    }
                                }
                                Err(e) => eprintln!("error: {e}"),
                            }
                        }
                        "group-info" => {
                            if parts.len() < 2 {
                                eprintln!("usage: group-info <group-jid>");
                            } else {
                                let jid = parts[1];
                                match bridge_for_repl.get_group_info(jid).await {
                                    Ok(info) => {
                                        println!("group: {} ({})", info.subject, info.jid);
                                        for p in &info.participants {
                                            let role = if p.is_admin { " [admin]" } else { "" };
                                            let phone = p.phone.as_deref().unwrap_or("?");
                                            println!("  {} ({}){}", p.jid, phone, role);
                                        }
                                    }
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-desc" => {
                            if parts.len() < 2 {
                                eprintln!("usage: group-desc <group-jid> [description]");
                            } else {
                                let jid = parts[1];
                                let desc = if parts.len() > 2 {
                                    Some(parts[2..].join(" "))
                                } else {
                                    None
                                };
                                match bridge_for_repl
                                    .set_group_description(jid, desc.as_deref())
                                    .await
                                {
                                    Ok(()) => println!("description updated"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-add" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-add <group-jid> <phone> [phone...]");
                            } else {
                                let jid = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.add_participants(jid, &phones).await {
                                    Ok(results) => {
                                        for (p, status) in &results {
                                            println!("  {} → {}", p, status.as_deref().unwrap_or("ok"));
                                        }
                                    }
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-remove" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-remove <group-jid> <phone> [phone...]");
                            } else {
                                let jid = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.remove_participants(jid, &phones).await {
                                    Ok(results) => {
                                        for (p, status) in &results {
                                            println!("  {} → {}", p, status.as_deref().unwrap_or("ok"));
                                        }
                                    }
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-promote" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-promote <group-jid> <phone> [phone...]");
                            } else {
                                let jid = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.promote_participants(jid, &phones).await {
                                    Ok(()) => println!("promoted"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-demote" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-demote <group-jid> <phone> [phone...]");
                            } else {
                                let jid = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.demote_participants(jid, &phones).await {
                                    Ok(()) => println!("demoted"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-invite" => {
                            if parts.len() < 2 {
                                eprintln!("usage: group-invite <group-jid>");
                            } else {
                                match bridge_for_repl.get_group_invite_link(parts[1]).await {
                                    Ok(link) => println!("{link}"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-create" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-create <name> <phone> [phone...]");
                            } else {
                                let name = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.create_group(name, &phones).await {
                                    Ok(gid) => println!("created group: {gid}"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-leave" => {
                            if parts.len() < 2 {
                                eprintln!("usage: group-leave <group-jid>");
                            } else {
                                match bridge_for_repl.leave_group(parts[1]).await {
                                    Ok(()) => println!("left group"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-rename" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-rename <group-jid> <new-name>");
                            } else {
                                let jid = parts[1];
                                let name = parts[2..].join(" ");
                                match bridge_for_repl.set_group_subject(jid, &name).await {
                                    Ok(()) => println!("renamed"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "pin-chat" | "unpin-chat" | "mute-chat" | "unmute-chat"
                        | "archive-chat" | "unarchive-chat" | "mark-read" | "mark-unread"
                        | "delete-chat" => {
                            if parts.len() < 2 {
                                eprintln!("usage: {} <jid>", parts[0]);
                            } else {
                                let jid = parts[1];
                                let result = match parts[0] {
                                    "pin-chat" => bridge_for_repl.pin_chat(jid).await,
                                    "unpin-chat" => bridge_for_repl.unpin_chat(jid).await,
                                    "mute-chat" => bridge_for_repl.mute_chat(jid).await,
                                    "unmute-chat" => bridge_for_repl.unmute_chat(jid).await,
                                    "archive-chat" => bridge_for_repl.archive_chat(jid).await,
                                    "unarchive-chat" => bridge_for_repl.unarchive_chat(jid).await,
                                    "mark-read" => bridge_for_repl.mark_chat_as_read(jid).await,
                                    "mark-unread" => bridge_for_repl.mark_chat_as_unread(jid).await,
                                    "delete-chat" => bridge_for_repl.delete_chat(jid).await,
                                    _ => unreachable!(),
                                };
                                match result {
                                    Ok(()) => println!("ok"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "delete-for-me" => {
                            if parts.len() < 3 {
                                eprintln!("usage: delete-for-me <jid> <msg_id> [sender] [from_me]");
                            } else {
                                let sender = parts.get(3).map(|s| *s);
                                let from_me = parts.get(4).map(|v| *v != "false").unwrap_or(true);
                                match bridge_for_repl.delete_message_for_me(parts[1], parts[2], sender, from_me).await {
                                    Ok(()) => println!("ok"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "star" | "unstar" => {
                            if parts.len() < 3 {
                                eprintln!("usage: {} <jid> <msg_id> [sender] [from_me]", parts[0]);
                            } else {
                                let sender = parts.get(3).map(|s| *s);
                                let from_me = parts.get(4).map(|v| *v != "false").unwrap_or(true);
                                let result = if parts[0] == "star" {
                                    bridge_for_repl.star_message(parts[1], parts[2], sender, from_me).await
                                } else {
                                    bridge_for_repl.unstar_message(parts[1], parts[2], sender, from_me).await
                                };
                                match result {
                                    Ok(()) => println!("ok"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "status-text" => {
                            if parts.len() < 3 {
                                println!("usage: status-text <recipients> <text>");
                                continue;
                            }
                            let recipients: Vec<String> = parts[1].split(',').map(|s| s.trim().to_string()).collect();
                            let text = parts[2..].join(" ");
                            match bridge_for_repl
                                .send_status_text(&recipients, &text, 0xFF1E6E4F, 0, None)
                                .await
                            {
                                Ok(id) => println!(">> status posted (id: {id})"),
                                Err(e) => println!("!! status-text failed: {e}"),
                            }
                        }
                        "status-revoke" => {
                            if parts.len() < 3 {
                                println!("usage: status-revoke <recipients> <message_id>");
                                continue;
                            }
                            let recipients: Vec<String> = parts[1].split(',').map(|s| s.trim().to_string()).collect();
                            match bridge_for_repl
                                .revoke_status(&recipients, parts[2], None)
                                .await
                            {
                                Ok(id) => println!(">> status revoked (id: {id})"),
                                Err(e) => println!("!! status-revoke failed: {e}"),
                            }
                        }
                        "quit" | "q" | "exit" => {
                            cancel_for_repl.cancel();
                            break;
                        }
                        _ => {
                            println!("unknown command: {}", parts[0]);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!(error = %e, "stdin read error");
                    break;
                }
            }
        }
    });

    // Wait for Ctrl-C, SIGTERM, or quit command
    let cancel_for_signals = cancel.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
            tokio::select! {
                _ = ctrl_c => info!("SIGINT received"),
                _ = sigterm.recv() => info!("SIGTERM received"),
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.expect("failed to listen for ctrl-c");
            info!("SIGINT received");
        }
        cancel_for_signals.cancel();
    });

    cancel.cancelled().await;
    info!("shutting down...");
    bridge.stop();
    if !bridge.wait_stopped(Duration::from_secs(5)).await {
        warn!("bridge did not fully stop within 5 seconds");
    }

    // Graceful shutdown is complete (bridge drained + state backed up). The
    // REPL spawns a blocking stdin reader thread (tokio::io::stdin), which stays
    // parked in a read() syscall whenever stdin is an open terminal/pipe. The
    // runtime's drop would block forever waiting on that thread, so exit
    // explicitly instead of returning and hanging.
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// Maintenance subcommands: --rollback and --migrate
// ---------------------------------------------------------------------------

/// `whatsrust --rollback [--bak <path>]`
///
/// Acquires the instance lock, restores `whatsapp.db` from a `.bak` file,
/// deletes stale WAL/SHM sidecars, and updates the migration pin to `rolled_back`.
/// EXITS (does not start the daemon).
async fn do_rollback(args: &[String]) -> Result<()> {
    let db_path = PathBuf::from("whatsapp.db");

    // Parse optional --bak <path>
    let bak_path: Option<PathBuf> = {
        let mut found = None;
        let mut i = 2usize; // args[0]=binary, args[1]="--rollback"
        while i < args.len() {
            if args[i] == "--bak" {
                if i + 1 < args.len() {
                    found = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    anyhow::bail!("--bak requires a path argument");
                }
            } else {
                i += 1;
            }
        }
        found
    };

    // Acquire instance lock (prevents concurrent daemon)
    let _lock = instance_lock::InstanceLock::acquire(&db_path, Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("cannot acquire instance lock (daemon running?): {e}"))?;

    // Verify migration pin exists and is in "failed" state
    let pin = storage::read_migration_pin_pub(&db_path);
    match pin.as_ref().and_then(|p| p.get("state")).and_then(|s| s.as_str()) {
        None => anyhow::bail!(
            "--rollback is only valid after a failed migration (no migration-pin found at {}).",
            storage::pin_path_pub(&db_path).display()
        ),
        Some("failed") => { /* proceed */ }
        Some(other) => anyhow::bail!(
            "--rollback is only valid after a FAILED migration, but the pin state is '{other}'. \
             If the DB was already rolled back, start normally or use `whatsrust --migrate` to retry; \
             do not roll back a healthy database. Pin: {}",
            storage::pin_path_pub(&db_path).display()
        ),
    }

    // Find backup file
    let bak = match bak_path {
        Some(p) => {
            if !p.exists() {
                anyhow::bail!("specified backup file does not exist: {}", p.display());
            }
            p
        }
        None => {
            storage::find_newest_bak_pub(&db_path)
                .ok_or_else(|| anyhow::anyhow!(
                    "no pre-migration backup found next to {}; specify one with --bak <path>",
                    db_path.display()
                ))?
        }
    };

    eprintln!("whatsrust --rollback: restoring from {}", bak.display());

    // Copy .bak → whatsapp.db
    std::fs::copy(&bak, &db_path)
        .map_err(|e| anyhow::anyhow!("failed to copy {} → {}: {e}", bak.display(), db_path.display()))?;

    // Delete WAL and SHM sidecars (CRITICAL: stale post-migration WAL replaying onto
    // restored old DB would corrupt it — treat NotFound as success).
    for ext in &["-wal", "-shm"] {
        let sidecar = {
            let mut p = db_path.clone();
            let name = format!(
                "{}{}",
                p.file_name().and_then(|n| n.to_str()).unwrap_or("whatsapp.db"),
                ext
            );
            p.set_file_name(name);
            p
        };
        match std::fs::remove_file(&sidecar) {
            Ok(()) => eprintln!("whatsrust --rollback: deleted {}", sidecar.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("whatsrust --rollback: warning: failed to delete {}: {e}", sidecar.display()),
        }
    }

    // Determine restored version by opening the restored DB
    let restored_version: i64 = {
        let conn = rusqlite::Connection::open(&db_path)
            .map_err(|e| anyhow::anyhow!("cannot open restored DB to read version: {e}"))?;
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| anyhow::anyhow!("cannot read user_version from restored DB: {e}"))?
    };

    // Update pin to state=rolled_back
    storage::write_migration_pin_pub(&db_path, "rolled_back", restored_version)?;

    eprintln!(
        "whatsrust --rollback: DONE. DB restored to v{restored_version} from {}.\n\
         \n\
         WARNING: Any messages received after the failed migration upgrade are LOST.\n\
         Run with the OLD binary (v{restored_version}) to continue, or use `whatsrust --migrate` \n\
         to re-attempt migration with the current binary.",
        bak.display()
    );

    std::process::exit(0);
}

/// `whatsrust --migrate`
///
/// Clears the circuit-breaker pin and starts the daemon normally (force migration retry).
/// This is equivalent to normal daemon startup with `MigrationMode::ForceMigrate`.
async fn do_force_migrate() -> Result<()> {
    let db_path = PathBuf::from("whatsapp.db");

    // Acquire instance lock
    let _lock = instance_lock::InstanceLock::acquire(&db_path, Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("cannot acquire instance lock (daemon running?): {e}"))?;

    eprintln!("whatsrust --migrate: clearing migration circuit-breaker pin and retrying...");

    // open_with_mode with ForceMigrate clears the pin and proceeds normally
    match open_with_mode(&db_path, MigrationMode::ForceMigrate) {
        Ok(_store) => {
            eprintln!("whatsrust --migrate: migration succeeded. Starting daemon normally...");
        }
        Err(e) => {
            eprintln!("whatsrust --migrate: migration failed again: {e}");
            std::process::exit(1);
        }
    }

    // After successful forced migration, the DB is ready — exit (caller restarts daemon normally)
    eprintln!("whatsrust --migrate: done. Restart without --migrate to run as daemon.");
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// CLI client mode
// ---------------------------------------------------------------------------

async fn cli_main(args: &[String]) -> Result<()> {
    let port = get_port();
    let cmd = args[0].as_str();

    match cmd {
        "help" | "--help" | "-h" => {
            print_cli_help();
            Ok(())
        }
        "status" => {
            let (status, body) = api::cli_get(port, "/api/status").await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "qr" => {
            // whatsrust qr [--png /path/to/file.png]
            if args.len() >= 3 && args[1] == "--png" {
                let (status, body) = api::cli_get(port, "/api/qr?format=png").await?;
                if status == 200 {
                    std::fs::write(&args[2], &body)?;
                    println!("{}", json!({"ok": true, "path": args[2]}));
                } else {
                    print_json_result(status, &body)?;
                    std::process::exit(1);
                }
            } else {
                let (status, body) = api::cli_get(port, "/api/qr?format=terminal").await?;
                let text = String::from_utf8_lossy(&body);
                print!("{text}");
                if status >= 400 {
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        "groups" => {
            let (status, body) = api::cli_get(port, "/api/groups").await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "group-info" => {
            require_args(args, 2, "group-info <jid>")?;
            let (status, body) = api::cli_get(port, &format!("/api/group-info?jid={}", args[1])).await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "send" => {
            require_args(args, 3, "send <jid> <text>")?;
            let text = args[2..].join(" ");
            let body = json!({"jid": args[1], "text": text}).to_string();
            let (status, resp) = api::cli_post(port, "/api/send", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "reply" => {
            require_args(args, 5, "reply <jid> <msg_id> <sender_jid> <text>")?;
            let text = args[4..].join(" ");
            let body = json!({"jid": args[1], "id": args[2], "sender": args[3], "text": text}).to_string();
            let (status, resp) = api::cli_post(port, "/api/reply", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "edit" => {
            require_args(args, 4, "edit <jid> <msg_id> <new text>")?;
            let text = args[3..].join(" ");
            let body = json!({"jid": args[1], "id": args[2], "text": text}).to_string();
            let (status, resp) = api::cli_post(port, "/api/edit", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "react" => {
            require_args(args, 4, "react <jid> <msg_id> <emoji> [from_me] [sender_jid]")?;
            let (from_me, sender_jid) = parse_cli_react_args(args)?;
            let body = json!({
                "jid": args[1],
                "id": args[2],
                "emoji": args[3],
                "from_me": from_me,
                "sender_jid": sender_jid,
            })
            .to_string();
            let (status, resp) = api::cli_post(port, "/api/react", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "unreact" => {
            require_args(args, 3, "unreact <jid> <msg_id> [from_me] [sender_jid]")?;
            let (from_me, sender_jid) = parse_cli_react_args_without_emoji(args)?;
            let body = json!({
                "jid": args[1],
                "id": args[2],
                "from_me": from_me,
                "sender_jid": sender_jid,
            })
            .to_string();
            let (status, resp) = api::cli_post(port, "/api/unreact", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "revoke" => {
            require_args(args, 3, "revoke <jid> <msg_id>")?;
            let body = json!({"jid": args[1], "id": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/revoke", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "image" => {
            require_args(args, 3, "image <jid> <path> [caption]")?;
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "path": args[2], "caption": caption}).to_string();
            let (status, resp) = api::cli_post(port, "/api/image", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "video" => {
            require_args(args, 3, "video <jid> <path> [caption]")?;
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "path": args[2], "caption": caption}).to_string();
            let (status, resp) = api::cli_post(port, "/api/video", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "audio" => {
            require_args(args, 3, "audio <jid> <path>")?;
            let body = json!({"jid": args[1], "path": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/audio", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "doc" => {
            require_args(args, 3, "doc <jid> <path>")?;
            let body = json!({"jid": args[1], "path": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/doc", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "sticker" => {
            require_args(args, 3, "sticker <jid> <path>")?;
            let body = json!({"jid": args[1], "path": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/sticker", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "location" => {
            require_args(args, 4, "location <jid> <lat> <lon>")?;
            let body = json!({"jid": args[1], "lat": args[2].parse::<f64>()?, "lon": args[3].parse::<f64>()?}).to_string();
            let (status, resp) = api::cli_post(port, "/api/location", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "contact" => {
            require_args(args, 4, "contact <jid> <name> <phone>")?;
            let body = json!({"jid": args[1], "name": args[2], "phone": args[3]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/contact", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "forward" => {
            require_args(args, 3, "forward <jid> <msg_id>")?;
            let body = json!({"jid": args[1], "msg_id": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/forward", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "poll" => {
            // whatsrust poll <jid> <count> <question> -- <opt1> <opt2> ...
            require_args(args, 4, "poll <jid> <count> <question> -- <opt1> <opt2> ...")?;
            let count: u32 = args[2].parse().map_err(|_| anyhow::anyhow!("invalid selectable count"))?;
            // Find -- separator
            let sep = args.iter().position(|a| a == "--");
            let (question, options) = match sep {
                Some(idx) => {
                    let q = args[3..idx].join(" ");
                    let opts: Vec<String> = args[idx + 1..].iter().map(|s| s.to_string()).collect();
                    (q, opts)
                }
                None => {
                    // No separator: question is args[3], remaining are options
                    let q = args[3].clone();
                    let opts: Vec<String> = args[4..].iter().map(|s| s.to_string()).collect();
                    (q, opts)
                }
            };
            let body = json!({"jid": args[1], "question": question, "options": options, "selectable_count": count}).to_string();
            let (status, resp) = api::cli_post(port, "/api/poll", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "vo-image" | "view-once-image" => {
            require_args(args, 3, "vo-image <jid> <path> [caption]")?;
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "path": args[2], "caption": caption}).to_string();
            let (status, resp) = api::cli_post(port, "/api/view-once-image", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "vo-video" | "view-once-video" => {
            require_args(args, 3, "vo-video <jid> <path> [caption]")?;
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "path": args[2], "caption": caption}).to_string();
            let (status, resp) = api::cli_post(port, "/api/view-once-video", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "typing" => {
            require_args(args, 2, "typing <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/typing", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "stop-typing" => {
            require_args(args, 2, "stop-typing <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/stop-typing", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "recording" => {
            require_args(args, 2, "recording <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/recording", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "stop-recording" => {
            require_args(args, 2, "stop-recording <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/stop-recording", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "subscribe" | "subscribe-presence" => {
            require_args(args, 2, "subscribe <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/subscribe-presence", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-create" => {
            require_args(args, 3, "group-create <name> <participant1> [participant2] ...")?;
            let body = json!({"name": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-create", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-subject" | "group-rename" => {
            require_args(args, 3, "group-subject <jid> <subject>")?;
            let body = json!({"jid": args[1], "subject": args[2..].join(" ")}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-subject", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-desc" | "group-description" => {
            require_args(args, 2, "group-desc <jid> [description]")?;
            let desc = if args.len() > 2 { Some(args[2..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "description": desc}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-description", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-leave" => {
            require_args(args, 2, "group-leave <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-leave", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-invite" | "group-invite-link" => {
            require_args(args, 2, "group-invite <jid>")?;
            let (status, resp) = api::cli_get(port, &format!("/api/group-invite-link?jid={}", args[1])).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-add" => {
            require_args(args, 3, "group-add <jid> <participant1> [participant2] ...")?;
            let body = json!({"jid": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-add", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-remove" => {
            require_args(args, 3, "group-remove <jid> <participant1> [participant2] ...")?;
            let body = json!({"jid": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-remove", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-promote" => {
            require_args(args, 3, "group-promote <jid> <participant1> [participant2] ...")?;
            let body = json!({"jid": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-promote", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-demote" => {
            require_args(args, 3, "group-demote <jid> <participant1> [participant2] ...")?;
            let body = json!({"jid": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-demote", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        // Chat management
        "pin-chat" | "unpin-chat" | "mute-chat" | "unmute-chat"
        | "archive-chat" | "unarchive-chat" | "mark-read" | "mark-unread"
        | "delete-chat" => {
            require_args(args, 2, &format!("{} <jid>", args[0]))?;
            let endpoint = format!("/api/{}", args[0]);
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, &endpoint, &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "delete-for-me" => {
            require_args(args, 3, "delete-for-me <jid> <msg_id> [sender] [from_me]")?;
            let mut payload = json!({"jid": args[1], "id": args[2]});
            if let Some(sender) = args.get(3) { payload["sender"] = json!(sender); }
            if let Some(fm) = args.get(4) { payload["from_me"] = json!(fm != "false"); }
            let (status, resp) = api::cli_post(port, "/api/delete-for-me", &payload.to_string()).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "star" | "unstar" => {
            require_args(args, 3, &format!("{} <jid> <msg_id> [sender] [from_me]", args[0]))?;
            let endpoint = format!("/api/{}", args[0]);
            let mut payload = json!({"jid": args[1], "id": args[2]});
            if let Some(sender) = args.get(3) { payload["sender"] = json!(sender); }
            if let Some(fm) = args.get(4) { payload["from_me"] = json!(fm != "false"); }
            let (status, resp) = api::cli_post(port, &endpoint, &payload.to_string()).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "events" => {
            api::cli_stream_sse(port).await?;
            Ok(())
        }
        "mcp" => {
            // MCP server mode — JSON-RPC over stdin/stdout, proxies to HTTP daemon
            mcp::run_mcp_server(port);
            Ok(())
        }
        "history" => {
            require_args(args, 2, "history <jid> [limit]")?;
            let limit = args.get(2).and_then(|v| v.parse::<i64>().ok()).unwrap_or(20);
            let (status, body) = api::cli_get(port, &format!("/api/history?jid={}&limit={}", args[1], limit)).await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "search" => {
            require_args(args, 2, "search <query> [jid]")?;
            let query = &args[1];
            let jid_param = args.get(2).map(|j| format!("&jid={j}")).unwrap_or_default();
            let (status, body) = api::cli_get(port, &format!("/api/search?q={query}{jid_param}")).await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "fetch-history" => {
            require_args(args, 3, "fetch-history <jid> <mode:all|since|count> [value]")?;
            let mut payload = json!({"chat_jid": args[1], "mode": args[2]});
            if let Some(tv) = args.get(3).and_then(|v| v.parse::<i64>().ok()) {
                payload["target_value"] = json!(tv);
            }
            let (status, resp) = api::cli_post(port, "/api/history-fetch", &payload.to_string()).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "fetch-status" => {
            // No arg → active-only (default); literal "all" → all jobs; integer → specific job.
            let url = match args.get(1).map(|s| s.as_str()) {
                Some("all") => "/api/history-fetch?active=false".to_string(),
                Some(v) => match v.parse::<i64>() {
                    Ok(id) => format!("/api/history-fetch?job_id={id}"),
                    Err(_) => return Err(anyhow::anyhow!("invalid argument: use a job_id integer or 'all'")),
                },
                None => "/api/history-fetch?active=true".to_string(),
            };
            let (status, body) = api::cli_get(port, &url).await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "fetch-cancel" => {
            require_args(args, 2, "fetch-cancel <job_id>")?;
            let job_id = args[1].parse::<i64>()
                .map_err(|_| anyhow::anyhow!("invalid job_id: must be an integer"))?;
            let payload = json!({"job_id": job_id});
            let (status, resp) = api::cli_post(port, "/api/history-fetch/cancel", &payload.to_string()).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "status-text" => {
            require_args(args, 3, "status-text <recipients> <text>")?;
            let body = json!({
                "recipients": args[1].split(',').collect::<Vec<&str>>(),
                "text": args[2..].join(" ")
            }).to_string();
            let (status, resp) = api::cli_post(port, "/api/status-text", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "status-image" => {
            require_args(args, 3, "status-image <recipients> <path> [caption]")?;
            let data = std::fs::read(&args[2])?;
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
            let mime = if args[2].ends_with(".png") { "image/png" } else { "image/jpeg" };
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({
                "recipients": args[1].split(',').collect::<Vec<&str>>(),
                "data": b64,
                "mime": mime,
                "caption": caption
            }).to_string();
            let (status, resp) = api::cli_post(port, "/api/status-image", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "status-video" => {
            require_args(args, 3, "status-video <recipients> <path> [caption]")?;
            let data = std::fs::read(&args[2])?;
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({
                "recipients": args[1].split(',').collect::<Vec<&str>>(),
                "data": b64,
                "mime": "video/mp4",
                "caption": caption
            }).to_string();
            let (status, resp) = api::cli_post(port, "/api/status-video", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "status-revoke" => {
            require_args(args, 3, "status-revoke <recipients> <msg_id>")?;
            let body = json!({
                "recipients": args[1].split(',').collect::<Vec<&str>>(),
                "message_id": args[2]
            }).to_string();
            let (status, resp) = api::cli_post(port, "/api/status-revoke", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        _ => {
            eprintln!("unknown command: {cmd}");
            print_cli_help();
            std::process::exit(1);
        }
    }
}

fn require_args(args: &[String], min: usize, usage: &str) -> Result<()> {
    if args.len() < min {
        anyhow::bail!("usage: whatsrust {usage}");
    }
    Ok(())
}

/// Parse optional [from_me|sender_jid] [sender_jid] into (from_me, sender_jid).
/// If the first optional arg is a sender JID (not a boolean), infers from_me=false.
/// Used by both REPL and CLI reaction commands.
fn parse_react_target(first_opt: Option<&str>, second_opt: Option<&str>) -> (bool, Option<String>) {
    match first_opt {
        None => (true, None),
        Some(v) => match parse_boolish(v) {
            Some(from_me) => (from_me, second_opt.map(|s| s.to_string())),
            None => (false, Some(v.to_string())), // sender JID implies from_me=false
        },
    }
}

fn parse_boolish(value: &str) -> Option<bool> {
    match value {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

fn parse_cli_react_args(args: &[String]) -> Result<(bool, Option<String>)> {
    if args.len() > 6 {
        anyhow::bail!("usage: whatsrust react <jid> <msg_id> <emoji> [from_me|sender_jid] [sender_jid]");
    }
    Ok(parse_react_target(
        args.get(4).map(|s| s.as_str()),
        args.get(5).map(|s| s.as_str()),
    ))
}

fn parse_cli_react_args_without_emoji(args: &[String]) -> Result<(bool, Option<String>)> {
    if args.len() > 5 {
        anyhow::bail!("usage: whatsrust unreact <jid> <msg_id> [from_me|sender_jid] [sender_jid]");
    }
    Ok(parse_react_target(
        args.get(3).map(|s| s.as_str()),
        args.get(4).map(|s| s.as_str()),
    ))
}

/// Print JSON response and return error if `ok` field is false.
fn print_json_result(status: u16, body: &[u8]) -> Result<()> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        // Exit with error if server returned non-200 or ok=false
        if status >= 400 || v.get("ok") == Some(&serde_json::Value::Bool(false)) {
            std::process::exit(1);
        }
    } else {
        println!("{}", String::from_utf8_lossy(body));
        if status >= 400 {
            std::process::exit(1);
        }
    }
    Ok(())
}

fn print_cli_help() {
    println!("whatsrust v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("DAEMON MODE:");
    println!("  whatsrust                              Start bridge with REPL + API server");
    println!();
    println!("CLI MODE (requires running daemon):");
    println!("  whatsrust status                       Bridge state and metrics");
    println!("  whatsrust qr                           Show QR code in terminal");
    println!("  whatsrust qr --png <path>              Save QR as PNG file");
    println!("  whatsrust groups                       List joined groups");
    println!("  whatsrust group-info <jid>             Group details + members");
    println!("  whatsrust send <jid> <text>            Send text message");
    println!("  whatsrust reply <jid> <id> <sender> <text>  Reply to a message");
    println!("  whatsrust edit <jid> <id> <text>       Edit a sent message");
    println!("  whatsrust react <jid> <id> <emoji> [from_me] [sender_jid]  React to a message");
    println!("  whatsrust unreact <jid> <id> [from_me] [sender_jid]  Remove a reaction");
    println!("  whatsrust revoke <jid> <id>            Delete message for everyone");
    println!("  whatsrust image <jid> <path> [caption] Send image");
    println!("  whatsrust video <jid> <path> [caption] Send video");
    println!("  whatsrust audio <jid> <path>           Send voice note");
    println!("  whatsrust doc <jid> <path>             Send document");
    println!("  whatsrust sticker <jid> <path>         Send sticker");
    println!("  whatsrust location <jid> <lat> <lon>   Send location pin");
    println!("  whatsrust contact <jid> <name> <phone> Send contact card");
    println!("  whatsrust forward <jid> <msg_id>       Forward a cached message");
    println!("  whatsrust poll <jid> <N> <Q> -- <opts> Create poll (N=selectable)");
    println!("  whatsrust vo-image <jid> <path> [cap]  Send view-once image");
    println!("  whatsrust vo-video <jid> <path> [cap]  Send view-once video");
    println!("  whatsrust typing <jid>                 Send typing indicator");
    println!("  whatsrust stop-typing <jid>            Clear typing indicator");
    println!("  whatsrust recording <jid>              Send recording indicator");
    println!("  whatsrust stop-recording <jid>         Clear recording indicator");
    println!("  whatsrust subscribe <jid>              Subscribe to presence updates");
    println!("  whatsrust group-create <name> <jid>... Create group with participants");
    println!("  whatsrust group-subject <jid> <subj>   Set group subject/name");
    println!("  whatsrust group-desc <jid> [desc]      Set/clear group description");
    println!("  whatsrust group-leave <jid>            Leave a group");
    println!("  whatsrust group-invite <jid>           Get group invite link");
    println!("  whatsrust group-add <jid> <jid>...     Add participants to group");
    println!("  whatsrust group-remove <jid> <jid>...  Remove participants from group");
    println!("  whatsrust group-promote <jid> <jid>... Promote participants to admin");
    println!("  whatsrust group-demote <jid> <jid>...  Demote admins to regular");
    println!("  whatsrust pin-chat <jid>               Pin a chat");
    println!("  whatsrust unpin-chat <jid>             Unpin a chat");
    println!("  whatsrust mute-chat <jid>              Mute a chat indefinitely");
    println!("  whatsrust unmute-chat <jid>            Unmute a chat");
    println!("  whatsrust archive-chat <jid>           Archive a chat");
    println!("  whatsrust unarchive-chat <jid>         Unarchive a chat");
    println!("  whatsrust mark-read <jid>              Mark chat as read");
    println!("  whatsrust mark-unread <jid>            Mark chat as unread");
    println!("  whatsrust delete-chat <jid>            Delete a chat");
    println!("  whatsrust delete-for-me <jid> <id>     Delete message for me only");
    println!("  whatsrust star <jid> <id>              Star a message");
    println!("  whatsrust unstar <jid> <id>            Unstar a message");
    println!("  whatsrust events                       Stream SSE events (inbound + status)");
    println!("  whatsrust mcp                          MCP server (JSON-RPC over stdio)");
    println!("  whatsrust history <jid> [limit]        Recent messages for a chat");
    println!("  whatsrust search <query> [jid]         Search message history");
    println!("  whatsrust fetch-history <jid> <mode> [value]  Trigger historical backfill (all|since|count)");
    println!("  whatsrust fetch-status [job_id|all]    Backfill job status: active only (default), one by id, or all");
    println!("  whatsrust fetch-cancel <job_id>        Cancel a backfill job");
    println!();
    println!("ENVIRONMENT:");
    println!("  WHATSRUST_PORT   API port (default: 7270, fallback: HEALTH_PORT)");
    println!("  WHATSRUST_BIND   API bind address (default: 127.0.0.1)");
    println!("  WHATSRUST_ALLOW_REMOTE=1  Permit non-loopback API binds");
    println!("  WHATSRUST_API_TOKEN  Optional API bearer token; required for remote binds");
    println!("  WHATSRUST_SEND_BURST     Max burst size before rate limit (default: 5)");
    println!("  WHATSRUST_SKIP_HISTORY_SYNC=1  Skip history sync (leaner pairing, but 1:1 DMs fail with 463; default: off)");
    println!();
    println!("JID FORMAT:");
    println!("  Phone number: 15551234567 or 15551234567@s.whatsapp.net");
    println!("  Group: 120363012345678901@g.us");
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn test_parse_cli_react_args_defaults() {
        let args = vec![
            "react".to_string(),
            "chat".to_string(),
            "msg".to_string(),
            "👍".to_string(),
        ];
        let (from_me, sender_jid) = parse_cli_react_args(&args).unwrap();
        assert!(from_me);
        assert!(sender_jid.is_none());
    }

    #[test]
    fn test_parse_cli_react_args_explicit_sender_implies_not_from_me() {
        let args = vec![
            "react".to_string(),
            "chat".to_string(),
            "msg".to_string(),
            "👍".to_string(),
            "alice@s.whatsapp.net".to_string(),
        ];
        let (from_me, sender_jid) = parse_cli_react_args(&args).unwrap();
        assert!(!from_me);
        assert_eq!(sender_jid.as_deref(), Some("alice@s.whatsapp.net"));
    }

    #[test]
    fn test_parse_cli_react_args_bool_and_sender() {
        let args = vec![
            "react".to_string(),
            "chat".to_string(),
            "msg".to_string(),
            "👍".to_string(),
            "false".to_string(),
            "alice@s.whatsapp.net".to_string(),
        ];
        let (from_me, sender_jid) = parse_cli_react_args(&args).unwrap();
        assert!(!from_me);
        assert_eq!(sender_jid.as_deref(), Some("alice@s.whatsapp.net"));
    }

    #[test]
    fn test_parse_cli_react_args_without_emoji_sender_implies_not_from_me() {
        let args = vec![
            "unreact".to_string(),
            "chat".to_string(),
            "msg".to_string(),
            "alice@s.whatsapp.net".to_string(),
        ];
        let (from_me, sender_jid) = parse_cli_react_args_without_emoji(&args).unwrap();
        assert!(!from_me);
        assert_eq!(sender_jid.as_deref(), Some("alice@s.whatsapp.net"));
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;

    fn all_ok() -> SafetyOverrides {
        SafetyOverrides { allow_fast: false, allow_high_concurrency: false, allow_huge_fetch: false }
    }

    fn all_overridden() -> SafetyOverrides {
        SafetyOverrides { allow_fast: true, allow_high_concurrency: true, allow_huge_fetch: true }
    }

    fn safe_defaults() -> BackfillSafety {
        BackfillSafety { interval_secs: 4, max_concurrent: 1, max_messages: 50_000 }
    }

    // (s-1) All knobs in range → Ok with no warnings
    #[test]
    fn test_safety_in_range_no_warnings() {
        let result = validate_backfill_safety(&safe_defaults(), &all_ok());
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        assert!(result.unwrap().is_empty(), "expected no warnings for safe defaults");
    }

    // (s-2) interval_secs < 3 without override → Err naming the var and the override flag
    #[test]
    fn test_safety_fast_interval_without_override_is_err() {
        let s = BackfillSafety { interval_secs: 2, ..safe_defaults() };
        let result = validate_backfill_safety(&s, &all_ok());
        assert!(result.is_err(), "expected Err for fast interval");
        let msg = result.unwrap_err();
        assert!(msg.contains("WHATSRUST_BACKFILL_INTERVAL_SECS"), "error must name the var: {msg}");
        assert!(msg.contains("WHATSRUST_DANGEROUSLY_ALLOW_FAST_BACKFILL"), "error must name the override flag: {msg}");
    }

    // (s-3) interval_secs < 3 WITH override → Ok with warning
    #[test]
    fn test_safety_fast_interval_with_override_is_ok_with_warning() {
        let s = BackfillSafety { interval_secs: 2, ..safe_defaults() };
        let ov = SafetyOverrides { allow_fast: true, ..all_ok() };
        let result = validate_backfill_safety(&s, &ov);
        assert!(result.is_ok(), "expected Ok when fast override set");
        let warnings = result.unwrap();
        assert!(!warnings.is_empty(), "expected at least one warning");
        assert!(warnings.iter().any(|w| w.contains("fast_backfill_override_active")));
    }

    // (s-4) max_concurrent > 3 without override → Err naming the var and the override flag
    #[test]
    fn test_safety_high_concurrency_without_override_is_err() {
        let s = BackfillSafety { max_concurrent: 4, ..safe_defaults() };
        let result = validate_backfill_safety(&s, &all_ok());
        assert!(result.is_err(), "expected Err for high concurrency");
        let msg = result.unwrap_err();
        assert!(msg.contains("WHATSRUST_BACKFILL_MAX_CONCURRENT"), "error must name the var: {msg}");
        assert!(msg.contains("WHATSRUST_DANGEROUSLY_ALLOW_HIGH_CONCURRENCY"), "error must name the override flag: {msg}");
    }

    // (s-5) max_concurrent > 3 WITH override → Ok with warning
    #[test]
    fn test_safety_high_concurrency_with_override_is_ok_with_warning() {
        let s = BackfillSafety { max_concurrent: 4, ..safe_defaults() };
        let ov = SafetyOverrides { allow_high_concurrency: true, ..all_ok() };
        let result = validate_backfill_safety(&s, &ov);
        assert!(result.is_ok(), "expected Ok when high-concurrency override set");
        let warnings = result.unwrap();
        assert!(warnings.iter().any(|w| w.contains("high_concurrency_override_active")));
    }

    // (s-6) max_messages > 50000 without override → Err naming the var and the override flag
    #[test]
    fn test_safety_huge_fetch_without_override_is_err() {
        let s = BackfillSafety { max_messages: 50_001, ..safe_defaults() };
        let result = validate_backfill_safety(&s, &all_ok());
        assert!(result.is_err(), "expected Err for huge fetch");
        let msg = result.unwrap_err();
        assert!(msg.contains("WHATSRUST_BACKFILL_MAX_MESSAGES"), "error must name the var: {msg}");
        assert!(msg.contains("WHATSRUST_DANGEROUSLY_ALLOW_HUGE_FETCH"), "error must name the override flag: {msg}");
    }

    // (s-7) max_messages > 50000 WITH override → Ok with warning
    #[test]
    fn test_safety_huge_fetch_with_override_is_ok_with_warning() {
        let s = BackfillSafety { max_messages: 100_000, ..safe_defaults() };
        let ov = SafetyOverrides { allow_huge_fetch: true, ..all_ok() };
        let result = validate_backfill_safety(&s, &ov);
        assert!(result.is_ok(), "expected Ok when huge-fetch override set");
        let warnings = result.unwrap();
        assert!(warnings.iter().any(|w| w.contains("huge_fetch_override_active")));
    }

    // (s-8) Multiple violations all overridden → Ok with all three warnings
    #[test]
    fn test_safety_all_violations_all_overridden_produces_three_warnings() {
        let s = BackfillSafety { interval_secs: 1, max_concurrent: 10, max_messages: 100_000 };
        let result = validate_backfill_safety(&s, &all_overridden());
        assert!(result.is_ok(), "expected Ok when all overrides set");
        let warnings = result.unwrap();
        assert_eq!(warnings.len(), 3, "expected three warnings, got: {warnings:?}");
    }

    // (s-9) Boundary values: exactly at limits are safe
    #[test]
    fn test_safety_boundary_values_are_safe() {
        // interval_secs == 3 (floor inclusive) → safe
        let s1 = BackfillSafety { interval_secs: 3, ..safe_defaults() };
        assert!(validate_backfill_safety(&s1, &all_ok()).is_ok());
        // max_concurrent == 3 (ceiling inclusive) → safe
        let s2 = BackfillSafety { max_concurrent: 3, ..safe_defaults() };
        assert!(validate_backfill_safety(&s2, &all_ok()).is_ok());
        // max_messages == 50000 (ceiling inclusive) → safe
        let s3 = BackfillSafety { max_messages: 50_000, ..safe_defaults() };
        assert!(validate_backfill_safety(&s3, &all_ok()).is_ok());
    }

    // (s-10) First violation without override → Err (fast interval, no override on fast,
    // even though other overrides are set — the first failing check stops processing)
    #[test]
    fn test_safety_first_violation_without_override_stops_at_first() {
        let s = BackfillSafety { interval_secs: 1, max_concurrent: 4, max_messages: 100_000 };
        // Only the fast-backfill override is NOT set
        let ov = SafetyOverrides { allow_fast: false, allow_high_concurrency: true, allow_huge_fetch: true };
        let result = validate_backfill_safety(&s, &ov);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        // Should report interval_secs (first check)
        assert!(msg.contains("WHATSRUST_BACKFILL_INTERVAL_SECS"), "should report first violation: {msg}");
    }

    /// (safety-edge) interval_secs = 0 is the most extreme under-floor case and must be rejected
    /// even without any override.
    #[test]
    fn test_safety_zero_interval_is_rejected() {
        let s = BackfillSafety { interval_secs: 0, ..safe_defaults() };
        let result = validate_backfill_safety(&s, &all_ok());
        assert!(result.is_err(), "interval_secs=0 must be rejected");
        let msg = result.unwrap_err();
        assert!(msg.contains("WHATSRUST_BACKFILL_INTERVAL_SECS"), "error must name the knob: {msg}");
    }
}
