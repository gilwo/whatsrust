# 0009. Unified messages table via rename-in-place migration

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Current schema: `inbound_messages` table stores live-received messages only. Historical backfill adds a second ingest path (backfilled messages) with overlapping fields but new requirements:
- `source` column: `live` vs `backfill` (dedup live duplicates during backfill)
- `from_me` column: backfill includes sent messages, live ingest only receives
- `embed_status` column: `pending` → `embedded` → `failed` (async embedding workflow)
- Media linkage: foreign key to `media_refs` table

Options:
1. Separate `backfilled_messages` table → complex JOIN for unified search, schema duplication
2. `messages` union view over two tables → can't insert into view, fragile
3. Rename `inbound_messages` → `messages`, add columns → single source of truth, live becomes full writer

## Decision

**Unified `messages` table** via rename-in-place migration (v7 → v8):
```sql
ALTER TABLE inbound_messages RENAME TO messages;
ALTER TABLE messages ADD COLUMN source TEXT DEFAULT 'live'; -- 'live' | 'backfill'
ALTER TABLE messages ADD COLUMN from_me INTEGER DEFAULT 0;
ALTER TABLE messages ADD COLUMN embed_status TEXT; -- NULL | 'pending' | 'embedded' | 'failed'
ALTER TABLE messages ADD COLUMN media_ref_id INTEGER REFERENCES media_refs(id);
```

**Live ingest becomes full writer:** persist media refs + mark embed_status=pending for new live messages (same path as backfill).

**New sibling tables:**
- `media_refs(id, chat_jid, message_id, media_key, direct_path, mime, size, width, height)`
- `embeddings(message_id, model_id, dim, vec BLOB)`
- `backfill_cursor(chat_jid, oldest_anchor, more_remain, exhausted, last_backfill_at)`

**Update points:** `insert_inbound` (→ `insert_message`), `search_inbound` (→ `search_messages`), 2 delete paths, `prune_old_data`, v5→v6 migration block.

## Consequences

**Positive:**
- Single table = single search query (no UNION, no JOIN complexity)
- Schema evolution isolated to one table
- Live + backfill share embedding workflow (no parallel paths)
- `source` column enables dedup + analytics (live vs backfill ratio)

**Negative:**
- Migration renames table (requires careful rollback plan if v8 fails)
- Live ingest adds media_refs write + embed_status update (minor overhead, ~1ms per message)

**Rollback:**
- v8 migration failure → restore from SQLite backup (WAL mode supports point-in-time rollback via wal checkpoint + backup)
