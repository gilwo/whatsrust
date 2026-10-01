//! Native Rust embedder sidecar using candle (ADR 0040)
//!
//! Wire protocol: newline-delimited JSON-RPC 2.0 over stdio
//! Supports: paraphrase-multilingual-MiniLM-L12-v2 (384-dim) [default]
//!           BAAI/bge-m3 (1024-dim)

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use tokenizers::Tokenizer;

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use candle_transformers::models::xlm_roberta::{XLMRobertaModel, Config as XLMRobertaConfig};

// ---------------------------------------------------------------------------
// Model selection and metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelType {
    MiniLM,
    BgeM3,
}

impl std::str::FromStr for ModelType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "minilm" => Ok(ModelType::MiniLM),
            "bge-m3" | "bgem3" => Ok(ModelType::BgeM3),
            _ => anyhow::bail!("unknown model type: {} (valid: minilm, bge-m3)", s),
        }
    }
}

impl ModelType {
    fn model_id(&self) -> &'static str {
        match self {
            ModelType::MiniLM => "paraphrase-multilingual-MiniLM-L12-v2",
            ModelType::BgeM3 => "BAAI/bge-m3",
        }
    }

    fn dim(&self) -> usize {
        match self {
            ModelType::MiniLM => 384,
            ModelType::BgeM3 => 1024,
        }
    }

    fn max_input_tokens(&self) -> usize {
        match self {
            ModelType::MiniLM => 256,
            ModelType::BgeM3 => 512, // Chat messages are typically short; longer input = more CPU cost
        }
    }

    fn hf_repo_id(&self) -> String {
        // HuggingFace repository ID (organization/model format)
        match self {
            ModelType::MiniLM => format!("sentence-transformers/{}", self.model_id()),
            ModelType::BgeM3 => self.model_id().to_string(), // Already in "BAAI/bge-m3" format
        }
    }

    fn max_batch(&self) -> usize {
        match self {
            // MiniLM is fast enough for larger batches
            ModelType::MiniLM => 32,
            // bge-m3 is slower; limit batch size to keep latency under 2s
            // (32-text batch takes ~7s, would timeout concurrent searches)
            ModelType::BgeM3 => 8,
        }
    }
}

// bge-m3's `main` revision ships only `pytorch_model.bin` (no safetensors).
// A safetensors conversion lives on the open Hugging Face PR `refs/pr/130`.
// We resolve files by *symbolic* revision name ("main" / "refs/pr/130"), not
// by raw commit hash: hf-hub's on-disk cache indexes revisions via a
// `refs/<revision>` pointer file (e.g. `refs/main`, `refs/refs/pr/130`). A
// bare commit hash has no such ref file, so resolving by hash alone would
// miss the cache and force a full re-download even when the blob is already
// present. Resolving by symbolic name lets hf-hub reuse the cache (or
// download on a cold cache) exactly like it does for MiniLM. To keep the
// build reproducible despite using symbolic names, we verify after the fact
// that the resolved commit matches the hash we pinned below; a mismatch
// (e.g. upstream force-pushed `main`) fails loudly instead of drifting
// silently.
const BGE_M3_MAIN_REVISION: &str = "main";
const BGE_M3_SAFETENSORS_REVISION: &str = "refs/pr/130";
const BGE_M3_MAIN_COMMIT: &str = "5617a9f61b028005a4858fdac845db406aefb181";
const BGE_M3_SAFETENSORS_COMMIT: &str = "9a0624b896d81da7492a910ffa53731274b6cf3d";

#[derive(Parser, Debug)]
#[command(name = "whatsrust-embedder")]
#[command(about = "Native Rust embedder sidecar using candle")]
struct Args {
    /// Model to use: minilm (default) or bge-m3
    #[arg(long, default_value = "minilm", env = "WHATSRUST_EMBEDDER_MODEL")]
    model: ModelType,

    /// Debug/test-only: resolve pinned-revision files via hf-hub (fetching
    /// small files like config.json if not already cached) and verify the
    /// commit-hash pins, without loading the model or fetching the
    /// multi-GB safetensors weights. Prints a one-line JSON summary to
    /// stdout and exits. Not part of the JSON-RPC server protocol; used by
    /// tests to prove cache-hit and cold-cache-download behavior cheaply.
    #[arg(long, hide = true)]
    resolve_only: bool,
}

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
    model_id: String,
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
    model_id: String,
    dim: usize,
}

