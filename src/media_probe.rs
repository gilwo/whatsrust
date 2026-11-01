//! Dev-only live probe for the on-demand media download (design
//! `docs/plans/2026-10-29-on-demand-media-download-design.md` §13, phase 1). For a stored history
//! message it (A) downloads the media from the stored CDN path, (B) asks the owner's own phone to
//! re-upload it, and (C) downloads it again from the new path, printing each step. The result
//! tells which JIDs address a re-upload request correctly and how many old files the phone still
//! holds. Besides the wa-rs request (which at this pin always goes to the account's phone number),
//! it sends a `web` variant built from the pin's public parts, addressed the way WhatsApp Web and
//! upstream wa-rs `main` do it (the account's own LID). Compiled only with the `media-probe` Cargo
//! feature and run from the daemon REPL; the media engine (phase 4) replaces it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use wacore::media_retry::{build_media_retry_receipt, encrypt_media_retry_receipt, parse_media_retry_notification};
use wacore_binary::jid::Jid;
use whatsapp_rust::download::{DownloadParams, MediaType};
use whatsapp_rust::features::{MediaRetryResult, MediaReuploadError, MediaReuploadRequest};
use whatsapp_rust::{Client, NodeFilter};

use crate::bridge::{parse_jid, resolve_alternate_chat_jid};
use crate::storage::{MediaRefRow, ProbeMediaRow, Store};

/// Message ids per `media-probe` command.
const MAX_IDS: usize = 10;
/// Re-upload requests per daemon run (the design's hourly cap, §6.5).
const MAX_REUPLOADS_PER_RUN: u32 = 20;
/// Minimum time between two re-upload requests (the design's re-upload pacing, §6.5).
const REUPLOAD_SPACING: Duration = Duration::from_secs(30);
/// Outer limit on one re-upload request; wa-rs itself waits 30 s for the phone's answer.
const REUPLOAD_TIMEOUT: Duration = Duration::from_secs(40);
/// `web` variant: how long the server has to ACK the request, and the phone to answer it.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Limit on one download (the HTTP client has no timeouts of its own).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// `media-probe list`: size limit of a candidate, and the per-class counts.
const LIST_MAX_BYTES: i64 = 20 * 1024 * 1024;
const LIST_DEFAULT: i64 = 2;
const LIST_MAX: i64 = 5;
/// Longest error text printed for one step.
const MAX_ERROR_CHARS: usize = 160;

/// When the last re-upload request was sent (pacing across commands).
static LAST_REUPLOAD: Mutex<Option<Instant>> = Mutex::new(None);
/// Re-upload requests sent in this daemon run.
static REUPLOADS_SENT: AtomicU32 = AtomicU32::new(0);

/// How a re-upload request is built and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    /// `client.media_reupload()`: the receipt goes to the account's phone number (pin behaviour).
    WaRs,
    /// Built here: the receipt goes to the account's own LID, as WhatsApp Web sends it.
    Web,
}

/// One way to address a re-upload request: the chat JID and, in a group, the sender.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Addressing {
    label: &'static str,
    via: Via,
    chat: String,
    participant: Option<String>,
}

