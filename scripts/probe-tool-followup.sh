#!/usr/bin/env bash
# 工具跑完之后，模型必须**继续**给出最终回答。
#
# 用户实测：问"你的内核是什么？" → 跑了一个命令（输出 47 字符）→ 回合结束，
# 没有下文。也就是说工具结果没回灌给模型，agent 循环在第一跳就断了。
# 现有 S1 探针只验"4 个事件到齐"，验不出这个——事件到齐了，答案没了。
#
# 断言：
#   1. 至少一次工具真的跑了（tool.pre / tool.post）
#   2. **最后一次工具之后**还有 assistant 文本（这才是"收尾"）
#   3. 回合只闭合一次
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# 探针跑的是**当前源码**，不是上次留下的二进制。
# 踩过：gate 和我自己的构建并发，探针拿到一个旧二进制，报\`unknown command Prompt\`——
# 看起来像内核坏了，其实只是二进制和源码不同步。
[ "${SKIP_BUILD:-0}" = "1" ] || cargo build -q -p harnessd || { echo "FAIL 编译 harnessd 失败"; exit 1; }
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/followup-ws.XXXXXX)
LOG=/tmp/followup-harnessd.log
: > "$LOG"
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

# 给模型一点东西可看，别让 ls 输出为空
printf 'fn main(){}\n' > "$WS/main.rs"
printf '# demo\nthis is a tiny rust demo project\n' > "$WS/README.md"

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/followup-engine.log 2>&1 &
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

send '{"id":"p1","type":"prompt","message":"先用 bash 执行 ls -a 看这个目录，然后一句话回答：这个目录里有几个文件？"}'

n=0
while [ "$n" -lt 300 ]; do
  grep -q '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null && break
  sleep 0.5; n=$((n + 1))
done
exec 9>&-
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

python3 - "$WS/timebus.ndjson" <<'PY'
import json,sys
path=sys.argv[1]
kinds=[]
try:
    for line in open(path, encoding='utf-8'):
        line=line.strip()
        if not line: continue
        d=json.loads(line)
        kinds.append((d.get('kind'), d.get('detail') or '', d.get('payload') or {}))
except FileNotFoundError:
    print("FAIL 没有 timebus 日志"); sys.exit(1)

order=[k for k,_,_ in kinds]
tools=[i for i,(k,_,_) in enumerate(kinds) if k=='tool.call']
ends=[i for i,(k,_,_) in enumerate(kinds) if k=='turn.end']

print(f"时间线：{ ' → '.join(order) }")
if not tools:
    print("SKIP 这一轮模型没调工具（模型行为，不是内核缺陷）——换个提示再试，不能算通过")
    sys.exit(2)

# 最后一次工具之后，模型有没有给出成品回答（日志里 message 落盘）
after=[]
for i in range(tools[-1]+1, len(kinds)):
    k, detail, payload = kinds[i]
    if k == 'message':
        after.append((payload.get('text') or detail or ''))
text=''.join(after).strip()

problems=[]
if not text:
    problems.append("最后一次工具之后日志里没有 message——assistant 回答没落盘/没回灌，回合在第一跳就结束了")
if len(ends) != 1:
    problems.append(f"回合闭合了 {len(ends)} 次（应当 1 次）")
if problems:
    print("FAIL " + "；".join(problems))
    sys.exit(1)
print(f"PASS 工具 {len(tools)} 次，之后模型给出了最终回答：{text[:60]}")
PY
