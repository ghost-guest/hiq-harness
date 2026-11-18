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

if [ "${1:-}" = "--run" ]; then
  shift
  if [ $# -eq 0 ]; then echo "用法: bash scripts/gate.sh --run '<solver 命令>'"; exit 2; fi
  python3 bench/run.py --solver "$*" --out "$BASELINE" || exit 1
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
