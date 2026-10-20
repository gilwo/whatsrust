#!/usr/bin/env bash
# Dev run profiles (F9): each debug/live run keeps its state (whatsapp.db, backups, instance lock,
# logs, .env) in runs/<profile>/ instead of the project root. No Rust change: the daemon simply
# starts with the profile folder as its working directory. Run with --help for the commands.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNS="$ROOT/runs"
DIR="" # set by require_profile
BIN="" # set by find_bin
PANE_SESSION="" # set by cmd_pane: the tmux session this process runs in as the daemon's pane
PANE_LOG=""     # set by cmd_pane: the log file the daemon output goes to
# Every tmux call goes through tmx; RUN_PROFILE_TMUX_SOCKET (testing/advanced) selects a private server.
TMUX_CMD=(tmux)
if [ -n "${RUN_PROFILE_TMUX_SOCKET:-}" ]; then
  TMUX_CMD=(tmux -L "$RUN_PROFILE_TMUX_SOCKET")
fi

usage() {
  cat <<'EOF'
Usage: scripts/run-profile.sh <command> [args]

Dev run profiles: each run keeps its state (whatsapp.db, backups, instance lock, logs, .env) in
runs/<profile>/ instead of the project root. Profile names: letters, digits, '.', '_', '-'.

Commands:
  new <profile> [--semantic]    create runs/<profile>/ (logs/ + a .env from .env.example) with the
                                next free port pair; --semantic enables the embedder sidecar
  list [--tmux]                 show every profile: REST port, MCP port, DB present, instance lock,
                                tmux (running, or -). --tmux shows only the profiles whose daemon
                                runs in a tmux session: REST, MCP, start time, attached (needs tmux
                                installed). (--all is accepted and means the default view.)
  daemon <profile> [--release] [--tmux] [--detach]
                                build once, then run the daemon inside runs/<profile>/ in THIS
                                terminal; output goes to the terminal and, without colours, to
                                logs/<UTC time>.log. Ctrl+C stops it gracefully. Options:
                                  --release  build and run the release binary (default: debug)
                                  --tmux     opt in to tmux: run it in the tmux session
                                             whatsrust-<profile>; from a terminal it attaches
                                             right away
                                  --detach   implies --tmux; leave the session running in the
                                             background and print how to attach
  attach <profile>              attach to the profile's tmux session (detach again with Ctrl-b d)
  cli <profile> <args...>       run `whatsrust <args...>` against that profile's daemon
  mcp <profile>                 run `whatsrust mcp` (stdio) against that profile's daemon
  env <profile>                 show the environment cli/mcp export (the token is never shown)

Ports: the first profile gets REST 9270 / MCP 9271, then 9280/9281, 9290/9291, ... The MCP
listener is off until you enable it in the profile's .env (WHATSRUST_MCP_PORT + a token you generate).

Examples:
  scripts/run-profile.sh new dev                 create runs/dev/
  scripts/run-profile.sh daemon dev              run in this terminal; Ctrl+C to stop
  scripts/run-profile.sh daemon dev --tmux       run in tmux session whatsrust-dev (Ctrl-b d detaches)
  scripts/run-profile.sh daemon dev --detach     same, but stay in the background
  scripts/run-profile.sh list                    every profile; add --tmux for the tmux ones only
  scripts/run-profile.sh attach dev              attach to the session again
  scripts/run-profile.sh cli dev status          `whatsrust status` against dev's daemon
  make run-profile P=dev [RELEASE=1] [USE_TMUX=1]    same as daemon (USE_TMUX=1 adds --tmux)

Environment:
  RUN_PROFILE_TMUX_SOCKET=<name>   testing/advanced: every tmux call uses the private server
                                   `tmux -L <name>` instead of the default one
  CARGO_TARGET_DIR                 where cargo builds (default: <repo>/target); daemon, cli and mcp
                                   look for the binary there
  Variables exported in your shell (WHATSRUST_*, WHATSAPP_*, HEALTH_PORT, BACKUP_DIR, RUST_LOG)
  beat the profile's .env when the daemon runs in this terminal; daemon names them (never their
  values) when it starts. With --tmux they are NOT applied (see the tmux note).

Exit codes:
  0  done
  1  failed: no such profile, the profile already exists, tmux not found (daemon --tmux/--detach,
     attach, list --tmux), tmux session already exists, no built binary, ...
  2  usage error (unknown command or option, missing argument)
  daemon in this terminal exits with the daemon's own exit code; a failed build exits with cargo's.
  cli and mcp exit with whatsrust's exit code.

