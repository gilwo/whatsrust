# 0019. External-content FTS5 with sync triggers

**Status:** Accepted  
**Date:** 2026-06-17

## Context

FTS5 index structure options:
- **External-content** (`content='messages'`): FTS index points at external table, no text duplication
- **Standalone**: FTS table stores its own copy of indexed text (duplication)
- **Contentless**: FTS index only, no stored text (no snippet support, restricted DML)

The unified `messages` table (ADR 0009) is the single source of truth for all message text (live + backfilled).

## Decision

**FTS5 index structure = EXTERNAL-CONTENT** over the `messages` table:
```sql
CREATE VIRTUAL TABLE messages_fts USING fts5(
    body_text,
    content='messages',
    content_rowid='message_id',
    tokenize='unicode61 remove_diacritics 2'
);
```

FTS index points at `messages.body_text`, no text duplication.

**Maintained by standard AFTER INSERT/UPDATE/DELETE sync-trigger trio** on `messages`:
```sql
-- INSERT: add to FTS
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body_text) VALUES (new.message_id, new.body_text);
END;

-- UPDATE: update FTS
CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages BEGIN
    UPDATE messages_fts SET body_text = new.body_text WHERE rowid = old.message_id;
END;

-- DELETE: remove from FTS
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    DELETE FROM messages_fts WHERE rowid = old.message_id;
END;
```

Backfilled rows flow through same triggers (no separate path).

**Repair mechanism** if FTS drifts from base table:
```sql
INSERT INTO messages_fts(messages_fts) VALUES('rebuild');
```

**Index REAL natural-language text ONLY** (same surface as ADR 0016 embeddable set: bodies/captions/poll/contact/location). Non-content rows have `body_text` = NULL → nothing indexed (no FTS hits on synthetic labels like `[sticker 40KB]`).

**Tokenizer = `unicode61 remove_diacritics 2`** (from ADR 0018).

## Consequences

**Positive:**
- No text duplication (single source of truth in `messages`)
- Standard FTS5 trigger pattern (well-documented, stable)
- Repair mechanism (`rebuild`) if drift occurs
- Backfilled messages auto-indexed via same triggers (uniform path)
- External-content allows separate retention policies (could purge old FTS entries without losing message text)

**Negative:**
- Three triggers per table DML (small overhead on insert/update/delete)
- Trigger drift possible if triggers disabled or DML bypasses them (mitigated by `rebuild`)
- External-content requires explicit trigger maintenance (vs standalone auto-sync)

**Rejected:**
- **Standalone FTS** (option B): duplicates all indexed text, wastes storage, dual source of truth
- **Contentless FTS** (option C): no snippet support (can't show match context), restricted DML (delete requires explicit rowid)
