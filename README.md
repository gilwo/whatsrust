<div align="center">

# WhatsRust

**WhatsApp in pure Rust. One 5 MB binary. 15 MB RAM. No Node.js required.**

<br />

[![Star this repo](https://img.shields.io/github/stars/199-biotechnologies/whatsrust?style=for-the-badge&logo=github&label=%E2%AD%90%20Star%20this%20repo&color=yellow)](https://github.com/199-biotechnologies/whatsrust/stargazers)
&nbsp;&nbsp;
[![Follow @longevityboris](https://img.shields.io/badge/Follow_%40longevityboris-000000?style=for-the-badge&logo=x&logoColor=white)](https://x.com/longevityboris)

<br />

[![Rust](https://img.shields.io/badge/Rust-stable-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org)
&nbsp;
[![License: MIT](https://img.shields.io/badge/License-MIT-blue?style=for-the-badge)](LICENSE)
&nbsp;
[![59 API Endpoints](https://img.shields.io/badge/API-59_endpoints-green?style=for-the-badge)](https://github.com/199-biotechnologies/whatsrust#api)
&nbsp;
[![35 MCP Tools](https://img.shields.io/badge/MCP-35_tools-blueviolet?style=for-the-badge)](https://github.com/199-biotechnologies/whatsrust#mcp-server-for-ai-agents)
&nbsp;
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen?style=for-the-badge)](https://github.com/199-biotechnologies/whatsrust/pulls)

---

A single Rust binary that handles the full WhatsApp Web protocol: text, images, audio, video, stickers, reactions, edits, polls, status stories, groups, and chat management. No npm. No Python. No Docker. Just `cargo build` and scan the QR code.

[Install](#install) | [How It Works](#how-it-works) | [Features](#features) | [API](#api) | [MCP Tools](#mcp-server-for-ai-agents) | [Contributing](#contributing)

</div>

---

## Why WhatsRust exists

Every WhatsApp automation library follows the same playbook: Node.js, Baileys, and a `node_modules` folder the size of a small country. It works until 3 AM when the process crashes, the memory leak finally wins, and you're reading someone else's JavaScript trying to figure out what went wrong.

We needed WhatsApp inside agent software and got tired of babysitting a Node sidecar. So we replaced it with one Rust binary.

### Before vs After

| | Node.js + Baileys | **WhatsRust** |
|---|---------|-----------|
| **Language** | JavaScript / Node.js | Rust |
| **Install size** | ~200 MB with node_modules | **5 MB** single binary |
| **Memory usage** | ~50-120 MB idle | **~15 MB** |
| **Dependencies** | `npm install` + hope | `cargo build` |
| **Crash recovery** | Roll your own | SQLite-backed queue + per-message backoff |
| **API** | None built-in | **59 REST endpoints** + CLI + MCP server |
| **Status/stories** | DIY | Text, image, video, revoke -- all built in |
| **Chat management** | Manual API calls | **12 operations** (pin, mute, archive, star...) |
| **Group management** | Manual | **12 methods**, full CRUD |
| **Anti-ban protection** | None | Read receipt batching, typing sim, jitter |
| **AI agent support** | None | **35 MCP tools** for Claude Code, etc. |
| **History & search** | DIY | Paced per-chat backfill + FTS5 lexical + opt-in semantic search (embedder sidecar, multilingual, relevance-ranked) |

---

## Install

```bash
git clone https://github.com/199-biotechnologies/whatsrust
cd whatsrust && cargo run
```

Scan the QR code with your phone. That's it.

```bash
# Phone number pairing instead of QR
WHATSAPP_PAIR_PHONE="+1234567890" cargo run

# With sender allowlist + custom port
WHATSAPP_ALLOWED="1234567890" WHATSRUST_PORT=8080 cargo run
```

---

## How it works

WhatsRust runs as a daemon with a built-in REPL, REST API, and MCP server. Every outbound message goes through a durable SQLite queue -- messages survive crashes and restarts. Inbound messages are deduplicated with a generation-tracked concurrent map.

```
src/
  bridge.rs          Core: events, messaging, queue, groups, chat management
  outbound.rs        21 typed outbound ops, durable SQLite queue
  media_utils.rs     Media enrichment: image dims + thumbnails, audio waveform
  bridge_events.rs   Broadcast event bus (tokio::sync::broadcast)
  api.rs             REST API (59 endpoints) + SSE + CLI HTTP client
  mcp.rs             MCP server (35 tools, JSON-RPC over stdio)
  storage.rs         rusqlite store + v8 messages table (FTS5, embed_status) + embeddings (vectors) + schema migrations
  backfill.rs        Historical backfill worker (on-demand fetch, FIFO, pacer)
  polls.rs           Poll crypto (HKDF-SHA256 + AES-256-GCM)
  dedup.rs           Generation-tracked DashMap (concurrent-safe)
  read_receipts.rs   Batched receipt scheduler
  qr.rs              QR rendering (terminal/PNG/HTML/SVG)
  instance_lock.rs   flock-based single-instance guard
  main.rs            Daemon (REPL) + CLI (50 commands) + MCP mode
  lib.rs             Library crate exports
```

~19,600 lines across 15 files. Built on [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust) (oxidezap main @ 9e8c70e2, post-v0.6.0; MIT) for the protocol layer.

### Four ways to run it

| Mode | How |
|------|-----|
| **Daemon** | `cargo run` -- REPL with 50 commands |
| **CLI** | `whatsrust send 15551234567 "Hello"` -- JSON to stdout |
| **Library** | `WhatsAppBridge::start(config, tx, cancel)` -- embed in your Rust app |
| **MCP server** | `whatsrust mcp` -- 35 tools for AI agents (Claude Code, etc.) |

---

## Features

### Every WhatsApp message type

Text, image, audio/voice, video, document, sticker, location, contact card, reaction (add/remove), edit, revoke, reply/quote, forward, view-once (image/video), poll (create + encrypted vote decryption), status/story (text/image/video/revoke).

### Chat management (12 operations)

Pin/unpin, mute/unmute, archive/unarchive, mark read/unread, delete chat, delete message for me, star/unstar. Direct app-state mutations that sync across all linked devices.

### Full group management

List groups, get info, create, rename, set description, add/remove/promote/demote participants, invite links. Group metadata cached with TTL and mutation invalidation.

### Status/story posting

Text stories with custom background colors and fonts. Image and video stories. Revoke posted stories. Privacy controls (contacts, allowlist, denylist). All go through the durable outbound queue.

### Historical fetch + local search (lexical + semantic)

Trigger a per-chat backfill on demand -- fetch `all` history, everything `since` a timestamp, or a `count` of messages -- and watch it run as a paced background job with the same anti-ban discipline as live sends. The daemon clamps every request (cooldown, queue depth, max messages) so a misbehaving script or agent can't hammer WhatsApp.

Once fetched, search your history locally: **FTS5 lexical** (BM25 relevance-ranked, fully offline, multilingual — Hebrew, Arabic, and more) is always-on. **Semantic search** (embedding-based, via an opt-in embedder sidecar) layers on top: configure `WHATSRUST_EMBEDDER_CMD` and the bridge will embed the query, recall FTS candidates, rerank by cosine similarity, and fall back additively to lexical results. When the sidecar is absent or down, search degrades gracefully to pure FTS5 (no errors, no blocking). The MVP sidecar (`scripts/embedder-sidecar.py`) uses sentence-transformers + multilingual MiniLM (384-dim). End-to-end validation is pending; the semantic path is code-complete but not yet live-tested against a running daemon.

### Stays alive

- **Crash-safe outbound queue** in SQLite. Messages survive restarts.
- **Atomic dedup** with generation-tracked DashMap. No double-processing under concurrent handlers.
- **Per-message exponential backoff.** One stuck message doesn't block the rest.
- **Graduated reconnect backoff** with jitter. Not the naive kind.
- **Graceful shutdown.** Drains in-flight messages on SIGINT/SIGTERM.
- **Single-instance flock.** Two bridges can't fight over one session.
- **SQLite backup** on startup and shutdown. WAL mode, no corruption.

### Doesn't get you banned

- **Read receipt batching.** Groups message IDs per chat into single stanzas on a 200 ms coalesce.
- **Flush-before-reply.** Marks messages as read before responding.
- **Recording indicator** before voice notes.
- **Configurable send pacing** with randomized jitter intervals.
- **Auto presence management.** Available on connect, unavailable on shutdown.

---

## CLI

With the daemon running, use `whatsrust` as a one-shot CLI. Every command returns JSON to stdout:

```bash
# Messaging
whatsrust send 15551234567 "Hello from the CLI"
whatsrust image 15551234567 /tmp/photo.jpg "Check this out"
whatsrust video 15551234567 /tmp/clip.mp4
whatsrust react 15551234567 MSG_ID "thumbsup"
whatsrust poll 15551234567 1 "Lunch?" -- "Pizza" "Sushi" "Tacos"

# Status/stories
whatsrust status-text 15551234567 "Hello world"
whatsrust status-image 15551234567 /tmp/photo.jpg

# Chat management
whatsrust pin-chat 15551234567
whatsrust mute-chat 15551234567
whatsrust archive-chat 15551234567
whatsrust mark-read 15551234567
whatsrust delete-chat 15551234567

# Groups
whatsrust groups
whatsrust group-info 120363012345678901@g.us
whatsrust group-create "My Group" 15551234567 15559876543

# History & search
whatsrust fetch-history 15551234567 all
whatsrust fetch-history 15551234567 since 1700000000000
whatsrust fetch-history 15551234567 count 500
whatsrust fetch-status
whatsrust fetch-status 42
whatsrust fetch-cancel 42
whatsrust search "dinner plans"
whatsrust search "printer" 15551234567
```

Output:
```json
{"ok": true, "id": "3EB0A1B2C3D4E5F6"}
```

---

## API

58 REST endpoints on `localhost:7270`. JSON in, JSON out. No framework -- raw TCP for minimal overhead.

### Messaging

| Endpoint | Body |
|----------|------|
| `POST /api/send` | `{jid, text, mentions?, schedule_at?}` |
| `POST /api/reply` | `{jid, id, sender, text}` |
| `POST /api/edit` | `{jid, id, text}` |
| `POST /api/react` | `{jid, id, emoji}` |
| `POST /api/revoke` | `{jid, id}` |
| `POST /api/image` | `{jid, data, mime?, caption?}` |
| `POST /api/video` | `{jid, data, mime?, caption?}` |
| `POST /api/audio` | `{jid, data}` |
| `POST /api/poll` | `{jid, question, options, selectable_count}` |
| `POST /api/forward` | `{jid, msg_id}` |

### Status/stories

| Endpoint | Body |
|----------|------|
| `POST /api/status-text` | `{recipients, text, background_argb?, font?}` |
| `POST /api/status-image` | `{recipients, data, caption?}` |
| `POST /api/status-video` | `{recipients, data, caption?}` |
| `POST /api/status-revoke` | `{recipients, message_id}` |

### Chat management

| Endpoint | Body |
|----------|------|
| `POST /api/pin-chat` | `{jid}` |
| `POST /api/mute-chat` | `{jid}` |
| `POST /api/archive-chat` | `{jid}` |
| `POST /api/mark-read` | `{jid}` |
| `POST /api/delete-chat` | `{jid}` |
| `POST /api/delete-for-me` | `{jid, id}` |
| `POST /api/star` | `{jid, id}` |

### History & search

| Endpoint | Body / Query |
|----------|------|
| `GET /api/history` | `?jid=<chat_jid>&limit=<n>&before=<epoch_ms>` |
| `GET /api/search` | `?q=<query>&jid=<chat_jid>?&limit=<n>` (FTS5, BM25 relevance-ranked) |
| `POST /api/history-fetch` | `{chat_jid, mode: all\|since\|count, target_value?}` -- trigger a backfill job |
| `GET /api/history-fetch` | `?job_id=<n>` for one job, or `?active=true` to list active jobs |
| `POST /api/history-fetch/cancel` | `{job_id}` |

Plus: SSE streaming (`GET /api/events` -- outbound status, backfill progress per-batch/on-completion, storage-growth alerts), groups, presence, health check, QR.

Async sends return `{ok, job_id}`. Add `?sync=true` to wait for the WhatsApp message ID.

---

## MCP server for AI agents

Run `whatsrust mcp` to start a Model Context Protocol server with 35 tools over JSON-RPC stdio. Connect it to Claude Code, Cursor, or any MCP-compatible AI agent and your agent can send messages, manage groups, post stories, handle chat operations, and search or backfill chat history (`whatsrust_search`, `whatsrust_fetch_history`, `whatsrust_fetch_status`, `whatsrust_fetch_cancel`) through WhatsApp.

Works out of the box with Claude Code -- just add it to your MCP config and your AI agent gets full WhatsApp access.

---

## Use as a library

Designed to be embedded. About 10 lines to wire up:

```rust
use whatsrust::bridge::{BridgeConfig, WhatsAppBridge, WhatsAppInbound};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

let (inbound_tx, mut inbound_rx) = mpsc::channel::<WhatsAppInbound>(256);
let cancel = CancellationToken::new();

let bridge = WhatsAppBridge::start(
    BridgeConfig {
        db_path: "whatsapp.db".into(),
        health_port: 8080,
        ..Default::default()
    },
    inbound_tx,
    cancel.clone(),
);

// Send
bridge.send_message_with_id("1234567890", "Hello from Rust").await?;

// Receive
while let Some(msg) = inbound_rx.recv().await {
    println!("{}: {}", msg.sender, msg.content.display_text());
}
```

Inbound messages arrive as `WhatsAppInbound` with sender, JID, message ID, reply context, `MessageFlags` (forwarded, view-once), and typed content. Media arrives as raw bytes -- no second download step.

---

## Config

| Variable | Default | What it does |
|----------|---------|-------------|
| `WHATSAPP_PAIR_PHONE` | *(QR mode)* | Phone number for pair-code linking |
| `WHATSAPP_ALLOWED` | *(everyone)* | Comma-separated sender allowlist |
| `WHATSRUST_PORT` | `7270` | API server port (0 = disabled) |
| `WHATSRUST_BIND` | `127.0.0.1` | API bind address |
| `WHATSRUST_API_TOKEN` | *(none)* | Bearer token for API auth |
| `BACKUP_DIR` | `whatsapp.db.backups` | SQLite backup directory |
| `RUST_LOG` | `info` | Log level (`debug` for protocol) |

---

## Contributing

PRs welcome. Run `cargo test` and `cargo clippy` before submitting. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)

---

<div align="center">

Built by [Boris Djordjevic](https://github.com/longevityboris) at [199 Biotechnologies](https://github.com/199-biotechnologies) | [Paperfoot AI](https://paperfoot.ai)

<br />

**If this is useful to you:**

[![Star this repo](https://img.shields.io/github/stars/199-biotechnologies/whatsrust?style=for-the-badge&logo=github&label=%E2%AD%90%20Star%20this%20repo&color=yellow)](https://github.com/199-biotechnologies/whatsrust/stargazers)
&nbsp;&nbsp;
[![Follow @longevityboris](https://img.shields.io/badge/Follow_%40longevityboris-000000?style=for-the-badge&logo=x&logoColor=white)](https://x.com/longevityboris)

</div>
