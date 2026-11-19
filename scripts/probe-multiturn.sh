#!/usr/bin/env bash
# 多回合探针：验证「先订阅，再发回合」这个顺序真的对。
#
# 为什么单独一条：变异扫描只能证明"改坏了单元测试会红"，
# 证明不了"真引擎跑起来是对的"。而这条修复的证据完全来自真引擎——
# 顺序错了的表现是「第一回合 201、60 秒 items=0、第二回合 409」，
# 单元测试里没有引擎，永远看不到这个。
#
# 断言三件事：
#   1. 同一线程能连着跑 N 个回合（不是每线程只能一回合）
#   2. 每个回合都闭合（turn.start 与 turn.end 数量相等）
#   3. 第二回合的命中率高于第一回合（前缀真的被复用了）
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
ROUNDS="${ROUNDS:-3}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
WS=$(mktemp -d /tmp/multiturn-ws.XXXXXX)
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/multiturn-engine.log 2>&1 &
ENGINE=$!
up=0
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && { up=1; break; }
  sleep 0.5
done
[ "$up" = 1 ] || { echo "FAIL 引擎没起来"; exit 1; }

# 一条一条发：等上一轮 turn.end 再发下一条。
# 一次性把 N 条塞进 stdin 是不对的——引擎只接受第 1 条。
FIFO="$WS/in.fifo"
mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":true}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > /tmp/multiturn-harnessd.log 2>&1 ) &
HP=$!
exec 9>"$FIFO"

turn_ends() { grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null || true; }
wait_end() {
  local want=$1 n=0 seen
  while [ "$n" -lt 180 ]; do
    seen=$(turn_ends); seen=${seen:-0}
    [ "$seen" -ge "$want" ] && return 0
    sleep 0.5; n=$((n + 1))
  done
  return 1
}

for i in $(seq 1 "$ROUNDS"); do
  python3 -c 'import json,sys;print(json.dumps({"id":"r"+sys.argv[1],"type":"prompt","message":"第"+sys.argv[1]+"回合：只回一个词，收到。"}))' "$i" >&9
  wait_end "$i" || { echo "FAIL 第 $i 回合没闭合（超时）—— 多半是订阅顺序又错了"; exit 1; }
done
exec 9>&-
rm -f "$FIFO"
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

# ---- 断言 ----
starts=$(grep -c '"kind":"turn.start"' "$WS/timebus.ndjson" 2>/dev/null || true)
ends=$(turn_ends)
starts=${starts:-0}; ends=${ends:-0}

hits=$(python3 - "$WS/timebus.ndjson" <<'PY'
import json, sys
rates = []
for line in open(sys.argv[1]):
    e = json.loads(line)
    if e["kind"] == "usage":
        p = e["payload"]
        i = p.get("input_tokens", 0)
        c = p.get("cached_tokens", 0)
        if i: rates.append(round(c * 100 / i))
print(" ".join(str(r) for r in rates))
PY
)

PROBLEMS=()
[ "$starts" -eq "$ROUNDS" ] || PROBLEMS+=("回合数不对：发了 $ROUNDS 条，开了 $starts 个回合")
[ "$ends" -eq "$starts" ] || PROBLEMS+=("有回合没闭合：start=$starts end=$ends（timebus 不闭合，投影全错）")

# 第二回合命中率应高于第一回合（前缀复用）。只有一回合时不判。
set -- $hits
if [ $# -ge 2 ]; then
  [ "$2" -gt "$1" ] || PROBLEMS+=("第二回合命中率没涨（$1% → $2%）——前缀没被复用")
fi

echo "回合：start=$starts end=$ends ｜ 逐回合命中率：${hits:-（无）}"
if [ ${#PROBLEMS[@]} -gt 0 ]; then
  echo "## !! 多回合不成立"
  for p in "${PROBLEMS[@]}"; do echo "  - $p"; done
  exit 1
fi
echo "PASS 多回合：$ROUNDS 个回合都闭合，前缀跨回合复用（命中率 ${hits}）"
