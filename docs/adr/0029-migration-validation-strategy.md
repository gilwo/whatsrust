# 0029. Migration validation strategy

**Status:** Accepted  
**Date:** 2026-06-22

## Context

After schema migration commits, we need confidence the new schema is operational before starting the daemon. Validation scope tension: comprehensive (full integrity_check, minutes) vs targeted (smoke probes, subsecond).

**What validation SHOULD catch:**
- Migration DDL typo (missing column, wrong type)
- FTS5 triggers didn't wire up
- Foreign-key constraint broken
- Embedding BLOB roundtrip fails

**What validation SHOULD NOT gate:**
- Pre-existing data corruption (not introduced by migration)
- Performance regressions (measurable at runtime)

## Decision

**Validation = V1 (structural) + V3 (smoke probes):**

**V1 — Structural checks:**
- `PRAGMA user_version` == `CURRENT_SCHEMA_VERSION`
- Expected tables exist via `PRAGMA table_list`
- Expected columns exist via `PRAGMA table_info(messages)`, `PRAGMA table_info(embeddings)`, etc.

**V3 — Smoke probes:**
- **FTS5 trigger wiring:** INSERT test row into `messages` → query `messages_fts` → row appears → DELETE test row. Proves sync triggers wired up correctly.
- **Set-difference drain query:** SELECT embeddable messages lacking `(message_id, active_model)` in `embeddings` (LEFT JOIN ... WHERE vec IS NULL). Query succeeds (may return zero rows, that's fine — proves JOIN syntax correct).
- **Vector roundtrip:** INSERT canned `(message_id, model_id, dim, vec)` → SELECT → deserialize BLOB → verify length == dim → DELETE. Proves BLOB encoding correct.

**V2 (full integrity_check) SKIPPED as a gate:** `PRAGMA integrity_check` takes seconds-to-minutes on large DB and checks pre-existing corruption (not what schema migration introduces). Available behind a flag (`--validate-full`) for manual debugging, but not a startup blocker.

**Validation failure → halt + instruct** (ADR 0030 circuit-breaker). Daemon DOES NOT START on failed validation. Prevents running on corrupt schema.

## Consequences

**Positive:**
- V1+V3 run in <1 second (subsecond on small DB, ~1s on large)
- Smoke probes catch migration-introduced errors (typos, missing triggers, FK breaks)
- Skipping integrity_check as gate avoids blocking on pre-existing corruption

**Negative:**
- Smoke probes may miss edge-case schema bugs (rare DDL interactions) — accept as "validated enough to start" rather than "proven perfect"
- Test rows briefly exist in tables during validation (cleaned up immediately) — harmless but conceptually impure

**Future:**
- Add V3 probe for `metadata` table (INSERT/SELECT/DELETE key-value pair)
- If repeated validation failures in production, expand V3 coverage (measure before adding).

**Hardening (2026-06-24, v2 review):**
- **Validation gap fix (B1):** Add `schema_validated_version` key in metadata, set only after validation passes; startup re-validates whenever != CURRENT. Closes shutdown-interrupted-validation gap.
- **Semantic validation accepted (M5):** Current V1+V3 approach accepted as-is (TX rollback prevents partial DDL; full-scan prohibitive). Optional future `--validate-full` flag for manual debugging. Documented accepted risk.
