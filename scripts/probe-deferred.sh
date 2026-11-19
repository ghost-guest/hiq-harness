#!/usr/bin/env bash
# 回合进行中收到的**非 steer** 命令，会不会被吞掉、顺序对不对。
#
# 这是拆 stdin 线程时新开的一条路，之前一行都没验过：
#   drain_async_events 里 try_recv 捞到的命令，steer 进 SteerGate，
#   其余进 deferred，回合结束后逆序插回 pending 队首。
# 假设：不丢、顺序不乱。假设是要验的。
#
# 断言：
#   1. 回合进行中发的 get_state 最终拿到成功响应（没被吞）
#   2. 回合进行中连发两条（get_state + get_entries），两条都回、且按发送顺序回
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/deferred-ws.XXXXXX)
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/deferred-engine.log 2>&1 &
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
      --log ./timebus.ndjson < "$FIFO" > /tmp/deferred-harnessd.log 2>&1 ) &
HP=$!
exec 9>"$FIFO"
send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }
logcount() { grep -c "$1" /tmp/deferred-harnessd.log 2>/dev/null | tail -1; }

# 慢回合
send '{"id":"d0","type":"prompt","message":"请写一篇 2500 字的故事，慢慢写，不要提前结束。"}'
for _ in $(seq 1 60); do
  [ "$(logcount '"type":"turn_start"')" -ge 1 ] && break
  sleep 0.1
done
sleep 0.4
# 回合进行中，连发两条非 steer 命令
send '{"id":"g1","type":"get_state"}'
send '{"id":"g2","type":"get_entries"}'

ended=0
for _ in $(seq 1 300); do
  [ "$(logcount '"type":"agent_end"')" -ge 1 ] && { ended=1; break; }
  sleep 0.5
done
# 回合结束后等两条命令被处理
for _ in $(seq 1 60); do
  [ "$(logcount '"id":"g2"')" -ge 1 ] && break
  sleep 0.5
done
sleep 1
exec 9>&-
rm -f "$FIFO"
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

result=$(python3 <<'PY'
import json
resp = {}
order = []
for line in open('/tmp/deferred-harnessd.log'):
    if not line.startswith('{'): continue
    try: d = json.loads(line)
    except: continue
    if d.get('type') == 'response' and d.get('id') in ('g1', 'g2'):
        resp[d['id']] = d.get('success')
        if d['id'] not in order:
            order.append(d['id'])

problems = []
if 'g1' not in resp: problems.append("回合中发的 get_state 没响应——被吞了")
elif not resp['g1']: problems.append("get_state 返回失败")
if 'g2' not in resp: problems.append("回合中发的 get_entries 没响应——被吞了")
elif not resp['g2']: problems.append("get_entries 返回失败")
if order and order != ['g1', 'g2']:
    problems.append(f"响应顺序乱了：{order}，应该是 ['g1', 'g2']")

if problems:
    print("FAIL " + "；".join(problems))
else:
    print(f"PASS 回合中发的两条非 steer 命令都回来了，顺序 {order}")
PY
)
echo "$result"
echo "$result" | grep -q '^FAIL' && exit 1
echo "PASS deferred：回合进行中的非 steer 命令不丢、顺序不乱"
