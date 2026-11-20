#!/usr/bin/env bash
# S0–S5 验收矩阵：一条一条跑，每条打印「命令 / 期望 / 实际 / 判定」。
#
# 为什么单独做一个脚本：证据散在 8 个地方时，"全部完成"这句话没法被检验。
# 这里每条都现场跑，不读历史产物（基线 JSON 是跑分留下的记录，算事实）。
#
# 用法：HARNESS_ENGINE_BIN=/path/to/codewhale bash scripts/acceptance.sh
# 退出码：0 = 全过（或明确 SKIP）；1 = 有红。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1
PROBLEMS=()
ROWS=()

row() { ROWS+=("$1|$2|$3|$4"); }
pass() { row "$1" "$2" "$3" "PASS"; }
fail() { row "$1" "$2" "$3" "**FAIL**"; PROBLEMS+=("$1：$3"); }
skip() { row "$1" "$2" "$3" "SKIP"; }

# 验收跑的是**当前源码**，不是上一次 build 留下的二进制。
# 踩过：改完 GetEntries 的翻译直接跑矩阵，用的是旧 harnessd，
# 矩阵红了而单跑 PASS——查了半天才发现是二进制没更新。
# 矩阵里"现场跑"的前提就是先编译现场的代码。
if ! cargo build -q -p harnessd 2>/dev/null; then
  echo "!! 编译 harnessd 失败，验收没有意义"
  exit 1
fi

# ---------- S0：进程依赖，不碰引擎源码 ----------
out=$(bash scripts/check-engine.sh 2>&1)
if printf '%s' "$out" | grep -q "^ok "; then
  pass "S0 进程依赖" "没有任何 crate 依赖引擎" "$(printf '%s' "$out" | head -1)"
else
  fail "S0 进程依赖" "没有任何 crate 依赖引擎" "$(printf '%s' "$out" | head -2 | tr '\n' ' ')"
fi

# 反方向：真加一条越界 path 依赖必须红（不能只验绿）
probe_dir="crates/__acc_probe"
mkdir -p "$probe_dir"
printf '[package]\nname="acc-probe"\nversion="0.0.0"\nedition="2021"\n[dependencies]\noutside={path="../../../acc-outside"}\n' > "$probe_dir/Cargo.toml"
printf '' > "$probe_dir/src.rs"; mkdir -p "$probe_dir/src"; printf '' > "$probe_dir/src/lib.rs"
red=$(bash scripts/check-engine.sh 2>&1)
rm -rf "$probe_dir"
if printf '%s' "$red" | grep -qE "仓库外|outside|escape"; then
  pass "S0 检查会红" "越界的 path 依赖必须被判红" "注入 ../../../acc-outside 后确实报错"
else
  fail "S0 检查会红" "越界的 path 依赖必须被判红" "注入越界依赖后仍然通过——这个检查是空的"
fi

# ---------- S1：接缝① 真引擎 ----------
# 引擎要能起，光有二进制不够——还得有它自己的 home（里面有 provider 配置）。
# 缺了就说 SKIP：这不是产品失败，是这台机器上少给了一个参数。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S1 接缝①" "流式文本 + 工具调用 + 收尾" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  s1=$(cd apps/orbit && HARNESSD_BIN="$ROOT/target/debug/harnessd" \
        HARNESSD_HTTP_ENGINE="${HARNESSD_HTTP_ENGINE:-}" \
        HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
        CODEWHALE_HOME="${CODEWHALE_HOME:-}" \
        cargo run -q -p orbit-rpc --example seam1_real_engine 2>&1)
  if printf '%s' "$s1" | grep -q "^PASS "; then
    pass "S1 接缝①" "流式文本 + 工具调用 + 收尾" "$(printf '%s' "$s1" | grep '^PASS ' | head -1)"
  else
    fail "S1 接缝①" "流式文本 + 工具调用 + 收尾" "$(printf '%s' "$s1" | tail -2 | tr '\n' ' ')"
  fi
else
  skip "S1 接缝①" "流式文本 + 工具调用 + 收尾" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 多回合：同一线程连着跑 N 轮，每轮都闭合，前缀跨轮复用。
