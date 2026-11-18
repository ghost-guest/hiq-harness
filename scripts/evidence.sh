#!/usr/bin/env bash
# 一条命令产出「S0–S5 验收证据」。跑完 stdout 就是一份可以直接贴进 issue 的 Markdown。
# 依赖：cargo、python3、curl；引擎二进制（外部，D1 进程依赖）用 HARNESS_ENGINE_BIN 指定。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ENGINE_BIN="${HARNESS_ENGINE_BIN:-/tmp/kh/codewhale/target/debug/codewhale}"
HOME_DIR="$(mktemp -d /tmp/evidence-home.XXXXXX)"
WORK="$(mktemp -d /tmp/evidence-ws.XXXXXX)"
# 挑一个真的空闲端口：端口上残留着上一轮的引擎时，新会话会被判成
# "selected workspace identity changed"（409），看起来像内核的错。
pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PY
}
PORT="${EVIDENCE_PORT:-$(pick_port)}"

say() { printf '%s\n' "$*"; }
code_block() { say '```'; "$@" 2>&1; say '```'; }

say "# S0–S5 验收证据"
say
say "- 生成时间：$(date '+%Y-%m-%d %H:%M:%S %Z')"
say "- 提交：\`$(git rev-parse --short HEAD)\` ($(git log -1 --pretty=%s))"
say "- 引擎：\`$ENGINE_BIN\`（外部二进制，D1 进程依赖）"
say

say "## 1. 测试"
say
say '| 套件 | 结果 |'
say '|---|---|'
ws=$(cargo test --workspace 2>&1 | grep -E "^test result" | awk '{p+=$4;f+=$6} END {print p" passed / "f" failed"}')
say "| \`cargo test --workspace\`（内核 9 个 crate） | $ws |"
rpc=$(cd apps/orbit && cargo test -p orbit-rpc 2>&1 | grep -E "^test result" | awk '{p+=$4;f+=$6} END {print p" passed / "f" failed"}')
say "| \`cargo test -p orbit-rpc\`（接缝① 客户端） | $rpc |"
pi=$(cd apps/orbit && cargo test -p orbit-pi 2>&1 | grep -E "^test result" | tail -1 | sed -E 's/^test result: (FAILED|ok)\. //; s/; [0-9]+ (ignored|measured)[^;]*//g')
say "| \`cargo test -p orbit-pi\`（GPUI 界面，无头） | $pi |"
say

say "## 2. 基准自检（不花 token，不碰网络）"
say
code_block python3 bench/run.py --selftest

say "## 3. 真模型基线（DeepSeek V4.1 Flash，经本机 3425 代理）"
say
if [ -f bench/baseline-deepseek.json ]; then
  code_block python3 -c "import json;d=json.load(open('bench/baseline-deepseek.json'));d=d.get('summary',d);print(json.dumps({k:d[k] for k in ('total','passed','pass_rate','rework_rate','false_kill_rate','cache_hit_rate','mean_duration_s')}, indent=2))"
else
  say "（未找到 bench/baseline-deepseek.json）"
fi

say "## 4. 引擎实况（现起现停，不依赖历史）"
say
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
( cd "$WORK" && CODEWHALE_HOME="$HOME_DIR" "$ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" >"$HOME_DIR/engine.log" 2>&1 & echo $! > "$HOME_DIR/pid" )
for _ in $(seq 1 40); do curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
say '### 4.1 健康检查'
code_block sh -c "curl -s http://127.0.0.1:$PORT/healthz; echo"

BODY='{"auto_approve":false,"model_provider_id":"magipe","model":"auto/deepseek-v4.1-flash"}'
TID=$(curl -s -m 10 -X POST "http://127.0.0.1:$PORT/v1/threads" -H 'content-type: application/json' -d "$BODY" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))' 2>/dev/null)
say '### 4.2 建会话'
code_block sh -c "curl -s -m 10 -X POST http://127.0.0.1:$PORT/v1/threads -H 'content-type: application/json' -d '$BODY' | head -c 300"

mkthread() { curl -s -m 10 -X POST "http://127.0.0.1:$PORT/v1/threads" -H 'content-type: application/json' -d "$BODY" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))' 2>/dev/null; }

say '### 4.3 档位（合法值被接受，非法值被拒绝并列出**全部**取值）'
EFF_TID=$(mkthread)
code_block sh -c "curl -s -o /dev/null -w 'reasoning_effort=low   -> %{http_code}\n' -X POST http://127.0.0.1:$PORT/v1/threads/$EFF_TID/turns -H 'content-type: application/json' -d '{\"prompt\":\"say ok\",\"reasoning_effort\":\"low\"}'"
code_block sh -c "curl -s -X POST http://127.0.0.1:$PORT/v1/threads/$EFF_TID/turns -H 'content-type: application/json' -d '{\"prompt\":\"say ok\",\"reasoning_effort\":\"bogus\"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"error\"][\"message\"])'"

say '### 4.4 事件流里真的有工具调用和用量'
TURN_TID=$(mkthread)
( curl -s -N -m 90 "http://127.0.0.1:$PORT/v1/threads/$TURN_TID/events" > "$HOME_DIR/sse.ndjson" & )
sleep 1
curl -s -m 10 -X POST "http://127.0.0.1:$PORT/v1/threads/$TURN_TID/turns" -H 'content-type: application/json' -d '{"prompt":"用 bash 工具执行 echo evidence，把输出贴出来，然后结束。"}' >/dev/null
for _ in $(seq 1 60); do grep -q '"turn.completed"' "$HOME_DIR/sse.ndjson" 2>/dev/null && break; sleep 1; done
code_block python3 - "$HOME_DIR/sse.ndjson" <<'PY'
import json,sys,collections
c=collections.Counter(); tools=[]; usage=None
for line in open(sys.argv[1]):
    if not line.startswith('data: '): continue
    d=json.loads(line[6:]); c[d.get('kind')]+=1
    p=d.get('payload') or {}
    it=p.get('item') or {}
    m=it.get('metadata') or {}
    if it.get('kind')=='tool_call' and m.get('tool_name'):
        tools.append(m['tool_name'])
    if p.get('usage'): usage=p['usage']
print('事件种类:', {k:v for k,v in c.items() if v})
print('工具调用:', tools or '(无)')
print('用量:', json.dumps(usage, ensure_ascii=False) if usage else '(无)')
PY

say '### 4.5 fork 出新会话（接在一个已完成轮次的会话上——空会话没有可分的枝）'
code_block sh -c "curl -s -m 15 -X POST http://127.0.0.1:$PORT/v1/threads/$TURN_TID/fork -H 'content-type: application/json' -d '{}' -w '\n-> %{http_code}\n' | tail -c 400"

kill "$(cat "$HOME_DIR/pid")" 2>/dev/null
pkill -f "app-server --http --insecure-no-auth --port $PORT" 2>/dev/null
wait 2>/dev/null
rm -rf "$HOME_DIR" "$WORK" 2>/dev/null || true
say
say "> 上面每一条都是刚刚现场跑出来的；没有任何一步读仓库里的历史产物（基准 JSON 除外，它是跑分留下的事实记录）。"
