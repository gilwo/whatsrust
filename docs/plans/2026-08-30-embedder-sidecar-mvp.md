# Embedder Sidecar MVP — Build Specification

**Date:** 2026-08-30  
**Purpose:** Self-contained build + test plan for implementing the whatsrust MVP embedder sidecar  
**Audience:** An implementer executing this in a fresh session (NO whatsrust code changes)  
**Output:** A working Python embedder sidecar that speaks the protocol whatsrust expects

---

## 1. Overview & Fixed Decision

This spec documents how to build the **MVP embedder sidecar** for whatsrust's semantic search
feature (M2, ADR 0006/0024). The sidecar is a **separate process** that whatsrust spawns to
convert text into embedding vectors.

### The Fixed Decision (do NOT relitigate)

**Runtime:** Python + `sentence-transformers`  
**Model:** `paraphrase-multilingual-MiniLM-L12-v2`
- 384 dimensions
- Multilingual (100+ languages including Hebrew, Arabic, CJK, Thai)
- **PREFIX-FREE** (symmetric: same embedding call for queries and documents)

**Why this MVP:**
- Fastest validation of the operation flow + value proof
- Multilingual coverage for whatsrust's Israeli +972 user base (Hebrew/Arabic/English mixed)
- Smallest model with adequate quality (384-dim vs 1024-dim is 2.7× smaller storage)

**NOT in MVP scope:** See §7 (Future/Evolution) for bge-m3 upgrade, Rust/Go reimpl, and
asymmetric-prefix models (e5). This spec is MVP-only.

---

## 2. Protocol Contract (WIRE-COMPATIBLE with whatsrust)

The sidecar MUST speak the exact protocol that `src/embedder.rs::StdioEmbedder` expects.
This section is transcribed from the **code** (the ground truth), not invented.

### 2.1 Framing

**JSON-RPC 2.0 over stdio, newline-delimited.**

- Read JSON requests from **stdin**, one per line
- Write JSON responses to **stdout**, one per line
- **FLUSH after every write** (critical: buffered stdout breaks the protocol)
- stderr is ignored (redirected to `/dev/null` by whatsrust)

**Request shape:**
```json
{
  "jsonrpc": "2.0",
  "id": <integer>,
  "method": <string>,
  "params": <object or omitted if none>
}
```

**Response shape:**
```json
{
  "jsonrpc": "2.0",
  "id": <same id from request>,
  "result": <object>,
  "error": <object if error, otherwise omit>
}
```

**Constraints:**
- One response per request, matching the `id`
- If `id` is present in request, echo it in response (whatsrust validates mismatches)
- `params` field MUST be **omitted entirely** (not `null`) when there are no params (for `model_info`/`health`)
- Max line length: 16 MiB (sanity bound, not a hard security limit — sidecar is trusted)

### 2.2 The Three Methods

#### Method 1: `model_info` (no params)

**Request:**
```json
{"jsonrpc": "2.0", "id": 1, "method": "model_info"}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "model_id": "paraphrase-multilingual-MiniLM-L12-v2",
    "dim": 384,
    "max_batch": 32,
    "max_input_tokens": 256
  }
}
```

**Fields:**
- `model_id` (string): EXACT model name, reported in every `embed` response for trust-but-verify
- `dim` (int): vector dimension (384 for this model)
- `max_batch` (int, optional but recommended): how many texts the sidecar can embed in one call
- `max_input_tokens` (int, optional but recommended): per-text token limit (whatsrust will truncate)

**Timing constraint:** whatsrust calls this **once at construction** with a **10-second timeout**.
If the model is still loading when `model_info` is called, it's OK to block up to ~10s to finish
loading, then return `model_info`. Do NOT return `model_info` with placeholder values before the
model is ready — whatsrust caches this response and trusts it for all subsequent `embed` calls.