# 变异扫描证明不了这个——"先订阅再发回合"和"turn_id 带回顶层"
# 都只在真引擎上才看得出来（假象：第 1 轮好，第 2 轮被误判成"被拒"）。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S1 多回合" "同一线程连续回合，每轮闭合、前缀复用" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  mt=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" ROUNDS=3 \
       bash scripts/probe-multiturn.sh 2>&1)
  if printf '%s' "$mt" | grep -q "^PASS "; then
    pass "S1 多回合" "同一线程连续回合，每轮闭合、前缀复用" "$(printf '%s' "$mt" | grep '^PASS ' | head -1)"
  else
    fail "S1 多回合" "同一线程连续回合，每轮闭合、前缀复用" "$(printf '%s' "$mt" | tail -3 | tr '\n' ' ')"
  fi
else
  skip "S1 多回合" "同一线程连续回合，每轮闭合、前缀复用" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# GetEntries：跑完回合后界面能不能读回历史。S2 的验收条件之一，
# 之前矩阵里完全没有。真引擎上引擎返回 items、orbit 读 entries。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S2 会话历史" "GetEntries 在真引擎上读回非空历史" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  hi=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-history.sh 2>&1)
  if printf '%s' "$hi" | grep -q "^PASS "; then
    pass "S2 会话历史" "GetEntries 在真引擎上读回非空历史" "$(printf '%s' "$hi" | grep '^PASS ' | head -1)"
  else
    fail "S2 会话历史" "GetEntries 在真引擎上读回非空历史" "$(printf '%s' "$hi" | tail -3 | tr '\n' ' ')"
  fi
else
  skip "S2 会话历史" "GetEntries 在真引擎上读回非空历史" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# steer：验引擎 API 接受插话 + harnessd 的 body 翻译正确。
# 中途插话的完整链路另有 S3 行（probe-steer-midturn.sh）验证。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S2 steer" "引擎接受插话 + body 方言翻译正确" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  st=$(env HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-steer.sh 2>&1)
  if printf '%s' "$st" | grep -q "^PASS "; then
    pass "S2 steer" "引擎接受插话 + body 方言翻译正确" "$(printf '%s' "$st" | grep -E '^引擎 steer|^PASS ' | grep -v '^注：' | head -2 | tr '\n' ' ')"
  else
    fail "S2 steer" "引擎接受插话 + body 方言翻译正确" "$(printf '%s' "$st" | tail -3 | tr '\n' ' ')"
  fi
else
  skip "S2 steer" "引擎接受插话 + body 方言翻译正确" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 回合中发的非 steer 命令不丢、顺序不乱。stdin 拆线程后新开的路，
# 红方向验过：删掉 deferred 重放，两条命令都被吞。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S3 deferred 命令" "回合中非 steer 命令不丢、顺序不乱" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  df=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-deferred.sh 2>&1)
  if printf '%s' "$df" | grep -q "^PASS "; then
    pass "S3 deferred 命令" "回合中非 steer 命令不丢、顺序不乱" "$(printf '%s' "$df" | grep '^PASS ' | head -1)"
  else
    fail "S3 deferred 命令" "回合中非 steer 命令不丢、顺序不乱" "$(printf '%s' "$df" | tail -3 | tr '\n' ' ')"
  fi
else
  skip "S3 deferred 命令" "回合中非 steer 命令不丢、顺序不乱" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 跨语言任务：让模型用 Rust 和 Go 各写一份快排并各跑一遍。
# 这条同时也验"建线程带 workspace"——没带的话文件会落到启动引擎的目录里。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S1 跨语言任务" "Rust 与 Go 各写一份快排，都跑通、有结论" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  ml=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-multilang.sh 2>&1)
  if printf '%s' "$ml" | grep -q "^PASS "; then
    pass "S1 跨语言任务" "Rust 与 Go 各写一份快排，都跑通、有结论" "$(printf '%s' "$ml" | grep -E '工具调用' | head -1)"
  else
    fail "S1 跨语言任务" "Rust 与 Go 各写一份快排，都跑通、有结论" "$(printf '%s' "$ml" | grep -E '^FAIL|工具调用' | tail -1)"
  fi
else
  skip "S1 跨语言任务" "Rust 与 Go 各写一份快排，都跑通、有结论" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 同一条跨语言题面，但走**界面自己的客户端**（orbit-rpc 的 PiClient），
