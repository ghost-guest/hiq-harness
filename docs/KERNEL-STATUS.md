# 融合内核实施状态（S0–S5）

> 方案：`docs/KERNEL-FUSION-PLAN.md`。本文件只记**已落地并可复跑**的东西。
> 代码：`crates/harnessd`（Rust 监督层）、`sdk/go/harnessplugin`（插件 SDK）、`bench/`（20 任务评测集）。

---

## 一句话

**引擎作为进程通过 JSON-RPC 接入，融合层全在我们自己的 crate 里。**
`cargo test -p harnessd` 37 项、Go SDK 3 项、20 个评测任务全部通过，零编译警告。

---

## S0 地基

| 项 | 落点 | 验证 |
|---|---|---|
| 新 crate | `crates/harnessd`（lib + bin） | `cargo check -p harnessd` |
| 引擎接法 | 进程 + stdio JSON-RPC（`engine.rs`） | `JsonRpcEngine` / `MockEngine` 双实现 |
| 禁止改引擎 | `scripts/check-engine.sh` + `vendor/ENGINE_PIN` | 有改动即 exit 1 |
| CI | `.github/workflows/kernel.yml` | rust + go + bench + pin 四步 |

**为什么不是 lib 依赖**：codewhale 的 engine 是单 crate 55.8 万行、无 feature gate。
编译进我们的树意味着每次上游变动都要重建，这正是"返工"的来源。走协议则上游随便改，
只要 method 不变我们就活着。

## S1 接缝①（orbit ⇄ kernel）

- `mapping.rs`：orbit 的 **35 个命令全部映射**，`every_known_command_maps_to_something` 断言数量等于 35。
- `Prompt` → `thread/start`（首次）/ `turn/start`（之后），`model`/`effort` 随 `turn/start` 透传（对应 codewhale `TurnStartParams`）。
- `protocol.rs`：`ui_kind_for` 把引擎通知翻译成 UI 事件；不可识别的走 `engine/<method>` **转发而不是丢弃**（静默丢弃是 UI bug 变隐形的根源）。
- `Auth*` / `Quota*` 六条**诚实返回不适用**，不伪造 OAuth。

## S2 模型与会话

- `SessionState` 持有 thread/model/effort/auto-compact/auto-retry/steer 模式。
- `Fork`/`CloneSession` → `thread/fork`；`SwitchSession` → `thread/resume`；`SetSessionName` → `thread/name/set`。
- 七档 effort 由 `GetAvailableThinkingLevels` 暴露，`turn/start.effort` 生效。

## S3 融合件（这是方案的核心）

| 融合件 | 机制 | 验证测试 |
|---|---|---|
| **守卫否决** | 引擎的审批反向 request（`item/commandExecution/requestApproval`）→ 守卫裁决 → 回 `denied` | `guard_veto_turns_into_a_denial_on_the_approval_channel` |
| **time-bus 六条** | append-only + seq、投影纯函数、只记影响行为的事实、副作用不重放、uncertain 阻塞 | `timebus::tests`（6 项） |
| **steer 安全点** | 只在 item/turn 边界注入，不打断飞行中的响应 | `steer_waits_for_item_boundary_then_reaches_the_engine` |
| **缓存守卫** | 窗口 85% + 命中 90% 两阈值：热前缀→压缩，冷前缀→冻结 | `full_window_with_hot_prefix_triggers_compaction` |
| **不确定即阻塞** | 工具结果无法确认时记 `Uncertain`，绝不重试 | `uncertain_tool_result_blocks_the_turn` |

## S4 插件层（Go）

- `sdk/go/harnessplugin`：单文件 SDK，`Serve(name, WithTool(...), WithEvent(...))`。
- 五条规则落地：协议版本不匹配→装载失败；`policy.bypass` cap 被内核剥离；崩溃三振出局；`uncertain`→阻塞；插件只返值不写状态。
- 六个事件：`tool.pre/tool.post/turn.start/turn.end/compact.pre/guard.deny`。
- 端到端：`crates/harnessd/tests/go_plugin.rs` 真编译 `plugins/example-echo` 并调用工具。

## S5 评测集

- `bench/tasks/*.json` 20 个任务：10 新建（greenfield）+ 10 维护（brownfield）。
- `bench/run.py --selftest` 对每个任务验证两件事：**修前必失败**、**修后必通过**。
  只满足其一的检查会被判 `INVALID`——防止"永远通过"的假判据。
- `bench/run.py --solver '<cmd>'` 用真实 harness 跑分，指标：通过率、新建/维护分别通过率、守卫拒绝数、阻塞轮数、耗时。

---

## 怎么跑

```bash
cargo test -p harnessd                      # 37 tests
cargo run -p harnessd -- selftest           # 8 项端到端（无模型、无网络、无 key）
cd sdk/go && go test ./...                  # 插件 SDK
python3 bench/run.py --selftest             # 20 任务判据自检
python3 bench/run.py --list                 # 看待跑任务

# 接真引擎：
harnessd serve --engine /path/to/codewhale-app-server --log .harness/timebus.ndjson
harnessd replay .harness/timebus.ndjson     # 日志是事实：任何状态可从它重建
harnessd doctor                             # 环境检查
```

## 已知缺口（不粉饰）

1. ~~审批应答的字段名未对真引擎验证过~~ **已按源码对正（未跑二进制）**：
   `CommandExecutionRequestApprovalResponse { decision }`，
   取值 `accept / acceptForSession / acceptWithExecpolicyAmendment / applyNetworkPolicyAmendment / decline / cancel`
   （`app-server-protocol/src/v2.rs:542-560`）；
   请求侧是 `command` + `commandActions[]`（`read|listFiles|search|unknown`）+ `itemId`，
   **没有 tool 字段**（同文件 `:584-608`）。
   我们已把 `commandActions` 映射成守卫认识的 `read_file/list_dir/grep`，应答改用 `accept/decline/cancel`，
   并由 `real_engine_payload_maps_command_actions_to_our_tool_names` 与
   `approval_accepts_with_the_engines_own_wording` 两个测试钉住。
2. **未接真引擎跑过任何任务**。`bench --solver` 还没有基线数字，因为需要 codewhale 二进制 + provider key。
3. **orbit 的 `client.rs` 尚未切到本协议**。S1 只保证了命令映射与事件形状，UI 侧改造还没做。
4. 守卫的三级阶梯阈值沿用现项目 `harness-guards`，没有在 20 任务上标定过误杀率。

## 下一步（按顺序）

1. 拿到 codewhale app-server 二进制 → 核对审批 schema → 跑 `bench --solver` 出第一版基线
2. orbit `client.rs` 换后端（SSE → app-server JSON-RPC），复用现有 `bridge.rs` 的事件映射
3. 用基线数字标定守卫阈值（误杀率目标：合法长任务 0 误杀）
