//! Shared MCP tool registry: the single source of truth for tool metadata (name, description,
//! input schema, annotations, access class), argument validation and the tool -> REST mapping.
//! The stdio server (`mcp.rs`) and the HTTP MCP listener (`mcp_http.rs`) use the same registry,
//! so the two transports cannot drift apart.

use std::sync::LazyLock;

use serde_json::{json, Map, Value};

/// Whether a tool only reads state or can change it. Server-side input for the read-only
/// switch; unlike the advisory annotation hints, this is enforced by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccessClass {
    Read,
    Write,
}

/// MCP tool annotation hints (advisory; clients must not trust them for enforcement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolAnnotations {
    pub(crate) read_only_hint: bool,
    pub(crate) destructive_hint: bool,
    pub(crate) idempotent_hint: bool,
    pub(crate) open_world_hint: bool,
}

impl ToolAnnotations {
    /// Read-only tool that only touches local state.
    const READ_LOCAL: Self = Self { read_only_hint: true, destructive_hint: false, idempotent_hint: false, open_world_hint: false };
    /// Read-only tool that also asks WhatsApp.
    const READ_REMOTE: Self = Self { read_only_hint: true, destructive_hint: false, idempotent_hint: false, open_world_hint: true };
    /// Changes WhatsApp state and is not idempotent: every call has a new effect (send, post, edit).
    const WRITE_REMOTE: Self = Self { read_only_hint: false, destructive_hint: true, idempotent_hint: false, open_world_hint: true };
    /// Changes WhatsApp state and is idempotent: repeating the call has no further effect.
    const WRITE_REMOTE_IDEMPOTENT: Self = Self { read_only_hint: false, destructive_hint: true, idempotent_hint: true, open_world_hint: true };

    /// The MCP `annotations` object. A read-only tool emits only `readOnlyHint` and
    /// `openWorldHint` (destructive and idempotent are meaningless for it); any other tool
    /// emits all four hints.
    pub(crate) fn to_json(self) -> Value {
        if self.read_only_hint {
            json!({ "readOnlyHint": true, "openWorldHint": self.open_world_hint })
        } else {
            json!({
                "readOnlyHint": false,
                "destructiveHint": self.destructive_hint,
                "idempotentHint": self.idempotent_hint,
                "openWorldHint": self.open_world_hint,
            })
        }
    }
}

/// How a tool call becomes a daemon REST request. Every function receives arguments that
/// already passed [`validated_args`], and applies defaults only to ABSENT keys.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RestMapping {
    /// HTTP method, `"GET"` or `"POST"`.
    pub(crate) method: &'static str,
    /// Route path without a query string.
    pub(crate) path: &'static str,
    /// Query pairs, in wire order, unencoded.
    pub(crate) query: fn(&Value) -> Vec<(String, String)>,
    /// JSON request body (takes the validated arguments by value); `None` for no body.
    pub(crate) body: fn(Value) -> Option<Value>,
}

impl RestMapping {
    /// `GET path` with the given query pairs and no body.
    fn get(path: &'static str, query: fn(&Value) -> Vec<(String, String)>) -> Self {
        Self { method: "GET", path, query, body: no_body }
    }

    /// `POST path` with the validated arguments as the JSON body and no query.
    fn post(path: &'static str) -> Self {
        Self { method: "POST", path, query: no_query, body: args_as_body }
    }
}

fn no_query(_: &Value) -> Vec<(String, String)> {
    Vec::new()
}

fn no_body(_: Value) -> Option<Value> {
    None
}

fn args_as_body(args: Value) -> Option<Value> {
    Some(args)
}

fn sync_query(_: &Value) -> Vec<(String, String)> {
    vec![("sync".to_string(), "true".to_string())]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

fn i64_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn group_info_query(args: &Value) -> Vec<(String, String)> {
    vec![("jid".to_string(), str_arg(args, "jid").to_string())]
}

fn history_query(args: &Value) -> Vec<(String, String)> {
    let mut query = vec![
        ("jid".to_string(), str_arg(args, "jid").to_string()),
        ("limit".to_string(), i64_arg(args, "limit").unwrap_or(20).to_string()),
    ];
    if let Some(before) = i64_arg(args, "before") {
        query.push(("before".to_string(), before.to_string()));
    }
    query
}

fn search_query(args: &Value) -> Vec<(String, String)> {
    let mut query = vec![
        ("q".to_string(), str_arg(args, "q").to_string()),
        ("limit".to_string(), i64_arg(args, "limit").unwrap_or(20).to_string()),
    ];
    if let Some(jid) = args.get("jid").and_then(Value::as_str) {
        query.push(("jid".to_string(), jid.to_string()));
    }
    query
}

fn fetch_status_query(args: &Value) -> Vec<(String, String)> {
    if let Some(job_id) = i64_arg(args, "job_id") {
        vec![("job_id".to_string(), job_id.to_string())]
    } else {
        // Default active=true (active-only) to match CLI behaviour; pass active:false
        // explicitly to list all jobs.
        let active = args.get("active").and_then(Value::as_bool).unwrap_or(true);
        vec![("active".to_string(), active.to_string())]
    }
}

/// `{chat_jid, mode (default "all"), target_value if present}`, moved out of the arguments.
fn fetch_history_body(args: Value) -> Option<Value> {
    let mut args = match args {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let mut body = json!({
        "chat_jid": args.remove("chat_jid").unwrap_or_else(|| json!("")),
        "mode": args.remove("mode").unwrap_or_else(|| json!("all")),
    });
    if let Some(target_value) = args.remove("target_value") {
        body["target_value"] = target_value;
    }
    Some(body)
}

/// One MCP tool: everything the transports need to list it, validate a call to it and
/// forward the call to the daemon's REST API.
#[derive(Debug)]
pub(crate) struct McpToolMeta {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    /// JSON Schema for the arguments. Always `"additionalProperties": false`, with an object
    /// `properties`.
    pub(crate) input_schema: Value,
    pub(crate) annotations: ToolAnnotations,
    /// Read by the HTTP listener's read-only switch ([`McpToolMeta::allowed`]).
    pub(crate) access_class: AccessClass,
    pub(crate) rest_mapping: RestMapping,
}

impl McpToolMeta {
    fn new(
        name: &'static str,
        description: &'static str,
        mut input_schema: Value,
        annotations: ToolAnnotations,
        access_class: AccessClass,
        rest_mapping: RestMapping,
    ) -> Self {
        // Every tool is strict: `validated_args` rejects undeclared keys, so the advertised
        // schema says the same. Set here once instead of in each schema literal.
        input_schema["additionalProperties"] = json!(false);
        Self { name, description, input_schema, annotations, access_class, rest_mapping }
    }

    /// A tool that only reads state.
    fn read(name: &'static str, description: &'static str, input_schema: Value, annotations: ToolAnnotations, rest_mapping: RestMapping) -> Self {
        Self::new(name, description, input_schema, annotations, AccessClass::Read, rest_mapping)
    }

    /// A tool that can change state.
    fn write(name: &'static str, description: &'static str, input_schema: Value, annotations: ToolAnnotations, rest_mapping: RestMapping) -> Self {
        Self::new(name, description, input_schema, annotations, AccessClass::Write, rest_mapping)
    }

    /// Whether the tool may be listed and called: always, except that a read-only listener
    /// (`read_only`) allows only [`AccessClass::Read`] tools.
    pub(crate) fn allowed(&self, read_only: bool) -> bool {
        !read_only || self.access_class == AccessClass::Read
    }

    /// The schema's `properties` object (every registered schema has one).
    pub(crate) fn properties(&self) -> &Map<String, Value> {
        self.input_schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("every tool schema declares an object `properties`")
    }

    /// The schema's `required` keys (none when the schema has no `required`).
    fn required(&self) -> impl Iterator<Item = &str> + '_ {
        self.input_schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str)
    }

    fn is_required(&self, key: &str) -> bool {
        self.required().any(|required| required == key)
    }
}

