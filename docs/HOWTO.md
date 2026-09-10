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
   - `BACKUP_DIR` (default `whatsapp.db.backups`) — where SQLite backups are
     written on startup/shutdown.
3. (Optional) point at a config file somewhere other than `./.env` by setting
   `WHATSRUST_ENV_FILE=/path/to/file` in the **real** environment (not inside
   `.env` itself — the daemon must know the path before it loads the file).
4. Remote API access is off by default. To allow a non-loopback bind, you
   must set both `WHATSRUST_ALLOW_REMOTE=1` **and** `WHATSRUST_API_TOKEN=...`
   (the server enforces the token when remote access is enabled; see
   `src/api.rs`).
5. Backfill (historical fetch) has three ban-critical knobs that are
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
   rather than a QR scan.
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
3. 35 tools are exposed (send, reply, react, groups, chat management,
   status/stories, `whatsrust_search`, `whatsrust_fetch_history`,
   `whatsrust_fetch_status`, `whatsrust_fetch_cancel`,
   `whatsrust_purge_embeddings`, etc.) — see `README.md` § "MCP server
   for AI agents" for the full narrative list.

**Verification:** the daemon's `/api/status` responding (§3) implies MCP tool
calls will succeed, since MCP mode has no state of its own beyond the proxy —
a failed daemon connection surfaces as a tool-call error identical to the
CLI's connection-refused message in §3.

---

## 5a. Embedder sidecar (optional semantic search)

**Goal:** enable semantic (embedding-based) search on top of the always-on
FTS5 lexical search. Opt-in via environment variables; gracefully degrades to
lexical when absent or down.

**Background:** whatsrust ships with FTS5 lexical search (M1, DONE). Semantic
search (M2.1–M2.5, code-complete; M2.6 + E2E validation pending) layers on top
via a stateless embedder sidecar (a separate process that embeds text on
demand). When configured, the bridge embeds the query, recalls FTS candidates,
reranks by cosine similarity, and appends an additive lexical fallback. When
the sidecar is absent or down, search is byte-identical to M1 lexical (no
errors, no blocking).

**Steps:**
1. Set the embedder command in `.env` (or export directly). `WHATSRUST_EMBEDDER_CMD`
   is a single executable path; the script path goes in `WHATSRUST_EMBEDDER_ARGS`
   (whitespace-split into args). There is **no** model-ID env var — the model ID is
   reported by the sidecar itself via its `model_info` response.
   ```bash
   WHATSRUST_EMBEDDER_CMD=".venv-embedder/bin/python3"
   WHATSRUST_EMBEDDER_ARGS="scripts/embedder-sidecar.py"
   ```
   The MVP sidecar (`scripts/embedder-sidecar.py`) requires Python 3.8+ and
   `sentence-transformers` (`pip install sentence-transformers`, ideally into the
   `.venv-embedder` venv). The default model (`paraphrase-multilingual-MiniLM-L12-v2`,
   384-dim, prefix-free) is downloaded on first run (~420 MB) to `~/.cache/huggingface`.
2. (Recommended) A convenience wrapper `scripts/run-embedder.sh` exists; it `cd`s to
   the repo root and execs the `.venv-embedder` Python on the sidecar script. To use
   it, set `WHATSRUST_EMBEDDER_CMD="./scripts/run-embedder.sh"` (no `WHATSRUST_EMBEDDER_ARGS`
   needed).
3. Start the daemon as usual (§3). The embedding-drain worker spawns
   automatically and begins processing `embed_status='pending'` rows in the
   background (default: 60s interval, 64 msgs/batch). No action required;
   watch the logs for `embedding-drain` activity.
4. Search as usual (`whatsrust search "query text"`, or via API/MCP). When
   embeddings exist for the query and recalled messages, results are reranked
   by semantic similarity; otherwise falls back to FTS5 lexical.

**Purging embeddings (model switch / disk reclaim):**
```bash
# Remove all embeddings for a specific model ID and reclaim disk space
whatsrust purge-embeddings paraphrase-multilingual-MiniLM-L12-v2
# Returns: {"ok": true, "model_id": "...", "rows_deleted": N, "bytes_reclaimed": B}
```
Per-model purge allows switching embedding models without losing lexical search
or requiring a schema migration. The drain worker will re-embed pending rows
for the new active model.

**Verification:** with the sidecar configured, trigger a search and check the
daemon logs for `sidecar embed` calls. Without the sidecar (or with an invalid
`WHATSRUST_EMBEDDER_CMD`), search still works (pure lexical) and the logs show
`no embedder configured` or a sidecar-down warning.

**Note:** End-to-end validation of the semantic search path against a live
daemon is pending (M2.6). The code is complete but not yet live-tested in a
real-world scenario. Use at your own risk; report issues to the repo.

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
make test        # cargo test — default suite (231 lib + 233 bin tests, verified)
make test-all     # cargo test --features fake-embedder — also runs the
                   # fake-embedder sidecar round-trip tests (234 lib tests;
                   # 3 extra tests vs `make test`, gated behind the
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
- `make lint` — `whatsrust (bin "whatsrust") generated 38 warnings`, exit 0 (non-fatal).
- `make test` — `test result: ok. 231 passed; 0 failed ...` (lib) and
  `test result: ok. 233 passed; 0 failed ...` (bin), exit 0.
- `make test-all` — `test result: ok. 234 passed; 0 failed ...` (lib, includes
  the 3 fake-embedder-gated tests) and `233 passed` (bin), exit 0.

`CONTRIBUTING.md` asks contributors to run `cargo test` and `cargo clippy`
before submitting a PR — `make test` and `make lint` are equivalent to that,
plus `make test-all` for full coverage of the embedder sidecar path.

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
