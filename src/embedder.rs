//! Embedder sidecar contract & transport — M2.1 (ADR 0006/0024/0025).
//!
//! Semantic search (M2) delegates text→vector embedding to a separate, stateless
//! sidecar process (ADR 0006). This module defines the transport-neutral contract
//! (`Embedder`) that the rest of whatsrust depends on, plus the real stdio
//! transport (`StdioEmbedder`) and a deterministic in-process fake (`FakeEmbedder`,
//! ADR 0025 seam 1) for tests.
//!
//! Key types:
//!   - `ModelInfo`     — `{model_id, dim, max_batch?, max_input_tokens?}` (ADR 0024)
//!   - `HealthStatus`  — `Ok | Loading | Error(detail)` (ADR 0024)
//!   - `Embedder`      — transport-neutral async trait seam (ADR 0006); dyn-safe via `#[async_trait]`
//!   - `StdioEmbedder` — child-process transport, newline-delimited JSON-RPC 2.0 (ADR 0024)
//!   - `FakeEmbedder`  — deterministic canned-vector fake, no child process (ADR 0025)
//!   - `TransportFailure` — typed error for trust-but-verify rejections (ADR 0024)
//!
//! No drain worker, no DB/schema changes, no bridge wiring here — that is M2.2+.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// ModelInfo / HealthStatus — transport-neutral domain types (ADR 0024)
// ---------------------------------------------------------------------------

/// Model identity + batching limits advertised by the sidecar's `model_info` call
/// (ADR 0024). `max_batch`/`max_input_tokens` are advisory: absent means "no
/// advertised limit" and callers (the future drain worker, M2.3) fall back to
/// their own configured defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model_id: String,
    pub dim: usize,
    pub max_batch: Option<usize>,
    pub max_input_tokens: Option<usize>,
}

/// Sidecar health as reported by the `health` RPC (ADR 0024). `StdioEmbedder`
/// relays this faithfully; the *policy* for how long to tolerate `Loading` before
/// treating it as an error (ADR 0015 B4 loading-timeout) lives in the future drain
/// worker (M2.3), NOT here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthStatus {
    Ok,
    Loading,
    Error(String),
}

// ---------------------------------------------------------------------------
// TransportFailure — typed trust-but-verify rejection error (ADR 0024)
// ---------------------------------------------------------------------------

/// A sidecar response could not be trusted (ADR 0024 trust-but-verify): the
/// `embed` response's `model_id`/`dim` didn't match the cached `model_info`, the
/// vector count didn't match the input count, or a vector's length didn't match
/// `dim`. Rows must stay `pending` — nothing is ever stored on this path.
///
/// A distinct type (rather than a bare `anyhow::anyhow!`) so callers that need to
/// distinguish "sidecar lied about its own output" from a plain I/O error can
/// downcast (`err.downcast_ref::<TransportFailure>()`); `embed()` itself still
/// returns `anyhow::Result` for consistency with the rest of the codebase
/// (`HistorySource`/`BatchSink` in `backfill.rs` use the same convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportFailure(pub String);

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "embedder transport failure (ADR 0024 trust-but-verify): {}", self.0)
    }
}

impl std::error::Error for TransportFailure {}

// ---------------------------------------------------------------------------
// Embedder — transport-neutral trait seam (ADR 0006)
// ---------------------------------------------------------------------------

/// Transport-neutral embedding seam (ADR 0006). `StdioEmbedder` is the v1 (and
/// only, for now) real implementation; a future `HttpEmbedder` could implement
/// the same trait without changing any caller. Dyn-safe via `#[async_trait]` so
/// the bridge can hold a single shared `Arc<dyn Embedder>` (M2.3.10).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Model identity + limits. Implementations that need I/O to learn this
    /// (e.g. `StdioEmbedder`) fetch it ONCE at construction and cache it — this
    /// accessor never does I/O and never fails.
    fn model_info(&self) -> ModelInfo;

    /// Embed a batch of texts, one vector per input, in input order. Any error
    /// (I/O failure, malformed response, trust-but-verify mismatch) is a
    /// transport failure: callers must not store partial/mismatched output.
    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;

    /// Current sidecar health, relayed faithfully (`Ok`/`Loading`/`Error(detail)`).
    /// Never panics; a transport-level failure while checking health is itself
    /// reported as `HealthStatus::Error(..)`, not a Rust `Err`.
    async fn health(&self) -> HealthStatus;
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 client framing (F-G) — fresh structs, NOT shared with mcp.rs
// ---------------------------------------------------------------------------
//
// `mcp.rs::run_mcp_server` (`mcp.rs:13-46`) is the *server* half of JSON-RPC over
// stdio: it reads requests from ITS OWN stdin and writes responses to ITS OWN
// stdout (`JsonRpcRequest`/`JsonRpcResponse`, `mcp.rs:48-67`). The embedder
// sidecar needs the mirror image: whatsrust spawns a CHILD process and must
// write requests to the CHILD's stdin and read responses from the CHILD's
// stdout — the client half (ADR 0024 §0 Finding F-G). The wire-format
// conventions are intentionally identical (`{jsonrpc, id, method, params}`
// request / `{jsonrpc, id, result|error}` response, newline-delimited, one JSON
// object per line, flush after write) but the structs are defined fresh here
// rather than shared/reused from `mcp.rs` — the I/O direction differs too much
// to make a shared abstraction worth it at this size (plan §0 F-G).

