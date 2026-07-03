# 0019. External-content FTS5 with sync triggers

**Status:** Accepted  
**Date:** 2026-06-17  
**Updated:** 2026-07-02 — Added the Ranking (BM25) subsection (verified against the bundled SQLite 3.50.2).

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
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
```

FTS index points at `messages.body_text`, no text duplication.

> **2026-07-02 correction (M1.1):** Two fixes to the DDL below.
> 1. `content_rowid` must be the table's **INTEGER** `PRIMARY KEY` alias (`id`), not the
>    TEXT `message_id` column — an FTS5 external-content rowid is a rowid. Earlier drafts
>    used `message_id`; the implementer spec (`docs/plans/2026-06-17-...-design.md`) already
>    uses `id`, which is correct.
> 2. The DELETE/UPDATE triggers must use the FTS5 **`'delete'` special-insert command with
>    the *old* column values** — `INSERT INTO messages_fts(messages_fts, rowid, body_text)
>    VALUES('delete', old.id, old.body_text)` — **not** a plain `DELETE FROM messages_fts` /
>    `UPDATE messages_fts`. External-content tables don't store the indexed text, so FTS5
>    needs the old values to reverse the index entries; a plain delete/update silently
>    corrupts the index (the `rebuild` repair exists for exactly this failure). This is the
>    canonical pattern from the SQLite FTS5 docs (§ "External Content Tables").

**Maintained by standard AFTER INSERT/UPDATE/DELETE sync-trigger trio** on `messages`:
```sql
-- INSERT: add to FTS
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body_text) VALUES (new.id, new.body_text);
END;

-- UPDATE: reverse the OLD index entry, then insert the NEW one
CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body_text) VALUES('delete', old.id, old.body_text);
    INSERT INTO messages_fts(rowid, body_text) VALUES (new.id, new.body_text);
END;

-- DELETE: reverse the OLD index entry
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body_text) VALUES('delete', old.id, old.body_text);
END;
```

Backfilled rows flow through same triggers (no separate path).

**Repair mechanism** if FTS drifts from base table:
```sql
INSERT INTO messages_fts(messages_fts) VALUES('rebuild');
```

**Index REAL natural-language text ONLY** (same surface as ADR 0016 embeddable set: bodies/captions/poll/contact/location). Non-content rows have `body_text` = NULL → nothing indexed (no FTS hits on synthetic labels like `[sticker 40KB]`).

**Tokenizer = `unicode61 remove_diacritics 2`** (from ADR 0018).

### Ranking (BM25)

**Rank with FTS5's built-in BM25 via `ORDER BY rank` — no enable step, no config.** Verified against our actual dependency: bundled SQLite **3.50.2** (rusqlite `bundled` → libsqlite3-sys 0.35.0, `-DSQLITE_ENABLE_FTS5` set). BM25 has been FTS5's default `rank` since SQLite 3.20.0 (`#define FTS5_DEFAULT_RANK "bm25"`), so it is available unconditionally — there is no separate "enable BM25" step beyond having FTS5 compiled (which ADR 0032 probes for).

Rules for the M1.3 search query:
- Use `... WHERE messages_fts MATCH ?1 ... ORDER BY rank` (ascending). `bm25()` returns a **negated** score (`-1.0 * score` in the source), so **most-relevant = most-negative → ascending `ORDER BY rank` is best-first. Never `ORDER BY rank DESC`.**
- **Column weights (`bm25(messages_fts, w1, ...)`) add no value with the current single-indexed-column schema** (`body_text` only). Defer explicit weights until/unless a later revision indexes multiple columns (e.g. sender name, poll question); only then does weighting body vs. metadata become meaningful.
- For the M2 semantic path (ADR 0007/0008), FTS5 is the candidate-recall stage before the Rust cosine rerank: use a larger, configurable recall `LIMIT` (default ~200), still `ORDER BY rank`, distinct from the final result `LIMIT`.
- **`rank` only exists on `MATCH` queries.** Chat-scoped / time-filtered search combines `MATCH` with base-table predicates; verify the plan with `EXPLAIN QUERY PLAN` at implementation time (a `body_text:term` column filter is a fallback if a JOIN predicate doesn't push down well at scale).

**Open (M1.3):** MATCH-query input sanitization is a separate concern — FTS5 `MATCH` has its own syntax (phrase / `OR` / `NOT` / `*` prefix / column filters), unlike the current `LIKE` path. M1.3 must pick a policy (simplest: quote user input as a phrase, escaping `"`→`""`) to avoid parse errors / unintended query semantics. Not decided here.

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
