# 0023. Env-var config with dotenv support

**Status:** Accepted  
**Date:** 2026-06-17

## Context

whatsrust currently has NO config file. Config = `BridgeConfig` struct + `Default` impl, populated in `main.rs` from env vars (`WHATSRUST_PORT`, `WHATSAPP_ALLOWED`, `WHATSAPP_PAIR_PHONE`, `BACKUP_DIR`, `WHATSRUST_SEND_BURST`, etc.).

New features (backfill, embeddings, safety knobs) add ~10-15 new config vars. Typing long env-var strings is tedious; users want a config file.

Options:
- TOML config file → new precedence model (file vs env), new parsing dependency, schema drift risk
- `.env` file → dotenv convention (fills unset env vars only), minimal dependency, zero precedence complexity

## Decision

**Config mechanism = env-var semantics + `.env` FILE support:**
- All config = `WHATSRUST_*` env vars read in `main.rs`
- `BridgeConfig` struct + defaults unchanged (env vars are source of truth)
- NO TOML, NO parsed-config struct, NO new precedence model

**Precedence = real env vars OVERRIDE `.env`** (dotenv convention):
- `.env` file fills UNSET env vars only
- Real shell env vars take precedence (export overrides .env)

**Load `./.env` (or `WHATSRUST_ENV_FILE=/path`) ONCE early in `main()` BEFORE any var reads:**
- Absent file → silent no-op (zero-config must still work)
- Dotenv errors (malformed file) → log warning + continue (don't block startup on bad .env)

**Dependency = `dotenvy` crate:**
- Probed: adds exactly 1 crate, ZERO transitive deps
- Handles quoting/escape/export/comment edge cases (better than hand-rolled ~30-50 line parser + test burden)
- Negligible footprint (aligns with lean-tree ethos)

**Files:**
- **Committed `.env.example`** documents EVERY `WHATSRUST_*` var:
  - Default value
  - For safety knobs (ADR 0022): causal-risk comment + exact override flag
  - This is where "documented danger" lives (user copies to `.env`, edits)
- **Real `.env` GITIGNORED** (may hold pair phone, allowlist, secrets)

**Example `.env.example` entry:**
```sh
# Backfill pacer interval (seconds per batch, default 4).
# Floor: 3s (rapid PDO to own phone is detectable automation, risks ban).
# Override: WHATSRUST_DANGEROUSLY_ALLOW_FAST_BACKFILL=1
WHATSRUST_BACKFILL_INTERVAL_SECS=4
```

## Consequences

**Positive:**
- Users get a config file (`.env`) without new precedence complexity
- Dotenv is industry-standard convention (12-factor app, used by Docker/Rails/Laravel/etc)
- Real env vars still override (CI/deploy can inject secrets without editing files)
- Zero-config still works (absent `.env` is no-op)
- `dotenvy` handles edge cases (quoting, escape, `export FOO=bar`, comments) without hand-rolling parser
- Committed `.env.example` is self-documenting (ADR 0022 causal warnings live here)

**Negative:**
- Dotenv is less structured than TOML (no nested sections, no arrays, just flat `KEY=value`)
- No schema validation (typos in `.env` silently ignored vs TOML parse error)
- `dotenvy` dependency adds 1 crate (but 0 transitive deps, negligible)

**Rejected:**
- **TOML config file** — new precedence model (file vs env vs CLI args), new parsing dep (`toml` crate + `serde`), schema drift risk (BridgeConfig struct duplication), added complexity
- **Hand-rolled dotenv parser** — ~30-50 lines + test burden for quoting/escape/export/comment edge cases; `dotenvy` is battle-tested and tiny

**Deferred:**
- Config file reload (runtime `SIGHUP` or API endpoint) — currently requires restart; hot-reload adds watcher + validation + state reconciliation complexity
