# Smoke test: the official Python MCP SDK (Streamable HTTP client) against the whatsrust MCP listener.
# Run: WHATSRUST_MCP_TOKEN=... uv run --quiet --with 'mcp<2' python scripts/mcp_sdk_smoke.py http://127.0.0.1:7271/mcp [--read-only]
# The token comes from the WHATSRUST_MCP_TOKEN environment variable, never argv (argv shows in `ps`); --read-only expects the 6-tool list of WHATSRUST_MCP_READ_ONLY.
# Exits 0 when initialize, tools/list, a tool call and an unknown-tool error all behave; every HTTP request is logged to stderr.
import argparse
import asyncio
import os
import sys
from importlib.metadata import version

import httpx
from mcp import ClientSession, McpError
from mcp.client.streamable_http import streamablehttp_client


def logging_client(headers=None, timeout=None, auth=None):
    """An httpx client like the SDK's own, plus one stderr line per response (method, path, status)."""

    async def log(response):
        request = response.request
        print(f"http: {request.method} {request.url.path} -> {response.status_code}", file=sys.stderr, flush=True)

    return httpx.AsyncClient(
        headers=headers,
        timeout=timeout,
        auth=auth,
        follow_redirects=True,
        event_hooks={"response": [log]},
    )


async def run(url: str, token: str, read_only: bool) -> None:
    print(f"mcp sdk {version('mcp')}")
    headers = {"Authorization": f"Bearer {token}"}
    async with streamablehttp_client(url, headers=headers, httpx_client_factory=logging_client) as streams:
        read_stream, write_stream = streams[0], streams[1]
        async with ClientSession(read_stream, write_stream) as session:
            init = await session.initialize()
            print(f"protocolVersion {init.protocolVersion}")

            tools = (await session.list_tools()).tools
            expected = 6 if read_only else 34
            assert len(tools) == expected, f"expected {expected} tools, got {len(tools)}"
            print(f"tools {len(tools)}")

            status = await session.call_tool("whatsrust_status", {})
            assert not status.isError, f"whatsrust_status failed: {status.content}"
            print("whatsrust_status ok")

            try:
                await session.call_tool("whatsrust_no_such_tool", {})
            except McpError as err:
                print(f"unknown tool -> JSON-RPC error {err.error.code}")
            else:
                raise AssertionError("an unknown tool must be a JSON-RPC error")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("url")
    parser.add_argument("--read-only", action="store_true")
    args = parser.parse_args()
    token = os.environ.get("WHATSRUST_MCP_TOKEN", "").strip()
    if not token:
        parser.error("set WHATSRUST_MCP_TOKEN in the environment")
    asyncio.run(run(args.url, token, args.read_only))
    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
