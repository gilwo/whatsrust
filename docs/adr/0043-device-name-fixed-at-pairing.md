# 0043. Device name is fixed at pairing and stored with the session

**Status:** Accepted
**Date:** 2026-10-13

## Context

The name shown in WhatsApp's "Linked Devices" list was hard-coded to `"Habb"`
(`BridgeConfig.device_name`, passed to wa-rs as `DevicePropsOverride::with_os` on every start).
The user wanted a configurable name defaulting to `"whatsrust"`.

WhatsApp reads the name only while a device is being **paired**. wa-rs
`Device::get_client_payload()` (`wacore/src/store/device.rs`) sends `device_props`, which carries the
name, only in the *registration* payload. That payload is used while the device has no JID. Once
paired, every connection sends the *login* payload, which never includes it (wa-rs test
`login_payload_has_no_device_props`). So changing the setting on a linked session has no visible
effect.

On logout (the device is unlinked from the phone), whatsrust already calls `Store::clear_device()`.
That deletes only the device credentials; messages, embeddings and backfill state are kept, and the
next start pairs again with a new QR code.

## Decision

The name is chosen once, when a device is paired, and stored with the session.

- **Not paired** (the device row has no `pn`, the same check as wa-rs's `Device::is_registered()`):
  use `WHATSRUST_DEVICE_NAME` (trimmed; blank counts as unset), else the default `"whatsrust"`. Save
  it as the `device_name` key in the `metadata` table and pass it to wa-rs for the registration
  payload.
- **Paired:** use the saved name and ignore `WHATSRUST_DEVICE_NAME`, with no warning. A session
  paired before this change has no saved name. Every such pairing used the hard-coded `"Habb"`, so
  `"Habb"` is saved for it.
- **Logout:** `clear_device()` deletes the saved name in the same transaction as the credentials, so
  the next pairing uses whatever `WHATSRUST_DEVICE_NAME` says then.
- `/api/status` reports the effective name as `device_name` (`null` before the first pairing).

The trigger is "not paired", not "empty database". The database keeps message history across a
re-pair, so an empty-database rule would never apply a new name after an unlink.

## Consequences

**Positive:** the configured name and what WhatsApp shows can't silently disagree; renaming is a
documented procedure (unlink, set the variable, restart and pair); re-pairing keeps all local
content; the status endpoint shows what the device is registered as.

**Negative:** changing `WHATSRUST_DEVICE_NAME` on a paired session does nothing until the device is
re-paired, by design and without a warning. Re-pairing creates a new device identity, so contacts'
encryption sessions are set up again and anything still in the outbound queue at logout may fail.

**Library consumers:** the `BridgeConfig.device_name` default changed from `"Habb"` to `"whatsrust"`.
A consumer such as habb that wants to keep its name must set `device_name` explicitly before its
next pairing. Its existing linked devices are unaffected.
