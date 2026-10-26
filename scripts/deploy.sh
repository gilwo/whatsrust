#!/usr/bin/env bash
# Deploy the whatsrust daemon as a background service under a supervisor, so it restarts after a crash:
# a launchd LaunchAgent on macOS, a systemd user unit on Linux. Each instance has its own directory
# outside the repo (binary copy, .env, logs, the daemon's DB) and one unit rendered from the templates in
# deploy/supervisor/. The unit starts bin/start-if-paired, a gate that starts the daemon only for a paired
# instance (or during a QR flow of `run` / `re-pair`), so an unpaired instance never runs unattended.
# Run with --help; scripts/run-profile.sh is for dev runs.
set -euo pipefail
# Bash 5.2 makes '&' in a ${v//a/b} replacement mean "the match". The XML escaping and the template
# rendering below need '&' to stay literal. Older bashes have no such option.
shopt -u patsub_replacement 2>/dev/null || true
# A set CDPATH makes a relative `cd` print the directory and may land elsewhere.
unset CDPATH

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TEMPLATES="$ROOT/deploy/supervisor"
INSTANCE=""         # set by parse_args (install: by step 1): the instance name
POS2=""             # set by parse_args: the second positional argument (autostart on|off)
OPT_DIR=""          # set by parse_args: --dir
OPT_BINARY=""       # set by parse_args: --binary
OPT_PLATFORM=""     # set by parse_args: --platform
OPT_LINES=50        # set by parse_args: logs -n
OPT_PORT=""         # set by parse_args: --port
OPT_API=""          # set by parse_args: --api-token generate|none|keep
OPT_MCP=""          # set by parse_args: --mcp on|off
OPT_MCP_PORT=""     # set by parse_args: --mcp-port
OPT_SEMANTIC=""     # set by parse_args: --semantic on|off
OPT_MODEL=""        # set by parse_args: --model bge-m3|minilm
OPT_EMBEDDER=""     # set by parse_args: --embedder
OPT_DEVICE=""       # set by parse_args: --device-name
OPT_PAIRHIST=""     # set by parse_args: --pairing-history on|off
OPT_AUTOSTART=""    # set by parse_args: --autostart on|off
OPT_MODE=""         # set by parse_args: re-pair --mode fresh|keep-history
OPT_BACKUP=""       # set by parse_args: uninstall --backup
OPT_CONFIRM=""      # set by parse_args: uninstall --confirm
OPT_KEEP_DATA=0     # 1 with --keep-data
OPT_DELETE_DATA=0   # 1 with --delete-data
OPT_NO_BACKUP=0     # 1 with --no-backup
YES=0               # 1 with --yes: accept the defaults, skip the confirmation
DRY=0               # 1 with --dry-run: print the plan and the commands, change nothing
FOLLOW=0            # 1 with --follow (logs)
PLATFORM=""         # set by set_platform: launchd or systemd
HOST=""             # set by set_platform: launchd or systemd, whichever the host (uname -s) has
LABEL=""            # set by set_names: the launchd label, local.whatsrust.<instance>
UNIT=""             # set by set_names: the systemd unit, whatsrust-<instance>.service
UID_NUM=""          # set by set_names: the numeric user id (launchd domain gui/<uid>)
DIR=""              # the instance directory (set by use_dir or load_registry)
PLIST_FILE=""       # launchd: <dir>/<label>.plist, the file run bootstraps (set by set_paths)
AGENT_FILE=""       # launchd: ~/Library/LaunchAgents/<label>.plist, the copy that autostart installs
UNIT_FILE=""        # systemd: the unit file under ${XDG_CONFIG_HOME:-~/.config}/systemd/user/
ENV_TMP=""          # the temp file env_edit is writing (removed on exit)
DEADLINE=0          # set by deadline: a $SECONDS value
QR_MARK=""          # set by qr_mark_set: the <dir>/.qr-flow file this process wrote (removed on exit)
QR_SVC=0            # 1 while this process has a service running for an UNPAIRED QR flow (the exit trap stops it)
REPAIR_BK=""        # set by repair_backup: the re-pair backup directory
POLL=2              # seconds between two checks of the daemon
LISTEN_NOTE=0       # 1 once we said that listening ports cannot be checked
NOANSWER_HINT="use the flags and --yes" # what ask_line tells when stdin ends before an answer

# Waits, in wall-clock seconds ($SECONDS). DEPLOY_WAIT_SCALE (testing only) divides them, 1 s at least.
API_WAIT=30         # for the REST API to answer after a start
CONNECT_WAIT=60     # for a paired instance to connect
QR_WAIT=300         # 5 min for a scan
STILL_LINKED=30     # without a QR code but connected: the old link is still valid
STOP_WAIT=35        # for the supervisor to stop the job (its ExitTimeOut is 30 s)
WAIT_SCALE=${DEPLOY_WAIT_SCALE:-1}
case $WAIT_SCALE in
  '' | *[!0-9]*) WAIT_SCALE=1 ;;
  *)
    # base 10: "08" and "09" are not octal; 0 would divide by zero
    WAIT_SCALE=$((10#${WAIT_SCALE:0:6}))
    if [ "$WAIT_SCALE" -lt 1 ]; then WAIT_SCALE=1; fi
    ;;
esac

EXIT_CODES_NOTE="exit codes of the daemon: 0 = clean stop (not restarted), 1 = startup refusal or error, 101 = panic"

usage() {
  cat <<'EOF'
Usage: scripts/deploy.sh <command> <instance> [options]
       scripts/deploy.sh <command> --help

Run the whatsrust daemon as a background service under a supervisor, so it restarts after a crash: a
launchd LaunchAgent on macOS, a systemd user unit on Linux. This script is for deployment.
scripts/run-profile.sh is for dev runs (state in runs/<profile>/, started in your terminal).

Rule: an unpaired instance never runs unattended. Only `run` and `re-pair` start one, and only while
your terminal shows the QR code. If you press Ctrl+C, or nobody scans in 5 minutes, the service is
stopped again. The unit starts a gate, bin/start-if-paired, not the daemon: the gate starts the daemon for
a paired instance (or during a QR flow) and otherwise logs "not paired, not starting" and exits 0, which
neither launchd nor systemd restarts, so a crash, a login or a reboot cannot start an unpaired instance.
Known limit: if the device is unlinked on the phone WHILE the daemon runs, the daemon keeps running and
shows a QR code until its next restart (`status` warns). A SIGKILL or a power loss of deploy.sh during a QR
flow cannot be trapped: the service then keeps running until the next stop or reboot. The gate keeps it from
restarting: a marker is honoured only while its PID is alive with the recorded start time, and for 15 minutes at most.
A DB that the gate cannot read fails closed (no start); only a missing sqlite3 starts with a warning.

Prerequisites:
  - A release build: make release (target/release/whatsrust). For semantic search also: make embedder-build.
  - A terminal: install asks questions, run and re-pair show the QR code.
  - sqlite3 (strongly advised): tells whether an instance is paired. Without it the answer is "unknown"
    and the start gate starts the daemon after a restart with only a warning in the log.
  - macOS: a logged-in user session. Linux: systemd --user (see autostart for loginctl enable-linger).

Commands:
  install <instance> [options]  wizard: directory, binary, ports, tokens, MCP, semantic search, device name,
                                autostart. Writes the directory, .env and unit. Starts nothing.
                                Repeat it to change the settings (the current values are the defaults)
  run <instance>                start the service. Paired: wait until it is connected. Not paired: show the
                                QR code here until you scan it (Ctrl+C or 5 min: stop the service again)
  stop <instance>               graceful stop; the service stays installed until the next run
  restart <instance>            graceful restart through the supervisor
  re-pair <instance> [options]  link the instance to the phone again (fresh DB or keep the history)
  autostart <instance> on|off   start the service at login (only a paired instance)
  uninstall <instance> [options]
                                stop and remove the unit; then ask whether to delete the data (with a backup)
  status <instance>             paired, connected, state, PID, last exit, restarts, autostart, directory
  logs <instance> [-n N] [--follow]
                                show the last N log lines (default 50); --follow keeps printing
  upgrade <instance> [--binary <path>] [--embedder <path>]
                                replace the installed binary (and embedder), then restart if it ran

Options:
  --dry-run                     (every command that changes something) print the plan, change nothing
  --yes                         (install, re-pair) accept the default of every step you did not give a flag for
  --platform launchd|systemd    use that platform instead of the host's. Without --dry-run it must match
                                the host (uname -s), so a reviewer can render the systemd unit on a Mac with:
                                install demo --dry-run --yes --platform systemd
  scripts/deploy.sh <command> --help   all options of one command

Instance names: lower-case letters, digits and '-', 1 to 32 characters, starting with a letter or digit.
install accepts only a directory that is missing, empty, or marked as made by this script. The script
never touches, overwrites, removes, stops or boots out a unit file, LaunchAgents copy or loaded label that
lacks its managed-by marker. Every command except install finds the instance directory in the registry file
${XDG_CONFIG_HOME:-~/.config}/whatsrust/deploy/<instance>.dir that install writes.

Instance directory (outside the repo):
  macOS   ~/Library/Application Support/whatsrust/<instance>/
  Linux   ${XDG_DATA_HOME:-~/.local/share}/whatsrust/<instance>/
    bin/whatsrust            a COPY of the binary (rebuilding the repo does not change a running service)
    bin/start-if-paired      the start gate that the unit runs (starts the daemon only for a paired instance)
    bin/whatsrust-embedder   a copy of the embedder, with semantic search on
    .whatsrust-deploy        ownership marker (mode 600): install and uninstall touch only marked directories
    .qr-flow                 present only during a QR flow of run / re-pair: the PID that runs it
    .env                     mode 600: ports, tokens, device name (the CLI does not read it)
    deploy.conf              mode 600: autostart and the binary sources
    local.whatsrust.<instance>.plist   macOS: the rendered unit that run loads
    logs/daemon.log          macOS logs. Linux logs go to the journal
    whatsapp.db ...          the daemon's own files (DB, WAL, backups/, instance lock)

Units:
  macOS   label local.whatsrust.<instance>; autostart copies the plist to ~/Library/LaunchAgents/
  Linux   whatsrust-<instance>.service in ${XDG_CONFIG_HOME:-~/.config}/systemd/user/
  Templates: deploy/supervisor/whatsrust.plist.in and whatsrust.service.in.

Restart policy: restart after ANY non-zero exit (a panic exits 101), wait 10 s between starts, never
restart after exit 0 (a graceful stop). A config error therefore restarts every 10 s until you fix it:
systemd gives up after 5 starts in 300 s, launchd keeps retrying. Look at `status` and `logs`.

Examples:
  make release
  scripts/deploy.sh install main                    answer the questions (Enter takes the default)
  scripts/deploy.sh install main --yes --dry-run    show the summary and the unit, write nothing
  scripts/deploy.sh run main                        first time: scan the QR code; later: just starts
  scripts/deploy.sh status main
  scripts/deploy.sh autostart main on
  make release && scripts/deploy.sh upgrade main    new binary, graceful restart
  scripts/deploy.sh uninstall main

Exit codes:
  0  done
  1  failed: not installed, no binary, supervisor command failed, not paired in time, aborted, ...
  2  usage error (unknown command or option, missing argument, bad value, no answer on stdin)

The script signals only through the supervisor (launchctl / systemctl) and only for services it
started. It never touches runs/, the repo's own whatsapp.db, or units it did not create.

More: docs/HOWTO.md section 3b, "Run as a service" (includes a demo playbook).
EOF
}

# usage_command <command>: the help of one command (scripts/deploy.sh <command> --help).
usage_command() {
  case $1 in
    install) usage_install ;;
    run)
      cat <<'EOF'
Usage: scripts/deploy.sh run <instance> [--platform launchd|systemd]

Start the service. It refuses when the service runs already.
  Paired      starts the service, waits up to 60 s until `status` reports connected: true, prints the PID.
  Not paired  starts the service, waits up to 30 s for the REST API, then draws the QR code from
              `whatsrust qr` here (redrawn only when it changes). Scan it with WhatsApp -> Settings ->
              Linked devices -> Link a device. When the phone confirms, it prints the device name and the PID.
              Ctrl+C, or 5 minutes without a scan: the service is stopped gracefully, exit 1.
Then it applies autostart when deploy.conf says on. The QR flow needs the REST API (WHATSRUST_PORT not 0).
For the QR flow it writes <dir>/.qr-flow (its PID, mode 600) before the start: the start gate lets the daemon
start for an unpaired instance only while that process is alive. The file goes away on every way out.
EOF
      ;;
    stop)
      cat <<'EOF'
Usage: scripts/deploy.sh stop <instance> [--platform launchd|systemd]

Stop the service gracefully (the daemon drains, backs up the DB and exits 0). The service stays installed
and stays stopped until `run` (or until your next login when autostart is on).
  macOS  launchctl bootout, then waits up to 35 s until the job is unloaded
  Linux  systemctl --user stop
EOF
      ;;
    restart)
      cat <<'EOF'
Usage: scripts/deploy.sh restart <instance> [--dry-run] [--platform launchd|systemd]

Restart the daemon gracefully through the supervisor. It refuses an unpaired instance (use run / re-pair).
  macOS  launchctl kill SIGTERM, waits up to 35 s until the process is gone (exit 0 is not restarted by
         KeepAlive), then launchctl kickstart. After 35 s it falls back to kickstart -k and says so.
  Linux  systemctl --user restart
EOF
      ;;
    're-pair')
      cat <<'EOF'
Usage: scripts/deploy.sh re-pair <instance> [--mode fresh|keep-history] [--device-name <name>] [--yes]
                                  [--dry-run] [--platform launchd|systemd]

Link the instance to the phone again, for example to change the device name. A wizard asks:
  1. mode  fresh (default): stop the service, move whatsapp.db(+ -wal, -shm) to <dir>/re-pair-backup-<UTC>/,
           pair into an empty DB. This avoids an open bug (a re-pair that keeps the DB keeps old app-state
           data: no push name, no archive or read-state events). The message history starts empty.
           Also remove the OLD device on the phone.
           keep-history: you unlink the device on the phone first; the DB files are COPIED to the backup
           directory and the DB is kept. It may hit that bug. If the daemon connects with the old link, the
           command says so and leaves it running: unlink on the phone, run re-pair again.
  2. device name (default: the current one). It is used only while unpaired, then fixed.
  3. confirmation.
Then the QR flow of `run` (see run --help). Autostart is switched off during a re-pair and applied again
when the new link works. --yes accepts the defaults and skips the questions (mode and name from the flags).
EOF
      ;;
    autostart)
      cat <<'EOF'
Usage: scripts/deploy.sh autostart <instance> on|off [--dry-run] [--platform launchd|systemd]

Start the service at login. It refuses `on` for an unpaired instance (run it first): autostart must never
start an unpaired instance. The choice is stored in <dir>/deploy.conf.
  macOS  on: copies the plist into ~/Library/LaunchAgents/ (launchd loads it at login). off: removes that copy.
  Linux  on: systemctl --user enable, and prints the linger hint (loginctl enable-linger "$USER", not run
         by this script). off: systemctl --user disable.
It does not start or stop the service now.
EOF
      ;;
    uninstall)
      cat <<'EOF'
Usage: scripts/deploy.sh uninstall <instance> [--keep-data | --delete-data] [--backup <path> | --no-backup]
                                    [--confirm <instance>] [--dry-run] [--platform launchd|systemd]

Stop the service gracefully, remove the unit (macOS: both plist copies; Linux: the unit file + daemon-reload)
and the registry file. Then it asks: Delete the instance data (DB, pairing, backups)? [y/N]
  No   prints the data directory; the data stays (install it again later with the same directory).
  Yes  asks Back up the data first? [Y/n] and where (default ~/whatsrust-backups/<instance>-<UTC>.tar.gz),
       writes and verifies the archive (mode 600), asks you to type the instance name, deletes the directory.
The directory is deleted only when it is the registered one, absolute, not / or $HOME or above it, not
inside the repo or above it (physical paths), no symbolic link, holds bin/whatsrust and carries the
.whatsrust-deploy marker of this instance. Deleting local data does not unlink the device: remove it on the phone.
Options for scripts: --keep-data / --delete-data answer the first question; --backup <path> / --no-backup the
backup questions; --confirm <instance> replaces the typed confirmation (required with --delete-data when
nobody can type).
EOF
      ;;
    status)
      cat <<'EOF'
Usage: scripts/deploy.sh status <instance> [--platform launchd|systemd]

Show paired (yes / no / unknown), the start gate (open / closed), connected (from the REST API while the
service runs), the supervisor state, PID, last exit, restart count, autostart, directory and the first 12
characters of the binary SHA-256. When the DB is unpaired it also switches autostart off, if it was on.
  paired      sqlite3 -readonly <dir>/whatsapp.db "SELECT pn FROM device WHERE id = 1" (no DB: no)
  macOS       parsed from `launchctl print gui/<uid>/<label>`
  Linux       systemctl --user show -p ActiveState,SubState,MainPID,ExecMainStatus,NRestarts ...
Exit code 0 means the state was printed, also when the daemon is not running.
EOF
      ;;
    logs)
      cat <<'EOF'
Usage: scripts/deploy.sh logs <instance> [-n N] [--follow] [--platform launchd|systemd]

Show the last N lines (default 50) of the daemon log; --follow keeps printing new lines (Ctrl+C stops).
  macOS  tail of <dir>/logs/daemon.log (launchd does not rotate it)
  Linux  journalctl --user -u whatsrust-<instance>.service
EOF
      ;;
    upgrade)
      cat <<'EOF'
Usage: scripts/deploy.sh upgrade <instance> [--binary <path>] [--embedder <path>] [--dry-run] [--platform launchd|systemd]

Replace <dir>/bin/whatsrust with a new binary. The copy is atomic (written as bin/whatsrust.new, then
renamed). Prints the old and the new SHA-256. Source: --binary, else the path stored in deploy.conf,
else <repo>/target/release/whatsrust (build it with: make release). With semantic search on, the embedder
copy is replaced the same way (--embedder, deploy.conf, or embedder-rs/target/release/whatsrust-embedder)
when a source exists. If the service ran, it restarts gracefully; if it was stopped, the files are only replaced.
EOF
      ;;
  esac
}

