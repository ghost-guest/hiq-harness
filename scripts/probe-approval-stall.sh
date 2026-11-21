#!/usr/bin/env bash
# 回归：**回合被接受之后不许无声无息地卡住**。
#
# 真实故障：界面线程默认 auto_approve=false，模型一调工具，引擎发
# `approval.required` 然后一直等；内核既不呈现、也不拒绝、也不报错，
# 回合永远 in_progress，界面只显示"已工作 0s"，后续消息排队。
#
# 断言（auto_approve=false 下跑一个必然用工具的回合）：
#   30 秒内必须出现下面之一，否则判红：
#     - 回合闭合（turn.end）
#     - 内核把审批抛给了上层（日志里有 request 帧 / approval 字样）
#     - 内核明确报错
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME
[ "${SKIP_BUILD:-0}" = "1" ] || cargo build -q -p harnessd || { echo "FAIL 编译 harnessd 失败"; exit 1; }

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/apprstall-ws.XXXXXX)
LOG=/tmp/apprstall-harnessd.log
: > "$LOG"
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" >/tmp/apprstall-engine.log 2>&1 &
for _ in $(seq 1 40); do curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.5; done

FIFO="$WS/in.fifo"; mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":false}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > "$LOG" 2>&1 ) &
HP=$!
exec 9>"$FIFO"
send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }

send '{"id":"p1","type":"prompt","message":"用 bash 执行 echo stall-check，然后把输出贴出来"}'

n=0; closed=0
while [ "$n" -lt 60 ]; do
  if grep -q '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null; then closed=1; break; fi
  if grep -qi 'approval' "$LOG" 2>/dev/null; then break; fi
  sleep 0.5; n=$((n+1))
done
sleep 1
exec 9>&-; kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true; HP=

msgs=$(wc -l < "$LOG" 2>/dev/null | tr -d ' ')
approval=$(grep -ci 'approval' "$LOG" 2>/dev/null || echo 0)
calls=$(grep -c '"kind":"tool.call"' "$WS/timebus.ndjson" 2>/dev/null || echo 0)
echo "回合闭合=${closed} ｜ 审批相关行=${approval} ｜ 工具调用=${calls} ｜ 内核输出行=${msgs}"

if [ "$closed" = "1" ]; then
  echo "PASS 审批路径没有卡死：回合在 30 秒内闭合（工具 ${calls} 次）"
elif [ "$approval" -gt 0 ]; then
  echo "PASS 审批被抛给了上层（没有静默卡死）"
else
  echo "FAIL 回合被接受后既没闭合、也没有把审批抛出来——这就是「一直没反应」"
  exit 1
fi
