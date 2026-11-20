# S0–S5 验收证据

- 生成时间：2026-11-20 08:55:30 CST
- 提交：`8714a9d` (fix(events): 补齐 agent_start，序列对齐界面测试用的标准序列)
- 引擎：`/tmp/kh/codewhale/target/debug/codewhale`（外部二进制，D1 进程依赖）

## 1. 测试

红点不是"允许红"，是"红点必须被解释"。实际失败的用例会与 `bench/known-red.txt`
求差：清单里没有的那一条，就是没人解释过的失败，本节直接判不合格。

| `cargo test --workspace`（内核 9 个 crate） | 418 passed / 0 failed |
| `cargo test -p orbit-rpc`（接缝① 客户端） | 94 passed / 0 failed |
| `cargo test -p orbit-pi`（GPUI 界面，无头） | 1938 passed; 1 failed; 0 filtered out; finished in 31.02s |

orbit-pi 的红点（每一条都在 `bench/known-red.txt` 里登记了原因）：`app::session_park_tests::parked_session_cancels_custom_ui_with_a_top_level_flag `
orbit-pi 偶发红：本次没有（每条未登记失败都单跑确认过，见上）

## 2. 基准自检（不花 token，不碰网络）

```
b01_off_by_one         brownfield  before=fail after=PASS  valid
b02_empty_input        brownfield  before=fail after=PASS  valid
b03_float_compare      brownfield  before=fail after=PASS  valid
b04_mutable_default    brownfield  before=fail after=PASS  valid
b05_early_return       brownfield  before=fail after=PASS  valid
b06_unflushed_file     brownfield  before=fail after=PASS  valid
b07_encoding           brownfield  before=fail after=PASS  valid
b08_timezone           brownfield  before=fail after=PASS  valid
b09_bare_except        brownfield  before=fail after=PASS  valid
b10_memo               brownfield  before=fail after=PASS  valid
g01_fizzbuzz           greenfield  before=fail after=PASS  valid
g02_csv_totals         greenfield  before=fail after=PASS  valid
g03_top_words          greenfield  before=fail after=PASS  valid
g04_flatten            greenfield  before=fail after=PASS  valid
g05_backoff            greenfield  before=fail after=PASS  valid
g06_lru                greenfield  before=fail after=PASS  valid
g07_semver             greenfield  before=fail after=PASS  valid
g08_logparse           greenfield  before=fail after=PASS  valid
g09_args               greenfield  before=fail after=PASS  valid
g10_todo               greenfield  before=fail after=PASS  valid

selftest ok: 20 tasks, every check fails before the fix and passes after
```
## 3. 真模型基线（DeepSeek V4.1 Flash，经本机 3425 代理）

```
{
  "total": 20,
  "passed": 20,
  "pass_rate": 1.0,
  "rework_rate": 0.01,
  "false_kill_rate": 0.0,
  "cache_hit_rate": 0.901,
  "mean_duration_s": 20.05
}
```
## 4. 引擎实况（现起现停，不依赖历史）

### 4.1 健康检查
```
{"status":"ok","protocol":"v2","service":"deepseek-app-server"}
```
### 4.2 建会话
```
{"schema_version":2,"id":"thr_c86aefd7f0484f74be9a3a4f209408a8","created_at":"2026-11-20T01:01:12.633367Z","updated_at":"2026-11-20T01:01:12.633367Z","model":"auto/deepseek-v4.1-flash","model_provider":"custom","model_provider_id":"magipe","workspace":"/private/tmp/evidence-ws.gr9cNJ","mode":"agent"```
### 4.3 档位（合法值被接受，非法值被拒绝并列出**全部**取值）
```
reasoning_effort=low   -> 201
```
```
Unrecognized reasoning effort "bogus". Expected one of: auto, off, minimal, low, medium, high, xhigh, ultra, or max.
```
### 4.4 事件流里真的有工具调用和用量
```
事件种类: {'thread.started': 1, 'turn.started': 1, 'item.started': 3, 'item.completed': 4, 'turn.lifecycle': 1, 'turn.workspace_snapshot': 4, 'model.tools.snapshot': 3, 'turn.provider_request': 2, 'turn.usage': 2, 'operation.activity_started': 1, 'operation.activity_completed': 1, 'item.delta': 1, 'turn.completed': 1, 'turn.artifacts': 1}
工具调用: ['bash', 'bash']
用量: {"input_tokens": 9223, "output_tokens": 26, "prompt_cache_hit_tokens": 9088, "prompt_cache_miss_tokens": 135, "reasoning_tokens": 0, "reasoning_replay_tokens": 4}
```
### 4.5 fork 出新会话（接在一个已完成轮次的会话上——空会话没有可分的枝）
```
{"error":{"message":"Workspace already has an active turn (thread thr_dc788b9c6bb44515935f124f8900ebd3); wait for it to finish before restoring files","status":409}}
-> 409
```
### 5. 审批通道：真引擎里 deny 一条命令，这一轮还能正常收尾

> 这条通道曾经整条是坏的（id 硬编码 0 / 命令没摊平 / 词表抄错 / 重复投递），
> 而所有基线都跑在 `auto_approve:true` 上，所以没人发现。探针会自己起一个引擎去验。
```
approval: id=approval_9f485011c6a4420cb7f89c013c3e7b7e tool=bash command='echo hi'
POST /v1/approvals/approval_9f485011c6a4420cb7f89c013c3e7b7e {'decision':'deny'} → 200 ok
turn end: turn.completed
PASS 审批通道：id 对、命令对、deny 被接受、这一轮正常收尾
```
## 6. 接缝① 真引擎（orbit 客户端 → harnessd → 引擎）

