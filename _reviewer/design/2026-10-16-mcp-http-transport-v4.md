# Design Review — MCP Streamable HTTP transport (F3, ADR 0045) — v4 (follow-up)

**Date:** 2026-10-16  
**Artifact:** `docs/plans/2026-10-15-mcp-http-transport-design.md` + `docs/adr/0045-mcp-streamable-http-transport-in-daemon.md` (Proposed, revised v4: dedicated `/mcp` listener)  
**Reviewer:** cold `design-reviewer` subagent (independent; not given the prior reviews)  
**Prior reviews:** [v1](2026-10-15-mcp-http-transport.md) (Needs rework) · [v2](2026-10-15-mcp-http-transport-v2.md) (Approve with changes, 1 blocking) · [v3](2026-10-16-mcp-http-transport-v3.md) (Approve with changes, 0 blocking, 5 major)  
**Spec reference given:** `tmp/mcp-spec-excerpts-2026-07-28.md` — a local, uncommitted file of verbatim excerpts from modelcontextprotocol.io (line numbers cited below refer to it); (incl. 2025-06-18 / 2025-11-25 changelogs)  
**Owner constraints given as fixed:** 2026-07-28 + 2025-11-25/2025-06-18, legacy first, no batching; separate opt-in `/mcp` listener with its own always-required token and shared allowlists, REST/CLI/stdio unchanged; config violations exit (separate release); `destructiveHint: true` for remote-state tools; `ApiResponse` refactor; `whatsrust_send` trimmed.  
**Note:** the report below is verbatim.

---

# Design review: MCP Streamable HTTP endpoint (F3, ADR 0045, design v4)

**Verdict:** Approve with changes. I found no Blocking defect. Eight Majors need decisions or fixes before the listener becomes bindable (Phase 2) or `tools/call` ships (Phase 3).

## Summary

The core architecture is sound. It uses a separate `/mcp`-only listener, a separate token, in-process dispatch through the REST handlers, and the gate and token check before the body is buffered. The code references I checked are accurate. The risk sits in these places:
- The new listener has no specified bounds on unauthenticated traffic.
- The timer arithmetic undercuts the ADR 0047 guarantee that the design relies on.
- Several claims are false or contradictory: the integer rule, "unset = disabled", and the `ApiResponse` duplicate.
- The shared bind address and allowlists re-couple the two listeners.
- The design changes stdio behaviour, against constraint 2.

One-way doors to scrutinise: the env-var names and default port, the tool-schema contract exposed to remote clients, the refusal rules, and the meaning given to the annotation hints.

## What's solid

- **Listener isolation.** It answers the confused-deputy problem structurally. `path` is read only in `handle_media` (`api.rs:1422`); the `status_*` handlers have no path mode (`api.rs:1574-1627`). `validated_args` plus `loopback_file_access=false` is a real double defence.
- **Head/body split.** It closes the "gate runs after the body is buffered" gap that ADR 0044 recorded as "Not done".
- **Era detection.** "`_meta` alone is not a modern signal" is correct. Header/body mismatch, `Mcp-Name` base64 decoding and 404 `-32601` for unknown modern methods match the excerpts.
- **Legacy handling.** Stateless legacy with 405 for GET and DELETE matches the excerpts. Rejecting batches is right for both supported legacy versions, since 2025-06-18 removed batching (excerpt line 469).
- **Task model.** "The deadline stops waiting, not the task" is correct. `Store::run` uses `spawn_blocking` (`storage.rs:1044-1056`), so a dropped future cannot cancel a DB write.
- **Threat model.** It is honest that content exfiltration is not blocked.
- **Golden test.** Comparing real results is better than the earlier tautological mapping comparison.
- **Verified references.**
  - Counts: 34 tools mapping to 33 paths.
  - `api.rs`: lines 245, 346, 1760, 1827 and 1839.
  - `mcp.rs`: lines 88, 309 and 331.
  - `bridge.rs`: lines 1866-1868.
  - Library: the 75 s IQ default at `request.rs:177`.

