#!/bin/sh
# Start gate of whatsrust instance @INSTANCE@: the launchd/systemd unit runs this script, not the daemon.
# It starts the daemon only when the instance is paired, or while a QR flow of `scripts/deploy.sh run` or
# `re-pair` is active, so an unpaired instance never runs unattended. Otherwise it logs one line and exits 0,
# which neither supervisor restarts; a DB that cannot be read fails closed, and only a missing sqlite3
# starts the daemon (with a warning). Rendered by scripts/deploy.sh install into <dir>/bin/start-if-paired:
# edit the template, not the copy.
INSTANCE="@INSTANCE@"
unset CDPATH

# The instance directory is the parent of bin/ (the unit also sets it as the working directory).
dir=$(cd "$(dirname "$0")/.." 2>/dev/null && pwd) || exit 1
cd "$dir" || exit 1
daemon="$dir/bin/whatsrust"

# Paired = the device row holds a phone number (the daemon's own test). No DB file: not paired.
#   yes / no   the answer of sqlite3
#   error      sqlite3 could not read the DB three times in a row (locked, permission, damaged): FAIL CLOSED
#   unknown    sqlite3 is not installed: the only case that starts without proof (see below)
# -init /dev/null: a ~/.sqliterc (for example ".headers on") must not change the output. Only stdout counts.
paired=unknown
read_error=""
if command -v sqlite3 >/dev/null 2>&1; then
  if [ ! -e whatsapp.db ]; then
    paired=no
  else
    errf=$(mktemp "${TMPDIR:-/tmp}/start-if-paired.XXXXXX" 2>/dev/null) || errf=/dev/null
    paired=error
    tries=0
    while [ "$tries" -lt 3 ] && [ "$paired" = error ]; do
      tries=$((tries + 1))
      if out=$(sqlite3 -batch -init /dev/null -readonly -cmd '.timeout 3000' whatsapp.db "SELECT pn FROM device WHERE id = 1;" 2>"$errf"); then
        out=$(printf '%s' "$out" | tr -d '[:space:]')
        if [ -n "$out" ]; then paired=yes; else paired=no; fi
      else
        read_error=$(head -n 1 "$errf" 2>/dev/null | cut -c1-80)
        case $read_error in
          *"no such table"*) paired=no ;;
          *) if [ "$tries" -lt 3 ]; then sleep 1; fi ;;
        esac
      fi
    done
    if [ "$errf" != /dev/null ]; then rm -f "$errf"; fi
  fi
fi

if [ "$paired" = yes ]; then
  exec "$daemon"
fi

# A QR flow is marked by .qr-flow: line 1 is the PID of the deploy.sh process that runs it, line 2 that
# process's start time (`ps -o lstart=`). The flow counts as active only when ALL of these hold: the PID is a
# number above 1; the marker is not older than 15 minutes; the PID is alive; and the live process has the
# recorded start time, so a reused PID (after a reboot or a SIGKILL of deploy.sh) never counts. `kill -0`
# sends NO signal: it only asks whether the PID exists (the one use of kill in this project). Any other case
# is a stale marker: it is ignored and removed.

# norm_ws: read stdin, squeeze white space to single spaces, drop leading and trailing space.
norm_ws() {
  tr -s '[:space:]' ' ' | sed 's/^ //; s/ $//'
}

qr_flow=no
if [ -f .qr-flow ]; then
  pid=$(sed -n 1p .qr-flow 2>/dev/null)
  started=$(sed -n 2p .qr-flow 2>/dev/null | norm_ws)
  case $pid in
    '' | *[!0-9]*) ;;
    *)
      if [ "${#pid}" -le 9 ] && [ "$pid" -gt 1 ] && [ -n "$started" ] &&
        [ -z "$(find .qr-flow -mmin +15 2>/dev/null)" ]; then
        if kill -0 "$pid" 2>/dev/null; then
          now=$(ps -p "$pid" -o lstart= 2>/dev/null | norm_ws)
          if [ -n "$now" ] && [ "$now" = "$started" ]; then qr_flow=yes; fi
        fi
      fi
      ;;
  esac
  if [ "$qr_flow" = no ]; then rm -f .qr-flow; fi
fi

if [ "$qr_flow" = yes ]; then
  exec "$daemon"
fi

if [ "$paired" = error ]; then
  echo "whatsrust $INSTANCE: cannot read whatsapp.db (${read_error:-unknown error}); not starting" >&2
  exit 0
fi

# Trade-off: without sqlite3 the gate cannot tell. Refusing would also keep a paired instance down after every
# restart, so it starts the daemon and says so in the log. install warns when sqlite3 is missing.
if [ "$paired" = unknown ]; then
  echo "whatsrust $INSTANCE: cannot tell whether it is paired (sqlite3 is not installed); starting anyway" >&2
  exec "$daemon"
fi

echo "whatsrust $INSTANCE: not paired, not starting; run: scripts/deploy.sh run $INSTANCE" >&2
exit 0
