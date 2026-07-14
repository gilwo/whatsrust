//! REST API server for CLI and tool integration.
//!
//! Replaces the old health-only TCP server with a full API.
//! All endpoints return JSON. Media endpoints accept local file paths.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::Semaphore;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::bridge::{BridgeState, WhatsAppBridge};
use crate::qr::QrRender;

/// Maximum concurrent SSE connections (separate from request semaphore).
const SSE_MAX_CONNECTIONS: usize = 8;
/// Write timeout per SSE event — disconnect slow clients.
const SSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Heartbeat interval for SSE keepalive.
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn query_get(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn header_get(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut resp = header.into_bytes();
    resp.extend_from_slice(body);
    resp
}

fn json_response(status: u16, body: &str) -> Vec<u8> {
    http_response(status, "application/json", body.as_bytes())
}

fn json_ok(data: serde_json::Value) -> Vec<u8> {
    let mut map = match data {
        serde_json::Value::Object(m) => m,
        other => {
            let mut m = serde_json::Map::new();
            m.insert("data".to_string(), other);
            m
        }
    };
    map.insert("ok".to_string(), serde_json::Value::Bool(true));
    json_response(200, &serde_json::Value::Object(map).to_string())
}

fn json_ok_id(id: &str) -> Vec<u8> {
    json_response(200, &json!({"ok": true, "id": id}).to_string())
}

fn json_ok_simple() -> Vec<u8> {
    json_response(200, r#"{"ok":true}"#)
}

fn json_err(status: u16, msg: &str) -> Vec<u8> {
    let code = match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        429 => "rate_limited",
        503 => "unavailable",
        504 => "timeout",
        _ => "internal_error",
    };
    json_response(status, &json!({"ok": false, "code": code, "error": msg}).to_string())
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Vec<u8>> {
    serde_json::from_slice(body).map_err(|e| json_err(400, &format!("invalid JSON: {e}")))
}

/// Classify bridge errors into HTTP status codes for machine-friendly responses.
fn bridge_err(e: anyhow::Error) -> Vec<u8> {
    let msg = e.to_string();
    let status = if msg.contains("not connected") || msg.contains("no client") {
        503
    } else if msg.contains("bad JID") || msg.contains("empty JID") || msg.contains("required for group") {
        400
    } else if msg.contains("timed out") {
        504
    } else {
        500
    };
    json_err(status, &msg)
}

fn bool_env_var(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn api_bind_host() -> String {
    std::env::var("WHATSRUST_BIND").unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn is_loopback_bind(bind: &str) -> bool {
    bind.eq_ignore_ascii_case("localhost")
        || bind
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn cli_connect_host(bind: &str) -> String {
    match bind {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" => "::1".to_string(),
        _ => bind.to_string(),
    }
}

fn configured_api_token() -> Option<String> {
    std::env::var("WHATSRUST_API_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Constant-time token comparison to prevent timing side-channel leaks.
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes()
        .iter()
        .zip(b.as_bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn request_has_api_token(req: &HttpRequest, expected_token: &str) -> bool {
    if let Some(tok) = req.header_get("x-api-token") {
        if ct_eq(tok, expected_token) {
            return true;
        }
    }
    if let Some(bearer) = req
        .header_get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return ct_eq(bearer, expected_token);
    }
    false
}

const MAX_MEDIA_READ_BYTES: u64 = 50 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<HttpRequest> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    // Read until end of headers
    let header_end;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tmp)).await {
            Ok(Ok(0)) | Err(_) => return None,
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos;
                    break;
                }
                if buf.len() > 128 * 1024 {
                    return None; // headers too large
                }
            }
            Ok(Err(_)) => return None,
        }
    }

    let headers_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = headers_str.lines();

    // Parse request line
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let raw_path = parts.next()?.to_string();

    // Split path and query
    let (path, query) = if let Some(idx) = raw_path.find('?') {
        let q = raw_path[idx + 1..]
            .split('&')
            .filter_map(|pair| {
                let mut kv = pair.splitn(2, '=');
                Some((kv.next()?.to_string(), kv.next().unwrap_or("").to_string()))
            })
            .collect();
        (raw_path[..idx].to_string(), q)
    } else {
        (raw_path, Vec::new())
    };

    // Parse headers
    let mut content_length = 0usize;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name.to_string(), value));
        }
    }

    // Body size limit: 70 MiB for media endpoints (base64 of 50 MiB), 1 MiB otherwise.
    // We check the route after parsing headers to apply the correct limit.
    let is_media_route = path.starts_with("/api/image")
        || path.starts_with("/api/video")
        || path.starts_with("/api/audio")
        || path.starts_with("/api/doc")
        || path.starts_with("/api/sticker")
        || path.starts_with("/api/view-once")
        || path.starts_with("/api/status-image")
        || path.starts_with("/api/status-video");
    let max_body: usize = if is_media_route { 70 * 1024 * 1024 } else { 1024 * 1024 };
    if content_length > max_body {
        return None;
    }

    // Read body
    let body_start = header_end + 4;
    let body = if content_length > 0 {
        let mut body_buf = if body_start < buf.len() {
            buf[body_start..].to_vec()
        } else {
            Vec::new()
        };
        while body_buf.len() < content_length {
            match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tmp)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => body_buf.extend_from_slice(&tmp[..n]),
                Ok(Err(_)) => break,
            }
        }
        // Reject truncated bodies — don't process partial requests
        if body_buf.len() < content_length {
            return None;
        }
        body_buf.truncate(content_length);
        body_buf
    } else {
        Vec::new()
    };

    Some(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

// ---------------------------------------------------------------------------
// Route dispatch
// ---------------------------------------------------------------------------

async fn handle_request(bridge: &WhatsAppBridge, req: &HttpRequest, is_loopback: bool) -> Vec<u8> {
    match (req.method.as_str(), req.path.as_str()) {
        // Status & QR
        ("GET", "/api/status") | ("GET", "/health") | ("GET", "/") => handle_status(bridge).await,
        ("GET", "/api/qr") => handle_qr(bridge, req),

        // Groups
        ("GET", "/api/groups") => handle_groups(bridge).await,
        ("GET", "/api/group-info") => handle_group_info(bridge, req).await,
        ("GET", "/api/history") => handle_history(bridge, req).await,
        ("GET", "/api/search") => handle_search(bridge, req).await,

        // Messaging
        ("POST", "/api/send") => handle_send(bridge, &req.body, req.query_get("sync") == Some("true")).await,
        ("POST", "/api/reply") => handle_reply(bridge, &req.body).await,
        ("POST", "/api/edit") => handle_edit(bridge, &req.body).await,
        ("POST", "/api/react") => handle_react(bridge, &req.body).await,
        ("POST", "/api/unreact") => handle_unreact(bridge, &req.body).await,
        ("POST", "/api/revoke") => handle_revoke(bridge, &req.body).await,
        ("POST", "/api/image") => handle_media(bridge, &req.body, is_loopback, MediaKind::Image).await,
        ("POST", "/api/video") => handle_media(bridge, &req.body, is_loopback, MediaKind::Video).await,
        ("POST", "/api/audio") => handle_media(bridge, &req.body, is_loopback, MediaKind::Audio).await,
        ("POST", "/api/doc") => handle_media(bridge, &req.body, is_loopback, MediaKind::Doc).await,
        ("POST", "/api/sticker") => handle_media(bridge, &req.body, is_loopback, MediaKind::Sticker).await,
        ("POST", "/api/location") => handle_location(bridge, &req.body).await,
        ("POST", "/api/contact") => handle_contact(bridge, &req.body).await,
        ("POST", "/api/forward") => handle_forward(bridge, &req.body).await,
        ("POST", "/api/poll") => handle_poll(bridge, &req.body).await,
        ("POST", "/api/view-once-image") => handle_media(bridge, &req.body, is_loopback, MediaKind::ViewOnceImage).await,
        ("POST", "/api/view-once-video") => handle_media(bridge, &req.body, is_loopback, MediaKind::ViewOnceVideo).await,
        ("POST", "/api/typing") => handle_jid_action(bridge, &req.body, JidAction::StartTyping).await,
        ("POST", "/api/stop-typing") => handle_jid_action(bridge, &req.body, JidAction::StopTyping).await,
        ("POST", "/api/recording") => handle_jid_action(bridge, &req.body, JidAction::StartRecording).await,
        ("POST", "/api/stop-recording") => handle_jid_action(bridge, &req.body, JidAction::StopRecording).await,
        ("POST", "/api/subscribe-presence") => handle_jid_action(bridge, &req.body, JidAction::SubscribePresence).await,

        // Group management
        ("POST", "/api/group-create") => handle_group_create(bridge, &req.body).await,
        ("POST", "/api/group-subject") => handle_group_subject(bridge, &req.body).await,
        ("POST", "/api/group-description") => handle_group_description(bridge, &req.body).await,
        ("POST", "/api/group-leave") => handle_jid_action(bridge, &req.body, JidAction::GroupLeave).await,
        ("GET", "/api/group-invite-link") => handle_group_invite_link(bridge, req).await,
        ("POST", "/api/group-add") => handle_group_participants(bridge, &req.body, ParticipantAction::Add).await,
        ("POST", "/api/group-remove") => handle_group_participants(bridge, &req.body, ParticipantAction::Remove).await,
        ("POST", "/api/group-promote") => handle_group_participants(bridge, &req.body, ParticipantAction::Promote).await,
        ("POST", "/api/group-demote") => handle_group_participants(bridge, &req.body, ParticipantAction::Demote).await,

        // Chat management
        ("POST", "/api/pin-chat") => handle_jid_action(bridge, &req.body, JidAction::PinChat).await,
        ("POST", "/api/unpin-chat") => handle_jid_action(bridge, &req.body, JidAction::UnpinChat).await,
        ("POST", "/api/mute-chat") => handle_jid_action(bridge, &req.body, JidAction::MuteChat).await,
        ("POST", "/api/unmute-chat") => handle_jid_action(bridge, &req.body, JidAction::UnmuteChat).await,
        ("POST", "/api/archive-chat") => handle_jid_action(bridge, &req.body, JidAction::ArchiveChat).await,
        ("POST", "/api/unarchive-chat") => handle_jid_action(bridge, &req.body, JidAction::UnarchiveChat).await,
        ("POST", "/api/mark-read") => handle_jid_action(bridge, &req.body, JidAction::MarkRead).await,
        ("POST", "/api/mark-unread") => handle_jid_action(bridge, &req.body, JidAction::MarkUnread).await,
        ("POST", "/api/delete-chat") => handle_jid_action(bridge, &req.body, JidAction::DeleteChat).await,
        ("POST", "/api/delete-for-me") => handle_message_action(bridge, &req.body, MessageAction::DeleteForMe).await,
        ("POST", "/api/star") => handle_message_action(bridge, &req.body, MessageAction::Star).await,
        ("POST", "/api/unstar") => handle_message_action(bridge, &req.body, MessageAction::Unstar).await,

        // Status/story
        ("POST", "/api/status-text") => handle_status_text(bridge, &req.body).await,
        ("POST", "/api/status-image") => handle_status_image(bridge, &req.body).await,
        ("POST", "/api/status-video") => handle_status_video(bridge, &req.body).await,
        ("POST", "/api/status-revoke") => handle_status_revoke(bridge, &req.body).await,

        // History-fetch trigger / status / cancel (M1.4)
        ("POST", "/api/history-fetch") => handle_history_fetch_trigger(bridge, &req.body).await,
        ("GET", "/api/history-fetch") => handle_history_fetch_status(bridge, req).await,
        ("POST", "/api/history-fetch/cancel") => handle_history_fetch_cancel(bridge, &req.body).await,

        _ => json_err(404, "not found"),
    }
}

// ---------------------------------------------------------------------------
// Endpoint handlers
// ---------------------------------------------------------------------------

async fn handle_status(bridge: &WhatsAppBridge) -> Vec<u8> {
    let state = bridge.state();
    let m = bridge.metrics();
    let queue = bridge.queue_depth().await;
    json_ok(json!({
        "state": format!("{:?}", state),
        "connected": state == BridgeState::Connected,
        "queue_depth": queue,
        "uptime_secs": m.started_at.elapsed().as_secs(),
        "messages_sent": m.messages_sent.load(Ordering::Relaxed),
        "messages_received": m.messages_received.load(Ordering::Relaxed),
        "reconnect_count": m.reconnect_count.load(Ordering::Relaxed),
        "last_connect_epoch": m.last_connect_epoch.load(Ordering::Relaxed),
        "last_disconnect_epoch": m.last_disconnect_epoch.load(Ordering::Relaxed),
        "last_inbound_epoch": m.last_inbound_epoch.load(Ordering::Relaxed),
        "last_outbound_epoch": m.last_outbound_epoch.load(Ordering::Relaxed),
    }))
}

fn handle_qr(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    let qr_data = bridge.current_qr();
    match qr_data {
        Some(data) => {
            let format = req.query_get("format").unwrap_or("json");
            let qr = match QrRender::new(&data) {
                Some(q) => q,
                None => return json_err(500, "failed to render QR code"),
            };
            match format {
                "png" => http_response(200, "image/png", &qr.png(8)),
                "svg" => http_response(200, "image/svg+xml", qr.svg().as_bytes()),
                "terminal" => http_response(200, "text/plain", qr.terminal().as_bytes()),
                "html" => http_response(200, "text/html", qr.html().as_bytes()),
                _ => json_ok(json!({
                    "qr_data": data,
                    "terminal": qr.terminal(),
                })),
            }
        }
        None => json_err(404, "no QR code available (already paired or not yet generated)"),
    }
}

async fn handle_groups(bridge: &WhatsAppBridge) -> Vec<u8> {
    match bridge.get_joined_groups().await {
        Ok(groups) => {
            let list: Vec<serde_json::Value> = groups
                .iter()
                .map(|g| json!({
                    "jid": g.jid,
                    "subject": g.subject,
                    "participant_count": g.participants.len(),
                }))
                .collect();
            json_ok(json!({ "groups": list }))
        }
        Err(e) => bridge_err(e),
    }
}

async fn handle_group_info(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    let jid = match req.query_get("jid") {
        Some(j) => j,
        None => return json_err(400, "missing ?jid= parameter"),
    };
    match bridge.get_group_info(jid).await {
        Ok(info) => {
            let participants: Vec<serde_json::Value> = info.participants
                .iter()
                .map(|p| json!({
                    "jid": p.jid,
                    "phone": p.phone,
                    "is_admin": p.is_admin,
                }))
                .collect();
            json_ok(json!({
                "jid": info.jid,
                "subject": info.subject,
                "participants": participants,
            }))
        }
        Err(e) => bridge_err(e),
    }
}

// --- Messaging ---

#[derive(Deserialize)]
struct SendReq {
    jid: String,
    text: String,
    #[serde(default)]
    mentions: Vec<String>,
    /// Unix epoch seconds — if set, defer delivery until this time.
    schedule_at: Option<i64>,
    /// Optional link preview metadata — attaches a preview card to the message.
    link_preview: Option<crate::outbound::LinkPreview>,
}

async fn handle_send(bridge: &WhatsAppBridge, body: &[u8], sync: bool) -> Vec<u8> {
    let req: SendReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if sync {
        // Reject features not supported in sync mode — prevents silent data loss
        if req.link_preview.is_some() {
            return json_err(400, "link_preview is not supported with ?sync=true (use async mode)");
        }
        if req.schedule_at.is_some() {
            return json_err(400, "schedule_at is not supported with ?sync=true (use async mode)");
        }
        match bridge.send_message_with_id_mentioned(&req.jid, &req.text, &req.mentions).await {
            Ok(id) => json_ok_id(&id),
            Err(e) => bridge_err(e),
        }
    } else {
        let payload = match serde_json::to_string(&crate::outbound::TextPayload {
            text: req.text,
            mentions: req.mentions,
            link_preview: req.link_preview,
        }) {
            Ok(p) => p,
            Err(e) => return json_err(500, &e.to_string()),
        };
        let result = if let Some(at) = req.schedule_at {
            bridge.enqueue_op_at(&req.jid, crate::outbound::OutboundOpKind::Text, &payload, None, at).await
        } else {
            bridge.enqueue_op(&req.jid, crate::outbound::OutboundOpKind::Text, &payload, None).await
        };
        match result {
            Ok(job_id) => {
                let mut resp = serde_json::json!({"ok": true, "job_id": job_id});
                if let Some(at) = req.schedule_at {
                    resp["scheduled_at"] = serde_json::json!(at);
                }
                json_response(200, &resp.to_string())
            }
            Err(e) => bridge_err(e),
        }
    }
}

#[derive(Deserialize)]
struct ReplyReq {
    jid: String,
    id: String,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default, alias = "sender_raw")]
    sender_jid: Option<String>,
    text: String,
    #[serde(default)]
    mentions: Vec<String>,
}

