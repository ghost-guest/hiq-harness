#!/usr/bin/env bash
# 状态表第 5 格：**回合进行中切会话**。
#
# 这一格没有"显然正确"的答案：切走之后旧回合该继续跑、该被打断、还是不该允许切？
# 先不拍板，**先量**：切走再切回来，旧会话还能不能发消息？会不会 409 / 卡死？
# 量出来的结果写进状态表，再决定行为。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME
[ "${SKIP_BUILD:-0}" = "1" ] || cargo build -q -p harnessd || { echo "FAIL 编译 harnessd 失败"; exit 1; }

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/midturn-ws.XXXXXX)
LOG=/tmp/midturn-harnessd.log
: > "$LOG"
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" >/tmp/midturn-engine.log 2>&1 &
for _ in $(seq 1 40); do curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.5; done
curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null || { echo "FAIL 引擎没起来"; exit 1; }

FIFO="$WS/in.fifo"; mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":true}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > "$LOG" 2>&1 ) &
HP=$!
exec 9>"$FIFO"
send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }
ends() { local n; n=$(grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null); echo "${n:-0}"; }

# A 上起一个**慢**回合
send '{"id":"p1","type":"prompt","message":"先跑 bash sleep 25，然后一句话总结"}'
sleep 3
A=$(python3 - "$WS/timebus.ndjson" <<'PY'
import json,sys
for l in open(sys.argv[1],encoding='utf-8'):
    try: d=json.loads(l)
    except: continue
    if d.get('kind')=='turn.start':
        t=(d.get('payload') or {}).get('thread')
        if t: print(t); break
PY
)
[ -n "$A" ] || { echo "FAIL 没拿到 A 的 id"; exit 1; }

# 回合还在跑时切走，再切回来
send '{"id":"n1","type":"new_session"}'
sleep 2
send "{\"id\":\"s1\",\"type\":\"switch_session\",\"sessionId\":\"$A\"}"
sleep 2
# 等原回合跑完（它不该被切会话弄死）
n=0; while [ "$n" -lt 240 ]; do [ "$(ends)" -ge 1 ] && break; sleep 0.5; n=$((n+1)); done
# 回合结束后再切一次：这次应该成功
send "{\"id\":\"s2\",\"type\":\"switch_session\",\"sessionId\":\"$A\"}"
sleep 2
# 再发一句，确认会话没被弄坏
send "{\"id\":\"p3\",\"type\":\"prompt\",\"message\":\"只回复 OK 两个字母\"}"
sleep 25
exec 9>&-
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

python3 - "$LOG" <<'PY'
import json,sys
resp={}
ends=0
for line in open(sys.argv[1],encoding='utf-8'):
    try: d=json.loads(line)
    except: continue
    if d.get('id') in ('p1','p3','n1','s1','s2'):
        resp[d['id']] = (d.get('success'), (d.get('error') or '')[:120])
    if d.get('kind')=='turn.end': ends+=1
print("回执:", {k:(v[0], v[1][:60]) for k,v in resp.items()})
problems=[]
s1 = resp.get('s1')
if s1 is None:
    problems.append("回合中的切会话没有回执")
elif s1[0] is not False:
    problems.append("回合进行中切会话被接受了——这会把旧回合的事件源扔掉，会话半死")
elif 'still running' not in s1[1]:
    problems.append(f"拒绝的理由不明确：{s1[1]}")
if ends < 1:
    problems.append("原回合没有闭合——被切会话弄死了")
s2 = resp.get('s2')
if s2 is None or s2[0] is not True:
    problems.append("回合结束后再切会话仍然失败")
p3 = resp.get('p3')
if p3 is None or p3[0] is not True:
    problems.append("切回来之后发消息失败，会话被弄坏了")
if problems:
    print("FAIL " + "；".join(problems)); sys.exit(1)
print(f"PASS 回合中切会话被明确拒绝（{s1[1][:40]}），原回合正常闭合（{ends} 个），之后照常可用")
