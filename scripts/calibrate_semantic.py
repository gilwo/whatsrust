#!/usr/bin/env python3
"""
Calibrate semantic recall thresholds for embedding models.

Per ADR 0041, this measures how well each model separates word-free queries
from unrelated messages, to choose optimal similarity floor thresholds.
"""

import argparse
import json
import sqlite3
import subprocess
import sys
from pathlib import Path
from typing import List, Tuple
import numpy as np

def cosine_similarity(a, b):
    """Compute cosine similarity."""
    a, b = np.array(a), np.array(b)
    return np.dot(a, b) / (np.linalg.norm(a) * np.linalg.norm(b))

def embed_texts_rust(model: str, texts: List[str], binary_path: str) -> List[List[float]]:
    """Embed texts using Rust embedder sidecar."""
    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "embed",
        "params": {"texts": texts}
    }

    result = subprocess.run(
        [binary_path, "--model", model],
        input=json.dumps(request) + "\n",
        capture_output=True,
        text=True,
        timeout=60
    )

    if result.returncode != 0:
        raise RuntimeError(f"Rust embedder failed: {result.stderr}")

    response = json.loads(result.stdout.strip().split('\n')[-1])
    if "error" in response:
        raise RuntimeError(f"Embedder error: {response['error']}")

    return response["result"]["vectors"]

def load_messages(db_path: str) -> List[Tuple[int, str]]:
    """Load text messages from database (read-only)."""
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    cursor = conn.cursor()

    cursor.execute("""
        SELECT id, body_text
        FROM messages
        WHERE content_kind = 'text' AND body_text IS NOT NULL
        ORDER BY id
    """)

    messages = [(row[0], row[1]) for row in cursor.fetchall()]
    conn.close()

    return messages

def find_target_in_messages(target_substr: str, messages: List[Tuple[int, str]]) -> int:
    """Find message index containing target substring (case-insensitive)."""
    target_lower = target_substr.lower()
    for i, (msg_id, content) in enumerate(messages):
        if target_lower in content.lower():
            return i
    raise ValueError(f"Target substring '{target_substr}' not found in any message")

def measure_model(model: str, queries: List[Tuple[str, str]], messages: List[Tuple[int, str]],
                  binary_path: str) -> dict:
    """Measure semantic separation for one model."""
    print(f"\n{'='*70}")
    print(f"Model: {model}")
    print(f"{'='*70}")

    # Extract message contents
    message_texts = [content for _, content in messages]

    # Embed all messages
    print(f"Embedding {len(message_texts)} messages...")
    message_vecs = embed_texts_rust(model, message_texts, binary_path)

    # Embed queries
    query_texts = [q for q, _ in queries]
    print(f"Embedding {len(query_texts)} queries...")
    query_vecs = embed_texts_rust(model, query_texts, binary_path)

    # Compute similarities
    print("\nQuery Results:")
    print(f"{'Query':<35} {'Target':<20} {'Cos':>7} {'Rank':>5} {'Above':>6}")
    print("-" * 80)

    all_unrelated_scores = []
    query_results = []

    for (query, target_substr), query_vec in zip(queries, query_vecs):
        target_idx = find_target_in_messages(target_substr, messages)

        # Compute cosines to all messages
        cosines = [cosine_similarity(query_vec, msg_vec) for msg_vec in message_vecs]

        target_cos = cosines[target_idx]

        # Rank (1-based, 1 = best)
        sorted_indices = sorted(range(len(cosines)), key=lambda i: cosines[i], reverse=True)
        target_rank = sorted_indices.index(target_idx) + 1

        # Unrelated messages scoring above target
        above_count = sum(1 for i, cos in enumerate(cosines) if i != target_idx and cos > target_cos)

        # Collect unrelated scores
        unrelated = [cos for i, cos in enumerate(cosines) if i != target_idx]
        all_unrelated_scores.extend(unrelated)

        query_short = (query[:32] + "...") if len(query) > 35 else query
        target_short = (target_substr[:17] + "...") if len(target_substr) > 20 else target_substr

        print(f"{query_short:<35} {target_short:<20} {target_cos:>7.4f} {target_rank:>5} {above_count:>6}")

        query_results.append({
            "query": query,
            "target": target_substr,
            "target_cos": target_cos,
            "target_rank": target_rank,
            "above_count": above_count
        })

    # Unrelated distribution
    unrelated_arr = np.array(all_unrelated_scores)
    print(f"\nUnrelated Distribution:")
    print(f"  Median:  {np.median(unrelated_arr):.4f}")
    print(f"  95th %%:  {np.percentile(unrelated_arr, 95):.4f}")
    print(f"  99th %%:  {np.percentile(unrelated_arr, 99):.4f}")
    print(f"  Max:     {np.max(unrelated_arr):.4f}")

    # Floor thresholds
    floors = [0.25, 0.35, 0.45, 0.55, 0.65]
    print(f"\nThreshold Analysis:")
    print(f"{'Floor':>6}  {'Unrelated %':>13}  {'Targets Pass':>13}")
    print("-" * 35)

    for floor in floors:
        unrelated_pass_pct = 100.0 * (unrelated_arr >= floor).sum() / len(unrelated_arr)
        targets_pass = sum(1 for r in query_results if r["target_cos"] >= floor)

        print(f"{floor:>6.2f}  {unrelated_pass_pct:>12.1f}%  {targets_pass:>7}/{len(query_results)}")

    return {
        "model": model,
        "query_results": query_results,
        "unrelated_stats": {
            "median": float(np.median(unrelated_arr)),
            "p95": float(np.percentile(unrelated_arr, 95)),
            "p99": float(np.percentile(unrelated_arr, 99)),
            "max": float(np.max(unrelated_arr))
        },
        "floors": {
            f"{floor:.2f}": {
                "unrelated_pass_pct": float(100.0 * (unrelated_arr >= floor).sum() / len(unrelated_arr)),
                "targets_pass": int(sum(1 for r in query_results if r["target_cos"] >= floor)),
                "targets_total": len(query_results)
            }
            for floor in floors
        }
    }

