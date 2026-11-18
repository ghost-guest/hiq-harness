#!/usr/bin/env bash
# 把回归门禁装成 pre-push 钩子：基线里返工率/误杀率/通过率不达标就推不上去。
# 只读基线 JSON，不花 token、不联网，所以每次 push 都能跑。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK="$ROOT/.git/hooks/pre-push"
cat > "$HOOK" <<'HOOKBODY'
#!/usr/bin/env bash
# 由 scripts/install-hooks.sh 生成。
root="$(git rev-parse --show-toplevel)"
if [ -x "$root/scripts/gate.sh" ]; then
  bash "$root/scripts/gate.sh" || {
    echo
    echo "推送被拦：基线不达标。想看细节就跑 bash scripts/gate.sh；"
    echo "确认要推（例如基线正在重跑）可以用 git push --no-verify。"
    exit 1
  }
fi
HOOKBODY
chmod +x "$HOOK"
echo "已安装 $HOOK"
