# 0024. Sidecar JSON-RPC protocol schema

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Embedder sidecar (ADR 0006) is a separate process. Bridge and sidecar must agree on wire format and method signatures.

whatsrust already has a JSON-RPC 2.0 over stdio implementation in `mcp.rs` (MCP server). Reusing it avoids inventing a second wire format.

Sidecar is stateless and batch-oriented (`Embedder` trait: `model_info()`, `embed(&[String])`, `health()`). Every embedding must be stamped with the model that produced it (ADR 0017 multi-model retention).

## Decision

**Framing: REUSE `mcp.rs`'s exact JSON-RPC 2.0 over stdio, newline-delimited.**
- No new wire format
- Sidecar = "another JSON-RPC-over-stdio peer"

**Methods (3, map to `Embedder` trait):**

### 1. `model_info` → `{model_id, dim, max_batch?, max_input_tokens?}`
Advertises model identity + limits so bridge can right-size batches and truncate long text.
- `model_id` (string): e.g., `"paraphrase-multilingual-MiniLM-L12-v2"`
- `dim` (int): vector dimension, e.g., `384`
- `max_batch` (int, optional): sidecar batch size limit (bridge respects it)
- `max_input_tokens` (int, optional): per-text token limit (bridge truncates)

### 2. `embed {texts: string[]}` → `{vectors: float[][], model_id, dim}`
Batch embed request. Response vectors in input order.
- Request: `{"jsonrpc": "2.0", "method": "embed", "params": {"texts": ["hello world", "foo bar"]}, "id": 1}`
- Response: `{"jsonrpc": "2.0", "result": {"vectors": [[0.1, 0.2, ...], [0.3, 0.4, ...]], "model_id": "...", "dim": 384}, "id": 1}`

**ECHO `model_id` + `dim` in EVERY response** so stored vector is stamped with the model that actually produced THAT batch (critical for ADR 0017 multi-model retention).

### 3. `health` → `{status: "ok"|"loading"|"error", detail?}`
- `ok`: sidecar ready
- `loading`: ONNX/GGUF model load in progress (takes seconds) — bridge should WAIT, don't fall back to FTS5 yet
- `error`: sidecar broken — bridge falls back to FTS5

**Trust-but-verify validation:** bridge validates every `embed` response:
1. `model_id` + `dim` match advertised (from `model_info`)
2. Vector count == input text count
3. Each vector length == `dim`

**Mismatch → REJECT batch as transport failure** (rows stay `pending` per ADR 0015, NEVER store mislabeled/corrupt vectors). Silent garbage poisons search worse than delayed embedding.

## Consequences

**Positive:**
- Reuses existing JSON-RPC framing from `mcp.rs` (no second wire format)
- Echo model+dim per response ensures every stored vector is correctly stamped
- `loading` health state prevents false-negative fallback during slow model load
- Trust-but-verify validation catches sidecar bugs before they poison search
- Optional `max_batch`/`max_input_tokens` in `model_info` enables right-sizing without hardcoded limits

**Negative:**
- Every `embed` response must echo model+dim (4-16 byte overhead per batch vs trust-once)
- Validation adds per-batch overhead (model/dim/count checks) — negligible vs embedding cost
- `loading` state requires bridge to distinguish "wait" vs "fallback" (more complex than binary ok/error)

**Rejected:**
- **gRPC / Protocol Buffers** — heavier deps, more complex than newline-delimited JSON-RPC
- **Trust-without-verify** — sidecar bug or model-swap mid-session could silently poison multi-model store (ADR 0017)

**Deferred:**
- Streaming embed (send texts incrementally, receive vectors as ready) — batch-oriented is simpler for v1
- Sidecar authentication (shared secret, mTLS) — stdio child is trusted (no network exposure)