/// What one re-upload attempt returned.
enum Reupload {
    Success(String),
    Failed { code: &'static str, detail: String },
}

/// Entry point of the `media-probe` REPL command. `client` is `None` when the bridge is not
/// connected. `live` holds the media references of the live messages in the bridge's in-memory
/// cache, newest first: live ingest stores no reference, so this is how a fresh file is probed.
/// Prints everything; never fails.
pub async fn run(client: Option<Arc<Client>>, store: &Store, live: &[(String, MediaRefRow)], args: &[&str]) {
    match args {
        [] | ["help"] => print_usage(),
        ["list"] => list(store, live, LIST_DEFAULT).await,
        ["list", n] => match n.parse::<i64>() {
            Ok(n) if (1..=LIST_MAX).contains(&n) => list(store, live, n).await,
            _ => println!("usage: media-probe list [1-{LIST_MAX}]"),
        },
        ids => {
            if let Some(jid) = ids.iter().find(|a| a.contains('@')) {
                println!("!! {jid} is a chat JID; media-probe takes message ids (see `media-probe list`)");
                return;
            }
            if ids.len() > MAX_IDS {
                println!("!! at most {MAX_IDS} message ids per command");
                return;
            }
            let Some(client) = client else {
                println!("!! not connected: wait until `status` shows connected, then retry");
                return;
            };
            let own = |j: Option<Jid>| j.map_or_else(|| "?".to_string(), |j| mask_jid(&j.to_non_ad().to_string()));
            println!("own account: {} / {}", own(client.get_pn()), own(client.get_lid()));
            let dir = match probe_dir() {
                Ok(d) => d,
                Err(e) => {
                    println!("!! cannot create the probe directory: {e}");
                    return;
                }
            };
            for (i, id) in ids.iter().enumerate() {
                probe_one(&client, store, live, &dir, id, i + 1, ids.len()).await;
            }
            println!("files: {} (delete it when done: rm -r '{}')", dir.display(), dir.display());
        }
    }
}

fn print_usage() {
    println!("media-probe list [n]        — probe candidates: per chat class (chat, lid, group) the n newest");
    println!("                              and n oldest stored media messages up to 20 MB (default {LIST_DEFAULT}, max {LIST_MAX})");
    println!("media-probe <id> [<id> ...] — per message (at most {MAX_IDS}): A download from the stored path,");
    println!("                              B ask your phone to re-upload it (stored JIDs, then the PN<->LID");
    println!("                              alternate if that fails), C download from the new path.");
    println!("                              One re-upload request per 30 s, at most {MAX_REUPLOADS_PER_RUN} per daemon run.");
    println!("Live media (sent or received since the daemon started) has no stored reference; the probe");
    println!("takes it from the in-memory message cache (the last 256 messages, lost on restart).");
}

async fn list(store: &Store, live: &[(String, MediaRefRow)], per_class: i64) {
    let rows = match store.probe_media_candidates(per_class, LIST_MAX_BYTES).await {
        Ok(r) => r,
        Err(e) => {
            println!("!! database error: {e}");
            return;
        }
    };
    let now = now_secs();
    println!("class  age  message id                         details");
    if rows.is_empty() {
        println!("(stored history: no media message with a complete reference up to 20 MB)");
    }
    for (class, age, row) in &rows {
        println!("{:<6} {:<4} {:<34} {}", class, age, row.message_id, describe(row, now));
    }
    let shown = usize::try_from(per_class * 2).unwrap_or(usize::MAX);
    if live.is_empty() {
        println!("(live: no media message in memory; send one, then list again)");
    }
    for (id, media) in live.iter().take(shown) {
        match store.probe_media_row(id).await {
            Ok(Some(mut row)) => {
                row.media = media.clone();
                println!("{:<6} {:<4} {:<34} {}", chat_class(&row.chat_jid), "live", id, describe(&row, now));
            }
            _ => println!("{:<6} {:<4} {:<34} (cached, but not in the messages table)", "?", "live", id),
        }
    }
}

async fn probe_one(client: &Client, store: &Store, live: &[(String, MediaRefRow)], dir: &Path, id: &str, n: usize, total: usize) {
    let mut row = match store.probe_media_row(id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            println!("== [{n}/{total}] {id}: unknown message id");
            println!("RESULT id={id} result=unknown_id");
            return;
        }
        Err(e) => {
            println!("!! [{n}/{total}] {id}: database error: {e}");
            return;
        }
    };
    let mut origin = "stored";
    if row.media == MediaRefRow::default() {
        let Some((_, cached)) = live.iter().find(|(cached_id, _)| cached_id == id) else {
            println!("== [{n}/{total}] {id}: no media reference (none stored, not in the live cache)");
            println!("RESULT id={id} result=no_reference");
            return;
        };
        row.media = cached.clone();
        origin = "live";
    }
    let own_ids: Vec<Jid> = [client.get_pn(), client.get_lid()].into_iter().flatten().collect();
    if !row.from_me && is_own_sender(&row.sender_jid, &own_ids) {
        println!("   (from_me set to 1: the sender is this account; live rows always store 0)");
        row.from_me = true;
    }
    let now = now_secs();
    println!("== [{n}/{total}] {id}  ref={origin}  {}", describe(&row, now));
    let result_head = format!(
        "RESULT id={id} ref={origin} class={} kind={} from_me={} age_days={} size={}",
        chat_class(&row.chat_jid),
        row.content_kind,
        u8::from(row.from_me),
        age_days(row.timestamp_secs, now),
        row.media.file_length.unwrap_or(0),
    );
    let Some(media_type) = media_type_for(&row.content_kind) else {
        println!("  not a downloadable media kind");
        println!("{result_head} result=no_media");
        return;
    };
    let (Some(key), Some(path), Some(enc)) =
        (row.media.media_key.as_deref(), row.media.direct_path.as_deref(), row.media.file_enc_sha256.as_deref())
    else {
        println!("  the reference lacks the media key, the path or the encrypted hash");
        println!("{result_head} result=incomplete_reference");
        return;
    };
    let length = row.media.file_length.and_then(|l| u64::try_from(l).ok()).unwrap_or(0);

