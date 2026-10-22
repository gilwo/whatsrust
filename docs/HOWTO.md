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

### Run profiles (state in `runs/<profile>/`)

**Why:** the daemon keeps its state in the directory it starts in: `whatsapp.db` (+ `-wal`, `-shm`),
`whatsapp.db.backups/`, `whatsapp.db.instance.lock`, the `whatsapp.db.pre-migration-*.bak` files, and it
loads `./.env`. Every debug or live run from the project root therefore shares one pile of state. A run
profile gives each run its own folder, `runs/<profile>/` (git-ignored): its own DB, backups, lock, `.env`
and `logs/`. The launcher `scripts/run-profile.sh` just starts the daemon with that folder as working
directory; there is no Rust change.

```bash
scripts/run-profile.sh new dev              # create runs/dev/ (logs/ + .env); --semantic enables the embedder sidecar, --mcp switches the HTTP MCP listener on
scripts/run-profile.sh daemon dev           # build once (--release for the release binary), run in runs/dev/ in THIS terminal; Ctrl+C stops it
scripts/run-profile.sh daemon dev --tmux    # same, but in tmux session whatsrust-dev (opt-in); attaches when run from a terminal
scripts/run-profile.sh daemon dev --detach  # implies --tmux; leave it running in the background (prints how to attach)
scripts/run-profile.sh attach dev           # attach to the session (Ctrl-b d detaches; the daemon keeps running)
scripts/run-profile.sh list                 # every profile: REST/MCP port, DB present, instance lock held, tmux running or -
scripts/run-profile.sh list --tmux          # only the profiles whose daemon runs in a tmux session
scripts/run-profile.sh cli dev status       # `whatsrust status` against dev's daemon
scripts/run-profile.sh mcp dev              # `whatsrust mcp` (stdio) against dev's daemon, for an MCP client config
scripts/run-profile.sh env dev              # port/bind, whether a token is set, whether the MCP listener is on (tokens are never shown)
scripts/run-profile.sh mcp-on dev           # switch the HTTP MCP listener (§5a) on: port + token go into the .env, the token is never shown
scripts/run-profile.sh mcp-off dev          # switch it off again (the token line stays)
scripts/run-profile.sh mcp-client dev       # URL, Claude Code config (token as ${WHATSRUST_MCP_TOKEN}) and how to export the token
scripts/run-profile.sh clear-data dev       # empty dev's message data and keep its pairing (daemon stopped; shows the SQL, asks twice, backs up first; --dry-run only looks, --all adds the send queue and sent store)
scripts/run-profile.sh --help               # all commands, options, environment variables, examples, exit codes
make new-profile P=dev [SEMANTIC=1] [MCP=1]         # same as `new` (MCP=1 adds --mcp)
make run-profile P=dev [RELEASE=1] [USE_TMUX=1]     # same as `daemon` (USE_TMUX=1 adds --tmux)
make attach-profile P=dev                           # same as `attach`
```

- **Ports:** the first profile gets REST `9270` and MCP `9271`, then `9280/9281`, `9290/9291`, … (step 10).
  `new` picks the lowest pair that no other profile's `.env` mentions; commented-out lines count, so a
  profile keeps the MCP port reserved for it while the listener is off. All of these differ from the default
  `7270/7271`.
- **`.env`:** generated from `.env.example` (every example line stays commented) with an active block on top:
  `WHATSRUST_PORT`, `WHATSRUST_DEVICE_NAME=whatsrust-<profile>` (each profile is its own linked device, so
  each shows up under its own name) and `RUST_LOG`. With `--semantic` it also sets
  `WHATSRUST_EMBEDDER_CMD`/`WHATSRUST_EMBEDDER_ARGS` (run `make embedder-build` once). The HTTP MCP
  listener (§5a) starts off, with its port reserved as a commented `# WHATSRUST_MCP_PORT=<port>` line: switch it
  on with `mcp-on` (below) or create the profile with `new --mcp`. Profile names use letters, digits, `.`, `_`, `-`.
- **Default: this terminal.** `daemon` builds once (build errors show right there), then runs the daemon in the
  current terminal with `runs/<profile>/` as its working directory. The REPL on stdin works as usual and **Ctrl+C**
  is the normal graceful stop. It ends with `daemon exited with code <n> (log: ...)`. Closing the terminal stops
  the daemon. In this mode the script neither checks for nor mentions tmux.
- **tmux (opt-in):** `daemon <profile> --tmux` (or `make run-profile P=<profile> USE_TMUX=1`) needs `tmux` 3.2 or
  newer. It builds in your terminal, then starts the daemon in a tmux session named `whatsrust-<profile>` whose
  start directory is `runs/<profile>/`. Run from a terminal it attaches right away (it switches your client when
  you are already inside tmux); `--detach` (which implies `--tmux`), or running without a terminal (a script, CI),
  leaves the session in the background and prints `attach with: scripts/run-profile.sh attach <profile>`. Either
  way it prints the session name and the log path. Detach with **Ctrl-b d**: the daemon keeps running, so a closed
  terminal no longer stops it. `attach <profile>` (or `make attach-profile P=<profile>`) reattaches; with no such
  session it says how to start one. A second `daemon <profile> --tmux` while the session exists is refused. A `.`
  in a profile name becomes `_` in the session name (tmux does that). The session is created at the size of your
  terminal so the pairing QR code is not cut off. If tmux is not installed, `--tmux` and `--detach` stop with
  `tmux not found (install: brew install tmux); run without --tmux to use this terminal` (exit 1): there is no
  silent fallback.
