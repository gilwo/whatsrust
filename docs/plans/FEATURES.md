# whatsrust Features

**Type:** Live document. It describes each feature: what it does, how it was broken down, the
issues met on the way, and the ADRs behind it. Update a feature's entry when its work lands or
when an issue is found.
**Last updated:** 2026-10-23

Where else to look:
- **Status, order, the feature map and the completed list:** `docs/plans/IMPLEMENTATION-ROADMAP.md`.
- **What changed, and when:** `CHANGELOG.md`.
- **Why (decisions):** the ADRs in `docs/adr/` (start at `0000-index.md`).
- **What/how blueprints:** `docs/plans/*-design.md` and the milestone plans.

**Status legend:** 🟢 Done · 🟡 In progress · 🔵 Designed (not started) · ⚪ Planned (not designed) · ⏸️ Deferred

**Entry layout:** summary · design and ADRs · breakdown (where relevant) · issues encountered
(date, issue, resolution, reference) · open items (each one is ranked in the roadmap's priority
backlog).

## Contents

- [F1 — Historical message fetch + lexical/semantic search](#f1)
- [F2 — Embedder sidecar](#f2)
- [F3 — MCP Streamable HTTP transport](#f3)
- [F4 — Multi-account support](#f4)
- [F5 — Access profiles](#f5)
- [F6 — Database encryption at rest](#f6)
- [F7 — Richer read tools](#f7)
- [F8 — Documentation overhaul](#f8)
- [F9 — Dev run profiles](#f9)

---

<a id="f1"></a>
## F1 — Historical message fetch + lexical/semantic search 🟢 Done (M1 2026-07-20; M2 live-validated 2026-10-05; open items below)

Per-chat historical backfill (`all` / `since:T` / `count:N`) + local FTS5 (lexical) + optional
vector (semantic) search.

- **Design:** `docs/plans/2026-06-17-historical-fetch-semantic-search-design.md`. Milestone plans:
  `docs/plans/2026-07-02-M1-detailed-plan.md`, `docs/plans/2026-07-23-M2-detailed-plan.md`.
- **Reviews:** 3 cold passes (`_reviewer/design/`), 2026-06-18 / -23 / -24 → final verdict
  **implementation-ready, no blockers**. The implementation-time concerns (I1–I7) and residual risks
  (R1–R6) in `_reviewer/design/2026-06-24-...-v3.md` were folded into each phase plan.
- **ADRs:** 0001–0036 (M1 and the shared base), 0037 (history sync), 0038–0042 (M2), 0048
  (pairing history); dated amendments to 0003 and 0005 (2026-10-20 → 22).

### Breakdown

**Shape (plan of 2026-06-25):** Phase 0 (a hard GO/NO-GO gate: the wa-rs adoption) → M1 (fetch +
lexical search; ships alone, no sidecar) → M2 (semantic search; layers on M1). Testing (fake seams,
ADR 0025) and safety/config (fail-closed config, pacer, daemon-side guards) were built into each
phase, not added after. Phase-level granularity: the detailed plan of each milestone was written at
its start, and nothing past Phase 0 was detailed before the gate passed.

**Phase 0 — adopt wa-rs upstream (HARD GO/NO-GO GATE, ADR 0002) — ✅ GO 2026-07-01 (3df4ce9)**
- **Plan:** a dependency bump, not a rebase (the old `199-biotechnologies` pin had zero custom
  commits). Repoint the 6 wa-rs crates to `oxidezap/whatsapp-rust` (upstream of record, byte-identical
  to `jlucaso1`) and fix the predicted breakage: `Event::Message(Box,_)` → `(Arc,Arc)`, the
  `.on_event` closure, the exhaustive match (+4 variants, −JoinedGroup), `HistorySync` →
  `Box<LazyHistorySync>`, and the JoinedGroup handler, verifying that the group cache still fills on
  demand (review I1).
- **GO criteria:** G1 compile + the existing tests pass; G2 history `WebMessageInfo.message` is
  usable plaintext through `extract_content_inner` (ADR 0014); G3 an ON_DEMAND response is
  correlatable to its request (or single-flight matching suffices, ADR 0026); a live smoke test
  (connect → send → receive → group-sender parsing → one small history fetch).
- **Pivot plan (not needed):** G1 deep breakage → cherry-pick the history-sync commits or defer F1;
  G2 encrypted → ADR 0014 fallback B; G3 no correlation → F1 not viable, stop and reassess.
- **Verdict:** pinned to `oxidezap` main HEAD `9e8c70e2`, not the v0.6.0 tag. The tag compiled and
  passed 150 tests, but 1:1 DM sends failed live (see the issues table); the upstream fix
  (#730/#731, `09a3b0c1`) landed after the tag. HEAD (+315 commits) was scope-analyzed: #852 bot
  API, #866 Message boxing and #893 typed errors handled; #911 bincode→prost no effect; #853 history
  sync verified F1-safe. G1 ✅ 150 tests; G2 ✅ bootstrap decoded; G3 ✅
  `peer_data_request_session_id` + `fetch_message_history` intact. Live smoke test ✅ connect /
  receive / group send (delivered+read) / 1:1 send (delivered) / history bootstrap processed.
  (ADR 0002/0037)

**M1 — fetch + lexical search — ✅ COMPLETE 2026-07-20** (all exit criteria met; live E2E passed on
a real account)
- [x] **M1.1 storage + migration** — done 2026-07-03. v8 unified `messages` table via
  **copy-then-drop** (not rename: schema-first ordering; ADR 0009 correction) + FTS5
  (external-content, BM25) + `'delete'`-command sync triggers (ADR 0019 correction); sibling tables
  (`media_refs`, `embeddings`, `backfill_cursor`, `backfill_jobs`, `metadata`); staged ceremony
  (fail-closed backup → FTS5 probe → migrate inside one transaction → post-commit validation →
  circuit-breaker pin + `--rollback`/`--migrate` → watchdog baseline seed); single-connection model;
  **prune age-DELETE removed (review I2)**. Verified: 168 tests + a real-data v7→v8 dry-run
  (Hebrew/Arabic FTS, pristine v7 backup). (ADR 0009/0019/0027/0028-0032/0036)
- [x] **M1.2 fetch worker + safety/config** — done 2026-07-03, live-validated. `HistorySource`
  trait (test seam), durable backfill-job queue, single-worker FIFO pagination (two-phase async
  on-demand fetch via `HistoryCorrelator`), cursor, contained-C target model (`since`/`all`/`count`),
  enqueue-time atomic validation, stuck-anchor guard, 3-level abort, dedicated backfill pacer,
  daemon-side guards (queue depth + `max_messages` clamp + cooldown), fail-closed safety config
  (`WHATSRUST_DANGEROUSLY_ALLOW_*`), dotenvy `.env`. Safety/config was built in here, not deferred.
  End-to-end verified: a count fetch auto-paginated to exhaustion, 14 messages stored under the
  phone JID. (ADR 0003/0010/0020/0021/0022/0023/0026/0033/0035)
- [x] **M1.3 FTS5 lexical search** — done 2026-07-13. `search_inbound` rewritten: `query=Some` →
  `messages_fts MATCH` + BM25 `ORDER BY rank` (ascending); `query=None` history browse unchanged.
  Quote-as-phrase MATCH sanitization (`"`→`""`; operators neutralized). One FTS path
  (EXPLAIN-verified: the FTS index drives, PK rowid lookup), no LIKE fallback. Multilingual
  (Hebrew/Arabic) and injection-safe. 9 tests; suite 188 lib / 203 bin. API/MCP JSON shape unchanged
  (now relevance-ranked). (ADR 0019)
- [x] **M1.4 API/MCP trigger + watchdog** — done 2026-07-16. `/api/history-fetch`
  (trigger/status/cancel) + MCP tools `whatsrust_fetch_history`/`_status`/`_cancel` (33 tools) + SSE
  `BackfillProgress` events (fuzzy/precise per target kind); storage-growth watchdog in the periodic
  prune tick (`wal_checkpoint(PASSIVE)` + `db+wal+shm` stat, ≥50% → WARN +
  `BridgeEvent::StorageAlert` + baseline reset); the temporary `WHATSRUST_BACKFILL_TEST` hook
  retired. **Live E2E passed 2026-07-20** (real account: API/CLI/MCP trigger, SSE `backfill` +
  `storage_alert` events, FTS search, cancel→404). (ADR 0011/0013/0034/0036)
- **M1 exit criteria (all met):** a user can trigger `all` / `since:T` / `count:N` for a chat or
  group via API/MCP/CLI and watch SSE progress; backfilled and live messages are unified, searchable
  by FTS5 (incl. Hebrew/Arabic) and kept indefinitely (no age pruning); the v7→v8 migration is
  staged, backed up, validated and rollback-able, with the circuit-breaker proven; cancel and
  graceful shutdown follow the 3-level abort model (resume on restart for shutdown, terminal for
  cancel); tests per ADR 0025 green and the manual E2E checklist passes; the semantic layer is
  absent by design (`embed_status` accrues `pending`, nothing consumes it yet).

**M2 — semantic search — ✅ code-complete 2026-09-30, live-validated 2026-10-05**
- [x] **M2.1 embedder sidecar contract & transport.** `Embedder` trait
  (`model_info`/`embed`/`health`), `StdioEmbedder` (JSON-RPC stdio client), `FakeEmbedder` test seam,
  trust-but-verify validation (dimension, model_id, timeout). (ADR 0006/0024/0025)
- [x] **M2.2 embeddable-text classification.** `InboundContent::embeddable_text()` (natural
  language only, distinct from `display_text()`), write-time classification (`pending`/`skipped`),
  set-difference drain query. Pre-M2 rows stay `pending` with `display_text()` stored; tolerated
  (small single-user backlog, raw captions unrecoverable). (ADR 0016/0017/0038)
- [x] **M2.3 embedding-drain worker.** Independent task (spawned beside backfill/prune, before the
  reconnect loop), periodic wake (default 60 s), batch sidecar calls (default 64), transport-failure
  retry (rows stay `pending`). The in-memory 3-attempt cap → terminal `failed` path is scaffolded
  but **dormant**: the batch protocol cannot tell a content rejection from a transport failure
  (deferred to M2.6.7). A pathological-pending circuit-breaker back-pressures **new backfill
  enqueues** (`EmbedderBacklogFull`) at a ceiling (default 100k); the drain keeps running.
  (ADR 0015/0017/0038)
- [x] **M2.4 semantic search path.** Embed the query → FTS recall (width max(200, limit)) → fetch
  vectors by `(message_id, active_model_id)` → Rust-side cosine rerank → additive lexical fallback
  (byte-identical to M1 when the sidecar is absent or down). Superseded by hybrid recall below as
  the active path; kept as the scan-cap fallback. (ADR 0007/0008/0017/0018)
- [x] **M2.5 multi-model retention & explicit purge.** `embeddings` PK `(message_id, model_id)`
  allows a model switch without a purge; `Store::purge_embeddings(model_id)` + `incremental_vacuum`;
  surfaces: API `POST /api/embeddings/purge`, MCP `whatsrust_purge_embeddings` (34 tools), CLI
  `purge-embeddings <model_id>`, returning `{model_id, rows_deleted, bytes_reclaimed}`. (ADR 0017)
- [x] **M2.6 config, integration test, wiring** — done 2026-09-02. Embedder knobs through
  `.env`/`BridgeConfig` with config tests; `.env.example` completed; fake-embedder misbehave mode +
  real-subprocess trust-but-verify integration tests (CI-safe); additive `/api/status` embed-status
  counts; the dormant per-row-rejection semantics reconciled in ADR 0015/0038. (ADR 0023/0024/0025)
- [x] **Sidecars:** the Python MVP (2026-08-31) and then the native Rust sidecar with bge-m3 as the
  default (2026-09-30) — see [F2](#f2). (ADR 0039/0040)
- [x] **Hybrid recall** — done 2026-09-30. `WhatsAppBridge::search` unions FTS5 lexical candidates
  with a bounded brute-force vector scan of the search scope (skipped above
  `WHATSRUST_SEMANTIC_SCAN_CAP`, default 50000); lexical matches are always kept; vector-only
  matches need `cosine ≥ WHATSRUST_SEMANTIC_MIN_SCORE` (default **0.55**, calibrated for bge-m3
  against the real read-only `whatsapp.db` with `make calibrate`; conservative for MiniLM).
  `/api/status` → `embeddings` reports per-active-model `{model_id, embedded, pending, failed, skipped}`.
  (ADR 0041)
- **Live validation (2026-10-05, real account, bge-m3):** English queries found Hebrew messages with
  no shared words; the first search after idle took 2–6 s while weights paged back in. Semantic
  search is **opt-in** via `WHATSRUST_EMBEDDER_CMD` and **dormant** (falls back to FTS5) when the
  sidecar is absent or down.
- **M2 exit criteria:** ✅ with a sidecar, semantic search returns relevant results; without one,
  search falls back to M1 lexical (validated live 2026-10-05). ✅ The drain keeps up under normal
  backfill; model switch validated live (MiniLM → bge-m3 re-embedded all rows, old vectors kept);
  per-model purge covered by tests, not exercised live. ⏸️ Semantic CJK handling: deferred to live
  testing.

**Follow-on: making fetch-history work on chats with no stored message (2026-10-20 → 22)**
- [x] **A fetch needs a usable anchor** — 2026-10-20. The anchor is the cursor's, else the oldest
  stored message; none → REST 409 `no_anchor` (also an MCP tool error) and the worker fails fast
  without calling the phone; empty cursors from older versions are re-seeded. (e140361; ADR 0003
  amendment)
- [x] **Anchors from archive/read-state syncs, 180 s wait** — 2026-10-22. An archive, unarchive or
  mark-read sync from the phone carries the chat's last message key; for a chat with no anchor (no
  usable cursor, no stored message, not exhausted) that key becomes the cursor's anchor, so such a
  chat can be fetched without sending anything (archive and unarchive it on the phone). The fetch
  waits 180 s for the phone (was 60 s); the timeout error says to open WhatsApp on the phone.
  `no_anchor` guidance updated. (ec3691f; ADR 0003 second amendment)
- [x] **Store the pairing history and late on-demand answers** — 2026-10-22. Every unmatched
  history-sync chunk with conversations is stored (`source=bootstrap`, keyed like live messages,
  duplicates skipped, embedding queued), so those chats have an anchor from the start; a late answer
  to an on-demand fetch is stored as `source=backfill` (the job stays failed; the next fetch
  continues from the stored messages). A `@lid` DM that carries `pnJid` is stored under the phone
  JID. Stored history downloads no media: a row keeps its label (`[image 12.1KB] caption`) and a
  `media_refs` row (ADR 0005 amendment). Storing runs in a spawned, best-effort task;
  `WHATSRUST_STORE_PAIRING_HISTORY=false` turns the pairing part off. Only a fresh pairing delivers
  the package. **Live-verified 2026-10-22** (dev re-pair): 5 + 3 conversations with only each chat's
  last message (5 chats stored), enough to fetch two group chats, including one stuck before.
  (eed8e19; ADR 0048)

### Issues encountered

| Date | Issue | Resolution | Reference |
|---|---|---|---|
| 2026-07-01 | 1:1 DM sends failed with the server nack `463 MissingTcToken` on the v0.6.0 tag | Pinned upstream HEAD (with the fix) and enabled history sync, which delivers the trusted-contact tokens; `skip_history_sync` default flipped to `false`, opt-out `WHATSRUST_SKIP_HISTORY_SYNC=1` | 3df4ce9; ADR 0037 |
| 2026-07-01 | Cold outreach to non-contacts can still fail with `463` | Known limitation: `nct_salt` (the cstoken path) is gated by the WhatsApp account, not fixable in code | ADR 0037 |
| 2026-07-01 | `MsgSecretStore` is a no-op stub, so edits, reactions and poll votes delivered through history cannot be decrypted | **Open.** Deferred at Phase 0; still a stub (storage.rs:3551, checked 2026-10-23) | ADR 0002 |
| 2026-07-03 | A rename-in-place migration conflicts with schema-first ordering | Copy-then-drop migration | ADR 0009 correction |
| 2026-07-03 | FTS5 external-content sync needs the `'delete'` command | `'delete'`-command AFTER triggers | ADR 0019 correction |
| 2026-07-03 | The age-based prune would delete backfilled history (review I2) | Prune age-DELETE removed; retention is indefinite | ADR 0012 |
| 2026-07 (M1.2 live) | Fetch while disconnected; responses keyed by the recipient's LID | Connection-gating (defer, not fail); rows stored under the phone JID | M1.2 |
| 2026-09-02 | M2 code review of the M2 diff (from 69f386a): defense-in-depth findings against a trusted sidecar or rare inputs | Real items are open and ranked in the roadmap backlog ("M2 review" rows). Checked and **dismissed** (do not re-investigate): H1 — `test_search_partial_coverage_keeps_unvectored` exists and covers the additive case; H3 — `test_search_recall_width_at_small_limit` is a real flip-check; a stale "305 lib / 318 bin" count (actual 312 lib / 327 bin, fake-embedder suite 333); M5 — `Instant` is monotonic and unaffected by NTP | M2 review |
| 2026-09-30 | Rerank-only search (M2.4) returned nothing for a word-free query (an English query matching a Hebrew-only message) | Hybrid recall | ADR 0041 |
| 2026-10-05 | A `+` in a search query was decoded wrongly (live run) | `decode_query_component` | ADR 0042 era |
| 2026-10-20 | A fetch of a chat with no stored message hung 60 s and left an empty cursor that blocked the chat for good (the phone ignores an empty anchor) | Anchor resolution + `no_anchor` fail-fast + re-seeding | e140361; ADR 0003 amendment |
| 2026-10-21 | The phone rejects every anchor-free request (empty, or timestamp-only with an empty or made-up id); a sleeping phone answered after the 60 s wait | Anchors from archive/read-state syncs; 180 s wait | ec3691f; ADR 0003 second amendment |
| 2026-10-21 | Fresh profiles had no anchors; late on-demand answers were dropped | Store the pairing package and late answers | eed8e19; ADR 0048 |
| 2026-10-22 | After logout + re-pair, the device gets no push name and no archive/read-state events (seen as an empty history after an archive/unarchive test) | **Open.** Suspected cause: `Store::clear_device` deletes only the device row and name, so the old device's app-state and Signal data stay. Confirm with a pairing from an empty DB | roadmap P1; HOWTO §3a |
| 2026-10-22 | A pairing-package DM without `pnJid` stays under its `@lid` JID, while live messages from that contact go under the phone JID | **Open** (the `@lid` split below) | roadmap P2 |

### Open items

- [ ] **On-demand media download** for stored history: REST/CLI/MCP download from `media_refs`
  (roadmap P1, next). **Findings (2026-10-23, wa-rs pin `9e8c70e`):** `Client::download_from_params`
  needs the media type, which `media_refs` does not store, but `messages.content_kind`
  (image/audio/video/document/sticker) maps 1:1 to it, so no schema change is needed; the plaintext
  hash is checked only for unencrypted (newsletter) media. An expired CDN link can be recovered:
  `client.media_reupload().request(...)` asks the phone to re-upload and returns a new `direct_path`
  (the phone must be online). `media_refs.hydrated_path` exists and nothing uses it yet. Open design
  questions: where the bytes go (a file, the response, or both), whether to re-upload automatically,
  a size limit, and what MCP returns.
- [ ] **Live check of archive-key seeding** (roadmap P1, owner): archive and unarchive a chat with no
  stored message, then fetch it. The code is done (ec3691f). The 2026-10-22 runs never received an
  archive event because the device had been re-paired (issue above). Retest on a device paired from
  an empty DB.
- [ ] **`@lid` DM split** (roadmap P2): re-key the rows once the LID mapping is known; needs a
  design. **Lead (2026-10-23, not verified):** the history package has a phone↔LID mapping list
  (`HistorySync.phoneNumberToLidMappings`, field 15) that neither wa-rs nor whatsrust reads; storing
  it in `lid_pn_mapping` before the chats may prevent the split at the source. Whether the owner's
  package carries the list is not checked. The same LID→phone resolution serves sender names (F7
  item 1).
- [ ] **`MsgSecretStore`** for history-delivered edits, reactions and poll votes (roadmap P3, not
  ranked by the owner).
- [ ] **M2 review follow-ups** (roadmap "M2 review" rows; sidecar items under [F2](#f2)). None is a
  correctness or security defect in normal operation; all are defense in depth against a trusted
  sidecar or rare inputs.
  - **(Med) Timeout on `fetch_embeddings_for_candidates`** in `WhatsAppBridge::search`
    (`src/bridge.rs:1487` at review time): the query embed has a bounded wait (ADR 0042), the
    candidate vector fetch does not. This is a property of the whole single-`Mutex<Connection>`
    storage layer (the M1 lexical `search_inbound` has no timeout either), not an M2 regression; if
    fixed, apply it everywhere. Reviewer H2.
  - **(Low) Cosine epsilon:** `cosine_similarity` (`src/embedder.rs`) guards zero magnitude with an
    exact `== 0.0`; use `< 1e-9` to avoid `Inf` or huge values from near-zero vectors. Reviewer M4.
  - **(Low) `max_input_tokens` floor:** a sidecar advertising `0` would truncate all text to empty
    (`src/embed_drain.rs`, `prepare_text_for_embedding`); treat an unreasonably small value as absent
    and WARN. Reviewer M2.
  - **(Low) Purge non-INCREMENTAL warning:** the WARN is log-only; the API/CLI/MCP response shows
    `bytes_reclaimed: 0` with no reason. Consider a `warning` field or a CLI note. Reviewer M1.
  - **(Low) Poison-pill-at-tail visibility:** an unembeddable row at the tail keeps the drain waking
    (bounded by backoff and notify-only quiescence); log loudly after N solo failures. Do NOT
    auto-`failed` (that is the deferred M2.6.7 cap-3 path). Reviewer M3.
  - Dismissed findings: see the issues table (H1 is `src/bridge.rs:5580`, H3 is `src/bridge.rs:5634`
    at review time; H1 covers the M2.4.3 (b) additive-survival case).
- ⏸️ Semantic CJK handling (M2 exit criterion): deferred to live testing.

---

<a id="f2"></a>
## F2 — Embedder sidecar (implementation) 🟢 Done (M2.1–M2.6 + ADR 0040/0041/0042 complete; live-validated 2026-10-05)

Stateless separate binary, pure vectorizer; stdio JSON-RPC v1; transport-neutral `Embedder` trait
(HTTP/localhost as a future sibling); multilingual model by default.

- **Design:** ADRs 0006, 0015, 0024 (+ 0007/0008/0016/0017/0018/0038/0039/0040/0041/0042). Protocol
  and validation fully specified.

### Breakdown

- [x] `Embedder` trait + stdio JSON-RPC transport (model_info / embed / health, trust-but-verify
  validation) — done in M2.1
- [x] Sidecar binary: Python MVP + sentence-transformers (`paraphrase-multilingual-MiniLM-L12-v2`,
  384-dim, prefix-free; `scripts/embedder-sidecar.py`) + `scripts/run-embedder.sh` — done 2026-08-31; superseded as the deployment
  default by the native Rust sidecar (`embedder-rs/`, candle), serving MiniLM or **bge-m3** (default
  for `make run-semantic`, `EMBED_MODEL ?= bge-m3`; `EMBED_MODEL=minilm` selects MiniLM) — done
  2026-09-30. The Python sidecar stays as the legacy parity-check reference. (ADR 0039/0040)
- [x] Drain worker integration (ADR 0015), multi-model store + purge (ADR 0017) — done in M2.3/M2.5
- [x] Cancellation-safe sidecar client; bounded query-embed wait with a shared cool-down (ADR 0042)

### Issues encountered

| Date | Issue | Resolution | Reference |
|---|---|---|---|
| 2026-09-30 | Cold-cache model download failed (hf-hub does not follow the redirect) | Fixed in `embedder-rs` for both models | ADR 0040 |
| 2026-10-05 | Sidecar-client desync found in live runs | Cancellation-safe client; one bounded wait for the same in-flight embed; cool-down (30 s doubling to 10 min) | ADR 0042 |
| 2026-09-02 | M2 review: `StdioEmbedder` checks the 16 MiB line cap after the allocation, so a runaway sidecar could exhaust memory first (reviewer M6) | **Open** | roadmap "M2 review" row |
| 2026-09-02 | A failed cold-cache download leaves a `<filename>.tmp-<pid>` file behind (no cleanup on error; reviewer L4) | **Open** (harmless stray file) | ADR 0040; roadmap "M2 review" row |
| – | The sidecar is not restarted if it dies; semantic search stays off until a daemon restart | **Open** | roadmap P2 |
| – | `embed_drain::tests::test_drain_worker_max_batch_clamp` failed once under heavy memory pressure (it polls against a wall-clock deadline) | **Open:** move it to tokio's paused clock | roadmap P2 |

### Open items

- [ ] Sidecar auto-respawn (roadmap P2)
- [ ] Bounded sidecar line read (roadmap P2, M2 review M6): `StdioEmbedder` reads a line with
  `read_until` into a `Vec` and checks the 16 MiB cap after the allocation; read in chunks and check
  the accumulated size before each chunk (a true hard bound).
- [ ] Flaky drain test → paused clock, as the ADR 0042 tests do (roadmap P2)
- [ ] Nits (roadmap P3, M2 review): fake-embedder env-configurable `max_batch`/`max_input_tokens`,
  including omitting them, to exercise the no-advertised-limit path (L1); cfg-gate the dormant
  `AttemptTracker` (L2); document the 100-iteration `incremental_vacuum` cap in `purge_embeddings`
  (L3); in `embedder-rs/src/main.rs` `try_resolve_file`, the `File::create`/`io::copy` steps have no
  cleanup-on-error path before the `rename`, so a partial download leaves `<filename>.tmp-<pid>`
  (L4)

---

<a id="f3"></a>
## F3 — MCP Streamable HTTP transport (on top of stdio) 🟢 Done (2026-10-19)

Add the MCP Streamable HTTP transport beside the existing stdio transport, so MCP clients can
connect over HTTP (remote/multiplexed) as well as spawn-as-child stdio.

- **Design:** a dedicated `/mcp` listener runs inside the daemon on its own port
  (`WHATSRUST_MCP_PORT`, off by default), serving only the `/mcp` route. It supports dual-era
  clients: modern (2026-07-28) and legacy (2025-11-25, 2025-06-18) from one stateless endpoint.
  In-process dispatch to the existing REST handlers via a shared tool registry
  (`src/mcp_registry.rs`). Separate configuration: `WHATSRUST_MCP_TOKEN` (required, ≥22 characters,
  must differ from `WHATSRUST_API_TOKEN`), `WHATSRUST_MCP_BIND` (default `127.0.0.1`, independent of
  `WHATSRUST_BIND`), `WHATSRUST_MCP_ALLOW_REMOTE` (opt-in for a non-loopback bind, logs a loud
  warning), and `WHATSRUST_MCP_READ_ONLY` (server-side read-only switch). The stdio proxy keeps its
  transport but shares the registry (annotations in `tools/list`, the same argument validation).
- **References:** design doc `docs/plans/2026-10-15-mcp-http-transport-design.md` (§14 has the phase
  descriptions and exit criteria); ADRs 0044 (request gate), 0045 (transport, accepted + amended),
  0046 (declared arguments only), 0047 (sync-send timeout returns `job_id`).

### Breakdown

- [x] Design pass + ADR
- [x] Phase 0a: pure refactors (head/body split, ApiResponse, RestRequest, CallContext) — done
  2026-10-17 (behaviour-neutral; byte-for-byte REST golden test)
- [x] Behaviour-change step after 0a: gate + token before the body read, lingering drain, new reason
  phrases, 415 code, 411 chunked, `Expect: 100-continue` after auth (other expectations 417), read
  deadlines — done 2026-10-17
- [x] Phase 0b: config enforcement (REST rules; the MCP-listener rules moved to Phase 2) — done
  2026-10-18
- [x] Phase 1: shared registry (access_class, integral float normalisation, ADR 0046 amendment) —
  done 2026-10-18
- [x] Phase 2: `/mcp` skeleton (opt-in, separate bind, routing, era detection, read-only switch) —
  done 2026-10-18 (initialize, ping, tools/list live; tools/call stubbed)
- [x] Phase 3: legacy request handling (tools/call live, lanes, golden test, official SDK test) —
  done 2026-10-18 (tools/call in-process via the registry, permit lanes split by stage, 40 s tool
  budget from body completion; SDK gate passed; the proxy test moved to Phase 5)
- [x] Phase 4: modern request handling (dual-era staging) — done 2026-10-19 (dual-era live; rmcp
  3.5.1 modern gate passed)
- [x] Phase 5: polish, logging, metrics, documentation — done 2026-10-19 (four log targets,
  `/api/status` mcp_http snapshot, Docker proxy playbook, docs/HOWTO.md §5a, ADR 0045 amendment)
- [x] Known nit: stdio `initialize` — done 2026-10-20 (see the issues table)

### Issues encountered

| Date | Issue | Resolution | Reference |
|---|---|---|---|
| 2026-10-15 | The MCP `whatsrust_send` schema declared `link_preview` and `schedule_at`, but the tool calls `/api/send?sync=true`, which rejects both with 400, so they could never work via MCP | Both fields removed from the MCP schema; REST and CLI keep them | ADR 0046 |
| 2026-10-15 | A sync send that times out must not be retried blindly | 30 s timeout returns HTTP 504 with the `job_id` and "do not retry"; the job stays queued and is usually delivered later | ADR 0047 |
| 2026-10-20 | Stdio `initialize` advertised `2024-11-05` (before tool annotations), returned no `instructions` and had no `ping` | Stdio negotiates 2025-11-25 / 2025-06-18 through the shared helpers in `src/mcp_registry.rs`, returns `instructions`, answers `ping` | 858de51; ADR 0045 amendment |

### Open items (follow-ups, not in the MVP)

- [ ] Rate limiting for read operations and non-queued network-touching tools (roadmap P1; F5 does
  not cover it)
- [ ] ADR 0047 job_id cell: a spawned enqueue publishes the `job_id`, so the rare MCP tool-budget
  timeout that stalls before the bridge's own 30 s wait can still report it (roadmap P2)
- [ ] `whatsrust_send_async` tool: non-blocking send that returns the `job_id` at once (roadmap P3)
- [ ] Two-token rotation with overlap; a token change needs a daemon restart today (roadmap P3)
- [ ] Native TLS for the MCP listener; the first release uses a reverse proxy for HTTPS (roadmap P3)
- [ ] F5 per-token access profiles, built on the `AccessClass` infrastructure (roadmap P3)
- [ ] Known nits (roadmap P3): HEAD `/mcp` returns 405 with a body; `#[cfg(test)]` capture hooks in
  `log_request`/`accept_loop` and a storage test that scans source text

---

<a id="f4"></a>
## F4 — Multi-account support ⚪ Planned (needs design)

Run/route multiple WhatsApp accounts from one daemon (or coordinated daemons).
- **Current state:** the `bridge_id` field already exists ("for multi-number routing"), BUT the
  bridge is **single-device only** (ARCHITECTURE.md: "No `device_id` column"; a single-instance file
  lock per db_path; one `WhatsAppBridge` per process). So multi-account is a substantial new
  capability, not a config tweak.
- **Open design questions (significant — grill before building):** one daemon hosting N bridges vs N
  daemons; per-account SQLite DB vs a shared DB with account scoping; how the instance-lock model
  changes; how API/MCP/CLI select the target account (path prefix? header? tool param?); event-bus
  routing per account; QR/pairing per account; resource/memory implications vs the lean ethos. →
  Needs its own design doc + ADR(s).
- **Investigation scope (2026-10-15):**
  - **Storage:** one DB holding all accounts (account-scoped tables) vs one DB per account.
  - **Process model:** one process per account vs one process hosting N bridges.
  - **Every existing feature must work per account:** backfill and its pacing (anti-ban limits are
    per account), FTS search, embeddings + the drain worker + per-model purge, the pairing-time
    device name (ADR 0043), outbound queue and rate limiting, SSE events, the storage watchdog.
  - **MCP and API addressing (open):** a tool/route parameter selecting the account vs a
    per-account endpoint (e.g. `/mcp/<account>`, `/api/<account>/...`) vs one MCP server per
    account process. Interacts with F3 (HTTP MCP) and F5 (profiles: per-account access?).
  - **Cross-feature:** F6 (encryption): one key for all accounts or one per account.
- [ ] Investigation + design doc + ADR(s) (roadmap P3, not ranked by the owner)
- [ ] Implementation

---

<a id="f5"></a>
## F5 — Access profiles: read-only vs full access ⚪ Planned (needs investigation)

Run whatsrust with a **read-only** profile (list, read, search, status, history, events, and so
on) as well as the current **full-access** behaviour, so an agent or remote client can be given
read access without the ability to send, delete or change anything.
- **Open questions (investigate before design):**
  - **Where a profile applies:** per process (a startup setting), per token (e.g. a separate
    read-only token alongside the full one), per transport (REST / MCP stdio / MCP HTTP), or a mix.
  - **Classifying every operation:** some are clearly read-only (status, groups, group-info,
    history, search, fetch-status, events, QR?). Others change state on WhatsApp without sending
    a message (mark-read, typing/presence, subscribe, pin/mute/archive, the first
    `group-invite-link` call), and `fetch-history` triggers network fetches with ban-risk pacing.
    Each needs a deliberate classification.
  - **Enforcement point:** the REST router (covers the CLI and the stdio MCP proxy) plus the MCP tool
    registry. F3 adds a server-side access class (Read/Write) in the tool registry, plus the
    `WHATSRUST_MCP_READ_ONLY` switch. Profiles build on the access class, not on the advisory hints.
  - REPL behaviour under a read-only profile, and how this composes with the ADR 0044 request gate
    and tokens.
- **Related:** F3 (tool metadata hook), F4 (per-account profiles?).
- [ ] Investigation + design + ADR (roadmap P3)
- [ ] Implementation

---

<a id="f6"></a>
## F6 — Database encryption at rest ⚪ Planned (needs investigation)

Encrypt `whatsapp.db`, which holds the Signal keys, the message history, the search index and
embeddings.
- **Unlock process (user's initial lean):** the passphrase is **entered when the daemon starts** —
  not from an env var and not from a command-line argument (both leak through process listings,
  shell history and `.env` files).
- **Open questions (investigate before design):**
  - **Mechanism:** SQLCipher via rusqlite's `bundled-sqlcipher` feature (page-level encryption,
    covers the WAL too) vs alternatives. Impact on build, binary size and query speed, and FTS5 /
    `incremental_vacuum` compatibility.
  - **Prompting:** read from the terminal without echo, and before the REPL starts (the REPL owns
    stdin). What happens with no terminal (`make run` from a script, launchd/systemd): fail closed
    with a clear message? A later optional unlock source (e.g. the macOS Keychain)?
  - **Migrating an existing plaintext DB**, wrong-passphrase handling, and passphrase change.
  - **Everything else that holds a copy of the data:** startup and `--rollback` backups
    (`whatsapp.db.backups/`, pre-migration `.bak` files) must be encrypted too, or they defeat the
    point. `--rollback` / `--migrate` need the passphrase. The CLI and the stdio MCP process don't
    open the DB (they talk to the daemon), so they need no passphrase.
  - Key handling in memory, and the threat model (lost/stolen disk vs a live compromised machine).
- **Related:** F4 (one key for all accounts or one per account).
- [ ] Investigation + design + ADR (roadmap P3, not ranked by the owner)
- [ ] Implementation

---

<a id="f7"></a>
## F7 — Richer read tools (names, counts, contacts, groups, pagination) ⚪ Planned (needs viability check)

Enhance read-only surfaces (search, history, status) with contact/sender names, message counts,
contact and group listing/search, and pagination for search results.
- **Cross-feature impact:** New/extended MCP tools (ADR 0046 strict schemas apply); F3 registry (access class: Read); F5 read-only profiles benefit.
- **Investigation scope (2026-10-17):**
  1. **Contact/sender names in search results.** Display contact name (1:1) or group name + sender's contact name (groups) in search/history results.
     - **Current state:** `InboundRow` (storage.rs:945) has `sender_jid` (phone@s.whatsapp.net or LID), not display names. `device.push_name` (storage.rs:61) stores only OUR device's name. `GroupInfo` (bridge.rs:298) has `subject` but no participant names.
     - **What's needed:** Persist push names received in inbound messages, OR fetch on-demand via wa-rs `UserInfo` (wacore/iq/usync.rs:323, has `status` field for about text, but no push_name getter). Group context adds the group subject; sender name resolution is the same problem.
     - **Verdict:** Viable with work — persist names from inbound `Event::Message` (likely carries push_name in protobuf, needs wa-rs source check), or fetch via contacts API.
     - **Owner request (2026-10-22): sender information on historical messages.** Messages read back from history, whether live, fetched or from the pairing package, should say who sent them: display name and phone number, not only `sender_jid`. Sources (checked in the wa-rs pin `9e8c70e`): live `MessageInfo.push_name` (wacore/src/types/message.rs:305); history `WebMessageInfo.pushName` (whatsapp.proto field 19), plus `participant` for the sender in a group. A `@lid` sender must resolve to the phone number (the same LID-mapping gap as the `@lid` DM split under F1). Needs: where to keep names (a column on `messages` vs a contacts table; design), and the fields in `history` / `search` output (REST + MCP schemas, ADR 0046).
     - **Update (2026-10-23): more name sources, and the dependencies.** Live `push_name` already reaches `WhatsAppInbound.push_name` (bridge.rs:652) but is not stored. Two more sources: the pairing package's push-name list (`HistorySync.pushnames`, field 7; not read today), and the owner's address-book names from the app-state sync (`Event::ContactUpdate`, only logged at debug level today, bridge.rs:3712; `Event::PushNameUpdate` at :3715 reports a contact's new push name). Depends on the `@lid` DM split (F1) for the LID→phone resolution, and on a working app-state sync for address-book names (the re-pair bug, roadmap P1). Push names do not need the app-state sync. Rank: roadmap P2, right after the `@lid` split.
  2. **Message counts (overall, per-chat, unread); attachment counts.**
     - **Current state:** `messages` table (storage.rs:205) supports `COUNT(*)` for overall and `WHERE chat_jid=?` for per-chat. Attachments countable via `media_refs` (storage.rs:226) or `content_kind` filter. Unread state: bridge receives `Event::ChatRead`/`Mute`/`Archive` (bridge.rs:3717) from app-state sync, but does NOT persist it locally.
     - **What's needed:** Counts are local queries (ready). Unread needs a chat-state table (mute/archive/unread bool, last-read timestamp?) and an insert/update path from the app-state events.
     - **Verdict:** Viable now for counts; viable with work for unread (persist app-state chat status).
  3. **List contacts; search contact names and descriptions (about text).**
     - **Current state:** NO contacts table. Only JIDs seen in `messages` exist. No names or status/about stored.
     - **What's needed:** Define the contact list (app-state contacts sync vs seen-JIDs heuristic). Persist `UserInfo` (wacore/iq/usync.rs:323: `status` = about text). wa-rs `Contacts` feature (contacts.rs:67) has `is_on_whatsapp()` check; unclear if a "get all contacts" API exists (may need app-state sync investigation).
     - **Verdict:** Viable with work — define contact-list source, persist UserInfo, build search index.
  4. **Search groups and group descriptions; resolve member names.**
     - **Current state:** `GroupCache` (bridge.rs:97) stores `GroupInfo` with `subject` (line 299), NO description. wa-rs `GroupMetadata` (groups.rs:95) HAS `description: Option<String>` (line 109), fetched via `get_metadata()` (bridge.rs:1242). Member names: same issue as #1.
     - **What's needed:** Add `description` field to `GroupInfo`, populate from wa-rs. Persist groups in SQLite (currently 5-min in-memory cache). Member name resolution identical to #1.
     - **Verdict:** Viable with work — extend GroupInfo, persist groups, resolve member names (same as #1).
  5. **Pagination for search results (in addition to history, which already has it).**
     - **Current state:** `/api/history` (api.rs:1285) and MCP `whatsrust_history` (mcp.rs:150) both support `before` timestamp pagination. `/api/search` (api.rs:1298) and MCP `whatsrust_search` (mcp.rs:155) do NOT expose `before` in their schemas, BUT the underlying `bridge.search()` (bridge.rs:1607) and `store.search_inbound()` (storage.rs:1415) already accept `before_ts: Option<i64>`.
     - **What's needed:** Add `before` parameter to `/api/search` query string and MCP `whatsrust_search` tool schema (ADR 0046: `additionalProperties: false`, so a schema change is required).
     - **Verdict:** Viable now — implementation ready, needs schema update (REST + MCP).
- [ ] Investigation + design
- [ ] Implementation

---

<a id="f8"></a>
## F8 — Documentation overhaul (README = what, operations guide = how, diagrams) 🟡 Started (planning docs + CHANGELOG, 2026-10-23)

Restructure the user-facing docs so each answers one question (added 2026-10-19).
- **README (the what):** say up front that this is an experimental, feature-rich fork (link the
  upstream project) and what it adds; keep the overview, features and pointers; drop the build
  section (not relevant to readers of the README); move CLI/API/config/MCP detail out.
- **Operational guide (the how):** one place for build, configure, run, CLI, REST, MCP (stdio and
  HTTP), embedder, proxies, fail2ban, and every helper script (`scripts/*`, `tools/*`,
  `deploy/proxy/`) with when and how to use it. Decide: extend `docs/HOWTO.md` or split it.
- **CONTRIBUTING:** review and update (Makefile targets, ignored SDK/proxy tests, commit style,
  docs matrix and change checklist, ADR rule, "ask before changing behaviour" golden-test rule).
- **ARCHITECTURE:** update to the current system (MCP HTTP listener, registry, request pipelines,
  workers) and add diagrams (Mermaid: component map, inbound/outbound data flow, REST and MCP
  request pipelines, backfill/embedding workers).
- **Planning docs (done ahead, 2026-10-23):** FEATURES.md holds the feature detail and issues; the
  roadmap holds the feature map, the one priority backlog and the completed list; `CHANGELOG.md`
  started (summarized fork history, then one line per change); the doc matrix in `AGENTS.md` lists which
  docs to update for each kind of change.
- [ ] Plan (target structure, what moves where)
- [x] Planning docs restructure + CHANGELOG + doc-update checklist — done 2026-10-23
- [ ] README + operational guide (roadmap P2)
- [ ] CONTRIBUTING (roadmap P2)
- [ ] ARCHITECTURE + diagrams (roadmap P2)

---

<a id="f9"></a>
## F9 — Dev run profiles (debug runs in their own folders) 🟢 Done (2026-10-19; owner step open)

Keep each debug/live run's state out of the project root (added 2026-10-19).
- **Why:** the daemon keeps everything relative to its working directory (`.env` via
  `WHATSRUST_ENV_FILE` or `./.env`, `whatsapp.db*`, `whatsapp.db.backups/`, the instance lock,
  pre-migration `.bak` files) and runs are started from the repo root, so the root collects DBs,
  backups and logs from every run.
- **As built:** one gitignored folder per profile, `runs/<profile>/`, with its own `.env`, DB and
  `logs/`. `scripts/run-profile.sh` (`new [--semantic] [--mcp]`, `list [--tmux]`, `daemon`, `attach`, `cli`,
  `mcp`, `env`, `mcp-on`, `mcp-off`, `mcp-client`) and `make new-profile` (`SEMANTIC=1`, `MCP=1`) /
  `make run-profile` / `make attach-profile`; no Rust change. `mcp-on` / `mcp-off` switch the profile's HTTP MCP
  listener (§5a) in its `.env`: port = the reserved one, else REST+1, else the next free; token = kept, else
  the exported `WHATSRUST_MCP_TOKEN` (validated), else `openssl rand -base64 32`; the `.env` is rewritten with
  bash builtins only (token never printed, never in argv, file stays mode 600); `mcp-off` keeps the token line;
  `mcp-client` prints the URL and a Claude Code config using `${WHATSRUST_MCP_TOKEN}` (2026-10-20). `daemon` runs in the
  current terminal by default; `--tmux` (`USE_TMUX=1` for make) opts in to session `whatsrust-<profile>`
  (attach when interactive; an exit note keeps the pane open until Enter) and `--detach` implies `--tmux`;
  with tmux missing, `--tmux` is an error, never a silent fallback. `list` shows every profile with a TMUX
  column, `list --tmux` only those running in a session (2026-10-19, tmux made opt-in 2026-10-20).
  `clear-data <profile> [--all] [--dry-run] [--yes]` empties a profile's message data and keeps its pairing:
  it refuses while the daemon runs, prints the exact SQL, asks y/N and then for the profile name, and backs up
  to `whatsapp.db.pre-clear-<UTC>.bak` first (b9c7dd2, 2026-10-21).
  Docs: HOWTO §3 "Run profiles" (incl. a demo playbook).
- [x] Approach decision — A: profile folders + launcher script (owner, 2026-10-19)
- [x] Script + Makefile target + docs — done 2026-10-19
- [x] `clear-data` command (message data only, pairing kept) — done 2026-10-21
- [ ] Move existing root run state into `runs/main/` (owner, daemon stopped; HOWTO checklist; roadmap P1)
