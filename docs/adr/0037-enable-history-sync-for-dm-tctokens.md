# 0037. Enable history sync by default to obtain the trusted-contact tokens required for 1:1 DMs

**Status:** Accepted
**Date:** 2026-07-01

## Context

During the Phase 0 (wa-rs HEAD adoption) live smoke test, **1:1 direct messages failed to deliver**: the API returned `ok:true` (the stanza was transmitted and the server ACK'd it), but the message never reached the recipient and no delivery receipt came back. Group sends and inbound both worked.

Root cause — proven from the captured wire stanza + wa-rs source + our DB:

- The server rejected the DM with `<ack error="463">` = **`463 MissingTcToken`**. WhatsApp requires a **privacy token** on a 1:1 message: a **tctoken** (trusted-contact token, tier-1) or a **cstoken** (first-contact fallback, tier-2 = `compute_cs_token(nct_salt, recipient_lid)`), else the message is nacked and silently dropped.
- Our device had **neither**: no stored tctoken for the recipient, and `device.nct_salt` was **empty** (so the tier-2 cstoken could not be computed).
- Both the tctokens and the `nct_salt` (and the account pushname) are delivered by the **history-sync bootstrap** that the server pushes right after device pairing. whatsrust set **`skip_history_sync = true`**, which discarded that bootstrap — so those tokens never arrived.

The `skip_history_sync = true` default dated to the v0.2-era wa-rs, added to prevent a **"deaf client" bug (Issue #125)**: that version processed the history blob *inline*, which could block the live message loop under large offline queues and leave the client unresponsive ("deaf") to live messages.

Investigation findings that drive this decision:

1. **The "deaf client" concern is obsolete on the current wa-rs.** History sync is now processed on a **dedicated sync worker**, with large blobs decoded via `spawn_blocking` — fully **decoupled from the live read loop** (`history_sync.rs`). Empirically, a full bootstrap (InitialBootstrap + Recent + Full + external blob) processed while the daemon stayed connected and responsive (it sent a DM and received the delivery receipt during/after the sync).
2. **Enabling history sync does not bloat storage or the live path.** History arrives as `Event::HistorySync` (handled as a no-op here), *not* as live `Event::Message` — our `inbound_messages` table was untouched by the sync.
3. **The bootstrap only fires at pairing.** Flipping the flag on an already-paired session does nothing; the fix requires the flag to be off *when the device links*.
4. **`nct_salt` (the cstoken/tier-2 path) is WhatsApp-account-gated.** Even a full bootstrap delivered **no** `nct_salt` for the test account (the `463` nack itself warns "may indicate a reachout timelock on the account"). This is not code-fixable — it lifts with account trust/age. But it is not needed for the common case: the bootstrap delivers **contacts' tctokens** (tier-1), which authorize DMs to them.
5. History sync is required by the F1 feature (historical fetch) regardless — so enabling it **advances** the roadmap rather than fighting it.

## Decision

**Enable history sync by default.** Specifically:

- `BridgeConfig::skip_history_sync` default flips from `true` to **`false`** (history sync ON).
- The daemon exposes an opt-out knob: **`WHATSRUST_SKIP_HISTORY_SYNC=1`** (or `true`) re-enables skipping (leaner initial pairing, but 1:1 DMs will fail with `463`).
- The `Event::HistorySync` handler remains a **no-op for now** — wa-rs persists the tctokens / pushname / nct_salt internally, so no handler work is needed to fix DMs. F1 (M1) will later make this handler extract and store historical messages.

## Consequences

**Positive:**
- 1:1 DMs to established contacts work (their tctokens arrive via the bootstrap and persist in the `tc_tokens` SQLite table across restarts — no per-message or per-restart wait; the only cost is the one-time bootstrap at pairing).
- pushname now populates (previously empty), fixing "cannot send presence: push_name is empty".
- Aligns with F1, which needs history sync anyway.
- Removes an obsolete, actively-harmful default.

**Negative / limitations (honest):**
- **Cold-outreach to non-contacts may still `463`** — no tctoken exists and `nct_salt` is account-gated. This is WhatsApp's anti-spam behavior, not a whatsrust bug; documented as a known limitation. It may resolve with account trust/age.
- **tctoken breadth is not exhaustively verified** — confirmed for one contact (surfaced from a pre-existing trust relationship). Coverage of *all* contacts via the bootstrap is presumed but unproven.
- **One-time bootstrap cost at pairing** scales with account history size (CPU/memory during the off-live-path decode; mitigated by wa-rs's streaming/compressed `LazyHistorySync`). A power user on a huge account can opt out via the env var.

**Related / carried forward:**
- `MsgSecretStore` is currently stubbed as no-ops (can't decrypt history-delivered edits/reactions/poll-votes) — an F1-era TODO, unrelated to the DM fix.
- This decision is part of the Phase 0 wa-rs HEAD adoption (ADR 0002); the DM-send bug was surfaced by that gate's live smoke test.
