//! Byte-for-byte golden tests for the REST server's wire output.
//!
//! Each case feeds raw request bytes to [`serve_connection`] over a real loopback socket pair
//! with an explicit [`ServeConfig`] (no environment variables) and compares every byte the
//! server writes against the expected response stored next to the request. The expected
//! strings were captured from the server before the ApiResponse / head-body-split refactors
//! and must not change in a behaviour-neutral change. The labelled step that followed (gate and
//! token before the body, 411 / 417 framing checks, `100 Continue`, reason phrases, the 415
//! code) changed some of them on purpose; the cases it added or changed are under "framing"
//! and "request order" below, plus every 415.
//!
//! The only normalisation is `"uptime_secs":<n>` in the status body, rewritten to `0` with
//! `Content-Length` recomputed (see [`normalize_uptime`]). Nothing else in the corpus is
//! volatile: the bridge is a fresh, disconnected test bridge. Cases that enqueue jobs run on
//! their own fresh bridge, in listed order, so the job ids in their responses are fixed.
//!
//! To refresh a fixture after an intentional behaviour change, run the test: every mismatch
//! prints the actual response as a Rust string literal.

use super::*;

use tokio::net::TcpStream;

/// Loopback `Host` header, as curl would send it.
const HOST: &str = "Host: 127.0.0.1:7270\r\n";
/// `Host` header for the non-loopback configs (the allowlist configs accept `proxy.example`).
const REMOTE_HOST: &str = "Host: proxy.example\r\n";
/// JSON content type for POSTs.
const JSON: &str = "Content-Type: application/json\r\n";
/// The API token the token-bearing configs require.
const TOKEN: &str = "golden-token";
/// `X-API-Token` header carrying [`TOKEN`].
const TOKEN_HEADER: &str = "X-API-Token: golden-token\r\n";

/// Which [`ServeConfig`] a case runs against.
#[derive(Clone, Copy)]
enum Cfg {
    /// Loopback bind, no token, no allowlists.
    Loopback,
    /// Loopback bind with [`TOKEN`] required.
    Token,
    /// Non-loopback bind with [`TOKEN`] required, no allowlists.
    Remote,
    /// Loopback bind with an Origin allowlist and a Host allowlist.
    Allowlists,
    /// Non-loopback bind with [`TOKEN`] required and a Host allowlist.
    RemoteAllowlist,
    /// Loopback bind whose SSE semaphore has no permits left.
    NoSse,
}

/// Build the explicit config for `kind`.
fn config(kind: Cfg) -> ServeConfig {
    let (is_loopback, api_token, origins, hosts, sse_permits) = match kind {
        Cfg::Loopback => (true, None, vec![], vec![], SSE_MAX_CONNECTIONS),
        Cfg::Token => (true, Some(TOKEN), vec![], vec![], SSE_MAX_CONNECTIONS),
        Cfg::Remote => (false, Some(TOKEN), vec![], vec![], SSE_MAX_CONNECTIONS),
        Cfg::Allowlists => (
            true,
            None,
            vec!["https://trusted.example".to_string()],
            vec!["proxy.example".to_string()],
            SSE_MAX_CONNECTIONS,
        ),
        Cfg::RemoteAllowlist => (
            false,
            Some(TOKEN),
            vec![],
            vec!["proxy.example".to_string()],
            SSE_MAX_CONNECTIONS,
        ),
        Cfg::NoSse => (true, None, vec![], vec![], 0),
    };
    ServeConfig {
        is_loopback,
        api_token: api_token.map(str::to_string),
        allowed_origins: origins,
        allowed_hosts: hosts,
        sse_sem: Arc::new(Semaphore::new(sse_permits)),
        sse_cancel: CancellationToken::new(),
    }
}

// --- Expected responses shared by several cases ---

/// 200 `{"ok":true}`.
const OK_SIMPLE: &str = "HTTP/1.1 200 OK\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 11\r\n\
    Connection: close\r\n\
    \r\n\
    {\"ok\":true}";

/// 503 from a handler that needs a connected WhatsApp client.
const NOT_CONNECTED: &str = "HTTP/1.1 503 Service Unavailable\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 57\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"unavailable\",\"error\":\"not connected\",\"ok\":false}";

/// 200 status snapshot of a fresh, disconnected bridge (`uptime_secs` normalised to 0). `mcp_http`
/// is null: only the daemon's own MCP listener reports there, and no test starts that one.
const STATUS_OK: &str = "HTTP/1.1 200 OK\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 362\r\n\
    Connection: close\r\n\
    \r\n\
    {\"connected\":false,\"device_name\":null,\"embeddings\":{\"embedded\":0,\"failed\":0,\"model_id\":\"test-model\",\"pending\":0,\"skipped\":0},\"last_connect_epoch\":0,\"last_disconnect_epoch\":0,\"last_inbound_epoch\":0,\"last_outbound_epoch\":0,\"mcp_http\":null,\"messages_received\":0,\"messages_sent\":0,\"ok\":true,\"queue_depth\":0,\"reconnect_count\":0,\"state\":\"Disconnected\",\"uptime_secs\":0}";

/// 404 for a route the server does not have.
const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 51\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"not_found\",\"error\":\"not found\",\"ok\":false}";

/// 400 for a request that cannot be read (also the read-timeout response).
const BAD_REQUEST: &str = "HTTP/1.1 400 Bad Request\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 55\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"bad_request\",\"error\":\"bad request\",\"ok\":false}";

/// 400 for a media request with empty base64 data.
const DECODED_DATA_EMPTY: &str = "HTTP/1.1 400 Bad Request\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 75\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"bad_request\",\"error\":\"decoded data is empty (0 bytes)\",\"ok\":false}";

/// 400 for a POST whose body parsed as empty.
const EMPTY_BODY_REJECTED: &str = "HTTP/1.1 400 Bad Request\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 102\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"bad_request\",\"error\":\"invalid JSON: EOF while parsing a value at line 1 column 0\",\"ok\":false}";

/// 403 for `Origin: https://evil.example`.
const ORIGIN_NOT_ALLOWED: &str = "HTTP/1.1 403 Forbidden\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 86\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"forbidden\",\"error\":\"Origin 'https://evil.example' is not allowed\",\"ok\":false}";

/// 403 for `Host: evil.example`.
const HOST_NOT_ALLOWED: &str = "HTTP/1.1 403 Forbidden\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 76\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"forbidden\",\"error\":\"Host 'evil.example' is not allowed\",\"ok\":false}";

/// Gate rejection of a POST without a JSON content type.
const UNSUPPORTED_MEDIA_TYPE: &str = "HTTP/1.1 415 Unsupported Media Type\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 99\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"unsupported_media_type\",\"error\":\"POST requires Content-Type: application/json\",\"ok\":false}";

/// 411 for any request carrying `Transfer-Encoding`.
const LENGTH_REQUIRED: &str = "HTTP/1.1 411 Length Required\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 103\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"length_required\",\"error\":\"Transfer-Encoding is not supported; send Content-Length\",\"ok\":false}";

/// 417 for an `Expect` value other than `100-continue`.
const EXPECTATION_FAILED: &str = "HTTP/1.1 417 Expectation Failed\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 69\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"expectation_failed\",\"error\":\"unsupported Expect\",\"ok\":false}";

/// The interim response written before the body of an `Expect: 100-continue` request.
const CONTINUE: &str = "HTTP/1.1 100 Continue\r\n\r\n";

/// 401 for a missing or wrong API token.
const UNAUTHORIZED: &str = "HTTP/1.1 401 Unauthorized\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 57\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"unauthorized\",\"error\":\"unauthorized\",\"ok\":false}";

/// 400 for a group reaction without `sender_jid`.
const SENDER_REQUIRED_FOR_GROUP: &str = "HTTP/1.1 400 Bad Request\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 86\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"bad_request\",\"error\":\"sender_jid is required for group reactions\",\"ok\":false}";

/// 400 for a status request with no recipients.
const RECIPIENTS_REQUIRED: &str = "HTTP/1.1 400 Bad Request\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 72\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"bad_request\",\"error\":\"recipients must not be empty\",\"ok\":false}";

/// 403 for a `path` media upload on a non-loopback bind.
const PATH_UPLOAD_FORBIDDEN: &str = "HTTP/1.1 403 Forbidden\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 106\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"forbidden\",\"error\":\"local-path media uploads are disabled for non-loopback API binds\",\"ok\":false}";

/// 200 with no messages.
const NO_MESSAGES: &str = "HTTP/1.1 200 OK\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 35\r\n\
    Connection: close\r\n\
    \r\n\
    {\"count\":0,\"messages\":[],\"ok\":true}";

