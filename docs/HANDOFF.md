# 交接文档：hiq-harness（给接手的 Codex）

> 读这份就够了。想更细的：`docs/KERNEL-FUSION-PLAN.md`（方案原文）、
> `docs/KERNEL-STATUS.md`（1483 行施工日志，每条结论都带文件行号）、
> `IMPLEMENTATION.md`（最初的需求文档）。
> 这份交接的目的是：**你不用重新趟一遍，也不要推翻已经验证过的东西。**

---

## 0. 一句话说清这是什么

一个**自研 agent harness**：底座是 codewhale 引擎（我们不改它一行源码），
我们自己写一层 `harnessd` 做融合与控制面，外层用 orbit 做桌面界面。

用户（owner）的核心要求，逐条都是硬约束：

1. **只准 Rust + Go**。出现别的语言就算问题，必须处理掉（`scripts/lang-audit.sh` 守这条）。
2. **不许自研传输**。curl 子进程、手写 WebSocket/SHA-1/base64、手写 HTTP 解析全部禁止 →
   必须用 reqwest / reqwest-eventsource / tungstenite / axum 这类现成库。
3. **需要新能力时先调研开源项目，基于已有项目二开**，不要上来就自己写。
4. **严格按方案做**，方案见 §2，尤其是"不改 engine 源码（禁止 D4）"。
5. **每一步都要能自测通过**，不接受"看起来好了"。

---

## 1. 仓库与运行

```
ghost-guest/hiq-harness   (git@github.com:ghost-guest/hiq-harness.git)
工作目录：/Users/mac_522/Desktop/personal-Agentharness
```

### 目录职责

| 路径 | 职责 | 行数 |
|---|---|---|
| `crates/harnessd` | **核心**。stdio app-server（兼容控制面）+ HTTP runtime API（唯一工具/审批面）；把 orbit 的 35 条命令映射到引擎的 thread/turn；SSE 投影；time-bus 日志 | ~7k |
| `crates/core` `context` `guards` `memory` `orchestrate` `persist` `plugin` | 融合层各自的件（三守卫、上下文策略、记忆、插件宿主） | |
| `crates/cli` | 自带 HTTP/WS 服务端（tungstenite 做 WS） | |
| `crates/desktop` | 桌面壳的 SSE 客户端（reqwest-eventsource） | |
| `crates/xtask` | 工具链：`lint`（事故规则）、`mutate`（变异扫描）、`ocr` | |
| `apps/orbit/crates/orbit-pi` | **界面**（GPUI）。不 fork 不重构，只改适配与展示 | ~30k |
| `apps/orbit/crates/orbit-rpc` | orbit 的 RPC 类型/客户端 | |
| `plugins/*` | **Go** 插件（进程外，stdio JSON-RPC） | |

Rust 侧约 2 万行（`crates/`），orbit 另有约 3 万行。

### 跑起来

```bash
# 引擎（外部二进制，不链接它的 crate）
export HARNESS_ENGINE_BIN=/tmp/kh/codewhale/target/debug/codewhale
export CODEWHALE_HOME=/tmp/cw-real6        # 引擎自己的会话/配置目录

bash scripts/run-hiq.sh                     # 起 harnessd + orbit
```

`scripts/run-hiq.sh` 默认会：
- `HARNESSD_ORBIT_DUMP=$HOME/.hiq/orbit-events.ndjson`（事件落盘，卡住时直接看内核到底发了什么）
- `HARNESSD_THREAD_BODY={"auto_approve":true}`

### 自测命令（这就是"验证通过"的定义）

```bash
cargo run -q -p xtask -- lint                 # 4 条事故规则
bash scripts/gate.sh                          # 门禁：lint + 27 条验收 + 基线阈值
MUTATE=1 bash scripts/gate.sh                 # 加跑变异扫描（约 100 秒）
bash scripts/acceptance.sh                    # 27 行验收矩阵
cargo test --workspace                        # 全量单测
```

**当前实测（2026-11-21 07:5x，本机）**

| 项 | 结果 |
|---|---|
| `xtask lint` | `ok lint：4 条事故规则全部满足` |
| 验收矩阵 | **27/27 PASS**（SKIP 的行会自己说出来，不算成立） |
| 变异扫描 | **26/26 KILLED**，误杀 0 |
| 基线 | pass 1.0 / rework 0.01 / false-kill 0.0 / solver_fail 0 / cache 0.901 |
| `cargo test --workspace` | 419 passed / 0 failed（orbit-pi 单跑 1930 passed，1 条已知设计差异红） |

---

## 2. 方案（完整版）

