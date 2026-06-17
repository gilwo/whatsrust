# 0014. Single content extraction path for live and backfilled messages

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Message content extraction (parse WhatsApp protobuf → structured `MessageContent` enum) is complex:
- Recursive descent for nested content (quoted messages, forwarded chains)
- Media decryption (thumbnails, full media)
- Special cases (polls, reactions, edits, deletes)

Current implementation: `extract_content_inner()` in `bridge.rs` handles live-received messages (wa-rs event → `Message` proto).

Historical backfill delivers `WebMessageInfo` protos (via `HistorySyncOnDemandResponse`). Question: is `WebMessageInfo` already plaintext, or does it need separate Signal Protocol decryption (like live e2e messages)?

Options:
- **A (adapter):** `WebMessageInfo` is plaintext or pre-decrypted by wa-rs. Write thin adapter to map `WebMessageInfo` → `extract_content_inner` inputs. Single extraction path.
- **B (parallel extractor):** `WebMessageInfo` needs separate decryption + extraction. Duplicate `extract_content_inner` logic for backfill path.

## Decision

**Single extraction path (option A):** adapter maps history-sync `WebMessageInfo` into existing `extract_content_inner` inputs.

**Verify during rebase spike** (ADR-0002): check whether wa-rs v0.6.0's `history_sync.rs` delivers plaintext `WebMessageInfo` or encrypted. If encrypted, determine if wa-rs decrypts transparently or exposes decrypt API.

**Fallback B:** only if spike proves `WebMessageInfo` cannot be mapped to existing inputs (e.g., fundamentally different proto structure, missing fields). Defer parallel extractor until proven necessary.

## Consequences

**Positive:**
- Single code path = easier maintenance (bug fixes apply to live + backfill)
- Reuses battle-tested extraction logic (no duplication)
- Adapter layer is thin (~50-100 LOC vs ~500 LOC for parallel extractor)

**Negative:**
- Adapter may need conditional logic if `WebMessageInfo` has quirks (e.g., missing sender in group history)
- If spike proves incompatible, fallback B adds significant code duplication

**Spike validation:**
- Confirm `WebMessageInfo` fields map to `extract_content_inner` inputs (message proto, sender JID, chat JID, timestamp)
- Test extraction of: text, image, video, audio, document, quoted reply, forwarded message, poll
- Document any `WebMessageInfo`-specific edge cases in adapter
