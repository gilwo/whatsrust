# 0042. Cancellation-safe sidecar client; bounded query-embed wait with cool-down

**Status:** Accepted
**Date:** 2026-10-05
**Amends:** ADR 0024 (sidecar protocol client behaviour) and the search fallback rules of
M2.4.4 / ADR 0041. The wire protocol is unchanged.

## Context

The first live run of hybrid recall with bge-m3 (2026-10-02) returned no semantic results at all.
Searches came back in about 20 ms with only keyword matches, although the sidecar was alive and
all 25 messages were embedded. Two causes combined:

1. **A client bug that broke the connection for good.** `StdioEmbedder::call_rpc` wrote a request and
   then awaited one reply line, all under a mutex. `search` wraps the query embed in a 5 s
   `tokio::time::timeout`. When the timeout fired between the write and the read, the late reply
   stayed in the pipe. Every later call read the previous call's reply and failed its id check. From
   then on every `health`/`embed` failed instantly and search fell back to keyword-only until the
   daemon was restarted.
2. **Occasional slow embeds.** The machine was swapping heavily (19 of 20 GB used). macOS had paged
   out the idle sidecar's ~2.2 GB of bge-m3 weights, so the first query embed after an idle period had
   to page them back in and took longer than 5 s.

Constraints: the sidecar handles requests strictly one at a time (one stdin loop). The CLI and MCP
clients give up after 30 s (`api.rs` `cli_get`, `mcp.rs` `http_get`/`http_post`). Slow embeds are
occasional, and the user accepts some extra waiting when they happen.

## Decision

1. **The client survives cancellation.** `StdioEmbedder` gets a writer task (owns stdin, fed by a
   channel), a reader task (owns stdout, bounded lines) and a map of pending requests keyed by id.
   A caller that is cancelled just stops waiting. Its late reply is discarded when it arrives. On
   EOF or a read error, all pending requests fail and later calls fail fast. More than one request
   can be in flight; the sidecar still answers them in order.

2. **Search waits for the same request instead of retrying.** One overall budget,
   `WHATSRUST_SEMANTIC_QUERY_TIMEOUT_SECS` (default **20 s**), covers both getting the embedder ready
   and embedding the query. If `health` reports `loading`, search polls until it's `ok`, within the
   budget, which matches ADR 0024's "wait, don't fall back yet". Search then waits for its embed reply
   with whatever budget is left. It logs at growing checkpoints (5 s, 10 s, 20 s), e.g. "query embed
   still pending after 10s (sidecar slow)". **It never re-sends the request.** A re-send would queue
   behind the original in the one-at-a-time sidecar, so it can't come back sooner. It would only make
   the sidecar compute the same embedding again on a machine that is already swapping. The default
   stays below the 30 s client timeouts; a configured value of 30 s or more logs a startup warning,
   because CLI/MCP callers would give up first.

3. **Cool-down after a cap-hit.** When a search uses up its whole budget, it returns keyword results
   and semantic search goes into a cool-down. Later searches skip the embed (instant keyword results)
   for **30 s**. The window doubles on each further cap-hit, up to **10 min**, and resets on the first
   successful embed from search or the drain worker. Entering and leaving the cool-down are each
   logged once. Occasional slowness still gets the full wait; only a long stall triggers the
   cool-down, so searches don't each pay 20 s and still get only keyword results.

4. **Unchanged:** the drain worker's own exponential backoff on sidecar failures (1 s doubling to the
   configured cap); drain embed calls have no per-call deadline. With no embedder, an embedder in
   `error`, or a cool-down in effect, search is byte-identical to M1 keyword search.

**Rejected:**
- Retrying with a doubling timeout: re-sends work and can't finish sooner (see 2).
- Keepalive embeds to keep the weights in memory: each one touches all 2.2 GB and competes with
  other apps for memory on an already-swapping machine.
- `mlock`: pins 2.2 GB in memory for good.
- Raising the 30 s client timeouts to allow longer waits: interactive searches could hang for a minute.

## Consequences

**Positive:** a timeout no longer breaks semantic search; occasional slow embeds return semantic
results after a short extra wait; long stalls degrade to instant keyword results instead of a 20 s
wait per search; the logs show what is slow and why.

**Negative:** a search can take up to the budget (20 s by default) when the sidecar is slow; one more
setting; the cool-down can briefly hide semantic results after a stall has ended, until the next
successful embed resets it.

## Amendment (2026-10-15)

ADR 0047 raised the CLI and MCP client timeout from 30 s to `SYNC_CLIENT_TIMEOUT` (45 s) so the
daemon's structured timeout response for synchronous sends (504 + job_id + "do not retry") arrives
first. The startup warning for `WHATSRUST_SEMANTIC_QUERY_TIMEOUT_SECS` now fires at `>=
SYNC_CLIENT_TIMEOUT` (45 s) instead of `>= 30 s`, because that is the effective ceiling for
interactive calls. The default query-embed budget (20 s) still stays comfortably below the client
timeout.