> 这一节是**最初定的方案**，一字不改地保留判断。实施中的修订放在 §2.6。

### 2.1 一句话

> codewhale 提供"骨头"（沙箱、快照、协议、审批），jcode 提供"神经"（steer 注入点、journal 重放），
> dim 提供"记性"（time-bus 语义），codex 提供"语言规范"（协议形状），openhuman 提供"刹车"（缓存守卫）。
> 五者通过 4 个接缝缝合，**engine 内核一行不改**。

### 2.2 三种融合深度（"不返工"的核心）

| 深度 | 含义 | 适用 | 风险 |
|---|---|---|---|
| **D1 依赖** | **进程依赖**：spawn 引擎的 app-server，只走 JSON-RPC / HTTP | 引擎全部能力 | 零源码耦合，升级 = 换二进制 |
| **D2 缝合** | 在我们自己的 crate 里写适配器，调它的 public API | steer / 协议映射 / model 路由 / 缓存守卫 | 低 |
| **D3 移植** | 抄源码进来（带来源注释） | dim time-bus 语义、jcode injection point | 中，要记来源 |
| **D4 改 engine 源码** | —— | **禁止** | 唯一会导致返工的操作 |

`scripts/check-engine.sh` 是 D4 红线的物理保障：工作区里任何 crate 依赖引擎、或 path 依赖逃出仓库 → 直接红。

### 2.3 架构图

```
┌────────────────────────────────────────────────────────────┐
│  orbit（桌面 UI，apps/orbit，不 fork 不重构）                │
└────────────────────────────────────────────────────────────┘
        │ 接缝①：orbit ⇄ harnessd（35 命令 + SSE 事件流）
        ▼
┌────────────────────────────────────────────────────────────┐
│  harnessd —— 我们自己的 crate                                │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ 适配层  orbit-compat    35 命令 → thread/turn 映射    │  │
│  │        model-router     provider/模型/effort 档位      │  │
│  ├──────────────────────────────────────────────────────┤  │
│  │ 融合层                                                │  │
│  │   steer-gate   ← jcode 注入点                         │  │
│  │   time-bus     ← dim time-bus 六条                    │  │
│  │   cache-guard  ← openhuman PromptCacheGuard           │  │
│  │   guards       ← 现项目 guards crate（三守卫）         │  │
│  │   hook-host    ← Go/任意语言插件（stdio JSON-RPC）      │  │
│  ├──────────────────────────────────────────────────────┤  │
│  │ codewhale engine（55.8 万行，D1 进程依赖，零改动）      │  │
│  └──────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────┘
```

### 2.4 五源分工（谁贡献什么，为什么是它）

| 来源 | 贡献 | 为什么 |
|---|---|---|
| **codewhale** | 执行骨架：turn 循环、工具、sandbox（seatbelt+bwrap）、execpolicy、每轮快照、审批、LSP、compaction、checkpoint | 唯一七机制全绿的候选；MIT；DeepSeek 原生；协议兼容 codex |
| **jcode** | steer 注入点、`batch` 并行上限、journal 式重放、provider 插件化范本 | steer 有两处精确注入点（`turn_streaming_mpsc.rs:1431-1450`），比 codex 的 `turn/steer` 更细；但它缺沙箱/快照，只能供机制不能当底座 |
| **dim** | time-bus 六条 + `context_policy` 保留窗口 | 与我们的 L0 近同构，是全场最好的时间线语义；但 alpha + rutis 依赖 → 只抄语义 |
| **codex** | 协议语言规范（JSON-RPC 无 `jsonrpc` 字段、反向 request、通知 vs 应答）、`item/*` delta 粒度、execpolicy DSL、apply_patch 格式 | codewhale 的协议本来就是它的分叉，沿用等于免费拿到生态兼容 |
| **openhuman** | `PromptCacheGuardMiddleware`（缓存前缀冻结 + miss 归因）、TokenJuice 压缩 | 缓存命中是实测 96.9% 的关键；它是唯一把"何时不能压缩"写进中间件的实现 |
| **grok-build** | 两遍压缩（先摘要再重写）、逐子进程 seccomp | 压缩质量明显更高；仓库已停更，只抄算法 |
| **现项目** | 三守卫（失败熔断 / 阶梯 / 只读螺旋）、收据与 uncertain 阻塞、effort 档位与 Magpie 映射 | 已在真机跑出来的东西，`crates/guards` 可直接搬 |
| **Go 插件层** | 工具、集成、hooks、审批判定 | owner 的语言偏好；进程外崩溃隔离，跨平台单二进制 |

### 2.5 五个 Sprint（原始排期）

