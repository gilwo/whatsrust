# Design Review — MCP Streamable HTTP transport (F3, ADR 0045) — v2 (follow-up)

**Date:** 2026-10-15  
**Artifact:** `docs/plans/2026-10-15-mcp-http-transport-design.md` + `docs/adr/0045-mcp-streamable-http-transport-in-daemon.md` (Proposed, revised v2)  
**Reviewer:** cold `design-reviewer` subagent (independent; not given the prior review)  
**Prior review:** [2026-10-15-mcp-http-transport.md](2026-10-15-mcp-http-transport.md) (v1 — Needs rework)  
**Spec reference given:** `tmp/mcp-spec-excerpts-2026-07-28.md` — a local, uncommitted file of verbatim excerpts from modelcontextprotocol.io (line numbers cited below refer to it); (verbatim excerpts, extended 2026-10-15 with the legacy-era and schema sections)  
**Owner constraints given as fixed:** both eras, legacy first; opt-in env var; token required when allowlists are set (REST + `/mcp`); `ApiResponse` refactor with in-process dispatch.  
**Note:** the report below is verbatim. The reviewer's final message was split by an output limit; the two parts were joined at the split point (inside finding m5). The reviewer disclosed that one repo-wide grep surfaced lines from `tmp/design-v1/` and `_reviewer/design/`, and states it did not open or use them.

---

## Verdict

**Approve with changes.** The architecture is sound: a stateless `/mcp` endpoint, one shared tool registry, and in-process dispatch through the REST router. But B1 is a security claim that is false for the deployment the doc recommends, and it must be fixed before Phase 2. Nine Majors also need decisions.

## Summary

The design adds an opt-in `POST /mcp` to the existing hand-rolled server. It serves both MCP eras statelessly, shares a registry with the stdio proxy, and adds the `ApiResponse` and head/body-split plumbing as an independent Phase 0. The strongest version of the proposal is that it reuses the hardened gate, keeps validation in one place (the REST handlers), and ships in reversible steps.

The weak points are these:
- The "token required" rule doesn't detect the reverse-proxy deployment it exists for.
- In-process dispatch silently drops protections that live in `read_request`.
- The era-detection and error-semantics rules don't match the spec text.
- Several threat-model and spec-MUST claims are not supported by the code.

## What's solid

- **Code claims that check out:**
  - The gate runs at `api.rs:1827` and the token check at `1839-1844`, after `read_request`.
  - `conn_sem` is at `1790`, the 128 KiB header cap at `415`, and `http_response`/`json_err` at `107-127`/`154-166`.
  - `handle_media` is at `1397-1457` and `declared_args_only` at `mcp.rs:293`.
  - The router has 57 arms, which is 59 route patterns once the `/health` and `/` aliases are counted. SSE is handled separately.
  - The tool table has 34 rows, and the annotation table has 34 rows.
  - `SYNC_SEND_WAIT` is 30 s and `SYNC_CLIENT_TIMEOUT` is 45 s (`bridge.rs:55-64`). ADR 0042 does give 20 s for the semantic-query wait.
- **The shared registry fixes real drift risk.** The hand-maintained mirror tables in `mcp.rs:603-674` are evidence of the problem.
- **The head/body split closes a gap ADR 0044 admitted.** Today the gate runs after the body is buffered (`api.rs:1816-1827`). Moving auth ahead of the body read helps every route.
- **The statelessness choices match the 2026-07-28 excerpt.** GET and DELETE return 405, `Mcp-Session-Id` and `Last-Event-ID` are ignored, notifications get 202, and `-32022` carries `supported` and `requested`.
- **Smaller strengths:**
  - Off by default.
  - Phase 0 can ship alone.
  - The prompt-injection threat is stated honestly.
  - Legacy unknown-method returns 200 with a reasoned rationale.
  - The test list is thorough.

## Findings

### Blocking