/// Client-side JSON-RPC 2.0 request (mirrors `mcp.rs::JsonRpcRequest`'s shape).
#[derive(Debug, Serialize)]
struct SidecarRequest {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

/// Client-side JSON-RPC 2.0 response (mirrors `mcp.rs::JsonRpcResponse`'s shape).
#[derive(Debug, Deserialize)]
struct SidecarResponse {
    #[allow(dead_code)]
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

/// `model_info` result wire shape (ADR 0024): `{model_id, dim, max_batch?, max_input_tokens?}`.
#[derive(Debug, Deserialize)]
struct ModelInfoWire {
    model_id: String,
    dim: usize,
    #[serde(default)]
    max_batch: Option<usize>,
    #[serde(default)]
    max_input_tokens: Option<usize>,
}

/// `embed` result wire shape (ADR 0024): `{vectors, model_id, dim}`. `model_id`/
/// `dim` are ECHOED per response so every batch can be verified against the
/// cached `model_info` (trust-but-verify) before its vectors are trusted.
#[derive(Debug, Deserialize)]
struct EmbedWire {
    vectors: Vec<Vec<f32>>,
    model_id: String,
    dim: usize,
}

/// `health` result wire shape (ADR 0024): `{status: "ok"|"loading"|"error", detail?}`.
#[derive(Debug, Deserialize)]
struct HealthWire {
    status: String,
    #[serde(default)]
    detail: Option<String>,
}

// ---------------------------------------------------------------------------
// StdioEmbedder — real transport: child process + newline-delimited JSON-RPC
// ---------------------------------------------------------------------------

/// Bound on a single sidecar stdout line — cheap insurance against a
/// runaway/broken sidecar that never terminates a line (ADR 0024 review v3-#7).
/// The sidecar is trusted, not adversarial, so this is a sanity check, not a
/// hard security boundary: 16 MiB comfortably covers a full batch of large
/// vectors while still catching a genuinely broken/unbounded stream.
const DEFAULT_MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Timeout bounding the construction-time `model_info` round trip so a
/// hanging/misbehaving sidecar can never block bridge startup (2.1.4).
const MODEL_INFO_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the writer task's request channel. Generous enough that bursts
/// of concurrent callers (drain worker + live search, M2.3/M2.4) never block on
/// enqueueing a request; the real backpressure is the oneshot each caller awaits.
const WRITER_CHANNEL_CAPACITY: usize = 64;

/// One in-flight request's completion channel, keyed by JSON-RPC id in
/// `PendingState::pending`. `method` is kept alongside the sender purely so a
/// stale/unroutable or error response can still log/report which RPC it was for.
struct PendingEntry {
    method: &'static str,
    tx: oneshot::Sender<anyhow::Result<Value>>,
}

/// Requests awaiting a reply, plus a sticky "transport is dead" reason once the
/// writer or reader task has observed a fatal I/O failure (EOF, read error,
/// write error). `dead` and `pending` share one lock so registering a new
/// request and declaring the transport dead can never race: either the
/// registration happens first (and is then drained/failed by `mark_dead`), or
/// `mark_dead` happens first (and the registration sees `dead` and fails fast)
/// — there is no window where a request is registered after the drain and is
/// never resolved.
struct PendingState {
    pending: HashMap<u64, PendingEntry>,
    dead: Option<String>,
}

/// Shared between `call_rpc` (registers/unregisters pending requests) and the
/// reader/writer background tasks (dispatch replies, declare the transport
/// dead). Cheap, synchronous, non-blocking critical sections only — a
/// `std::sync::Mutex` is intentionally used instead of an async one (ADR 0024
/// cancellation-safety rework): no `.await` is ever held across this lock.
struct SharedState {
    state: StdMutex<PendingState>,
}

impl SharedState {
    fn new() -> Arc<Self> {
        Arc::new(Self { state: StdMutex::new(PendingState { pending: HashMap::new(), dead: None }) })
    }

    /// Register a pending request BEFORE it is sent, so a reply that wins the
    /// race against the caller's own registration can never be dispatched to a
    /// missing waiter. Fails fast if the transport is already known dead.
    fn register(&self, id: u64, method: &'static str, tx: oneshot::Sender<anyhow::Result<Value>>) -> anyhow::Result<()> {
        let mut guard = self.state.lock().expect("StdioEmbedder pending-state mutex poisoned");
        if let Some(reason) = &guard.dead {
            anyhow::bail!("sidecar exited: {reason}");
        }
        guard.pending.insert(id, PendingEntry { method, tx });
        Ok(())
    }

    /// Remove and return the pending entry for `id`, if any. Called by the
    /// reader task when a reply arrives (dispatch), and by `PendingGuard`'s
    /// `Drop` when a caller is cancelled before its reply arrives (cleanup —
    /// no leak, and the eventual late reply is then unroutable and discarded).
    fn remove(&self, id: u64) -> Option<PendingEntry> {
        self.state.lock().expect("StdioEmbedder pending-state mutex poisoned").pending.remove(&id)
    }

    /// Declare the transport dead (idempotent — only the first call has any
    /// effect) and fail every currently-pending request with a transport error
    /// so no caller can ever hang waiting on a reply that will never come.
    /// Called by the reader task on EOF/read error and by the writer task on a
    /// write error or channel closure.
    fn mark_dead(&self, reason: String) {
        let pending = {
            let mut guard = self.state.lock().expect("StdioEmbedder pending-state mutex poisoned");
            if guard.dead.is_some() {
                return;
            }
            guard.dead = Some(reason.clone());
            std::mem::take(&mut guard.pending)
        };
        for (_, entry) in pending {
            let _ = entry.tx.send(Err(anyhow::anyhow!("sidecar exited: {reason}")));
        }
    }
}

/// Drop guard that removes a call's pending entry if the call is cancelled
/// (e.g. the caller's `tokio::time::timeout` fires) before a reply arrives —
/// this is the fix for the cancellation-safety bug (ADR 0024 rework): without
/// it, a cancelled call's id stays registered forever, and when its late reply
/// finally arrives there is nothing to notice it's stale. With it, cancellation
/// deregisters immediately, so the late reply is simply an unknown id to the
/// reader task and is discarded (`debug!`) instead of corrupting a later call.
struct PendingGuard<'a> {
    shared: &'a SharedState,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        // No-op if the reader task already removed it (normal completion) —
        // only matters for the cancelled-before-reply case.
        self.shared.remove(self.id);
    }
}

/// Write one JSON-RPC request and await its reply via the writer task + reader
/// task described above. Cancellation-safe at every await point (ADR 0024
/// rework): the pending entry is registered before anything is sent, so a
/// cancelled caller (its future dropped mid-await) is cleaned up by
/// `PendingGuard` rather than leaving stale state for the next call to trip
/// over. Multiple callers may have requests in flight at once — the sidecar is
/// expected to process/reply to them in order, but routing here is by id, not
/// position, so out-of-order replies would still be routed correctly.
async fn call_rpc(
    next_id: &AtomicU64,
    shared: &Arc<SharedState>,
    writer_tx: &mpsc::Sender<String>,
    method: &'static str,
    params: Option<Value>,
) -> anyhow::Result<Value> {
    let id = next_id.fetch_add(1, Ordering::Relaxed);

    let (tx, rx) = oneshot::channel();
    shared.register(id, method, tx)?;
    let _guard = PendingGuard { shared, id };

    let req = SidecarRequest { jsonrpc: "2.0", id, method, params };
    let mut line = serde_json::to_string(&req).context("serializing sidecar request")?;
    line.push('\n');
    writer_tx
        .send(line)
        .await
        .map_err(|_| anyhow::anyhow!("sidecar exited: writer task is no longer running"))?;

    match rx.await {
        Ok(result) => result,
        // The reader task dropped our sender without replying — only happens via
        // `mark_dead` draining it on transport failure, which already encodes a
        // "sidecar exited" message, but a bare oneshot cancellation (shouldn't
        // normally happen) still needs a message of its own.
        Err(_) => Err(anyhow::anyhow!("sidecar exited: reply channel closed without a response")),
    }
}

