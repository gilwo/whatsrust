//! Smoke test: the Rust MCP SDK (`rmcp`, Streamable HTTP client) against the whatsrust MCP listener.
//! Run: WHATSRUST_MCP_TOKEN=... cargo run --manifest-path tools/mcp-smoke-rs/Cargo.toml -- http://127.0.0.1:7271/mcp [--mode modern|auto|legacy] [--read-only]
//! The token comes from the environment, never argv. Exits 0 when the handshake, tools/list, a tool call and an unknown-tool error all behave.

use anyhow::{bail, ensure, Context, Result};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, ProtocolVersion,
    RequestMetaObject,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientLifecycleMode, ClientServiceExt, ServiceError};

/// The `rmcp` version `Cargo.toml` pins (`=3.5.1`); keep the two in step.
const RMCP_VERSION: &str = "3.5.1";

/// How the client starts the session.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Force protocol 2026-07-28: `server/discover`, no `initialize`; a legacy answer is an error.
    Modern,
    /// The SDK's own behaviour: probe with `server/discover`, fall back to `initialize`.
    Auto,
    /// Force the `initialize` handshake at protocol 2025-11-25.
    Legacy,
}

impl Mode {
    fn parse(text: &str) -> Result<Self> {
        match text {
            "modern" => Ok(Self::Modern),
            "auto" => Ok(Self::Auto),
            "legacy" => Ok(Self::Legacy),
            other => bail!("unknown --mode {other:?} (use modern, auto or legacy)"),
        }
    }
}

struct Args {
    url: String,
    mode: Mode,
    read_only: bool,
}

fn parse_args() -> Result<Args> {
    let mut url = None;
    let mut mode = Mode::Auto;
    let mut read_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => mode = Mode::parse(&args.next().context("--mode needs a value")?)?,
            "--read-only" => read_only = true,
            flag if flag.starts_with("--") => bail!("unknown option {flag}"),
            _ if url.is_none() => url = Some(arg),
            _ => bail!("unexpected argument {arg:?}"),
        }
    }
    let url = url.context("usage: mcp-smoke-rs <url> [--mode modern|auto|legacy] [--read-only]")?;
    Ok(Args {
        url,
        mode,
        read_only,
    })
}

fn client_config(protocol_version: ProtocolVersion) -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("mcp-smoke-rs", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(protocol_version)
}

/// Start the session the way `mode` says.
async fn connect(args: &Args, token: &str) -> Result<RunningService<RoleClient, ClientConfig>> {
    let config =
        StreamableHttpClientTransportConfig::with_uri(args.url.as_str()).auth_header(token);
    let transport = StreamableHttpClientTransport::from_config(config);
    let client = match args.mode {
        Mode::Modern => {
            let lifecycle = ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            };
            client_config(ProtocolVersion::V_2026_07_28)
                .serve_with_lifecycle(transport, lifecycle)
                .await?
        }
        Mode::Auto => {
            let lifecycle = ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
            };
            client_config(ProtocolVersion::V_2026_07_28)
                .serve_with_lifecycle(transport, lifecycle)
                .await?
        }
        Mode::Legacy => {
            let config = client_config(ProtocolVersion::LATEST_WITH_INITIALIZE);
            config
                .serve_with_lifecycle(transport, ClientLifecycleMode::Initialize)
                .await?
        }
    };
    Ok(client)
}

async fn run(args: Args, token: &str) -> Result<()> {
    let client = connect(&args, token)
        .await
        .context("starting the session failed")?;
    let info = client.peer_info().context("the server sent no peer info")?;
    let negotiated = info.protocol_version.clone();
    println!("rmcp {RMCP_VERSION}");
    println!("negotiated protocol {negotiated}");
    println!(
        "server {}",
        info.server_info
            .as_ref()
            .map_or("(none)".to_string(), |s| format!(
                "{} {}",
                s.name, s.version
            ))
    );
    println!(
        "instructions {}",
        info.instructions.as_deref().unwrap_or("(none)")
    );

    match args.mode {
        Mode::Modern => ensure!(
            negotiated == ProtocolVersion::V_2026_07_28,
            "modern mode negotiated {negotiated}"
        ),
        Mode::Legacy => ensure!(
            negotiated == ProtocolVersion::V_2025_11_25,
            "legacy mode negotiated {negotiated}"
        ),
        Mode::Auto => {}
    }

    if !negotiated.has_initialize() {
        let meta = RequestMetaObject::with_client_context(
            negotiated.clone(),
            Implementation::new("mcp-smoke-rs", env!("CARGO_PKG_VERSION")),
            ClientCapabilities::default(),
        );
        let discovered = client
            .discover(meta)
            .await
            .context("server/discover failed")?;
        let versions: Vec<String> = discovered
            .supported_versions
            .iter()
            .map(ToString::to_string)
            .collect();
        println!(
            "server/discover supportedVersions {} ttlMs {}",
            versions.join(","),
            discovered.ttl_ms
        );
        ensure!(
            versions.iter().any(|v| v == "2026-07-28"),
            "server/discover does not list 2026-07-28"
        );
    }

    let tools = client.list_all_tools().await.context("tools/list failed")?;
    let expected = if args.read_only { 6 } else { 34 };
    ensure!(
        tools.len() == expected,
        "expected {expected} tools, got {}",
        tools.len()
    );
    println!("tools {}", tools.len());

    let status = client
        .call_tool(CallToolRequestParams::new("whatsrust_status"))
        .await
        .context("whatsrust_status failed")?;
    ensure!(
        status.is_error != Some(true),
        "whatsrust_status returned an error result: {:?}",
        status.content
    );
    println!("whatsrust_status ok");

    match client
        .call_tool(CallToolRequestParams::new("whatsrust_no_such_tool"))
        .await
    {
        Err(ServiceError::McpError(error)) => {
            println!("unknown tool -> JSON-RPC error {}", error.code.0)
        }
        Err(other) => bail!("an unknown tool must be a JSON-RPC error, got: {other}"),
        Ok(result) => bail!("an unknown tool must be a JSON-RPC error, got a result: {result:?}"),
    }

    client
        .cancel()
        .await
        .context("closing the session failed")?;
    println!("OK");
    Ok(())
}

#[tokio::main]
async fn main() {
    let result = async {
        let args = parse_args()?;
        let token = std::env::var("WHATSRUST_MCP_TOKEN").unwrap_or_default();
        ensure!(
            !token.trim().is_empty(),
            "set WHATSRUST_MCP_TOKEN in the environment"
        );
        run(args, token.trim()).await
    }
    .await;
    if let Err(error) = result {
        eprintln!("FAILED: {error:#}");
        std::process::exit(1);
    }
}
