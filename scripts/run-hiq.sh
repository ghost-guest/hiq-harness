#!/usr/bin/env bash
# 一键启动 hiq：外部引擎 + harnessd 内核 + 界面。
#
# 为什么要有这个脚本：界面只有在 `HARNESSD_BIN` 指到 harnessd 时才走内核
# （见 orbit-rpc/client.rs 的后端选择）。不设它，界面会退回**进程内执行器**，
# 也就是完全没有 harnessd 那层：守卫、time-bus、插件、事件投影全都不在。
# 那样看到的"bug"有一半根本不是内核的。以前每次手敲 5 个环境变量，漏一个就白测。
#
# 用法：
#   bash scripts/run-hiq.sh          # 前台跑，Ctrl-C 退出
#   DEV=1 bash scripts/run-hiq.sh    # 改代码后想重启又不想重编译，跳过 build
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

ENGINE_BIN="${ENGINE_BIN:-/tmp/kh/codewhale/target/debug/codewhale}"
CODEWHALE_HOME="${CODEWHALE_HOME:-/tmp/cw-real6}"
PORT="${PORT:-34567}"
STATE_DIR="${HIQ_STATE:-$HOME/.hiq}"
HARNESSD="$ROOT/target/debug/harnessd"
ORBIT="$ROOT/apps/orbit/target/debug/orbit-pi"

[ -x "$ENGINE_BIN" ] || { echo "缺引擎：$ENGINE_BIN（用 ENGINE_BIN=… 指定）"; exit 1; }
[ -d "$CODEWHALE_HOME" ] || { echo "缺引擎 home：$CODEWHALE_HOME（用 CODEWHALE_HOME=… 指定）"; exit 1; }

if [ "${DEV:-0}" != "1" ]; then
  echo "构建 harnessd…"
  ( cd "$ROOT" && cargo build -p harnessd ) || exit 1
  echo "构建界面…"
  ( cd "$ROOT/apps/orbit" && cargo build -p orbit-pi ) || exit 1
fi

# 旧实例先清掉：残留的界面会让截图/OCR 读到上一个窗口，也会抢同一个引擎端口。
pkill -f "apps/orbit/target/debug/orbit-pi" 2>/dev/null
pkill -f "target/debug/harnessd serve" 2>/dev/null
pkill -f "app-server --http.*--port $PORT" 2>/dev/null
sleep 1

mkdir -p "$STATE_DIR"
if ! curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
  echo "启动引擎 :${PORT}（日志 ${STATE_DIR}/engine.log）…"
  ( cd "$STATE_DIR" && CODEWHALE_HOME="$CODEWHALE_HOME" nohup "$ENGINE_BIN" \
      app-server --http --insecure-no-auth --port "$PORT" \
      > "$STATE_DIR/engine.log" 2>&1 & )
  up=0
  for _ in $(seq 1 40); do
    curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && { up=1; break; }
    sleep 0.5
  done
  [ "$up" = 1 ] || { echo "引擎没起来，看 $STATE_DIR/engine.log"; exit 1; }
fi

echo "启动界面（内核走 harnessd，不是进程序内执行器）…"
cd "$ROOT"
exec env \
  HARNESSD_BIN="$HARNESSD" \
  HARNESSD_HTTP_ENGINE="http://127.0.0.1:$PORT" \
  CODEWHALE_HOME="$CODEWHALE_HOME" \
  HARNESS_PROVIDER="${HARNESS_PROVIDER:-magipe}" \
  "$ORBIT"