- **When the daemon stops inside tmux** (it exits, Ctrl+C, SIGTERM, SIGHUP, a crash), the pane does not vanish. It
  prints `daemon ended: exit code <n>` (for codes above 128 also the signal, e.g. `exit code 143 (SIGTERM)`), the
  log path, and `You are in tmux session whatsrust-<profile>: press Enter to close it, or Ctrl-b d to detach.`
  Press Enter to close the session. If you kill the whole session (`tmux kill-session`) no note is possible.
- **Logs:** `daemon` prints to the terminal (or the tmux pane) with colours and writes the same output, without
  colour codes, to `runs/<profile>/logs/<UTC-YYYYmmdd-HHMMSS>.log` (mode 600; treat it as private). In tmux,
  type REPL commands into the attached pane.
- **Environment:** variables exported in your shell beat the profile's `.env` (§2); when `daemon` runs in your
  terminal (the default) it names any such variable (never its value) when it starts. **A tmux session
  (`--tmux`) does not get your shell's environment:** only `WHATSRUST_ENV_FILE` (the profile's `.env`) is passed
  in, and no other variable, so tokens never appear in a process argument list. Exported `WHATSRUST_*`, `WHATSAPP_*`, `HEALTH_PORT`,
  `BACKUP_DIR` and `RUST_LOG` are therefore **not applied** inside the session (the warning at start says so):
  put those settings in the profile's `.env`. (If an already running tmux server carries such variables in its
  own environment, the pane warns that they override the `.env`.) `cli` and `mcp` read only `WHATSRUST_PORT`,
  `WHATSRUST_BIND` and `WHATSRUST_API_TOKEN` from the profile's `.env`. For an MCP client config, use
  `scripts/run-profile.sh` as the command and `["mcp", "<profile>"]` as the arguments.
- **`list`:** by default shows every profile with REST, MCP, DB, LOCK and a TMUX column (`running` when its
  `whatsrust-<profile>` session exists, else `-`; also `-` when tmux is not installed, with no note). `list --tmux`
  shows only the profiles whose daemon runs in such a session (SESSION, PROFILE, REST, MCP, STARTED in local time,
  ATTACHED yes/no); other tmux sessions are ignored. With none it prints
  `no run-profile daemons in tmux (scripts/run-profile.sh list shows every profile)`; without tmux installed it
  stops with `tmux not found (install: brew install tmux)` (exit 1). `list --all` is an alias of the default
  view; `--all` and `--tmux` cannot be combined. A session whose daemon has already stopped but is still waiting
  for Enter counts as running.
- **`list` lock column:** `HELD` means a process has `runs/<profile>/whatsapp.db.instance.lock` open
  (checked with `lsof`; `unknown` if `lsof` is missing). The lock file stays on disk after a stop; that is normal.
- **Testing/advanced:** `RUN_PROFILE_TMUX_SOCKET=<name>` makes every tmux call of the script use the private
  server `tmux -L <name>` instead of your default one (used by the script's own checks).

**MCP listener (HTTP) of a profile.** `mcp-on`, `mcp-off` and `mcp-client` switch and use the opt-in HTTP listener of §5a
for one profile, so you never edit the `.env` by hand or paste a token. `new <profile> --mcp` (`make new-profile P=<profile> MCP=1`,
combinable with `SEMANTIC=1`) does the `mcp-on` step while creating the profile.

- **`mcp-on <profile>`** writes two active lines to the profile's `.env`, replacing the commented placeholders so there is
  exactly one active line per key (the `.env.example` part is only touched if a placeholder is missing: the first commented
  line of that key is used, else a line is appended):
  - *Port:* the one reserved for the profile (its commented `WHATSRUST_MCP_PORT`), else the REST port + 1 when no other
    profile mentions it, else the next port that is neither the profile's REST port nor mentioned in another profile's
    `.env`. The daemon refuses an MCP port equal to the REST port.
  - *Token:* (1) an active `WHATSRUST_MCP_TOKEN` already in the `.env` is **kept** (`token: kept`); (2) else a
    `WHATSRUST_MCP_TOKEN` exported in your shell is used (`token: from your environment`): it must be at least 22
    characters (after trimming), differ from the profile's `WHATSRUST_API_TOKEN` and consist of `A-Z a-z 0-9 + / = _ . ~ -`
    (so it stays intact in an unquoted `.env` value); otherwise `mcp-on` stops with the reason and changes nothing, it does
    not fall back to generating one; (3) else it **generates** one with `openssl rand -base64 32` (`token: generated`, 44
    characters; without `openssl` it stops and says how to install it or to export your own token). A token already in the
    `.env` that the daemon would refuse (too short, equal to the API token) also stops `mcp-on` instead of being replaced.
  - The token is never printed, never on any command line, and the `.env` stays mode 600: the file is rewritten with bash
    builtins into a mode-600 temp file next to it and moved over the original.
  - Already on: `MCP listener already enabled on port <n>`, exit 0, the file is not rewritten (a missing token is added).
- **`mcp-off <profile>`** comments out `WHATSRUST_MCP_PORT` and **keeps the token line**, so a later `mcp-on` reuses the same
  token (`token: kept`) and clients that already hold it keep working. Already off: `MCP listener is already off for <profile>`.
- **Restart:** the daemon reads its `.env` only at start. When the profile's daemon is running (its instance lock is held, or
  a `whatsrust-<profile>` tmux session exists) both commands add `restart the daemon to apply`.
