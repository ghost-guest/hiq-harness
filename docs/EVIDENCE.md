# S0–S5 验收证据（自动生成）

这份文件由 `bash scripts/evidence.sh` 生成，**每一次都是现场跑出来的**，不是手抄的历史。
想复核就自己跑一遍：

```bash
HARNESS_ENGINE_BIN=/path/to/codewhale bash scripts/evidence.sh
```

引擎是外部二进制（D1 进程依赖，不进本仓库），所以要先把它编译好再指过来。

---

# S0–S5 验收证据

- 生成时间：2026-11-19 04:02:49 CST
- 提交：`cb2d533` (fix(seam1): 认 PI_BIN；界面层红 32→6（回执广播、ProcessExited、PI_BIN 三处真缺陷）)
- 引擎：`/tmp/kh/codewhale/target/debug/codewhale`（外部二进制，D1 进程依赖）

## 1. 测试

| 套件 | 结果 |
|---|---|
| `cargo test --workspace`（内核 9 个 crate） | 386 passed / 0 failed |
| `cargo test -p orbit-rpc`（接缝① 客户端） | 94 passed / 0 failed |
| `cargo test -p orbit-pi`（GPUI 界面，无头） | 1930 passed / 6 failed（剩 6 条见 `KERNEL-STATUS.md` 的分类） |

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
  "rework_rate": 0.013,
  "false_kill_rate": 0.0,
  "cache_hit_rate": 0.956,
  "mean_duration_s": 20.01
}
```
## 4. 引擎实况（现起现停，不依赖历史）

### 4.1 健康检查
```
{"status":"ok","protocol":"v2","service":"deepseek-app-server"}
```
### 4.2 建会话
```
{"schema_version":2,"id":"thr_9717a33252b64d18870604ac40f1a62c","created_at":"2026-11-18T20:04:33.166023Z","updated_at":"2026-11-18T20:04:33.166023Z","model":"auto/deepseek-v4.1-flash","model_provider":"custom","model_provider_id":"magipe","workspace":"/private/tmp/evidence-ws.29oKcV","mode":"agent"```
### 4.3 档位（合法值被接受，非法值被拒绝并列出**全部**取值）
```
reasoning_effort=low   -> 201
```
```
Unrecognized reasoning effort "bogus". Expected one of: auto, off, minimal, low, medium, high, xhigh, ultra, or max.
```
### 4.4 事件流里真的有工具调用和用量
```
事件种类: {'thread.started': 1, 'turn.started': 1, 'item.started': 4, 'item.completed': 5, 'turn.lifecycle': 1, 'turn.workspace_snapshot': 4, 'model.tools.snapshot': 3, 'item.delta': 2, 'turn.provider_request': 2, 'turn.usage': 2, 'operation.activity_started': 1, 'operation.activity_completed': 1, 'turn.completed': 1, 'turn.artifacts': 1}
工具调用: ['bash', 'bash']
用量: {"input_tokens": 9305, "output_tokens": 16, "prompt_cache_hit_tokens": 8704, "prompt_cache_miss_tokens": 601, "reasoning_tokens": 1, "reasoning_replay_tokens": 16}
```
### 4.5 fork 出新会话（接在一个已完成轮次的会话上——空会话没有可分的枝）
```
n_observation":{"sessions_dir":"/tmp/evidence-home.mAABMx/sessions","session_id":"a325231c-97b4-4975-84e3-92b4a24a8685","document_sha256":"4e3687a9e0260e8d8f00ac39efa96e8ff746c0d12dcfdc78acad1feeb2015a3b","checkpoint_sha256":null},"covered_turn_id":"turn_6c2eb49878bf4dd0a8323fb8b4c77357","messages_sha256":"a889dbd1fa542b0ce98e672e87163ab461da62e5d00e15eb7f80f8bdfdf5c467","messages_len":4}}
-> 201
```

> 上面每一条都是刚刚现场跑出来的；没有任何一步读仓库里的历史产物（基准 JSON 除外，它是跑分留下的事实记录）。