#[derive(Debug, Serialize)]
struct HealthResult {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

// ---------------------------------------------------------------------------
// HuggingFace Hub resolution helpers
// ---------------------------------------------------------------------------
//
// We resolve files through `hf_hub::Cache` (the on-disk cache index) plus our
// own small downloader, rather than `hf_hub::api::sync::Api::download()`.
// Reason: HuggingFace's edge cache currently answers some
// `.../resolve/<revision>/<file>` requests with a 307 whose `Location`
// header is a *relative* URL (verified independently with `curl`, so this is
// HF-side behavior, not a sandboxing artifact). hf-hub 0.3.2's own
// `Api::download()` manually replays that redirect via
// `self.client.get(<raw Location header>)`, which requires an absolute URL
// and panics-via-`Result::Err` on a relative one -- breaking *any* cold-cache
// download (affects MiniLM too, not just bge-m3; it's simply been masked
// because this machine already has both models cached). A plain HTTP GET
// (letting the client follow redirects itself, as `ureq` does by default)
// has no such problem, and we don't need the failing manual redirect hop at
// all: the headers we actually need (`x-repo-commit`, etag) are present on
// the *first* response, before any redirect is followed.
//
// This also fixes the original defect this module was written to fix:
// resolving revisions purely from the on-disk cache index (`Cache::repo(...).get()`)
// means a warm cache is reused with zero network calls, and a cold cache
// falls through to our downloader -- for both minilm and bge-m3 alike.

/// Build the HuggingFace Hub cache handle, honoring `HF_HUB_CACHE` in
/// addition to hf-hub's built-in `HF_HOME` handling. `HF_HUB_CACHE`, when
/// set, points directly at the `hub/` cache directory (matching the
/// behavior of the official Python `huggingface_hub` library, where it
/// overrides `$HF_HOME/hub`). Without it, this falls back to hf-hub's
/// default (`$HF_HOME/hub`, or `~/.cache/huggingface/hub`).
fn build_hf_cache() -> hf_hub::Cache {
    match std::env::var("HF_HUB_CACHE") {
        Ok(dir) => hf_hub::Cache::new(std::path::PathBuf::from(dir)),
        Err(_) => hf_hub::Cache::default(),
    }
}

/// Resolve `filename` from `repo_id`@`revision`: reuse the on-disk cache
/// entry if present (no network), otherwise download it and populate the
/// cache (so this call and hf-hub's own cache lookups both find it next
/// time). Returns `Ok(None)` on a 404 (file doesn't exist at that
/// revision) so callers can fall back to another filename, e.g.
/// `pytorch_model.bin` when `model.safetensors` is absent.
fn try_resolve_file(
    cache: &hf_hub::Cache,
    repo_id: &str,
    revision: &str,
    filename: &str,
) -> Result<Option<std::path::PathBuf>> {
    let repo = hf_hub::Repo::with_revision(
        repo_id.to_string(),
        hf_hub::RepoType::Model,
        revision.to_string(),
    );
    let cache_repo = cache.repo(repo.clone());

    // Warm cache: the `refs/<revision>` pointer file resolves to a snapshot
    // that already has this file on disk. No network involved.
    if let Some(path) = cache_repo.get(filename) {
        return Ok(Some(path));
    }

    // Cold cache: download directly (see module comment above for why we
    // don't use hf-hub's own `Api::download()` here).
    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        repo.url(),
        repo.url_revision(),
        filename
    );

    let agent = ureq::AgentBuilder::new().build();
    let mut request = agent.get(&url);
    if let Some(token) = cache.token() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }

    let response = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("failed to download {filename} from {repo_id}@{revision}"))
        }
    };

    let commit_hash = response
        .header("x-repo-commit")
        .ok_or_else(|| anyhow::anyhow!("missing x-repo-commit header for {filename}"))?
        .to_string();

    let repo_dir = cache.path().join(repo.folder_name());
    let snapshot_dir = repo_dir.join("snapshots").join(&commit_hash);
    std::fs::create_dir_all(&snapshot_dir)
        .with_context(|| format!("failed to create snapshot dir {:?}", snapshot_dir))?;

    let dest = snapshot_dir.join(filename);
    let tmp_dest = snapshot_dir.join(format!("{filename}.tmp-{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp_dest)
            .with_context(|| format!("failed to create {:?}", tmp_dest))?;
        std::io::copy(&mut response.into_reader(), &mut file)
            .with_context(|| format!("failed to write downloaded {filename} to {:?}", tmp_dest))?;
    }
    std::fs::rename(&tmp_dest, &dest)
        .with_context(|| format!("failed to finalize download at {:?}", dest))?;

    // Record the revision -> commit mapping so future lookups (ours or
    // hf-hub's own `Api`) hit the cache without any network round-trip.
    cache_repo
        .create_ref(&commit_hash)
        .with_context(|| format!("failed to write ref for {repo_id}@{revision}"))?;

    Ok(Some(dest))
}

