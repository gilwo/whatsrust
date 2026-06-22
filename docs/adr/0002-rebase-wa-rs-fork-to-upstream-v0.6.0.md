# 0002. Rebase wa-rs fork onto upstream v0.6.0 before building history features

**Status:** Accepted  
**Date:** 2026-06-17  
**Updated:** 2026-06-22 — Added GO/NO-GO gate criteria + minimal spike result

## Context

Historical message fetch depends on `HistorySyncOnDemandRequest` machinery in wa-rs. Our fork is pinned to `9fb13a7` (≈ upstream v0.2 era). Upstream `jlucaso1/whatsapp-rust` is at v0.6.0 (`d441e5f`) with heavily reworked history subsystem:

- `pdo.rs`: 501 → 870 lines
- `history_sync.rs`: 281 → 1066 lines

Building on-demand fetch atop the old fork risks duplicating work already done upstream and diverging further. Rebase merges upstream improvements and exposes breaking changes early.

## Decision

Rebase the `199-biotechnologies/whatsapp-rust` fork onto upstream v0.6.0 as **step 0** before adding on-demand wiring. **Rebase spike is a HARD GO/NO-GO GATE** — must produce written verdict against criteria below + pre-attached pivot paths BEFORE any F1 implementation. Spike is throwaway/exploratory (answer the gates, not ship code); real productionized rebase only on GO.

**Gate criteria:**
- **G1:** whatsrust compiles + 89 tests pass vs v0.6 (API-breakage blast radius must be tractable).
- **G2:** History `WebMessageInfo` already plaintext (feeds ADR 0014 single-extraction adapter).
- **G3:** ON_DEMAND response correlatable to its request (drives the paginated loop). ADR 0026 single-worker LOWERS bar: one PDO outstanding at a time → "match the only in-flight" suffices even without explicit id.

**Pivot paths (pre-attached):**
- **G1 deep-breakage:** Time-box rebase ≤N days, else cherry-pick only history-sync/PDO commits onto current pin OR defer F1 + ship audit quick-wins first.
- **G2 encrypted:** ADR 0014 fallback B (parallel extractor).
- **G3 no-correlation:** Single-flight matching (ADR 0026 already serial); if even that impossible → F1 not viable on this protocol, STOP & reassess.

## Consequences

**Positive:**
- Gain upstream's history_sync refactoring (basis for on-demand fetch)
- Avoid duplicating work or maintaining parallel implementations
- Reduce long-term merge debt
- GO/NO-GO gate makes failure CHEAP & DECIDABLE vs sunk-cost spiral

**Negative:**
- Rebase breakage in whatsrust bridge code (expected: JID normalization, message builder changes, event type shifts)
- Adds rebase-spike step before feature work begins
- Risk of regressions if upstream introduced breaking assumptions

**Plan:**
1. Rebase spike in `../whatsapp-rust` (branch `rebase-v0.6.0`)
2. Evaluate against G1/G2/G3 → GO or pivot
3. If GO: update whatsrust to fix breakage
4. Verify existing features (send, receive, groups, polls) still work
5. Then proceed with on-demand fetch implementation

---

## Spike Result (2026-06-22, static inspection of upstream v0.6.0 tag 56ed1b09)

**Overall lean: GO, MEDIUM effort (~1-2 days mechanical).** No architectural landmines requiring rearchitecture.

**G1 = LIKELY-FAIL-but-mechanical** (~15-20 call sites):
1. `Event::Message(Box<wa::Message>, MessageInfo)` → `(Arc<wa::Message>, Arc<MessageInfo>)` (v0.6 events.rs:412; ~5-8 sites incl bridge.rs:2408).
2. `.on_event` closure now `Fn(Arc<Event>, Arc<Client>)` not owned Event (bridge.rs:2168-2191).
3. Exhaustive Event match breaks — v0.6 ADDS IncomingCall/IdentityChange/RawNode/MexNotification, REMOVES JoinedGroup (bridge.rs:2332+).
4. Event::HistorySync → Box<LazyHistorySync> w/ .get()/.raw_bytes() (currently skip_history_sync so no immediate break).
5. LANDMINE VERIFIED (defused): whatsrust DOES use Event::JoinedGroup (bridge.rs:2815) which v0.6 REMOVED — BUT handler is low-stakes (logs + group_cache.invalidate only). Loss = stale group-cache entry until next refresh, NOT a correctness break. v0.6: find where group-join now surfaces (likely LazyHistorySync path) + move the invalidate, or accept minor staleness. **Not a blocker.**

Unchanged: Bot::builder + .with_* chain, Connected/LoggedOut/Receipt/DeviceListUpdate, core proto types (MessageKey/Message/WebMessageInfo).

**G2 = LIKELY-PASS.** History WebMessageInfo.message is populated plaintext wa::Message (v0.6 waproto:16106; HistorySyncMsg waproto:6013; Conversation.messages waproto:4794). Phone decrypts before packing into the (separately-encrypted, wa-rs-decrypted) blob. NO separate Signal decryption. → ADR 0014 single-extraction-path holds.

**G3 = LIKELY-PASS.** `HistorySyncNotification.peer_data_request_session_id` (field 12, waproto:8185) exposed via LazyHistorySync accessor — explicit correlation. Plus single-flight fallback. `fetch_message_history` exists (v0.6 pdo.rs:173-226).

**Other flags:** LazyHistorySync .get() memory (don't decode unused chunks); v0.6 has extensive LID/PN resolution churn — watch for edge cases vs whatsrust's current LID work.
