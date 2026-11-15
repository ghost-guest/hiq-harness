# personal-AgentHarness 实施文档

> 基于 69 个开源 harness 项目源码调研的落地计划。
> 语言：**Rust（核心）+ Go（插件/工具层）**，不使用其他语言。

生成日期：2026-11-15
工作目录：`/Users/mac_522/Desktop/personal-Agentharness`

---

## 0. 一页速览

| 项 | 决定 |
|---|---|
| 核心语言 | Rust 1.94 |
| 插件层语言 | Go 1.27.4 |
| 桌面 | Tauri（Rust 后端 + Web 前端），后期 |
| 目录布局 | Cargo workspace + Go module 并列 |
| 第一个里程碑 | 事件日志 + 最小循环 + 三个守卫（2–3 天） |
| 核心原则 | **日志是事实，其余都是投影** |

七层架构，从下往上建：

```
L7 扩展      kiso 单文件五钩子
L6 编排      LoopForge SOP + Alma 三层任务
L5 持久化    kiso + LoopForge 租约 + Codewhale 拒绝交出
L4 记忆      Codewhale 准入 + ownmem 棘轮 + Alma 睡眠
L3 压缩      opencode 触发 + maka 校验 + Deva 三段式
L2 守卫      maka + Raven + 天枢
L1 循环      Codewhale 四阶段 + opencode 末轮技巧
L0 日志      kiso 三契约 + beacon seq + Deva mtime
```

---

## 1. 环境准备

本机已具备（核实过）：

```bash
rustc 1.94.0
cargo 1.94.0
go 1.27.4 (darwin/arm64)
git 2.50.1
```

`/opt/homebrew/bin` 和 `~/.cargo/bin` 不在默认 PATH 里，命令前加：

```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
```

建议装几个必用的 Rust 工具：

```bash
cargo install cargo-watch    # 增量编译监控
cargo install cargo-nextest  # 更快的测试
```

Go 侧不需要额外工具。

---

## 2. 目录结构

```
personal-Agentharness/
├── IMPLEMENTATION.md          # 本文档
├── Cargo.toml                 # Rust workspace 根
├── rust-toolchain.toml        # 锁定 Rust 版本
├── rustfmt.toml
├── .gitignore
│
├── crates/
│   ├── core/                  # L0 + L1：事件日志、消息类型、循环
│   ├── guards/                # L2：守卫
│   ├── context/               # L3：预算、压缩
│   ├── memory/                # L4：记忆
│   ├── persist/               # L5：持久化、检查点
│   ├── orchestrate/           # L6：SOP 编排
│   ├── plugin/                # L7：扩展宿主（调 Go 子进程）
│   └── cli/                   # 二进制入口
│
├── plugins/                   # Go 插件层
│   ├── go.mod
│   ├── proto/                 # 与 Rust 的协议定义
│   └── builtin/
│       └── example/           # 示例插件
│
└── docs/
    ├── adr/                   # 架构决策记录
    └── layers/                # 各层设计细节
```

为什么 Rust 和 Go 并列而不是嵌在一起：两者通过**进程边界 + 协议**通信（JSON-RPC over stdio），这比 FFI 或 CGO 简单得多，且能崩溃隔离。这也是 kiso 和 Codewhale 的选择。

---

## 3. 分阶段实施计划

### 阶段一：地基（2–3 天）

**目标：能跑通一个完整回合，且有守卫。**

| 步骤 | 内容 | 工作量 | 验收 |
|---|---|---|---|
| 1.1 | L0 事件日志 | 半天 | 能 append 事件、能重放出消息列表 |
| 1.2 | L1 最小循环 | 一天 | 能完成「读文件 → 调模型 → 写文件」 |
| 1.3 | L2 守卫三件套 | 两小时 | 人为造死循环，能被拦下 |

做完这三步就有了能用的骨架，**不要继续往下写别的**，先拿真实任务跑一次。

### 阶段二：上下文（3–4 天）

