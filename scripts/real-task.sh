#!/usr/bin/env bash
# 真实仓库维护实测：把本仓库克隆一份到 /tmp，让 harness 在上面做一次真实改动，
# 再用 cargo test 当验收门。不是自造题——文件是真的、测试是真的、失败会真的红。
#
#   HARNESS_ENGINE_BIN=/path/to/codewhale bash scripts/real-task.sh [克隆目录]
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="${1:-/tmp/real-repo}"
ENGINE="${HARNESS_ENGINE_BIN:-/tmp/kh/codewhale/target/debug/codewhale}"
HOME_DIR="$(mktemp -d /tmp/cw-real.XXXXXX)"

cat > "$HOME_DIR/config.toml" <<'TOML'
route_preferences_version = 1
provider = "magipe"

[providers.magipe]
kind = "openai-compatible"
model = "auto/deepseek-v4.1-flash"
api_key = "local-proxy"
base_url = "http://127.0.0.1:3425/v1"
context_window = 256000
TOML

rm -rf "$REPO" && git clone -q --local "$ROOT" "$REPO"
cd "$REPO"

run_task() {
  local prompt="$1"
  rm -rf .harness
  env HARNESS_THREAD_BODY='{"auto_approve":true,"model_provider_id":"magipe","model":"auto/deepseek-v4.1-flash"}' \
      TASK_PROMPT="$prompt" \
      HARNESS_PROVIDER=magipe \
      HARNESSD_BIN="$ROOT/target/debug/harnessd" \
      HARNESS_ENGINE_BIN="$ENGINE" \
      CODEWHALE_HOME="$HOME_DIR" \
      "$ROOT/scripts/bench-solve.sh"
}

echo "== 任务 A：单文件加一个方法 + 测试 =="
run_task "在 crates/guards/src/lib.rs 里给 GuardSet 加一个 describe(&self) -> Vec<String>，返回每一层守卫的名字（先读这个文件，按现有层级顺序）。同时补一个 #[test]，断言返回三项且包含 no_progress 和 read_only_spiral。最后运行 cargo test -p harness-guards 确认通过。"
git diff --stat
cargo test -p harness-guards 2>&1 | grep -E "^test result" | head -3

git checkout -q -- . && git stash -q 2>/dev/null

echo
echo "== 任务 B：跨 crate 接线 + 全量测试 =="
run_task "crates/guards 的 GuardSet 如果没有 describe() 就先加；然后在 crates/cli 里找一处列状态/自检的命令，把 describe() 接进去（先读这两个文件再改）。改完运行 cargo test --workspace，并把失败的测试修到通过。"
git diff --stat
cargo test --workspace 2>&1 | grep -E "^test result" | awk '{p+=$4;f+=$6} END {print "workspace:",p" passed, "f" failed"}'

echo
echo "== 指标（从 timebus 算，不看模型自述） =="
python3 - .harness/timebus.ndjson <<'PY'
import json,sys
calls=[]
for line in open(sys.argv[1]):
    try: d=json.loads(line)
    except: continue
    pl=d.get('payload') or {}
    if d.get('kind')=='tool.call': calls.append((pl.get('tool'),pl.get('repeat')))
    elif d.get('kind')=='guard.deny': pass
repeats=sum(1 for _,r in calls if r)
print(json.dumps({'tool_calls':len(calls),'repeats':repeats,
                  'rework_rate':round(repeats/max(len(calls),1),3)}, ensure_ascii=False))
PY