Notes:
  - cli and mcp read only WHATSRUST_PORT, WHATSRUST_BIND and WHATSRUST_API_TOKEN from the profile's
    .env (nothing else is sourced) and use the newest built target/{release,debug}/whatsrust.
  - cli --rollback / --migrate run inside runs/<profile>/ because they act on its whatsapp.db.
  - list: LOCK is "HELD" when a process has runs/<profile>/whatsapp.db.instance.lock open (checked
    with lsof; "unknown" if lsof is missing). The lock file itself stays behind after a stop.
    TMUX is "running" while the profile's session exists (also while its pane waits for Enter).
  - tmux is opt-in and needs tmux 3.2 or newer. A session does not inherit your shell's
    environment: only WHATSRUST_ENV_FILE is passed in, so variables exported in your shell
    (WHATSRUST_*, RUST_LOG, ...) are NOT applied inside the session: put them in the profile's
    .env. A '.' in a profile name becomes '_' in the session name. A second daemon --tmux for a
    profile whose session exists is refused.
  - When the daemon ends inside tmux (exit, Ctrl+C, a signal, a crash), the pane prints the exit
    code (and the signal name for codes above 128) and waits for Enter before the session closes.
  - Never run two copies of the same paired DB at once; each profile is its own linked device.

More: docs/HOWTO.md section 3, "Run profiles" (includes a demo playbook with expected output).
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

tmx() {
  "${TMUX_CMD[@]}" "$@"
}

have_tmux() {
  command -v tmux >/dev/null 2>&1
}

need_tmux() {
  have_tmux || die "tmux not found (install: brew install tmux)"
}

# session_name <profile>: the tmux session of a profile's daemon. tmux turns '.' in a session name
# into '_', so do the same here to find the session again.
session_name() {
  printf 'whatsrust-%s\n' "${1//./_}"
}

# has_session <session>: true when exactly that session exists ('=' stops tmux's prefix matching, so
# whatsrust-dev never matches whatsrust-dev2).
has_session() {
  tmx has-session -t "=$1" 2>/dev/null
}

# attach_session <session>: become a tmux client of the session (switch-client when already inside
# tmux). Does not return.
attach_session() {
  if [ -n "${TMUX:-}" ]; then
    exec "${TMUX_CMD[@]}" switch-client -t "=$1"
  fi
  exec "${TMUX_CMD[@]}" attach-session -t "=$1"
}

# tmux_sessions: one "<created-epoch> <attached-clients> <name>" line per tmux session (the name
# last, so a name with spaces stays in one field); nothing when no tmux server runs.
tmux_sessions() {
  tmx list-sessions -F '#{session_created} #{session_attached} #{session_name}' 2>/dev/null || true
}

# session_row <tmux_sessions output> <session>: print "<created> <attached>" of that session.
session_row() {
  local created attached name
  while read -r created attached name; do
    if [ "$name" = "$2" ]; then
      printf '%s %s\n' "$created" "$attached"
      return 0
    fi
  done <<<"$1"
  return 1
}

# fmt_epoch <epoch>: local time (BSD date -r, else GNU date -d; the raw number as a last resort).
fmt_epoch() {
  date -r "$1" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || date -d "@$1" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || printf '%s\n' "$1"
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

# profile_ports <profile-dir>: set P_REST and P_MCP (what the list views show for the two ports).
profile_ports() {
  P_REST=$(env_get "$1/.env" WHATSRUST_PORT)
  P_REST=${P_REST:-default}
  P_MCP=$(env_get "$1/.env" WHATSRUST_MCP_PORT)
  if [ -z "$P_MCP" ]; then
    P_MCP=$(commented_port "$1/.env" WHATSRUST_MCP_PORT 2>/dev/null || true)
    if [ -n "$P_MCP" ]; then P_MCP="off ($P_MCP)"; else P_MCP="off"; fi
  fi
}

# list_all <tmux_sessions output>: every profile, with the TMUX column ("running" when its session exists).
list_all() {
  local d name db lock tm found=0
  for d in "$RUNS"/*/; do
    [ -d "$d" ] || continue
    d=${d%/}
    name=${d##*/}
    profile_ports "$d"
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
    if session_row "$1" "$(session_name "$name")" >/dev/null; then tm=running; else tm=-; fi
    if [ "$found" -eq 0 ]; then
      printf '%-20s %-7s %-12s %-4s %-7s %s\n' PROFILE REST MCP DB LOCK TMUX
      found=1
    fi
    printf '%-20s %-7s %-12s %-4s %-7s %s\n' "$name" "$P_REST" "$P_MCP" "$db" "$lock" "$tm"
  done
  if [ "$found" -eq 0 ]; then
    echo "no profiles in $RUNS (create one: scripts/run-profile.sh new <name>)"
  fi
}

