#!/usr/bin/env bash
# 真引擎上的「切会话」验证。
#
# 为什么单独一条：HTTP 传输里 `thread/resume` 一直是靠"能力集空 = 全支持"
# 蒙过去的——状态先被赋值、调用随后报 not mapped，看着能用。能力集收敛后
# 立刻露馅：切回原会话失败，GetEntries 继续读上一个线程，
# 于是"别的会话的消息跑进了这个会话的历史"。
#
# 断言：
#   1. 在 A 上跑一回合（留标记 MA）
#   2. 新开一个会话 B，跑一回合（留标记 MB）
#   3. SwitchSession 回 A → GetEntries 里有 MA，**没有** MB
#   4. 再切到 B → 能读到 MB
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

[ "${SKIP_BUILD:-0}" = "1" ] || cargo build -q -p harnessd || { echo "FAIL 编译 harnessd 失败"; exit 1; }

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/switch-ws.XXXXXX)
LOG=/tmp/switch-harnessd.log
: > "$LOG"
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/switch-engine.log 2>&1 &
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
wait_end() { local want=$1 n=0; while [ "$n" -lt 600 ]; do [ "$(ends)" -ge "$want" ] && return 0; sleep 0.5; n=$((n+1)); done; return 1; }

MA="SESS_A_$RANDOM"; MB="SESS_B_$RANDOM"

# 会话 A
send "{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"只回复这个词，不要解释：$MA\"}"
wait_end 1 || { echo "FAIL 会话 A 第一回合没闭合"; exit 1; }
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
[ -n "$A" ] || { echo "FAIL 没拿到会话 A 的 id"; exit 1; }

# 新会话 B
send '{"id":"n1","type":"new_session"}'
sleep 2
send "{\"id\":\"p2\",\"type\":\"prompt\",\"message\":\"只回复这个词，不要解释：$MB\"}"
wait_end 2 || { echo "FAIL 会话 B 第一回合没闭合"; exit 1; }
B=$(python3 - "$WS/timebus.ndjson" <<'PY2'
import json,sys
last=None
for l in open(sys.argv[1],encoding='utf-8'):
    try: d=json.loads(l)
    except: continue
    if d.get('kind')=='turn.start':
        t=(d.get('payload') or {}).get('thread')
        if t: last=t
print(last or "")
PY2
)
[ -n "$B" ] || { echo "FAIL 没拿到会话 B 的 id"; exit 1; }

# 切回 A，读历史
send "{\"id\":\"s1\",\"type\":\"switch_session\",\"sessionId\":\"$A\"}"
sleep 2
send '{"id":"e1","type":"get_entries"}'
sleep 3
# 切到 B，读历史
send "{\"id\":\"s2\",\"type\":\"switch_session\",\"sessionId\":\"$B\"}"
sleep 2
send '{"id":"e2","type":"get_entries"}'
sleep 3
exec 9>&-
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

python3 - "$LOG" "$MA" "$MB" <<'PY'
import json,sys
log, ma, mb = sys.argv[1], sys.argv[2], sys.argv[3]
entries = []
for line in open(log, encoding='utf-8'):
    try: d = json.loads(line)
    except: continue
    if d.get('id') in ('e1', 'e2'):
        entries.append((d.get('id'), json.dumps(d.get('data') or {}, ensure_ascii=False)))
a_text = " ".join(t for i, t in entries if i == 'e1')
b_text = " ".join(t for i, t in entries if i == 'e2')
text = a_text
if not text:
    print("FAIL 切会话后 GetEntries 没有任何回执"); sys.exit(1)
if ma not in text:
    print("FAIL 切回原会话后读不到它自己的消息 MA"); sys.exit(1)
if mb in text:
    print("FAIL 另一个会话的消息 MB 出现在了这个会话的历史里")
    sys.exit(1)
if not b_text:
    print("FAIL 切到会话 B 后 GetEntries 没有回执"); sys.exit(1)
if mb not in b_text:
    print("FAIL 切到会话 B 后读不到它自己的消息 MB"); sys.exit(1)
print("PASS 切会话：A 只看到 A 的消息，B 只看到 B 的消息")
PY
