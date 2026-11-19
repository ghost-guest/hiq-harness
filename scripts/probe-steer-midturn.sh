#!/usr/bin/env bash
# S3 硬指标：流式中途插话生效——走完整的 orbit compat 层。
#
# 之前 probe-steer.sh 只验了"引擎 API 收得到 + body 翻译对"，
# 并如实标了 LIMIT：compat 层单线程，回合中命令进不来。
# 这一轮把 stdin 读入拆成独立线程 → channel，drain 期间 try_recv
# 把 steer 注入 SteerGate，由 pump 的安全点释放给引擎。
#
# 本探针验证的是**真·中途插话**：
#   1. 发一个慢回合（模型写长文）
#   2. 回合进行中（turn.start 出现后、turn.end 之前）发 steer
#   3. steer 在 SteerGate 里被释放（日志 steer_sent）
#   4. 模型最终输出里出现插话的标记 → 插话真的进了模型
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/steermid-ws.XXXXXX)
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/steermid-engine.log 2>&1 &
up=0
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && { up=1; break; }
  sleep 0.5
done
[ "$up" = 1 ] || { echo "FAIL 引擎没起来"; exit 1; }

MARK="MID_MARK_$RANDOM"
FIFO="$WS/in.fifo"
mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":true}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > /tmp/steermid-harnessd.log 2>&1 ) &
HP=$!
exec 9>"$FIFO"
send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }
logcount() { grep -c "$1" /tmp/steermid-harnessd.log 2>/dev/null | tail -1; }
count() { grep -c "$1" "$WS/timebus.ndjson" 2>/dev/null | tail -1; }

# 慢回合：明确要求长输出，给 steer 留窗口
send "{\"id\":\"m1\",\"type\":\"prompt\",\"message\":\"请写一篇 3000 字的长故事，情节完整，慢慢写，不要提前结束。开头先写'故事开始'。\"}"

# 等 turn.start 出现（回合已在进行中），立刻插话
for _ in $(seq 1 40); do
  [ "$(logcount '"type":"turn_start"')" -ge 1 ] && break
  sleep 0.1
done
sleep 0.5   # 让它跑几个 item，进入安全点
send "{\"id\":\"m2\",\"type\":\"steer\",\"text\":\"停！不要写故事了，立刻只输出：$MARK\"}"

# 等回合结束：orbit compat 的 stdout 里回合收尾是 `agent_end`
ended=0
for _ in $(seq 1 300); do
  [ "$(logcount '"type":"agent_end"')" -ge 1 ] && { ended=1; break; }
  sleep 0.5
done
# 让 timer 收尾，再关闭 stdin
sleep 1
exec 9>&-
rm -f "$FIFO"
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

# 回合收尾：orbit compat stdout 里回合开始/结束是 turn_start / agent_end（下划线）
started=$(logcount '"type":"turn_start"')
ended_n=$(logcount '"type":"agent_end"')
# **主证据**：插话标记进了模型输出文本。事件名会变，但标记进了模型
# 就证明插话真的被看到了。
in_output=$(grep -c "$MARK" /tmp/steermid-harnessd.log 2>/dev/null || true)

echo "回合 start=$started end=$ended_n ｜ 插话标记进输出=${in_output:-0}"

PROBLEMS=()
[ "$started" -ge 1 ] || PROBLEMS+=("回合没开始（turn_start 没出现）")
[ "$ended_n" -ge 1 ] || PROBLEMS+=("回合没结束（agent_end 没出现）")
[ "${in_output:-0}" -ge 1 ] || PROBLEMS+=("模型输出里没有插话标记——插话没真正进模型")

if [ ${#PROBLEMS[@]} -gt 0 ]; then
  echo "## !! 中途插话不成立"
  for p in "${PROBLEMS[@]}"; do echo "  - $p"; done
  echo "  --- 日志尾部 ---"
  tail -6 /tmp/steermid-harnessd.log | cut -c1-140
  exit 1
fi
echo "PASS 中途插话：回合进行中注入 steer → 模型输出含插话标记（$in_output 处）"