# list_tmux <tmux_sessions output>: the profiles that have a whatsrust-<profile> session; sessions
# that are not whatsrust-<existing profile> are ignored.
list_tmux() {
  local d name row created attached att found=0
  for d in "$RUNS"/*/; do
    [ -d "$d" ] || continue
    d=${d%/}
    name=${d##*/}
    row=$(session_row "$1" "$(session_name "$name")") || continue
    created=${row%% *}
    attached=${row##* }
    case $attached in 0 | '') att=no ;; *) att=yes ;; esac
    profile_ports "$d"
    if [ "$found" -eq 0 ]; then
      printf '%-24s %-16s %-7s %-12s %-19s %s\n' SESSION PROFILE REST MCP STARTED ATTACHED
      found=1
    fi
    printf '%-24s %-16s %-7s %-12s %-19s %s\n' "$(session_name "$name")" "$name" "$P_REST" "$P_MCP" "$(fmt_epoch "$created")" "$att"
  done
  if [ "$found" -eq 0 ]; then
    echo "no run-profile daemons in tmux (scripts/run-profile.sh list shows every profile)"
  fi
}

# list: every profile (--all is an alias of that view); --tmux: only the profiles with a tmux session.
cmd_list() {
  local all=0 only_tmux=0 arg rows=""
  for arg in "$@"; do
    case $arg in
      --all) all=1 ;;
      --tmux) only_tmux=1 ;;
      -*) usage_err "list: unknown option: $arg" ;;
      *) usage_err "list: unexpected argument: $arg" ;;
    esac
  done
  [ "$all$only_tmux" != 11 ] || usage_err "list: --all and --tmux exclude each other"
  if [ "$only_tmux" -eq 1 ]; then
    need_tmux
    list_tmux "$(tmux_sessions)"
    return 0
  fi
  # Without tmux there are no sessions, so every TMUX cell is "-" (no note).
  if have_tmux; then rows=$(tmux_sessions); fi
  list_all "$rows"
}

cmd_attach() {
  [ $# -eq 1 ] || usage_err "attach: usage: attach <profile>"
  need_tmux
  require_profile "$1"
  local sess
  sess=$(session_name "$1")
  has_session "$sess" || die "no tmux session for profile $1 (start it with: scripts/run-profile.sh daemon $1 --tmux)"
  attach_session "$sess"
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

# ambient_names: exported variables that would reach (and beat) the daemon's .env, space separated.
# Names only, never values.
ambient_names() {
  compgen -e | grep -E '^(WHATSRUST_|WHATSAPP_|HEALTH_PORT$|BACKUP_DIR$|RUST_LOG$)' | grep -v '^WHATSRUST_ENV_FILE$' | tr '\n' ' ' || true
}

# warn_ambient <where>: environment variables beat the profile's .env (dotenvy only fills unset
# ones), so name any that are set. where: shell (daemon runs in this terminal), tmux (daemon is about
# to run in a tmux session, which does not get them) or pane (seen from inside the session).
warn_ambient() {
  local names
  names=$(ambient_names)
  [ -n "$names" ] || return 0
  case $1 in
    shell) note "warning: exported in your shell, these override the profile's .env: $names" ;;
    tmux) note "warning: exported in your shell, these are NOT applied inside the tmux session (put them in $DIR/.env): $names" ;;
    pane) note "warning: set in the tmux server's environment, these override the profile's .env: $names" ;;
  esac
}

# pane_exit_note <exit-code>: what the tmux pane prints when its process ends for any reason, then
# wait for Enter so the output stays readable (the session closes afterwards).
pane_exit_note() {
  local rc=$1 desc sig
  desc="exit code $rc"
  if [ "$rc" -gt 128 ]; then
    sig=$(kill -l $((rc - 128)) 2>/dev/null) || sig=""
    if [ -n "$sig" ]; then desc="$desc (SIG$sig)"; fi
  fi
  printf '\n' >&2
  note "daemon ended: $desc"
  note "log: $PANE_LOG"
  note "You are in tmux session $PANE_SESSION: press Enter to close it, or Ctrl-b d to detach."
  read -r _ || true
}

