// Byte-index slicing of a `str` (`&s[a..b]`) panics when an index is not on a
// UTF-8 char boundary. A panic in the daemon aborts the process (release builds
// use `panic = "abort"`), as happened on 2026-10-19 with a reply preview that
// cut a multi-byte character. Use `str::get`, `split_once`, `strip_prefix`,
// `floor_char_boundary` etc. instead; where a slice is provably safe, scope an
// `#[allow(clippy::string_slice)]` to the smallest item with a `// safe:` reason.
#![warn(clippy::string_slice)]

pub mod api;
pub mod backfill;
pub mod bridge_events;
pub mod dedup;
pub mod embed_drain;
pub mod embedder;
pub mod instance_lock;
pub mod mcp;
pub mod mcp_http;
pub mod mcp_registry;
pub mod media_utils;
pub mod outbound;
pub mod polls;
pub mod qr;
pub mod read_receipts;
pub mod storage;
pub mod bridge;
