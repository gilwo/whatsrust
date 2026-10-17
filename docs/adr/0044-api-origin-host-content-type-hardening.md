# 0044. Origin/Host/Content-Type gate closes CSRF and DNS-rebinding holes in the API server

**Status:** Accepted
**Date:** 2026-10-14

## Context

The hand-rolled REST server (`src/api.rs`) does no `Origin` or `Host` validation, and
`parse_body` parses JSON regardless of `Content-Type`. On the default bind
(`WHATSRUST_BIND=127.0.0.1`), `WHATSRUST_API_TOKEN` is optional — it's only *required* for a
non-loopback bind (ADR-less convention, enforced in `serve()`). A token-less loopback API is
exactly the configuration browsers can reach without the user asking for it:

- **CSRF via a "simple" cross-site request.** A page the user merely visits can run
  `fetch("http://127.0.0.1:7270/api/send", {method:"POST", mode:"no-cors", body:"...", headers:
  {"Content-Type":"text/plain"}})`. Because the body is `text/plain` (or
  `application/x-www-form-urlencoded`, `multipart/form-data`, or no header at all — the CORS
  "safelisted" content types), the browser sends it as a simple request with **no preflight**.
  The daemon has nothing to answer a preflight anyway. It executes the request: sends messages,
  deletes chats, reacts, etc., all under the real session.
- **DNS rebinding defeats an Origin check alone.** A page on an attacker-controlled domain whose
  DNS later re-resolves to `127.0.0.1` is treated by the browser as same-origin to that domain
  for the *new* resolution, so a plain fetch from it can also *read* responses — message
  history, the `/api/events` SSE stream — not just cause side effects. Its `Host` header carries
  the attacker's domain, not `127.0.0.1`, which an Origin-only check wouldn't catch (the request
  may legitimately omit `Origin` for a same-site-looking navigation, or TOCTOU around the
  rebind).

Only an Origin check (CSRF) or only a Host check (rebinding) each leaves the other hole open, so
both are needed, plus a Content-Type requirement to force the browser into the preflight path
this server doesn't implement.

## Decision

A single pure function, `gate_request(method, headers, is_loopback_bind, allowed_origins,
allowed_hosts) -> Result<(), (status, reason)>` (`src/api.rs`), is called in `serve()`'s
connection loop immediately after `read_request` succeeds and **before** the Bearer-token check,
the SSE special case, and `handle_request` dispatch — i.e. before every route, with no
exceptions for GETs or SSE. Being pure (no I/O, no logging) makes it unit-testable in isolation;
the caller logs rejections (`debug!`: method, path, reason — never header values beyond the
canned reason, bodies, or tokens) and returns the existing `json_err` shape. No CORS headers or
`OPTIONS` handling are added — a cross-site preflight must keep failing.

Three checks, in order:

1. **Origin (primary CSRF defence).** If an `Origin` header is present — including the literal
   string `null`, which browsers send for sandboxed/file-origin requests — it must exactly match
   an entry in `WHATSRUST_ALLOWED_ORIGINS` (comma-separated, default empty) or the request is
   rejected with 403. More than one `Origin` header is also rejected with 403. A matching
   Origin only passes this check; the Host check still applies.
2. **Host (defeats DNS rebinding).** Checked on every request when the bind is loopback or
   `WHATSRUST_ALLOWED_HOSTS` is non-empty, including requests with an allowlisted Origin. Exactly one
   `Host` header must be present — zero or more-than-one is rejected with 403 — and its host part
   (port stripped, trailing dots stripped, IPv6 brackets unwrapped, lowercased) must be
   `127.0.0.1`, `localhost`, or `::1`, or an entry in `WHATSRUST_ALLOWED_HOSTS` (comma-separated,
   default empty — e.g. for a reverse proxy that forwards its own public Host). Other loopback
   spellings (`127.0.0.2`, `127.1`, `[::ffff:127.0.0.1]`, hex/octal forms) are rejected by
   default; add them to `WHATSRUST_ALLOWED_HOSTS` if a client connects that way. On a
   non-loopback bind with no `WHATSRUST_ALLOWED_HOSTS` set, this check is skipped entirely (that
   bind already requires `WHATSRUST_ALLOW_REMOTE=1` and a mandatory token).