| Sprint | 内容 | 深度 | 验收 | 工期 |
|---|---|---|---|---|
| **S0 地基** | 引擎不进 `Cargo.toml`，只走外部二进制 + 协议 | D1 | 两仓独立构建；harnessd 通过协议完成一次策略判断 | 2 天 |
| **S1 接缝① 打通** | 35 命令映射；SSE 事件流；Prompt 能真跑一轮 | D2 | orbit 能发一条消息，看到流式回复和工具调用 | 5 天 |
| **S2 模型 + 会话** | model-router、`thread/fork`、`GetEntries`、session 持久化 | D2 | 切换模型/档位生效；fork 能跑到不同结论 | 4 天 |
| **S3 融合件** | steer-gate、guards 三件、cache-guard、time-bus log | D2+D3 | 流式中途插话生效；10 个长任务不被误杀；压缩前后缓存命中 >90% | 6 天 |
| **S4 插件层** | hook-host（Go stdio）、握手、崩溃隔离、6 个事件 | D2 | 写一个 Go 插件，deny 一条命令，内核记录并继续 | 5 天 |
| **S5 评测与固化** | 20 任务评测集 + 跑分器 + 回归门 | — | 通过率/返工率/误杀率有基线数字 | 4 天 |

（26 个工作日。S1 结束就能"看见东西"，刻意安排——早看见能早纠正方向。）

### 2.6 四个接缝的设计要点

**接缝① orbit ⇄ harnessd：35 条命令全部映射，一个不删，UI 一行不改。**

- `Prompt` → `thread/start` + `turn/start`（`turn/start` 自带 `model`/`effort`/`approvalPolicy`）
- `Steer`/`FollowUp` → `turn/steer`；`Abort` → `turn/interrupt`
- `SetModel`/`CycleModel`/`SetThinkingLevel` → 我们自己的状态，下次 `turn/start` 带上
- `GetMessages`/`GetEntries` → `thread/items/list` + 形状转换
- `Fork`/`CloneSession` → `thread/fork`；`Compact` → `thread/compact/start`
- `GetSessionStats` → `thread/tokenUsage/updated` 累积成快照
- **`Auth*` 与 `Quota*` 六条不伪造**：诚实返回"不适用"，UI 上隐藏这些面

**接缝② 融合层 ⇄ engine（只调 public API）**

| 融合件 | 挂载点 | 来源 |
|---|---|---|
| steer-gate | `turn/steer` 注入时机 | jcode + codex |
| guards | 工具执行前的 policy 层 | `crates/guards` |
| cache-guard | 压缩触发前 | openhuman |
| time-bus | 事件出口，统一 append-only log | dim 六条 |
| hook-host | 进程外插件，工具调用前后 | 自定（接缝④） |

**接缝③ 模型层**：provider（deepseek/magpie/anthropic/openai）+ effort 档位映射 + 缓存前缀守卫。
实测引擎是 **9 档**：`auto, off, minimal, low, medium, high, xhigh, ultra, max`（方案原写 7 档，已修正）。

**接缝④ 插件层（Go，stdio JSON-RPC）**，五条规则各对应一次真实失败：

1. `plugin/hello` 必须带 `protocol` 版本；不匹配 → 启动即失败，不降级
2. 插件声明 `caps`，内核复核；**插件不能放宽 execpolicy**
3. 插件崩溃 → 标记不可用，内核不崩；连续 3 次崩溃自动禁用
4. 工具返回 `uncertain` → 收据记 `Uncertain` 并阻塞（防重复副作用）
5. 插件**不得直接写日志/状态**，只能通过返回值（保 time-bus 的"日志是真相"）

可用事件：`tool.pre` / `tool.post` / `turn.start` / `turn.end` / `compact.pre` / `guard.deny`。

### 2.7 实施后的修订（原文保留，改动在此说明）

| 原文 | 实施 | 为什么 |
|---|---|---|
| engine 作为 lib 依赖 | **改成 spawn app-server + 协议**（D1 进程依赖） | 编译面（55.8 万行单 crate 无 feature gate）、升级面（上游重构内部类型不影响我们）、否决权不受影响（审批本来就是反向 request） |
| submodule + "无 diff" CI 检查 | **引擎不进 `Cargo.toml`** + `scripts/check-engine.sh` | 更硬的等价物：编译期就不存在这个依赖，不可能改到源码。代价：部署多带一个二进制 |
| effort 7 档 | **9 档** | 实测引擎枚举 |
| 35 命令 | 36 个变体（`CommandBody` 实际数） | 数了一遍 |

---

## 3. 已经踩过的坑：**不许再犯**（每条都有 lint 或测试守着）

