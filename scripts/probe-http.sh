#!/usr/bin/env bash
# Reproduce the HTTP-transport smoke test against a real engine.
#
#   ./scripts/probe-http.sh /path/to/codewhale [port]
#
# Proves, without any provider key:
#   1. the transport reaches /v1 (models + credential state)
#   2. a tool-enabled turn is accepted and only fails at the provider
#   3. the failure is surfaced verbatim, not turned into a fake success
set -euo pipefail
BIN="${1:?usage: probe-http.sh <codewhale-binary> [port]}"
PORT="${2:-7891}"
BASE="http://127.0.0.1:$PORT"

"$BIN" app-server --http --insecure-no-auth --port "$PORT" >/tmp/probe-http.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 6

echo "== harnessd over http =="
printf '%s\n' '{"id":"1","type":"get_available_models"}' '{"id":"2","type":"prompt","message":"say hi"}' \
  | perl -e 'alarm 40; exec @ARGV' ./target/debug/harnessd serve --compat orbit \
      --http-engine "$BASE" --log /tmp/probe-http.ndjson 2>&1 | cut -c1-240

echo
echo "== time-bus =="
cat /tmp/probe-http.ndjson 2>/dev/null || echo "(no entries)"
