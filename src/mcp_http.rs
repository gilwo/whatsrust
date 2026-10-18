//! The opt-in MCP Streamable HTTP listener. It serves only `POST /mcp`, statelessly (no
//! `Mcp-Session-Id` is ever issued), on its own port with its own token, bind address and
//! connection cap. This phase implements the legacy-era handshake (`initialize`, `ping`,
//! `tools/list`) and accepts notifications; `tools/call` is a stub until Phase 3.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::api::{
    bad_request, bool_env_var, gate_request, is_loopback_bind, json_err, parse_allowlist_env, read_request_body,
    read_request_head, request_has_api_token, respond_and_drain, token_from_env, ApiResponse, RequestHead,
    CONTINUE_RESPONSE, HEAD_READ_DEADLINE,
};
use crate::mcp_registry::REGISTRY;

/// Most connections the listener serves at once; the rest get 503.
const MAX_CONNECTIONS: usize = 32;
/// Largest request body (the JSON-RPC envelope) the listener reads.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// Shortest time, from accept, a request body may take to arrive.
const BODY_BASE_BUDGET: Duration = Duration::from_secs(20);
/// Longest time, from accept, a request body may take to arrive.
const BODY_MAX_BUDGET: Duration = Duration::from_secs(60);
/// Extra body time per MiB of declared `Content-Length`, in milliseconds.
const BODY_MS_PER_MIB: u64 = 4_000;
/// Fewest characters a `WHATSRUST_MCP_TOKEN` may have (128 bits as base64).
const MIN_TOKEN_CHARS: usize = 22;
/// The one path the listener serves.
const MCP_PATH: &str = "/mcp";

/// Protocol versions of the legacy era (`initialize` handshake), newest first. The first entry
/// is the counter-offer for an `initialize` that asks for anything else.
const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];
/// The modern era's protocol version (no handshake; every request carries it).
const MODERN_VERSION: &str = "2026-07-28";
/// Every protocol version this endpoint serves: `data.supported` of each `-32022` error.
/// Phase 4 (modern era) adds [`MODERN_VERSION`] here, which also lets [`detect_era`] return
/// [`Era::Modern`].
const SUPPORTED_VERSIONS: &[&str] = LEGACY_VERSIONS;
/// Request header that carries the protocol version.
const VERSION_HEADER: &str = "mcp-protocol-version";
/// Flat key in `params._meta` that marks a modern-era request.
const META_VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const HEADER_MISMATCH: i64 = -32020;
const UNSUPPORTED_VERSION: i64 = -32022;

// ---------------------------------------------------------------------------
// Settings and startup rules
// ---------------------------------------------------------------------------

/// MCP listener settings read from the environment.
struct McpSettings {
    /// `WHATSRUST_MCP_PORT`: `Ok(0)` is off; `Err` holds the text of an unparseable value.
    port: Result<u16, String>,
    bind: String,
    allow_remote: bool,
    read_only: bool,
    token: Option<String>,
    allowed_origins: Vec<String>,
    allowed_hosts: Vec<String>,
}

impl McpSettings {
    fn from_env() -> Self {
        let port = match std::env::var("WHATSRUST_MCP_PORT") {
            Ok(raw) => parse_mcp_port(Some(&raw)),
            Err(std::env::VarError::NotPresent) => Ok(0),
            Err(std::env::VarError::NotUnicode(raw)) => Err(raw.to_string_lossy().into_owned()),
        };
        Self {
            port,
            bind: parse_mcp_bind(std::env::var("WHATSRUST_MCP_BIND").ok().as_deref()),
            allow_remote: bool_env_var("WHATSRUST_MCP_ALLOW_REMOTE"),
            read_only: bool_env_var("WHATSRUST_MCP_READ_ONLY"),
            token: token_from_env("WHATSRUST_MCP_TOKEN"),
            allowed_origins: parse_allowlist_env("WHATSRUST_ALLOWED_ORIGINS"),
            allowed_hosts: parse_allowlist_env("WHATSRUST_ALLOWED_HOSTS"),
        }
    }
}

/// The MCP port from the raw `WHATSRUST_MCP_PORT` text: unset, empty and `0` are off (`Ok(0)`);
/// anything else must be a `u16`, else `Err` with the trimmed text.
fn parse_mcp_port(raw: Option<&str>) -> Result<u16, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(0),
        Some(value) => value.parse::<u16>().map_err(|_| value.to_string()),
    }
}

/// The MCP bind host from the raw `WHATSRUST_MCP_BIND` text: trimmed, default `127.0.0.1`.
fn parse_mcp_bind(raw: Option<&str>) -> String {
    match raw.map(str::trim) {
        None | Some("") => "127.0.0.1".to_string(),
        Some(value) => value.to_string(),
    }
}

/// The MCP listener's port from `WHATSRUST_MCP_PORT`; 0 means off. An unparseable value also
/// reads as 0 here: [`validate_mcp_startup_from_env`] refuses it before anything binds.
pub fn mcp_port_from_env() -> u16 {
    McpSettings::from_env().port.unwrap_or(0)
}

/// The MCP listener's bind host from `WHATSRUST_MCP_BIND` (default `127.0.0.1`).
pub fn mcp_bind_host() -> String {
    McpSettings::from_env().bind
}

/// Startup rules for the MCP listener (design doc §13). Port `Ok(0)` is off, so nothing is
/// checked; otherwise, in order: an unparseable port, a non-loopback bind without
/// `allow_remote`, a missing token, a token under [`MIN_TOKEN_CHARS`] characters, the MCP token
/// equal to the REST token, and the MCP port equal to the REST port (the last two only while
/// the REST listener is on, `rest_port > 0`). A token that is empty or only whitespace counts
/// as unset. The `Err` text is the whole message: what is wrong, the exact fix, "Refusing to
/// start." It never contains a token.
fn validate_mcp_startup(
    port: &Result<u16, String>,
    bind: &str,
    allow_remote: bool,
    token: Option<&str>,
    rest_port: u16,
    rest_token: Option<&str>,
) -> Result<(), String> {
    let port = match port {
        Ok(0) => return Ok(()),
        Ok(port) => *port,
        Err(raw) => {
            return Err(format!(
                "WHATSRUST_MCP_PORT is not a valid port number (got: {raw}). Set it to a port such as \
                 7271, or unset it to turn the MCP listener off. Refusing to start."
            ));
        }
    };
    if !is_loopback_bind(bind) && !allow_remote {
        return Err(format!(
            "WHATSRUST_MCP_BIND={bind} is not a loopback address, but WHATSRUST_MCP_ALLOW_REMOTE is not set.\n\
             To expose the MCP listener on the network, set WHATSRUST_MCP_ALLOW_REMOTE=1 and \
             WHATSRUST_MCP_TOKEN (e.g. `openssl rand -base64 32`); to keep it local, unset \
             WHATSRUST_MCP_BIND or set WHATSRUST_MCP_BIND=127.0.0.1. Refusing to start."
        ));
    }
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Err(format!(
            "WHATSRUST_MCP_PORT={port} turns the MCP listener on, but WHATSRUST_MCP_TOKEN is not set.\n\
             The MCP listener always requires a token. Set WHATSRUST_MCP_TOKEN (e.g. \
             `openssl rand -base64 32`), or unset WHATSRUST_MCP_PORT to turn the MCP listener off. \
             Refusing to start."
        ));
    };
    let chars = token.chars().count();
    if chars < MIN_TOKEN_CHARS {
        return Err(format!(
            "WHATSRUST_MCP_TOKEN must be at least {MIN_TOKEN_CHARS} characters (currently: {chars}); \
             generate with `openssl rand -base64 32`. Refusing to start."
        ));
    }
    if rest_port > 0 {
        if rest_token.map(str::trim) == Some(token) {
            return Err(
                "WHATSRUST_MCP_TOKEN must differ from WHATSRUST_API_TOKEN, so that a leaked MCP token \
                 cannot open the REST API.\n\
                 Set a different WHATSRUST_MCP_TOKEN (e.g. `openssl rand -base64 32`). Refusing to start."
                    .to_string(),
            );
        }
        if port == rest_port {
            return Err("WHATSRUST_MCP_PORT must differ from WHATSRUST_PORT. Refusing to start.".to_string());
        }
    }
    Ok(())
}

/// [`validate_mcp_startup`] on the environment's settings, for a REST listener on `rest_port`
/// (0 = off). The daemon calls it before the bridge is built and exits with the `Err` text.
pub fn validate_mcp_startup_from_env(rest_port: u16) -> Result<(), String> {
    let s = McpSettings::from_env();
    validate_mcp_startup(
        &s.port,
        &s.bind,
        s.allow_remote,
        s.token.as_deref(),
        rest_port,
        crate::api::configured_api_token().as_deref(),
    )
}

