# 0017. Multi-model vector retention with explicit per-model purge

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Users may want to switch embedding models (e.g., upgrade to better model, try different dimension). Two competing concerns:
- Model switches should be cheap and reversible (don't destroy old vectors just to try something new)
- Non-active model vectors consume storage indefinitely

Earlier considered option (auto-stale and re-embed on model change) would silently destroy old vectors and force full rebuild on switch-back.

## Decision

**Schema:** `embeddings` table has composite primary key `(message_id, model_id)` with columns `(dim, vec BLOB)`.

**Only ONE model active at a time.** Search ALWAYS filters `WHERE model_id = <active>`. Non-active model vectors are pure **cold storage** kept only for cheap switch-back; never queried.

**Model switch is free + reversible:**
- Switch X→Y: drain worker starts embedding new messages for Y (old X vectors untouched)
- Switch Y→X: old X vectors immediately usable, drain worker auto-completes any X gaps via set-difference

**NO automatic re-embedding** on model change.

**Explicit per-model purge:** deliberate admin operation (API/MCP/CLI):
```sql
DELETE FROM embeddings WHERE model_id = ?;
-- followed by PRAGMA incremental_vacuum
```
Returns count + bytes reclaimed. NEVER automatic. **Destructive but losslessly reapplicable** (message text is retained source of truth; embeddings are deterministic for fixed model → re-drain reproduces them).

**Drain-work derivation = set difference (no per-message done flag):**
"Embed for active model M" = embeddable messages (ADR 0016 classification) lacking an `(message_id, M)` row in `embeddings`:
```sql
SELECT message_id, body_text FROM messages
WHERE embed_status = 'pending'  -- embeddable, not skipped/failed
  AND NOT EXISTS (
    SELECT 1 FROM embeddings
    WHERE embeddings.message_id = messages.message_id
      AND embeddings.model_id = ?
  )
```

The `embeddings` table IS the source of truth for "what's embedded". Per-model failure tracking (sidecar rejects text, cap 3) is separate lightweight state (in-memory or `embed_failures(message_id, model_id, attempts)` table).

## Consequences

**Positive:**
- Model switch-back is instant (old vectors still present) and costs nothing
- Model experimentation is cheap (try Y, revert to X without rebuild)
- Drain logic is stateless (set-difference query, no bookkeeping)
- Purge is explicit and deliberate (pairs with storage watchdog as manual space-reclaim remedy)
- Purge is losslessly reapplicable (message text is source of truth)

**Negative:**
- Non-active model vectors accumulate storage indefinitely unless manually purged
- No visibility into "which models have vectors" without querying `embeddings` table
- Per-model failure tracking requires separate state (attempt counters not in `embeddings` schema)

**Superseded:**
- Earlier option A (auto-stale on model change) would silently destroy old vectors and force rebuild on switch-back — rejected as user-hostile for model experimentation

**Future:**
- Multi-active-model search (query multiple models, fuse rankings) — schema supports it, just needs search-layer changes
