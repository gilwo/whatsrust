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
        "tools/list" => json_rpc_ok(req.id.clone(), json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let params = req.params.as_ref().unwrap_or(&Value::Null);
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let result = call_tool(name, &args, port);
            json_rpc_ok(req.id.clone(), result)
        }
        _ => json_rpc_error(req.id.clone(), -32601, &format!("method not found: {}", req.method)),
    }
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool_def("whatsrust_status", "Get bridge status, metrics, and connection state", json!({"type":"object","properties":{}})),
        tool_def("whatsrust_send", "Send a text message (optionally with @mentions)",
            json!({"type":"object","properties":{
                "jid":{"type":"string","description":"Recipient JID (phone@s.whatsapp.net or group@g.us)"},
                "text":{"type":"string","description":"Message text"},
                "mentions":{"type":"array","items":{"type":"string"},"description":"JIDs to @mention (optional)"}
            },"required":["jid","text"]})),
        tool_def("whatsrust_reply", "Reply to a message (quote)",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},
                "id":{"type":"string","description":"Message ID to reply to"},
                "sender":{"type":"string","description":"Sender JID of the quoted message"},
                "text":{"type":"string"},
                "mentions":{"type":"array","items":{"type":"string"},"description":"JIDs to @mention"}
            },"required":["jid","id","sender","text"]})),
        tool_def("whatsrust_react", "React to a message with an emoji",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},
                "id":{"type":"string","description":"Target message ID"},
                "emoji":{"type":"string"},
                "from_me":{"type":"boolean","description":"Whether target is from us (default true)"},
                "sender_jid":{"type":"string","description":"Target sender JID (required for group reactions when from_me=false)"}
            },"required":["jid","id","emoji"]})),
        tool_def("whatsrust_edit", "Edit a previously sent message",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string"},"text":{"type":"string"}
            },"required":["jid","id","text"]})),
        tool_def("whatsrust_revoke", "Delete a message for everyone",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string"}
            },"required":["jid","id"]})),
        tool_def("whatsrust_image", "Send an image (base64)",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"data":{"type":"string","description":"Base64-encoded image"},
                "mime":{"type":"string","description":"MIME type (default image/jpeg)"},
                "caption":{"type":"string"}
            },"required":["jid","data"]})),
        tool_def("whatsrust_groups", "List all joined groups", json!({"type":"object","properties":{}})),
        tool_def("whatsrust_group_info", "Get group details and members",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_history", "Get recent messages for a chat",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"limit":{"type":"integer","description":"Max messages (default 20)"},
                "before":{"type":"integer","description":"Unix timestamp for pagination"}
            },"required":["jid"]})),
        tool_def("whatsrust_search", "Search message history",
            json!({"type":"object","properties":{
                "q":{"type":"string","description":"Search query"},
                "jid":{"type":"string","description":"Limit to specific chat (optional)"},
                "limit":{"type":"integer"}
            },"required":["q"]})),
        tool_def("whatsrust_fetch_history",
            "Trigger a per-chat historical backfill. Runs as a paced background job (anti-ban) and is daemon-clamped (cooldown, queue-depth, max-messages); poll whatsrust_fetch_status for progress.",
            json!({"type":"object","properties":{
                "chat_jid":{"type":"string","description":"Chat JID to backfill"},
                "mode":{"type":"string","description":"all | since | count; default all"},
                "target_value":{"type":"integer","description":"ms-timestamp for since, count for count"}
            },"required":["chat_jid"]})),
        tool_def("whatsrust_fetch_status", "Poll history-fetch job status. Pass job_id for a single job, or active=true to list only active jobs, or omit both for all jobs.",
            json!({"type":"object","properties":{
                "job_id":{"type":"integer","description":"Specific job ID to query (optional)"},
                "active":{"type":"boolean","description":"List only active jobs (optional)"}
            }})),
        tool_def("whatsrust_fetch_cancel", "Cancel a history-fetch backfill job",
            json!({"type":"object","properties":{
                "job_id":{"type":"integer","description":"Job ID to cancel"}
            },"required":["job_id"]})),
        tool_def("whatsrust_typing", "Send typing indicator",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_location", "Send a location pin",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"lat":{"type":"number"},"lon":{"type":"number"},
                "name":{"type":"string"},"address":{"type":"string"}
            },"required":["jid","lat","lon"]})),
        tool_def("whatsrust_contact", "Send a contact card",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"name":{"type":"string"},
                "phone":{"type":"string","description":"Phone number of the contact in international format, digits only (e.g. 972500000000)"}
            },"required":["jid","name","phone"]})),
        tool_def("whatsrust_poll", "Create a poll",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"question":{"type":"string"},
                "options":{"type":"array","items":{"type":"string"}},
                "selectable_count":{"type":"integer","description":"Max selectable options"}
            },"required":["jid","question","options","selectable_count"]})),
        // Chat management
        tool_def("whatsrust_pin_chat", "Pin a chat to the top",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_unpin_chat", "Unpin a chat",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_mute_chat", "Mute a chat indefinitely",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_unmute_chat", "Unmute a chat",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_archive_chat", "Archive a chat",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_unarchive_chat", "Unarchive a chat",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_mark_read", "Mark a chat as read",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_delete_chat", "Delete a chat and its media",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_delete_for_me", "Delete a message for me only",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
            },"required":["jid","id"]})),
        tool_def("whatsrust_star", "Star a message",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
            },"required":["jid","id"]})),
        tool_def("whatsrust_unstar", "Unstar a message",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
            },"required":["jid","id"]})),
        tool_def("whatsrust_status_text", "Post a text status/story to specified recipients",
            json!({"type":"object","properties":{
                "recipients":{"type":"array","items":{"type":"string"},"description":"Phone numbers of recipients"},
                "text":{"type":"string","description":"Status text"},
                "background_argb":{"type":"integer","description":"Background color as 0xAARRGGBB (default 0xFF1E6E4F)"},
                "font":{"type":"integer","description":"Font style 0-4 (default 0)"},
                "privacy":{"type":"string","description":"contacts|allowlist|denylist (default contacts)"}
            },"required":["recipients","text"]})),
        tool_def("whatsrust_status_image", "Post an image status/story",
            json!({"type":"object","properties":{
                "recipients":{"type":"array","items":{"type":"string"}},
                "data":{"type":"string","description":"Base64-encoded image"},
                "mime":{"type":"string","description":"MIME type (default image/jpeg)"},
                "caption":{"type":"string"},
                "privacy":{"type":"string"}
            },"required":["recipients","data"]})),
        tool_def("whatsrust_status_video", "Post a video status/story",
            json!({"type":"object","properties":{
                "recipients":{"type":"array","items":{"type":"string"}},
                "data":{"type":"string","description":"Base64-encoded video"},
                "mime":{"type":"string","description":"MIME type (default video/mp4)"},
                "caption":{"type":"string"},
                "seconds":{"type":"integer","description":"Duration in seconds"},
                "privacy":{"type":"string"}
            },"required":["recipients","data"]})),
        tool_def("whatsrust_status_revoke", "Revoke a previously posted status/story",
            json!({"type":"object","properties":{
                "recipients":{"type":"array","items":{"type":"string"},"description":"Same recipients as original post"},
                "message_id":{"type":"string","description":"ID returned when status was posted"},
                "privacy":{"type":"string"}
            },"required":["recipients","message_id"]})),
        tool_def("whatsrust_purge_embeddings", "Purge all embeddings for a given model and reclaim disk space (M2.5)",
            json!({"type":"object","properties":{
                "model_id":{"type":"string","description":"Model ID to purge embeddings for"}
            },"required":["model_id"]})),
    ]
}

