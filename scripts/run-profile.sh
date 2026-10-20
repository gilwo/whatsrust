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
MCP_TOKEN=""     # set by choose_token: the MCP token about to be written to a .env (cleared right after)
MCP_TOKEN_SRC="" # set by choose_token / enable_mcp: generated, from your environment or kept
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
  new <profile> [--semantic] [--mcp]
                                create runs/<profile>/ (logs/ + a .env from .env.example) with the
                                next free port pair; --semantic enables the embedder sidecar; --mcp
                                also switches the HTTP MCP listener on (like mcp-on)
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
  env <profile>                 show the environment cli/mcp export and whether the HTTP MCP listener
                                is on (tokens are never shown)
  mcp-on <profile>              switch the profile's HTTP MCP listener on: sets WHATSRUST_MCP_PORT and
                                WHATSRUST_MCP_TOKEN in its .env (details below)
  mcp-off <profile>             switch it off: comments out WHATSRUST_MCP_PORT, keeps the token line
  mcp-client <profile>          print the URL, a Claude Code config snippet (the token only as
                                ${WHATSRUST_MCP_TOKEN}, never its value) and how to export the token

Ports: the first profile gets REST 9270 / MCP 9271, then 9280/9281, 9290/9291, ... The HTTP MCP
listener is off until you switch it on (mcp-on, or new --mcp).

MCP listener (docs/HOWTO.md section 5a): mcp-on sets two active lines in the profile's .env.
  Port   the one reserved for the profile (its commented WHATSRUST_MCP_PORT), else REST port + 1,
         else the next port that is not the profile's REST port or any port another profile uses.
  Token  1. an active WHATSRUST_MCP_TOKEN already in the .env is kept          (token: kept)
         2. else WHATSRUST_MCP_TOKEN exported in your shell is used            (token: from your environment)
            it must be at least 22 characters of A-Z a-z 0-9 + / = _ . ~ - and differ from the
            profile's WHATSRUST_API_TOKEN; if not, mcp-on stops and changes nothing
         3. else a new one from `openssl rand -base64 32` (needs openssl)      (token: generated)
  A token is never printed and never put on a command line; the .env stays mode 600. Already on:
  nothing is rewritten (a missing token is added). A port or a token already in the .env that the
  daemon would refuse (token under 22 characters or equal to WHATSRUST_API_TOKEN, port equal to the
  REST port) stops mcp-on with the reason and changes nothing. mcp-off keeps the token line, so a
  later mcp-on reuses the same token and the clients keep working. The daemon reads the .env only at
  start: when the profile's daemon runs (instance lock held, or its tmux session exists) both
  commands add "restart the daemon to apply".

Examples:
  scripts/run-profile.sh new dev                 create runs/dev/ (new dev --mcp: with the MCP listener on)
  scripts/run-profile.sh daemon dev              run in this terminal; Ctrl+C to stop
  scripts/run-profile.sh daemon dev --tmux       run in tmux session whatsrust-dev (Ctrl-b d detaches)
  scripts/run-profile.sh daemon dev --detach     same, but stay in the background
  scripts/run-profile.sh list                    every profile; add --tmux for the tmux ones only
  scripts/run-profile.sh attach dev              attach to the session again
  scripts/run-profile.sh cli dev status          `whatsrust status` against dev's daemon
  scripts/run-profile.sh mcp-on dev              switch dev's HTTP MCP listener on (restart a running daemon)
  scripts/run-profile.sh mcp-client dev          URL, Claude Code config and token export hint for it
  scripts/run-profile.sh mcp-off dev             switch it off again (the token stays)
  make new-profile P=dev [SEMANTIC=1] [MCP=1]        same as new (MCP=1 adds --mcp)
  make run-profile P=dev [RELEASE=1] [USE_TMUX=1]    same as daemon (USE_TMUX=1 adds --tmux)

Environment:
  RUN_PROFILE_TMUX_SOCKET=<name>   testing/advanced: every tmux call uses the private server
                                   `tmux -L <name>` instead of the default one
  CARGO_TARGET_DIR                 where cargo builds (default: <repo>/target); daemon, cli and mcp
                                   look for the binary there
  Variables exported in your shell (WHATSRUST_*, WHATSAPP_*, HEALTH_PORT, BACKUP_DIR, RUST_LOG)
  beat the profile's .env when the daemon runs in this terminal; daemon names them (never their
  values) when it starts. With --tmux they are NOT applied (see the tmux note).
  WHATSRUST_MCP_TOKEN   mcp-on / new --mcp use it as the token when the .env has none (see above).
                        Exported in the shell that starts the daemon it also overrides the .env
                        token, so for a Claude Code client export it in another terminal.

