#!/usr/bin/env bash
# 带审批的多回合：验证**守卫/审批/插件**这条路跨回合还活着。
#
# 为什么单独一条：`probe-multiturn.sh` 用 `auto_approve:true` 拿到
# "3 回合全闭合"——但那会**关掉整条守卫/审批/插件路径**。
# 也就是说那条证据是在守卫关着的条件下成立的。
# 这条用 `auto_approve:false`，每轮真跑一个 bash 工具，跨轮验：
#   1. 每轮的回合都闭合
#   2. 每轮都真的产生了工具调用（不是模型自己说话完事）
#   3. 每轮都真的写了文件（审批通道通了——否则引擎会挂在等待审批上）
#      ※ 审批 Request 帧在 `on_reverse_request` 里被消费，不写 timebus，
#        所以不能靠数 approval 事件。工具跑完本身就是通道通了的证据。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
ROUNDS="${ROUNDS:-2}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/guarded-ws.XXXXXX)
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  [ -n "${APPROVER_PID:-}" ] && kill "$APPROVER_PID" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/guarded-engine.log 2>&1 &
up=0
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && { up=1; break; }
  sleep 0.5
done
[ "$up" = 1 ] || { echo "FAIL 引擎没起来"; exit 1; }

# 探针里没有 UI。守卫在 harnessd 里已经跑过，走到审批说明守卫放行了，
# 所以这里答 allow 不是绕过守卫。
python3 - "$PORT" <<'APPROVER' >/tmp/guarded-approver.log 2>&1 &
import json, sys, time, urllib.request
port = sys.argv[1]; base = f"http://127.0.0.1:{port}"
answered = set(); deadline = time.time() + 600
while time.time() < deadline:
    try:
        pend = json.loads(urllib.request.urlopen(base + "/v1/approvals", timeout=5).read())
        for a in (pend.get("approvals") or pend.get("pending") or []):
            aid = a.get("id") or a.get("approval_id")
            if not aid or aid in answered:
                continue
            answered.add(aid)
            req = urllib.request.Request(base + f"/v1/approvals/{aid}",
                data=json.dumps({"decision": "allow"}).encode(),
                headers={"content-type": "application/json"}, method="POST")
            try:
                urllib.request.urlopen(req, timeout=10)
            except Exception:
                pass
    except Exception:
        pass
    time.sleep(1)
APPROVER
APPROVER_PID=$!

FIFO="$WS/in.fifo"
mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":false}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > /tmp/guarded-harnessd.log 2>&1 ) &
HP=$!
exec 9>"$FIFO"

turn_ends() { grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null || true; }
wait_end() {
  local want=$1 n=0 seen
  while [ "$n" -lt 240 ]; do
    seen=$(turn_ends); seen=${seen:-0}
    [ "$seen" -ge "$want" ] && return 0
    sleep 0.5; n=$((n + 1))
  done
  return 1
}

for i in $(seq 1 "$ROUNDS"); do
  # 必须用**写**命令：只读命令（echo/cat/ls）引擎会自动放行，不触发审批，
  # 那样这条探针就测不到守卫路径。实测 `echo hi > f` 会触发 approval.required。
  rm -f "/tmp/guarded-$i.txt"
  MSG="第${i}轮：请用 bash 工具执行 echo guarded-$i > /tmp/guarded-$i.txt 然后 cat /tmp/guarded-$i.txt。必须真的执行。"
  python3 -c 'import json,sys;print(json.dumps({"id":"g"+sys.argv[1],"type":"prompt","message":sys.argv[2]}))' "$i" "$MSG" >&9
  wait_end "$i" || { echo "FAIL 第 $i 轮没闭合（超时）"; exit 1; }
done
exec 9>&-
rm -f "$FIFO"
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=
kill "$APPROVER_PID" 2>/dev/null; APPROVER_PID=

# ---- 断言（按轮切分 timebus）----
read -r STARTS ENDS <<EOF
$(python3 - "$WS/timebus.ndjson" <<'PY'
import json, sys
s = e = 0
for line in open(sys.argv[1]):
    e2 = json.loads(line)
    if e2["kind"] == "turn.start": s += 1
    elif e2["kind"] == "turn.end": e += 1
print(s, e)
PY
)
EOF

PER=$(python3 - "$WS/timebus.ndjson" "$ROUNDS" <<'PY'
import json, sys
rounds = int(sys.argv[2])
tools, approvals = [], []
for line in open(sys.argv[1]):
    e = json.loads(line)
    if e["kind"] == "turn.start":
        tools.append(0); approvals.append(0)
    elif e["kind"] == "tool.call" and tools:
        tools[-1] += 1
    elif "approval" in e["kind"] and approvals:
        approvals[-1] += 1
print(" ".join(str(t) for t in tools), "|", " ".join(str(a) for a in approvals))
PY
)

TOOLS=$(printf '%s' "$PER" | cut -d'|' -f1 | xargs)
APPR=$(printf '%s' "$PER" | cut -d'|' -f2 | xargs)

PROBLEMS=()
[ "${STARTS:-0}" -eq "$ROUNDS" ] || PROBLEMS+=("回合数不对：发了 $ROUNDS 条，开了 ${STARTS:-0} 个")
[ "${ENDS:-0}" -eq "${STARTS:-0}" ] || PROBLEMS+=("有回合没闭合：start=${STARTS:-0} end=${ENDS:-0}")

set -- $TOOLS
if [ $# -ge 2 ]; then
  first=$1
  [ "$first" -ge 1 ] || PROBLEMS+=("第一轮没有工具调用——模型没真跑工具，这条探针没测到东西")
  for t in "$@"; do
    [ "$t" -eq "$first" ] || { PROBLEMS+=("每轮工具数不一致（$TOOLS）——第 1 轮之后就失效了"); break; }
  done
fi

WRITTEN=0
for i in $(seq 1 "$ROUNDS"); do
  [ -f "/tmp/guarded-$i.txt" ] && WRITTEN=$((WRITTEN + 1))
done
[ "$WRITTEN" -eq "$ROUNDS" ] || PROBLEMS+=("只有 $WRITTEN/$ROUNDS 轮真的写了文件——审批通道没跨回合通")
cleanup_files() { for i in $(seq 1 "$ROUNDS"); do rm -f "/tmp/guarded-$i.txt"; done; }
cleanup_files

echo "回合：start=${STARTS:-0} end=${ENDS:-0} ｜ 每轮工具数：$TOOLS ｜ 审批通道实际写文件：$WRITTEN/$ROUNDS"
if [ ${#PROBLEMS[@]} -gt 0 ]; then
  echo "## !! 守卫路径跨回合不成立"
  for p in "${PROBLEMS[@]}"; do echo "  - $p"; done
  exit 1
fi
echo "PASS 守卫跨回合：$ROUNDS 轮都闭合，每轮工具数一致（${TOOLS}），审批通道 $WRITTEN/$ROUNDS 都通"