    let a = download(client, dir, id, "A", path, key, enc, length, media_type).await;
    println!("  A download (stored path): {}", a.1);

    let mut b_codes = Vec::new();
    let mut new_path = None;
    let own_lid = client.get_lid().map(|j| j.to_non_ad());
    for addressing in addressing_variants(&row, store, own_lid.is_some()).await {
        println!("  B re-upload [{}]", describe_addressing(&addressing));
        match reupload(client, &row, key, &addressing, own_lid.as_ref()).await {
            Reupload::Success(p) => {
                println!("    success: the phone re-uploaded the file");
                b_codes.push(format!("{}:success", addressing.label));
                new_path = Some(p);
                break;
            }
            Reupload::Failed { code, detail } => {
                println!("    {code}: {detail}");
                b_codes.push(format!("{}:{code}", addressing.label));
                if code == "capped" {
                    break;
                }
            }
        }
    }

    let c_code = match new_path {
        Some(p) => {
            let c = download(client, dir, id, "C", &p, key, enc, length, media_type).await;
            println!("  C download (new path): {}", c.1);
            c.0
        }
        None => "skipped".to_string(),
    };
    println!("{result_head} A={} B={} C={c_code}", a.0, b_codes.join(","));
}

/// Download into `<dir>/<id>-<step>.bin`. Returns a short code (`ok`, `expired`, `auth`,
/// `failed`, `timeout`) and a line of detail. A failed download leaves no file.
#[allow(clippy::too_many_arguments)]
async fn download(
    client: &Client,
    dir: &Path,
    id: &str,
    step: &str,
    direct_path: &str,
    media_key: &[u8],
    file_enc_sha256: &[u8],
    length: u64,
    media_type: MediaType,
) -> (String, String) {
    if !direct_path_is_safe(direct_path) {
        return ("unsafe_path".to_string(), "refused: the CDN path is not a plain absolute path".to_string());
    }
    let file_path = dir.join(format!("{}-{step}.bin", safe_name(id)));
    let file = match std::fs::File::create(&file_path) {
        Ok(f) => f,
        Err(e) => return ("failed".to_string(), format!("cannot create {}: {e}", file_path.display())),
    };
    // An empty plaintext hash is fine: wa-rs checks it only for unencrypted (newsletter) media.
    let params = DownloadParams::encrypted(direct_path, media_key, &[], file_enc_sha256, length, media_type);
    let started = Instant::now();
    let outcome = tokio::time::timeout(DOWNLOAD_TIMEOUT, client.download_from_params_to_writer(&params, file)).await;
    let ms = started.elapsed().as_millis();
    match outcome {
        Ok(Ok(file)) => {
            drop(file);
            match hash_file(&file_path).await {
                Ok((size, sha)) => {
                    let size_note = if size == length { String::new() } else { format!(" (reference says {length} B)") };
                    ("ok".to_string(), format!("ok, {size} B in {ms} ms, sha256 {sha}…{size_note} -> {}", file_path.display()))
                }
                Err(e) => ("failed".to_string(), format!("downloaded, but cannot read it back: {e}")),
            }
        }
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&file_path);
            let text = format!("{e:#}");
            (download_error_code(&text), format!("failed after {ms} ms: {}", truncate(&text)))
        }
        Err(_) => {
            let _ = std::fs::remove_file(&file_path);
            ("timeout".to_string(), format!("no result in {} s", DOWNLOAD_TIMEOUT.as_secs()))
        }
    }
}

