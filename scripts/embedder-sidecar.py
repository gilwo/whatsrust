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
