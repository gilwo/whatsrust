# Supervisor templates

Templates that run the whatsrust daemon under a supervisor, so it restarts after a crash.
Do not install them by hand: `scripts/deploy.sh` renders them and loads the result.

| File | Platform | Rendered to |
|---|---|---|
| `whatsrust.plist.in` | macOS (launchd LaunchAgent) | `<instance directory>/local.whatsrust.<instance>.plist`. `run` loads it from there. `autostart on` copies it to `~/Library/LaunchAgents/`. |
| `whatsrust.service.in` | Linux (systemd user unit) | `${XDG_CONFIG_HOME:-~/.config}/systemd/user/whatsrust-<instance>.service`. `autostart on` enables it. |
| `start-if-paired.sh` | both | `<instance directory>/bin/start-if-paired` (mode 755): the program that both units run. |

## The start gate

Both units run `bin/start-if-paired`, not the daemon. The gate runs in the instance directory and:

1. exec-s `bin/whatsrust` when the DB is paired (`sqlite3 -batch -init /dev/null -readonly -cmd '.timeout 3000'
   whatsapp.db "SELECT pn FROM device WHERE id = 1"` prints a value; only stdout counts, so a `~/.sqliterc` cannot
   change the answer; three tries);
2. otherwise exec-s it while a QR flow is active: `<dir>/.qr-flow` holds the PID of the `scripts/deploy.sh run` or
   `re-pair` process (line 1) and that process's start time (`ps -o lstart=`, line 2). The marker counts only when
   the PID is above 1, the file is at most 15 minutes old, the PID is alive (`kill -0` sends no signal) and the live
   process still has the recorded start time, so a reused PID never counts. Anything else is a stale file: it is
   ignored and removed;
3. otherwise, when the DB cannot be read (locked, no permission, damaged) it FAILS CLOSED: it logs
   `whatsrust <instance>: cannot read whatsapp.db (<reason>); not starting` and exits 0;
4. otherwise (not paired) it logs `whatsrust <instance>: not paired, not starting; run: scripts/deploy.sh run
   <instance>` to stderr (`daemon.log` or the journal) and exits 0, which neither supervisor restarts.

Only a missing `sqlite3` makes the gate start the daemon without proof: it writes a warning line. The trade-off is
deliberate: refusing would keep a paired instance down after every restart. `scripts/deploy.sh install` warns when
`sqlite3` is missing. Known limits: the gate runs only at a start, so if the device is unlinked on the phone WHILE
the daemon runs, the daemon keeps running and shows a QR code until its next restart. A SIGKILL or a power loss of
`deploy.sh` during a QR flow cannot be trapped; the service then runs until the next stop or reboot, and the
marker rules above keep it from starting again. The gate needs a `ps` that supports `-p <pid> -o lstart=` (macOS,
procps).

## Ownership markers

`scripts/deploy.sh` touches only what it made. Each unit carries `managed-by: scripts/deploy.sh` (plist: an XML
comment plus the label; systemd: `# managed-by: scripts/deploy.sh instance=<name>`), and each instance directory
holds `.whatsrust-deploy` (`managed by scripts/deploy.sh; instance=<name>`). The script refuses to install over,
overwrite, remove, stop or boot out a unit without the marker, and to install into or delete a directory without it.

Both templates restart the daemon after any non-zero exit (a panic exits 101) and never after exit 0
(a graceful stop). The wait between two starts is 10 s.

| Setting | launchd | systemd |
|---|---|---|
| Restart | `KeepAlive` with `SuccessfulExit` false | `Restart=on-failure` |
| Wait between starts | `ThrottleInterval` 10 | `RestartSec=10` |
| Give up | never | `StartLimitIntervalSec=300`, `StartLimitBurst=5` |
| Stop | SIGTERM, then SIGKILL after `ExitTimeOut` 30 | `KillSignal=SIGTERM`, `TimeoutStopSec=30` |
| Start | `RunAtLoad` true (`launchctl bootstrap` starts the job) | `systemctl --user start` |
| Program | `ProgramArguments`: `start-if-paired` | `ExecStart="…/start-if-paired"` |
| Standard input | not set (`/dev/null`) | `StandardInput=null` |
| Logs | `<dir>/logs/daemon.log` | the journal |

`install` never starts the service and never enables autostart. An unpaired instance never runs
unattended: only `run` and `re-pair` start one, while the terminal shows the QR code (the gate enforces it).

## Placeholders

`scripts/deploy.sh install` replaces these, with plain bash string replacement. It escapes each value
for the target format: XML escapes in the plist, `%` written as `%%` in the systemd unit.

| Placeholder | Value |
|---|---|
| `@INSTANCE@` | the instance name, for example `main` (also replaced in `start-if-paired.sh`) |
| `@LABEL@` | the launchd label, `local.whatsrust.<instance>` |
| `@UNIT@` | the systemd unit name, `whatsrust-<instance>.service` |
| `@DIR@` | the instance directory: the daemon's working directory and home of its state |
| `@START@` | `<dir>/bin/start-if-paired`, the start gate that the unit runs (it starts `<dir>/bin/whatsrust`) |
| `@ENV_FILE@` | `<dir>/.env`, passed to the daemon as `WHATSRUST_ENV_FILE` |
| `@LOG@` | `<dir>/logs/daemon.log` (launchd only: systemd logs to the journal) |

The script refuses a directory with control characters, `"`, `\`, `$`, a backtick or `@` (the placeholder
character). Spaces are fine.

## Use

```bash
scripts/deploy.sh install demo --yes --dry-run                       # print the summary and the rendered unit
scripts/deploy.sh install demo --yes --dry-run --platform systemd    # render the Linux unit on a Mac
scripts/deploy.sh --help
```

Details, the owner checklist and a demo playbook: `docs/HOWTO.md` section 3b.
