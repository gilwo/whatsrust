# HOWTO — Operational Playbook

Task-oriented, copy-pasteable steps for building, configuring, running, and
operating **whatsrust**. Every command below is grounded in the real binary
(`src/main.rs`, `src/api.rs`, `src/mcp.rs`) or the root `Makefile` — nothing
here is speculative. Where a detail couldn't be verified, it's called out
explicitly instead of guessed at.

For the "why" behind these behaviors, see the ADRs linked from
`docs/adr/0000-index.md`. For a narrative overview, see `README.md` and
`ARCHITECTURE.md`.

---

## 1. Build

**Goal:** produce a `whatsrust` binary (debug or release).

**Prerequisites:**
- A Rust toolchain. The repo pins **nightly** via `rust-toolchain.toml`
  (`channel = "nightly"`) — if you use `rustup`, it auto-installs/selects this
  toolchain the first time you run `cargo`/`rustc`/`make` inside the repo; no
  manual step needed beyond having `rustup` itself installed.
- A C compiler/linker on `PATH` (e.g. Xcode Command Line Tools on macOS,
  `build-essential` on Debian/Ubuntu). Required because `rusqlite` is built
  with the `bundled` feature, which compiles SQLite from source via the `cc`
  crate.
- No system SQLite, OpenSSL, or Node.js is required — this is the whole point
  of the project (single static-ish Rust binary).

**Steps:**
```bash
git clone https://github.com/199-biotechnologies/whatsrust
cd whatsrust

# Debug build (fast, unoptimized) — verified working
make build          # == cargo build
# binary: target/debug/whatsrust

# Release build (optimized: opt-level=z, LTO, codegen-units=1, panic=abort,
# stripped — see [profile.release] in Cargo.toml). Noticeably slower to
# compile than debug because of LTO; this is expected.
make release         # == cargo build --release
# binary: target/release/whatsrust

# Release build of BOTH the daemon and the embedder sidecar (embedder-rs/) — the
# two binaries a deployment needs. Reuses `release` + `embedder-build`; equally slow.
make release-all
# binaries: target/release/whatsrust, embedder-rs/target/release/whatsrust-embedder

# Collects both of the above into dist/ and prints their paths plus a ready-to-use
# WHATSRUST_EMBEDDER_CMD hint — handy for copying a self-contained deployment elsewhere.
make dist
# dist/whatsrust, dist/whatsrust-embedder
```

**Verification:**
```bash
./target/debug/whatsrust --help
```
This prints the full command list and exits 0 — it does **not** require a
running daemon or a network connection (confirmed: `--help`/`help`/`-h` are
handled locally in `cli_main` before any HTTP call).

---

## 2. Configure

**Goal:** set the knobs that matter before first run.

whatsrust reads config from environment variables, optionally loaded from a
`.env` file via `dotenvy` (see ADR 0023). **Precedence: real environment
variables win; `.env` only fills variables that are otherwise unset.** An
absent `.env` file is a silent no-op; a malformed one prints a warning to
stderr and the daemon continues with the real environment only.

**Note:** `.env` is loaded only by the daemon (`whatsrust` with no arguments).
The CLI and `whatsrust mcp` read the process environment, so export
`WHATSRUST_API_TOKEN` (and any other needed vars) for them. See §2a "Upgrade
notes" for more.

**Steps:**
1. Copy the example file:
   ```bash
   cp .env.example .env
   ```
2. Edit `.env` and set the values you need. The knobs that matter most for a
   first run:
   - `WHATSRUST_PORT` (default `7270`) — REST API port. `HEALTH_PORT` is a
     legacy fallback name.
   - `WHATSRUST_BIND` (default `127.0.0.1`) — API bind address; loopback-only
     unless you explicitly opt in (see below).
   - `WHATSAPP_PAIR_PHONE` — set this to use phone-number pair-code linking
     instead of scanning a QR code.
   - `WHATSAPP_ALLOWED` — comma-separated digit-only phone numbers; empty
     means accept inbound messages from everyone.
   - `WHATSRUST_DEVICE_NAME` (default `whatsrust`) — the name shown in
     WhatsApp's Linked Devices list. WhatsApp only reads this **while pairing
     a new device**; once paired, the name is stored with the session (the
     `device_name` metadata key) and this var is ignored on every later start,
     even if you change it. To rename an existing session: unlink the device
     in WhatsApp → Linked Devices, set the new `WHATSRUST_DEVICE_NAME`, and
     restart the daemon to re-pair (scan a new QR code / pair code).
   - `BACKUP_DIR` (default `whatsapp.db.backups`) — where SQLite backups are
     written on startup/shutdown.
