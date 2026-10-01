//! Native Rust embedder sidecar using candle (ADR 0040)
//!
//! Wire protocol: newline-delimited JSON-RPC 2.0 over stdio
//! Model: paraphrase-multilingual-MiniLM-L12-v2 (384-dim)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use tokenizers::Tokenizer;

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};

// Static model metadata (ADR 0040: answer model_info IMMEDIATELY, never block on loading)
const MODEL_ID: &str = "paraphrase-multilingual-MiniLM-L12-v2";
const DIM: usize = 384;
const MAX_BATCH: usize = 32;
const MAX_INPUT_TOKENS: usize = 256;

// ---------------------------------------------------------------------------
// JSON-RPC wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

#[derive(Debug, Serialize)]
struct ModelInfoResult {
    model_id: &'static str,
    dim: usize,
    max_batch: usize,
    max_input_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct EmbedParams {
    texts: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EmbedResult {
    vectors: Vec<Vec<f32>>,
    model_id: &'static str,
    dim: usize,
}

#[derive(Debug, Serialize)]
struct HealthResult {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

// ---------------------------------------------------------------------------
// Model loading and embedding
// ---------------------------------------------------------------------------

struct EmbedderModel {
    tokenizer: Tokenizer,
    model: BertModel,
    device: Device,
}

impl EmbedderModel {
    /// Load model from HuggingFace Hub (fetch-on-first-run, then cached)
    fn load() -> Result<Self> {
        let device = Device::Cpu;

        // Download model files from HuggingFace Hub
        let api = hf_hub::api::sync::Api::new()?;
        let repo = api.model(format!("sentence-transformers/{}", MODEL_ID));

        // Fetch required files
        let config_path = repo.get("config.json")
            .context("failed to fetch config.json from HuggingFace")?;
        let tokenizer_path = repo.get("tokenizer.json")
            .context("failed to fetch tokenizer.json from HuggingFace")?;
        let weights_path = repo.get("model.safetensors")
            .context("failed to fetch model.safetensors from HuggingFace")?;

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {}", e))?;

        // Load BERT config
        let config_json = std::fs::read_to_string(config_path)
            .context("failed to read config.json")?;
        let config: BertConfig = serde_json::from_str(&config_json)
            .context("failed to parse config.json")?;

        // Load model weights
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)?
        };

        let model = BertModel::load(vb, &config)
            .context("failed to load BERT model")?;

        Ok(Self { tokenizer, model, device })
    }

    /// Embed a batch of texts using sentence-transformers semantics:
    /// tokenize → BERT forward → mean-pool with attention mask → L2-normalize
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Tokenize all texts
        let encodings = self.tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {}", e))?;

        // Build input tensors
        let token_ids: Vec<Vec<u32>> = encodings.iter()
            .map(|enc| enc.get_ids().to_vec())
            .collect();
        let attention_masks: Vec<Vec<u32>> = encodings.iter()
            .map(|enc| enc.get_attention_mask().to_vec())
            .collect();

        // Pad to max sequence length in batch
        let max_len = token_ids.iter().map(|ids| ids.len()).max().unwrap_or(0);
        let batch_size = texts.len();

        let mut padded_ids = vec![vec![0u32; max_len]; batch_size];
        let mut padded_masks = vec![vec![0u32; max_len]; batch_size];

        for (i, (ids, mask)) in token_ids.iter().zip(attention_masks.iter()).enumerate() {
            padded_ids[i][..ids.len()].copy_from_slice(ids);
            padded_masks[i][..mask.len()].copy_from_slice(mask);
        }

        // Flatten for Tensor creation
        let flat_ids: Vec<u32> = padded_ids.into_iter().flatten().collect();
        let flat_masks: Vec<u32> = padded_masks.into_iter().flatten().collect();

        // Create tensors
        let token_ids_tensor = Tensor::from_vec(flat_ids, (batch_size, max_len), &self.device)?;
        let attention_mask_tensor = Tensor::from_vec(flat_masks, (batch_size, max_len), &self.device)?;

        // token_type_ids: all zeros for single sequence (sentence embeddings)
        let token_type_ids = Tensor::zeros((batch_size, max_len), candle_core::DType::U32, &self.device)?;

        // BERT forward pass (input_ids, token_type_ids, attention_mask)
        let embeddings = self.model.forward(&token_ids_tensor, &token_type_ids, Some(&attention_mask_tensor))?;

        // Mean pooling with attention mask
        // embeddings shape: (batch_size, seq_len, hidden_size)
        // attention_mask shape: (batch_size, seq_len)
        let pooled = mean_pool(&embeddings, &attention_mask_tensor)?;

        // L2 normalize
        let normalized = l2_normalize(&pooled)?;

        // Convert to Vec<Vec<f32>>
        let vectors = tensor_to_vec2(&normalized)?;

        Ok(vectors)
    }
}

