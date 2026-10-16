# Design Review — MCP Streamable HTTP transport (F3, ADR 0045)

**Date:** 2026-10-15  
**Artifact:** `docs/plans/2026-10-15-mcp-http-transport-design.md` + `docs/adr/0045-mcp-streamable-http-transport-in-daemon.md` (Proposed)  
**Reviewer:** cold `design-reviewer` subagent (first review; no prior reviews of this design)  
**Spec reference given:** `tmp/mcp-spec-excerpts-2026-07-28.md` — a local, uncommitted file of verbatim excerpts from modelcontextprotocol.io (line numbers cited below refer to it); (verbatim excerpts)

---

**Verdict: Needs rework.** The architecture is sound, but the dispatch seam is not implementable as written, several premises are factually wrong, and the security and spec-conformance sections have gaps. The fixes mostly leave the architecture alone.

## Summary

The design adds a stateless `POST /mcp` endpoint to the daemon's hand-rolled HTTP server. It shares one tool registry and one tool-to-REST mapping with the stdio proxy, and executes `tools/call` in-process through the existing REST handlers. The shape is right: a stateless endpoint for a spec that removed sessions, with validation kept in the REST layer. But the central mechanism, in-process REST execution, was specified without reading how `handle_request` actually works. Five of the 34 tools would break, and the stated tests would not catch it. The MVP also ignores the agent and prompt-injection threat that dominates for an LLM-facing endpoint.

## What's solid

- **Gate reuse.** I checked `serve()` (`src/api.rs:1800-1828`). `gate_request` and the token check run before every route, so `/mcp` inherits them for free. Running them once on the outer request is the correct model.
- **Spec fit.** GET and DELETE returning 405 matches both spec revisions. Dropping sessions is justified because no tool is stateful. JSON-only responses are explicitly allowed by the excerpts.
- **Validation stays in one place.** A shared pure mapping function with the REST layer as the single validator is better than reimplementing 34 tools in the MCP layer.
- **Pacing claim holds.** `send_message_with_id_mentioned` goes through `enqueue_and_wait` to the outbound worker, which calls `send_pacer.wait_turn()` (`bridge.rs:1824`, `bridge.rs:3046`). Sends are paced on this path.
- **Shared connection cap exists.** `conn_sem` is 64 connections (`api.rs:1774`). The design said it was "assumed to exist".

## Findings

### Blocking

**B1. The in-process dispatch contract does not match the code, and as specified 5 of 34 tools break.**

- **Missing API.**
  - `handle_request(bridge, &HttpRequest, is_loopback)` is `async` and returns `Vec<u8>`, a fully serialized HTTP response (`api.rs:496`).
  - `handle_request_inner` does not exist, and there is no `.status` or `.body` to read.
  - The design's `handle_mcp_call_tool` is sync, but every handler is async.
  - Every handler builds its result via `json_ok` and `json_err`, which emit raw HTTP bytes.
  - The only existing bytes-to-status parser is `parse_cli_response` (`api.rs:1940`).
- **Path and query bug.**
  - The router matches `req.path` exactly, so a path containing a query string falls through to 404.
  - Handlers read `req.query_get(..)`, for example `sync` at `api.rs:509`.
  - The design puts `?sync=true` or `?jid=..` into `path` and sets `query: vec![]` with the comment "Already in path".
  - So `whatsrust_send`, `group_info`, `history`, `search` and `fetch_status` all return 404 (reported as `isError`).
  - If someone "fixes" this by stripping the query, `send` silently loses `sync=true`. It flips from "wait and return the WhatsApp message id" to "enqueue and return a job_id", and the other four lose their parameters.
- **Mapping is not trivial.**
  - The design says the mapping is about five lines per tool.
  - In `mcp.rs:279-342`, eight tools do real argument work: default `limit=20`, default `active=true`, default `mode=all`, `urlencode`, and `fetch_history` rebuilding its body from three fields.
  - A `path: String` with an embedded query is the wrong contract. Use `RestRequest { method, path, query: Vec<(String,String)>, body }`. Stdio encodes and joins it, and in-process passes the pairs through.
- **The test plan cannot catch this.**
  - The only success-path `tools/call` test is `whatsrust_status`, which has no query string.
  - Direction: make handlers return a structured `ApiResponse {status, content_type, body}`, with `serve()` serializing it. Parsing the bytes is an acceptable stopgap.
  - Add one table-driven test over all registry tools. It should assert that the (method, path, query, body) the router sees in-process equals what stdio puts on the wire.