`orbit-pi` 的渲染测试用的是假 pi 子进程。这一节用**真的**：orbit 自己的
`PiClient` 拉起 harnessd（环境变量指定 HTTP 引擎，和界面走的是同一条路），
发一句 prompt，要求它真的调用 bash，然后检查流式文本 / 工具名 / 收尾三件事。

```
warning: variable does not need to be mutable
   --> /Users/mac_522/Desktop/personal-Agentharness/crates/cli/src/llm_transport.rs:192:13
    |
192 |         let mut options = self.build_options(tools)?;
    |             ----^^^^^^^
    |             |
    |             help: remove this `mut`
    |
    = note: `#[warn(unused_mut)]` (part of `#[warn(unused)]`) on by default

文本长度: 32
文本片段: "`echo seam1` 的输出：\n\n```\nseam1\n```"
工具调用: ["bash"]
agent_end: true
PASS 接缝① 真引擎：orbit 客户端 → harnessd → 引擎，流式文本 + 工具调用 + 收尾都在
```
## 7. S4 的 6 个事件：接线 ≠ 发得出来

`grep broadcast` 只能证明接了线。这一节真跑一轮，让插件把收到的每种事件记一行，然后对账。
4 种常驻事件必须出现；`guard.deny` / `compact.pre` 是条件触发，只报告不判死——
含糊成"都有"比承认"这一轮没发生"危险得多。

```
收到的事件：tool.post tool.pre turn.end turn.start
条件触发（这一轮没有发生，不是缺陷）：guard.deny（守卫拒绝时才发）、compact.pre（压缩时才发）
PASS 常驻 4 事件（turn.start / tool.pre / tool.post / turn.end）在真回合里都到了
```
## 8. 真压缩一次：compact.pre 端到端

事件探针只跑普通回合，看不到 `compact.pre`。这一节把 `HARNESSD_WINDOW` 调到 9000，
跑一个 1.5 万 token 的回合，让缓存守卫判定"窗口满了且前缀还很热"，逼它真的请求压缩。

```
收到的事件：turn.end turn.start 
用量（cached_tokens 尾两条）："cached_tokens":8704 "cached_tokens":19840 
scripts/probe-compact.sh: line 96: 26763 Terminated: 15          python3 - "$PORT"  > /tmp/compact-approver.log 2>&1 <<'APPROVER'
import json, sys, time, urllib.request
port = sys.argv[1]; base = f"http://127.0.0.1:{port}"
answered = set(); deadline = time.time() + 600
while time.time() < deadline:
    try:
        pend = json.loads(urllib.request.urlopen(base + "/v1/approvals", timeout=5).read())
        for a in (pend.get("approvals") or pend.get("pending") or []):
            aid = a.get("id") or a.get("approval_id")
            if not aid or aid in answered: continue
            answered.add(aid)
            req = urllib.request.Request(base + f"/v1/approvals/{aid}",
                data=json.dumps({"decision": "allow"}).encode(),
                headers={"content-type": "application/json"}, method="POST")
            try: urllib.request.urlopen(req, timeout=10)
            except Exception: pass
    except Exception:
        pass
    time.sleep(1)
APPROVER

…第 4/6 次没触发（实际命中率：hit only 76.7% hit only 35.5% hit only 68.2% ）
…第 2 回合就触发了压缩，提前结束（还剩 2 回合没跑）
收到的事件：compact.pre turn.end turn.start 
用量（cached_tokens 尾两条）："cached_tokens":15232 "cached_tokens":19840 
scripts/probe-compact.sh: line 96: 27660 Terminated: 15          python3 - "$PORT"  > /tmp/compact-approver.log 2>&1 <<'APPROVER'
import json, sys, time, urllib.request
port = sys.argv[1]; base = f"http://127.0.0.1:{port}"
answered = set(); deadline = time.time() + 600
while time.time() < deadline:
    try:
        pend = json.loads(urllib.request.urlopen(base + "/v1/approvals", timeout=5).read())
        for a in (pend.get("approvals") or pend.get("pending") or []):
            aid = a.get("id") or a.get("approval_id")
            if not aid or aid in answered: continue
            answered.add(aid)
            req = urllib.request.Request(base + f"/v1/approvals/{aid}",
                data=json.dumps({"decision": "allow"}).encode(),
                headers={"content-type": "application/json"}, method="POST")
            try: urllib.request.urlopen(req, timeout=10)
            except Exception: pass
    except Exception:
        pass
    time.sleep(1)
APPROVER

PASS compact.pre 端到端到达插件（真引擎 + 真 harnessd + 真插件，第 2 次）
```
OK 真压缩探针：这一轮触发了 compact.pre
环境自检：外部引擎 home 的 config.toml 完好（218 字节）
## 9. 界面真的渲染出来了（OCR，不是人眼）

界面这一格以前是"没人看过"。现在用截图 + OCR 断言关键控件在位——
锁屏时会明确 SKIP（截到的是登录框，不是界面），不会假装通过。

```
SKIP 屏幕锁着——截图只会得到登录框，这不是界面测试。解锁后重跑。
```
界面探针：SKIP（屏幕锁着，这一格本轮没验，不算通过）

- 本次起的引擎已清理；残留：无

> 上面每一条都是刚刚现场跑出来的；没有任何一步读仓库里的历史产物（基准 JSON 除外，它是跑分留下的事实记录）。