- **`mcp-client <profile>`** prints the URL (`http://<WHATSRUST_MCP_BIND or 127.0.0.1>:<port>/mcp`), the Claude Code
  config as a `.mcp.json` snippet and as a `claude mcp add-json` line (both as in §5a, with `"timeout": 120000`; the token is
  only the text `${WHATSRUST_MCP_TOKEN}`, so Claude Code expands it from its environment), and an `export` line that reads the
  token from the `.env` into your shell without printing it. With the listener off it prints
  `MCP listener is off for <profile> (enable it with: scripts/run-profile.sh mcp-on <profile>)` and exits 1. A non-loopback
  `WHATSRUST_MCP_BIND` (which the daemon accepts only with `WHATSRUST_MCP_ALLOW_REMOTE=1`, set by hand in the `.env`) gets a
  note that a TLS reverse proxy is required (§5a).
- **One variable, one profile:** Claude Code reads `${WHATSRUST_MCP_TOKEN}` when it starts, so start it from a shell where the
  variable is exported, and note that one variable holds one token. To use several profiles at once, give each its own
  variable: `mcp-client` also shows the snippet's variable renamed (for profile `demo`: `${WHATSRUST_MCP_TOKEN_DEMO}`) and the
  matching `export`.
- **Do not export `WHATSRUST_MCP_TOKEN` in the shell that starts the daemon:** an exported variable beats the profile's
  `.env` (see Environment above), so that daemon would use the exported token instead of the `.env` one. Export it in the
  shell for Claude Code (a daemon in a tmux session never sees it).
- `env <profile>` shows `WHATSRUST_MCP_PORT=<n>` or `<off>` and `WHATSRUST_MCP_TOKEN=<set>` or `<unset>`; `list` shows the
  active port, or `off (<reserved port>)`.

**Empty a profile's message data (`clear-data`).** `clear-data <profile> [--all] [--dry-run] [--yes]` deletes the message
rows in `runs/<profile>/whatsapp.db` and keeps the pairing, so you can retest history fetches without deleting the DB.
Deleting `whatsapp.db` forces a re-pair (a new QR code and a new linked device). It needs `sqlite3` on the PATH.

- **Cleared by default:** `embeddings`, `media_refs`, `messages`, `backfill_jobs`, `backfill_cursor`. The `messages_fts`
  search index is emptied with `messages` by its delete trigger. **`--all`** also clears `outbound_queue` and
  `sent_messages`; without `sent_messages`, messages sent earlier can no longer be edited or revoked.
- **Never cleared:** `poll_keys` (needed to read votes on existing polls), `device`, `identities`, `sessions`, `prekeys`,
  `signed_prekeys`, `sender_keys`, `sender_key_devices`, `base_keys`, `device_registry`, `app_state_keys`,
  `app_state_versions`, `app_state_mutation_macs`, `lid_pn_mapping`, `tc_tokens`, `metadata`. They hold the pairing, the
  encryption state, the app-state sync, the LID map, the DM delivery tokens and the device name.
- **It refuses while the daemon runs:** the instance lock is held, or the `whatsrust-<profile>` tmux session exists. If
  `lsof` is missing the lock state is `unknown` and it refuses unless you pass `--yes`.
- **It shows the plan first:** the DB path, the row count of every table it will clear, and the exact SQL (below).
  `--dry-run` stops there, exit 0.
- **It asks twice** (exit 1 on "no"). (1) `y/N`, skipped by `--yes`. (2) "Are you sure?": you type the profile name;
  anything else aborts. Only the environment variable `RUN_PROFILE_SKIP_CLEAR_DB_VERIFICATION=true` (exactly `true`)
  skips the second question; `--yes` does not. Without a terminal, a question that is not skipped is refused, so a script
  needs both: `RUN_PROFILE_SKIP_CLEAR_DB_VERIFICATION=true scripts/run-profile.sh clear-data dev --yes`.
- **Backup, then delete:** `whatsapp.db.pre-clear-<UTC YYYYmmdd-HHMMSS>.bak` (mode 600) is written next to the
  `whatsapp.db.pre-migration-*.bak` files, not into `whatsapp.db.backups/` (the daemon rotates that folder). If the backup
  fails, nothing is deleted. The deletes run in one transaction; an error rolls them back.
- **The SQL** (default; `--all` adds `DELETE FROM outbound_queue;` and `DELETE FROM sent_messages;` before `COMMIT;`):
  ```sql
  BEGIN;
  DELETE FROM embeddings;
  DELETE FROM media_refs;
  DELETE FROM messages;
  DELETE FROM backfill_jobs;
  DELETE FROM backfill_cursor;
  COMMIT;
  PRAGMA wal_checkpoint(TRUNCATE);
  VACUUM;
  ```
- **Side effect of looking:** like the daemon, `sqlite3` opens the DB in WAL mode, which can leave empty `whatsapp.db-wal` /
  `whatsapp.db-shm` files behind, even for `--dry-run`. `whatsapp.db` itself is not changed by `--dry-run`.

**Demo playbook.** Four short flows. (a) to (c) use a throwaway profile named `demo`; (d) needs a paired profile, here
`dev`. Paths are shown as `<repo>/...`; the port pair is the next free one (`9270/9271` when `demo` is your first
profile). The daemon's own lines are summarised as `...`; the `run-profile:` lines are the launcher's.

