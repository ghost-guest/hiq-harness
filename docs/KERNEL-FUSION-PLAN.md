# 融合内核方案：以 codewhale 为根，融 jcode / dim / codex / grok-build / openhuman

> 目标：一个自研 harness，底座 codewhale，外层 orbit 做界面。
> 原则：**融合只发生在四类接缝上，不重写 engine。**
> 所有可以移植的内容在 §4 有明确的「源文件 → 适配点」对照。

---

## 0.1 实施时的修正：D1 用进程，不用 lib

原方案写的是「把 engine 当 crate 依赖引入」。落地时改成 **spawn app-server + JSON-RPC**，三个理由：

1. **编译面**：engine 是单 crate 55.8 万行、无 feature gate。作为 lib 引进来，我们的 `cargo test` 就要为它付编译时间。
2. **升级面**：进程依赖下，上游重构内部类型不影响我们；lib 依赖下，任何 `pub` 变动都要跟着改。
3. **否决权不受影响**：审批是**反向 request**，客户端本来就是必须应答的一方——
   我们回 `decline` 就是真拒绝，不需要进它的 `ExecPolicyEngine`。

代价：拿不到 `set_exec_policy` 那种进程内钩子，守卫只能在「审批点」与「steer 点」生效。
这已经覆盖三守卫的全部判定（都在工具调用边界），所以不构成功能缺口。

## 0. 一句话

**codewhale 提供"骨头"（沙箱、快照、协议、审批），jcode 提供"神经"（steer 注入点、journal 重放），
dim 提供"记性"（time-bus 语义），codex 提供"语言规范"（协议形状），openhuman 提供"刹车"（缓存守卫）。
五者通过 4 个接缝缝合，engine 内核一行不改。**

---

## 1. 融合架构：四类接缝，三种融合深度

```
┌────────────────────────────────────────────────────────────┐
│  orbit（桌面 UI，apps/orbit，不 fork 不重构）                │
│  crates/orbit-rpc/src/client.rs —— 只换后端适配              │
└────────────────────────────────────────────────────────────┘
        │ 接缝①：orbit ⇄ kernel（35 命令 + SSE 事件流）
        ▼
┌────────────────────────────────────────────────────────────┐
│  harnessd —— 我们自己的 crate（新写，8–12k 行）              │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ 适配层  orbit-compat    35 命令 → thread/turn 映射    │  │
│  │        model-router     Magpie/DeepSeek/7 档 effort   │  │
│  ├──────────────────────────────────────────────────────┤  │
│  │ 融合层（接缝②③④ 都挂在这里）                          │  │
│  │   steer-gate      ← jcode 注入点                      │  │
│  │   time-bus        ← dim time-bus 六条                 │  │
│  │   cache-guard     ← openhuman PromptCacheGuard        │  │
│  │   guards          ← 现项目 guards crate               │  │
│  │   hook-host       ← Go/任意语言插件（stdio JSON-RPC）  │  │
│  ├──────────────────────────────────────────────────────┤  │
│  │ codewhale engine（55.8 万行，作为 lib 依赖引入，零改动）│  │
│  └──────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────┘
```

**三种融合深度（严格区分，这是"不返工"的核心）：**

| 深度 | 含义 | 适用 | 改动风险 |
|---|---|---|---|
| **D1 依赖** | **进程依赖**：spawn 它的 app-server，只走 JSON-RPC（实施时从 lib 依赖改成这个，理由见 §0.1） | engine 全部能力 | 零源码耦合，升级 = 换二进制 |
| **D2 缝合** | 在我们自己的 crate 里写适配器，调它的 public API | steer / 协议映射 / model 路由 / 缓存守卫 | 低，只跟 public API |
| **D3 移植** | 抄源码进来（带来源注释） | dim time-bus 语义、jcode injection point | 中，但要记来源 |

**禁止 D4「改 engine 源码」**——这是唯一会导致返工的操作。

---

## 2. 五源分工表（谁贡献什么，为什么是它）