/// Background task owning the child's stdin: serializes writes from concurrent
/// callers (via the mpsc channel) onto the one pipe. A write/flush failure (or
/// the channel closing because every `StdioEmbedder` clone was dropped) marks
/// the transport dead so any requests already in flight fail fast instead of
/// hanging.
async fn run_writer_task(mut stdin: ChildStdin, mut rx: mpsc::Receiver<String>, shared: Arc<SharedState>) {
    while let Some(line) = rx.recv().await {
        if let Err(e) = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await
        }
        .await
        {
            shared.mark_dead(format!("writing to sidecar stdin failed: {e}"));
            return;
        }
    }
    // Channel closed with no write error: either `StdioEmbedder` was dropped
    // (expected, harmless — nothing is waiting) or something else stopped
    // sending. Either way, mark dead so no caller added after this point hangs.
    shared.mark_dead("sidecar writer task exited (request channel closed)".to_string());
}

/// Background task owning the child's stdout: reads replies continuously and
/// dispatches each to its waiting caller by JSON-RPC id. Runs independently of
/// callers so a reply can be read (and, for the right id, delivered) regardless
/// of whether some OTHER caller has already timed out and moved on.
async fn run_reader_task(mut reader: BufReader<ChildStdout>, max_line_bytes: usize, shared: Arc<SharedState>) {
    loop {
        let raw_line = match read_line_bounded(&mut reader, max_line_bytes).await {
            Ok(line) => line,
            Err(e) => {
                shared.mark_dead(format!("reading sidecar stdout failed: {e}"));
                return;
            }
        };

        let resp: SidecarResponse = match serde_json::from_str(&raw_line) {
            Ok(resp) => resp,
            Err(e) => {
                warn!(error = %e, line = %raw_line, "sidecar: malformed response line, dropping");
                continue;
            }
        };

        let id = match resp.id.as_u64() {
            Some(id) => id,
            None => {
                // Null id (e.g. the sidecar's own parse-error response) or a
                // non-numeric id can't be routed to any waiter at all.
                warn!(id = ?resp.id, "sidecar: response id is null/non-numeric, cannot route, dropping");
                continue;
            }
        };

        let entry = match shared.remove(id) {
            Some(entry) => entry,
            None => {
                // Unknown/stale id: the caller that owned it already cancelled
                // (timed out) and `PendingGuard` deregistered it. This is the
                // cancellation-safety fix in action — the late reply is simply
                // discarded instead of being misread by whatever call comes next.
                debug!(id, "sidecar: discarding response for unknown/stale id (caller likely timed out)");
                continue;
            }
        };

        let method = entry.method;
        let result = if let Some(err) = resp.error {
            Err(anyhow::anyhow!("sidecar returned an error for {method}: {err}"))
        } else {
            resp.result
                .ok_or_else(|| anyhow::anyhow!("sidecar response for {method} had neither result nor error"))
        };
        let _ = entry.tx.send(result);
    }
}

/// Read one newline-delimited line from the sidecar's stdout, bounded by
/// `max_bytes` (2.1.4 / ADR 0024 review v3-#7).
async fn read_line_bounded(
    reader: &mut BufReader<ChildStdout>,
    max_bytes: usize,
) -> anyhow::Result<String> {
    let mut buf = Vec::new();
    let n = reader
        .read_until(b'\n', &mut buf)
        .await
        .context("reading sidecar stdout")?;
    if n == 0 {
        anyhow::bail!("sidecar stdout closed (EOF) — child process likely exited");
    }
    if buf.len() > max_bytes {
        anyhow::bail!(
            "sidecar response line exceeded max length ({} > {max_bytes} bytes) — treating as transport failure",
            buf.len(),
        );
    }
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
    String::from_utf8(buf).context("sidecar response line was not valid UTF-8")
}

/// Split a whitespace-separated args string into owned tokens. Intentionally
/// simple (no quoting support) — matches the KISS posture of the rest of this
/// module's env parsing; `WHATSRUST_EMBEDDER_ARGS` is expected to be a handful of
/// plain flags, not shell-quoted text.
fn parse_args(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(str::to_string).collect()
}

/// Real `Embedder` transport: spawns a child process and speaks newline-
/// delimited JSON-RPC 2.0 over its stdin/stdout (ADR 0006/0024). Internally,
/// a writer task and a reader task (both spawned at construction, see
/// `spawn_with`) own the child's stdin/stdout respectively; `call_rpc` talks to
/// them via a channel + a per-request oneshot rather than holding the pipes
/// directly, which is what makes a cancelled caller (e.g. a `tokio::time::
/// timeout`) safe: dropping its future can never leave a half-written request
/// or an unread reply on the wire (ADR 0024 cancellation-safety rework).
pub struct StdioEmbedder {
    /// Kept alive so `kill_on_drop(true)` (set at spawn) reaps the child when
    /// `StdioEmbedder` is dropped. Only read directly by the
    /// `fake-embedder`-gated tests (`.id()`); otherwise it's held purely for
    /// its Drop side effect, hence the lint allow.
    #[allow(dead_code)]
    child: Child,
    /// Hands a serialized request line to the writer task. Dropping every
    /// clone of this (i.e. dropping `StdioEmbedder`) closes the channel, which
    /// is the writer task's signal to exit.
    writer_tx: mpsc::Sender<String>,
    /// Pending-request map shared with the reader/writer tasks (registration,
    /// dispatch, dead-transport bookkeeping).
    shared: Arc<SharedState>,
    next_id: AtomicU64,
    /// Cached at construction (2.1.4) — `model_info()` is a plain, infallible
    /// accessor with no I/O.
    cached_model_info: ModelInfo,
}

impl StdioEmbedder {
    /// Spawn the sidecar from `WHATSRUST_EMBEDDER_CMD` (+ optional
    /// `WHATSRUST_EMBEDDER_ARGS`). Returns `Err` if `WHATSRUST_EMBEDDER_CMD` is
    /// unset — callers (the future M2.3 drain-worker spawn site) should treat
    /// that as "no embedder configured", i.e. the feature is simply off.
    pub async fn from_env() -> anyhow::Result<Self> {
        let cmd = std::env::var("WHATSRUST_EMBEDDER_CMD")
            .context("WHATSRUST_EMBEDDER_CMD not set")?;
        let args = std::env::var("WHATSRUST_EMBEDDER_ARGS").unwrap_or_default();
        Self::spawn(&cmd, &parse_args(&args)).await
    }

