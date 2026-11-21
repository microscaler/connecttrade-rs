#!/usr/bin/env bash
# One HTTP client: may_minihttp (docs/coding-standards.md, "One HTTP client").
#
#   1. the client crate (connecttrade, default features) depends on may_minihttp, and on no other HTTP client;
#   2. its tungstenite is built without "handshake": tungstenite frames WebSocket messages on the connection that
#      may_minihttp upgraded, and has no HTTP client of its own;
#   3. nothing in the workspace, tests and build scripts included, brings another HTTP client crate, or hyper's client.
#
# The mock is a server (axum, and through it hyper's server and tokio-tungstenite's accept side); servers are not
# checked. Run from anywhere in the repository; needs the lock file's dependencies fetched.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# HTTP client crates. hyper and hyper-util are checked by feature below (axum needs their server side).
CLIENTS='^(ureq|reqwest|reqwest-middleware|isahc|curl|curl-sys|attohttpc|surf|minreq|awc|ehttp|http-req|hyper-tls|hyper-rustls|ureq-proto|reqwless|async-h1|http-client|tokio-websockets|websocket|ws)$'
fail=0

client_tree=$(cargo tree -p connecttrade -e normal --prefix none --no-dedupe -f '{p} {f}' 2>/dev/null)
if ! grep -q '^may_minihttp ' <<<"$client_tree"; then
  echo "connecttrade does not depend on may_minihttp: its HTTP must go through may_minihttp"
  fail=1
fi
if grep '^tungstenite ' <<<"$client_tree" | grep -q 'handshake'; then
  echo "connecttrade's tungstenite has the \"handshake\" feature: that is tungstenite's own HTTP client; open the"
  echo "connection with may_minihttp's HttpClient::upgrade and frame it with WebSocket::from_raw_socket"
  fail=1
fi

workspace_tree=$(cargo tree --workspace -e normal,dev,build --prefix none --no-dedupe -f '{p} {f}' 2>/dev/null)
found=$(awk '{print $1}' <<<"$workspace_tree" | sort -u | grep -E "$CLIENTS" || true)
if [ -n "$found" ]; then
  echo "HTTP client crates other than may_minihttp in the workspace (normal, dev or build dependencies):"
  sed 's/^/  /' <<<"$found"
  for crate in $found; do
    cargo tree --workspace -e normal,dev,build -i "$crate" --depth 4 2>/dev/null | sed 's/^/    /' || true
  done
  fail=1
fi
hyper_client=$(grep -E '^hyper(-util)? ' <<<"$workspace_tree" | grep -E '(^| |,)client(-legacy)?(,|$)' | sort -u || true)
if [ -n "$hyper_client" ]; then
  echo "hyper's client is compiled in (a client feature of hyper or hyper-util):"
  sed 's/^/  /' <<<"$hyper_client"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "use may_minihttp (connecttrade's MayTransport, or may_minihttp::client in tests); see docs/coding-standards.md"
  exit 1
fi
echo "one HTTP client: may_minihttp"