| 步骤 | 内容 | 验收 |
|---|---|---|
| 2.1 | token 估算 + 校准 | 估算误差可观测 |
| 2.2 | 三段式工具结果降级 | 长会话不爆上下文 |
| 2.3 | 压缩触发与摘要校验 | 缓存命中率 >90% |

### 阶段三：记忆（5–7 天）

| 步骤 | 内容 | 验收 |
|---|---|---|
| 3.1 | 记忆存储与 schema | 项目内 `.harness/memory/` |
| 3.2 | 准入控制 | 密钥不落盘 |
| 3.3 | 检索（RRF 三路） | 能召回相关记忆 |
| 3.4 | 睡眠整合 | 定时归档，五类原因可统计 |

### 阶段四：持久化（3–4 天）

| 步骤 | 内容 | 验收 |
|---|---|---|
| 4.1 | 检查点 | kill -9 后能续 |
| 4.2 | 租约与代次围栏 | 两个实例不打架 |
| 4.3 | 记忆失效拒绝交出状态 | 失效时 state 为 null |

### 阶段五：编排（5–7 天）

| 步骤 | 内容 | 验收 |
|---|---|---|
| 5.1 | SOP 五节点 DAG | 分支并行不等最慢 |
| 5.2 | 人工网关 | 能暂停、批准、打回 |
| 5.3 | 前置能力声明 | 缺失在运行前暴露 |

### 阶段六：扩展（3 天）

| 步骤 | 内容 | 验收 |
|---|---|---|
| 6.1 | Go 插件协议 | JSON-RPC over stdio |
| 6.2 | 钩子面（五类） | plugin 能挂 tools/hooks/approvals |
| 6.3 | 安全规则 | 项目扩展不能覆盖内建 |

**扩展一定放最后。** 前五个机制形状稳定后，钩子面自然清楚。反过来会全部返工。

---

## 4. 各层实现规范

### L0 事件日志

**契约三条（来自 kiso）**：

1. **轨迹即产物**：append-only、带 `seq`。模型看到的消息是日志的**纯函数**。
2. **裁决即事实**：审批结果持久化，`kill -9` 后已决定的不重问。
3. **副作用留收据**：工具调用带 `executionId`；开始了但没报告的是 `uncertain`，**阻塞等人决定**。

**seq 的诚实语义（来自 beacon）**：单写入者单调递增；跨写入者靠时间戳排序，seq 只做 tiebreak。

**关键性能纪律（来自 pi 实测）**：**绝对不要每条事件单独 fsync。** 组提交实测把 26.8 runs/s 提到 250 runs/s。

```rust
// crates/core/src/event.rs 的核心类型
pub struct Event {
    pub seq: u64,
    pub ts: i64,
    pub kind: EventKind,
}

pub enum EventKind {
    UserMessage { text: String },
    AssistantMessage { blocks: Vec<Block> },
    ToolCall { id: String, name: String, input: serde_json::Value },
    ToolResult { call_id: String, output: ToolOutcome },
    Compaction { checkpoint: CheckpointRef },
    PermissionDecided { call_id: String, verdict: Verdict, decided_by: String },
}

pub enum ToolOutcome {
    Ok(String),
    Err(String),
    /// 开始了但没确认 —— 必须阻塞等人
    Uncertain { execution_id: String },
}
```

### L1 主循环

**不设硬上限**（Codewhale 的结论：步数不是进展的度量）。终止靠守卫判断。

**末轮处理**（opencode 的技巧）：禁工具但**保留工具定义**，只为不打碎前缀缓存。

```rust
// 循环骨架
loop {
    let req = build_request(&log.projection(), tools);  // 工具定义始终带上
    let resp = model.call(req).await?;

    match resp.stop_reason {
        Stop::NoToolCalls => break,
        Stop::ToolCalls(calls) => {
            for call in guard.filter(calls) {      // 守卫在这里介入
                let out = execute(call).await;
                log.append(Event::ToolResult { .. });
            }
        }
        Stop::Length => { /* 续写 */ }
    }
}
```

### L2 守卫

**三件套，按优先级**：

**第一：只对相同的失败熔断**（maka，阈值 3）

