#!/usr/bin/env bash
# D4（改引擎源码）是红线的**编译期等价物**：只要工作区里没有任何 crate 依赖引擎，
# 就不可能改到它的源码——不是靠纪律，是靠 Cargo 解析不到。
#
# 这个脚本守两件事：
#   1. 没有 Cargo.toml 把引擎（codewhale / deepseek-engine / app-server 之类）当依赖；
#   2. 没有 path 依赖指向本仓库之外（那是最容易"顺手"把引擎链接进来的方式，也最容易
#      在别的机器上编不过）。
#
# 如果哪天真的改成 vendored 模式，它会退化成"钉住 commit + 工作区必须干净"的检查。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="${1:-$ROOT/vendor/codewhale}"
PIN_FILE="$ROOT/vendor/ENGINE_PIN"
fail=0

# 1) 任何 Cargo.toml 里都不该出现引擎的 crate 名。
hits=$(grep -rn --include=Cargo.toml -E '(codewhale|app-server-protocol|app_server_protocol)' "$ROOT" \
        --exclude-dir=target --exclude-dir=.git 2>/dev/null | grep -v '^Binary' || true)
if [ -n "$hits" ]; then
  echo "FAIL 工作区里有 Cargo.toml 提到了引擎 crate（D4 的第一个口子）："
  echo "$hits"
  fail=1
fi

# 2) 没有 path 依赖逃出仓库。
escaped=$(python3 - "$ROOT" <<'PY'
import os, re, sys
root = os.path.realpath(sys.argv[1])
bad = []
for dirpath, dirnames, filenames in os.walk(root):
    dirnames[:] = [d for d in dirnames if d not in ("target", ".git", "node_modules")]
    if "Cargo.toml" not in filenames:
        continue
    cargo = os.path.join(dirpath, "Cargo.toml")
    for i, line in enumerate(open(cargo, encoding="utf-8", errors="replace"), 1):
        m = re.search(r'path\s*=\s*"([^"]+)"', line)
        if not m:
            continue
        target = os.path.realpath(os.path.join(dirpath, m.group(1)))
        if not (target == root or target.startswith(root + os.sep)):
            bad.append(f"{os.path.relpath(cargo, root)}:{i} → {target}")
print("\n".join(bad))
PY
)
if [ -n "$escaped" ]; then
  echo "FAIL 有 path 依赖指向仓库之外（很容易把引擎顺手链接进来）："
  echo "$escaped"
  fail=1
fi

# 3) 如果以后真的 vendored 了，退化成钉住检查。
if [ -d "$DIR/.git" ]; then
  if [ ! -f "$PIN_FILE" ]; then
    echo "FAIL 引擎被 vendored 了，但没有 $PIN_FILE"; fail=1
  else
    want=$(tr -d '[:space:]' < "$PIN_FILE")
    have=$(git -C "$DIR" rev-parse HEAD)
    if [ "$want" != "$have" ]; then
      echo "FAIL 引擎 commit 与 pin 不符：want $want, have $have"; fail=1
    fi
    if [ -n "$(git -C "$DIR" status --porcelain)" ]; then
      echo "FAIL 引擎工作区有本地改动（D4）："; git -C "$DIR" status --short; fail=1
    fi
  fi
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "→ 引擎只能作为**进程**消费（JSON-RPC / HTTP）。见 docs/KERNEL-FUSION-PLAN.md §1 与 §7。"
  exit 1
fi

echo "ok 没有任何 crate 依赖引擎（进程依赖成立）；path 依赖都在仓库内"