## Findings

### Blocking
None.

### Major

**M1. Unauthenticated resource bounds on the new listener are unspecified or absent.**
- **Statement.**
  - §6 acquires the 8-permit semaphore only after head read, gate, token and routing. No outer connection cap is given for the MCP accept loop.
  - The head read has only a 10 s per-read timeout, and the design declines a total deadline.
  - The "lingering close" drain has no time or size bound.
  - File descriptors are process-wide, often 256 by default on macOS. Exhaustion therefore hits REST, SQLite and the WhatsApp socket, not just `/mcp`.
  - The claim that the 5 s permit wait "prevents a client trickling bytes from holding a permit indefinitely" confuses acquire-wait with hold-time. A token holder trickling a body at 1 byte per 9 s holds a permit indefinitely.
- **Evidence.** Design lines 440-446, 531-535 and 557. `api.rs:407` and `415` (per-read timeout only). `api.rs:1790-1807` (REST caps at accept).
- **Direction.**
  - Give the MCP accept loop its own outer cap.
  - Add a wall-clock deadline for the head and a length-scaled deadline for the body.
  - Bound the drain, for example ≤64 KiB or ≤1 s.
  - Order the steps: route, then permit, then 100-continue, then body.
  - Consider dropping `Expect: 100-continue`; the MCP SDKs I know of don't send it.

**M2. The 40 s deadline does not preserve its 5 s margin.**
- **Statement.**
  - The justification is that the daemon's structured timeout must beat the client's 45 s read timeout.
  - The 40 s timer starts at dispatch, after the head read, up to 5 s of permit wait, the body read and the JSON parse.
  - The client's 45 s starts when it sends.
  - With a 5 s permit wait, the response lands at about 45 s or later. That is exactly the "client gave up before the `job_id` arrived" failure ADR 0047 exists to prevent.
  - The "third parties must use ≥50 s" advice papers over a wrong clock origin.
- **Evidence.** Design lines 533 and 546-551. `bridge.rs:55-64`.
- **Direction.** Use one absolute deadline from request arrival, with the tool wait set to deadline minus now. Permit wait and body read then count against it.

**M3. The ADR 0047 amendment cannot deliver "a `job_id` exists whenever a job does".**
- **Statement.**
  - `enqueue_and_wait` already captures `job_id` before the timer (`bridge.rs:1866` precedes `1868`), so "capture before the timer" is a no-op.
  - "Move the enqueue inside the timer" is worse. A timed-out `spawn_blocking` closure still commits, so a job can exist that nobody learns the id of.
  - The stall source is the single `Mutex<Connection>`. It is held across long operations such as `purge_embeddings` (`storage.rs:1884+`). `busy_timeout=5000` does not bound that wait. The design's "SQLite contention" is the wrong cause.
  - The real defect is two stacked timers: outer 40 s versus enqueue plus 30 s.
- **Direction.**
  - Remove the outer timer on the enqueue-and-wait routes and let the bridge enforce one deadline.
  - Alternatively, run the enqueue as a spawned task that publishes `job_id` through a shared cell the timeout path can read.
  - Ship this separately from the config-refusal release. Constraint 3 wants that release to carry the refusal rules only.

**M4. The integer rule rests on a false claim.**
- **Statement.**
  - Design §4 and Phase 1 assert that `20.0.as_i64()` succeeds. In serde_json, `Number::as_i64` returns `None` for `N::Float` (`serde_json-1.0.151/src/number.rs:132-144`).
  - Mappings such as history's `limit` would silently replace a present `5.0` with the default 20. That breaks the design's own rule of "defaults only on absent keys".
  - REST serde rejects floats for `i64` and `u32` fields, so body-passthrough tools get a 400 for a value `validated_args` accepted.