# run_daemon <log>: run $BIN inside the profile folder with its output on the terminal and, without
# colour codes, in <log>. Never returns: exits with the daemon's exit code.
run_daemon() {
  local log=$1 esc=$'\033' rc
  cd "$DIR"
  export WHATSRUST_ENV_FILE="$DIR/.env"
  # The script survives Ctrl+C (the daemon handles it and shuts down gracefully); tee and sed ignore
  # it so the daemon's shutdown output still reaches the terminal and the log. The pipeline keeps
  # stdin on the terminal for the REPL; PIPESTATUS[0] is the daemon's own exit code. The group's
  # 2>/dev/null hides only bash's own "Terminated"/"Done" job line when a signal kills the daemon
  # (the daemon's stderr is merged into stdout by its own 2>&1, so none of its output is lost).
  trap ':' INT
  set +e
  {
    "$BIN" 2>&1 | (
      trap '' INT
      exec tee >(sed -u "s/${esc}\[[0-9;]*[A-Za-z]//g" >>"$log")
    )
    rc=${PIPESTATUS[0]}
  } 2>/dev/null
  set -e
  trap - INT
  # As a tmux pane process the EXIT trap (pane_exit_note) reports instead.
  if [ -z "$PANE_SESSION" ]; then
    note "daemon exited with code $rc (log: $log)"
  fi
  exit "$rc"
}

# start_session <profile> <session> <log> <detach 0|1>: run the already built $BIN in a new detached
# tmux session whose start directory is the profile folder; then attach, or say how to.
start_session() {
  local name=$1 sess=$2 log=$3 detach=$4 opts size rows cols n tty=0
  opts=(-d)
  if [ -t 0 ] && [ -t 1 ]; then
    tty=1
    # A detached session defaults to 80x24, too small for the pairing QR code: use this terminal's size.
    size=$(stty size 2>/dev/null) || size=""
    rows=${size% *}
    cols=${size#* }
    case "$rows$cols" in
      '' | *[!0-9]*) ;;
      *) if [ "$rows" -gt 0 ] && [ "$cols" -gt 0 ]; then opts+=(-x "$cols" -y "$rows"); fi ;;
    esac
  fi
  # A tmux server started by the command below takes its global environment from this process, so
  # drop what warn_ambient said is not applied. Only WHATSRUST_ENV_FILE is passed in (-e); no
  # secret ever appears in tmux's arguments.
  for n in $(ambient_names); do unset "$n"; done
  tmx new-session "${opts[@]}" -s "$sess" -c "$DIR" -e "WHATSRUST_ENV_FILE=$DIR/.env" \
    "$BASH" "$ROOT/scripts/run-profile.sh" _pane "$name" "$log" "$BIN" ||
    die "could not create tmux session $sess (needs tmux 3.2 or newer; see tmux's message above)"
  note "tmux session: $sess"
  note "log: $log"
  if [ "$detach" -eq 0 ] && [ "$tty" -eq 1 ]; then
    attach_session "$sess"
  fi
  note "attach with: scripts/run-profile.sh attach $name   (detach again with Ctrl-b d)"
}

# daemon: run in this terminal; --tmux (or --detach, which implies it) opts in to a tmux session.
cmd_daemon() {
  local name="" release=0 detach=0 use_tmux=0 arg
  for arg in "$@"; do
    case $arg in
      --release) release=1 ;;
      --tmux) use_tmux=1 ;;
      --detach)
        detach=1
        use_tmux=1
        ;;
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

  local sess=""
  if [ "$use_tmux" -eq 1 ]; then
    have_tmux || die "tmux not found (install: brew install tmux); run without --tmux to use this terminal"
    sess=$(session_name "$name")
    if has_session "$sess"; then
      die "session $sess already exists; attach with: scripts/run-profile.sh attach $name"
    fi
    warn_ambient tmux
  else
    warn_ambient shell
  fi

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

  if [ "$use_tmux" -eq 1 ]; then
    start_session "$name" "$sess" "$log" "$detach"
  else
    note "log: $log"
    run_daemon "$log"
  fi
}

# _pane <profile> <log> <binary>: internal, not for people. The command a tmux session runs (see
# start_session): the foreground logging pipeline, then a note and a wait for Enter. The EXIT trap
# covers every way this process can end, including a failed start.
cmd_pane() {
  [ $# -eq 3 ] || usage_err "_pane is internal (use: daemon <profile> --tmux)"
  PANE_LOG=$2
  PANE_SESSION=$(session_name "$1")
  BIN=$3
  trap 'pane_exit_note "$?"' EXIT
  require_profile "$1"
  warn_ambient pane
  run_daemon "$PANE_LOG"
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
    attach)
      shift
      cmd_attach "$@"
      ;;
    _pane)
      shift
      cmd_pane "$@"
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