/// Like `try_resolve_file`, but treats a missing file as an error (for files
/// that must exist, e.g. config.json/tokenizer.json).
fn resolve_file(
    cache: &hf_hub::Cache,
    repo_id: &str,
    revision: &str,
    filename: &str,
) -> Result<std::path::PathBuf> {
    try_resolve_file(cache, repo_id, revision, filename)?
        .with_context(|| format!("{filename} not found at {repo_id}@{revision}"))
}

/// Extract the `<commit_hash>` path component from a hf-hub cache path of
/// the form `.../snapshots/<commit_hash>/<filename>`.
fn extract_snapshot_commit(path: &std::path::Path) -> Option<String> {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == "snapshots" {
            return components
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned());
        }
    }
    None
}

/// Verify a resolved file path actually came from the commit we pinned.
/// This guards against the symbolic revisions used in
/// `resolve_bge_m3_files` (`main`, `refs/pr/130`) drifting to a different
/// commit upstream (e.g. a force-push) without us noticing.
fn verify_pinned_commit(path: &std::path::Path, expected_commit: &str, what: &str) -> Result<()> {
    let actual = extract_snapshot_commit(path).with_context(|| {
        format!(
            "could not determine resolved commit hash for {} from path {:?}",
            what, path
        )
    })?;
    if actual != expected_commit {
        anyhow::bail!(
            "pin mismatch for {}: expected commit {} but resolved to {} (path: {:?}). \
The pinned revision has moved upstream; after reviewing the change, update the \
expected commit hash in embedder-rs/src/main.rs.",
            what,
            expected_commit,
            actual,
            path
        );
    }
    Ok(())
}

/// Resolve bge-m3's config/tokenizer/weights files for production loading.
/// See the module comment above for why two revisions are used and why
/// they're resolved by symbolic name with a pin check rather than by raw
/// commit hash.
fn resolve_bge_m3_files(
    cache: &hf_hub::Cache,
) -> Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)> {
    let repo_id = ModelType::BgeM3.hf_repo_id();

    // Config/tokenizer from the main branch.
    let config = resolve_file(cache, &repo_id, BGE_M3_MAIN_REVISION, "config.json")
        .context("failed to fetch bge-m3 config.json from main revision")?;
    verify_pinned_commit(&config, BGE_M3_MAIN_COMMIT, "bge-m3 config.json (main)")?;

    let tokenizer = resolve_file(cache, &repo_id, BGE_M3_MAIN_REVISION, "tokenizer.json")
        .context("failed to fetch bge-m3 tokenizer.json from main revision")?;
    verify_pinned_commit(&tokenizer, BGE_M3_MAIN_COMMIT, "bge-m3 tokenizer.json (main)")?;

    // Safetensors weights from the conversion PR branch.
    let weights = resolve_file(
        cache,
        &repo_id,
        BGE_M3_SAFETENSORS_REVISION,
        "model.safetensors",
    )
    .context("failed to fetch bge-m3 model.safetensors from refs/pr/130")?;
    verify_pinned_commit(
        &weights,
        BGE_M3_SAFETENSORS_COMMIT,
        "bge-m3 model.safetensors (refs/pr/130)",
    )?;

    Ok((config, tokenizer, weights))
}

