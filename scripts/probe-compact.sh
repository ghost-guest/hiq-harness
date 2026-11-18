#!/usr/bin/env bash
# 逼出一次真压缩，验证 `compact.pre` 端到端真的会发给插件。
#
# 为什么不能一发就中：缓存守卫的规则是"窗口快满 **且** 前缀缓存还很热（≥90%）才建议压缩"，
# 否则它只会冻结前缀（压缩反而更贵）。第一回合没有任何缓存，命中率天然低，
# 所以必须**在同一线程里跑第二回合**：前缀复用了，命中率才上得去。
#
# 这也是这一格一直没被跑通的原因：之前的探针都是单回合。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
PLUGIN=/tmp/event-tap-compact
TAP=/tmp/event-tap-compact.log
WS=$(mktemp -d /tmp/compact-ws.XXXXXX)
: > "$TAP"

( cd "$ROOT/plugins/event-tap" && go build -o "$PLUGIN" . ) || { echo "FAIL 插件编译失败"; exit 1; }

export CODEWHALE_HOME
"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/compact-engine.log 2>&1 &
ENGINE=$!
cleanup() {
  kill "$ENGINE" 2>/dev/null
  wait "$ENGINE" 2>/dev/null
  # 兜底：万一 exec 出的新进程换了 PID，按端口再清一次。
  # 残留引擎会占住端口和 CPU，让别的时序测试（比如心跳重绘）偶发红——
  # 那种红查起来极贵，而且是假的。
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
}
trap cleanup EXIT

for _ in $(seq 1 40); do curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.5; done
curl -sf -m 5 -X POST "http://127.0.0.1:$PORT/v1/providers/magipe/switch" \
  -H 'content-type: application/json' -d '{}' >/dev/null || true

# 多回合：同样的长前缀，每回合尾巴换一句。命中率是**逐回合爬上去的**
# （实测第一回合 76%，第二回合 92%），所以不能只跑一回合就断言"不压缩"。
# 上限 ROUNDS 回合；见到 compact.pre 就算成。
LONG=$(python3 -c "print('这段材料用来把上下文撑到窗口压力线以上，内容本身没有意义。'*260)")
python3 - "$LONG" > /tmp/compact-prompts.ndjson <<'PY'
import json, os, sys
long = sys.argv[1]
for i in range(1, int(os.environ.get("ROUNDS", "4")) + 1):
    print(json.dumps({"id": "x", "type": "prompt",
                      "message": f"第{i}回合，只回一个词：收到。 材料：{long}"}))
PY

( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":false,"auto_compact":true}' \
    HARNESSD_WINDOW="${HARNESSD_WINDOW:-9000}" EVENT_TAP_LOG="$TAP" \
    "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --plugin "$PLUGIN" --log ./timebus.ndjson \
    < /tmp/compact-prompts.ndjson > /tmp/compact-harnessd.log 2>&1 )

got=$(sort "$TAP" 2>/dev/null | uniq | tr '\n' ' ')
hits=$(grep -o '"cached_tokens":[0-9]*' "$WS/timebus.ndjson" 2>/dev/null | tail -2 | tr '\n' ' ')
# 保留工作区：失败时要能看 timebus
[ "${KEEP_WS:-0}" = "1" ] || rm -rf "$WS" || true

echo "收到的事件：${got:-（空）}"
echo "用量（cached_tokens 尾两条）：${hits:-（无）}"

# 是否压缩，取决于**上游代理这一回合的缓存命中率**（守卫要求 ≥90%），
# 而那不是我们能控制的：实测同一份 prompt 有时 76%、有时 93%。
# 所以这里按"最多 ATTEMPTS 次"收敛，而不是撞一次就下结论——
# 但每次失败都打印出来了，不是静默重试。
ATTEMPTS="${ATTEMPTS:-3}"
if printf '%s' "$got" | grep -q "compact.pre"; then
  echo "PASS compact.pre 端到端到达插件（真引擎 + 真 harnessd + 真插件）"
  exit 0
fi
if [ "$ATTEMPTS" -gt 1 ]; then
  echo "…第 1 次没触发（这一回合的命中率不够），重试"
  ATTEMPTS=$((ATTEMPTS - 1)) exec env ATTEMPTS="$ATTEMPTS" bash "$0"
fi
echo "FAIL $ATTEMPTS 次都没收到 compact.pre（看 /tmp/compact-harnessd.log 与 /tmp/compact-engine.log）"
exit 1
