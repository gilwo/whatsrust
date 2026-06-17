# 0016. Embeddable text is genuine natural language only

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Not all WhatsApp messages contain natural language worth embedding. Embedding synthetic labels (e.g., `[sticker 40KB]`) or metadata (delivery receipts, reactions) pollutes the vector space and wastes sidecar compute.

Text-only embeddings are simplest for v1. Image/audio content embedding and audio transcription add complexity and dependencies.

## Decision

**Embeddable text = genuine natural language ONLY:**
- Text message body
- Image/video/document captions
- Poll question + options
- Contact name
- Location name

**Everything else → new terminal state `embed_status='skipped'`:**
- Caption-less media (image/video/sticker without text)
- Stickers (even with alt-text)
- Reactions, unreactions, edits, revokes
- Delivery receipts, read receipts
- Poll votes

**Classifier runs at WRITE time** (ingest/backfill knows the message kind) so drain worker (`WHERE embed_status='pending'`) never sees non-content rows.

**Text-only embeddings in v1:**
- NO image content embedding (caption text is embeddable, image bytes are not)
- NO audio transcription (voice note bytes not processed)
- Caption-bearing media: caption text is embeddable, media bytes are not

Anchor: existing `InboundContent::display_text()` (bridge.rs:433) shows the per-kind text shape.

## Consequences

**Positive:**
- Keeps lexical (FTS5) and semantic (vector) search over identical content surface
- Reduces embedding compute load by ~40-60% (stickers, reactions, receipts are common)
- Avoids vector-space pollution from synthetic labels
- Clear semantic: `skipped` means "never embeddable", `failed` means "tried and rejected"

**Negative:**
- Cannot search stickers by alt-text via semantic search (FTS5 also won't index them)
- No image/audio content search in v1 (text captions only)
- Requires per-kind classification logic at write time

**Deferred:**
- Image content embedding (CLIP-style) — requires different model, multimodal sidecar
- Audio transcription (Whisper-style) — requires speech model, transcription pipeline
- Sticker alt-text embedding — debatable value (most stickers lack descriptive alt-text)