**B1. "Unauthorized network access: Mitigated" is false for the recommended TLS-proxy deployment.**
- **Statement:**
  - Constraint 3 requires a token only if `ALLOWED_HOSTS` or `ALLOWED_ORIGINS` is non-empty.
  - Those are set only when a proxy forwards a non-loopback `Host` or a browser sends `Origin`.
  - A proxy that rewrites `Host` to its upstream presents `127.0.0.1:7270`. That passes the gate with no allowlist, and non-browser MCP clients send no `Origin`.
  - nginx's default for `proxy_pass http://127.0.0.1:7270` behaves this way (`Host $proxy_host`). So does Apache without `ProxyPreserveHost`. This is general knowledge, not from the repo or the excerpts.
  - The daemon cannot tell a proxied connection from a local one. In that setup the token stays optional.
  - The result is anonymous remote send and delete on the user's WhatsApp account, through the endpoint the design tells users to put behind a proxy.
- **Evidence:**
  - Design §8: "prevents anonymous remote access via a proxy" (lines 481-483), and the threat row "Mitigated (tightened policy)" (line 528).
  - `api.rs:364-378`: loopback `Host` names pass on a loopback bind.
  - `api.rs:1839-1844`: the token is optional.
  - `.env.example:24-26`.
- **Direction:**
  - Keep constraint 3 as an additional rule, but stop claiming it detects proxying.
  - Options, which can be combined:
    - Require a token (refuse to start, or at least WARN) whenever `WHATSRUST_MCP_HTTP_ENABLE` is on.
    - With no token configured, reject requests carrying `Forwarded` or `X-Forwarded-*`. This is defence in depth, not a guarantee.
  - Otherwise, reword the row to "partial: only when the proxy preserves Host or a browser sends Origin". Ship a documented proxy snippet that sets a token.

### Major

**M1. The recommended proxy setup exposes the whole REST API, and local-file `path` mode stays open there.**
- **Statement:**
  - `/mcp` shares the port and the token with all 57 REST routes.
  - `is_loopback` is derived from the bind address, not the peer (`api.rs:1788`, `531-541`, `1424`).
  - Behind a proxy on a loopback bind, a token holder can `POST /api/image {"path": "..."}`. That reads and sends an arbitrary local file.
  - ADR 0046 and `CallContext.loopback_file_access=false` protect only the MCP route.
  - A single shared token also can't support F5's "per token" profile (FEATURES.md:84-90). REST remains a bypass.
- **Direction:**
  - Document that the proxy must forward only `/mcp`, or enforce it in the daemon: a non-loopback `Host` class reaches `/mcp` only.
  - Derive `loopback_file_access` from the Host class or the allowlist state, not from the bind.
  - Consider a separate `WHATSRUST_MCP_TOKEN`. In-process dispatch makes it cheap, because `/mcp` never re-runs the REST token check.

**M2. `/mcp` bypasses the per-route body caps, so "same validation as stdio" is false.**
- **Statement:**
  - The caps live in `read_request` (`api.rs:459-472`: 1 MiB for non-media routes, 70 MiB for media). The handlers have none. `handle_send` (`701-740`) only calls `parse_body`.
  - The one exception is the 50 MiB cap on decoded media (`1416`).
  - In-process dispatch skips `read_request`, so every tool accepts the 10 MiB envelope. `whatsrust_send.text`, `poll.options` and `status_text.recipients` can carry 10 MiB. Via stdio they are capped at 1 MiB.
  - The memory bound is also understated. One body gets copied several times (read buffer, `Value`, `args.clone()`, re-serialized `Vec<u8>`, handler parse), so "8 × 10 MiB" is not the real ceiling.
- **Direction:**
  - Extract `max_body_for(path)` and enforce it on the serialized body inside `dispatch_rest_request`.
  - Build the body bytes once.
  - Size the `/mcp` envelope from the registry's largest legitimate tool, not a flat 10 MiB.

**M3. Era detection: `_meta` presence is not a modern signal, and the rule set is under-determined.**
- **Statement:**
  - Design §5 line 292 says "legacy clients never send `_meta`" and sends header-absent plus `_meta` to modern, answering 400 `-32020`.
  - The legacy protocol defines `params._meta.progressToken` for progress. The excerpts don't cover it, so verify against the 2025-03-26 progress utility.
  - A 2025-03-26 client never sends the version header. If it sets a progress token on `tools/call`, it gets 400 for a legal request.
  - The ADR (line 31, "method = initialize → legacy") and the design table (header 2026-07-28 plus `initialize` → 404) disagree. Precedence is undefined for cases like `initialize` plus `_meta` and no header.
  - The design writes the key as nested `io.modelcontextprotocol/protocolVersion` and `_meta: {serverInfo}`. The excerpt (line 205) shows flat namespaced keys such as `"io.modelcontextprotocol/serverInfo"`.
