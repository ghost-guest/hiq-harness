#!/usr/bin/env bash
# 用户实测：第一回合（带工具）答完，紧接着追问一句 —— 界面一直转圈，没有输出。
#
# 单独一条的原因：probe-multiturn 的回合都是纯文本，probe-tool-followup 只跑一回合，
# 谁都没测"用过工具的回合之后还能不能再问"。
#
# 断言：
#   1. 第二回合真的闭合（turn.end 到 2）
#   2. 第二回合确实回答了（日志里出现第二条 assistant message）
#   3. 第二回合没有再跑工具也能答（问题本身不需要工具）
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/second-ws.XXXXXX)
LOG=/tmp/second-harnessd.log
: > "$LOG"
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

printf 'fn main(){}\n' > "$WS/main.rs"
printf '# demo\n' > "$WS/README.md"

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/second-engine.log 2>&1 &
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
ends() { local n; n=$(grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null); echo "${n:-0}"; }
wait_end() {
  local want=$1 n=0
  while [ "$n" -lt 300 ]; do
    [ "$(ends)" -ge "$want" ] && return 0
    sleep 0.5; n=$((n + 1))
  done
  return 1
}

# 回合 1：必须调工具
send '{"id":"p1","type":"prompt","message":"用 bash 执行 uname -s，然后一句话说明这是什么系统。"}'
wait_end 1 || { echo "FAIL 第一回合没闭合"; exit 1; }

# 回合 2：同一线程追问，不需要工具
send '{"id":"p2","type":"prompt","message":"刚才那条命令的输出是什么？原样复述一遍。"}'
if ! wait_end 2; then
  echo "FAIL 第一回合答完后，第二回合没有闭合（界面会一直转圈）"
  exec 9>&-; kill "$HP" 2>/dev/null; exit 1
fi

exec 9>&-
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

python3 - "$WS/timebus.ndjson" "$LOG" <<'PY'
import json,sys
kinds=[]
try:
    for line in open(sys.argv[1], encoding='utf-8'):
        line=line.strip()
        if not line: continue
        d=json.loads(line)
        kinds.append((d.get('kind'), d.get('payload') or {}))
except FileNotFoundError:
    print("FAIL 没有 timebus 日志"); sys.exit(1)

order=[k for k,_ in kinds]
print(f"时间线：{ ' → '.join(order) }")
msgs=[(p.get('text') or '') for k,p in kinds if k=='message']
ends=[k for k in order if k=='turn.end']
problems=[]
if len(ends) != 2:
    problems.append(f"回合闭合 {len(ends)} 次（应当 2 次）")
if len(msgs) < 2:
    problems.append(f"只有 {len(msgs)} 条 assistant 回答（第二回合没答）")
if problems:
    print("FAIL " + "；".join(problems)); sys.exit(1)
# 界面靠 `agent_settled` 才认为回合真的结束（end_live_run 只挂在它上面）。
# 只发 agent_end 的话，第一回合答完界面仍认为在跑，下一句就永远转圈。
ui=[]
for line in open(sys.argv[2], encoding='utf-8'):
    try: d=json.loads(line)
    except: continue
    t=d.get('type')
    if t in ('agent_start','agent_end','agent_settled','turn_start'):
        ui.append(t)
print(f"界面序列：{ ' '.join(ui) }")
ends_ui=ui.count('agent_end'); settled=ui.count('agent_settled')
if settled != ends_ui:
    print(f"FAIL agent_end {ends_ui} 次但 agent_settled 只有 {settled} 次——界面不会认为回合结束")
    sys.exit(1)
if ui and ui[-1] != 'agent_settled':
    print(f"FAIL 最后一条不是 agent_settled 而是 {ui[-1]}——回合没真正收尾")
    sys.exit(1)
print(f"PASS 两回合都闭合且都有回答 ｜ 第二回合：{msgs[-1][:50]}")
PY
