# 0006. Embeddings via stateless sidecar binary

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Semantic search requires embedding text into high-dimensional vectors (~384-1024 dims). Embedding models are Python-heavy (sentence-transformers, transformers) or need special runtimes (ONNX, TensorFlow Lite).

Options:
1. Embed Rust embedding runtime (e.g., `candle`, `tract`) — adds large dependencies, complicates build
2. Call external HTTP service (e.g., OpenAI API) — network dependency, cost, latency
3. Stateless sidecar binary via stdio — clean boundary, swappable transport

## Decision

Embeddings via **stateless sidecar** (separate binary). v1 = stdio child process (JSON-RPC, mirrors `mcp.rs` pattern).

`Embedder` trait is transport-neutral:
```rust
trait Embedder {
  fn model_info(&self) -> ModelInfo; // {model_id, dim}
  fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
  fn health(&self) -> Result<()>;
}
```

Batch-aware (send multiple texts per call). v1 = `StdioEmbedder`. Future: `HttpEmbedder` (localhost:PORT) as sibling impl behind same trait.

Sidecar is **pure vectorizer** — owns no storage, no search logic, no message history. whatsrust owns all persistence and search orchestration.

## Consequences

**Positive:**
- Clean separation: whatsrust = Rust-only, sidecar = Python/ML-friendly
- Swappable transport (stdio → HTTP) without changing whatsrust search code
- No embedding runtime in whatsrust build (keeps binary size small, single Rust toolchain)
- Batch API reduces IPC overhead

**Negative:**
- Adds second binary to deploy (whatsrust + embedder sidecar)
- Stdio IPC latency (~1-5ms per batch, acceptable for async workload)
- Sidecar crash = embedding unavailable (mitigated by FTS5 fallback, see ADR-0007)

**Future:**
- HTTP transport for remote GPU embedder or cloud API
- Replace Python sidecar with pure Rust ONNX runtime if `candle` matures