- **Direction:**
  - Treat a request as modern only if `_meta["io.modelcontextprotocol/protocolVersion"]` exists.
  - Use a fixed precedence: header value, then that key, then method `initialize`, then legacy default.
  - Put it in one table-driven function with a legacy progress-token test case.

**M4. 2025-03-26 is advertised, but its batching is rejected.**
- **Statement:**
  - A header-less request defaults to 2025-03-26 (design line 343).
  - The 2025-03-26 transport allows array bodies (excerpt lines 408-411).
  - The server lists that version in `supported` and in `initialize` counter-offers, yet answers batches with 400 `-32600`. That is non-conformant for an advertised version, and constraint 1 names 2025-03-26.
  - The excerpt (lines 74-77) lets a server that doesn't support header-less clients reject them.
  - Unspecified: a client-sent JSON-RPC response body (legal in legacy → 202, illegal in modern → 400).
- **Direction:** Either implement JSON-only batching (serial execution, array response, all-notifications → 202, no `initialize` in a batch, a size cap because each element can hold the permit for up to 30 s), or stop advertising 2025-03-26 and reject header-less requests. Either choice makes the matrix honest.

**M5. Argument validation is incoherent: `-32602` versus `isError`, and the mapping silently coerces bad types.**
- **Statement:**
  - Design lines 263 and 462 send "malformed arguments" to `-32602`. The excerpt (lines 124-128) puts "input validation errors" under `isError: true`.
  - No "before REST" validation exists. `declared_args_only` turns non-object arguments into `{}` (`mcp.rs:294-296`).
  - The mapping closures then coerce silently: `as_i64().unwrap_or(20)` (`mcp.rs:317-318`, `326`) and the `fetch_status` branch (`360-366`). The design snippet copies this.
  - `fetch_status {job_id:"5"}` becomes "list active jobs". `history {before:"123"}` returns page 1 forever.
  - The REST handler never sees the original bad value, so the "MUST validate all tool inputs" mapping (design line 536) does not hold for the GET tools.
- **Direction:**
  - Validate arguments against the registry `inputSchema` (types and required fields) at the choke point, and return `isError` with a readable message. This also fixes stdio.
  - Reserve `-32602` for a non-object `arguments` or a missing `name`.
  - Make the filter part of the only constructor of `RestRequest`, for example `registry.build(tool, raw_args)`. Today there are two call sites, and ADR 0046's own "negative" warns about a future caller skipping the filter.

**M6. Token-policy enforcement is log-and-continue, and token strength and brute force are unaddressed.**
- **Statement:**
  - `serve()` logs and returns. `main.rs:427-433` spawns it and nothing observes the return. The daemon keeps holding the WhatsApp session with its API dead, so the CLI, stdio MCP and `/health` all fail.
  - ADR 0022's pattern is to exit non-zero with the exact fix. The design cites 0022 for naming, not for behaviour.
  - Setups following ADR 0044 and `.env.example:28-39` (allowlist, no token) break silently on upgrade. Phase 0 ships this alone.
  - The token is the only control for off-machine exposure, but nothing checks its strength. The example placeholder is `your-secret-token-here`.
  - A 401 is unthrottled (`api.rs:1839-1844`), so a token can be brute-forced through a proxy.
- **Direction:**
  - Validate in `main` before the spawn and exit 1 with the fix, or at least WARN each startup and surface it in status.
  - Give the policy change its own ADR, or amend 0044. Update `.env.example`, the README and the HOWTO.
  - Enforce a minimum token length when the off-machine rule triggers. Document proxy-level rate limiting on 401.

