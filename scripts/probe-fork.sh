#!/usr/bin/env bash
# fork 之后，新线程能不能继续跑。
#
# 为什么单独一条：`fork_switches_to_the_new_thread` 只验了"id 换了"。
# 但换 id 之后 `self.sse` 还是**旧线程**的订阅，而 `ensure_stream()` 见到
# `sse.is_some()` 就直接返回——新线程永远没有订阅者。
# 没有订阅者时引擎会让回合停住（实测 POST 201 → 60 秒 items=0 → 下一回合 409）。
# 单元测试里没有 SSE，永远看不到这个。
#
# 断言：
#   1. 已完成回合的线程能 fork 成功，拿到新 id
#   2. **在新线程上还能跑完一个回合**（这才是要害）
#   3. 新线程的回合闭合
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/fork-ws.XXXXXX)
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/fork-engine.log 2>&1 &
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
      --log ./timebus.ndjson < "$FIFO" > /tmp/fork-harnessd.log 2>&1 ) &
HP=$!
exec 9>"$FIFO"

turn_ends() { grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null || true; }
wait_end() {
  local want=$1 n=0 seen
  while [ "$n" -lt 200 ]; do
    seen=$(turn_ends); seen=${seen:-0}
    [ "$seen" -ge "$want" ] && return 0
    sleep 0.5; n=$((n + 1))
  done
  return 1
}
send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }

# 1) 先跑完一个回合（fork 要求已完成的回合，空线程会 409）
send '{"id":"a1","type":"prompt","message":"回一个字：好"}'
wait_end 1 || { echo "FAIL 前置回合没闭合"; exit 1; }

# 2) fork
# 先从第一条 response 里取旧线程 id——判据要证明"真的换了线程"，
# 不能只看"回合跑完了"（原地不动也能跑完）。
OLD_THREAD=$(python3 -c "
import json
for l in open('/tmp/fork-harnessd.log'):
    if not l.startswith('{'): continue
    try: d=json.loads(l)
    except: continue
    if d.get('type')=='response' and d.get('id')=='a1':
        print((d.get('data') or {}).get('thread',{}).get('id','')); break
")
# fork 要求引擎那边回合真的结束。`turn.end` 落账只说明 harnessd 看见了，
# 引擎自己可能还慢一拍——实测拿到 409 "Workspace already has an active turn"。
# 所以这里重试几次，而不是一次 409 就判红（那是引擎约束，不是内核缺陷）。
FORKED=0
for attempt in 1 2 3 4 5; do
  send "{\"id\":\"a2_$attempt\",\"type\":\"fork\"}"
  for _ in $(seq 1 30); do
    if grep -q "\"id\":\"a2_$attempt\",\"success\":true" /tmp/fork-harnessd.log 2>/dev/null; then
      FORKED=1; break 2
    fi
    sleep 0.3
  done
done
[ "$FORKED" -eq 1 ] || echo "注意：5 次 fork 都失败（看 /tmp/fork-harnessd.log）"
# 3) 在新线程上再跑一个回合——**要害在这里**
send '{"id":"a3","type":"prompt","message":"回一个字：好"}'
if ! wait_end 2; then
  echo "FAIL fork 之后的线程跑不完回合（多半是 SSE 还订在旧线程上）"
  echo "  已闭合回合数：$(turn_ends)"
  exit 1
fi

exec 9>&-
rm -f "$FIFO"
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

read -r STARTS ENDS THREADS <<EOF
$(python3 - "$WS/timebus.ndjson" <<'PY'
import json, sys
s = e = 0; t = set()
for line in open(sys.argv[1]):
    d = json.loads(line)
    if d["kind"] == "turn.start":
        s += 1
        if isinstance(d.get("payload"), dict) and d["payload"].get("thread"):
            t.add(d["payload"]["thread"])
    elif d["kind"] == "turn.end":
        e += 1
print(s, e, len(t))
PY
)
EOF

PROBLEMS=()
[ "${STARTS:-0}" -ge 2 ] || PROBLEMS+=("只开了 ${STARTS:-0} 个回合，fork 后那条没跑")
[ "${ENDS:-0}" -eq "${STARTS:-0}" ] || PROBLEMS+=("回合没闭合：start=${STARTS:-0} end=${ENDS:-0}")
NEW_THREAD=$(python3 -c "
import json
NEW = ''
for l in open('/tmp/fork-harnessd.log'):
    if not l.startswith('{'): continue
    try: d=json.loads(l)
    except: continue
    # 重试过：前几条 a2_N 会是 409，只有 success:true 那条才有新 id。
    # 取**最后一条**成功的，不是第一条（第一条失败时 id 为空）。
    if d.get('type')=='response' and str(d.get('id','')).startswith('a2_') and d.get('success'):
        data = d.get('data') or {}
        if isinstance(data, dict) and data.get('id'):
            NEW = data['id']
print(NEW)
")
[ -n "$NEW_THREAD" ] || PROBLEMS+=("fork 没返回新线程 id")
[ "$NEW_THREAD" != "$OLD_THREAD" ] || PROBLEMS+=("fork 前后线程 id 一样（$OLD_THREAD）：$NEW_THREAD")

echo "回合：start=${STARTS:-0} end=${ENDS:-0} ｜ fork：${OLD_THREAD:-?} → ${NEW_THREAD:-?}"
if [ ${#PROBLEMS[@]} -gt 0 ]; then
  echo "## !! fork 之后跑不动"
  for p in "${PROBLEMS[@]}"; do echo "  - $p"; done
  exit 1
fi
echo "PASS fork：原线程跑完 → fork 出新线程 → 新线程上回合也闭合"
