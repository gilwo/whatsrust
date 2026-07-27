//! Minimal fake embedder sidecar (ADR 0025 seam 3, M2.1.8). Test/dev only — gated
//! behind the `fake-embedder` Cargo feature so it never ships in a default or
//! release build of the primary `whatsrust` binary.
//!
//! Speaks the same newline-delimited JSON-RPC 2.0 wire format as a real embedder
//! sidecar (ADR 0024): reads `model_info` / `embed` / `health` requests from
//! stdin, writes deterministic responses to stdout. `embed` returns
//! `vec![0.1 * (index+1); DIM]` per input text so tests can assert exact output
//! without a real ML model.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

const MODEL_ID: &str = "fake-embedder-v1";
const DIM: usize = 8;

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

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
                let vectors: Vec<Vec<f32>> = texts
                    .iter()
                    .enumerate()
                    .map(|(i, _)| vec![0.1 * (i + 1) as f32; DIM])
                    .collect();
                json!({"vectors": vectors, "model_id": MODEL_ID, "dim": DIM})
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
