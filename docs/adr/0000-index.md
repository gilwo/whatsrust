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

## Future ADRs

Start numbering at 0026. Follow the established format. Keep each ADR focused on one decision.