```rust
// 关键：成功或换调用即重置 streak
// 这样轮询（反复查 status 直到通过）和迭代-重试（改文件后重跑同一测试）
// 天然不受影响，不需要工具白名单
pub struct FailureGate {
    last_failed: Option<Signature>,
    streak: u32,
    threshold: u32,  // 3
}
```

**第二：三级阶梯 + 可解冻**（Raven）

| 级别 | 阈值 | 动作 |
|---|---|---|
| 提示 | 8 次相同答案 | 追加提醒 |
| 拒跑 | 12 次 | 不执行，用拒绝文案替换结果 |
| 终止 | 拒跑 3 次 | 结束这一轮 |

```rust
pub struct NoProgressGuard {
    seen: HashMap<Key, u32>,      // 宽松，驱动提示
    streak: HashMap<Key, u32>,    // 严格，驱动拒跑
    novelty: u64,                 // 全局新证据计数
    frozen: HashMap<Key, Frozen>, // 冻结时可被解冻
    semi: Semantics,              // 8 / 12 / 3
}

// 解冻判定：frozen.novelty != self.novelty 就放行
// 预算：used = refusals + max(0, thawed - 1)
```

**两个必须处理的盲区**：
- 指纹要含 `blocks`（否则「重绘图片再检查」这种真进展会被误判）
- 不同文本不算进展（只认显式 markProgress 和验证类证据）

**第三：只读螺旋**（天枢，12 / 15）

```rust
const EXPLORATION_TOOLS: &[&str] = &[
    "grep", "glob", "read_file", "read_section",
    "semantic_search", "repo_map", "inspect_symbol",
    "find_references", "diff",
];
const ADVISORY_THRESHOLD: u32 = 12;
const HARD_THRESHOLD: u32 = 15;
```

### L3 压缩

**触发常量（opencode）**：

```rust
const SHRINK_STEPS: &[f64] = &[0.85, 0.7, 0.5, 0.35];  // 被拒后逐级降
const RESERVE_SHARE: f64 = 0.15;
const RESERVE_MIN: usize = 16_000;
const PRECOMPUTE_LEAD: usize = 60_000;   // 提前 6 万 token 动手
const CACHE_MARGIN: usize = 30_000;      // 只在缓存快到期时
const SUMMARY_OUTPUT_FLOOR: usize = 32_000;
```

**为什么提前 6 万**：摘要要 30–60 秒，这期间中位数涨几千 token，**但 1/20 的情况涨 25k 以上**（大量文件读取同时到达）。

**三段式工具结果降级（Deva）**：

```rust
pub enum ToolResultTier {
    Full(String),              // 原始全文
    Summary(String),           // 一行摘要
    DiskRef { path: PathBuf, bytes: usize },  // 磁盘引用
}
```

**摘要格式校验（maka）**：

| 规则 | 值 |
|---|---|
| 必需段落 | Goal / Progress / Next Steps / **Critical Context** |
| 回显防护 | 模板行不算内容，照抄格式不能通过 |
| 大折叠下限 | >10k 输入要求 ≥200 输出 |
| 截断检测 | 结尾是省略号/逗号/冒号即判中途停止 |

**缓存铁律（Codewhale）**：header 冻结、历史只增、未命中必须可归因。

### L4 记忆

**存储位置**：项目内 `.harness/memory/`（ownmem、天枢的结论——按 cwd 哈希放家目录会成孤儿）。

**准入（Codewhale）**：

```rust
/// Access 结构刻意不实现 Deserialize
/// 理由：请求体不能创造或放大授权
pub struct Access {
    caps: CapSet,       // 11 个能力位
    scopes: Vec<Scope>, // 精确匹配，无通配，最多 64 个
}
```

写入前做密钥正则拦截：PEM / sk- / gh* / AKIA / xox / JWT / bearer / api_key=。

**睡眠整合（Alma）**：

```rust
pub enum ArchiveReason {
    Exact,      // 完全重复
    Expired,    // 过期
    Orphan,     // 来源已不存在
    Similarity, // 相似需合并
    Llm,        // 模型判断
}

// 归档条目必须留 merged_into
pub struct ArchivedMemory {
    archived_reason: ArchiveReason,
    archived_by: String,
    merged_into: Option<Id>,  // 追溯「从哪条合并来的」
}
```