### Major

**M1. The `/mcp` body limit is 1 MiB, not "50 MB", so media tools fail.**
- `read_request` allows 70 MiB only for path prefixes like `/api/image` and `/api/status-image`. Everything else gets 1 MiB (`api.rs:443-456`).
- `/mcp` gets 1 MiB. `whatsrust_image`, `status_image` and `status_video` fail for any payload over roughly 768 KB raw.
- The failure is a bare, non-JSON-RPC `400 bad request` from `serve()`.
- Stdio hits `/api/image` with a 70 MiB allowance, so "stdio and HTTP behave identically" is false.
- Raising the cap for `/mcp` has a cost. The body is read before the gate and token check (`api.rs:1800` versus `api.rs:1811`), so 64 connections times 70 MiB is about 4.5 GB buffered before authentication.
- The `Value`, `args.clone()`, `to_vec`, serde and base64-decode chain also multiplies memory per call.
- Direction:
  - Split `read_request` into head and body phases so the gate and token run before the body for `/mcp`. ADR 0044 explicitly deferred this.
  - Move the body into the synthesized request instead of cloning and re-serializing it.
  - Or cap MCP media explicitly and document it.

**M2. LLM-supplied arguments can read arbitrary local files, and `is_loopback` is not what the design thinks it is.**
- `handle_media` accepts `path` when `is_loopback` is true (`api.rs:1406-1411`). `read_file_for_media` has no path restriction (`api.rs:1026`).
- The registry schemas do not advertise `path`, but the mapping forwards `args.clone()` verbatim.
- `is_loopback` is `is_loopback_bind(&bind)` (`api.rs:1772`), a daemon-wide constant. It is not a per-request or per-peer property, which is how the design reads it.
- A prompt-injected agent can call `whatsrust_image {jid: <attacker>, path: "~/.ssh/id_ed25519"}` and send any file up to 50 MiB, including the Signal-key DB, over WhatsApp.
- This already exists via stdio. But the design cements it as a feature ("must pass the outer request's `is_loopback`"), and it must not claim "no new attack surface".
- In `handle_request`, `is_loopback` is consumed only by the media arms (`api.rs:515-525`).
- Direction: pass `false` for MCP in-process calls, and/or have the mapping strip keys not in the tool's `inputSchema`.

**M3. The threat model omits the agent and prompt-injection threat.**
- The table lists CSRF, rebinding, token leakage, and a "malicious local client".
- The realistic threat is an injected agent. `history` and `search` return arbitrary third-party message text into an LLM. The same server exposes `send`, `revoke`, `delete_chat`, status posts, and the M2 file read.
- The excerpts list "Validate all tool inputs; Sanitize tool outputs; Rate limit" as server MUSTs. The design addresses none of the three.
- Annotations are advisory, so they are not a mitigation.
- Direction: add this threat explicitly. Apply the M2 fix now, and state what F5 will and will not cover.

**M4. Choosing the era from the body's `_meta` lets a request skip header validation.**
- A request with `MCP-Protocol-Version: 2026-07-28`, `Mcp-Method` and `Mcp-Name` headers but no `_meta` is classed Legacy and served unvalidated.
- That violates the excerpt's "header MUST match `_meta`", and it breaks the headers' purpose.
- The headers exist so that proxies and WAFs can route or authorize without parsing the body. Here `Mcp-Name: whatsrust_status` could front a body that calls `delete_chat`.
- Direction: use the header as the primary discriminator. Validate any `Mcp-*` header that is present regardless of era. Treat "header says 2026-07-28 but body has no `_meta`" as `-32020`.

**M5. The legacy path is the least specified, but it is the one real clients will hit.**
- The ADR itself says Claude Code will use the modern protocol only "with a future update", and the manual test uses Claude Code. The primary tested path is legacy.
- Gaps:
  - `initialize` version negotiation. The design says an unsupported version returns `-32022` "Both eras". For legacy `initialize`, the requested version is a proposal. The server should counter-offer, not error.
  - Listing `2024-11-05` as a supported Streamable-HTTP version is incoherent. Those clients use the deprecated HTTP+SSE transport, which this endpoint does not implement. Justifying it by "stdio advertises 2024-11-05" conflates two different things.
  - No decision on `ping`. Stdio does not handle it either.
  - No decision on a POSTed JSON-RPC batch or a posted response. The excerpts require a single request or notification.
  - A 404 for unknown methods in legacy. Many legacy clients read 404 as "endpoint or session gone". A 200 with `-32601`, as stdio does, is safer.
  - Response `id` echo is absent from the pseudocode.

