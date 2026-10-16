# Design Review — MCP Streamable HTTP transport (F3, ADR 0045) — v3 (follow-up)

**Date:** 2026-10-16  
**Artifact:** `docs/plans/2026-10-15-mcp-http-transport-design.md` + `docs/adr/0045-mcp-streamable-http-transport-in-daemon.md` (Proposed, revised v3)  
**Reviewer:** cold `design-reviewer` subagent (independent; not given the prior reviews)  
**Prior reviews:** [v1](2026-10-15-mcp-http-transport.md) (Needs rework) · [v2](2026-10-15-mcp-http-transport-v2.md) (Approve with changes, 1 blocking)  
**Spec reference given:** `tmp/mcp-spec-excerpts-2026-07-28.md` — a local, uncommitted file of verbatim excerpts from modelcontextprotocol.io (line numbers cited below refer to it); (verbatim excerpts incl. the 2026-07-28 changelog)  
**Owner constraints given as fixed:** 2026-07-28 + 2025-11-25/2025-06-18 (no 2025-03-26, no batching), legacy first; opt-in; token always required for `/mcp`; token required with allowlists; config violations exit; `ApiResponse` refactor with in-process dispatch; `whatsrust_send` trimmed and undeclared arguments rejected.  
**Note:** the report below is verbatim.

---

**Verdict:** Approve with changes. I found no Blocking issue. M1 to M5 should be settled before Phase 0 merges, and before any off-machine use is documented.

## Summary

A stateless, opt-in `/mcp` behind the existing gate, with one shared registry and in-process dispatch, is a coherent way to meet the seven constraints. Most code claims check out. The protocol layer is not where the risk is. The risk is in four places:
- what the proxy topology does to the REST surface that shares the port and token (M1);
- a proxy recipe that does not work as written (M2);
- breaking config changes hidden inside "refactor without behaviour change" (M3, M4);
- a timeout model calibrated to whatsrust's own client (M5).

The design and ADR have also drifted from the code and from each other (m7).

## What's solid

- **Auth before body.** The head/body split fixes a real gap. Today `read_request` buffers up to 70 MiB before the gate and token run (api.rs:400-506, then gate at :1827, token at :1840).
- **No request smuggling through the listener.** The server is one request per connection with `Connection: close` (api.rs:121), so smuggling through it is largely moot.
- **No HTTP 504 for sync-send timeouts.** Mapping the 504 to JSON-RPC 200 plus `isError` avoids transport-level auto-retry of a non-idempotent send.
- **Stateless in both eras.** There is no session store to leak or expire.
- **Opt-in with a kill switch.** The endpoint is off by default.
- **Honest threat table.** The gaps are named (content exfiltration, unthrottled reads).
- **`path` is unreachable through the tool registry.** ADR 0046 holds for the registry-built paths.
- **Code claims I verified:**
  - 34 tools map to 33 paths (mcp.rs:108-265).
  - `gate_request` is at api.rs:346 and called at :1827; the token check is at :1840; `serve()` is at :1760.
  - `SYNC_SEND_WAIT` is 30 s and `SYNC_CLIENT_TIMEOUT` is 45 s (bridge.rs:55-64).
  - `POST_OUTCOME_UNKNOWN` is at api.rs:31.
  - The library's default IQ timeout is 75 s (library `src/request.rs:177`).
  - `handle_request` has 59 method/path pairs, plus SSE.
- **Constraint 7 is already in the code.** The `whatsrust_send` schema lacks `link_preview` and `schedule_at` (mcp.rs:111-116), with a test at :751.

## Findings

### Blocking

None.

### Major

**M1. The supported off-machine topology silently re-enables REST `path` mode, and the only guard is a proxy snippet.**
- **Evidence:**
  - "Loopback" is derived from the bind address, not the peer: `let is_loopback = is_loopback_bind(&bind)` (api.rs:1788), passed to every handler at :1860.
  - `handle_media` refuses `path` only when `!is_loopback` (api.rs:1424-1425). That is how the code keeps remote callers away from local-file reads today.
  - Constraint 3 makes "loopback bind plus reverse proxy" the way off-machine, so every proxied REST request is classed as loopback.
  - The token is shared with `/mcp` and will sit in remote client configs and proxy logs. A leaked token then allows `POST /api/image {"path": ...}`, which reads any file the daemon user can read (`whatsapp.db` with Signal keys, SSH keys) and sends it to a JID, whenever the proxy forwards more than `/mcp`.
  - Design §8 calls this "documented residual risk".
  - `CallContext.loopback_file_access=false` (§4) only protects the in-process MCP route, which ADR 0046 already closes.
