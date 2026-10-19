#!/usr/bin/env bash
# Smoke-test deploy/proxy/nginx-mcp.conf and deploy/proxy/Caddyfile: real nginx and Caddy in Docker in
# front of a running MCP listener (loopback-bound; containers reach it as host.docker.internal).
# Needs docker (daemon running), openssl, curl. Env: WHATSRUST_MCP_TOKEN (required), MCP_UPSTREAM_PORT
# (default 7271). Exit 0 only if every check passes.
set -euo pipefail

: "${WHATSRUST_MCP_TOKEN:?set WHATSRUST_MCP_TOKEN to the token of the MCP listener}"
MCP_UPSTREAM_PORT="${MCP_UPSTREAM_PORT:-7271}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NGINX_PORT=18443
CADDY_PORT=18444
NGINX_NAME="whatsrust-mcp-smoke-nginx-$$"
CADDY_NAME="whatsrust-mcp-smoke-caddy-$$"
NGINX_IMAGE="nginx:alpine"
CADDY_IMAGE="caddy:2-alpine"

for tool in docker openssl curl; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing prerequisite: $tool" >&2; exit 2; }
done
docker info >/dev/null 2>&1 || { echo "the docker daemon is not running" >&2; exit 2; }

# Everything below lives in one private temp dir, removed (with both containers) on any exit.
umask 077
TMP="$(mktemp -d)"
cleanup() {
  docker rm -f "$NGINX_NAME" "$CADDY_NAME" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# The token goes to curl through this mode-600 header file, never on a command line.
AUTH_HEADER_FILE="$TMP/auth.hdr"
printf 'Authorization: Bearer %s\n' "$WHATSRUST_MCP_TOKEN" >"$AUTH_HEADER_FILE"
chmod 600 "$AUTH_HEADER_FILE"

# --- the listener must be there, on the host's loopback ---
direct="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST "http://127.0.0.1:$MCP_UPSTREAM_PORT/mcp" || true)"
if [ "$direct" = "000" ]; then
  echo "nothing is listening on 127.0.0.1:$MCP_UPSTREAM_PORT (set MCP_UPSTREAM_PORT, or start the daemon with WHATSRUST_MCP_PORT)" >&2
  exit 2
fi

# --- certificate and container configs ---
mkdir "$TMP/certs"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=localhost" \
  -keyout "$TMP/certs/key.pem" -out "$TMP/certs/cert.pem" >/dev/null 2>&1

# The user-facing configs, with exactly these rewrites for the containers: the upstream (Host stays
# 127.0.0.1), the certificate paths, the domain.
render() {
  sed -e "s|127\\.0\\.0\\.1:7271|host.docker.internal:${MCP_UPSTREAM_PORT}|g" \
      -e 's|/path/to/cert\.pem|/certs/cert.pem|g' \
      -e 's|/path/to/key\.pem|/certs/key.pem|g' \
      -e 's|mcp\.example\.com|localhost|g' \
      "$1" >"$2"
  grep -q 'Host 127\.0\.0\.1' "$2" || { echo "$1: the Host rewrite to 127.0.0.1 is missing" >&2; exit 2; }
}
render "$ROOT/deploy/proxy/nginx-mcp.conf" "$TMP/nginx.conf"
render "$ROOT/deploy/proxy/Caddyfile" "$TMP/Caddyfile"

docker run --rm -d --name "$NGINX_NAME" --add-host=host.docker.internal:host-gateway \
  -p "127.0.0.1:$NGINX_PORT:443" \
  -v "$TMP/nginx.conf:/etc/nginx/conf.d/default.conf:ro" -v "$TMP/certs:/certs:ro" \
  "$NGINX_IMAGE" >/dev/null
docker run --rm -d --name "$CADDY_NAME" --add-host=host.docker.internal:host-gateway \
  -p "127.0.0.1:$CADDY_PORT:443" \
  -v "$TMP/Caddyfile:/etc/caddy/Caddyfile:ro" -v "$TMP/certs:/certs:ro" \
  "$CADDY_IMAGE" >/dev/null

wait_for_proxy() { # NAME URL
  for _ in $(seq 1 60); do
    if curl -ks -o /dev/null --max-time 2 "$2"; then
      return 0
    fi
    sleep 0.5
  done
  echo "$1 did not start answering on $2; its log:" >&2
  docker logs "$1" >&2 || true
  exit 1
}
wait_for_proxy "$NGINX_NAME" "https://localhost:$NGINX_PORT/"
wait_for_proxy "$CADDY_NAME" "https://localhost:$CADDY_PORT/"

# --- requests and verdicts ---
BODY_FILE="$TMP/body"
STATUS=""
PASSED=0
FAILED=0
UNREACHABLE=0

# send URL [CURL_ARG...]: sets STATUS (the HTTP status, 000 when there was no response) and BODY_FILE.
# A nonzero curl exit is fine when a status came back: a proxy may answer 413 and close mid-upload.
send() {
  local url=$1
  shift
  STATUS="$(curl -ksS -o "$BODY_FILE" -w '%{http_code}' --max-time 60 "$@" "$url" 2>"$TMP/curl.err" || true)"
  case "$STATUS" in 000 | 502 | 504) UNREACHABLE=1 ;; esac
}

