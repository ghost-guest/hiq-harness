#!/usr/bin/env bash
# S2 原文："fork 出新会话能跑到不同结论"。
# probe-fork.sh 只验了"新线程上回合能闭合"，没验**两条会话真的分叉**。
#
# 真分叉的判据不是"id 不一样"，而是**数据隔离**：
#   - 在 fork 出来的新线程上发一条消息，它绝不能出现在原线程的历史里。
#   - 新线程应当继承 fork 点之前的消息（分支共享前缀）。
# 如果 fork 只是换了个 id、底层还写同一个会话，这条会红。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# 探针跑的是**当前源码**，不是上次留下的二进制。
# 踩过：gate 和我自己的构建并发，探针拿到一个旧二进制，报\`unknown command Prompt\`——
# 看起来像内核坏了，其实只是二进制和源码不同步。
[ "${SKIP_BUILD:-0}" = "1" ] || cargo build -q -p harnessd || { echo "FAIL 编译 harnessd 失败"; exit 1; }
: "${HARNESSD_BIN:?}" "${HARNESS_ENGINE_BIN:?}" "${CODEWHALE_HOME:?}"
export CODEWHALE_HOME

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close();')
WS=$(mktemp -d /tmp/forkbr-ws.XXXXXX)
cleanup() {
  [ -n "${HP:-}" ] && kill "$HP" 2>/dev/null
  pkill -f "app-server --http.*--port $PORT" 2>/dev/null
  rm -rf "$WS"
}
trap cleanup EXIT

"$HARNESS_ENGINE_BIN" app-server --http --insecure-no-auth --port "$PORT" \
  >/tmp/forkbr-engine.log 2>&1 &
up=0
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && { up=1; break; }
  sleep 0.5
done
[ "$up" = 1 ] || { echo "FAIL 引擎没起来"; exit 1; }

FIFO="$WS/in.fifo"
mkfifo "$FIFO"
( cd "$WS" && HARNESSD_THREAD_BODY='{"auto_approve":true}' \
    exec "$HARNESSD_BIN" serve --compat orbit --http-engine "http://127.0.0.1:$PORT" \
      --log ./timebus.ndjson < "$FIFO" > /tmp/forkbr-harnessd.log 2>&1 ) &
HP=$!
exec 9>"$FIFO"

turn_ends() {
  local n
  n=$(grep -c '"kind":"turn.end"' "$WS/timebus.ndjson" 2>/dev/null)
  echo "${n:-0}"
}
wait_end() {
  local want=$1 n=0
  while [ "$n" -lt 240 ]; do
    [ "$(turn_ends)" -ge "$want" ] && return 0
    sleep 0.5; n=$((n + 1))
  done
  return 1
}
send() { python3 -c 'import json,sys;print(json.dumps(json.loads(sys.argv[1])))' "$1" >&9; }

MA="BRANCH_A_$RANDOM"
MB="BRANCH_B_$RANDOM"

# 1) 原线程跑完一回合（fork 要求已完成回合）
send "{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"只回复这个词，不要解释：$MA\"}"
wait_end 1 || { echo "FAIL 前置回合没闭合"; exit 1; }

# 2) 取原线程 id
send '{"id":"st1","type":"get_state"}'
sleep 1
OLD=$(python3 - <<'PY'
import json
old=''
for l in open('/tmp/forkbr-harnessd.log'):
    try: d=json.loads(l)
    except: continue
    if d.get('id')=='st1' and d.get('success'):
        data=d.get('data') or d.get('result') or d.get('reply') or {}
        t=(data or {}).get('thread') or {}
        old=t.get('id','')
print(old)
PY
)
[ -n "$OLD" ] || { echo "FAIL 没拿到原线程 id"; exit 1; }

# 3) fork（409 重试）
NEW=''
for attempt in 1 2 3 4 5; do
  send "{\"id\":\"f$attempt\",\"type\":\"fork\"}"
  sleep 2
  NEW=$(python3 - "$attempt" <<'PY'
import json,sys
att=sys.argv[1]; new=''
for l in open('/tmp/forkbr-harnessd.log'):
    try: d=json.loads(l)
    except: continue
    if d.get('id')==f'f{att}' and d.get('success'):
        data=d.get('data') or d.get('result') or {}
        # 引擎的 fork 响应是**扁平**的：data.id 直接就是新线程 id。
        # 以前这里找 data.thread.id，找不到就当"fork 没返回"，白 fork 了 5 次。
        cand=((data.get('thread') or {}).get('id')) or data.get('threadId') or data.get('id') or ''
        new=cand if str(cand).startswith('thr_') else ''
print(new)
PY
)
  [ -n "$NEW" ] && break
done
[ -n "$NEW" ] || { echo "FAIL fork 没返回新线程"; exit 1; }
[ "$NEW" != "$OLD" ] || { echo "FAIL fork 前后 id 一样：$OLD"; exit 1; }

# 4) 在**新线程**上发一条不同的消息
send "{\"id\":\"p2\",\"type\":\"prompt\",\"message\":\"只回复这个词，不要解释：$MB\"}"
wait_end 2 || { echo "FAIL 新线程回合没闭合"; exit 1; }

# 5) 读新线程历史：应含 MB（也应继承 MA 前缀）
send '{"id":"e1","type":"get_entries"}'
sleep 1
# 6) 切回原线程，读它的历史
send "{\"id\":\"sw1\",\"type\":\"switch_session\",\"sessionId\":\"$OLD\"}"
sleep 2
send '{"id":"e2","type":"get_entries"}'
sleep 2

exec 9>&-
sleep 1
kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null || true
HP=

python3 - "$OLD" "$NEW" "$MA" "$MB" <<'PY'
import json,sys
old,new,ma,mb=sys.argv[1:5]
def entries(id_):
    last=None
    for l in open('/tmp/forkbr-harnessd.log'):
        try: d=json.loads(l)
        except: continue
        if d.get('id')==id_:
            last=d
    if not last or not last.get('success'): return None
    data=last.get('data') or last.get('result') or {}
    return json.dumps(data, ensure_ascii=False)

e_new=entries('e1')
e_old=entries('e2')
print(f"原线程 {old[:12]}… ｜ 新线程 {new[:12]}…")
problems=[]
if e_new is None: problems.append("新线程 get_entries 没回")
else:
    if mb not in e_new: problems.append("新线程历史里没有它自己的消息 MB（分支没写进去）")
if e_old is None: problems.append("切回原线程后 get_entries 没回")
else:
    if mb in e_old:
        problems.append("新线程的消息 MB 漏进了原线程历史——两条会话没真分叉")
    if ma not in e_old: problems.append("原线程历史里找不到它自己的消息 MA")
if problems:
    print("FAIL " + "；".join(problems))
    sys.exit(1)
print("PASS 分叉成立：新线程有自己的消息 MB，原线程历史干净（不含 MB）")
PY