**M7. Registry annotations silently change stdio's approval posture, and some hints are inaccurate.**
- **Statement:**
  - Design §4 waves through "stdio `tools/list` now includes annotations". Existing stdio users may stop being prompted for `history` and `search`. Clients commonly treat `readOnlyHint` as an auto-approve signal (general knowledge, not in the excerpts).
  - `history` and `search` are the prompt-injection ingress.
  - `send` is `destructiveHint:false` ("additive"), though its effect on third parties is irreversible.
  - `unpin`, `unmute`, `unarchive` and `unstar` are labelled "additive", but they remove state.
  - `whatsrust_groups` is described as "reads local cache; no WhatsApp interaction". But `bridge.rs:1261-1263` needs a live client and calls `client.groups().get_participating()`. The comment at line 1260 says "cached"; whether the library caches is unverified.
- **Direction:**
  - Ship annotations on `/mcp` first, or state the stdio change explicitly in the ADR consequences and release notes.
  - Decide `destructiveHint` for outbound tools deliberately.
  - Fix the table rows above.

**M8. The "rate limit tool invocations" MUST is deferred to a roadmap item that doesn't own it, and the premise is wrong.**
- **Statement:**
  - The excerpt (line 133) says servers MUST rate limit tool invocations. Design lines 33, 531-532 and 788 defer it to F5. F5's scope (FEATURES.md:80-99) is access profiles and says nothing about rate limiting.
  - The premise that reads are local is wrong. `group_info` hits the network on a cache miss (`bridge.rs:1242`) and `groups` calls the library (`1263`).
  - Typing, pin, mute, mark-read and delete-chat go straight to the library (`api.rs:885-903`; `bridge.rs:1458-1466`, `2194-2198`). They are not queued, so "outbound paced by token bucket" (design §8) covers only `enqueue_op` operations.
  - Given the project's anti-ban stance (ADRs 0020 and 0022), a looping injected agent is a real hazard.
- **Direction:** Assign rate limiting to a named roadmap item, or add a small limiter for non-queued network-touching tools in Phase 5. Correct the threat rows.

**M9. No server-side deadline, plus `try_acquire` on 8 permits, makes `/mcp` easy to wedge.**
- **Statement:** This answers the design's open question 3: yes, a deadline is needed.
  - Stdio has a 45 s read-timeout backstop (`mcp.rs:392`, `422`). The in-process path has none.
  - `get_group_info` (`bridge.rs:1242`), `start_typing` (`1458`) and `pin_chat` (`2194`) await library calls with no timeout in the bridge. One stuck call holds a permit indefinitely, and 8 stuck calls kill the endpoint until restart.
  - The permit is acquired before the body read, and `read_request`'s timeouts are per-read (`api.rs:407`, `483`). A client trickling one byte every 9 s holds a permit forever.
  - Agents often issue parallel tool calls. The 9th gets a REST-shaped 503, which an SDK treats as a transport error rather than a tool result.
- **Direction:**
  - Wrap dispatch in `tokio::time::timeout`. For POST-class tools, return `isError` text reusing `POST_OUTCOME_UNKNOWN`.
  - Add a total deadline for the head and body reads.
  - Use a short bounded wait for the permit, or a JSON-RPC-shaped 503 with `Retry-After`.

### Minor

**m1. The head/body split as specified can't work, and closing before reading the body can hide the 401/403.**
- `read_request` reads 4096-byte chunks and keeps the leftover (`api.rs:402-411`, `476-481`). `read_request_body(stream, content_length, limit)` has no way to receive it, so a small body that arrived with the head would be lost or hang for 10 s.
- `read_request` is also `pub(crate)` and used by `mcp.rs` tests (`514`, `535`). Keep a wrapper.
- Responding before draining a large body risks a TCP RST that destroys the response, so use a lingering close.
- The head phase now makes `Expect: 100-continue` cheap to honour.

**m2. `ApiResponse` has no headers field.**
- A 405 needs `Allow` (RFC 9110, outside the excerpts). A 401 may need `WWW-Authenticate`.
- The design adds a raw-bytes `http_202_accepted()`, giving two response shapes.
- This refactor touches every handler (50 `Vec<u8>` sites), so add the headers field now.
- The reason-phrase table (`api.rs:108-119`) lacks 202, 405, 411 and 415. A 415 renders as "Unknown" today.

