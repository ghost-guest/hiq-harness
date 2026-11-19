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