/// 400 for a search without `q`.
const QUERY_REQUIRED: &str = "HTTP/1.1 400 Bad Request\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 75\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"bad_request\",\"error\":\"query parameter 'q' is required\",\"ok\":false}";

/// 404 for an unknown history-fetch job id.
const JOB_NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\n\
    Content-Type: application/json\r\n\
    Content-Length: 55\r\n\
    Connection: close\r\n\
    \r\n\
    {\"code\":\"not_found\",\"error\":\"job not found\",\"ok\":false}";

/// One wire-level golden case.
struct Case {
    name: &'static str,
    cfg: Cfg,
    request: Vec<u8>,
    expected: &'static str,
}

fn case(name: &'static str, cfg: Cfg, request: Vec<u8>, expected: &'static str) -> Case {
    Case {
        name,
        cfg,
        request,
        expected,
    }
}

/// `GET {path}` with the given header lines.
fn get(path: &str, headers: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\n{headers}\r\n").into_bytes()
}

/// `POST {path}` with the given header lines and a matching `Content-Length`.
fn post(path: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {path} HTTP/1.1\r\n{headers}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// `POST {path}` whose `Content-Length` says `declared` bytes while `body` is what is actually sent.
fn post_declaring(path: &str, headers: &str, declared: usize, body: &str) -> Vec<u8> {
    format!("POST {path} HTTP/1.1\r\n{headers}Content-Length: {declared}\r\n\r\n{body}").into_bytes()
}

/// Loopback JSON POST.
fn json_post(path: &str, body: &str) -> Vec<u8> {
    post(path, &format!("{HOST}{JSON}"), body)
}

/// Token-carrying JSON POST for the non-loopback configs.
fn remote_post(path: &str, body: &str) -> Vec<u8> {
    post(path, &format!("{REMOTE_HOST}{JSON}{TOKEN_HEADER}"), body)
}

/// Adapter from a handler's return value to the bytes put on the wire.
fn wire(resp: ApiResponse) -> Vec<u8> {
    resp.into_bytes()
}

/// Write `request` to a fresh loopback connection served by [`serve_connection`] and return
/// every byte the server wrote back until it closed. `half_close` shuts down the client's
/// write side after the request, so a server waiting for more bytes sees EOF instead of
/// waiting out its read timeout.
async fn exchange(
    bridge: &WhatsAppBridge,
    cfg: &ServeConfig,
    request: &[u8],
    half_close: bool,
) -> Vec<u8> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr).await.unwrap();
    let (server_stream, _) = listener.accept().await.unwrap();
    let client_side = async {
        client.write_all(request).await.unwrap();
        if half_close {
            let _ = client.shutdown().await;
        }
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        out
    };
    let (_, out) = tokio::join!(serve_connection(server_stream, bridge, cfg), client_side);
    out
}

/// Rewrite the status body's `"uptime_secs":<n>` to `0` and fix `Content-Length` to match, so
/// the status responses are comparable byte-for-byte. Other responses pass through unchanged.
fn normalize_uptime(raw: Vec<u8>) -> Vec<u8> {
    const KEY: &str = "\"uptime_secs\":";
    let Ok(text) = String::from_utf8(raw.clone()) else {
        return raw;
    };
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return raw;
    };
    let Some((before, after)) = body.split_once(KEY) else { return raw };
    let body = format!("{before}{KEY}0{}", after.trim_start_matches(|c: char| c.is_ascii_digit()));
    let head: Vec<String> = head
        .split("\r\n")
        .map(|line| match line.starts_with("Content-Length:") {
            true => format!("Content-Length: {}", body.len()),
            false => line.to_string(),
        })
        .collect();
    format!("{}\r\n\r\n{}", head.join("\r\n"), body).into_bytes()
}