/// Send one re-upload request, after the pacing wait and within the per-run cap.
async fn reupload(client: &Client, row: &ProbeMediaRow, media_key: &[u8], addressing: &Addressing, own_lid: Option<&Jid>) -> Reupload {
    if REUPLOADS_SENT.load(Ordering::Relaxed) >= MAX_REUPLOADS_PER_RUN {
        return Reupload::Failed {
            code: "capped",
            detail: format!("{MAX_REUPLOADS_PER_RUN} re-upload requests already sent in this daemon run; restart it to probe more"),
        };
    }
    let (chat, participant) = match (parse_jid(&addressing.chat), addressing.participant.as_deref().map(parse_jid).transpose()) {
        (Ok(c), Ok(p)) => (c, p),
        (Err(e), _) | (_, Err(e)) => return Reupload::Failed { code: "bad_jid", detail: format!("{e:#}") },
    };
    let wait = pacing_wait(*LAST_REUPLOAD.lock().unwrap_or_else(|p| p.into_inner()), Instant::now());
    if !wait.is_zero() {
        println!("    waiting {} s (one re-upload request per {} s)", wait.as_secs().max(1), REUPLOAD_SPACING.as_secs());
        tokio::time::sleep(wait).await;
    }
    *LAST_REUPLOAD.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    REUPLOADS_SENT.fetch_add(1, Ordering::Relaxed);

    match (addressing.via, own_lid) {
        (Via::WaRs, _) => request_via_wa_rs(client, &row.message_id, &chat, row.from_me, participant.as_ref(), media_key).await,
        (Via::Web, Some(own_lid)) => {
            request_via_web(client, own_lid, &row.message_id, &chat, row.from_me, participant.as_ref(), media_key).await
        }
        (Via::Web, None) => Reupload::Failed { code: "no_own_lid", detail: "this account has no LID yet".to_string() },
    }
}

/// The pin's own request: `client.media_reupload()` (the receipt goes to the phone number).
async fn request_via_wa_rs(
    client: &Client,
    msg_id: &str,
    chat: &Jid,
    from_me: bool,
    participant: Option<&Jid>,
    media_key: &[u8],
) -> Reupload {
    let request = MediaReuploadRequest { msg_id, chat_jid: chat, media_key, is_from_me: from_me, participant };
    let started = Instant::now();
    let outcome = tokio::time::timeout(REUPLOAD_TIMEOUT, client.media_reupload().request(&request)).await;
    match outcome {
        Ok(Ok(result)) => retry_result(result, started.elapsed()),
        Ok(Err(MediaReuploadError::Timeout)) => Reupload::Failed {
            code: "timeout",
            detail: "no answer from the phone in 30 s (offline, asleep, or a wrong address)".to_string(),
        },
        Ok(Err(e)) => Reupload::Failed { code: "error", detail: truncate(&format!("{e:#}")) },
        Err(_) => Reupload::Failed { code: "timeout", detail: format!("no result in {} s", REUPLOAD_TIMEOUT.as_secs()) },
    }
}

