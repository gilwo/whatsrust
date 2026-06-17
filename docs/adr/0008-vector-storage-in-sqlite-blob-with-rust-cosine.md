# 0008. Vector storage as BLOB in SQLite, cosine rerank in Rust

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Vector similarity search typically uses Approximate Nearest Neighbor (ANN) indexes (HNSW, IVF) for O(log n) lookup. SQLite options:
- `sqlite-vec` loadable extension: HNSW/IVF in C, but requires runtime `load_extension()` + per-platform `.dylib` (breaks single-binary ethos)
- Standalone ANN libraries (e.g., `hnswlib-rs`): separate datastore sync burden
- Pure SQLite BLOB + brute-force cosine: O(k) over FTS5-filtered candidates, simple, no external deps

whatsrust ships as single 5MB binary (no loadable extensions, no per-platform artifacts).

FTS5 confirmed available in bundled SQLite (empirically probed). No need for runtime feature detection.

## Decision

Store embeddings as **BLOB columns** in SQLite `embeddings` table:
```sql
CREATE TABLE embeddings (
    message_id INTEGER PRIMARY KEY,
    model_id TEXT NOT NULL,
    dim INTEGER NOT NULL,
    vec BLOB NOT NULL
);
```

Search = FTS5 lexical recall → ~50-200 candidates → fetch their vectors → **cosine rerank in Rust** → top-k.

**No ANN index** in v1. No loadable extension. Preserves single-binary ethos.

BLOB schema is forward-compatible: future ANN index (e.g., statically compiled `sqlite-vec` = option C) is additive, not a schema change.

## Consequences

**Positive:**
- Single binary (no .dylib, no platform-specific artifacts)
- Schema works immediately (no extension loading, no version skew)
- O(k) cosine over FTS5 candidates is fast enough for k ≤ 200 (< 5ms on modern CPU)
- Future ANN index is drop-in (same BLOB format)

**Negative:**
- Pure-semantic queries (no lexical filter) require O(n) brute-force (mitigated by recency/chat scoping, deferred to v2)
- Large k (> 500 candidates) slows rerank (but FTS5 limits k to top-200 by default)

**Future option C:**
- Statically compile `sqlite-vec` into whatsrust (no runtime load_extension) if upstream supports static linking
- Or migrate to standalone ANN index (e.g., `hnswlib-rs`) if O(n) becomes bottleneck
