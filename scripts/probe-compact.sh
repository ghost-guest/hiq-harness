#!/usr/bin/env bash
# 逼出一次真压缩，验证 `compact.pre` 端到端真的会发给插件。
#
# 为什么不能一发就中：缓存守卫的规则是"窗口快满 **且** 前缀缓存还很热（≥90%）才建议压缩"，
# 否则它只会冻结前缀（压缩反而更贵）。第一回合没有任何缓存，命中率天然低，
# 所以必须**在同一线程里跑第二回合**：前缀复用了，命中率才上得去。
#
# 是否压缩还取决于**上游代理这一回合的缓存命中率**，那不是我们能控制的
# （实测同一份 prompt 有时 76%、有时 93%）。所以按次数收敛，
# 但每次失败都打印出来——不是静默重试。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"

PLUGIN=/tmp/event-tap-compact
TAP=/tmp/event-tap-compact.log
ROUNDS="${ROUNDS:-4}"
ATTEMPTS="${ATTEMPTS:-6}"
BUDGET="${TURN_BUDGET:-90}"
export CODEWHALE_HOME

( cd "$ROOT/plugins/event-tap" && go build -o "$PLUGIN" . ) || { echo "FAIL 插件编译失败"; exit 1; }

LONG=$(python3 -c "print('这段材料用来把上下文撑到窗口压力线以上，内容本身没有意义。'*260)")

