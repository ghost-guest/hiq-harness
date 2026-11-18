<!-- 这份是 `bash scripts/evidence.sh` 的原样输出，不是手抄的。
     重跑：HARNESS_ENGINE_BIN=/path/to/codewhale bash scripts/evidence.sh -->

# S0–S5 验收证据

- 生成时间：2026-11-19 05:18:19 CST
- 提交：`d90bb47` (fix(kernel): HTTP 审批通道四个真缺陷——id 硬编码、命令没摊平、词表抄错、重复审批；默认不再让引擎自己批)
- 引擎：`/tmp/kh/codewhale/target/debug/codewhale`（外部二进制，D1 进程依赖）

## 1. 测试

| 套件 | 结果 |
|---|---|
| `cargo test --workspace`（内核 9 个 crate） | 395 passed / 0 failed |
| `cargo test -p orbit-rpc`（接缝① 客户端） | 94 passed / 0 failed |
| `cargo test -p orbit-pi`（GPUI 界面，无头） | 1931 passed; 6 failed; 0 filtered out; finished in 40.81s |

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
{"schema_version":2,"id":"thr_48c41d8ad0cb4ecb80c77cf05f0f5af2","created_at":"2026-11-18T21:20:05.556700Z","updated_at":"2026-11-18T21:20:05.556700Z","model":"auto/deepseek-v4.1-flash","model_provider":"custom","model_provider_id":"magipe","workspace":"/private/tmp/evidence-ws.vImpkV","mode":"agent"```
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
用量: {"input_tokens": 9221, "output_tokens": 5, "prompt_cache_hit_tokens": 9088, "prompt_cache_miss_tokens": 133, "reasoning_tokens": 0, "reasoning_replay_tokens": 4}
```
### 4.5 fork 出新会话（接在一个已完成轮次的会话上——空会话没有可分的枝）
```
n_observation":{"sessions_dir":"/tmp/evidence-home.5loytX/sessions","session_id":"6328e5d5-6648-4be9-83cb-80278a1ada47","document_sha256":"647bb5e6ed57827189c77357c0f08ecda4f071e4955a7e1563355f91bf29e61e","checkpoint_sha256":null},"covered_turn_id":"turn_16acfe152d664f809d07688164e36fa6","messages_sha256":"37073be471dab2d41f0fcf91e01555c3fd06c28cadd56ab14d698e7317b5415c","messages_len":4}}
-> 201
```
### 5. 审批通道：真引擎里 deny 一条命令，这一轮还能正常收尾

> 这条通道曾经整条是坏的（id 硬编码 0 / 命令没摊平 / 词表抄错 / 重复投递），
> 而所有基线都跑在 `auto_approve:true` 上，所以没人发现。探针会自己起一个引擎去验。
```
approval: id=approval_e50fbb760f8e4578a619c2a406130a0d tool=bash command='echo hi'
POST /v1/approvals/approval_e50fbb760f8e4578a619c2a406130a0d {'decision':'deny'} → 200 ok
turn end: turn.completed
PASS 审批通道：id 对、命令对、deny 被接受、这一轮正常收尾
```

> 上面每一条都是刚刚现场跑出来的；没有任何一步读仓库里的历史产物（基准 JSON 除外，它是跑分留下的事实记录）。