| 来源 | 贡献 | 为什么是它（对比过谁） |
|---|---|---|
| **codewhale** | 执行骨架：turn 循环、工具、sandbox（seatbelt+bwrap）、execpolicy、每轮快照、审批、LSP、compaction、checkpoint | 唯一七机制全绿（见 V3 §2）；MIT；DeepSeek 原生；协议兼容 codex |
| **jcode** | **steer 注入点**、`batch` 并行上限策略、journal 式重放、provider crate 插件化范本 | steer 在 jcode 是"两处 injection point"的精确位置（`turn_streaming_mpsc.rs:1431-1450`），比 codex 的 `turn/steer` 更细；但它缺沙箱/快照，只能供机制不能当底座 |
| **dim** | **time-bus 六条**（只认序号 / 状态=事件折叠 / 只管影响后续的结果 / 写前复核归各状态机 / 副作用不重放 / 总线之外）、`context_policy` 的保留窗口设计 | 与本项目 L0 近同构，是全场最好的时间线语义；但 alpha + rutis 依赖 → 只抄语义 |
| **codex** | **协议语言规范**（JSON-RPC 无 `jsonrpc` 字段、反向 request、通知 vs 应答）、`item/*` 的 delta 粒度、execpolicy DSL 语义、apply_patch 格式 | codewhale 的协议本来就是它的分叉，直接沿用等于免费拿到生态兼容 |
| **openhuman** | **PromptCacheGuardMiddleware**（缓存前缀冻结 + miss 归因）、TokenJuice 压缩思路 | 缓存命中是本项目实测 96.9% 的关键，它的 guard 是唯一把"何时不能压缩"写进中间件的实现 |
| **grok-build** | 两遍压缩（先摘要再重写）、逐子进程 seccomp 的思路 | 压缩质量明显高于单遍；但仓库已停更，只抄算法 |
| **现项目** | 三守卫（失败熔断 / 阶梯 / 只读螺旋）、收据与 uncertain 阻塞、七档 effort 与 Magpie 映射 | 这些是已经在真机上跑出来的，`crates/guards` 可直接搬 |
| **Go 插件层** | 工具、集成、hooks、审批判定 | 你的语言偏好；进程外，崩溃隔离，跨平台单二进制 |

---

## 3. 四个接缝的详细设计

### 接缝① orbit ⇄ harnessd（**最优先落地**）

orbit 现有 35 个命令（`apps/orbit/crates/orbit-rpc/src/types.rs` 的 `CommandBody`）：
`Prompt, Abort, ClearQueue, Steer, FollowUp, SetSteeringMode, SetFollowUpMode, Compact,
SetAutoCompaction, SetAutoRetry, AbortRetry, SetSessionName, NewSession, SwitchSession,
GetState, GetMessages, GetAvailableModels, SetModel, CycleModel, GetAvailableThinkingLevels,
SetThinkingLevel, CycleThinkingLevel, GetCommands, GetSessionStats, GetForkMessages, GetEntries,
Fork, CloneSession, AuthList, AuthStatus, AuthLogin, AuthLogout, AuthCancel, QuotaList, Raw`

**映射策略：36 个全部映射，一个不删。** orbit UI 一行不改。

| orbit 命令 | codewhale 侧 | 做法 |
|---|---|---|
| `Prompt` | `thread/start` + `turn/start` | `turn/start` 自带 `model`/`effort`/`approvalPolicy`（`v2.rs:3773-3793`），不用额外方法 |
| `Steer` / `FollowUp` | `turn/steer` | 直接对应 |
| `Abort` | `turn/interrupt` | 直接对应 |
| `SetModel` / `CycleModel` | 我们自己的 `model-router` | 记在 harnessd 状态里，下次 `turn/start` 带上 |
| `SetThinkingLevel` / `SetAutoRetry` | `turn/start.effort` | 七档 → `ReasoningEffort` 映射表 |
| `GetMessages` / `GetEntries` | `thread/items/list` | 加一层 item→orbit message 的形状转换 |
| `Fork` / `CloneSession` | `thread/fork` | 直接对应 |
| `Compact` / `SetAutoCompaction` | `thread/compact/start` + 本地开关 | 自动压缩开关由 harnessd 持有 |
| `GetSessionStats` | `thread/tokenUsage/updated` 通知累积 | 事件累积成快照 |
| `AuthList/AuthStatus/AuthLogin/Logout/Cancel` | **不需要** | DeepSeek/Magpie 用 API key，本地配置即可，返回固定的 `authenticated` |
| `QuotaList` | **不需要** | 从 usage 事件算，返回本地统计 |
| `Raw` | 透传到协议层 | 逃生舱，保留 |