### 3.1 四条事故规则（`crates/xtask/src/lint.rs`，已接进门禁）

| # | 规则 | 对应事故 |
|---|---|---|
| 1 | `http_engine::thread_body()` 必须含字面量 `body["auto_approve"]` 兜底 | 引擎 `auto_approve:false` 时发 `approval.required` 后只发 keepalive，而 harnessd 不实现审批 UI → 回合永远 `in_progress`，用户看到"一直没反应" |
| 2 | `ensure_stream()` 必须比对 `same_thread` | `thread/start` 换了线程但没换订阅 → 新会话的回合永远等不到 `turn.completed` |
| 3 | `drain_async_events` 必须 per-batch 调 `on_batch` | 事件攒到回合末才吐 → 用户"只看到结束，看不到过程" |
| 4 | 启动脚本里不许出现 `auto_approve.*false`（探针除外） | 同上第 1 条 |

**红方向必须自己验过**：去掉兜底后要看到 `!! http_engine::thread_body() 没有兜底 auto_approve` 和 `FAIL lint：1 条规则没满足`。曾经因为规则 grep 的词被自己的注释命中，红方向静默变绿——所以规则用字面量匹配。

### 3.2 必须记住的不变量

1. **回合结束的权威信号是引擎的 `turn.completed`**。`turn.failed`/`turn.cancelled` 在 `http_engine.rs` 里归一化。
   `turn.end` 日志只有一个写点（`server.rs:461-462`）；`SafePoint::TurnDone` 是下游副作用；
   drain 的 idle/timeout 退出、`is_terminal()` 的 `agent_end`/`error` **一个字都不写日志**。
2. **订阅线程必须等于当前线程**。SSE 是**单订阅者**，且订阅必须先于 POST。
3. **事件名和方法名是两套方言**：`ui_kind_for()` 投影的是事件名（`usage`、`text_delta`）；
   `on_notification` 收到的是引擎方法名（`thread/tokenUsage/updated`、`item/agentMessage/delta`）。
   **匹配错一个，静默得到 0 条**——这个坑踩过两次（`agent_settled`、`usage`）。
4. **事件要边产生边写 + flush**，不许攒。
5. **`log.project().turn_open` 只是保守近似**，有已知假开启（`turn.start` 写在 `engine.request()` 之前，
   请求被拒就永远开着）。真正的真相仍是 `turn.completed`。

### 3.3 界面侧的形状要求（错一个就白屏 / 空列表）

- `ChatMessage::from_value` 需要**顶层 `role`**
- 文本必须在 `content: [{type:"text", text}]`
- 模型条目需要 `id` + `name` + `provider`（三个 `?` 在 `filter_map` 里，缺一个整条消失）
- 会话统计需要 `{"contextUsage":{tokens,contextWindow,percent}}`

### 3.4 自研传输一律禁止（已清零，别加回来）

- `grep -rn 'Command::new("curl")'` 全仓为空（harnessd 早期、desktop 都是 curl 子进程，已换成 reqwest / reqwest-eventsource）
- CLI 的 WS 用 `tungstenite`；曾有手写 SHA-1/base64 + 手写 HTTP 解析，已删
- **已知未改**：`crates/cli` 的普通 HTTP 仍是手写解析。实测 `tiny_http` 不可行
  （`Protocol(SecWebSocketAcceptKeyMismatch)`，它自己写 101 但 tungstenite 要算 `Sec-WebSocket-Accept`）。
  **需要改成 axum + tokio-tungstenite，或拆两个端口**。这是"待办"，不是"已完成"。

---

## 4. 当前状态：什么能用，什么不能

### 能用的（真引擎实测过）

- 发消息 → 流式思考 + 工具调用 + 文本，**边产生边显示**（不再等回合结束）
- 活动条三行结构：`<工具名> <命令前 72 字>` / `│ 思考末行或一言` / 右侧 `已工作 · tok/s`
- 一言（16 条中文名句，每 6 秒轮换）用 `ShimmerText` 彩色流动
- 新建会话 / 切会话 / fork / resume 由同一条不变量管着（3 个真引擎探针 PASS）
- 模型列表、上下文用量百分比接的是真实数值
- 审批卡死已修（`auto_approve` 默认 true）+ 探针守着

### 不能用的 / 已知缺陷（**照实写，别当成能做**）