    /// Spawn `cmd args...` as a child process and construct a `StdioEmbedder`
    /// around it. **Fallible + non-fatal by design (2.1.4):** a bad/missing
    /// command, an immediately-exiting child, or a hanging/erroring `model_info`
    /// call all surface as `Err` — this function NEVER panics. The
    /// construction-time `model_info` round trip is bounded by
    /// `MODEL_INFO_TIMEOUT` so a stuck sidecar can never block startup.
    pub async fn spawn(cmd: &str, args: &[String]) -> anyhow::Result<Self> {
        Self::spawn_with(cmd, args, &[], MODEL_INFO_TIMEOUT, DEFAULT_MAX_LINE_BYTES).await
    }

    /// Spawn with custom environment variables (M2.6.4 misbehave testing).
    #[cfg(feature = "fake-embedder")]
    pub async fn spawn_with_env(
        cmd: &str,
        args: &[String],
        env_vars: &[(&str, &str)],
    ) -> anyhow::Result<Self> {
        Self::spawn_with(cmd, args, env_vars, MODEL_INFO_TIMEOUT, DEFAULT_MAX_LINE_BYTES).await
    }

    async fn spawn_with(
        cmd: &str,
        args: &[String],
        env_vars: &[(&str, &str)],
        model_info_timeout: Duration,
        max_line_bytes: usize,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new(cmd);
        command
            .args(args)
            .envs(env_vars.iter().map(|(k, v)| (*k, *v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn embedder sidecar: {cmd:?} {args:?}"))?;

        let stdin = child.stdin.take().context("sidecar child has no stdin pipe")?;
        let stdout = child.stdout.take().context("sidecar child has no stdout pipe")?;
        let reader = BufReader::new(stdout);

        let shared = SharedState::new();
        let (writer_tx, writer_rx) = mpsc::channel::<String>(WRITER_CHANNEL_CAPACITY);
        tokio::spawn(run_writer_task(stdin, writer_rx, shared.clone()));
        tokio::spawn(run_reader_task(reader, max_line_bytes, shared.clone()));

        let next_id = AtomicU64::new(1);
        let model_info_value = tokio::time::timeout(
            model_info_timeout,
            call_rpc(&next_id, &shared, &writer_tx, "model_info", None),
        )
        .await
        .context("timed out waiting for sidecar model_info at construction")??;

        let wire: ModelInfoWire = serde_json::from_value(model_info_value)
            .context("parsing model_info response at construction")?;

        Ok(Self {
            child,
            writer_tx,
            shared,
            next_id,
            cached_model_info: ModelInfo {
                model_id: wire.model_id,
                dim: wire.dim,
                max_batch: wire.max_batch,
                max_input_tokens: wire.max_input_tokens,
            },
        })
    }

    async fn call_rpc(&self, method: &'static str, params: Option<Value>) -> anyhow::Result<Value> {
        call_rpc(&self.next_id, &self.shared, &self.writer_tx, method, params).await
    }

    /// Trust-but-verify (ADR 0024): `model_id`/`dim` must match the cached
    /// `model_info`, the vector count must match the input count, and every
    /// vector's length must equal `dim`. Pure function — no I/O — so it's unit
    /// testable directly against constructed fixtures (2.1.5).
    fn verify_embed_response(
        expected: &ModelInfo,
        expected_count: usize,
        wire: &EmbedWire,
    ) -> Result<(), TransportFailure> {
        if wire.model_id != expected.model_id {
            return Err(TransportFailure(format!(
                "embed response model_id mismatch: expected {:?}, got {:?}",
                expected.model_id, wire.model_id
            )));
        }
        if wire.dim != expected.dim {
            return Err(TransportFailure(format!(
                "embed response dim mismatch: expected {}, got {}",
                expected.dim, wire.dim
            )));
        }
        if wire.vectors.len() != expected_count {
            return Err(TransportFailure(format!(
                "embed response vector count mismatch: expected {}, got {}",
                expected_count,
                wire.vectors.len()
            )));
        }
        for (i, v) in wire.vectors.iter().enumerate() {
            if v.len() != expected.dim {
                return Err(TransportFailure(format!(
                    "embed response vector[{i}] length mismatch: expected {}, got {}",
                    expected.dim,
                    v.len()
                )));
            }
        }
        Ok(())
    }

    /// Map the `health` wire shape to `HealthStatus`, relaying faithfully
    /// (2.1.6). Pure function — unit testable without any I/O.
    fn map_health(wire: HealthWire) -> HealthStatus {
        match wire.status.as_str() {
            "ok" => HealthStatus::Ok,
            "loading" => HealthStatus::Loading,
            "error" => HealthStatus::Error(wire.detail.unwrap_or_default()),
            other => HealthStatus::Error(format!("unrecognised health status: {other:?}")),
        }
    }
}

#[async_trait]
impl Embedder for StdioEmbedder {
    fn model_info(&self) -> ModelInfo {
        self.cached_model_info.clone()
    }

    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let params = serde_json::json!({ "texts": texts });
        let raw = self.call_rpc("embed", Some(params)).await?;
        let wire: EmbedWire = serde_json::from_value(raw).context("parsing embed response")?;
        Self::verify_embed_response(&self.cached_model_info, texts.len(), &wire)?;
        Ok(wire.vectors)
    }

    async fn health(&self) -> HealthStatus {
        match self.call_rpc("health", None).await {
            Ok(raw) => match serde_json::from_value::<HealthWire>(raw) {
                Ok(wire) => Self::map_health(wire),
                Err(e) => HealthStatus::Error(format!("malformed health response: {e}")),
            },
            Err(e) => HealthStatus::Error(format!("health check transport failure: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// FakeEmbedder — deterministic in-process fake (ADR 0025 seam 1)
// ---------------------------------------------------------------------------

/// Deterministic in-process fake `Embedder` (ADR 0025 seam 1) — no child
/// process, no I/O. `embed` returns canned vectors (`vec![0.1 * (i+1); dim]` per
/// input, 1-indexed) so higher-level worker/search tests (M2.3/M2.4) can assert
/// exact output without depending on a real ML model or sidecar process.
///
/// Extended with failure-injection modes for Wave A drain-worker integration tests:
///   - `always_fail` — all `embed()` calls return `Err` (transport failure)
///   - `reject_text` — reject a specific text content (per-row rejection)
///   - `health_override` — override the health status (e.g. `Loading`)
///   - `max_batch` / `max_input_tokens` — advertise limits to test clamping
pub struct FakeEmbedder {
    model_id: String,
    dim: usize,
    max_batch: Option<usize>,
    max_input_tokens: Option<usize>,
    always_fail: bool,
    reject_text: Option<String>,
    health_override: Option<HealthStatus>,
}

impl FakeEmbedder {
    pub fn new(model_id: impl Into<String>, dim: usize) -> Self {
        Self {
            model_id: model_id.into(),
            dim,
            max_batch: None,
            max_input_tokens: None,
            always_fail: false,
            reject_text: None,
            health_override: None,
        }
    }

    /// Configure `max_batch` limit (for testing batch-size clamping).
    pub fn with_max_batch(mut self, max_batch: usize) -> Self {
        self.max_batch = Some(max_batch);
        self
    }

    /// Configure `max_input_tokens` limit.
    pub fn with_max_input_tokens(mut self, max_tokens: usize) -> Self {
        self.max_input_tokens = Some(max_tokens);
        self
    }

    /// Always return transport failure on `embed()` calls.
    pub fn always_fail(mut self) -> Self {
        self.always_fail = true;
        self
    }

    /// Reject a specific text (by exact match) — returns `Err` when the text is in the batch.
    pub fn reject_text(mut self, text: impl Into<String>) -> Self {
        self.reject_text = Some(text.into());
        self
    }

    /// Override health status (e.g. report `Loading` continuously for timeout tests).
    pub fn with_health(mut self, status: HealthStatus) -> Self {
        self.health_override = Some(status);
        self
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    fn model_info(&self) -> ModelInfo {
        ModelInfo {
            model_id: self.model_id.clone(),
            dim: self.dim,
            max_batch: self.max_batch,
            max_input_tokens: self.max_input_tokens,
        }
    }

    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if self.always_fail {
            anyhow::bail!("fake embedder: always_fail mode");
        }
        if let Some(ref reject) = self.reject_text {
            if texts.iter().any(|t| t == reject) {
                anyhow::bail!("fake embedder: rejected text '{}'", reject);
            }
        }
        Ok(texts
            .iter()
            .enumerate()
            .map(|(i, _)| vec![0.1 * (i + 1) as f32; self.dim])
            .collect())
    }

    async fn health(&self) -> HealthStatus {
        self.health_override.clone().unwrap_or(HealthStatus::Ok)
    }
}

// ---------------------------------------------------------------------------
// Cosine similarity — M2.4.1 pure function for semantic reranking
// ---------------------------------------------------------------------------

/// Compute cosine similarity between two vectors: `dot(a, b) / (||a|| * ||b||)`.
/// Returns a value in `[-1.0, 1.0]` where:
///   - `1.0` = identical direction (most similar)
///   - `0.0` = orthogonal (no similarity)
///   - `-1.0` = opposite direction
///
/// Returns `Err` if the vectors have mismatched dimensions. Returns `Ok(0.0)` if
/// either vector has zero magnitude (defined behavior, not NaN/panic — a zero-magnitude
/// vector is orthogonal to everything per ADR 0008).
///
/// Used by `WhatsAppBridge::search()` (M2.4.3) to rerank FTS5-recalled candidates
/// against the query embedding. Pure logic, no I/O.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> anyhow::Result<f32> {
    if a.len() != b.len() {
        anyhow::bail!(
            "cosine_similarity: dimension mismatch (a={}, b={})",
            a.len(),
            b.len()
        );
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    // Zero-magnitude vector → 0.0 (defined, not NaN/panic per M2.4.1 Verify).
    if mag_a == 0.0 || mag_b == 0.0 {
        return Ok(0.0);
    }

    Ok(dot / (mag_a * mag_b))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- 2.1.3: JSON-RPC request/response wire shapes round-trip ---

    #[test]
    fn test_sidecar_request_response_json_roundtrip() {
        let req = SidecarRequest {
            jsonrpc: "2.0",
            id: 7,
            method: "embed",
            params: Some(serde_json::json!({"texts": ["hi", "there"]})),
        };
        let s = serde_json::to_string(&req).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "embed");
        assert_eq!(v["params"]["texts"][0], "hi");

        let resp_json =
            r#"{"jsonrpc":"2.0","id":7,"result":{"vectors":[[0.1,0.2]],"model_id":"m","dim":2}}"#;
        let resp: SidecarResponse = serde_json::from_str(resp_json).unwrap();
        assert_eq!(resp.id, Value::from(7));
        assert!(resp.error.is_none());
        let embed: EmbedWire = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(embed.model_id, "m");
        assert_eq!(embed.dim, 2);
        assert_eq!(embed.vectors, vec![vec![0.1_f32, 0.2_f32]]);

        // Error-shaped response also round-trips.
        let err_json = r#"{"jsonrpc":"2.0","id":8,"error":{"code":-32000,"message":"boom"}}"#;
        let err_resp: SidecarResponse = serde_json::from_str(err_json).unwrap();
        assert!(err_resp.result.is_none());
        assert!(err_resp.error.is_some());

        // params omitted entirely when None (e.g. model_info/health requests).
        let no_params = SidecarRequest { jsonrpc: "2.0", id: 1, method: "model_info", params: None };
        let s2 = serde_json::to_string(&no_params).unwrap();
        assert!(!s2.contains("params"), "params must be omitted, not null: {s2}");
    }

    // --- 2.1.2: Embedder is dyn-safe (M2.3.10 needs a single shared `Arc<dyn Embedder>`) ---

    #[tokio::test]
    async fn test_embedder_trait_is_dyn_compatible() {
        let boxed: std::sync::Arc<dyn Embedder> = std::sync::Arc::new(FakeEmbedder::new("m", 3));
        let info = boxed.model_info();
        assert_eq!(info.dim, 3);
        let vecs = boxed.embed(&["x".to_string()]).await.unwrap();
        assert_eq!(vecs.len(), 1);
        assert!(matches!(boxed.health().await, HealthStatus::Ok));
    }

    // --- 2.1.7: FakeEmbedder shape ---

    #[tokio::test]
    async fn test_fake_embedder_embed_output_shape() {
        let fake = FakeEmbedder::new("fake-model", 5);
        let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let vectors = fake.embed(&texts).await.unwrap();
        assert_eq!(vectors.len(), texts.len());
        for v in &vectors {
            assert_eq!(v.len(), 5);
        }
        assert_eq!(vectors[0], vec![0.1_f32; 5]);
        assert_eq!(vectors[2], vec![0.3_f32; 5]);

        let info = fake.model_info();
        assert_eq!(info.model_id, "fake-model");
        assert_eq!(info.dim, 5);

        assert!(matches!(fake.health().await, HealthStatus::Ok));
    }

    // --- 2.1.5: trust-but-verify ---

    fn sample_model_info() -> ModelInfo {
        ModelInfo { model_id: "m1".into(), dim: 4, max_batch: None, max_input_tokens: None }
    }

    #[test]
    fn test_verify_embed_response_accepts_well_formed_batch() {
        let info = sample_model_info();
        let wire = EmbedWire { vectors: vec![vec![0.0; 4], vec![1.0; 4]], model_id: "m1".into(), dim: 4 };
        assert!(StdioEmbedder::verify_embed_response(&info, 2, &wire).is_ok());
    }

    #[test]
    fn test_verify_embed_response_rejects_wrong_dim() {
        let info = sample_model_info();
        let wire = EmbedWire { vectors: vec![vec![0.0; 3]], model_id: "m1".into(), dim: 3 };
        assert!(StdioEmbedder::verify_embed_response(&info, 1, &wire).is_err());
    }

    #[test]
    fn test_verify_embed_response_rejects_wrong_count() {
        let info = sample_model_info();
        let wire = EmbedWire { vectors: vec![vec![0.0; 4]], model_id: "m1".into(), dim: 4 };
        assert!(StdioEmbedder::verify_embed_response(&info, 2, &wire).is_err());
    }

    #[test]
    fn test_verify_embed_response_rejects_wrong_model_id() {
        let info = sample_model_info();
        let wire = EmbedWire { vectors: vec![vec![0.0; 4]], model_id: "wrong".into(), dim: 4 };
        assert!(StdioEmbedder::verify_embed_response(&info, 1, &wire).is_err());
    }

    #[test]
    fn test_verify_embed_response_rejects_per_vector_length_mismatch() {
        let info = sample_model_info();
        // Count/model_id/dim all match, but one vector is the wrong length.
        let wire = EmbedWire {
            vectors: vec![vec![0.0; 4], vec![0.0; 2]],
            model_id: "m1".into(),
            dim: 4,
        };
        assert!(StdioEmbedder::verify_embed_response(&info, 2, &wire).is_err());
    }

    // --- 2.1.6: health() relays all three states unchanged ---

    #[test]
    fn test_map_health_relays_all_three_states() {
        assert_eq!(
            StdioEmbedder::map_health(HealthWire { status: "ok".into(), detail: None }),
            HealthStatus::Ok
        );
        assert_eq!(
            StdioEmbedder::map_health(HealthWire { status: "loading".into(), detail: None }),
            HealthStatus::Loading
        );
        assert_eq!(
            StdioEmbedder::map_health(HealthWire { status: "error".into(), detail: Some("oom".into()) }),
            HealthStatus::Error("oom".into())
        );
    }

    // --- 2.1.4: fallible, non-fatal construction ---

    #[tokio::test]
    async fn test_stdio_embedder_construction_nonexistent_command_is_err_not_panic() {
        let result = StdioEmbedder::spawn("/definitely/does/not/exist/whatsrust-embedder", &[]).await;
        assert!(result.is_err(), "spawning a nonexistent command must return Err, not panic");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_stdio_embedder_construction_immediate_exit_is_err_not_panic() {
        // /bin/false spawns fine but exits immediately with no output — the
        // construction-time model_info call should observe EOF and fail cleanly
        // rather than panic or hang.
        let result = StdioEmbedder::spawn("/bin/false", &[]).await;
        assert!(
            result.is_err(),
            "a command that exits immediately with no output must fail construction, not panic"
        );
    }

    // --- 2.1.8/2.1.9: real subprocess round-trip + concurrency + drop-safety ---
    //
    // Gated behind the `fake-embedder` Cargo feature so plain `cargo build`/
    // `cargo test` (default features) never need the dev-only sidecar binary to
    // exist; run with `cargo test --features fake-embedder` to include these.

    /// Locate (building if necessary) the real `fake-embedder` executable.
    ///
    /// Cargo's `CARGO_BIN_EXE_<name>` env var is only populated for
    /// integration-test/bench targets under `tests/`/`benches/` — NOT for the
    /// lib's own `#[cfg(test)]` unit-test harness, which is what these inline
    /// tests are (ADR 0025: no `tests/` dir in this project). And a plain
    /// `cargo test --features fake-embedder` compiles `fake-embedder` as its
    /// OWN (empty) test harness rather than uplifting the runnable binary to
    /// `target/<profile>/fake-embedder`, so we can't just guess a path either.
    ///
    /// So: explicitly invoke `cargo build --bin fake-embedder --features
    /// fake-embedder` (idempotent — near-instant if already built) and then
    /// resolve the uplifted binary at its well-known `target/<profile>/`
    /// location. No network access is needed (all deps are already fetched),
    /// so this stays CI-safe per ADR 0025.
    #[cfg(feature = "fake-embedder")]
    fn fake_embedder_bin_path() -> std::path::PathBuf {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| manifest_dir.join("target"));
        let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
        let bin_path = target_dir.join(profile).join("fake-embedder");

        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--bin", "fake-embedder", "--features", "fake-embedder"])
            .current_dir(&manifest_dir)
            .status()
            .expect("invoking `cargo build --bin fake-embedder` from within the test");
        assert!(status.success(), "`cargo build --bin fake-embedder --features fake-embedder` failed");
        assert!(bin_path.is_file(), "expected fake-embedder binary at {bin_path:?} after build");
        bin_path
    }

    #[cfg(feature = "fake-embedder")]
    #[tokio::test]
    async fn test_stdio_embedder_real_subprocess_roundtrip() {
        let bin = fake_embedder_bin_path();
        assert!(bin.exists(), "fake-embedder binary not found at {bin:?} — build with --features fake-embedder");

        let embedder = StdioEmbedder::spawn(bin.to_str().unwrap(), &[])
            .await
            .expect("fake-embedder sidecar should spawn and answer model_info");

        let info = embedder.model_info();
        assert_eq!(info.model_id, "fake-embedder-v1");
        assert_eq!(info.dim, 8);

        let texts = vec!["hello".to_string(), "world".to_string()];
        let vectors = embedder.embed(&texts).await.expect("embed should succeed");
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0], vec![0.1_f32; 8]);
        assert_eq!(vectors[1], vec![0.2_f32; 8]);

        let health = embedder.health().await;
        assert_eq!(health, HealthStatus::Ok);
    }

    #[cfg(feature = "fake-embedder")]
    #[tokio::test]
    async fn test_stdio_embedder_concurrent_calls_serialize_without_corruption() {
        let bin = fake_embedder_bin_path();
        let embedder = std::sync::Arc::new(
            StdioEmbedder::spawn(bin.to_str().unwrap(), &[]).await.expect("spawn fake-embedder"),
        );

        let e1 = embedder.clone();
        let e2 = embedder.clone();
        let e3 = embedder.clone();
        let texts1 = vec!["a".to_string()];
        let texts2 = vec!["bb".to_string(), "cc".to_string()];

        // Fire three concurrent calls (two embeds + a health check) at the SAME
        // child over the SAME stdin/stdout pipe. Requests may now genuinely be
        // in flight at once (ADR 0024 cancellation-safety rework — no more
        // single-mutex serialization); correctness instead relies on id-based
        // routing in the reader task. If that routing were broken, responses
        // would be delivered to the wrong caller or fail to parse.
        let (r1, r2, r3) = tokio::join!(
            e1.embed(&texts1),
            e2.embed(&texts2),
            e3.health(),
        );

        let v1 = r1.expect("embed 1 should succeed");
        let v2 = r2.expect("embed 2 should succeed");
        assert_eq!(v1.len(), 1);
        assert_eq!(v2.len(), 2);
        assert_eq!(v1[0], vec![0.1_f32; 8]);
        assert_eq!(v2[0], vec![0.1_f32; 8]);
        assert_eq!(v2[1], vec![0.2_f32; 8]);
        assert_eq!(r3, HealthStatus::Ok);
    }

    // --- FLIP-CHECK: cancellation-safety regression test ---
    //
    // Reproduces the live-daemon bug: a caller's `tokio::time::timeout` fires
    // after the request was written but before the reply was read, so the
    // in-flight reply is left stale in the pipe. On the buggy client this
    // poisons every subsequent call (one-reply-behind, permanent id mismatch /
    // wrong-sized response). On the fixed client, cancellation deregisters the
    // call so the late reply is just discarded, and later calls get their own
    // reply.
    //
    // Confirmed to fail on the pre-fix client with exactly the predicted
    // symptom (`sidecar response id mismatch for embed: expected 3, got 2`)
    // before this fix was implemented.
    #[cfg(feature = "fake-embedder")]
    #[tokio::test]
    async fn test_stdio_embedder_cancelled_call_does_not_poison_later_calls() {
        let bin = fake_embedder_bin_path();
        let embedder = StdioEmbedder::spawn_with_env(
            bin.to_str().unwrap(),
            &[],
            &[("FAKE_EMBEDDER_DELAY_FIRST_MS", "1500")],
        )
        .await
        .expect("spawn fake-embedder with delay-first");

        // First embed call: 3 texts, the sidecar will sit on this reply for
        // ~1.5s (delay-first). A 300ms timeout fires well before that, so the
        // caller's future (and its pending registration) is dropped while the
        // reply is still in flight.
        let three_texts = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let timed_out =
            tokio::time::timeout(Duration::from_millis(300), embedder.embed(&three_texts)).await;
        assert!(
            timed_out.is_err(),
            "the 3-text embed call must time out while the sidecar is still delaying its reply"
        );

        // Second call: a single text, no timeout. The stale 3-vector reply for
        // the first call is still unread in the pipe at this point. A correct,
        // cancellation-safe client must not confuse that stale reply for this
        // call's own reply — it must wait for (and correctly route) ITS OWN
        // reply, giving back exactly 1 vector.
        let one_text = vec!["solo".to_string()];
        let result = embedder.embed(&one_text).await;
        let vectors = result.expect(
            "embed with 1 text must succeed with its own reply, not the stale 3-vector reply \
             left behind by the cancelled call",
        );
        assert_eq!(
            vectors.len(),
            1,
            "must get exactly 1 vector (its own reply), not 3 (the stale reply)"
        );
        assert_eq!(vectors[0].len(), embedder.model_info().dim);

        // health() must also still work — a buggy client leaves EVERY call one
        // reply behind forever, so this would fail too if the bug were present.
        let health = embedder.health().await;
        assert_eq!(health, HealthStatus::Ok, "health() must succeed after the cancelled call");
    }

    /// Two genuinely concurrent `embed` calls (fired together via
    /// `tokio::join!`, no artificial delay), distinguished by batch size, each
    /// get their OWN reply — never swapped. Both requests can be in flight on
    /// the wire at once now (ADR 0024 rework); correctness relies on the
    /// reader task routing each reply by JSON-RPC id rather than by position.
    #[cfg(feature = "fake-embedder")]
    #[tokio::test]
    async fn test_stdio_embedder_concurrent_calls_each_get_own_reply() {
        let bin = fake_embedder_bin_path();
        let embedder = std::sync::Arc::new(
            StdioEmbedder::spawn(bin.to_str().unwrap(), &[]).await.expect("spawn fake-embedder"),
        );

        let e_a = embedder.clone();
        let e_b = embedder.clone();
        // Distinguish the two batches by size (4 vs 1) so a swapped/misrouted
        // reply would be caught by a length mismatch, not just a value check.
        let texts_a = vec!["a1".to_string(), "a2".to_string(), "a3".to_string(), "a4".to_string()];
        let texts_b = vec!["b1".to_string()];

        let (result_a, result_b) = tokio::join!(e_a.embed(&texts_a), e_b.embed(&texts_b));

        let vec_a = result_a.expect("embed A should succeed");
        let vec_b = result_b.expect("embed B should succeed");
        assert_eq!(vec_a.len(), 4, "batch A (4 texts) must get its own 4-vector reply, not B's");
        assert_eq!(vec_b.len(), 1, "batch B (1 text) must get its own 1-vector reply, not A's");
    }

    /// When the child exits (EOF on stdout), both an already-in-flight request
    /// and any subsequent request must fail fast with a transport error —
    /// never hang waiting for a reply that will never arrive.
    #[cfg(feature = "fake-embedder")]
    #[cfg(unix)]
    #[tokio::test]
    async fn test_stdio_embedder_eof_fails_pending_and_subsequent_calls_fast() {
        let bin = fake_embedder_bin_path();
        let embedder = std::sync::Arc::new(
            StdioEmbedder::spawn_with_env(
                bin.to_str().unwrap(),
                &[],
                &[("FAKE_EMBEDDER_DELAY_FIRST_MS", "2000")],
            )
            .await
            .expect("spawn fake-embedder with delay-first"),
        );

        // Kick off an embed call the sidecar will sit on for 2s (first embed
        // call is delayed) — it is still genuinely pending when we kill the
        // child below, before any reply is ever written.
        let e_pending = embedder.clone();
        let pending_texts = vec!["x".to_string()];
        let pending_call = tokio::spawn(async move { e_pending.embed(&pending_texts).await });

        // Give the write a moment to land so the request is genuinely in
        // flight (registered + sent), not racing construction. Generous
        // margin: under a loaded test runner (whole suite in parallel,
        // several fake-embedder subprocesses at once) scheduling can lag well
        // past 100ms, and the 2s delay-first above leaves plenty of room.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let pid = embedder.child.id().expect("child should have a pid");
        let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();

        // The in-flight call must fail fast once the child dies, not hang
        // forever waiting for a reply that will never come. The timeout here
        // is just an upper bound on a broken ("hangs forever") outcome, not
        // expected latency — it only costs wall-clock time if the test fails.
        let pending_result = tokio::time::timeout(Duration::from_secs(5), pending_call)
            .await
            .expect("pending embed call must not hang after the child dies")
            .expect("task must not panic");
        assert!(pending_result.is_err(), "pending embed call must fail once the sidecar dies");

        // Give the reader task a moment to observe EOF and mark the transport
        // dead (generous margin for the same reason as above).
        tokio::time::sleep(Duration::from_millis(600)).await;

        // A brand-new call issued AFTER the child is known-dead must also fail
        // fast rather than hang.
        let subsequent = tokio::time::timeout(Duration::from_secs(2), embedder.embed(&["y".to_string()]))
            .await
            .expect("post-EOF embed call must not hang");
        assert!(subsequent.is_err(), "post-EOF embed call must fail fast, not succeed");
    }

    #[cfg(feature = "fake-embedder")]
    #[cfg(unix)]
    #[tokio::test]
    async fn test_stdio_embedder_drop_kills_child() {
        let bin = fake_embedder_bin_path();
        let embedder = StdioEmbedder::spawn(bin.to_str().unwrap(), &[]).await.expect("spawn fake-embedder");
        let pid = embedder.child.id().expect("child should have a pid");

        drop(embedder);
        // Give the OS a moment to process the kill triggered by kill_on_drop.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            !alive,
            "child process {pid} should be reaped after StdioEmbedder is dropped (kill_on_drop)"
        );
    }

    // --- M2.6.4: misbehave mode validation (real subprocess, trust-but-verify) ---

    #[cfg(feature = "fake-embedder")]
    #[tokio::test]
    async fn test_stdio_embedder_misbehave_wrong_dim_rejected() {
        let bin = fake_embedder_bin_path();
        // Spawn with FAKE_EMBEDDER_MISBEHAVE=wrong_dim
        let embedder = StdioEmbedder::spawn_with_env(
            bin.to_str().unwrap(),
            &[],
            &[("FAKE_EMBEDDER_MISBEHAVE", "wrong_dim")],
        )
        .await
        .expect("spawn fake-embedder with misbehave mode");

        let texts = vec!["test".to_string()];
        let result = embedder.embed(&texts).await;
        assert!(
            result.is_err(),
            "embed should fail when sidecar returns wrong dimension (trust-but-verify)"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("length") || err.to_string().contains("dim"),
            "error should mention dimension-related mismatch: {err}"
        );
    }

    #[cfg(feature = "fake-embedder")]
    #[tokio::test]
    async fn test_stdio_embedder_misbehave_wrong_count_rejected() {
        let bin = fake_embedder_bin_path();
        let embedder = StdioEmbedder::spawn_with_env(
            bin.to_str().unwrap(),
            &[],
            &[("FAKE_EMBEDDER_MISBEHAVE", "wrong_count")],
        )
        .await
        .expect("spawn fake-embedder with misbehave mode");

        let texts = vec!["a".to_string(), "b".to_string()];
        let result = embedder.embed(&texts).await;
        assert!(
            result.is_err(),
            "embed should fail when sidecar returns wrong count (trust-but-verify)"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("count"),
            "error should mention count mismatch: {err}"
        );
    }

    #[cfg(feature = "fake-embedder")]
    #[tokio::test]
    async fn test_stdio_embedder_misbehave_wrong_model_id_rejected() {
        let bin = fake_embedder_bin_path();
        let embedder = StdioEmbedder::spawn_with_env(
            bin.to_str().unwrap(),
            &[],
            &[("FAKE_EMBEDDER_MISBEHAVE", "wrong_model_id")],
        )
        .await
        .expect("spawn fake-embedder with misbehave mode");

        let texts = vec!["test".to_string()];
        let result = embedder.embed(&texts).await;
        assert!(
            result.is_err(),
            "embed should fail when sidecar returns wrong model_id (trust-but-verify)"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("model_id"),
            "error should mention model_id mismatch: {err}"
        );
    }

    // --- M2.4.1: cosine_similarity unit tests ---

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!((sim - 0.0).abs() < 1e-6, "orthogonal vectors should have cosine=0");
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!((sim - 1.0).abs() < 1e-6, "identical vectors should have cosine=1");
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!((sim - (-1.0)).abs() < 1e-6, "opposite vectors should have cosine=-1");
    }

    #[test]
    fn test_cosine_similarity_mismatched_dim() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        let result = cosine_similarity(&a, &b);
        assert!(
            result.is_err(),
            "mismatched dimensions should return Err"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("dimension mismatch"));
    }

    #[test]
    fn test_cosine_similarity_zero_magnitude() {
        let zero = vec![0.0, 0.0, 0.0];
        let nonzero = vec![1.0, 2.0, 3.0];

        // Zero vs nonzero -> 0.0 (defined, not NaN)
        let sim1 = cosine_similarity(&zero, &nonzero).unwrap();
        assert_eq!(sim1, 0.0, "zero-magnitude vector should yield 0.0, not NaN");

        // Nonzero vs zero -> 0.0
        let sim2 = cosine_similarity(&nonzero, &zero).unwrap();
        assert_eq!(sim2, 0.0);

        // Zero vs zero -> 0.0
        let sim3 = cosine_similarity(&zero, &zero).unwrap();
        assert_eq!(sim3, 0.0);
    }
}