/// Debug/test-only (`--resolve-only`): prove file resolution works at
/// bge-m3's two pinned revisions -- from either a warm cache (no network) or
/// a cold cache (downloads) -- without fetching the 2.1GB safetensors
/// weights. Fetches `config.json` at both `main` and `refs/pr/130` (both
/// small files) and applies the same pin check `resolve_bge_m3_files` uses
/// for the real weights file.
fn resolve_bge_m3_probe(
    cache: &hf_hub::Cache,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let repo_id = ModelType::BgeM3.hf_repo_id();

    let main_config = resolve_file(cache, &repo_id, BGE_M3_MAIN_REVISION, "config.json")
        .context("resolve-only: failed to fetch config.json from main revision")?;
    verify_pinned_commit(&main_config, BGE_M3_MAIN_COMMIT, "bge-m3 config.json (main)")?;

    let pr_config = resolve_file(cache, &repo_id, BGE_M3_SAFETENSORS_REVISION, "config.json")
        .context("resolve-only: failed to fetch config.json from refs/pr/130")?;
    verify_pinned_commit(
        &pr_config,
        BGE_M3_SAFETENSORS_COMMIT,
        "bge-m3 config.json (refs/pr/130)",
    )?;

    Ok((main_config, pr_config))
}

// ---------------------------------------------------------------------------
// Model loading and embedding
// ---------------------------------------------------------------------------

struct EmbedderModel {
    model_type: ModelType,
    tokenizer: Tokenizer,
    bert_model: Option<BertModel>,
    xlm_roberta_model: Option<XLMRobertaModel>,
    device: Device,
}

impl EmbedderModel {
    /// Load model from HuggingFace Hub (fetch-on-first-run, then cached)
    fn load(model_type: ModelType) -> Result<Self> {
        let device = Device::Cpu;

        let cache = build_hf_cache();

        // For bge-m3, we need two different revisions (see the "HuggingFace
        // Hub resolution helpers" module comment above for why symbolic
        // revisions + a pin check are used instead of raw commit hashes).
        let (config_path, tokenizer_path, weights_path) = match model_type {
            ModelType::MiniLM => {
                // MiniLM has safetensors in main.
                let repo_id = model_type.hf_repo_id();
                let config = resolve_file(&cache, &repo_id, "main", "config.json")
                    .context("failed to fetch config.json from HuggingFace")?;
                let tokenizer = resolve_file(&cache, &repo_id, "main", "tokenizer.json")
                    .context("failed to fetch tokenizer.json from HuggingFace")?;
                let weights = match try_resolve_file(&cache, &repo_id, "main", "model.safetensors")? {
                    Some(path) => path,
                    None => resolve_file(&cache, &repo_id, "main", "pytorch_model.bin")
                        .context("failed to fetch model weights")?,
                };
                (config, tokenizer, weights)
            }
            ModelType::BgeM3 => resolve_bge_m3_files(&cache)?,
        };

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {}", e))?;

        // Load config
        let config_json = std::fs::read_to_string(&config_path)
            .context("failed to read config.json")?;

        // Load model weights (prefer memory-mapped safetensors for lower RAM usage)
        let vb = if weights_path.to_string_lossy().ends_with(".safetensors") {
            unsafe {
                VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)?
            }
        } else {
            // PyTorch format (.bin) - loads entire model into RAM
            VarBuilder::from_pth(&weights_path, candle_core::DType::F32, &device)?
        };

        // Load the appropriate model and config based on type
        let (bert_model, xlm_roberta_model) = match model_type {
            ModelType::MiniLM => {
                let config: BertConfig = serde_json::from_str(&config_json)
                    .context("failed to parse BERT config.json")?;
                let model = BertModel::load(vb, &config)
                    .context("failed to load BERT model")?;
                (Some(model), None)
            }
            ModelType::BgeM3 => {
                let config: XLMRobertaConfig = serde_json::from_str(&config_json)
                    .context("failed to parse XLM-RoBERTa config.json")?;
                let model = XLMRobertaModel::new(&config, vb)
                    .context("failed to load XLM-RoBERTa model")?;
                (None, Some(model))
            }
        };

