# 0046. MCP forwards only declared tool arguments

**Status:** Accepted
**Date:** 2026-10-15
**Revised:** 2026-10-15 (drop → reject)

## Context

The stdio MCP server (`src/mcp.rs`) proxies tool calls to the REST API (`src/api.rs`) by forwarding
the tool's `arguments` as JSON to the corresponding HTTP endpoint. Some REST endpoints accept
parameters that are not declared in the MCP tool schemas — notably `path`, which the media handlers
(`/api/image`, `/api/status-image`, `/api/status-video`) accept alongside `data`. When `path` is
present, `handle_media` reads the file from the local filesystem and sends it to the given JID.

Before this change, MCP forwarded arguments verbatim. An LLM agent (possibly prompt-injected by
third-party message text it read via `whatsrust_history`/`whatsrust_search`) could call
`whatsrust_image` with an undeclared `path` argument. The MCP server proxies to the daemon on a
loopback bind and sends `WHATSRUST_API_TOKEN` if configured, so the attack works with or without a
token. The REST handler reads the file (e.g. SSH keys, `whatsapp.db`) and sends it to an
attacker-controlled JID.

The MCP tool schemas never declared `path` — only `data` (base64), `mime`, and `caption`. The REST
API accepts both modes to support the CLI: the `image <jid> <path>` command uses the local-file
mode. But accepting an argument the schema does not declare violates the least-privilege principle:
the MCP client believes it is invoking a base64-only tool, yet the server reads arbitrary files.

A second issue was found during the audit: the `whatsrust_contact` schema declared `vcard`
(requiring the caller to build + sanitize it), but the REST handler (`ContactReq`) expects `phone`
and builds the vCard itself. The schema was never usable.

## Decision

The MCP `call_tool` function validates and rejects calls with undeclared arguments at a single choke
point, before any HTTP request. Every tool below only ever sees schema-declared arguments (or an
error has already been returned).

`tool_def(name, description, input_schema)` sets `"additionalProperties": false` on every tool
schema. `validated_args(name, args)` checks the call's `arguments` against the declared properties:
- Undeclared top-level keys → `Err` with message `toolname: unknown argument(s): x, y. Allowed arguments: a, b. Nothing was sent.` (or `(none)` for no-param tools)
- Absent or `null` arguments → `Ok({})`
- Non-object arguments → `Err` with `"arguments must be a JSON object"`
- Unknown tool → `Ok({})` (dispatch reports the unknown tool)

`call_tool` wraps the result in `tool_error(message)` (`isError: true`) on validation failure, so
nothing is sent. Helper functions: `unknown_keys`, `unknown_args_message`.

Rejecting instead of dropping prevents silent misbehaviour: an agent passing the now-removed
`schedule_at` to `whatsrust_send` would get an immediate, irreversible send with silent drop,
instead of an error. Rejection also matches the MCP spec's "validate all tool inputs" and is
consistent with the advertised `additionalProperties: false` schemas.

The REST API keeps both `data` and `path` modes (the CLI `image <jid> <path>` command uses the
local-file mode), but the `path` field is now unreachable through MCP.

The `whatsrust_contact` schema is corrected: it now declares `phone` (required: jid, name, phone)
instead of `vcard`, matching what `ContactReq` expects.

The `whatsrust_send` schema was trimmed: it no longer declares `link_preview` or `schedule_at`
(description now "Send a text message (optionally with @mentions)"), because the tool always calls
`/api/send?sync=true`, which rejects both with 400 — they never worked via MCP. REST and CLI keep
both parameters.

Two guard tests enforce the filter invariant (hand-maintained mirrors of REST structs in the mcp.rs
test module, updated with those structs):
- `test_no_tool_schema_declares_a_path_property` — rejects any schema that declares `path`.
- `test_schemas_declare_every_field_the_rest_handler_requires` — checks a table
  (`REST_REQUIRED_FIELDS: &[(&str, &[&str])]`) that maps tool names to the fields their REST struct
  requires (non-`Option`, no `#[serde(default)]`), e.g. `("whatsrust_contact", &["jid", "name",
  "phone"])`. If a REST struct grows a required field that the schema doesn't declare, the guard
  fails.

**Rejected:**
- Dropping undeclared keys silently: with the `whatsrust_send` schema trim, an agent passing the
  now-undeclared `schedule_at` would get an immediate, irreversible send instead of an error;
  silent drop also contradicts the `additionalProperties: false` schemas and the spec's "validate
  all tool inputs".
- Removing `path` mode from the REST API: breaks the CLI local-file workflow.
- Restricting `path` to an allowlisted directory: the MCP surface doesn't need path mode at all, and
  the CLI is a trusted local user; filtering at the MCP layer is simpler and sufficient.

## Consequences

**Positive:** the local-file exfiltration hole is closed; MCP tool calls cannot read arbitrary
filesystem paths; the `whatsrust_contact` tool is now usable.

**Negative:** the guard tests are hand-maintained — when a REST struct changes, the
`REST_REQUIRED_FIELDS` table must be updated with it; a future HTTP-mode MCP endpoint (ADR 0045)
must apply the same validation or introduce its own guards; an agent written against an older schema
that declares a since-removed parameter (like `schedule_at` on `whatsrust_send`) will fail the call
until it's updated (but this is correct behaviour: the parameter never worked).