# usage_install: the help of install (long: it lists the steps and their flags).
usage_install() {
  cat <<'EOF'
Usage: scripts/deploy.sh install [<instance>] [options]

A wizard that sets up an instance, one step at a time. Each step shows its default: Enter takes it, a number
picks from the list, typed text sets a value. A wrong answer is asked again. At the end it prints a summary
and asks `Write it? [y/N]`. It writes the directory (mode 700), the binary copy, .env (mode 600), deploy.conf,
the registry file and the unit. IT STARTS NOTHING: the next step is `scripts/deploy.sh run <instance>`.

Steps (flag in brackets)                              Default
   1 instance name                                    the argument, else main
   2 directory                    (--dir <path>)      macOS ~/Library/Application Support/whatsrust/<instance>,
                                                      Linux ${XDG_DATA_HOME:-~/.local/share}/whatsrust/<instance>
   3 daemon binary                (--binary <path>)   <repo>/target/release/whatsrust (its SHA-256 is shown)
   4 REST port                    (--port <n>)        the first free port from 7280 in steps of 10
   5 REST API token               (--api-token generate|none|keep)   generate (none = loopback only)
   6 MCP HTTP listener            (--mcp on|off, --mcp-port <n>)     off; on: port = REST port + 1, a token
   7 semantic search              (--semantic on|off, --model bge-m3|minilm, --embedder <path>)
                                                      off; on: bge-m3, <repo>/embedder-rs/target/release/whatsrust-embedder
   8 device name                  (--device-name <n>) whatsrust (fixed at pairing; a paired instance needs re-pair)
   9 store the pairing history    (--pairing-history on|off)         on
  10 autostart at login           (--autostart on|off)               off (applied only for a paired instance)

Options:
  --yes             accept the default of every step without a flag, and skip the confirmation
  --dry-run         show the summary, the rendered unit and the steps; write nothing
  --platform <p>    launchd or systemd (without --dry-run it must match the host)
  Prompts read stdin, so answers can be piped. If stdin ends before an answer: exit 2
  "no answer: use the flags and --yes".

Re-install on an existing instance: the wizard starts with the current values (read from .env and
deploy.conf). It changes only the keys it manages in .env (WHATSRUST_PORT, WHATSRUST_API_TOKEN,
WHATSRUST_MCP_PORT, WHATSRUST_MCP_TOKEN, WHATSRUST_EMBEDDER_CMD, WHATSRUST_EMBEDDER_ARGS,
WHATSRUST_DEVICE_NAME, WHATSRUST_STORE_PAIRING_HISTORY) and keeps every other line. A switched-off feature
comments its keys out. An installed binary stays (replace it with `upgrade`). If the service ran, it
restarts gracefully. The directory cannot change: uninstall first (the data stays), then install again.
Tokens come from `openssl rand -base64 32`; they are never printed and never passed in a command line.

Directory rules: absolute; missing, empty, or marked as made by this script (.whatsrust-deploy); judged by
its physical path (symbolic links resolved): not /, not $HOME or above it, not inside the repo or above it;
no control characters, " \ $ ` @ and no '.' or '..' part.
EOF
}

# die <message>: print it to stderr (a closed stderr is no reason to stop earlier) and exit 1.
die() {
  printf 'deploy: %s\n' "$*" >&2 || true
  exit 1
}

usage_err() {
  printf 'deploy: %s\nTry: scripts/deploy.sh --help\n' "$*" >&2 || true
  exit 2
}

note() {
  printf 'deploy: %s\n' "$*" >&2 || true
}

# deadline <seconds>: set DEADLINE to the $SECONDS value after that many seconds (divided by
# DEPLOY_WAIT_SCALE, 1 s at least). A plain assignment, not a $(...) inside $((...)): bash 3.2 mishandles
# that inside a trap handler.
deadline() {
  local n=$(($1 / WAIT_SCALE))
  if [ "$n" -lt 1 ]; then n=1; fi
  DEADLINE=$((SECONDS + n))
}

# in_list <word> <space-separated words>
in_list() {
  case " $2 " in *" $1 "*) return 0 ;; esac
  return 1
}

# field <name> <value>: one aligned "  name: value" line.
field() {
  printf '  %-16s %s\n' "$1:" "$2"
}

# trim <text>: the text without leading and trailing whitespace (bash builtins only: it may be a token).
trim() {
  local v=$1
  v=${v#"${v%%[![:space:]]*}"}
  v=${v%"${v##*[![:space:]]}"}
  printf '%s\n' "$v"
}

# lower <text>: the text in lower case (tr: bash 3.2 has no ${v,,}).
lower() {
  printf '%s\n' "$1" | tr '[:upper:]' '[:lower:]'
}

# run <command> [args...]: run the command; with --dry-run only print it. Returns the command's status.
run() {
  if [ "$DRY" -eq 1 ]; then
    printf '  would run:'
    printf ' %q' "$@"
    printf '\n'
    return 0
  fi
  "$@"
}

# will <what>: true when the step must really happen. With --dry-run it prints what would happen
# and returns false.
will() {
  if [ "$DRY" -eq 1 ]; then
    printf '  would %s\n' "$1"
    return 1
  fi
  return 0
}

# sha256_of <file>: the SHA-256 of the file, hex only (sha256sum, else shasum).
sha256_of() {
  local out
  if command -v sha256sum >/dev/null 2>&1; then
    out=$(sha256sum -- "$1")
  elif command -v shasum >/dev/null 2>&1; then
    out=$(shasum -a 256 -- "$1")
  else
    die "sha256sum or shasum not found (needed for the binary checksum)"
  fi
  printf '%s\n' "${out%% *}"
}

# cleanup: runs on every exit. If this process still has a service running for an unpaired QR flow (an error,
# a set -e or set -u exit, a signal that the flow did not handle), it stops that service first: an unpaired
# instance must not outlive the terminal that shows its QR code. Then it removes the temp file of an
# unfinished .env edit and the QR flow marker. (A SIGKILL or a power loss cannot be trapped.)
cleanup() {
  set +e
  if [ "$QR_SVC" -eq 1 ]; then
    QR_SVC=0
    trap '' INT TERM HUP PIPE QUIT
    if ( stop_service ); then
      echo "stopped the service: nothing unpaired is left running" 2>/dev/null || true
    else
      note "WARNING: could not stop the service of the unfinished QR flow. Stop it with: scripts/deploy.sh stop $INSTANCE"
    fi
  fi
  if [ -n "$ENV_TMP" ]; then rm -f -- "$ENV_TMP" || true; fi
  qr_mark_clear
}

# valid_port <text>: true for a TCP port number, 1-65535 (digits only).
valid_port() {
  case $1 in '' | *[!0-9]*) return 1 ;; esac
  [ "${#1}" -le 5 ] && [ "$((10#$1))" -ge 1 ] && [ "$((10#$1))" -le 65535 ]
}

# enum_opt <command> <option> <value> <allowed words>: usage error unless the value is one of the words.
enum_opt() {
  in_list "$3" "$4" || usage_err "$1: $2 must be one of: $4 (not '$3')"
}

# set_option <command> <option> <value>: store the value of one option that takes a value.
set_option() {
  local cmd=$1 opt=$2 val=$3
  case $opt in
    --dir) OPT_DIR=$val ;;
    --binary) OPT_BINARY=$val ;;
    --platform) OPT_PLATFORM=$val ;;
    -n) OPT_LINES=$val ;;
    --embedder) OPT_EMBEDDER=$val ;;
    --device-name) OPT_DEVICE=$val ;;
    --backup) OPT_BACKUP=$val ;;
    --confirm) OPT_CONFIRM=$val ;;
    --port | --mcp-port)
      valid_port "$val" || usage_err "$cmd: $opt needs a port number from 1 to 65535, not '$val'"
      if [ "$opt" = --port ]; then OPT_PORT=$((10#$val)); else OPT_MCP_PORT=$((10#$val)); fi
      ;;
    --api-token)
      enum_opt "$cmd" "$opt" "$val" "generate none keep"
      OPT_API=$val
      ;;
    --mcp)
      enum_opt "$cmd" "$opt" "$val" "on off"
      OPT_MCP=$val
      ;;
    --semantic)
      enum_opt "$cmd" "$opt" "$val" "on off"
      OPT_SEMANTIC=$val
      ;;
    --model)
      enum_opt "$cmd" "$opt" "$val" "bge-m3 minilm"
      OPT_MODEL=$val
      ;;
    --pairing-history)
      enum_opt "$cmd" "$opt" "$val" "on off"
      OPT_PAIRHIST=$val
      ;;
    --autostart)
      enum_opt "$cmd" "$opt" "$val" "on off"
      OPT_AUTOSTART=$val
      ;;
    --mode)
      enum_opt "$cmd" "$opt" "$val" "fresh keep-history"
      OPT_MODE=$val
      ;;
  esac
}

# parse_args <command> <allowed options, space separated> [args...]: fill INSTANCE, POS2, OPT_*, DRY,
# YES and FOLLOW from the arguments; --help prints the command's help and exits 0.
parse_args() {
  local cmd=$1 allowed=$2 arg
  shift 2
  while [ $# -gt 0 ]; do
    arg=$1
    case $arg in
      -h | --help)
        usage_command "$cmd"
        exit 0
        ;;
      -*) in_list "$arg" "$allowed" || usage_err "$cmd: unknown option: $arg" ;;
    esac
    case $arg in
      --dry-run) DRY=1 ;;
      --follow) FOLLOW=1 ;;
      --yes) YES=1 ;;
      --keep-data) OPT_KEEP_DATA=1 ;;
      --delete-data) OPT_DELETE_DATA=1 ;;
      --no-backup) OPT_NO_BACKUP=1 ;;
      -*)
        [ $# -ge 2 ] || usage_err "$cmd: $arg needs a value"
        set_option "$cmd" "$arg" "$2"
        shift
        ;;
      *)
        if [ -z "$INSTANCE" ]; then
          INSTANCE=$arg
        elif [ "$cmd" = autostart ] && [ -z "$POS2" ]; then
          POS2=$arg
        else
          usage_err "$cmd: unexpected argument: $arg"
        fi
        ;;
    esac
    shift
  done
  if [ -z "$INSTANCE" ] && [ "$cmd" != install ]; then usage_err "$cmd: missing <instance>"; fi
  if [ -n "$INSTANCE" ]; then check_instance_name "$INSTANCE"; fi
  case $OPT_PLATFORM in
    '' | launchd | systemd) ;;
    *) usage_err "$cmd: --platform must be launchd or systemd, not '$OPT_PLATFORM'" ;;
  esac
  case $OPT_LINES in
    '' | 0 | *[!0-9]*) usage_err "$cmd: -n needs a positive number, not '$OPT_LINES'" ;;
  esac
  if [ "$OPT_KEEP_DATA$OPT_DELETE_DATA" = 11 ]; then usage_err "$cmd: --keep-data and --delete-data exclude each other"; fi
  if [ -n "$OPT_BACKUP" ] && [ "$OPT_NO_BACKUP" -eq 1 ]; then usage_err "$cmd: --backup and --no-backup exclude each other"; fi
  if [ "$OPT_KEEP_DATA" -eq 1 ] && { [ -n "$OPT_BACKUP" ] || [ "$OPT_NO_BACKUP" -eq 1 ] || [ -n "$OPT_CONFIRM" ]; }; then
    usage_err "$cmd: --keep-data cannot be combined with --backup, --no-backup or --confirm"
  fi
}

# check_instance_name <name>: ^[a-z0-9][a-z0-9-]{0,31}$ (the characters are spelled out so that no
# locale can widen a range).
check_instance_name() {
  case $1 in
    '' | [!abcdefghijklmnopqrstuvwxyz0123456789]* | *[!abcdefghijklmnopqrstuvwxyz0123456789-]*)
      usage_err "invalid instance name '$1' (lower-case letters, digits and '-'; start with a letter or digit)"
      ;;
  esac
  [ "${#1}" -le 32 ] || usage_err "invalid instance name '$1' (at most 32 characters)"
}

# name_problem <name>: why the name is not a valid instance name (nothing printed when it is fine).
name_problem() {
  case $1 in
    '' | [!abcdefghijklmnopqrstuvwxyz0123456789]* | *[!abcdefghijklmnopqrstuvwxyz0123456789-]*)
      printf "use lower-case letters, digits and '-', starting with a letter or digit\n"
      return 0
      ;;
  esac
  if [ "${#1}" -gt 32 ]; then printf 'at most 32 characters\n'; fi
}

# set_platform: set HOST and PLATFORM from --platform, else from the host (uname -s). Without --dry-run
# the platform must be the host's, and its supervisor tool must exist.
set_platform() {
  case ${HOME:-} in /*) ;; *) die "HOME is not set to an absolute path" ;; esac
  HOST=""
  case $(uname -s) in
    Darwin) HOST=launchd ;;
    Linux) HOST=systemd ;;
  esac
  PLATFORM=${OPT_PLATFORM:-$HOST}
  [ -n "$PLATFORM" ] || die "unsupported OS $(uname -s): launchd (macOS) and systemd (Linux) only; --dry-run --platform <p> still renders a unit"
  if [ "$DRY" -eq 1 ]; then
    return 0
  fi
  [ "$PLATFORM" = "$HOST" ] ||
    die "--platform $PLATFORM does not match this host ($(uname -s)): add --dry-run to only render it"
  case $PLATFORM in
    launchd) command -v launchctl >/dev/null 2>&1 || die "launchctl not found" ;;
    systemd) command -v systemctl >/dev/null 2>&1 || die "systemctl not found" ;;
  esac
}

# xdg_dir <value> <fallback>: an XDG base directory is used only when it is an absolute path.
xdg_dir() {
  case $1 in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s\n' "$2" ;;
  esac
}

# set_names: set the names that depend on INSTANCE: label, unit name, user id.
set_names() {
  LABEL="local.whatsrust.$INSTANCE"
  UNIT="whatsrust-$INSTANCE.service"
  UID_NUM=$(id -u)
}

# set_paths: set the unit file paths (needs DIR for launchd).
set_paths() {
  case $PLATFORM in
    launchd)
      PLIST_FILE="$DIR/$LABEL.plist"
      AGENT_FILE="$HOME/Library/LaunchAgents/$LABEL.plist"
      ;;
    systemd) UNIT_FILE="$(xdg_dir "${XDG_CONFIG_HOME:-}" "$HOME/.config")/systemd/user/$UNIT" ;;
  esac
}

# default_dir: the instance directory when the wizard has no other.
default_dir() {
  case $PLATFORM in
    launchd) printf '%s\n' "$HOME/Library/Application Support/whatsrust/$INSTANCE" ;;
    systemd) printf '%s\n' "$(xdg_dir "${XDG_DATA_HOME:-}" "$HOME/.local/share")/whatsrust/$INSTANCE" ;;
  esac
}

# registry_dir: where install records the directory of each instance.
registry_dir() {
  printf '%s\n' "$(xdg_dir "${XDG_CONFIG_HOME:-}" "$HOME/.config")/whatsrust/deploy"
}

# registry_file <instance>: the one-line file that holds the instance directory.
registry_file() {
  printf '%s/%s.dir\n' "$(registry_dir)" "$1"
}

# registry_get <instance>: print the directory registered for the instance (nonzero when none).
registry_get() {
  local f d=""
  f=$(registry_file "$1")
  [ -f "$f" ] || return 1
  IFS= read -r d <"$f" || true
  case $d in /*) printf '%s\n' "$d" ;; *) return 1 ;; esac
}

# registry_put: record DIR for INSTANCE (temp file + rename, mode 600).
registry_put() {
  local f tmp
  f=$(registry_file "$INSTANCE")
  (umask 077 && mkdir -p -- "${f%/*}") || die "could not create ${f%/*}"
  tmp="$f.new"
  (umask 077 && printf '%s\n' "$DIR" >"$tmp") || die "could not write $tmp"
  mv -f -- "$tmp" "$f" || die "could not write $f"
}

# begin [nodir]: for every command except install: set the platform, the names and DIR (from the
# registry); a clear error when the instance is not installed. The directory must exist, unless "nodir".
begin() {
  set_platform
  set_names
  DIR=$(registry_get "$INSTANCE") ||
    die "instance '$INSTANCE' is not installed (no registry file $(registry_file "$INSTANCE")); install it with: scripts/deploy.sh install $INSTANCE"
  set_paths
  [ "${1:-}" = nodir ] || [ -d "$DIR" ] || die "the directory of instance '$INSTANCE' is gone: $DIR (data deleted by hand? install it again, or remove $(registry_file "$INSTANCE"))"
  require_units_ours
}

# unit_ours <file>: true when the file is a unit that this script wrote for INSTANCE: the managed-by marker
# (plist: an XML comment plus the Label; systemd: a comment line that names the instance).
unit_ours() {
  [ -f "$1" ] || return 1
  case $PLATFORM in
    launchd) grep -q 'managed-by: scripts/deploy.sh' "$1" && grep -qF "<string>$LABEL</string>" "$1" ;;
    systemd) grep -qxF "# managed-by: scripts/deploy.sh instance=$INSTANCE" "$1" ;;
  esac
}

# require_units_ours: a unit file (or LaunchAgents copy) that exists must be ours. The script never touches,
# overwrites, removes, stops or boots out a unit that lacks the marker.
require_units_ours() {
  local f
  for f in "$PLIST_FILE" "$AGENT_FILE" "$UNIT_FILE"; do
    [ -n "$f" ] || continue
    if [ -L "$f" ]; then
      die "refusing to continue: $f is a symbolic link (the script writes plain files there, and never follows or replaces a link). Move it away yourself if it is yours."
    fi
    if [ -e "$f" ] && ! unit_ours "$f"; then
      die "refusing to continue: $f exists but was not written by scripts/deploy.sh for instance '$INSTANCE' (no managed-by marker). Move it away yourself if it is yours to remove."
    fi
  done
}

# supervisor_reachable: true when the user's launchd domain / systemd user instance answers. A job that is
# not loaded is no error; a supervisor that cannot be asked is.
supervisor_reachable() {
  case $PLATFORM in
    launchd) launchctl print "gui/$UID_NUM" >/dev/null 2>&1 ;;
    systemd) systemctl --user show -p LoadState "$UNIT" >/dev/null 2>&1 ;;
  esac
}

# require_supervisor: stop with a clear message, before anything is changed, when the supervisor cannot be
# asked (no login session, no systemd user instance). "Not loaded" is an answer; "cannot ask" is not.
require_supervisor() {
  if [ "$DRY" -eq 1 ]; then return 0; fi
  if supervisor_reachable; then return 0; fi
  case $PLATFORM in
    launchd) die "cannot reach your launchd user domain (gui/$UID_NUM): run this in your own macOS login session (not over a plain ssh login). Nothing was changed." ;;
    systemd) die "cannot reach the systemd user instance: run this in your own login session (see: loginctl enable-linger \"\$USER\" for services without a session). Nothing was changed." ;;
  esac
}

# unit_file: the file that defines the service: the plist in the instance directory (launchd) or the unit.
unit_file() {
  case $PLATFORM in
    launchd) printf '%s\n' "$PLIST_FILE" ;;
    systemd) printf '%s\n' "$UNIT_FILE" ;;
  esac
}

# require_unit: the unit file must exist (install writes it).
require_unit() {
  [ -f "$(unit_file)" ] ||
    die "the unit file of '$INSTANCE' is missing: $(unit_file); write it again with: scripts/deploy.sh install $INSTANCE"
}

# norm_dir <path>: the path without trailing slashes.
norm_dir() {
  local d=$1
  while [ "${#d}" -gt 1 ] && [ "${d%/}" != "$d" ]; do
    d=${d%/}
  done
  printf '%s\n' "$d"
}

# phys_path <path>: the physical path (symbolic links resolved, as pwd -P does). The part of the path that
# does not exist yet is appended as it is. Nonzero when no ancestor can be entered.
phys_path() {
  local p=$1 rest="" out
  while [ ! -d "$p" ]; do
    case $p in */*) ;; *) return 1 ;; esac
    rest="/${p##*/}$rest"
    p=${p%/*}
    if [ -z "$p" ]; then p=/; fi
  done
  out=$(cd "$p" 2>/dev/null && pwd -P) || return 1
  if [ "$out" = / ]; then out=""; fi
  out="$out$rest"
  printf '%s\n' "${out:-/}"
}

# pkey <path>: the string that paths are compared by. macOS volumes (APFS) are usually case-insensitive, so on
# the launchd platform the physical path is case-folded; elsewhere the comparison is exact.
pkey() {
  if [ "$PLATFORM" = launchd ]; then lower "$1"; else printf '%s\n' "$1"; fi
}

# path_in <path> <base>: true when path is base itself or below it (compared by pkey).
path_in() {
  local p b
  p=$(pkey "$1")
  b=$(pkey "$2")
  if [ "$b" = / ]; then return 0; fi
  [ "$p" = "$b" ] || [[ $p == "$b"/* ]]
}

# dir_problem <path>: why the path cannot be an instance directory (nothing printed when it is fine):
# absolute, free of the characters no unit file or template can carry safely (spaces are fine), and, by its
# PHYSICAL path, outside the repo and neither $HOME nor an ancestor of the repo or of $HOME.
dir_problem() {
  local d=$1 pd ph why=""
  case $d in
    /) why='must not be /' ;;
    /*)
      case $d in
        *[[:cntrl:]]*) why='has a control character or a newline' ;;
        *'"'* | *\\* | *'$'* | *'`'*) why='has one of the characters " \ $ or a backtick' ;;
        *'@'*) why='has the character @ (the templates use it as a placeholder)' ;;
        *' ') why='ends with a space' ;;
        */. | */./* | */.. | */../*) why="has a '.' or '..' part" ;;
      esac
      ;;
    *) why='must be an absolute path' ;;
  esac
  if [ -n "$why" ]; then
    printf '%s\n' "$why"
    return 0
  fi
  pd=$(phys_path "$d") || {
    printf 'cannot be resolved (no parent directory can be entered)\n'
    return 0
  }
  ph=$(cd "$HOME" 2>/dev/null && pwd -P) || ph=$HOME
  if [ "$pd" != "$d" ]; then why=" (it resolves to $pd)"; fi
  if [ "$pd" = / ]; then
    printf 'must not be /%s\n' "$why"
  elif path_in "$pd" "$ROOT"; then
    printf 'is inside the repo checkout (%s); keep the state outside it%s\n' "$ROOT" "$why"
  elif [ "$(pkey "$pd")" = "$(pkey "$ph")" ]; then
    printf 'is your home directory%s\n' "$why"
  elif path_in "$ROOT" "$pd"; then
    printf 'contains the repo checkout%s\n' "$why"
  elif path_in "$ph" "$pd"; then
    printf 'contains your home directory%s\n' "$why"
  fi
}

# dir_overlap <physical dir>: print why the directory overlaps the directory of ANOTHER registered instance
# (equal, below it, or above it); nothing when it does not. So no instance can hold, back up or delete the
# data of another one.
dir_overlap() {
  local f name d pd
  for f in "$(registry_dir)"/*.dir; do
    [ -f "$f" ] || continue
    name=${f##*/}
    name=${name%.dir}
    [ "$name" != "$INSTANCE" ] || continue
    d=$(registry_get "$name") || continue
    pd=$(phys_path "$d") || pd=$d
    if path_in "$1" "$pd"; then
      printf "is, or is inside, the directory of instance '%s' (%s)\n" "$name" "$pd"
      return 0
    elif path_in "$pd" "$1"; then
      printf "contains the directory of instance '%s' (%s)\n" "$name" "$pd"
      return 0
    fi
  done
}