| 缺口 | 位置 | 严重度 | 说明 |
|---|---|---|---|
| 中途切会话（第 5 格） | `server.rs` 状态机 | 高 | 已实测为真缺陷：回合进行中切走，订阅被换，原回合的结果落到新线程。修法已有依据：用 `log.project().turn_open` 保守判断 + `turn.completed` 校正 |
| CLI 普通 HTTP 手写解析 | `crates/cli/src/server.rs` | 中 | 需换 axum/tokio-tungstenite 或双端口 |
| `session_park_cancels_custom_ui_with_a_top_level_flag` | orbit-pi | 中 | 设计差异：缺 extension-UI 子协议。要么补协议，要么正式声明不支持 |
| §9 UI 探针 SKIP | `scripts/acceptance.sh` | 中 | 只在屏幕解锁时跑；锁屏下 SKIP 会自己说出来 |
| ~~界面"一阵一阵"~~ | `transcript_view.rs` | — | **已修**（见 §5.1）。根因不是时序，是"调试标记只挂了第一行"掩盖了布局差 |
| zh-CN.yml 还有 ~158 个 `pi` 字样 | `apps/orbit/crates/orbit-pi/locales/` | 低 | 需要逐个判断，不能全局替换 |
| X/Twitter 上的候选调研 | — | 低 | 需要登录态 |

---

## 5. 接手后请优先做的三件事

### 5.1 界面"一阵一阵"（**已修，2026-11-21**）

用户原话："思考和工具调用不连续，思考完要停顿一下，然后才显示工具调用，然后工具调用后又停顿一下，参考 workbuddy，让这个连续起来。"

**根因不是时序**（事件从 `drain_async_events` 起就是边产生边 flush 的），是布局在跳：
运行中活动组展开（`show_work = paint.live || …`），组内每张工具卡片又按
`agent_calls.open_by_default(false)` 自动展开，参数和输出一起摊开 —— 每来一个工具就长高一大块。

**修法**：运行中不给卡片自动展开。
```rust
let tool_open = explicit_open || (!live && agent_calls.open_by_default(false));
```
跑的时候每张卡片只有一行标题（当前命令本来就在活动条第一行），用户自己点开的照旧展开，
回合结束后再按偏好显示。改完 `cargo test -p orbit-pi` **1933 passed / 1 failed**
（唯一红的是已知的 `session_park_cancels_custom_ui_with_a_top_level_flag`）。

**顺带修掉一个真 bug（这是上一轮卡住的原因）**：
`debug_selector("working-indicator")` 之前挂在三行结构的**第一行**头上，
于是"指示器底部"落在第二行上方 —— 贴底距离凭空多一行（24px → 46px），
既让测试红，也会让滚动贴底的判断偏移。标记已挪到整个指示器容器上。
上一轮两次改法被否，其实不是产品意图冲突，是这个 bug 把指标搅了。

### 5.2 中途切会话（第 5 格）

真缺陷，`docs/KERNEL-STATUS.md` 有 7 行状态表和实测记录。修法依据已查清（§3.2 第 1、5 条）。

### 5.3 CLI HTTP 换库

把手写 HTTP 解析换成 axum + tokio-tungstenite（或双端口）。
注意：不能为了换库而破坏 `crates/cli/tests/relay_e2e.rs` 的两个 e2e（它们真抓过两个 bug：
`BufReader` 预读 WS 帧、非阻塞 socket 的 `EAGAIN`）。

---

## 6. 干活时的规矩（owner 明确要求过的）

1. **先有验收，再动实现**。写不出会红的断言，就别改代码。
2. **红方向必须自己验一遍**。改坏了要真红，不能靠"我觉得"。
3. **空比红更危险**。SKIP、`|| echo 0`、静默跳过，都必须在输出里说出来。
4. **不许伪造**。`Auth*`/`Quota*` 这类不适用就诚实返回不适用，别编假实现。
5. **不许自研已有轮子**，先调研再二开。
6. **只 Rust + Go**。
7. 变更后跑：`xtask lint` → `gate.sh` →（必要时）`MUTATE=1 gate.sh`。

---

## 7. 相关文档索引

| 文档 | 内容 |
|---|---|
| `docs/KERNEL-FUSION-PLAN.md` | **方案原文**（本文 §2 的来源，带修订表） |
| `docs/KERNEL-STATUS.md` | 1483 行施工日志：每个结论的文件行号、7 行线程×订阅状态表、已知缺口 |
| `docs/KERNEL-RESEARCH-AND-PLAN.md` | 选型调研（为什么是 codewhale 当底座） |
| `docs/KERNEL-PLAN-V3.md` | V3 计划与 20 任务评测集 |
| `docs/KERNEL-ANALYSIS-V2.md` | 七个机制的全绿分析 |
| `docs/orbit-harness.md` | orbit 侧集成与教训 |
| `IMPLEMENTATION.md` | 最初的需求文档 |
| `docs/EVIDENCE.md` | 证据与跑分 |
