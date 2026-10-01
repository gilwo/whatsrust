#!/usr/bin/env python3
"""Test parity between Rust embedder and Python sentence-transformers."""

import json
import subprocess
import sys
from pathlib import Path
import numpy as np
from sentence_transformers import SentenceTransformer

def cosine_similarity(a, b):
    """Compute cosine similarity between two vectors."""
    a = np.array(a)
    b = np.array(b)
    return np.dot(a, b) / (np.linalg.norm(a) * np.linalg.norm(b))

def embed_rust(model_name, texts, binary_path):
    """Embed texts using Rust embedder."""
    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "embed",
        "params": {"texts": texts}
    }

    result = subprocess.run(
        [binary_path, "--model", model_name],
        input=json.dumps(request) + "\n",
        capture_output=True,
        text=True
    )

    # Skip the stderr line about model starting
    response = json.loads(result.stdout.strip().split('\n')[-1])
    if "error" in response:
        raise RuntimeError(f"Rust embedder error: {response['error']}")

    return response["result"]["vectors"]

# Pin the same weights revision the Rust sidecar loads. bge-m3's `main` only has
# pytorch_model.bin; its safetensors weights live in the conversion revision
# (refs/pr/130). See embedder-rs/src/main.rs.
PINNED_REVISIONS = {"BAAI/bge-m3": "9a0624b896d81da7492a910ffa53731274b6cf3d"}

def embed_python(model_id, texts):
    """Embed texts using Python sentence-transformers."""
    model = SentenceTransformer(model_id, revision=PINNED_REVISIONS.get(model_id))
    embeddings = model.encode(texts, normalize_embeddings=True)
    return embeddings.tolist()

def test_parity(model_name, model_id, texts, binary_path, threshold=0.999):
    """Test parity between Rust and Python embeddings."""
    print(f"\n{'='*70}")
    print(f"Testing {model_name} ({model_id})")
    print(f"{'='*70}")

    print(f"\nEmbedding {len(texts)} texts with Rust...")
    rust_vecs = embed_rust(model_name, texts, binary_path)

    print(f"Embedding {len(texts)} texts with Python...")
    python_vecs = embed_python(model_id, texts)

    print(f"\nParity Results (threshold: {threshold}):")
    print(f"{'Text':<40} {'Cosine':>10} {'Pass':>6}")
    print("-" * 60)

    all_pass = True
    for i, text in enumerate(texts):
        cos = cosine_similarity(rust_vecs[i], python_vecs[i])
        passed = cos >= threshold
        all_pass = all_pass and passed

        text_short = text[:37] + "..." if len(text) > 40 else text
        status = "✓" if passed else "✗"
        print(f"{text_short:<40} {cos:>10.6f} {status:>6}")

    print("-" * 60)
    if all_pass:
        print(f"✓ All texts pass parity check (cosine >= {threshold})")
    else:
        print(f"✗ Some texts failed parity check")
        return False

    return True

def main():
    if len(sys.argv) < 2:
        print("Usage: test_embedder_parity.py <path-to-rust-binary>")
        sys.exit(1)

    binary_path = sys.argv[1]

    # Test texts: English, Hebrew, Arabic, and mixed
    test_texts = [
        "Hello world",
        "שלום עולם",
        "مرحبا بالعالم",
        "This is a longer sentence with multiple words to test tokenization.",
        "חחחח זה מצחיק מאוד",
        "هذا نص طويل لاختبار التمييز الدلالي",
        "Mixed: Hello שלום مرحبا"
    ]

    # Test MiniLM
    minilm_pass = test_parity(
        "minilm",
        "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2",
        test_texts,
        binary_path
    )

    # Test bge-m3
    bgem3_pass = test_parity(
        "bge-m3",
        "BAAI/bge-m3",
        test_texts,
        binary_path
    )

    print(f"\n{'='*70}")
    print("SUMMARY:")
    print(f"  MiniLM: {'PASS' if minilm_pass else 'FAIL'}")
    print(f"  bge-m3: {'PASS' if bgem3_pass else 'FAIL'}")
    print(f"{'='*70}\n")

    sys.exit(0 if (minilm_pass and bgem3_pass) else 1)

if __name__ == "__main__":
    main()