- **Direction:** Make safety independent of proxy config.
  - (a) Serve `/mcp` from its own listener carrying only that route, so a proxy pointed at it cannot reach REST. This is absent from alternatives A to C. FEATURES.md:48-50 lists "separate listener" as an open question, and the design never answers it.
  - (b) At minimum, treat a request carrying `X-Forwarded-For`, `Forwarded` or `Via` as non-loopback for `path` mode. The design's own snippet adds `X-Forwarded-For`.
  - (c) Introduce a separate `WHATSRUST_MCP_TOKEN` now instead of in F5. Splitting later means rotating every deployed client.
  - Record the choice in the ADR.

**M2. The documented nginx recipe fails on first use, and its rate limit throttles legitimate sessions.**
- **Evidence:**
  1. `proxy_set_header Host $host` forwards the public hostname. On a loopback bind the gate returns 403 unless `WHATSRUST_ALLOWED_HOSTS` lists that name (api.rs:364-377; ADR 0044 "Negative" says the same). Neither §8 nor the §13 table says a proxied deployment must set `WHATSRUST_ALLOWED_HOSTS`. The alternative is rewriting Host to `127.0.0.1`, which gives up the rebinding check on that path.
  2. `limit_req rate=10r/m burst=5 nodelay` is on `location = /mcp` and counts every request, authenticated or not. A stateless endpoint receives discover, list and each call as separate POSTs, so one agent session exceeds 10 per minute within seconds. nginx then returns 503 to the legitimate client. This is the only brute-force mitigation in §8.
  3. nginx's default `client_max_body_size` is 1 MiB, below the 10 MiB envelope the design promises.
- **Direction:**
  - Test the recipe against a real nginx (and Caddy) before documenting it.
  - Choose Host rewrite or allowlist explicitly.
  - Throttle failures only (auth-failure counting, fail2ban), or drop the throttle and rely on a high-entropy token (see M3).
  - Put body size in the snippet.

**M3. Phase 0 is billed "refactor without behaviour change" but ships breaking config changes with no migration path.**
- **Evidence:**
  - Phase 0 and §13 make the daemon exit when:
    - a token is shorter than 32 characters;
    - an allowlist is set without a token (constraint 4);
    - the non-loopback rules are violated. Today `serve()` logs and returns (api.rs:1762-1776).
  - An existing short-token deployment, or ADR 0044's documented proxy setup (allowlist, no token), goes from working to not starting on upgrade.
  - ADR 0022's template, which the design cites, pairs each refusal with a scoped override. Here there is none.
  - The 32-character rule is not an owner constraint, and its scope is ambiguous. Phase 0 says any set token; §13 says any token a rule requires.
  - Length is a weak proxy for entropy, and the arithmetic is off: 32 base64 characters is 192 bits, and `openssl rand -base64 32` prints 44 characters.
  - A mandatory token also breaks the CLI and the stdio proxy for anyone following `.env.example`.
    - `.env` loads only in daemon mode (main.rs:239-258, after the `cli_main` early return).
    - `whatsrust mcp` and the CLI read the token from the process environment (mcp.rs:424 and :455; api.rs:1876) and would send none, getting 401.
    - That contradicts the "stdio without regression" goal (§2).
- **Direction:**
  - Split Phase 0 into pure refactors (head/body split, `ApiResponse`) and a separately released policy change with release notes.
  - Warn for one release before refusing the existing rules.
  - Make the length rule a warning, or scope it to `/mcp` enabled.
  - Decide whether the CLI and `mcp` load `.env`, or document that the token must be in the process environment.

**M4. The fail-closed check is placed after the bridge has started, contradicting the ADR 0022 pattern it cites.**
- **Evidence:**
  - §3 and Phase 0 put the check "in `main()` before spawning the API task". That is between `WhatsAppBridge::start` (main.rs:421) and the spawn (main.rs:427-433).
  - The existing fail-closed block says it "MUST run before bridge construction" (main.rs:347-369).
  - `start` calls `Store::new` (bridge.rs:1099), which runs the pre-migration backup and migrations (storage.rs:~790-800).
  - An exit at that point comes after DB open and migration, with WhatsApp workers running, via `process::exit` with no shutdown.
  - The check must also respect `api_port > 0` (main.rs:427), or an API-disabled daemon would refuse to start.
