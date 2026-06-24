# Architecture Decision Records

This directory contains Architecture Decision Records (ADRs) for the whatsrust project.

## Convention

- **Format:** Modified [MADR](https://adr.github.io/madr/) style
- **Filename:** `NNNN-kebab-case-title.md` (zero-padded 4-digit sequence)
- **Structure:** Title, Status, Date, Context, Decision, Consequences
- **Status:** Accepted (for committed decisions), Proposed (for pending), Superseded (when replaced)

## Index

| ADR | Title | Date | Status |
|-----|-------|------|--------|
| [0001](0001-increase-tokio-worker-stack-to-8mb.md) | Increase tokio worker thread stack to 8 MB | 2026-06-17 | Accepted |
| [0002](0002-rebase-wa-rs-fork-to-upstream-v0.6.0.md) | Rebase wa-rs fork onto upstream v0.6.0 before building history features | 2026-06-17 | Accepted |
| [0003](0003-per-chat-backward-pagination-fetch-model.md) | Per-chat backward-pagination fetch model with resumable cursor | 2026-06-17 | Accepted |
| [0004](0004-communities-out-of-scope-v1.md) | Communities out of scope for v1 | 2026-06-17 | Accepted |
| [0005](0005-lazy-media-hydration-with-persistent-refs.md) | Store media refs always, hydrate bytes lazily | 2026-06-17 | Accepted |
| [0006](0006-stateless-embedder-sidecar.md) | Embeddings via stateless sidecar binary | 2026-06-17 | Accepted |
| [0007](0007-fts5-baseline-with-optional-vector-rerank.md) | FTS5 always-on baseline with optional vector rerank | 2026-06-17 | Accepted |
| [0008](0008-vector-storage-in-sqlite-blob-with-rust-cosine.md) | Vector storage as BLOB in SQLite, cosine rerank in Rust | 2026-06-17 | Accepted |
| [0009](0009-unified-messages-table-migration.md) | Unified messages table via rename-in-place migration | 2026-06-17 | Accepted |
| [0010](0010-durable-backfill-job-queue.md) | Durable backfill-job queue with async progress tracking | 2026-06-17 | Accepted |
| [0011](0011-fetch-history-api-surface.md) | Fetch history API surface: trigger, status, cancel, SSE progress | 2026-06-17 | Accepted |
| [0012](0012-indefinite-message-retention.md) | Indefinite message retention (no time-based deletion) | 2026-06-17 | Accepted |
| [0013](0013-storage-growth-watchdog.md) | Storage growth watchdog with WAL checkpoint and baseline tracking | 2026-06-17 | Accepted |
| [0014](0014-single-content-extraction-path.md) | Single content extraction path for live and backfilled messages | 2026-06-17 | Accepted |
| [0015](0015-embedding-drain-worker.md) | Embedding-drain worker with sidecar-down resilience | 2026-06-17 | Accepted |
| [0016](0016-embeddable-text-definition.md) | Embeddable text is genuine natural language only | 2026-06-17 | Accepted |
| [0017](0017-multi-model-vector-retention-explicit-purge.md) | Multi-model vector retention with explicit per-model purge | 2026-06-17 | Accepted |
| [0018](0018-multilingual-fts-and-vector-strategy.md) | Multilingual FTS and vector strategy | 2026-06-17 | Accepted |
| [0019](0019-external-content-fts5-with-sync-triggers.md) | External-content FTS5 with sync triggers | 2026-06-17 | Accepted |
| [0020](0020-conservative-backfill-anti-ban-pacing.md) | Conservative backfill anti-ban pacing | 2026-06-17 | Accepted |
| [0021](0021-daemon-side-uniform-safety-enforcement.md) | Daemon-side uniform safety enforcement vs misbehaving agents | 2026-06-17 | Accepted |
| [0022](0022-fail-closed-config-safety-with-scoped-overrides.md) | Fail-closed config safety with scoped DANGEROUSLY overrides | 2026-06-17 | Accepted |
| [0023](0023-env-var-config-with-dotenv.md) | Env-var config with dotenv support | 2026-06-17 | Accepted |
| [0024](0024-sidecar-jsonrpc-protocol-schema.md) | Sidecar JSON-RPC protocol schema | 2026-06-17 | Accepted |
| [0025](0025-layered-testing-strategy.md) | Layered testing strategy with two fake seams | 2026-06-17 | Accepted |
| [0026](0026-backfill-worker-topology-and-abort-granularity.md) | Backfill worker topology and abort granularity | 2026-06-22 | Accepted |
| [0027](0027-single-connection-db-contention-model.md) | Single-connection DB contention model | 2026-06-22 | Accepted |
| [0028](0028-staged-migration-mode-with-validation.md) | Staged migration mode with pre-migration backup and validation | 2026-06-22 | Accepted |
| [0029](0029-migration-validation-strategy.md) | Migration validation strategy | 2026-06-22 | Accepted |
| [0030](0030-migration-circuit-breaker.md) | Migration circuit-breaker with rollback and migrate flags | 2026-06-22 | Accepted |
| [0031](0031-single-integer-schema-version-invariant.md) | Single-integer schema version invariant | 2026-06-22 | Accepted |
| [0032](0032-fts5-availability-probe-at-migration-boundary.md) | FTS5 availability probe at migration boundary | 2026-06-22 | Accepted |
| [0033](0033-fetch-target-model-contained-c.md) | Fetch target model: contained-C (single target kind, autonomy backstop) | 2026-06-22 | Accepted |
| [0034](0034-fuzzy-progress-for-auto-continuing-fetch.md) | Fuzzy progress for auto-continuing fetch targets | 2026-06-22 | Accepted |
| [0035](0035-cooldown-and-dedup-via-single-frontier.md) | Cooldown and dedup via single frontier model | 2026-06-22 | Accepted |
| [0036](0036-metadata-kv-table.md) | Generic metadata KV table for singleton scalars | 2026-06-22 | Accepted |

## Revised ADRs

The following ADRs were revised during the 2026-06-18 design review resolution:
- **0002** — Added GO/NO-GO gate criteria + minimal spike result (G1/G2/G3 + pivot paths + spike findings)
- **0003** — Noted composable-stop-conditions schema superseded by ADR 0033 contained-C
- **0011** — Noted request/response schema refined by ADR 0033/0034/0035
- **0013** — Added metadata table seed-on-absence behavioral note (ADR 0036)

The following ADRs received hardening notes during the 2026-06-23 v2 design review resolution (2026-06-24):
- **0002** — GO criteria extended: requires live smoke test (fork R1) + actual compile (M1)
- **0013** — Watchdog baseline seeding revised: at migration completion, not lazy (B2)
- **0015** — Spawn location (B3), no-embedder idle (M2), loading timeout (B4), decoupled from backfill (R3)
- **0017** — Per-model purge uses incremental_vacuum, not full VACUUM (R-prior5)
- **0021** — Global backfill queue-depth limit (R5)
- **0024** — Loading timeout (B4)
- **0026** — Stuck-anchor guard (R2), R3 decoupling note
- **0028** — Shutdown-race fix (B1)
- **0029** — Validation gap fix (B1), semantic validation accepted (M5)
- **0030** — Pin consistency check (R4)
- **0032** — Startup FTS5 probe (M4)
- **0033** — Autonomy backstop global-config-only (fork M3)
- **0035** — Cooldown TOCTOU fix (B5)
- **0036** — Watchdog baseline seeding revised (B2)

## Future ADRs

Start numbering at 0037. Follow the established format. Keep each ADR focused on one decision.