        Ok(Self {
            model_type,
            tokenizer,
            bert_model,
            xlm_roberta_model,
            device
        })
    }

    /// Embed a batch of texts using appropriate pooling strategy per model
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

        // Forward pass
        let embeddings = match self.model_type {
            ModelType::MiniLM => {
                let model = self.bert_model.as_ref().unwrap();
                model.forward(&token_ids_tensor, &token_type_ids, Some(&attention_mask_tensor))?
            }
            ModelType::BgeM3 => {
                let model = self.xlm_roberta_model.as_ref().unwrap();
                // XLM-RoBERTa has different forward signature
                model.forward(
                    &token_ids_tensor,
                    &attention_mask_tensor,
                    &token_type_ids,
                    None,  // past_key_value
                    None,  // encoder_hidden_states
                    None,  // encoder_attention_mask
                )?
            }
        };

        // Pool based on model type
        let pooled = match self.model_type {
            ModelType::MiniLM => {
                // Mean pooling with attention mask
                mean_pool(&embeddings, &attention_mask_tensor)?
            }
            ModelType::BgeM3 => {
                // CLS token (first token) pooling
                cls_pool(&embeddings)?
            }
        };

        // L2 normalize
        let normalized = l2_normalize(&pooled)?;

        // Convert to Vec<Vec<f32>>
        let vectors = tensor_to_vec2(&normalized)?;

        Ok(vectors)
    }
}

/// Mean pooling: average token embeddings weighted by attention mask
/// Used by MiniLM
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