*(a) Foreground, in this terminal:*

1. Create it:
   ```bash
   scripts/run-profile.sh new demo
   ```
   ```
   created profile 'demo' in <repo>/runs/demo
     REST port 9270, MCP port 9271 (off: switch it on with: scripts/run-profile.sh mcp-on demo), device name whatsrust-demo
   next: scripts/run-profile.sh daemon demo
   ```
2. Start it (the default is this terminal; no tmux involved):
   ```bash
   scripts/run-profile.sh daemon demo
   ```
   ```
   run-profile: building (debug)...
   ...
   run-profile: profile demo, state in <repo>/runs/demo
   run-profile: log: <repo>/runs/demo/logs/20261020-083913.log
   ...the daemon's startup output (on a new profile a pairing QR code; ignore it, nothing is linked until you scan it)...
   ```
3. Press **Ctrl+C**. The daemon shuts down gracefully and the launcher reports:
   ```
   ... SIGINT received
   ... shutting down...
   run-profile: daemon exited with code 0 (log: <repo>/runs/demo/logs/20261020-083913.log)
   ```
4. Read the log, which holds the same output without colour codes (mode 600, so keep it private):
   ```bash
   ls -l runs/demo/logs/
   less runs/demo/logs/20261020-083913.log
   ```

*(b) In tmux (opt-in), detach and come back:*

1. Start it in a session (`make run-profile P=demo USE_TMUX=1` does the same):
   ```bash
   scripts/run-profile.sh daemon demo --tmux
   ```
   ```
   run-profile: building (debug)...
   ...
   run-profile: profile demo, state in <repo>/runs/demo
   run-profile: tmux session: whatsrust-demo
   run-profile: log: <repo>/runs/demo/logs/20261020-090012.log
   ```
   Then your terminal shows the session with the daemon's output. (`--detach` would stop here and print
   `run-profile: attach with: scripts/run-profile.sh attach demo   (detach again with Ctrl-b d)` instead of attaching.)
2. Press **Ctrl-b**, release, then **d**. You are back in your shell and the daemon keeps running:
   ```
   [detached (from session whatsrust-demo)]
   ```
3. See it running:
   ```bash
   scripts/run-profile.sh list --tmux
   ```
   ```
   SESSION                  PROFILE          REST    MCP          STARTED             ATTACHED
   whatsrust-demo           demo             9270    off (9271)   2026-10-20 12:00:12 no
   ```
4. Go back to it: `scripts/run-profile.sh attach demo` (the same session, output as you left it; `list --tmux` from
   another terminal shows `ATTACHED` `yes` while you are in it).
5. Press **Ctrl+C** inside the session. The daemon stops gracefully and the pane stays open with the exit note:
   ```
   ... SIGINT received
   ... shutting down...

   run-profile: daemon ended: exit code 0
   run-profile: log: <repo>/runs/demo/logs/20261020-090012.log
   run-profile: You are in tmux session whatsrust-demo: press Enter to close it, or Ctrl-b d to detach.
   ```
6. Press **Enter**. The session closes and you are back in your shell (`[exited]`). `scripts/run-profile.sh list --tmux`
   now prints `no run-profile daemons in tmux (scripts/run-profile.sh list shows every profile)`.

*(c) Enable the HTTP MCP listener and connect Claude Code* (needs the `demo` profile from (a), and the `claude` CLI):

1. Switch the listener on:
   ```bash
   scripts/run-profile.sh mcp-on demo
   ```
   ```
   MCP listener enabled on port 9271 (token: generated)
   ```
   (`new demo --mcp` does this while creating the profile. If `demo`'s daemon is already running you also get
   `  the daemon of 'demo' is running: restart the daemon to apply`: stop it with Ctrl+C and start it again in step 2.)
2. Start the daemon (this terminal stays busy: use a second terminal for the rest, or add `--detach` to run it in tmux in the background):
   ```bash
   scripts/run-profile.sh daemon demo
   ```
   Among the startup output (abbreviated here) the daemon logs that the listener is up:
   ```
   ... INFO whatsrust::mcp::infra: mcp listener started bind=127.0.0.1 port=9271 read_only=false
   ```
   `scripts/run-profile.sh list` now shows `9271` in the MCP column. If the daemon exits instead, it prints the exact fix
   (§5a, "Startup refusals").
3. In a second terminal, ask for the client setup:
   ```bash
   scripts/run-profile.sh mcp-client demo
   ```
   ```
   MCP listener of profile demo: http://127.0.0.1:9271/mcp

   Claude Code config, as a project .mcp.json (the token is not in it, only ${WHATSRUST_MCP_TOKEN}):
   {"mcpServers":{"whatsrust-demo":{"type":"http","url":"http://127.0.0.1:9271/mcp","headers":{"Authorization":"Bearer ${WHATSRUST_MCP_TOKEN}"},"timeout":120000}}}

   or add it with the claude CLI (add --scope user to use it in all projects):
   claude mcp add-json whatsrust-demo '{"type":"http","url":"http://127.0.0.1:9271/mcp","headers":{"Authorization":"Bearer ${WHATSRUST_MCP_TOKEN}"},"timeout":120000}'

   Export the token in the shell you start Claude Code from (this does not print it):
   export WHATSRUST_MCP_TOKEN="$(sed -n 's/^WHATSRUST_MCP_TOKEN=//p' "<repo>/runs/demo/.env")"

   Notes:
     ... (one variable per profile, the Claude Code start-up rule, and the warning about `--header`)
   ```
