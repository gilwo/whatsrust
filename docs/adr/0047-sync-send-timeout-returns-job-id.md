# 0047. Sync send timeout returns job_id and says "do not retry"

**Status:** Accepted
**Date:** 2026-10-15

## Context

A synchronous send waits for the outbound worker to report the job Sent or Failed via the broadcast
event bus. Before this ADR, a timeout (30 s) returned HTTP 504 (via `bridge_err`'s message-matching
rule for "timed out") with the text "send timed out (30s)" — no `job_id` field and no retry
guidance. The queued job (which is usually delivered later) had no identifier in the response.

Whatsrust's own clients — the CLI (`cli_get`/`cli_post`, `tokio::time::timeout(30s)`) and the stdio
MCP proxy (`http_get`/`http_post`, `set_read_timeout(30s)`) — also timed out at 30 s, so the client
usually gave up first and saw a client-side "read timeout" or "connection closed" error with no
indication that the daemon accepted the request. If the agent naively retries, it delivers a
duplicate.

Two separate hazards: the daemon's timeout gives no job identifier and no "do not retry" guidance;
and a client timeout that fires before the daemon's response arrives gives no hint that the request
may already be processed.

## Decision

A synchronous send that times out returns HTTP **504** with a structured response carrying the
`job_id` and an explicit "do not retry" message. The job **stays queued** and is not cancelled,
because cancelling races the worker (which may have already sent it) and requires polling the
session's delivery receipts to tell "sent vs not sent" apart. Leaving it queued is the safe default:
the job will usually be delivered within seconds, and the caller has its identifier to check later.

### Daemon-side contract

`enqueue_and_wait` returns a typed `SendTimedOut { job_id }` error (instead of a plain string) on
timeout. The wait duration is a named constant, `SYNC_SEND_WAIT = 30 s` (`src/bridge.rs`).

`bridge_err` (`src/api.rs`) walks the error chain (so added `.context()` layers cannot hide it) and
converts `SendTimedOut` to HTTP 504:

```json
{
  "ok": false,
  "code": "timeout",
  "error": "send timed out after 30s: job N is still queued and may still be delivered; do not retry",
  "job_id": N
}
```

The error text explicitly states "still queued", "may still be delivered", and "do not retry", so a
caller that logs or displays it sees the correct action. A broadcast `Lagged` receiver can miss the
Sent event, so a 504 can also mean "already sent" — the message wording ("may still be delivered")
covers both cases.

Affected endpoints: every REST route whose bridge method uses `enqueue_and_wait`:
- `/api/send` waits only when `?sync=true` is present (the MCP `whatsrust_send` tool always uses it)
- `/api/poll`, `/api/forward`, `/api/reply`, `/api/status-text`, `/api/status-image`,
  `/api/status-video`, `/api/status-revoke` always wait

### Client-side contract

CLI and MCP clients wait **longer than the daemon** so the daemon's structured 504 normally arrives
first. A new constant, `SYNC_CLIENT_TIMEOUT = SYNC_SEND_WAIT + 15 s = 45 s` (`src/bridge.rs`),
gives a margin for network delay and the daemon's response serialization. The CLI (`cli_get`,
`cli_post` in `src/api.rs`) uses `tokio::time::timeout(SYNC_CLIENT_TIMEOUT, ...)`. The MCP
(`http_get`, `http_post` in `src/mcp.rs`) uses
`stream.set_read_timeout(Some(SYNC_CLIENT_TIMEOUT))`.

If a POST client's read timeout fires anyway (or a `read()` fails for any reason), the error text
includes: **"the request may still be processed by the daemon; check before retrying"** (literal
`POST_OUTCOME_UNKNOWN` constant in `src/api.rs`). A caller that sees this knows it must verify the
outcome before re-sending (e.g. subscribe to SSE `/api/events` for `OutboundStatus` events with the
`job_id`, or check `/api/history` for the sent message).

The startup warning for `WHATSRUST_SEMANTIC_QUERY_TIMEOUT_SECS` (ADR 0042) now fires at `>=
SYNC_CLIENT_TIMEOUT` (45 s) instead of `>= 30 s`, because the client timeout is the effective
ceiling for interactive calls.

**Rejected:**
- Cancelling the queued job on timeout: races the worker (which may have already sent it) and
  requires polling delivery receipts to distinguish "sent vs not sent"; leaving it queued is safer.
- Shorter timeouts: the 30 s daemon wait is unchanged from before (kept as is; tuning it is out of
  scope); the 15 s client margin only has to cover response delivery on loopback.
- Retrying automatically: delivers duplicates when the timeout is spurious (the worker is slow, not
  stuck).

## Consequences

**Positive:** a timed-out sync send returns a machine-parseable job identifier; the "do not retry"
wording prevents naive duplicate sends; clients outwait the daemon, so the structured 504 normally
arrives instead of a bare read timeout; POST read failures carry a reminder to check before
retrying.

**Negative:** a broadcast `Lagged` receiver can miss the Sent event and report a timeout for a job
that was already sent (the message wording covers this, but it is an edge case to be aware of); the
job is not cancelled on timeout, so it may deliver later when the caller has given up (but this is
the safer default); there is no API route that exposes outbound job state by id — a caller must
subscribe to SSE `/api/events` to track `OutboundStatus` events, or poll `/api/history` to see if
the message appeared.