- **Evidence.** Design lines 286 and 297-302. The planned test at line 873 would fail.
- **Direction.** Define integer as `is_i64() || is_u64()` and reject `5.0` with a field-named error. Or have `validated_args` normalise integral floats to integers in the args it returns.

**M5. Shared bind address and allowlists undercut the isolation rationale and couple the listeners.**
- **Statement.**
  - §4 justifies separate tokens with "MCP clients are often remote; REST is often local-only". One `WHATSRUST_BIND`/`ALLOW_REMOTE` makes remote MCP mean remote REST, with the bearer token in cleartext since there is no TLS.
  - The design itself says a proxy is the expected off-machine topology, so remote bind is unnecessary.
  - The chosen proxy recipe sets `ALLOWED_HOSTS` to the public name. By constraint 3 that forces `WHATSRUST_API_TOKEN` on a loopback-only REST listener.
  - The CLI and stdio MCP then also need that token in their process environment. `.env` is not loaded in CLI mode (`main.rs:239-243`; clients read the env at `api.rs:1876` and `mcp.rs:424`).
  - REST also starts accepting the proxy's Host.
  - A proxy that rewrites `Host` to `127.0.0.1` needs none of this, and the design dismissed it as "less simple".
- **Direction.**
  - Add `WHATSRUST_MCP_BIND`, loopback-only by default, or do not support remote bind for MCP in v1.
  - Make Host-rewrite the documented recipe.
  - Add an ADR 0044 amendment. Rule 4 contradicts its statement that reverse proxies opt in via allowlists without a token (ADR 0044 line 85-97).

**M6. Startup and bind semantics are underspecified and partly wrong.**
- **Statement.**
  - **Port parsing.**
    - Design line 887 says `WHATSRUST_PORT` "0 or unset = disabled". `get_port()` returns 7270 when unset or unparseable (`main.rs:170-176`).
    - The MCP port is described as "default 7271" and also "unset = disabled".
    - If an implementer copies `get_port()`, MCP is on by default. Every upgrade without a token would then refuse to start.
  - **Bind failure.**
    - "Partial bind continues" and "neither binds exits" lack a mechanism. `serve()` returns `()` from a spawned task after logging (`api.rs:1777-1786`, `main.rs:427-433`).
    - An explicitly opted-in MCP listener that fails to bind (port in use) leaves a running daemon with only a log line. That contradicts the fail-closed spirit of constraint 3.
    - A headless config (REST port 0, MCP unset) is accepted today and now "exits". That is an unlisted breaking change.
  - **Port collision.** No rule covers MCP port equal to REST port.
- **Direction.**
  - Parse the MCP port as `Option`, with unset meaning off.
  - Pre-bind both listeners in `main()` before `WhatsAppBridge::start`.
  - Make an explicitly enabled listener that cannot bind a fatal exit with the fix.
  - Keep headless working.
  - Add the port-equality rule.

**M7. The starvation fix is deferred, yet Phase 3 ships `tools/call`.**
- **Statement.**
  - 8 permits are shared by 30 s sync sends, 75 s library calls and DB-mutex stalls (for example a purge).
  - Permits are held after the response, and LLM clients retry.
  - Parallel tool calls can exhaust the pool, which locks out `status`, the tool the operator uses to diagnose.
  - The design says "Phase 3 or 4" and "decision made in Phase 4".
  - The golden parity test is also Phase 4, after all 34 tools are live.
- **Evidence.** Design lines 532 and 971, and Phase 3 exit criteria (line 976).
- **Direction.**
  - Decide now. Two lanes work: a local-read lane with reserved permits and a write/network lane capped below 8.
  - Move the golden test and the lane decision into the Phase 3 exit.
  - Consider a separate 2-permit lane for bodies over 1 MiB. That cuts the ~380 MiB peak (8 × ~48 MiB) on a daemon meant to be lean.