MARKER_NAME=".whatsrust-deploy"

# marker_instance <dir>: print the instance named in <dir>/.whatsrust-deploy (the ownership marker that install
# writes); nonzero when the file is missing or not a marker.
marker_instance() {
  local line=""
  [ -f "$1/$MARKER_NAME" ] || return 1
  IFS= read -r line <"$1/$MARKER_NAME" || true
  case $line in
    "managed by scripts/deploy.sh; instance="?*) printf '%s\n' "${line#*instance=}" ;;
    *) return 1 ;;
  esac
}

# conf_load: set CONF_AUTOSTART, CONF_BINARY, CONF_EMBEDDER from <dir>/deploy.conf (KEY=value lines, never
# sourced). Missing file or key: off / empty.
CONF_AUTOSTART="off"
CONF_BINARY=""
CONF_EMBEDDER=""
conf_load() {
  local f="$DIR/deploy.conf" line
  CONF_AUTOSTART=off
  CONF_BINARY=""
  CONF_EMBEDDER=""
  [ -f "$f" ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in
      AUTOSTART=on) CONF_AUTOSTART=on ;;
      BINARY_SRC=*) CONF_BINARY=${line#BINARY_SRC=} ;;
      EMBEDDER_SRC=*) CONF_EMBEDDER=${line#EMBEDDER_SRC=} ;;
    esac
  done <"$f"
}

# conf_save: write deploy.conf from CONF_* (mode 600, temp file + rename).
conf_save() {
  local f="$DIR/deploy.conf" tmp
  tmp=$(umask 077 && mktemp "$DIR/.deploy.conf.XXXXXX") || die "could not create a temp file in $DIR"
  {
    printf '%s\n' "# whatsrust deploy choices of instance $INSTANCE (scripts/deploy.sh). Not read by the daemon."
    printf 'AUTOSTART=%s\n' "$CONF_AUTOSTART"
    if [ -n "$CONF_BINARY" ]; then printf 'BINARY_SRC=%s\n' "$CONF_BINARY"; fi
    if [ -n "$CONF_EMBEDDER" ]; then printf 'EMBEDDER_SRC=%s\n' "$CONF_EMBEDDER"; fi
  } >"$tmp" || die "could not write $tmp"
  chmod 600 "$tmp" || die "could not set the mode of $tmp"
  mv -f -- "$tmp" "$f" || die "could not write $f"
}

# xml_escape <text>: the text for an XML string (& first, so that the others are not escaped twice).
xml_escape() {
  local s=$1
  s=${s//&/&amp;}
  s=${s//</&lt;}
  s=${s//>/&gt;}
  s=${s//\"/&quot;}
  s=${s//\'/&apos;}
  printf '%s\n' "$s"
}

# systemd_escape <text>: the text for a unit file value. systemd expands %-specifiers there.
systemd_escape() {
  local s=$1
  printf '%s\n' "${s//\%/%%}"
}

# render_unit: print the platform's template with the placeholders @LABEL@ @UNIT@ @INSTANCE@ @DIR@
# @START@ @ENV_FILE@ @LOG@ replaced (values escaped for the file format; plain bash, no sed).
render_unit() {
  local tpl esc line v_label v_unit v_instance v_dir v_start v_env v_log
  case $PLATFORM in
    launchd)
      tpl="$TEMPLATES/whatsrust.plist.in"
      esc=xml_escape
      ;;
    systemd)
      tpl="$TEMPLATES/whatsrust.service.in"
      esc=systemd_escape
      ;;
  esac
  [ -f "$tpl" ] || die "missing template: $tpl"
  v_label=$("$esc" "$LABEL")
  v_unit=$("$esc" "$UNIT")
  v_instance=$("$esc" "$INSTANCE")
  v_dir=$("$esc" "$DIR")
  v_start=$("$esc" "$DIR/bin/start-if-paired")
  v_env=$("$esc" "$DIR/.env")
  v_log=$("$esc" "$DIR/logs/daemon.log")
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line//@LABEL@/$v_label}
    line=${line//@UNIT@/$v_unit}
    line=${line//@INSTANCE@/$v_instance}
    line=${line//@DIR@/$v_dir}
    line=${line//@START@/$v_start}
    line=${line//@ENV_FILE@/$v_env}
    line=${line//@LOG@/$v_log}
    printf '%s\n' "$line"
  done <"$tpl"
}

# render_gate: print the start gate (deploy/supervisor/start-if-paired.sh) with @INSTANCE@ replaced.
render_gate() {
  local tpl="$TEMPLATES/start-if-paired.sh" line
  [ -f "$tpl" ] || die "missing template: $tpl"
  while IFS= read -r line || [ -n "$line" ]; do
    printf '%s\n' "${line//@INSTANCE@/$INSTANCE}"
  done <"$tpl"
}

# write_file_atomic <dest> <mode> <content>: write the content to dest.new, set the mode, rename.
write_file_atomic() {
  local dest=$1 mode=$2 tmp="$1.new"
  mkdir -p -- "${dest%/*}" || die "could not create ${dest%/*}"
  (umask 077 && printf '%s\n' "$3" >"$tmp") || die "could not write $tmp"
  chmod "$mode" "$tmp" || die "could not set the mode of $tmp"
  mv -f -- "$tmp" "$dest" || die "could not write $dest"
}

# check_gate_template: render the start gate once, before anything is written, so a missing or empty template
# stops install and upgrade while the instance (and its old gate) is still untouched.
check_gate_template() {
  local gate
  gate=$(render_gate) || die "could not render the start gate"
  [ -n "$gate" ] || die "the start gate template is empty: $TEMPLATES/start-if-paired.sh (nothing was changed)"
}

# place_gate: install the rendered start gate as <dir>/bin/start-if-paired (mode 755).
place_gate() {
  local gate
  gate=$(render_gate) || die "could not render the start gate"
  [ -n "$gate" ] || die "the start gate template is empty: $TEMPLATES/start-if-paired.sh (the installed gate is untouched)"
  write_file_atomic "$DIR/bin/start-if-paired" 755 "$gate"
}

# write_unit <dest> <content>: write a rendered unit (a plist is checked with plutil first).
write_unit() {
  local dest=$1 tmp="$1.new" lint
  mkdir -p -- "${dest%/*}" || die "could not create ${dest%/*}"
  printf '%s\n' "$2" >"$tmp" || die "could not write $tmp"
  chmod 644 "$tmp" || die "could not set the mode of $tmp"
  if [ "$PLATFORM" = launchd ] && command -v plutil >/dev/null 2>&1; then
    if ! lint=$(plutil -lint "$tmp" 2>&1); then
      rm -f -- "$tmp"
      die "the rendered plist is not valid, nothing was loaded: $lint"
    fi
  fi
  mv -f -- "$tmp" "$dest" || die "could not write $dest"
}

# place_binary <source> <dest>: copy the executable atomically (copy to dest.new, then rename; a running
# daemon keeps the old file until it restarts).
place_binary() {
  local tmp="$2.new"
  cp -- "$1" "$tmp" || {
    rm -f -- "$tmp"
    die "could not copy $1 to $tmp"
  }
  chmod 755 "$tmp" || die "could not set the mode of $tmp"
  mv -f -- "$tmp" "$2" || die "could not write $2"
}

# binary_problem <path>: why the file is not usable as a binary to copy (nothing printed when it is fine).
binary_problem() {
  case $1 in
    '') printf 'no path\n' ;;
    *[[:cntrl:]]*) printf 'has a control character\n' ;;
    /*)
      if [ ! -e "$1" ]; then
        printf 'not found\n'
      elif [ ! -f "$1" ] || [ ! -x "$1" ]; then
        printf 'not an executable file\n'
      fi
      ;;
    *) printf 'must be an absolute path\n' ;;
  esac
}

# env_get <env-file> <KEY>: value of the first active "KEY=value" line (optional "export ", surrounding
# quotes or a trailing " # comment" removed); empty when absent. Only the requested key is parsed and the
# file is never sourced. The first occurrence wins, like dotenvy in the daemon.
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

# The .env keys that install manages, as active ("KEY=...") and commented ("# KEY=...") lines.
ENV_KEYS="WHATSRUST_PORT|WHATSRUST_API_TOKEN|WHATSRUST_MCP_PORT|WHATSRUST_MCP_TOKEN|WHATSRUST_EMBEDDER_CMD|WHATSRUST_EMBEDDER_ARGS|WHATSRUST_DEVICE_NAME|WHATSRUST_STORE_PAIRING_HISTORY"
KEY_ACTIVE="^[[:space:]]*(export[[:space:]]+)?($ENV_KEYS)[[:space:]]*="
KEY_COMMENTED="^#[[:space:]]*(export[[:space:]]+)?($ENV_KEYS)[[:space:]]*="
LK_STATE="" # set by env_key_line: active, commented or other
LK_KEY=""   # set by env_key_line: the key of an active or commented line

# env_key_line <line>: classify a .env line (LK_STATE, LK_KEY).
env_key_line() {
  LK_STATE=other
  LK_KEY=""
  if [[ $1 =~ $KEY_ACTIVE ]]; then
    LK_STATE=active
    LK_KEY=${BASH_REMATCH[2]}
  elif [[ $1 =~ $KEY_COMMENTED ]]; then
    LK_STATE=commented
    LK_KEY=${BASH_REMATCH[2]}
  fi
}

# env_edit <env-file> <spec>...: rewrite the .env with bash builtins only, so a token in a spec never
# reaches the argument list of a command. A spec is KEY=VALUE or -KEY (KEY: one of the managed keys).
#   KEY=VALUE  the first active KEY line becomes KEY=VALUE; when there is none, the first commented
#              "# KEY=..." line does; with neither, KEY=VALUE is appended. Further active KEY lines
#              are commented out (the daemon would use only the first one anyway).
#   -KEY       every active KEY line is commented out.
# Every other line stays as it is. The result is written next to the original as a mode-600 temp file
# and moved over it.
env_edit() {
  local file=$1 line out spec key value done_keys=" " active_keys=" "
  shift
  while IFS= read -r line || [ -n "$line" ]; do
    env_key_line "$line"
    if [ "$LK_STATE" = active ]; then active_keys="$active_keys$LK_KEY "; fi
  done <"$file"

  ENV_TMP=$(mktemp "${file%/*}/.env.XXXXXX") || die "could not create a temp file next to $file"
  {
    while IFS= read -r line || [ -n "$line" ]; do
      env_key_line "$line"
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
  chmod 600 "$ENV_TMP" || die "could not set the mode of $ENV_TMP"
  mv -f -- "$ENV_TMP" "$file" || die "could not write $file"
  ENV_TMP=""
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
    printf 'is the same as WHATSRUST_API_TOKEN (the daemon refuses that)\n'
  elif [ "${3:-}" = strict ]; then
    case $t in
      *[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=_.~-]*)
        printf 'has characters that do not survive in an unquoted .env value (allowed: A-Z a-z 0-9 + / = _ . ~ -)\n'
        ;;
    esac
  fi
}

# gen_token: print a new token (openssl rand -base64 32, as scripts/run-profile.sh does).
gen_token() {
  local t reason
  command -v openssl >/dev/null 2>&1 ||
    die "openssl not found, it generates the tokens (install: brew install openssl, or apt install openssl)"
  t=$(openssl rand -base64 32) || die "openssl could not generate a token"
  reason=$(token_problem "$t" "" strict)
  [ -z "$reason" ] || die "openssl produced a token that $reason"
  printf '%s\n' "$t"
}

# device_problem <name>: why the name cannot go into the .env as WHATSRUST_DEVICE_NAME (nothing printed
# when it is fine).
device_problem() {
  local t
  t=$(trim "$1")
  case $t in
    '') printf 'is empty\n' ;;
    *[[:cntrl:]]*) printf 'has a control character\n' ;;
    *'"'* | *\\* | *'$'* | *'`'* | *'#'* | *"'"*) printf "has one of the characters \" ' \\ \$ # or a backtick\n" ;;
    *)
      if [ "${#t}" -gt 64 ]; then printf 'is longer than 64 characters\n'; fi
      ;;
  esac
}

# port_listening <port>: true when something listens on the TCP port (macOS lsof, Linux ss). When the tool
# is missing the answer is "no", and a note says so once.
port_listening() {
  local out
  case $(uname -s) in
    Darwin)
      if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1
        return
      fi
      ;;
    Linux)
      if command -v ss >/dev/null 2>&1; then
        out=$(ss -ltnH "sport = :$1" 2>/dev/null) || out=""
        [ -n "$out" ]
        return
      fi
      ;;
  esac
  if [ "$LISTEN_NOTE" -eq 0 ]; then
    note "cannot check which ports are in use (no lsof / ss): ports are checked against other instances only"
    LISTEN_NOTE=1
  fi
  return 1
}

# other_port_owner <port>: print the instance (not INSTANCE) whose .env uses the port; nonzero when none.
other_port_owner() {
  local f name d key p
  for f in "$(registry_dir)"/*.dir; do
    [ -f "$f" ] || continue
    name=${f##*/}
    name=${name%.dir}
    [ "$name" != "$INSTANCE" ] || continue
    d=$(registry_get "$name") || continue
    for key in WHATSRUST_PORT WHATSRUST_MCP_PORT; do
      p=$(env_get "$d/.env" "$key")
      if [ "$p" = "$1" ]; then
        printf '%s\n' "$name"
        return 0
      fi
    done
  done
  return 1
}

# port_problem <port> <own> <other>: why the instance cannot use the port (nothing printed when it is
# fine). own = the port its .env has now (it may listen: this is the instance itself); other = a port it
# must differ from (the REST port, for the MCP port).
port_problem() {
  local p=$1 who
  valid_port "$p" || {
    printf 'not a port number from 1 to 65535\n'
    return 0
  }
  p=$((10#$p))
  if [ "$p" = "${3:-}" ]; then
    printf 'the REST port itself (the daemon refuses the same port twice)\n'
  elif who=$(other_port_owner "$p"); then
    printf "used by instance '%s'\n" "$who"
  elif [ "$p" != "${2:-}" ] && port_listening "$p"; then
    printf 'something listens on it\n'
  fi
}

PICKED_PORT="" # set by pick_free_port
PORT_NOTES=""  # set by pick_free_port: the ports it skipped, with the reason

# pick_free_port <start> <step> <own> <other>: set PICKED_PORT to the first port from start (in steps)
# that port_problem accepts; PORT_NOTES lists the ones it skipped.
pick_free_port() {
  local p=$1 why
  PORT_NOTES=""
  while :; do
    [ "$p" -le 65535 ] || die "no free port found from $1 upward"
    why=$(port_problem "$p" "$3" "${4:-}")
    if [ -z "$why" ]; then
      PICKED_PORT=$p
      return 0
    fi
    PORT_NOTES="$PORT_NOTES${PORT_NOTES:+, }$p ($why)"
    p=$((p + $2))
  done
}

# rest_port: the REST port of the instance (WHATSRUST_PORT in its .env, else the daemon's default 7270).
rest_port() {
  local v
  v=$(env_get "$DIR/.env" WHATSRUST_PORT)
  printf '%s\n' "${v:-7270}"
}

# paired_state: yes / no / unknown. Paired = the device row has a phone number (src/storage.rs
# is_paired). No DB file: no. No sqlite3: unknown.
paired_state() {
  local db="$DIR/whatsapp.db" out rc=0
  [ -e "$db" ] || {
    echo no
    return 0
  }
  command -v sqlite3 >/dev/null 2>&1 || {
    echo unknown
    return 0
  }
  # -init /dev/null: a ~/.sqliterc (for example ".headers on") must not change the output. stdout only.
  out=$(sqlite3 -batch -init /dev/null -readonly -cmd '.timeout 1000' "$db" "SELECT pn FROM device WHERE id = 1;" 2>/dev/null) || rc=$?
  if [ "$rc" -ne 0 ]; then
    out=$(sqlite3 -batch -init /dev/null -readonly -cmd '.timeout 1000' "$db" "SELECT pn FROM device WHERE id = 1;" 2>&1 >/dev/null) || true
    case $out in
      *"no such table"*) echo no ;;
      *) echo unknown ;;
    esac
    return 0
  fi
  out=$(trim "$out")
  if [ -n "$out" ]; then echo yes; else echo no; fi
}

# client_cli <args...>: run the installed binary as a CLI client against this instance. The CLI does not
# read .env, so the port, the bind address and the token go in as environment variables (never argv).
client_cli() {
  (
    unset WHATSRUST_PORT WHATSRUST_BIND WHATSRUST_API_TOKEN HEALTH_PORT
    local key v
    for key in WHATSRUST_PORT WHATSRUST_BIND WHATSRUST_API_TOKEN; do
      v=$(env_get "$DIR/.env" "$key")
      if [ -n "$v" ]; then export "$key=$v"; fi
    done
    exec "$DIR/bin/whatsrust" "$@"
  )
}

API_JSON="" # set by api_status: the JSON of `whatsrust status`

# api_status: fetch the status JSON into API_JSON; nonzero when the API does not answer.
api_status() {
  local rc=0
  API_JSON=$(client_cli status 2>/dev/null) || rc=$?
  return "$rc"
}

# json_top <json> <key>: the value of a top-level key of the pretty-printed status JSON (two-space
# indent), without quotes and comma; nonzero when the key is absent.
json_top() {
  local line v
  while IFS= read -r line; do
    case $line in
      "  \"$2\": "*)
        v=${line#*: }
        v=${v%,}
        v=${v#\"}
        v=${v%\"}
        printf '%s\n' "$v"
        return 0
        ;;
    esac
  done <<<"$1"
  return 1
}

# api_connected: yes / no / unreachable (needs the service to run).
api_connected() {
  local v
  if ! api_status; then
    echo unreachable
    return 0
  fi
  v=$(json_top "$API_JSON" connected) || v=""
  case $v in true) echo yes ;; *) echo no ;; esac
}

# sup_fail: stop because the supervisor cannot be asked (not the same as "the job is not loaded").
sup_fail() {
  case $PLATFORM in
    launchd) die "cannot query launchd for $LABEL (run this in your own macOS login session). The state of the service is UNKNOWN: look with 'launchctl print gui/$UID_NUM/$LABEL', and stop it with 'scripts/deploy.sh stop $INSTANCE' from your login session." ;;
    systemd) die "cannot query the systemd user instance for $UNIT (run this in your own login session). The state of the service is UNKNOWN: look with 'systemctl --user status $UNIT', and stop it with 'scripts/deploy.sh stop $INSTANCE' from your login session." ;;
  esac
}

# is_loaded: true when launchd has the job loaded (launchd only). Exit 113 is launchd's "no such job"; any
# other failure means launchd cannot be asked, and stops the script.
is_loaded() {
  local rc=0
  launchctl print "gui/$UID_NUM/$LABEL" >/dev/null 2>&1 || rc=$?
  case $rc in
    0) return 0 ;;
    113) return 1 ;;
  esac
  sup_fail
}

# launchd_field <key>: the first top-level value of `launchctl print` for the key (top-level keys start
# with exactly one tab). Empty when the key is absent or the job is not loaded; nonzero when launchd cannot
# be asked.
launchd_field() {
  local out line rc=0 tab=$'\t'
  out=$(launchctl print "gui/$UID_NUM/$LABEL" 2>/dev/null) || rc=$?
  case $rc in
    0) ;;
    113) return 0 ;;
    *) return 1 ;;
  esac
  while IFS= read -r line; do
    case $line in
      "${tab}$1 = "*)
        printf '%s\n' "${line#*= }"
        return 0
        ;;
    esac
  done <<<"$out"
}