4. Export the token in the shell you will start Claude Code from: copy the `export ...` line from the output and run it. Check
   it worked without printing the token:
   ```bash
   echo ${#WHATSRUST_MCP_TOKEN}
   ```
   ```
   44
   ```
5. Add the server: either put the `{"mcpServers":...}` snippet in a `.mcp.json` in your project, or run the printed
   `claude mcp add-json whatsrust-demo '...'` line. Both store only `${WHATSRUST_MCP_TOKEN}`, never the token.
6. Start Claude Code **from that same shell** (`claude`) and type `/mcp`. It lists `whatsrust-demo` as connected, with the 34
   tools of §5a (the exact layout depends on your Claude Code version). If it shows the server as failed, check that the
   variable is exported in the shell that started Claude Code (step 4), that the daemon was restarted after `mcp-on` (step 2)
   and was not started from a shell with another `WHATSRUST_MCP_TOKEN` exported; `scripts/run-profile.sh cli demo status`
   reports an `mcp_http` object (instead of `null`) when the listener is up.
7. Undo it:
   ```bash
   scripts/run-profile.sh mcp-off demo
   ```
   ```
   MCP listener switched off for demo (the token line stays in the .env, so mcp-on keeps the same token)
   ```
   (plus the restart note when the daemon is running; the change applies at its next start). `scripts/run-profile.sh list`
   then shows `off (9271)`. A later `mcp-on demo` prints `MCP listener enabled on port 9271 (token: kept)`, so the Claude
   Code config keeps working. To remove the server from Claude Code: `claude mcp remove whatsrust-demo`.

*(d) Empty a paired profile's messages and retest history fetches* (needs a paired profile, here `dev`, and `sqlite3`;
the row counts below are an example):

1. Stop the daemon: **Ctrl+C** in its terminal. For a tmux daemon, `scripts/run-profile.sh attach dev`, Ctrl+C, then Enter
   to close the session. `clear-data` refuses while the instance lock is held or the session exists.
2. Look first. `--dry-run` prints the plan and the exact SQL and changes nothing:
   ```bash
   scripts/run-profile.sh clear-data dev --dry-run
   ```
   ```
   clear-data plan for profile 'dev'
   database: <repo>/runs/dev/whatsapp.db

   rows to delete:
     embeddings       3980
     media_refs       412
     messages         4210
     backfill_jobs    6
     backfill_cursor  14

   SQL that will run (one sqlite3 -bail call; the deletes are one transaction):
   BEGIN;
   DELETE FROM embeddings;
   DELETE FROM media_refs;
   DELETE FROM messages;
   DELETE FROM backfill_jobs;
   DELETE FROM backfill_cursor;
   COMMIT;
   PRAGMA wal_checkpoint(TRUNCATE);
   VACUUM;

   not touched: every other table (the pairing, the encryption state, the app-state sync, the LID map,
   the DM delivery tokens, the device name, and poll_keys, which reads votes on existing polls)
   kept too: outbound_queue and sent_messages (--all clears them; without sent_messages, messages
   sent earlier can no longer be edited or revoked)

   dry run: nothing was changed
   ```
3. Do it. After the plan it asks `y/N`, then makes you type the profile name, writes the backup, runs the SQL and reports:
   ```bash
   scripts/run-profile.sh clear-data dev
   ```
   ```
   ...the same plan...
   Delete these rows? [y/N] y
   run-profile: Second verification: this deletes the message history of 'dev' from its database.
   run-profile: A backup is written first; the rows only come back by restoring it.
   Are you sure? Type the profile name (dev) to continue: dev
   run-profile: backup written: <repo>/runs/dev/whatsapp.db.pre-clear-20261021-210548.bak
   cleared profile 'dev'. Rows now:
     embeddings       0
     media_refs       0
     messages         0
     backfill_jobs    0
     backfill_cursor  0

   backup: <repo>/runs/dev/whatsapp.db.pre-clear-20261021-210548.bak
   restore (stop the daemon of 'dev' first):
     cp <repo>/runs/dev/whatsapp.db.pre-clear-20261021-210548.bak <repo>/runs/dev/whatsapp.db
     rm -f <repo>/runs/dev/whatsapp.db-wal <repo>/runs/dev/whatsapp.db-shm
   ```
4. Start the daemon again: `scripts/run-profile.sh daemon dev`.
5. Expected result:
   - The pairing is kept: it connects with the existing credentials and prints **no QR code**; the phone still lists the
     same linked device.
   - `scripts/run-profile.sh cli dev history 15551234567 20` returns 0 messages (the local history is empty).
   - `scripts/run-profile.sh cli dev fetch-history 15551234567 all` fails with HTTP 409 and `"code": "no_anchor"` and queues
     no job: the cursor and the stored messages that served as the starting point are gone. It works again once you
     archive and then unarchive the chat on the phone, or a message arrives in that chat (or you send one from the
     phone), which becomes the new anchor (§4).
6. Restore, if you want the old data back: stop the daemon, then run the two commands the report printed (copy the backup
   over `whatsapp.db`, delete `whatsapp.db-wal` and `whatsapp.db-shm`) and start the daemon. The backup is a normal
   file in `runs/dev/`: delete it when you no longer need it (it holds all messages and the session keys; keep it private).

To script it, set `RUN_PROFILE_SKIP_CLEAR_DB_VERIFICATION=true` and pass `--yes` (there is no terminal to answer the two
questions): `RUN_PROFILE_SKIP_CLEAR_DB_VERIFICATION=true scripts/run-profile.sh clear-data dev --yes`.

