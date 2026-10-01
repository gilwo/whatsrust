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
only admission depends on a similarity floor that is model-specific. It must be re-calibrated whenever
the active model changes (`make calibrate EMBED_MODEL=<model>`, ADR 0039). A too-low floor adds
noise; a too-high floor hides real matches.

**Calibration requirement:** the default `WHATSRUST_SEMANTIC_MIN_SCORE` must be chosen from measured
cosine values of the active model on real related and unrelated message pairs, not guessed. Record
the measured values in the implementation note below.

**Future:** ANN or a recency window if histories outgrow the scan cap; weighted lexical × semantic
score fusion (still deferred from ADR 0007); a query/passage hint for asymmetric models (ADR 0024).

## Implementation Note: MIN_SCORE Calibration — MiniLM vs bge-m3 (2026-09-30)

Supersedes the single-model calibration dated 2026-09-26 (which measured
`paraphrase-multilingual-MiniLM-L12-v2` only and chose a default of 0.25). Re-run against the
current Rust sidecar binary (`embedder-rs`, ADR 0040) for **both** models now that bge-m3 is
supported (ADR 0039) and is the default for `make run-semantic`. Reproducible via
`make calibrate EMBED_MODEL=minilm` / `make calibrate EMBED_MODEL=bge-m3`
(`scripts/calibrate_semantic.py`, read-only against `whatsapp.db`).

**Method (unchanged in spirit, same script for both models):** 12 word-free queries, each targeting
one of the 24 stored text messages with **no shared words** with the query (paraphrase, synonym, or
cross-language — mostly English query → Hebrew target). Cosine similarity computed between each
query vector and all 24 message vectors; "Rank" is the target's position among all 24 (1 = closest).
"Unrelated distribution" pools the other 23 scores for every query (12 × 23 = 276 scores per model).

**Per-query target cosine and rank (out of 24 messages), MiniLM vs bge-m3:**

| Query | Target (Hebrew/other) | MiniLM cos | MiniLM rank | bge-m3 cos | bge-m3 rank |
|-------|------------------------|-----------:|------------:|-----------:|------------:|
| "summer camp" | קייטנה | 0.1413 | 11/24 | 0.4843 | 5/24 |
| "companies" | חברות | 0.3706 | 2/24 | 0.4731 | 8/24 |
| "wonderful" | מושלם | 0.4064 | 6/24 | 0.5652 | 2/24 |
| "laughter" | חחחח | 0.2793 | 7/24 | 0.5180 | 6/24 |
| "surprise" | Yay | 0.4286 | 4/24 | 0.5576 | **1/24** |
| "pasta sauce" | בולונז | 0.2804 | 3/24 | 0.4958 | **1/24** |
| "is this about the picture" | לתמונה | 0.8140 | **1/24** | 0.7044 | **1/24** |
| "you are crazy" | לא נורמלי | 0.5347 | 2/24 | 0.6156 | **1/24** |
| "related to" | קשור | 0.2236 | 19/24 | 0.5500 | **1/24** |
| "name call" | אלירן | 0.3952 | **1/24** | 0.4550 | 10/24 |
| "save for tomorrow" | שמרי | 0.4317 | **1/24** | 0.6479 | **1/24** |
| "smoke" | Smoke | 0.8835 | **1/24** | 0.7300 | **1/24** |

Target ranks #1 (closest of 24): **MiniLM 3/11; bge-m3 6/11** over the 11 word-free queries.
`"smoke"` → `Smoke` shares a word with its target, so keyword search finds it regardless. It is
excluded from the counts; including it gives 4/12 vs 7/12. The
model switch does what ADR 0039 predicted: bge-m3 is substantially better at ranking a word-free
cross-language/paraphrase target as the single best match, not merely "somewhere in the top few."

**Unrelated distribution (276 scores/model — the other 23 messages for each of the 12 queries):**

| Metric | MiniLM | bge-m3 |
|--------|-------:|-------:|
| Median | 0.1851 | 0.4435 |
| 95th percentile | 0.4417 | 0.5512 |
| 99th percentile | 0.5478 | 0.5639 |
| Max | 0.6106 | 0.5830 |

bge-m3's cosine values run uniformly higher — both for targets and for unrelated messages — than
MiniLM's. This is a known property of this embedding space (less separation in absolute cosine
terms), not a defect: `cosine ≥ floor` still has to be read as model-relative, which is exactly why
the floor can't be ported from one model to the other.

**Floor table (targets admitted / 12, unrelated-pass %):**

| Floor | MiniLM unrelated-pass | MiniLM targets | bge-m3 unrelated-pass | bge-m3 targets |
|------:|----------------------:|----------------:|----------------------:|-----------------:|
| 0.25 | 31.5% | 10/12 | 100.0% | 12/12 |
| 0.35 | 11.6% | 8/12 | 91.3% | 12/12 |
| 0.45 | 4.3% | 3/12 | 48.2% | 12/12 |
| **0.55** | **1.1%** | **2/12** | **5.4%** | **6/12** |
| 0.65 | 0.0% | 2/12 | 0.0% | 2/12 |

These fresh numbers corroborate the floor-0.55 row previously estimated for this rollout (MiniLM
2/12 targets at ~1% unrelated noise; bge-m3 6/12 targets at ~5% unrelated noise) — no material
discrepancy, so this table is the numbers to use going forward.

**Chosen default: `WHATSRUST_SEMANTIC_MIN_SCORE = 0.55`.**

**Rationale:**
- **Right for bge-m3, the default model.** At 0.55, bge-m3 admits half of the word-free targets
  (6/12, mostly the ones whose target already ranked #1 — the clearest cross-language/paraphrase
  wins), while the weaker/ambiguous ones (e.g. "summer camp"→קייטנה at rank 5/24, "name call"→אלירן
  at rank 10/24) are correctly held back — at a noise cost of only ~5% of unrelated messages. 0.45
  would let in all 12/12 targets but at 48.2% unrelated noise, far too loose to be useful as a floor;
  0.65 drops bge-m3 to the same 2/12 as MiniLM, losing most of its advantage. 0.55 is the floor where
  bge-m3's extra separation actually pays off.
- **Conservative for MiniLM** by design, not oversight: at 0.55, MiniLM's vector-only recall passes
  only 2/12 targets (1.1% noise) — i.e. MiniLM's vector-only contribution to hybrid recall is
  **effectively switched off** at the shared default. This is intentional: bge-m3 is the default
  model this floor is tuned for (ADR 0039); a user who deliberately switches to MiniLM for its
  speed/footprint should lower the floor (e.g. toward 0.35, where MiniLM admits 8/12 targets at an
  11.6% noise cost) and re-measure, rather than get silently-degraded cross-language recall at the
  bge-m3-tuned default.
- **Lexical matches are unaffected either way:** a query that shares a word with a message always
  takes the lexical path (ADR 0041's "lexical candidates are always kept" guarantee); this floor only
  gates vector-only candidates — the cross-language/paraphrase cases hybrid recall exists to add.

**The floor is model-specific and must be re-measured whenever the active model changes:**
`make calibrate EMBED_MODEL=<model>` reruns this exact procedure against the live binary and the
real (read-only) database. Do not reuse one model's floor for another without re-running it — as
the unrelated-distribution table above shows, the two models' cosine values aren't on comparable
absolute scales.