- **Direction:** Validate beside `validate_backfill_safety` (main.rs:358), gated on `api_port > 0`. Reuse `configured_api_token()` so trim and empty handling match `serve()` (api.rs:226-231).

**M5. The server-side deadline can pre-empt the mechanism it relies on, and is calibrated only to whatsrust's own client.**
- **Evidence:** §6 wraps `dispatch_rest_request` in a 40 s `tokio::time::timeout`, so cancellation means dropping the future.
  1. In `enqueue_and_wait` the DB enqueue (`enqueue_op`, bridge.rs:1866) runs before, and outside, the 30 s timer (bridge.rs:1868). If the enqueue stalls more than about 10 s (SQLite contention with backfill or the embed drain), the outer drop fires with the job persisted and no `SendTimedOut{job_id}` ever built. The caller gets "outcome unknown" text and no `job_id`, which is the duplicate-send hazard ADR 0047 exists to remove.
  2. For the non-queued network tools the drop lands at an arbitrary await inside a library call. Their cancel-safety is never examined. For pin, mute, archive, mark_read and delete_chat the path is `send_app_state_mutation` then `send_app_state_patch` (library `chat_actions.rs:744-783`). I did not verify that path is cancel-safe either. ADR 0042 is this daemon's own precedent: a timeout dropped mid-exchange corrupted a client until restart. The wording "may still be processed by the daemon" (api.rs:31) is also wrong after a drop. The daemon has stopped, and the outcome on WhatsApp is unknown.
  3. "40 s is 5 s before the client timeout" holds only for the 45 s `SYNC_CLIENT_TIMEOUT` of the CLI and stdio proxy. Third-party MCP clients and proxies set their own timeouts, so ADR 0047's "the structured 504 arrives first" is not established for them.
- **Direction:**
  - Run the handler as a task the timeout merely stops waiting on, as ADR 0042 does.
  - Keep the permit until that task ends.
  - Bring the enqueue under the send deadline so a `job_id` exists whenever a job does.
  - Say in HOWTO which minimum client timeout is needed.

### Minor

**m1. One 8-permit pool is shared by all tools, and `send` holds a permit for up to 30 s.**
- **Evidence:**
  - `sync=true` is hard-wired (mcp.rs:361). Sends are paced by `SendPacer` (default burst 5, main.rs:292-295).
  - Stdio is strictly serial (mcp.rs:24-48), so `/mcp` is the first path that can run 8 calls at once.
  - A burst of parallel sends fills the pool. `whatsrust_status` and `fetch_status`, the calls used to check progress, then wait 5 s and fail with a non-JSON-RPC 503.
  - More concurrent sync waiters also raise the chance of a broadcast `Lagged` that drops the Sent event (bridge.rs:1889), giving a false 504.
- **Direction:** Reserve permits for local reads, or cap in-flight sync sends below the pool size. Report "busy" as a tool error the model can read.

**m2. REST-shaped rejections collide with the modern client's 400 fallback rule.**
- **Evidence:** Spec excerpt, Streamable HTTP, "On `400 Bad Request`, the client SHOULD inspect the response body": a 400 whose body is not a recognized modern JSON-RPC error makes the client fall back to `initialize`. The design returns REST-shaped or text 400s for:
  - oversize bodies (§7);
  - era step 4;
  - batching;
  - a bare 411.

  A modern client that uploads 11 MiB silently downgrades itself to legacy and never learns why.
- **Direction:** Use 413 for oversize. `Content-Length` is known at the head phase, so refuse before reading. Give all `/mcp` rejections after routing a JSON-RPC error body (for example -32600).

**m3. The annotation table contradicts its own rule and the spec text it quotes.**
- **Evidence (§9):**
  - `edit` is destructive because it "overwrites".
  - `pin`, `mute`, `archive`, `star` and `mark_read` are "not additive → destructive=false". The quoted definition says the opposite: "If false, the tool performs only additive updates".
  - `send` is destructive for "irreversible third-party effect", yet `reply`, `image`, `location`, `contact`, `poll` and `status_*` have the same effect and are `false`.
  - `fetch_cancel` is called "additive".
  - Hosts use these hints for auto-approval and confirmation, so an agent (or injected text) can dodge a prompt by calling `reply` instead of `send`.
