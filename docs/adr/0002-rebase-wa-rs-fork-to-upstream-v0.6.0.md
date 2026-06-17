# 0002. Rebase wa-rs fork onto upstream v0.6.0 before building history features

**Status:** Accepted  
**Date:** 2026-06-17

## Context

Historical message fetch depends on `HistorySyncOnDemandRequest` machinery in wa-rs. Our fork is pinned to `9fb13a7` (≈ upstream v0.2 era). Upstream `jlucaso1/whatsapp-rust` is at v0.6.0 (`d441e5f`) with heavily reworked history subsystem:

- `pdo.rs`: 501 → 870 lines
- `history_sync.rs`: 281 → 1066 lines

Building on-demand fetch atop the old fork risks duplicating work already done upstream and diverging further. Rebase merges upstream improvements and exposes breaking changes early.

## Decision

Rebase the `199-biotechnologies/whatsapp-rust` fork onto upstream v0.6.0 as **step 0** before adding on-demand wiring. Conduct a rebase spike first to size whatsrust breakage (changed APIs, removed methods, new required fields).

## Consequences

**Positive:**
- Gain upstream's history_sync refactoring (basis for on-demand fetch)
- Avoid duplicating work or maintaining parallel implementations
- Reduce long-term merge debt

**Negative:**
- Rebase breakage in whatsrust bridge code (expected: JID normalization, message builder changes, event type shifts)
- Adds rebase-spike step before feature work begins
- Risk of regressions if upstream introduced breaking assumptions

**Plan:**
1. Rebase spike in `../whatsapp-rust` (branch `rebase-v0.6.0`)
2. Update whatsrust to fix breakage
3. Verify existing features (send, receive, groups, polls) still work
4. Then proceed with on-demand fetch implementation