# 不是我的探针直连 harnessd。这条证明"桌面版这条路"能跑重任务。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S1 跨语言（界面客户端）" "orbit 客户端跑跨语言题，流式+工具+收尾都在" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  se=$(cd apps/orbit && env HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       HARNESSD_BIN="$ROOT/target/debug/harnessd" CODEWHALE_HOME="$CODEWHALE_HOME" \
       SEAM1_PROMPT='用 Rust 和 Go 各写一个快速排序放到同一目录，各跑一遍，确认两边对同一组输入输出一致，然后一句话给结论。' \
       cargo run -q -p orbit-rpc --example seam1_real_engine 2>&1 | tail -6)
  if printf '%s' "$se" | grep -q "^PASS "; then
    pass "S1 跨语言（界面客户端）" "orbit 客户端跑跨语言题，流式+工具+收尾都在" "$(printf '%s' "$se" | grep -E '文本长度|工具调用' | tr '\n' ' ' | cut -c1-90)"
  else
    fail "S1 跨语言（界面客户端）" "orbit 客户端跑跨语言题，流式+工具+收尾都在" "$(printf '%s' "$se" | tail -2 | tr '\n' ' ' | cut -c1-140)"
  fi
else
  skip "S1 跨语言（界面客户端）" "orbit 客户端跑跨语言题，流式+工具+收尾都在" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 界面一打开就调 get_commands。引擎没有 prompt/capabilities 端点 → 探测失败
# → 能力集为空 → has() 把空集当"什么都能做" → 去调不存在的方法 → 界面红条。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S4 能力探测" "get_commands 成功（能力集收敛到可路由方法）" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  cp_=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-capabilities.sh 2>&1)
  if printf '%s' "$cp_" | grep -q "^PASS "; then
    pass "S4 能力探测" "get_commands 成功（能力集收敛到可路由方法）" "$(printf '%s' "$cp_" | grep '^PASS ' | head -1)"
  else
    fail "S4 能力探测" "get_commands 成功（能力集收敛到可路由方法）" "$(printf '%s' "$cp_" | grep -E '^FAIL' | head -1)"
  fi
else
  skip "S4 能力探测" "get_commands 成功（能力集收敛到可路由方法）" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 用户实测：第一回合答完后追问一句，界面一直转圈。
# 根因：回合结束只发 agent_end，界面等的 agent_settled 从来没发过。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S3 第二回合" "同线程追问能闭合、能答、且发 agent_settled" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  st=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-second-turn.sh 2>&1)
  if printf '%s' "$st" | grep -q "^PASS "; then
    pass "S3 第二回合" "同线程追问能闭合、能答、且发 agent_settled" "$(printf '%s' "$st" | grep '^PASS ' | head -1)"
  else
    fail "S3 第二回合" "同线程追问能闭合、能答、且发 agent_settled" "$(printf '%s' "$st" | grep -E '^FAIL|界面序列' | head -2 | tr '\n' ' ')"
  fi
else
  skip "S3 第二回合" "同线程追问能闭合、能答、且发 agent_settled" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 用户实测：问一句"你的内核是什么"，跑了一个命令，然后没有下文。
# 根因是日志只认 item.type，HTTP/SSE 用的是 item.kind —— 每条 assistant 回答
# 都没进日志；不流式的 provider 更是界面一个字都没有。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S1 工具后收尾" "工具跑完模型必须给出最终回答（进日志）" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  fu=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-tool-followup.sh 2>&1)
  if printf '%s' "$fu" | grep -q "^PASS "; then
    pass "S1 工具后收尾" "工具跑完模型必须给出最终回答（进日志）" "$(printf '%s' "$fu" | grep '^PASS ' | head -1)"
  elif printf '%s' "$fu" | grep -q "^SKIP "; then
    skip "S1 工具后收尾" "工具跑完模型必须给出最终回答（进日志）" "$(printf '%s' "$fu" | grep '^SKIP ' | head -1)"
  else
    fail "S1 工具后收尾" "工具跑完模型必须给出最终回答（进日志）" "$(printf '%s' "$fu" | grep -E '^FAIL|时间线' | head -2 | tr '\n' ' ')"
  fi
else
  skip "S1 工具后收尾" "工具跑完模型必须给出最终回答（进日志）" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# S2 原文："fork 出新会话能跑到不同结论"。probe-fork 只验了"新线程回合能闭合"，
