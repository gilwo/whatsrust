#!/usr/bin/env bash
# Dev run profiles (F9): each debug/live run keeps its state (whatsapp.db, backups, instance lock,
# logs, .env) in runs/<profile>/ instead of the project root. No Rust change: the daemon simply
# starts with the profile folder as its working directory. Run with --help for the commands.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNS="$ROOT/runs"
DIR="" # set by require_profile
BIN="" # set by find_bin

usage() {
  cat <<'EOF'
Usage: scripts/run-profile.sh <command> [args]

Dev run profiles: each run keeps its state (whatsapp.db, backups, instance lock, logs, .env) in
runs/<profile>/ instead of the project root. Profile names: letters, digits, '.', '_', '-'.

Commands:
  new <profile> [--semantic]    create runs/<profile>/ (logs/ + a .env from .env.example) with the
                                next free port pair; --semantic enables the embedder sidecar
  list                          show profiles: REST port, MCP port, DB present, instance lock
  daemon <profile> [--release]  build once, then run the daemon inside runs/<profile>/; output goes
                                to the terminal (colours) and to logs/<UTC time>.log (no colours)
  cli <profile> <args...>       run `whatsrust <args...>` against that profile's daemon
  mcp <profile>                 run `whatsrust mcp` (stdio) against that profile's daemon
  env <profile>                 show the environment cli/mcp export (the token is never shown)

Ports: the first profile gets REST 9270 / MCP 9271, then 9280/9281, 9290/9291, ... The MCP
listener is off until you enable it in the profile's .env (WHATSRUST_MCP_PORT + a token you generate).

Notes:
  - cli and mcp read only WHATSRUST_PORT, WHATSRUST_BIND and WHATSRUST_API_TOKEN from the profile's
    .env (nothing else is sourced) and use the newest built target/{release,debug}/whatsrust.
  - cli --rollback / --migrate run inside runs/<profile>/ because they act on its whatsapp.db.
  - list: LOCK is "HELD" when a process has runs/<profile>/whatsapp.db.instance.lock open (checked
    with lsof; "unknown" if lsof is missing). The lock file itself stays behind after a stop.
  - Never run two copies of the same paired DB at once; each profile is its own linked device.
EOF
}

die() {
  printf 'run-profile: %s\n' "$*" >&2
  exit 1
}

usage_err() {
  printf 'run-profile: %s\nTry: scripts/run-profile.sh --help\n' "$*" >&2
  exit 2
}

note() {
  printf 'run-profile: %s\n' "$*" >&2
}

# Profile names become folder names under runs/: no slashes, no leading '.' or '-'.
check_name() {
  case $1 in
    '' | [!A-Za-z0-9]* | *[!A-Za-z0-9._-]*)
      usage_err "invalid profile name '$1' (letters, digits, '.', '_', '-'; start with a letter or digit)"
      ;;
  esac
}

# require_profile <name>: validate the name and set DIR to the existing runs/<name>.
require_profile() {
  check_name "$1"
  DIR="$RUNS/$1"
  [ -d "$DIR" ] || die "no such profile: $1 (create it with: scripts/run-profile.sh new $1)"
}

