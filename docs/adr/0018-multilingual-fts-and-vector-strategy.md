# 0018. Multilingual FTS and vector strategy

**Status:** Accepted  
**Date:** 2026-06-17

## Context

User's WhatsApp data is mixed-language: Hebrew, Arabic, Thai, English (Israeli +972 numbers). Different scripts have different tokenization needs:
- Space-delimited (Latin, Hebrew, Arabic, Cyrillic, Greek) → word boundary tokenization
- CJK / Thai (no spaces) → require special tokenization (bigrams/trigrams or dictionary-based)

SQLite FTS5 tokenizer options:
- `unicode61`: Unicode-aware word-boundary tokenization, handles diacritics
- `porter`: English stemming (e.g., "running" → "run") — DAMAGES non-English text
- `trigram`: Language-agnostic character n-grams, works for CJK/Thai, larger index
- ICU tokenizer: Locale-aware, dictionary-based — NOT in bundled SQLite (requires loadable extension, breaks single-binary ethos)

Embedding models can be monolingual (English-only) or multilingual (100+ languages including CJK/Hebrew/Arabic/Thai).

## Decision

**FTS5 tokenizer = `unicode61` with `remove_diacritics=2`:**
- Correct for ALL space-delimited scripts (Hebrew, Arabic, Latin, Cyrillic, Greek)
- `remove_diacritics=2` normalizes accents (e.g., "café" matches "cafe")
- **NEVER use `porter` stemmer** (English-only, damages non-English)

**ICU tokenizer rejected:** not in bundled SQLite, requires loadable extension (breaks single-binary), per-platform dependency.

**CJK/Thai (no spaces) degrade to whole-message token** under `unicode61` → lean on vector layer instead.

**Embedder: language handling is sidecar MODEL choice.**

**Default sidecar config should specify a MULTILINGUAL model:**
- Examples: `paraphrase-multilingual-MiniLM-L12-v2`, `multilingual-e5-base`, `bge-m3`
- Covers 100+ languages including CJK, Hebrew, Arabic, Thai
- Enables semantic search for CJK/Thai even when FTS5 can't tokenize
- Free cross-lingual search (query in English, match Hebrew results if semantically similar)

**whatsrust stays language-neutral:**
- Text is opaque bytes
- No language detection, no per-language indexing
- Just stamps `model_id` + `dim` on vectors

**DEFERRED option C:** add a second `trigram` FTS5 index (language-agnostic, works for CJK/Thai) IF "CJK-without-embedder" becomes a stated requirement. Schema is additive (no breaking change). Not in v1.

## Consequences

**Positive:**
- Correct tokenization for user's actual data (Hebrew/Arabic space-delimited)
- No English-stemming damage to non-English text
- Preserves single-binary ethos (no ICU loadable extension)
- Multilingual embedder fixes CJK/Thai FTS5 weakness via semantic search
- Cross-lingual search for free (query in one language, match semantically similar text in another)

**Negative:**
- CJK/Thai lexical search degrades (whole-message token under `unicode61`)
- Requires sidecar with multilingual model for good CJK/Thai coverage
- No fallback for CJK/Thai if embedder is absent (unless option C trigram index added)

**Deferred:**
- Trigram FTS5 index as fallback for CJK-without-embedder (option C) — additive, can be added later if needed
- Language detection (not needed; embedder handles all languages)
