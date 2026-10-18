//! MCP (Model Context Protocol) server — JSON-RPC over stdio.
//!
//! Proxies tool calls to the running whatsrust daemon via its HTTP API.
//! Start with `whatsrust mcp` — typically invoked by Claude Code or other
//! MCP-compatible AI tool harnesses.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::bridge::SYNC_CLIENT_TIMEOUT;
use crate::mcp_registry::{validated_args, ArgsError, REGISTRY};

/// Run the MCP server on stdin/stdout. Blocks until EOF.
pub fn run_mcp_server(port: u16) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    // Send server info on startup (initialize handshake)
    // MCP clients send initialize first, but we also need to be ready to handle it.

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err = json_rpc_error(Value::Null, -32700, &format!("parse error: {e}"));
                let _ = writeln!(stdout, "{}", serde_json::to_string(&err).unwrap());
                continue;
            }
        };

        // Notifications have null id — don't send a response
        let is_notification = req.id.is_null() || req.method.starts_with("notifications/");
        let response = handle_rpc(&req, port);
        if !is_notification {
            let _ = writeln!(stdout, "{}", serde_json::to_string(&response).unwrap());
            let _ = stdout.flush();
        }
    }
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    /// Null for notifications (no response expected).
    #[serde(default)]
    id: Value,
    method: String,
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

fn json_rpc_ok(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse { jsonrpc: "2.0".to_string(), id, result: Some(result), error: None }
}

fn json_rpc_error(id: Value, code: i32, msg: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(json!({"code": code, "message": msg})),
    }
}

fn handle_rpc(req: &JsonRpcRequest, port: u16) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => json_rpc_ok(req.id.clone(), json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "whatsrust",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
        "notifications/initialized" => json_rpc_ok(req.id.clone(), json!({})),
        "tools/list" => json_rpc_ok(req.id.clone(), json!({ "tools": REGISTRY.tool_definitions() })),
        "tools/call" => {
            let params = req.params.as_ref().unwrap_or(&Value::Null);
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            match call_tool(name, args, port) {
                Ok(result) => json_rpc_ok(req.id.clone(), result),
                Err(message) => json_rpc_error(req.id.clone(), -32602, &message),
            }
        }
        _ => json_rpc_error(req.id.clone(), -32601, &format!("method not found: {}", req.method)),
    }
}

/// MCP tool result carrying an error message (`isError: true`).
fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

/// The request target for a tool's REST call: `path`, then `?k=v&k=v` when there are query
/// pairs, with every key and value URL-encoded.
fn rest_target(path: &str, query: &[(String, String)]) -> String {
    let mut target = path.to_string();
    for (i, (key, value)) in query.iter().enumerate() {
        target.push(if i == 0 { '?' } else { '&' });
        target.push_str(&crate::api::urlencode(key));
        target.push('=');
        target.push_str(&crate::api::urlencode(value));
    }
    target
}

/// Validate one `tools/call` and proxy it to the daemon. `Err` is a JSON-RPC invalid-params
/// message (unknown tool, non-object `arguments`); every other failure is an `isError` tool
/// result in `Ok`.
fn call_tool(name: &str, args: Value, port: u16) -> Result<Value, String> {
    // Single choke point: a call with undeclared, mistyped or missing arguments is rejected
    // here, before any HTTP request, so the REST mapping only ever sees valid arguments.
    let (tool, args) = match validated_args(name, args) {
        Ok(validated) => validated,
        Err(ArgsError::InvalidParams(message)) => return Err(message),
        Err(ArgsError::Tool(message)) => return Ok(tool_error(&message)),
    };
    let mapping = &tool.rest_mapping;
    let target = rest_target(mapping.path, &(mapping.query)(&args));
    let result = match mapping.method {
        "GET" => http_get(port, &target),
        "POST" => {
            let body = (mapping.body)(args).unwrap_or_else(|| json!({}));
            http_post(port, &target, &body)
        }
        other => Err(format!("{name}: unsupported REST method {other}")),
    };

    Ok(match result {
        Ok(body) => json!({ "content": [{ "type": "text", "text": body }] }),
        Err(e) => tool_error(&e),
    })
}

/// Blocking HTTP GET to the daemon.
fn http_get(port: u16, path: &str) -> Result<String, String> {
    use std::io::Read;
    use std::net::TcpStream;

    let host = mcp_connect_host();
    let mut stream = TcpStream::connect((&*host, port))
        .map_err(|e| format!("cannot connect to daemon on {host}:{port}: {e}"))?;
    stream.set_read_timeout(Some(SYNC_CLIENT_TIMEOUT)).ok();

    let auth = std::env::var("WHATSRUST_API_TOKEN").ok()
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Connection: close\r\n\r\n");
    std::io::Write::write_all(&mut stream, req.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read failed: {e}"))?;
    extract_http_body(&buf)
}

/// Blocking HTTP POST to the daemon. Waits up to [`SYNC_CLIENT_TIMEOUT`] (longer than the
/// daemon's own sync-send wait) so the daemon's 504 + job_id normally arrives first.
fn http_post(port: u16, path: &str, body: &Value) -> Result<String, String> {
    http_post_with_timeout(port, path, body, SYNC_CLIENT_TIMEOUT)
}

/// [`http_post`] with an explicit read timeout. A read failure (including that timeout) leaves
/// the outcome unknown, so its error text says the daemon may still process the request.
fn http_post_with_timeout(port: u16, path: &str, body: &Value, read_timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    use std::net::TcpStream;

    let host = mcp_connect_host();
    let mut stream = TcpStream::connect((&*host, port))
        .map_err(|e| format!("cannot connect to daemon on {host}:{port}: {e}"))?;
    stream.set_read_timeout(Some(read_timeout)).ok();

    let body_str = serde_json::to_string(body).unwrap();
    let auth = std::env::var("WHATSRUST_API_TOKEN").ok()
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n{body_str}",
        body_str.len()
    );
    std::io::Write::write_all(&mut stream, req.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}; {}", crate::api::POST_OUTCOME_UNKNOWN))?;
    extract_http_body(&buf)
}