Exit codes:
  0  done
  1  failed: no such profile, the profile already exists, tmux not found (daemon --tmux/--detach,
     attach, list --tmux), tmux session already exists, no built binary, an unusable token or
     missing openssl (mcp-on, new --mcp), mcp-client while the listener is off, ...
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
  sed -n -E "/^#[[:space:]]*(export[[:space:]]+)?${2}[[:space:]]*=[[:space:]]*[0-9]/{s/^[^=]*=[[:space:]]*([0-9]+).*/\1/p;q;}" "$1"
}

# ports_in_use [skip-profile-dir]: every WHATSRUST_PORT / WHATSRUST_MCP_PORT number in any profile's
# .env, including commented-out lines, so the MCP port reserved by `new` stays reserved after the
# owner enables it. The skipped profile's own .env is left out.
ports_in_use() {
  local f
  for f in "$RUNS"/*/.env; do
    [ -f "$f" ] || continue
    [ "${f%/.env}" != "${1:-}" ] || continue
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

# valid_port <text>: true for a TCP port number, 1-65535 (digits only).
valid_port() {
  case $1 in '' | *[!0-9]*) return 1 ;; esac
  [ "${#1}" -le 5 ] && [ "$((10#$1))" -ge 1 ] && [ "$((10#$1))" -le 65535 ]
}

# trim <text>: the text without leading and trailing whitespace (bash builtins only: it may be a token).
trim() {
  local v=$1
  v=${v#"${v%%[![:space:]]*}"}
  v=${v%"${v##*[![:space:]]}"}
  printf '%s\n' "$v"
}

# lock_state <profile-dir>: HELD while a process has the instance lock open (lsof), else free;
# unknown when lsof is missing.
lock_state() {
  if [ ! -e "$1/whatsapp.db.instance.lock" ]; then
    echo free
  elif ! command -v lsof >/dev/null 2>&1; then
    echo unknown
  elif lsof -t -- "$1/whatsapp.db.instance.lock" >/dev/null 2>&1; then
    echo HELD
  else
    echo free
  fi
}

# daemon_running <profile> <profile-dir>: true while the profile's daemon seems to run: its instance
# lock is held, or its tmux session exists.
daemon_running() {
  [ "$(lock_state "$2")" = HELD ] && return 0
  have_tmux && has_session "$(session_name "$1")"
}

# restart_note <profile>: the daemon reads its .env only at start, so say so when it is running.
restart_note() {
  if daemon_running "$1" "$DIR"; then
    echo "  the daemon of '$1' is running: restart the daemon to apply"
  fi
}

# rest_port_of <env-file>: the profile's REST port (the daemon's default 7270 when the .env has none).
rest_port_of() {
  local v
  v=$(env_get "$1" WHATSRUST_PORT)
  v=${v:-7270}
  valid_port "$v" || die "WHATSRUST_PORT=$v in $1 is not a valid port number"
  printf '%s\n' "$((10#$v))"
}

# pick_mcp_port <profile-dir>: the MCP port mcp-on should use: the port reserved in the .env (the
# commented WHATSRUST_MCP_PORT), else REST port + 1, else the next port above that. Never the
# profile's REST port, 7270/7271, or a port that another profile's .env mentions (commented lines count).
pick_mcp_port() {
  local file="$1/.env" rest used p
  rest=$(rest_port_of "$file")
  # 7270/7271 are the daemon's own defaults (and .env.example mentions them): never for a profile.
  used="7270 7271 $(ports_in_use "$1" | tr '\n' ' ')"
  p=$(commented_port "$file" WHATSRUST_MCP_PORT 2>/dev/null || true)
  if valid_port "$p" && [ "$((10#$p))" -ne "$rest" ] && ! in_list "$((10#$p))" "$used"; then
    printf '%s\n' "$((10#$p))"
    return 0
  fi
  p=$((rest + 1))
  while [ "$p" -eq "$rest" ] || in_list "$p" "$used"; do
    p=$((p + 1))
  done
  [ "$p" -le 65535 ] || die "no free port left above $rest for the MCP listener"
  printf '%s\n' "$p"
}

# token_problem <token> <api-token> [strict]: why the daemon would refuse this MCP token (nothing
# printed when it is fine): fewer than 22 characters after trimming, or equal to the API token.
# strict (a token that will be written to a .env): also only characters that stay intact in an
# unquoted .env value.
token_problem() {
  local t api
  t=$(trim "$1")
  api=$(trim "$2")
  if [ "${#t}" -lt 22 ]; then
    printf 'is too short (%s characters after trimming; the daemon needs at least 22)\n' "${#t}"
  elif [ -n "$api" ] && [ "$t" = "$api" ]; then
    printf 'is the same as WHATSRUST_API_TOKEN (the daemon refuses that, so that a leaked MCP token cannot open the REST API)\n'
  elif [ "${3:-}" = strict ]; then
    case $t in
      *[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=_.~-]*)
        printf 'has characters that do not survive in an unquoted .env value (allowed: A-Z a-z 0-9 + / = _ . ~ -)\n'
        ;;
    esac
  fi
}

# choose_token <api-token>: set MCP_TOKEN and MCP_TOKEN_SRC for a profile that has no token yet: the
# one exported in the caller's environment (validated), else a new one from openssl. Dies, with the
# reason, instead of falling back when the exported token is unusable.
choose_token() {
  local reason
  if [ -n "${WHATSRUST_MCP_TOKEN:-}" ]; then
    MCP_TOKEN=$(trim "$WHATSRUST_MCP_TOKEN")
    reason=$(token_problem "$MCP_TOKEN" "$1" strict)
    [ -z "$reason" ] || die "WHATSRUST_MCP_TOKEN in your environment $reason
  nothing was changed; unset it to get a generated token, or export a valid one"
    MCP_TOKEN_SRC="from your environment"
    return 0
  fi
  command -v openssl >/dev/null 2>&1 ||
    die "openssl not found, it generates the MCP token (install: brew install openssl, or apt install openssl);
  or export your own WHATSRUST_MCP_TOKEN (at least 22 characters) and run again"
  MCP_TOKEN=$(openssl rand -base64 32) || die "openssl could not generate a token"
  reason=$(token_problem "$MCP_TOKEN" "$1" strict)
  [ -z "$reason" ] || die "openssl produced a token that $reason"
  MCP_TOKEN_SRC=generated
}

# The two .env keys the MCP commands manage, as active ("KEY=...") and commented ("# KEY=...") lines.
MCP_KEY_ACTIVE='^[[:space:]]*(export[[:space:]]+)?(WHATSRUST_MCP_PORT|WHATSRUST_MCP_TOKEN)[[:space:]]*='
MCP_KEY_COMMENTED='^#[[:space:]]*(export[[:space:]]+)?(WHATSRUST_MCP_PORT|WHATSRUST_MCP_TOKEN)[[:space:]]*='
LK_STATE="" # set by mcp_key_line: active, commented or other
LK_KEY=""   # set by mcp_key_line: the key of an active or commented line
ENV_TMP=""  # the temp file env_edit is writing (removed on exit unless moved into place)

# mcp_key_line <line>: classify a .env line (LK_STATE, LK_KEY).
mcp_key_line() {
  LK_STATE=other
  LK_KEY=""
  if [[ $1 =~ $MCP_KEY_ACTIVE ]]; then
    LK_STATE=active
    LK_KEY=${BASH_REMATCH[2]}
  elif [[ $1 =~ $MCP_KEY_COMMENTED ]]; then
    LK_STATE=commented
    LK_KEY=${BASH_REMATCH[2]}
  fi
}

# env_edit <env-file> <spec>...: rewrite the .env with bash builtins only, so a token in a spec never
# reaches the argument list of a command. A spec is KEY=VALUE or -KEY (KEY: WHATSRUST_MCP_PORT or
# WHATSRUST_MCP_TOKEN).
#   KEY=VALUE  the first active KEY line becomes KEY=VALUE; when there is none, the first commented
#              "# KEY=..." line does; with neither, KEY=VALUE is appended. Further active KEY lines
#              are commented out (the daemon would use only the first one anyway).
#   -KEY       every active KEY line is commented out.
# The result is written next to the original as a mode-600 temp file and moved over it.
env_edit() {
  local file=$1 line out spec key value done_keys=" " active_keys=" "
  shift
  while IFS= read -r line || [ -n "$line" ]; do
    mcp_key_line "$line"
    if [ "$LK_STATE" = active ]; then active_keys="$active_keys$LK_KEY "; fi
  done <"$file"

  trap '[ -z "$ENV_TMP" ] || rm -f -- "$ENV_TMP"' EXIT
  ENV_TMP=$(mktemp "${file%/*}/.env.XXXXXX") || die "could not create a temp file next to $file"
  {
    while IFS= read -r line || [ -n "$line" ]; do
      mcp_key_line "$line"
      out=$line
      if [ "$LK_STATE" != other ]; then
        for spec in "$@"; do
          case $spec in
            -*)
              key=${spec#-}
              value=""
              ;;
            *)
              key=${spec%%=*}
              value=${spec#*=}
              ;;
          esac
          [ "$key" = "$LK_KEY" ] || continue
          if [ "$LK_STATE" = active ]; then
            if [ "$spec" != "-$key" ] && ! in_list "$key" "$done_keys"; then
              out="$key=$value"
              done_keys="$done_keys$key "
            else
              out="# $line"
            fi
          elif [ "$spec" != "-$key" ] && ! in_list "$key" "$active_keys" && ! in_list "$key" "$done_keys"; then
            out="$key=$value"
            done_keys="$done_keys$key "
          fi
          break
        done
      fi
      printf '%s\n' "$out"
    done <"$file"
    for spec in "$@"; do
      key=${spec%%=*}
      case $spec in -*) continue ;; esac
      in_list "$key" "$done_keys" || printf '%s\n' "$spec"
    done
  } >"$ENV_TMP"
  mv -f -- "$ENV_TMP" "$file"
  ENV_TMP=""
  trap - EXIT
}

# enable_mcp <profile> (DIR is the profile folder): make the profile's .env turn the HTTP MCP listener
# on, see the "MCP listener" section of --help for the rules. Prints one status line, never the token.
enable_mcp() {
  local name=$1 file="$DIR/.env" rest api port tok reason edits=()
  [ -f "$file" ] || die "$file not found (a profile needs its .env)"
  rest=$(rest_port_of "$file")
  api=$(env_get "$file" WHATSRUST_API_TOKEN)
  port=$(env_get "$file" WHATSRUST_MCP_PORT)
  tok=$(env_get "$file" WHATSRUST_MCP_TOKEN)

  if [ -n "$port" ] && [ "$port" != 0 ]; then
    valid_port "$port" || die "WHATSRUST_MCP_PORT=$port in $file is not a valid port number; fix or remove that line, then run mcp-on again"
    [ "$((10#$port))" -ne "$rest" ] || die "WHATSRUST_MCP_PORT=$port in $file is the profile's REST port (WHATSRUST_PORT); the daemon refuses that. Fix or remove that line, then run mcp-on again"
    port=$((10#$port))
  else
    port=$(pick_mcp_port "$DIR")
    edits+=("WHATSRUST_MCP_PORT=$port")
  fi
  if [ -n "$tok" ]; then
    reason=$(token_problem "$tok" "$api")
    [ -z "$reason" ] || die "WHATSRUST_MCP_TOKEN in $file $reason
  fix or remove that line, then run mcp-on again"
    MCP_TOKEN_SRC=kept
  else
    choose_token "$api"
    edits+=("WHATSRUST_MCP_TOKEN=$MCP_TOKEN")
  fi

  if [ "${#edits[@]}" -eq 0 ]; then
    echo "MCP listener already enabled on port $port"
    return 0
  fi
  env_edit "$file" "${edits[@]}"
  MCP_TOKEN=""
  echo "MCP listener enabled on port $port (token: $MCP_TOKEN_SRC)"
  restart_note "$name"
}

cmd_new() {
  local name="" semantic=0 mcp=0 arg
  for arg in "$@"; do
    case $arg in
      --semantic) semantic=1 ;;
      --mcp) mcp=1 ;;
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
  if [ "$mcp" -eq 1 ]; then
    # Fail before anything is created: an unusable exported token, or no openssl. The token chosen
    # here is dropped; enable_mcp chooses again once the profile exists.
    choose_token ""
    MCP_TOKEN=""
  fi

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

# MCP listener over HTTP (docs/HOWTO.md section 5a): on while WHATSRUST_MCP_PORT below is active
# (no leading '#'). Switch it with: scripts/run-profile.sh mcp-on|mcp-off $name
# The port is reserved for this profile. A token you set by hand needs at least 22 characters and
# must differ from WHATSRUST_API_TOKEN (generate one with: openssl rand -base64 32).
# WHATSRUST_MCP_PORT=$mcp_port
# WHATSRUST_MCP_TOKEN=<paste the generated token here>

# ---------------------------------------------------------------------------
# .env.example (documentation, all commented out)
# ---------------------------------------------------------------------------
EOF
    cat "$ROOT/.env.example"
  } >"$dir/.env"

  echo "created profile '$name' in $dir"
  if [ "$mcp" -eq 1 ]; then
    echo "  REST port $rest_port, device name whatsrust-$name"
  else
    echo "  REST port $rest_port, MCP port $mcp_port (off: switch it on with: scripts/run-profile.sh mcp-on $name), device name whatsrust-$name"
  fi
  if [ "$semantic" -eq 1 ] && [ ! -x "$embedder" ]; then
    echo "  embedder sidecar not built yet: run 'make embedder-build' before starting the daemon"
  fi
  if [ "$mcp" -eq 1 ]; then
    DIR=$dir
    enable_mcp "$name"
    echo "next: scripts/run-profile.sh daemon $name, then scripts/run-profile.sh mcp-client $name"
  else
    echo "next: scripts/run-profile.sh daemon $name"
  fi
}

# profile_ports <profile-dir>: set P_REST and P_MCP (what the list views show for the two ports).
profile_ports() {
  P_REST=$(env_get "$1/.env" WHATSRUST_PORT)
  P_REST=${P_REST:-default}
  P_MCP=$(env_get "$1/.env" WHATSRUST_MCP_PORT)
  if [ -z "$P_MCP" ] || [ "$P_MCP" = 0 ]; then
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
    lock=$(lock_state "$d")
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
  # The HTTP MCP listener is read by the daemon only; cli and mcp do not export these two.
  local mcp_port
  mcp_port=$(env_get "$DIR/.env" WHATSRUST_MCP_PORT)
  if [ -n "$mcp_port" ] && [ "$mcp_port" != 0 ]; then
    echo "WHATSRUST_MCP_PORT=$mcp_port"
  else
    echo "WHATSRUST_MCP_PORT=<off>"
  fi
  if [ -n "$(env_get "$DIR/.env" WHATSRUST_MCP_TOKEN)" ]; then
    echo "WHATSRUST_MCP_TOKEN=<set>"
  else
    echo "WHATSRUST_MCP_TOKEN=<unset>"
  fi
}

cmd_mcp_on() {
  [ $# -eq 1 ] || usage_err "mcp-on: usage: mcp-on <profile>"
  require_profile "$1"
  enable_mcp "$1"
}

cmd_mcp_off() {
  [ $# -eq 1 ] || usage_err "mcp-off: usage: mcp-off <profile>"
  require_profile "$1"
  local file="$DIR/.env" port
  [ -f "$file" ] || die "$file not found (a profile needs its .env)"
  port=$(env_get "$file" WHATSRUST_MCP_PORT)
  if [ -z "$port" ] || [ "$port" = 0 ]; then
    echo "MCP listener is already off for $1"
    return 0
  fi
  env_edit "$file" -WHATSRUST_MCP_PORT
  if [ -n "$(env_get "$file" WHATSRUST_MCP_TOKEN)" ]; then
    echo "MCP listener switched off for $1 (the token line stays in the .env, so mcp-on keeps the same token)"
  else
    echo "MCP listener switched off for $1"
  fi
  restart_note "$1"
}

# export_hint <variable> <env-file>: the shell line that exports the .env's token without printing it
# (the value goes from sed's output straight into the variable; it is on no command line).
export_hint() {
  printf "export %s=\"\$(sed -n 's/^WHATSRUST_MCP_TOKEN=//p' \"%s\")\"\n" "$1" "$2"
}

# cmd_mcp_client: what a Claude Code user needs to connect to the profile's listener. The snippets
# carry the token only as ${VAR}; the token itself is never printed.
cmd_mcp_client() {
  [ $# -eq 1 ] || usage_err "mcp-client: usage: mcp-client <profile>"
  require_profile "$1"
  local name=$1 file="$DIR/.env" port tok bind host lower url server tokvar altvar json plain
  port=$(env_get "$file" WHATSRUST_MCP_PORT)
  if [ -z "$port" ] || [ "$port" = 0 ]; then
    die "MCP listener is off for $name (enable it with: scripts/run-profile.sh mcp-on $name)"
  fi
  valid_port "$port" || die "WHATSRUST_MCP_PORT=$port in $file is not a valid port number"
  tok=$(env_get "$file" WHATSRUST_MCP_TOKEN)
  [ -n "$tok" ] || die "the MCP listener is on (port $port) but $file has no WHATSRUST_MCP_TOKEN, so the daemon would refuse to start (add one with: scripts/run-profile.sh mcp-on $name)"

  bind=$(env_get "$file" WHATSRUST_MCP_BIND)
  bind=${bind:-127.0.0.1}
  host=${bind#[}
  host=${host%]}
  case $host in
    *[!A-Za-z0-9.:_-]*) die "WHATSRUST_MCP_BIND=$bind in $file does not look like a host or IP address" ;;
  esac
  lower=$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')
  # A wildcard bind is reached through loopback from this machine (the daemon's own CLI does the same).
  case $host in
    0.0.0.0) host=127.0.0.1 ;;
    ::) host=::1 ;;
  esac
  case $host in *:*) url="http://[$host]:$((10#$port))/mcp" ;; *) url="http://$host:$((10#$port))/mcp" ;; esac
  server="whatsrust-$name"
  tokvar=WHATSRUST_MCP_TOKEN
  altvar=${tokvar}_$(printf '%s' "${name//[!A-Za-z0-9]/_}" | tr '[:lower:]' '[:upper:]')

  echo "MCP listener of profile $name: $url"
  case $lower in
    localhost | 127.* | ::1) ;;
    *)
      echo "  WHATSRUST_MCP_BIND=$bind is not a loopback address: put a TLS reverse proxy in front and give clients its https URL"
      echo "  (the token and all messages are cleartext otherwise; see docs/HOWTO.md section 5a and deploy/proxy/)"
      ;;
  esac
  echo
  echo "Claude Code config, as a project .mcp.json (the token is not in it, only \${$tokvar}):"
  # shellcheck disable=SC2016 # the ${...} must reach the output as written
  json=$(printf '{"type":"http","url":"%s","headers":{"Authorization":"Bearer ${%s}"},"timeout":120000}' "$url" "$tokvar")
  printf '{"mcpServers":{"%s":%s}}\n' "$server" "$json"
  echo
  echo "or add it with the claude CLI (add --scope user to use it in all projects):"
  printf "claude mcp add-json %s '%s'\n" "$server" "$json"
  echo
  echo "Export the token in the shell you start Claude Code from (this does not print it):"
  export_hint "$tokvar" "$file"
  echo
  echo "Notes:"
  echo "  - Claude Code reads \${$tokvar} when it starts, so start it from a shell where the variable is exported; /mcp then lists $server."
  echo "  - One $tokvar variable holds one token, so it serves one profile at a time. To use several profiles at once,"
  echo "    give each its own variable: write \${$altvar} instead of \${$tokvar} in the snippet and export that name:"
  printf '    '
  export_hint "$altvar" "$file"
  echo "  - Do not put the token in a command line (claude mcp add --header \"Authorization: Bearer <token>\" would, and stores it in ~/.claude.json)."
  plain=$(sed -n -E '/^[[:space:]]*(export[[:space:]]+)?WHATSRUST_MCP_TOKEN[[:space:]]*=/{p;q;}' "$file")
  if [ "$plain" != "WHATSRUST_MCP_TOKEN=$tok" ]; then
    echo "  - The token line in the .env is not in the plain WHATSRUST_MCP_TOKEN=<value> form (export, spaces, quotes or a comment),"
    echo "    so the export line above would not give the exact token: rewrite that line in the plain form."
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
    mcp-on)
      shift
      cmd_mcp_on "$@"
      ;;
    mcp-off)
      shift
      cmd_mcp_off "$@"
      ;;
    mcp-client)
      shift
      cmd_mcp_client "$@"
      ;;
    -h | --help | help) usage ;;
    '') usage_err "missing command" ;;
    *) usage_err "unknown command: $cmd" ;;
  esac
}

main "$@"
