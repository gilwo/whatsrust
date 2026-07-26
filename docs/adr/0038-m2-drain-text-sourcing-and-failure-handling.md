# 0038. M2 embedding-drain text sourcing and failure handling

**Status:** Accepted
**Date:** 2026-07-23

## Context

M2 (semantic search) adds an embedding-drain worker that reads stored messages and sends their
natural-language text to the embedder sidecar (ADR 0006/0015/0024). Three questions the design
blueprint left open — or that the as-built M1 code forced — were settled during M2 planning
(`docs/plans/2026-07-23-M2-detailed-plan.md`, §0 Findings F-C/F-D and the v2/v3 cold reviews):

1. **Where does per-row embed-failure attempt state live?** ADR 0017 explicitly left this as
   "in-memory OR a table."
2. **How does the drain worker get the *bare* NL text to embed?** M1 stores the **decorated**
   `display_text()` label in `messages.body_text` (e.g. `"[image 40KB] caption"`,
   `"[location: Home (32.1,34.8)]"`), not bare NL as the design sketch assumed (F-C), and the drain
   worker no longer holds the original `InboundContent` enum — only the stored string.
3. **What about the pre-M2 backlog**, where every migrated row is `embed_status='pending'` and the
   raw `wa::Message` is not retained?

A fourth issue surfaced in the v3 review: the sidecar `embed` protocol is **batch-only** (ADR 0024:
one response per call, no per-item error channel), so a single un-embeddable row in a batch can
livelock a deterministic oldest-first drain.

## Decision

1. **In-memory attempt tracking** (resolves ADR 0017's open item for v1). A process-local map keyed
   by `(message_id, model_id)`; a per-row content rejection increments it; cap 3 →
   `embed_status='failed'`. **No `embed_failures` table, no schema migration** —
   `CURRENT_SCHEMA_VERSION` stays 8. Accepted tradeoff: counts reset on daemon restart (benign — a
   handful of wasted sidecar calls, never data loss; the terminal `failed` write is durable once
   reached). Entries are evicted once a row reaches terminal `failed`. Promotable to a durable table
   later if measured (additive, lossless — message text is the retained source of truth).

2. **Kind-gated drain text preparation (Option C).** The drain query selects `(content_kind,
   body_text)` and the worker branches by kind to derive the text to embed — **never a blind
   strip-after-`]`**, which yields empty strings for location/contact and noise for poll (their
   payload is *inside* the brackets — `bridge.rs:473-484`):
   - `text` → `body_text` as-is (protects a genuine message starting with `"["`);
   - `image`/`video`/`document` (caption trails the `]`) → strip the `"[… ] "` prefix to the caption;
   - `location`/`contact`/`poll` → embed the decorated `body_text` **as-is** (the label still
     carries the name/question; mild bracket/coordinate/`(pick N)` noise accepted on these rare
     kinds).

   This reconciles "`body_text` is decorated" (F-C) with "embed bare NL" (ADR 0016) **without a
   schema migration**. `body_text` itself stays the FTS-indexed label, untouched.

   **Deferred alternative (Option A):** persist a bare `embed_text` column at write time from
   `InboundContent::embeddable_text()` — exact for all kinds, no coupling to the `display_text()`
   format — a clean additive v8→v9 migration if embedding quality on location/contact/poll ever
   warrants it.

3. **Tolerate the pre-M2 backlog.** No one-time reclassify migration. Existing `pending` rows drain
   from their stored `body_text` through the same kind-gated prep. Rationale: the raw `wa::Message`
   needed for a perfect reclassify is not retained; volume is small (single-user); and stray non-NL
   text is bounded by the cap-3 → `failed` backstop. The one-time first drain therefore processes
   ~2× the NL-only volume the throughput math assumed (ADR 0015) — a slower one-time catch-up, not a
   correctness issue.

4. **Poison-pill batch bisection.** Because the protocol is batch-only, on K consecutive
   whole-batch failures of the *same* message-id set the worker halves the batch, converging to
   solo-batches so the per-row cap-3 (decision 1) can actually engage and retire the offending row
   to `failed` — draining the innocent rows meanwhile. Pure whatsrust-side retry logic; no ADR 0024
   protocol change.

## Consequences

**Positive:**
- M2 ships with **zero schema migration** (the single biggest scope/risk reduction available).
- The drain worker is robust to a single poison-pill message (no account-wide livelock).
- Failure tracking is trivial (a map), and the set-difference `embeddings` table remains the durable
  source of truth for "what's embedded" (ADR 0017).

**Negative:**
- Mild embedding-quality noise on `location`/`contact`/`poll` (rare kinds) until/unless Option A is
  adopted.
- Attempt counts reset on restart (bounded, benign).
- Bisection adds retry latency for a genuinely poisoned batch (rare).

**Refines / resolves:**
- **ADR 0017** — resolves its "in-memory OR a table" open item to *in-memory* for v1.
- **ADR 0016** — the classifier uses a new `embeddable_text()` distinct from `display_text()`; the
  pre-M2 backlog is tolerated, not reclassified.
- Builds on **ADR 0015** (drain worker), **ADR 0024** (batch-only protocol), **ADR 0031** (no schema
  bump; the supporting `idx_messages_embed_status` is a non-versioned `CREATE INDEX IF NOT EXISTS`).
