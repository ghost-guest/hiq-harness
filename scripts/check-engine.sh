#!/usr/bin/env bash
# Fusion depth D4 is forbidden: if the engine is vendored, it must be byte-identical
# to the pinned commit. Any local edit is how a "fusion" turns into a fork nobody
# can upgrade. See docs/KERNEL-FUSION-PLAN.md §1.
set -euo pipefail
DIR="${1:-vendor/codewhale}"
PIN_FILE="vendor/ENGINE_PIN"

if [ ! -d "$DIR/.git" ]; then
  echo "engine not vendored — consumed as a process over JSON-RPC; nothing to check"
  exit 0
fi
[ -f "$PIN_FILE" ] || { echo "missing $PIN_FILE"; exit 1; }

want=$(tr -d '[:space:]' < "$PIN_FILE")
have=$(git -C "$DIR" rev-parse HEAD)

if [ "$want" != "$have" ]; then
  echo "FAIL engine pin mismatch: want $want, have $have"
  echo "     bump vendor/ENGINE_PIN deliberately, in its own commit"
  exit 1
fi

if [ -n "$(git -C "$DIR" status --porcelain)" ]; then
  echo "FAIL engine has local modifications (depth D4):"
  git -C "$DIR" status --short
  exit 1
fi

echo "ok engine unmodified at $have"