def main():
    parser = argparse.ArgumentParser(description="Calibrate semantic recall thresholds")
    parser.add_argument("--db", default="whatsapp.db", help="Path to database")
    parser.add_argument("--binary", required=True, help="Path to Rust embedder binary")
    parser.add_argument("--models", nargs="+", default=["minilm", "bge-m3"],
                        help="Models to test")
    args = parser.parse_args()

    # Word-free query → target-substring pairs
    # CRITICAL: query shares NO words with target (different language, transliteration doesn't count)
    queries = [
        ("summer camp", "קייטנה"),
        ("companies", "חברות"),
        ("wonderful", "מושלם"),
        ("laughter", "חחחח"),
        ("surprise", "Yay"),
        ("pasta sauce", "בולונז"),
        ("is this about the picture", "לתמונה"),
        ("you are crazy", "לא נורמלי"),
        # Additional word-free pairs (verified to exist in db)
        ("related to", "קשור"),
        ("name call", "אלירן"),
        ("save for tomorrow", "שמרי"),
        ("smoke", "Smoke"),
    ]

    print(f"Loading messages from {args.db}...")
    messages = load_messages(args.db)
    print(f"Loaded {len(messages)} text messages")

    results = {}
    for model in args.models:
        results[model] = measure_model(model, queries, messages, args.binary)

    # Side-by-side comparison
    print(f"\n{'='*70}")
    print("SIDE-BY-SIDE COMPARISON")
    print(f"{'='*70}")

    if len(args.models) == 2:
        m1, m2 = args.models
        r1, r2 = results[m1], results[m2]

        print(f"\nUnrelated Distribution:")
        print(f"{'Metric':<12} {m1:>12} {m2:>12}")
        print("-" * 40)
        for key in ["median", "p95", "p99", "max"]:
            print(f"{key:<12} {r1['unrelated_stats'][key]:>12.4f} {r2['unrelated_stats'][key]:>12.4f}")

        print(f"\nThreshold Analysis:")
        print(f"{'Floor':<8} {m1 + ' (unrel%)':>18} {m1 + ' (tgt)':>12} {m2 + ' (unrel%)':>18} {m2 + ' (tgt)':>12}")
        print("-" * 72)
        for floor in [0.25, 0.35, 0.45, 0.55, 0.65]:
            floor_key = f"{floor:.2f}"
            f1, f2 = r1["floors"][floor_key], r2["floors"][floor_key]
            print(f"{floor:<8.2f} {f1['unrelated_pass_pct']:>17.1f}% {f1['targets_pass']:>6}/{f1['targets_total']:>2} "
                  f"{f2['unrelated_pass_pct']:>17.1f}% {f2['targets_pass']:>6}/{f2['targets_total']:>2}")

    # Assessment
    print(f"\n{'='*70}")
    print("ASSESSMENT")
    print(f"{'='*70}")
    if len(args.models) == 2:
        m1, m2 = args.models
        r1, r2 = results[m1], results[m2]

        # Find best floor for each model (target at least ~70% pass, unrelated ≤5%)
        def find_best_floor(r):
            best = None
            for floor in [0.25, 0.35, 0.45, 0.55, 0.65]:
                fk = f"{floor:.2f}"
                f = r["floors"][fk]
                tgt_pct = 100.0 * f["targets_pass"] / f["targets_total"]
                if f["unrelated_pass_pct"] <= 5.0 and tgt_pct >= 70.0:
                    best = (floor, f, tgt_pct)
            return best

        best1 = find_best_floor(r1)
        best2 = find_best_floor(r2)

        print(f"\n{m1}:")
        if best1:
            floor, f, tgt_pct = best1
            print(f"  Best floor: {floor:.2f}")
            print(f"  Targets pass: {tgt_pct:.0f}% ({f['targets_pass']}/{f['targets_total']})")
            print(f"  Unrelated pass: {f['unrelated_pass_pct']:.1f}%")
            print(f"  → USABLE for hybrid recall with floor={floor:.2f}")
        else:
            print(f"  No floor admits ≥70% targets while keeping unrelated ≤5%")
            print(f"  → NOT IDEAL for hybrid recall")

        print(f"\n{m2}:")
        if best2:
            floor, f, tgt_pct = best2
            print(f"  Best floor: {floor:.2f}")
            print(f"  Targets pass: {tgt_pct:.0f}% ({f['targets_pass']}/{f['targets_total']})")
            print(f"  Unrelated pass: {f['unrelated_pass_pct']:.1f}%")
            print(f"  → USABLE for hybrid recall with floor={floor:.2f}")
        else:
            print(f"  No floor admits ≥70% targets while keeping unrelated ≤5%")
            print(f"  → NOT IDEAL for hybrid recall")

        # Winner
        print(f"\nWinner:")
        if best1 and best2:
            # Compare by targets passing at lowest floor
            if best2[1]["targets_pass"] > best1[1]["targets_pass"]:
                print(f"  {m2} passes more targets at an acceptable floor → Better semantic separation")
            elif best1[1]["targets_pass"] > best2[1]["targets_pass"]:
                print(f"  {m1} passes more targets at an acceptable floor → Better semantic separation")
            else:
                print(f"  Tie")
        elif best2:
            print(f"  {m2} is usable, {m1} is not → {m2} wins")
        elif best1:
            print(f"  {m1} is usable, {m2} is not → {m1} wins")
        else:
            print(f"  Neither model achieves acceptable separation")

if __name__ == "__main__":
    main()