# 跑一轮。成功（tap 里出现 compact.pre）返回 0。
attempt() {
  PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
  WS=$(mktemp -d /tmp/compact-ws.XXXXXX)
  : > "$TAP"

  "$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
    >/tmp/compact-engine.log 2>&1 &
  ENGINE=$!
  # 显式清理。不要用 `trap ... RETURN`：它在 `return 1` 之前不会触发，
  # 结果是每次失败的尝试都漏一个引擎——实测跑三次漏了 19 个。
  reap() {
    kill "$ENGINE" 2>/dev/null
    wait "$ENGINE" 2>/dev/null
    pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  }

  up=0
  for _ in $(seq 1 40); do
    if curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null; then up=1; break; fi
    sleep 0.5
  done
  [ "$up" = 1 ] || { echo "!! 引擎没起来（端口 $PORT）"; reap; return 1; }

  # 一条一条发：等上一轮 `turn.end` 再发下一条。
  # 一次性把 N 条塞进 stdin 是不对的——引擎只接受第 1 条，其余被拒
  # （那个"静默丢弃"已在 harnessd 里修成报错，这里按真实用法发）。
  FIFO=/tmp/compact-fifo.$PORT
  rm -f "$FIFO"; mkfifo "$FIFO"
  # 探针里没有 UI，没人回答审批。模型一旦想跑工具，回合就挂到超时。
  # 开一个后台线程，看到 approval.required 就答 allow——
  # 这不是绕过守卫：守卫在 harnessd 里已经跑过了，走到审批说明守卫放行了。
  python3 - "$PORT" <<'APPROVER' >/tmp/compact-approver.log 2>&1 &
import json, sys, time, urllib.request
port = sys.argv[1]; base = f"http://127.0.0.1:{port}"
answered = set(); deadline = time.time() + 600
while time.time() < deadline:
    try:
        pend = json.loads(urllib.request.urlopen(base + "/v1/approvals", timeout=5).read())
        for a in (pend.get("approvals") or pend.get("pending") or []):
            aid = a.get("id") or a.get("approval_id")
            if not aid or aid in answered: continue
            answered.add(aid)
            req = urllib.request.Request(base + f"/v1/approvals/{aid}",
                data=json.dumps({"decision": "allow"}).encode(),
                headers={"content-type": "application/json"}, method="POST")
            try: urllib.request.urlopen(req, timeout=10)
            except Exception: pass
    except Exception:
        pass
    time.sleep(1)
APPROVER
  APPROVER_PID=$!

  ( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":false,"auto_compact":true}' \
      HARNESSD_WINDOW="${HARNESSD_WINDOW:-9000}" EVENT_TAP_LOG="$TAP" \
      exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
        --plugin "$PLUGIN" --log ./timebus.ndjson \
      < "$FIFO" > /tmp/compact-harnessd.log 2>&1 ) &
  HP=$!

  # 数 `turn.end` 落账了几轮。注意：`grep -c` 在**零匹配**时返回退出码 1，
  # 直接写成 `$(grep -c ... || echo 0)` 会多出一行、算术比较报
  # "integer expression expected"。这里让 grep 的输出单独落到变量。
  turn_ends() {
    grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null || true
  }

  # 等待预算跟 TURN_BUDGET 走：两个各写一个数就会互相打架
  # （实测等回合 120s、杀 harnessd 60s，结果总时长翻倍还没人解释）。
  wait_turn_end() {
    local want=$1 n=0 seen limit=$((BUDGET * 2))
    while [ "$n" -lt "$limit" ]; do
      seen=$(turn_ends); seen=${seen:-0}
      [ "$seen" -ge "$want" ] && return 0
      sleep 0.5; n=$((n + 1))
    done
    return 1
  }
  exec 9>"$FIFO"
  for i in $(seq 1 "$ROUNDS"); do
    python3 -c 'import json,sys;print(json.dumps({"id":"r"+sys.argv[1],"type":"prompt","message":"第"+sys.argv[1]+"回合，只回一个词：收到。 材料："+sys.argv[2]}))' "$i" "$LONG" >&9
    # 每轮之间查一下 tap：`compact.pre` 一旦到了就**提前结束**，
    # 不用把剩下的回合也跑完。
    if grep -q "compact.pre" "$TAP" 2>/dev/null; then
      echo "…第 $i 回合就触发了压缩，提前结束（还剩 $((ROUNDS - i)) 回合没跑）"
      break
    fi
    wait_turn_end "$i" || { echo "!! 第 $i 回合没结束（超时），继续往后试" ; i=$((i + 1)); continue; }
  done
  exec 9>&-
  rm -f "$FIFO"

  # 回合可能挂住（模型想跑工具但这里没人回答审批）。到点就杀，并且说出来——
  # 静默卡死比报错难查得多。macOS 没有 `timeout`，所以自己盯。
  for _ in $(seq 1 "$BUDGET"); do
    kill -0 "$HP" 2>/dev/null || break
    sleep 1
  done
  if kill -0 "$HP" 2>/dev/null; then
    echo "!! harnessd 跑满 ${BUDGET}s 还没退出——回合挂住了，强杀"
    kill -TERM "$HP" 2>/dev/null; sleep 2; kill -KILL "$HP" 2>/dev/null
  fi
  wait "$HP" 2>/dev/null || true

  got=$(sort "$TAP" 2>/dev/null | uniq | tr '\n' ' ')
  hits=$(grep -o '"cached_tokens":[0-9]*' "$WS/timebus.ndjson" 2>/dev/null | tail -2 | tr '\n' ' ')

  if [ ! -s "$TAP" ]; then
    echo "!! 插件一个事件都没收到——这不是\"没压缩\"，是插件没跑起来（EVENT_TAP_LOG=$TAP）"
    grep -i "plugin" /tmp/compact-harnessd.log 2>/dev/null | tail -3
  fi
  echo "收到的事件：${got:-（空）}"
  echo "用量（cached_tokens 尾两条）：${hits:-（无）}"

  kill "$APPROVER_PID" 2>/dev/null
  [ "${KEEP_WS:-0}" = "1" ] || rm -rf "$WS" || true
  reap
  printf '%s' "$got" | grep -q "compact.pre"
}

for i in $(seq 1 "$ATTEMPTS"); do
  if attempt; then
    echo "PASS compact.pre 端到端到达插件（真引擎 + 真 harnessd + 真插件，第 $i 次）"
    exit 0
  fi
  echo "…第 $i/$ATTEMPTS 次没触发（这一回合的命中率不够）"
done

echo "FAIL $ATTEMPTS 次都没收到 compact.pre（看 /tmp/compact-harnessd.log 与 /tmp/compact-engine.log）"
exit 1
