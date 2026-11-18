#!/usr/bin/env python3
"""真引擎的审批通道探针：起引擎 → 建线程（auto_approve=false）→ 起轮 →
等到 approval.required → 用事件里的 id 答 deny → 断言引擎 200 且这一轮正常收尾。

为什么要有这个脚本：这条通道曾经整条是坏的（id 硬编码 0、命令没摊平、
词表抄错、重复投递），而所有基线都跑在 auto_approve=true 上，所以没人发现。
坏掉的时候内核不会报"审批通道坏了"，它报的是 turn 死了。

用法：
  HARNESS_ENGINE_BIN=/path/to/codewhale \
  CODEWHALE_HOME=/tmp/some-clean-home \
  python3 scripts/probe-approval.py
"""
import json, os, shutil, socket, subprocess, sys, tempfile, threading, time, urllib.error, urllib.request

ENGINE = os.environ.get("HARNESS_ENGINE_BIN")
if not ENGINE or not os.path.exists(ENGINE):
    print("need HARNESS_ENGINE_BIN"); sys.exit(2)
HOME = os.environ.get("CODEWHALE_HOME")
if not HOME or not os.path.exists(HOME):
    print("need CODEWHALE_HOME (a clean dir with config.toml)"); sys.exit(2)

def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p

def call(url, body=None, method=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers={"content-type": "application/json"}, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:200]

port = free_port()
base = f"http://127.0.0.1:{port}"
log = open("/tmp/probe-approval-engine.log", "w")
proc = subprocess.Popen([ENGINE, "app-server", "--http", "--insecure-no-auth", "--port", str(port)],
                        env=dict(os.environ, CODEWHALE_HOME=HOME), stdout=log, stderr=subprocess.STDOUT)
try:
    for _ in range(60):
        try:
            if call(base + "/healthz")[0] == 200: break
        except Exception: pass
        time.sleep(0.5)
    else:
        print("FAIL engine never became healthy"); sys.exit(1)

    # PROBE_AUTO_APPROVE=1 让线程自己批命令——探针此时**应该**报 FAIL，
    # 否则它就是个永远绿的空测试。
    auto = os.environ.get("PROBE_AUTO_APPROVE") == "1"
    code, thread = call(base + "/v1/threads", {
        "auto_approve": auto,
        "model_provider_id": os.environ.get("HARNESS_PROVIDER", "magipe"),
        "model": os.environ.get("HARNESS_MODEL", "auto/deepseek-v4.1-flash"),
    })
    if code != 201 or not thread.get("id"):
        print(f"FAIL create thread → {code} {thread}"); sys.exit(1)
    tid = thread["id"]

    seen = {}
    def reader():
        try:
            with urllib.request.urlopen(f"{base}/v1/threads/{tid}/events", timeout=120) as r:
                for line in r:
                    line = line.decode(errors="replace").strip()
                    if not line.startswith("data:"): continue
                    try: ev = json.loads(line[5:])
                    except Exception: continue
                    k = ev.get("event") or ev.get("kind") or ""
                    seen.setdefault("kinds", []).append(k)
                    if "approval" in k and "approval" not in seen:
                        seen["approval"] = ev
                    if k in ("turn.completed", "turn.failed", "turn.cancelled"):
                        seen.setdefault("end", k)
        except Exception as e:
            seen["reader_error"] = str(e)
    threading.Thread(target=reader, daemon=True).start()
    time.sleep(1)

    code, _ = call(f"{base}/v1/threads/{tid}/turns", {"prompt": "运行 echo hi"})
    if code not in (200, 201):
        print(f"FAIL start turn → {code}"); sys.exit(1)

    for _ in range(240):
        if "approval" in seen: break
        time.sleep(0.25)
    if "approval" not in seen:
        print("FAIL no approval.required arrived (auto_approve off?)"); sys.exit(1)

    pl = seen["approval"].get("payload") or {}
    aid = pl.get("approval_id") or pl.get("id")
    tool = pl.get("tool_name")
    cmd = (pl.get("input") or {}).get("command")
    print(f"approval: id={aid} tool={tool} command={cmd!r}")
    if not aid or not tool or not cmd:
        print("FAIL the event is missing id/tool/command — this is what broke the channel before")
        sys.exit(1)

    code, body = call(f"{base}/v1/approvals/{aid}", {"decision": "deny"})
    print(f"POST /v1/approvals/{aid} {{'decision':'deny'}} → {code} {body if code != 200 else 'ok'}")
    if code != 200:
        print("FAIL the engine refused the denial (wrong id or wrong word?)"); sys.exit(1)

    for _ in range(240):
        if "end" in seen: break
        time.sleep(0.25)
    end = seen.get("end")
    print(f"turn end: {end}")
    if end != "turn.completed":
        print(f"FAIL the turn did not finish cleanly after a denial ({end})"); sys.exit(1)
    if "turn.failed" in seen.get("kinds", []):
        print("FAIL a denial produced turn.failed"); sys.exit(1)

    print("PASS 审批通道：id 对、命令对、deny 被接受、这一轮正常收尾")
finally:
    proc.terminate()
    try: proc.wait(timeout=5)
    except Exception: proc.kill()
    log.close()