*Clean up.* A throwaway profile that was never paired holds nothing worth keeping: `rm -rf runs/demo`. Never do that
for a paired profile, because its `whatsapp.db` is a linked WhatsApp device; move it aside instead.

**Caveat:** never run two copies of the same paired DB at once. Each profile is its own linked device, so
pair a new one (scan its QR code) or move (not copy) an existing DB into the profile folder. Pair one profile
at a time: the QR image and HTML files go to the system temp directory under fixed names.

**Move today's root state into `runs/main/` (one-time checklist for the owner):**
1. Stop the running daemon (Ctrl+C, or `quit` in the REPL) so nothing holds `whatsapp.db`.
2. Create the folder: `mkdir -p runs/main/logs`
3. Move (not copy) the DB and its companions; only the ones that exist are moved:
   ```bash
   for f in whatsapp.db whatsapp.db-wal whatsapp.db-shm whatsapp.db.backups whatsapp.db.pre-migration-*.bak; do
     [ -e "$f" ] && mv "$f" runs/main/
   done
   ```
4. Move the config: `mv .env runs/main/.env` (it keeps its current ports, so `main` behaves as before).
5. Move the logs: `mv ./*.log runs/main/logs/`
6. Start it from the profile: `scripts/run-profile.sh daemon main`

The old `whatsapp.db.instance.lock` can stay in the root (it is git-ignored) or be deleted; the daemon creates
a fresh one in `runs/main/`.

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
- History sync runs again, as on any first pairing. The phone sends a package
  with the messages of some of its chats (a few chats and recent messages;
  how many depends on the phone), and whatsrust stores them (`source` =
  `bootstrap`, ADR 0048). Those chats now have a message to start from, so you
  can run `fetch-history` on them at once (§4). A message already in the
  database is not stored twice: each WhatsApp message ID is kept once. Media is
  not downloaded for these messages: the stored text shows its kind and size
  (for example `[image 12.1KB]`), and the file's reference is kept in
  `media_refs` for a later on-demand download (not built yet). To turn the
  storing off, set `WHATSRUST_STORE_PAIRING_HISTORY=false` before pairing.
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

To drive a run profile's daemon (§3, "Run profiles"), use `scripts/run-profile.sh cli <profile> <subcommand> ...`
instead of `whatsrust <subcommand> ...`; it exports that profile's `WHATSRUST_PORT`, `WHATSRUST_BIND` and
`WHATSRUST_API_TOKEN` first.

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

**`fetch-history` needs a message to start from.** WhatsApp pages history
backwards from a known message, so the chat must have at least one stored
message. Without one, the call fails with HTTP 409 and `"code": "no_anchor"`
(the CLI prints that body and exits 1; the MCP tool returns `isError: true`), and
no job is queued. To fix it, **archive and then unarchive the chat on the phone**
(the other side sees nothing; the phone's sync tells the daemon the chat's last
message, which becomes the starting point, so the daemon must be running and
connected when you do it), or receive a message in the chat, or send one **from the phone**, then
retry. Messages sent through whatsrust are not stored, so they do not count.

After a **pairing**, the chats in the history package the phone sends already
have stored messages (§3a), so they can be fetched without this step. Only a
fresh pairing delivers the package: a profile that is already paired gets it
only if you re-pair (§3a). Chats the phone left out of the package still need
the archive and unarchive step. The package is stored unless
`WHATSRUST_STORE_PAIRING_HISTORY=false` (ADR 0048).

If a fetch fails with "the phone did not answer within 180 s; it may be asleep or
offline", the phone did not answer in time: a sleeping phone can take minutes to
receive the request. Open WhatsApp on the phone, then retry. If the answer
arrives later anyway, whatsrust stores its messages (the job stays failed), and
the retry continues from them.

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

## 5a. MCP over HTTP (opt-in listener)

**Goal:** serve MCP tools over HTTP instead of stdio, enabling clients like
Claude Code, MCP Inspector, and official SDK clients (tested: Python SDK
1.30.0, Rust rmcp 3.5.1) to connect over HTTP. The listener is off by
default; enable it with its own port, token, and bind address. TLS is added
through a reverse proxy.

**Enable:** set `WHATSRUST_MCP_PORT` (e.g. 7271; unset or 0 = off),
`WHATSRUST_MCP_TOKEN` (required; at least 22 characters; generate with
`openssl rand -base64 32`; must differ from `WHATSRUST_API_TOKEN`), and
`WHATSRUST_MCP_BIND` (default 127.0.0.1). To bind to a non-loopback address
(e.g. 0.0.0.0), set `WHATSRUST_MCP_ALLOW_REMOTE=1` (the token and messages
travel in cleartext unless a TLS proxy is in front; the daemon logs a
warning). Read-only mode: `WHATSRUST_MCP_READ_ONLY=1` offers only the 6 read
tools; write tools are refused with JSON-RPC `-32600`.

**Run profiles:** `scripts/run-profile.sh mcp-on <profile>` does this enable step for a profile's `.env` (port, a token kept,
taken from your environment or generated), `mcp-client <profile>` prints the Claude Code config; see §3, "Run profiles".

**Startup refusals (daemon exits):** unparseable port, remote bind without
`WHATSRUST_MCP_ALLOW_REMOTE=1`, missing or short token (under 22 characters),
token equal to `WHATSRUST_API_TOKEN`, or MCP port equal to the REST port. The
daemon prints the exact fix before exiting.

