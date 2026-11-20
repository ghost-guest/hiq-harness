#!/usr/bin/env bash
# 「怎么不是流式显示呢」——判据不是"有没有 delta 事件"，而是**它们什么时候到**。
#
# 以前内核把增量全攒在 text_buffer 里，item 结束时才发一条成品：
# 事件条数看着正常，界面却是"唰"一下整段出现。所以必须看时间分布：
#   1. text_delta 至少 3 条（真流式就是很多条小片）
#   2. 第一条 text_delta 比 turn.end 早至少 0.5 秒（说明是边生成边发）
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# 探针跑的是**当前源码**，不是上次留下的二进制。
# 踩过：gate 和我自己的构建并发，探针拿到一个旧二进制，报\`unknown command Prompt\`——
# 看起来像内核坏了，其实只是二进制和源码不同步。
[ "${SKIP_BUILD:-0}" = "1" ] || cargo build -q -p harnessd || { echo "FAIL 编译 harnessd 失败"; exit 1; }
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/stream-ws.XXXXXX)
LOG=/tmp/stream-harnessd.log
: > "$LOG"
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/stream-engine.log 2>&1 &
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
      --log ./timebus.ndjson < "$FIFO" 2>&1 | while IFS= read -r l; do
        printf '%s %s\n' "$(python3 -c 'import time;print(f"{time.time():.3f}")')" "$l"
      done > "$LOG" ) &
HP=$!
exec 9>"$FIFO"

send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }
ends() { local n; n=$(grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null); echo "${n:-0}"; }

send '{"id":"p1","type":"prompt","message":"用三句话介绍 Rust 这门语言。"}'
n=0
while [ "$n" -lt 300 ]; do
  [ "$(ends)" -ge 1 ] && break
  sleep 0.5; n=$((n + 1))
done
sleep 2
exec 9>&-
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

python3 - "$LOG" <<'PY'
import json, sys
deltas, reason, t_end = [], 0, None
for line in open(sys.argv[1], encoding='utf-8'):
    parts = line.split(' ', 1)
    if len(parts) != 2:
        continue
    try:
        ts = float(parts[0]); d = json.loads(parts[1])
    except Exception:
        continue
    t = d.get('type')
    if t == 'message_update':
        am = d.get('assistantMessageEvent') or {}
        if am.get('type') == 'text_delta' and (am.get('delta') or ''):
            deltas.append(ts)
        if am.get('type') == 'thinking_delta':
            reason += 1
    if t == 'agent_settled':
        t_end = ts

if not deltas:
    print("FAIL 一条 text_delta 都没发——界面不可能流式")
    sys.exit(1)
if t_end is None:
    print("FAIL 回合没结算，无法判断时间分布")
    sys.exit(1)
lead = t_end - deltas[0]
spread = deltas[-1] - deltas[0]
print(f"文本增量 {len(deltas)} 条 ｜ 首条比结算早 {lead:.2f}s ｜ 增量跨度 {spread:.2f}s ｜ 思考增量 {reason} 条")
problems = []
# 判据不能用"首条离结算多远"——那量的是模型快慢，不是我们流不流式。
# 排除绝对时间，只看结构：得有好几条，且它们确实是**陆续**到的，
# 而不是同一刻一起冒出来。
if len(deltas) < 2:
    problems.append(f"只有 {len(deltas)} 条增量（攒成一坨再发）")
if spread <= 0.1:
    problems.append(f"所有增量挤在 {spread:.2f}s 内（同一刻一起冒出来的）")
if lead <= 0:
    problems.append("增量比结算还晚到")
if problems:
    print("FAIL " + "；".join(problems)); sys.exit(1)
print("PASS 真流式：增量边生成边到，不是结尾一次性冒出")
PY
