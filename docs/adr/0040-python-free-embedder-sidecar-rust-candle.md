# 0040. Python-free embedder sidecar: Rust + candle

**Status:** Accepted
**Date:** 2026-09-22

## Context

The MVP embedder sidecar is Python + `sentence-transformers` (ADR 0039). Two problems motivate a
native rewrite:

1. **Heavy runtime dependency.** Deploying Python + PyTorch (~GB of deps) alongside an otherwise
   single-binary Rust daemon undercuts whatsrust's "one lean binary, no Node/Python" premise.
2. **A real integration bug that live E2E surfaced.** The Python sidecar loads the model
   *synchronously at startup* (~15 s) before it can answer any request, but whatsrust's construction
   handshake times out `model_info` at a hardcoded **10 s** (`MODEL_INFO_TIMEOUT`, `src/embedder.rs:190`).
   10 s < 15 s → `StdioEmbedder::from_env()` returns `Err` → `embedder = None` → the drain worker is
   never spawned → semantic search is silently disabled (the failure logs at DEBUG only).

ADR 0039 and the sidecar MVP spec (`docs/plans/2026-08-30-embedder-sidecar-mvp.md` §7.2) both
anticipated a Rust/Go reimplementation "for a Python-free deploy — decided later." This ADR is that
decision.

Constraints: the sidecar wire contract is fixed (ADR 0024 — newline-delimited JSON-RPC over stdio;
`model_info`/`embed`/`health`; trust-but-verify). Models are **prefix-free** (MiniLM MVP → bge-m3;
ADR 0039). whatsrust is model-agnostic (spawns `WHATSRUST_EMBEDDER_CMD`). **Zero embeddings are
currently stored**, so switching sidecars has no back-compat / re-embed cost — parity matters only
for search quality, not for reading existing vectors.

## Decision