/// The `web` request: the same receipt, built with the pin's public parts, but sent to the
/// account's own LID (as WhatsApp Web's `WAWebSendServerErrorReceiptJob` and upstream wa-rs
/// `main` do). It waits for the server's ACK first, so a rejection shows its error code.
async fn request_via_web(
    client: &Client,
    own_lid: &Jid,
    msg_id: &str,
    chat: &Jid,
    from_me: bool,
    participant: Option<&Jid>,
    media_key: &[u8],
) -> Reupload {
    let (ciphertext, iv) = match encrypt_media_retry_receipt(media_key, msg_id) {
        Ok(v) => v,
        Err(e) => return Reupload::Failed { code: "error", detail: truncate(&format!("{e:#}")) },
    };
    // Both waiters exist before the send, so neither answer can be missed.
    let notification = client.wait_for_node(NodeFilter::tag("notification").attr("type", "mediaretry").attr("id", msg_id));
    let ack = client.wait_for_node(NodeFilter::tag("ack").attr("id", msg_id).attr("class", "receipt"));
    let receipt = build_media_retry_receipt(own_lid, msg_id, chat, from_me, participant, &ciphertext, &iv);
    let started = Instant::now();
    if let Err(e) = client.send_node(receipt).await {
        return Reupload::Failed { code: "send_error", detail: truncate(&format!("{e:#}")) };
    }
    let ack_note = match tokio::time::timeout(ACK_TIMEOUT, ack).await {
        Ok(Ok(node)) => match node.get().get_attr("error").map(|v| v.as_str().into_owned()) {
            Some(code) => {
                return Reupload::Failed { code: "rejected", detail: format!("the server rejected the request: error code {code}") };
            }
            None => "the server accepted the request",
        },
        Ok(Err(_)) => "the ACK waiter was cancelled",
        Err(_) => "no ACK from the server in 10 s",
    };
    println!("    {ack_note} ({:.1} s)", started.elapsed().as_secs_f32());
    match tokio::time::timeout(NOTIFICATION_TIMEOUT.saturating_sub(started.elapsed()), notification).await {
        Ok(Ok(node)) => match parse_media_retry_notification(node.get(), media_key) {
            Ok(result) => retry_result(result, started.elapsed()),
            Err(e) => Reupload::Failed { code: "bad_answer", detail: truncate(&format!("{e:#}")) },
        },
        Ok(Err(_)) => Reupload::Failed { code: "error", detail: "the notification waiter was cancelled".to_string() },
        Err(_) => Reupload::Failed {
            code: "timeout",
            detail: format!("no answer from the phone in {} s", NOTIFICATION_TIMEOUT.as_secs()),
        },
    }
}

/// The phone's answer as a probe outcome.
fn retry_result(result: MediaRetryResult, elapsed: Duration) -> Reupload {
    let secs = elapsed.as_secs_f32();
    match result {
        MediaRetryResult::Success { direct_path } => Reupload::Success(direct_path),
        MediaRetryResult::NotFound => Reupload::Failed {
            code: "not_found",
            detail: format!("the phone answered in {secs:.1} s: not found (no file on the phone, or a wrong address)"),
        },
        MediaRetryResult::DecryptionError => Reupload::Failed {
            code: "decryption_error",
            detail: format!("the phone answered in {secs:.1} s: it could not decrypt the request"),
        },
        MediaRetryResult::GeneralError => Reupload::Failed {
            code: "general_error",
            detail: format!("the phone answered in {secs:.1} s: general error"),
        },
    }
}

/// The addressing variants to try for `row`: looks up the PN<->LID alternates, then
/// [`variants_from`].
async fn addressing_variants(row: &ProbeMediaRow, store: &Store, own_lid_known: bool) -> Vec<Addressing> {
    let is_group = row.chat_jid.ends_with("@g.us");
    let participant = is_group.then(|| full_user_jid(&row.sender_jid));
    let alt_chat = if is_group { None } else { resolve_alternate_chat_jid(&row.chat_jid, store).await };
    let alt_participant = match participant.as_deref() {
        Some(p) => resolve_alternate_chat_jid(p, store).await,
        None => None,
    };
    variants_from(&row.chat_jid, participant, alt_chat, alt_participant, own_lid_known)
}

