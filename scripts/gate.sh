#!/usr/bin/env bash
# 回归门禁：把"不反复返工"变成一条会红的命令。
#
#   bash scripts/gate.sh                 # 用现有基线 JSON 判（不花 token）
#   bash scripts/gate.sh --run '<命令>'   # 先重跑基线再判
#
# 阈值按第一次真模型基线的实测值定（rework 1.3% / 误杀 0 / 通过 1.0），
# 留了一倍余量给模型波动；超过就红，不靠人看数字。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
BASELINE="${BASELINE:-bench/baseline-deepseek.json}"

# D4 红线：任何 crate 依赖引擎、或 path 依赖逃出仓库 → 直接挡。
if [ -x scripts/check-engine.sh ]; then
  bash scripts/check-engine.sh || { echo "门禁未通过：D4 检查红了"; exit 1; }
fi

# S0–S5 验收矩阵。默认跑；`SKIP=1` 可跳（比如 CI 上没有引擎二进制）。
# 跳了要说出来——静默跳过就是又一个"永远绿"。
if [ "${SKIP_ACCEPTANCE:-0}" != "1" ] && [ -x scripts/acceptance.sh ]; then
  if ! ACCEPTANCE_FAST="${ACCEPTANCE_FAST:-1}" bash scripts/acceptance.sh >/tmp/acceptance.out 2>&1; then
    echo "门禁未通过：验收矩阵有红"
    grep -E "^## !!|^- " /tmp/acceptance.out | head -10
    exit 1
  fi
elif [ "${SKIP_ACCEPTANCE:-0}" = "1" ]; then
  echo "注意：SKIP_ACCEPTANCE=1，S0–S5 验收矩阵这一轮没跑。"
fi

# 变异扫描：把内核改坏，看测试抓不抓得住。
# 默认**跳过**（一轮约 100 秒，不适合每次提交都跑），
# 但跳过要说出来——静默跳过就是又一个"永远绿"。
if [ "${MUTATE:-0}" = "1" ]; then
  if ! cargo run -q -p xtask -- mutate >/tmp/mutate.out 2>&1; then
    echo "门禁未通过：有变异体存活（测试抓不住那些改动）"
    grep -E "^## !!|^- " /tmp/mutate.out | head -6
    exit 1
  fi
  echo "变异扫描：$(grep -c '| KILLED |' /tmp/mutate.out) 个变异体全部被抓"
else
  echo "注意：MUTATE 未设，变异扫描这一轮没跑（想跑：MUTATE=1 bash scripts/gate.sh）。"
fi

if [ "${1:-}" = "--run" ]; then
  shift
  if [ $# -eq 0 ]; then echo "用法: bash scripts/gate.sh --run '<solver 命令>'"; exit 2; fi
  cargo run -q -p xtask -- bench --solver "$*" --out "$BASELINE" || exit 1
fi

[ -f "$BASELINE" ] || { echo "找不到基线：$BASELINE"; exit 2; }

python3 - "$BASELINE" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
s = d.get("summary", d)
# 阈值：返工率 > 5%、误杀率 > 0、通过率 < 0.95 任一触发即失败。
checks = [
    ("pass_rate",      s.get("pass_rate"),      lambda v: v >= 0.95, "通过率 >= 0.95"),
    ("rework_rate",    s.get("rework_rate"),    lambda v: v <= 0.05, "返工率 <= 0.05"),
    ("false_kill_rate",s.get("false_kill_rate"),lambda v: v == 0.0,  "误杀率 == 0"),
    # solver 自己挂掉时，通过率也接近 0——但那不说明模型/内核退化。
    # 基线里必须明确"没有 solver 失败"，否则这个基线不能当门禁的输入。
    ("solver_failures",s.get("solver_failures"),lambda v: v == 0, "solver 失败数 == 0"),
]
bad = []
for name, value, ok, text in checks:
    if value is None:
        bad.append(f"{name}: 缺失（基线里没有这个指标）")
        continue
    mark = "ok " if ok(value) else "RED"
    print(f"{mark} {text}  实测 {value}")
    if not ok(value):
        bad.append(f"{text}（实测 {value}）")
if bad:
    print("\n门禁未通过：")
    for b in bad:
        print(" -", b)
    sys.exit(1)
print("\n门禁通过")
PY
