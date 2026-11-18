#!/usr/bin/env bash
# Probe a real codewhale app-server before trusting the mapping.
#
#   ./scripts/probe-engine.sh /path/to/codewhale
#
# Prints the first frames it answers, so the shapes in crates/harnessd/src/mapping.rs
# can be checked against reality instead of against our memory of it.
set -euo pipefail
BIN="${1:-}"
[ -n "$BIN" ] || { echo "usage: $0 <codewhale-binary>"; exit 2; }
[ -x "$BIN" ] || { echo "not executable: $BIN"; exit 2; }

echo "== help =="
"$BIN" --help 2>&1 | sed -n '1,25p' || true

echo
echo "== frames =="
{
  printf '%s\n' '{"id":1,"method":"initialize","params":{"clientInfo":{"name":"probe","version":"0"}}}'
  printf '%s\n' '{"id":2,"method":"model/list","params":{}}'
  printf '%s\n' '{"id":3,"method":"thread/start","params":{"input":[]}}'
} | perl -e 'alarm 20; exec @ARGV' "$BIN" app-server 2>&1 | sed -n '1,12p' || \
  echo "(no `app-server` subcommand — check --help above)"