/// The addressing variants, in order: `stored` (the stored JIDs); `alt` (the PN<->LID alternate
/// of the chat or the sender, when one is known and differs); `web` (the LID forms, sent to the
/// account's own LID; only when the account has a LID).
fn variants_from(
    chat: &str,
    participant: Option<String>,
    alt_chat: Option<String>,
    alt_participant: Option<String>,
    own_lid_known: bool,
) -> Vec<Addressing> {
    let stored = Addressing { label: "stored", via: Via::WaRs, chat: chat.to_string(), participant };
    let alt = Addressing {
        label: "alt",
        via: Via::WaRs,
        chat: alt_chat.unwrap_or_else(|| stored.chat.clone()),
        participant: alt_participant.or_else(|| stored.participant.clone()),
    };
    let lid_form = |a: &str, b: &str| if b.ends_with("@lid") { b.to_string() } else { a.to_string() };
    let web = Addressing {
        label: "web",
        via: Via::Web,
        chat: lid_form(&stored.chat, &alt.chat),
        participant: match (stored.participant.as_deref(), alt.participant.as_deref()) {
            (Some(s), Some(a)) => Some(lid_form(s, a)),
            _ => None,
        },
    };
    let mut out = vec![stored.clone()];
    if alt.chat != stored.chat || alt.participant != stored.participant {
        out.push(alt);
    }
    if own_lid_known {
        out.push(web);
    }
    out
}

/// Whether a stored sender is this account (`own` = its phone number and LID). Live rows always
/// store `from_me = 0` (`Store::insert_inbound`), so the probe recognises own messages this way.
fn is_own_sender(sender: &str, own: &[Jid]) -> bool {
    let user = sender.split('@').next().unwrap_or(sender);
    let user = user.split(':').next().unwrap_or(user);
    !user.is_empty() && own.iter().any(|j| j.user.as_str() == user)
}

/// How long to wait before the next re-upload request.
fn pacing_wait(last: Option<Instant>, now: Instant) -> Duration {
    last.map_or(Duration::ZERO, |t| REUPLOAD_SPACING.saturating_sub(now.saturating_duration_since(t)))
}

/// A sender as a full JID: history rows store full JIDs; a bare number is a phone number.
fn full_user_jid(sender: &str) -> String {
    if sender.contains('@') {
        sender.to_string()
    } else {
        format!("{sender}@s.whatsapp.net")
    }
}

/// The wa-rs media type of a stored `content_kind`, or `None` for a kind with no media.
fn media_type_for(kind: &str) -> Option<MediaType> {
    match kind {
        "image" => Some(MediaType::Image),
        "video" => Some(MediaType::Video),
        "audio" => Some(MediaType::Audio),
        "document" => Some(MediaType::Document),
        "sticker" => Some(MediaType::Sticker),
        _ => None,
    }
}

/// The chat class the design's folder layout uses (§5.2).
fn chat_class(chat_jid: &str) -> &'static str {
    match chat_jid.rsplit_once('@').map(|(_, s)| s) {
        Some("s.whatsapp.net") => "chat",
        Some("lid") => "lid",
        Some("g.us") => "group",
        Some("newsletter") => "newsletter",
        _ => "other",
    }
}

/// A short code for a download error text. At this wa-rs pin the ureq client turns every 4xx/5xx
/// into `http status: <n>` (ureq's default `http_status_as_error`), so wa-rs's own
/// "not found/expired" and "Download failed with status" texts are matched as well, but are
/// not expected.
fn download_error_code(text: &str) -> String {
    let status = ["http status: ", "with status: "].iter().find_map(|marker| {
        let (_, rest) = text.split_once(marker)?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u16>().ok()
    });
    match status {
        Some(404 | 410) => "expired".to_string(),
        Some(401 | 403) => "auth".to_string(),
        Some(n) => format!("http_{n}"),
        None => "failed".to_string(),
    }
}