3. (Optional) point at a config file somewhere other than `./.env` by setting
   `WHATSRUST_ENV_FILE=/path/to/file` in the **real** environment (not inside
   `.env` itself — the daemon must know the path before it loads the file).
4. Remote API access is off by default. To allow a non-loopback bind, you
   must set both `WHATSRUST_ALLOW_REMOTE=1` **and** `WHATSRUST_API_TOKEN=...`
   (the server enforces the token when remote access is enabled; see
   `src/api.rs`).
5. **API security (ADR 0044):** even on the loopback default, every request is
   checked before it's routed. A cross-site `Origin` header is rejected unless
   it's in `WHATSRUST_ALLOWED_ORIGINS` (comma-separated, default empty). With no
   `Origin`, the `Host` header must be `127.0.0.1`/`localhost`/`::1` (any port) or
   listed in `WHATSRUST_ALLOWED_HOSTS` (comma-separated, default empty — e.g. for a
   reverse proxy) — this defeats DNS rebinding. **Either allowlist requires
   `WHATSRUST_API_TOKEN`, or the daemon refuses to start** (amended 2026-10-18, ADR 0044).
   Every `POST` must send `Content-Type: application/json`; a raw `curl -X POST ... -d '...'`
   needs `-H 'Content-Type: application/json'` added or it gets `415 Unsupported Media Type`
   (`code: "unsupported_media_type"`). The body is read only after the gate and token checks.
   `Transfer-Encoding` (e.g. chunked) is not supported (411; send `Content-Length`).
   `Expect: 100-continue` is honoured after authentication. A token
   (`WHATSRUST_API_TOKEN`) is still recommended even when loopback-only.
6. Backfill (historical fetch) has three ban-critical knobs that are
   validated at startup and will **refuse to start** the daemon if violated
   without an explicit override (ADR 0022):
   - `WHATSRUST_BACKFILL_INTERVAL_SECS` — floor 3s (override:
     `WHATSRUST_DANGEROUSLY_ALLOW_FAST_BACKFILL=1`)
   - `WHATSRUST_BACKFILL_MAX_CONCURRENT` — ceiling 3 (override:
     `WHATSRUST_DANGEROUSLY_ALLOW_HIGH_CONCURRENCY=1`)
   - `WHATSRUST_BACKFILL_MAX_MESSAGES` — ceiling 50000 (override:
     `WHATSRUST_DANGEROUSLY_ALLOW_HUGE_FETCH=1`)
   Each override triggers a persistent `WARN` in the logs. Don't set these
   unless you know what you're doing.

**Verification:** every knob above (with its default and a one-line
description) is documented, in the same order groups appear in the daemon's
own `--help` "ENVIRONMENT" section and in `.env.example`. Cross-check with:
```bash
./target/debug/whatsrust --help | sed -n '/ENVIRONMENT:/,/JID FORMAT:/p'
```

---

## 2a. Upgrade notes

### 2026-10-18: API startup validation

The daemon now refuses to start (exits non-zero with the fix) for invalid API configs:

| **Config** | **Fix** |
|---|---|
| `WHATSRUST_BIND` is non-loopback and `WHATSRUST_ALLOW_REMOTE` is not set | Set `WHATSRUST_ALLOW_REMOTE=1` and `WHATSRUST_API_TOKEN`, or use loopback (unset `WHATSRUST_BIND` or set `WHATSRUST_BIND=127.0.0.1`) |
| `WHATSRUST_BIND` is non-loopback, `WHATSRUST_ALLOW_REMOTE=1`, and no `WHATSRUST_API_TOKEN` | Set `WHATSRUST_API_TOKEN` (e.g. `openssl rand -base64 32`) |
| `WHATSRUST_ALLOWED_HOSTS` or `WHATSRUST_ALLOWED_ORIGINS` is non-empty and no `WHATSRUST_API_TOKEN`, even on loopback | Set `WHATSRUST_API_TOKEN`, or unset the allowlist |
| REST port in use, or `WHATSRUST_BIND` is not an address of this machine | Stop the other process, or set `WHATSRUST_PORT` to a free port (0 turns the API off) |