# 没验两条会话真的分叉。这条验数据隔离：新线程的消息不能漏回原线程历史。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S2 fork 真分叉" "新线程的消息不漏进原线程历史" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  fb=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-fork-branch.sh 2>&1)
  if printf '%s' "$fb" | grep -q "^PASS "; then
    pass "S2 fork 真分叉" "新线程的消息不漏进原线程历史" "$(printf '%s' "$fb" | grep '^PASS ' | head -1)"
  else
    fail "S2 fork 真分叉" "新线程的消息不漏进原线程历史" "$(printf '%s' "$fb" | grep -E '^FAIL|^原线程' | head -2 | tr '\n' ' ')"
  fi
else
  skip "S2 fork 真分叉" "新线程的消息不漏进原线程历史" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# S3 硬指标：流式中途插话生效。走完整 orbit compat 层——
# 回合进行中从 stdin 发 steer，验证它进 SteerGate、被安全点释放、
# 最终模型输出里出现插话标记。之前这条是明确未达成的缺口，现已打通。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S3 中途插话" "回合进行中插话，模型输出含插话标记" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  sm=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" bash scripts/probe-steer-midturn.sh 2>&1)
  if printf '%s' "$sm" | grep -q "^PASS "; then
    pass "S3 中途插话" "回合进行中插话，模型输出含插话标记" "$(printf '%s' "$sm" | grep '^PASS ' | head -1)"
  else
    fail "S3 中途插话" "回合进行中插话，模型输出含插话标记" "$(printf '%s' "$sm" | grep -E '回合 start|  - ' | head -3 | tr '\n' ' ')"
  fi
else
  skip "S3 中途插话" "回合进行中插话，模型输出含插话标记" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# fork 之后新线程还能跑完回合。单元测试只验了"id 换了"，
# 没验 SSE 订阅也跟着换——真引擎上那会让回合静默挂住。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S2 fork 后回合" "fork 出新线程，新线程上回合闭合" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  fk=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" \
       bash scripts/probe-fork.sh 2>&1)
  if printf '%s' "$fk" | grep -q "^PASS "; then
    pass "S2 fork 后回合" "fork 出新线程，新线程上回合闭合" "$(printf '%s' "$fk" | grep '^PASS ' | head -1)"
  else
    fail "S2 fork 后回合" "fork 出新线程，新线程上回合闭合" "$(printf '%s' "$fk" | tail -3 | tr '\n' ' ')"
  fi
else
  skip "S2 fork 后回合" "fork 出新线程，新线程上回合闭合" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 带审批的多回合：上一条用 auto_approve:true，关掉了守卫/审批路径。
# 这条用 false，真跑工具，确认守卫路径跨回合没死。
if [ "${ACCEPTANCE_FAST:-0}" = "1" ]; then
  skip "S3 守卫跨回合" "auto_approve=false 下每轮工具真跑、回合闭合" "ACCEPTANCE_FAST=1：真引擎项这一轮没跑"
elif [ -n "${HARNESS_ENGINE_BIN:-}" ] && [ -n "${CODEWHALE_HOME:-}" ]; then
  gt=$(env HARNESSD_BIN="$ROOT/target/debug/harnessd" \
       HARNESS_ENGINE_BIN="$HARNESS_ENGINE_BIN" \
       CODEWHALE_HOME="$CODEWHALE_HOME" ROUNDS=2 \
       bash scripts/probe-guarded-turns.sh 2>&1)
  if printf '%s' "$gt" | grep -q "^PASS "; then
    pass "S3 守卫跨回合" "auto_approve=false 下每轮工具真跑、回合闭合" "$(printf '%s' "$gt" | grep '^PASS ' | head -1)"
  else
    fail "S3 守卫跨回合" "auto_approve=false 下每轮工具真跑、回合闭合" "$(printf '%s' "$gt" | tail -3 | tr '\n' ' ')"
  fi
else
  skip "S3 守卫跨回合" "auto_approve=false 下每轮工具真跑、回合闭合" "缺 HARNESS_ENGINE_BIN 或 CODEWHALE_HOME"
fi