/// Bind the MCP listener to `bind:port`. The `Err` text is the whole startup message: the
/// variables involved, the OS error and the fix. A non-loopback bind (which the startup rules
/// only allow with `WHATSRUST_MCP_ALLOW_REMOTE`) logs a warning about cleartext.
pub async fn bind_mcp_listener(bind: &str, port: u16) -> Result<TcpListener, String> {
    match TcpListener::bind((bind, port)).await {
        Ok(listener) => {
            info!(bind = %bind, port = port, "MCP server listening");
            if !is_loopback_bind(bind) {
                warn!(
                    bind = %bind,
                    port = port,
                    "MCP listener is reachable from the network: the token and all message content \
                     travel in cleartext unless a TLS-terminating proxy is in front"
                );
            }
            Ok(listener)
        }
        Err(e) => {
            let fix = if e.kind() == std::io::ErrorKind::AddrInUse {
                format!(
                    "Port {port} is already in use; stop the other process or set WHATSRUST_MCP_PORT to a \
                     free port (unset it to turn the MCP listener off)."
                )
            } else {
                "Check that WHATSRUST_MCP_BIND is an address of this machine and that WHATSRUST_MCP_PORT \
                 is allowed (unset it to turn the MCP listener off)."
                    .to_string()
            };
            Err(format!(
                "Cannot bind the MCP listener (WHATSRUST_MCP_BIND={bind}, WHATSRUST_MCP_PORT={port}): {e}.\n\
                 {fix} Refusing to start."
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC responses
// ---------------------------------------------------------------------------

/// A JSON-RPC error and the HTTP status it travels with.
#[derive(Debug)]
struct RpcError {
    status: u16,
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(status: u16, code: i64, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), data: None }
    }

    fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// One JSON-RPC 2.0 response, `{"jsonrpc":"2.0","id":..,<member>:<payload>}` with `member`
/// `"result"` or `"error"`, as an `application/json` HTTP response. Every response this
/// listener builds goes through here.
fn rpc_response(status: u16, id: &Value, member: &str, payload: Value) -> ApiResponse {
    let body = json!({ "jsonrpc": "2.0", "id": id, member: payload });
    ApiResponse::new(status, "application/json", body.to_string().into_bytes())
}

/// A successful JSON-RPC response (HTTP 200).
fn rpc_ok(id: &Value, result: Value) -> ApiResponse {
    rpc_response(200, id, "result", result)
}

/// The JSON-RPC error response for `err`, echoing `id` (`null` when the request had no usable
/// one), with `headers` after the standard ones. Header values must be constants, so no CR/LF
/// can reach the wire.
fn rpc_error(err: RpcError, id: &Value, headers: &[(&'static str, &'static str)]) -> ApiResponse {
    let mut error = json!({ "code": err.code, "message": err.message });
    if let Some(data) = err.data {
        error["data"] = data;
    }
    let mut response = rpc_response(err.status, id, "error", error);
    response.headers.extend(headers.iter().map(|(name, value)| (*name, (*value).to_string())));
    response
}

/// `-32022`: the requested protocol version is not one of [`SUPPORTED_VERSIONS`].
fn unsupported_version(requested: &str) -> RpcError {
    RpcError::new(400, UNSUPPORTED_VERSION, "Unsupported protocol version")
        .with_data(json!({ "supported": SUPPORTED_VERSIONS, "requested": requested }))
}

// ---------------------------------------------------------------------------
// Era detection
// ---------------------------------------------------------------------------

/// Which protocol era a request speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Era {
    /// `2025-11-25` / `2025-06-18`: `initialize` handshake.
    Legacy,
    /// `2026-07-28`: no handshake, version on every request.
    Modern,
}

/// Decide the era of a request, in fixed precedence (design doc §5):
/// 1. the `MCP-Protocol-Version` header (name case-insensitive; more than one is `-32020`):
///    a legacy version is [`Era::Legacy`], `2026-07-28` is [`Era::Modern`] once it is in
///    [`SUPPORTED_VERSIONS`] (else `-32022`), and any other value is [`Era::Legacy`] for
///    `initialize` (the server counter-offers) and `-32022` for every other method;
/// 2. without the header, the flat `params._meta["io.modelcontextprotocol/protocolVersion"]`
///    key marks a modern request that lacks its required header: `-32020`. A `_meta` object
///    without that key (e.g. only `progressToken`) is not a modern signal;
/// 3. without either, `initialize` is [`Era::Legacy`];
/// 4. anything else is `-32600`: the header is required.
fn detect_era(headers: &[(String, String)], body: &Value, method: &str) -> Result<Era, RpcError> {
    let mut versions = headers
        .iter()
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case(VERSION_HEADER))
        .map(|(_, value)| value.as_str());
    let header = versions.next();
    if versions.next().is_some() {
        return Err(RpcError::new(400, HEADER_MISMATCH, "HeaderMismatch: duplicate MCP-Protocol-Version header"));
    }

    if let Some(version) = header {
        if LEGACY_VERSIONS.contains(&version) {
            return Ok(Era::Legacy);
        }
        if version == MODERN_VERSION {
            return if SUPPORTED_VERSIONS.contains(&MODERN_VERSION) {
                Ok(Era::Modern)
            } else {
                Err(unsupported_version(version))
            };
        }
        // An unknown version on `initialize` is a counter-offer, not an error.
        return if method == "initialize" { Ok(Era::Legacy) } else { Err(unsupported_version(version)) };
    }

    let has_meta_version = body
        .get("params")
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get(META_VERSION_KEY))
        .is_some();
    if has_meta_version {
        return Err(RpcError::new(400, HEADER_MISMATCH, "HeaderMismatch: MCP-Protocol-Version header is required"));
    }
    if method == "initialize" {
        return Ok(Era::Legacy);
    }
    Err(RpcError::new(
        400,
        INVALID_REQUEST,
        "the MCP-Protocol-Version header is required (send the version negotiated by initialize)",
    ))
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

/// Per-listener settings that [`serve_connection`] reads for every accepted connection.
/// [`serve_mcp`] builds one from the environment; tests build one explicitly.
struct McpConfig {
    /// Whether the listener is bound to a loopback address (the default `Host` allowance).
    is_loopback: bool,
    /// The required token. Never empty.
    token: String,
    /// Whether write tools are hidden and refused.
    read_only: bool,
    /// `Origin` allowlist for the request gate.
    allowed_origins: Vec<String>,
    /// `Host` allowlist for the request gate.
    allowed_hosts: Vec<String>,
}

/// How long after accept a body of `content_length` bytes may take to arrive: 20 s plus 4 s per
/// MiB, capped at 60 s (so 10 MiB gets the cap). Each read still has the per-read idle timeout
/// on top.
fn body_budget(content_length: usize) -> Duration {
    let scaled_ms = (content_length as u128 * u128::from(BODY_MS_PER_MIB)) / (1024 * 1024);
    let total_ms = BODY_BASE_BUDGET.as_millis() + scaled_ms;
    Duration::from_millis(total_ms.min(BODY_MAX_BUDGET.as_millis()) as u64)
}

/// The checks that need only the request head, in order: routing (404 for any path but
/// `/mcp`, 405 for any method but `POST`), the CSRF / DNS-rebinding / Content-Type gate
/// (403/415), the token (401, without `WWW-Authenticate`), then the framing checks (411 for
/// `Transfer-Encoding`, 413 over [`MAX_BODY_BYTES`], 417 for an unsupported `Expect`). `Some`
/// is the response to send instead of reading the body. Routing runs first, so a stranger
/// probing other paths learns nothing; 404, 401, 403, 415 and 417 are REST-shaped, the rest
/// carry a JSON-RPC error body.
fn reject_from_head(head: &RequestHead, cfg: &McpConfig) -> Option<ApiResponse> {
    if head.path != MCP_PATH {
        return Some(json_err(404, "not found"));
    }
    if head.method != "POST" {
        let err = RpcError::new(405, INVALID_REQUEST, "Only POST is supported on /mcp");
        return Some(rpc_error(err, &Value::Null, &[("Allow", "POST")]));
    }
    if let Err((status, reason)) =
        gate_request(&head.method, &head.headers, cfg.is_loopback, &cfg.allowed_origins, &cfg.allowed_hosts)
    {
        debug!(status, "MCP request rejected by origin/host/content-type gate");
        return Some(json_err(status, &reason));
    }
    if !request_has_api_token(&head.headers, &cfg.token) {
        return Some(json_err(401, "unauthorized"));
    }
    if head.has_transfer_encoding() {
        let err = RpcError::new(411, INVALID_REQUEST, "Transfer-Encoding is not supported; send Content-Length");
        return Some(rpc_error(err, &Value::Null, &[]));
    }
    if head.content_length() > MAX_BODY_BYTES {
        let err = RpcError::new(413, INVALID_REQUEST, "request body exceeds 10 MiB");
        return Some(rpc_error(err, &Value::Null, &[]));
    }
    head.has_unsupported_expect().then(|| json_err(417, "unsupported Expect"))
}

/// Read one request from `stream`, run its head through [`reject_from_head`] before reading
/// the body, answer it, and close. `accepted` is when the connection was accepted: the head
/// must be in 20 s after it and the body [`body_budget`] after it. A response written before
/// the body is fully read goes through [`respond_and_drain`].
async fn serve_connection<S>(mut stream: S, cfg: &McpConfig, accepted: Instant)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(head) = read_request_head(&mut stream, accepted + HEAD_READ_DEADLINE).await else {
        return respond_and_drain(stream, bad_request()).await;
    };
    if let Some(response) = reject_from_head(&head, cfg) {
        return respond_and_drain(stream, response).await;
    }

    // Only a request that passed every check above gets its 100 Continue.
    if head.awaiting_continue() && stream.write_all(CONTINUE_RESPONSE).await.is_err() {
        return;
    }
    let body_deadline = accepted + body_budget(head.content_length());
    let body = timeout_at(body_deadline, read_request_body(&mut stream, &head, MAX_BODY_BYTES)).await;
    let Ok(Some(body)) = body else {
        return respond_and_drain(stream, bad_request()).await;
    };

    // Phase 3 starts the 40 s tool budget here, now that the body is complete.
    let response = handle_body(&head.headers, &body, cfg);
    let _ = stream.write_all(&response.into_bytes()).await;
}

/// The request id when it is valid (a string or an integer), else `null`.
fn echo_id(request: &Value) -> Value {
    match request.get("id") {
        Some(id) if id.is_string() || id.is_i64() || id.is_u64() => id.clone(),
        _ => Value::Null,
    }
}

/// Turn a complete request body into the response: parse, validate the JSON-RPC envelope,
/// accept notifications, detect the era and dispatch.
fn handle_body(headers: &[(String, String)], body: &[u8], cfg: &McpConfig) -> ApiResponse {
    let null = Value::Null;
    let request: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return rpc_error(RpcError::new(400, PARSE_ERROR, "Parse error"), &null, &[]),
    };
    match &request {
        Value::Object(_) => {}
        Value::Array(_) => {
            return rpc_error(RpcError::new(400, INVALID_REQUEST, "Batching is not supported"), &null, &[]);
        }
        _ => {
            let err = RpcError::new(400, INVALID_REQUEST, "Invalid Request: the body must be a JSON-RPC request object");
            return rpc_error(err, &null, &[]);
        }
    }

    let method = match (request.get("jsonrpc"), request.get("method")) {
        (Some(Value::String(version)), Some(Value::String(method))) if version == "2.0" => method.as_str(),
        _ => {
            let err = RpcError::new(
                400,
                INVALID_REQUEST,
                "Invalid Request: jsonrpc must be \"2.0\" and method must be a string",
            );
            return rpc_error(err, &echo_id(&request), &[]);
        }
    };

    // Notifications are accepted whatever the era headers say, with or without an id.
    if method.starts_with("notifications/") {
        return ApiResponse::new(202, "application/json", Vec::new());
    }

    let id = echo_id(&request);
    if id.is_null() {
        let err = RpcError::new(400, INVALID_REQUEST, "Invalid Request: id must be a string or integer");
        return rpc_error(err, &null, &[]);
    }

    match detect_era(headers, &request, method) {
        Ok(Era::Legacy) => dispatch_legacy(&id, method, request.get("params").unwrap_or(&null), cfg),
        // Phase 4 replaces this arm with the modern dispatch; `detect_era` returns `Modern`
        // only once `SUPPORTED_VERSIONS` lists it.
        Ok(Era::Modern) => rpc_error(unsupported_version(MODERN_VERSION), &id, &[]),
        Err(err) => rpc_error(err, &id, &[]),
    }
}

