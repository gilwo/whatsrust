# 0002. Adopt wa-rs upstream v0.6.0 directly before building history features

**Status:** Accepted  
**Date:** 2026-06-17  
**Updated:** 2026-06-22 — Added GO/NO-GO gate criteria + minimal spike result  
**Updated:** 2026-06-25 — NO REBASE NEEDED (fork has zero custom commits); adopt upstream v0.6.0 directly. Upstream of record = `oxidezap/whatsapp-rust`.

## Correction (2026-06-25) — "rebase" was a misnomer; this is a dependency bump

Investigation showed the `199-biotechnologies/whatsapp-rust` fork pinned at `9fb13a7` is a **pure ancestor** of upstream v0.6.0 with **0 custom commits** (138 commits behind). It is not a fork carrying local changes — it is upstream *frozen at the v0.2 era*. Therefore **there is nothing to rebase, nothing to clone, no `../whatsapp-rust` sibling, no `rev` to push.**

`oxidezap/whatsapp-rust` and `jlucaso1/whatsapp-rust` are byte-identical (same `main` `302d4787`, same tags). **`oxidezap` is adopted as the upstream of record** (the project's active home). Verified at v0.6.0 (tag `56ed1b09`): all 6 consumed crates exist (whatsapp-rust, wacore, wacore-binary, waproto, whatsapp-rust-tokio-transport, whatsapp-rust-ureq-http-client) and all 7 requested features still exist (tokio-runtime, tokio-transport, ureq-client, moka-cache, simd, signal, tokio-native).

**So Phase 0 collapses from "rebase a fork" to:** edit `Cargo.toml` to point all 6 wa-rs deps at `github.com/oxidezap/whatsapp-rust` tag `v0.6.0` → `cargo update` → fix the ~15-20 mechanical API breakages (Spike Result below) → pass the GO gate. The gate criteria, spike findings, and pivot paths below ALL STILL APPLY unchanged — only the mechanism (dep bump, not rebase) changed.

The original fork-rebase framing is preserved below for history.

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

**Hardening (2026-06-24, v2 review):** GO requires **ACTUAL compile + 89 unit tests + ~15-30min LIVE smoke test** (connect, send, receive, verify group-sender parsing, small history fetch) against a real account (fork decision R1). Rationale: unit tests are pure-logic/no-live-WA by culture; runtime breakage (decryption, media, JID/LID) is exactly what they miss. Folds in M1 (spike must actually compile+test, not static-predict).

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

**Plan (revised 2026-06-25 — dep bump, not rebase):**
1. On a branch, edit `Cargo.toml`: repoint the 6 wa-rs deps `git = "https://github.com/oxidezap/whatsapp-rust", tag = "v0.6.0"` (drop the `199-biotechnologies` rev). `cargo update`.
2. Fix the ~15-20 mechanical breakages (Spike Result below) until `cargo build` + 89 tests pass.
3. Evaluate against G1/G2/G3 + run the live smoke test → GO or pivot.
4. Verify existing features (send, receive, groups, polls) still work (the live smoke test).
5. If GO: commit the dep bump + fixes. Then proceed with on-demand fetch implementation (M1).

(No fork, no `../whatsapp-rust` clone, no `rev` to push — see Correction above.)

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