# sd_prop <property>: the value of one `systemctl --user show` property of the unit; nonzero when the
# systemd user instance cannot be asked. (A unit that is not loaded still answers: inactive.)
sd_prop() {
  local out
  out=$(systemctl --user show -p "$1" "$UNIT" 2>/dev/null) || return 1
  printf '%s\n' "${out#*=}"
}

# svc_loaded: true when the supervisor holds the service: launchd has the job loaded (even idle after a
# clean exit), systemd has the unit active or restarting.
svc_loaded() {
  case $PLATFORM in
    launchd) is_loaded ;;
    systemd) svc_running ;;
  esac
}

# svc_running: true while the service runs or waits to be restarted after a crash.
svc_running() {
  local v
  case $PLATFORM in
    launchd)
      v=$(launchd_field pid) || sup_fail
      [ -n "$v" ]
      ;;
    systemd)
      v=$(sd_prop ActiveState) || sup_fail
      case $v in
        active | activating | reloading | deactivating) return 0 ;;
      esac
      return 1
      ;;
  esac
}

# svc_pid: the PID of the running daemon (empty when none or unknown).
svc_pid() {
  local p=""
  case $PLATFORM in
    launchd) p=$(launchd_field pid) || p="" ;;
    systemd) p=$(sd_prop MainPID) || p="" ;;
  esac
  case $p in '' | 0) ;; *) printf '%s\n' "$p" ;; esac
}

# wait_unloaded: launchctl bootout may return before the job is gone; wait until it is (STOP_WAIT seconds) so
# that a following bootstrap does not fail. Nonzero when the job stays.
wait_unloaded() {
  local dl
  deadline "$STOP_WAIT"
  dl=$DEADLINE
  while is_loaded && [ "$SECONDS" -lt "$dl" ]; do
    sleep 1
  done
  if is_loaded; then
    note "launchd still has $LABEL loaded after $STOP_WAIT s; check: launchctl print gui/$UID_NUM/$LABEL"
    return 1
  fi
}

# stop_service: stop the service gracefully (SIGTERM: the daemon drains, backs up the DB, exits 0). Nonzero
# when the supervisor fails.
stop_service() {
  case $PLATFORM in
    launchd)
      if is_loaded; then
        launchctl bootout "gui/$UID_NUM/$LABEL" || {
          note "launchctl bootout failed"
          return 1
        }
        wait_unloaded || return 1
      fi
      ;;
    systemd)
      systemctl --user stop "$UNIT" || {
        note "systemctl --user stop failed"
        return 1
      }
      ;;
  esac
}

# start_service: start the service under the supervisor. A launchd job that is loaded but idle is unloaded
# first, so that bootstrap loads the current plist. A systemd unit in the failed state is reset first.
start_service() {
  case $PLATFORM in
    launchd)
      if is_loaded; then
        stop_service || die "could not unload the old $LABEL job"
      fi
      launchctl bootstrap "gui/$UID_NUM" "$PLIST_FILE" ||
        die "launchctl bootstrap failed (a LaunchAgent needs a logged-in macOS user session)"
      ;;
    systemd)
      systemctl --user reset-failed "$UNIT" >/dev/null 2>&1 || true
      systemctl --user start "$UNIT" || die "systemctl --user start failed (see: scripts/deploy.sh logs $INSTANCE)"
      ;;
  esac
}

# restart_service: restart the daemon gracefully. launchd: SIGTERM, wait until the process is gone (exit 0
# is not restarted by KeepAlive), then kickstart; after STOP_WAIT seconds fall back to kickstart -k.
# systemd: restart.
restart_service() {
  local dl v
  case $PLATFORM in
    launchd)
      if [ "$DRY" -eq 1 ]; then
        run launchctl kill SIGTERM "gui/$UID_NUM/$LABEL"
        echo "      (then waits up to $STOP_WAIT s until the process is gone)"
        run launchctl kickstart "gui/$UID_NUM/$LABEL"
        return 0
      fi
      is_loaded || die "$LABEL is not loaded: start it with: scripts/deploy.sh run $INSTANCE"
      launchctl kill SIGTERM "gui/$UID_NUM/$LABEL" 2>/dev/null || true
      deadline "$STOP_WAIT"
      dl=$DEADLINE
      v=$(launchd_field pid) || sup_fail
      while [ -n "$v" ] && [ "$SECONDS" -lt "$dl" ]; do
        sleep 1
        v=$(launchd_field pid) || sup_fail
      done
      if [ -n "$v" ]; then
        note "the daemon did not stop within $STOP_WAIT s: falling back to kickstart -k (launchd sends SIGKILL)"
        launchctl kickstart -k "gui/$UID_NUM/$LABEL" || die "launchctl kickstart -k failed"
      else
        launchctl kickstart "gui/$UID_NUM/$LABEL" || die "launchctl kickstart failed"
      fi
      ;;
    systemd)
      run systemctl --user restart "$UNIT" || die "systemctl --user restart failed"
      ;;
  esac
}

AUTOSTART_NOW="" # set by autostart_actual: on or off

# autostart_actual: set AUTOSTART_NOW to on / off as the supervisor has it now (launchd: the LaunchAgents copy
# exists; systemd: the unit is enabled). Call it directly, never inside $(...): when the supervisor cannot be
# asked it must stop the script (sup_fail), and an exit inside a command substitution would not.
autostart_actual() {
  local out=""
  AUTOSTART_NOW=off
  case $PLATFORM in
    launchd) if [ -e "$AGENT_FILE" ] || [ -L "$AGENT_FILE" ]; then AUTOSTART_NOW=on; fi ;;
    systemd)
      out=$(systemctl --user is-enabled "$UNIT" 2>/dev/null) || true
      case $out in
        enabled | enabled-runtime) AUTOSTART_NOW=on ;;
        '')
          # no answer on stdout: either the unit is unknown to systemd or the user instance cannot be asked
          supervisor_reachable || sup_fail
          ;;
      esac
      ;;
  esac
}

# autostart_apply on|off: make the supervisor start the service at login (or not). Does not start or stop
# it now and does not touch deploy.conf. Every step is checked: callers may run with errexit off.
autostart_apply() {
  case $PLATFORM in
    launchd)
      if [ "$1" = on ]; then
        if will "copy $PLIST_FILE to $AGENT_FILE"; then
          mkdir -p -- "${AGENT_FILE%/*}" || die "could not create ${AGENT_FILE%/*}"
          cp -- "$PLIST_FILE" "$AGENT_FILE.new" || die "could not copy $PLIST_FILE to $AGENT_FILE.new"
          chmod 644 "$AGENT_FILE.new" || die "could not set the mode of $AGENT_FILE.new"
          mv -f -- "$AGENT_FILE.new" "$AGENT_FILE" || die "could not write $AGENT_FILE"
        fi
      elif will "remove $AGENT_FILE"; then
        rm -f -- "$AGENT_FILE" || die "could not remove $AGENT_FILE"
      fi
      ;;
    systemd)
      if [ "$1" = on ]; then
        run systemctl --user enable "$UNIT" || die "systemctl --user enable failed"
      else
        run systemctl --user disable "$UNIT" || die "systemctl --user disable failed"
      fi
      ;;
  esac
}

# conf_set_autostart on|off: store the autostart choice in deploy.conf.
conf_set_autostart() {
  conf_load
  CONF_AUTOSTART=$1
  conf_save
}

# linger_hint: what to know about systemd user services and logout (printed, never run).
linger_hint() {
  if [ "$PLATFORM" = systemd ]; then
    echo "  A user service stops when your last session ends, and starts at boot, only with lingering on."
    echo "  To switch it on (this script does not run it):  loginctl enable-linger \"\$USER\""
  else
    echo "  A LaunchAgent runs only while you are logged in; autostart starts it at your next login."
  fi
}

# effective_paired: true when the instance is paired: the DB says yes, or (no sqlite3) the running daemon
# reports connected.
effective_paired() {
  case $(paired_state) in
    yes) return 0 ;;
    no) return 1 ;;
  esac
  [ "$(api_connected)" = yes ]
}

# apply_conf_autostart: after a successful pairing or start: apply the autostart that deploy.conf records.
apply_conf_autostart() {
  conf_load
  if [ "$CONF_AUTOSTART" = on ]; then
    autostart_apply on
    echo "  autostart: on (the service starts at login)"
    linger_hint
  fi
}

# print_pid: one "PID" field (waits a moment for the supervisor to report it).
print_pid() {
  local p="" i=0
  while [ -z "$p" ] && [ "$i" -lt 5 ]; do
    p=$(svc_pid)
    if [ -z "$p" ]; then
      sleep 1
      i=$((i + 1))
    fi
  done
  field PID "${p:--}"
}

# status_launchd: print the fields of `launchctl print gui/<uid>/<label>` (top-level keys only: they
# start with exactly one tab; the first value of each key wins).
status_launchd() {
  local out line state="" pid="" runs="" code="" sig="" rc=0 tab=$'\t'
  out=$(launchctl print "gui/$UID_NUM/$LABEL" 2>/dev/null) || rc=$?
  if [ "$rc" -eq 0 ]; then
    while IFS= read -r line; do
      case $line in
        "${tab}state = "*) [ -n "$state" ] || state=${line#*= } ;;
        "${tab}pid = "*) [ -n "$pid" ] || pid=${line#*= } ;;
        "${tab}runs = "*) [ -n "$runs" ] || runs=${line#*= } ;;
        "${tab}last exit code = "*) [ -n "$code" ] || code=${line#*= } ;;
        "${tab}last terminating signal = "*) [ -n "$sig" ] || sig=${line#*= } ;;
      esac
    done <<<"$out"
    field state "${state:-unknown}"
    field PID "${pid:--}"
    field "last exit code" "${code:-unknown}"
    if [ -n "$sig" ]; then field "last signal" "$sig"; fi
    case $runs in
      '' | *[!0-9]* | 0) ;;
      *) field restarts "$((runs - 1)) (runs since load: $runs)" ;;
    esac
  elif [ "$rc" -eq 113 ]; then
    field state "stopped (not loaded; start it with: scripts/deploy.sh run $INSTANCE)"
  else
    field state "unknown (cannot query launchd; run this in your own macOS login session)"
  fi
}

# status_systemd: print the fields of `systemctl --user show` for the unit.
status_systemd() {
  local out k v active="" sub="" result="" pid="" status="" code="" nrestarts="" last
  out=$(systemctl --user show -p ActiveState,SubState,Result,MainPID,ExecMainStatus,ExecMainCode,NRestarts "$UNIT" 2>&1) ||
    die "systemctl --user show failed (is the systemd user instance running?): $out"
  while IFS='=' read -r k v; do
    case $k in
      ActiveState) active=$v ;;
      SubState) sub=$v ;;
      Result) result=$v ;;
      MainPID) pid=$v ;;
      ExecMainStatus) status=$v ;;
      ExecMainCode) code=$v ;;
      NRestarts) nrestarts=$v ;;
    esac
  done <<<"$out"
  field state "${active:-unknown} (${sub:-unknown}), result ${result:-unknown}"
  case $pid in '' | 0) pid=- ;; esac
  field PID "$pid"
  case $code in
    1) last="exit code $status" ;;
    2 | 3) last="killed by signal $status" ;;
    *) last="(never exited)" ;;
  esac
  field "last exit" "$last"
  if [ -n "$nrestarts" ]; then field restarts "$nrestarts"; fi
}

# abs_path <path>: the path made absolute with the current directory (no symlink or '..' resolution).
abs_path() {
  case $1 in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s\n' "$PWD/$1" ;;
  esac
}

# flow_trap: during a QR flow, a signal stops the service (see on_interrupt). SIGHUP (terminal closed),
# SIGPIPE (output pipe closed) and SIGQUIT count as much as Ctrl+C.
flow_trap() {
  trap on_interrupt INT TERM HUP PIPE QUIT
}

# flow_untrap: the QR flow is over and the service may keep running: default signal handling again.
flow_untrap() {
  trap - INT TERM HUP PIPE QUIT
}

# on_interrupt: a signal during a QR flow: stop the service gracefully FIRST, so that no unpaired instance is
# left running, then say so. Further signals are ignored until the stop is done, and a failed write (closed
# terminal or pipe) never keeps the script from stopping the service.
on_interrupt() {
  set +e
  trap '' INT TERM HUP PIPE QUIT
  QR_SVC=0
  if stop_service; then
    echo "" 2>/dev/null || true
    echo "interrupted: stopped the service; nothing unpaired is left running. Pair later with: scripts/deploy.sh run $INSTANCE" 2>/dev/null || true
  else
    note "interrupted, but the service could NOT be stopped. Stop it with: scripts/deploy.sh stop $INSTANCE"
  fi
  exit 1
}

# flow_stop <why>: end a QR flow that failed: stop the service and say that nothing unpaired is left. The
# signals stay ignored while it stops.
flow_stop() {
  trap '' INT TERM HUP PIPE QUIT
  QR_SVC=0
  note "$1"
  if stop_service; then
    echo "stopped; nothing unpaired is left running. Try again with: scripts/deploy.sh run $INSTANCE" 2>/dev/null || true
  else
    note "WARNING: could not stop the service. Stop it with: scripts/deploy.sh stop $INSTANCE"
  fi
}

# qr_mark_set: mark a QR flow as active: <dir>/.qr-flow (mode 600) holds the PID of this script. The start
# gate (bin/start-if-paired) starts an unpaired instance only while that PID is alive. qr_mark_clear
# removes the file; the exit trap does it on every way out.
qr_mark_set() {
  local f="$DIR/.qr-flow"
  local started
  QR_MARK=$f # first: the exit trap then removes the file even when a step below fails
  # Line 2 is this process's start time: the gate accepts the marker only while the PID still has it, so a
  # reused PID never counts as a live QR flow.
  started=$(ps -p "$$" -o lstart= 2>/dev/null | tr -s '[:space:]' ' ' | sed 's/^ //; s/ $//') || started=""
  [ -n "$started" ] || die "cannot read the start time of this process (ps -o lstart=); the QR flow needs it"
  (umask 077 && printf '%s\n%s\n' "$$" "$started" >"$f") || die "could not write $f"
  chmod 600 "$f" || die "could not set the mode of $f"
}

# qr_mark_clear: remove the QR flow marker, if this process wrote it and it is still ours.
qr_mark_clear() {
  local cur=""
  if [ -n "$QR_MARK" ]; then
    if [ -f "$QR_MARK" ]; then IFS= read -r cur <"$QR_MARK" || true; fi
    if [ "$cur" = "$$" ]; then rm -f -- "$QR_MARK" || true; fi
    QR_MARK=""
  fi
}