**CLI and `whatsrust mcp` environment note:** Both read `WHATSRUST_API_TOKEN` from the process environment. If you set a token in `.env`, also `export WHATSRUST_API_TOKEN=<value>` for CLI and MCP use — `.env` is loaded only by the daemon.

**Startup migration note:** The REST port is now bound before the bridge starts, so before the DB opens and migrates. During a long migration, CLI and stdio-MCP requests wait in the kernel backlog instead of failing fast with "connection refused". The CLI gives up after 45 s.

---

## 3. Run the daemon

**Goal:** get the bridge connected to WhatsApp and the API/REPL live.

**Steps:**
1. Start the daemon:
   ```bash
   make run        # == cargo run (daemon mode: no CLI args)
   ```
   or run the built binary directly: `./target/debug/whatsrust`.
2. **First run:** a QR code is rendered to the terminal. Scan it with
   WhatsApp on your phone (Linked Devices → Link a Device). If
   `WHATSAPP_PAIR_PHONE` is set instead, you'll be prompted for a pair code
   rather than a QR scan. The device appears in Linked Devices under
   `WHATSRUST_DEVICE_NAME` (default `whatsrust`); the name is fixed at this
   point (see §2 and §3a).
3. Once paired, the process keeps running as:
   - an **interactive REPL** on stdin (type `send <jid> <message>`, `status`,
     `groups`, `quit`, etc. — the daemon prints the full command list at
     startup),
   - a **REST API** on `127.0.0.1:7270` by default (loopback-only unless you
     opted into remote access per §2),
   - a background outbound-queue worker, backfill worker, and read-receipt
     scheduler.
4. Inbound messages print to the console as they arrive
   (`<< [<jid>] <sender> (<kind>): <text>`).
5. **This is interactive and does not exit on its own.** Stop it cleanly with
   **Ctrl+C** (SIGINT) or `SIGTERM`, or type `quit`/`q`/`exit` in the REPL.
   Shutdown drains in-flight outbound jobs (waits up to 5s) and writes a
   SQLite backup before the process exits.

**Verification:**
```bash
# In a second terminal, once the daemon is up:
curl -s http://127.0.0.1:7270/api/status
# or
./target/debug/whatsrust status
```
A JSON status blob confirms the API is live. If the daemon isn't running,
the CLI fails fast with a clear message rather than hanging:
```
whatsrust: cannot connect to whatsrust daemon on 127.0.0.1:7270: Connection refused (os error 61)
Is the daemon running? Start it with: WHATSRUST_PORT=7270 WHATSRUST_BIND=127.0.0.1 whatsrust
```
(exit code 1 — verified by running `whatsrust status` with no daemon up).

---

## 3a. Re-pairing (rename the device, or after a logout)

**Goal:** link whatsrust to WhatsApp again without losing local history.

Re-pairing does **not** delete your content. When the device is unlinked,
whatsrust clears only the device credentials and the saved device name
(`clear_device()`). Messages, the search index, embeddings, backfill jobs and
cursors all stay in `whatsapp.db` (ADR 0043).

**Steps:**
1. (Optional) To use a new name, set `WHATSRUST_DEVICE_NAME` in `.env` or the
   environment first.
2. On the phone: WhatsApp → Linked Devices → whatsrust → **Log out**. The
   daemon receives the logout and clears the credentials.
3. Restart the daemon (or let it reconnect) and scan the new QR code (or enter
   the pair code).

**What to expect:**
- History sync runs again, as on any first pairing. The messages it sends are
  already in the database; duplicates are skipped because each WhatsApp
  message ID is stored once.
- Older history stays local; fetch more on demand with `fetch-history` (§4).
- It's a new device identity to WhatsApp, so contacts' encryption sessions
  are set up again, and anything still in the outbound queue at the moment of
  logout may fail and need resending.
- **Do not delete `whatsapp.db` to re-pair.** That's the only way to lose the
  content. A startup backup is kept in `whatsapp.db.backups/` if you want an
  extra copy first.