3. **Content-Type on every POST** (the method is compared case-insensitively). Must be `application/json` (parameters such as `;
   charset=utf-8` allowed, case-insensitive); otherwise 415. This denies the CORS-safelisted
   content types (`text/plain`, `application/x-www-form-urlencoded`, `multipart/form-data`) and a
   missing header, forcing a preflight. Checked first that every POST route already parses a JSON
   body via `parse_body` (true for all 49 current POST routes, including the media routes, which
   carry base64 inside JSON, not multipart) — so this doesn't break any legitimate request shape.

**whatsrust's own HTTP clients were verified against the gate** (and both already satisfied it
without changes): `api.rs::cli_get`/`cli_post` and `mcp.rs::http_get`/`http_post` always send a
`Host` header (to the real connect address, which is loopback by default) and never send
`Origin`; `cli_post`/`http_post` already send `Content-Type: application/json`. Covered by new
tests in both modules that run the real request builders against a listening socket and assert
the bytes they put on the wire pass `gate_request`.

**GET routes were reviewed for hidden state mutation**, since a no-cors GET carries no `Origin`
and a valid `Host` and so bypasses checks 1-2 by design: all are read-only
(`/api/status`, `/health`, `/`, `/api/qr`, `/api/groups`, `/api/group-info`, `/api/history`,
`/api/search`, `/api/history-fetch`, `/api/events`). `GET /api/group-invite-link` calls
`client.groups().get_invite_link(jid, reset=false)` — WhatsApp lazily creates an invite code
server-side the first time it's asked for one, but whatsrust always passes `reset: false`, so
this GET never revokes/regenerates an existing link; left unchanged.

## Consequences

**Positive:** the default loopback API, which has no token, now has a real browser-facing
defence; a reverse-proxy or multi-origin-tool deployment can still opt in via the two allowlists;
the check is a single, independently-tested function reused for every route including the
planned HTTP MCP endpoint (the MCP spec's Streamable HTTP transport requires Origin validation
for the same reason — this gate is written to be reused there directly, not re-derived).

**Negative / behaviour change:** a raw `curl -X POST ... -d '...'` without `-H 'Content-Type:
application/json'` now gets 415 instead of being processed — grepped for in docs and fixed (no
`curl -X POST` examples existed in README/HOWTO at the time of this ADR, so no example needed
updating, but the "API security" notes added call this out for anyone scripting against the
API). A reverse proxy that doesn't forward a `Host` matching 127.0.0.1/localhost/::1 and doesn't
set `WHATSRUST_ALLOWED_HOSTS` will now get 403 on a loopback-bound daemon — this is the intended
tightening; such a deployment should set the new allowlist.

**Not done:** CORS headers / `OPTIONS` handling (deliberately — would reopen the hole this ADR
closes); limiting body read before the
gate runs (amended 2026-10-17, see below: the gate now runs before the body is read; the existing per-route body-size cap in `read_request` is unchanged; the original implementation had the gate run
after the body was already buffered, which was a minor, pre-existing DoS consideration unrelated
to CSRF/rebinding and out of scope for the original decision).

**Adversarial review (2026-10-14):** a review that tried to bypass the gate found no critical or
high issues. Header-parsing tricks, `Host` spellings, `Content-Type` look-alikes, browser flows
without `Origin`, and ordering were all blocked. Three hardening fixes followed: the POST method is
compared case-insensitively, duplicate `Origin` headers are rejected, and the Host check now runs
even when an allowlisted Origin is present (it previously ran only when `Origin` was absent).
Known, accepted: a header name with whitespace before the colon (`Host : x`) gets 403 rather than
the RFC's 400; it's still rejected.

## Amendment (2026-10-17)

The gate and token checks now run before the body is read (request pipeline: head → gate → token → framing → body). This closes the DoS consideration mentioned in the original "Not done" section. Large uploads are now buffered only after authentication passes.

The `415 Unsupported Media Type` response is now `HTTP/1.1 415 Unsupported Media Type` (reason phrase and JSON `code: "unsupported_media_type"`). The original implementation returned `415 Unknown` with `code: "internal_error"`.