# draw_qr <qr text>: clear the terminal and draw the code with the instruction (without a terminal: a
# separator line instead of the clear).
draw_qr() {
  if [ -t 1 ]; then
    printf '\033[2J\033[H'
  else
    printf -- '----- QR code -----\n'
  fi
  printf '%s\n' "WhatsApp → Settings → Linked devices → Link a device, then scan this code:"
  printf '%s\n' "$1"
  printf '%s\n' "The code changes about every 20 s. Waiting for the scan. Ctrl+C stops the service."
}

# qr_wait <guard 0|1>: with the service started and its API up, poll every POLL s: draw the QR code when
# it changes; finish when the daemon reports connected: true (and the DB, if sqlite3 can tell, is paired).
# guard 1 (re-pair that keeps the DB): connected without any QR code for STILL_LINKED seconds means the old
# link still works. Returns 0 linked, 3 still linked (guard), 1 no scan in time (QR_WAIT seconds).
qr_wait() {
  local guard=$1 conn name qr last="" shown=0 waiting=0 dl still
  deadline "$QR_WAIT"
  dl=$DEADLINE
  deadline "$STILL_LINKED"
  still=$DEADLINE
  while [ "$SECONDS" -lt "$dl" ]; do
    if api_status; then
      conn=$(json_top "$API_JSON" connected) || conn=""
      if [ "$conn" = true ]; then
        if [ "$shown" -eq 1 ] || [ "$guard" -eq 0 ]; then
          if [ "$(paired_state)" != no ]; then
            name=$(json_top "$API_JSON" device_name) || name=""
            case $name in '' | null) name=$(env_get "$DIR/.env" WHATSRUST_DEVICE_NAME) ;; esac
            flow_untrap
            QR_SVC=0
            qr_mark_clear
            printf 'linked as %s; the service runs in the background\n' "${name:-whatsrust}"
            print_pid
            return 0
          fi
        elif [ "$SECONDS" -ge "$still" ]; then
          flow_untrap
          QR_SVC=0
          qr_mark_clear
          return 3
        fi
      elif qr=$(client_cli qr 2>/dev/null) && [ -n "$qr" ]; then
        if [ "$qr" != "$last" ]; then
          draw_qr "$qr"
          last=$qr
          shown=1
        fi
      elif [ "$waiting" -eq 0 ]; then
        if [ "$shown" -eq 0 ]; then
          echo "waiting for the QR code ..."
        else
          echo "the QR code is gone: waiting for the phone to finish the link ..."
        fi
        waiting=1
      fi
    fi
    sleep "$POLL"
  done
  return 1
}

# qr_flow <guard 0|1>: mark the QR flow, start the service, wait for its REST API, run qr_wait. Any failure
# stops the service again. Returns 0 linked, 3 still linked (guard 1), 1 failed.
qr_flow() {
  local guard=$1 rc=0 dl
  flow_trap
  qr_mark_set
  QR_SVC=1 # from now on, every way out of this process stops the service (see cleanup)
  start_service
  echo "service started; waiting for the REST API (up to $API_WAIT s) ..."
  deadline "$API_WAIT"
  dl=$DEADLINE
  until api_status; do
    if [ "$SECONDS" -ge "$dl" ]; then
      flow_stop "the REST API did not answer within $API_WAIT s (look at: scripts/deploy.sh logs $INSTANCE)"
      return 1
    fi
    sleep "$POLL"
  done
  qr_wait "$guard" || rc=$?
  case $rc in
    0) return 0 ;;
    3) return 3 ;;
  esac
  flow_stop "no scan within $((QR_WAIT / 60)) minutes"
  return 1
}

# require_rest_api: the QR flow and the connection check need the REST API.
require_rest_api() {
  [ "$(rest_port)" != 0 ] ||
    die "the REST API is off for '$INSTANCE' (WHATSRUST_PORT=0 in $DIR/.env); the QR flow needs it. Set a port with: scripts/deploy.sh install $INSTANCE"
}

# require_binary_installed: the installed daemon binary and the start gate must be there.
require_binary_installed() {
  [ -x "$DIR/bin/whatsrust" ] ||
    die "no binary at $DIR/bin/whatsrust; install it again with: scripts/deploy.sh install $INSTANCE"
  [ -x "$DIR/bin/start-if-paired" ] ||
    die "no start gate at $DIR/bin/start-if-paired; install it again with: scripts/deploy.sh install $INSTANCE"
}

# autostart_off_until_linked: an unpaired instance must not start at login: switch the autostart off
# (deploy.conf keeps the choice; apply_conf_autostart restores it after the link works).
autostart_off_until_linked() {
  autostart_actual
  if [ "$AUTOSTART_NOW" = on ]; then
    note "autostart is on but the instance is not paired: switching it off until the link works"
    autostart_apply off
  fi
}

# finish_linked: after a QR flow that linked the device.
finish_linked() {
  QR_SVC=0
  apply_conf_autostart
  echo "  next: scripts/deploy.sh status $INSTANCE"
}

# run_paired: start a paired instance and wait until it is connected. If the DB turns out unpaired while
# waiting (the phone unlinked the device), continue with the QR flow. Returns 0 connected, 1 failed.
# (Called as `run_paired || exit 1`, so errexit is off here: every step checks itself.)
run_paired() {
  local conn rc=0 dl
  start_service
  if [ "$(rest_port)" = 0 ]; then
    echo "service started. The REST API is off, so the connection cannot be checked."
    print_pid
    apply_conf_autostart
    return 0
  fi
  echo "service started; waiting until it is connected (up to $CONNECT_WAIT s) ..."
  deadline "$CONNECT_WAIT"
  dl=$DEADLINE
  while [ "$SECONDS" -lt "$dl" ]; do
    conn=$(api_connected)
    if [ "$conn" = yes ]; then
      echo "connected; the service runs in the background"
      print_pid
      apply_conf_autostart
      return 0
    fi
    if [ "$(paired_state)" = no ]; then
      note "the DB is not paired any more (unlinked on the phone?): switching to the QR flow"
      flow_trap
      qr_mark_set
      QR_SVC=1 # the service already runs for an unpaired instance: a failure below must stop it
      autostart_off_until_linked
      qr_wait 0 || rc=$?
      if [ "$rc" -eq 0 ]; then
        finish_linked
        return 0
      fi
      flow_stop "no scan within $((QR_WAIT / 60)) minutes"
      return 1
    fi
    sleep "$POLL"
  done
  note "not connected after $CONNECT_WAIT s. The service keeps running and the supervisor restarts it after a crash."
  note "look at: scripts/deploy.sh status $INSTANCE   and   scripts/deploy.sh logs $INSTANCE"
  return 1
}

cmd_run() {
  parse_args run " --dry-run --platform " "$@"
  begin
  require_unit
  require_binary_installed
  require_supervisor
  local paired pid
  pid=$(svc_pid)
  if [ -n "$pid" ]; then
    die "instance '$INSTANCE' runs already (PID $pid); use restart, or stop it first"
  fi
  paired=$(paired_state)
  if [ "$DRY" -eq 1 ]; then
    echo "dry run: nothing is started. paired: $paired"
    if [ "$paired" = yes ]; then
      echo "  would start the service, wait until the API reports connected: true, print the PID, apply autostart"
    else
      echo "  would mark the QR flow (<dir>/.qr-flow), start the service, show the QR code from 'whatsrust qr' until you scan it (5 min at most; Ctrl+C stops it)"
    fi
    return 0
  fi
  if [ "$paired" = yes ]; then
    run_paired || exit 1
    return 0
  fi
  require_rest_api
  autostart_off_until_linked
  note "instance '$INSTANCE' is not paired (paired: $paired): starting it with the QR code"
  qr_flow 0 || exit 1
  finish_linked
}

cmd_stop() {
  parse_args stop " --dry-run --platform " "$@"
  begin
  require_supervisor
  if [ "$DRY" -eq 1 ]; then
    echo "dry run: nothing is stopped"
    case $PLATFORM in
      launchd) run launchctl bootout "gui/$UID_NUM/$LABEL" ;;
      systemd) run systemctl --user stop "$UNIT" ;;
    esac
    return 0
  fi
  if ! svc_loaded; then
    echo "instance '$INSTANCE' is not running"
    return 0
  fi
  stop_service || die "could not stop instance '$INSTANCE'"
  echo "stopped instance '$INSTANCE' (graceful). It stays installed; start it with: scripts/deploy.sh run $INSTANCE"
}

cmd_restart() {
  parse_args restart " --dry-run --platform " "$@"
  begin
  require_unit
  require_supervisor
  [ "$DRY" -ne 1 ] || echo "dry run: nothing is changed"
  if [ "$DRY" -ne 1 ]; then
    svc_loaded || die "instance '$INSTANCE' is not running; start it with: scripts/deploy.sh run $INSTANCE"
    if ! effective_paired; then
      die "instance '$INSTANCE' is not paired (or cannot be told to be): an unpaired instance never runs unattended. Stop it (stop) and pair it with: scripts/deploy.sh run $INSTANCE"
    fi
  fi
  restart_service
  if [ "$DRY" -eq 0 ]; then
    echo "restarted instance '$INSTANCE'"
    print_pid
  fi
}

cmd_autostart() {
  parse_args autostart " --dry-run --platform " "$@"
  case $POS2 in
    on | off) ;;
    '') usage_err "autostart: missing on|off" ;;
    *) usage_err "autostart: expected on or off, not '$POS2'" ;;
  esac
  begin
  require_unit
  require_supervisor
  if [ "$POS2" = on ] && ! effective_paired; then
    die "instance '$INSTANCE' is not paired: run it first (scripts/deploy.sh run $INSTANCE). Autostart never starts an unpaired instance."
  fi
  [ "$DRY" -ne 1 ] || echo "dry run: nothing is changed"
  autostart_apply "$POS2"
  if will "store AUTOSTART=$POS2 in $DIR/deploy.conf"; then
    conf_set_autostart "$POS2"
    echo "autostart of instance '$INSTANCE' is $POS2"
    if [ "$POS2" = on ]; then linger_hint; fi
  fi
}

cmd_logs() {
  parse_args logs " -n --follow --platform " "$@"
  begin
  case $PLATFORM in
    launchd)
      local log="$DIR/logs/daemon.log"
      [ -f "$log" ] || die "no log file yet: $log"
      if [ "$FOLLOW" -eq 1 ]; then
        exec tail -n "$OPT_LINES" -f "$log"
      fi
      exec tail -n "$OPT_LINES" "$log"
      ;;
    systemd)
      if [ "$FOLLOW" -eq 1 ]; then
        exec journalctl --user -u "$UNIT" -n "$OPT_LINES" -f
      fi
      exec journalctl --user -u "$UNIT" -n "$OPT_LINES" --no-pager
      ;;
  esac
}

# qr_flow_active: print the PID in <dir>/.qr-flow when the QR flow is active. The same rule as the start gate:
# a number above 1, a marker not older than 15 minutes, and a live process whose command line holds deploy.sh
# (a reused PID of another program does not count).
qr_flow_active() {
  local pid="" started="" now
  [ -f "$DIR/.qr-flow" ] || return 1
  { IFS= read -r pid && IFS= read -r started; } <"$DIR/.qr-flow" || true
  started=$(printf '%s' "$started" | tr -s '[:space:]' ' ' | sed 's/^ //; s/ $//')
  case $pid in '' | *[!0-9]*) return 1 ;; esac
  [ "${#pid}" -le 9 ] || return 1
  [ "$((10#$pid))" -gt 1 ] || return 1
  [ -n "$started" ] || return 1
  if [ -n "$(find "$DIR/.qr-flow" -mmin +15 2>/dev/null)" ]; then return 1; fi
  now=$(ps -p "$pid" -o lstart= 2>/dev/null | tr -s '[:space:]' ' ' | sed 's/^ //; s/ $//') || return 1
  [ -n "$now" ] && [ "$now" = "$started" ] || return 1
  printf '%s\n' "$pid"
}

# gate_report <paired yes|no|unknown>: what the start gate (bin/start-if-paired) would do now.
gate_report() {
  local pid
  if [ ! -x "$DIR/bin/start-if-paired" ]; then
    echo "MISSING $DIR/bin/start-if-paired (run: scripts/deploy.sh install $INSTANCE)"
  elif [ "$1" = yes ]; then
    echo "open (paired): the service starts"
  elif pid=$(qr_flow_active); then
    echo "open: a QR flow of run / re-pair is active (PID $pid)"
  elif [ "$1" = no ]; then
    echo "closed (not paired): the service does not start. Pair it with: scripts/deploy.sh run $INSTANCE"
  elif command -v sqlite3 >/dev/null 2>&1; then
    echo "closed (cannot read whatsapp.db: the gate fails closed and does not start)"
  else
    echo "open with a warning (sqlite3 is not installed: the gate cannot tell whether it is paired)"
  fi
}

cmd_status() {
  parse_args status " --platform " "$@"
  begin
  local paired conn running=0 sup=1 name="" act sum
  paired=$(paired_state)
  if ! supervisor_reachable; then sup=0; fi
  if [ "$sup" -eq 1 ] && svc_running; then running=1; fi
  case $PLATFORM in
    launchd) echo "instance '$INSTANCE' (launchd, $LABEL)" ;;
    systemd) echo "instance '$INSTANCE' (systemd, $UNIT)" ;;
  esac
  field paired "$paired"
  if [ "$paired" = unknown ] && ! command -v sqlite3 >/dev/null 2>&1; then
    echo "                   (sqlite3 not found: install it for a reliable answer)"
  fi
  field "start gate" "$(gate_report "$paired")"
  if [ "$sup" -eq 0 ]; then
    conn="unknown (cannot ask the supervisor)"
  elif [ "$running" -eq 0 ]; then
    conn="no (the service is not running)"
  elif [ "$(rest_port)" = 0 ]; then
    conn="unknown (the REST API is off)"
  elif api_status; then
    case $(json_top "$API_JSON" connected || true) in
      true)
        conn=yes
        name=$(json_top "$API_JSON" device_name || true)
        case $name in '' | null) ;; *) conn="yes (device name: $name)" ;; esac
        ;;
      *) conn="no (state: $(json_top "$API_JSON" state || echo unknown))" ;;
    esac
  else
    conn="unknown (the REST API does not answer)"
  fi
  field connected "$conn"
  if [ "$sup" -eq 0 ]; then
    field state "unknown (cannot query the supervisor; run this in your own login session)"
  else
    case $PLATFORM in
      launchd) status_launchd ;;
      systemd) status_systemd ;;
    esac
  fi
  conf_load
  act=unknown
  if [ "$sup" -eq 1 ]; then
    autostart_actual
    act=$AUTOSTART_NOW
  fi
  if [ "$paired" = no ] && [ "$act" = on ]; then
    autostart_off_until_linked
    act=off
  fi
  if [ "$act" = "$CONF_AUTOSTART" ]; then
    field autostart "$act"
  else
    field autostart "$act (deploy.conf says $CONF_AUTOSTART)"
  fi
  field directory "$DIR"
  if [ -f "$DIR/bin/whatsrust" ]; then
    sum=$(sha256_of "$DIR/bin/whatsrust") || sum=""
    field "binary sha256" "${sum:0:12}"
  else
    field "binary" "missing: $DIR/bin/whatsrust"
  fi
  if [ "$paired" = no ] && [ "$running" -eq 1 ]; then
    echo "WARNING: the service runs but the instance is not paired (it shows a QR code that nobody scans)."
    echo "         Likely cause: the device was unlinked on the phone while the daemon ran. The daemon keeps"
    echo "         running until its next restart. Stop it with: scripts/deploy.sh stop $INSTANCE"
    echo "         then pair with: scripts/deploy.sh run $INSTANCE"
  fi
  echo "$EXIT_CODES_NOTE; every non-zero exit restarts the daemon after 10 s"
}

# reload_if_running <reload|restart>: after a re-install or an upgrade: if the service ran, restart it
# gracefully. reload (re-install) on launchd is stop + start, so that a changed plist is loaded; restart
# (upgrade) is the graceful restart of the restart command. An unpaired instance is only stopped.
reload_if_running() {
  if ! svc_loaded; then
    echo "  the service is stopped; start it with: scripts/deploy.sh run $INSTANCE"
    return 0
  fi
  if ! effective_paired; then
    stop_service || die "could not stop instance '$INSTANCE'"
    echo "  the instance is not paired (or cannot be told to be), so the service was stopped (an unpaired instance never runs unattended)"
    return 0
  fi
  if [ "$1" = reload ] && [ "$PLATFORM" = launchd ]; then
    stop_service || die "could not stop instance '$INSTANCE'"
    start_service
  else
    restart_service
  fi
  echo "  the service was restarted gracefully"
  print_pid
}

cmd_upgrade() {
  parse_args upgrade " --binary --embedder --dry-run --platform " "$@"
  begin
  require_unit
  require_supervisor
  conf_load
  local old_emb new_emb dest="$DIR/bin/whatsrust" src why old new was_running=0 emb_src="" emb_dest="$DIR/bin/whatsrust-embedder" semantic=0
  [ -f "$dest" ] || die "no installed binary at $dest (run: scripts/deploy.sh install $INSTANCE)"
  src=${OPT_BINARY:-${CONF_BINARY:-$ROOT/target/release/whatsrust}}
  src=$(abs_path "$src")
  why=$(binary_problem "$src")
  [ -z "$why" ] || die "binary $src: $why (build it with: make release, or pass --binary <path>)"
  if [ "$(env_get "$DIR/.env" WHATSRUST_EMBEDDER_CMD)" = "$emb_dest" ] && [ -f "$emb_dest" ]; then
    semantic=1
    emb_src=${OPT_EMBEDDER:-${CONF_EMBEDDER:-$ROOT/embedder-rs/target/release/whatsrust-embedder}}
    emb_src=$(abs_path "$emb_src")
    why=$(binary_problem "$emb_src")
    if [ -n "$why" ]; then
      [ -z "$OPT_EMBEDDER" ] || die "embedder $emb_src: $why"
      note "embedder source $emb_src: $why; the installed embedder stays (build it with: make embedder-build)"
      emb_src=""
    fi
  elif [ -n "$OPT_EMBEDDER" ]; then
    note "semantic search is off for '$INSTANCE', so --embedder is ignored (switch it on with install)"
  fi
  check_gate_template
  old=$(sha256_of "$dest")
  new=$(sha256_of "$src")
  [ "$DRY" -ne 1 ] || echo "dry run: nothing is changed"
  field "old sha256" "$old"
  field "new sha256" "$new"
  if [ "$old" = "$new" ]; then echo "  the new binary is identical to the installed one"; fi
  if svc_loaded; then was_running=1; fi
  if will "copy $src to $dest (through bin/whatsrust.new)"; then
    place_binary "$src" "$dest"
    if [ -n "$OPT_BINARY" ]; then CONF_BINARY=$src; fi
  fi
  if will "refresh the start gate $DIR/bin/start-if-paired"; then place_gate; fi
  if [ "$semantic" -eq 1 ] && [ -n "$emb_src" ]; then
    old_emb=$(sha256_of "$emb_dest")
    new_emb=$(sha256_of "$emb_src")
    field "embedder sha256" "$old_emb -> $new_emb"
    if will "copy $emb_src to $emb_dest (through whatsrust-embedder.new)"; then
      place_binary "$emb_src" "$emb_dest"
      if [ -n "$OPT_EMBEDDER" ]; then CONF_EMBEDDER=$emb_src; fi
    fi
  fi
  if [ "$DRY" -eq 1 ]; then
    if [ "$was_running" -eq 1 ]; then echo "  would restart the service gracefully"; fi
    return 0
  fi
  if [ -n "$OPT_BINARY" ] || [ -n "$OPT_EMBEDDER" ]; then conf_save; fi
  echo "upgraded instance '$INSTANCE'"
  if [ "$was_running" -eq 1 ]; then
    reload_if_running restart
  else
    echo "  the service is stopped: the files are replaced only; start it with: scripts/deploy.sh run $INSTANCE"
  fi
}