#### Method 2: `embed` with `{texts: string[]}`

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "embed",
  "params": {
    "texts": ["hello world", "semantic search"]
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "vectors": [
      [0.123, -0.456, 0.789, ...],   // 384 floats
      [0.321, 0.654, -0.987, ...]    // 384 floats
    ],
    "model_id": "paraphrase-multilingual-MiniLM-L12-v2",
    "dim": 384
  }
}
```

**Fields:**
- `vectors` (float[][]): one vector per input text, **in input order**
- `model_id` (string): MUST echo the model_id from `model_info` **in every response**
- `dim` (int): MUST echo the dim from `model_info` **in every response**

**Trust-but-verify validation (whatsrust applies this to EVERY response):**
1. `model_id` matches the cached `model_info.model_id` → mismatch = transport failure
2. `dim` matches the cached `model_info.dim` → mismatch = transport failure
3. `vectors.len() == texts.len()` → mismatch = transport failure
4. Every vector's length == `dim` → mismatch = transport failure

**If validation fails:** whatsrust rejects the batch as a transport failure; rows stay `pending`;
nothing is stored. Silent garbage poisons search worse than delayed embedding.

**Empty input:** `texts=[]` → `vectors=[]` is valid (zero-length batch).

**Normalization:** The model (`paraphrase-multilingual-MiniLM-L12-v2`) produces L2-normalized
embeddings by default via `sentence-transformers`. **No additional normalization needed** — just
return the model's output as-is. (Cosine similarity works regardless, but normalization is
standard for this model.)

**Prefix constraint (MVP):** This model is **prefix-free** (no special `"query:"` or `"passage:"`
tokens). Embed queries and documents with the SAME call. Models that need asymmetric prefixes
(e5) are out of scope until whatsrust's protocol grows a query/document hint (see §7 Future).

#### Method 3: `health` (no params)

**Request:**
```json
{"jsonrpc": "2.0", "id": 3, "method": "health"}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "status": "ok"
  }
}
```

OR while loading:
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "status": "loading",
    "detail": "Loading sentence-transformers model..."
  }
}
```

OR on error:
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "status": "error",
    "detail": "CUDA out of memory"
  }
}
```

**Status values:**
- `ok`: sidecar is ready to embed
- `loading`: model is loading (takes seconds) — whatsrust WAITS (up to ~60s continuous timeout per ADR 0024/0015 B4), does NOT fall back to FTS5 yet
- `error`: sidecar is broken → whatsrust falls back to FTS5

**Lifecycle:** Emit `loading` while the model is loading, then switch to `ok` once ready.

---

## 3. Build Steps (Python Implementation)

### 3.1 Prerequisites

- Python 3.8+ (3.10+ recommended)
- pip or uv

### 3.2 Create the Sidecar Script

**File:** `embedder-sidecar.py` (place this in whatsrust's repo root or a `scripts/` dir)

```python
#!/usr/bin/env python3
"""
whatsrust embedder sidecar MVP (ADR 0006/0024)
Model: paraphrase-multilingual-MiniLM-L12-v2 (384-dim, prefix-free, multilingual)
"""

import sys
import json
from sentence_transformers import SentenceTransformer

MODEL_NAME = "paraphrase-multilingual-MiniLM-L12-v2"
DIM = 384
MAX_BATCH = 32
MAX_INPUT_TOKENS = 256

def write_response(response):
    """Write JSON response to stdout and flush."""
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()

def main():
    # Load model (blocks here during startup, which is fine — model_info will wait)
    model = SentenceTransformer(MODEL_NAME)
    
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            write_response({
                "jsonrpc": "2.0",
                "id": None,
                "error": {"code": -32700, "message": f"parse error: {e}"}
            })
            continue
        
        req_id = req.get("id")
        method = req.get("method", "")
        
        try:
            if method == "model_info":
                result = {
                    "model_id": MODEL_NAME,
                    "dim": DIM,
                    "max_batch": MAX_BATCH,
                    "max_input_tokens": MAX_INPUT_TOKENS
                }
            elif method == "embed":
                texts = req.get("params", {}).get("texts", [])
                # Model returns numpy arrays; convert to lists of Python floats
                embeddings = model.encode(texts, convert_to_tensor=False)
                vectors = [embedding.tolist() for embedding in embeddings]
                result = {
                    "vectors": vectors,
                    "model_id": MODEL_NAME,
                    "dim": DIM
                }
            elif method == "health":
                # Once we reach here, model is loaded
                result = {"status": "ok"}
            else:
                write_response({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {"code": -32601, "message": f"method not found: {method}"}
                })
                continue
            
            write_response({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": result
            })
        
        except Exception as e:
            write_response({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32000, "message": f"internal error: {e}"}
            })

if __name__ == "__main__":
    main()