/// CLS token pooling: extract first token embedding
/// Used by bge-m3 (XLM-RoBERTa architecture)
fn cls_pool(embeddings: &Tensor) -> Result<Tensor> {
    // embeddings: (batch, seq_len, hidden)
    // Extract first token (CLS) for each sequence in batch
    // Use narrow to get [:, 0, :] which gives us (batch, hidden)
    let cls_embeddings = embeddings.narrow(1, 0, 1)?;  // (batch, 1, hidden)
    Ok(cls_embeddings.squeeze(1)?)  // (batch, hidden)
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
// Eager model loading state (background thread at startup)
// ---------------------------------------------------------------------------

enum ModelState {
    Loading,
    Loaded(EmbedderModel),
    Error(String),
}

struct EagerModel {
    model_type: ModelType,
    state: Mutex<ModelState>,
}

impl EagerModel {
    /// Start loading the model on a background thread immediately
    fn new(model_type: ModelType) -> Arc<Self> {
        let model = Arc::new(Self {
            model_type,
            state: Mutex::new(ModelState::Loading),
        });

        let model_clone = Arc::clone(&model);
        std::thread::spawn(move || {
            eprintln!("Background thread: starting model load for {}", model_clone.model_type.model_id());
            match EmbedderModel::load(model_clone.model_type) {
                Ok(loaded) => {
                    eprintln!("Background thread: model loaded successfully");
                    let mut guard = model_clone.state.lock().unwrap();
                    *guard = ModelState::Loaded(loaded);
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    eprintln!("Background thread: model load failed: {}", err_msg);
                    let mut guard = model_clone.state.lock().unwrap();
                    *guard = ModelState::Error(err_msg);
                }
            }
        });

        model
    }

    /// Wait for model to be loaded (or return error if load failed)
    fn wait_for_load(&self) -> Result<std::sync::MutexGuard<'_, ModelState>> {
        loop {
            let guard = self.state.lock().unwrap();
            match &*guard {
                ModelState::Loading => {
                    drop(guard);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                ModelState::Loaded(_) => return Ok(guard),
                ModelState::Error(ref e) => {
                    anyhow::bail!("model failed to load: {}", e)
                }
            }
        }
    }

    fn health_status(&self) -> HealthResult {
        let guard = self.state.lock().unwrap();
        match *guard {
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
// --resolve-only debug entry point (see Args::resolve_only)
// ---------------------------------------------------------------------------

/// Resolve model files via hf-hub and print a one-line JSON summary to
/// stdout, without starting the JSON-RPC server or loading model weights.
/// For bge-m3 this proves both pinned revisions resolve (from cache or by
/// downloading) without touching the multi-GB safetensors file.
fn run_resolve_only(model_type: ModelType) -> Result<()> {
    let cache = build_hf_cache();

    let summary = match model_type {
        ModelType::MiniLM => {
            let repo_id = model_type.hf_repo_id();
            let config = resolve_file(&cache, &repo_id, "main", "config.json")
                .context("resolve-only: failed to fetch config.json")?;
            serde_json::json!({
                "model": model_type.model_id(),
                "config_json": config.display().to_string(),
            })
        }
        ModelType::BgeM3 => {
            let (main_config, pr_config) = resolve_bge_m3_probe(&cache)?;
            serde_json::json!({
                "model": model_type.model_id(),
                "main_revision": BGE_M3_MAIN_REVISION,
                "main_config_json": main_config.display().to_string(),
                "main_expected_commit": BGE_M3_MAIN_COMMIT,
                "safetensors_revision": BGE_M3_SAFETENSORS_REVISION,
                "pr130_config_json": pr_config.display().to_string(),
                "pr130_expected_commit": BGE_M3_SAFETENSORS_COMMIT,
            })
        }
    };

    println!("{}", summary);
    Ok(())
}

// ---------------------------------------------------------------------------
// Main JSON-RPC server loop
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse();
    let model_type = args.model;

    if args.resolve_only {
        return run_resolve_only(model_type);
    }

    eprintln!("whatsrust-embedder starting with model: {} ({})",
              model_type.model_id(),
              model_type.dim());

    // Start eager loading on background thread
    let model = EagerModel::new(model_type);

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
                    model_id: model_type.model_id().to_string(),
                    dim: model_type.dim(),
                    max_batch: model_type.max_batch(),
                    max_input_tokens: model_type.max_input_tokens(),
                };
                JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req_id,
                    result: Some(serde_json::to_value(result)?),
                    error: None,
                }
            }
            "embed" => {
                match handle_embed(&model, &req.params, model_type) {
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

fn handle_embed(model: &Arc<EagerModel>, params: &Option<Value>, model_type: ModelType) -> Result<EmbedResult> {
    let params: EmbedParams = match params {
        Some(p) => serde_json::from_value(p.clone())?,
        None => anyhow::bail!("embed requires 'texts' parameter"),
    };

    if params.texts.is_empty() {
        return Ok(EmbedResult {
            vectors: vec![],
            model_id: model_type.model_id().to_string(),
            dim: model_type.dim(),
        });
    }

    // Wait for model to load (if still loading)
    let guard = model.wait_for_load()?;
    let embedder = match &*guard {
        ModelState::Loaded(m) => m,
        _ => anyhow::bail!("model not loaded"),
    };

    // Embed
    let vectors = embedder.embed(&params.texts)?;

    Ok(EmbedResult {
        vectors,
        model_id: model_type.model_id().to_string(),
        dim: model_type.dim(),
    })
}

fn write_response(stdout: &mut std::io::Stdout, response: &JsonRpcResponse) -> Result<()> {
    let json = serde_json::to_string(response)?;
    writeln!(stdout, "{}", json)?;
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extract_snapshot_commit_finds_hash() {
        let path = PathBuf::from(
            "/home/user/.cache/huggingface/hub/models--BAAI--bge-m3/snapshots/\
5617a9f61b028005a4858fdac845db406aefb181/config.json",
        );
        assert_eq!(
            extract_snapshot_commit(&path),
            Some("5617a9f61b028005a4858fdac845db406aefb181".to_string())
        );
    }

    #[test]
    fn extract_snapshot_commit_missing_returns_none() {
        let path = PathBuf::from("/tmp/some/other/path/config.json");
        assert_eq!(extract_snapshot_commit(&path), None);
    }

    #[test]
    fn verify_pinned_commit_passes_on_match() {
        let path = PathBuf::from(
            "/cache/hub/models--BAAI--bge-m3/snapshots/\
5617a9f61b028005a4858fdac845db406aefb181/config.json",
        );
        assert!(verify_pinned_commit(
            &path,
            "5617a9f61b028005a4858fdac845db406aefb181",
            "test"
        )
        .is_ok());
    }

    #[test]
    fn verify_pinned_commit_fails_on_mismatch() {
        let path = PathBuf::from(
            "/cache/hub/models--BAAI--bge-m3/snapshots/\
deadbeefdeadbeefdeadbeefdeadbeefdeadbeef/config.json",
        );
        let err = verify_pinned_commit(
            &path,
            "5617a9f61b028005a4858fdac845db406aefb181",
            "test",
        )
        .expect_err("expected a pin mismatch error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("5617a9f61b028005a4858fdac845db406aefb181"),
            "error should mention expected commit: {msg}"
        );
        assert!(
            msg.contains("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
            "error should mention actual commit: {msg}"
        );
    }

    #[test]
    fn verify_pinned_commit_fails_when_not_a_snapshot_path() {
        let path = PathBuf::from("/tmp/some/other/path/config.json");
        let err = verify_pinned_commit(&path, "deadbeef", "test")
            .expect_err("expected an error when no snapshots/ component is present");
        let msg = format!("{:#}", err);
        assert!(msg.contains("could not determine resolved commit hash"));
    }
}