**M6. The remote story contradicts the token policy.**
- The token is optional on loopback "because a loopback daemon is single-user". The design also recommends a reverse proxy for TLS.
- A proxy in front of a loopback-bound daemon with `ALLOWED_HOSTS` set and no token means anonymous remote access, with the M2 file read enabled.
- The `ALLOW_REMOTE` recipe (bind 0.0.0.0, bearer token) has no transport security and sends the token and message content in cleartext.
- "Startup aborts if the token is unset" is wrong. `serve()` logs an error and returns inside a spawned task (`api.rs:1754-1760`, `main.rs:428`). The daemon keeps running with no API at all.
- Direction: require a token whenever `WHATSRUST_ALLOWED_HOSTS` or `WHATSRUST_ALLOWED_ORIGINS` is non-empty. Document TLS as required for any non-local use. Fix the "aborts" wording.

**M7. A default-on endpoint ships knowingly without rate limiting, and shares the 64-connection pool.**
- The ADR itself says a client "can spam read requests until the daemon is overwhelmed or the database locks contend".
- `/mcp` calls compete with REST, CLI and health for the same 64-permit `conn_sem`. Excess connections get an immediate 503.
- A cheap in-MVP control would be a dedicated `/mcp` in-flight semaphore, for example 8. It does not need F5's per-token machinery.

**M8. The long-running tool risk is misdiagnosed, and it hides a duplicate-send hazard.**
- The "client 30 s timeout" attributed to `mcp.rs:360,383` is the stdio proxy's read timeout to the daemon, not the MCP client's.
- The real limit is the bridge's own 30 s wait in `enqueue_and_wait` (`bridge.rs:1829-1857`). On timeout it returns `send timed out (30s)` but does not cancel the queued job.
- So the message is still sent after an `isError` result. An agent that retries sends it twice.
- The proposed mitigation, `notifications/progress`, does not help, and the error carries no `job_id`.
- Direction: return the `job_id` and say "may still be delivered; do not retry". Correct the Risks section.

### Minor

