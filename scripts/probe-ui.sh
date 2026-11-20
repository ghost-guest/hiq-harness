#!/usr/bin/env bash
# 界面探针：启动 orbit，截图，OCR，断言关键控件真的画出来了。
#
# 为什么是 OCR：这个环境里的模型看不到图，人眼验证也只在当时有效。
# "界面渲染出来了"这句话要么有可复跑的证据，要么就是一句猜测。
#
# 依赖：一块**解锁**的屏幕。锁屏时截图只有登录框，那就根本不是界面测试。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
HARNESSD_BIN="${HARNESSD_BIN:-$ROOT/target/debug/harnessd}"
ORBIT_BIN="${ORBIT_BIN:-$ROOT/apps/orbit/target/debug/orbit-pi}"
SHOT="${SHOT:-/tmp/orbit-ui-probe.png}"
# OCR 用 `xtask ocr`（Rust，基于 objc2-vision）。以前是 swiftc 编译一个
# 20 行的 Swift 脚本——仓库只允许 Rust + Go，所以收编进 xtask。
OCR_CMD=(cargo run -q -p xtask -- ocr)

for f in "$HARNESSD_BIN" "$ORBIT_BIN" "$HARNESS_ENGINE_BIN"; do
  [ -x "$f" ] || { echo "FAIL 缺少可执行文件：$f"; exit 1; }
done

# 锁屏时截图是登录框，测的就不是界面了。必须明说，不能装作通过。
if python3 -c "
import sys
try:
    import Quartz
except ImportError:
    sys.exit(0)          # 没装 pyobjc 就跳过这个判断，别假装知道
sys.exit(0 if Quartz.CGSessionCopyCurrentDictionary().get('CGSSessionScreenIsLocked') else 1)
" 2>/dev/null; then
  echo "SKIP 屏幕锁着——截图只会得到登录框，这不是界面测试。解锁后重跑。"
  exit 2
fi

# 先确认 OCR 工具能编译（xtask 是 Rust，不需要额外编译器）。
cargo build -q -p xtask || { echo "FAIL 编译 OCR helper 失败"; exit 1; }

# 屏幕上只要有别的 orbit 窗口，OCR 就会读到它的内容，探针就会假绿——
# 上一轮残留的窗口曾经让 `/bin/sleep` 冒充 orbit 也能通过。先清场。
pkill -f "apps/orbit/target/debug/orbit-pi" 2>/dev/null
pkill -f "target/debug/orbit-pi" 2>/dev/null
sleep 1

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
WS=$(mktemp -d /tmp/ui-probe-ws.XXXXXX)
cleanup() {
  [ -n "${ORBIT_PID:-}" ] && kill "$ORBIT_PID" 2>/dev/null
  [ -n "${ENGINE_PID:-}" ] && kill "$ENGINE_PID" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

export CODEWHALE_HOME
"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" >/tmp/ui-probe-engine.log 2>&1 &
ENGINE_PID=$!
for _ in $(seq 1 40); do curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.5; done
curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null || { echo "FAIL 引擎没起来"; exit 1; }

( cd "$WS" && PI_BIN="$HARNESSD_BIN" HARNESSD_HTTP_ENGINE="http://127.0.0.1:$PORT" \
    HARNESSD_THREAD_BODY='{"auto_approve":false}' \
    "$ORBIT_BIN" >/tmp/ui-probe-orbit.log 2>&1 ) &
ORBIT_PID=$!
disown 2>/dev/null || true
sleep "${UI_SETTLE:-12}"

# 探针自己起的那个进程必须还活着。死了却还能读到界面，读到的就是别人的窗口。
if ! kill -0 "$ORBIT_PID" 2>/dev/null; then
  echo "FAIL 探针起的 orbit 已经退出（${ORBIT_BIN}），屏幕上的界面不是它画的"
  tail -5 /tmp/ui-probe-orbit.log
  exit 1
fi

if [ -n "${SHOT_OVERRIDE:-}" ]; then
  # 自检用：拿一张已知内容的图喂进来，验证"界面缺元素时探针真的会红"。
  cp "$SHOT_OVERRIDE" "$SHOT"
else
  screencapture -x "$SHOT" || { echo "FAIL 截图失败"; exit 1; }
fi
TEXT="$("${OCR_CMD[@]}" "$SHOT" 2>/dev/null)"
echo "--- OCR 读到 $(( $(printf '%s' "$TEXT" | wc -l) )) 行 ---"
printf '%s\n' "$TEXT"

# 锁屏兜底：万一截图时刚好锁了，别把登录框的密码提示当成界面。
if printf '%s' "$TEXT" | grep -q "使用触控ID"; then
  echo "FAIL 截到的是锁屏，不是 orbit"
  exit 1
fi

PROBLEMS=()
need() { printf '%s' "$TEXT" | grep -q "$1" || PROBLEMS+=("$2"); }
need "项目"      "侧栏的项目区没画出来"
need "搜索会话"  "会话搜索框没画出来"
need "描述任务"  "任务输入框没画出来"

if [ ${#PROBLEMS[@]} -gt 0 ]; then
  echo "## !! 界面不完整"
  for p in "${PROBLEMS[@]}"; do echo "  - $p"; done
  exit 1
fi
echo "PASS 界面渲染（项目区 / 会话搜索 / 任务输入框 都在），截图 $SHOT"