**m3. Staging and observability.**
- Define Phase 2 and 3 behaviour for modern traffic: 400 `-32022` with a legacy-only `supported` list. That triggers the spec's `initialize` fallback.
- Keep the `supported` list as one constant that Phase 4 flips.
- Move the official-SDK or Inspector test from Phase 5 to the Phase 3 exit. Legacy-first means interop is the main risk.
- Log rejects (era, version, client name). Only successful tool calls are logged today.

**m4. Opt-in semantics.**
- The pipeline says "set?" while the docs say `=1`.
- Use `bool_env_var` (`api.rs:199`) and read it once at startup, like the allowlists (`1794-1795`). `=0` or `=false` must not enable the endpoint.
- Surface `mcp_http_enabled` in status. WARN when it is enabled without a token.

**m5. The parity test as specified is tautological.**
- Both transports call the same mapping closure.
- The real divergences go untested:
  - body caps (M2);
  - the bind-based `is_loopback`;
  - stdio treating status ≥ 400 as an error (`mcp.rs:456`, where the design instead uses "not 2xx");
  - an unknown tool, which is `isError` in stdio (`mcp.rs:375`) but `-32602` over HTTP.
- Direction: run one call corpus through both TCP `serve()` and `dispatch_rest_request`, then diff the resulting `ApiResponse`.

**m6. The "sanitize tool outputs" claim is contradicted by the code.**
- Design §8 says "no raw SQL or internals exposed". But store errors are returned raw: `json_err(500, &e.to_string())` at `api.rs:1110` and `1126`. `bridge_err` (`182-197`) returns arbitrary `anyhow` text.
- These strings go verbatim to the LLM and to a possibly remote client.
- Direction: scrub 500s on the MCP path, or soften the claim.

**m7. Threat-model wording.**
- The "prompt-injected agent" row says "exfiltration closed", but only file exfiltration is closed. An agent that reads `history` can still `send` or `status_text` the content to an attacker-chosen JID. Say so.
- Also missing: another local user or process hitting a token-less loopback `/mcp`.

**m8. Doc accuracy.** Line references are stale, apparently because the working tree has moved on.
- `mcp.rs`:
  - `run_mcp_server` is at 16-49 (doc says 13-46).
  - The `2024-11-05` literal is at 88 (doc says 85).
  - `tool_definitions` is at 108-273 (doc says 105-269).
  - `call_tool` is at 305-382 (doc says 305-374).
  - `http_get` and `http_post` are at 385-440 (doc says 391-419).
- `api.rs`:
  - `handle_send` is at 701-740 (doc says 676-726).
  - `serve()` is at 1760-1867 (doc says 1760-1863).
  - "Lines 1754-1760" has no matching code.
  - The task spawn is `main.rs:430`, not `api.rs:1767`.
- Internal inconsistencies:
  - The §7 table gives the 415 `code` as `internal_error`, while §11 changes it to `unsupported_media_type`.
  - There is no mapping for the 411 `code` in `json_err`.
  - `RestMapping.path` "may include query" while a separate `query` field exists.
  - "Populated at compile time" is not possible for `String`/`Value` fields without `LazyLock`.
  - The §5 table says "counter-offer with latest supported version", while the text says `2025-11-25`.

**m9. `whatsrust_send` schema drift is worse than "open".**
- MCP always sends `sync=true` (`mcp.rs:331`), and `handle_send` rejects `link_preview` and `schedule_at` in sync mode (`api.rs:703-710`). So those two fields can never succeed via MCP.
- The tool description still advertises link preview (`mcp.rs:111`).
- Removing them from the schema is a two-line edit. Do it in Phase 1 rather than treating it as the "next step" gate.
- The ADR 0046 guard tests (`mcp.rs:603-674`) must migrate with the registry.

**m10. Duplicate MCP headers.**
- The gate rejects duplicate `Origin` and `Host` (`api.rs:353-377`).
- The design is silent on duplicate `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name`. `find_header` takes the first (`api.rs:279`).
- Reject duplicates.

### Question