/// The legacy-era version to answer an `initialize` that asks for `requested`: the same
/// version when it is a legacy one, else the newest legacy version (a counter-offer).
fn negotiated_version(requested: &str) -> &'static str {
    LEGACY_VERSIONS.iter().copied().find(|v| *v == requested).unwrap_or(LEGACY_VERSIONS[0])
}

/// Answer a legacy-era request (HTTP 200 unless stated). Stateless: no session is created.
fn dispatch_legacy(id: &Value, method: &str, params: &Value, cfg: &McpConfig) -> ApiResponse {
    match method {
        "initialize" => {
            let Some(requested) = params.get("protocolVersion").and_then(Value::as_str) else {
                return rpc_error(RpcError::new(200, INVALID_PARAMS, "protocolVersion is required"), id, &[]);
            };
            rpc_ok(
                id,
                json!({
                    "protocolVersion": negotiated_version(requested),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "whatsrust", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
        }
        "ping" => rpc_ok(id, json!({})),
        // `params.cursor` is ignored: the whole list is one page.
        "tools/list" => rpc_ok(id, json!({ "tools": REGISTRY.tool_definitions_for(cfg.read_only) })),
        "tools/call" => call_tool(id, params, cfg),
        _ => rpc_error(RpcError::new(200, METHOD_NOT_FOUND, format!("Method not found: {method}")), id, &[]),
    }
}

/// `tools/call`: check the tool exists and may be called, then answer 501 until Phase 3 runs
/// it against the daemon.
fn call_tool(id: &Value, params: &Value, cfg: &McpConfig) -> ApiResponse {
    let name = params.get("name").and_then(Value::as_str);
    let Some(tool) = name.and_then(|name| REGISTRY.find_tool(name)) else {
        let shown = name.unwrap_or("(name is missing or not a string)");
        return rpc_error(RpcError::new(200, INVALID_PARAMS, format!("Unknown tool: {shown}")), id, &[]);
    };
    if !tool.allowed(cfg.read_only) {
        return rpc_error(RpcError::new(200, INVALID_REQUEST, "write access denied in read-only mode"), id, &[]);
    }
    let err = RpcError::new(
        501,
        INTERNAL_ERROR,
        "tools/call over HTTP is not implemented yet; use the stdio server (`whatsrust mcp`)",
    );
    rpc_error(err, id, &[])
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Serve MCP on an already-bound `listener` until cancelled. The caller has validated the
/// settings ([`validate_mcp_startup_from_env`]) and bound `listener` to `WHATSRUST_MCP_BIND`;
/// token, read-only switch and allowlists come from the environment. Without a token it logs
/// and returns: the MCP listener never serves unauthenticated.
pub async fn serve_mcp(listener: TcpListener, cancel: CancellationToken) {
    let s = McpSettings::from_env();
    let Some(token) = s.token else {
        error!("WHATSRUST_MCP_TOKEN is not set; not serving MCP");
        return;
    };
    let cfg = Arc::new(McpConfig {
        is_loopback: is_loopback_bind(&s.bind),
        token,
        read_only: s.read_only,
        allowed_origins: s.allowed_origins,
        allowed_hosts: s.allowed_hosts,
    });
    accept_loop(listener, cfg, cancel).await;
}

/// Accept connections on `listener` until cancelled, serving at most [`MAX_CONNECTIONS`] at once.
/// A connection over the cap gets 503 with `Retry-After: 10` and is closed without draining.
async fn accept_loop(listener: TcpListener, cfg: Arc<McpConfig>, cancel: CancellationToken) {
    let conn_sem = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((mut stream, _)) = result else { continue };
                let accepted = Instant::now();
                let permit = match conn_sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        let mut busy = json_err(503, "too many connections");
                        busy.headers.push(("Retry-After", "10".to_string()));
                        let _ = stream.write_all(&busy.into_bytes()).await;
                        continue;
                    }
                };
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let _permit = permit; // held until the connection is done
                    serve_connection(stream, &cfg, accepted).await;
                });
            }
            _ = cancel.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 22-character token (the shortest the rules accept).
    const TOKEN: &str = "mcp-token-0123456789ab";
    /// A different token of the same length, for the REST side.
    const REST_TOKEN: &str = "rest-token-0123456789a";

    fn on(port: u16) -> Result<u16, String> {
        Ok(port)
    }

    // -------------------------------------------------------------------------
    // Startup rules
    // -------------------------------------------------------------------------

    /// Run the validator with a loopback bind and the given MCP port and tokens.
    fn validate(port: Result<u16, String>, token: Option<&str>, rest_port: u16, rest_token: Option<&str>) -> Result<(), String> {
        validate_mcp_startup(&port, "127.0.0.1", false, token, rest_port, rest_token)
    }

    #[test]
    fn test_parse_mcp_port() {
        for off in [None, Some(""), Some("   "), Some("0"), Some(" 0 ")] {
            assert_eq!(parse_mcp_port(off), Ok(0), "{off:?}");
        }
        assert_eq!(parse_mcp_port(Some("7271")), Ok(7271));
        assert_eq!(parse_mcp_port(Some(" 7271 ")), Ok(7271));
        assert_eq!(parse_mcp_port(Some("65535")), Ok(65535));
        for bad in ["65536", "-1", "abc", "72 71", "7271.0"] {
            assert_eq!(parse_mcp_port(Some(bad)), Err(bad.to_string()), "{bad}");
        }
        assert_eq!(parse_mcp_port(Some("  nope ")), Err("nope".to_string()), "the text is trimmed");
    }

    #[test]
    fn test_parse_mcp_bind() {
        assert_eq!(parse_mcp_bind(None), "127.0.0.1");
        assert_eq!(parse_mcp_bind(Some("")), "127.0.0.1");
        assert_eq!(parse_mcp_bind(Some("  ")), "127.0.0.1");
        assert_eq!(parse_mcp_bind(Some(" 0.0.0.0 ")), "0.0.0.0");
    }

    #[test]
    fn test_validate_port_zero_checks_nothing() {
        // Everything else is wrong, and none of it matters while the listener is off.
        assert_eq!(validate_mcp_startup(&on(0), "0.0.0.0", false, None, 7270, Some("x")), Ok(()));
        assert_eq!(validate_mcp_startup(&on(0), "0.0.0.0", true, Some("short"), 0, None), Ok(()));
    }

    #[test]
    fn test_validate_unparseable_port_is_reported_even_though_it_is_not_above_zero() {
        let err = validate_mcp_startup(&Err("abc".to_string()), "127.0.0.1", false, Some(TOKEN), 7270, None).unwrap_err();
        assert_eq!(
            err,
            "WHATSRUST_MCP_PORT is not a valid port number (got: abc). Set it to a port such as 7271, \
             or unset it to turn the MCP listener off. Refusing to start."
        );
        // It is the first rule: nothing else is looked at.
        let err = validate_mcp_startup(&Err("abc".to_string()), "0.0.0.0", false, None, 0, None).unwrap_err();
        assert!(err.starts_with("WHATSRUST_MCP_PORT is not a valid port number"), "{err}");
    }

    #[test]
    fn test_validate_non_loopback_bind_needs_allow_remote() {
        let err = validate_mcp_startup(&on(7271), "0.0.0.0", false, Some(TOKEN), 0, None).unwrap_err();
        assert_eq!(
            err,
            "WHATSRUST_MCP_BIND=0.0.0.0 is not a loopback address, but WHATSRUST_MCP_ALLOW_REMOTE is not set.\n\
             To expose the MCP listener on the network, set WHATSRUST_MCP_ALLOW_REMOTE=1 and \
             WHATSRUST_MCP_TOKEN (e.g. `openssl rand -base64 32`); to keep it local, unset \
             WHATSRUST_MCP_BIND or set WHATSRUST_MCP_BIND=127.0.0.1. Refusing to start."
        );
        // It comes before the token rules.
        let err = validate_mcp_startup(&on(7271), "0.0.0.0", false, None, 0, None).unwrap_err();
        assert!(err.contains("WHATSRUST_MCP_ALLOW_REMOTE is not set"), "{err}");
        // Allowed once opted in, and loopback names never need it.
        assert_eq!(validate_mcp_startup(&on(7271), "0.0.0.0", true, Some(TOKEN), 0, None), Ok(()));
        for bind in ["127.0.0.1", "::1", "localhost", "LOCALHOST"] {
            assert_eq!(validate_mcp_startup(&on(7271), bind, false, Some(TOKEN), 0, None), Ok(()), "{bind}");
        }
    }

    #[test]
    fn test_validate_token_is_required() {
        let expected = "WHATSRUST_MCP_PORT=7271 turns the MCP listener on, but WHATSRUST_MCP_TOKEN is not set.\n\
             The MCP listener always requires a token. Set WHATSRUST_MCP_TOKEN (e.g. \
             `openssl rand -base64 32`), or unset WHATSRUST_MCP_PORT to turn the MCP listener off. \
             Refusing to start.";
        assert_eq!(validate(on(7271), None, 0, None).unwrap_err(), expected);
        // Empty or whitespace-only counts as unset.
        for blank in ["", " ", " \t "] {
            assert_eq!(validate(on(7271), Some(blank), 0, None).unwrap_err(), expected, "{blank:?}");
        }
        // The token is required on a non-loopback bind too.
        let err = validate_mcp_startup(&on(7271), "0.0.0.0", true, None, 0, None).unwrap_err();
        assert!(err.contains("WHATSRUST_MCP_TOKEN is not set"), "{err}");
    }

    #[test]
    fn test_validate_token_length_21_fails_and_22_passes() {
        let short = "a".repeat(21);
        let err = validate(on(7271), Some(&short), 0, None).unwrap_err();
        assert_eq!(
            err,
            "WHATSRUST_MCP_TOKEN must be at least 22 characters (currently: 21); generate with \
             `openssl rand -base64 32`. Refusing to start."
        );
        assert_eq!(validate(on(7271), Some(&"a".repeat(22)), 0, None), Ok(()));
        // Whitespace around the token does not count towards its length.
        let err = validate(on(7271), Some(&format!("  {short}  ")), 0, None).unwrap_err();
        assert!(err.contains("(currently: 21)"), "{err}");
        assert_eq!(validate(on(7271), Some(&format!("  {}  ", "a".repeat(22))), 0, None), Ok(()));
        // Characters, not bytes: 11 two-byte characters are 22 bytes but only 11 characters.
        let err = validate(on(7271), Some(&"é".repeat(11)), 0, None).unwrap_err();
        assert!(err.contains("(currently: 11)"), "{err}");
        assert_eq!(validate(on(7271), Some(&"é".repeat(22)), 0, None), Ok(()));
    }

    #[test]
    fn test_validate_mcp_token_must_differ_from_rest_token() {
        let err = validate(on(7271), Some(TOKEN), 7270, Some(TOKEN)).unwrap_err();
        assert_eq!(
            err,
            "WHATSRUST_MCP_TOKEN must differ from WHATSRUST_API_TOKEN, so that a leaked MCP token \
             cannot open the REST API.\n\
             Set a different WHATSRUST_MCP_TOKEN (e.g. `openssl rand -base64 32`). Refusing to start."
        );
        // Surrounding whitespace does not make them different.
        assert!(validate(on(7271), Some(TOKEN), 7270, Some(&format!(" {TOKEN} "))).is_err());
        // Different tokens, no REST token, or REST off: fine.
        assert_eq!(validate(on(7271), Some(TOKEN), 7270, Some(REST_TOKEN)), Ok(()));
        assert_eq!(validate(on(7271), Some(TOKEN), 7270, None), Ok(()));
        assert_eq!(validate(on(7271), Some(TOKEN), 0, Some(TOKEN)), Ok(()), "REST is off, so nothing to leak into");
    }

    #[test]
    fn test_validate_mcp_port_must_differ_from_rest_port() {
        let err = validate(on(7270), Some(TOKEN), 7270, None).unwrap_err();
        assert_eq!(err, "WHATSRUST_MCP_PORT must differ from WHATSRUST_PORT. Refusing to start.");
        assert_eq!(validate(on(7271), Some(TOKEN), 7270, None), Ok(()));
        // Token equality is reported before port equality.
        let err = validate(on(7270), Some(TOKEN), 7270, Some(TOKEN)).unwrap_err();
        assert!(err.contains("must differ from WHATSRUST_API_TOKEN"), "{err}");
    }

    #[test]
    fn test_validate_mcp_enabled_with_rest_disabled_is_allowed() {
        // Rules e and f are skipped when the REST port is 0, even with the same number or token.
        assert_eq!(validate(on(7271), Some(TOKEN), 0, None), Ok(()));
        assert_eq!(validate(on(7271), Some(TOKEN), 0, Some(TOKEN)), Ok(()));
    }

    #[test]
    fn test_validate_messages_never_contain_a_token() {
        let secret = "SECRET-TOKEN-VALUE-0123456789";
        let short_secret = "SHORT-SECRET";
        let cases = [
            validate_mcp_startup(&on(7270), "0.0.0.0", false, Some(secret), 7270, Some(secret)),
            validate_mcp_startup(&on(7270), "127.0.0.1", false, Some(secret), 7270, Some(secret)),
            validate_mcp_startup(&on(7270), "127.0.0.1", false, Some(secret), 7270, Some("other-rest-token-0123456789")),
            validate_mcp_startup(&on(7271), "127.0.0.1", false, Some(short_secret), 7270, Some(short_secret)),
        ];
        let mut errors = 0;
        for case in cases {
            if let Err(msg) = case {
                errors += 1;
                assert!(!msg.contains(secret) && !msg.contains(short_secret), "{msg}");
            }
        }
        assert_eq!(errors, 4, "every case is a refusal");
    }

    #[tokio::test]
    async fn test_bind_mcp_listener_binds_a_free_port() {
        let listener = bind_mcp_listener("127.0.0.1", 0).await.expect("a free port binds");
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn test_bind_mcp_listener_port_in_use_names_the_fix() {
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = taken.local_addr().unwrap().port();

        let msg = bind_mcp_listener("127.0.0.1", port).await.expect_err("the port is taken");

        for part in [
            "WHATSRUST_MCP_BIND=127.0.0.1",
            &format!("WHATSRUST_MCP_PORT={port}"),
            &format!("Port {port} is already in use"),
            "set WHATSRUST_MCP_PORT to a free port (unset it to turn the MCP listener off)",
            "(os error",
        ] {
            assert!(msg.contains(part), "missing {part:?} in: {msg}");
        }
        assert!(msg.ends_with("Refusing to start."), "{msg}");
    }

    #[tokio::test]
    async fn test_bind_mcp_listener_other_failure_names_bind_and_port() {
        // TEST-NET-3 is not an address of this machine, so the OS refuses the bind.
        let msg = bind_mcp_listener("203.0.113.1", 7271).await.expect_err("not a local address");

        for part in ["WHATSRUST_MCP_BIND=203.0.113.1", "WHATSRUST_MCP_PORT=7271", "Check that WHATSRUST_MCP_BIND"] {
            assert!(msg.contains(part), "missing {part:?} in: {msg}");
        }
        assert!(!msg.contains("already in use"), "{msg}");
        assert!(msg.ends_with("Refusing to start."), "{msg}");
    }

    // -------------------------------------------------------------------------
    // Body deadline
    // -------------------------------------------------------------------------

    #[test]
    fn test_body_budget_scales_with_length_and_is_capped() {
        const MIB: usize = 1024 * 1024;
        assert_eq!(body_budget(0), Duration::from_secs(20));
        assert_eq!(body_budget(MIB), Duration::from_secs(24));
        assert_eq!(body_budget(5 * MIB), Duration::from_secs(40));
        assert_eq!(body_budget(10 * MIB), Duration::from_secs(60));
        assert_eq!(body_budget(10 * MIB + 1), Duration::from_secs(60), "capped");
        assert_eq!(body_budget(usize::MAX), Duration::from_secs(60), "no overflow");
        assert_eq!(body_budget(MIB / 2), Duration::from_secs(22), "computed in milliseconds");
    }

    // -------------------------------------------------------------------------
    // Era detection
    // -------------------------------------------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    fn version_header(value: &str) -> Vec<(String, String)> {
        headers(&[("MCP-Protocol-Version", value)])
    }

    fn assert_rejects(result: Result<Era, RpcError>, status: u16, code: i64, message: &str) -> RpcError {
        let err = result.expect_err("the request is rejected");
        assert_eq!((err.status, err.code, err.message.as_str()), (status, code, message));
        err
    }

    #[test]
    fn test_detect_era_header_picks_the_era() {
        let body = json!({});
        for version in ["2025-11-25", "2025-06-18"] {
            for method in ["tools/list", "initialize", "ping"] {
                assert_eq!(detect_era(&version_header(version), &body, method).unwrap(), Era::Legacy, "{version} {method}");
            }
        }
    }

    #[test]
    fn test_detect_era_header_name_is_case_insensitive_and_trimmed() {
        let body = json!({});
        for name in ["mcp-protocol-version", "MCP-PROTOCOL-VERSION", "Mcp-Protocol-Version", "MCP-Protocol-Version "] {
            let h = headers(&[(name, "2025-06-18")]);
            assert_eq!(detect_era(&h, &body, "tools/list").unwrap(), Era::Legacy, "{name:?}");
        }
    }

    #[test]
    fn test_detect_era_duplicate_header_is_a_header_mismatch() {
        let both = headers(&[("MCP-Protocol-Version", "2025-06-18"), ("mcp-protocol-version", "2025-06-18")]);
        for method in ["tools/list", "initialize"] {
            assert_rejects(
                detect_era(&both, &json!({}), method),
                400,
                -32020,
                "HeaderMismatch: duplicate MCP-Protocol-Version header",
            );
        }
    }

    #[test]
    fn test_detect_era_modern_header_is_unsupported_until_phase_4() {
        // `SUPPORTED_VERSIONS` does not list 2026-07-28 yet, so even a header-only modern request
        // (and an `initialize` carrying it) is a -32022 with the legacy list.
        for method in ["tools/list", "initialize", "server/discover"] {
            let err = assert_rejects(
                detect_era(&version_header("2026-07-28"), &json!({}), method),
                400,
                -32022,
                "Unsupported protocol version",
            );
            assert_eq!(
                err.data,
                Some(json!({"supported": ["2025-11-25", "2025-06-18"], "requested": "2026-07-28"}))
            );
        }
    }

    #[test]
    fn test_detect_era_unsupported_header_value() {
        // `initialize` is a counter-offer, so it stays legacy.
        for value in ["2024-11-05", "2025-03-26", "garbage", ""] {
            assert_eq!(detect_era(&version_header(value), &json!({}), "initialize").unwrap(), Era::Legacy, "{value:?}");
            // Any other method is a -32022 that names the version asked for.
            let err = assert_rejects(
                detect_era(&version_header(value), &json!({}), "tools/list"),
                400,
                -32022,
                "Unsupported protocol version",
            );
            assert_eq!(
                err.data,
                Some(json!({"supported": ["2025-11-25", "2025-06-18"], "requested": value})),
                "{value:?}"
            );
        }
    }

    #[test]
    fn test_detect_era_meta_version_key_without_header_is_a_header_mismatch() {
        let body = json!({"params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}});
        for method in ["tools/list", "initialize"] {
            assert_rejects(
                detect_era(&[], &body, method),
                400,
                -32020,
                "HeaderMismatch: MCP-Protocol-Version header is required",
            );
        }
        // Any value counts: the key's presence is the signal.
        let body = json!({"params": {"_meta": {"io.modelcontextprotocol/protocolVersion": null}}});
        assert!(detect_era(&[], &body, "tools/list").is_err());
    }

    #[test]
    fn test_detect_era_header_wins_over_the_meta_key() {
        let body = json!({"params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}});
        assert_eq!(detect_era(&version_header("2025-06-18"), &body, "tools/list").unwrap(), Era::Legacy);
    }

    #[test]
    fn test_detect_era_meta_without_the_version_key_is_not_modern() {
        // Legacy requests may carry `_meta.progressToken`.
        let body = json!({"params": {"_meta": {"progressToken": "abc"}}});
        assert_eq!(detect_era(&[], &body, "initialize").unwrap(), Era::Legacy);
        assert_rejects(
            detect_era(&[], &body, "tools/list"),
            400,
            -32600,
            "the MCP-Protocol-Version header is required (send the version negotiated by initialize)",
        );
        // The key is flat: a nested spelling is not the modern marker.
        let nested = json!({"params": {"_meta": {"io.modelcontextprotocol": {"protocolVersion": "2026-07-28"}}}});
        assert_eq!(detect_era(&[], &nested, "initialize").unwrap(), Era::Legacy);
        // `_meta` is only read under `params`, and only when it is an object.
        for body in [json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "x"}}), json!({"params": {"_meta": "x"}}), json!({"params": []})] {
            assert_eq!(detect_era(&[], &body, "initialize").unwrap(), Era::Legacy, "{body}");
        }
    }

    #[test]
    fn test_detect_era_no_signals() {
        assert_eq!(detect_era(&[], &json!({}), "initialize").unwrap(), Era::Legacy);
        for method in ["tools/list", "tools/call", "ping", "server/discover"] {
            assert_rejects(
                detect_era(&[], &json!({}), method),
                400,
                -32600,
                "the MCP-Protocol-Version header is required (send the version negotiated by initialize)",
            );
        }
    }

    // -------------------------------------------------------------------------
    // JSON-RPC response builders
    // -------------------------------------------------------------------------

    fn body_json(response: &ApiResponse) -> Value {
        serde_json::from_slice(&response.body).expect("the body is JSON")
    }

    #[test]
    fn test_rpc_error_shape_data_and_headers() {
        let err = RpcError::new(405, -32600, "nope");
        let response = rpc_error(err, &Value::Null, &[("Allow", "POST")]);
        assert_eq!(response.status, 405);
        assert_eq!(response.content_type, "application/json");
        assert_eq!(response.headers, vec![("Allow", "POST".to_string())]);
        assert_eq!(body_json(&response), json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "nope"}}));

        let err = RpcError::new(400, -32022, "m").with_data(json!({"k": 1}));
        let response = rpc_error(err, &json!("abc"), &[]);
        assert!(response.headers.is_empty());
        assert_eq!(
            body_json(&response),
            json!({"jsonrpc": "2.0", "id": "abc", "error": {"code": -32022, "message": "m", "data": {"k": 1}}})
        );
    }

    #[test]
    fn test_rpc_ok_shape() {
        let response = rpc_ok(&json!(7), json!({"a": 1}));
        assert_eq!(response.status, 200);
        assert_eq!(body_json(&response), json!({"jsonrpc": "2.0", "id": 7, "result": {"a": 1}}));
    }

    #[test]
    fn test_negotiated_version() {
        assert_eq!(negotiated_version("2025-11-25"), "2025-11-25");
        assert_eq!(negotiated_version("2025-06-18"), "2025-06-18");
        for other in ["2024-11-05", "2025-03-26", "2026-07-28", "", "x"] {
            assert_eq!(negotiated_version(other), "2025-11-25", "{other:?}");
        }
    }

    // -------------------------------------------------------------------------
    // Pipeline over a loopback connection
    // -------------------------------------------------------------------------

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// The six tools `WHATSRUST_MCP_READ_ONLY` leaves, in registry order.
    const READ_TOOLS: [&str; 6] = [
        "whatsrust_status",
        "whatsrust_groups",
        "whatsrust_group_info",
        "whatsrust_history",
        "whatsrust_search",
        "whatsrust_fetch_status",
    ];

    /// Loopback bind with [`TOKEN`] required and no allowlists.
    fn test_config(read_only: bool) -> McpConfig {
        McpConfig {
            is_loopback: true,
            token: TOKEN.to_string(),
            read_only,
            allowed_origins: Vec::new(),
            allowed_hosts: Vec::new(),
        }
    }

    /// What a client read back from the server.
    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Reply {
        /// Split raw response bytes. Checks what every response must satisfy: a status line,
        /// a `Content-Length` that matches the body, and no `Mcp-Session-Id`.
        fn parse(bytes: &[u8]) -> Self {
            let split = bytes
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap_or_else(|| panic!("no response head in {:?}", String::from_utf8_lossy(bytes)));
            let head = std::str::from_utf8(&bytes[..split]).expect("the head is text");
            let mut lines = head.lines();
            let status_line = lines.next().unwrap();
            let status = status_line.split(' ').nth(1).and_then(|s| s.parse().ok()).expect("a status code");
            let headers: Vec<(String, String)> = lines
                .map(|l| {
                    let (name, value) = l.split_once(": ").expect("a header line");
                    (name.to_string(), value.to_string())
                })
                .collect();
            let reply = Self { status, headers, body: bytes[split + 4..].to_vec() };
            assert_eq!(reply.header("Content-Length"), Some(reply.body.len().to_string().as_str()));
            assert!(reply.header("Mcp-Session-Id").is_none(), "the endpoint is stateless");
            reply
        }

        fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("body is not JSON ({e}): {:?}", String::from_utf8_lossy(&self.body)))
        }

        /// The JSON-RPC `error` object, after checking the envelope.
        fn rpc_error(&self) -> Value {
            let body = self.json();
            assert_eq!(body["jsonrpc"], "2.0");
            assert!(body.get("result").is_none(), "{body}");
            body["error"].clone()
        }

        /// The JSON-RPC `result`, after checking the envelope.
        fn rpc_result(&self) -> Value {
            let body = self.json();
            assert_eq!(body["jsonrpc"], "2.0");
            assert!(body.get("error").is_none(), "{body}");
            body["result"].clone()
        }
    }

    /// Accept one connection on a fresh loopback listener and serve it with `cfg`; return the
    /// client end and the server task.
    async fn connect_to_server(cfg: McpConfig) -> (TcpStream, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream, &cfg, Instant::now()).await;
        });
        (TcpStream::connect(addr).await.unwrap(), server)
    }

    /// Read until the server closes the connection.
    async fn read_until_closed(client: &mut TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut bytes))
            .await
            .expect("the server answers and closes")
            .unwrap();
        bytes
    }

    /// Send `raw` as one connection's request and return the response.
    async fn exchange(cfg: McpConfig, raw: &[u8]) -> Reply {
        let (mut client, server) = connect_to_server(cfg).await;
        client.write_all(raw).await.unwrap();
        let bytes = read_until_closed(&mut client).await;
        drop(client);
        server.await.unwrap();
        Reply::parse(&bytes)
    }

    /// A well-formed `POST /mcp` with the token; `extra` is whole header lines (each ending in
    /// CRLF) added after the standard ones.
    fn post_with(extra: &str, body: &str) -> Vec<u8> {
        format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:7271\r\nContent-Type: application/json\r\n\
             Authorization: Bearer {TOKEN}\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// POST the JSON-RPC `body` with an optional `MCP-Protocol-Version`, return the response.
    async fn rpc(read_only: bool, version: Option<&str>, body: &Value) -> Reply {
        let extra = version.map(|v| format!("MCP-Protocol-Version: {v}\r\n")).unwrap_or_default();
        exchange(test_config(read_only), &post_with(&extra, &body.to_string())).await
    }

    /// `rpc` with a current legacy version header and read-write access.
    async fn legacy(body: &Value) -> Reply {
        rpc(false, Some("2025-06-18"), body).await
    }

    fn call(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    }

    fn assert_rpc_error(reply: &Reply, status: u16, code: i64, message: &str, id: Value) {
        assert_eq!(reply.status, status, "{:?}", String::from_utf8_lossy(&reply.body));
        assert_eq!(reply.header("Content-Type"), Some("application/json"));
        let body = reply.json();
        assert_eq!(body["id"], id, "{body}");
        let error = reply.rpc_error();
        assert_eq!((error["code"].as_i64(), error["message"].as_str()), (Some(code), Some(message)), "{body}");
    }

    fn assert_rest_error(reply: &Reply, status: u16, code: &str) {
        assert_eq!(reply.status, status);
        let body = reply.json();
        assert_eq!((body["ok"].clone(), body["code"].as_str()), (json!(false), Some(code)), "{body}");
    }

    // --- routing, gate, token ---

    #[tokio::test]
    async fn test_other_paths_get_404_before_the_token() {
        // No token is sent: the route check runs first, so these learn nothing but 404.
        for path in ["/", "/mcp/", "/MCP", "/api/status", "/api/send", "/health", "/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-authorization-server"] {
            let raw = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}");
            let reply = exchange(test_config(false), raw.as_bytes()).await;
            assert_rest_error(&reply, 404, "not_found");
            assert_eq!(reply.json()["error"], "not found", "{path}");
            assert!(reply.header("WWW-Authenticate").is_none());
        }
        // The same for GET, whatever the method.
        let reply = exchange(test_config(false), b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
        assert_rest_error(&reply, 404, "not_found");
    }

    #[tokio::test]
    async fn test_query_string_does_not_change_the_route() {
        let body = call("ping", json!({})).to_string();
        let raw = format!(
            "POST /mcp?x=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\n\
             MCP-Protocol-Version: 2025-06-18\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let reply = exchange(test_config(false), raw.as_bytes()).await;
        assert_eq!(reply.status, 200);
    }

    #[tokio::test]
    async fn test_other_methods_on_mcp_get_405_with_allow_post() {
        // Before the gate and token: no Content-Type, no token.
        for method in ["GET", "DELETE", "PUT", "HEAD", "OPTIONS", "PATCH", "post"] {
            let raw = format!("{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
            let reply = exchange(test_config(false), raw.as_bytes()).await;
            assert_rpc_error(&reply, 405, -32600, "Only POST is supported on /mcp", Value::Null);
            assert_eq!(reply.header("Allow"), Some("POST"), "{method}");
            assert!(reply.header("WWW-Authenticate").is_none());
        }
    }

    #[tokio::test]
    async fn test_missing_or_wrong_token_is_401_without_www_authenticate() {
        let body = call("ping", json!({})).to_string();
        let head = "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n";
        for auth in [
            String::new(),
            "Authorization: Bearer wrong-token-0123456789\r\n".to_string(),
            "Authorization: Basic abc\r\n".to_string(),
            format!("Authorization: {TOKEN}\r\n"),
            "X-API-Token: wrong-token-0123456789\r\n".to_string(),
            format!("Authorization: Bearer {}\r\n", &TOKEN[..21]),
        ] {
            let raw = format!("{head}{auth}Content-Length: {}\r\n\r\n{body}", body.len());
            let reply = exchange(test_config(false), raw.as_bytes()).await;
            assert_rest_error(&reply, 401, "unauthorized");
            assert_eq!(reply.json()["error"], "unauthorized");
            assert!(reply.header("WWW-Authenticate").is_none(), "{auth:?}");
        }
    }

    #[tokio::test]
    async fn test_token_may_come_as_x_api_token() {
        let body = call("ping", json!({})).to_string();
        let extra = format!("X-API-Token: {TOKEN}\r\nMCP-Protocol-Version: 2025-06-18\r\n");
        let raw = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(exchange(test_config(false), raw.as_bytes()).await.status, 200);
    }

    #[tokio::test]
    async fn test_gate_rejects_foreign_origin_host_and_content_type() {
        let ping = call("ping", json!({})).to_string();
        let reply = exchange(test_config(false), &post_with("Origin: https://evil.example\r\n", &ping)).await;
        assert_rest_error(&reply, 403, "forbidden");

        let raw = format!(
            "POST /mcp HTTP/1.1\r\nHost: evil.example\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: {}\r\n\r\n{ping}",
            ping.len()
        );
        assert_rest_error(&exchange(test_config(false), raw.as_bytes()).await, 403, "forbidden");

        let raw = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: text/plain\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: {}\r\n\r\n{ping}",
            ping.len()
        );
        assert_rest_error(&exchange(test_config(false), raw.as_bytes()).await, 415, "unsupported_media_type");
    }

    #[tokio::test]
    async fn test_gate_comes_before_the_token() {
        // A foreign Origin without a token is a 403, not a 401.
        let raw = "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\n\
                   Content-Length: 2\r\n\r\n{}";
        assert_rest_error(&exchange(test_config(false), raw.as_bytes()).await, 403, "forbidden");
    }

    #[tokio::test]
    async fn test_allowlisted_origin_and_host_pass_the_gate() {
        let mut cfg = test_config(false);
        cfg.allowed_origins = vec!["https://trusted.example".to_string()];
        cfg.allowed_hosts = vec!["proxy.example".to_string()];
        let body = call("ping", json!({})).to_string();
        let raw = format!(
            "POST /mcp HTTP/1.1\r\nHost: proxy.example\r\nOrigin: https://trusted.example\r\nContent-Type: application/json\r\n\
             Authorization: Bearer {TOKEN}\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(exchange(cfg, raw.as_bytes()).await.status, 200);
    }

    // --- framing ---

    #[tokio::test]
    async fn test_transfer_encoding_is_411_with_a_json_rpc_body() {
        for te in ["Transfer-Encoding: chunked\r\n", "transfer-encoding: identity\r\n", "Transfer-Encoding : chunked\r\n"] {
            let reply = exchange(test_config(false), &post_with(te, "{}")).await;
            assert_rpc_error(&reply, 411, -32600, "Transfer-Encoding is not supported; send Content-Length", Value::Null);
        }
        // Even with a Content-Length that would otherwise be fine.
        let raw = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\n\
             Transfer-Encoding: chunked\r\n\r\n2\r\n{{}}\r\n0\r\n\r\n"
        );
        let reply = exchange(test_config(false), raw.as_bytes()).await;
        assert_eq!(reply.status, 411);
    }

    #[tokio::test]
    async fn test_content_length_over_10_mib_is_413_without_reading_the_body() {
        let limit = 10 * 1024 * 1024;
        // Only the head is sent; the server must answer without waiting for the body.
        let raw = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: {}\r\n\r\n",
            limit + 1
        );
        let reply = exchange(test_config(false), raw.as_bytes()).await;
        assert_rpc_error(&reply, 413, -32600, "request body exceeds 10 MiB", Value::Null);
        assert!(String::from_utf8_lossy(&reply.body).contains("10 MiB"));

        // Exactly 10 MiB is within the limit: the server waits for the body (and the idle
        // timeout is far away), so with only a head it must not have answered 413.
        let (mut client, server) = connect_to_server(test_config(false)).await;
        let at_limit = raw.replace(&(limit + 1).to_string(), &limit.to_string());
        client.write_all(at_limit.as_bytes()).await.unwrap();
        let mut byte = [0u8; 1];
        let waited = tokio::time::timeout(Duration::from_millis(300), client.read(&mut byte)).await;
        assert!(waited.is_err(), "10 MiB is accepted, so the server is waiting for the body");
        drop(client);
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_unsupported_expect_is_417_rest_shaped() {
        let reply = exchange(test_config(false), &post_with("Expect: 200-ok\r\n", "{}")).await;
        assert_rest_error(&reply, 417, "expectation_failed");
        assert_eq!(reply.json()["error"], "unsupported Expect");
    }

    #[tokio::test]
    async fn test_framing_checks_come_after_the_token() {
        // No token: 401, not 411 / 413 / 417.
        for extra in ["Transfer-Encoding: chunked\r\n", "Expect: 200-ok\r\n", "Content-Length: 99999999\r\n"] {
            let raw = format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n{extra}\r\n");
            assert_rest_error(&exchange(test_config(false), raw.as_bytes()).await, 401, "unauthorized");
        }
    }

    #[tokio::test]
    async fn test_expect_100_continue_with_a_delayed_body() {
        let body = call("ping", json!({})).to_string();
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\n\
             MCP-Protocol-Version: 2025-06-18\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let (mut client, server) = connect_to_server(test_config(false)).await;
        client.write_all(head.as_bytes()).await.unwrap();

        // The interim response arrives before any body byte is sent.
        let mut interim = vec![0u8; "HTTP/1.1 100 Continue\r\n\r\n".len()];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut interim))
            .await
            .expect("the server sends 100 Continue")
            .unwrap();
        assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n");

        client.write_all(body.as_bytes()).await.unwrap();
        let reply = Reply::parse(&read_until_closed(&mut client).await);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.rpc_result(), json!({}));
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_no_100_continue_when_the_body_came_with_the_head_or_a_check_failed() {
        // Body already complete: no interim response, the first bytes are the final status line.
        let body = call("ping", json!({})).to_string();
        let reply_bytes = {
            let (mut client, server) = connect_to_server(test_config(false)).await;
            let raw = post_with("MCP-Protocol-Version: 2025-06-18\r\nExpect: 100-continue\r\n", &body);
            client.write_all(&raw).await.unwrap();
            let bytes = read_until_closed(&mut client).await;
            drop(client);
            server.await.unwrap();
            bytes
        };
        assert!(reply_bytes.starts_with(b"HTTP/1.1 200 "), "{}", String::from_utf8_lossy(&reply_bytes));

        // A request that fails the token check gets its 401 straight away, never a 100.
        let raw = "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nExpect: 100-continue\r\n\
                   Content-Length: 50\r\n\r\n";
        let (mut client, server) = connect_to_server(test_config(false)).await;
        client.write_all(raw.as_bytes()).await.unwrap();
        let bytes = read_until_closed(&mut client).await;
        assert!(bytes.starts_with(b"HTTP/1.1 401 "), "{}", String::from_utf8_lossy(&bytes));
        drop(client);
        server.await.unwrap();
    }

    // --- body parsing and envelope ---

    async fn send_raw_body(body: &str) -> Reply {
        exchange(test_config(false), &post_with("MCP-Protocol-Version: 2025-06-18\r\n", body)).await
    }

    #[tokio::test]
    async fn test_malformed_json_is_400_parse_error() {
        for body in ["", "{", "not json", "{\"jsonrpc\":\"2.0\",", "{} trailing"] {
            assert_rpc_error(&send_raw_body(body).await, 400, -32700, "Parse error", Value::Null);
        }
    }

    #[tokio::test]
    async fn test_batches_and_non_objects_are_400_invalid_request() {
        let ping = call("ping", json!({}));
        for batch in ["[]".to_string(), format!("[{ping}]"), format!("[{ping},{ping}]")] {
            assert_rpc_error(&send_raw_body(&batch).await, 400, -32600, "Batching is not supported", Value::Null);
        }
        for body in ["5", "\"x\"", "null", "true"] {
            let reply = send_raw_body(body).await;
            assert_eq!(reply.status, 400, "{body}");
            assert_eq!(reply.rpc_error()["code"], -32600, "{body}");
            assert_eq!(reply.json()["id"], Value::Null);
        }
    }

    #[tokio::test]
    async fn test_bad_envelope_is_400_and_echoes_a_valid_id() {
        let message = "Invalid Request: jsonrpc must be \"2.0\" and method must be a string";
        let cases = [
            (json!({"id": 1, "method": "ping"}), json!(null)),
            (json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}), json!(null)),
            (json!({"jsonrpc": 2.0, "id": 1, "method": "ping"}), json!(null)),
            (json!({"jsonrpc": "2.0", "id": 5}), json!(null)),
            (json!({"jsonrpc": "2.0", "id": 5, "method": 7}), json!(null)),
            (json!({"jsonrpc": "2.0", "id": "abc", "method": null}), json!(null)),
        ];
        for (body, _) in cases {
            let reply = send_raw_body(&body.to_string()).await;
            // The id is echoed when it is valid, else null.
            let expected_id = match body.get("id") {
                Some(id) if id.is_string() || id.is_i64() => id.clone(),
                _ => Value::Null,
            };
            assert_rpc_error(&reply, 400, -32600, message, expected_id);
        }
        // An invalid id on a bad envelope is null.
        let reply = send_raw_body(&json!({"jsonrpc": "2.0", "id": 1.5, "method": 3}).to_string()).await;
        assert_rpc_error(&reply, 400, -32600, message, Value::Null);
    }

    #[tokio::test]
    async fn test_notifications_get_202_with_no_body() {
        let cases = [
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 9, "method": "notifications/cancelled", "params": {"requestId": 1}}),
            json!({"jsonrpc": "2.0", "id": "x", "method": "notifications/anything/else"}),
        ];
        for body in cases {
            // With no era header, and with an unsupported one: notifications skip era detection.
            for version in [None, Some("2025-06-18"), Some("1999-01-01"), Some("2026-07-28")] {
                let reply = rpc(false, version, &body).await;
                assert_eq!(reply.status, 202, "{body} {version:?}");
                assert!(reply.body.is_empty(), "{body}");
                assert_eq!(reply.header("Content-Length"), Some("0"));
            }
        }
        // Only the `notifications/` prefix makes a notification.
        let reply = legacy(&json!({"jsonrpc": "2.0", "id": 1, "method": "notifications"})).await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.rpc_error()["code"], -32601);
    }

    #[tokio::test]
    async fn test_a_notification_still_needs_a_valid_envelope() {
        let reply = send_raw_body(&json!({"method": "notifications/initialized"}).to_string()).await;
        assert_eq!(reply.status, 400);
    }

    #[tokio::test]
    async fn test_id_must_be_a_string_or_an_integer() {
        let message = "Invalid Request: id must be a string or integer";
        let missing = r#"{"jsonrpc":"2.0","method":"ping"}"#;
        for body in [
            missing,
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1.0,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":[1],"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#,
        ] {
            assert_rpc_error(&send_raw_body(body).await, 400, -32600, message, Value::Null);
        }
        // Strings and integers (negative, zero, up to u64::MAX) are echoed exactly.
        for id in [json!("abc"), json!(""), json!(0), json!(-7), json!(i64::MAX), json!(u64::MAX)] {
            let reply = legacy(&json!({"jsonrpc": "2.0", "id": id, "method": "ping"})).await;
            assert_eq!(reply.status, 200);
            assert_eq!(reply.json()["id"], id);
            assert_eq!(reply.rpc_result(), json!({}));
        }
    }

    // --- legacy dispatch ---

    #[tokio::test]
    async fn test_initialize_echoes_a_supported_version() {
        for version in ["2025-06-18", "2025-11-25"] {
            // With and without the header: `initialize` is the one request that may omit it.
            for header in [None, Some(version)] {
                let reply = rpc(false, header, &call("initialize", json!({"protocolVersion": version, "capabilities": {}}))).await;
                assert_eq!(reply.status, 200);
                assert_eq!(reply.header("Content-Type"), Some("application/json"));
                assert_eq!(reply.json()["id"], 1);
                assert_eq!(
                    reply.rpc_result(),
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "whatsrust", "version": env!("CARGO_PKG_VERSION")},
                    })
                );
            }
        }
    }

    #[tokio::test]
    async fn test_initialize_counter_offers_the_newest_legacy_version() {
        for requested in ["2024-11-05", "2025-03-26", "2026-07-28", "nonsense", ""] {
            let reply = rpc(false, None, &call("initialize", json!({"protocolVersion": requested}))).await;
            assert_eq!(reply.status, 200, "{requested:?}");
            assert_eq!(reply.rpc_result()["protocolVersion"], "2025-11-25", "{requested:?}");
        }
        // An unsupported header value on `initialize` is a counter-offer too, not a -32022.
        let reply = rpc(false, Some("2024-11-05"), &call("initialize", json!({"protocolVersion": "2024-11-05"}))).await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.rpc_result()["protocolVersion"], "2025-11-25");
    }

    #[tokio::test]
    async fn test_initialize_needs_a_string_protocol_version() {
        for params in [json!({}), json!({"protocolVersion": 1}), json!({"protocolVersion": null}), json!(null), json!([])] {
            let reply = rpc(false, None, &call("initialize", params.clone())).await;
            assert_rpc_error(&reply, 200, -32602, "protocolVersion is required", json!(1));
        }
        // No params at all.
        let body = json!({"jsonrpc": "2.0", "id": "i", "method": "initialize"});
        assert_rpc_error(&rpc(false, None, &body).await, 200, -32602, "protocolVersion is required", json!("i"));
    }

    #[tokio::test]
    async fn test_ping_returns_an_empty_result() {
        let reply = legacy(&call("ping", json!({}))).await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.json(), json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
    }

    #[tokio::test]
    async fn test_tools_list_returns_all_34_tools_with_annotations() {
        let reply = legacy(&call("tools/list", json!({}))).await;
        assert_eq!(reply.status, 200);
        let result = reply.rpc_result();
        assert!(result.get("nextCursor").is_none(), "the list is one page");
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 34);
        assert_eq!(result["tools"], json!(REGISTRY.tool_definitions()));
        for tool in tools {
            assert!(tool["annotations"].is_object(), "{}: annotations", tool["name"]);
            assert!(tool["inputSchema"].is_object(), "{}: inputSchema", tool["name"]);
            assert!(tool["description"].is_string(), "{}: description", tool["name"]);
        }
    }

    #[tokio::test]
    async fn test_tools_list_ignores_the_cursor() {
        let reply = legacy(&call("tools/list", json!({"cursor": "whatever"}))).await;
        assert_eq!(reply.rpc_result()["tools"].as_array().unwrap().len(), 34);
    }

    #[tokio::test]
    async fn test_tools_list_in_read_only_mode_is_exactly_the_six_read_tools() {
        let reply = rpc(true, Some("2025-06-18"), &call("tools/list", json!({}))).await;
        assert_eq!(reply.status, 200);
        let result = reply.rpc_result();
        let tools = result["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, READ_TOOLS);
        for tool in tools {
            assert_eq!(tool["annotations"]["readOnlyHint"], true, "{}", tool["name"]);
        }
    }

    #[tokio::test]
    async fn test_tools_call_on_a_known_tool_is_a_501_stub() {
        for (read_only, tool) in [(false, "whatsrust_status"), (false, "whatsrust_send"), (true, "whatsrust_status"), (true, "whatsrust_search")] {
            let reply = rpc(read_only, Some("2025-06-18"), &call("tools/call", json!({"name": tool, "arguments": {}}))).await;
            assert_rpc_error(
                &reply,
                501,
                -32603,
                "tools/call over HTTP is not implemented yet; use the stdio server (`whatsrust mcp`)",
                json!(1),
            );
        }
    }

    #[tokio::test]
    async fn test_tools_call_for_a_write_tool_in_read_only_mode_is_denied() {
        for tool in ["whatsrust_send", "whatsrust_purge_embeddings", "whatsrust_fetch_cancel", "whatsrust_typing"] {
            let reply = rpc(true, Some("2025-06-18"), &call("tools/call", json!({"name": tool, "arguments": {}}))).await;
            assert_rpc_error(&reply, 200, -32600, "write access denied in read-only mode", json!(1));
        }
    }

    #[tokio::test]
    async fn test_tools_call_with_an_unknown_tool_is_invalid_params() {
        for read_only in [false, true] {
            let reply = rpc(read_only, Some("2025-06-18"), &call("tools/call", json!({"name": "whatsrust_nope"}))).await;
            assert_rpc_error(&reply, 200, -32602, "Unknown tool: whatsrust_nope", json!(1));
        }
        for params in [json!({}), json!({"name": 7}), json!({"name": null}), json!(null)] {
            let reply = legacy(&call("tools/call", params.clone())).await;
            assert_eq!(reply.status, 200, "{params}");
            assert_eq!(reply.rpc_error()["code"], -32602, "{params}");
            assert!(reply.rpc_error()["message"].as_str().unwrap().starts_with("Unknown tool: "), "{params}");
        }
    }

    #[tokio::test]
    async fn test_unknown_methods_are_200_method_not_found() {
        for method in ["foo/bar", "server/discover", "resources/list", "prompts/list", "initialized", "tools/List"] {
            let reply = legacy(&call(method, json!({}))).await;
            assert_rpc_error(&reply, 200, -32601, &format!("Method not found: {method}"), json!(1));
        }
    }

    // --- era detection through the pipeline ---

    #[tokio::test]
    async fn test_modern_header_is_400_unsupported_version_with_the_legacy_list() {
        let reply = rpc(false, Some("2026-07-28"), &call("tools/list", json!({}))).await;
        assert_rpc_error(&reply, 400, -32022, "Unsupported protocol version", json!(1));
        assert_eq!(
            reply.rpc_error()["data"],
            json!({"supported": ["2025-11-25", "2025-06-18"], "requested": "2026-07-28"})
        );
    }

    #[tokio::test]
    async fn test_unsupported_header_version_on_other_methods_is_32022() {
        let reply = rpc(false, Some("2024-11-05"), &call("tools/list", json!({}))).await;
        assert_rpc_error(&reply, 400, -32022, "Unsupported protocol version", json!(1));
        assert_eq!(reply.rpc_error()["data"]["requested"], "2024-11-05");
        assert_eq!(reply.rpc_error()["data"]["supported"], json!(["2025-11-25", "2025-06-18"]));
    }

    #[tokio::test]
    async fn test_missing_version_header_is_400_except_on_initialize() {
        let reply = rpc(false, None, &call("tools/list", json!({}))).await;
        assert_rpc_error(
            &reply,
            400,
            -32600,
            "the MCP-Protocol-Version header is required (send the version negotiated by initialize)",
            json!(1),
        );
        // `_meta` with only a progress token is not a modern signal.
        let reply = rpc(false, None, &call("tools/list", json!({"_meta": {"progressToken": "p"}}))).await;
        assert_eq!(reply.rpc_error()["code"], -32600);
    }

    #[tokio::test]
    async fn test_meta_version_key_without_the_header_is_a_header_mismatch() {
        let params = json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}});
        let reply = rpc(false, None, &call("tools/list", params)).await;
        assert_rpc_error(&reply, 400, -32020, "HeaderMismatch: MCP-Protocol-Version header is required", json!(1));
    }

    #[tokio::test]
    async fn test_duplicate_version_header_is_a_header_mismatch() {
        let body = call("ping", json!({})).to_string();
        let extra = "MCP-Protocol-Version: 2025-06-18\r\nmcp-protocol-version: 2025-06-18\r\n";
        let reply = exchange(test_config(false), &post_with(extra, &body)).await;
        assert_rpc_error(&reply, 400, -32020, "HeaderMismatch: duplicate MCP-Protocol-Version header", json!(1));
    }

    #[tokio::test]
    async fn test_mcp_method_and_name_headers_are_ignored_in_this_phase() {
        let body = call("ping", json!({})).to_string();
        let extra = "MCP-Protocol-Version: 2025-06-18\r\nMcp-Method: tools/call\r\nMcp-Name: whatever\r\n";
        let reply = exchange(test_config(false), &post_with(extra, &body)).await;
        assert_eq!(reply.status, 200);
    }

    #[tokio::test]
    async fn test_responses_never_carry_a_session_id_or_www_authenticate() {
        // `Reply::parse` already rejects Mcp-Session-Id on every response above; this sends one in.
        let body = call("initialize", json!({"protocolVersion": "2025-06-18"})).to_string();
        let reply = exchange(test_config(false), &post_with("Mcp-Session-Id: abc\r\n", &body)).await;
        assert_eq!(reply.status, 200);
        assert!(reply.header("Mcp-Session-Id").is_none());
        assert!(reply.header("WWW-Authenticate").is_none());
    }

    // --- body deadline ---

    #[tokio::test(start_paused = true)]
    async fn test_slow_body_hits_the_body_deadline_before_the_idle_timeout() {
        let (mut client, server_side) = tokio::io::duplex(64 * 1024);
        let cfg = test_config(false);
        let started = Instant::now();
        // The server task reports when it was done, since the client keeps sleeping after that.
        let server = tokio::spawn(async move {
            serve_connection(server_side, &cfg, Instant::now()).await;
            started.elapsed()
        });

        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: 100\r\n\r\n"
        );
        client.write_all(head.as_bytes()).await.unwrap();
        // One byte every 9 s never trips the 10 s idle timeout, so only the 20 s body
        // deadline (100 bytes: 20 s + under a millisecond) can end the request.
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(9)).await;
            let _ = client.write_all(b"x").await;
        }
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        let elapsed = server.await.unwrap();

        let reply = Reply::parse(&bytes);
        assert_rest_error(&reply, 400, "bad_request");
        assert!(elapsed >= Duration::from_secs(20), "answered at {elapsed:?}, before the body deadline");
        assert!(elapsed < Duration::from_secs(22), "answered at {elapsed:?}, after the deadline plus the 1 s drain");
    }

    #[tokio::test(start_paused = true)]
    async fn test_stalled_body_hits_the_idle_timeout() {
        let (mut client, server_side) = tokio::io::duplex(64 * 1024);
        let cfg = test_config(false);
        let started = Instant::now();
        let server = tokio::spawn(async move { serve_connection(server_side, &cfg, Instant::now()).await });

        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: 100\r\n\r\n"
        );
        client.write_all(head.as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap();

        assert_rest_error(&Reply::parse(&bytes), 400, "bad_request");
        assert!(started.elapsed() >= Duration::from_secs(10), "the per-read idle timeout is unchanged");
        assert!(started.elapsed() < Duration::from_secs(12));
    }

    // --- connection cap ---

    #[tokio::test]
    async fn test_the_33rd_concurrent_connection_gets_503_with_retry_after() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let server = tokio::spawn(accept_loop(listener, Arc::new(test_config(false)), cancel.clone()));

        // 32 connections that say nothing hold every permit (the head read waits for them).
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            held.push(TcpStream::connect(addr).await.unwrap());
        }
        let mut over = TcpStream::connect(addr).await.unwrap();
        let reply = Reply::parse(&read_until_closed(&mut over).await);

        assert_rest_error(&reply, 503, "unavailable");
        assert_eq!(reply.json()["error"], "too many connections");
        assert_eq!(reply.header("Retry-After"), Some("10"));

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the accept loop stops on cancel")
            .unwrap();
        drop(held);
    }

    #[tokio::test]
    async fn test_accept_loop_serves_requests_until_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let server = tokio::spawn(accept_loop(listener, Arc::new(test_config(false)), cancel.clone()));

        let mut client = TcpStream::connect(addr).await.unwrap();
        let body = call("ping", json!({})).to_string();
        client.write_all(&post_with("MCP-Protocol-Version: 2025-06-18\r\n", &body)).await.unwrap();
        let reply = Reply::parse(&read_until_closed(&mut client).await);
        assert_eq!(reply.status, 200);

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), server).await.expect("stops on cancel").unwrap();
    }
}