# env_get <env-file> <KEY>: value of the first active "KEY=value" line (optional "export ",
# surrounding quotes or a trailing " # comment" removed); empty when absent. Only the requested key
# is parsed and the file is never sourced. The first occurrence wins, like dotenvy in the daemon.
env_get() {
  local file=$1 key=$2 v
  [ -f "$file" ] || return 0
  v=$(sed -n -E "/^[[:space:]]*(export[[:space:]]+)?${key}[[:space:]]*=/{s/^[^=]*=[[:space:]]*//p;q;}" "$file") || true
  case $v in
    \"*)
      v=${v#\"}
      v=${v%%\"*}
      ;;
    \'*)
      v=${v#\'}
      v=${v%%\'*}
      ;;
    *)
      v=${v%%[[:space:]]#*}
      v=${v%"${v##*[![:space:]]}"}
      ;;
  esac
  printf '%s\n' "$v"
}

# commented_port <env-file> <KEY>: digits of the first commented-out "# KEY=<digits>" line.
commented_port() {
  sed -n -E "/^#[[:space:]]*${2}[[:space:]]*=[[:space:]]*[0-9]/{s/^[^=]*=[[:space:]]*([0-9]+).*/\1/p;q;}" "$1"
}

# ports_in_use: every WHATSRUST_PORT / WHATSRUST_MCP_PORT number in any profile's .env, including
# commented-out lines, so the MCP port reserved by `new` stays reserved after the owner enables it.
ports_in_use() {
  local f
  for f in "$RUNS"/*/.env; do
    [ -f "$f" ] || continue
    sed -n -E 's/^[#[:space:]]*(export[[:space:]]+)?WHATSRUST_(MCP_)?PORT[[:space:]]*=[[:space:]]*([0-9]+).*/\3/p' "$f"
  done
}

# in_list <word> <space-separated words>
in_list() {
  case " $2 " in *" $1 "*) return 0 ;; esac
  return 1
}

# next_port_pair: lowest base 9270 + 10n whose base and base+1 are used by no profile.
next_port_pair() {
  local used p=9270
  used=$(ports_in_use | tr '\n' ' ')
  while in_list "$p" "$used" || in_list "$((p + 1))" "$used"; do
    p=$((p + 10))
  done
  printf '%s\n' "$p"
}

cmd_new() {
  local name="" semantic=0 arg
  for arg in "$@"; do
    case $arg in
      --semantic) semantic=1 ;;
      -*) usage_err "new: unknown option: $arg" ;;
      *)
        [ -z "$name" ] || usage_err "new: unexpected argument: $arg"
        name=$arg
        ;;
    esac
  done
  [ -n "$name" ] || usage_err "new: missing <profile>"
  check_name "$name"
  [ -f "$ROOT/.env.example" ] || die "missing $ROOT/.env.example"
  local dir="$RUNS/$name"
  [ ! -e "$dir" ] || die "profile '$name' already exists: $dir"

  local rest_port mcp_port embedder="$ROOT/embedder-rs/target/release/whatsrust-embedder"
  rest_port=$(next_port_pair)
  mcp_port=$((rest_port + 1))

  umask 077 # folder 700, .env 600
  mkdir -p "$RUNS"
  mkdir "$dir" "$dir/logs"
  {
    cat <<EOF
# ---------------------------------------------------------------------------
# Run profile "$name": generated by scripts/run-profile.sh new. Active settings come first (the
# daemon keeps the first value it sees for a key); the rest is .env.example, all commented out.
# ---------------------------------------------------------------------------
WHATSRUST_PORT=$rest_port
WHATSRUST_DEVICE_NAME=whatsrust-$name
RUST_LOG=whatsrust=info,whatsapp_rust=info
EOF
    if [ "$semantic" -eq 1 ]; then
      printf 'WHATSRUST_EMBEDDER_CMD="%s"\n' "$embedder"
      printf '%s\n' 'WHATSRUST_EMBEDDER_ARGS="--model bge-m3"'
    fi
    cat <<EOF

# MCP listener over HTTP is OFF. Its port is reserved for this profile. To enable it, generate a
# token yourself with: openssl rand -base64 32   (it must differ from WHATSRUST_API_TOKEN), then
# uncomment both lines below and paste the token.
# WHATSRUST_MCP_PORT=$mcp_port
# WHATSRUST_MCP_TOKEN=<paste the generated token here>

# ---------------------------------------------------------------------------
# .env.example (documentation, all commented out)
# ---------------------------------------------------------------------------
EOF
    cat "$ROOT/.env.example"
  } >"$dir/.env"

  echo "created profile '$name' in $dir"
  echo "  REST port $rest_port, MCP port $mcp_port (off: enable it in $dir/.env), device name whatsrust-$name"
  if [ "$semantic" -eq 1 ] && [ ! -x "$embedder" ]; then
    echo "  embedder sidecar not built yet: run 'make embedder-build' before starting the daemon"
  fi
  echo "next: scripts/run-profile.sh daemon $name"
}

