# 0041. Hybrid recall: bounded vector scan alongside FTS5

**Status:** Accepted
**Date:** 2026-09-26
**Extends:** ADR 0007 (activates its deferred "pure-semantic queries → bounded brute-force cosine"
item). ADR 0008 is unchanged (BLOB vectors + Rust cosine, no ANN).

## Context

The first live end-to-end run (2026-09-25, Rust sidecar, ADR 0040) confirmed the M2 pipeline works:
all 25 embeddable messages got vectors for the active model, and the cosine rerank runs. It also
exposed a limit of ADR 0007's design. **Search candidates come only from FTS5.** Semantic scoring
only *reorders* messages that already share words with the query. A query with no word overlap
returns nothing, even when the matching message is embedded and would score highly. Live example:
`summer camp` returns 0 results, although a stored message says `…עם קייטנה` ("…with a summer camp").
Cross-language and paraphrase matching is the main reason to add semantic search in this user's
mixed Hebrew/Arabic/English data. With recall gated on FTS5, most of that value is lost.

Constraints still hold: no ANN index and no loadable SQLite extension (ADR 0008); single
`Mutex<Connection>` storage; loopback single-user scale (thousands to low hundreds of thousands of
rows per chat, not internet scale); the lexical path must stay byte-identical when no embedder is
active.

## Decision

When an embedder is active and the query embeds successfully, search uses **hybrid recall**:

1. **Lexical recall** (unchanged): FTS5 candidates, `recall_width = max(200, limit)`.
2. **Vector recall** (new): brute-force cosine between the query vector and every stored vector for
   the active model in the search scope. The scope is the same `chat_jid` filter (or all chats when
   `None`) and the same `before_ts` bound as the lexical query. Keep the top `max(200, limit)`.
3. **Merge**: union the two candidate sets, deduplicated by `message_id`.
   - Candidates that have a vector are ranked by cosine, descending.
   - **Lexical candidates are always kept.** They matched the query's words, so a semantic score
     never drops them (this keeps the "additive-only" guarantee from M2.4).
   - **Vector-only candidates** (no lexical match) are admitted only when
     `cosine ≥ WHATSRUST_SEMANTIC_MIN_SCORE`. Vector recall always returns *something*, so without a
     floor every query would surface unrelated messages. The default is calibrated against the real
     model on real data (see Consequences). The floor is not applied to lexical candidates.
   - Unvectored lexical candidates are appended after the vector-ranked set in FTS rank order, as today.
   - Truncate to `limit`.
4. **Bounded scan (safety cap)**: if the scope holds more than `WHATSRUST_SEMANTIC_SCAN_CAP` vectors
   (default 50 000), skip vector recall for that query. Log at DEBUG and fall back to the M2.4 path
   (lexical recall + rerank). ADR 0007 rejected unbounded O(n) cosine for large histories; the cap
   keeps per-query cost bounded (50 000 × 384 × 4 B ≈ 77 MB decoded, worst case) until an ANN or
   recency-window strategy is needed.

Everything else stays the same: no embedder, or a query-embed failure or timeout, gives the M1
lexical path byte-for-byte; the query-embed timeout is unchanged; vectors of a different model are
never compared (ADR 0007/0017).

**Observability correction (same change):** `/api/status` `embeddings` currently counts rows by the
`embed_status` column. That column stays `pending` for a row's whole life by design (the
`embeddings` table is the source of truth, ADR 0017), so fully embedded rows are reported as
"pending". Replace it with per-active-model counts from the `embeddings` table:
`{model_id, embedded, pending, failed, skipped}`. Here `embedded` is rows with a vector for the active
model, and `pending` is embeddable rows without one (the drain's set-difference). When no embedder is
active, report `model_id: null` and omit `embedded`.

## Consequences

**Positive:** paraphrase, synonym and cross-language queries find matching messages with no shared
words. Lexical results are never lost. The no-embedder path is untouched. No new storage format, no
ANN dependency, schema stays v8. The status counts now show real embedding progress.

**Negative:** per-query cost is linear in the scoped vector count (bounded by the scan cap). Vector-
only admission depends on a similarity floor that is model-specific. It must be re-calibrated when
the active model changes (e.g. bge-m3, ADR 0039). A too-low floor adds noise; a too-high floor hides
real matches.

**Calibration requirement:** the default `WHATSRUST_SEMANTIC_MIN_SCORE` must be chosen from measured
cosine values of the active model (MiniLM) on real related and unrelated message pairs, not guessed.
Record the measured values in the implementation note below.

**Future:** ANN or a recency window if histories outgrow the scan cap; weighted lexical × semantic
score fusion (still deferred from ADR 0007); a query/passage hint for asymmetric models (ADR 0024).