/// Mean pooling: average token embeddings weighted by attention mask
fn mean_pool(embeddings: &Tensor, attention_mask: &Tensor) -> Result<Tensor> {
    // embeddings: (batch, seq_len, hidden)
    // attention_mask: (batch, seq_len) - U32, need to convert to F32

    // Convert mask to F32 and expand to match embeddings shape: (batch, seq_len, 1)
    let mask = attention_mask.to_dtype(candle_core::DType::F32)?.unsqueeze(2)?;

    // Multiply embeddings by mask
    let masked = embeddings.broadcast_mul(&mask)?;

    // Sum over sequence dimension
    let sum_embeddings = masked.sum(1)?;

    // Sum of mask values (to compute average)
    let sum_mask = mask.sum(1)?;

    // Avoid division by zero
    let sum_mask = sum_mask.clamp(1e-9, f32::MAX)?;

    // Compute mean
    let mean = sum_embeddings.broadcast_div(&sum_mask)?;

    Ok(mean)
}

/// L2 normalization: divide each vector by its L2 norm
fn l2_normalize(tensor: &Tensor) -> Result<Tensor> {
    // tensor shape: (batch, hidden)
    let norms = tensor.sqr()?.sum_keepdim(1)?.sqrt()?;
    let norms = norms.clamp(1e-12, f32::MAX)?; // Avoid division by zero
    Ok(tensor.broadcast_div(&norms)?)
}

