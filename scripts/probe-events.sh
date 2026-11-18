#!/usr/bin/env bash
# S4 的事件验收：内核广播的 6 种事件里，一次普通回合**实际**看得到哪几种。
#
# 为什么要问这个：`grep broadcast` 只能证明接了线，证明不了发得出来。
# 这里的做法是真的跑一轮（真引擎 + 真 harnessd + 真插件），让插件把收到的
# 每一种事件写一行，然后对账。
#
# 4 种常驻事件必须出现；guard.deny / compact.pre 是条件触发（守卫真的拒了、
# 上下文真的压缩了才会发），所以只报告、不判死——但必须说清楚，不许含糊成"都有"。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"

PLUGIN=/tmp/event-tap-plugin
TAP=/tmp/event-tap.log
WS=$(mktemp -d /tmp/event-tap-ws.XXXXXX)

( cd "$ROOT/plugins/event-tap" && go build -o "$PLUGIN" . ) || { echo "FAIL 插件编译失败"; exit 1; }
: > "$TAP"
printf 'fn main(){}\n' > "$WS/main.rs"

( cd "$WS" && env \
    HARNESSD_BIN="$HARNESSD_BIN" HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
    CODEWHALE_HOME="$CODEWHALE_HOME" HARNESS_PROVIDER="${HARNESS_PROVIDER:-magipe}" \
    HARNESS_THREAD_BODY="${HARNESS_THREAD_BODY:-{\"auto_approve\":false}}" \
    HARNESS_PLUGIN="$PLUGIN" EVENT_TAP_LOG="$TAP" \
    TASK_PROMPT='用 bash 执行 echo tap，把输出贴出来，然后结束。' \
    bash "$ROOT/scripts/bench-solve.sh" ) > /tmp/event-tap-run.log 2>&1

got=$(sort "$TAP" 2>/dev/null | uniq)
rm -rf "$WS" || true
if [ -z "$got" ]; then
  echo "FAIL 插件一个事件都没收到（看 /tmp/event-tap-run.log）"
  exit 1
fi

missing=""
for k in turn.start tool.pre tool.post turn.end; do
  printf '%s\n' "$got" | grep -qx "$k" || missing="$missing $k"
done

echo "收到的事件：$(printf '%s' "$got" | tr '\n' ' ')"
echo "条件触发（这一轮没有发生，不是缺陷）：guard.deny（守卫拒绝时才发）、compact.pre（压缩时才发）"
if [ -n "$missing" ]; then
  echo "FAIL 常驻事件缺失：$missing"
  exit 1
fi
echo "PASS 常驻 4 事件（turn.start / tool.pre / tool.post / turn.end）在真回合里都到了"
