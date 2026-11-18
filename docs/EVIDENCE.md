<!-- 这份是 `bash scripts/evidence.sh` 的原样输出，不是手抄的。
     重跑：HARNESS_ENGINE_BIN=/path/to/codewhale bash scripts/evidence.sh
     脚本自带检查：任何一格取不到数字 / 出现未登记的失败用例，都会打印「证据不完整」并 exit 1。 -->

# S0–S5 验收证据

- 生成时间：2026-11-19 06:34:20 CST
- 提交：`5986169` (fix(test): 统一等待预算——满载时 5s 截止把'慢'误报成'坏'，连跑两遍失败集稳定为 6)
- 引擎：`/tmp/kh/codewhale/target/debug/codewhale`（外部二进制，D1 进程依赖）

## 1. 测试

红点不是"允许红"，是"红点必须被解释"。实际失败的用例会与 `bench/known-red.txt`
求差：清单里没有的那一条，就是没人解释过的失败，本节直接判不合格。

| `cargo test --workspace`（内核 9 个 crate） | 395 passed / 0 failed |
| `cargo test -p orbit-rpc`（接缝① 客户端） | 94 passed / 0 failed |
| `cargo test -p orbit-pi`（GPUI 界面，无头） | 1932 passed; 6 failed; 0 filtered out; finished in 40.06s |

orbit-pi 的红点（每一条都在 `bench/known-red.txt` 里登记了原因）：`app::session_park_tests::parked_session_cancels_custom_ui_with_a_top_level_flag backdrop::tests::mark_placement_and_small_fallback git_accounts::tests::workspace_local_identity_and_remotes_are_read_without_remote_access onboarding::tests::every_dependency_has_an_install_hint onboarding::tests::host_facts_cover_the_platform_and_the_stores onboarding::tests::install_hints_match_the_host `

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
{"schema_version":2,"id":"thr_aa2a4c29b12d41b0aa749e2f357e4fa8","created_at":"2026-11-18T22:36:03.368225Z","updated_at":"2026-11-18T22:36:03.368225Z","model":"auto/deepseek-v4.1-flash","model_provider":"custom","model_provider_id":"magipe","workspace":"/private/tmp/evidence-ws.TjlgDw","mode":"agent"```
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
用量: {"input_tokens": 9224, "output_tokens": 18, "prompt_cache_hit_tokens": 9088, "prompt_cache_miss_tokens": 136, "reasoning_tokens": 0, "reasoning_replay_tokens": 4}
```
### 4.5 fork 出新会话（接在一个已完成轮次的会话上——空会话没有可分的枝）
```
n_observation":{"sessions_dir":"/tmp/evidence-home.7wldVR/sessions","session_id":"aaec40d5-fb93-4e85-8296-602b56f6d198","document_sha256":"61d7957162aef345b7a57568ca1fb4bb38e19ef6e346775582c5999bc2adc279","checkpoint_sha256":null},"covered_turn_id":"turn_25c40e7efc2944b4824d1a4458223c51","messages_sha256":"4830971aaab41844efaab4707e3d64d9868ef93f4cd08497ac0bdb8b316e2ca1","messages_len":4}}
-> 201
```
### 5. 审批通道：真引擎里 deny 一条命令，这一轮还能正常收尾

> 这条通道曾经整条是坏的（id 硬编码 0 / 命令没摊平 / 词表抄错 / 重复投递），
> 而所有基线都跑在 `auto_approve:true` 上，所以没人发现。探针会自己起一个引擎去验。
```
approval: id=approval_d1b956b6b5ce47b7b04dbadba8331c37 tool=bash command='echo hi'
POST /v1/approvals/approval_d1b956b6b5ce47b7b04dbadba8331c37 {'decision':'deny'} → 200 ok
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

文本长度: 13
文本片段: "```\nseam1\n```"
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

> 上面每一条都是刚刚现场跑出来的；没有任何一步读仓库里的历史产物（基准 JSON 除外，它是跑分留下的事实记录）。