**配额棘轮（ownmem）**：只降不升。

```rust
// 活跃主题达到 50 后锁死，之后上调直接报错
// 满了给「提名清单」由人决定，不自动淘汰
```

### L5 持久化

**租约 + 代次围栏（LoopForge）**：

```rust
pub struct Lease {
    owner_instance: String,
    lease_until: i64,
    generation: u64,  // 围栏：防旧实例复活后写乱
}
```

**记忆失效则拒绝交出状态（Codewhale，最重要的一条）**：

```rust
pub struct ResumeResult {
    pub state_usable: bool,           // invalidated_ids.is_empty()
    pub state: Option<State>,          // 不可用时为 null
    pub invalidated_memory_ids: Vec<Id>,
    pub restores_permissions: bool,    // 恒为 false
    pub reconcile_pending_operations: bool,
}
```

理由：**记忆不成立，恢复出的状态就是错的。宁可不恢复。**

**存活契约（Codewhale）**：
- 尾部占阈值剩余 1/5，上限 20,000 tokens
- 工具批次与其结果永不切开
- 未决调用阻止跨批切分
- 原始历史先存盘再改写
- 失败压缩不得替换活历史
- 第二次压缩复写而非堆叠

### L6 编排

**SOP 五节点（LoopForge）**：

| 节点 | 作用 |
|---|---|
| Agent | 角色（提示词、执行器、模型、规则） |
| **Script** | 确定性 shell：build/test/lint —— **No AI where AI is not needed** |
| HumanGate | 暂停等人批准或打回 |
| SubSop | 子工作流 |
| PreviewBuild | 构建并归档供审阅 |

**就绪判定**：节点 pending 且所有上游 `succeeded`。

**分支不等同批最慢**：「Branches run as soon as their dependencies succeed」。

**前置能力声明**：缺失的 Git Token / MCP 配置在**首次运行前**暴露，不是流水线中途。

**任务三层（Alma）**：mission → sprint → run，配结构化 `packet` 交接。

### L7 扩展

**单文件五钩子（kiso）**：一个插件同时提供 tools、hooks、approvals、compaction 参数、systemPrompt 追加。

**Rust ↔ Go 边界**：JSON-RPC over stdio。Go 插件是独立进程，崩溃不影响宿主。

```go
// plugins/builtin/example/main.go
type Plugin interface {
    Name() string
    Tools() []Tool
    Decide(call Call, ctx Ctx) Verdict  // allow / deny / ask
}
```

**四条安全规则**：

1. **加载必须大声**：坏文件启动即失败，不能静默改变行为
2. **用户扩展可覆盖内建，项目扩展不可**：`a repo you cloned must never silently replace the CLI's own behavior`
3. **策略链单调**：`deny > allow > ask`，`systemPrompt.append` 只追加不替换
4. **抛异常算 ask**：无审批通道时退化为诚实拒绝，不是默认允许

---

## 5. 三个必须避开的坑

| 坑 | 后果 | 解法 |
|---|---|---|
| thinking 模型不给 max_tokens | Anthropic 默认 8192，思考模型一字未出就截断 | 流式 64000 / 非流式 16000 |
| 每条事件单独 fsync | 多开会话必然卡死 | 组提交（实测差 10 倍） |
| 用第二个 LLM 当安全裁判 | 会被生成命令的同一套推理绕过 | 反射闸：拒绝一次，要求补交 justification |

---

## 6. 测试策略

| 层 | 测什么 | 怎么测 |
|---|---|---|
| L0 | 日志重放 | 写入 N 条事件，重放出等价消息列表 |
| L1 | 循环终止 | 假模型返回预设响应，验证退出 |
| L2 | 守卫 | 构造 A-A-A / A-B-A-B / 只读螺旋，各自触发 |
| L3 | 压缩 | 造超长历史，验证降级顺序与缓存命中 |
| L4 | 记忆 | 写入 / 检索 / 归档 / 合并追溯 |
| L5 | 恢复 | kill -9 后 resume，验证不重复执行 |
| L6 | 编排 | 构造 DAG，验证并行与回边 |
| L7 | 插件 | 坏插件启动失败、越权被拒 |