cmd_list() {
  [ $# -eq 0 ] || usage_err "list takes no arguments"
  local d name rest mcp db lock found=0
  for d in "$RUNS"/*/; do
    [ -d "$d" ] || continue
    d=${d%/}
    name=${d##*/}
    rest=$(env_get "$d/.env" WHATSRUST_PORT)
    mcp=$(env_get "$d/.env" WHATSRUST_MCP_PORT)
    if [ -z "$mcp" ]; then
      mcp=$(commented_port "$d/.env" WHATSRUST_MCP_PORT 2>/dev/null || true)
      if [ -n "$mcp" ]; then mcp="off ($mcp)"; else mcp="off"; fi
    fi
    if [ -e "$d/whatsapp.db" ]; then db=yes; else db=no; fi
    if [ ! -e "$d/whatsapp.db.instance.lock" ]; then
      lock=free
    elif ! command -v lsof >/dev/null 2>&1; then
      lock=unknown
    elif lsof -t -- "$d/whatsapp.db.instance.lock" >/dev/null 2>&1; then
      lock=HELD
    else
      lock=free
    fi
    if [ "$found" -eq 0 ]; then
      printf '%-20s %-7s %-12s %-4s %s\n' PROFILE REST MCP DB LOCK
      found=1
    fi
    printf '%-20s %-7s %-12s %-4s %s\n' "$name" "${rest:-default}" "$mcp" "$db" "$lock"
  done
  if [ "$found" -eq 0 ]; then
    echo "no profiles in $RUNS (create one: scripts/run-profile.sh new <name>)"
  fi
}

# target_dir: cargo's target directory (CARGO_TARGET_DIR or <repo>/target), absolute.
target_dir() {
  local t=${CARGO_TARGET_DIR:-$ROOT/target}
  case $t in /*) ;; *) t="$ROOT/$t" ;; esac
  printf '%s\n' "$t"
}

# find_bin: set BIN to the newest built target/{release,debug}/whatsrust (no build is triggered).
find_bin() {
  local t rel dbg
  t=$(target_dir)
  rel="$t/release/whatsrust"
  dbg="$t/debug/whatsrust"
  if [ -x "$rel" ] && { [ ! -x "$dbg" ] || [ "$rel" -nt "$dbg" ]; }; then
    BIN=$rel
  elif [ -x "$dbg" ]; then
    BIN=$dbg
  else
    die "no built binary under $t (run: make build)"
  fi
}

# load_client_env: export the three variables the CLI and `whatsrust mcp` read, taken from the
# profile's .env (an unset key stays unset, so nothing leaks in from the calling shell).
load_client_env() {
  local key v
  unset WHATSRUST_PORT WHATSRUST_BIND WHATSRUST_API_TOKEN HEALTH_PORT
  for key in WHATSRUST_PORT WHATSRUST_BIND WHATSRUST_API_TOKEN; do
    v=$(env_get "$DIR/.env" "$key")
    if [ -n "$v" ]; then
      export "$key=$v"
    fi
  done
}

cmd_cli() {
  [ $# -ge 2 ] || usage_err "cli: usage: cli <profile> <whatsrust args...>"
  local name=$1
  shift
  require_profile "$name"
  find_bin
  load_client_env
  case $1 in
    --rollback | --migrate) cd "$DIR" ;;
  esac
  exec "$BIN" "$@"
}

cmd_mcp() {
  [ $# -eq 1 ] || usage_err "mcp: usage: mcp <profile>"
  require_profile "$1"
  find_bin
  load_client_env
  exec "$BIN" mcp
}

cmd_env() {
  [ $# -eq 1 ] || usage_err "env: usage: env <profile>"
  require_profile "$1"
  load_client_env
  echo "profile: $1 ($DIR)"
  echo "WHATSRUST_PORT=${WHATSRUST_PORT:-<unset> (CLI default 7270)}"
  echo "WHATSRUST_BIND=${WHATSRUST_BIND:-<unset> (CLI default 127.0.0.1)}"
  if [ -n "${WHATSRUST_API_TOKEN:-}" ]; then
    echo "WHATSRUST_API_TOKEN=<set>"
  else
    echo "WHATSRUST_API_TOKEN=<unset>"
  fi
}

# check_embedder: when the profile's .env enables the embedder sidecar, make sure it exists.
check_embedder() {
  local cmd ok=0
  cmd=$(env_get "$DIR/.env" WHATSRUST_EMBEDDER_CMD)
  [ -n "$cmd" ] || return 0
  case $cmd in
    */*) if (cd "$DIR" && [ -x "$cmd" ]); then ok=1; fi ;;
    *) if command -v "$cmd" >/dev/null 2>&1; then ok=1; fi ;;
  esac
  [ "$ok" -eq 1 ] || die "WHATSRUST_EMBEDDER_CMD is not an executable file: $cmd
  (the repo's sidecar is built with: make embedder-build)"
}