# ---------------------------------------------------------------------------
# install: the wizard
# ---------------------------------------------------------------------------

ANSWER=""    # set by ask_line: the trimmed answer
CHOICE=0     # set by ask_pick: the number picked (0 = typed text)
PICK_TEXT="" # set by ask_pick: the typed text when CHOICE is 0
# The values the wizard collects.
W_BIN_MODE="" W_BIN_SRC="" W_PORT="" W_API="" W_MCP="" W_MCP_PORT="" W_MCP_TOK="" W_SEM="" W_MODEL=""
W_EMB_MODE="" W_EMB_SRC="" W_DEVICE="" W_PAIRHIST="" W_AUTO=""
# The current values of an instance that is installed already (read from its .env and deploy.conf).
CUR_PORT="" CUR_API=0 CUR_MCP_PORT="" CUR_MCP_TOK=0 CUR_EMB="" CUR_MODEL="bge-m3" CUR_DEVICE="whatsrust" CUR_PAIRHIST="on" CUR_AUTO="off"
DIR_FIXED=0  # 1 when the instance is registered already (its directory cannot change)
SOURCE=""    # set by pick_source
RENDERED=""  # the unit file content that install writes

# ask_line <label>: print the label, read one answer from stdin into ANSWER (trimmed). End of input before
# an answer is a usage error. Without a terminal the answer is echoed, so a transcript reads well.
ask_line() {
  printf '  %s: ' "$1"
  if ! IFS= read -r ANSWER; then
    if [ -z "$ANSWER" ]; then
      printf '\n'
      usage_err "no answer: $NOANSWER_HINT"
    fi
  fi
  ANSWER=${ANSWER%$'\r'}
  ANSWER=$(trim "$ANSWER")
  if [ ! -t 0 ]; then printf '%s\n' "$ANSWER"; fi
}

# ask_pick <default> <max> <text-ok 0|1>: ask until the answer is Enter (the default), a number from 1 to
# max (CHOICE) or, with text-ok, any other text (CHOICE 0, PICK_TEXT).
ask_pick() {
  local def=$1 max=$2 text=$3
  while :; do
    ask_line "choice [$def]"
    case $ANSWER in
      '')
        CHOICE=$def
        return 0
        ;;
      *[!0-9]*) ;;
      *)
        if [ "${#ANSWER}" -le 4 ] && [ "$((10#$ANSWER))" -ge 1 ] && [ "$((10#$ANSWER))" -le "$max" ]; then
          CHOICE=$((10#$ANSWER))
          return 0
        fi
        ;;
    esac
    if [ "$text" -eq 1 ]; then
      CHOICE=0
      PICK_TEXT=$ANSWER
      return 0
    fi
    echo "  not accepted: enter a number from 1 to $max, or press Enter for $def"
  done
}

# step_head <n> <title>: the heading of a wizard step (silent with --yes).
step_head() {
  if [ "$YES" -eq 0 ]; then printf '\n[%s/10] %s\n' "$1" "$2"; fi
}

# menu_item <n> <text>: one numbered choice.
menu_item() {
  printf '  %s) %s\n' "$1" "$2"
}

# load_current: set CUR_* from <dir>/.env and <dir>/deploy.conf (empty / defaults when absent).
load_current() {
  local env="$DIR/.env" a t m v
  CUR_PORT=$(env_get "$env" WHATSRUST_PORT)
  if valid_port "$CUR_PORT"; then CUR_PORT=$((10#$CUR_PORT)); else CUR_PORT=""; fi
  a=$(env_get "$env" WHATSRUST_API_TOKEN)
  CUR_API=0
  if [ -n "$a" ]; then CUR_API=1; fi
  CUR_MCP_PORT=$(env_get "$env" WHATSRUST_MCP_PORT)
  if valid_port "$CUR_MCP_PORT"; then CUR_MCP_PORT=$((10#$CUR_MCP_PORT)); else CUR_MCP_PORT=""; fi
  t=$(env_get "$env" WHATSRUST_MCP_TOKEN)
  CUR_MCP_TOK=0
  if [ -n "$t" ] && [ -z "$(token_problem "$t" "$a")" ]; then CUR_MCP_TOK=1; fi
  CUR_EMB=$(env_get "$env" WHATSRUST_EMBEDDER_CMD)
  m=$(env_get "$env" WHATSRUST_EMBEDDER_ARGS)
  case $m in *minilm*) CUR_MODEL=minilm ;; *) CUR_MODEL=bge-m3 ;; esac
  CUR_DEVICE=$(env_get "$env" WHATSRUST_DEVICE_NAME)
  if [ -z "$CUR_DEVICE" ]; then CUR_DEVICE=whatsrust; fi
  v=$(lower "$(env_get "$env" WHATSRUST_STORE_PAIRING_HISTORY)")
  case $v in false | 0 | no) CUR_PAIRHIST=off ;; *) CUR_PAIRHIST=on ;; esac
  conf_load
  CUR_AUTO=$CONF_AUTOSTART
}

# dir_install_problem <dir>: dir_problem plus the install rules: not a file, no overlap with another instance,
# and either missing, empty, or holding the ownership marker (<dir>/.whatsrust-deploy) of THIS instance. So
# install never takes over a directory with somebody else's files, nor the data of another instance.
dir_install_problem() {
  local why pd entries mi
  why=$(dir_problem "$1")
  if [ -n "$why" ]; then
    printf '%s\n' "$why"
    return 0
  fi
  pd=$(phys_path "$1") || pd=$1
  if [ -e "$pd" ] && [ ! -d "$pd" ]; then
    printf 'exists and is not a directory\n'
    return 0
  fi
  why=$(dir_overlap "$pd")
  if [ -n "$why" ]; then
    printf '%s\n' "$why"
    return 0
  fi
  if [ -d "$pd" ]; then
    entries=$(ls -A "$pd" 2>&1) || {
      printf 'cannot be read\n'
      return 0
    }
    if [ -n "$entries" ]; then
      mi=$(marker_instance "$pd") || mi=""
      if [ -z "$mi" ]; then
        printf 'is not empty and was not made by scripts/deploy.sh (no %s marker in it). Use a new or empty directory\n' "$MARKER_NAME"
      elif [ "$mi" != "$INSTANCE" ]; then
        printf "holds the data of instance '%s' (its %s marker). Install it again under that name: scripts/deploy.sh install %s --dir <that directory>. To use it for '%s' on purpose, edit the marker yourself first\n" "$mi" "$MARKER_NAME" "$mi" "$INSTANCE"
      fi
    fi
  fi
}

# step_name: step 1.
step_name() {
  local def=${INSTANCE:-main} why
  if [ "$YES" -eq 0 ]; then
    cat <<'EOF'
install: one step at a time. Press Enter to take the default in [brackets], type a number to pick from
a list, or type text to set a value. Nothing is written before the summary and your yes.
EOF
    step_head 1 "instance name"
    while :; do
      ask_line "instance name [$def]"
      INSTANCE=${ANSWER:-$def}
      why=$(name_problem "$INSTANCE")
      if [ -z "$why" ]; then break; fi
      echo "  not accepted: $why"
    done
  else
    INSTANCE=$def
  fi
  check_instance_name "$INSTANCE"
  set_names
}

# step_dir: step 2. A registered instance keeps its directory. DIR is the physical path.
step_dir() {
  local reg def ans why
  def=$(default_dir)
  if reg=$(registry_get "$INSTANCE"); then
    if [ -n "$OPT_DIR" ] && [ "$(pkey "$(phys_path "$(norm_dir "$OPT_DIR")" || true)")" != "$(pkey "$(phys_path "$reg" || true)")" ]; then
      usage_err "instance '$INSTANCE' is installed with the directory $reg; to move it, run uninstall first (the data stays) and install again with --dir"
    fi
    why=$(dir_install_problem "$reg")
    [ -z "$why" ] || die "the directory of instance '$INSTANCE' ($reg) $why"
    DIR_FIXED=1
    DIR=$reg
    step_head 2 "directory"
    if [ "$YES" -eq 0 ]; then echo "  $reg  (fixed: this instance is installed; to move it, uninstall first, the data stays)"; fi
    return 0
  fi
  if [ -n "$OPT_DIR" ] || [ "$YES" -eq 1 ]; then
    ans=$(norm_dir "${OPT_DIR:-$def}")
    why=$(dir_install_problem "$ans")
    [ -z "$why" ] || usage_err "instance directory $(printf '%q' "$ans") $why"
  else
    step_head 2 "directory"
    menu_item 1 "$def  (default)"
    echo "  or type an absolute path"
    while :; do
      ask_pick 1 1 1
      if [ "$CHOICE" -eq 1 ]; then ans=$def; else ans=$(norm_dir "$PICK_TEXT"); fi
      why=$(dir_install_problem "$ans")
      if [ -z "$why" ]; then break; fi
      echo "  not accepted: $(printf '%q' "$ans") $why"
    done
  fi
  DIR=$(phys_path "$ans") || die "cannot resolve the directory $ans"
}

# pick_source <title> <flag value> <default> <hint>: set SOURCE to the executable to copy: from the flag,
# else the default (--yes), else asked. A source that is missing is an error (a warning with --dry-run).
pick_source() {
  local title=$1 flag=$2 def=$3 hint=$4 ans why sum
  if [ -n "$flag" ] || [ "$YES" -eq 1 ]; then
    ans=$(abs_path "${flag:-$def}")
    why=$(binary_problem "$ans")
    if [ -n "$why" ]; then
      if [ "$DRY" -eq 1 ]; then
        note "warning: $title $ans: $why (a real install would stop here; $hint)"
      else
        die "$title $ans: $why ($hint)"
      fi
    fi
  else
    menu_item 1 "$def  (default)"
    echo "  or type a path"
    while :; do
      ask_pick 1 1 1
      if [ "$CHOICE" -eq 1 ]; then ans=$def; else ans=$(abs_path "$PICK_TEXT"); fi
      why=$(binary_problem "$ans")
      if [ -z "$why" ]; then
        sum=$(sha256_of "$ans")
        echo "  sha256 $sum"
        break
      fi
      if [ "$DRY" -eq 1 ]; then
        note "warning: $ans: $why (a real install would stop here)"
        break
      fi
      echo "  not accepted: $ans: $why ($hint)"
    done
  fi
  SOURCE=$ans
}

# step_binary: step 3. An installed binary stays (upgrade replaces it).
step_binary() {
  local sum
  W_BIN_MODE=new
  W_BIN_SRC=""
  step_head 3 "daemon binary"
  if [ -f "$DIR/bin/whatsrust" ]; then
    W_BIN_MODE=keep
    if [ -n "$OPT_BINARY" ]; then
      note "$DIR/bin/whatsrust exists, so --binary is ignored (replace it with: scripts/deploy.sh upgrade $INSTANCE --binary <path>)"
    fi
    if [ "$YES" -eq 0 ]; then
      sum=$(sha256_of "$DIR/bin/whatsrust")
      echo "  installed copy kept: $DIR/bin/whatsrust"
      echo "  sha256 $sum (replace it with: scripts/deploy.sh upgrade $INSTANCE)"
    fi
    return 0
  fi
  pick_source "binary" "$OPT_BINARY" "${CONF_BINARY:-$ROOT/target/release/whatsrust}" "build it with: make release"
  W_BIN_SRC=$SOURCE
}

# other_ports_line: the ports of the other deploy instances, for the port steps.
other_ports_line() {
  local f name d key p out=""
  for f in "$(registry_dir)"/*.dir; do
    [ -f "$f" ] || continue
    name=${f##*/}
    name=${name%.dir}
    [ "$name" != "$INSTANCE" ] || continue
    d=$(registry_get "$name") || continue
    for key in WHATSRUST_PORT WHATSRUST_MCP_PORT; do
      p=$(env_get "$d/.env" "$key")
      if [ -n "$p" ]; then out="$out${out:+, }$p ($name)"; fi
    done
  done
  printf '%s\n' "$out"
}

# step_port: step 4.
step_port() {
  local def ans why taken
  if [ -n "$CUR_PORT" ] && [ -z "$(port_problem "$CUR_PORT" "$CUR_PORT" "")" ]; then
    def=$CUR_PORT
    PORT_NOTES=""
  else
    pick_free_port 7280 10 "$CUR_PORT" ""
    def=$PICKED_PORT
  fi
  step_head 4 "REST port"
  if [ -n "$OPT_PORT" ]; then
    why=$(port_problem "$OPT_PORT" "$CUR_PORT" "")
    [ -z "$why" ] || usage_err "--port $OPT_PORT: $why"
    W_PORT=$OPT_PORT
    return 0
  fi
  if [ "$YES" -eq 1 ]; then
    W_PORT=$def
    return 0
  fi
  taken=$(other_ports_line)
  echo "  ports of other deploy instances: ${taken:-none}"
  if [ -n "$PORT_NOTES" ]; then echo "  taken: $PORT_NOTES"; fi
  while :; do
    ask_line "REST port [$def]"
    ans=${ANSWER:-$def}
    why=$(port_problem "$ans" "$CUR_PORT" "")
    if [ -z "$why" ]; then break; fi
    echo "  not accepted: $ans: $why"
  done
  W_PORT=$((10#$ans))
}

# step_api: step 5. W_API is generate, none or keep.
step_api() {
  local def
  step_head 5 "REST API token"
  if [ -n "$OPT_API" ]; then
    if [ "$OPT_API" = keep ] && [ "$CUR_API" -eq 0 ]; then
      usage_err "--api-token keep: instance '$INSTANCE' has no REST API token yet (use generate or none)"
    fi
    W_API=$OPT_API
    return 0
  fi
  if [ "$CUR_API" -eq 1 ]; then def=keep; else def=generate; fi
  if [ "$YES" -eq 1 ]; then
    W_API=$def
    return 0
  fi
  if [ "$CUR_API" -eq 1 ]; then
    menu_item 1 "keep the current token  (default)"
    menu_item 2 "generate a new token"
    menu_item 3 "none (loopback only)"
    ask_pick 1 3 0
    case $CHOICE in 1) W_API=keep ;; 2) W_API=generate ;; 3) W_API=none ;; esac
  else
    menu_item 1 "generate a token  (default)"
    menu_item 2 "none (loopback only)"
    ask_pick 1 2 0
    case $CHOICE in 1) W_API=generate ;; 2) W_API=none ;; esac
  fi
}

# step_mcp: step 6. W_MCP on|off; with on: W_MCP_PORT and W_MCP_TOK (keep or generate).
step_mcp() {
  local def_on=0 on=0 def why ans
  if [ -n "$CUR_MCP_PORT" ]; then def_on=1; fi
  step_head 6 "MCP HTTP listener"
  if [ "$OPT_MCP" = off ] && [ -n "$OPT_MCP_PORT" ]; then usage_err "--mcp off cannot be combined with --mcp-port"; fi
  if [ -n "$OPT_MCP" ]; then
    if [ "$OPT_MCP" = on ]; then on=1; fi
  elif [ -n "$OPT_MCP_PORT" ]; then
    on=1
  elif [ "$YES" -eq 1 ]; then
    on=$def_on
  else
    if [ "$def_on" -eq 1 ]; then
      menu_item 1 "on  (current)"
      menu_item 2 "off"
      ask_pick 1 2 0
      if [ "$CHOICE" -eq 1 ]; then on=1; fi
    else
      menu_item 1 "off  (default)"
      menu_item 2 "on (port $((W_PORT + 1)) or the next free one, a generated token)"
      ask_pick 1 2 0
      if [ "$CHOICE" -eq 2 ]; then on=1; fi
    fi
  fi
  if [ "$on" -eq 0 ]; then
    W_MCP=off
    W_MCP_PORT=""
    W_MCP_TOK=""
    return 0
  fi
  W_MCP=on
  if [ -n "$CUR_MCP_PORT" ] && [ -z "$(port_problem "$CUR_MCP_PORT" "$CUR_MCP_PORT" "$W_PORT")" ]; then
    def=$CUR_MCP_PORT
  else
    pick_free_port $((W_PORT + 1)) 1 "$CUR_MCP_PORT" "$W_PORT"
    def=$PICKED_PORT
  fi
  if [ -n "$OPT_MCP_PORT" ]; then
    why=$(port_problem "$OPT_MCP_PORT" "$CUR_MCP_PORT" "$W_PORT")
    [ -z "$why" ] || usage_err "--mcp-port $OPT_MCP_PORT: $why"
    W_MCP_PORT=$OPT_MCP_PORT
  elif [ "$YES" -eq 1 ] || [ -n "$OPT_MCP" ]; then
    W_MCP_PORT=$def
  else
    while :; do
      ask_line "MCP port [$def]"
      ans=${ANSWER:-$def}
      why=$(port_problem "$ans" "$CUR_MCP_PORT" "$W_PORT")
      if [ -z "$why" ]; then break; fi
      echo "  not accepted: $ans: $why"
    done
    W_MCP_PORT=$((10#$ans))
  fi
  if [ "$CUR_MCP_TOK" -eq 1 ]; then W_MCP_TOK=keep; else W_MCP_TOK=generate; fi
}

# step_semantic: step 7. W_SEM on|off; with on: W_MODEL and the embedder (kept copy or a source).
step_semantic() {
  local def_on=0 on=0 emb="$DIR/bin/whatsrust-embedder"
  if [ -n "$CUR_EMB" ]; then def_on=1; fi
  step_head 7 "semantic search (embedder sidecar)"
  if [ "$OPT_SEMANTIC" = off ] && { [ -n "$OPT_MODEL" ] || [ -n "$OPT_EMBEDDER" ]; }; then
    usage_err "--semantic off cannot be combined with --model or --embedder"
  fi
  if [ -n "$OPT_SEMANTIC" ]; then
    if [ "$OPT_SEMANTIC" = on ]; then on=1; fi
  elif [ -n "$OPT_MODEL" ] || [ -n "$OPT_EMBEDDER" ]; then
    on=1
  elif [ "$YES" -eq 1 ]; then
    on=$def_on
  elif [ "$def_on" -eq 1 ]; then
    menu_item 1 "on  (current)"
    menu_item 2 "off"
    ask_pick 1 2 0
    if [ "$CHOICE" -eq 1 ]; then on=1; fi
  else
    menu_item 1 "off  (default)"
    menu_item 2 "on"
    ask_pick 1 2 0
    if [ "$CHOICE" -eq 2 ]; then on=1; fi
  fi
  W_EMB_MODE=""
  W_EMB_SRC=""
  W_MODEL=""
  if [ "$on" -eq 0 ]; then
    W_SEM=off
    return 0
  fi
  W_SEM=on
  W_MODEL=${OPT_MODEL:-$CUR_MODEL}
  if [ -z "$OPT_MODEL" ] && [ -z "$OPT_SEMANTIC" ] && [ "$YES" -eq 0 ]; then
    echo "  model:"
    if [ "$W_MODEL" = minilm ]; then
      menu_item 1 "bge-m3 (1024-dim, stronger multilingual recall)"
      menu_item 2 "minilm (384-dim, lighter)  (current)"
      ask_pick 2 2 0
    else
      menu_item 1 "bge-m3 (1024-dim, stronger multilingual recall)  (default)"
      menu_item 2 "minilm (384-dim, lighter)"
      ask_pick 1 2 0
    fi
    case $CHOICE in 1) W_MODEL=bge-m3 ;; 2) W_MODEL=minilm ;; esac
  fi
  if [ -f "$emb" ]; then
    W_EMB_MODE=keep
    if [ -n "$OPT_EMBEDDER" ]; then
      note "$emb exists, so --embedder is ignored (replace it with: scripts/deploy.sh upgrade $INSTANCE --embedder <path>)"
    fi
    if [ "$YES" -eq 0 ]; then echo "  embedder: installed copy kept: $emb"; fi
    return 0
  fi
  W_EMB_MODE=new
  if [ "$YES" -eq 0 ] && [ -z "$OPT_EMBEDDER" ] && [ -z "$OPT_SEMANTIC" ]; then echo "  embedder binary (copied into $DIR/bin/):"; fi
  pick_source "embedder" "$OPT_EMBEDDER" "${CONF_EMBEDDER:-$ROOT/embedder-rs/target/release/whatsrust-embedder}" "build it with: make embedder-build"
  W_EMB_SRC=$SOURCE
}