**Verification:** `curl -s http://127.0.0.1:7270/api/status` shows
`"device_name"`: `null` until pairing completes, then the name you paired with.

---

## 4. Use the CLI

**Goal:** drive a running daemon from the command line (every CLI command
sends one HTTP request to the local API and prints JSON to stdout).

**Shape:** `whatsrust <subcommand> [args...]` — requires a daemon already
running (per §3) on the same `WHATSRUST_PORT`/`HEALTH_PORT` the CLI resolves
(default 7270).

**Steps / examples** (all verified against the `cli_main` match arms in
`src/main.rs`):
```bash
# Send a text message
whatsrust send 15551234567 "Hello from the CLI"

# Reply, quoting an earlier message
whatsrust reply 15551234567 3EB0A1B2C3D4E5F6 15551234567@s.whatsapp.net "sounds good"

# React to a message
whatsrust react 15551234567 3EB0A1B2C3D4E5F6 thumbsup

# Recent messages in a chat (local SQLite history)
whatsrust history 15551234567 20

# Full-text search across stored history (FTS5, BM25 relevance-ranked)
whatsrust search "dinner plans"
whatsrust search "printer" 15551234567     # scoped to one chat

# Trigger a historical backfill job for a chat
whatsrust fetch-history 15551234567 all
whatsrust fetch-history 15551234567 since 1700000000000
whatsrust fetch-history 15551234567 count 500

# Check backfill job status (no arg = active jobs only; "all" = every job;
# an integer = one job by id)
whatsrust fetch-status
whatsrust fetch-status all
whatsrust fetch-status 42

# Cancel a running backfill job
whatsrust fetch-cancel 42

# Purge embeddings for a specific model (reclaim disk space)
whatsrust purge-embeddings paraphrase-multilingual-MiniLM-L12-v2

# Groups
whatsrust groups
whatsrust group-info 120363012345678901@g.us

# Live event stream (SSE: inbound messages, outbound status, backfill
# progress, storage-growth alerts)
whatsrust events
```
JID format (from the daemon's own `--help`): a bare phone number
(`15551234567`) or the full JID (`15551234567@s.whatsapp.net`); groups use
`<id>@g.us`.

**Verification:** `whatsrust <cmd> ... ` prints a JSON object to stdout and
exits non-zero on failure (`{"ok": false, ...}` or an HTTP error status).
Run `whatsrust --help` (or `whatsrust help`) at any time for the authoritative,
in-binary list of every subcommand.

---

## 5. MCP mode

**Goal:** expose whatsrust to an MCP-compatible AI agent (Claude Code,
Cursor, etc.) over JSON-RPC/stdio.

**Steps:**
1. With a daemon already running (per §3) on some port, launch the MCP
   server pointed at that same port:
   ```bash
   whatsrust mcp
   ```
   This blocks on stdin, reading JSON-RPC requests line-by-line and writing
   responses to stdout until EOF (`src/mcp.rs::run_mcp_server`). It does
   **not** start its own bridge — every tool call is proxied over HTTP to the
   already-running daemon's REST API on the resolved port (default 7270,
   `WHATSRUST_PORT`/`HEALTH_PORT`).
2. Point your MCP client at it. This repo's own `.mcp.json` shows the
   pattern used for local development:
   ```json
   {
     "mcpServers": {
       "whatsrust": {
         "type": "stdio",
         "command": "/absolute/path/to/target/debug/whatsrust",
         "args": ["mcp"]
       }
     }
   }
   ```
   Adjust the `command` path to your built binary (debug or release) and add
   any `env` vars you need (e.g. `WHATSRUST_PORT` if not using the default).
3. 34 tools are exposed (send, reply, react, groups, chat management,
   status/stories, `whatsrust_search`, `whatsrust_fetch_history`,
   `whatsrust_fetch_status`, `whatsrust_fetch_cancel`,
   `whatsrust_purge_embeddings`, etc.) — see `README.md` § "MCP server
   for AI agents" for the full narrative list.