**.env loading:** the daemon reads `.env` only in daemon mode; the CLI and
`whatsrust mcp` (stdio) read tokens from the process environment.

**Endpoint:** `POST /mcp` only; other paths get 404, other methods 405 + `Allow: POST`. Stateless (no `Mcp-Session-Id` ever issued). Dual-era: modern (2026-07-28: `server/discover`, request-metadata headers `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, plus `_meta` version key) and legacy (2025-11-25 / 2025-06-18: `initialize` handshake). 34 tools, same validation as stdio.

**Claude Code config:** the official Claude Code MCP docs (https://code.claude.com/docs/en/mcp.md) support `${VAR}` / `${VAR:-default}` expansion in `url` and `headers`, and a per-server `timeout` (ms) that overrides `MCP_TOOL_TIMEOUT`. Recommended config (keeps the token out of argv and the config file):
```json
{ "mcpServers": { "whatsrust": { "type": "http", "url": "http://127.0.0.1:7271/mcp",
    "headers": { "Authorization": "Bearer ${WHATSRUST_MCP_TOKEN}" }, "timeout": 120000 } } }
```
or `claude mcp add-json whatsrust '{"type":"http","url":"http://127.0.0.1:7271/mcp","headers":{"Authorization":"Bearer ${WHATSRUST_MCP_TOKEN}"},"timeout":120000}'` (add `--scope user` for all projects).

**Warning:** `claude mcp add --header "Authorization: Bearer $TOKEN"` with double quotes puts the token in argv and stores it in plain text in `~/.claude.json`.

**Timeout:** why 120000 ms: the listener sends nothing until the tool finishes; the tool budget is 40 s from the moment the request body is complete, and a body may take up to min(60 s, 20 s + 4 s per MiB) to arrive, so a slow 10 MiB upload can approach 100 s. Any generic MCP HTTP client should allow at least 50 s plus its upload time.

**curl test (zsh/bash safe, token not in argv):** write the header to a mode-600 file and pass it through `@`:
```bash
umask 077
printf 'Authorization: Bearer %s\n' "$WHATSRUST_MCP_TOKEN" > ~/.whatsrust-mcp.hdr

# Legacy initialize handshake
curl -s -X POST http://127.0.0.1:7271/mcp \
  -H 'Content-Type: application/json' \
  -H @"$HOME/.whatsrust-mcp.hdr" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}'

# Legacy tools/list (note: header must be sent for legacy after initialize)
curl -s -X POST http://127.0.0.1:7271/mcp \
  -H 'Content-Type: application/json' \
  -H 'MCP-Protocol-Version: 2025-06-18' \
  -H @"$HOME/.whatsrust-mcp.hdr" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'

# Modern server/discover (no initialize): MCP-Protocol-Version and Mcp-Method headers plus the _meta
# version key; a modern tools/call also needs Mcp-Name: <tool name>
curl -s -X POST http://127.0.0.1:7271/mcp \
  -H 'Content-Type: application/json' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: server/discover' \
  -H @"$HOME/.whatsrust-mcp.hdr" \
  -d '{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}'