/// A CDN path is safe to fetch only as a plain absolute path: wacore puts it straight after the
/// host name in the URL, so `@`, a backslash, `#`, `//` or a missing leading `/` could change
/// the host.
fn direct_path_is_safe(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && path.chars().all(|c| c.is_ascii_graphic() && !matches!(c, '@' | '\\' | '#'))
}

/// A JID with all but the last 4 characters of its user part hidden: `…4567@s.whatsapp.net`.
fn mask_jid(jid: &str) -> String {
    let (user, server) = jid.split_once('@').map_or((jid, None), |(u, s)| (u, Some(s)));
    let chars: Vec<char> = user.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    match server {
        Some(s) => format!("…{tail}@{s}"),
        None => format!("…{tail}"),
    }
}

/// A message id as a safe file name part (letters, digits, `-`, `_`).
fn safe_name(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn describe(row: &ProbeMediaRow, now: i64) -> String {
    let date = chrono::DateTime::from_timestamp(row.timestamp_secs, 0)
        .map_or_else(|| "?".to_string(), |d| d.format("%Y-%m-%d").to_string());
    format!(
        "{} {}  chat {}  sender {}  from_me={}  {} ({} d)  {} B  {}",
        row.content_kind,
        chat_class(&row.chat_jid),
        mask_jid(&row.chat_jid),
        mask_jid(&row.sender_jid),
        u8::from(row.from_me),
        date,
        age_days(row.timestamp_secs, now),
        row.media.file_length.unwrap_or(0),
        row.media.mimetype.as_deref().unwrap_or("?"),
    )
}

fn describe_addressing(a: &Addressing) -> String {
    let to = match a.via {
        Via::WaRs => "to own phone number",
        Via::Web => "to own LID",
    };
    match a.participant.as_deref() {
        Some(p) => format!("{}: {to}, chat {}, participant {}", a.label, mask_jid(&a.chat), mask_jid(p)),
        None => format!("{}: {to}, chat {}", a.label, mask_jid(&a.chat)),
    }
}

fn age_days(ts: i64, now: i64) -> i64 {
    (now - ts).max(0) / 86_400
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn truncate(text: &str) -> String {
    let one_line = text.replace(['\n', '\r'], " ");
    if one_line.chars().count() <= MAX_ERROR_CHARS {
        one_line
    } else {
        let cut: String = one_line.chars().take(MAX_ERROR_CHARS).collect();
        format!("{cut}…")
    }
}

/// `<temp dir>/whatsrust-media-probe`, private to the user on Unix.
fn probe_dir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join("whatsrust-media-probe");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    Ok(dir)
}

/// The size and the first 6 bytes (hex) of the SHA-256 of a file.
async fn hash_file(path: &Path) -> std::io::Result<(u64, String)> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut size = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
        }
        let digest = hasher.finalize();
        Ok((size, digest.iter().take(6).map(|b| format!("{b:02x}")).collect()))
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn mask_jid_keeps_the_last_four_characters_and_the_server() {
        assert_eq!(mask_jid("972501234567@s.whatsapp.net"), "…4567@s.whatsapp.net");
        assert_eq!(mask_jid("120363000000001234@g.us"), "…1234@g.us");
        assert_eq!(mask_jid("12@lid"), "…12@lid");
        assert_eq!(mask_jid("972501234567"), "…4567");
    }

    #[test]
    fn media_types_cover_the_stored_media_kinds() {
        for kind in ["image", "video", "audio", "document", "sticker"] {
            assert!(media_type_for(kind).is_some(), "{kind}");
        }
        assert!(media_type_for("text").is_none());
    }

    #[test]
    fn chat_classes_follow_the_server() {
        assert_eq!(chat_class("1@s.whatsapp.net"), "chat");
        assert_eq!(chat_class("1@lid"), "lid");
        assert_eq!(chat_class("1@g.us"), "group");
        assert_eq!(chat_class("1@newsletter"), "newsletter");
        assert_eq!(chat_class("status@broadcast"), "other");
    }

    #[test]
    fn a_bare_sender_is_a_phone_number() {
        assert_eq!(full_user_jid("972501234567"), "972501234567@s.whatsapp.net");
        assert_eq!(full_user_jid("1234@lid"), "1234@lid");
    }

    #[test]
    fn download_errors_map_to_short_codes() {
        assert_eq!(download_error_code("http status: 410"), "expired");
        assert_eq!(download_error_code("http status: 403"), "auth");
        assert_eq!(download_error_code("http status: 500"), "http_500");
        assert_eq!(download_error_code("Download media not found/expired with status: 404"), "expired");
        assert_eq!(download_error_code("Download failed with status: 401"), "auth");
        assert_eq!(download_error_code("MAC mismatch"), "failed");
    }

    #[test]
    fn only_plain_absolute_cdn_paths_are_fetched() {
        assert!(direct_path_is_safe("/v/t62.7118-24/12345_n.enc?ccb=11-4&oh=01_AQ&oe=65F0&_nc_sid=5e03e0"));
        for bad in ["v/t62/x", "//evil.example/x", "@evil.example/x", "/a\\b", "/a#b", "/a b", "/a\nb", ""] {
            assert!(!direct_path_is_safe(bad), "{bad:?}");
        }
    }

    #[test]
    fn pacing_waits_out_the_rest_of_the_spacing() {
        let now = Instant::now();
        assert_eq!(pacing_wait(None, now), Duration::ZERO);
        let ten_s_ago = now.checked_sub(Duration::from_secs(10)).expect("monotonic clock past 10 s");
        assert_eq!(pacing_wait(Some(ten_s_ago), now), Duration::from_secs(20));
        let long_ago = now.checked_sub(Duration::from_secs(31)).expect("monotonic clock past 31 s");
        assert_eq!(pacing_wait(Some(long_ago), now), Duration::ZERO);
    }

    #[test]
    fn variants_end_with_the_web_form_when_the_account_has_a_lid() {
        let group = variants_from("g@g.us", Some("111@s.whatsapp.net".into()), None, Some("999@lid".into()), true);
        let got: Vec<(&str, Via, &str, Option<&str>)> =
            group.iter().map(|a| (a.label, a.via, a.chat.as_str(), a.participant.as_deref())).collect();
        assert_eq!(
            got,
            vec![
                ("stored", Via::WaRs, "g@g.us", Some("111@s.whatsapp.net")),
                ("alt", Via::WaRs, "g@g.us", Some("999@lid")),
                ("web", Via::Web, "g@g.us", Some("999@lid")),
            ]
        );

        let dm = variants_from("111@s.whatsapp.net", None, Some("999@lid".into()), None, true);
        assert_eq!(dm.last().map(|a| (a.label, a.chat.as_str(), a.participant.clone())), Some(("web", "999@lid", None)));

        let unmapped = variants_from("111@s.whatsapp.net", None, None, None, true);
        assert_eq!(unmapped.iter().map(|a| a.label).collect::<Vec<_>>(), vec!["stored", "web"], "web keeps the PN chat");
        assert_eq!(variants_from("111@s.whatsapp.net", None, None, None, false).len(), 1, "no own LID: no web");
    }

    #[test]
    fn own_senders_are_recognised_by_number_or_lid() {
        let own = [Jid::from_str("111@s.whatsapp.net").expect("pn"), Jid::from_str("999:11@lid").expect("lid")];
        assert!(is_own_sender("111", &own), "a live row's bare number");
        assert!(is_own_sender("111@s.whatsapp.net", &own));
        assert!(is_own_sender("999@lid", &own));
        assert!(!is_own_sender("222", &own));
        assert!(!is_own_sender("", &own));
    }

    #[test]
    fn safe_names_and_truncation() {
        assert_eq!(safe_name("3EB0A1/../x"), "3EB0A1____x");
        assert_eq!(truncate("a\nb"), "a b");
        assert_eq!(truncate(&"é".repeat(200)).chars().count(), MAX_ERROR_CHARS + 1);
    }
}