Reimplement the sidecar as a **native Rust binary using `candle`** (Hugging Face's Rust ML framework)
plus the HF `tokenizers` crate. Chosen over Go and over Rust + `ort`:

- **Rust unifies the toolchain** with whatsrust (one `cargo` build/CI, no second language runtime) and
  leaves open a future *in-process* embedder (the `Embedder` trait is transport-neutral).
- **`tokenizers` gives byte-identical tokenization** — it's the same Rust library `sentence-transformers`
  uses under the hood. Go would need a reimplemented tokenizer (parity drift risk).
- **`candle` is pure Rust** — loads safetensors directly (no ONNX export step), CPU inference, **no
  C/C++/Python runtime dependency**, fully achieving "drop the heavy dependency." Go's only mature ML
  path (`onnxruntime-go`) needs cgo + shipping Microsoft's ONNX Runtime shared library, which only
  half-achieves the goal.
- **`ort`** (ONNX Runtime Rust bindings) is the sanctioned fallback if `candle` cannot cleanly run a
  target model (e.g. bge-m3), accepting a native ONNX Runtime lib dependency.

Implementation requirements:

1. **Identical wire protocol** (ADR 0024) — drop-in via `WHATSRUST_EMBEDDER_CMD`; **no whatsrust code change.**
2. **Answer `model_info` immediately** from static metadata (`model_id`, `dim`) — never block it on model
   loading. Load weights lazily (first `embed`) or on a background thread that reports `health: loading`
   (which the drain worker already waits on, up to `WHATSRUST_EMBEDDER_LOADING_TIMEOUT_SECS`). This
   structurally fixes the 10 s-handshake bug regardless of load time.
3. **Match `sentence-transformers` embedding semantics**: tokenize → transformer forward →
   **mean-pool over tokens with the attention mask** → **L2-normalize**. Verify parity (cosine of
   native vs Python vectors ≈ 1.0 on sample English/Hebrew/Arabic texts) before switching the active model.
4. **Models:** MVP `paraphrase-multilingual-MiniLM-L12-v2` (384-dim); path to `bge-m3` (1024-dim) per
   ADR 0039 (model switch is free/lossless, ADR 0017).
5. **Form:** a separate sidecar binary (preserves process isolation + the model-agnostic design). Keep
   the Python sidecar as a reference/fallback; retire it once the Rust one is validated end-to-end.

## Consequences

**Positive:** single-language deploy (no Python/PyTorch); fast startup (candle loads MiniLM in ~1–2 s);
structurally fixes the handshake bug; byte-identical tokenization; one toolchain; future in-process option.

**Negative:** must implement pooling/normalization correctly and verify parity against Python; candle's
model coverage is narrower than ONNX Runtime (the `ort` fallback exists); adds a Rust build target plus
model-weight distribution (fetch-on-first-run or vendored safetensors + tokenizer.json + config.json).

**Neutral:** whatsrust is unchanged (protocol identical). The hardcoded 10 s `MODEL_INFO_TIMEOUT`
(`embedder.rs:190`) is left as-is — answering `model_info` fast makes it moot; making it
env-configurable stays an optional defense-in-depth item (see the M2 review follow-ups in
`docs/plans/IMPLEMENTATION-ROADMAP.md`).

**Implementation note (2026-09-24, parity verified):** `paraphrase-multilingual-MiniLM-L12-v2`'s
sentence-transformers pipeline is `Transformer` + `Pooling` only, with **no `Normalize` module**. The
Python sidecar therefore emits *un*-normalized vectors (norm ≈ 4–5). The earlier "normalized by
default" claim here and in the MVP spec §2 was wrong. The Rust sidecar mean-pools and then
L2-normalizes. Its output is **direction-identical** to Python: cosine = 1.000000 on English, Hebrew,
Arabic, mixed, emoji and truncated inputs, and the max element diff after normalizing the Python
vector is about 1e-7. whatsrust ranks with scale-invariant `cosine_similarity`, so the two sidecars
are interchangeable under the same `model_id`.

**Supersedes:** the Python sidecar (ADR 0039 / spec §3) as the *deployment* target, once validated.
ADR 0039's model-rollout decision (MiniLM → bge-m3, prefix-free) still stands — only the runtime changes.

**Implementation note (2026-09-30, bge-m3 added; commit `46b070a`):** the Rust sidecar now serves
**bge-m3** (`BAAI/bge-m3`, XLM-RoBERTa-large, 1024-dim, CLS-token pooling) alongside MiniLM, selected
via `--model minilm|bge-m3` (default `minilm`; `WHATSRUST_EMBEDDER_MODEL` env var also works, and is
inherited by the child process so `WHATSRUST_EMBEDDER_ARGS="--model bge-m3"` and the env var are
equivalent). bge-m3's `main` revision on Hugging Face ships only `pytorch_model.bin`; its weights load
memory-mapped from a community safetensors conversion at `refs/pr/130`, with config/tokenizer taken
from `main` — both revisions are pinned by commit hash and the resolved commit is checked after
lookup (`verify_pinned_commit`), so an upstream force-push fails loudly instead of drifting silently.
bge-m3 advertises `max_batch: 8` (a 32-text batch measured ~7s; capped at 8 keeps a batch to ~0.5s so
a concurrent query doesn't stall behind a full drain batch) — the daemon already clamps
`embedder_batch_size` to the advertised `max_batch`. The model loads on a background thread at
startup so `model_info` stays instant and `health` reports `loading` → `ok` (or `error`); an `embed`
call made before load finishes blocks until the background thread finishes (see
`EagerModel::wait_for_load`), which is expected, not a bug. Apple's Accelerate BLAS backend was
measured 2-3x slower than the plain backend for this model and was not adopted.

**Cold-cache download fix (same commit):** Hugging Face answers some `resolve` requests with a
relative `307 Location` that the pinned `hf-hub 0.3.2` can't follow, which silently broke first-run
downloads for both models (masked on machines with an already-warm cache). Files are now looked up in
the local hf-hub cache first (no network if present), and on a genuine miss fetched with a plain
`ureq` GET (which follows redirects itself) into a `<filename>.tmp-<pid>` sibling file that's renamed
into place on success; `HF_HUB_CACHE` is honored. Parity with Python `sentence-transformers` was
re-verified at cosine 1.000000 for both models. Known gap: if the GET or the write fails partway
through, the `.tmp-<pid>` file is left behind (no cleanup-on-error path) — harmless (a stray file in
the HF cache, cleaned up by a retry that reuses the same pid-based name only by coincidence) but worth
fixing opportunistically (tracked in `docs/plans/IMPLEMENTATION-ROADMAP.md`'s M2 review follow-ups).