**Q1.** Which concrete clients and versions are the acceptance targets, and can they send a static `Authorization` header? The problem statement names none. OAuth-only hosted clients cannot use a static bearer token.

**Q2.** Is the off-machine, TLS-proxy deployment a supported scenario of this feature, or only a tolerated one? The answer sets how hard B1, M1 and M6 must be fixed.

**Q3.** Does 2026-07-28 keep `ping`? The excerpts cover only the legacy ping page, so the design's "both eras use the same ping semantics" is unsupported. Answering `{}` is low-risk either way.

**Q4.** What are `ttlMs` and `cacheScope` for `server/discover` and `tools/list`? The design leaves `ttlMs` as `<ms>`. The excerpt (lines 112-117) says the tool set MAY vary by authorization. Once F5 makes the list per-credential, `"public"` would be wrong and cached lists stale.

**Q5.** Do the library's `get_participating` and `get_metadata` cache or time out internally? This is needed to finalize the annotations (M7) and the deadline values (M9).

## Open questions

- Q1 and Q2 are needed to judge the real threat surface.
- Q3 and Q4 are spec points the excerpts don't settle.
- Q5 is a library behaviour I could not verify from this repo.
- Which B1 option does the owner choose: token required whenever `/mcp` is enabled, forwarded-header rejection, or a reworded claim only?

## Assumptions

- Constraints 1-4 are fixed. I critiqued only their consequences.
- I read the working tree, which has uncommitted changes in `src/api.rs`, `src/mcp.rs` and `src/bridge.rs`. No `/mcp` code exists yet.
- I used general knowledge, not the excerpts, for: nginx and Apache default `Host` rewriting, `_meta.progressToken` in legacy requests, clients using `readOnlyHint` to auto-approve, and `Allow` on 405.
- I did not verify the whatsapp-rust library's internal caching or timeouts.
- **Disclosure:**
  - A repo-wide grep I ran for env-var names returned matching lines from `tmp/design-v1/` and `_reviewer/design/`.
  - I did not open those files and did not use that content. Every finding rests on the permitted inputs.

## Outside the requested structure

- **Alternatives not considered (Note):**
  - A separate listener or separate MCP credential. FEATURES.md:49 lists "separate listener" as an open F3 question, and the design never evaluates it. Alternative A's rejection cites TLS setup, which is a non-goal for both options. The real tradeoff is isolation versus operational simplicity.
  - Using an existing Rust MCP SDK for the protocol layer, or a one-line reason for not doing so.
  - Putting the protocol code in its own module (for example `src/mcp_http.rs`) instead of growing `api.rs`.
- **One-way doors (Note):**
  - The advertised version list and legacy statelessness, which clients will key off.
  - The stdio annotation change, which is silent.
  - The `ApiResponse` shape, which touches every handler.
  - The token-policy break.
- **Complexity (Note):** `CallContext` for hypothetical F4/F5 fields and the three `/api/status` counters are speculative. They are cheap, so they are not a gate.

## Relevant files

- `/Users/gila/prv/whatsrust/docs/plans/2026-10-15-mcp-http-transport-design.md`
- `/Users/gila/prv/whatsrust/docs/adr/0045-mcp-streamable-http-transport-in-daemon.md`
- `/Users/gila/prv/whatsrust/tmp/mcp-spec-excerpts-2026-07-28.md`
- `/Users/gila/prv/whatsrust/src/api.rs`
- `/Users/gila/prv/whatsrust/src/mcp.rs`
- `/Users/gila/prv/whatsrust/src/bridge.rs`
- `/Users/gila/prv/whatsrust/src/main.rs`
- `/Users/gila/prv/whatsrust/docs/adr/0022-fail-closed-config-safety-with-scoped-overrides.md`
- `/Users/gila/prv/whatsrust/docs/adr/0044-api-origin-host-content-type-hardening.md`
- `/Users/gila/prv/whatsrust/docs/adr/0046-mcp-forwards-only-declared-tool-arguments.md`
- `/Users/gila/prv/whatsrust/docs/adr/0047-sync-send-timeout-returns-job-id.md`
- `/Users/gila/prv/whatsrust/docs/plans/FEATURES.md`
- `/Users/gila/prv/whatsrust/.env.example`