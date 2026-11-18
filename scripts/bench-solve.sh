#!/usr/bin/env bash
# 让真实内核解一道 bench 题：引擎 ← harnessd ← orbit 方言。
# bench/run.py 在任务目录里以 cwd 调用，题目在 $TASK_PROMPT。
#
# 环境：HARNESSD_BIN、HARNESS_ENGINE_BIN(codewhale)、CODEWHALE_HOME、HARNESS_PROVIDER(默认 ollama)
set -uo pipefail
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
PROVIDER="${HARNESS_PROVIDER:-ollama}"
WAIT="${HARNESS_TURN_WAIT:-200}"

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
export CODEWHALE_HOME
"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" >/tmp/bench-engine.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT

for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && break
  sleep 0.5
done
curl -sf -m 5 -X POST "http://127.0.0.1:$PORT/v1/providers/$PROVIDER/switch" \
  -H 'content-type: application/json' -d '{}' >/dev/null || true

TID=$(curl -sf -m 10 -X POST "http://127.0.0.1:$PORT/v1/threads" -H 'content-type: application/json' \
  -d '{"auto_approve":true}' | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))' 2>/dev/null)
[ -z "$TID" ] && exit 1

mkdir -p .harness
JSON=$(python3 - "$TASK_PROMPT" <<'PY'
import json, sys
print(json.dumps({"id": "1", "type": "prompt", "message": sys.argv[1]}))
PY
)

# harnessd 会一直泵到引擎的终态事件（turn_end / turn.failed）才返回，
# 所以这里不需要自己判定结束。
printf '%s\n' "$JSON" | HARNESSD_THREAD_BODY='{"auto_approve":true}' \
  "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
    --log ./.harness/timebus.ndjson >/tmp/bench-harnessd.log 2>&1
exit 0