/// Convert Tensor to Vec<Vec<f32>>
fn tensor_to_vec2(tensor: &Tensor) -> Result<Vec<Vec<f32>>> {
    let shape = tensor.dims();
    if shape.len() != 2 {
        anyhow::bail!("expected 2D tensor, got shape {:?}", shape);
    }

    let batch_size = shape[0];
    let dim = shape[1];

    // Flatten to 1D, then reshape into Vec<Vec<f32>>
    let flat = tensor.flatten_all()?.to_vec1::<f32>()?;

    let mut result = Vec::with_capacity(batch_size);
    for i in 0..batch_size {
        let start = i * dim;
        let end = start + dim;
        result.push(flat[start..end].to_vec());
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Lazy model loading state
// ---------------------------------------------------------------------------

enum ModelState {
    NotLoaded,
    Loading,
    Loaded(EmbedderModel),
    Error(String),
}

struct LazyModel {
    state: Mutex<ModelState>,
}

impl LazyModel {
    fn new() -> Self {
        Self {
            state: Mutex::new(ModelState::NotLoaded),
        }
    }

    fn get_or_load(&self) -> Result<std::sync::MutexGuard<ModelState>> {
        let mut guard = self.state.lock().unwrap();

        match *guard {
            ModelState::NotLoaded => {
                *guard = ModelState::Loading;
                drop(guard);

                // Load model (this can take time)
                match EmbedderModel::load() {
                    Ok(model) => {
                        let mut g = self.state.lock().unwrap();
                        *g = ModelState::Loaded(model);
                        Ok(g)
                    }
                    Err(e) => {
                        let err_msg = format!("{:#}", e);
                        let mut g = self.state.lock().unwrap();
                        *g = ModelState::Error(err_msg.clone());
                        anyhow::bail!("failed to load model: {}", err_msg)
                    }
                }
            }
            ModelState::Loading => {
                // Another thread is loading; wait and retry
                drop(guard);
                std::thread::sleep(std::time::Duration::from_millis(100));
                self.get_or_load()
            }
            ModelState::Loaded(_) => Ok(guard),
            ModelState::Error(ref e) => {
                anyhow::bail!("model failed to load previously: {}", e)
            }
        }
    }

    fn health_status(&self) -> HealthResult {
        let guard = self.state.lock().unwrap();
        match *guard {
            ModelState::NotLoaded => HealthResult {
                status: "ok",
                detail: Some("model not yet loaded (lazy load on first embed)".to_string()),
            },
            ModelState::Loading => HealthResult {
                status: "loading",
                detail: Some("model loading in progress".to_string()),
            },
            ModelState::Loaded(_) => HealthResult {
                status: "ok",
                detail: None,
            },
            ModelState::Error(ref e) => HealthResult {
                status: "error",
                detail: Some(e.clone()),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Main JSON-RPC server loop
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let model = Arc::new(LazyModel::new());

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error reading stdin: {}", e);
                continue;
            }
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Parse request
        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("parse error: {}", e),
                    }),
                };
                write_response(&mut stdout, &resp)?;
                continue;
            }
        };

        let req_id = req.id.clone().unwrap_or(Value::Null);

        // Dispatch method
        let response = match req.method.as_str() {
            "model_info" => {
                // IMMEDIATE response (ADR 0040 key requirement)
                let result = ModelInfoResult {
                    model_id: MODEL_ID,
                    dim: DIM,
                    max_batch: MAX_BATCH,
                    max_input_tokens: MAX_INPUT_TOKENS,
                };
                JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: Some(serde_json::to_value(result)?),
                    error: None,
                }
            }
            "embed" => {
                match handle_embed(&model, &req.params) {
                    Ok(result) => JsonRpcResponse {
                        jsonrpc: "2.0",
                        id: req_id,
                        result: Some(serde_json::to_value(result)?),
                        error: None,
                    },
                    Err(e) => JsonRpcResponse {
                        jsonrpc: "2.0",
                        id: req_id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32000,
                            message: format!("embed failed: {:#}", e),
                        }),
                    },
                }
            }
            "health" => {
                let result = model.health_status();
                JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: Some(serde_json::to_value(result)?),
                    error: None,
                }
            }
            method => JsonRpcResponse {
                jsonrpc: "2.0",
                id: req_id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("method not found: {}", method),
                }),
            },
        };

        write_response(&mut stdout, &response)?;
    }

    Ok(())
}

fn handle_embed(model: &Arc<LazyModel>, params: &Option<Value>) -> Result<EmbedResult> {
    let params: EmbedParams = match params {
        Some(p) => serde_json::from_value(p.clone())?,
        None => anyhow::bail!("embed requires 'texts' parameter"),
    };

    if params.texts.is_empty() {
        return Ok(EmbedResult {
            vectors: vec![],
            model_id: MODEL_ID,
            dim: DIM,
        });
    }

    // Get or load model (lazy)
    let guard = model.get_or_load()?;
    let embedder = match &*guard {
        ModelState::Loaded(m) => m,
        _ => anyhow::bail!("model not loaded"),
    };

    // Embed
    let vectors = embedder.embed(&params.texts)?;

    Ok(EmbedResult {
        vectors,
        model_id: MODEL_ID,
        dim: DIM,
    })
}

fn write_response(stdout: &mut std::io::Stdout, response: &JsonRpcResponse) -> Result<()> {
    let json = serde_json::to_string(response)?;
    writeln!(stdout, "{}", json)?;
    stdout.flush()?;
    Ok(())
}