fn tool_def(name: &str, description: &str, mut input_schema: Value) -> Value {
    // Every tool is strict: `validated_args` rejects undeclared keys, so the advertised
    // schema says the same. Set here once instead of in each schema literal.
    input_schema["additionalProperties"] = json!(false);
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

/// The `inputSchema.properties` of the named tool, or `None` for an unknown tool or a schema
/// without an object `properties`.
fn declared_properties(name: &str) -> Option<serde_json::Map<String, Value>> {
    tool_definitions()
        .into_iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|t| t.pointer("/inputSchema/properties").and_then(Value::as_object).cloned())
}

/// The top-level keys of `given` that are not in `declared`, sorted.
fn unknown_keys(given: &serde_json::Map<String, Value>, declared: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut unknown: Vec<String> = given.keys().filter(|k| !declared.contains_key(k.as_str())).cloned().collect();
    unknown.sort();
    unknown
}

/// Error text for a call that passed undeclared arguments: names the tool, the unknown key(s)
/// and the allowed keys (both sorted), and states that nothing was sent.
fn unknown_args_message(name: &str, unknown: &[String], declared: &serde_json::Map<String, Value>) -> String {
    let mut allowed: Vec<&str> = declared.keys().map(String::as_str).collect();
    allowed.sort_unstable();
    let allowed = if allowed.is_empty() { "(none)".to_string() } else { allowed.join(", ") };
    format!(
        "{name}: unknown argument(s): {}. Allowed arguments: {allowed}. Nothing was sent.",
        unknown.join(", ")
    )
}