**守卫测试最关键**：每个守卫都要有「应该拦」和「不该拦」两组用例。后者包括轮询、迭代-重试、长等待。

---

## 7. 里程碑与验收

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| **M1** | 骨架 | 真实任务跑通，死循环被拦 |
| M2 | 上下文 | 长会话不爆，缓存命中 >90% |
| M3 | 记忆 | 跨会话召回，密钥不落盘 |
| M4 | 持久 | kill -9 能续，不重复执行 |
| M5 | 编排 | 多节点流程能交付 |
| M6 | 扩展 | 插件可装、越权被拒 |

**M1 是分水岭。** 没跑通之前不要进入 M2。

---

## 8. 与调研的对应关系

每个决定的出处：

| 决定 | 来源 | 文件 |
|---|---|---|
| 日志是事实，其余是投影 | maka | `docs/architecture/llm-compaction-events-log-projection-draft.md` |
| 三契约（轨迹/裁决/收据） | kiso | `docs/durability.md` |
| 组提交 | pi | `packages/durable/docs/group-commit.md` |
| 不设硬上限 | Codewhale | `crates/runtime/src/turn_budget.rs` |
| 末轮禁工具留定义 | opencode | `packages/core/src/session/runner/llm.ts` |
| 只对失败熔断 | maka | `packages/runtime/src/tool-runtime.ts` |
| 三级阶梯 + 解冻 | Raven | `raven/agent/loop/no_progress.py` |
| 只读螺旋 12/15 | 天枢 | `src/agent/exploration-stall.ts` |
| 渐进收缩 + 预压缩 | opencode | `packages/core/src/session/compaction.ts` |
| 三段式降级 | Deva | `~/.deva/data/chats/*.json` 实测 |
| 摘要校验 | maka | `packages/runtime/src/history-compact-summary-validation.ts` |
| 逐字引用报错 | goose | `crates/goose-context-management/src/prompts/compaction.md` |
| 缓存铁律 | Codewhale | `docs/CACHE.md` |
| Access 不可反序列化 | Codewhale | `crates/memory/src/auth.rs` |
| 配额棘轮 | ownmem | `lib/memory-audit.mjs` |
| 睡眠整合 + merged_into | Alma | `chat_threads.db` 的 `memory_sleep_runs`/`memory_archive` |
| 租约 + 代次围栏 | LoopForge | `internal/engine/run_recovery.go` |
| 拒绝交出失效状态 | Codewhale | `crates/memory/src/protocol.rs` |
| SOP 五节点 | LoopForge | `internal/domain/workflow.go` |
| 单文件五钩子 | kiso | `docs/extensions.md` |

---

## 9. 不做的事

明确列出，防止范围失控：

| 不做 | 理由 |
|---|---|
| 多智能体协作 | 先把单 agent 做对；子智能体用同一套循环 + 缩小权限即可，不需要第二套引擎 |
| Web UI / 移动端 | 桌面（Tauri）是终态，前期纯 CLI |
| 向量数据库 | SQLite + FTS5 + 可选 embedding 足够；Milvus/Neo4j 是过度设计 |
| 自研模型适配层 | 用现成的 OpenAI 兼容协议 |
| 插件市场 | 先有插件机制再说分发 |
| 跨机器编排 | 单机做透再扩展 |

---

## 10. 第一天的清单

```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
cd /Users/mac_522/Desktop/personal-Agentharness

# 1. 初始化 Rust workspace
cargo new --lib crates/core
cargo new --bin crates/cli

# 2. 初始化 Go module
cd plugins && go mod init github.com/local/personal-agentharness/plugins

# 3. 写 L0 事件日志（见第 4 节）
# 4. 写 L1 最小循环
# 5. 加 L2 只读螺旋守卫（两小时）

cargo run --bin cli -- --help
```

**当天目标：能读一个文件、调一次模型、写一个文件。**