# step_device: step 8.
step_device() {
  local ans why
  step_head 8 "device name (shown in WhatsApp -> Linked devices)"
  if [ -n "$OPT_DEVICE" ]; then
    ans=$(trim "$OPT_DEVICE")
    why=$(device_problem "$ans")
    [ -z "$why" ] || usage_err "--device-name $why"
  elif [ "$YES" -eq 1 ]; then
    ans=$CUR_DEVICE
    why=$(device_problem "$ans")
    [ -z "$why" ] || die "the current device name in $DIR/.env $why: set a valid one with --device-name <name>"
  else
    echo "  Fixed at pairing. On a paired instance a change needs: scripts/deploy.sh re-pair $INSTANCE"
    menu_item 1 "$CUR_DEVICE  (default)"
    echo "  or type a name"
    while :; do
      ask_pick 1 1 1
      if [ "$CHOICE" -eq 1 ]; then ans=$CUR_DEVICE; else ans=$PICK_TEXT; fi
      why=$(device_problem "$ans")
      if [ -z "$why" ]; then break; fi
      echo "  not accepted: the name $why"
    done
  fi
  W_DEVICE=$(trim "$ans")
}

# step_pairhist: step 9.
step_pairhist() {
  step_head 9 "store the pairing history (the chats the phone sends when it links the device)"
  if [ -n "$OPT_PAIRHIST" ]; then
    W_PAIRHIST=$OPT_PAIRHIST
  elif [ "$YES" -eq 1 ]; then
    W_PAIRHIST=$CUR_PAIRHIST
  elif [ "$CUR_PAIRHIST" = off ]; then
    menu_item 1 "off  (current)"
    menu_item 2 "on"
    ask_pick 1 2 0
    if [ "$CHOICE" -eq 1 ]; then W_PAIRHIST=off; else W_PAIRHIST=on; fi
  else
    menu_item 1 "on  (default)"
    menu_item 2 "off"
    ask_pick 1 2 0
    if [ "$CHOICE" -eq 1 ]; then W_PAIRHIST=on; else W_PAIRHIST=off; fi
  fi
}

# step_autostart: step 10.
step_autostart() {
  step_head 10 "autostart at login"
  if [ -n "$OPT_AUTOSTART" ]; then
    W_AUTO=$OPT_AUTOSTART
  elif [ "$YES" -eq 1 ]; then
    W_AUTO=$CUR_AUTO
  else
    echo "  Applied only for a paired instance: 'run' applies it after the first scan."
    if [ "$CUR_AUTO" = on ]; then
      menu_item 1 "on  (current)"
      menu_item 2 "off"
      ask_pick 1 2 0
      if [ "$CHOICE" -eq 1 ]; then W_AUTO=on; else W_AUTO=off; fi
    else
      menu_item 1 "off  (default)"
      menu_item 2 "on"
      ask_pick 1 2 0
      if [ "$CHOICE" -eq 1 ]; then W_AUTO=off; else W_AUTO=on; fi
    fi
  fi
}

# tok_label <mode> <had-before 0|1>: how the summary names a token choice (never the token).
tok_label() {
  case $1 in
    generate) if [ "$2" -eq 1 ]; then echo "regenerated"; else echo "generated"; fi ;;
    keep) echo "kept" ;;
    none) echo "none (loopback only)" ;;
  esac
}

# show_summary: every value the wizard collected (tokens only as generated / kept / none).
show_summary() {
  local sum="" mcp sem
  printf '\nSummary for instance %s (%s):\n' "'$INSTANCE'" "$PLATFORM"
  field directory "$DIR"
  if [ "$W_BIN_MODE" = keep ]; then
    field "daemon binary" "installed copy kept (replace it with upgrade)"
  else
    if [ -f "$W_BIN_SRC" ]; then sum=$(sha256_of "$W_BIN_SRC" | cut -c1-12); fi
    field "daemon binary" "copy of $W_BIN_SRC (sha256 ${sum:-unknown})"
  fi
  field "REST port" "$W_PORT"
  field "REST API token" "$(tok_label "$W_API" "$CUR_API")"
  if [ "$W_MCP" = on ]; then
    mcp="on, port $W_MCP_PORT, token $(tok_label "$W_MCP_TOK" "$CUR_MCP_TOK")"
  else
    mcp=off
  fi
  field "MCP listener" "$mcp"
  if [ "$W_SEM" = on ]; then
    sem="on, model $W_MODEL, embedder "
    if [ "$W_EMB_MODE" = keep ]; then sem="${sem}installed copy kept"; else sem="${sem}copy of $W_EMB_SRC"; fi
  else
    sem=off
  fi
  field "semantic search" "$sem"
  field "device name" "$W_DEVICE"
  field "pairing history" "$W_PAIRHIST"
  field autostart "$W_AUTO"
  if [ -f "$DIR/.env" ]; then
    field ".env" "update the managed keys, keep every other line"
  else
    field ".env" "create (mode 600)"
  fi
  if [ "$W_DEVICE" != "$CUR_DEVICE" ] && [ "$(paired_state)" = yes ]; then
    echo "  note: the instance is paired, so the new device name applies only after: scripts/deploy.sh re-pair $INSTANCE"
  fi
  if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "  WARNING: sqlite3 is not installed. The start gate cannot tell whether the instance is paired, so after a"
    echo "  restart it starts the daemon with only a warning in the log, and 'unpaired never runs unattended' holds"
    echo "  only for run / re-pair. Install sqlite3 (macOS: brew install sqlite; Linux: apt install sqlite3)."
  fi
}

# confirm_write: ask `Write it? [y/N]` (not with --yes or --dry-run).
confirm_write() {
  if [ "$YES" -eq 1 ] || [ "$DRY" -eq 1 ]; then return 0; fi
  printf '\n'
  ask_line "Write it? [y/N]"
  case $(lower "$ANSWER") in
    y | yes) ;;
    *) die "aborted: nothing was written" ;;
  esac
}

ENV_SPECS=() # set by build_env_specs: the edits install makes in the .env

# build_env_specs <api token> <mcp token>: set ENV_SPECS from the wizard values (the tokens are used only
# for the generate choices).
build_env_specs() {
  ENV_SPECS=("WHATSRUST_PORT=$W_PORT")
  case $W_API in
    generate) ENV_SPECS+=("WHATSRUST_API_TOKEN=$1") ;;
    none) ENV_SPECS+=("-WHATSRUST_API_TOKEN") ;;
  esac
  if [ "$W_MCP" = on ]; then
    ENV_SPECS+=("WHATSRUST_MCP_PORT=$W_MCP_PORT")
    if [ "$W_MCP_TOK" = generate ]; then ENV_SPECS+=("WHATSRUST_MCP_TOKEN=$2"); fi
  else
    ENV_SPECS+=("-WHATSRUST_MCP_PORT" "-WHATSRUST_MCP_TOKEN")
  fi
  if [ "$W_SEM" = on ]; then
    ENV_SPECS+=("WHATSRUST_EMBEDDER_CMD=\"$DIR/bin/whatsrust-embedder\"" "WHATSRUST_EMBEDDER_ARGS=\"--model $W_MODEL\"")
  else
    ENV_SPECS+=("-WHATSRUST_EMBEDDER_CMD" "-WHATSRUST_EMBEDDER_ARGS")
  fi
  ENV_SPECS+=("WHATSRUST_DEVICE_NAME=\"$W_DEVICE\"")
  if [ "$W_PAIRHIST" = off ]; then
    ENV_SPECS+=("WHATSRUST_STORE_PAIRING_HISTORY=false")
  else
    ENV_SPECS+=("-WHATSRUST_STORE_PAIRING_HISTORY")
  fi
}

GEN_API_TOK="" # set by install_write: the new REST API token, until write_env has used it
GEN_MCP_TOK="" # set by install_write: the new MCP token, until write_env has used it

# write_env: create the .env when missing (managed keys first, then a comment about .env.example), else
# update only the managed keys in place. Never prints a token.
write_env() {
  local env="$DIR/.env" fresh=0
  if [ ! -f "$env" ]; then
    fresh=1
    (umask 077 && : >"$env") || die "could not create $env"
  fi
  build_env_specs "$GEN_API_TOK" "$GEN_MCP_TOK"
  GEN_API_TOK=""
  GEN_MCP_TOK=""
  env_edit "$env" "${ENV_SPECS[@]}"
  ENV_SPECS=()
  chmod 600 "$env" || die "could not set the mode of $env"
  if [ "$fresh" -eq 1 ]; then
    {
      printf '\n'
      printf '# Written by scripts/deploy.sh install (instance %s). The daemon keeps the FIRST value it sees for a key.\n' "$INSTANCE"
      printf '# The other options are documented in .env.example in the whatsrust repo: %s/.env.example\n' "$ROOT"
    } >>"$env" || die "could not write $env"
  fi
}

# install_precheck: everything that can refuse an install, before the first write (and before the question).
install_precheck() {
  local why
  why=$(dir_install_problem "$DIR")
  [ -z "$why" ] || die "instance directory $DIR $why"
  require_units_ours
  if [ "$DRY" -eq 0 ]; then
    require_supervisor
    if [ "$DIR_FIXED" -eq 0 ] && [ "$PLATFORM" = launchd ] && is_loaded; then
      die "launchd has a job $LABEL loaded that scripts/deploy.sh did not install for '$INSTANCE'. Nothing was changed. Remove it yourself (launchctl bootout gui/$UID_NUM/$LABEL) if it is yours."
    fi
    if [ "$DIR_FIXED" -eq 0 ] && [ "$PLATFORM" = systemd ]; then
      precheck_new_unit
    fi
  fi
}

# precheck_new_unit: for a NEW systemd instance: no unit of that name may be active, and none may come from
# another place than our unit file (for example /etc/systemd/user).
precheck_new_unit() {
  local frag pf pu
  if svc_running; then
    die "systemd has an active unit $UNIT that scripts/deploy.sh did not install for '$INSTANCE'. Nothing was changed."
  fi
  frag=$(sd_prop FragmentPath) || sup_fail
  if [ -n "$frag" ]; then
    pf=$(phys_path "$frag") || pf=$frag
    pu=$(phys_path "$UNIT_FILE") || pu=$UNIT_FILE
    if [ "$pf" != "$pu" ]; then
      die "a systemd unit named $UNIT already exists at $frag (not $UNIT_FILE). Nothing was changed. Choose another instance name, or remove that unit yourself."
    fi
  fi
}

# install_write: do the steps of install (with --dry-run only print them). Every step is checked.
install_write() {
  local was_up=0 paired why dirs="$DIR/bin"
  if [ "$DRY" -eq 0 ]; then
    if [ "$W_API" = generate ]; then GEN_API_TOK=$(gen_token); fi
    if [ "$W_MCP_TOK" = generate ]; then GEN_MCP_TOK=$(gen_token); fi
    # look again right before the first write: the directory may have changed since the first check
    why=$(dir_install_problem "$DIR")
    [ -z "$why" ] || die "instance directory $DIR $why (it changed since the first check). Nothing was written."
    if svc_loaded; then was_up=1; fi
  fi
  if [ "$PLATFORM" = launchd ]; then dirs="$dirs and $DIR/logs"; fi
  if will "create $dirs (mode 700)"; then
    (umask 077 && mkdir -p -- "$DIR/bin") || die "could not create $DIR/bin"
    chmod 700 "$DIR" "$DIR/bin" || die "could not set the mode of $DIR"
    if [ "$PLATFORM" = launchd ]; then
      (umask 077 && mkdir -p -- "$DIR/logs" && : >>"$DIR/logs/daemon.log") || die "could not create $DIR/logs/daemon.log"
      chmod 700 "$DIR/logs" || die "could not set the mode of $DIR/logs"
      chmod 600 "$DIR/logs/daemon.log" || die "could not set the mode of $DIR/logs/daemon.log"
    fi
  fi
  if will "write the ownership marker $DIR/$MARKER_NAME (mode 600)"; then
    write_file_atomic "$DIR/$MARKER_NAME" 600 "managed by scripts/deploy.sh; instance=$INSTANCE"
  fi
  if [ "$W_BIN_MODE" = new ]; then
    if will "copy $W_BIN_SRC to $DIR/bin/whatsrust (mode 755, through bin/whatsrust.new)"; then
      place_binary "$W_BIN_SRC" "$DIR/bin/whatsrust"
    fi
  else
    echo "  keep the installed binary $DIR/bin/whatsrust"
  fi
  if will "install the start gate $DIR/bin/start-if-paired (mode 755)"; then place_gate; fi
  if [ "$W_SEM" = on ] && [ "$W_EMB_MODE" = new ]; then
    if will "copy $W_EMB_SRC to $DIR/bin/whatsrust-embedder (mode 755)"; then
      place_binary "$W_EMB_SRC" "$DIR/bin/whatsrust-embedder"
    fi
  fi
  if [ -f "$DIR/.env" ]; then
    if will "update the managed keys in $DIR/.env (mode 600), keep every other line"; then write_env; fi
  elif will "create $DIR/.env (mode 600): managed keys first, then a pointer to .env.example"; then
    write_env
  fi
  if will "write $DIR/deploy.conf (mode 600)"; then
    CONF_AUTOSTART=$W_AUTO
    if [ "$W_BIN_MODE" = new ]; then CONF_BINARY=$W_BIN_SRC; fi
    if [ "$W_SEM" = on ] && [ "$W_EMB_MODE" = new ]; then CONF_EMBEDDER=$W_EMB_SRC; fi
    conf_save
  fi
  if will "record the directory in $(registry_file "$INSTANCE")"; then registry_put; fi
  if will "write $(unit_file)"; then write_unit "$(unit_file)" "$RENDERED"; fi
  if [ "$PLATFORM" = systemd ]; then
    run systemctl --user daemon-reload || die "systemctl --user daemon-reload failed"
  fi
  paired=$(paired_state)
  if [ "$W_AUTO" = on ]; then
    if [ "$paired" = yes ]; then
      autostart_apply on
    else
      echo "  autostart is on in deploy.conf; 'run' applies it after the first scan (paired: $paired)"
      if [ "$DRY" -eq 0 ]; then autostart_off_until_linked; fi
    fi
  elif [ "$DRY" -eq 1 ]; then
    echo "  would switch autostart off, if it is on"
  else
    autostart_actual
    if [ "$AUTOSTART_NOW" = on ]; then autostart_apply off; fi
  fi
  if [ "$DRY" -eq 1 ]; then
    echo "  would restart the service gracefully, if it runs"
  elif [ "$was_up" -eq 1 ]; then
    reload_if_running reload
  fi
}

cmd_install() {
  parse_args install " --dir --binary --port --api-token --mcp --mcp-port --semantic --model --embedder --device-name --pairing-history --autostart --yes --dry-run --platform " "$@"
  set_platform
  step_name
  step_dir
  set_paths
  load_current
  step_binary
  step_port
  step_api
  step_mcp
  step_semantic
  step_device
  step_pairhist
  step_autostart
  RENDERED=$(render_unit)
  [ -n "$RENDERED" ] || die "the rendered unit is empty (template: $TEMPLATES)"
  check_gate_template
  show_summary
  if [ "$DRY" -eq 1 ]; then
    printf '\ndry run: nothing is written. The unit file %s, rendered from %s/:\n-----\n' "$(unit_file)" "$TEMPLATES"
    printf '%s\n' "$RENDERED"
    printf -- '-----\n\nsteps:\n'
  fi
  install_precheck
  confirm_write
  install_write
  if [ "$DRY" -eq 1 ]; then
    echo
    echo "dry run, nothing was written."
    return 0
  fi
  echo
  if [ "$DIR_FIXED" -eq 1 ]; then echo "re-installed instance '$INSTANCE' ($PLATFORM):"; else echo "installed instance '$INSTANCE' ($PLATFORM):"; fi
  field directory "$DIR"
  field "unit file" "$(unit_file)"
  field ".env" "$DIR/.env (mode 600)"
  echo "  The CLI does not read .env: give it WHATSRUST_PORT and WHATSRUST_API_TOKEN as environment variables."
  echo
  echo "next: scripts/deploy.sh run $INSTANCE"
}

# utc_stamp: the UTC time for a file name, 20261025T101500Z.
utc_stamp() {
  date -u +%Y%m%dT%H%M%SZ
}

# ask_yn <prompt> <default y|n>: ask a yes / no question; Enter takes the default.
ask_yn() {
  local def=$2
  ask_line "$1"
  case $(lower "$ANSWER") in
    '') [ "$def" = y ] ;;
    y | yes) return 0 ;;
    *) return 1 ;;
  esac
}

# set_device_name <name>: write WHATSRUST_DEVICE_NAME into the instance's .env (the other lines stay).
set_device_name() {
  env_edit "$DIR/.env" "WHATSRUST_DEVICE_NAME=\"$1\""
}

# db_files: print the names of the DB files that exist in DIR (whatsapp.db, -wal, -shm).
db_files() {
  local f
  for f in whatsapp.db whatsapp.db-wal whatsapp.db-shm; do
    if [ -e "$DIR/$f" ]; then printf '%s\n' "$f"; fi
  done
}