- **Direction:** Apply one rule. Consider keeping the spec default (`destructiveHint: true`) for every non-read-only tool that changes remote state.

**m4. The cache hints are wrong for this endpoint.**
- **Evidence:**
  - §10 Q1 proposes `ttlMs: 3600000` and `cacheScope: "public"` for an authenticated list that F5 will make per-token (FEATURES.md:93-94).
  - This same change rewrites the send schema, so clients would hold the old list for up to an hour after upgrade.
  - Flipping to `private` later cannot recall copies already marked `public`.
- **Direction:** Use `private` and a short TTL now.

**m5. The memory bound is understated.**
- **Evidence:**
  - §6 lists five in-memory copies of a body up to 10 MiB: read buffer, `Value`, `args.clone()`, serialized `RestRequest.body`, and the handler's `MediaReq`. The base64-decoded bytes (up to 7.5 MiB) come on top. That is roughly 55 to 60 MiB per request, or about 450 to 480 MiB at 8 permits, not "8 × 20 MiB".
  - `args.clone()` and the re-serialization are avoidable by moving ownership.
  - Wording error: 70 MiB is the cap on the base64 body (api.rs:459-469). The decoded cap is 50 MiB (api.rs:394).

**m6. The parity test is close to tautological.**
- **Evidence:**
  - Both transports consume the same mapping, so comparing its output with itself proves little.
  - The wire path goes through `read_request`'s query split and `decode_query_component` (api.rs:82, :433-444); the in-process path skips both.
  - `args.get("limit").as_i64()` on `20.0` returns `None` and silently applies the default 20. `20.0` is an integer under JSON Schema 2020-12. This contradicts "defaults only for absent keys" (§4).
- **Direction:** Add a golden test for all 34 tools through a real `serve()`, comparing stdio and `/mcp`. Share one integer rule between `validated_args` and the mappings.

**m7. Drift to reconcile before the ADR moves to Accepted.**
- Phase 1 and the ADR's "Negative" list still treat removing `link_preview` and `schedule_at` as work to do. It is already done (mcp.rs:111-116; ADR 0046).
- ADR 0046 deliberately leaves value types unchecked, and a test asserts it (mcp.rs:644-647). Phase 1's type and required checks, plus the stdio change from `isError` to -32602, need an ADR 0046 amendment. They are stdio behaviour changes under a "no regression" goal.
- Token-length scope differs between Phase 0 and §13.
- Batching body is JSON in §5 but "Text body" in the §7 table.
- The §7 table gives 415 the code `internal_error`; §11 says `unsupported_media_type`.
- §12 says "Phase 2 ships legacy support only"; §14 puts that in Phase 3. The Phase 2 manual test expects a 501.
- `max_body_for(path)` is described as coming "from the route table". No route table exists; the cap is an inline prefix test (api.rs:461-469).
- `try_acquire_owned()` "with a bounded timeout" (§6) is not an API. The pipeline diagram also says "wait … or return 503 immediately".
- §3's mcp.rs line references are stale. `run_mcp_server` is at 15-49, `tool_definitions` 108-266, `call_tool` 331-412, `http_get`/`http_post` 414-470, and the version string at line 88. Only `validated_args` (309) and the api.rs references are right.

**m8. Two unstated non-goals.**
- `notifications/cancelled` is answered 202 and ignored. The design covers disconnects only.
- Browser-hosted MCP clients cannot work. The daemon deliberately answers no CORS preflight (ADR 0044), and a preflight carries no token, so it gets 401. `WHATSRUST_ALLOWED_ORIGINS` does not change that.
- Say both in HOWTO.

### Question

**Q1. Does era step 4 break legacy handshakes?** It rejects every non-`initialize` request that has neither the header nor the `_meta` key. That includes `notifications/initialized` and a pre-initialize `ping`. Do the target legacy clients always send `MCP-Protocol-Version` on `notifications/initialized`? A 400 there is fatal in clients that treat the notification as part of the handshake. Nothing needs a response, so consider accepting notifications unconditionally. I could not check client behaviour from the repo.

