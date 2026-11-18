<!-- 这份是 `bash scripts/evidence.sh` 的原样输出，不是手抄的。
     重跑：HARNESS_ENGINE_BIN=/path/to/codewhale bash scripts/evidence.sh
     脚本自带检查：任何一格取不到数字 / 出现未登记的失败用例，都会打印「证据不完整」并 exit 1。 -->

# S0–S5 验收证据

- 生成时间：2026-11-19 06:28:38 CST
- 提交：`acea1a4` (docs+bench: 区分红点与抖动——实测 6/6 过的那条移出 known-red，抖动不许登记成已知红)
- 引擎：`/tmp/kh/codewhale/target/debug/codewhale`（外部二进制，D1 进程依赖）

## 1. 测试

红点不是"允许红"，是"红点必须被解释"。实际失败的用例会与 `bench/known-red.txt`
求差：清单里没有的那一条，就是没人解释过的失败，本节直接判不合格。

| `cargo test --workspace`（内核 9 个 crate） | 395 passed / 0 failed |
| `cargo test -p orbit-rpc`（接缝① 客户端） | 94 passed / 0 failed |
| `cargo test -p orbit-pi`（GPUI 界面，无头） | 1932 passed; 6 failed; 0 filtered out; finished in 39.96s |

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
{"schema_version":2,"id":"thr_b62b37c7115f4fa29a725467b9186948","created_at":"2026-11-18T22:30:24.856139Z","updated_at":"2026-11-18T22:30:24.856139Z","model":"auto/deepseek-v4.1-flash","model_provider":"custom","model_provider_id":"magipe","workspace":"/private/tmp/evidence-ws.5ReZru","mode":"agent"```
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
用量: {"input_tokens": 9225, "output_tokens": 10, "prompt_cache_hit_tokens": 9088, "prompt_cache_miss_tokens": 137, "reasoning_tokens": 0, "reasoning_replay_tokens": 4}
```
### 4.5 fork 出新会话（接在一个已完成轮次的会话上——空会话没有可分的枝）
```
n_observation":{"sessions_dir":"/tmp/evidence-home.QZDKBJ/sessions","session_id":"75e336a8-1c8a-420f-b8e1-40b8fe31e94e","document_sha256":"292d8b6bc4f33efc8bc82691e6b810e8e02f757ce3cd4fbde06228fc3ee274b8","checkpoint_sha256":null},"covered_turn_id":"turn_43b1bca5dd8543e1a06df207010f46dc","messages_sha256":"e37d79b16629db43ec3f10a91fd4724cecab850f149fe5d315446e72ffdc65b9","messages_len":4}}
-> 201
```
### 5. 审批通道：真引擎里 deny 一条命令，这一轮还能正常收尾

> 这条通道曾经整条是坏的（id 硬编码 0 / 命令没摊平 / 词表抄错 / 重复投递），
> 而所有基线都跑在 `auto_approve:true` 上，所以没人发现。探针会自己起一个引擎去验。
```
approval: id=approval_6856d26c42a9490cbae48888e5d1dd1c tool=bash command='echo hi'
POST /v1/approvals/approval_6856d26c42a9490cbae48888e5d1dd1c {'decision':'deny'} → 200 ok
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

> 上面每一条都是刚刚现场跑出来的；没有任何一步读仓库里的历史产物（基准 JSON 除外，它是跑分留下的事实记录）。
