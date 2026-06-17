# 0004. Communities out of scope for v1

**Status:** Accepted  
**Date:** 2026-06-17

## Context

WhatsApp communities are a container for multiple groups with shared membership and announcements. They use a distinct JID format and have additional metadata (community settings, linked groups).

Historical fetch and semantic search for communities would require:
- Community JID normalization and parsing
- Community metadata storage (linked groups, admin structures)
- Filtering/aggregation across linked groups
- Testing without easy access to production communities

## Decision

Communities are **out of scope** for v1 historical fetch and semantic search. Reject community JIDs at the API boundary.

**Targets for v1:** Direct messages (DM) + groups only.

## Consequences

**Positive:**
- Simplifies JID validation (only `@s.whatsapp.net` and `@g.us`)
- Reduces schema complexity (no community linkage table)
- Avoids testing/dev burden without access to real community chats
- Can add community support in v2 once DM + group paths are proven

**Negative:**
- Users in communities cannot backfill community-level announcements or search across linked groups
- May need schema migration later to add community support (but schema is designed to be extensible)

**Future:**
- v2 can add community JID support (`@c.us` or whatever format wa-rs exposes) once base path is stable