fn extract_http_body(raw: &[u8]) -> Result<String, String> {
    let header_end = raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("invalid HTTP response")?;
    let header_str = String::from_utf8_lossy(&raw[..header_end]);
    // Parse HTTP status code from first line
    let status: u16 = header_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = String::from_utf8(raw[header_end + 4..].to_vec())
        .map_err(|e| format!("invalid UTF-8: {e}"))?;
    if status >= 400 {
        Err(body)
    } else {
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_definitions_include_fetch_history() {
        let tools = REGISTRY.tool_definitions();
        let names: Vec<&str> = tools.iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"whatsrust_fetch_history"), "missing whatsrust_fetch_history");
        assert!(names.contains(&"whatsrust_fetch_status"), "missing whatsrust_fetch_status");
        assert!(names.contains(&"whatsrust_fetch_cancel"), "missing whatsrust_fetch_cancel");
    }

    #[test]
    fn test_fetch_history_description_mentions_pacing() {
        let tools = REGISTRY.tool_definitions();
        let tool = tools.iter().find(|t| t.get("name").and_then(|v| v.as_str()) == Some("whatsrust_fetch_history"))
            .expect("whatsrust_fetch_history must exist");
        let desc = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!desc.is_empty(), "description must be non-empty");
        let desc_lower = desc.to_lowercase();
        assert!(
            desc_lower.contains("paced") || desc_lower.contains("clamp") || desc_lower.contains("background"),
            "description must mention pacing/limits (paced/clamp/background); got: {desc}"
        );
    }

    // -------------------------------------------------------------------------
    // ADR 0044: the MCP proxy's own request builders must keep passing the
    // CSRF/DNS-rebinding/Content-Type gate (src/api.rs::gate_request).
    // -------------------------------------------------------------------------

    /// Minimal well-formed HTTP response so the blocking client (http_get/http_post)
    /// gets a reply and returns instead of hanging until its read timeout.
    const STUB_RESPONSE: &[u8] =
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";

    #[tokio::test]
    async fn test_mcp_http_get_request_passes_the_gate() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let client = std::thread::spawn(move || {
            let _ = http_get(port, "/api/status");
        });

        let (mut stream, _) = listener.accept().await.unwrap();
        let req = crate::api::read_request(&mut stream).await.expect("request should parse");
        let decision = crate::api::gate_request(&req.method, &req.headers, true, &[], &[]);
        assert!(decision.is_ok(), "mcp http_get's request must pass the gate: {decision:?}");

        let _ = stream.write_all(STUB_RESPONSE).await;
        let _ = stream.shutdown().await;
        client.join().expect("client thread panicked");
    }

    #[tokio::test]
    async fn test_mcp_http_post_request_passes_the_gate() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let client = std::thread::spawn(move || {
            let _ = http_post(port, "/api/send", &json!({"jid": "1@s.whatsapp.net", "text": "hi"}));
        });

        let (mut stream, _) = listener.accept().await.unwrap();
        let req = crate::api::read_request(&mut stream).await.expect("request should parse");
        let decision = crate::api::gate_request(&req.method, &req.headers, true, &[], &[]);
        assert!(decision.is_ok(), "mcp http_post's request must pass the gate: {decision:?}");

        let _ = stream.write_all(STUB_RESPONSE).await;
        let _ = stream.shutdown().await;
        client.join().expect("client thread panicked");
    }

    // -------------------------------------------------------------------------
    // Strict arguments: a call with a key the tool's schema does not declare is rejected
    // (tool error, nothing sent) instead of silently trimmed. REST-only fields such as `path`
    // (a local-file read) are unreachable, and e.g. `schedule_at` cannot degrade into an
    // immediate send.
    // -------------------------------------------------------------------------

    const STUB_OK_RESPONSE: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}";

    /// Accept one connection, read the whole request (the client half-closes after writing),
    /// answer with `response`, and return the raw request bytes.
    async fn serve_one_request(listener: &tokio::net::TcpListener, response: &[u8]) -> Vec<u8> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.expect("read request");
        let _ = stream.write_all(response).await;
        let _ = stream.shutdown().await;
        raw
    }

    /// Split a raw HTTP request into its head (request line + headers) and body.
    fn request_head_and_body(raw: &[u8]) -> (String, Vec<u8>) {
        let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("header terminator");
        (String::from_utf8_lossy(&raw[..header_end]).into_owned(), raw[header_end + 4..].to_vec())
    }

    /// Run `call_tool` against a local listener and assert it came back as a tool error naming the
    /// tool, saying "unknown argument" and mentioning `key`, and that nothing ever connected.
    async fn assert_rejected_before_sending(tool: &str, args: Value, key: &str) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let result = call_tool(tool, args, port).expect("a tool error, not a JSON-RPC error");

        assert_eq!(result["isError"], json!(true), "{tool}: must be a tool error");
        let text = result["content"][0]["text"].as_str().expect("error text");
        assert!(
            text.contains(tool) && text.contains("unknown argument") && text.contains(key),
            "{tool}: unexpected error text: {text}"
        );
        assert_no_connection(&listener, tool).await;
    }

    /// `call_tool` is synchronous: had it dialled the port, the connection would already be
    /// queued and accept() would return at once.
    async fn assert_no_connection(listener: &tokio::net::TcpListener, tool: &str) {
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "{tool}: a rejected call must not contact the daemon");
    }

    /// Like [`assert_rejected_before_sending`], for a tool error with exactly this text.
    async fn assert_tool_error_before_sending(tool: &str, args: Value, expected: &str) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let result = call_tool(tool, args.clone(), port).expect("a tool error, not a JSON-RPC error");

        assert_eq!(result["isError"], json!(true), "{tool} {args}: must be a tool error");
        assert_eq!(result["content"][0]["text"], json!(expected), "{tool} {args}");
        assert_no_connection(&listener, tool).await;
    }

    /// Run a `tools/call` against a local listener and return the request line it sent.
    async fn sent_request_line(tool: &str, args: Value) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let tool_name = tool.to_string();
        let client = std::thread::spawn(move || rpc_tools_call(&tool_name, args, port));
        let raw = serve_one_request(&listener, STUB_OK_RESPONSE).await;
        client.join().expect("client thread panicked");

        let (head, _) = request_head_and_body(&raw);
        head.lines().next().expect("request line").to_string()
    }

    #[test]
    fn test_tools_call_non_object_arguments_is_invalid_params() {
        // Port 1 is never dialled: validation fails before any HTTP call.
        for args in [json!("path"), json!(5), json!(true), json!(["jid"])] {
            let response = rpc_tools_call("whatsrust_image", args.clone(), 1);
            assert_eq!(response.result, None, "args: {args}: a JSON-RPC error, not a tool result");
            assert_eq!(response.id, json!(1), "args: {args}");
            let error = response.error.expect("error object");
            assert_eq!(error["code"], json!(-32602), "args: {args}");
            assert_eq!(error["message"], json!("arguments must be a JSON object"), "args: {args}");
        }
    }

    #[tokio::test]
    async fn test_call_tool_rejects_path_on_media_tools_without_sending() {
        let cases = [
            ("whatsrust_image", json!({"jid": "1@s.whatsapp.net", "data": "AAAA", "path": "/etc/passwd"})),
            ("whatsrust_status_image", json!({"recipients": ["972500000000"], "data": "AAAA", "path": "/etc/passwd"})),
            ("whatsrust_status_video", json!({"recipients": ["972500000000"], "data": "AAAA", "path": "/etc/passwd"})),
        ];
        for (tool, args) in cases {
            assert_rejected_before_sending(tool, args, "path").await;
        }
    }

    #[tokio::test]
    async fn test_call_tool_send_rejects_schedule_at_and_link_preview_without_sending() {
        // Sync mode cannot honour either field; dropping them would turn a scheduled or
        // previewed send into an immediate plain one, which cannot be undone.
        let scheduled = json!({"jid": "1@s.whatsapp.net", "text": "hi", "schedule_at": 1_900_000_000});
        assert_rejected_before_sending("whatsrust_send", scheduled, "schedule_at").await;

        let previewed = json!({"jid": "1@s.whatsapp.net", "text": "see https://example.com", "link_preview": {"url": "https://example.com"}});
        assert_rejected_before_sending("whatsrust_send", previewed, "link_preview").await;
    }

    #[tokio::test]
    async fn test_call_tool_image_forwards_exactly_the_declared_keys() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let args = json!({"jid": "1@s.whatsapp.net", "data": "AAAA", "mime": "image/png", "caption": "c"});
        let sent_args = args.clone();
        let client = std::thread::spawn(move || call_tool("whatsrust_image", sent_args, port));

        let raw = serve_one_request(&listener, STUB_RESPONSE).await;
        client.join().expect("client thread panicked").expect("a tool result");

        let (head, body) = request_head_and_body(&raw);
        assert!(head.starts_with("POST /api/image "), "unexpected request line: {head}");
        let sent: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(sent, args, "a call with only declared keys is forwarded with exactly those keys");
    }

    #[tokio::test]
    async fn test_call_tool_null_arguments_work_for_a_no_param_tool() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let client = std::thread::spawn(move || call_tool("whatsrust_status", Value::Null, port));

        let raw = serve_one_request(&listener, STUB_OK_RESPONSE).await;
        let result = client.join().expect("client thread panicked").expect("a tool result");

        let (head, _) = request_head_and_body(&raw);
        assert!(head.starts_with("GET /api/status "), "unexpected request line: {head}");
        assert!(result.get("isError").is_none(), "null arguments must be treated as {{}}: {result}");
        assert_eq!(result["content"][0]["text"], json!("{\"ok\":true}"));
    }

    // -------------------------------------------------------------------------
    // Wire equivalence: for every tool, the exact request (method, target including the query
    // string and its parameter order, JSON body) that a valid `tools/call` puts on the wire.
    // Pins the tool -> REST mapping so it can move (shared registry) without changing what
    // the daemon receives.
    // -------------------------------------------------------------------------

    /// All 34 tool names, in `tools/list` order.
    const ALL_TOOL_NAMES: [&str; 34] = [
        "whatsrust_status", "whatsrust_send", "whatsrust_reply", "whatsrust_react", "whatsrust_edit",
        "whatsrust_revoke", "whatsrust_image", "whatsrust_groups", "whatsrust_group_info",
        "whatsrust_history", "whatsrust_search", "whatsrust_fetch_history", "whatsrust_fetch_status",
        "whatsrust_fetch_cancel", "whatsrust_typing", "whatsrust_location", "whatsrust_contact",
        "whatsrust_poll", "whatsrust_pin_chat", "whatsrust_unpin_chat", "whatsrust_mute_chat",
        "whatsrust_unmute_chat", "whatsrust_archive_chat", "whatsrust_unarchive_chat",
        "whatsrust_mark_read", "whatsrust_delete_chat", "whatsrust_delete_for_me", "whatsrust_star",
        "whatsrust_unstar", "whatsrust_status_text", "whatsrust_status_image", "whatsrust_status_video",
        "whatsrust_status_revoke", "whatsrust_purge_embeddings",
    ];

    /// A `tools/call` for `name` through the JSON-RPC entry point.
    fn rpc_tools_call(name: &str, args: Value, port: u16) -> JsonRpcResponse {
        let req = JsonRpcRequest {
            jsonrpc: None,
            id: json!(1),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": name, "arguments": args })),
        };
        handle_rpc(&req, port)
    }

    /// One expected request: tool, valid arguments, the exact request line without the HTTP
    /// version (`"METHOD target"`), and the exact JSON body (`None` for GET).
    struct WireCase {
        tool: &'static str,
        args: Value,
        request: &'static str,
        body: Option<Value>,
    }

    fn wire_get(tool: &'static str, args: Value, request: &'static str) -> WireCase {
        WireCase { tool, args, request, body: None }
    }

    /// A POST whose body is the arguments verbatim.
    fn wire_post(tool: &'static str, request: &'static str, args: Value) -> WireCase {
        WireCase { tool, body: Some(args.clone()), args, request }
    }

    fn wire_cases() -> Vec<WireCase> {
        let jid = "123456789@s.whatsapp.net";
        let group = "120363000000000000@g.us";
        let msg_target = json!({"jid": group, "id": "3EB0AAAA", "sender": jid, "from_me": false});
        let msg_minimal = json!({"jid": group, "id": "3EB0AAAA"});
        let chat = json!({"jid": group});
        vec![
            wire_get("whatsrust_status", json!({}), "GET /api/status"),
            wire_get("whatsrust_groups", json!({}), "GET /api/groups"),
            wire_get("whatsrust_group_info", json!({"jid": group}), "GET /api/group-info?jid=120363000000000000%40g.us"),
            wire_get(
                "whatsrust_group_info",
                json!({"jid": "a b&c=d+\u{e9}/x?y#z@g.us"}),
                "GET /api/group-info?jid=a%20b%26c%3Dd%2B%C3%A9%2Fx%3Fy%23z%40g.us",
            ),
            // history: jid, limit (default 20), then optional before
            wire_get("whatsrust_history", json!({"jid": jid}), "GET /api/history?jid=123456789%40s.whatsapp.net&limit=20"),
            wire_get("whatsrust_history", json!({"jid": jid, "limit": 5}), "GET /api/history?jid=123456789%40s.whatsapp.net&limit=5"),
            wire_get(
                "whatsrust_history",
                json!({"jid": jid, "limit": 5, "before": 1_700_000_000}),
                "GET /api/history?jid=123456789%40s.whatsapp.net&limit=5&before=1700000000",
            ),
            wire_get(
                "whatsrust_history",
                json!({"jid": jid, "before": 1_700_000_000}),
                "GET /api/history?jid=123456789%40s.whatsapp.net&limit=20&before=1700000000",
            ),
            // search: q, limit (default 20), then optional jid
            wire_get("whatsrust_search", json!({"q": "hello world"}), "GET /api/search?q=hello%20world&limit=20"),
            wire_get("whatsrust_search", json!({"q": "x", "jid": jid}), "GET /api/search?q=x&limit=20&jid=123456789%40s.whatsapp.net"),
            wire_get(
                "whatsrust_search",
                json!({"q": "h\u{e9}llo", "jid": jid, "limit": 7}),
                "GET /api/search?q=h%C3%A9llo&limit=7&jid=123456789%40s.whatsapp.net",
            ),
            // fetch_history: body rebuilt as {chat_jid, mode (default "all"), target_value if present}
            WireCase {
                tool: "whatsrust_fetch_history",
                args: json!({"chat_jid": group}),
                request: "POST /api/history-fetch",
                body: Some(json!({"chat_jid": group, "mode": "all"})),
            },
            WireCase {
                tool: "whatsrust_fetch_history",
                args: json!({"chat_jid": group, "mode": "count", "target_value": 50}),
                request: "POST /api/history-fetch",
                body: Some(json!({"chat_jid": group, "mode": "count", "target_value": 50})),
            },
            WireCase {
                tool: "whatsrust_fetch_history",
                args: json!({"chat_jid": group, "mode": "since", "target_value": 1_700_000_000_000_i64}),
                request: "POST /api/history-fetch",
                body: Some(json!({"chat_jid": group, "mode": "since", "target_value": 1_700_000_000_000_i64})),
            },
            WireCase {
                tool: "whatsrust_fetch_history",
                args: json!({"chat_jid": group, "target_value": 10}),
                request: "POST /api/history-fetch",
                body: Some(json!({"chat_jid": group, "mode": "all", "target_value": 10})),
            },
            // fetch_status: job_id if present, else active (default true)
            wire_get("whatsrust_fetch_status", json!({"job_id": 42}), "GET /api/history-fetch?job_id=42"),
            wire_get("whatsrust_fetch_status", json!({}), "GET /api/history-fetch?active=true"),
            wire_get("whatsrust_fetch_status", json!({"active": false}), "GET /api/history-fetch?active=false"),
            wire_get("whatsrust_fetch_status", json!({"active": true}), "GET /api/history-fetch?active=true"),
            wire_get("whatsrust_fetch_status", json!({"job_id": 7, "active": false}), "GET /api/history-fetch?job_id=7"),
            wire_post("whatsrust_fetch_cancel", "POST /api/history-fetch/cancel", json!({"job_id": 3})),
            // send is always sync
            wire_post(
                "whatsrust_send",
                "POST /api/send?sync=true",
                json!({"jid": jid, "text": "hi there", "mentions": ["1@s.whatsapp.net", "2@s.whatsapp.net"]}),
            ),
            wire_post("whatsrust_send", "POST /api/send?sync=true", json!({"jid": jid, "text": "hi"})),
            wire_post(
                "whatsrust_reply",
                "POST /api/reply",
                json!({"jid": group, "id": "3EB0AAAA", "sender": jid, "text": "yes", "mentions": [jid]}),
            ),
            wire_post(
                "whatsrust_react",
                "POST /api/react",
                json!({"jid": group, "id": "3EB0AAAA", "emoji": "\u{1f44d}", "from_me": false, "sender_jid": jid}),
            ),
            wire_post("whatsrust_react", "POST /api/react", json!({"jid": group, "id": "3EB0AAAA", "emoji": "\u{1f44d}"})),
            wire_post("whatsrust_edit", "POST /api/edit", json!({"jid": group, "id": "3EB0AAAA", "text": "fixed"})),
            wire_post("whatsrust_revoke", "POST /api/revoke", msg_minimal.clone()),
            wire_post(
                "whatsrust_image",
                "POST /api/image",
                json!({"jid": jid, "data": "AAAA", "mime": "image/png", "caption": "c"}),
            ),
            wire_post("whatsrust_image", "POST /api/image", json!({"jid": jid, "data": "AAAA"})),
            wire_post("whatsrust_typing", "POST /api/typing", chat.clone()),
            wire_post(
                "whatsrust_location",
                "POST /api/location",
                json!({"jid": jid, "lat": 31.5, "lon": 34.75, "name": "Here", "address": "1 Main St"}),
            ),
            wire_post("whatsrust_contact", "POST /api/contact", json!({"jid": jid, "name": "Ann", "phone": "972500000000"})),
            wire_post(
                "whatsrust_poll",
                "POST /api/poll",
                json!({"jid": group, "question": "Lunch?", "options": ["pizza", "sushi"], "selectable_count": 1}),
            ),
            wire_post("whatsrust_pin_chat", "POST /api/pin-chat", chat.clone()),
            wire_post("whatsrust_unpin_chat", "POST /api/unpin-chat", chat.clone()),
            wire_post("whatsrust_mute_chat", "POST /api/mute-chat", chat.clone()),
            wire_post("whatsrust_unmute_chat", "POST /api/unmute-chat", chat.clone()),
            wire_post("whatsrust_archive_chat", "POST /api/archive-chat", chat.clone()),
            wire_post("whatsrust_unarchive_chat", "POST /api/unarchive-chat", chat.clone()),
            wire_post("whatsrust_mark_read", "POST /api/mark-read", chat.clone()),
            wire_post("whatsrust_delete_chat", "POST /api/delete-chat", chat.clone()),
            wire_post("whatsrust_delete_for_me", "POST /api/delete-for-me", msg_target.clone()),
            wire_post("whatsrust_delete_for_me", "POST /api/delete-for-me", msg_minimal.clone()),
            wire_post("whatsrust_star", "POST /api/star", msg_target.clone()),
            wire_post("whatsrust_unstar", "POST /api/unstar", msg_target.clone()),
            wire_post(
                "whatsrust_status_text",
                "POST /api/status-text",
                json!({"recipients": ["972500000000"], "text": "hello", "background_argb": 4_280_184_399_i64, "font": 1, "privacy": "contacts"}),
            ),
            wire_post(
                "whatsrust_status_image",
                "POST /api/status-image",
                json!({"recipients": ["972500000000"], "data": "AAAA", "mime": "image/png", "caption": "c", "privacy": "contacts"}),
            ),
            wire_post(
                "whatsrust_status_video",
                "POST /api/status-video",
                json!({"recipients": ["972500000000"], "data": "AAAA", "mime": "video/mp4", "caption": "c", "seconds": 12, "privacy": "contacts"}),
            ),
            wire_post(
                "whatsrust_status_revoke",
                "POST /api/status-revoke",
                json!({"recipients": ["972500000000"], "message_id": "3EB0BBBB", "privacy": "contacts"}),
            ),
            wire_post("whatsrust_purge_embeddings", "POST /api/embeddings/purge", json!({"model_id": "bge-m3"})),
        ]
    }

    #[tokio::test]
    async fn test_call_tool_wire_requests_for_every_tool() {
        let cases = wire_cases();

        let covered: std::collections::BTreeSet<&str> = cases.iter().map(|c| c.tool).collect();
        let expected: std::collections::BTreeSet<&str> = ALL_TOOL_NAMES.iter().copied().collect();
        assert_eq!(ALL_TOOL_NAMES.len(), expected.len(), "ALL_TOOL_NAMES must not repeat a tool");
        assert_eq!(covered, expected, "every one of the 34 tools needs at least one wire case");

        for case in cases {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();

            let (tool, args) = (case.tool, case.args.clone());
            let client = std::thread::spawn(move || rpc_tools_call(tool, args, port));
            let raw = serve_one_request(&listener, STUB_OK_RESPONSE).await;
            let response = client.join().expect("client thread panicked");

            let label = format!("{} {}", case.tool, case.args);
            let result = response.result.unwrap_or_else(|| panic!("{label}: expected a tool result, got {:?}", response.error));
            assert!(result.get("isError").is_none(), "{label}: valid arguments must not be a tool error: {result}");
            assert_eq!(result["content"][0]["text"], json!("{\"ok\":true}"), "{label}");

            let (head, body) = request_head_and_body(&raw);
            let request_line = head.lines().next().expect("request line");
            assert_eq!(request_line, format!("{} HTTP/1.1", case.request), "{label}: request line");
            match &case.body {
                Some(expected) => {
                    let body = String::from_utf8(body).expect("UTF-8 body");
                    assert_eq!(body, serde_json::to_string(expected).unwrap(), "{label}: body");
                    assert!(
                        head.contains(&format!("Content-Length: {}", body.len())),
                        "{label}: Content-Length must match the body: {head}"
                    );
                }
                None => assert!(body.is_empty(), "{label}: a GET carries no body"),
            }
        }
    }

    // -------------------------------------------------------------------------
    // Shared registry: the stdio server lists, validates and maps tools through it.
    // -------------------------------------------------------------------------

    #[test]
    fn test_registry_lists_the_34_tools_once_each_in_the_original_order() {
        let names: Vec<String> = REGISTRY
            .tool_definitions()
            .iter()
            .map(|t| t["name"].as_str().expect("tool has a name").to_string())
            .collect();
        assert_eq!(names, ALL_TOOL_NAMES);
        let unique: std::collections::BTreeSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), 34, "tool names must be unique");
    }

    #[test]
    fn test_tools_list_entries_carry_annotations() {
        let req = JsonRpcRequest { jsonrpc: None, id: json!(7), method: "tools/list".to_string(), params: None };
        let response = handle_rpc(&req, 1);
        assert_eq!(response.id, json!(7));
        let result = response.result.expect("tools/list result");
        let tools = result["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 34);
        for tool in tools {
            let annotations = tool["annotations"].as_object().unwrap_or_else(|| panic!("{}: missing annotations", tool["name"]));
            assert!(annotations["readOnlyHint"].is_boolean(), "{}: readOnlyHint", tool["name"]);
            assert!(annotations["openWorldHint"].is_boolean(), "{}: openWorldHint", tool["name"]);
        }
        let status = tools.iter().find(|t| t["name"] == "whatsrust_status").expect("whatsrust_status");
        assert_eq!(status["annotations"], json!({"readOnlyHint": true, "openWorldHint": false}));
        let send = tools.iter().find(|t| t["name"] == "whatsrust_send").expect("whatsrust_send");
        assert_eq!(
            send["annotations"],
            json!({"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true})
        );
    }

    #[tokio::test]
    async fn test_call_tool_rejects_wrong_type_and_missing_required_without_sending() {
        assert_tool_error_before_sending(
            "whatsrust_send",
            json!({"jid": 5, "text": "hi"}),
            "whatsrust_send: invalid argument(s): jid must be a string. Nothing was sent.",
        ).await;
        assert_tool_error_before_sending(
            "whatsrust_send",
            json!({"jid": "1@s.whatsapp.net"}),
            "whatsrust_send: invalid argument(s): text is required. Nothing was sent.",
        ).await;
        assert_tool_error_before_sending(
            "whatsrust_history",
            json!({"jid": "1@s.whatsapp.net", "limit": 5.5}),
            "whatsrust_history: invalid argument(s): limit must be an integer (64-bit). Nothing was sent.",
        ).await;
        assert_tool_error_before_sending(
            "whatsrust_poll",
            json!({"jid": "1@g.us", "options": "pizza"}),
            "whatsrust_poll: invalid argument(s): options must be an array; question is required; selectable_count is required. Nothing was sent.",
        ).await;
    }

    #[tokio::test]
    async fn test_tools_call_integral_float_is_sent_as_an_integer() {
        let line = sent_request_line("whatsrust_history", json!({"jid": "1@g.us", "limit": 5.0})).await;
        assert_eq!(line, "GET /api/history?jid=1%40g.us&limit=5 HTTP/1.1");
    }

    #[tokio::test]
    async fn test_tools_call_optional_null_is_not_forwarded() {
        let line = sent_request_line("whatsrust_history", json!({"jid": "1@g.us", "before": null})).await;
        assert_eq!(line, "GET /api/history?jid=1%40g.us&limit=20 HTTP/1.1");

        let line = sent_request_line("whatsrust_search", json!({"q": "x", "jid": null, "limit": null})).await;
        assert_eq!(line, "GET /api/search?q=x&limit=20 HTTP/1.1");
    }

    #[test]
    fn test_whatsrust_send_schema_declares_neither_schedule_at_nor_link_preview() {
        // Sync-only (`/api/send?sync=true`), which rejects both fields with a 400. REST and the
        // CLI keep supporting them.
        let declared = declared_properties("whatsrust_send").expect("whatsrust_send exists");
        for key in ["schedule_at", "link_preview"] {
            assert!(!declared.contains_key(key), "whatsrust_send must not declare `{key}`");
        }
        let tools = REGISTRY.tool_definitions();
        let send = tools.iter().find(|t| t["name"] == "whatsrust_send").unwrap();
        let description = send["description"].as_str().unwrap().to_lowercase();
        assert!(!description.contains("link preview"), "description must not advertise link previews: {description}");
    }

    #[test]
    fn test_every_tool_schema_forbids_additional_properties() {
        for tool in REGISTRY.tool_definitions() {
            let name = tool["name"].as_str().expect("tool has a name");
            assert_eq!(
                tool.pointer("/inputSchema/additionalProperties"),
                Some(&json!(false)),
                "{name}: inputSchema must set additionalProperties:false (undeclared keys are rejected)"
            );
        }
    }

    /// The `inputSchema.properties` of the named tool, or `None` for an unknown tool.
    fn declared_properties(name: &str) -> Option<serde_json::Map<String, Value>> {
        REGISTRY.find_tool(name).map(|t| t.properties().clone())
    }

    /// Hand-maintained mirror of `src/api.rs`: for each POST-forwarding tool, the fields its REST
    /// request struct REQUIRES (non-`Option`, no `#[serde(default)]`). Update it together with
    /// those structs. It exists so the strict argument check can never reject a field that the
    /// REST handler cannot do without (the schema would make the tool unusable).
    const REST_REQUIRED_FIELDS: &[(&str, &[&str])] = &[
        ("whatsrust_send", &["jid", "text"]),                  // SendReq
        ("whatsrust_reply", &["jid", "id", "text"]),           // ReplyReq
        ("whatsrust_react", &["jid", "id", "emoji"]),          // ReactReq
        ("whatsrust_edit", &["jid", "id", "text"]),            // EditReq
        ("whatsrust_revoke", &["jid", "id"]),                  // RevokeReq
        ("whatsrust_image", &["jid"]),                         // MediaReq
        ("whatsrust_typing", &["jid"]),                        // JidReq
        ("whatsrust_location", &["jid", "lat", "lon"]),        // LocationReq
        ("whatsrust_contact", &["jid", "name", "phone"]),      // ContactReq
        ("whatsrust_poll", &["jid", "question", "options", "selectable_count"]), // PollReq
        ("whatsrust_pin_chat", &["jid"]),                      // JidReq
        ("whatsrust_unpin_chat", &["jid"]),
        ("whatsrust_mute_chat", &["jid"]),
        ("whatsrust_unmute_chat", &["jid"]),
        ("whatsrust_archive_chat", &["jid"]),
        ("whatsrust_unarchive_chat", &["jid"]),
        ("whatsrust_mark_read", &["jid"]),
        ("whatsrust_delete_chat", &["jid"]),
        ("whatsrust_delete_for_me", &["jid", "id"]),           // MessageActionReq
        ("whatsrust_star", &["jid", "id"]),
        ("whatsrust_unstar", &["jid", "id"]),
        ("whatsrust_status_text", &["recipients", "text"]),    // StatusTextReq
        ("whatsrust_status_image", &["recipients", "data"]),   // StatusMediaReq
        ("whatsrust_status_video", &["recipients", "data"]),
        ("whatsrust_status_revoke", &["recipients", "message_id"]), // StatusRevokeReq
    ];

    /// Tools intentionally absent from `REST_REQUIRED_FIELDS`: GET tools, and POST tools whose
    /// REST struct has no required field (all `Option`) or whose body its registry mapping rebuilds.
    const NO_REQUIRED_REST_STRUCT: &[&str] = &[
        "whatsrust_status", "whatsrust_groups", "whatsrust_group_info", "whatsrust_history",
        "whatsrust_search", "whatsrust_fetch_status", "whatsrust_fetch_history",
        "whatsrust_fetch_cancel", "whatsrust_purge_embeddings",
    ];

    #[test]
    fn test_schemas_declare_every_field_the_rest_handler_requires() {
        for (tool, required) in REST_REQUIRED_FIELDS {
            let declared = declared_properties(tool)
                .unwrap_or_else(|| panic!("{tool}: in REST_REQUIRED_FIELDS but not a tool definition"));
            for field in *required {
                assert!(
                    declared.contains_key(*field),
                    "{tool}: REST requires `{field}` but the schema does not declare it, \
                     so the declared-args filter would drop it"
                );
            }
        }
    }

    #[test]
    fn test_every_tool_is_classified_for_the_rest_required_fields_guard() {
        // A new tool must be added to REST_REQUIRED_FIELDS (or consciously exempted), so the
        // guard above cannot be bypassed by simply not listing it.
        for tool in REGISTRY.tool_definitions() {
            let name = tool["name"].as_str().expect("tool has a name");
            let in_table = REST_REQUIRED_FIELDS.iter().filter(|(t, _)| *t == name).count();
            let exempt = NO_REQUIRED_REST_STRUCT.contains(&name);
            assert!(
                (in_table == 1 && !exempt) || (in_table == 0 && exempt),
                "{name}: must be in exactly one of REST_REQUIRED_FIELDS / NO_REQUIRED_REST_STRUCT"
            );
        }
        for name in NO_REQUIRED_REST_STRUCT {
            assert!(declared_properties(name).is_some(), "{name}: exempt entry is not a tool definition");
        }
    }

    #[test]
    fn test_every_required_schema_key_is_declared_in_properties() {
        for tool in REGISTRY.tool_definitions() {
            let name = tool["name"].as_str().expect("tool has a name");
            let declared = declared_properties(name).expect("properties object");
            let required = tool.pointer("/inputSchema/required").and_then(Value::as_array);
            for key in required.into_iter().flatten().filter_map(Value::as_str) {
                assert!(declared.contains_key(key), "{name}: required `{key}` is not declared in properties");
            }
        }
    }

    #[test]
    fn test_tools_call_unknown_tool_is_invalid_params_without_panicking() {
        // Port 1 is never dialled: the unknown tool is rejected before any HTTP call, whatever
        // the arguments look like.
        for args in [json!({"path": "/etc/passwd"}), json!("not an object"), Value::Null] {
            let response = rpc_tools_call("whatsrust_nope", args.clone(), 1);
            assert_eq!(response.result, None, "args: {args}");
            let error = response.error.expect("error object");
            assert_eq!(error["code"], json!(-32602), "args: {args}");
            assert_eq!(error["message"], json!("Unknown tool: whatsrust_nope"), "args: {args}");
        }
        let response = rpc_tools_call("", json!({}), 1);
        assert_eq!(response.error.expect("error object")["message"], json!("Unknown tool: "));
    }

    #[test]
    fn test_every_tool_definition_has_an_object_input_schema_properties() {
        // Guards the validator's single source of truth: a tool whose schema loses its
        // `properties` object would reject every argument.
        for tool in REGISTRY.tool_definitions() {
            let name = tool["name"].as_str().expect("tool has a name");
            assert!(
                matches!(tool.pointer("/inputSchema/properties"), Some(Value::Object(_))),
                "{name}: inputSchema.properties must be an object"
            );
            assert!(declared_properties(name).is_some(), "{name}: declared_properties must resolve it");
        }
    }

    #[test]
    fn test_no_tool_schema_declares_a_path_property() {
        // `path` makes the REST media handlers read a local file and send it; declaring it in an
        // MCP schema would let it through the strict argument check and re-open that route.
        for tool in REGISTRY.tool_definitions() {
            let name = tool["name"].as_str().expect("tool has a name");
            let declared = declared_properties(name).expect("properties object");
            assert!(!declared.contains_key("path"), "{name}: must not declare `path`");
        }
    }

    // -------------------------------------------------------------------------
    // POST read failures leave the outcome unknown: the error must say so.
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_mcp_http_post_read_timeout_says_request_may_still_be_processed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let client = std::thread::spawn(move || {
            http_post_with_timeout(port, "/api/send?sync=true", &json!({}), Duration::from_millis(200))
        });

        // Accept but never answer, and keep the connection open until the client gives up.
        let (_stream, _) = listener.accept().await.unwrap();
        let err = client.join().expect("client thread panicked").expect_err("must time out");
        assert!(err.contains("read failed"), "unexpected error: {err}");
        assert!(err.contains("may still be processed"), "POST timeout must warn about retrying: {err}");
    }
}

/// Normalize bind host — replace wildcard "0.0.0.0" with "127.0.0.1" for connections.
fn mcp_connect_host() -> String {
    let bind = std::env::var("WHATSRUST_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    if bind == "0.0.0.0" || bind == "::" {
        "127.0.0.1".to_string()
    } else {
        bind
    }
}