# 命令覆盖：orbit 声明的每个命令都要有映射（"能跑"不够，要"没有漏"）
cov=$(cargo test -q -p harnessd every_orbit_command_maps_to_something 2>&1)
ty=$(cargo test -q -p harnessd every_orbit_type_we_claim_is_mapped 2>&1)
if printf '%s%s' "$cov" "$ty" | grep -qE "0 failed" && \
   printf '%s' "$cov" | grep -qE "1 passed"; then
  pass "S1 命令覆盖" "orbit 声明的命令与事件都有映射" "every_orbit_command_maps_to_something + every_orbit_type_we_claim_is_mapped 都通过"
else
  fail "S1 命令覆盖" "orbit 声明的命令与事件都有映射" "$(printf '%s' "$cov" | tail -2 | tr '\n' ' ')"
fi

# ---------- S2：模型 / 档位 / fork ----------
eff=$(cargo test -q -p harnessd effort_is_sent_as_reasoning_effort 2>&1)
printf '%s' "$eff" | grep -qE "1 passed" \
  && pass "S2 档位" "effort 以 reasoning_effort 送到引擎" "effort_is_sent_as_reasoning_effort 通过" \
  || fail "S2 档位" "effort 以 reasoning_effort 送到引擎" "$(printf '%s' "$eff" | tail -2 | tr '\n' ' ')"

fk=$(cargo test -q -p harnessd fork_switches_to_the_new_thread 2>&1)
printf '%s' "$fk" | grep -qE "1 passed" \
  && pass "S2 fork" "fork 后会话切到新线程" "fork_switches_to_the_new_thread 通过" \
  || fail "S2 fork" "fork 后会话切到新线程" "$(printf '%s' "$fk" | tail -2 | tr '\n' ' ')"

# ---------- S3：守卫 / 缓存 / 时线 ----------
# 从大到大取：`tail -1` 可能拿到 0 条的空 target
g=$(cargo test -q -p harness-guards 2>&1 | grep -E "^test result" | head -1)
gn=$(printf '%s' "$g" | sed -nE 's/.* ([0-9]+) passed.*/\1/p')
if [ "${gn:-0}" -ge 10 ] && printf '%s' "$g" | grep -q "0 failed"; then
  pass "S3 守卫" "三件守卫语义正确" "${gn} 条守卫测试通过"
else
  fail "S3 守卫" "三件守卫语义正确" "${gn:-0} 条（${g}）"
fi

# 注意：`cargo test -q` 不打印 "test X ... ok" 那些行，只能从 test result 里数。
# 我第一版用 `grep -c "^test .* ok$"`，数出来恒为 0——判定写错比不判还糟。
cg=$(cargo test -q -p harnessd cacheguard 2>&1 | grep -E "^test result" | head -1)
cgn=$(printf '%s' "$cg" | sed -nE 's/.* ([0-9]+) passed.*/\1/p')
[ "${cgn:-0}" -ge 3 ] \
  && pass "S3 缓存守卫" "命中率与压力两条线都判对" "${cgn} 条缓存测试通过" \
  || fail "S3 缓存守卫" "命中率与压力两条线都判对" "只有 ${cgn:-0} 条（${cg}）"

# 回合必须闭合——这是"日志是事实"的底线
cl=$(cargo test -q -p harnessd a_failed_compaction_does_not_swallow_turn_end 2>&1)
printf '%s' "$cl" | grep -qE "1 passed" \
  && pass "S3 回合闭合" "压缩失败也要写 turn.end" "a_failed_compaction_does_not_swallow_turn_end 通过" \
  || fail "S3 回合闭合" "压缩失败也要写 turn.end" "$(printf '%s' "$cl" | tail -2 | tr '\n' ' ')"

# ---------- S4：Go 插件 ----------
if command -v go >/dev/null 2>&1; then
  p1=$(cargo test -q -p harnessd --test go_plugin 2>&1 | grep -E "^test result" | tail -1)
  p2=$(cargo test -q -p harnessd --test go_plugin_guard 2>&1 | grep -E "^test result" | tail -1)
  if printf '%s%s' "$p1" "$p2" | grep -q "FAILED\|failed; [1-9]"; then
    fail "S4 插件" "真 Go 插件能 deny 一条命令" "$p1 / $p2"
  else
    pass "S4 插件" "真 Go 插件能 deny 一条命令" "${p1}；$p2"
  fi
else
  skip "S4 插件" "真 Go 插件能 deny 一条命令" "PATH 里没有 go"
fi