**M8. The design deviates from constraint 2, and one choice contradicts the excerpts.**
- **Statement.**
  - Constraint 2 says stdio MCP stays unchanged. Annotations in `tools/list` follow from constraints 4 and 6.
  - These stdio changes are not required: non-object `arguments` and unknown tool move from `isError` to `-32602`, and type/required checks are added. The cost is an amendment to accepted ADR 0046 (lines 39-44) and a rewrite of the test at `mcp.rs:644`.
  - "Non-object arguments → `-32602`, matching the spec" is not supported by the excerpts. Excerpt lines 124-128 list "Input validation errors" under tool execution errors. The 2025-11-25 changelog item 5 says they should be `isError` so the model can self-correct.
  - "Byte-identical" stdio/HTTP results cannot hold:
    - stdio omits `isError` on success (`mcp.rs:409`).
    - Modern results add `resultType`.
    - stdio decodes the body with strict UTF-8 (`mcp.rs:484`).
    - Tool timeouts differ (40 s versus 45 s).
- **Direction.**
  - Leave stdio as-is apart from annotations.
  - For HTTP, return unknown tool as `-32602` (excerpt line 126) and non-object arguments as `isError`, unless the owner decides otherwise.
  - Make the golden test compare normalised results.

### Minor

**m1. The annotation table contradicts the code, the spec definition and the roadmap.**
- Constraint 4 makes 26 of 34 tools carry an identical `destructiveHint: true`. That includes `revoke`, `delete_chat` and `status_*`, as well as `star`, `mute`, `typing` and `mark_read`. Clients cannot tell irreversible from trivial, and the hint invites approval fatigue.
- The design's `false` criterion is "local-only". The spec says false means "only additive updates" (excerpt line 301). `purge_embeddings` deletes data yet is marked `destructiveHint: false`, `openWorldHint: false`.
- `fetch_cancel` is marked remote-changing, but `handle_history_fetch_cancel` only updates the local store (`api.rs:1370`).
- `fetch_history` is "local state only" in §9 and "network-touching" in §6. FEATURES.md F5 says it triggers "network fetches with ban-risk pacing" (line 90).
- F5 plans to filter on these hints (FEATURES.md 92-94). Add a server-side access class to `McpToolMeta` rather than overloading advisory hints.

**m2. Phase 0a is not "pure/behaviour-neutral".**
- It contains REST-visible changes:
  - The `json_err` code for 415 changes from `internal_error` to `unsupported_media_type`.
  - Chunked bodies get 411.
  - Gate-before-body changes error ordering.
  - It adds 100-continue and lingering close.
- Its stale instruction says to "remove the duplicate `ApiResponse` at ~734". No `ApiResponse` exists in the tree.
- Constraint 3 wants the refusal release separate from pure refactors.
- Direction: add a byte-for-byte REST golden test before and after the refactor, and move the behaviour changes into a labelled change.

**m3. Protocol and document inconsistencies.**
- (a) Phase 3 legacy `tools/list` carries modern-only `ttlMs`/`cacheScope` (design line 964) while §5 says legacy results are plain.
- (b) The handshake test (line 874) accepts a counter-offer for a 2025-06-18 `initialize`, but the rule it cites is MUST echo a supported version.
- (c) Define what happens for a request with no `id` and a non-notification method, and for `notifications/*` with an `id`. stdio treats a null id as a notification (`mcp.rs:42`).
- (d) The pipeline puts the token check before routing, so unauthenticated `GET /.well-known/oauth-protected-resource*` returns 401, not the 404 claimed (lines 125 and 683). Choose one on purpose.
- (e) §8 lists six refusal rules and §13 lists five, with different conditions. §8's "both tokens unset" would pass a config that §13 refuses. Keep §13, which matches constraint 3.
- (f) The token-strength rule checks length but its message claims entropy.
- (g) 503 for capacity uses `-32600`. Use an implementation-defined code.
- (h) `/mcp/` and PUT/HEAD/OPTIONS on `/mcp` are undefined.
- (i) The permit step says `try_acquire_owned()` wrapped in a timeout, which is contradictory (it is non-blocking).

