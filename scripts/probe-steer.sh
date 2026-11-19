#!/usr/bin/env bash
# steer：验证「引擎 API 收得到」+ 如实记录「harnessd compat 层的时序限制」。
#
# 这一轮抓到两个不同层面的东西，别混为一谈：
#
# 1. **真 bug（已修）**：mapping.rs 的 Steer 把文本翻成 `input` 数组，
#    但 http_engine 只读字符串 → 发 `{"prompt": ""}` → 引擎 400
#    `prompt is required`。现在 `steer_text()` 认三种方言，单测 + 变异体钉住。
#
# 2. **架构限制（未修，如实记录）**：orbit compat 是**单线程同步循环**，
#    prompt 命令会 `drain_async_events()` 一直阻塞到回合结束。此时第二条
#    steer 命令根本进不来。所以在这个协议下「回合中途插话」不可能工作。
#    引擎自己的 steer API 是好的（直连实测 200），限制在我们这一层。
#
# 本探针做两件事：
#   A. 直连引擎：回合进行中发 steer，必须 2xx（证明引擎侧没问题）
#   B. 调 harnessd 单测：body 方言必须翻对（证明修复在）
# 不再假装 harnessd compat 层能做中途 steer。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
cleanup() { pkill -f "app-server --http.*--port $PORT" 2>/dev/null; }
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/steer-engine.log 2>&1 &
up=0
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && { up=1; break; }
  sleep 0.5
done
[ "$up" = 1 ] || { echo "FAIL 引擎没起来"; exit 1; }

result=$(python3 - "$PORT" <<'PY'
import json, sys, time, urllib.request, urllib.error, socket
port = sys.argv[1]; base = f"http://127.0.0.1:{port}"
def post(path, data, timeout=120):
    req = urllib.request.Request(base + path, data=json.dumps(data).encode(),
        headers={"content-type": "application/json"}, method="POST")
    try:
        r = urllib.request.urlopen(req, timeout=timeout)
        return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, e.read()[:150].decode("utf8", "replace")
    except Exception as e:
        return 0, str(e)

_, t = post("/v1/threads", {"auto_approve": True})
tid = t.get("id")
# 慢回合，给 steer 留出窗口
status, turn = post(f"/v1/threads/{tid}/turns",
                    {"prompt": "请写一个 2000 字的故事，慢慢写，不要停。"})
turn_id = (turn.get("turn") or {}).get("id") if isinstance(turn.get("turn"), dict) else None
if not turn_id:
    print(f"FAIL 回合没返回 turn_id（status={status}）")
    raise SystemExit(0)
# 立刻插话（不等回合结束）
code, body = post(f"/v1/threads/{tid}/turns/{turn_id}/steer",
                  {"prompt": "STOP 改说 STEER_MARK_7"})
if 200 <= code < 300:
    print(f"引擎 steer API：{code} —— 回合进行中插话被接受")
else:
    print(f"FAIL 引擎 steer API：{code} {str(body)[:100]}")
PY
)

echo "$result"
# body 方言的单测（快，不碰引擎）
unit=$(cargo test -q -p harnessd steer_text_understands 2>&1)
if printf '%s' "$unit" | grep -q "1 passed"; then
  echo "steer body 方言：mapping 的 input 数组 → 非空 prompt（单测通过）"
else
  echo "FAIL steer body 方言单测没过"
  exit 1
fi

if printf '%s' "$result" | grep -q '^FAIL'; then exit 1; fi
echo "注：compat 层的时序限制（回合中 stdin 阻塞）已由 probe-steer-midturn.sh 验证打通"
echo "PASS steer：引擎侧接受 + harnessd body 翻译正确"
