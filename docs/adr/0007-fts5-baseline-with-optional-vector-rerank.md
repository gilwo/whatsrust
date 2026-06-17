# 0007. FTS5 always-on baseline with optional vector rerank

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Semantic search via embeddings requires:
1. Lexical recall stage (fast filter to ~50-200 candidates)
2. Semantic rerank stage (cosine similarity over candidate vectors)

Pure vector search (exhaustive cosine over all messages) is O(n) and slow for large history. Approximate Nearest Neighbor (ANN) indexes (HNSW, IVF) need external libraries or loadable extensions (see ADR-0008).

Embeddings may be unavailable:
- Sidecar not running / crashed
- User disabled embedding for privacy / perf reasons
- Model switched (old vectors incompatible with new model)

## Decision

**FTS5 always-on** as lexical baseline. Every message gets FTS5-indexed text (sender, body, caption).

**Vector rerank optional:** only when embeddings exist for the active model. Search flow:
1. FTS5 lexical query → top 50-200 candidates
2. If embeddings available for active model: fetch candidate vectors → cosine rerank in Rust → top-k semantic results
3. If embeddings unavailable: return FTS5 lexical results (graceful fallback)

Every vector is stamped with `(model_id, dim)`. Refuse cross-model cosine comparison (incompatible embedding spaces).

## Consequences

**Positive:**
- Lexical search works immediately (no embedding required)
- Graceful degradation when embedder down or model switched
- FTS5 filters candidates to O(k) before expensive cosine computation
- User can disable embeddings without losing search entirely

**Negative:**
- Lexical-only results are weaker for semantic queries ("messages about X" vs exact keyword match)
- FTS5 index overhead (~10-30% table size increase, acceptable)

**Future:**
- Pure-semantic queries (no lexical match) → optional bounded brute-force cosine over recency/chat-scoped subset (deferred to v2)
- Hybrid scoring (lexical score × semantic score) for richer ranking (deferred)