/// All MCP tools, in `tools/list` order.
pub(crate) struct McpRegistry {
    tools: Vec<McpToolMeta>,
}

impl McpRegistry {
    /// The `tools/list` entries: `name`, `description`, `inputSchema` and `annotations`.
    pub(crate) fn tool_definitions(&self) -> Vec<Value> {
        self.tool_definitions_for(false)
    }

    /// The `tools/list` entries a listener exposes: every tool, or with `read_only` only the
    /// tools [`McpToolMeta::allowed`] lets through.
    pub(crate) fn tool_definitions_for(&self, read_only: bool) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|t| t.allowed(read_only))
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                    "annotations": t.annotations.to_json(),
                })
            })
            .collect()
    }

    /// The tool called `name`, if there is one.
    pub(crate) fn find_tool(&self, name: &str) -> Option<&McpToolMeta> {
        self.tools.iter().find(|t| t.name == name)
    }

    fn build() -> Self {
        let jid_only = || json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]});
        Self {
            tools: vec![
                McpToolMeta::read("whatsrust_status", "Get bridge status, metrics, and connection state",
                    json!({"type":"object","properties":{}}),
                    ToolAnnotations::READ_LOCAL, RestMapping::get("/api/status", no_query)),
                McpToolMeta::write("whatsrust_send", "Send a text message (optionally with @mentions)",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string","description":"Recipient JID (phone@s.whatsapp.net or group@g.us)"},
                        "text":{"type":"string","description":"Message text"},
                        "mentions":{"type":"array","items":{"type":"string"},"description":"JIDs to @mention (optional)"}
                    },"required":["jid","text"]}),
                    ToolAnnotations::WRITE_REMOTE,
                    RestMapping { method: "POST", path: "/api/send", query: sync_query, body: args_as_body }),
                McpToolMeta::write("whatsrust_reply", "Reply to a message (quote)",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},
                        "id":{"type":"string","description":"Message ID to reply to"},
                        "sender":{"type":"string","description":"Sender JID of the quoted message"},
                        "text":{"type":"string"},
                        "mentions":{"type":"array","items":{"type":"string"},"description":"JIDs to @mention"}
                    },"required":["jid","id","sender","text"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/reply")),
                McpToolMeta::write("whatsrust_react", "React to a message with an emoji",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},
                        "id":{"type":"string","description":"Target message ID"},
                        "emoji":{"type":"string"},
                        "from_me":{"type":"boolean","description":"Whether target is from us (default true)"},
                        "sender_jid":{"type":"string","description":"Target sender JID (required for group reactions when from_me=false)"}
                    },"required":["jid","id","emoji"]}),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/react")),
                McpToolMeta::write("whatsrust_edit", "Edit a previously sent message",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"id":{"type":"string"},"text":{"type":"string"}
                    },"required":["jid","id","text"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/edit")),
                McpToolMeta::write("whatsrust_revoke", "Delete a message for everyone",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"id":{"type":"string"}
                    },"required":["jid","id"]}),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/revoke")),
                McpToolMeta::write("whatsrust_image", "Send an image (base64)",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"data":{"type":"string","description":"Base64-encoded image"},
                        "mime":{"type":"string","description":"MIME type (default image/jpeg)"},
                        "caption":{"type":"string"}
                    },"required":["jid","data"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/image")),
                McpToolMeta::read("whatsrust_groups", "List all joined groups",
                    json!({"type":"object","properties":{}}),
                    ToolAnnotations::READ_REMOTE, RestMapping::get("/api/groups", no_query)),
                McpToolMeta::read("whatsrust_group_info", "Get group details and members",
                    json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]}),
                    ToolAnnotations::READ_REMOTE, RestMapping::get("/api/group-info", group_info_query)),
                McpToolMeta::read("whatsrust_history", "Get recent messages for a chat",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"limit":{"type":"integer","description":"Max messages (default 20)"},
                        "before":{"type":"integer","description":"Unix timestamp for pagination"}
                    },"required":["jid"]}),
                    ToolAnnotations::READ_LOCAL, RestMapping::get("/api/history", history_query)),
                McpToolMeta::read("whatsrust_search", "Search message history",
                    json!({"type":"object","properties":{
                        "q":{"type":"string","description":"Search query"},
                        "jid":{"type":"string","description":"Limit to specific chat (optional)"},
                        "limit":{"type":"integer"}
                    },"required":["q"]}),
                    ToolAnnotations::READ_LOCAL, RestMapping::get("/api/search", search_query)),
                McpToolMeta::write("whatsrust_fetch_history",
                    "Trigger a per-chat historical backfill. Runs as a paced background job (anti-ban) and is daemon-clamped (cooldown, queue-depth, max-messages); poll whatsrust_fetch_status for progress.",
                    json!({"type":"object","properties":{
                        "chat_jid":{"type":"string","description":"Chat JID to backfill"},
                        "mode":{"type":"string","description":"all | since | count; default all"},
                        "target_value":{"type":"integer","description":"ms-timestamp for since, count for count"}
                    },"required":["chat_jid"]}),
                    // Enqueues a local job (not destructive) that then talks to WhatsApp.
                    ToolAnnotations { read_only_hint: false, destructive_hint: false, idempotent_hint: false, open_world_hint: true },
                    RestMapping { method: "POST", path: "/api/history-fetch", query: no_query, body: fetch_history_body }),
                McpToolMeta::read("whatsrust_fetch_status",
                    "Poll history-fetch job status. Pass job_id for a single job; otherwise lists active jobs only (active defaults to true); pass active=false to list all jobs.",
                    json!({"type":"object","properties":{
                        "job_id":{"type":"integer","description":"Specific job ID to query (optional)"},
                        "active":{"type":"boolean","description":"List only active jobs (optional)"}
                    }}),
                    ToolAnnotations::READ_LOCAL, RestMapping::get("/api/history-fetch", fetch_status_query)),
                McpToolMeta::write("whatsrust_fetch_cancel", "Cancel a history-fetch backfill job",
                    json!({"type":"object","properties":{
                        "job_id":{"type":"integer","description":"Job ID to cancel"}
                    },"required":["job_id"]}),
                    // Only updates the local store.
                    ToolAnnotations { read_only_hint: false, destructive_hint: false, idempotent_hint: true, open_world_hint: false },
                    RestMapping::post("/api/history-fetch/cancel")),
                McpToolMeta::write("whatsrust_typing", "Send typing indicator", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/typing")),
                McpToolMeta::write("whatsrust_location", "Send a location pin",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"lat":{"type":"number"},"lon":{"type":"number"},
                        "name":{"type":"string"},"address":{"type":"string"}
                    },"required":["jid","lat","lon"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/location")),
                McpToolMeta::write("whatsrust_contact", "Send a contact card",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"name":{"type":"string"},
                        "phone":{"type":"string","description":"Phone number of the contact in international format, digits only (e.g. 972500000000)"}
                    },"required":["jid","name","phone"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/contact")),
                McpToolMeta::write("whatsrust_poll", "Create a poll",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"question":{"type":"string"},
                        "options":{"type":"array","items":{"type":"string"}},
                        "selectable_count":{"type":"integer","description":"Max selectable options"}
                    },"required":["jid","question","options","selectable_count"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/poll")),
                // Chat management
                McpToolMeta::write("whatsrust_pin_chat", "Pin a chat to the top", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/pin-chat")),
                McpToolMeta::write("whatsrust_unpin_chat", "Unpin a chat", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/unpin-chat")),
                McpToolMeta::write("whatsrust_mute_chat", "Mute a chat indefinitely", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/mute-chat")),
                McpToolMeta::write("whatsrust_unmute_chat", "Unmute a chat", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/unmute-chat")),
                McpToolMeta::write("whatsrust_archive_chat", "Archive a chat", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/archive-chat")),
                McpToolMeta::write("whatsrust_unarchive_chat", "Unarchive a chat", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/unarchive-chat")),
                McpToolMeta::write("whatsrust_mark_read", "Mark a chat as read", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/mark-read")),
                McpToolMeta::write("whatsrust_delete_chat", "Delete a chat and its media", jid_only(),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/delete-chat")),
                McpToolMeta::write("whatsrust_delete_for_me", "Delete a message for me only",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                        "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                        "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
                    },"required":["jid","id"]}),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/delete-for-me")),
                McpToolMeta::write("whatsrust_star", "Star a message",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                        "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                        "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
                    },"required":["jid","id"]}),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/star")),
                McpToolMeta::write("whatsrust_unstar", "Unstar a message",
                    json!({"type":"object","properties":{
                        "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                        "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                        "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
                    },"required":["jid","id"]}),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/unstar")),
                McpToolMeta::write("whatsrust_status_text", "Post a text status/story to specified recipients",
                    json!({"type":"object","properties":{
                        "recipients":{"type":"array","items":{"type":"string"},"description":"Phone numbers of recipients"},
                        "text":{"type":"string","description":"Status text"},
                        "background_argb":{"type":"integer","description":"Background color as 0xAARRGGBB (default 0xFF1E6E4F)"},
                        "font":{"type":"integer","description":"Font style 0-4 (default 0)"},
                        "privacy":{"type":"string","description":"contacts|allowlist|denylist (default contacts)"}
                    },"required":["recipients","text"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/status-text")),
                McpToolMeta::write("whatsrust_status_image", "Post an image status/story",
                    json!({"type":"object","properties":{
                        "recipients":{"type":"array","items":{"type":"string"}},
                        "data":{"type":"string","description":"Base64-encoded image"},
                        "mime":{"type":"string","description":"MIME type (default image/jpeg)"},
                        "caption":{"type":"string"},
                        "privacy":{"type":"string"}
                    },"required":["recipients","data"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/status-image")),
                McpToolMeta::write("whatsrust_status_video", "Post a video status/story",
                    json!({"type":"object","properties":{
                        "recipients":{"type":"array","items":{"type":"string"}},
                        "data":{"type":"string","description":"Base64-encoded video"},
                        "mime":{"type":"string","description":"MIME type (default video/mp4)"},
                        "caption":{"type":"string"},
                        "seconds":{"type":"integer","description":"Duration in seconds"},
                        "privacy":{"type":"string"}
                    },"required":["recipients","data"]}),
                    ToolAnnotations::WRITE_REMOTE, RestMapping::post("/api/status-video")),
                McpToolMeta::write("whatsrust_status_revoke", "Revoke a previously posted status/story",
                    json!({"type":"object","properties":{
                        "recipients":{"type":"array","items":{"type":"string"},"description":"Same recipients as original post"},
                        "message_id":{"type":"string","description":"ID returned when status was posted"},
                        "privacy":{"type":"string"}
                    },"required":["recipients","message_id"]}),
                    ToolAnnotations::WRITE_REMOTE_IDEMPOTENT, RestMapping::post("/api/status-revoke")),
                McpToolMeta::write("whatsrust_purge_embeddings", "Purge all embeddings for a given model and reclaim disk space (M2.5)",
                    json!({"type":"object","properties":{
                        "model_id":{"type":"string","description":"Model ID to purge embeddings for"}
                    },"required":["model_id"]}),
                    // Deletes local data only, so a closed world; destructive by owner decision.
                    ToolAnnotations { read_only_hint: false, destructive_hint: true, idempotent_hint: true, open_world_hint: false },
                    RestMapping::post("/api/embeddings/purge")),
            ],
        }
    }
}

/// The registry of all MCP tools.
pub(crate) static REGISTRY: LazyLock<McpRegistry> = LazyLock::new(McpRegistry::build);

/// Why a tool call's arguments were rejected, and how the transport reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArgsError {
    /// A protocol-level error: JSON-RPC `-32602` (unknown tool, `arguments` not an object).
    InvalidParams(String),
    /// A tool execution error: an `isError: true` tool result (bad or missing arguments).
    Tool(String),
}

/// The top-level keys of `given` that are not in `declared`, sorted.
fn unknown_keys(given: &Map<String, Value>, declared: &Map<String, Value>) -> Vec<String> {
    let mut unknown: Vec<String> = given.keys().filter(|k| !declared.contains_key(k.as_str())).cloned().collect();
    unknown.sort();
    unknown
}

/// Error text for a call that passed undeclared arguments: names the tool, the unknown key(s)
/// and the allowed keys (both sorted), and states that nothing was sent.
fn unknown_args_message(name: &str, unknown: &[String], declared: &Map<String, Value>) -> String {
    let mut allowed: Vec<&str> = declared.keys().map(String::as_str).collect();
    allowed.sort_unstable();
    let allowed = if allowed.is_empty() { "(none)".to_string() } else { allowed.join(", ") };
    format!(
        "{name}: unknown argument(s): {}. Allowed arguments: {allowed}. Nothing was sent.",
        unknown.join(", ")
    )
}

/// Check a schema `"type": "integer"` value: it must fit `i64`. An integral float inside the
/// `i64` range (`5.0`) is accepted and rewritten to the integer (`5`); everything else
/// (fractional floats, `u64` above `i64::MAX`, out-of-range floats) is rejected.
fn normalise_integer(value: &mut Value) -> bool {
    let Value::Number(number) = value else { return false };
    if number.as_i64().is_some() {
        return true;
    }
    match number.as_f64() {
        Some(f) if number.is_f64() && f.fract() == 0.0 && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) => {
            *value = json!(f as i64);
            true
        }
        _ => false,
    }
}

/// Check one value against its property's top-level schema `type`. `Err` holds the phrase for
/// the expected type. A type the check does not know is accepted unchecked.
fn check_type(ty: &str, value: &mut Value) -> Result<(), &'static str> {
    let (expected, ok) = match ty {
        "string" => ("a string", value.is_string()),
        "boolean" => ("a boolean", value.is_boolean()),
        "array" => ("an array", value.is_array()),
        "object" => ("an object", value.is_object()),
        "number" => ("a number", value.is_number()),
        "integer" => ("an integer (64-bit)", normalise_integer(value)),
        _ => return Ok(()),
    };
    if ok {
        Ok(())
    } else {
        Err(expected)
    }
}

/// Check a tool call and return the tool plus the arguments to forward.
///
/// In order: an unknown tool and non-object `arguments` are [`ArgsError::InvalidParams`]
/// (absent or `null` `arguments` mean `{}`). The rest are [`ArgsError::Tool`]: any undeclared
/// top-level key (e.g. `path`, which would make the daemon read a local file and send it) is
/// reported alone; otherwise a declared optional key set to `null` is dropped as absent, each
/// remaining value must match its property's top-level `type` (integral floats are normalised
/// to integers), and every `required` key must be present and non-null. All type and required
/// problems are reported together in one message, sorted by field name. Nested values (array
/// items, nested objects) are not inspected: the REST handlers validate them.
pub(crate) fn validated_args(name: &str, args: Value) -> Result<(&'static McpToolMeta, Value), ArgsError> {
    let tool = REGISTRY
        .find_tool(name)
        .ok_or_else(|| ArgsError::InvalidParams(format!("Unknown tool: {name}")))?;
    let mut given = match args {
        Value::Null => Map::new(),
        Value::Object(map) => map,
        _ => return Err(ArgsError::InvalidParams("arguments must be a JSON object".to_string())),
    };

    let declared = tool.properties();
    let unknown = unknown_keys(&given, declared);
    if !unknown.is_empty() {
        return Err(ArgsError::Tool(unknown_args_message(name, &unknown, declared)));
    }

    // An optional key given as null means "not given"; a required one stays to be reported.
    given.retain(|key, value| !value.is_null() || tool.is_required(key));

    let mut problems: Vec<(String, String)> = Vec::new();
    for (key, value) in given.iter_mut() {
        if value.is_null() {
            continue;
        }
        let Some(ty) = declared.get(key.as_str()).and_then(|property| property.get("type")).and_then(Value::as_str) else {
            continue;
        };
        if let Err(expected) = check_type(ty, value) {
            problems.push((key.clone(), format!("{key} must be {expected}")));
        }
    }
    for key in tool.required() {
        if given.get(key).is_none_or(Value::is_null) {
            problems.push((key.to_string(), format!("{key} is required")));
        }
    }
    if !problems.is_empty() {
        problems.sort_unstable();
        let problems: Vec<String> = problems.into_iter().map(|(_, message)| message).collect();
        return Err(ArgsError::Tool(format!(
            "{name}: invalid argument(s): {}. Nothing was sent.",
            problems.join("; ")
        )));
    }
    Ok((tool, Value::Object(given)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIX_TYPES: [&str; 6] = ["string", "boolean", "array", "object", "number", "integer"];

    /// Expected annotations. A read tool emits (and is checked on) only read-only + open-world.
    #[derive(Clone, Copy)]
    enum Hints {
        Read { open_world: bool },
        Write { destructive: bool, idempotent: bool, open_world: bool },
    }

    /// Every tool's expected annotations, in `tools/list` order.
    fn annotation_table() -> Vec<(&'static str, Hints)> {
        use Hints::{Read, Write};
        let send_like = Write { destructive: true, idempotent: false, open_world: true };
        let idempotent = Write { destructive: true, idempotent: true, open_world: true };
        vec![
            ("whatsrust_status", Read { open_world: false }),
            ("whatsrust_send", send_like),
            ("whatsrust_reply", send_like),
            ("whatsrust_react", idempotent),
            ("whatsrust_edit", send_like),
            ("whatsrust_revoke", idempotent),
            ("whatsrust_image", send_like),
            ("whatsrust_groups", Read { open_world: true }),
            ("whatsrust_group_info", Read { open_world: true }),
            ("whatsrust_history", Read { open_world: false }),
            ("whatsrust_search", Read { open_world: false }),
            ("whatsrust_fetch_history", Write { destructive: false, idempotent: false, open_world: true }),
            ("whatsrust_fetch_status", Read { open_world: false }),
            ("whatsrust_fetch_cancel", Write { destructive: false, idempotent: true, open_world: false }),
            ("whatsrust_typing", idempotent),
            ("whatsrust_location", send_like),
            ("whatsrust_contact", send_like),
            ("whatsrust_poll", send_like),
            ("whatsrust_pin_chat", idempotent),
            ("whatsrust_unpin_chat", idempotent),
            ("whatsrust_mute_chat", idempotent),
            ("whatsrust_unmute_chat", idempotent),
            ("whatsrust_archive_chat", idempotent),
            ("whatsrust_unarchive_chat", idempotent),
            ("whatsrust_mark_read", idempotent),
            ("whatsrust_delete_chat", idempotent),
            ("whatsrust_delete_for_me", idempotent),
            ("whatsrust_star", idempotent),
            ("whatsrust_unstar", idempotent),
            ("whatsrust_status_text", send_like),
            ("whatsrust_status_image", send_like),
            ("whatsrust_status_video", send_like),
            ("whatsrust_status_revoke", idempotent),
            ("whatsrust_purge_embeddings", Write { destructive: true, idempotent: true, open_world: false }),
        ]
    }

    fn tool(name: &str) -> &'static McpToolMeta {
        REGISTRY.find_tool(name).unwrap_or_else(|| panic!("{name}: not in the registry"))
    }

    /// The arguments `validated_args` would forward, or the error.
    fn validated(name: &str, args: Value) -> Result<Value, ArgsError> {
        validated_args(name, args).map(|(_, args)| args)
    }

    /// The text of the `Tool` error `validated_args` returns; panics on anything else.
    fn tool_error(name: &str, args: Value) -> String {
        match validated_args(name, args) {
            Err(ArgsError::Tool(message)) => message,
            other => panic!("{name}: expected a tool error, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Registry, lookup, annotations, access class
    // -------------------------------------------------------------------------

    #[test]
    fn test_registry_has_34_uniquely_named_tools() {
        assert_eq!(REGISTRY.tools.len(), 34);
        let names: std::collections::BTreeSet<&str> = REGISTRY.tools.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 34, "tool names must be unique");
        assert!(names.iter().all(|n| n.starts_with("whatsrust_")));
    }

    #[test]
    fn test_find_tool_hit_and_miss() {
        assert_eq!(REGISTRY.find_tool("whatsrust_send").map(|t| t.name), Some("whatsrust_send"));
        assert!(REGISTRY.find_tool("whatsrust_nope").is_none());
        assert!(REGISTRY.find_tool("").is_none());
        assert!(REGISTRY.find_tool("WHATSRUST_SEND").is_none(), "lookup is case-sensitive");
    }

    #[test]
    fn test_tool_definitions_carry_name_description_schema_and_annotations() {
        let definitions = REGISTRY.tool_definitions();
        assert_eq!(definitions.len(), 34);
        for (definition, tool) in definitions.iter().zip(&REGISTRY.tools) {
            assert_eq!(definition["name"], json!(tool.name));
            assert!(!tool.description.is_empty(), "{}: empty description", tool.name);
            assert_eq!(definition["description"], json!(tool.description));
            assert_eq!(definition["inputSchema"], tool.input_schema);
            assert_eq!(definition["annotations"], tool.annotations.to_json());
            assert_eq!(definition.as_object().map(Map::len), Some(4), "{}: exactly four keys", tool.name);
        }
    }

    #[test]
    fn test_read_tool_annotations_json_has_exactly_two_keys() {
        let annotations = tool("whatsrust_history").annotations.to_json();
        assert_eq!(annotations, json!({"readOnlyHint": true, "openWorldHint": false}));
        assert_eq!(annotations.as_object().map(Map::len), Some(2));
        let annotations = tool("whatsrust_groups").annotations.to_json();
        assert_eq!(annotations, json!({"readOnlyHint": true, "openWorldHint": true}));
    }

    #[test]
    fn test_write_tool_annotations_json_has_all_four_keys() {
        let annotations = tool("whatsrust_react").annotations.to_json();
        assert_eq!(
            annotations,
            json!({"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": true})
        );
        assert_eq!(annotations.as_object().map(Map::len), Some(4));
    }

    #[test]
    fn test_annotation_table_for_all_34_tools() {
        let table = annotation_table();
        assert_eq!(table.len(), 34);
        let names: Vec<&str> = REGISTRY.tools.iter().map(|t| t.name).collect();
        let table_names: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
        assert_eq!(table_names, names, "table must list every tool in registry order");

        for (name, hints) in table {
            let annotations = tool(name).annotations;
            match hints {
                Hints::Read { open_world } => {
                    assert!(annotations.read_only_hint, "{name}: readOnlyHint");
                    assert_eq!(annotations.open_world_hint, open_world, "{name}: openWorldHint");
                    assert_eq!(
                        annotations.to_json(),
                        json!({"readOnlyHint": true, "openWorldHint": open_world}),
                        "{name}: a read tool emits only readOnlyHint and openWorldHint"
                    );
                }
                Hints::Write { destructive, idempotent, open_world } => {
                    assert!(!annotations.read_only_hint, "{name}: readOnlyHint");
                    assert_eq!(annotations.destructive_hint, destructive, "{name}: destructiveHint");
                    assert_eq!(annotations.idempotent_hint, idempotent, "{name}: idempotentHint");
                    assert_eq!(annotations.open_world_hint, open_world, "{name}: openWorldHint");
                    assert_eq!(
                        annotations.to_json(),
                        json!({
                            "readOnlyHint": false,
                            "destructiveHint": destructive,
                            "idempotentHint": idempotent,
                            "openWorldHint": open_world,
                        }),
                        "{name}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_access_class_read_iff_read_only_hint_and_exactly_six_read_tools() {
        for t in &REGISTRY.tools {
            assert_eq!(
                t.access_class == AccessClass::Read,
                t.annotations.read_only_hint,
                "{}: access_class and readOnlyHint must agree",
                t.name
            );
        }
        let mut read: Vec<&str> = REGISTRY.tools.iter().filter(|t| t.access_class == AccessClass::Read).map(|t| t.name).collect();
        read.sort_unstable();
        assert_eq!(
            read,
            [
                "whatsrust_fetch_status", "whatsrust_group_info", "whatsrust_groups",
                "whatsrust_history", "whatsrust_search", "whatsrust_status",
            ]
        );
        let writes = REGISTRY.tools.iter().filter(|t| t.access_class == AccessClass::Write).count();
        assert_eq!(writes, 28);
    }

    #[test]
    fn test_allowed_and_read_only_tool_definitions() {
        for t in &REGISTRY.tools {
            assert!(t.allowed(false), "{}: everything is allowed without the read-only switch", t.name);
            assert_eq!(t.allowed(true), t.access_class == AccessClass::Read, "{}", t.name);
        }
        assert_eq!(REGISTRY.tool_definitions_for(false), REGISTRY.tool_definitions());

        let names: Vec<String> = REGISTRY
            .tool_definitions_for(true)
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        // Registry order, and exactly the six Read tools.
        assert_eq!(
            names,
            [
                "whatsrust_status", "whatsrust_groups", "whatsrust_group_info",
                "whatsrust_history", "whatsrust_search", "whatsrust_fetch_status",
            ]
        );
    }

    #[test]
    fn test_read_tools_get_and_write_tools_post_on_a_static_api_path() {
        for t in &REGISTRY.tools {
            let expected = if t.access_class == AccessClass::Read { "GET" } else { "POST" };
            assert_eq!(t.rest_mapping.method, expected, "{}: method", t.name);
            assert!(t.rest_mapping.path.starts_with("/api/"), "{}: path {}", t.name, t.rest_mapping.path);
            assert!(!t.rest_mapping.path.contains('?'), "{}: the query belongs in `query`, not `path`", t.name);
        }
    }

    #[test]
    fn test_every_declared_property_has_one_of_the_six_checked_types() {
        // Keeps the "property without a recognised type is not checked" branch unreachable.
        for t in &REGISTRY.tools {
            for (key, property) in t.properties() {
                let ty = property.get("type").and_then(Value::as_str);
                assert!(ty.is_some_and(|ty| SIX_TYPES.contains(&ty)), "{}.{key}: type {ty:?}", t.name);
            }
        }
    }

    #[test]
    fn test_every_schema_forbids_additional_properties_and_has_object_properties() {
        for t in &REGISTRY.tools {
            assert_eq!(t.input_schema["additionalProperties"], json!(false), "{}", t.name);
            assert_eq!(t.input_schema["type"], json!("object"), "{}", t.name);
            assert!(t.input_schema["properties"].is_object(), "{}", t.name);
        }
    }

    #[test]
    fn test_fetch_status_description_states_the_active_only_default() {
        assert_eq!(
            tool("whatsrust_fetch_status").description,
            "Poll history-fetch job status. Pass job_id for a single job; otherwise lists active jobs only (active defaults to true); pass active=false to list all jobs."
        );
    }

    // -------------------------------------------------------------------------
    // validated_args: protocol errors
    // -------------------------------------------------------------------------

    #[test]
    fn test_validated_args_unknown_tool_is_invalid_params() {
        for (name, args) in [
            ("whatsrust_nope", json!({"path": "/etc/passwd"})),
            ("", json!("not an object")),
            ("WHATSRUST_SEND", Value::Null),
        ] {
            assert_eq!(
                validated(name, args),
                Err(ArgsError::InvalidParams(format!("Unknown tool: {name}"))),
                "tool {name:?}"
            );
        }
    }

    #[test]
    fn test_validated_args_non_object_arguments_is_invalid_params() {
        for args in [json!("path"), json!(5), json!(5.5), json!(true), json!(["jid"])] {
            assert_eq!(
                validated("whatsrust_image", args.clone()),
                Err(ArgsError::InvalidParams("arguments must be a JSON object".to_string())),
                "args: {args}"
            );
        }
    }

    #[test]
    fn test_validated_args_null_arguments_mean_empty_object() {
        assert_eq!(validated("whatsrust_status", Value::Null), Ok(json!({})));
        assert_eq!(validated("whatsrust_status", json!({})), Ok(json!({})));
        assert_eq!(validated("whatsrust_fetch_status", Value::Null), Ok(json!({})));
        // ...which a tool with required arguments then rejects as missing them.
        assert_eq!(
            tool_error("whatsrust_image", Value::Null),
            "whatsrust_image: invalid argument(s): data is required; jid is required. Nothing was sent."
        );
    }

    #[test]
    fn test_validated_args_returns_the_matching_tool() {
        let (tool, args) = validated_args("whatsrust_group_info", json!({"jid": "1@g.us"})).unwrap();
        assert_eq!(tool.name, "whatsrust_group_info");
        assert_eq!(args, json!({"jid": "1@g.us"}));
    }

    // -------------------------------------------------------------------------
    // validated_args: undeclared keys
    // -------------------------------------------------------------------------

    #[test]
    fn test_validated_args_error_names_tool_sorted_unknown_keys_and_allowed_keys() {
        assert_eq!(
            tool_error("whatsrust_image", json!({"jid": "j", "data": "AAAA", "zeta": 1, "path": "/etc/passwd"})),
            "whatsrust_image: unknown argument(s): path, zeta. Allowed arguments: caption, data, jid, mime. Nothing was sent."
        );
        assert_eq!(
            tool_error("whatsrust_status", json!({"x": 1})),
            "whatsrust_status: unknown argument(s): x. Allowed arguments: (none). Nothing was sent."
        );
    }

    #[test]
    fn test_validated_args_undeclared_key_is_reported_alone_even_with_other_problems() {
        // Wrong type (jid), missing required (text) and an undeclared key: only the undeclared
        // key is reported.
        assert_eq!(
            tool_error("whatsrust_send", json!({"jid": 5, "schedule_at": 1})),
            "whatsrust_send: unknown argument(s): schedule_at. Allowed arguments: jid, mentions, text. Nothing was sent."
        );
    }

    #[test]
    fn test_validated_args_undeclared_key_is_rejected_whatever_its_value() {
        for value in [json!(null), json!(false), json!("x"), json!({})] {
            let message = tool_error("whatsrust_typing", json!({"jid": "1@g.us", "extra": value.clone()}));
            assert!(message.contains("unknown argument(s): extra"), "value {value}: {message}");
        }
    }

    #[test]
    fn test_validated_args_contact_accepts_phone_and_rejects_vcard() {
        // REST builds (and sanitizes) the vCard itself from `name` + `phone`; a raw `vcard`
        // must never be forwarded.
        let ok = json!({"jid": "1@s.whatsapp.net", "name": "Ann", "phone": "972500000000"});
        assert_eq!(validated("whatsrust_contact", ok.clone()), Ok(ok));

        let with_vcard = json!({"jid": "1@s.whatsapp.net", "name": "Ann", "phone": "972500000000", "vcard": "BEGIN:VCARD"});
        let message = tool_error("whatsrust_contact", with_vcard);
        assert!(message.contains("vcard"), "{message}");
    }

    // -------------------------------------------------------------------------
    // validated_args: types, required, null, integers
    // -------------------------------------------------------------------------

    #[test]
    fn test_validated_args_accepts_declared_keys_verbatim_including_nested_values() {
        let send = json!({"jid": "1@g.us", "text": "hi", "mentions": ["1@s.whatsapp.net", "2@s.whatsapp.net"]});
        assert_eq!(validated("whatsrust_send", send.clone()), Ok(send));

        let required_only = json!({"jid": "1@g.us", "text": "hi"});
        assert_eq!(validated("whatsrust_send", required_only.clone()), Ok(required_only));

        // Only the top-level type is checked: array items are the REST handler's business.
        let odd_items = json!({"jid": "1@g.us", "text": "hi", "mentions": [1, {"a": [2]}, null, "x"]});
        assert_eq!(validated("whatsrust_send", odd_items.clone()), Ok(odd_items));
        let poll = json!({"jid": "1@g.us", "question": "q", "options": [], "selectable_count": 1});
        assert_eq!(validated("whatsrust_poll", poll.clone()), Ok(poll));
    }

    #[test]
    fn test_validated_args_wrong_type_is_a_tool_error_naming_the_field() {
        assert_eq!(
            tool_error("whatsrust_send", json!({"jid": 5, "text": "hi"})),
            "whatsrust_send: invalid argument(s): jid must be a string. Nothing was sent."
        );
        assert_eq!(
            tool_error("whatsrust_send", json!({"jid": "1@g.us", "text": "hi", "mentions": "1@s.whatsapp.net"})),
            "whatsrust_send: invalid argument(s): mentions must be an array. Nothing was sent."
        );
        assert_eq!(
            tool_error("whatsrust_react", json!({"jid": "1@g.us", "id": "A", "emoji": "x", "from_me": "yes"})),
            "whatsrust_react: invalid argument(s): from_me must be a boolean. Nothing was sent."
        );
        assert_eq!(
            tool_error("whatsrust_location", json!({"jid": "1@g.us", "lat": "31.5", "lon": 34.75})),
            "whatsrust_location: invalid argument(s): lat must be a number. Nothing was sent."
        );
        assert_eq!(
            tool_error("whatsrust_history", json!({"jid": "1@g.us", "limit": "5"})),
            "whatsrust_history: invalid argument(s): limit must be an integer (64-bit). Nothing was sent."
        );
        assert_eq!(
            tool_error("whatsrust_history", json!({"jid": "1@g.us", "limit": true})),
            "whatsrust_history: invalid argument(s): limit must be an integer (64-bit). Nothing was sent."
        );
    }

    #[test]
    fn test_validated_args_missing_required_is_a_tool_error() {
        assert_eq!(
            tool_error("whatsrust_send", json!({"jid": "1@g.us"})),
            "whatsrust_send: invalid argument(s): text is required. Nothing was sent."
        );
        assert_eq!(
            tool_error("whatsrust_fetch_cancel", json!({})),
            "whatsrust_fetch_cancel: invalid argument(s): job_id is required. Nothing was sent."
        );
    }

    #[test]
    fn test_validated_args_reports_all_field_errors_in_one_message_sorted_by_field() {
        assert_eq!(
            tool_error("whatsrust_poll", json!({"selectable_count": "1", "options": {}})),
            "whatsrust_poll: invalid argument(s): jid is required; options must be an array; question is required; \
             selectable_count must be an integer (64-bit). Nothing was sent."
        );
    }

    #[test]
    fn test_validated_args_null_optional_is_dropped_from_the_forwarded_args() {
        let (_, args) = validated_args("whatsrust_send", json!({"jid": "1@g.us", "text": "hi", "mentions": null})).unwrap();
        assert_eq!(args, json!({"jid": "1@g.us", "text": "hi"}));
        assert!(args.get("mentions").is_none());

        let (_, args) = validated_args("whatsrust_fetch_status", json!({"job_id": null, "active": null})).unwrap();
        assert_eq!(args, json!({}));
    }

    #[test]
    fn test_validated_args_null_required_is_reported_as_required_not_as_a_type_error() {
        assert_eq!(
            tool_error("whatsrust_send", json!({"jid": null, "text": "hi"})),
            "whatsrust_send: invalid argument(s): jid is required. Nothing was sent."
        );
    }

    #[test]
    fn test_validated_args_integral_float_is_normalised_to_an_integer() {
        let (_, args) = validated_args("whatsrust_history", json!({"jid": "1@g.us", "limit": 5.0, "before": -3.0})).unwrap();
        assert_eq!(args, json!({"jid": "1@g.us", "limit": 5, "before": -3}));
        assert_eq!(args["limit"].as_i64(), Some(5), "5.0 must come back as the integer 5");
        assert!(args["before"].is_i64());
    }

    #[test]
    fn test_validated_args_integer_range_is_i64() {
        let history = |limit: Value| validated("whatsrust_history", json!({"jid": "1@g.us", "limit": limit}));
        let rejected = Err(ArgsError::Tool(
            "whatsrust_history: invalid argument(s): limit must be an integer (64-bit). Nothing was sent.".to_string(),
        ));

        // Accepted: the i64 extremes, as integers and as exactly representable floats.
        assert_eq!(history(json!(i64::MAX)).map(|a| a["limit"].clone()), Ok(json!(i64::MAX)));
        assert_eq!(history(json!(i64::MIN)).map(|a| a["limit"].clone()), Ok(json!(i64::MIN)));
        assert_eq!(history(json!(-9_223_372_036_854_775_808.0_f64)).map(|a| a["limit"].clone()), Ok(json!(i64::MIN)));

        // Rejected: fractional floats, u64 above i64::MAX, and floats outside the i64 range.
        assert_eq!(history(json!(5.5)), rejected);
        assert_eq!(history(json!(-0.5)), rejected);
        assert_eq!(history(json!(u64::MAX)), rejected);
        assert_eq!(history(json!(i64::MAX as u64 + 1)), rejected);
        assert_eq!(history(json!(1e19)), rejected);
        assert_eq!(history(json!(9_223_372_036_854_775_808.0_f64)), rejected);
        assert_eq!(history(json!(-1e19)), rejected);
    }

    #[test]
    fn test_validated_args_number_accepts_integers_and_floats_unchanged() {
        let ok = json!({"jid": "1@g.us", "lat": 1, "lon": 1.5});
        assert_eq!(validated("whatsrust_location", ok.clone()), Ok(ok));
        let ok = json!({"jid": "1@g.us", "lat": -31.25, "lon": 34});
        assert_eq!(validated("whatsrust_location", ok.clone()), Ok(ok));
    }

    #[test]
    fn test_check_type_covers_each_json_type_and_ignores_unknown_ones() {
        let cases: [(&str, Value, Value); 5] = [
            ("string", json!("s"), json!(1)),
            ("boolean", json!(false), json!("false")),
            ("array", json!([]), json!({})),
            ("object", json!({}), json!([])),
            ("number", json!(1.5), json!("1.5")),
        ];
        for (ty, good, bad) in cases {
            assert_eq!(check_type(ty, &mut good.clone()), Ok(()), "{ty} accepts {good}");
            assert!(check_type(ty, &mut bad.clone()).is_err(), "{ty} rejects {bad}");
        }
        assert_eq!(check_type("integer", &mut json!(3)), Ok(()));
        assert_eq!(check_type("integer", &mut json!("3")), Err("an integer (64-bit)"));
        assert_eq!(check_type("object", &mut json!(1)), Err("an object"));
        assert_eq!(check_type("array", &mut json!(1)), Err("an array"));
        assert_eq!(check_type("mystery", &mut json!(1)), Ok(()), "an unrecognised type is not checked");
    }
}