/// Check a tool call's `arguments` against that tool's declared `inputSchema.properties` and
/// return the arguments to forward. Absent or `null` arguments mean `{}`; any non-object value
/// is an error, and so is any top-level key the tool does not declare (e.g. `path`, which would
/// make the daemon read a local file and send it, or `schedule_at`, which a sync send cannot
/// honour and would silently turn into an immediate send). Only key names are checked, not
/// value types. An unknown tool passes through as `{}`; `call_tool`'s dispatch reports it.
fn validated_args(name: &str, args: &Value) -> Result<Value, String> {
    let Some(declared) = declared_properties(name) else {
        return Ok(json!({}));
    };
    let given = match args {
        Value::Null => return Ok(json!({})),
        Value::Object(map) => map,
        _ => return Err("arguments must be a JSON object".to_string()),
    };
    let unknown = unknown_keys(given, &declared);
    if unknown.is_empty() {
        Ok(args.clone())
    } else {
        Err(unknown_args_message(name, &unknown, &declared))
    }
}

/// MCP tool result carrying an error message (`isError: true`).
fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn call_tool(name: &str, args: &Value, port: u16) -> Value {
    // Single choke point: a call with undeclared or malformed arguments is rejected here,
    // before any HTTP request, so every tool below only ever sees schema-declared arguments.
    let args = &match validated_args(name, args) {
        Ok(args) => args,
        Err(message) => return tool_error(&message),
    };
    let result = match name {
        "whatsrust_status" => http_get(port, "/api/status"),
        "whatsrust_groups" => http_get(port, "/api/groups"),
        "whatsrust_group_info" => {
            let jid = args.get("jid").and_then(|v| v.as_str()).unwrap_or("");
            http_get(port, &format!("/api/group-info?jid={}", crate::api::urlencode(jid)))
        }
        "whatsrust_history" => {
            let jid = args.get("jid").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
            let before = args.get("before").and_then(|v| v.as_i64());
            let mut url = format!("/api/history?jid={}&limit={limit}", crate::api::urlencode(jid));
            if let Some(b) = before { url.push_str(&format!("&before={b}")); }
            http_get(port, &url)
        }
        "whatsrust_search" => {
            let q = args.get("q").and_then(|v| v.as_str()).unwrap_or("");
            let jid = args.get("jid").and_then(|v| v.as_str());
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
            let mut url = format!("/api/search?q={}&limit={limit}", crate::api::urlencode(q));
            if let Some(j) = jid { url.push_str(&format!("&jid={}", crate::api::urlencode(j))); }
            http_get(port, &url)
        }
        "whatsrust_send" => http_post(port, "/api/send?sync=true", args),
        "whatsrust_reply" => http_post(port, "/api/reply", args),
        "whatsrust_react" => http_post(port, "/api/react", args),
        "whatsrust_edit" => http_post(port, "/api/edit", args),
        "whatsrust_revoke" => http_post(port, "/api/revoke", args),
        "whatsrust_image" => http_post(port, "/api/image", args),
        "whatsrust_typing" => http_post(port, "/api/typing", args),
        "whatsrust_location" => http_post(port, "/api/location", args),
        "whatsrust_contact" => http_post(port, "/api/contact", args),
        "whatsrust_poll" => http_post(port, "/api/poll", args),
        "whatsrust_pin_chat" => http_post(port, "/api/pin-chat", args),
        "whatsrust_unpin_chat" => http_post(port, "/api/unpin-chat", args),
        "whatsrust_mute_chat" => http_post(port, "/api/mute-chat", args),
        "whatsrust_unmute_chat" => http_post(port, "/api/unmute-chat", args),
        "whatsrust_archive_chat" => http_post(port, "/api/archive-chat", args),
        "whatsrust_unarchive_chat" => http_post(port, "/api/unarchive-chat", args),
        "whatsrust_mark_read" => http_post(port, "/api/mark-read", args),
        "whatsrust_delete_chat" => http_post(port, "/api/delete-chat", args),
        "whatsrust_delete_for_me" => http_post(port, "/api/delete-for-me", args),
        "whatsrust_star" => http_post(port, "/api/star", args),
        "whatsrust_unstar" => http_post(port, "/api/unstar", args),
        "whatsrust_fetch_history" => {
            let chat_jid = args.get("chat_jid").and_then(|v| v.as_str()).unwrap_or("");
            let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("all");
            let mut body = serde_json::json!({"chat_jid": chat_jid, "mode": mode});
            if let Some(tv) = args.get("target_value").and_then(|v| v.as_i64()) { body["target_value"] = serde_json::json!(tv); }
            http_post(port, "/api/history-fetch", &body)
        }
        "whatsrust_fetch_status" => {
            if let Some(id) = args.get("job_id").and_then(|v| v.as_i64()) {
                http_get(port, &format!("/api/history-fetch?job_id={id}"))
            } else {
                // Default active=true (active-only) to match CLI behaviour; pass active:false
                // explicitly to list all jobs.
                let active = args.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
                http_get(port, &format!("/api/history-fetch?active={active}"))
            }
        }
        "whatsrust_fetch_cancel" => http_post(port, "/api/history-fetch/cancel", args),
        "whatsrust_status_text" => http_post(port, "/api/status-text", args),
        "whatsrust_status_image" => http_post(port, "/api/status-image", args),
        "whatsrust_status_video" => http_post(port, "/api/status-video", args),
        "whatsrust_status_revoke" => http_post(port, "/api/status-revoke", args),
        "whatsrust_purge_embeddings" => http_post(port, "/api/embeddings/purge", args),
        _ => Err(format!("unknown tool: {name}")),
    };

    match result {
        Ok(body) => json!({ "content": [{ "type": "text", "text": body }] }),
        Err(e) => tool_error(&e),
    }
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
        let tools = tool_definitions();
        let names: Vec<&str> = tools.iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"whatsrust_fetch_history"), "missing whatsrust_fetch_history");
        assert!(names.contains(&"whatsrust_fetch_status"), "missing whatsrust_fetch_status");
        assert!(names.contains(&"whatsrust_fetch_cancel"), "missing whatsrust_fetch_cancel");
    }

    #[test]
    fn test_fetch_history_description_mentions_pacing() {
        let tools = tool_definitions();
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

        let result = call_tool(tool, &args, port);

        assert_eq!(result["isError"], json!(true), "{tool}: must be a tool error");
        let text = result["content"][0]["text"].as_str().expect("error text");
        assert!(
            text.contains(tool) && text.contains("unknown argument") && text.contains(key),
            "{tool}: unexpected error text: {text}"
        );
        // call_tool is synchronous: had it dialled the port, the connection would already be
        // queued and accept() would return at once.
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "{tool}: a rejected call must not contact the daemon");
    }

    #[test]
    fn test_validated_args_error_names_tool_sorted_unknown_keys_and_allowed_keys() {
        let err = validated_args("whatsrust_image", &json!({"jid": "j", "data": "AAAA", "zeta": 1, "path": "/etc/passwd"}))
            .expect_err("undeclared keys must be rejected");
        assert_eq!(
            err,
            "whatsrust_image: unknown argument(s): path, zeta. Allowed arguments: caption, data, jid, mime. Nothing was sent."
        );

        let err = validated_args("whatsrust_status", &json!({"x": 1})).expect_err("no-param tool takes nothing");
        assert_eq!(err, "whatsrust_status: unknown argument(s): x. Allowed arguments: (none). Nothing was sent.");
    }

    #[test]
    fn test_validated_args_accepts_declared_keys_verbatim_including_nested_values() {
        let send = json!({"jid": "1@g.us", "text": "hi", "mentions": ["1@s.whatsapp.net", "2@s.whatsapp.net"]});
        assert_eq!(validated_args("whatsrust_send", &send), Ok(send.clone()));

        let required_only = json!({"jid": "1@g.us", "text": "hi"});
        assert_eq!(validated_args("whatsrust_send", &required_only), Ok(required_only.clone()));

        // Only key names are checked, not value types (that is a later design item).
        let wrong_type = json!({"jid": 5, "text": ["not", "a", "string"]});
        assert_eq!(validated_args("whatsrust_send", &wrong_type), Ok(wrong_type.clone()));
    }

    #[test]
    fn test_validated_args_null_arguments_mean_empty_object() {
        assert_eq!(validated_args("whatsrust_status", &Value::Null), Ok(json!({})));
        assert_eq!(validated_args("whatsrust_image", &Value::Null), Ok(json!({})));
    }

    #[test]
    fn test_validated_args_rejects_non_object_arguments() {
        for args in [json!("path"), json!(5), json!(true), json!(["jid"])] {
            assert_eq!(
                validated_args("whatsrust_image", &args),
                Err("arguments must be a JSON object".to_string()),
                "args: {args}"
            );
        }
    }

    #[test]
    fn test_validated_args_unknown_tool_passes_through_for_dispatch_to_report() {
        assert_eq!(validated_args("whatsrust_nope", &json!({"path": "/etc/passwd"})), Ok(json!({})));
        assert_eq!(validated_args("", &json!("not an object")), Ok(json!({})));
    }

    #[test]
    fn test_validated_args_contact_accepts_phone_and_rejects_vcard() {
        // REST builds (and sanitizes) the vCard itself from `name` + `phone`; a raw `vcard`
        // must never be forwarded.
        let ok = json!({"jid": "1@s.whatsapp.net", "name": "Ann", "phone": "972500000000"});
        assert_eq!(validated_args("whatsrust_contact", &ok), Ok(ok.clone()));

        let with_vcard = json!({"jid": "1@s.whatsapp.net", "name": "Ann", "phone": "972500000000", "vcard": "BEGIN:VCARD"});
        let err = validated_args("whatsrust_contact", &with_vcard).expect_err("vcard is not declared");
        assert!(err.contains("vcard"), "{err}");
    }

    #[test]
    fn test_call_tool_non_object_arguments_is_a_tool_error() {
        // Port 1 is never dialled: validation fails before any HTTP call.
        for args in [json!("path"), json!(5), json!(true), json!(["jid"])] {
            let result = call_tool("whatsrust_image", &args, 1);
            assert_eq!(result["isError"], json!(true), "args: {args}");
            assert_eq!(result["content"][0]["text"], json!("arguments must be a JSON object"), "args: {args}");
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
        let client = std::thread::spawn(move || call_tool("whatsrust_image", &sent_args, port));

        let raw = serve_one_request(&listener, STUB_RESPONSE).await;
        client.join().expect("client thread panicked");

        let (head, body) = request_head_and_body(&raw);
        assert!(head.starts_with("POST /api/image "), "unexpected request line: {head}");
        let sent: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(sent, args, "a call with only declared keys is forwarded with exactly those keys");
    }

    #[tokio::test]
    async fn test_call_tool_null_arguments_work_for_a_no_param_tool() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let client = std::thread::spawn(move || call_tool("whatsrust_status", &Value::Null, port));

        let raw = serve_one_request(&listener, STUB_OK_RESPONSE).await;
        let result = client.join().expect("client thread panicked");

        let (head, _) = request_head_and_body(&raw);
        assert!(head.starts_with("GET /api/status "), "unexpected request line: {head}");
        assert!(result.get("isError").is_none(), "null arguments must be treated as {{}}: {result}");
        assert_eq!(result["content"][0]["text"], json!("{\"ok\":true}"));
    }

    #[test]
    fn test_whatsrust_send_schema_declares_neither_schedule_at_nor_link_preview() {
        // Sync-only (`/api/send?sync=true`), which rejects both fields with a 400. REST and the
        // CLI keep supporting them.
        let declared = declared_properties("whatsrust_send").expect("whatsrust_send exists");
        for key in ["schedule_at", "link_preview"] {
            assert!(!declared.contains_key(key), "whatsrust_send must not declare `{key}`");
        }
        let tools = tool_definitions();
        let send = tools.iter().find(|t| t["name"] == "whatsrust_send").unwrap();
        let description = send["description"].as_str().unwrap().to_lowercase();
        assert!(!description.contains("link preview"), "description must not advertise link previews: {description}");
    }

    #[test]
    fn test_every_tool_schema_forbids_additional_properties() {
        for tool in tool_definitions() {
            let name = tool["name"].as_str().expect("tool has a name");
            assert_eq!(
                tool.pointer("/inputSchema/additionalProperties"),
                Some(&json!(false)),
                "{name}: inputSchema must set additionalProperties:false (undeclared keys are rejected)"
            );
        }
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
    /// REST struct has no required field (all `Option`) or whose body `call_tool` rebuilds.
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
        for tool in tool_definitions() {
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
        for tool in tool_definitions() {
            let name = tool["name"].as_str().expect("tool has a name");
            let declared = declared_properties(name).expect("properties object");
            let required = tool.pointer("/inputSchema/required").and_then(Value::as_array);
            for key in required.into_iter().flatten().filter_map(Value::as_str) {
                assert!(declared.contains_key(key), "{name}: required `{key}` is not declared in properties");
            }
        }
    }

    #[test]
    fn test_call_tool_unknown_tool_reports_error_without_panicking() {
        // Port 1 is never dialled: the unknown-tool arm fails before any HTTP call. The unknown
        // tool is reported whatever the arguments look like.
        for args in [json!({"path": "/etc/passwd"}), json!("not an object"), Value::Null] {
            let result = call_tool("whatsrust_nope", &args, 1);
            assert_eq!(result["isError"], json!(true), "args: {args}");
            assert_eq!(result["content"][0]["text"], json!("unknown tool: whatsrust_nope"), "args: {args}");
        }
    }

    #[test]
    fn test_every_tool_definition_has_an_object_input_schema_properties() {
        // Guards the validator's single source of truth: a tool whose schema loses its
        // `properties` object would reject every argument.
        for tool in tool_definitions() {
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
        for tool in tool_definitions() {
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