```

**Reverse proxy:** use `deploy/proxy/nginx-mcp.conf` or `deploy/proxy/Caddyfile` as production templates. They add TLS, forward only `/mcp` (other paths 404), rewrite Host to 127.0.0.1 (so the loopback gate passes without `WHATSRUST_ALLOWED_HOSTS`), and cap the body at 10 MiB. nginx needs version 1.25.1 or newer for `http2 on;`. Caddy's `MB` is 10^6 bytes, so the file uses `10MiB`. nginx always buffers the request body and sends a Content-Length; Caddy forwards a chunked HTTP/1.1 upload as chunked, which the listener answers with 411. ngrok: not used for testing (it would expose the listener publicly and tests ngrok's edge); if someone uses it anyway, it forwards the public Host unless run with `--host-header=rewrite`.

**Proxy playbook:** `scripts/mcp-proxy-smoke.sh` runs real `nginx:alpine` and `caddy:2-alpine` in Docker in front of a running listener on 127.0.0.1:18443 / 18444 with self-signed certs in a temp dir; needs docker, openssl, curl; reads `WHATSRUST_MCP_TOKEN` and `MCP_UPSTREAM_PORT` (default 7271); cleans up on exit. Run it against your own daemon with the listener on, or as `cargo test proxy_smoke_docker -- --ignored --nocapture` (test listener, no WhatsApp contact). Checks per proxy (8): initialize, tools/list 34, tools/call status, modern server/discover with Mcp-* headers, /api/status → 404, 10.4 MB body passes, >10 MiB → 413, no token → 401. Verified 2026-10-19: 16/16 PASS (nginx 1.31.7, Caddy 2.11.7, Docker Desktop 29.8.2).

**Logging:** every request gets one log line in one of four `whatsrust::mcp::*` targets, so `RUST_LOG` can filter each. Targets: `whatsrust::mcp::call` (tools/call; info ok, warn otherwise), `whatsrust::mcp::infra` (listener start, initialize/server/discover with peer, tools/list, ping, notifications; accept/close at debug; 503 at warn), `whatsrust::mcp::reject` (pre-dispatch refusals: routing, framing, 415 content type, parse/batch/envelope, -32022, -32020; debug), `whatsrust::mcp::auth` (401 token, 403 Origin/Host gate, with peer; warn). One line per request (message "mcp request" / "mcp request rejected"); never arguments, bodies, tokens, the JSON-RPC id or raw header values; unknown method/tool/version strings are masked. Example lines:
```
INFO whatsrust::mcp::call: mcp request method="tools/call" tool="whatsrust_status" era="legacy" outcome="ok" status=200 duration_ms=0
WARN whatsrust::mcp::auth: mcp request rejected outcome="rejected" status=401 reason="token" peer=127.0.0.1:51652 duration_ms=1
```

**RUST_LOG filter examples:** `RUST_LOG=whatsrust=info,whatsrust::mcp::infra=warn` (quiet list/ping polling), `RUST_LOG=whatsrust=info,whatsrust::mcp::reject=debug` (see refusals).

**Note:** modern clients may skip `server/discover`, so not every client gets an "initialization" line with its IP.

**fail2ban:** filter on `whatsrust::mcp::auth` lines (`status=401 ... peer=<HOST>:<port>`). The daemon logs to stdout and colours its output unless `NO_COLOR` is set to a non-empty value (tracing-subscriber 0.3.23), so write a plain log file for fail2ban, e.g. `NO_COLOR=1 whatsrust >> /var/log/whatsrust.log 2>&1` (or match the escape codes). Two files:
```ini
# /etc/fail2ban/filter.d/whatsrust-mcp.conf
[Definition]
failregex = whatsrust::mcp::auth: mcp request rejected .*status=401 .*peer=<HOST>:\d+
```
```ini
# /etc/fail2ban/jail.d/whatsrust-mcp.conf
[whatsrust-mcp]
enabled  = true
filter   = whatsrust-mcp
port     = 7271
logpath  = /var/log/whatsrust.log
maxretry = 5
bantime  = 600
```
Behind a proxy the peer is the proxy's address, so fail2ban belongs on the proxy's access log in that setup.

**Metrics:** `/api/status` (and `/health`, `/`) now has `"mcp_http"`: `null` when the listener is off, otherwise an object with: `requests_total` (requests whose head was read, whatever became of them), `tools_called_total` (tools/call requests that ran a REST call), `errors_total` (dispatched requests that ended in a JSON-RPC error or an isError result), `rejects_total` (requests refused before dispatch: routing, token, gate, framing, unreadable body, bad envelope/id/era/version/headers), `timeouts_total` (tool calls that outlasted the 40 s budget), `unavailable_503_total` (responses with status 503: no connection slot or no lane permit), `tasks_outlived_response` (gauge: tool tasks still running after their timeout response was sent), `in_flight` (read/write/large_body: permits in use), `available_permits` (read/write/large_body: permits free). Visible through `whatsrust status` and the `whatsrust_status` MCP tool. This is a deliberate REST change (one golden fixture updated).

**Verification:** see the test counts and ignored tests in §6.

---

## 5b. Embedder sidecar (optional semantic search)

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
make test        # cargo test — default suite (620 lib + 625 bin tests, 3 ignored per target, verified 2026-10-19)
make test-all     # cargo test --features fake-embedder — also runs the
                   # fake-embedder sidecar round-trip tests (629 lib + 634 bin;
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
- `make test` — `test result: ok. 620 passed; 0 failed; 3 ignored ...` (lib) and
  `test result: ok. 625 passed; 0 failed; 3 ignored ...` (bin), exit 0 (verified 2026-10-19).
- `make test-all` — `test result: ok. 629 passed; 0 failed; 3 ignored ...` (lib, includes
  the 9 extra fake-embedder-gated tests per target) and `test result: ok. 634 passed; 0 failed; 3 ignored ...` (bin), exit 0 (verified 2026-10-19).

`CONTRIBUTING.md` asks contributors to run `cargo test` and `cargo clippy`
before submitting a PR — `make test` and `make lint` are equivalent to that,
plus `make test-all` for full coverage of the embedder sidecar path.

**Ignored tests (MCP SDK smokes + proxy smoke):** The test suite includes three ignored tests per target. `sdk_smoke_python` and `sdk_smoke_rmcp` run official MCP SDKs (Python 1.30.0, Rust rmcp 3.5.1) against a test MCP listener. `proxy_smoke_docker` runs `scripts/mcp-proxy-smoke.sh`: real nginx and Caddy in Docker in front of a test listener. None contact WhatsApp; all pass their test token through `WHATSRUST_MCP_TOKEN`.
- `sdk_smoke_python`: The official Python MCP SDK (mcp<2, Streamable HTTP client). Run with `cargo test sdk_smoke_python -- --ignored --nocapture`. Needs `uv` (Python package manager) and downloads `mcp<2` from PyPI. Verifies the legacy handshake and tools/call.
- `sdk_smoke_rmcp`: The official Rust MCP SDK `rmcp` =3.5.1 (client + transport-streamable-http-client-reqwest) from a standalone crate (`tools/mcp-smoke-rs/`). Run with `cargo test sdk_smoke_rmcp -- --ignored --nocapture`. The first build downloads crates. Supports three modes: `--mode modern` (server/discover), `--mode auto` (probe server/discover, fallback to initialize), `--mode legacy` (initialize). Verifies dual-era support and modern request handling.

To run the scripts against your own daemon, set `WHATSRUST_MCP_TOKEN` yourself; never pass the token as a command-line argument (it shows in `ps`). These tests verify that the MCP listener works with the official SDKs, not just hand-rolled requests.

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
