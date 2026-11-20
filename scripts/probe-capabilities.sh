#!/usr/bin/env bash
# 界面上开着就会调 `get_commands`。以前它必定失败：
#   引擎的能力清单列的是**引擎自己**的方法（含 `prompt/capabilities`），
#   我们把它整个当成"可路由"，转发过去就得到
#   "'prompt/capabilities' is not mapped on the HTTP transport"，
#   界面上是一整条红：`Get commands 失败`。
#
# 断言：get_commands 必须 success，且不带 error。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/caps-ws.XXXXXX)
LOG=/tmp/caps-harnessd.log
: > "$LOG"
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/caps-engine.log 2>&1 &
up=0
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && { up=1; break; }
  sleep 0.5
done
[ "$up" = 1 ] || { echo "FAIL 引擎没起来"; exit 1; }

FIFO="$WS/in.fifo"
mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":true}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > "$LOG" 2>&1 ) &
HP=$!
exec 9>"$FIFO"

send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }
send '{"id":"c1","type":"get_commands"}'
sleep 3
exec 9>&-
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

python3 - "$LOG" <<'PY'
import json,sys
last=None
for line in open(sys.argv[1], encoding='utf-8'):
    try: d=json.loads(line)
    except: continue
    if d.get('id')=='c1':
        last=d
if last is None:
    print("FAIL get_commands 没有任何回执"); sys.exit(1)
err=last.get('error')
if not last.get('success') or err:
    print(f"FAIL get_commands 失败：{err}"); sys.exit(1)
print("PASS get_commands 正常返回（能力清单已按传输层能力收敛）")
PY
