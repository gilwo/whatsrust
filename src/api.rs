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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::bridge::{BridgeState, SendTimedOut, WhatsAppBridge, SYNC_CLIENT_TIMEOUT};
use crate::qr::QrRender;

/// Maximum concurrent SSE connections (separate from request semaphore).
const SSE_MAX_CONNECTIONS: usize = 8;
/// Write timeout per SSE event — disconnect slow clients.
const SSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Heartbeat interval for SSE keepalive.
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Appended to a POST client's timeout/read-failure text: the daemon may already have accepted
/// the request and may still complete it, so the caller must check before retrying.
pub(crate) const POST_OUTCOME_UNKNOWN: &str =
    "the request may still be processed by the daemon; check before retrying";

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// A whole request, head and body, as [`read_request`] returns it to tests. The server itself
/// goes from [`RequestHead`] straight to [`RestRequest`].
#[cfg(test)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    query: Vec<(String, String)>,
    pub(crate) headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[cfg(test)]
impl HttpRequest {
    /// Drop the transport-level headers and attach `context`, leaving what a route handler needs.
    fn into_rest(self, context: CallContext) -> RestRequest {
        RestRequest {
            method: self.method,
            path: self.path,
            query: self.query,
            body: self.body,
            context,
        }
    }
}

/// What a route handler may know about its caller beyond the request itself. Handlers take
/// these facts from here rather than from the request, so a caller that is not a socket client
/// can state them explicitly.
pub(crate) struct CallContext {
    /// Whether the caller may make the daemon read local files (`path` media uploads).
    pub(crate) loopback_file_access: bool,
}

/// A REST request as the router sees it: no socket, no transport headers. The socket server
/// builds one per request; any other caller can construct one directly.
pub(crate) struct RestRequest {
    /// HTTP method, e.g. `GET`.
    pub(crate) method: String,
    /// Request path without the query string.
    pub(crate) path: String,
    /// Decoded query parameters in request order.
    pub(crate) query: Vec<(String, String)>,
    /// Raw request body.
    pub(crate) body: Vec<u8>,
    /// Facts about the caller.
    pub(crate) context: CallContext,
}

