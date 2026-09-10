//! Minimal fake embedder sidecar (ADR 0025 seam 3, M2.1.8). Test/dev only — gated
//! behind the `fake-embedder` Cargo feature so it never ships in a default or
//! release build of the primary `whatsrust` binary.
//!
//! Speaks the same newline-delimited JSON-RPC 2.0 wire format as a real embedder
//! sidecar (ADR 0024): reads `model_info` / `embed` / `health` requests from
//! stdin, writes deterministic responses to stdout. `embed` returns
//! `vec![0.1 * (index+1); DIM]` per input text so tests can assert exact output
//! without a real ML model.
//!
//! Misbehave mode (M2.6.3): set FAKE_EMBEDDER_MISBEHAVE env var to emit malformed
//! responses on demand for validation testing. Supported values:
//! - "wrong_dim": vectors with incorrect dimension (7 instead of 8)
//! - "wrong_count": vector count mismatched with input count (always 1 vector)
//! - "wrong_model_id": model_id in embed response differs from model_info

use std::io::{BufRead, Write};

use serde_json::{json, Value};

const MODEL_ID: &str = "fake-embedder-v1";
const DIM: usize = 8;

enum MisbehaviorMode {
    None,
    WrongDim,
    WrongCount,
    WrongModelId,
}

fn parse_misbehavior() -> MisbehaviorMode {
    match std::env::var("FAKE_EMBEDDER_MISBEHAVE").as_deref() {
        Ok("wrong_dim") => MisbehaviorMode::WrongDim,
        Ok("wrong_count") => MisbehaviorMode::WrongCount,
        Ok("wrong_model_id") => MisbehaviorMode::WrongModelId,
        _ => MisbehaviorMode::None,
    }
}

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let misbehavior = parse_misbehavior();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_line(&mut out, json!({"jsonrpc":"2.0","id":Value::Null,"error":{"code":-32700,"message":format!("parse error: {e}")}}));
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");

        let result = match method {
            "model_info" => json!({"model_id": MODEL_ID, "dim": DIM, "max_batch": 32, "max_input_tokens": 256}),
            "embed" => {
                let texts = req
                    .get("params")
                    .and_then(|p| p.get("texts"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                // Apply misbehavior mode if configured (M2.6.3)
                match misbehavior {
                    MisbehaviorMode::WrongDim => {
                        // Emit vectors with wrong dimension (7 instead of 8)
                        let vectors: Vec<Vec<f32>> = texts
                            .iter()
                            .enumerate()
                            .map(|(i, _)| vec![0.1 * (i + 1) as f32; DIM - 1])
                            .collect();
                        json!({"vectors": vectors, "model_id": MODEL_ID, "dim": DIM})
                    }
                    MisbehaviorMode::WrongCount => {
                        // Emit wrong count (always 1 vector regardless of input count)
                        let vectors: Vec<Vec<f32>> = vec![vec![0.1_f32; DIM]];
                        json!({"vectors": vectors, "model_id": MODEL_ID, "dim": DIM})
                    }
                    MisbehaviorMode::WrongModelId => {
                        // Emit wrong model_id
                        let vectors: Vec<Vec<f32>> = texts
                            .iter()
                            .enumerate()
                            .map(|(i, _)| vec![0.1 * (i + 1) as f32; DIM])
                            .collect();
                        json!({"vectors": vectors, "model_id": "wrong-model-id", "dim": DIM})
                    }
                    MisbehaviorMode::None => {
                        // Normal happy-path behavior
                        let vectors: Vec<Vec<f32>> = texts
                            .iter()
                            .enumerate()
                            .map(|(i, _)| vec![0.1 * (i + 1) as f32; DIM])
                            .collect();
                        json!({"vectors": vectors, "model_id": MODEL_ID, "dim": DIM})
                    }
                }
            }
            "health" => json!({"status": "ok"}),
            _ => {
                write_line(&mut out, json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("method not found: {method}")}}));
                continue;
            }
        };
        write_line(&mut out, json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
}

fn write_line(out: &mut impl Write, v: Value) {
    let _ = writeln!(out, "{}", serde_json::to_string(&v).unwrap());
    let _ = out.flush();
}
