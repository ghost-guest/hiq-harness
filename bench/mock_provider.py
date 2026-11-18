#!/usr/bin/env python3
"""假 provider：说 OpenAI Chat Completions，但回答是**脚本化的**。

用途只有一个：把 harness 管道本身测出来（引擎→工具→审批→改文件→自测）。
它每次收到请求就查一遍 bench/tasks/*.json，找到与用户消息匹配的题，
用 `bash` 工具把 gold 内容写进工作区，然后正常结束。

所以跑分数字的含义是「在模型永远做对的前提下，harness 能不能把活干完」，
不是模型能力。真实模型基线另算。
"""
import json
import pathlib
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

ROOT = pathlib.Path(__file__).resolve().parent
TASKS = [json.loads(p.read_text()) for p in sorted((ROOT / "tasks").glob("*.json"))]


def pick_task(messages):
    text = "\n".join(
        m.get("content") or "" for m in messages if m.get("role") == "user"
    )
    for t in TASKS:
        if t["prompt"] in text:
            return t
    return None


def materialize(gold):
    """把 gold 变成「文件名 → 内容」。

    `__script__` 是**要执行的 python**，不是文件内容。这里就地跑一遍再收集产物——
    因为内核的执行策略只自动放行一类命令（`cat` 放行、`python3 -` 会走审批），
    所以脚本要在 mock 侧跑完，工具侧只留 `cat > 文件`。
    """
    files = {name: content for name, content in gold.items() if name != "__script__"}
    script = gold.get("__script__")
    if script:
        import subprocess
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(["python3", "-c", script], cwd=tmp, check=True,
                           capture_output=True, text=True)
            for path in pathlib.Path(tmp).rglob("*"):
                if path.is_file():
                    files[str(path.relative_to(tmp))] = path.read_text()
    return files


def write_command(gold):
    """只用 `cat > 文件 <<'EOF'`：实测这是内核自动放行的写法。

    管道（`| base64 -d`）会走审批，没人应答就卡住——所以内容不用管道传。
    """
    parts = []
    for name, content in materialize(gold).items():
        if not content.endswith("\n"):
            content += "\n"
        tag = "GOLD_EOF_" + format(abs(hash(name + content)) % 10**8, "08d")
        parts.append(f"cat > {name} <<'{tag}'\n{content}{tag}")
    return " && ".join(parts)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.endswith("/models"):
            self._send({"data": [{"id": "scripted", "object": "model"}]})
        else:
            self._send({"error": "not found"}, 404)

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        messages = req.get("messages", [])
        task = pick_task(messages)
        already_acted = any(m.get("role") == "tool" for m in messages)

        if task and not already_acted:
            cmd = write_command(task["gold"])
            msg = {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_gold",
                    "type": "function",
                    "function": {"name": "bash", "arguments": json.dumps({"command": cmd})},
                }],
            }
            finish = "tool_calls"
        else:
            msg = {"role": "assistant", "content": "done"}
            finish = "stop"

        payload = {
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": req.get("model", "scripted"),
            "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
        }
        if not req.get("stream"):
            self._send(payload)
            return

        # 引擎一定用流式；不走 [DONE] 会被判 “stream closed before [DONE]”。
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()

        def chunk(delta, finish_reason=None):
            body = {
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "model": payload["model"],
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
            }
            self.wfile.write(f"data: {json.dumps(body)}\n\n".encode())
            self.wfile.flush()

        chunk({"role": "assistant"})
        if msg.get("tool_calls"):
            call = msg["tool_calls"][0]
            chunk({"tool_calls": [{
                "index": 0,
                "id": call["id"],
                "type": "function",
                "function": call["function"],
            }]})
        elif msg.get("content"):
            chunk({"content": msg["content"]})
        chunk({}, finish)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8123
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
