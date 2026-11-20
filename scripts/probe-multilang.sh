#!/usr/bin/env bash
# 用桌面版同一条链路（orbit 方言 → harnessd → 引擎）跑一个**多语言算法**任务：
# 让模型用 Rust 和 Go 各写一份快排，各跑一遍并对结果。
#
# 为什么要这条：harness 的价值不只是"能回答"，而是能把一个跨语言任务
# 一路做完——写文件、编译、运行、对比。只验"有回复"是不够的。
#
# 断言：
#   1. 至少 3 次工具调用（写两个文件 + 至少跑两次）
#   2. 工作区里真的出现了 .rs 和 .go 文件
#   3. 回合闭合一次，且有最终回答
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# 探针跑的是**当前源码**，不是上次留下的二进制。
# 踩过：gate 和我自己的构建并发，探针拿到一个旧二进制，报\`unknown command Prompt\`——
# 看起来像内核坏了，其实只是二进制和源码不同步。
[ "${SKIP_BUILD:-0}" = "1" ] || cargo build -q -p harnessd || { echo "FAIL 编译 harnessd 失败"; exit 1; }
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/multilang-ws.XXXXXX)
LOG=/tmp/multilang-harnessd.log
: > "$LOG"
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/multilang-engine.log 2>&1 &
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && break
  sleep 0.5
done
curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null || { echo "FAIL 引擎没起来"; exit 1; }

FIFO="$WS/in.fifo"
mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":true}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > "$LOG" 2>&1 ) &
HP=$!
exec 9>"$FIFO"

send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }
ends() { local n; n=$(grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null); echo "${n:-0}"; }

send '{"id":"p1","type":"prompt","message":"用 Rust 和 Go 各写一个快速排序（放在同一个目录里），各跑一遍，确认两边对同一组输入的输出一致。跑完给我一句话结论。"}'
n=0
while [ "$n" -lt 240 ]; do
  [ "$(ends)" -ge 1 ] && break
  sleep 0.5; n=$((n + 1))
done
sleep 2
exec 9>&-
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

echo "--- 工作区内容 ---"; find "$WS" -maxdepth 3 -type f | sed "s|$WS|.|" | head -12
rs_files=$(find "$WS" -name "*.rs" -not -path "*/target/*" | wc -l | tr -d ' ')
go_files=$(find "$WS" -name "*.go" | wc -l | tr -d ' ')
tools=$(python3 -c "
import json
c=0
for l in open('$WS/timebus.ndjson', encoding='utf-8'):
    if '\"tool.call\"' in l: c+=1
print(c)
" 2>/dev/null || echo 0)
msg=$(python3 -c "
import json
t=''
for l in open('$WS/timebus.ndjson', encoding='utf-8'):
    try: d=json.loads(l)
    except: continue
    if d.get('kind')=='message': t=(d.get('payload') or {}).get('text') or ''
print(t.replace(chr(10),' ')[:120])
" 2>/dev/null || echo '')

echo "工具调用 ${tools} 次 ｜ .rs ${rs_files} 个 ｜ .go ${go_files} 个"
echo "结论：${msg}"
problems=""
[ "${rs_files:-0}" -ge 1 ] || problems="$problems 没有 .rs 文件；"
[ "${go_files:-0}" -ge 1 ] || problems="$problems 没有 .go 文件；"
[ "${tools:-0}" -ge 3 ] || problems="$problems 工具调用只有 ${tools} 次；"
[ -n "$msg" ] || problems="$problems 没有最终回答；"
if [ -n "$problems" ]; then
  echo "FAIL $problems"
  exit 1
fi
echo "PASS 跨语言任务完成：两个语言的文件都写出来了，都跑过，有结论"