# 判的是"插件测试有没有被 ignored 掉"。grep -c "ignored" 会把
# "0 ignored" 这一行也算一次，所以必须从 test result 里把数字取出来。
ev=$( { cargo test -q -p harnessd --test go_plugin 2>&1
        cargo test -q -p harnessd --test go_plugin_guard 2>&1; } \
      | grep -E "^test result" \
      | sed -nE 's/.* ([0-9]+) ignored.*/\1/p' | paste -sd+ - | bc 2>/dev/null)
ev=${ev:-0}
[ "$ev" -eq 0 ] \
  && pass "S4 不空转" "没有 go 时必须红，不能静默跳过" "插件测试 ignored=0" \
  || fail "S4 不空转" "没有 go 时必须红，不能静默跳过" "有 ${ev} 条被 ignored——静默跳过回来了"

# ---------- S5：评测与门禁 ----------
# 三份基线各自该有什么，明写。local-model 是本地小模型跑的对照，
# 当时没记 rework/false-kill——这是**已知的记录缺口**，不是缺陷，
# 所以它只要求有 pass_rate 和 solver_failures，其余两个如实标出。
basecheck=$(python3 - <<'PY'
import json
req = {"deepseek": ("pass_rate","rework_rate","false_kill_rate","solver_failures"),
       "scripted": ("pass_rate","rework_rate","false_kill_rate","solver_failures"),
       "local-model": ("pass_rate","solver_failures")}
bad, notes = [], []
for name, need in req.items():
    try:
        s = json.load(open(f"bench/baseline-{name}.json"))["summary"]
    except Exception as e:
        bad.append(f"{name}: 读不到 ({e})"); continue
    miss = [k for k in need if k not in s]
    if miss: bad.append(f"{name}: 缺 {miss}")
    else: notes.append(name)
print("OK" if not bad else "BAD " + "; ".join(bad))
PY
)
# 基线数字用 python 读出来再判。**不要**写成 `python3 -c "...f\"...\"..."`：
# 嵌套引号在 bash 里会被拆坏，python 收到半句就等 stdin，整条脚本静默挂住
# （我踩过：脚本跑了 400 秒没有任何输出，看起来像"慢"，其实是卡在输入上）。
bl=$(python3 - <<'PY'
import json
s = json.load(open("bench/baseline-deepseek.json"))["summary"]
print("pass {} / rework {} / false-kill {} / solver_fail {} / cache {}".format(
    s["pass_rate"], s["rework_rate"], s["false_kill_rate"],
    s["solver_failures"], s["cache_hit_rate"]))
PY
)
if python3 - <<'PY'
import json, sys
s = json.load(open("bench/baseline-deepseek.json"))["summary"]
ok = (s["pass_rate"] >= 0.95 and s["rework_rate"] <= 0.05
      and s["false_kill_rate"] == 0.0 and s["solver_failures"] == 0)
sys.exit(0 if ok else 1)
PY
then
  pass "S5 基线" "通过率/返工率/误杀率有基线数字" "$bl"
else
  fail "S5 基线" "通过率/返工率/误杀率有基线数字" "$bl"
fi

# 必须 `SKIP_ACCEPTANCE=1`：gate.sh 会反过来调本脚本。
# 不加这一句就是 gate → acceptance → gate → … 无限递归，
# 表现为"脚本跑了几百秒、输出 0 行"——很难查，因为看起来像慢，其实是自我 fork。
gate=$(SKIP_ACCEPTANCE=1 bash scripts/gate.sh 2>&1)
printf '%s' "$gate" | grep -q "门禁通过" \
  && pass "S5 门禁" "四条门禁同时成立" "门禁通过" \
  || fail "S5 门禁" "四条门禁同时成立" "$(printf '%s' "$gate" | tail -3 | tr '\n' ' ')"

# ---------- 输出 ----------
echo "| 验收项 | 期望 | 实际 | 判定 |"
echo "|---|---|---|---|"
for r in "${ROWS[@]}"; do echo "| ${r//|/ | } |"; done
echo
if [ ${#PROBLEMS[@]} -gt 0 ]; then
  echo "## !! 有 ${#PROBLEMS[@]} 条没成立"
  for p in "${PROBLEMS[@]}"; do echo "- $p"; done
  exit 1
fi
echo "全部成立（SKIP 的不算成立，但也不算失败——它明说了自己没验）。"