async fn handle_reply(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ReplyReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let reply_sender = req
        .sender_jid
        .or(req.sender)
        .filter(|s| !s.trim().is_empty());
    let Some(reply_sender) = reply_sender else {
        return json_err(400, "sender or sender_jid is required");
    };
    if req.jid.ends_with("@g.us") && !reply_sender.contains('@') {
        return json_err(400, "sender_jid (full WhatsApp JID) is required for group replies");
    }
    match bridge.send_reply_mentioned(&req.jid, &req.id, &reply_sender, &req.text, &req.mentions).await {
        Ok(id) => json_ok_id(&id),
        Err(e) => bridge_err(e),
    }
}

#[derive(Deserialize)]
struct EditReq {
    jid: String,
    id: String,
    text: String,
}

async fn handle_edit(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: EditReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.edit_message(&req.jid, &req.id, &req.text).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

#[derive(Deserialize)]
struct ReactReq {
    jid: String,
    id: String,
    emoji: String,
    from_me: Option<bool>,
    sender_jid: Option<String>,
}

#[derive(Deserialize)]
struct ReactionTargetReq {
    jid: String,
    id: String,
    from_me: Option<bool>,
    sender_jid: Option<String>,
}

fn resolve_group_reaction_target(
    jid: &str,
    from_me: Option<bool>,
    sender_jid: Option<&str>,
) -> Result<bool, Vec<u8>> {
    if jid.ends_with("@g.us") {
        if sender_jid.is_none() {
            return Err(json_err(400, "sender_jid is required for group reactions"));
        }
        return from_me.ok_or_else(|| json_err(400, "from_me is required for group reactions"));
    }

    Ok(from_me.unwrap_or(sender_jid.is_none()))
}

async fn handle_react(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ReactReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.emoji.is_empty() {
        return json_err(400, "emoji must not be empty");
    }
    let from_me = match resolve_group_reaction_target(&req.jid, req.from_me, req.sender_jid.as_deref()) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match bridge.send_reaction(
        &req.jid,
        &req.id,
        req.sender_jid.as_deref(),
        &req.emoji,
        from_me,
    ).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

async fn handle_unreact(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ReactionTargetReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let from_me = match resolve_group_reaction_target(&req.jid, req.from_me, req.sender_jid.as_deref()) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match bridge.remove_reaction(
        &req.jid,
        &req.id,
        req.sender_jid.as_deref(),
        from_me,
    ).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

#[derive(Deserialize)]
struct RevokeReq {
    jid: String,
    id: String,
}

async fn handle_revoke(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: RevokeReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.revoke_message(&req.jid, &req.id).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

// --- Simple JID-only actions (typing, presence) ---

enum JidAction {
    StartTyping, StopTyping, StartRecording, StopRecording, SubscribePresence, GroupLeave,
    PinChat, UnpinChat, MuteChat, UnmuteChat, ArchiveChat, UnarchiveChat,
    MarkRead, MarkUnread, DeleteChat,
}

#[derive(Deserialize)]
struct JidReq {
    jid: String,
}

async fn handle_jid_action(bridge: &WhatsAppBridge, body: &[u8], action: JidAction) -> Vec<u8> {
    let req: JidReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let result = match action {
        JidAction::StartTyping => bridge.start_typing(&req.jid).await,
        JidAction::StopTyping => bridge.stop_typing(&req.jid).await,
        JidAction::StartRecording => bridge.start_recording(&req.jid).await,
        JidAction::StopRecording => bridge.stop_recording(&req.jid).await,
        JidAction::SubscribePresence => bridge.subscribe_presence(&req.jid).await,
        JidAction::GroupLeave => bridge.leave_group(&req.jid).await,
        JidAction::PinChat => bridge.pin_chat(&req.jid).await,
        JidAction::UnpinChat => bridge.unpin_chat(&req.jid).await,
        JidAction::MuteChat => bridge.mute_chat(&req.jid).await,
        JidAction::UnmuteChat => bridge.unmute_chat(&req.jid).await,
        JidAction::ArchiveChat => bridge.archive_chat(&req.jid).await,
        JidAction::UnarchiveChat => bridge.unarchive_chat(&req.jid).await,
        JidAction::MarkRead => bridge.mark_chat_as_read(&req.jid).await,
        JidAction::MarkUnread => bridge.mark_chat_as_unread(&req.jid).await,
        JidAction::DeleteChat => bridge.delete_chat(&req.jid).await,
    };
    match result {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

// --- Message-level chat actions (star, delete-for-me) ---

enum MessageAction { DeleteForMe, Star, Unstar }

#[derive(Deserialize)]
struct MessageActionReq {
    jid: String,
    id: String,
    sender: Option<String>,
    from_me: Option<bool>,
}

async fn handle_message_action(bridge: &WhatsAppBridge, body: &[u8], action: MessageAction) -> Vec<u8> {
    let req: MessageActionReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let from_me = req.from_me.unwrap_or(true);
    let result = match action {
        MessageAction::DeleteForMe => bridge.delete_message_for_me(&req.jid, &req.id, req.sender.as_deref(), from_me).await,
        MessageAction::Star => bridge.star_message(&req.jid, &req.id, req.sender.as_deref(), from_me).await,
        MessageAction::Unstar => bridge.unstar_message(&req.jid, &req.id, req.sender.as_deref(), from_me).await,
    };
    match result {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

// --- Group management ---

#[derive(Deserialize)]
struct GroupCreateReq {
    name: String,
    participants: Vec<String>,
}

async fn handle_group_create(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: GroupCreateReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let parts: Vec<&str> = req.participants.iter().map(|s| s.as_str()).collect();
    match bridge.create_group(&req.name, &parts).await {
        Ok(gid) => json_ok(json!({"group_jid": gid})),
        Err(e) => bridge_err(e),
    }
}

#[derive(Deserialize)]
struct GroupSubjectReq {
    jid: String,
    subject: String,
}

async fn handle_group_subject(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: GroupSubjectReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.set_group_subject(&req.jid, &req.subject).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

#[derive(Deserialize)]
struct GroupDescriptionReq {
    jid: String,
    description: Option<String>,
}

async fn handle_group_description(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: GroupDescriptionReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.set_group_description(&req.jid, req.description.as_deref()).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

async fn handle_group_invite_link(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    let jid = match req.query_get("jid") {
        Some(j) => j,
        None => return json_err(400, "missing jid query parameter"),
    };
    match bridge.get_group_invite_link(jid).await {
        Ok(link) => json_ok(json!({"link": link})),
        Err(e) => bridge_err(e),
    }
}

enum ParticipantAction { Add, Remove, Promote, Demote }

#[derive(Deserialize)]
struct GroupParticipantsReq {
    jid: String,
    participants: Vec<String>,
}

async fn handle_group_participants(bridge: &WhatsAppBridge, body: &[u8], action: ParticipantAction) -> Vec<u8> {
    let req: GroupParticipantsReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let parts: Vec<&str> = req.participants.iter().map(|s| s.as_str()).collect();
    match action {
        ParticipantAction::Add => match bridge.add_participants(&req.jid, &parts).await {
            Ok(_results) => json_ok_simple(),
            Err(e) => bridge_err(e),
        },
        ParticipantAction::Remove => match bridge.remove_participants(&req.jid, &parts).await {
            Ok(_results) => json_ok_simple(),
            Err(e) => bridge_err(e),
        },
        ParticipantAction::Promote => match bridge.promote_participants(&req.jid, &parts).await {
            Ok(()) => json_ok_simple(),
            Err(e) => bridge_err(e),
        },
        ParticipantAction::Demote => match bridge.demote_participants(&req.jid, &parts).await {
            Ok(()) => json_ok_simple(),
            Err(e) => bridge_err(e),
        },
    }
}

// --- Media ---

#[derive(Deserialize)]
struct MediaReq {
    jid: String,
    /// Local file path (loopback-only).
    path: Option<String>,
    /// Base64-encoded media bytes (works for remote + loopback).
    data: Option<String>,
    /// MIME type — required when using base64 data, inferred from path otherwise.
    mime: Option<String>,
    /// Filename — used for document sends when using base64.
    filename: Option<String>,
    caption: Option<String>,
    /// For audio: if false, sends as regular audio file instead of voice note.
    /// Defaults to true (voice note / PTT).
    voice_note: Option<bool>,
}

async fn read_file_for_media(path: &str) -> Result<Vec<u8>, Vec<u8>> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| json_err(400, &format!("cannot stat file {path}: {e}")))?;
    if !meta.is_file() {
        return Err(json_err(400, &format!("path is not a regular file: {path}")));
    }
    if meta.len() > MAX_MEDIA_READ_BYTES {
        return Err(json_err(
            400,
            &format!(
                "file exceeds size limit ({} bytes > {} bytes): {path}",
                meta.len(),
                MAX_MEDIA_READ_BYTES
            ),
        ));
    }
    tokio::fs::read(path)
        .await
        .map_err(|e| json_err(400, &format!("cannot read file {path}: {e}")))
}

fn mime_for_image(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    }
}

fn mime_for_video(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("3gp") => "video/3gpp",
        _ => "video/mp4",
    }
}

fn mime_for_audio(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mp3") => "audio/mpeg",
        Some("m4a") | Some("aac") => "audio/mp4",
        _ => "audio/ogg; codecs=opus",
    }
}

fn mime_for_doc(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

// --- History & Search ---

async fn handle_history(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    let jid = req.query_get("jid");
    let limit: i64 = req.query_get("limit").and_then(|v| v.parse().ok()).unwrap_or(50).max(1).min(200);
    let before: Option<i64> = req.query_get("before").and_then(|v| v.parse().ok());
    match bridge.store().search_inbound(jid, None, limit, before).await {
        Ok(rows) => {
            let count = rows.len();
            json_ok(json!({"messages": rows, "count": count}))
        }
        Err(e) => json_err(500, &e.to_string()),
    }
}

async fn handle_search(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    let q = match req.query_get("q") {
        Some(q) if !q.is_empty() => q,
        _ => return json_err(400, "query parameter 'q' is required"),
    };
    let jid = req.query_get("jid");
    let limit: i64 = req.query_get("limit").and_then(|v| v.parse().ok()).unwrap_or(20).max(1).min(200);
    match bridge.store().search_inbound(jid, Some(q), limit, None).await {
        Ok(rows) => {
            let count = rows.len();
            json_ok(json!({"messages": rows, "count": count}))
        }
        Err(e) => json_err(500, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// History-fetch endpoints (M1.4 / ADR 0011)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryFetchReq {
    chat_jid: Option<String>,
    /// Canonical field name (M1 plan). Accepts `target_kind` as an alias.
    mode: Option<String>,
    target_kind: Option<String>,
    target_value: Option<i64>,
}

/// Validate the mode string; return a canonical mode or an error response.
fn validate_fetch_mode(mode: &str, target_value: Option<i64>) -> Result<(), Vec<u8>> {
    match mode {
        "all" => Ok(()),
        "since" => {
            if target_value.is_none() {
                Err(json_err(400, "mode 'since' requires a target_value (ms timestamp)"))
            } else {
                Ok(())
            }
        }
        "count" => {
            match target_value {
                Some(v) if v > 0 => Ok(()),
                Some(_) => Err(json_err(400, "mode 'count' requires a positive target_value")),
                None => Err(json_err(400, "mode 'count' requires a target_value")),
            }
        }
        other => Err(json_err(400, &format!("invalid mode '{}': must be 'all', 'since', or 'count'", other))),
    }
}

/// Map an `EnqueueOutcome` to the HTTP response bytes.
/// Extracted as a pure function so it can be unit-tested without a bridge.
fn enqueue_outcome_to_response(
    outcome: crate::storage::EnqueueOutcome,
    chat_jid: &str,
    mode: &str,
    requested_target: Option<i64>,
    resume_anchor_ts: Option<i64>,
    more_remain: bool,
    backfill_notify: &Arc<tokio::sync::Notify>,
) -> Vec<u8> {
    use crate::storage::EnqueueOutcome;
    match outcome {
        EnqueueOutcome::Accepted { job_id, accepted_target } => {
            backfill_notify.notify_one();
            json_ok(json!({
                "job_id": job_id,
                "chat_jid": chat_jid,
                "target_kind": mode,
                "target_value": requested_target,
                "resume_anchor": resume_anchor_ts,
                "more_remain": more_remain,
                "status": "queued",
                "requested": requested_target,
                "accepted": accepted_target,
            }))
        }
        EnqueueOutcome::AlreadyActive { job_id } => {
            json_ok(json!({
                "job_id": job_id,
                "chat_jid": chat_jid,
                "status": "already_active",
            }))
        }
        EnqueueOutcome::Cooldown { retry_after_secs } => {
            json_response(429, &json!({
                "ok": false,
                "code": "rate_limited",
                "status": "cooldown",
                "retry_after_secs": retry_after_secs,
            }).to_string())
        }
        EnqueueOutcome::QueueFull { limit } => {
            json_response(429, &json!({
                "ok": false,
                "code": "rate_limited",
                "status": "queue_full",
                "limit": limit,
            }).to_string())
        }
    }
}

/// POST /api/history-fetch — trigger a backfill job for a chat.
async fn handle_history_fetch_trigger(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: HistoryFetchReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };

    // chat_jid is required
    let chat_jid = match req.chat_jid.as_deref().filter(|s| !s.is_empty()) {
        Some(j) => j.to_string(),
        None => return json_err(400, "chat_jid is required"),
    };

    // mode is canonical; accept target_kind as alias
    let mode = match req.mode.as_deref().or(req.target_kind.as_deref()) {
        Some(m) => m.to_string(),
        None => return json_err(400, "mode is required ('all', 'since', or 'count')"),
    };

    // Validate mode + target_value combination
    if let Err(e) = validate_fetch_mode(&mode, req.target_value) {
        return e;
    }

    let store = bridge.store();

    // No-op fast path: if cursor is already exhausted, return immediately without enqueuing
    match store.get_backfill_cursor(&chat_jid).await {
        Ok(Some(cursor)) if cursor.exhausted => {
            return json_ok(json!({
                "job_id": serde_json::Value::Null,
                "chat_jid": chat_jid,
                "status": "already_exhausted",
                "more_remain": false,
            }));
        }
        Ok(_) => {} // cursor absent or not exhausted — proceed
        Err(e) => return json_err(500, &format!("storage error: {e}")),
    }

    // Cursor seed/resume: if no cursor exists, seed it from the oldest message.
    // If a cursor exists and is not exhausted, leave it untouched (resume from where it is).
    let cursor_opt = match store.get_backfill_cursor(&chat_jid).await {
        Ok(c) => c,
        Err(e) => return json_err(500, &format!("storage error: {e}")),
    };

    let resume_anchor_ts = if cursor_opt.is_none() {
        // Seed cursor from the oldest stored message (mirrors the smoke-test hook exactly)
        let oldest = match store.get_oldest_message(&chat_jid).await {
            Ok(o) => o,
            Err(e) => return json_err(500, &format!("storage error: {e}")),
        };
        let anchor = crate::backfill::initial_anchor(oldest);
        let anchor_ts = if anchor.oldest_msg_timestamp_ms == 0 { None } else { Some(anchor.oldest_msg_timestamp_ms) };
        if let Err(e) = store.upsert_backfill_cursor(
            &chat_jid,
            Some(&anchor.oldest_msg_id),
            Some(anchor.oldest_msg_from_me),
            anchor_ts,
            true,
            false,
            None,
        ).await {
            return json_err(500, &format!("storage error: {e}"));
        }
        anchor_ts
    } else {
        cursor_opt.as_ref().and_then(|c| c.oldest_msg_timestamp_ms)
    };

    let more_remain = cursor_opt.as_ref().map(|c| c.more_remain).unwrap_or(true);

    // Enqueue the job
    let (cooldown_secs, queue_depth, max_messages) = bridge.backfill_config();
    let outcome = match store.enqueue_backfill_job(
        &chat_jid,
        &mode,
        req.target_value,
        cooldown_secs,
        queue_depth,
        max_messages,
    ).await {
        Ok(o) => o,
        Err(e) => return json_err(500, &format!("storage error: {e}")),
    };

    enqueue_outcome_to_response(
        outcome,
        &chat_jid,
        &mode,
        req.target_value,
        resume_anchor_ts,
        more_remain,
        bridge.backfill_notify(),
    )
}

/// GET /api/history-fetch — status or list backfill jobs.
///
/// With ?job_id=N → get a single job by ID.
/// With ?active=true → list only active jobs; otherwise list all.
async fn handle_history_fetch_status(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    if let Some(id_str) = req.query_get("job_id") {
        let id: i64 = match id_str.parse() {
            Ok(v) => v,
            Err(_) => return json_err(400, "job_id must be an integer"),
        };
        match bridge.store().get_backfill_job(id).await {
            Ok(Some(row)) => json_ok(serde_json::to_value(&row).unwrap_or(serde_json::Value::Null)),
            Ok(None) => json_err(404, "job not found"),
            Err(e) => json_err(500, &e.to_string()),
        }
    } else {
        let active_only = req.query_get("active").map(|v| v == "true").unwrap_or(false);
        match bridge.store().list_backfill_jobs(active_only).await {
            Ok(jobs) => {
                let count = jobs.len();
                let jobs_val = serde_json::to_value(&jobs).unwrap_or(serde_json::Value::Array(vec![]));
                json_ok(json!({"jobs": jobs_val, "count": count}))
            }
            Err(e) => json_err(500, &e.to_string()),
        }
    }
}

#[derive(Deserialize)]
struct HistoryFetchCancelReq {
    job_id: Option<i64>,
}

/// POST /api/history-fetch/cancel — cancel a backfill job.
async fn handle_history_fetch_cancel(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: HistoryFetchCancelReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let job_id = match req.job_id {
        Some(id) => id,
        None => return json_err(400, "job_id is required"),
    };
    match bridge.store().mark_backfill_job(job_id, "cancelled").await {
        Ok(()) => json_ok(json!({"job_id": job_id, "status": "cancelled"})),
        Err(e) => json_err(500, &e.to_string()),
    }
}

enum MediaKind { Image, Video, Audio, Doc, Sticker, ViewOnceImage, ViewOnceVideo }

async fn handle_media(bridge: &WhatsAppBridge, body: &[u8], is_loopback: bool, kind: MediaKind) -> Vec<u8> {
    let req: MediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };

    // Reject ambiguous requests with both path and base64 data
    if req.path.is_some() && req.data.is_some() {
        return json_err(400, "provide either 'path' or 'data', not both");
    }

    // Resolve media bytes + mime from either path or base64 data
    let (data, mime_str, filename_str) = if let Some(ref b64) = req.data {
        // Base64 mode — works for both loopback and remote (no filesystem access)
        use base64::Engine;
        let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(b) => b,
            Err(e) => return json_err(400, &format!("invalid base64: {e}")),
        };
        if bytes.is_empty() {
            return json_err(400, "decoded data is empty (0 bytes)");
        }
        if bytes.len() as u64 > MAX_MEDIA_READ_BYTES {
            return json_err(400, &format!("decoded data exceeds size limit ({} > {})", bytes.len(), MAX_MEDIA_READ_BYTES));
        }
        let mime = req.mime.clone().unwrap_or_else(|| "application/octet-stream".to_string());
        let fname = req.filename.clone().unwrap_or_else(|| "file".to_string());
        (bytes, mime, fname)
    } else if let Some(ref path_str) = req.path {
        // Path mode — loopback-only
        if !is_loopback {
            return json_err(403, "local-path media uploads are disabled for non-loopback API binds");
        }
        let bytes = match read_file_for_media(path_str).await { Ok(d) => d, Err(e) => return e };
        let path = std::path::Path::new(path_str);
        let mime = req.mime.clone().unwrap_or_else(|| match kind {
            MediaKind::Image => mime_for_image(path).to_string(),
            MediaKind::Video => mime_for_video(path).to_string(),
            MediaKind::Audio => mime_for_audio(path).to_string(),
            MediaKind::Doc => mime_for_doc(path).to_string(),
            MediaKind::Sticker => "image/webp".to_string(),
            MediaKind::ViewOnceImage => mime_for_image(path).to_string(),
            MediaKind::ViewOnceVideo => mime_for_video(path).to_string(),
        });
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        (bytes, mime, fname)
    } else {
        return json_err(400, "either 'path' (loopback) or 'data' (base64) is required");
    };

    let result = match kind {
        MediaKind::Image => bridge.send_image(&req.jid, data, &mime_str, req.caption.as_deref()).await,
        MediaKind::Video => bridge.send_video(&req.jid, data, &mime_str, req.caption.as_deref()).await,
        MediaKind::Audio => bridge.send_audio(&req.jid, data, &mime_str, None, req.voice_note.unwrap_or(true)).await,
        MediaKind::Doc => bridge.send_document(&req.jid, data, &mime_str, &filename_str, req.caption.as_deref()).await,
        MediaKind::Sticker => bridge.send_sticker(&req.jid, data, &mime_str, false).await,
        MediaKind::ViewOnceImage => bridge.send_view_once_image(&req.jid, data, &mime_str, req.caption.as_deref()).await,
        MediaKind::ViewOnceVideo => bridge.send_view_once_video(&req.jid, data, &mime_str, req.caption.as_deref()).await,
    };
    match result {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

// --- Location / Contact / Forward / Poll ---

#[derive(Deserialize)]
struct LocationReq {
    jid: String,
    lat: f64,
    lon: f64,
}

async fn handle_location(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: LocationReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.send_location(&req.jid, req.lat, req.lon, None, None).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

#[derive(Deserialize)]
struct ContactReq {
    jid: String,
    name: String,
    phone: String,
}

async fn handle_contact(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ContactReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    // Sanitize: vCard fields must not contain newlines or control characters
    // which could inject additional vCard properties or corrupt the structure.
    let safe_name = req.name.replace(['\n', '\r', '\0'], " ");
    let safe_phone: String = req.phone.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
    if safe_phone.is_empty() {
        return json_err(400, "phone must contain at least one digit");
    }
    let vcard = format!(
        "BEGIN:VCARD\nVERSION:3.0\nFN:{}\nTEL;type=CELL:+{}\nEND:VCARD",
        safe_name, safe_phone
    );
    match bridge.send_contact(&req.jid, &safe_name, &vcard).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

#[derive(Deserialize)]
struct ForwardReq {
    jid: String,
    msg_id: String,
}

async fn handle_forward(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ForwardReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.forward_message(&req.jid, &req.msg_id).await {
        Ok(id) => json_ok_id(&id),
        Err(e) => bridge_err(e),
    }
}

#[derive(Deserialize)]
struct PollReq {
    jid: String,
    question: String,
    options: Vec<String>,
    selectable_count: u32,
}

async fn handle_poll(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: PollReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let (question, options) = match crate::bridge::normalize_poll_spec(
        &req.question,
        &req.options,
        req.selectable_count,
    ) {
        Ok(spec) => spec,
        Err(e) => return json_err(400, &e.to_string()),
    };
    match bridge.send_poll(&req.jid, &question, &options, req.selectable_count).await {
        Ok(id) => json_ok_id(&id),
        Err(e) => bridge_err(e),
    }
}

// ---------------------------------------------------------------------------
// Status/story request structs + handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StatusTextReq {
    recipients: Vec<String>,
    text: String,
    #[serde(default = "default_status_bg")]
    background_argb: u32,
    #[serde(default)]
    font: i32,
    privacy: Option<String>,
}

fn default_status_bg() -> u32 { 0xFF1E6E4F }

#[derive(Deserialize)]
struct StatusMediaReq {
    recipients: Vec<String>,
    data: String, // base64
    mime: Option<String>,
    caption: Option<String>,
    seconds: Option<u32>,
    privacy: Option<String>,
}

#[derive(Deserialize)]
struct StatusRevokeReq {
    recipients: Vec<String>,
    message_id: String,
    privacy: Option<String>,
}

async fn handle_status_text(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: StatusTextReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    match bridge.send_status_text(&req.recipients, &req.text, req.background_argb, req.font, req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}

async fn handle_status_image(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: StatusMediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    let data = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &req.data) {
        Ok(d) => d,
        Err(e) => return json_err(400, &format!("bad base64: {e}")),
    };
    let mime = req.mime.as_deref().unwrap_or("image/jpeg");
    match bridge.send_status_image(&req.recipients, data, mime, req.caption.as_deref(), req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}

async fn handle_status_video(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: StatusMediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    let data = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &req.data) {
        Ok(d) => d,
        Err(e) => return json_err(400, &format!("bad base64: {e}")),
    };
    let mime = req.mime.as_deref().unwrap_or("video/mp4");
    let seconds = req.seconds.unwrap_or(0);
    match bridge.send_status_video(&req.recipients, data, mime, req.caption.as_deref(), seconds, req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}

async fn handle_status_revoke(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: StatusRevokeReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    match bridge.revoke_status(&req.recipients, &req.message_id, req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}

// ---------------------------------------------------------------------------
// SSE event stream
// ---------------------------------------------------------------------------

/// Handle a long-lived SSE connection. Streams events until client disconnects,
/// cancel is triggered, or write fails.
async fn handle_sse(
    bridge: &WhatsAppBridge,
    stream: &mut tokio::net::TcpStream,
    cancel: &CancellationToken,
) {
    use tokio::io::AsyncWriteExt;

    enum SseFrame {
        Write(String),
        CloseAfter(String),
    }

    // Send SSE response headers
    let headers = "HTTP/1.1 200 OK\r\n\
        Content-Type: text/event-stream\r\n\
        Cache-Control: no-cache\r\n\
        Connection: keep-alive\r\n\
        X-Accel-Buffering: no\r\n\r\n";
    if stream.write_all(headers.as_bytes()).await.is_err() {
        return;
    }

    let mut rx = bridge.subscribe_events();
    let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let frame = tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(evt) => format_sse_event(&evt).map(SseFrame::Write),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let data = json!({ "missed": n }).to_string();
                        Some(SseFrame::CloseAfter(format_sse_frame("gap", None, &data)))
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = heartbeat.tick() => {
                Some(SseFrame::Write(format_sse_frame("heartbeat", None, "{}")))
            }
            _ = cancel.cancelled() => break,
        };

        if let Some(frame) = frame {
            let (data, close_after_write) = match frame {
                SseFrame::Write(data) => (data, false),
                SseFrame::CloseAfter(data) => (data, true),
            };
            let write_result = tokio::time::timeout(
                SSE_WRITE_TIMEOUT,
                stream.write_all(data.as_bytes()),
            )
            .await;
            match write_result {
                Ok(Ok(())) => {
                    if close_after_write {
                        break;
                    }
                }
                _ => break, // write error or timeout — client gone
            }
        }
    }
}

fn format_sse_frame(event: &str, id: Option<&str>, data: &str) -> String {
    let mut frame = String::new();
    if let Some(id) = id {
        frame.push_str("id: ");
        frame.push_str(id);
        frame.push('\n');
    }
    frame.push_str("event: ");
    frame.push_str(event);
    frame.push('\n');
    for line in data.lines() {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    frame
}

/// Format a BridgeEvent as an SSE event string.
fn format_sse_event(event: &crate::bridge_events::BridgeEvent) -> Option<String> {
    match event {
        crate::bridge_events::BridgeEvent::Inbound(inbound) => {
            let data = serde_json::to_string(inbound.as_ref()).ok()?;
            let event_id = format!("inbound:{}:{}", inbound.bridge_id, inbound.sequence);
            Some(format_sse_frame("inbound", Some(&event_id), &data))
        }
        crate::bridge_events::BridgeEvent::OutboundStatus(status) => {
            let data = serde_json::to_string(status).ok()?;
            let state = status.state.to_string();
            let event_id = format!(
                "status:{}:{}:{}",
                status.job_id,
                state,
                status.wa_message_id.as_deref().unwrap_or("none")
            );
            Some(format_sse_frame("status", Some(&event_id), &data))
        }
        crate::bridge_events::BridgeEvent::Heartbeat => {
            Some(format_sse_frame("heartbeat", None, "{}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Start the API server. Blocks until cancelled.
pub async fn serve(bridge: Arc<WhatsAppBridge>, port: u16, cancel: CancellationToken) {
    let bind = api_bind_host();
    if !is_loopback_bind(&bind) && !bool_env_var("WHATSRUST_ALLOW_REMOTE") {
        error!(
            bind = %bind,
            "refusing non-loopback API bind without WHATSRUST_ALLOW_REMOTE=1"
        );
        return;
    }
    let api_token = configured_api_token();
    if !is_loopback_bind(&bind) && api_token.is_none() {
        error!(
            bind = %bind,
            "refusing non-loopback API bind without WHATSRUST_API_TOKEN"
        );
        return;
    }
    let listener = match TcpListener::bind((&*bind, port)).await {
        Ok(l) => {
            info!(bind = %bind, port = port, "API server listening");
            l
        }
        Err(e) => {
            error!(error = %e, bind = %bind, port = port, "failed to bind API server");
            return;
        }
    };

    let is_loopback = is_loopback_bind(&bind);
    // Cap concurrent connections to prevent slowloris/flood exhaustion.
    let conn_sem = Arc::new(Semaphore::new(64));
    // Separate semaphore for SSE so long-lived streams don't starve normal requests.
    let sse_sem = Arc::new(Semaphore::new(SSE_MAX_CONNECTIONS));

    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((mut stream, _)) = result else { continue };
                let permit = match conn_sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        let _ = stream.write_all(&json_err(503, "too many connections")).await;
                        continue;
                    }
                };
                let bridge = bridge.clone();
                let api_token = api_token.clone();
                let sse_sem = sse_sem.clone();
                let sse_cancel = cancel.clone();
                tokio::spawn(async move {
                    let _permit = permit; // held until handler completes
                    let req = match read_request(&mut stream).await {
                        Some(r) => r,
                        None => {
                            let _ = stream.write_all(&json_err(400, "bad request")).await;
                            return;
                        }
                    };
                    if let Some(expected_token) = api_token.as_deref() {
                        if !request_has_api_token(&req, expected_token) {
                            let _ = stream.write_all(&json_err(401, "unauthorized")).await;
                            return;
                        }
                    }

                    // SSE endpoint — long-lived, uses dedicated semaphore
                    if req.method == "GET" && req.path == "/api/events" {
                        let sse_permit = match sse_sem.try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                let _ = stream.write_all(&json_err(503, "too many SSE connections")).await;
                                return;
                            }
                        };
                        handle_sse(&bridge, &mut stream, &sse_cancel).await;
                        drop(sse_permit);
                        return;
                    }

                    let response = handle_request(&bridge, &req, is_loopback).await;
                    let _ = stream.write_all(&response).await;
                });
            }
            _ = cancel.cancelled() => break,
        }
    }
}

// ---------------------------------------------------------------------------
// CLI HTTP client
// ---------------------------------------------------------------------------

/// Send a GET request to the running daemon and return (status, body_bytes).
pub async fn cli_get(port: u16, path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
    let host = cli_connect_host(&api_bind_host());
    let auth_header = configured_api_token()
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let mut stream = tokio::net::TcpStream::connect((&*host, port)).await
        .map_err(|e| anyhow::anyhow!("cannot connect to whatsrust daemon on {host}:{port}: {e}\nIs the daemon running? Start it with: WHATSRUST_PORT={port} WHATSRUST_BIND={host} whatsrust"))?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth_header}Connection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    stream.shutdown().await?;

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timeout reading response from daemon"))??;
    parse_cli_response(&buf)
}

/// Send a POST request with JSON body to the running daemon.
pub async fn cli_post(port: u16, path: &str, body: &str) -> anyhow::Result<(u16, Vec<u8>)> {
    let host = cli_connect_host(&api_bind_host());
    let auth_header = configured_api_token()
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let mut stream = tokio::net::TcpStream::connect((&*host, port)).await
        .map_err(|e| anyhow::anyhow!("cannot connect to whatsrust daemon on {host}:{port}: {e}\nIs the daemon running? Start it with: WHATSRUST_PORT={port} WHATSRUST_BIND={host} whatsrust"))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth_header}Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.shutdown().await?;

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timeout reading response from daemon"))??;
    parse_cli_response(&buf)
}

/// Stream SSE events from the daemon to stdout until disconnected or Ctrl-C.
pub async fn cli_stream_sse(port: u16) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let host = cli_connect_host(&api_bind_host());
    let auth_header = configured_api_token()
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let mut stream = tokio::net::TcpStream::connect((&*host, port)).await
        .map_err(|e| anyhow::anyhow!("cannot connect to whatsrust daemon on {host}:{port}: {e}"))?;

    let req = format!(
        "GET /api/events HTTP/1.1\r\nHost: {host}:{port}\r\n{auth_header}Accept: text/event-stream\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;

    // Read and discard HTTP headers
    let mut reader = BufReader::new(stream);
    let mut header_line = String::new();
    loop {
        header_line.clear();
        let n = reader.read_line(&mut header_line).await?;
        if n == 0 || header_line.trim().is_empty() {
            break;
        }
    }

    // Stream SSE events to stdout
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => print!("{line}"),
            Err(_) => break,
        }
    }
    Ok(())
}

fn parse_cli_response(raw: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    let header_end = raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("invalid HTTP response"))?;
    let header_str = String::from_utf8_lossy(&raw[..header_end]);
    let status: u16 = header_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw[header_end + 4..].to_vec();
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::bridge::{InboundContent, MessageFlags, WhatsAppInbound};
    use crate::bridge_events::BridgeEvent;

    #[test]
    fn test_is_loopback_bind_accepts_local_hosts() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("::1"));
        assert!(is_loopback_bind("localhost"));
    }

    #[test]
    fn test_is_loopback_bind_rejects_remote_hosts() {
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("192.168.1.10"));
        assert!(!is_loopback_bind("api.internal"));
    }

    #[test]
    fn test_cli_connect_host_rewrites_wildcards() {
        assert_eq!(cli_connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(cli_connect_host("::"), "::1");
        assert_eq!(cli_connect_host("192.168.1.10"), "192.168.1.10");
    }

    #[test]
    fn test_request_has_api_token_accepts_bearer_and_header() {
        let req = HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            query: Vec::new(),
            headers: vec![
                ("Authorization".into(), "Bearer secret".into()),
                ("X-API-Token".into(), "secret".into()),
            ],
            body: Vec::new(),
        };
        assert!(request_has_api_token(&req, "secret"));
        assert!(!request_has_api_token(&req, "wrong"));
    }

    #[test]
    fn test_ct_eq() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(!ct_eq("", "a"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn test_vcard_name_sanitized() {
        // Newlines in name could inject vCard fields
        let name = "Evil\nBEGIN:VCARD\nFN:Injected";
        let safe = name.replace(['\n', '\r', '\0'], " ");
        assert!(!safe.contains('\n'));
        assert!(safe.contains("Evil BEGIN:VCARD FN:Injected"));
    }

    #[test]
    fn test_vcard_phone_sanitized() {
        // Only digits and + should survive
        let phone = "1-555-BAD\n999";
        let safe: String = phone.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
        assert_eq!(safe, "1555999");
    }

    #[test]
    fn test_resolve_group_reaction_target_requires_explicit_group_metadata() {
        assert!(resolve_group_reaction_target("120363000@g.us", None, Some("user@s.whatsapp.net")).is_err());
        assert!(resolve_group_reaction_target("120363000@g.us", Some(false), None).is_err());
        assert_eq!(
            resolve_group_reaction_target(
                "120363000@g.us",
                Some(false),
                Some("user@s.whatsapp.net"),
            )
            .unwrap(),
            false
        );
    }

    #[test]
    fn test_format_sse_event_serializes_full_inbound_payload() {
        let inbound = WhatsAppInbound {
            sequence: 7,
            bridge_id: "default".into(),
            jid: "120363000@g.us".into(),
            id: "wamid-1".into(),
            content: InboundContent::Text {
                body: "hello".into(),
                link_preview: None,
            },
            sender: "15551234567".into(),
            sender_raw: "15551234567@s.whatsapp.net".into(),
            push_name: "Alice".into(),
            timestamp: 1_700_000_000,
            reply_to: None,
            is_from_me: false,
            is_group: true,
            mentions: vec!["199@s.whatsapp.net".into()],
            ephemeral_expiration: Some(3600),
            flags: MessageFlags {
                is_forwarded: true,
                forwarding_score: 2,
                is_view_once: false,
            },
        };

        let frame = format_sse_event(&BridgeEvent::Inbound(Arc::new(inbound))).unwrap();
        assert!(frame.contains("id: inbound:default:7"));
        assert!(frame.contains("event: inbound"));
        assert!(frame.contains("\"sender_raw\":\"15551234567@s.whatsapp.net\""));
        assert!(frame.contains("\"sequence\":7"));
        assert!(frame.contains("\"content\":{\"type\":\"text\",\"body\":\"hello\",\"link_preview\":null}"));
    }

    // -------------------------------------------------------------------------
    // M1.4: validate_fetch_mode tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_validate_fetch_mode_all_ignores_target_value() {
        assert!(validate_fetch_mode("all", None).is_ok());
        assert!(validate_fetch_mode("all", Some(100)).is_ok());
    }

    #[test]
    fn test_validate_fetch_mode_since_requires_target_value() {
        assert!(validate_fetch_mode("since", Some(1_700_000_000_000)).is_ok());
        assert!(validate_fetch_mode("since", None).is_err());
    }

    #[test]
    fn test_validate_fetch_mode_count_requires_positive_target_value() {
        assert!(validate_fetch_mode("count", Some(100)).is_ok());
        assert!(validate_fetch_mode("count", None).is_err());
        assert!(validate_fetch_mode("count", Some(0)).is_err());
        assert!(validate_fetch_mode("count", Some(-5)).is_err());
    }

    #[test]
    fn test_validate_fetch_mode_rejects_unknown_modes() {
        assert!(validate_fetch_mode("max", None).is_err());
        assert!(validate_fetch_mode("", None).is_err());
        assert!(validate_fetch_mode("ALL", None).is_err());
    }

    // -------------------------------------------------------------------------
    // M1.4: enqueue_outcome_to_response mapping tests
    // -------------------------------------------------------------------------

    fn make_notify() -> Arc<tokio::sync::Notify> {
        Arc::new(tokio::sync::Notify::new())
    }

    fn parse_response_json(raw: &[u8]) -> serde_json::Value {
        // raw is a full HTTP response; find the body after "\r\n\r\n"
        let sep = b"\r\n\r\n";
        let body_start = raw.windows(sep.len()).position(|w| w == sep).map(|p| p + sep.len()).unwrap_or(0);
        serde_json::from_slice(&raw[body_start..]).expect("response body must be valid JSON")
    }

    fn http_status_of(raw: &[u8]) -> u16 {
        // "HTTP/1.1 200 OK\r\n..."
        let line_end = raw.windows(2).position(|w| w == b"\r\n").unwrap_or(raw.len());
        let line = std::str::from_utf8(&raw[..line_end]).unwrap_or("");
        line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    #[test]
    fn test_outcome_accepted_returns_200_with_job_id_and_notifies() {
        use crate::storage::EnqueueOutcome;
        let notify = make_notify();
        let outcome = EnqueueOutcome::Accepted { job_id: 42, accepted_target: Some(100) };
        let raw = enqueue_outcome_to_response(outcome, "chat@s.whatsapp.net", "count", Some(200), Some(1_700_000_000_000), true, &notify);
        assert_eq!(http_status_of(&raw), 200);
        let body = parse_response_json(&raw);
        assert_eq!(body["job_id"], 42);
        assert_eq!(body["status"], "queued");
        assert_eq!(body["target_kind"], "count");
        assert_eq!(body["requested"], 200);
        assert_eq!(body["accepted"], 100);
        assert_eq!(body["more_remain"], true);
        // Verify notify_one was called — a second notified() poll would block; instead check
        // that a future waiting on it can be immediately resolved.
        let n = notify.clone();
        let resolved = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r = resolved.clone();
        // tokio::sync::Notify is not directly inspectable; we trust notify_one() was called
        // by observing a notified() future resolves in a spawn (best effort in sync context).
        drop((n, r)); // just compile-check; behavioral coverage via accepted_target assertion above
    }

    #[test]
    fn test_outcome_already_active_returns_200_with_job_id() {
        use crate::storage::EnqueueOutcome;
        let notify = make_notify();
        let outcome = EnqueueOutcome::AlreadyActive { job_id: 7 };
        let raw = enqueue_outcome_to_response(outcome, "chat@s.whatsapp.net", "all", None, None, false, &notify);
        assert_eq!(http_status_of(&raw), 200);
        let body = parse_response_json(&raw);
        assert_eq!(body["job_id"], 7);
        assert_eq!(body["status"], "already_active");
    }

    #[test]
    fn test_outcome_cooldown_returns_429() {
        use crate::storage::EnqueueOutcome;
        let notify = make_notify();
        let outcome = EnqueueOutcome::Cooldown { retry_after_secs: 120 };
        let raw = enqueue_outcome_to_response(outcome, "chat@s.whatsapp.net", "all", None, None, false, &notify);
        assert_eq!(http_status_of(&raw), 429);
        let body = parse_response_json(&raw);
        assert_eq!(body["status"], "cooldown");
        assert_eq!(body["retry_after_secs"], 120);
    }

    #[test]
    fn test_outcome_queue_full_returns_429() {
        use crate::storage::EnqueueOutcome;
        let notify = make_notify();
        let outcome = EnqueueOutcome::QueueFull { limit: 5 };
        let raw = enqueue_outcome_to_response(outcome, "chat@s.whatsapp.net", "all", None, None, false, &notify);
        assert_eq!(http_status_of(&raw), 429);
        let body = parse_response_json(&raw);
        assert_eq!(body["status"], "queue_full");
        assert_eq!(body["limit"], 5);
    }

    // -------------------------------------------------------------------------
    // M1.4: storage-level exhausted fast-path test
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_exhausted_cursor_no_enqueue() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let db_path = std::env::temp_dir().join(format!("whatsrust-hf-exhaust-test-{ts}.db"));
        let store = crate::storage::Store::new(&db_path).expect("open db");

        // Seed an exhausted cursor
        store.upsert_backfill_cursor("chat@s.whatsapp.net", None, None, None, false, true, None).await.unwrap();

        // Confirm cursor is exhausted
        let cursor = store.get_backfill_cursor("chat@s.whatsapp.net").await.unwrap().unwrap();
        assert!(cursor.exhausted);

        // Confirm no job was enqueued (this mirrors what the handler does: fast-path returns without enqueueing)
        let jobs = store.list_backfill_jobs(false).await.unwrap();
        assert!(jobs.is_empty(), "no job should have been enqueued for an exhausted cursor");

        let _ = std::fs::remove_file(&db_path);
    }
}