4. **MCP-specific behaviour (ADR 0046):** undeclared arguments are rejected
   (isError naming them, nothing sent); wrong-type values and missing required
   fields are rejected (isError naming the fields, nothing sent); unknown tools
   and non-object arguments return JSON-RPC `-32602`; every tool carries MCP
   annotations (`readOnlyHint` and `openWorldHint`; write tools also
   `destructiveHint` and `idempotentHint`). Media tools accept only base64 `data` — the local-file
   `path` mode (available in the REST API for CLI use) is unreachable via MCP.
   `whatsrust_contact` takes `phone` (international format, digits only), not a
   pre-built vCard. `whatsrust_send` via MCP has no link preview or scheduling —
   use REST async `/api/send` for those. Synchronous sends that time out (30 s)
   return HTTP 504 with `job_id` and "do not retry" (ADR 0047); the job stays
   queued and is usually delivered later.

**Verification:** the daemon's `/api/status` responding (§3) implies MCP tool
calls will succeed, since MCP mode has no state of its own beyond the proxy —
a failed daemon connection surfaces as a tool-call error identical to the
CLI's connection-refused message in §3.

---

## 5a. Embedder sidecar (optional semantic search)

**Goal:** enable semantic (embedding-based) search on top of the always-on
FTS5 lexical search, with **hybrid recall** (ADR 0041): paraphrase and
cross-language queries that share no words with a message can still find it,
not just get it reordered. Opt-in via environment variables; gracefully
degrades to pure lexical when the sidecar is absent or down.

### How the sidecar works

The daemon never embeds anything itself. At startup it spawns whatever
`WHATSRUST_EMBEDDER_CMD` points at (plus `WHATSRUST_EMBEDDER_ARGS`, if set) as
a child process, and speaks newline-delimited JSON-RPC 2.0 over its
stdin/stdout (ADR 0006/0024); the child's stderr is discarded. **The sidecar,
not the daemon, decides which model it serves** — the Rust sidecar reads its
own `--model` flag (or inherits `WHATSRUST_EMBEDDER_MODEL`, since env vars
propagate to child processes). The daemon just reads back whatever
`model_info` reports and stores vectors keyed by `(message_id, model_id)`, so
it works with any model the sidecar advertises, past or future.

`model_info` answers instantly from built-in static metadata (`model_id`,
`dim`, `max_batch`, `max_input_tokens`) — it never blocks on loading the
model. The actual weights load on a background thread at startup; `health`
reports `"loading"` and then `"ok"` (or `"error"`) once that thread finishes,
and the embedding-drain worker waits on `health` before sending real work, up
to `WHATSRUST_EMBEDDER_LOADING_TIMEOUT_SECS`. Each `embed` call does
tokenize → transformer forward pass → pool (mean-pooling for MiniLM,
first-token/CLS for bge-m3) → L2-normalize, and the daemon validates every
response (dimension, vector count, model ID match) before trusting it
(trust-but-verify, ADR 0024). Supporting a different/future model means
changing the sidecar, not the daemon — see `embedder-rs/src/main.rs`.

### Choosing a model

| | MiniLM (`paraphrase-multilingual-MiniLM-L12-v2`) | bge-m3 (`BAAI/bge-m3`) |
|---|---|---|
| Dimensions | 384 | 1024 |
| Disk (first-run download) | ~0.46 GB | ~2.2 GB |
| RAM while running | ~0.5–1.3 GB | ~1.3–2.3 GB |
| Warm single query | 30–70 ms | 230–480 ms |
| Max embed batch | 32 | 8 |
| Cross-language/paraphrase recall | weaker | stronger |

bge-m3 is the better default for mixed-language (e.g. Hebrew/Arabic/English)
history: in calibration, 6 of 11 word-free cross-language queries ranked
their intended target #1 of 24 candidates, vs. 3 of 11 for MiniLM (see
docs/adr/0041-hybrid-recall-bounded-vector-scan.md). MiniLM stays worth
choosing deliberately when you want the smaller download/RAM footprint and
faster queries, and can tolerate weaker cross-language recall.

- **bge-m3 (default):**
  ```bash
  make run-semantic
  ```
- **MiniLM:**
  ```bash
  make run-semantic EMBED_MODEL=minilm
  ```