**关键点**：`Auth*` 与 `Quota*` 六条命令**不要伪造**——按 orbit-harness.md 的教训，
诚实返回"不适用"比编一个假实现好。UI 上这些面直接隐藏。

### 接缝② 融合层 ⇄ engine（D2，只调 public API）

| 融合件 | 挂载点（codewhale 侧） | 来源 |
|---|---|---|
| steer-gate | `turn/steer` 的注入时机 → 复用 engine 的 `inject_items` | jcode `turn_streaming_mpsc.rs:1431-1450` + codex `turn/steer` |
| guards | 工具执行前的 policy 层：`engine/src/core/engine/approval.rs:3395` 的 `WorkspaceExecPolicyBuilder` | 现项目 `crates/guards` |
| cache-guard | 压缩触发前：`thread/compact/start` 的调用方（harnessd 决定何时调） | openhuman `PromptCacheGuardMiddleware` |
| time-bus | 事件出口：所有 `item/*` 与 `turn/*` 通知统一写一个 append-only log | dim `time-bus.md` 六条 |
| hook-host | 进程外插件，工具调用前后 | 自定，见接缝④ |

**为什么 guards 能挂在 execpolicy 上**：codewhale 的 `ExecPolicyEngine` 已经是一个
「可插拔的规则引擎」（`crates/engine/src/core/engine/handle.rs:61-66` 有 `set_exec_policy`），
三守卫写成一组规则集注入进去，不需要改 engine 的判定逻辑。这是 D2 而不是 D4。

### 接缝③ 模型层

```
turn/start { model, effort, input }
        ↓
model-router（harnessd）
  ├─ provider: deepseek / magpie / anthropic / openai  （复用现有 llm_transport.rs 的 aimux 实现）
  ├─ effort 七档 → ReasoningEffort（codewhale 已有枚举）
  └─ 缓存前缀守卫（openhuman 的 guard 逻辑）
        ↓
engine 内部原有的 provider trait（codewhale 已支持 deepseek-v4-pro / deepseek-flash）
```

**注意一个真实约束**：codewhale 的 `auto_model.rs` 里 pro 模型是 `deepseek-v4-pro`，
说明它默认接的就是 DeepSeek 系。这对你有利——**第一个能跑通的版本几乎不用写 provider 代码**。
Magpie 若不在它的 provider 列表里，走 OpenAI 兼容实现即可。

### 接缝④ 插件层（Go，你要的那条）

```
harnessd ──stdio JSON-RPC──▶ 插件进程（Go）
```

五条规则（每条对应一个已发生的真实失败）：
1. `plugin/hello` 必须带 `protocol` 版本；不匹配 → **启动即失败**，不是降级
2. 插件声明 `caps`，内核复核；插件不能放宽 execpolicy（防"clone 下来的仓库改掉行为"）
3. 插件崩溃 → 标记不可用，**内核不崩**；连续 3 次崩溃自动禁用
4. 工具返回 `uncertain` → 收据记 `Uncertain` 并阻塞（防重复副作用）
5. 插件**不得直接写日志/状态**，只能通过返回值（保 time-bus 的"日志是真相"）

可用事件：`tool.pre` / `tool.post` / `turn.start` / `turn.end` / `compact.pre` / `guard.deny`
（这 6 个足够覆盖 90% 的扩展需求：自定义工具、格式化、通知、审计、成本统计、策略）

---

## 4. 实施计划：五个 Sprint，每个 Sprint 都有可跑的验收

