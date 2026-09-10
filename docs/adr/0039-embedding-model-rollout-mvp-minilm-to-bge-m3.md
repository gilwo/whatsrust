# 0039. Embedding-model rollout: MVP MiniLM (Python sidecar) → bge-m3; prefix-free models only

**Status:** Accepted
**Date:** 2026-08-30

## Context

M2 built the whatsrust *side* of the embedding boundary (the `Embedder` trait, `StdioEmbedder`
transport, drain worker, and semantic search path) but deliberately ships **no embedding model** —
whatsrust is model-agnostic and spawns whatever `WHATSRUST_EMBEDDER_CMD` points at, reading the
sidecar's `model_info()` at startup (ADR 0006/0024). Until a real sidecar exists, semantic search
never activates (every query falls back to the M1 FTS5/BM25 lexical path). Two questions had to be
settled before building that sidecar: **which model**, and **what runtime / rollout order**.

Two constraints shaped the answer:

1. **Multilingual is required.** The user's data is Hebrew/Arabic; FTS5 `unicode61` covers
   space-delimited scripts lexically, but the vector layer is what provides semantic recall and the
   CJK/Thai coverage FTS5 lacks (ADR 0018).
2. **The `embed()` protocol is symmetric.** `WhatsAppBridge::search()` embeds the query with the
   *same* `embed(texts)` call the drain worker uses for documents — the sidecar cannot tell a query
   from a passage. Models that require asymmetric `"query:"` / `"passage:"` prefixes (the e5 family)
   therefore cannot be used correctly without adding a query/passage hint to the protocol
   (an ADR 0024 change + `StdioEmbedder`/search rework).

## Decision

**Rollout (MVP-first):**

1. **MVP:** a **separate Python sidecar** (`sentence-transformers`) serving
   **`paraphrase-multilingual-MiniLM-L12-v2`** (384-dim). Rationale: fastest path to validate the
   end-to-end operation flow and the *value* of semantic search on real Hebrew/Arabic data; light,
   CPU-friendly, and **prefix-free** (works with the current symmetric protocol, no ADR 0024 change).
2. **Evolve:** upgrade to **`BAAI/bge-m3`** (1024-dim, prefix-free, stronger multilingual) once value
   is confirmed, and/or reimplement the sidecar in **Rust/Go** for a Python-free deploy — decided at
   that time based on the state of things. Neither step requires a protocol change (both models are
   prefix-free).

**Prefix-free models only (for now).** The e5 family is excluded until/unless the protocol grows a
query/passage hint. `.env.example`'s recommendation is updated accordingly (MiniLM → bge-m3; e5
noted as requiring a protocol change).

**Model switching is cheap and lossless (ADR 0017), which is what makes MVP-first safe.** Vectors are
keyed `(message_id, model_id)` with a per-row `dim`; search always filters to the active model only,
so a 384-dim MiniLM vector and a 1024-dim bge-m3 vector for the same message coexist without
collision. Switching MiniLM→bge-m3 = restart with the bge-m3 sidecar → active model becomes bge-m3 →
the drain worker's set-difference query re-embeds everything under bge-m3; the old MiniLM vectors are
untouched cold storage (switch-back is instant). Message text is the retained source of truth, so
starting small costs nothing later.

## Consequences

**Positive:**
- Fastest possible validation of the semantic-search value proposition (small model, mature Python
  stack), with a clean, no-cost upgrade path to a stronger model.
- No protocol change needed on either the MVP or the bge-m3 step (both prefix-free).
- whatsrust stays entirely model-agnostic; the model/runtime decision lives in the sidecar.

**Negative:**
- MiniLM has a lower quality ceiling than bge-m3 — acceptable for an MVP value-check.
- The real sidecar is a **separate deliverable** (its own tool, not part of the whatsrust binary) —
  see `docs/plans/2026-08-30-embedder-sidecar-mvp.md`. Semantic search stays dormant (lexical
  fallback) until it exists.
- Switching MiniLM→bge-m3 on a large history re-drains every message under the new model and will
  **transiently trip the M2.3.8 pathological-pending circuit breaker** (backfill enqueue pauses)
  until the drain catches up — expected, not a bug.
- Cloud/HTTP embedders were rejected for the private-archive use case (would ship message text
  off-device); the sidecar is local-only.

**Builds on / refines:**
- **ADR 0006** (stateless sidecar) — concretizes the MVP as Python, future as Rust/Go.
- **ADR 0018** (multilingual strategy) — picks concrete models (MiniLM → bge-m3).
- **ADR 0024** (protocol) — records that the current symmetric `embed()` excludes prefix-asymmetric
  (e5) models; a query/passage hint is a future extension.
- **ADR 0017** (multi-model retention) — the switch-is-free property that makes MVP-first lossless.