# repair_rollback <names>: put the DB files that a failed move already carried to REPAIR_BK back into DIR.
repair_rollback() {
  local f bad=""
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    mv -- "$REPAIR_BK/$f" "$DIR/$f" || bad="$bad $f"
  done <<<"$1"
  if [ -n "$bad" ]; then
    die "the DB files are SPLIT between $DIR and $REPAIR_BK (could not move back:$bad). Put them together by hand before you start the service."
  fi
  rmdir -- "$REPAIR_BK" 2>/dev/null || true
}

# repair_backup <move|copy>: put the DB files (whatsapp.db, -wal, -shm) into a new
# <dir>/re-pair-backup-<UTC>/ (mode 700); sets REPAIR_BK. Not meant for $(...): every step is checked here.
# A move that fails midway is rolled back, so the DB is never split between two directories. At the end each
# file must be in the backup (and, for a move, gone from <dir>). Signals are ignored for the whole step (no trap
# is set at this point, so default handling comes back after it): an interrupt must not leave the DB split.
repair_backup() {
  local mode=$1 f done_list="" files base n=1
  base="$DIR/re-pair-backup-$(utc_stamp)"
  REPAIR_BK=$base
  while [ -e "$REPAIR_BK" ]; do
    n=$((n + 1))
    REPAIR_BK="$base-$n" # two re-pairs in the same second
  done
  files=$(db_files)
  trap '' INT TERM HUP PIPE QUIT
  (umask 077 && mkdir -- "$REPAIR_BK") || die "could not create $REPAIR_BK; nothing was changed"
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    if [ "$mode" = move ]; then
      if ! mv -- "$DIR/$f" "$REPAIR_BK/$f"; then
        note "could not move $f to $REPAIR_BK: moving back what was moved"
        repair_rollback "$done_list"
        die "re-pair stopped: the DB files are back in $DIR"
      fi
    elif ! cp -p -- "$DIR/$f" "$REPAIR_BK/$f"; then
      rm -rf -- "$REPAIR_BK" || true
      die "could not copy $f to $REPAIR_BK; the DB files in $DIR are untouched"
    fi
    done_list="$done_list$f"$'\n'
  done <<<"$files"
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    if [ ! -f "$REPAIR_BK/$f" ] || { [ "$mode" = move ] && [ -e "$DIR/$f" ]; }; then
      if [ "$mode" = move ]; then
        repair_rollback "$done_list"
      else
        rm -rf -- "$REPAIR_BK" || true
      fi
      die "the re-pair backup of $f could not be verified; the DB files are as they were"
    fi
  done <<<"$done_list"
  trap - INT TERM HUP PIPE QUIT
}

cmd_repair() {
  parse_args re-pair " --mode --device-name --yes --dry-run --platform " "$@"
  begin
  require_unit
  require_binary_installed
  require_rest_api
  require_supervisor
  local mode=${OPT_MODE:-fresh} name why bk rc=0 cur guard
  cur=$(env_get "$DIR/.env" WHATSRUST_DEVICE_NAME)
  cur=${cur:-whatsrust}
  name=${OPT_DEVICE:-$cur}
  if [ -z "$OPT_MODE" ] && [ -z "$OPT_DEVICE" ] && [ "$YES" -eq 0 ]; then
    printf 're-pair instance %s: link it to the phone again.\n\nmode:\n' "'$INSTANCE'"
    menu_item 1 "fresh  (default): the old DB moves to a backup folder, the new link starts from an empty DB."
    echo "     This avoids an open bug: a re-pair that keeps the DB keeps the old app-state data (no push name,"
    echo "     no archive or read-state events). The message history starts empty."
    menu_item 2 "keep-history: keep the DB. It may hit that bug."
    ask_pick 1 2 0
    case $CHOICE in 1) mode=fresh ;; 2) mode=keep-history ;; esac
    echo
    echo "device name (used only while unpaired, then fixed):"
    menu_item 1 "$cur  (default)"
    echo "  or type a name"
    while :; do
      ask_pick 1 1 1
      if [ "$CHOICE" -eq 1 ]; then name=$cur; else name=$PICK_TEXT; fi
      why=$(device_problem "$name")
      if [ -z "$why" ]; then break; fi
      echo "  not accepted: the name $why"
    done
  fi
  name=$(trim "$name")
  why=$(device_problem "$name")
  [ -z "$why" ] || usage_err "re-pair: the device name $why"
  printf '\nre-pair instance %s: mode %s, device name %s\n' "'$INSTANCE'" "$mode" "$name"
  if [ "$DRY" -eq 1 ]; then
    echo "dry run: nothing is changed. It would:"
    echo "  stop the service, switch autostart off until the link works"
    if [ "$mode" = fresh ]; then
      echo "  move the DB files to $DIR/re-pair-backup-<UTC>/"
    else
      echo "  after you unlink the device on the phone: copy the DB files to $DIR/re-pair-backup-<UTC>/"
    fi
    echo "  set WHATSRUST_DEVICE_NAME in $DIR/.env, then show the QR code (see run --help)"
    return 0
  fi
  if [ "$YES" -eq 0 ]; then
    if [ "$mode" = keep-history ]; then
      echo
      echo "Unlink the device on the phone first: WhatsApp → Settings → Linked devices → the device → Log out."
      ask_yn "done? [y/N]" n || die "aborted: nothing was changed"
    else
      ask_yn "Go on? [y/N]" n || die "aborted: nothing was changed"
    fi
  fi
  if svc_loaded; then
    echo "stopping the service (graceful) ..."
    stop_service || die "could not stop instance '$INSTANCE'"
  fi
  autostart_actual
  if [ "$AUTOSTART_NOW" = on ]; then
    echo "autostart is switched off until the new link works"
    autostart_apply off
  fi
  set_device_name "$name"
  if [ "$mode" = fresh ]; then
    repair_backup move
    bk=$REPAIR_BK
    echo "moved the DB files to $bk"
  else
    repair_backup copy
    bk=$REPAIR_BK
    echo "copied the DB files to $bk"
  fi
  guard=0
  if [ "$mode" = keep-history ]; then guard=1; fi
  qr_flow "$guard" || rc=$?
  case $rc in
    0)
      finish_linked
      echo "  re-paired. Also remove the OLD device on the phone (WhatsApp → Linked devices)."
      echo "  The old DB is kept in $bk"
      ;;
    3)
      apply_conf_autostart
      echo "the device is still linked: unlink it on the phone, then run re-pair again"
      echo "  (the service keeps running with the old link; the DB copy is in $bk)"
      exit 1
      ;;
    *) exit 1 ;;
  esac
}

# human_size <bytes>: a short size, 1.4M.
human_size() {
  local b=$1
  if [ "$b" -ge 1073741824 ]; then
    printf '%s.%sG\n' "$((b / 1073741824))" "$((b % 1073741824 * 10 / 1073741824))"
  elif [ "$b" -ge 1048576 ]; then
    printf '%s.%sM\n' "$((b / 1048576))" "$((b % 1048576 * 10 / 1048576))"
  elif [ "$b" -ge 1024 ]; then
    printf '%s.%sK\n' "$((b / 1024))" "$((b % 1024 * 10 / 1024))"
  else
    printf '%sB\n' "$b"
  fi
}

# delete_guard <dir>: why the directory must not be deleted (nothing printed when it may be): the rules of
# dir_problem (by physical path), no symbolic link, bin/whatsrust inside, and the ownership marker
# (.whatsrust-deploy) that names this instance.
delete_guard() {
  local why pd mi
  why=$(dir_problem "$1")
  if [ -n "$why" ]; then
    printf '%s\n' "$why"
    return 0
  fi
  pd=$(phys_path "$1") || pd=$1
  why=$(dir_overlap "$pd")
  if [ -n "$why" ]; then
    printf '%s\n' "$why"
  elif [ -L "$1" ]; then
    printf 'is a symbolic link\n'
  elif [ ! -f "$pd/bin/whatsrust" ]; then
    printf 'has no bin/whatsrust, so it does not look like an instance directory\n'
  elif ! mi=$(marker_instance "$pd"); then
    printf 'has no %s marker: scripts/deploy.sh did not make it\n' "$MARKER_NAME"
  elif [ "$mi" != "$INSTANCE" ]; then
    printf "belongs to instance '%s' by its %s marker\n" "$mi" "$MARKER_NAME"
  fi
}

# backup_path_problem <path>: why the archive cannot go there (nothing printed when it is fine). The check
# compares physical paths: the archive must not end up inside the directory that gets deleted.
backup_path_problem() {
  local pb pd
  case $1 in
    /*) ;;
    *)
      printf 'must be an absolute path\n'
      return 0
      ;;
  esac
  case $1 in
    *[[:cntrl:]]*)
      printf 'has a control character\n'
      return 0
      ;;
    */. | */./* | */.. | */../*)
      printf "has a '.' or '..' part\n"
      return 0
      ;;
  esac
  pb=$(phys_path "$1") || {
    printf 'cannot be resolved (no parent directory can be entered)\n'
    return 0
  }
  pd=$(phys_path "$DIR") || pd=$DIR
  if path_in "$pb" "$pd"; then
    printf 'is inside the directory that is about to be deleted\n'
  elif [ -e "$1" ] || [ -L "$1" ]; then
    printf 'exists already\n'
  fi
}

# write_backup <archive>: tar the instance directory to the archive (umask 077), verify it, print path and
# size. Dies, keeping the data, when anything fails.
write_backup() {
  local out=$1 parent listing bytes perms f nl=$'\n'
  parent=${out%/*}
  [ -n "$parent" ] || parent=/
  if [ ! -d "$parent" ]; then
    (umask 077 && mkdir -p -- "$parent") || die "could not create $parent"
    chmod 700 "$parent" || die "could not set the mode of $parent"
  else
    perms=$(ls -ld -- "$parent")
    perms=${perms%% *}
    case ${perms:4:6} in
      ------) ;;
      *) note "warning: the backup folder $parent is not mode 700 (other users can list it); the archive itself is mode 600" ;;
    esac
  fi
  if ! (umask 077 && tar -czf "$out" -C "$DIR" .); then
    rm -f -- "$out"
    die "the backup failed; the data stays in $DIR"
  fi
  listing=$(tar -tzf "$out") || listing=""
  bytes=$(wc -c <"$out")
  bytes=$(trim "$bytes")
  if [ -z "$listing" ] || [ "${bytes:-0}" -le 0 ]; then
    rm -f -- "$out"
    die "the backup archive is empty or unreadable; the data stays in $DIR"
  fi
  # exact members, as `tar -C <dir> .` names them: ./whatsapp.db and the -wal / -shm files that exist
  for f in whatsapp.db whatsapp.db-wal whatsapp.db-shm; do
    if [ -e "$DIR/$f" ]; then
      case "$nl$listing$nl" in
        *"$nl./$f$nl"*) ;;
        *)
          rm -f -- "$out"
          die "the backup archive does not list ./$f; the data stays in $DIR"
          ;;
      esac
    fi
  done
  echo "backup written and verified:"
  field file "$out"
  field size "$(human_size "$bytes") ($bytes bytes)"
}

# remove_unit: stop the service and remove the unit files of the instance. The registry entry stays until the
# data step is done (remove_registry_entry), so that a failed backup leaves an instance that a second
# uninstall can finish.
remove_unit() {
  if [ "$DRY" -eq 1 ]; then
    case $PLATFORM in
      launchd)
        run launchctl bootout "gui/$UID_NUM/$LABEL"
        echo "      (only when the job is loaded)"
        will "remove $AGENT_FILE and $PLIST_FILE" || true
        ;;
      systemd)
        run systemctl --user stop "$UNIT"
        run systemctl --user disable "$UNIT"
        will "remove $UNIT_FILE" || true
        run systemctl --user daemon-reload
        ;;
    esac
    will "remove the registry entry $(registry_file "$INSTANCE") (last)" || true
    return 0
  fi
  if svc_loaded; then
    echo "stopping the service (graceful, up to 30 s) ..."
    stop_service || die "could not stop instance '$INSTANCE'; nothing was removed"
  fi
  case $PLATFORM in
    launchd) rm -f -- "$AGENT_FILE" "$PLIST_FILE" || die "could not remove the plist files" ;;
    systemd)
      systemctl --user disable "$UNIT" >/dev/null 2>&1 || true
      rm -f -- "$UNIT_FILE" || die "could not remove $UNIT_FILE"
      systemctl --user daemon-reload || note "systemctl --user daemon-reload failed"
      ;;
  esac
  echo "removed the unit of instance '$INSTANCE'"
}

# remove_registry_entry: the last step of uninstall.
remove_registry_entry() {
  rm -f -- "$(registry_file "$INSTANCE")" || die "could not remove $(registry_file "$INSTANCE")"
  echo "removed the registry entry of instance '$INSTANCE'"
}

cmd_uninstall() {
  parse_args uninstall " --keep-data --delete-data --backup --no-backup --confirm --dry-run --platform " "$@"
  begin nodir
  require_supervisor
  local reg_dir=$DIR data_dir=$DIR guard="" del=0 out="" ts why_backup now
  NOANSWER_HINT="use --keep-data or --delete-data (with --backup <path> or --no-backup, and --confirm <instance>)"
  # 1. the guard comes first: it decides whether the data question is asked at all
  if [ -d "$reg_dir" ]; then
    guard=$(delete_guard "$reg_dir")
    data_dir=$(phys_path "$reg_dir") || data_dir=$reg_dir
    DIR=$data_dir
  else
    guard="does not exist"
  fi
  # Refuse what the flags already settle BEFORE the first change.
  if [ "$OPT_DELETE_DATA" -eq 1 ]; then
    [ -z "$guard" ] || die "refusing to delete $data_dir: it $guard. Nothing was changed."
    if [ -n "$OPT_CONFIRM" ] && [ "$OPT_CONFIRM" != "$INSTANCE" ]; then
      die "--confirm '$OPT_CONFIRM' is not the instance name '$INSTANCE'. Nothing was changed."
    fi
  fi
  if [ -n "$OPT_BACKUP" ]; then
    why_backup=$(backup_path_problem "$(abs_path "$OPT_BACKUP")")
    [ -z "$why_backup" ] || die "backup path $(abs_path "$OPT_BACKUP") $why_backup. Nothing was changed."
  fi
  if [ "$DRY" -eq 1 ]; then echo "dry run: nothing is changed"; fi
  # 2. stop the service, remove the unit files (the registry entry stays until the end)
  remove_unit
  # 3. the data question
  if [ "$OPT_KEEP_DATA" -eq 1 ]; then
    del=0
  elif [ "$OPT_DELETE_DATA" -eq 1 ]; then
    del=1
  elif [ -n "$guard" ]; then
    echo "scripts/deploy.sh cannot delete the data directory $data_dir: it $guard. The data stays (no question asked)."
    del=0
  elif [ "$DRY" -eq 1 ]; then
    echo "  then it asks: Delete the instance data (DB, pairing, backups)? [y/N]"
    echo "  data directory: $data_dir"
    return 0
  elif ask_yn "Delete the instance data (DB, pairing, backups)? [y/N]" n; then
    del=1
  fi
  if [ "$del" -eq 0 ]; then
    if [ "$DRY" -eq 1 ]; then return 0; fi
    remove_registry_entry
    echo "the data stays in: $data_dir"
    echo "  (DB, pairing, backups). Install again with: scripts/deploy.sh install $INSTANCE --dir \"$data_dir\""
    return 0
  fi
  [ -z "$guard" ] || die "refusing to delete $data_dir: it $guard. The data stays."
  ts=$(utc_stamp)
  if [ "$DRY" -eq 1 ]; then
    if [ "$OPT_NO_BACKUP" -eq 1 ]; then
      echo "  would delete $data_dir without a backup"
    else
      echo "  would write a backup (${OPT_BACKUP:-$HOME/whatsrust-backups/$INSTANCE-$ts.tar.gz}), then delete $data_dir"
    fi
    return 0
  fi
  # 4. backup (optional) and confirmation
  if [ "$OPT_NO_BACKUP" -eq 1 ]; then
    out=""
  elif [ -n "$OPT_BACKUP" ]; then
    out=$(abs_path "$OPT_BACKUP")
  elif ask_yn "Back up the data first? [Y/n]" y; then
    out="$HOME/whatsrust-backups/$INSTANCE-$ts.tar.gz"
    while :; do
      ask_line "backup file [$out]"
      if [ -n "$ANSWER" ]; then out=$(abs_path "$ANSWER"); fi
      if [ -z "$(backup_path_problem "$out")" ]; then break; fi
      echo "  not accepted: $(backup_path_problem "$out")"
      out="$HOME/whatsrust-backups/$INSTANCE-$ts.tar.gz"
    done
  fi
  if [ -n "$out" ]; then
    why_backup=$(backup_path_problem "$out")
    [ -z "$why_backup" ] || die "backup path $out $why_backup; the data stays in $data_dir (instance '$INSTANCE' is still registered: run uninstall again)"
    write_backup "$out"
  fi
  if [ -n "$OPT_CONFIRM" ]; then
    [ "$OPT_CONFIRM" = "$INSTANCE" ] || die "--confirm '$OPT_CONFIRM' is not the instance name '$INSTANCE'; the data stays in $data_dir"
  else
    ask_line "type the instance name ($INSTANCE) to delete $data_dir"
    [ "$ANSWER" = "$INSTANCE" ] || die "not confirmed; the data stays in $data_dir (instance '$INSTANCE' is still registered: run uninstall again)"
  fi
  # 5. look again right before rm -rf: the directory may have changed since the first check
  guard=$(delete_guard "$reg_dir")
  [ -z "$guard" ] || die "refusing to delete $data_dir: it $guard now (it changed since the first check). The data stays."
  now=$(phys_path "$reg_dir") || now=""
  [ "$now" = "$data_dir" ] || die "refusing to delete: $reg_dir resolves to '$now' now, not to $data_dir. The data stays."
  rm -rf -- "$data_dir" || die "could not delete all of $data_dir (some files remain; the instance is still registered: run uninstall again)"
  echo "deleted $data_dir"
  remove_registry_entry
  echo "Deleting local data does not unlink the device: remove it on the phone (WhatsApp → Linked devices)."
}

main() {
  trap cleanup EXIT
  local cmd=${1:-}
  case $cmd in
    install)
      shift
      cmd_install "$@"
      ;;
    run)
      shift
      cmd_run "$@"
      ;;
    stop)
      shift
      cmd_stop "$@"
      ;;
    re-pair)
      shift
      cmd_repair "$@"
      ;;
    autostart)
      shift
      cmd_autostart "$@"
      ;;
    uninstall)
      shift
      cmd_uninstall "$@"
      ;;
    status)
      shift
      cmd_status "$@"
      ;;
    restart)
      shift
      cmd_restart "$@"
      ;;
    logs)
      shift
      cmd_logs "$@"
      ;;
    upgrade)
      shift
      cmd_upgrade "$@"
      ;;
    -h | --help | help) usage ;;
    '') usage_err "missing command" ;;
    *) usage_err "unknown command: $cmd" ;;
  esac
}

main "$@"