- **m1. Factual errors in the doc.**
  - The tool count is 34 (34 `tool_def(` calls in `mcp.rs`, and 34 rows in the design's own table), not 35. This appears throughout the docs. The "returns 35 tools" test would fail on first run.
  - The table header says "30 endpoints", but there are 33 distinct paths.
  - The handler line counts (54 and 71 lines) are wrong: 40 and 61.
  - `validate_api_token`, `&state` and `handle_request_inner` do not exist.
  - The "~2100 lines of duplicated validation" strawman is inflated. `handle_jid_action` alone serves nine tools.
  - The error table says a gate rejection is a "JSON-RPC error", but `serve()` emits REST-shaped `json_err`. That is spec-compliant (MAY) but contradicts the doc.
- **m2. The registry has two parallel lists.** Tool metadata and the `match` in `map_tool_to_rest_request` can drift, which undermines "single source of truth". Hold the mapping in the tool entry and add a test that every tool maps. The row "malformed arguments return `-32602`" is dead, because REST returns 400 and `map_tool_to_rest_request` only errors on an unknown tool. The excerpts also say input validation errors are `isError`. Schema drift already exists: `whatsrust_send` advertises `schedule_at` and `link_preview`, which `sync=true` rejects with 400. Sharing the registry also adds annotations to stdio `tools/list`, so "no behaviour change" is inaccurate.
- **m3. Annotation table.**
  - `openWorldHint: true` on local-DB reads (`status`, `history`, `search`, `fetch_status`) contradicts the design's own definition.
  - `edit` overwrites content but is marked non-destructive.
  - `fetch_history` is marked idempotent, but each call enqueues a job and can return 429 (cooldown).
  - The hint semantics the doc quotes are not in the supplied excerpts, so I could not verify them.
- **m4. HTTP helpers.**
  - `http_response` has no reason phrases for 202, 405, 415 or 501 (`api.rs:104-115`).
  - `json_err` maps 405 and 415 to `"internal_error"` (`api.rs:150-161`).
  - A bodyless 202 helper is needed.
- **m5. Hand-rolled HTTP versus mainstream clients.** `read_request` supports `Content-Length` only. A chunked body yields an empty body and a misleading JSON parse error. Return 411 or 501 when `Transfer-Encoding` is present on `/mcp`, and state the assumption.
- **m6. Operability and tests.**
  - No logging, metrics or audit for MCP calls, on an endpoint that sends and deletes under the user's account.
  - Add at least a tool-name and outcome log line (no args) and counters in `status`.
  - Add a run against an official MCP SDK or inspector, not only curl and Claude Code.
  - Add concurrency and oversize-body tests.
- **m7. Default-on versus the F3 text and reversibility.**
  - `FEATURES.md` F3 says "opt-in alongside stdio", and the design drops this without noting the change.
  - A default-on endpoint is hard to turn into opt-in later.
  - `WHATSRUST_PORT=0` disables the whole API, including the stdio proxy.
  - A kill switch is cheap, and the ADR's workaround of "block it in a reverse proxy" is poor.
- **m8. ADR hygiene.** ADR 0045 duplicates the design doc, including pseudocode, and will rot. It contradicts itself: §4 says factoring out the mapping "eliminates duplication", while "Negative" says the mapping duplicates ~175 lines.

### Question

- **Q1. How does a modern client learn server info and capabilities?** Phase 2 says modern `initialize` "just returns an error", and no replacement is defined. The excerpts do not cover discovery, so I cannot judge it. If the 2026-07-28 spec requires a discovery RPC, the modern half of the MVP cannot connect. This is high impact.
- **Q2. Where do `-32022` and the `supported` payload shape come from?** The excerpts name `UnsupportedProtocolVersionError` but give no code or shape. Only `-32020` is stated.
- **Q3. What do the target clients do on a 401 without `WWW-Authenticate`?** Some may try OAuth discovery. With a token set, every path returns 401, including discovery paths.

### Note

- **Alternatives to record in the ADR.**
  - A thin `whatsrust mcp --http` listener reusing today's proxy, with no daemon-side parser, no registry refactor, a separate kill switch and a separate token. It is the main road not taken.
  - Extracting a typed core from the handlers, so REST and MCP both call functions returning structured results. This also fixes B1 and gives F5 one enforcement point.
- **F4/F5 compatibility.** `outer_is_loopback: bool` is too thin a context. Pass a `CallContext` (loopback, principal or profile, later account) so F5 per-token profiles and F4 account selection are not a signature rewrite. The claim that the registry is "agnostic" is currently vacuous.
- **Phase order.** If legacy clients are the real target, implement them first. Phase 2 (modern) is unverifiable until Q1 is answered.

## Open questions

1. Q1 to Q3 above.
2. Is local-file `path` mode meant to be reachable from MCP callers? If not, adopt the M2 fix.
3. What is the intended primary client and protocol version for the first release, and how will it be validated?
4. What is the intended remote deployment (TLS, token, proxy), given M6?
5. What server-side deadline applies to in-process tool execution? None is specified.

## Assumptions

- I treated `tmp/mcp-spec-excerpts-2026-07-28.md` as the only spec text. It lacks discovery and capabilities, error code numbers beyond `-32020`, and the annotation semantics.
- I assumed mainstream MCP clients send `Content-Length` bodies and no `Expect` header.
- I did not run the code or tests. All code claims come from reading the source.

## Relevant files

- `/Users/gila/prv/whatsrust/docs/plans/2026-10-15-mcp-http-transport-design.md`
- `/Users/gila/prv/whatsrust/docs/adr/0045-mcp-streamable-http-transport-in-daemon.md`
- `/Users/gila/prv/whatsrust/src/api.rs`: `read_request` 384, `handle_request` 496, `handle_media` 1381, `serve` 1744, `parse_cli_response` 1940
- `/Users/gila/prv/whatsrust/src/mcp.rs`: `call_tool` 275-350, timeouts 360 and 383
- `/Users/gila/prv/whatsrust/src/bridge.rs`: `enqueue_and_wait` 1824
- `/Users/gila/prv/whatsrust/src/main.rs`: API spawn at 425-431
- `/Users/gila/prv/whatsrust/docs/plans/FEATURES.md`: F3 to F5
