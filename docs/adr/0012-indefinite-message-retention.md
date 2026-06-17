# 0012. Indefinite message retention (no time-based deletion)

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Current `prune_old_data()` (called periodically, default 1-hour interval) deletes:
- `inbound_messages` older than 30 days (hardcoded cutoff)
- `outbound_queue` completed/failed jobs older than 7 days

Historical fetch invests effort (network, time, storage) to backfill old messages. Auto-deleting them after 30 days contradicts the feature's purpose (long-term search/archival).

Users expect explicit control over message deletion (privacy, compliance, storage limits) rather than silent auto-pruning.

## Decision

**Remove time-based deletion** of message history. Keep messages indefinitely (until explicit user deletion).

Change `prune_old_data()`:
- **Drop** `DELETE FROM inbound_messages WHERE created_at < cutoff`
- **Keep** `DELETE FROM outbound_queue WHERE status IN ('completed', 'failed') AND updated_at < cutoff` (completed sends don't need retention)

User-driven deletion:
- Existing API: `DELETE /chats/:jid/messages/:msg_id` (single message)
- Future: `DELETE /chats/:jid/messages?before=<ts>` (bulk delete by time range)
- Future: `DELETE /chats/:jid` (delete all messages in chat)

## Consequences

**Positive:**
- Historical backfill not wasted by auto-pruning
- User controls retention (explicit delete vs silent auto-delete)
- Storage growth is predictable (no surprise bulk deletes)

**Negative:**
- Database grows unbounded without user action (mitigated by storage watchdog, see ADR-0013)
- Compliance use cases (e.g., GDPR "right to be forgotten") require explicit delete API calls (acceptable trade-off)

**Future:**
- Configurable retention policy (opt-in time-based pruning via config)
- Per-chat retention settings (e.g., "keep only last 90 days in high-volume groups")