- **Manual/`.env` form (either model):**
  ```bash
  WHATSRUST_EMBEDDER_CMD=/absolute/path/to/whatsrust/embedder-rs/target/release/whatsrust-embedder
  WHATSRUST_EMBEDDER_ARGS="--model bge-m3"      # or: --model minilm
  ```
  `make run-semantic` builds the sidecar first (`make embedder-build`,
  producing `embedder-rs/target/release/whatsrust-embedder` — a separate
  crate so candle stays out of the daemon's own build) and wires both env
  vars with an absolute path automatically; no manual `.env` edit needed.
  Test the sidecar standalone (without starting the daemon) with
  `make embedder-smoke` (bge-m3) or `make embedder-smoke EMBED_MODEL=minilm` —
  it pipes a `model_info` + one `embed` request into the binary and prints the
  raw JSON-RPC responses.

### Switching models

Changing the active model means **restarting the daemon** with a different
`--model`/`WHATSRUST_EMBEDDER_ARGS` (there's no live hot-swap). On the next
start:
- The drain worker's set-difference query sees every embeddable row as
  missing a vector for the *new* `model_id` and re-embeds all of them —
  expect a burst of drain activity proportional to history size.
- The *old* model's vectors are **not deleted**. They sit as inert cold
  storage (`embeddings` table, keyed by `(message_id, model_id)`), so
  switching back to the old model is instant (no re-embedding) — but they are
  never read while a different model is active (ADR 0007/0017). Free the disk
  space with:
  ```bash
  whatsrust purge-embeddings paraphrase-multilingual-MiniLM-L12-v2
  # Returns: {"ok": true, "model_id": "...", "rows_deleted": N, "bytes_reclaimed": B}
  ```
- **Re-measure the similarity floor for the new model** — `WHATSRUST_SEMANTIC_MIN_SCORE`
  is model-specific (ADR 0041); the shipped default (0.55) is calibrated for
  bge-m3 and is deliberately conservative for MiniLM. Run
  `make calibrate EMBED_MODEL=<minilm|bge-m3>` against your own data
  (read-only against `whatsapp.db`) and set `WHATSRUST_SEMANTIC_MIN_SCORE` if
  the result suggests a different floor.
- Watch re-embedding progress in `/api/status` → `embeddings`. As of ADR 0041
  this is keyed to the **active** model only:
  `{"model_id": "...", "embedded": N, "pending": M, "failed": F, "skipped": S}`
  (`embedded`/`pending` are omitted and `model_id` is `null` when no embedder
  is configured). `pending` dropping to 0 means the new model has caught up.

### How search uses it

When the sidecar is active and the query embeds successfully, `search` is
**hybrid**: FTS5 keyword candidates, plus a bounded brute-force cosine scan
of every vector stored for the active model in the search scope (same chat
filter, same time bound), merged and deduplicated by message ID. Keyword
matches are **always** kept, no matter their semantic score. Vector-only
matches (no shared keyword — the case this exists for) need
`cosine ≥ WHATSRUST_SEMANTIC_MIN_SCORE` to be admitted.
`WHATSRUST_SEMANTIC_SCAN_CAP` (default 50000 vectors) bounds the scan; a
scope larger than that skips vector recall for that query and falls back to
plain FTS5 + rerank. With no sidecar configured (or an unhealthy one, a
query-embed failure, or a query-embed timeout — see below), search is
byte-identical to plain FTS5 keyword search — no errors, no blocking. See
`docs/adr/0041-hybrid-recall-bounded-vector-scan.md`.

**The query-embed wait and cool-down (ADR 0042).** Each search gets one overall
budget, `WHATSRUST_SEMANTIC_QUERY_TIMEOUT_SECS` (default **20s**), covering both
waiting for the sidecar to finish loading (if `health()` reports `Loading`) and
waiting for the embed reply. The embed request is sent once and never re-sent —
re-sending would just queue behind the original in the sidecar's one-at-a-time
stdin loop. If it's still pending at 5s, 10s, and 20s (whichever checkpoints
fall inside the budget), you'll see a log line like:
```
search: query embed still pending after 10s (sidecar slow — likely swapping)
```
If the whole budget runs out, that search falls back to lexical-only AND
semantic search enters a **cool-down**: later searches skip the embed entirely
(instant keyword-only results, no sidecar round trip) for 30s. The window
doubles on each further consecutive cap-hit, up to 10 minutes, and resets back
to 30s on the first successful embed afterward (from either a search or the
embedding-drain worker). Entering and leaving the cool-down are each logged
once. Occasional slowness (e.g. the sidecar paging bge-m3's weights back in
after being idle) still gets the full wait and returns semantic results; only
a genuinely long stall triggers the cool-down.

### Model files and cache

Both models' weights/tokenizer/config are fetched from Hugging Face Hub into
the standard local cache, `~/.cache/huggingface/hub` (override with `HF_HOME`
or `HF_HUB_CACHE`, both honored). The sidecar starts loading its model in the
background at startup, which also downloads it on a cold cache; once cached, `HF_HUB_OFFLINE=1` works with no network
access. bge-m3's weights are loaded from a pinned community safetensors
conversion revision rather than its `main` branch (which ships only a
PyTorch checkpoint) — see ADR 0040 for why, and for the cold-cache download
fix that makes first-run downloads work reliably for both models.

### Legacy Python sidecar (reference/fallback)

`scripts/embedder-sidecar.py` (sentence-transformers, MiniLM only) remains
available for parity checks against the Rust sidecar:
```bash
make py-sidecar-setup   # creates .venv-embedder, installs sentence-transformers
make py-sidecar-smoke   # standalone model_info check
make run-semantic-py    # run the daemon with it (or WHATSRUST_EMBEDDER_CMD=./scripts/run-embedder.sh + make run)
```
Direction-identical to the Rust sidecar (cosine = 1.0 on parity tests,
`scripts/test_embedder_parity.py`) but needs Python 3.8+ and ~GB of PyTorch
deps, and doesn't support bge-m3. Use the Rust sidecar for real deployment.

### Verification

Start the daemon with a sidecar configured and check the log for, in order:
`embedder configured model=… dim=…` (construction-time `model_info` round
trip succeeded) and then `embedding-drain worker starting` (the drain
worker spawned). Without a sidecar (or an invalid `WHATSRUST_EMBEDDER_CMD`),
search still works (pure FTS5) and the daemon logs
`no embedder configured or construction failed` at DEBUG — non-fatal by
design.

**Note:** the semantic search path (including ADR 0041 hybrid recall) has been
live-validated (2026-10-05) against a running daemon with real data. Report
issues to the repo.

---

## 6. DB migration recovery (`--rollback` / `--migrate`)

**Goal:** recover from a failed schema migration without losing the ability
to run the old binary against the old data.

Background (ADR 0028/0029/0030): on startup, whatsrust takes a pre-migration
backup (`<db>.pre-migration-v<old-version>-<unix-ts>.bak`, e.g.
`whatsapp.db.pre-migration-v7-1783287781.bak`) before attempting a schema
migration. If migration fails, it writes a **circuit-breaker pin file**
(`<db>.migration-pin`) recording `state=failed` and refuses to auto-retry the
migration on subsequent starts — you must act with one of the two flags
below. Both flags are maintenance subcommands: they run **before** the
instance lock and before CLI-mode dispatch, and both **exit the process**
instead of starting the daemon.

### 6a. `--rollback` — restore the pre-migration backup

**When:** the pin file shows `state=failed` (a migration attempt failed) and
you want to go back to the old schema/binary.

**Steps:**
```bash
# Uses the newest matching *.pre-migration-v*.bak next to whatsapp.db
whatsrust --rollback

# Or specify an exact backup file
whatsrust --rollback --bak whatsapp.db.pre-migration-v7-1783287781.bak
```
This acquires the instance lock (fails if a daemon is already running),
copies the `.bak` over `whatsapp.db`, deletes any stale `-wal`/`-shm`
sidecars (critical: a stale WAL replaying onto the restored DB would corrupt
it), updates the pin to `state=rolled_back`, and prints the restored schema
version. It refuses to run if there's no pin file at all, or if the pin
isn't in the `failed` state (guards against rolling back a healthy DB).

**Verification:** the command prints
`whatsrust --rollback: DONE. DB restored to v<N> from <path>.` along with a
warning that any messages received after the failed migration are lost.
After this, **run the OLD binary** (matching the restored schema version) to
continue operating, or proceed to §6b to retry migration with the current
(new) binary.

### 6b. `--migrate` — clear the breaker and retry migration

**When:** you've fixed whatever caused the migration to fail (or just want
to force a retry attempt) and want the **current** binary to try again.