```

**Make it executable:**
```bash
chmod +x embedder-sidecar.py
```

### 3.3 Install Dependencies

**Option A: venv (traditional)**
```bash
cd /path/to/whatsrust
python3 -m venv .venv-embedder
source .venv-embedder/bin/activate
pip install sentence-transformers
```

**Option B: uv (faster, recommended)**
```bash
pip install uv  # if not already installed
uv venv .venv-embedder
source .venv-embedder/bin/activate
uv pip install sentence-transformers
```

**Dependencies installed:**
- `sentence-transformers` (pulls in PyTorch, transformers, etc.)
- PyTorch CPU backend (default; GPU not needed for MVP, but works if available)

**Verify installation:**
```bash
python3 -c "from sentence_transformers import SentenceTransformer; print('OK')"
```

**First-run model download:** The first time the script runs, `sentence-transformers` will
download the model weights (~420 MB) to `~/.cache/huggingface/hub/`. This takes ~30-60s
depending on network speed. Subsequent runs are instant.

---

## 4. Wiring into whatsrust

### 4.1 Environment Variables

whatsrust spawns the sidecar via two env vars (read by `StdioEmbedder::from_env()` in `src/embedder.rs`):

1. **`WHATSRUST_EMBEDDER_CMD`** (required): path to the sidecar executable
2. **`WHATSRUST_EMBEDDER_ARGS`** (optional): whitespace-separated args (no quoting support, simple split)

**Example (venv on macOS/Linux):**
```bash
export WHATSRUST_EMBEDDER_CMD="/path/to/whatsrust/.venv-embedder/bin/python3"
export WHATSRUST_EMBEDDER_ARGS="/path/to/whatsrust/embedder-sidecar.py"
```

OR as a wrapper script (recommended for production):
```bash
# File: scripts/run-embedder.sh
#!/bin/bash
cd "$(dirname "$0")/.."
exec .venv-embedder/bin/python3 embedder-sidecar.py
```
```bash
chmod +x scripts/run-embedder.sh
export WHATSRUST_EMBEDDER_CMD="/path/to/whatsrust/scripts/run-embedder.sh"
```

**Absence = semantic search disabled:** If `WHATSRUST_EMBEDDER_CMD` is unset, `StdioEmbedder::from_env()`
returns `Err` and whatsrust runs in pure FTS5 mode (no errors, no blocking — graceful degradation).

### 4.2 Test Standalone First

Before wiring into whatsrust, test the sidecar in isolation:

```bash
# In a terminal:
source .venv-embedder/bin/activate
python3 embedder-sidecar.py
```

Then pipe in test requests (one JSON object per line):
```bash
echo '{"jsonrpc":"2.0","id":1,"method":"model_info"}' | python3 embedder-sidecar.py
```

Expected output (model_info):
```json
{"jsonrpc": "2.0", "id": 1, "result": {"model_id": "paraphrase-multilingual-MiniLM-L12-v2", "dim": 384, "max_batch": 32, "max_input_tokens": 256}}
```

Test embed:
```bash
echo '{"jsonrpc":"2.0","id":2,"method":"embed","params":{"texts":["hello","world"]}}' | python3 embedder-sidecar.py
```

Expected: `vectors` array with 2 vectors of 384 floats each, plus echoed `model_id` and `dim`.

Test health:
```bash
echo '{"jsonrpc":"2.0","id":3,"method":"health"}' | python3 embedder-sidecar.py
```

Expected: `{"status": "ok"}`

---

## 5. Acceptance / Test Plan

### 5.1 Standalone Protocol Smoke Test

✅ **Pass criteria:**
1. `model_info` returns the correct model_id (`paraphrase-multilingual-MiniLM-L12-v2`), dim (384), and optional limits
2. `embed` with 2 texts returns 2 vectors, each 384 floats, with echoed model_id+dim
3. `health` returns `{"status": "ok"}`
4. All responses are newline-delimited JSON with matching `id` fields
5. Invalid method returns a JSON-RPC error response

**Run the standalone tests from §4.2.** If they pass, the protocol is correct.

### 5.2 Integration with whatsrust (End-to-End)

**Setup:**
1. Build whatsrust with the M2 drain worker merged (or the branch that implements it)
2. Set `WHATSRUST_EMBEDDER_CMD` and optionally `WHATSRUST_EMBEDDER_ARGS` as in §4.1
3. Start the whatsrust daemon (`./target/debug/whatsrust daemon`)

**Test 1: Sidecar spawns and answers model_info**
- **Expected:** Daemon starts without errors; logs show embedder model_id + dim if logging is enabled
- **Verify:** `grep -i "embed\|model" <log-file>` (or check stdout if verbose)

**Test 2: Drain worker embeds pending rows**
- **Setup:** Have some `messages` rows with `embed_status='pending'` (from backfill or live messages)
- **Trigger:** Let the drain worker run (it should auto-drain pending rows)
- **Verify:** Query `SELECT COUNT(*) FROM embeddings WHERE model_id = 'paraphrase-multilingual-MiniLM-L12-v2';` — count increases as drain runs
- **Verify:** `SELECT COUNT(*) FROM messages WHERE embed_status = 'pending';` — count decreases

**Test 3: Semantic search returns reranked results**
- **Setup:** Have some embedded messages in Hebrew/Arabic/English
- **Query:** Use `/api/search?q=<semantic-query>&chat_jid=...` (or MCP `whatsrust_search`)
- **Expected:** Results are reranked by cosine similarity (check the `content` field — semantically similar messages rank higher than exact-match-only)
- **Baseline:** Compare with FTS5-only results (unset `WHATSRUST_EMBEDDER_CMD`, restart daemon, same query) — semantic should surface different/better results

**Test 4: Graceful fallback when sidecar is absent/broken**
- **Scenario A (absent):** Unset `WHATSRUST_EMBEDDER_CMD`, restart daemon
  - **Expected:** Daemon starts fine, semantic search is skipped, FTS5 search works normally (no errors, no blocking)
- **Scenario B (broken):** Set `WHATSRUST_EMBEDDER_CMD` to `/bin/false` (exits immediately), restart daemon
  - **Expected:** Daemon starts (construction failure is non-fatal per `src/embedder.rs:310`), embedder is unavailable, FTS5 fallback works

**Test 5: Trust-but-verify rejects mismatches**
- **Scenario:** Modify the sidecar to return wrong `dim` (e.g., `"dim": 128`) in an `embed` response
- **Expected:** whatsrust logs a transport failure, the batch is rejected, rows stay `pending`
- **Verify:** `embeddings` table does NOT gain corrupt rows (count stays the same or only correct batches are inserted)

### 5.3 Compatibility Bar

The sidecar passes if whatsrust's own `StdioEmbedder::verify_embed_response()` accepts every
response. That function (in `src/embedder.rs:370-404`) is the reference implementation of
trust-but-verify validation. If the sidecar violates any of those checks, whatsrust will reject
the batch — use that as the ground truth for "is my sidecar compliant?"

---

## 6. Live Validation on Real Data (Hebrew/Arabic)

**Goal:** Prove the multilingual model handles the user's actual data (Israeli +972 numbers,
Hebrew/Arabic/English mixed).

**Test queries (use real whatsrust data or synthetic if unavailable):**
1. **Hebrew query:** `"פגישה"` (meeting) — should match messages about meetings even if exact word differs
2. **Arabic query:** `"مرحبا"` (hello) — should match greetings
3. **Cross-lingual:** Query in English `"when is the meeting"` — should match Hebrew/Arabic messages about meetings (semantic cross-lingual works for free)
4. **Mixed-script:** A message with both Hebrew and English — should rank appropriately

**Known caveat (expected, not a blocker):**
- CJK/Thai lexical search degrades under FTS5's `unicode61` tokenizer (whole-message token) — semantic search fixes this (ADR 0018).
- If switching to bge-m3 later, the drain will re-embed all pending rows under the new model_id. The pathological-pending circuit breaker (ADR 0015) will transiently trip if >100k pending rows exist. This is expected and recoverable (just wait for the drain to finish).

---

## 7. Future / Evolution (NOT MVP Scope)

### 7.1 Upgrade to bge-m3 (cheap switch)

**Why:** Stronger multilingual model, 1024-dim (2.7× larger storage, but better quality).

**Model:** `BAAI/bge-m3` (prefix-free, multilingual, supports 100+ languages)

**How to switch (per ADR 0017):**
1. Update the sidecar script: change `MODEL_NAME` to `"BAAI/bge-m3"`, `DIM` to `1024`
2. Restart whatsrust — the drain worker will see new `model_id` and auto-drain pending rows under the new model
3. Old `paraphrase-multilingual-MiniLM-L12-v2` vectors remain in `embeddings` table as cold storage (instant switch-back if needed)
4. Purge old model vectors when confident: `/api/purge-embeddings?model_id=paraphrase-multilingual-MiniLM-L12-v2` (explicit, deliberate, destructive but reapplicable)

**Cost:** Free + reversible (ADR 0017). No schema change. Old vectors are pure cold storage (never queried).

### 7.2 Reimplement in Rust/Go

**Why:** Eliminate Python dependency, faster startup, smaller binary.

**Options:**
- Rust: `candle` (Hugging Face's Rust ML framework) or `tract` (ONNX runtime)
- Go: `onnxruntime-go` (ONNX runtime bindings)

**Protocol:** Identical JSON-RPC over stdio (no whatsrust code changes — just swap the `WHATSRUST_EMBEDDER_CMD`).

### 7.3 Asymmetric-Prefix Models (e5, e5-mistral)

**Blocked by:** whatsrust's protocol is symmetric (the same `embed` call for queries and documents). Models like `multilingual-e5-base` need `"query: <text>"` for queries and `"passage: <text>"` for documents.

**To support e5:**
1. Extend the `embed` protocol: add a `query_mode: bool` param (or `role: "query"|"document"`)
2. Update `StdioEmbedder` and the sidecar to handle the hint
3. Update the drain worker (always `query_mode=false`) and search (always `query_mode=true`)

**Decision:** Defer until a prefix-asymmetric model is actually desired. MiniLM and bge-m3 are prefix-free and sufficient for MVP+evolution.

---

## 8. Code-vs-ADR-0024 Discrepancy Check

**Verdict:** NO discrepancies found. ADR 0024 accurately documents the protocol as implemented in `src/embedder.rs` and `src/bin/fake-embedder.rs`. The field names, JSON shapes, validation rules, and lifecycle (`loading`→`ok`) all match exactly.

**Minor clarifications in the code (not discrepancies):**
- Max line length (16 MiB) is in the code (`src/embedder.rs:186`) but not in ADR 0024 — this is a sanity bound added in a code review (ADR 0024 v3-#7), documented in the code comments.
- Model-info timeout (10s) is in the code (`src/embedder.rs:190`) but not in ADR 0024 — again, a code-level detail.

---

## 9. Extracted Protocol Contract (Summary)

For quick reference, the three methods' exact JSON shapes as found in the code:

### `model_info` → `{model_id, dim, max_batch?, max_input_tokens?}`
```json
{
  "jsonrpc": "2.0",
  "id": <int>,
  "result": {
    "model_id": <string>,
    "dim": <int>,
    "max_batch": <int, optional>,
    "max_input_tokens": <int, optional>
  }
}
```

### `embed {texts: string[]}` → `{vectors: float[][], model_id, dim}`
```json
{
  "jsonrpc": "2.0",
  "id": <int>,
  "params": {"texts": [<string>, ...]},
  ...
}
→
{
  "jsonrpc": "2.0",
  "id": <int>,
  "result": {
    "vectors": [[<float>, ...], ...],  // same count as texts, each vector length == dim
    "model_id": <string>,              // MUST match model_info
    "dim": <int>                       // MUST match model_info
  }
}
```

### `health` → `{status: "ok"|"loading"|"error", detail?}`
```json
{
  "jsonrpc": "2.0",
  "id": <int>,
  "result": {
    "status": <"ok"|"loading"|"error">,
    "detail": <string, optional>
  }
}
```

---

## Status: DONE

- [x] Created spec document: `/Users/gila/prv/whatsrust/docs/plans/2026-08-30-embedder-sidecar-mvp.md`
- [x] Extracted exact protocol contract from `src/embedder.rs` + `src/bin/fake-embedder.rs` + ADR 0024
- [x] Documented MVP decision (MiniLM, Python, prefix-free, multilingual)
- [x] Provided concrete build steps (Python script + venv setup)
- [x] Documented wiring into whatsrust (env vars, graceful degradation)
- [x] Specified acceptance tests (standalone protocol + E2E + trust-but-verify)
- [x] Included live validation plan (Hebrew/Arabic real data)
- [x] Documented evolution path (bge-m3, Rust/Go, asymmetric-prefix)
- [x] Verified no code-vs-ADR-0024 discrepancies

**Implementer:** Execute this spec in a separate session. Do NOT modify whatsrust code. The protocol
is fixed; the sidecar must speak it exactly as documented here.