/// Compare every `(name, actual, expected)`; report all mismatches at once, each with the
/// actual response as a Rust string literal ready to paste back as the new fixture.
fn assert_golden(results: Vec<(String, Vec<u8>, &'static str)>) {
    let mut failures = Vec::new();
    for (name, actual, expected) in &results {
        if actual.as_slice() != expected.as_bytes() {
            failures.push(format!(
                "{name}: actual = {:?}",
                String::from_utf8_lossy(actual)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} golden mismatch(es):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

async fn test_bridge(tag: &str) -> (WhatsAppBridge, std::path::PathBuf) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = std::env::temp_dir().join(format!("whatsrust-golden-{tag}-{ts}.db"));
    let bridge = crate::bridge::build_test_bridge_with_embedder(
        &db_path,
        crate::embedder::FakeEmbedder::new("test-model", 8),
    )
    .await;
    (bridge, db_path)
}

/// The wire-level corpus, in run order. Stateful cases (they enqueue jobs) come last.
fn corpus() -> Vec<Case> {
    // 1.5 MiB JSON body: above the 1 MiB default limit, below the 70 MiB media limit.
    let big_media_body = format!(
        r#"{{"jid":"1@s.whatsapp.net","data":"","caption":"{}"}}"#,
        "x".repeat(1_500_000)
    );
    let pin_body = r#"{"jid":"1@s.whatsapp.net"}"#;
    let duplicate_cl = format!(
        "POST /api/pin-chat HTTP/1.1\r\n{HOST}{JSON}Content-Length: 5\r\nContent-Length: {}\r\n\r\n{pin_body}",
        pin_body.len()
    );
    let trailing_bytes = format!(
        "POST /api/pin-chat HTTP/1.1\r\n{HOST}{JSON}Content-Length: {}\r\n\r\n{pin_body}TRAILING-BYTES",
        pin_body.len()
    );
    let invalid_cl =
        format!("POST /api/pin-chat HTTP/1.1\r\n{HOST}{JSON}Content-Length: abc\r\n\r\n{pin_body}");
    let expect_continue = post(
        "/api/pin-chat",
        &format!("{HOST}{JSON}Expect: 100-continue\r\n"),
        pin_body,
    );
    let chunked = format!(
        "POST /api/pin-chat HTTP/1.1\r\n{HOST}{JSON}Transfer-Encoding: chunked\r\n\r\n1a\r\n{pin_body}\r\n0\r\n\r\n"
    );
    vec![
        // --- status & routing ---
        case("health", Cfg::Loopback, get("/health", HOST), STATUS_OK),
        case("root", Cfg::Loopback, get("/", HOST), STATUS_OK),
        case("api_status", Cfg::Loopback, get("/api/status", HOST), STATUS_OK),
        case("not_found_get", Cfg::Loopback, get("/nope", HOST), NOT_FOUND),
        case("not_found_post", Cfg::Loopback, json_post("/api/nope", "{}"), NOT_FOUND),
        case("get_on_post_route", Cfg::Loopback, get("/api/send", HOST), NOT_FOUND),
        case("post_on_get_route", Cfg::Loopback, json_post("/api/status", "{}"), NOT_FOUND),
        case(
            "lowercase_method",
            Cfg::Loopback,
            format!("get /health HTTP/1.1\r\n{HOST}\r\n").into_bytes(),
            NOT_FOUND,
        ),
        case(
            "options_not_answered",
            Cfg::Loopback,
            format!("OPTIONS /api/send HTTP/1.1\r\n{HOST}\r\n").into_bytes(),
            NOT_FOUND,
        ),
        case(
            "head_method",
            Cfg::Loopback,
            format!("HEAD /health HTTP/1.1\r\n{HOST}\r\n").into_bytes(),
            NOT_FOUND,
        ),
        case("events_trailing_slash", Cfg::Loopback, get("/api/events/", HOST), NOT_FOUND),
        case(
            "qr_unavailable",
            Cfg::Loopback,
            get("/api/qr", HOST),
            "HTTP/1.1 404 Not Found\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 100\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"not_found\",\"error\":\"no QR code available (already paired or not yet generated)\",\"ok\":false}",
        ),
        // --- malformed requests ---
        case("garbage_request_line", Cfg::Loopback, b"garbage\r\n\r\n".to_vec(), BAD_REQUEST),
        case("empty_request_line", Cfg::Loopback, b"\r\n\r\n".to_vec(), BAD_REQUEST),
        case(
            "eof_before_head_end",
            Cfg::Loopback,
            b"GET /health HTTP/1.1\r\nHost: 127.0.0.1".to_vec(),
            BAD_REQUEST,
        ),
        case("empty_connection", Cfg::Loopback, Vec::new(), BAD_REQUEST),
        case(
            "non_utf8_head",
            Cfg::Loopback,
            b"GET /health HTTP/1.1\r\nHost: \xff\r\n\r\n".to_vec(),
            BAD_REQUEST,
        ),
        case(
            "head_over_128k",
            Cfg::Loopback,
            // 128 KiB + 1 bytes and no blank line: the server consumes all of it, then gives up.
            vec![b'x'; 128 * 1024 + 1],
            BAD_REQUEST,
        ),
        case(
            "oversize_content_length",
            Cfg::Loopback,
            format!("POST /api/send HTTP/1.1\r\n{HOST}{JSON}Content-Length: 1048577\r\n\r\n").into_bytes(),
            BAD_REQUEST,
        ),
        case(
            "oversize_content_length_media",
            Cfg::Loopback,
            format!("POST /api/image HTTP/1.1\r\n{HOST}{JSON}Content-Length: 73400321\r\n\r\n").into_bytes(),
            BAD_REQUEST,
        ),
        case(
            "truncated_body",
            Cfg::Loopback,
            format!("POST /api/pin-chat HTTP/1.1\r\n{HOST}{JSON}Content-Length: 100\r\n\r\n0123456789").into_bytes(),
            BAD_REQUEST,
        ),
        case("media_body_above_default_limit", Cfg::Loopback, json_post("/api/image", &big_media_body), DECODED_DATA_EMPTY),
        case("invalid_content_length", Cfg::Loopback, invalid_cl.into_bytes(), EMPTY_BODY_REJECTED),
        case("duplicate_content_length_last_wins", Cfg::Loopback, duplicate_cl.into_bytes(), NOT_CONNECTED),
        case("bytes_beyond_content_length_ignored", Cfg::Loopback, trailing_bytes.into_bytes(), NOT_CONNECTED),
        case(
            "header_without_colon_ignored",
            Cfg::Loopback,
            format!("GET /health HTTP/1.1\r\n{HOST}NoColonHere\r\n\r\n").into_bytes(),
            STATUS_OK,
        ),
        // --- framing: Transfer-Encoding and Expect, decided from the head before any body byte ---
        case("expect_continue_body_already_sent", Cfg::Loopback, expect_continue, NOT_CONNECTED),
        case(
            "expect_continue_case_insensitive",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}{JSON}Expect: 100-Continue\r\n"), pin_body),
            NOT_CONNECTED,
        ),
        case(
            "expect_continue_no_body_no_interim",
            Cfg::Loopback,
            get("/health", &format!("{HOST}Expect: 100-continue\r\n")),
            STATUS_OK,
        ),
        case(
            "expect_other_value_rejected",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}{JSON}Expect: 200-ok\r\n"), pin_body),
            EXPECTATION_FAILED,
        ),
        case(
            "expect_other_value_rejected_on_get",
            Cfg::Loopback,
            get("/health", &format!("{HOST}Expect: nope\r\n")),
            EXPECTATION_FAILED,
        ),
        case(
            "expect_other_value_body_never_read",
            Cfg::Loopback,
            // 100 bytes declared, none sent: reading the body would end in the truncated-body 400.
            post_declaring("/api/pin-chat", &format!("{HOST}{JSON}Expect: nope\r\n"), 100, ""),
            EXPECTATION_FAILED,
        ),
        case(
            "expect_second_header_other_value_rejected",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}{JSON}Expect: 100-continue\r\nExpect: nope\r\n"), pin_body),
            EXPECTATION_FAILED,
        ),
        case("chunked_body_rejected", Cfg::Loopback, chunked.into_bytes(), LENGTH_REQUIRED),
        case(
            "transfer_encoding_identity_rejected",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}{JSON}Transfer-Encoding: identity\r\n"), pin_body),
            LENGTH_REQUIRED,
        ),
        case(
            "transfer_encoding_with_content_length_rejected",
            Cfg::Loopback,
            // `post` adds a Content-Length too: that pair is the ambiguous one, refused like any other.
            post("/api/pin-chat", &format!("{HOST}{JSON}Transfer-Encoding: chunked\r\n"), pin_body),
            LENGTH_REQUIRED,
        ),
        case(
            "transfer_encoding_padded_name_rejected",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}{JSON}Transfer-Encoding : chunked\r\n"), pin_body),
            LENGTH_REQUIRED,
        ),
        case(
            "transfer_encoding_on_get_rejected",
            Cfg::Loopback,
            get("/health", &format!("{HOST}Transfer-Encoding: chunked\r\n")),
            LENGTH_REQUIRED,
        ),
        case(
            "transfer_encoding_on_sse_rejected",
            Cfg::Loopback,
            get("/api/events", &format!("{HOST}Transfer-Encoding: chunked\r\n")),
            LENGTH_REQUIRED,
        ),
        case(
            "transfer_encoding_before_size_check",
            Cfg::Loopback,
            post_declaring("/api/send", &format!("{HOST}{JSON}Transfer-Encoding: chunked\r\n"), 1_048_577, ""),
            LENGTH_REQUIRED,
        ),
        case(
            "size_check_before_expect",
            Cfg::Loopback,
            post_declaring("/api/send", &format!("{HOST}{JSON}Expect: nope\r\n"), 1_048_577, ""),
            BAD_REQUEST,
        ),
        case(
            "oversize_with_expect_continue_gets_no_interim",
            Cfg::Loopback,
            post_declaring("/api/send", &format!("{HOST}{JSON}Expect: 100-continue\r\n"), 1_048_577, ""),
            BAD_REQUEST,
        ),
        // --- request order: gate, then token, then framing, all before the body ---
        case(
            "oversize_content_length_missing_token",
            Cfg::Token,
            post_declaring("/api/send", &format!("{HOST}{JSON}"), 1_048_577, ""),
            UNAUTHORIZED,
        ),
        case(
            "oversize_content_length_with_token",
            Cfg::Token,
            post_declaring("/api/send", &format!("{HOST}{JSON}{TOKEN_HEADER}"), 1_048_577, ""),
            BAD_REQUEST,
        ),
        case(
            "oversize_media_content_length_missing_token",
            Cfg::Token,
            post_declaring("/api/image", &format!("{HOST}{JSON}"), 73_400_321, ""),
            UNAUTHORIZED,
        ),
        case(
            "truncated_body_bad_origin",
            Cfg::Loopback,
            post_declaring("/api/pin-chat", &format!("{HOST}{JSON}Origin: https://evil.example\r\n"), 100, "0123456789"),
            ORIGIN_NOT_ALLOWED,
        ),
        case(
            "truncated_body_bad_content_type",
            Cfg::Loopback,
            post_declaring("/api/pin-chat", &format!("{HOST}Content-Type: text/plain\r\n"), 100, "0123456789"),
            UNSUPPORTED_MEDIA_TYPE,
        ),
        case(
            "truncated_body_missing_token",
            Cfg::Token,
            post_declaring("/api/pin-chat", &format!("{HOST}{JSON}"), 100, "0123456789"),
            UNAUTHORIZED,
        ),
        case(
            "truncated_body_with_token",
            Cfg::Token,
            post_declaring("/api/pin-chat", &format!("{HOST}{JSON}{TOKEN_HEADER}"), 100, "0123456789"),
            BAD_REQUEST,
        ),
        case(
            "chunked_bad_origin",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}{JSON}Origin: https://evil.example\r\nTransfer-Encoding: chunked\r\n"), pin_body),
            ORIGIN_NOT_ALLOWED,
        ),
        case(
            "chunked_missing_token",
            Cfg::Token,
            post("/api/pin-chat", &format!("{HOST}{JSON}Transfer-Encoding: chunked\r\n"), pin_body),
            UNAUTHORIZED,
        ),
        case(
            "chunked_with_token",
            Cfg::Token,
            post("/api/pin-chat", &format!("{HOST}{JSON}{TOKEN_HEADER}Transfer-Encoding: chunked\r\n"), pin_body),
            LENGTH_REQUIRED,
        ),
        case(
            "expect_other_value_missing_token",
            Cfg::Token,
            post("/api/pin-chat", &format!("{HOST}{JSON}Expect: nope\r\n"), pin_body),
            UNAUTHORIZED,
        ),
        case(
            "expect_continue_missing_token_gets_no_interim",
            Cfg::Token,
            // The body is not sent and not read: a 100 or a truncated-body 400 would show here.
            post_declaring("/api/pin-chat", &format!("{HOST}{JSON}Expect: 100-continue\r\n"), 50, ""),
            UNAUTHORIZED,
        ),
        case(
            "expect_continue_bad_origin_gets_no_interim",
            Cfg::Loopback,
            post_declaring("/api/pin-chat", &format!("{HOST}{JSON}Origin: https://evil.example\r\nExpect: 100-continue\r\n"), 50, ""),
            ORIGIN_NOT_ALLOWED,
        ),
        // --- request gate (ADR 0044) ---
        case(
            "gate_origin_denied",
            Cfg::Loopback,
            get("/health", &format!("{HOST}Origin: https://evil.example\r\n")),
            ORIGIN_NOT_ALLOWED,
        ),
        case(
            "gate_origin_null",
            Cfg::Loopback,
            get("/health", &format!("{HOST}Origin: null\r\n")),
            "HTTP/1.1 403 Forbidden\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 70\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"forbidden\",\"error\":\"Origin 'null' is not allowed\",\"ok\":false}",
        ),
        case(
            "gate_origin_duplicate",
            Cfg::Allowlists,
            get("/health", &format!("{HOST}Origin: https://trusted.example\r\nOrigin: https://evil.example\r\n")),
            "HTTP/1.1 403 Forbidden\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 65\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"forbidden\",\"error\":\"duplicate Origin header\",\"ok\":false}",
        ),
        case(
            "gate_origin_allowed",
            Cfg::Allowlists,
            get("/health", &format!("{HOST}Origin: https://trusted.example\r\n")),
            STATUS_OK,
        ),
        case(
            "gate_origin_allowed_bad_host",
            Cfg::Allowlists,
            get("/health", "Host: evil.example\r\nOrigin: https://trusted.example\r\n"),
            HOST_NOT_ALLOWED,
        ),
        case(
            "gate_host_missing",
            Cfg::Loopback,
            get("/health", ""),
            "HTTP/1.1 403 Forbidden\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 61\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"forbidden\",\"error\":\"missing Host header\",\"ok\":false}",
        ),
        case("gate_host_evil", Cfg::Loopback, get("/health", "Host: evil.example\r\n"), HOST_NOT_ALLOWED),
        case(
            "gate_host_duplicate",
            Cfg::Loopback,
            get("/health", &format!("{HOST}Host: evil.example\r\n")),
            "HTTP/1.1 403 Forbidden\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 63\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"forbidden\",\"error\":\"duplicate Host header\",\"ok\":false}",
        ),
        case("gate_host_localhost_dot", Cfg::Loopback, get("/health", "Host: LOCALHOST.\r\n"), STATUS_OK),
        case("gate_host_ipv6", Cfg::Loopback, get("/health", "Host: [::1]:7270\r\n"), STATUS_OK),
        case("gate_host_allowlisted_on_loopback", Cfg::Allowlists, get("/health", REMOTE_HOST), STATUS_OK),
        case(
            "gate_remote_any_host_without_allowlist",
            Cfg::Remote,
            get("/health", &format!("Host: whatever.example\r\n{TOKEN_HEADER}")),
            STATUS_OK,
        ),
        case(
            "gate_remote_allowlist_denied",
            Cfg::RemoteAllowlist,
            get("/health", &format!("Host: whatever.example\r\n{TOKEN_HEADER}")),
            "HTTP/1.1 403 Forbidden\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 80\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"forbidden\",\"error\":\"Host 'whatever.example' is not allowed\",\"ok\":false}",
        ),
        case(
            "gate_remote_allowlist_allowed",
            Cfg::RemoteAllowlist,
            get("/health", &format!("{REMOTE_HOST}{TOKEN_HEADER}")),
            STATUS_OK,
        ),
        case(
            "gate_post_without_content_type",
            Cfg::Loopback,
            post("/api/pin-chat", HOST, pin_body),
            UNSUPPORTED_MEDIA_TYPE,
        ),
        case(
            "gate_post_text_plain",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}Content-Type: text/plain\r\n"), pin_body),
            UNSUPPORTED_MEDIA_TYPE,
        ),
        case(
            "gate_post_form_urlencoded",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}Content-Type: application/x-www-form-urlencoded\r\n"), pin_body),
            UNSUPPORTED_MEDIA_TYPE,
        ),
        case(
            "gate_post_json_with_charset",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}Content-Type: application/json; charset=utf-8\r\n"), pin_body),
            NOT_CONNECTED,
        ),
        case(
            "gate_post_json_uppercase",
            Cfg::Loopback,
            post("/api/pin-chat", &format!("{HOST}Content-Type: APPLICATION/JSON\r\n"), pin_body),
            NOT_CONNECTED,
        ),
        case(
            "gate_before_route",
            Cfg::Loopback,
            post("/api/nope", &format!("{HOST}Content-Type: text/plain\r\n"), "{}"),
            UNSUPPORTED_MEDIA_TYPE,
        ),
        case(
            "gate_lowercase_post_text_plain",
            Cfg::Loopback,
            format!(
                "post /api/pin-chat HTTP/1.1\r\n{HOST}Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n{pin_body}",
                pin_body.len()
            )
            .into_bytes(),
            UNSUPPORTED_MEDIA_TYPE,
        ),
        // --- API token ---
        case("token_missing", Cfg::Token, get("/health", HOST), UNAUTHORIZED),
        case("token_wrong_x_api_token", Cfg::Token, get("/health", &format!("{HOST}X-API-Token: nope\r\n")), UNAUTHORIZED),
        case(
            "token_wrong_bearer",
            Cfg::Token,
            get("/health", &format!("{HOST}Authorization: Bearer nope\r\n")),
            UNAUTHORIZED,
        ),
        case("token_empty_value", Cfg::Token, get("/health", &format!("{HOST}X-API-Token:\r\n")), UNAUTHORIZED),
        case(
            "token_basic_scheme",
            Cfg::Token,
            get("/health", &format!("{HOST}Authorization: Basic {TOKEN}\r\n")),
            UNAUTHORIZED,
        ),
        case(
            "token_lowercase_bearer_scheme",
            Cfg::Token,
            get("/health", &format!("{HOST}Authorization: bearer {TOKEN}\r\n")),
            UNAUTHORIZED,
        ),
        case("token_x_api_token_ok", Cfg::Token, get("/health", &format!("{HOST}{TOKEN_HEADER}")), STATUS_OK),
        case(
            "token_bearer_ok",
            Cfg::Token,
            get("/health", &format!("{HOST}Authorization: Bearer {TOKEN}\r\n")),
            STATUS_OK,
        ),
        case(
            "token_wrong_header_right_bearer",
            Cfg::Token,
            get("/health", &format!("{HOST}X-API-Token: nope\r\nAuthorization: Bearer {TOKEN}\r\n")),
            STATUS_OK,
        ),
        case("token_checked_before_route", Cfg::Token, get("/nope", HOST), UNAUTHORIZED),
        case("token_ok_unknown_route", Cfg::Token, get("/nope", &format!("{HOST}{TOKEN_HEADER}")), NOT_FOUND),
        case(
            "token_after_gate_origin",
            Cfg::Token,
            get("/health", &format!("{HOST}Origin: https://evil.example\r\n")),
            ORIGIN_NOT_ALLOWED,
        ),
        case(
            "token_after_gate_content_type",
            Cfg::Token,
            post("/api/pin-chat", &format!("{HOST}Content-Type: text/plain\r\n"), pin_body),
            UNSUPPORTED_MEDIA_TYPE,
        ),
        case(
            "token_ok_reaches_handler",
            Cfg::Token,
            post("/api/pin-chat", &format!("{HOST}{JSON}{TOKEN_HEADER}"), pin_body),
            NOT_CONNECTED,
        ),
        case("token_sse_missing", Cfg::Token, get("/api/events", HOST), UNAUTHORIZED),
        case(
            "sse_capacity_exhausted",
            Cfg::NoSse,
            get("/api/events", HOST),
            "HTTP/1.1 503 Service Unavailable\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 68\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"unavailable\",\"error\":\"too many SSE connections\",\"ok\":false}",
        ),
        // --- JSON bodies and validation ---
        case(
            "bad_json",
            Cfg::Loopback,
            json_post("/api/pin-chat", "{not json"),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 97\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"invalid JSON: key must be a string at line 1 column 2\",\"ok\":false}",
        ),
        case("empty_body_post", Cfg::Loopback, json_post("/api/pin-chat", ""), EMPTY_BODY_REJECTED),
        case(
            "missing_required_fields",
            Cfg::Loopback,
            json_post("/api/send", "{}"),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 96\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"invalid JSON: missing field `jid` at line 1 column 2\",\"ok\":false}",
        ),
        case(
            "wrong_field_type",
            Cfg::Loopback,
            json_post("/api/send", r#"{"jid":1,"text":"x"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 121\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"invalid JSON: invalid type: integer `1`, expected a string at line 1 column 8\",\"ok\":false}",
        ),
        case(
            "send_sync_rejects_link_preview",
            Cfg::Loopback,
            json_post(
                "/api/send?sync=true",
                r#"{"jid":"1@s.whatsapp.net","text":"hi","link_preview":{"url":"https://example.com"}}"#,
            ),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 106\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"link_preview is not supported with ?sync=true (use async mode)\",\"ok\":false}",
        ),
        case(
            "send_sync_rejects_schedule_at",
            Cfg::Loopback,
            json_post("/api/send?sync=true", r#"{"jid":"1@s.whatsapp.net","text":"hi","schedule_at":4102444800}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 105\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"schedule_at is not supported with ?sync=true (use async mode)\",\"ok\":false}",
        ),
        case(
            "reply_without_sender",
            Cfg::Loopback,
            json_post("/api/reply", r#"{"jid":"1@s.whatsapp.net","id":"A","text":"t"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 76\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"sender or sender_jid is required\",\"ok\":false}",
        ),
        case(
            "reply_group_short_sender",
            Cfg::Loopback,
            json_post("/api/reply", r#"{"jid":"1@g.us","id":"A","sender":"972500000000","text":"t"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 104\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"sender_jid (full WhatsApp JID) is required for group replies\",\"ok\":false}",
        ),
        case(
            "react_empty_emoji",
            Cfg::Loopback,
            json_post("/api/react", r#"{"jid":"1@s.whatsapp.net","id":"A","emoji":""}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 67\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"emoji must not be empty\",\"ok\":false}",
        ),
        case(
            "react_group_without_sender",
            Cfg::Loopback,
            json_post("/api/react", r#"{"jid":"1@g.us","id":"A","emoji":"+"}"#),
            SENDER_REQUIRED_FOR_GROUP,
        ),
        case(
            "react_group_without_from_me",
            Cfg::Loopback,
            json_post("/api/react", r#"{"jid":"1@g.us","id":"A","emoji":"+","sender_jid":"2@s.whatsapp.net"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 83\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"from_me is required for group reactions\",\"ok\":false}",
        ),
        case("unreact_group_without_sender", Cfg::Loopback, json_post("/api/unreact", r#"{"jid":"1@g.us","id":"A"}"#), SENDER_REQUIRED_FOR_GROUP),
        case(
            "contact_without_digits",
            Cfg::Loopback,
            json_post("/api/contact", r#"{"jid":"1@s.whatsapp.net","name":"N","phone":"abc"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 81\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"phone must contain at least one digit\",\"ok\":false}",
        ),
        case(
            "poll_too_few_options",
            Cfg::Loopback,
            json_post("/api/poll", r#"{"jid":"1@s.whatsapp.net","question":"q","options":["a"],"selectable_count":1}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 90\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"poll must include at least 2 non-empty options\",\"ok\":false}",
        ),
        case("status_text_no_recipients", Cfg::Loopback, json_post("/api/status-text", r#"{"recipients":[],"text":"t"}"#), RECIPIENTS_REQUIRED),
        case(
            "status_revoke_no_recipients",
            Cfg::Loopback,
            json_post("/api/status-revoke", r#"{"recipients":[],"message_id":"M"}"#),
            RECIPIENTS_REQUIRED,
        ),
        case(
            "group_info_without_jid",
            Cfg::Loopback,
            get("/api/group-info", HOST),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 67\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"missing ?jid= parameter\",\"ok\":false}",
        ),
        case(
            "group_invite_link_without_jid",
            Cfg::Loopback,
            get("/api/group-invite-link", HOST),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 71\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"missing jid query parameter\",\"ok\":false}",
        ),
        // --- bridge not connected (503) through each handler family. Handlers that enqueue and wait
        // for the outbound worker (/api/poll, /api/status-text, ...) are not here: they wait 30 s. ---
        case("groups_not_connected", Cfg::Loopback, get("/api/groups", HOST), NOT_CONNECTED),
        case("group_info_not_connected", Cfg::Loopback, get("/api/group-info?jid=1%40g.us", HOST), NOT_CONNECTED),
        case("jid_action_not_connected", Cfg::Loopback, json_post("/api/pin-chat", pin_body), NOT_CONNECTED),
        case("message_action_not_connected", Cfg::Loopback, json_post("/api/star", r#"{"jid":"1@s.whatsapp.net","id":"A"}"#), NOT_CONNECTED),
        case(
            "group_participants_not_connected",
            Cfg::Loopback,
            json_post("/api/group-add", r#"{"jid":"1@g.us","participants":["2@s.whatsapp.net"]}"#),
            NOT_CONNECTED,
        ),
        // --- media ---
        case(
            "media_path_on_remote_bind",
            Cfg::Remote,
            remote_post("/api/image", r#"{"jid":"1@s.whatsapp.net","path":"/tmp/golden.png"}"#),
            PATH_UPLOAD_FORBIDDEN,
        ),
        case(
            "media_doc_path_on_remote_bind",
            Cfg::Remote,
            remote_post("/api/doc", r#"{"jid":"1@s.whatsapp.net","path":"/tmp/golden.pdf"}"#),
            PATH_UPLOAD_FORBIDDEN,
        ),
        case(
            "media_path_missing_file",
            Cfg::Loopback,
            json_post("/api/image", r#"{"jid":"1@s.whatsapp.net","path":"/nonexistent-golden-dir/x.png"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 130\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"cannot stat file /nonexistent-golden-dir/x.png: No such file or directory (os error 2)\",\"ok\":false}",
        ),
        case(
            "media_path_is_directory",
            Cfg::Loopback,
            json_post("/api/video", r#"{"jid":"1@s.whatsapp.net","path":"/"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 73\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"path is not a regular file: /\",\"ok\":false}",
        ),
        case(
            "media_path_and_data",
            Cfg::Loopback,
            json_post("/api/audio", r#"{"jid":"1@s.whatsapp.net","path":"/tmp/a.ogg","data":"aGk="}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 85\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"provide either 'path' or 'data', not both\",\"ok\":false}",
        ),
        case(
            "media_neither_path_nor_data",
            Cfg::Loopback,
            json_post("/api/sticker", r#"{"jid":"1@s.whatsapp.net"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 99\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"either 'path' (loopback) or 'data' (base64) is required\",\"ok\":false}",
        ),
        case("media_empty_data", Cfg::Loopback, json_post("/api/image", r#"{"jid":"1@s.whatsapp.net","data":""}"#), DECODED_DATA_EMPTY),
        // --- local-DB reads on an empty database ---
        case("history_empty", Cfg::Loopback, get("/api/history", HOST), NO_MESSAGES),
        case("history_filtered_empty", Cfg::Loopback, get("/api/history?jid=1%40g.us&limit=5&before=99", HOST), NO_MESSAGES),
        case("search_without_query", Cfg::Loopback, get("/api/search", HOST), QUERY_REQUIRED),
        case("search_empty_query", Cfg::Loopback, get("/api/search?q=", HOST), QUERY_REQUIRED),
        case("search_empty_db", Cfg::Loopback, get("/api/search?q=hello&limit=3", HOST), NO_MESSAGES),
        case(
            "fetch_list_empty",
            Cfg::Loopback,
            get("/api/history-fetch", HOST),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 31\r\n\
                Connection: close\r\n\
                \r\n\
                {\"count\":0,\"jobs\":[],\"ok\":true}",
        ),
        case(
            "fetch_status_bad_id",
            Cfg::Loopback,
            get("/api/history-fetch?job_id=abc", HOST),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 69\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"job_id must be an integer\",\"ok\":false}",
        ),
        case("fetch_status_unknown_job", Cfg::Loopback, get("/api/history-fetch?job_id=999", HOST), JOB_NOT_FOUND),
        case(
            "fetch_trigger_without_chat",
            Cfg::Loopback,
            json_post("/api/history-fetch", r#"{"mode":"all"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 64\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"chat_jid is required\",\"ok\":false}",
        ),
        case(
            "fetch_trigger_without_mode",
            Cfg::Loopback,
            json_post("/api/history-fetch", r#"{"chat_jid":"1@g.us"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 89\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"mode is required ('all', 'since', or 'count')\",\"ok\":false}",
        ),
        case(
            "fetch_trigger_bad_mode",
            Cfg::Loopback,
            json_post("/api/history-fetch", r#"{"chat_jid":"1@g.us","mode":"max"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 98\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"invalid mode 'max': must be 'all', 'since', or 'count'\",\"ok\":false}",
        ),
        case(
            "fetch_trigger_since_without_target",
            Cfg::Loopback,
            json_post("/api/history-fetch", r#"{"chat_jid":"1@g.us","mode":"since"}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 95\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"mode 'since' requires a target_value (ms timestamp)\",\"ok\":false}",
        ),
        case(
            "fetch_trigger_count_not_positive",
            Cfg::Loopback,
            json_post("/api/history-fetch", r#"{"chat_jid":"1@g.us","target_kind":"count","target_value":0}"#),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 89\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"mode 'count' requires a positive target_value\",\"ok\":false}",
        ),
        case(
            "fetch_cancel_without_job_id",
            Cfg::Loopback,
            json_post("/api/history-fetch/cancel", "{}"),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 62\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"job_id is required\",\"ok\":false}",
        ),
        case("fetch_cancel_unknown_job", Cfg::Loopback, json_post("/api/history-fetch/cancel", r#"{"job_id":999}"#), JOB_NOT_FOUND),
        case(
            "purge_without_model",
            Cfg::Loopback,
            json_post("/api/embeddings/purge", "{}"),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 64\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"model_id is required\",\"ok\":false}",
        ),
        case(
            "purge_unknown_model",
            Cfg::Loopback,
            json_post("/api/embeddings/purge", r#"{"model_id":"no-such-model"}"#),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 75\r\n\
                Connection: close\r\n\
                \r\n\
                {\"bytes_reclaimed\":0,\"model_id\":\"no-such-model\",\"ok\":true,\"rows_deleted\":0}",
        ),
    ]
}

/// Cases that enqueue jobs. They run on their own fresh bridge, in this order, so the job ids
/// in the responses are fixed.
fn stateful_corpus() -> Vec<Case> {
    vec![
        case(
            "react_enqueued",
            Cfg::Loopback,
            json_post("/api/react", r#"{"jid":"1@s.whatsapp.net","id":"A","emoji":"+"}"#),
            OK_SIMPLE,
        ),
        case(
            "contact_enqueued",
            Cfg::Loopback,
            json_post("/api/contact", r#"{"jid":"1@s.whatsapp.net","name":"N","phone":"+123"}"#),
            OK_SIMPLE,
        ),
        case(
            "media_data_enqueued_on_remote_bind",
            Cfg::Remote,
            remote_post("/api/image", r#"{"jid":"1@s.whatsapp.net","data":"aGk=","mime":"image/png"}"#),
            OK_SIMPLE,
        ),
        case(
            "media_data_enqueued",
            Cfg::Loopback,
            json_post("/api/image", r#"{"jid":"1@s.whatsapp.net","data":"aGk=","mime":"image/png"}"#),
            OK_SIMPLE,
        ),
        case(
            "send_async_enqueued",
            Cfg::Loopback,
            json_post("/api/send", r#"{"jid":"1@s.whatsapp.net","text":"hi"}"#),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 22\r\n\
                Connection: close\r\n\
                \r\n\
                {\"job_id\":5,\"ok\":true}",
        ),
        case(
            "send_async_scheduled",
            Cfg::Loopback,
            json_post("/api/send", r#"{"jid":"1@s.whatsapp.net","text":"later","schedule_at":4102444800}"#),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 48\r\n\
                Connection: close\r\n\
                \r\n\
                {\"job_id\":6,\"ok\":true,\"scheduled_at\":4102444800}",
        ),
        case(
            "fetch_trigger_accepted",
            Cfg::Loopback,
            json_post("/api/history-fetch", r#"{"chat_jid":"1@g.us","mode":"all"}"#),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 173\r\n\
                Connection: close\r\n\
                \r\n\
                {\"accepted\":null,\"chat_jid\":\"1@g.us\",\"job_id\":1,\"more_remain\":true,\"ok\":true,\"requested\":null,\"resume_anchor\":null,\"status\":\"queued\",\"target_kind\":\"all\",\"target_value\":null}",
        ),
        case(
            "fetch_trigger_already_active",
            Cfg::Loopback,
            json_post("/api/history-fetch", r#"{"chat_jid":"1@g.us","mode":"all"}"#),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 68\r\n\
                Connection: close\r\n\
                \r\n\
                {\"chat_jid\":\"1@g.us\",\"job_id\":1,\"ok\":true,\"status\":\"already_active\"}",
        ),
        case(
            "fetch_cancel_ok",
            Cfg::Loopback,
            json_post("/api/history-fetch/cancel", r#"{"job_id":1}"#),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 43\r\n\
                Connection: close\r\n\
                \r\n\
                {\"job_id\":1,\"ok\":true,\"status\":\"cancelled\"}",
        ),
    ]
}

/// Run `cases` in order against `bridge`; returns `(name, response, expected)` per case.
async fn run_cases(
    bridge: &WhatsAppBridge,
    cases: Vec<Case>,
) -> Vec<(String, Vec<u8>, &'static str)> {
    let mut results = Vec::new();
    for c in cases {
        let actual = normalize_uptime(exchange(bridge, &config(c.cfg), &c.request, true).await);
        results.push((c.name.to_string(), actual, c.expected));
    }
    results
}

#[tokio::test]
async fn test_rest_wire_golden_corpus() {
    let (bridge, db_path) = test_bridge("corpus").await;
    assert_golden(run_cases(&bridge, corpus()).await);
    let _ = std::fs::remove_file(&db_path);
}

#[tokio::test]
async fn test_rest_wire_golden_stateful_corpus() {
    let (bridge, db_path) = test_bridge("stateful").await;
    assert_golden(run_cases(&bridge, stateful_corpus()).await);
    let _ = std::fs::remove_file(&db_path);
}

/// Response-helper outputs that no route reaches on a disconnected bridge (success shapes,
/// the rarer error classes, every `bridge_err` class, every enqueue outcome).
#[test]
fn test_rest_response_helpers_golden() {
    use crate::storage::EnqueueOutcome;
    let chat = "chat@s.whatsapp.net";
    let results: Vec<(String, Vec<u8>, &'static str)> = vec![
        (
            "json_ok_object".into(),
            wire(json_ok(json!({"a": 1, "b": "x"}))),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 25\r\n\
                Connection: close\r\n\
                \r\n\
                {\"a\":1,\"b\":\"x\",\"ok\":true}",
        ),
        (
            "json_ok_non_object".into(),
            wire(json_ok(json!([1, 2]))),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 24\r\n\
                Connection: close\r\n\
                \r\n\
                {\"data\":[1,2],\"ok\":true}",
        ),
        (
            "json_ok_id".into(),
            wire(json_ok_id("3EB0ABCD")),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 27\r\n\
                Connection: close\r\n\
                \r\n\
                {\"id\":\"3EB0ABCD\",\"ok\":true}",
        ),
        ("json_ok_simple".into(), wire(json_ok_simple()), OK_SIMPLE),
        (
            "json_err_400".into(),
            wire(json_err(400, "m")),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 45\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_401".into(),
            wire(json_err(401, "m")),
            "HTTP/1.1 401 Unauthorized\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 46\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"unauthorized\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_403".into(),
            wire(json_err(403, "m")),
            "HTTP/1.1 403 Forbidden\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 43\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"forbidden\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_404".into(),
            wire(json_err(404, "m")),
            "HTTP/1.1 404 Not Found\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 43\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"not_found\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_405".into(),
            wire(json_err(405, "m")),
            "HTTP/1.1 405 Method Not Allowed\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 52\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"method_not_allowed\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_411".into(),
            wire(json_err(411, "m")),
            "HTTP/1.1 411 Length Required\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 49\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"length_required\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_415".into(),
            wire(json_err(415, "m")),
            "HTTP/1.1 415 Unsupported Media Type\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 56\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"unsupported_media_type\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_417".into(),
            wire(json_err(417, "m")),
            "HTTP/1.1 417 Expectation Failed\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 52\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"expectation_failed\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_429".into(),
            wire(json_err(429, "m")),
            "HTTP/1.1 429 Too Many Requests\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 46\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"rate_limited\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_500".into(),
            wire(json_err(500, "m")),
            "HTTP/1.1 500 Internal Server Error\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 48\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"internal_error\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_503".into(),
            wire(json_err(503, "m")),
            "HTTP/1.1 503 Service Unavailable\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 45\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"unavailable\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_504".into(),
            wire(json_err(504, "m")),
            "HTTP/1.1 504 Gateway Timeout\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 41\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"timeout\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_418".into(),
            wire(json_err(418, "m")),
            "HTTP/1.1 418 Unknown\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 48\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"internal_error\",\"error\":\"m\",\"ok\":false}",
        ),
        (
            "json_err_quotes_escaped".into(),
            wire(json_err(400, "a \"quoted\" \\ value\n")),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 67\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"a \\\"quoted\\\" \\\\ value\\n\",\"ok\":false}",
        ),
        ("bridge_err_not_connected".into(), wire(bridge_err(anyhow::anyhow!("not connected"))), NOT_CONNECTED),
        (
            "bridge_err_no_client".into(),
            wire(bridge_err(anyhow::anyhow!("no client"))),
            "HTTP/1.1 503 Service Unavailable\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 53\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"unavailable\",\"error\":\"no client\",\"ok\":false}",
        ),
        (
            "bridge_err_bad_jid".into(),
            wire(bridge_err(anyhow::anyhow!("bad JID: x"))),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 54\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"bad JID: x\",\"ok\":false}",
        ),
        (
            "bridge_err_empty_jid".into(),
            wire(bridge_err(anyhow::anyhow!("empty JID after normalization: x"))),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 76\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"empty JID after normalization: x\",\"ok\":false}",
        ),
        (
            "bridge_err_required_for_group".into(),
            wire(bridge_err(anyhow::anyhow!("sender_jid required for group reactions"))),
            "HTTP/1.1 400 Bad Request\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 83\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"bad_request\",\"error\":\"sender_jid required for group reactions\",\"ok\":false}",
        ),
        (
            "bridge_err_timed_out".into(),
            wire(bridge_err(anyhow::anyhow!("upstream request timed out"))),
            "HTTP/1.1 504 Gateway Timeout\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 66\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"timeout\",\"error\":\"upstream request timed out\",\"ok\":false}",
        ),
        (
            "bridge_err_other".into(),
            wire(bridge_err(anyhow::anyhow!("boom"))),
            "HTTP/1.1 500 Internal Server Error\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 51\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"internal_error\",\"error\":\"boom\",\"ok\":false}",
        ),
        (
            "bridge_err_send_timed_out".into(),
            wire(bridge_err(anyhow::Error::new(SendTimedOut { job_id: 42 }))),
            "HTTP/1.1 504 Gateway Timeout\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 141\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"timeout\",\"error\":\"send timed out after 30s: job 42 is still queued and may still be delivered; do not retry\",\"job_id\":42,\"ok\":false}",
        ),
        (
            "bridge_err_send_timed_out_with_context".into(),
            wire(bridge_err(anyhow::Error::new(SendTimedOut { job_id: 7 }).context("sending reply"))),
            "HTTP/1.1 504 Gateway Timeout\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 139\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"timeout\",\"error\":\"send timed out after 30s: job 7 is still queued and may still be delivered; do not retry\",\"job_id\":7,\"ok\":false}",
        ),
        (
            "outcome_accepted".into(),
            wire(enqueue_outcome_to_response(
                EnqueueOutcome::Accepted { job_id: 42, accepted_target: Some(100) },
                chat,
                "count",
                Some(200),
                Some(1_700_000_000_000),
                true,
            )),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 195\r\n\
                Connection: close\r\n\
                \r\n\
                {\"accepted\":100,\"chat_jid\":\"chat@s.whatsapp.net\",\"job_id\":42,\"more_remain\":true,\"ok\":true,\"requested\":200,\"resume_anchor\":1700000000000,\"status\":\"queued\",\"target_kind\":\"count\",\"target_value\":200}",
        ),
        (
            "outcome_already_active".into(),
            wire(enqueue_outcome_to_response(EnqueueOutcome::AlreadyActive { job_id: 7 }, chat, "all", None, None, false)),
            "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 81\r\n\
                Connection: close\r\n\
                \r\n\
                {\"chat_jid\":\"chat@s.whatsapp.net\",\"job_id\":7,\"ok\":true,\"status\":\"already_active\"}",
        ),
        (
            "outcome_cooldown".into(),
            wire(enqueue_outcome_to_response(EnqueueOutcome::Cooldown { retry_after_secs: 120 }, chat, "all", None, None, false)),
            "HTTP/1.1 429 Too Many Requests\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 77\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"rate_limited\",\"ok\":false,\"retry_after_secs\":120,\"status\":\"cooldown\"}",
        ),
        (
            "outcome_queue_full".into(),
            wire(enqueue_outcome_to_response(EnqueueOutcome::QueueFull { limit: 5 }, chat, "all", None, None, false)),
            "HTTP/1.1 429 Too Many Requests\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 66\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"rate_limited\",\"limit\":5,\"ok\":false,\"status\":\"queue_full\"}",
        ),
        (
            "outcome_embedder_backlog_full".into(),
            wire(enqueue_outcome_to_response(EnqueueOutcome::EmbedderBacklogFull { pending_count: 100_001 }, chat, "all", None, None, false)),
            "HTTP/1.1 429 Too Many Requests\r\n\
                Content-Type: application/json\r\n\
                Content-Length: 158\r\n\
                Connection: close\r\n\
                \r\n\
                {\"code\":\"rate_limited\",\"message\":\"pathological pending embeddings (>100k); wait for drain\",\"ok\":false,\"pending_count\":100001,\"status\":\"embedder_backlog_full\"}",
        ),
    ];
    // Reason phrases: only these statuses have one; every other code is written as "Unknown".
    let reason_lines = [
        (200, "HTTP/1.1 200 OK"),
        (202, "HTTP/1.1 202 Accepted"),
        (400, "HTTP/1.1 400 Bad Request"),
        (401, "HTTP/1.1 401 Unauthorized"),
        (403, "HTTP/1.1 403 Forbidden"),
        (404, "HTTP/1.1 404 Not Found"),
        (405, "HTTP/1.1 405 Method Not Allowed"),
        (411, "HTTP/1.1 411 Length Required"),
        (415, "HTTP/1.1 415 Unsupported Media Type"),
        (417, "HTTP/1.1 417 Expectation Failed"),
        (418, "HTTP/1.1 418 Unknown"),
        (429, "HTTP/1.1 429 Too Many Requests"),
        (500, "HTTP/1.1 500 Internal Server Error"),
        (503, "HTTP/1.1 503 Service Unavailable"),
        (504, "HTTP/1.1 504 Gateway Timeout"),
        (0, "HTTP/1.1 0 Unknown"),
        (999, "HTTP/1.1 999 Unknown"),
    ];
    for (status, line) in reason_lines {
        let bytes = wire(json_response(status, "{}"));
        let expected = format!("{line}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            expected,
            "status {status}"
        );
    }
    assert_golden(results);
}

/// The SSE stream is a separate byte path: fixed headers, then the immediate first heartbeat.
#[tokio::test]
async fn test_rest_sse_stream_golden() {
    let (bridge, db_path) = test_bridge("sse").await;
    let cfg = config(Cfg::Loopback);
    let expected = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    Connection: keep-alive\r\n\
                    X-Accel-Buffering: no\r\n\
                    \r\n\
                    event: heartbeat\ndata: {}\n\n";

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server_stream, _) = listener.accept().await.unwrap();
    let client_side = async {
        client.write_all(&get("/api/events", HOST)).await.unwrap();
        let mut got = vec![0u8; expected.len()];
        client.read_exact(&mut got).await.unwrap();
        cfg.sse_cancel.cancel();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        (got, rest)
    };
    let (_, (got, rest)) =
        tokio::join!(serve_connection(server_stream, &bridge, &cfg), client_side);

    assert_eq!(String::from_utf8(got).unwrap(), expected);
    assert!(
        rest.is_empty(),
        "nothing is written after cancel, got {rest:?}"
    );
    let _ = std::fs::remove_file(&db_path);
}

/// The 10 s per-read timeouts: a stalled head and a stalled body both end in the same
/// 400 "bad request" as any other unreadable request. Virtual time makes the waits instant.
#[tokio::test(start_paused = true)]
async fn test_rest_read_timeouts_golden() {
    let (bridge, db_path) = test_bridge("timeouts").await;
    let stalled = [
        ("stalled head", format!("GET /health HTTP/1.1\r\n{HOST}")),
        (
            "stalled body",
            format!("POST /api/pin-chat HTTP/1.1\r\n{HOST}{JSON}Content-Length: 50\r\n\r\n01234"),
        ),
    ];
    for (name, request) in stalled {
        let start = tokio::time::Instant::now();
        let actual = exchange(&bridge, &config(Cfg::Loopback), request.as_bytes(), false).await;
        assert_eq!(String::from_utf8(actual).unwrap(), BAD_REQUEST, "{name}");
        assert!(
            start.elapsed() >= Duration::from_secs(10),
            "{name} must wait out the 10 s read timeout"
        );
    }
    let _ = std::fs::remove_file(&db_path);
}

/// A connected loopback pair: `(client, server_side)`.
async fn connected_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (client, server)
}

/// `Expect: 100-continue` with valid auth: the server writes `100 Continue` first, and the
/// client sends the body only after reading it, whether none or part of the body came with
/// the head.
#[tokio::test]
async fn test_rest_expect_continue_interim_precedes_the_final_response() {
    let (bridge, db_path) = test_bridge("expect-continue").await;
    let cfg = config(Cfg::Token);
    let body = r#"{"jid":"1@s.whatsapp.net"}"#;
    let head = format!(
        "POST /api/pin-chat HTTP/1.1\r\n{HOST}{JSON}{TOKEN_HEADER}Expect: 100-continue\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    for already_sent in [0, 5] {
        let (mut client, server) = connected_pair().await;
        let client_side = async {
            client.write_all(head.as_bytes()).await.unwrap();
            client.write_all(&body.as_bytes()[..already_sent]).await.unwrap();
            // The body is withheld until the server has said to send it.
            let mut interim = vec![0u8; CONTINUE.len()];
            client.read_exact(&mut interim).await.unwrap();
            assert_eq!(String::from_utf8(interim).unwrap(), CONTINUE, "{already_sent} body bytes sent early");
            client.write_all(&body.as_bytes()[already_sent..]).await.unwrap();
            let mut rest = Vec::new();
            client.read_to_end(&mut rest).await.unwrap();
            rest
        };
        let (_, rest) = tokio::join!(serve_connection(server, &bridge, &cfg), client_side);
        assert_eq!(String::from_utf8(rest).unwrap(), NOT_CONNECTED, "{already_sent} body bytes sent early");
    }
    let _ = std::fs::remove_file(&db_path);
}

/// A request that fails the gate, the token check or a framing check gets only its final error:
/// no `100 Continue`, and the body that the client never sends is never waited for. The client
/// keeps the connection open and reads to EOF, so a server that went on to read the body would
/// stall for its 10 s read timeout.
#[tokio::test]
async fn test_rest_rejected_expect_continue_gets_no_interim_and_no_body_read() {
    let (bridge, db_path) = test_bridge("expect-rejected").await;
    let with_expect = |headers: &str| format!("{HOST}{headers}Expect: 100-continue\r\n");
    let cases = [
        ("bad origin", Cfg::Loopback, with_expect(&format!("{JSON}Origin: https://evil.example\r\n")), ORIGIN_NOT_ALLOWED),
        ("bad content type", Cfg::Loopback, with_expect("Content-Type: text/plain\r\n"), UNSUPPORTED_MEDIA_TYPE),
        ("missing token", Cfg::Token, with_expect(JSON), UNAUTHORIZED),
        ("chunked", Cfg::Loopback, with_expect(&format!("{JSON}Transfer-Encoding: chunked\r\n")), LENGTH_REQUIRED),
    ];
    for (name, kind, headers, expected) in cases {
        let (mut client, server) = connected_pair().await;
        let client_side = async {
            let request = post_declaring("/api/pin-chat", &headers, 50, "01234");
            client.write_all(&request).await.unwrap();
            let mut out = Vec::new();
            client.read_to_end(&mut out).await.unwrap();
            out
        };
        let cfg = config(kind);
        let both = async { tokio::join!(serve_connection(server, &bridge, &cfg), client_side).1 };
        let out = tokio::time::timeout(Duration::from_secs(5), both)
            .await
            .unwrap_or_else(|_| panic!("{name}: the server waited for a body it should not read"));
        assert_eq!(String::from_utf8(out).unwrap(), expected, "{name}");
    }
    let _ = std::fs::remove_file(&db_path);
}

/// Block the test thread briefly so bytes just written on loopback reach the other socket. A
/// paused clock jumps to the next timer as soon as the runtime is idle, which can beat bytes
/// still in flight in the kernel; the exact-timing tests therefore run over in-memory pipes (see
/// the `head_from_slow_client` unit tests), and the wire-level ones call this after each write.
fn let_bytes_land() {
    std::thread::sleep(Duration::from_millis(5));
}

/// The head has a 20 s wall-clock deadline: a head trickled at 7 s, 7 s and 6.5 s steps, so
/// never idle for the 10 s read timeout, but whose blank line would only arrive at 20.5 s, is
/// answered with the usual 400. Without the deadline this head would complete and be served
/// 200. The last chunk lands while the server is draining after its 400, so nothing is left
/// unread when it closes. Virtual time makes the waits instant; the exact 20 s cutoff is
/// asserted by the unit tests over an in-memory pipe.
#[tokio::test(start_paused = true)]
async fn test_rest_head_wall_clock_deadline_golden() {
    let (bridge, db_path) = test_bridge("head-deadline").await;
    let cfg = config(Cfg::Loopback);
    let (mut client, server) = connected_pair().await;
    let steps: [(Duration, &[u8]); 4] = [
        (Duration::ZERO, b"GET /health HTTP/1.1\r\n"),
        (Duration::from_secs(7), b"X-A: 1\r\n"),
        (Duration::from_secs(7), b"X-B: 2\r\n"),
        (Duration::from_millis(6500), b"\r\n"),
    ];
    let client_side = async {
        for (wait, chunk) in steps {
            tokio::time::sleep(wait).await;
            client.write_all(chunk).await.unwrap();
            let_bytes_land();
        }
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        out
    };
    let (_, out) = tokio::join!(serve_connection(server, &bridge, &cfg), client_side);
    assert_eq!(String::from_utf8(out).unwrap(), BAD_REQUEST);
    let _ = std::fs::remove_file(&db_path);
}

/// An early rejection while the client is still sending a body of up to 64 KiB delivers the
/// complete response: the server keeps reading until the client has finished, instead of
/// closing on unread data (which resets the connection and can discard the response).
#[tokio::test]
async fn test_rest_early_rejection_delivers_response_while_the_body_is_still_sent() {
    let (bridge, db_path) = test_bridge("drain").await;
    let body = vec![b'x'; 60 * 1024];
    let cases = [
        ("gate", Cfg::Loopback, format!("{HOST}Content-Type: text/plain\r\n"), UNSUPPORTED_MEDIA_TYPE),
        ("token", Cfg::Token, format!("{HOST}{JSON}"), UNAUTHORIZED),
        ("chunked", Cfg::Loopback, format!("{HOST}{JSON}Transfer-Encoding: chunked\r\n"), LENGTH_REQUIRED),
    ];
    for (name, kind, headers, expected) in cases {
        let (mut client, server) = connected_pair().await;
        let client_side = async {
            let head = format!("POST /api/pin-chat HTTP/1.1\r\n{headers}Content-Length: {}\r\n\r\n", body.len());
            client.write_all(head.as_bytes()).await.unwrap();
            // The rejection is written long before the upload ends.
            for piece in body.chunks(4096) {
                tokio::task::yield_now().await;
                client.write_all(piece).await.unwrap_or_else(|e| panic!("{name}: upload cut off: {e}"));
            }
            let mut out = Vec::new();
            client.read_to_end(&mut out).await.unwrap_or_else(|e| panic!("{name}: response lost: {e}"));
            out
        };
        let cfg = config(kind);
        let (_, out) = tokio::join!(serve_connection(server, &bridge, &cfg), client_side);
        assert_eq!(String::from_utf8(out).unwrap(), expected, "{name}");
    }
    let _ = std::fs::remove_file(&db_path);
}

/// The drain is bounded: a client that never stops sending cannot keep the server waiting. The
/// exact 64 KiB and 1 s limits are pinned by the `respond_and_drain` unit tests; here only the
/// connection's end is checked. What the client receives past 64 KiB is deliberately not
/// asserted, since the connection may reset once the server stops reading.
#[tokio::test]
async fn test_rest_early_rejection_does_not_wait_for_an_endless_upload() {
    let (bridge, db_path) = test_bridge("drain-bounded").await;
    let cfg = config(Cfg::Token);
    let (mut client, server) = connected_pair().await;
    let head = format!("POST /api/send HTTP/1.1\r\n{HOST}{JSON}Content-Length: 1048576\r\n\r\n");
    client.write_all(head.as_bytes()).await.unwrap();
    let (mut reader, mut writer) = client.into_split();
    let uploader = tokio::spawn(async move {
        let chunk = vec![b'x'; 16 * 1024];
        while writer.write_all(&chunk).await.is_ok() {}
    });
    let downloader = tokio::spawn(async move {
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink).await;
    });
    tokio::time::timeout(Duration::from_secs(10), serve_connection(server, &bridge, &cfg))
        .await
        .expect("the server must stop draining an endless upload");
    uploader.abort();
    downloader.abort();
    let _ = std::fs::remove_file(&db_path);
}
