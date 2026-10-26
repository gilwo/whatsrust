# 0049. Deploy as a supervised user service, with a start gate for unpaired instances

**Status:** Accepted
**Date:** 2026-10-26

## Context

On 2026-10-19 a UTF-8 slicing panic killed a 9 h live session, and nothing restarted the daemon (roadmap P0, Q-PANIC). The daemon ran in a terminal, from the repo root or a dev profile (`runs/<profile>/`, F9), with the binary in `target/release/`.

Facts that shape a supervised deployment (verified in `src/main.rs`, `src/storage.rs`):

- All state is relative to the working directory: `whatsapp.db` (+ WAL/SHM), the instance lock, `whatsapp.db.backups/`, and `.env` unless `WHATSRUST_ENV_FILE` points elsewhere.
- Exit codes: 0 = graceful stop (SIGINT/SIGTERM drain the queues and back up the DB); 1 = startup refusal or an error; **101 = a panic** (the panic hook calls `exit(101)`, so a panic ends with an exit code, not a signal).
- stdin may be `/dev/null`: the REPL ends on EOF and the daemon keeps running.
- An unpaired daemon connects, shows a QR code and waits. A supervised service has no terminal, so nobody can scan that code; the daemon would wait unattended.
- Paired means `SELECT pn FROM device WHERE id = 1` is non-empty (`Store::is_paired`).

The owner decided (2026-10-24 → 26): a separate deployment script (not the dev script `run-profile.sh`); each instance outside the repo; an interactive install wizard; and the rule **"an unpaired instance never runs unattended"**.

## Decision

1. **A user service per instance.** `scripts/deploy.sh` runs the daemon as a launchd LaunchAgent (macOS) or a systemd user unit (Linux), rendered from `deploy/supervisor/`.
2. **The instance lives outside the repo,** in `~/Library/Application Support/whatsrust/<instance>/` or `${XDG_DATA_HOME:-~/.local/share}/whatsrust/<instance>/` (or an absolute `--dir`), with **its own copy of the binary** in `bin/`. A rebuild of the repo never changes a running service; `upgrade` replaces the copy and restarts. A registry file in `${XDG_CONFIG_HOME:-~/.config}/whatsrust/deploy/` maps the instance to its directory.
3. **Restart on any non-zero exit, never after exit 0,** at most every 10 s (launchd `KeepAlive {SuccessfulExit=false}` + `ThrottleInterval 10`; systemd `Restart=on-failure` + `RestartSec=10`, start limit 5 in 300 s). A signal-only policy would miss panics (exit 101). The cost: a config error (exit 1) restarts every 10 s until it is fixed; systemd gives up after the start limit, launchd keeps trying.
4. **A start gate.** The unit runs `bin/start-if-paired`, not the daemon. The gate starts the daemon only when the instance is paired, or while a `run`/`re-pair` QR flow is active (`.qr-flow` holds the PID and start time of the live `deploy.sh` process, so a reused PID never matches, and expires after 15 minutes). Otherwise it logs one line and exits 0, which neither supervisor restarts. So login, crash restarts and upgrades never start an unpaired instance. Without `sqlite3` the state is unknown and the gate starts the daemon with a warning (a refusal would keep a paired instance down after every restart); `install` warns when `sqlite3` is missing. A DB the gate cannot read fails closed (no start).
5. **Pairing only in front of the user.** `install` starts nothing. `run` starts a paired instance in the background; for an unpaired one it starts the service, draws its QR code in the terminal and waits; Ctrl+C or a 5-minute timeout stops the service again. `autostart on` refuses an unpaired instance.
6. **Ownership markers.** `install` accepts only a new, empty or marked directory (`.whatsrust-deploy`); `uninstall` deletes data only from a directory with the marker for that instance, after an optional verified backup and a typed confirmation. Each rendered unit carries `managed-by: scripts/deploy.sh`; the script refuses to overwrite, remove or stop a unit without it.
7. **Graceful restart on macOS** is `launchctl kill SIGTERM` → wait for the exit → `launchctl kickstart`, because the signal of `kickstart -k` is not documented.

## Consequences

- A crash no longer ends a live run; `status` shows the last exit code (1 = startup refusal, 101 = panic).
- **Known limit:** the gate runs only at a start. If the device is unlinked on the phone while the daemon runs, the daemon clears its credentials and shows a QR code until its next restart. Only a daemon change can close this (roadmap P2: exit on `LoggedOut` when supervised).
- `deploy.sh` is large (about 3500 lines of bash). A port to a Rust `whatsrust deploy` subcommand is a P3 roadmap item.
- Tested with a stub harness (bash 3.2 and 5), linters and three adversarial review passes; **not live-tested on real launchd or systemd** (the owner chose no smoke test on 2026-10-26).
- Operator documentation: `docs/HOWTO.md` §3b (commands, the wizard, the gate, the owner checklist to move a live state, a demo playbook). Feature detail: `docs/plans/FEATURES.md` F10.

## Alternatives considered

- **Supervise the dev profiles (`run-profile.sh`):** rejected by the owner; the dev script stays for dev runs.
- **Run the service from the repo or `target/release/`:** rejected; a rebuild would silently change the next restart, and `git clean` could remove the state.
- **Restart only on signals:** rejected; it misses panics (exit 101).
- **Pair in a separate foreground process, then hand over to the service:** rejected; the history package the phone sends right after linking (ADR 0048) could fall into the handover gap.