**m4. Operability and simplification.**
- The four counters cannot show the page-worthy state: permits held by tasks that outlived their response, timeouts, and 503s. Add gauges for in-flight and available permits, and counters for timeouts and 503s.
- Kill switch and token rotation need a daemon restart, which drops the WhatsApp session briefly. Consider accepting two tokens for overlap.
- The POST-class and GET-class timeout classification (§6, §10) is already inconsistent. One message ("timed out after N s; the operation may still complete; check before retrying") is accurate for every class and removes the table.

**m5. Documentation drift.**
- ADR 0045 §10 bundles the token policy into "Phase 0 — ship independently".
- ADR 0045 §6 still describes the tautological parity test.
- FEATURES.md F3 still says "on the existing API server".

### Question

**Q1. Is it intended that F3 ships a network-capable, full-access credential before F5?**
- The threat model accepts unrestricted send, delete and status-post for a prompt-injected agent.
- The registry already has `readOnlyHint`. A startup flag filtering `tools/list` and `tools/call` to read-only tools would be small. Would a deferred F5 be acceptable without it?

**Q2. What does a modern-first client do on `-32022` listing only legacy versions?**
- Phase 3 staging relies on it falling back to `initialize`. The excerpts don't cover this.
- Test with a modern-first SDK before shipping Phase 3.

**Q3. Should `instructions` and the `history`/`search` descriptions label returned message text as untrusted?**
- This is the stated top threat (prompt injection) and a cheap soft mitigation. The design does not mention it.

## Open questions

1. Does the MCP accept loop have an outer cap, and what are the deadline values (M1)?
2. Is a headless daemon with REST port 0 and MCP unset still supported (M6)?
3. Is remote bind for MCP without a proxy intended (M5)?
4. The excerpts don't cover these points the design relies on:
   - the HTTP status for modern JSON-RPC errors other than `-32601`, `-32020` and `-32022`
   - how legacy clients treat 404
   - whether 2025-06-18 and 2025-11-25 differ in ways the "selected" changelog lines omit
   - how clients behave on a 401 for a static bearer token

## Assumptions

- I treated the working tree as the baseline. `git status` shows modified `api.rs`, `bridge.rs`, `mcp.rs` and `main.rs`, and I could not see the diff.
- `serde_json` has default features. `Cargo.toml` says `serde_json = "1"`. Even with `arbitrary_precision`, `as_i64` parses the number text, so `"20.0"` still fails.
- I did not test real SDK clients, nginx or Caddy.
- The golden test needs `build_test_bridge_*`, which is private to `bridge.rs`'s test module. I assumed it will be exposed to other test modules.

## Relevant files

- `/Users/gila/prv/whatsrust/docs/plans/2026-10-15-mcp-http-transport-design.md`
- `/Users/gila/prv/whatsrust/docs/adr/0045-mcp-streamable-http-transport-in-daemon.md`
- `/Users/gila/prv/whatsrust/src/api.rs`, lines 245, 346, 400-421, 1397-1442, 1760-1867 and 1364-1375
- `/Users/gila/prv/whatsrust/src/mcp.rs`, lines 42, 309-324, 331-412 and 644
- `/Users/gila/prv/whatsrust/src/main.rs`, lines 170-176, 239-258 and 427-433
- `/Users/gila/prv/whatsrust/src/bridge.rs`, lines 55-64 and 1863-1914
- `/Users/gila/prv/whatsrust/src/storage.rs`, lines 678, 1044-1056 and 1884+
- `/Users/gila/prv/whatsrust/docs/plans/FEATURES.md`, lines 42-97
- `/Users/gila/prv/whatsrust/docs/adr/0044-api-origin-host-content-type-hardening.md`, `0046-…`, `0047-…`
- `/Users/gila/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde_json-1.0.151/src/number.rs`