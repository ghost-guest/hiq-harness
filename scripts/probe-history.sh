#!/usr/bin/env bash
# GetEntries：跑完一个回合后，界面能不能读回历史。
#
# 这是 S2 的验收条件之一，但验收矩阵里一直没有。
# 真引擎实测：引擎把历史放在 `items`，orbit 的 GetEntries 读 `entries`。
# 不翻译的话，引擎明明返回了完整历史（含我们的标记），界面却显示空——
# 又一个"数据在、投影错"。
#
# 断言：
#   1. GetEntries 返回里有 `entries` 数组，且非空
#   2. 历史里能找到我们发的标记文本
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/history-ws.XXXXXX)
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/history-engine.log 2>&1 &
up=0
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && { up=1; break; }
  sleep 0.5
done
[ "$up" = 1 ] || { echo "FAIL 引擎没起来"; exit 1; }

MARK="HISTORY_MARK_$RANDOM"
FIFO="$WS/in.fifo"
mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":true}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > /tmp/history-harnessd.log 2>&1 ) &
HP=$!
exec 9>"$FIFO"
send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }
turn_ends() { grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null || true; }

send "{\"id\":\"h1\",\"type\":\"prompt\",\"message\":\"只回复收到：$MARK\"}"
e=0
for _ in $(seq 1 240); do
  x=$(turn_ends); x=${x:-0}
  [ "$x" -ge 1 ] && { e=1; break; }
  sleep 0.5
done
[ "$e" -eq 1 ] || { echo "FAIL 前置回合没闭合"; exit 1; }

send '{"id":"h2","type":"get_entries"}'
r=0
for _ in $(seq 1 40); do
  grep -q '"id":"h2"' /tmp/history-harnessd.log 2>/dev/null && { r=1; break; }
  sleep 0.25
done
exec 9>&-
rm -f "$FIFO"
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=
[ "$r" -eq 1 ] || { echo "FAIL GetEntries 没返回"; exit 1; }

# 提取 h2 的 data
result=$(python3 - "$MARK" <<'PY'
import json, sys
mark = sys.argv[1]
data = None
for line in open('/tmp/history-harnessd.log'):
    if not line.startswith('{'): continue
    try: d = json.loads(line)
    except: continue
    if d.get('type') == 'response' and d.get('id') == 'h2':
        data = d.get('data') or {}
if data is None:
    print("FAIL 找不到 h2 的响应数据")
    raise SystemExit(0)
entries = data.get('entries')
if not isinstance(entries, list):
    print(f"FAIL GetEntries 没有 entries 数组（有 {list(data)}）")
    raise SystemExit(0)
if not entries:
    print("FAIL entries 是空的——引擎有历史，界面却看不到")
    raise SystemExit(0)
payload = json.dumps(data, ensure_ascii=False)
if mark not in payload:
    print("FAIL 历史里找不到我们发的标记——读回的不是这一轮")
    raise SystemExit(0)
print(f"PASS GetEntries：entries 非空（{len(entries)} 条），历史里找到标记 {mark}")
PY
)
echo "$result"
printf '%s' "$result" | grep -q '^FAIL' && exit 1
echo "PASS 会话历史：GetEntries 能在真引擎上读回完整历史"