impl RestRequest {
    fn query_get(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// Percent-encode a value for use in a URL query component (RFC 3986):
/// keep unreserved chars, %XX-encode every other byte. UTF-8 safe (operates on bytes).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode a URL query-string component using `application/x-www-form-urlencoded`
/// rules (the WHATWG URL spec's query encoding): `%XX`-decode as usual, and
/// also treat a literal `+` as a space. Invalid/incomplete `%` escapes are
/// left literal (so a raw string with no valid escapes is returned unchanged).
///
/// `%2B` always decodes to a literal `+` and is never turned into a space —
/// only *raw* `+` bytes in the input are treated as space. This matches what
/// browsers, `curl --data-urlencode`, and Python's `urllib.parse.urlencode`
/// send for query strings; `api::urlencode` emits `%20` for space and `%2B`
/// for a literal `+`, so whatsrust's own CLI/MCP clients round-trip exactly.
fn decode_query_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let hex = |c: u8| -> Option<u8> {
        match c { b'0'..=b'9' => Some(c - b'0'), b'a'..=b'f' => Some(c - b'a' + 10), b'A'..=b'F' => Some(c - b'A' + 10), _ => None }
    };
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        if b[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A structured REST response. [`ApiResponse::into_bytes`] is the one place that turns it into
/// HTTP/1.1 bytes; SSE streams bypass it and write their own headers.
#[derive(Debug)]
pub(crate) struct ApiResponse {
    /// HTTP status code.
    pub(crate) status: u16,
    /// `Content-Type` header value.
    pub(crate) content_type: &'static str,
    /// Extra response headers, written in order after `Content-Length`.
    pub(crate) headers: Vec<(&'static str, String)>,
    /// Response body.
    pub(crate) body: Vec<u8>,
}

impl ApiResponse {
    /// A response with no extra headers.
    pub(crate) fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self { status, content_type, headers: Vec::new(), body }
    }

    /// Serialize as an HTTP/1.1 response: status line, `Content-Type`, `Content-Length`, any
    /// extra headers, `Connection: close`, a blank line, then the body.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        let status_text = match self.status {
            200 => "OK",
            202 => "Accepted",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            411 => "Length Required",
            413 => "Content Too Large",
            415 => "Unsupported Media Type",
            417 => "Expectation Failed",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Unknown",
        };
        let mut header = format!(
            "HTTP/1.1 {} {status_text}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.headers {
            header.push_str(&format!("{name}: {value}\r\n"));
        }
        header.push_str("Connection: close\r\n\r\n");
        let mut resp = header.into_bytes();
        resp.extend_from_slice(&self.body);
        resp
    }
}

fn json_response(status: u16, body: &str) -> ApiResponse {
    ApiResponse::new(status, "application/json", body.as_bytes().to_vec())
}

fn json_ok(data: serde_json::Value) -> ApiResponse {
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

fn json_ok_id(id: &str) -> ApiResponse {
    json_response(200, &json!({"ok": true, "id": id}).to_string())
}

fn json_ok_simple() -> ApiResponse {
    json_response(200, r#"{"ok":true}"#)
}

pub(crate) fn json_err(status: u16, msg: &str) -> ApiResponse {
    let code = match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        405 => "method_not_allowed",
        411 => "length_required",
        415 => "unsupported_media_type",
        417 => "expectation_failed",
        429 => "rate_limited",
        503 => "unavailable",
        504 => "timeout",
        _ => "internal_error",
    };
    json_response(status, &json!({"ok": false, "code": code, "error": msg}).to_string())
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiResponse> {
    serde_json::from_slice(body).map_err(|e| json_err(400, &format!("invalid JSON: {e}")))
}

/// 504 for a sync send that timed out while its job is still queued. Carries the `job_id`
/// and a "do not retry" message so a client does not re-send and deliver a duplicate.
fn send_timeout_response(t: &SendTimedOut) -> ApiResponse {
    let body = json!({"ok": false, "code": "timeout", "error": t.to_string(), "job_id": t.job_id});
    json_response(504, &body.to_string())
}

/// Classify bridge errors into HTTP status codes for machine-friendly responses.
/// A [`SendTimedOut`] anywhere in the error chain wins (found by downcast, so added
/// `.context()` layers cannot hide it); every other error is classified by message.
fn bridge_err(e: anyhow::Error) -> ApiResponse {
    if let Some(t) = e.chain().find_map(|c| c.downcast_ref::<SendTimedOut>()) {
        return send_timeout_response(t);
    }
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

pub(crate) fn bool_env_var(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// The REST listener's bind host from `WHATSRUST_BIND` (default `127.0.0.1`).
pub fn api_bind_host() -> String {
    std::env::var("WHATSRUST_BIND").unwrap_or_else(|_| "127.0.0.1".to_string())
}

pub(crate) fn is_loopback_bind(bind: &str) -> bool {
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

/// The token in the environment variable `name`: trimmed, and `None` when the variable is unset,
/// empty or only whitespace.
pub(crate) fn token_from_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn configured_api_token() -> Option<String> {
    token_from_env("WHATSRUST_API_TOKEN")
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

/// Whether `headers` carry `expected_token` as `X-API-Token` or as an `Authorization: Bearer`
/// token. Reads headers only, so it runs before any body byte is read.
pub(crate) fn request_has_api_token(headers: &[(String, String)], expected_token: &str) -> bool {
    if let Some(tok) = find_header(headers, "x-api-token") {
        if ct_eq(tok, expected_token) {
            return true;
        }
    }
    if let Some(bearer) = find_header(headers, "authorization").and_then(|v| v.strip_prefix("Bearer ")) {
        return ct_eq(bearer, expected_token);
    }
    false
}

// ---------------------------------------------------------------------------
// Request gate: CSRF / DNS-rebinding / Content-Type hardening (ADR 0044)
// ---------------------------------------------------------------------------
//
// The hand-rolled server above has no browser-level same-origin protection of
// its own, so it must enforce one. Applied before routing to EVERY route
// (status, SSE, GETs, media — everything), never just the "obviously
// dangerous" ones, because a no-cors GET carries no Origin and a valid Host
// and would otherwise bypass checks 1-2.
//
// Deliberately NOT doing: CORS headers or OPTIONS handling. A cross-site
// preflight should keep failing — answering it would be the first step
// toward allowing the exact browser requests this gate exists to block.

/// Host values accepted on a loopback bind regardless of the literal bind
/// address — curl/browsers may address the daemon as any of these three.
const LOOPBACK_HOST_NAMES: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// Case-insensitive lookup of the first header matching `name`.
fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Case-insensitive lookup of every header matching `name` — used to detect
/// duplicates (e.g. two `Host` headers, which must be rejected outright).
fn find_all_headers<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect()
}

/// Parse a comma-separated allowlist env var into trimmed, non-empty entries.
/// Default (unset or empty) is an empty allowlist.
pub(crate) fn parse_allowlist_env(name: &str) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Extract and normalize the host part of a `Host` header value for
/// loopback/allowlist comparison:
/// - un-wraps IPv6 bracket notation (`[::1]:7270` -> `::1`)
/// - strips a trailing `:port` for the one-colon (hostname/IPv4) case only —
///   a bare, unbracketed IPv6 literal (two or more colons, e.g. `::1`) is left
///   as-is rather than risk mis-parsing it as `host:port`
/// - strips one or more trailing dots (`localhost.` -> `localhost`)
/// - lowercases the result
fn normalize_host_header(value: &str) -> String {
    let v = value.trim();
    let host_part = if let Some(rest) = v.strip_prefix('[') {
        match rest.split_once(']') {
            Some((host, _)) => host,
            None => v,
        }
    } else if v.matches(':').count() == 1 {
        match v.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
            _ => v,
        }
    } else {
        v
    };
    host_part.trim_end_matches('.').to_ascii_lowercase()
}

/// The CSRF / DNS-rebinding / Content-Type gate. Pure — no I/O, no logging —
/// so it's unit-testable in isolation; callers log the rejection (method,
/// path, reason) and must never log header values beyond `reason`, bodies,
/// or tokens.
///
/// Checks, in order:
/// 1. **Origin** (primary CSRF defence): if present (including the literal
///    `null`), it must exactly match an entry in `allowed_origins` or the
///    request is rejected. Browsers set this header on cross-site requests
///    and it can't be forged by a hostile page, so a match is trusted and
///    the Host check below is skipped.
/// 2. **Host** (defeats DNS rebinding, which carries no Origin): required
///    exactly once, and must resolve to 127.0.0.1/localhost/[::1] on a
///    loopback bind, or to an `allowed_hosts` entry on either bind type.
///    Checked whenever Origin is absent and (the bind is loopback OR
///    `allowed_hosts` is non-empty) — i.e. skipped by default on a
///    non-loopback bind with no configured allowlist.
/// 3. **Content-Type** on every POST: must be `application/json` (parameters
///    like `; charset=utf-8` allowed). This denies the CORS-safelisted
///    content types (`text/plain`, `application/x-www-form-urlencoded`,
///    `multipart/form-data`) and a missing type, forcing a preflight that
///    this server doesn't answer.
pub(crate) fn gate_request(
    method: &str,
    headers: &[(String, String)],
    is_loopback_bind: bool,
    allowed_origins: &[String],
    allowed_hosts: &[String],
) -> Result<(), (u16, String)> {
    match find_all_headers(headers, "origin").as_slice() {
        [] => {}
        [origin] => {
            if !allowed_origins.iter().any(|o| o == origin) {
                return Err((403, format!("Origin '{origin}' is not allowed")));
            }
        }
        _ => return Err((403, "duplicate Origin header".to_string())),
    }

    // Host is checked on every request, including ones with an allowlisted Origin.
    if is_loopback_bind || !allowed_hosts.is_empty() {
        match find_all_headers(headers, "host").as_slice() {
            [] => return Err((403, "missing Host header".to_string())),
            [value] => {
                let normalized = normalize_host_header(value);
                let is_default_loopback =
                    is_loopback_bind && LOOPBACK_HOST_NAMES.contains(&normalized.as_str());
                let is_allowlisted = allowed_hosts.iter().any(|h| normalize_host_header(h) == normalized);
                if !is_default_loopback && !is_allowlisted {
                    return Err((403, format!("Host '{value}' is not allowed")));
                }
            }
            _ => return Err((403, "duplicate Host header".to_string())),
        }
    }

    // HTTP methods are case-sensitive tokens, but compare case-insensitively so a
    // lowercase "post" can't skip this check if the router ever accepts it.
    if method.eq_ignore_ascii_case("POST") {
        let is_json = find_header(headers, "content-type")
            .map(|ct| ct.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"))
            .unwrap_or(false);
        if !is_json {
            return Err((415, "POST requires Content-Type: application/json".to_string()));
        }
    }

    Ok(())
}

const MAX_MEDIA_READ_BYTES: u64 = 50 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

/// Longest a single read of the head or the body may wait for the client.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Wall-clock budget for receiving a whole request head, counted from accept. On the REST
/// listener only the head has a total deadline: the body is bounded per read by
/// [`READ_IDLE_TIMEOUT`] alone, because after the auth checks only authenticated clients reach it
/// and a large upload can be slow. The MCP listener adds its own body deadline.
pub(crate) const HEAD_READ_DEADLINE: Duration = Duration::from_secs(20);
/// Interim response for `Expect: 100-continue`, written before the body is read.
pub(crate) const CONTINUE_RESPONSE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// The instant by which the head of a request accepted now must be complete.
fn head_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + HEAD_READ_DEADLINE
}

/// A parsed request line and headers, plus any body bytes that arrived in the same reads.
pub(crate) struct RequestHead {
    /// HTTP method as sent, e.g. `GET`.
    pub(crate) method: String,
    /// Request path without the query string.
    pub(crate) path: String,
    query: Vec<(String, String)>,
    /// Request headers in arrival order, names as sent.
    pub(crate) headers: Vec<(String, String)>,
    /// Declared `Content-Length`; 0 when absent or unparseable.
    content_length: usize,
    /// Body bytes read past the blank line that ends the head.
    leftover: Vec<u8>,
}

impl RequestHead {
    /// Combine the head with its body and the caller's `context` into the request the router
    /// sees. The transport headers were already used by the checks on the head and are dropped.
    fn into_rest(self, body: Vec<u8>, context: CallContext) -> RestRequest {
        RestRequest {
            method: self.method,
            path: self.path,
            query: self.query,
            body,
            context,
        }
    }

    /// Combine the head with its body into a whole request.
    #[cfg(test)]
    fn into_request(self, body: Vec<u8>) -> HttpRequest {
        HttpRequest {
            method: self.method,
            path: self.path,
            query: self.query,
            headers: self.headers,
            body,
        }
    }

    /// The rejection for a request whose framing this server cannot honour, decided from the
    /// head alone so no body byte is read for it. Checked in order:
    /// - any `Transfer-Encoding` header (the reader cannot decode chunked bodies, and refusing
    ///   every value leaves no `Content-Length` / `Transfer-Encoding` ambiguity): 411;
    /// - `Content-Length` over `limit`: 400, as for any unreadable request;
    /// - an `Expect` value other than `100-continue`: 417.
    fn framing_error(&self, limit: usize) -> Option<ApiResponse> {
        if self.has_transfer_encoding() {
            return Some(json_err(411, "Transfer-Encoding is not supported; send Content-Length"));
        }
        if self.content_length > limit {
            return Some(bad_request());
        }
        self.has_unsupported_expect().then(|| json_err(417, "unsupported Expect"))
    }

    /// The declared `Content-Length`; 0 when absent or unparseable.
    pub(crate) fn content_length(&self) -> usize {
        self.content_length
    }

    /// Whether any `Transfer-Encoding` header is present (the reader cannot decode chunked
    /// bodies, and refusing every value leaves no `Content-Length` / `Transfer-Encoding`
    /// ambiguity). The name is trimmed so `Transfer-Encoding : chunked` is caught too.
    pub(crate) fn has_transfer_encoding(&self) -> bool {
        self.headers.iter().any(|(name, _)| name.trim().eq_ignore_ascii_case("transfer-encoding"))
    }

    /// Whether any `Expect` header has a value other than `100-continue`.
    pub(crate) fn has_unsupported_expect(&self) -> bool {
        find_all_headers(&self.headers, "expect").iter().any(|v| !v.eq_ignore_ascii_case("100-continue"))
    }

    /// Whether the client sent `Expect: 100-continue` and has not sent the whole body yet, so
    /// it is waiting for a `100 Continue` before it does.
    pub(crate) fn awaiting_continue(&self) -> bool {
        let expects_continue = find_header(&self.headers, "expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
        expects_continue && self.leftover.len() < self.content_length
    }
}

/// The response for a request that cannot be read: a malformed, oversize or stalled head, an
/// oversize `Content-Length`, or a body that never fully arrives.
pub(crate) fn bad_request() -> ApiResponse {
    json_err(400, "bad request")
}

/// Body size limit for a request to `path`: 70 MiB for media endpoints (base64 of 50 MiB),
/// 1 MiB otherwise.
fn max_body_bytes(path: &str) -> usize {
    let is_media_route = path.starts_with("/api/image")
        || path.starts_with("/api/video")
        || path.starts_with("/api/audio")
        || path.starts_with("/api/doc")
        || path.starts_with("/api/sticker")
        || path.starts_with("/api/view-once")
        || path.starts_with("/api/status-image")
        || path.starts_with("/api/status-video");
    if is_media_route { 70 * 1024 * 1024 } else { 1024 * 1024 }
}

/// Read until the blank line that ends the request head and parse it. Each read waits at most
/// [`READ_IDLE_TIMEOUT`], and the whole head must be in by `deadline` (see [`head_deadline`]).
/// `None` on either timeout, EOF, I/O error, a head over 128 KiB, or a head that does not parse.
pub(crate) async fn read_request_head<R: AsyncRead + Unpin>(
    stream: &mut R,
    deadline: tokio::time::Instant,
) -> Option<RequestHead> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    // Read until end of headers
    let header_end;
    loop {
        let wait_until = (tokio::time::Instant::now() + READ_IDLE_TIMEOUT).min(deadline);
        match tokio::time::timeout_at(wait_until, stream.read(&mut tmp)).await {
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
    let (path, query) = if let Some((path_part, query_part)) = raw_path.split_once('?') {
        let q = query_part
            .split('&')
            .filter_map(|pair| {
                let mut kv = pair.splitn(2, '=');
                Some((decode_query_component(kv.next()?), decode_query_component(kv.next().unwrap_or(""))))
            })
            .collect();
        (path_part.to_string(), q)
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

    Some(RequestHead {
        method,
        path,
        query,
        headers,
        content_length,
        leftover: buf[header_end + 4..].to_vec(),
    })
}

/// Read the body that `head` declares, starting from the bytes that arrived with the head. Each
/// read waits at most [`READ_IDLE_TIMEOUT`]; there is no total deadline. `None` if the declared
/// length exceeds `limit` (nothing is read then), or the stream ends, errors or stalls before
/// the whole body arrives.
pub(crate) async fn read_request_body<R: AsyncRead + Unpin>(
    stream: &mut R,
    head: &RequestHead,
    limit: usize,
) -> Option<Vec<u8>> {
    let content_length = head.content_length;
    if content_length > limit {
        return None;
    }
    if content_length == 0 {
        return Some(Vec::new());
    }

    let mut tmp = [0u8; 4096];
    let mut body = head.leftover.clone();
    while body.len() < content_length {
        match tokio::time::timeout(READ_IDLE_TIMEOUT, stream.read(&mut tmp)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => body.extend_from_slice(&tmp[..n]),
            Ok(Err(_)) => break,
        }
    }
    // Reject truncated bodies — don't process partial requests
    if body.len() < content_length {
        return None;
    }
    body.truncate(content_length);
    Some(body)
}

/// Read one whole request: the head, then the body under the limit for its path.
#[cfg(test)]
pub(crate) async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<HttpRequest> {
    let head = read_request_head(stream, head_deadline()).await?;
    let body = read_request_body(stream, &head, max_body_bytes(&head.path)).await?;
    Some(head.into_request(body))
}

// ---------------------------------------------------------------------------
// Route dispatch
// ---------------------------------------------------------------------------

/// Route `req` to its handler and return the structured response. No socket is involved, so
/// any caller that can build a [`RestRequest`] can use it.
pub(crate) async fn handle_request(bridge: &WhatsAppBridge, req: &RestRequest) -> ApiResponse {
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
        ("POST", "/api/image") => handle_media(bridge, &req.body, &req.context, MediaKind::Image).await,
        ("POST", "/api/video") => handle_media(bridge, &req.body, &req.context, MediaKind::Video).await,
        ("POST", "/api/audio") => handle_media(bridge, &req.body, &req.context, MediaKind::Audio).await,
        ("POST", "/api/doc") => handle_media(bridge, &req.body, &req.context, MediaKind::Doc).await,
        ("POST", "/api/sticker") => handle_media(bridge, &req.body, &req.context, MediaKind::Sticker).await,
        ("POST", "/api/location") => handle_location(bridge, &req.body).await,
        ("POST", "/api/contact") => handle_contact(bridge, &req.body).await,
        ("POST", "/api/forward") => handle_forward(bridge, &req.body).await,
        ("POST", "/api/poll") => handle_poll(bridge, &req.body).await,
        ("POST", "/api/view-once-image") => handle_media(bridge, &req.body, &req.context, MediaKind::ViewOnceImage).await,
        ("POST", "/api/view-once-video") => handle_media(bridge, &req.body, &req.context, MediaKind::ViewOnceVideo).await,
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

        // Embeddings purge (M2.5)
        ("POST", "/api/embeddings/purge") => handle_purge_embeddings(bridge, &req.body).await,

        _ => json_err(404, "not found"),
    }
}

// ---------------------------------------------------------------------------
// Endpoint handlers
// ---------------------------------------------------------------------------

async fn handle_status(bridge: &WhatsAppBridge) -> ApiResponse {
    let state = bridge.state();
    let m = bridge.metrics();
    let queue = bridge.queue_depth().await;
    let embeddings = bridge.embedding_status_counts().await;
    let device_name = bridge.device_name().await;
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
        "embeddings": embeddings,
        // Additive (Part 1, device-name-at-pairing): the name saved at pairing time, or
        // null when the device hasn't completed pairing yet. See
        // bridge::resolve_device_name / BridgeConfig::device_name.
        "device_name": device_name,
    }))
}

fn handle_qr(bridge: &WhatsAppBridge, req: &RestRequest) -> ApiResponse {
    let qr_data = bridge.current_qr();
    match qr_data {
        Some(data) => {
            let format = req.query_get("format").unwrap_or("json");
            let qr = match QrRender::new(&data) {
                Some(q) => q,
                None => return json_err(500, "failed to render QR code"),
            };
            match format {
                "png" => ApiResponse::new(200, "image/png", qr.png(8)),
                "svg" => ApiResponse::new(200, "image/svg+xml", qr.svg().into_bytes()),
                "terminal" => ApiResponse::new(200, "text/plain", qr.terminal().into_bytes()),
                "html" => ApiResponse::new(200, "text/html", qr.html().into_bytes()),
                _ => json_ok(json!({
                    "qr_data": data,
                    "terminal": qr.terminal(),
                })),
            }
        }
        None => json_err(404, "no QR code available (already paired or not yet generated)"),
    }
}

async fn handle_groups(bridge: &WhatsAppBridge) -> ApiResponse {
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

async fn handle_group_info(bridge: &WhatsAppBridge, req: &RestRequest) -> ApiResponse {
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

async fn handle_send(bridge: &WhatsAppBridge, body: &[u8], sync: bool) -> ApiResponse {
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

async fn handle_reply(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_edit(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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
) -> Result<bool, ApiResponse> {
    if jid.ends_with("@g.us") {
        if sender_jid.is_none() {
            return Err(json_err(400, "sender_jid is required for group reactions"));
        }
        return from_me.ok_or_else(|| json_err(400, "from_me is required for group reactions"));
    }

    Ok(from_me.unwrap_or(sender_jid.is_none()))
}

async fn handle_react(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_unreact(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_revoke(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_jid_action(bridge: &WhatsAppBridge, body: &[u8], action: JidAction) -> ApiResponse {
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

async fn handle_message_action(bridge: &WhatsAppBridge, body: &[u8], action: MessageAction) -> ApiResponse {
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

async fn handle_group_create(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_group_subject(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_group_description(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
    let req: GroupDescriptionReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.set_group_description(&req.jid, req.description.as_deref()).await {
        Ok(()) => json_ok_simple(),
        Err(e) => bridge_err(e),
    }
}

async fn handle_group_invite_link(bridge: &WhatsAppBridge, req: &RestRequest) -> ApiResponse {
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

async fn handle_group_participants(bridge: &WhatsAppBridge, body: &[u8], action: ParticipantAction) -> ApiResponse {
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

async fn read_file_for_media(path: &str) -> Result<Vec<u8>, ApiResponse> {
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

async fn handle_history(bridge: &WhatsAppBridge, req: &RestRequest) -> ApiResponse {
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

async fn handle_search(bridge: &WhatsAppBridge, req: &RestRequest) -> ApiResponse {
    let q = match req.query_get("q") {
        Some(q) if !q.is_empty() => q,
        _ => return json_err(400, "query parameter 'q' is required"),
    };
    let jid = req.query_get("jid");
    let limit: i64 = req.query_get("limit").and_then(|v| v.parse().ok()).unwrap_or(20).max(1).min(200);
    match bridge.search(jid, q, limit, None).await {
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
fn validate_fetch_mode(mode: &str, target_value: Option<i64>) -> Result<(), ApiResponse> {
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
/// Pure function — no side effects — so it can be unit-tested without a bridge.
fn enqueue_outcome_to_response(
    outcome: crate::storage::EnqueueOutcome,
    chat_jid: &str,
    mode: &str,
    requested_target: Option<i64>,
    resume_anchor_ts: Option<i64>,
    more_remain: bool,
) -> ApiResponse {
    use crate::storage::EnqueueOutcome;
    match outcome {
        EnqueueOutcome::Accepted { job_id, accepted_target } => {
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
        EnqueueOutcome::EmbedderBacklogFull { pending_count } => {
            json_response(429, &json!({
                "ok": false,
                "code": "rate_limited",
                "status": "embedder_backlog_full",
                "pending_count": pending_count,
                "message": "pathological pending embeddings (>100k); wait for drain",
            }).to_string())
        }
    }
}

/// POST /api/history-fetch — trigger a backfill job for a chat.
async fn handle_history_fetch_trigger(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

    // Read cursor once; use for both the exhausted fast-path check and the seed/resume decision.
    let cursor_opt = match store.get_backfill_cursor(&chat_jid).await {
        Ok(c) => c,
        Err(e) => return json_err(500, &format!("storage error: {e}")),
    };

    // No-op fast path: if cursor is already exhausted, return immediately without enqueuing.
    if cursor_opt.as_ref().map(|c| c.exhausted).unwrap_or(false) {
        return json_ok(json!({
            "job_id": serde_json::Value::Null,
            "chat_jid": chat_jid,
            "status": "already_exhausted",
            "more_remain": false,
        }));
    }

    // Cursor seed/resume: if no cursor exists, seed it from the oldest message.
    // If a cursor exists and is not exhausted, leave it untouched (resume from where it is).

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
    let active_model_id = bridge.active_model_id();
    let outcome = match store.enqueue_backfill_job(
        &chat_jid,
        &mode,
        req.target_value,
        cooldown_secs,
        queue_depth,
        max_messages,
        active_model_id.as_deref(),
    ).await {
        Ok(o) => o,
        Err(e) => return json_err(500, &format!("storage error: {e}")),
    };

    // Notify the worker before formatting the response so the SSE event can
    // be observed immediately after the HTTP 200 is received.
    if matches!(outcome, crate::storage::EnqueueOutcome::Accepted { .. }) {
        bridge.backfill_notify().notify_one();
    }

    enqueue_outcome_to_response(
        outcome,
        &chat_jid,
        &mode,
        req.target_value,
        resume_anchor_ts,
        more_remain,
    )
}

/// GET /api/history-fetch — status or list backfill jobs.
///
/// With ?job_id=N → get a single job by ID.
/// With ?active=true → list only active jobs; otherwise list all.
async fn handle_history_fetch_status(bridge: &WhatsAppBridge, req: &RestRequest) -> ApiResponse {
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

#[derive(Deserialize)]
struct PurgeEmbeddingsReq {
    model_id: Option<String>,
}

/// POST /api/history-fetch/cancel — cancel a backfill job.
async fn handle_history_fetch_cancel(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
    let req: HistoryFetchCancelReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let job_id = match req.job_id {
        Some(id) => id,
        None => return json_err(400, "job_id is required"),
    };
    match bridge.store().mark_backfill_job(job_id, "cancelled").await {
        Ok(0) => json_err(404, "job not found"),
        Ok(_) => json_ok(json!({"job_id": job_id, "status": "cancelled"})),
        Err(e) => json_err(500, &e.to_string()),
    }
}

async fn handle_purge_embeddings(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
    let req: PurgeEmbeddingsReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };

    let model_id = match req.model_id.as_deref().filter(|s| !s.is_empty()) {
        Some(m) => m,
        None => return json_err(400, "model_id is required"),
    };

    match bridge.purge_embeddings(model_id).await {
        Ok(result) => json_ok(json!({
            "model_id": result.model_id,
            "rows_deleted": result.rows_deleted,
            "bytes_reclaimed": result.bytes_reclaimed,
        })),
        Err(e) => json_err(500, &format!("purge failed: {e}")),
    }
}

enum MediaKind { Image, Video, Audio, Doc, Sticker, ViewOnceImage, ViewOnceVideo }

async fn handle_media(bridge: &WhatsAppBridge, body: &[u8], ctx: &CallContext, kind: MediaKind) -> ApiResponse {
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
        if !ctx.loopback_file_access {
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

async fn handle_location(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_contact(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_forward(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_poll(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_status_text(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
    let req: StatusTextReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    match bridge.send_status_text(&req.recipients, &req.text, req.background_argb, req.font, req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}

async fn handle_status_image(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_status_video(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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

async fn handle_status_revoke(bridge: &WhatsAppBridge, body: &[u8]) -> ApiResponse {
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
        crate::bridge_events::BridgeEvent::BackfillProgress(p) => {
            let data = serde_json::to_string(p).ok()?;
            let event_id = format!("backfill:{}:{}", p.job_id, p.fetched);
            Some(format_sse_frame("backfill", Some(&event_id), &data))
        }
        crate::bridge_events::BridgeEvent::StorageAlert(a) => {
            let data = serde_json::to_string(a).ok()?;
            let event_id = format!("storage_alert:{}:{}", a.current_bytes, a.baseline_bytes);
            Some(format_sse_frame("storage_alert", Some(&event_id), &data))
        }
        crate::bridge_events::BridgeEvent::Heartbeat => {
            Some(format_sse_frame("heartbeat", None, "{}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// REST listener settings read from the environment: the startup rules check them and
/// [`serve_with_listener`] enforces them.
struct ApiSettings {
    bind: String,
    allow_remote: bool,
    token: Option<String>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
}

impl ApiSettings {
    fn from_env() -> Self {
        Self {
            bind: api_bind_host(),
            allow_remote: bool_env_var("WHATSRUST_ALLOW_REMOTE"),
            token: configured_api_token(),
            allowed_hosts: parse_allowlist_env("WHATSRUST_ALLOWED_HOSTS"),
            allowed_origins: parse_allowlist_env("WHATSRUST_ALLOWED_ORIGINS"),
        }
    }
}

/// Startup rules for a REST listener that is about to bind (design doc §13, rules 4 and 5).
/// The `Err` text is the whole message: what is wrong, the exact fix, "Refusing to start."
/// A token that is empty or only whitespace counts as unset, as in [`configured_api_token`].
fn validate_api_listener(
    bind: &str,
    allow_remote: bool,
    token: Option<&str>,
    allowed_hosts: &[String],
    allowed_origins: &[String],
) -> Result<(), String> {
    let has_token = token.is_some_and(|t| !t.trim().is_empty());
    if !is_loopback_bind(bind) {
        if !allow_remote {
            return Err(format!(
                "WHATSRUST_BIND={bind} is not a loopback address, but WHATSRUST_ALLOW_REMOTE is not set.\n\
                 To expose the API on the network, set WHATSRUST_ALLOW_REMOTE=1 and WHATSRUST_API_TOKEN \
                 (e.g. `openssl rand -base64 32`); to keep it local, unset WHATSRUST_BIND or set \
                 WHATSRUST_BIND=127.0.0.1. Refusing to start."
            ));
        }
        if !has_token {
            return Err(format!(
                "WHATSRUST_BIND={bind} is not a loopback address and WHATSRUST_API_TOKEN is not set.\n\
                 Set WHATSRUST_API_TOKEN (e.g. `openssl rand -base64 32`), or keep the API local by \
                 unsetting WHATSRUST_BIND and WHATSRUST_ALLOW_REMOTE. Refusing to start."
            ));
        }
    }
    if !has_token && (!allowed_hosts.is_empty() || !allowed_origins.is_empty()) {
        let set = match (allowed_hosts.is_empty(), allowed_origins.is_empty()) {
            (false, false) => "WHATSRUST_ALLOWED_HOSTS and WHATSRUST_ALLOWED_ORIGINS",
            (false, true) => "WHATSRUST_ALLOWED_HOSTS",
            _ => "WHATSRUST_ALLOWED_ORIGINS",
        };
        return Err(format!(
            "A proxy allowlist is configured ({set}) but WHATSRUST_API_TOKEN is not set.\n\
             Set WHATSRUST_API_TOKEN (e.g. `openssl rand -base64 32`), or unset {set}. \
             Refusing to start."
        ));
    }
    Ok(())
}

/// Startup rules for the REST listener on `port`. Port 0 is headless (no listener), so nothing
/// is checked. [`serve`] has no headless mode and calls [`validate_api_listener`] directly.
fn validate_api_startup(
    port: u16,
    bind: &str,
    allow_remote: bool,
    token: Option<&str>,
    allowed_hosts: &[String],
    allowed_origins: &[String],
) -> Result<(), String> {
    if port == 0 {
        return Ok(());
    }
    validate_api_listener(bind, allow_remote, token, allowed_hosts, allowed_origins)
}

/// [`validate_api_startup`] on the environment's settings. The daemon calls it before the
/// bridge is built and exits with the `Err` text.
pub fn validate_api_startup_from_env(port: u16) -> Result<(), String> {
    let s = ApiSettings::from_env();
    validate_api_startup(port, &s.bind, s.allow_remote, s.token.as_deref(), &s.allowed_hosts, &s.allowed_origins)
}

/// Bind the REST listener to `bind:port`. The `Err` text is the whole startup message: the
/// variables involved, the OS error and the fix.
pub async fn bind_api_listener(bind: &str, port: u16) -> Result<TcpListener, String> {
    match TcpListener::bind((bind, port)).await {
        Ok(listener) => {
            info!(bind = %bind, port = port, "API server listening");
            Ok(listener)
        }
        Err(e) => {
            let fix = if e.kind() == std::io::ErrorKind::AddrInUse {
                format!(
                    "Port {port} is already in use; stop the other process or set WHATSRUST_PORT to a \
                     free port (WHATSRUST_PORT=0 turns the API off)."
                )
            } else {
                "Check that WHATSRUST_BIND is an address of this machine and that WHATSRUST_PORT is \
                 allowed (WHATSRUST_PORT=0 turns the API off)."
                    .to_string()
            };
            Err(format!(
                "Cannot bind the REST API (WHATSRUST_BIND={bind}, WHATSRUST_PORT={port}): {e}.\n\
                 {fix} Refusing to start."
            ))
        }
    }
}

/// Start the API server. Blocks until cancelled.
///
/// Library entry point: it binds `WHATSRUST_BIND:port` itself. A configuration that fails the
/// startup rules, or a bind that fails, is logged and the function returns; it never exits the
/// process. The daemon validates and binds in `main` and calls [`serve_with_listener`].
#[allow(dead_code)] // Public API for library consumers (habb); the daemon uses serve_with_listener
pub async fn serve(bridge: Arc<WhatsAppBridge>, port: u16, cancel: CancellationToken) {
    let s = ApiSettings::from_env();
    // `serve` always listens (port 0 picks an ephemeral port), so there is no headless case.
    if let Err(msg) = validate_api_listener(&s.bind, s.allow_remote, s.token.as_deref(), &s.allowed_hosts, &s.allowed_origins) {
        error!("{msg}");
        return;
    }
    let listener = match bind_api_listener(&s.bind, port).await {
        Ok(l) => l,
        Err(msg) => {
            error!("{msg}");
            return;
        }
    };
    serve_with_listener(bridge, listener, cancel).await;
}

/// Serve the API on an already-bound `listener` until cancelled. The caller has validated the
/// settings and bound `listener` to `WHATSRUST_BIND`; token and allowlists come from the
/// environment.
pub async fn serve_with_listener(bridge: Arc<WhatsAppBridge>, listener: TcpListener, cancel: CancellationToken) {
    let s = ApiSettings::from_env();

    // Allowlists (ADR 0044) and the SSE semaphore are built once at startup.
    let cfg = Arc::new(ServeConfig {
        is_loopback: is_loopback_bind(&s.bind),
        api_token: s.token,
        allowed_origins: s.allowed_origins,
        allowed_hosts: s.allowed_hosts,
        sse_sem: Arc::new(Semaphore::new(SSE_MAX_CONNECTIONS)),
        sse_cancel: cancel.clone(),
    });
    // Cap concurrent connections to prevent slowloris/flood exhaustion.
    let conn_sem = Arc::new(Semaphore::new(64));

    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((mut stream, _)) = result else { continue };
                let permit = match conn_sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        let _ = stream.write_all(&json_err(503, "too many connections").into_bytes()).await;
                        continue;
                    }
                };
                let bridge = bridge.clone();
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let _permit = permit; // held until handler completes
                    serve_connection(stream, &bridge, &cfg).await;
                });
            }
            _ = cancel.cancelled() => break,
        }
    }
}

/// Per-listener settings that [`serve_connection`] reads for every accepted connection.
/// [`serve_with_listener`] builds one from the environment; tests build one explicitly.
struct ServeConfig {
    /// Whether the listener is bound to a loopback address (gates `path` media uploads and
    /// the default `Host` allowance).
    is_loopback: bool,
    /// Required API token, when configured.
    api_token: Option<String>,
    /// `Origin` allowlist for the request gate (ADR 0044).
    allowed_origins: Vec<String>,
    /// `Host` allowlist for the request gate (ADR 0044).
    allowed_hosts: Vec<String>,
    /// Caps concurrent SSE streams so long-lived connections don't starve normal requests.
    sse_sem: Arc<Semaphore>,
    /// Ends SSE streams on shutdown.
    sse_cancel: CancellationToken,
}

/// Most request bytes [`respond_and_drain`] discards after an early rejection.
const DRAIN_MAX_BYTES: usize = 64 * 1024;
/// Longest [`respond_and_drain`] spends discarding them.
const DRAIN_MAX_TIME: Duration = Duration::from_secs(1);

/// Write `response` to a client whose request body has not been fully read, then close without
/// a reset: shut down the write half, read and discard what the client is still sending (until
/// EOF, [`DRAIN_MAX_BYTES`] or [`DRAIN_MAX_TIME`]), and drop the stream. Closing a socket with
/// unread data makes the OS reset the connection, and the client can lose the response.
pub(crate) async fn respond_and_drain<S>(mut stream: S, response: ApiResponse)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if stream.write_all(&response.into_bytes()).await.is_err() {
        return;
    }
    let _ = stream.shutdown().await;
    let mut sink = [0u8; 4096];
    let mut drained = 0;
    let _ = tokio::time::timeout(DRAIN_MAX_TIME, async {
        while drained < DRAIN_MAX_BYTES {
            let want = sink.len().min(DRAIN_MAX_BYTES - drained);
            match stream.read(&mut sink[..want]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => drained += n,
            }
        }
    })
    .await;
}

/// The checks that need only the request head, in order: the CSRF / DNS-rebinding /
/// Content-Type gate (403/415, ADR 0044), the API token (401), then the framing checks
/// (411/400/417). `Some` is the response to send instead of reading the body, so no body byte
/// is read for a request that fails any of them.
fn reject_from_head(head: &RequestHead, cfg: &ServeConfig) -> Option<ApiResponse> {
    // The gate applies to EVERY route, including SSE and auth, so a cross-site "simple"
    // request never reaches a handler.
    if let Err((status, reason)) = gate_request(
        &head.method,
        &head.headers,
        cfg.is_loopback,
        &cfg.allowed_origins,
        &cfg.allowed_hosts,
    ) {
        debug!(method = %head.method, path = %head.path, reason = %reason, "request rejected by origin/host/content-type gate");
        return Some(json_err(status, &reason));
    }
    if let Some(expected_token) = cfg.api_token.as_deref() {
        if !request_has_api_token(&head.headers, expected_token) {
            return Some(json_err(401, "unauthorized"));
        }
    }
    head.framing_error(max_body_bytes(&head.path))
}

/// Read one request from `stream`, run its head through the gate, token and framing checks
/// before reading the body, and write the response (or hand the stream to the SSE handler).
/// A response written before the body is fully read goes through [`respond_and_drain`]. The
/// stream closes when this returns.
async fn serve_connection(mut stream: tokio::net::TcpStream, bridge: &WhatsAppBridge, cfg: &ServeConfig) {
    let Some(head) = read_request_head(&mut stream, head_deadline()).await else {
        return respond_and_drain(stream, bad_request()).await;
    };
    if let Some(response) = reject_from_head(&head, cfg) {
        return respond_and_drain(stream, response).await;
    }

    // SSE endpoint — long-lived, uses dedicated semaphore
    if head.method == "GET" && head.path == "/api/events" {
        let sse_permit = match cfg.sse_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return respond_and_drain(stream, json_err(503, "too many SSE connections")).await,
        };
        handle_sse(bridge, &mut stream, &cfg.sse_cancel).await;
        drop(sse_permit);
        return;
    }

    // Only a request that passed every check above gets its 100 Continue.
    if head.awaiting_continue() && stream.write_all(CONTINUE_RESPONSE).await.is_err() {
        return;
    }
    let Some(body) = read_request_body(&mut stream, &head, max_body_bytes(&head.path)).await else {
        return respond_and_drain(stream, bad_request()).await;
    };

    let context = CallContext { loopback_file_access: cfg.is_loopback };
    let response = handle_request(bridge, &head.into_rest(body, context)).await;
    let _ = stream.write_all(&response.into_bytes()).await;
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
    tokio::time::timeout(SYNC_CLIENT_TIMEOUT, stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timeout reading response from daemon"))??;
    parse_cli_response(&buf)
}

/// Send a POST request with JSON body to the running daemon.
///
/// Waits up to [`SYNC_CLIENT_TIMEOUT`] (longer than the daemon's own sync-send wait) so the
/// daemon's 504 + job_id normally arrives first. A timeout or read failure here leaves the
/// outcome unknown, so the error says the request may still be processed.
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
    tokio::time::timeout(SYNC_CLIENT_TIMEOUT, stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timeout reading response from daemon; {POST_OUTCOME_UNKNOWN}"))?
        .map_err(|e| anyhow::anyhow!("failed reading response from daemon: {e}; {POST_OUTCOME_UNKNOWN}"))?;
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
mod golden_tests;

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
        let headers = vec![
            ("Authorization".to_string(), "Bearer secret".to_string()),
            ("X-API-Token".to_string(), "secret".to_string()),
        ];
        assert!(request_has_api_token(&headers, "secret"));
        assert!(!request_has_api_token(&headers, "wrong"));
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
    // ApiResponse serialization
    // -------------------------------------------------------------------------

    /// The serializer `ApiResponse::into_bytes` replaced, verbatim — the oracle for
    /// "the wire format did not change". It predates the reason phrases for 202, 405, 411, 415
    /// and 417, so the comparison below leaves those five out.
    fn legacy_http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
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

    #[test]
    fn test_api_response_into_bytes_matches_legacy_serializer() {
        let statuses = [200, 400, 401, 403, 404, 418, 429, 500, 503, 504, 0, 999];
        let bodies: [&[u8]; 3] = [b"", br#"{"ok":true}"#, &[0, 159, 146, 150, 255, b'\r', b'\n']];
        for status in statuses {
            for content_type in ["application/json", "image/png", "text/plain"] {
                for body in bodies {
                    let got = ApiResponse::new(status, content_type, body.to_vec()).into_bytes();
                    assert_eq!(
                        got,
                        legacy_http_response(status, content_type, body),
                        "status {status}, {content_type}, {} body bytes",
                        body.len()
                    );
                }
            }
        }
    }

    #[test]
    fn test_api_response_reason_phrases() {
        let cases = [
            (200, "OK"),
            (202, "Accepted"),
            (400, "Bad Request"),
            (405, "Method Not Allowed"),
            (411, "Length Required"),
            (413, "Content Too Large"),
            (415, "Unsupported Media Type"),
            (417, "Expectation Failed"),
            (418, "Unknown"),
            (501, "Not Implemented"),
            (0, "Unknown"),
        ];
        for (status, phrase) in cases {
            let raw = ApiResponse::new(status, "application/json", b"{}".to_vec()).into_bytes();
            let status_line = String::from_utf8(raw).unwrap().lines().next().unwrap().to_string();
            assert_eq!(status_line, format!("HTTP/1.1 {status} {phrase}"));
        }
    }

    #[test]
    fn test_json_err_codes() {
        let cases = [
            (400, "bad_request"),
            (401, "unauthorized"),
            (403, "forbidden"),
            (404, "not_found"),
            (405, "method_not_allowed"),
            (411, "length_required"),
            (415, "unsupported_media_type"),
            (417, "expectation_failed"),
            (429, "rate_limited"),
            (500, "internal_error"),
            (503, "unavailable"),
            (504, "timeout"),
            (418, "internal_error"),
        ];
        for (status, code) in cases {
            let body = parse_response_json(&json_err(status, "m").into_bytes());
            assert_eq!(body["code"], code, "status {status}");
            assert_eq!(body["ok"], false);
        }
    }

    #[test]
    fn test_api_response_extra_headers_follow_content_length_in_order() {
        let resp = ApiResponse {
            status: 503,
            content_type: "application/json",
            headers: vec![("Retry-After", "10".to_string()), ("Allow", "POST".to_string())],
            body: b"{}".to_vec(),
        };
        assert_eq!(
            String::from_utf8(resp.into_bytes()).unwrap(),
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 2\r\nRetry-After: 10\r\nAllow: POST\r\nConnection: close\r\n\r\n{}"
        );
    }

    // -------------------------------------------------------------------------
    // M1.4: enqueue_outcome_to_response mapping tests
    // -------------------------------------------------------------------------

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
        // Fix 3: enqueue_outcome_to_response is now pure (no notify param).
        // notify_one() is called by the handler (handle_history_fetch_trigger) instead.
        use crate::storage::EnqueueOutcome;
        let outcome = EnqueueOutcome::Accepted { job_id: 42, accepted_target: Some(100) };
        let raw = enqueue_outcome_to_response(outcome, "chat@s.whatsapp.net", "count", Some(200), Some(1_700_000_000_000), true).into_bytes();
        assert_eq!(http_status_of(&raw), 200);
        let body = parse_response_json(&raw);
        assert_eq!(body["job_id"], 42);
        assert_eq!(body["status"], "queued");
        assert_eq!(body["target_kind"], "count");
        assert_eq!(body["requested"], 200);
        assert_eq!(body["accepted"], 100);
        assert_eq!(body["more_remain"], true);
    }

    #[test]
    fn test_outcome_already_active_returns_200_with_job_id() {
        use crate::storage::EnqueueOutcome;
        let outcome = EnqueueOutcome::AlreadyActive { job_id: 7 };
        let raw = enqueue_outcome_to_response(outcome, "chat@s.whatsapp.net", "all", None, None, false).into_bytes();
        assert_eq!(http_status_of(&raw), 200);
        let body = parse_response_json(&raw);
        assert_eq!(body["job_id"], 7);
        assert_eq!(body["status"], "already_active");
    }

    #[test]
    fn test_outcome_cooldown_returns_429() {
        use crate::storage::EnqueueOutcome;
        let outcome = EnqueueOutcome::Cooldown { retry_after_secs: 120 };
        let raw = enqueue_outcome_to_response(outcome, "chat@s.whatsapp.net", "all", None, None, false).into_bytes();
        assert_eq!(http_status_of(&raw), 429);
        let body = parse_response_json(&raw);
        assert_eq!(body["status"], "cooldown");
        assert_eq!(body["retry_after_secs"], 120);
    }

    #[test]
    fn test_outcome_queue_full_returns_429() {
        use crate::storage::EnqueueOutcome;
        let outcome = EnqueueOutcome::QueueFull { limit: 5 };
        let raw = enqueue_outcome_to_response(outcome, "chat@s.whatsapp.net", "all", None, None, false).into_bytes();
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

    // -------------------------------------------------------------------------
    // Query param hardening: urlencode / decode_query_component
    // -------------------------------------------------------------------------

    #[test]
    fn test_urlencode_decode_query_component_round_trip() {
        let cases = [
            "972542271337@s.whatsapp.net",
            "120363000@g.us",
            "foo&bar=baz #x",
            " ",
            "50%",
            "שלום",
            "مرحبا",
        ];
        for s in cases {
            let encoded = urlencode(s);
            assert_eq!(decode_query_component(&encoded), s, "round-trip failed for {s:?}");
        }
    }

    #[test]
    fn test_urlencode_keeps_unreserved_chars_literal() {
        assert_eq!(urlencode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(urlencode("@"), "%40");
        assert_eq!(urlencode(" "), "%20");
        assert_eq!(urlencode("&"), "%26");
    }

    #[test]
    fn test_decode_query_component_raw_string_is_unchanged() {
        // Backward compat: no '%' escapes and no '+' means the decode is a no-op.
        let raw = "972542271337@s.whatsapp.net";
        assert_eq!(decode_query_component(raw), raw);
    }

    #[test]
    fn test_decode_query_component_trailing_percent_left_literal() {
        // Incomplete escape at end of string must not panic or truncate.
        assert_eq!(decode_query_component("50%"), "50%");
        assert_eq!(decode_query_component("50%4"), "50%4");
        assert_eq!(decode_query_component("%"), "%");
    }

    #[test]
    fn test_decode_query_component_decodes_valid_escapes() {
        assert_eq!(decode_query_component("972%40s.whatsapp.net"), "972@s.whatsapp.net");
        assert_eq!(decode_query_component("foo%26bar%3Dbaz"), "foo&bar=baz");
    }

    #[test]
    fn test_decode_query_component_plus_is_space() {
        // application/x-www-form-urlencoded: literal '+' in a query component means space.
        assert_eq!(decode_query_component("you+are+crazy"), "you are crazy");
    }

    #[test]
    fn test_decode_query_component_percent_2b_is_literal_plus() {
        // %2B must decode to a literal '+' and must NOT then be turned into a space.
        assert_eq!(decode_query_component("C%2B%2B"), "C++");
        assert_eq!(decode_query_component("a%2Bb+c"), "a+b c");
    }

    #[test]
    fn test_decode_query_component_mixed_percent20_and_plus() {
        assert_eq!(decode_query_component("a%20b+c"), "a b c");
    }

    #[tokio::test]
    async fn test_read_request_percent_decodes_query_params() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut server_stream, _) = listener.accept().await.unwrap();

        client
            .write_all(b"GET /api/search?q=foo%26bar&jid=972%40s.whatsapp.net HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();

        let req = read_request(&mut server_stream)
            .await
            .expect("request should parse")
            .into_rest(CallContext { loopback_file_access: true });
        assert_eq!(req.query_get("q"), Some("foo&bar"));
        assert_eq!(req.query_get("jid"), Some("972@s.whatsapp.net"));
    }

    #[tokio::test]
    async fn test_read_request_decodes_plus_as_space_in_query_params() {
        // Form-encoded clients (curl --data-urlencode, HTML forms, Python's
        // urllib.parse.urlencode) send spaces as '+' in query strings.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut server_stream, _) = listener.accept().await.unwrap();

        client
            .write_all(b"GET /api/search?q=you+are+crazy&limit=5 HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();

        let req = read_request(&mut server_stream)
            .await
            .expect("request should parse")
            .into_rest(CallContext { loopback_file_access: true });
        assert_eq!(req.query_get("q"), Some("you are crazy"));
        assert_eq!(req.query_get("limit"), Some("5"));
    }

    #[tokio::test]
    async fn test_read_request_leaves_raw_query_unchanged_for_backward_compat() {
        // Un-updated clients (and direct curl) send raw, unencoded values — verify
        // the server-side decode is a no-op when there are no '%' escapes.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut server_stream, _) = listener.accept().await.unwrap();

        client
            .write_all(b"GET /api/history?jid=972542271337@s.whatsapp.net&limit=20 HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();

        let req = read_request(&mut server_stream)
            .await
            .expect("request should parse")
            .into_rest(CallContext { loopback_file_access: true });
        assert_eq!(req.query_get("jid"), Some("972542271337@s.whatsapp.net"));
        assert_eq!(req.query_get("limit"), Some("20"));
    }

    // -------------------------------------------------------------------------
    // Head / body split of the request reader
    // -------------------------------------------------------------------------

    /// A connected loopback pair: `(client, server_side)`.
    async fn socket_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn test_request_head_keeps_body_bytes_that_arrived_with_it() {
        let (mut client, mut server) = socket_pair().await;
        // Head and the whole body leave in one write, so the head read picks the body up too.
        client
            .write_all(b"POST /api/send?sync=true HTTP/1.1\r\nHost: x\r\nContent-Length: 11\r\n\r\nhello world")
            .await
            .unwrap();

        let head = read_request_head(&mut server, head_deadline()).await.expect("head should parse");
        assert_eq!(head.method, "POST");
        assert_eq!(head.path, "/api/send");
        assert_eq!(head.query, vec![("sync".to_string(), "true".to_string())]);
        assert_eq!(head.headers.len(), 2);
        assert_eq!(head.content_length, 11);
        assert_eq!(head.leftover, b"hello world");

        // The body is already complete, so it must come back without waiting for more bytes
        // (the client sends nothing further and keeps the connection open).
        let body = tokio::time::timeout(Duration::from_secs(2), read_request_body(&mut server, &head, 1024))
            .await
            .expect("body read must not wait for more bytes")
            .expect("body should be complete");
        assert_eq!(body, b"hello world");
    }

    #[tokio::test]
    async fn test_request_head_splits_path_and_query_around_multibyte_text() {
        let (mut client, mut server) = socket_pair().await;
        // Raw (unencoded) UTF-8 on both sides of the `?` and inside the query.
        client
            .write_all("GET /שלום?q=עולם&e=😀 HTTP/1.1\r\nHost: x\r\n\r\n".as_bytes())
            .await
            .unwrap();
        let head = read_request_head(&mut server, head_deadline()).await.expect("head should parse");
        assert_eq!(head.path, "/שלום");
        assert_eq!(
            head.query,
            vec![("q".to_string(), "עולם".to_string()), ("e".to_string(), "😀".to_string())]
        );
    }

    #[tokio::test]
    async fn test_request_body_continues_across_later_reads() {
        let (mut client, mut server) = socket_pair().await;
        client
            .write_all(b"POST /api/send HTTP/1.1\r\nContent-Length: 10\r\n\r\n012")
            .await
            .unwrap();
        let head = read_request_head(&mut server, head_deadline()).await.expect("head should parse");
        assert_eq!(head.leftover, b"012", "only the first body bytes arrived with the head");

        client.write_all(b"3456").await.unwrap();
        client.write_all(b"789").await.unwrap();
        let body = read_request_body(&mut server, &head, 1024).await.expect("body should be complete");
        assert_eq!(body, b"0123456789");
    }

    #[tokio::test]
    async fn test_request_body_drops_bytes_beyond_content_length() {
        let (mut client, mut server) = socket_pair().await;
        client
            .write_all(b"POST /api/send HTTP/1.1\r\nContent-Length: 4\r\n\r\nabcdEXTRA")
            .await
            .unwrap();
        let head = read_request_head(&mut server, head_deadline()).await.expect("head should parse");
        let body = read_request_body(&mut server, &head, 1024).await.expect("body should be complete");
        assert_eq!(body, b"abcd");
    }

    #[tokio::test]
    async fn test_request_body_over_limit_is_rejected_without_reading() {
        let (mut client, mut server) = socket_pair().await;
        client.write_all(b"POST /api/send HTTP/1.1\r\nContent-Length: 10\r\n\r\n").await.unwrap();
        let head = read_request_head(&mut server, head_deadline()).await.expect("head should parse");

        // Nothing more is sent: a reader that tried to fetch the body would wait out its 10 s timeout.
        let over = tokio::time::timeout(Duration::from_secs(2), read_request_body(&mut server, &head, 9))
            .await
            .expect("over-limit rejection must not wait for the body");
        assert!(over.is_none());

        // The limit is inclusive: a body of exactly `limit` bytes is accepted.
        client.write_all(b"0123456789").await.unwrap();
        assert_eq!(read_request_body(&mut server, &head, 10).await.as_deref(), Some(&b"0123456789"[..]));
    }

    #[tokio::test]
    async fn test_request_body_truncated_by_eof_is_rejected() {
        let (mut client, mut server) = socket_pair().await;
        client
            .write_all(b"POST /api/send HTTP/1.1\r\nContent-Length: 10\r\n\r\n01234")
            .await
            .unwrap();
        let head = read_request_head(&mut server, head_deadline()).await.expect("head should parse");
        client.shutdown().await.unwrap();
        assert!(read_request_body(&mut server, &head, 1024).await.is_none());
    }

    #[tokio::test]
    async fn test_request_without_content_length_has_empty_body() {
        let (mut client, mut server) = socket_pair().await;
        client.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\nstray").await.unwrap();
        let head = read_request_head(&mut server, head_deadline()).await.expect("head should parse");
        assert_eq!(head.content_length, 0);
        assert_eq!(read_request_body(&mut server, &head, 1024).await, Some(Vec::new()));
    }

    #[tokio::test]
    async fn test_read_request_wrapper_equals_head_then_body() {
        let raw: &[u8] = b"POST /api/send?sync=true&x=a%20b HTTP/1.1\r\nHost: x\r\nX-Two: 2\r\nContent-Length: 5\r\n\r\nhello";

        let (mut client, mut server) = socket_pair().await;
        client.write_all(raw).await.unwrap();
        let whole = read_request(&mut server).await.expect("wrapper should parse");

        let (mut client, mut server) = socket_pair().await;
        client.write_all(raw).await.unwrap();
        let head = read_request_head(&mut server, head_deadline()).await.expect("head should parse");
        let body = read_request_body(&mut server, &head, max_body_bytes(&head.path)).await.expect("body");
        let split = head.into_request(body);

        assert_eq!(whole.method, split.method);
        assert_eq!(whole.path, split.path);
        assert_eq!(whole.query, split.query);
        assert_eq!(whole.headers, split.headers);
        assert_eq!(whole.body, split.body);
        assert_eq!(whole.body, b"hello");
    }

    // -------------------------------------------------------------------------
    // Head deadline, framing checks and the lingering drain
    // -------------------------------------------------------------------------

    /// A head as the parser would build it, for the framing checks.
    fn head_with(headers: &[(&str, &str)], content_length: usize, leftover: &[u8]) -> RequestHead {
        RequestHead {
            method: "POST".to_string(),
            path: "/api/send".to_string(),
            query: Vec::new(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            content_length,
            leftover: leftover.to_vec(),
        }
    }

    /// Status and `code` of a rejection, or `None` when the request may proceed.
    fn rejection(head: &RequestHead, limit: usize) -> Option<(u16, String)> {
        head.framing_error(limit).map(|r| {
            let status = r.status;
            let code = parse_response_json(&r.into_bytes())["code"].as_str().unwrap().to_string();
            (status, code)
        })
    }

    #[test]
    fn test_framing_error_accepts_plain_and_expect_continue_requests() {
        assert_eq!(rejection(&head_with(&[], 0, b""), 10), None);
        assert_eq!(rejection(&head_with(&[("Content-Length", "10")], 10, b""), 10), None, "limit is inclusive");
        for value in ["100-continue", "100-Continue", "100-CONTINUE"] {
            assert_eq!(rejection(&head_with(&[("Expect", value)], 5, b""), 10), None, "{value}");
        }
    }

    #[test]
    fn test_framing_error_rejects_any_transfer_encoding_with_411() {
        for headers in [
            vec![("Transfer-Encoding", "chunked")],
            vec![("transfer-encoding", "identity")],
            vec![("Transfer-Encoding", "")],
            vec![("Transfer-Encoding", "chunked"), ("Content-Length", "5")],
            vec![("Transfer-Encoding ", "chunked")],
            vec![(" Transfer-Encoding", "chunked")],
        ] {
            assert_eq!(rejection(&head_with(&headers, 5, b""), 10), Some((411, "length_required".to_string())), "{headers:?}");
        }
    }

    #[test]
    fn test_framing_error_oversize_content_length_is_the_plain_400() {
        let head = head_with(&[], 11, b"");
        assert_eq!(rejection(&head, 10), Some((400, "bad_request".to_string())));
        let body = parse_response_json(&head.framing_error(10).unwrap().into_bytes());
        assert_eq!(body["error"], "bad request");
    }

    #[test]
    fn test_framing_error_rejects_other_expect_values_with_417() {
        for headers in [
            vec![("Expect", "200-ok")],
            vec![("Expect", "100-continue, other")],
            vec![("Expect", "")],
            vec![("Expect", "100-continue"), ("Expect", "other")],
        ] {
            assert_eq!(rejection(&head_with(&headers, 5, b""), 10), Some((417, "expectation_failed".to_string())), "{headers:?}");
        }
    }

    #[test]
    fn test_framing_error_checks_transfer_encoding_then_size_then_expect() {
        let both = [("Transfer-Encoding", "chunked"), ("Expect", "nope")];
        assert_eq!(rejection(&head_with(&both, 99, b""), 10).map(|r| r.0), Some(411));
        assert_eq!(rejection(&head_with(&[("Expect", "nope")], 99, b""), 10).map(|r| r.0), Some(400));
        assert_eq!(rejection(&head_with(&[("Expect", "nope")], 5, b""), 10).map(|r| r.0), Some(417));
    }

    #[test]
    fn test_awaiting_continue_only_while_the_body_is_incomplete() {
        let expect = [("Expect", "100-Continue")];
        assert!(head_with(&expect, 10, b"").awaiting_continue());
        assert!(head_with(&expect, 10, b"012345678").awaiting_continue());
        assert!(!head_with(&expect, 10, b"0123456789").awaiting_continue(), "body already complete");
        assert!(!head_with(&expect, 0, b"").awaiting_continue(), "no body to wait for");
        assert!(!head_with(&[], 10, b"").awaiting_continue(), "client did not ask");
        assert!(!head_with(&[("Expect", "other")], 10, b"").awaiting_continue());
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_head_deadline_beats_the_idle_timeout() {
        let (_client, mut server) = socket_pair().await;
        // A silent client would get READ_IDLE_TIMEOUT per read; the deadline ends it sooner.
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_secs(3);
        assert!(read_request_head(&mut server, deadline).await.is_none());
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_head_idle_timeout_still_applies_within_the_deadline() {
        let (_client, mut server) = socket_pair().await;
        let start = tokio::time::Instant::now();
        assert!(read_request_head(&mut server, start + HEAD_READ_DEADLINE).await.is_none());
        assert_eq!(start.elapsed(), READ_IDLE_TIMEOUT);
    }

    /// Pipe `chunks` to [`read_request_head`] over an in-memory pipe, `gap` of virtual time
    /// apart, with the writer end held open as a live slow client's would be. Returns what the
    /// reader returned and the virtual time it took. A pipe, not a socket: a chunk still in flight
    /// in the kernel when a paused clock goes idle could lose a race against a timer.
    async fn head_from_slow_client(chunks: &[&[u8]], gap: Duration) -> (Option<RequestHead>, Duration) {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let start = tokio::time::Instant::now();
        let writer = async {
            for (i, chunk) in chunks.iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(gap).await;
                }
                client.write_all(chunk).await.unwrap();
            }
        };
        let reader = async {
            let head = read_request_head(&mut server, start + HEAD_READ_DEADLINE).await;
            (head, start.elapsed())
        };
        let (_, result) = tokio::join!(writer, reader);
        result
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_head_wall_clock_deadline_cuts_off_a_head_trickled_within_the_idle_timeout() {
        // Chunks 7 s apart never leave the reader idle for 10 s, but the blank line would only
        // arrive at 28 s: the reader gives up at exactly 20 s.
        let chunks: [&[u8]; 5] = [b"GET /health HTTP/1.1\r\n", b"X-A: 1\r\n", b"X-B: 2\r\n", b"X-C: 3\r\n", b"\r\n"];
        let (head, took) = head_from_slow_client(&chunks, Duration::from_secs(7)).await;
        assert!(head.is_none());
        assert_eq!(took, Duration::from_secs(20));
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_head_arriving_slowly_but_inside_the_deadline_is_accepted() {
        let chunks: [&[u8]; 3] = [b"GET /health HTTP/1.1\r\n", b"Host: x\r\n", b"\r\n"];
        let (head, took) = head_from_slow_client(&chunks, Duration::from_secs(7)).await;
        assert_eq!(head.expect("head completes at 14 s").path, "/health");
        assert_eq!(took, Duration::from_secs(14));
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_head_idle_gap_over_the_read_timeout_ends_it_at_10s() {
        let chunks: [&[u8]; 2] = [b"GET /health HTTP/1.1\r\n", b"\r\n"];
        let (head, took) = head_from_slow_client(&chunks, Duration::from_secs(11)).await;
        assert!(head.is_none());
        assert_eq!(took, READ_IDLE_TIMEOUT);
    }

    /// Pipe a head declaring `body.len()` bytes, then `body` in `piece`-byte chunks `gap` of
    /// virtual time apart, to the head and body readers. Returns the body result and the
    /// virtual time it took.
    async fn body_from_slow_client(body: &[u8], piece: usize, gap: Duration) -> (Option<Vec<u8>>, Duration) {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let start = tokio::time::Instant::now();
        let writer = async {
            let head = format!("POST /api/send HTTP/1.1\r\nContent-Length: {}\r\n\r\n", body.len());
            client.write_all(head.as_bytes()).await.unwrap();
            for part in body.chunks(piece) {
                tokio::time::sleep(gap).await;
                client.write_all(part).await.unwrap();
            }
        };
        let reader = async {
            let head = read_request_head(&mut server, start + HEAD_READ_DEADLINE).await.expect("head");
            let body = read_request_body(&mut server, &head, 1024).await;
            (body, start.elapsed())
        };
        let (_, result) = tokio::join!(writer, reader);
        result
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_body_has_no_total_deadline_only_a_per_read_idle_timeout() {
        // Five pieces 7 s apart: the body is complete at 35 s, past the head's 20 s deadline.
        let (body, took) = body_from_slow_client(&[b'x'; 25], 5, Duration::from_secs(7)).await;
        assert_eq!(body, Some(vec![b'x'; 25]));
        assert_eq!(took, Duration::from_secs(35));

        // A gap over the idle timeout still ends it, at 10 s.
        let (body, took) = body_from_slow_client(&[b'x'; 25], 5, Duration::from_secs(11)).await;
        assert_eq!(body, None);
        assert_eq!(took, READ_IDLE_TIMEOUT);
    }

    #[test]
    fn test_read_and_drain_limits_have_their_documented_values() {
        assert_eq!(READ_IDLE_TIMEOUT, Duration::from_secs(10));
        assert_eq!(HEAD_READ_DEADLINE, Duration::from_secs(20));
        assert_eq!(DRAIN_MAX_BYTES, 64 * 1024);
        assert_eq!(DRAIN_MAX_TIME, Duration::from_secs(1));
    }

    /// `respond_and_drain` on an in-memory pipe: returns the bytes of the pipe the helper left
    /// unread, and what the client side received (everything up to EOF).
    async fn drain_pipe(sent: usize) -> (usize, Vec<u8>) {
        let (mut client, mut server) = tokio::io::duplex(1 << 20);
        client.write_all(&vec![b'x'; sent]).await.unwrap();
        respond_and_drain(&mut server, json_err(403, "no")).await;
        // The helper stopped reading; whatever is left in the pipe is what it did not discard.
        client.shutdown().await.unwrap();
        let mut left = Vec::new();
        server.read_to_end(&mut left).await.unwrap();
        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();
        (left.len(), received)
    }

    #[tokio::test]
    async fn test_respond_and_drain_discards_up_to_the_byte_cap_and_delivers_the_response() {
        let expected = json_err(403, "no").into_bytes();

        let (left, received) = drain_pipe(10_000).await;
        assert_eq!(left, 0, "a body under the cap is discarded whole");
        assert_eq!(received, expected, "response arrives intact, then EOF");

        let (left, received) = drain_pipe(DRAIN_MAX_BYTES).await;
        assert_eq!(left, 0, "a body of exactly the cap is discarded whole");
        assert_eq!(received, expected);

        let (left, received) = drain_pipe(200 * 1024).await;
        assert_eq!(left, 200 * 1024 - DRAIN_MAX_BYTES, "stops reading at exactly 64 KiB");
        assert_eq!(received, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn test_respond_and_drain_gives_up_on_a_silent_client_after_the_time_cap() {
        let (mut client, mut server) = tokio::io::duplex(1 << 20);
        client.write_all(b"partial body").await.unwrap(); // then silence, connection left open
        let start = tokio::time::Instant::now();
        respond_and_drain(&mut server, json_err(401, "no")).await;
        assert_eq!(start.elapsed(), DRAIN_MAX_TIME);

        // The client still got the response and the server's EOF while it was silent.
        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, json_err(401, "no").into_bytes());
    }

    #[tokio::test]
    async fn test_respond_and_drain_returns_at_once_when_the_client_closes() {
        let (mut client, mut server) = tokio::io::duplex(1 << 20);
        client.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_millis(500), respond_and_drain(&mut server, json_err(403, "no")))
            .await
            .expect("EOF from the client ends the drain without waiting for the time cap");
    }

    #[test]
    fn test_max_body_bytes_media_prefixes_get_the_larger_limit() {
        const DEFAULT: usize = 1024 * 1024;
        const MEDIA: usize = 70 * 1024 * 1024;
        for path in [
            "/api/image",
            "/api/video",
            "/api/audio",
            "/api/doc",
            "/api/sticker",
            "/api/view-once-image",
            "/api/view-once-video",
            "/api/status-image",
            "/api/status-video",
        ] {
            assert_eq!(max_body_bytes(path), MEDIA, "{path}");
        }
        for path in ["/api/send", "/api/status", "/api/status-text", "/api/history-fetch", "/health", "/"] {
            assert_eq!(max_body_bytes(path), DEFAULT, "{path}");
        }
    }

    // -------------------------------------------------------------------------
    // ADR 0044: CSRF / DNS-rebinding / Content-Type gate — unit tests
    // -------------------------------------------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn test_gate_rejects_disallowed_origin() {
        let h = headers(&[("Origin", "https://evil.example"), ("Host", "127.0.0.1:7270")]);
        let err = gate_request("GET", &h, true, &[], &[]).unwrap_err();
        assert_eq!(err.0, 403);
    }

    #[test]
    fn test_gate_rejects_null_origin() {
        let h = headers(&[("Origin", "null"), ("Host", "127.0.0.1:7270")]);
        let err = gate_request("GET", &h, true, &[], &[]).unwrap_err();
        assert_eq!(err.0, 403);
    }

    #[test]
    fn test_gate_allows_allowlisted_origin() {
        let h = headers(&[("Origin", "https://trusted.example"), ("Host", "127.0.0.1:7270")]);
        let allowed = vec!["https://trusted.example".to_string()];
        assert!(gate_request("GET", &h, true, &allowed, &[]).is_ok());
    }

    #[test]
    fn test_gate_checks_host_even_with_allowlisted_origin() {
        // An allowlisted Origin must not skip Host validation (DNS rebinding defence).
        let allowed = vec!["https://trusted.example".to_string()];
        let evil_host = headers(&[("Origin", "https://trusted.example"), ("Host", "evil.example")]);
        assert_eq!(gate_request("GET", &evil_host, true, &allowed, &[]).unwrap_err().0, 403);
        let no_host = headers(&[("Origin", "https://trusted.example")]);
        assert_eq!(gate_request("GET", &no_host, true, &allowed, &[]).unwrap_err().0, 403);
    }

    #[test]
    fn test_gate_rejects_duplicate_origin() {
        let allowed = vec!["https://trusted.example".to_string()];
        let h = headers(&[
            ("Origin", "https://trusted.example"),
            ("Origin", "https://evil.example"),
            ("Host", "127.0.0.1:7270"),
        ]);
        let err = gate_request("GET", &h, true, &allowed, &[]).unwrap_err();
        assert_eq!(err, (403, "duplicate Origin header".to_string()));
    }

    #[test]
    fn test_gate_content_type_check_is_method_case_insensitive() {
        let h = headers(&[("Host", "127.0.0.1:7270"), ("Content-Type", "text/plain")]);
        for m in ["POST", "post", "Post"] {
            assert_eq!(gate_request(m, &h, true, &[], &[]).unwrap_err().0, 415, "method {m}");
        }
    }

    #[test]
    fn test_gate_rejects_evil_host_on_loopback() {
        let h = headers(&[("Host", "evil.example")]);
        let err = gate_request("GET", &h, true, &[], &[]).unwrap_err();
        assert_eq!(err.0, 403);
    }

    #[test]
    fn test_gate_allows_loopback_host_variants() {
        for host in ["127.0.0.1:7270", "localhost", "[::1]:7270", "LOCALHOST."] {
            let h = headers(&[("Host", host)]);
            assert!(gate_request("GET", &h, true, &[], &[]).is_ok(), "Host {host} should be allowed");
        }
    }

    #[test]
    fn test_gate_rejects_duplicate_host() {
        let h = headers(&[("Host", "127.0.0.1"), ("Host", "evil.example")]);
        let err = gate_request("GET", &h, true, &[], &[]).unwrap_err();
        assert_eq!(err.0, 403);
        assert!(err.1.contains("duplicate"), "expected a duplicate-Host rejection, got: {}", err.1);
    }

    #[test]
    fn test_gate_rejects_missing_host_on_loopback() {
        let h: Vec<(String, String)> = Vec::new();
        let err = gate_request("GET", &h, true, &[], &[]).unwrap_err();
        assert_eq!(err.0, 403);
    }

    #[test]
    fn test_gate_allows_odd_host_on_nonloopback_bind_without_allowlist() {
        // Non-loopback bind + no ALLOWED_HOSTS configured => Host check is skipped by default.
        let h = headers(&[("Host", "whatever.example")]);
        assert!(gate_request("GET", &h, false, &[], &[]).is_ok());
    }

    #[test]
    fn test_gate_nonloopback_bind_with_allowlist_enforces_it() {
        let allowed = vec!["proxy.example".to_string()];
        let bad = headers(&[("Host", "whatever.example")]);
        assert!(gate_request("GET", &bad, false, &[], &allowed).is_err());
        let good = headers(&[("Host", "proxy.example")]);
        assert!(gate_request("GET", &good, false, &[], &allowed).is_ok());
    }

    #[test]
    fn test_gate_rejects_post_without_json_content_type() {
        for ct in ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data; boundary=x"] {
            let h = headers(&[("Host", "127.0.0.1"), ("Content-Type", ct)]);
            let err = gate_request("POST", &h, true, &[], &[]).unwrap_err();
            assert_eq!(err.0, 415, "Content-Type {ct} should be rejected");
        }
        // Missing Content-Type entirely.
        let h = headers(&[("Host", "127.0.0.1")]);
        let err = gate_request("POST", &h, true, &[], &[]).unwrap_err();
        assert_eq!(err.0, 415);
    }

    #[test]
    fn test_gate_allows_post_with_json_content_type_and_charset() {
        let h = headers(&[("Host", "127.0.0.1"), ("Content-Type", "application/json; charset=utf-8")]);
        assert!(gate_request("POST", &h, true, &[], &[]).is_ok());
    }

    #[test]
    fn test_gate_allows_get_without_content_type() {
        let h = headers(&[("Host", "127.0.0.1")]);
        assert!(gate_request("GET", &h, true, &[], &[]).is_ok());
    }

    #[test]
    fn test_normalize_host_header_edge_cases() {
        assert_eq!(normalize_host_header("127.0.0.1:7270"), "127.0.0.1");
        assert_eq!(normalize_host_header("LOCALHOST."), "localhost");
        assert_eq!(normalize_host_header("[::1]:7270"), "::1");
        assert_eq!(normalize_host_header("::1"), "::1"); // bare IPv6, no brackets — left intact, not mangled as host:port
        assert_eq!(normalize_host_header("localhost."), "localhost");
        assert_eq!(normalize_host_header("127.0.0.1."), "127.0.0.1");
    }

    #[test]
    fn test_normalize_host_header_malformed_and_multibyte_never_panics() {
        // Unclosed bracket, bare brackets, empty/non-numeric port: value is kept as-is.
        assert_eq!(normalize_host_header("[::1"), "[::1");
        assert_eq!(normalize_host_header("[::1]"), "::1");
        assert_eq!(normalize_host_header("[]:80"), "");
        assert_eq!(normalize_host_header("localhost:"), "localhost:");
        assert_eq!(normalize_host_header("localhost:80a"), "localhost:80a");
        // Multi-byte text around every split point (`[`, `]`, `:`) — Hebrew is 2 bytes/char,
        // the emoji 4. ASCII is lowercased, everything else is left alone.
        assert_eq!(normalize_host_header("שלום:7270"), "שלום");
        assert_eq!(normalize_host_header("שלום:ש"), "שלום:ש");
        assert_eq!(normalize_host_header("[שלום]:7270"), "שלום");
        assert_eq!(normalize_host_header("[😀"), "[😀");
        assert_eq!(normalize_host_header("ש:"), "ש:");
        assert_eq!(normalize_host_header("Ünï.EXAMPLE.:80"), "Ünï.example");
    }

    // -------------------------------------------------------------------------
    // ADR 0044: gate integration — through the real read_request parser +
    // (for the passing case) the real handle_request dispatch.
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_csrf_post_with_origin_and_text_plain_is_rejected_before_handler() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut server_stream, _) = listener.accept().await.unwrap();

        // Mirrors `fetch(url, {method:"POST", mode:"no-cors", body, headers:{"Content-Type":"text/plain"}})`
        // from an attacker page — no preflight is sent, and the cross-site Origin is present.
        let body = r#"{"jid":"1@s.whatsapp.net","text":"pwned"}"#;
        let raw = format!(
            "POST /api/send HTTP/1.1\r\nHost: 127.0.0.1:7270\r\nOrigin: https://evil.example\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        client.write_all(raw.as_bytes()).await.unwrap();

        let req = read_request(&mut server_stream).await.expect("request should parse");
        let decision = gate_request(&req.method, &req.headers, true, &[], &[]);
        assert_eq!(
            decision,
            Err((403, "Origin 'https://evil.example' is not allowed".to_string()))
        );
        // Structural guarantee (see serve_connection()): it returns on this
        // Err immediately, before ever calling handle_request/handle_send — so the
        // attack never reaches a handler that could act on it.
    }

    #[tokio::test]
    async fn test_equivalent_request_without_origin_passes_gate_then_hits_bridge_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut server_stream, _) = listener.accept().await.unwrap();

        // /api/pin-chat calls the WhatsApp client directly (unlike /api/send's
        // SQLite-first async queue, which accepts the job regardless of connection
        // state) — with no client connected it deterministically fails with
        // "not connected" => 503, which is what we want to observe here.
        let body = r#"{"jid":"1@s.whatsapp.net"}"#;
        let raw = format!(
            "POST /api/pin-chat HTTP/1.1\r\nHost: 127.0.0.1:7270\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        client.write_all(raw.as_bytes()).await.unwrap();

        let req = read_request(&mut server_stream).await.expect("request should parse");
        assert!(gate_request(&req.method, &req.headers, true, &[], &[]).is_ok());

        // The gate passed — dispatch to the real handler. With no WhatsApp client
        // connected, it fails on bridge state (503); that's expected and fine —
        // the gate's job is CSRF/rebinding/content-type defence, not connectivity.
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let db_path = std::env::temp_dir().join(format!("whatsrust-gate-test-{ts}.db"));
        let bridge = crate::bridge::build_test_bridge_with_embedder(
            &db_path,
            crate::embedder::FakeEmbedder::new("test-model", 8),
        )
        .await;

        let req = req.into_rest(CallContext { loopback_file_access: true });
        let response = handle_request(&bridge, &req).await.into_bytes();
        assert_eq!(http_status_of(&response), 503, "expected a bridge-state failure, not a gate rejection");

        let _ = std::fs::remove_file(&db_path);
    }

    // -------------------------------------------------------------------------
    // ADR 0044: whatsrust's own CLI client must keep passing the gate
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_cli_get_request_passes_the_gate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let req = read_request(&mut stream).await.expect("request should parse");
            let decision = gate_request(&req.method, &req.headers, true, &[], &[]);
            let _ = stream.write_all(&json_err(503, "test stub").into_bytes()).await;
            decision
        });
        let _ = cli_get(port, "/api/status").await;
        let decision = server.await.unwrap();
        assert!(decision.is_ok(), "cli_get's request must pass the gate: {decision:?}");
    }

    #[tokio::test]
    async fn test_cli_post_request_passes_the_gate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let req = read_request(&mut stream).await.expect("request should parse");
            let decision = gate_request(&req.method, &req.headers, true, &[], &[]);
            let _ = stream.write_all(&json_err(503, "test stub").into_bytes()).await;
            decision
        });
        let _ = cli_post(port, "/api/send", r#"{"jid":"1@s.whatsapp.net","text":"hi"}"#).await;
        let decision = server.await.unwrap();
        assert!(decision.is_ok(), "cli_post's request must pass the gate: {decision:?}");
    }

    // -------------------------------------------------------------------------
    // Sync-send timeout: typed SendTimedOut -> 504 with job_id + "do not retry",
    // and clients that outwait the daemon (and warn on POST read failures).
    // -------------------------------------------------------------------------

    fn assert_send_timeout_response(raw: &[u8], job_id: i64) {
        assert_eq!(http_status_of(raw), 504);
        let body = parse_response_json(raw);
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["code"], json!("timeout"));
        assert_eq!(body["job_id"], json!(job_id));
        let msg = body["error"].as_str().expect("error message");
        assert!(msg.contains("timed out"), "{msg}");
        assert!(msg.contains("still queued"), "{msg}");
        assert!(msg.contains("do not retry"), "{msg}");
        assert!(msg.contains(&format!("job {job_id}")), "{msg}");
    }

    #[test]
    fn test_bridge_err_send_timed_out_is_504_with_job_id_and_do_not_retry() {
        let raw = bridge_err(anyhow::Error::new(SendTimedOut { job_id: 42 })).into_bytes();
        assert_send_timeout_response(&raw, 42);
    }

    #[test]
    fn test_bridge_err_finds_send_timed_out_under_context_layers() {
        let wrapped = anyhow::Error::new(SendTimedOut { job_id: 7 })
            .context("sending reply")
            .context("handling request");
        let raw = bridge_err(wrapped).into_bytes();
        assert_send_timeout_response(&raw, 7);
    }

    #[test]
    fn test_bridge_err_other_errors_keep_their_status_and_carry_no_job_id() {
        let cases = [
            ("client not connected", 503),
            ("no client", 503),
            ("bad JID: x", 400),
            ("empty JID", 400),
            ("sender_jid required for group reactions", 400),
            ("upstream request timed out", 504), // old string mapping is unchanged
            ("boom", 500),
        ];
        for (msg, status) in cases {
            let raw = bridge_err(anyhow::anyhow!(msg)).into_bytes();
            assert_eq!(http_status_of(&raw), status, "{msg}");
            assert!(parse_response_json(&raw).get("job_id").is_none(), "{msg}: no job_id without a typed timeout");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_sync_send_timeout_over_rest_returns_504_with_the_queued_job_id() {
        // Full path: handle_send(sync) -> enqueue_and_wait (no worker, virtual 30s) -> bridge_err.
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let db_path = std::env::temp_dir().join(format!("whatsrust-sync-timeout-test-{ts}.db"));
        let bridge = crate::bridge::build_test_bridge_with_embedder(
            &db_path,
            crate::embedder::FakeEmbedder::new("test-model", 8),
        )
        .await;

        let response = handle_send(&bridge, br#"{"jid":"1@s.whatsapp.net","text":"hi"}"#, true).await.into_bytes();

        let queued = bridge
            .store()
            .claim_next_job()
            .await
            .unwrap()
            .expect("the timed-out job must still be queued");
        assert_send_timeout_response(&response, queued.id);

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test(start_paused = true)]
    async fn test_cli_post_client_timeout_says_request_may_still_be_processed() {
        // Accepts at the kernel level (listen backlog) but never answers.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let start = tokio::time::Instant::now();
        let err = cli_post(port, "/api/send", r#"{"jid":"1@s.whatsapp.net","text":"hi"}"#)
            .await
            .expect_err("no reply, so the client must time out");

        assert!(start.elapsed() >= SYNC_CLIENT_TIMEOUT, "must wait the shared client timeout");
        let msg = err.to_string();
        assert!(msg.contains("timeout reading response"), "{msg}");
        assert!(msg.contains("may still be processed"), "{msg}");
        drop(listener);
    }

    // -------------------------------------------------------------------------
    // F3 Phase 0b: REST listener startup rules (pure, no environment)
    // -------------------------------------------------------------------------

    /// Owned allowlist entries for the startup-rule tests.
    fn allowlist(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// A startup refusal names every `needles` entry and ends with the closing line.
    fn assert_refusal(msg: &str, needles: &[&str]) {
        assert!(msg.ends_with("Refusing to start."), "{msg}");
        for needle in needles {
            assert!(msg.contains(needle), "message must contain {needle:?}: {msg}");
        }
    }

    #[test]
    fn test_startup_allowlist_without_token_is_refused() {
        let hosts = allowlist(&["proxy.example"]);
        let origins = allowlist(&["https://proxy.example"]);

        let msg = validate_api_startup(7270, "127.0.0.1", false, None, &hosts, &[]).unwrap_err();
        assert_refusal(&msg, &["WHATSRUST_ALLOWED_HOSTS", "WHATSRUST_API_TOKEN", "openssl rand -base64 32"]);
        assert!(!msg.contains("WHATSRUST_ALLOWED_ORIGINS"), "{msg}");

        let msg = validate_api_startup(7270, "127.0.0.1", false, None, &[], &origins).unwrap_err();
        assert_refusal(&msg, &["WHATSRUST_ALLOWED_ORIGINS", "WHATSRUST_API_TOKEN", "openssl rand -base64 32"]);
        assert!(!msg.contains("WHATSRUST_ALLOWED_HOSTS"), "{msg}");

        let msg = validate_api_startup(7270, "127.0.0.1", false, None, &hosts, &origins).unwrap_err();
        assert_refusal(&msg, &["WHATSRUST_ALLOWED_HOSTS and WHATSRUST_ALLOWED_ORIGINS", "WHATSRUST_API_TOKEN"]);
    }

    #[test]
    fn test_startup_allowlist_with_token_is_ok() {
        let hosts = allowlist(&["proxy.example"]);
        let origins = allowlist(&["https://proxy.example"]);
        assert!(validate_api_startup(7270, "127.0.0.1", false, Some("secret"), &hosts, &origins).is_ok());
    }

    #[test]
    fn test_startup_non_loopback_without_allow_remote_is_refused() {
        // With or without a token: the missing opt-in is reported first.
        for token in [None, Some("secret")] {
            let msg = validate_api_startup(7270, "0.0.0.0", false, token, &[], &[]).unwrap_err();
            assert_refusal(
                &msg,
                &["WHATSRUST_BIND=0.0.0.0", "WHATSRUST_ALLOW_REMOTE=1", "WHATSRUST_API_TOKEN", "openssl rand -base64 32"],
            );
        }
    }

    #[test]
    fn test_startup_non_loopback_allow_remote_without_token_is_refused() {
        let msg = validate_api_startup(7270, "192.168.1.10", true, None, &[], &[]).unwrap_err();
        assert_refusal(&msg, &["WHATSRUST_BIND=192.168.1.10", "WHATSRUST_API_TOKEN", "openssl rand -base64 32"]);
        assert!(!msg.contains("WHATSRUST_ALLOW_REMOTE is not set"), "the opt-in is present: {msg}");
    }

    #[test]
    fn test_startup_non_loopback_allow_remote_with_token_is_ok() {
        assert!(validate_api_startup(7270, "0.0.0.0", true, Some("secret"), &[], &[]).is_ok());
    }

    #[test]
    fn test_startup_loopback_default_needs_no_token() {
        for bind in ["127.0.0.1", "::1", "localhost"] {
            assert!(validate_api_startup(7270, bind, false, None, &[], &[]).is_ok(), "{bind}");
            // ALLOW_REMOTE only matters for a non-loopback bind.
            assert!(validate_api_startup(7270, bind, true, None, &[], &[]).is_ok(), "{bind}");
        }
    }

    #[test]
    fn test_startup_headless_port_zero_skips_every_rule() {
        let hosts = allowlist(&["proxy.example"]);
        let origins = allowlist(&["https://proxy.example"]);
        assert!(validate_api_startup(0, "0.0.0.0", false, None, &hosts, &origins).is_ok());
        assert!(validate_api_startup(0, "0.0.0.0", true, None, &[], &[]).is_ok());
        assert!(validate_api_startup(0, "127.0.0.1", false, None, &hosts, &[]).is_ok());
    }

    #[test]
    fn test_startup_whitespace_only_token_counts_as_unset() {
        let hosts = allowlist(&["proxy.example"]);
        for blank in ["", " ", "   ", "\t\n"] {
            assert!(
                validate_api_startup(7270, "127.0.0.1", false, Some(blank), &hosts, &[]).is_err(),
                "allowlist with token {blank:?}"
            );
            assert!(
                validate_api_startup(7270, "0.0.0.0", true, Some(blank), &[], &[]).is_err(),
                "remote bind with token {blank:?}"
            );
        }
        // Padding around a real token is trimmed away, not rejected.
        assert!(validate_api_startup(7270, "0.0.0.0", true, Some("  secret  "), &hosts, &[]).is_ok());
    }

    #[test]
    fn test_listener_rules_have_no_headless_case() {
        // `serve` has no port-0 headless mode, so it calls the rules without the port gate.
        assert!(validate_api_listener("0.0.0.0", false, Some("secret"), &[], &[]).is_err());
        assert!(validate_api_listener("127.0.0.1", false, None, &[], &[]).is_ok());
    }

    // -------------------------------------------------------------------------
    // F3 Phase 0b: binding the REST listener
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_bind_api_listener_binds_a_free_port() {
        let listener = bind_api_listener("127.0.0.1", 0).await.expect("a free port binds");
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn test_bind_api_listener_port_in_use_names_the_fix() {
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = taken.local_addr().unwrap().port();

        let msg = bind_api_listener("127.0.0.1", port).await.expect_err("the port is taken");

        assert_refusal(
            &msg,
            &[
                "WHATSRUST_BIND=127.0.0.1",
                &format!("WHATSRUST_PORT={port}"),
                &format!("Port {port} is already in use"),
                "stop the other process or set WHATSRUST_PORT",
                "(os error",
            ],
        );
    }

    #[tokio::test]
    async fn test_bind_api_listener_other_failure_names_bind_and_port() {
        // TEST-NET-3 is not an address of this machine, so the OS refuses the bind.
        let msg = bind_api_listener("203.0.113.1", 7270).await.expect_err("not a local address");

        assert_refusal(&msg, &["WHATSRUST_BIND=203.0.113.1", "WHATSRUST_PORT=7270", "Check that WHATSRUST_BIND"]);
        assert!(!msg.contains("already in use"), "{msg}");
    }

    // -------------------------------------------------------------------------
    // F3 Phase 0b: library entry points
    // -------------------------------------------------------------------------

    /// A disconnected test bridge and its database path (the caller removes the file).
    async fn test_bridge_for_serve(tag: &str) -> (Arc<WhatsAppBridge>, std::path::PathBuf) {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let db_path = std::env::temp_dir().join(format!("whatsrust-serve-{tag}-{ts}.db"));
        let bridge = crate::bridge::build_test_bridge_with_embedder(
            &db_path,
            crate::embedder::FakeEmbedder::new("test-model", 8),
        )
        .await;
        (Arc::new(bridge), db_path)
    }

    /// `serve` reads its settings from the environment, which tests must not change, so this
    /// drives the bind-failure path; a refused configuration also just returns. The refused
    /// path itself is covered by the `validate_api_listener` tests, which `serve` calls with
    /// no copy of the rules.
    #[tokio::test]
    async fn test_serve_logs_and_returns_when_it_cannot_listen() {
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = taken.local_addr().unwrap().port();
        let (bridge, db_path) = test_bridge_for_serve("bind-fail").await;

        tokio::time::timeout(Duration::from_secs(5), serve(bridge, port, CancellationToken::new()))
            .await
            .expect("serve must return, not run or exit, when it cannot listen");

        drop(taken);
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn test_serve_with_listener_serves_the_given_listener_until_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bridge, db_path) = test_bridge_for_serve("listener").await;
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve_with_listener(bridge, listener, cancel.clone()));

        // Whatever the environment's token or allowlists say, the accept loop answers in HTTP.
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .expect("the server answers")
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 "), "{}", String::from_utf8_lossy(&response));

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the accept loop stops on cancel")
            .unwrap();
        let _ = std::fs::remove_file(&db_path);
    }
}