| Sprint | 内容 | 改动深度 | 验收 | 工期 |
|---|---|---|---|---|
| **S0 地基** | codewhale 作为 git submodule + 一个空 harnessd crate 能 `cargo build`；engine 作为 lib 路径依赖 | D1 | 两仓独立构建，harnessd 能调 `ExecPolicyEngine` | 2 天 |
| **S1 接缝① 打通** | 36 命令映射（Auth/Quota 诚实返回不适用）；SSE 事件流；`Prompt` 能真跑一轮 | D2 | orbit 界面能发一条消息，看到流式回复和工具调用 | 5 天 |
| **S2 模型 + 会话** | model-router（DeepSeek + Magpie + 七档 effort）、`thread/fork`、`GetEntries`、session 持久化 | D2 | 切换模型/档位生效；fork 出新会话能跑到不同结论 | 4 天 |
| **S3 融合件** | steer-gate、guards 三件、cache-guard、time-bus log | D2+D3 | 流式中途插话生效；10 个长任务不被误杀；压缩前后缓存命中 >90% | 6 天 |
| **S4 插件层** | hook-host（Go stdio）、`plugin/hello` 握手、崩溃隔离、6 个事件 | D2 | 写一个 Go 插件，deny 一条命令，内核记录并继续 | 5 天 |
| **S5 评测与固化** | V3 的 20 任务评测集 + 跑分器 + 回归门 | — | 通过率/返工率/误杀率有基线数字 | 4 天 |

**总计 26 个工作日。** S1 结束就能"看见东西"，这是刻意安排的——早看见能早纠正方向。

**S0 的一个决定性细节**：把 codewhale 放成 **git submodule（或 vendored 但只读，配一个 `PATCH-LOG.md`）**，
并用 CI 检查「我们有没有改过 submodule 里的文件」。有 diff 就红。
这条 CI 检查是"不返工"的物理保障——比任何纪律都有效。

---

## 5. 风险与反方举证

| 风险 | 概率 | 影响 | 应对 |
|---|---|---|---|
| codewhale 的 engine public API 不够用，被迫改源码（D4） | 中 | 高（返工开始） | S1/S3 先做小实验验证 `set_exec_policy` / `inject_items` / `turn/steer` 三个口子够不够；不够就改用**协议层包装**（不改 Rust，用 app-server 协议本身） |
| codewhale 上游停更或大改 | 中 | 中 | 我们只依赖 4 个 crate 的 public API；它停更我们照用，它大改我们钉老 tag |
| orbit 的命令语义和 codewhale 差异大（比如 orbit 的 `FollowUp` 与 `turn/steer` 语义不同） | 高 | 低 | 36 条命令里只有 4–5 条需要真正的语义翻译，其余一一对应；差异写在适配层注释里 |
| 55.8 万行 engine 编译慢 | 高 | 低 | 增量编译 + `sccache`；已验证 codewhale 支持 workspace 分 crate |
| 我漏了某个 star 里的关键机制 | 中 | 中 | V3 的 20 任务评测集就是补漏器；跑分不达标时回头找机制 |

**我可能错的地方，明说三条：**
1. 「只改外围不改 engine」在 55.8 万行单 crate 上可能不够——如果 public API 缺口太大，S3 会变成 D4。止损：S1 结束就判定。
2. codewhale 的 DeepSeek 支持可能只覆盖它自己的用法（`auto_model.rs` 是自动路由），不一定支持任意 provider。S2 第一天就验证。
3. orbit 的后端适配可能比 36 条命令更麻烦——它的 UI 事件模型（`Event::AgentEnd` / `MessageUpdate` / `ToolExecutionStart/End`，见 `bridge.rs:330-374`）要一一对应，这块被低估的风险最高。

---

## 6. 现在就能开始的三件事

1. **S0 落地**：把 codewhale 拉成 submodule、建 harnessd 空 crate、CI 加"submodule 无 diff"检查
2. **S1 探针**：写一个 200 行的最小程序，验证 `turn/start` + `turn/steer` + `set_exec_policy` 三个口子
3. **评测集**：20 个任务 + 跑分脚本（S5 要它，但越早建越好，它是补漏器和回归门）

---

## 附：一图流

```
dim time-bus ──语义──┐
jcode steer ──注入点─┤
openhuman cache ─卫士┤──▶ harnessd（8–12k 行，我们自己的）
现项目 guards ──规则─┤        │
Go 插件 ──6 事件─────┘        │ D2 缝合
                             ▼
                    codewhale engine（D1 只读依赖）
                             │ app-server 协议（codex 语言）
                             ▼
                        orbit（UI 不改）
```