**Steps:**
```bash
whatsrust --migrate
```
This acquires the instance lock, clears/ignores the circuit-breaker pin, and
runs the normal staged migration path. On success it prints
`whatsrust --migrate: migration succeeded. Starting daemon normally...` and
exits — **restart without `--migrate`** to actually run the daemon. On
failure it prints the error and exits 1 (the pin is re-written as `failed`,
so you're back to needing `--rollback` or another `--migrate` attempt).

**Verification:** check the pin state directly if needed —
`cat whatsapp.db.migration-pin` (JSON) shows `state` (`failed` /
`rolled_back`) and `pinned_version`.

---

## 7. Test & lint

**Goal:** run the project's test and lint suites the same way CI/contributors
do, via the Makefile.

**Steps:**
```bash
make test        # cargo test — default suite (470 lib + 437 bin tests, verified 2026-10-18)
make test-all     # cargo test --features fake-embedder — also runs the
                   # fake-embedder sidecar round-trip tests (479 lib + 446 bin;
                   # 9 extra tests per target vs `make test`, gated behind the
                   # non-default `fake-embedder` Cargo feature so a plain
                   # `cargo build`/`cargo test` never touches that sidecar
                   # binary)

make lint         # cargo clippy — reports lints, non-fatal. The crate
                   # currently carries ~30 pre-existing findings, so this is
                   # intentionally NOT `-D warnings` (a strict run would fail
                   # out of the box on a clean checkout).
make lint-strict  # cargo clippy -- -D warnings — fatal-on-warning variant;
                   # expected to fail today until the existing backlog is
                   # cleaned up. Use this only when you want to confirm your
                   # own change introduced zero *new* clippy warnings by
                   # comparing counts before/after, not as a pass/fail gate.

make fmt-check    # cargo fmt --check — reports formatting diffs without
                   # writing anything. NOTE: this currently reports a large
                   # diff across the codebase (it has not been run through
                   # rustfmt historically) — treat as informational.
make fmt          # cargo fmt — reformats the codebase in place. Not run as
                   # part of any other target; use deliberately.
```

**Verification (outputs captured while writing this doc):**
- `make help` — lists all targets with one-line descriptions, default target.
- `make build` — `Finished \`dev\` profile [unoptimized + debuginfo] target(s)`, exit 0.
- `make lint` — `whatsrust (bin "whatsrust") generated 48 warnings`, exit 0 (non-fatal).
- `make test` — `test result: ok. 470 passed; 0 failed ...` (lib) and
  `test result: ok. 437 passed; 0 failed ...` (bin), exit 0 (verified 2026-10-18).
- `make test-all` — `test result: ok. 479 passed; 0 failed ...` (lib, includes
  the 9 extra fake-embedder-gated tests per target) and `test result: ok. 446 passed; 0 failed ...` (bin), exit 0 (verified 2026-10-18).

`CONTRIBUTING.md` asks contributors to run `cargo test` and `cargo clippy`
before submitting a PR — `make test` and `make lint` are equivalent to that,
plus `make test-all` for full coverage of the embedder sidecar path.

**REST golden tests:** The test suite includes byte-for-byte golden tests of the REST API's wire output (`src/api/golden_tests.rs`). These tests pin every byte the server writes for a comprehensive set of full request/response cycles plus helper-level fixtures (status lines, headers, JSON bodies, error responses). A failing golden case means the REST wire output changed. If the change is deliberate (e.g. fixing a status code or reason phrase), paste the printed literal into the fixture and call the change out in the commit message. Otherwise it is a regression.

---

## Gaps / not verified

- No CI workflow file exists in this repo (no `.github/workflows/`) to cross-
  check these targets against; the verification above is direct execution,
  not CI parity.
- `cargo fmt` was **not** run for real as part of producing this doc (only
  `--check`), since doing so would rewrite most of the source tree — treat
  the `fmt`/`fmt-check` targets as available but not exercised destructively
  here.
- A full `cargo build --release` (with LTO) was not executed end-to-end while
  writing this doc, to keep turnaround fast; the release profile settings
  were verified by reading `Cargo.toml` (`opt-level = "z"`, `lto = true`,
  `codegen-units = 1`, `panic = "abort"`, `strip = true`) and `make release`
  simply invokes `cargo build --release`, which is the standard, unmodified
  command.