# warn_ambient: environment variables beat the profile's .env (dotenvy only fills unset ones), so
# name any that the calling shell exports. Names only, never values.
warn_ambient() {
  local names
  names=$(compgen -e | grep -E '^(WHATSRUST_|WHATSAPP_|HEALTH_PORT$|BACKUP_DIR$|RUST_LOG$)' | grep -v '^WHATSRUST_ENV_FILE$' | tr '\n' ' ' || true)
  if [ -n "$names" ]; then
    note "warning: exported in your shell, these override the profile's .env: $names"
  fi
}

cmd_daemon() {
  local name="" release=0 arg
  for arg in "$@"; do
    case $arg in
      --release) release=1 ;;
      -*) usage_err "daemon: unknown option: $arg" ;;
      *)
        [ -z "$name" ] || usage_err "daemon: unexpected argument: $arg"
        name=$arg
        ;;
    esac
  done
  [ -n "$name" ] || usage_err "daemon: missing <profile>"
  require_profile "$name"
  [ -f "$DIR/.env" ] || note "warning: $DIR/.env is missing; the daemon runs with built-in defaults"
  check_embedder
  warn_ambient

  local flavor=debug
  if [ "$release" -eq 1 ]; then flavor=release; fi
  note "building ($flavor)..."
  if [ "$release" -eq 1 ]; then
    (cd "$ROOT" && cargo build --release)
  else
    (cd "$ROOT" && cargo build)
  fi
  BIN="$(target_dir)/$flavor/whatsrust"
  [ -x "$BIN" ] || die "expected binary not found after build: $BIN"

  mkdir -p "$DIR/logs"
  local log
  log="$DIR/logs/$(date -u +%Y%m%d-%H%M%S).log"
  (umask 077 && : >>"$log")
  note "profile $name, state in $DIR"
  note "log: $log"

  cd "$DIR"
  export WHATSRUST_ENV_FILE="$DIR/.env"
  local esc=$'\033' rc
  # The script survives Ctrl+C (the daemon handles it and shuts down gracefully); tee and sed ignore
  # it so the daemon's shutdown output still reaches the terminal and the log. The pipeline keeps
  # stdin on the terminal for the REPL; PIPESTATUS[0] is the daemon's own exit code.
  trap ':' INT
  set +e
  "$BIN" 2>&1 | (
    trap '' INT
    exec tee >(sed -u "s/${esc}\[[0-9;]*[A-Za-z]//g" >>"$log")
  )
  rc=${PIPESTATUS[0]}
  set -e
  trap - INT
  note "daemon exited with code $rc (log: $log)"
  exit "$rc"
}

main() {
  local cmd=${1:-}
  case $cmd in
    new)
      shift
      cmd_new "$@"
      ;;
    list)
      shift
      cmd_list "$@"
      ;;
    daemon)
      shift
      cmd_daemon "$@"
      ;;
    cli)
      shift
      cmd_cli "$@"
      ;;
    mcp)
      shift
      cmd_mcp "$@"
      ;;
    env)
      shift
      cmd_env "$@"
      ;;
    -h | --help | help) usage ;;
    '') usage_err "missing command" ;;
    *) usage_err "unknown command: $cmd" ;;
  esac
}

main "$@"