# rpc URL JSON [HEADER...]: POST JSON with the token and the given extra headers.
rpc() {
  local url=$1 body=$2 args=() header
  shift 2
  for header in "$@"; do
    args+=(-H "$header")
  done
  send "$url" -X POST -H 'Content-Type: application/json' -H "@$AUTH_HEADER_FILE" ${args[@]+"${args[@]}"} --data-binary "$body"
}

# verdict LABEL WHY: PASS when WHY is empty, else FAIL with WHY.
verdict() {
  if [ -z "$2" ]; then
    echo "PASS  $1"
    PASSED=$((PASSED + 1))
  else
    echo "FAIL  $1 ($2)"
    FAILED=$((FAILED + 1))
  fi
}

# expect LABEL WANTED_STATUS [BODY_MUST_CONTAIN [BODY_MUST_NOT_CONTAIN]]: judge the last response.
expect() {
  local why=""
  if [ "$STATUS" = 000 ]; then
    why="no response: $(tr '\n' ' ' <"$TMP/curl.err")"
  elif [ "$STATUS" != "$2" ]; then
    why="status $STATUS, wanted $2"
  elif [ -n "${3:-}" ] && ! grep -q -- "$3" "$BODY_FILE"; then
    why="body lacks $3"
  elif [ -n "${4:-}" ] && grep -q -- "$4" "$BODY_FILE"; then
    why="body has $4"
  fi
  verdict "$1" "$why"
}

meta='"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}'

check_proxy() { # NAME PORT
  local name=$1 base="https://localhost:$2" count why
  echo "--- $name ($base) ---"

  rpc "$base/mcp" '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"mcp-proxy-smoke","version":"0"}}}'
  expect "$name: legacy initialize -> 200 with a protocolVersion" 200 '"protocolVersion"'

  rpc "$base/mcp" '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' 'MCP-Protocol-Version: 2025-11-25'
  count="$(grep -o '"name":"whatsrust_' "$BODY_FILE" | wc -l | tr -d ' ' || true)"
  why=""
  if [ "$STATUS" != 200 ]; then why="status $STATUS, wanted 200"; elif [ "$count" != 34 ]; then why="$count tools, wanted 34"; fi
  verdict "$name: tools/list -> 34 tools" "$why"

  rpc "$base/mcp" '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"whatsrust_status","arguments":{}}}' 'MCP-Protocol-Version: 2025-11-25'
  expect "$name: tools/call whatsrust_status -> result without isError" 200 '"result"' 'isError'

  rpc "$base/mcp" '{"jsonrpc":"2.0","id":4,"method":"server/discover","params":{'"$meta"'}}' \
    'MCP-Protocol-Version: 2026-07-28' 'Mcp-Method: server/discover'
  expect "$name: modern server/discover with Mcp-* headers -> 200" 200 '"supportedVersions"'

  rpc "$base/api/status" '{}'
  expect "$name: POST /api/status -> 404 from the proxy" 404 "" '"code":"not_found"'

  # 10 MiB is 10485760 bytes: 10400000 is under it (and over 10 MB, so a limit of 10^7 would refuse it).
  head -c 10400000 /dev/zero | tr '\0' 'a' >"$TMP/big.body"
  send "$base/mcp" -X POST -H 'Content-Type: application/json' -H "@$AUTH_HEADER_FILE" --data-binary "@$TMP/big.body"
  expect "$name: body of 10.4 MB, under 10 MiB -> reaches the daemon (400 parse error)" 400 '"code":-32700'

  # Over the limit: nginx answers 413 itself; Caddy forwards the head and the listener, which has the
  # same limit, may answer first. Either way the client gets 413 and the body is not processed.
  head -c $((10 * 1024 * 1024 + 1024)) /dev/zero | tr '\0' 'a' >"$TMP/big.body"
  send "$base/mcp" -X POST -H 'Content-Type: application/json' -H "@$AUTH_HEADER_FILE" --data-binary "@$TMP/big.body"
  rm -f "$TMP/big.body"
  expect "$name: body over 10 MiB -> 413" 413

  send "$base/mcp" -X POST -H 'Content-Type: application/json' --data-binary '{"jsonrpc":"2.0","id":5,"method":"ping"}'
  expect "$name: no token -> 401 from the daemon" 401 '"code":"unauthorized"'
}

check_proxy "nginx" "$NGINX_PORT"
check_proxy "caddy" "$CADDY_PORT"

echo
echo "$PASSED passed, $FAILED failed"
if [ "$UNREACHABLE" = 1 ]; then
  echo "hint: a 000, 502 or 504 means a proxy could not reach the listener at host.docker.internal:$MCP_UPSTREAM_PORT." >&2
  echo "      Docker Desktop forwards that name to the host's loopback; on Linux it is the bridge gateway," >&2
  echo "      which cannot reach a loopback-bound service (run the proxy on the host instead, as the HOWTO does)." >&2
fi
[ "$FAILED" -eq 0 ]