**Q2. What evidence supports 2025-06-18?**
- The spec excerpts contain no 2025-06-18 text. "Treated identically" to 2025-11-25 is an assumption.
- The exit criterion ("official SDK or Inspector") exercises whichever version that client negotiates, probably not 2025-06-18.
- Consider a scripted 2025-06-18 handshake.

**Q3. Is a missing `Mcp-Method` or `Mcp-Name` rejected in the modern era?** The spec marks them REQUIRED, and "a required standard header is missing" is a listed `-32020` condition. The design only says "if present, validated" for "any era", and "MUST match" for modern. State the missing-header behaviour.

**Q4. Is a dedicated listener or token genuinely off the table?** Constraint 6 (in-process dispatch) does not require sharing the port. See M1.

**Q5. What happens with a bad `WHATSRUST_MCP_HTTP_ENABLE` value or a disabled API?**
- `bool_env_var` accepts only `1`, `true`, `TRUE`, `yes` and `YES` (api.rs:199-204). `True` or `on` silently leaves `/mcp` off, and the user sees a 404. Log a warning on an unrecognized value.
- What should `WHATSRUST_MCP_HTTP_ENABLE=1` with `WHATSRUST_PORT=0` (API disabled) do?

## Open questions

1. Is "same port as REST" an owner decision, or only the draft's choice? (M1, Q4)
2. Which MCP clients are the target? Their timeouts, header behaviour on notifications, chunked uploads and static-bearer support determine M5, Q1 and the §5 compatibility claims.
3. Will existing deployments get a warning period for the new refusal rules, or a hard break? (M3)
4. Do the CLI and `whatsrust mcp` read the same `.env` as the daemon? (M3)
5. Are the four proposals in §10 (TTL and scope, permit wait, GET-class timeout text) confirmed? They are still marked as proposals.

## Assumptions

- I treated `tmp/mcp-spec-excerpts-2026-07-28.md` as the spec. I did not consult outside text.
- Some design claims about third-party clients are outside the excerpts and unverified:
  - "many legacy clients read 404 as gone";
  - "modern SDKs send Content-Length bodies";
  - behaviour of nginx, Caddy and the SDKs.
- I did not run code or tests.
- The working tree has uncommitted edits in api.rs, mcp.rs and main.rs, so line numbers are as read today.
- I assumed a single `WHATSRUST_API_TOKEN` per daemon, and that "same port" is not an owner constraint.
- Library cancel-safety (M5) is flagged as unexamined, not shown to be broken.

## Relevant files

- `/Users/gila/prv/whatsrust/docs/plans/2026-10-15-mcp-http-transport-design.md`
- `/Users/gila/prv/whatsrust/docs/adr/0045-mcp-streamable-http-transport-in-daemon.md`
- `/Users/gila/prv/whatsrust/src/api.rs` (gate :346, request reader :400, `serve` :1760, `handle_media` :1397)
- `/Users/gila/prv/whatsrust/src/mcp.rs` (`validated_args` :309, `call_tool` :331)
- `/Users/gila/prv/whatsrust/src/main.rs` (:239-258, :347-369, :407-433)
- `/Users/gila/prv/whatsrust/src/bridge.rs` (`enqueue_and_wait` :1863, constants :55-64)
- `/Users/gila/prv/whatsrust/docs/adr/0022-fail-closed-config-safety-with-scoped-overrides.md`
- `/Users/gila/prv/whatsrust/docs/adr/0042-cancellation-safe-sidecar-client-and-bounded-query-wait.md`
- `/Users/gila/prv/whatsrust/docs/adr/0044-api-origin-host-content-type-hardening.md`
- `/Users/gila/prv/whatsrust/docs/adr/0046-mcp-forwards-only-declared-tool-arguments.md`
- `/Users/gila/prv/whatsrust/docs/adr/0047-sync-send-timeout-returns-job-id.md`
- `/Users/gila/prv/whatsrust/docs/plans/FEATURES.md` (F3 to F5, lines 42-99)
- `/Users/gila/.cargo/git/checkouts/whatsapp-rust-6e26428647c827f3/9e8c70e/src/request.rs` (:177)
- `/Users/gila/.cargo/git/checkouts/whatsapp-rust-6e26428647c827f3/9e8c70e/src/features/chat_actions.rs` (:744-783)