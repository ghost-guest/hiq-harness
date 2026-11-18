# 内核重构：全量调研与方案

> 证据基础：`ghost-guest` 的 670 个 star 全量拉取（GitHub API，last page=670 校验一致）+
> 14 个仓库浅克隆逐源码阅读（codex / codewhale / jcode / goose / grok-build / openhuman /
> prime-agent / herdr / dim / codeg / OpenBitFun / pi_agent_rust / PiG / pigo / crush / LoopForge /
> opencode / pi）。所有结论附 `路径:行号`。
>
> 本文与 `IMPLEMENTATION.md` 的关系：那份是七层机制的取舍清单；这份是**选底座**的结论。
> 二者不冲突，但本文假设「可以完全推翻重来」，不迁就现有代码。

---

## 0. 一句话结论

**没有一个项目可以原样当底座。** 每个候选的「强」都落在不同维度，且三个硬约束互相冲突：
体量可维护、协议可驱动、安全可兜底。所以推荐 **方案 A：自建内核，但每一块都移植现成规格**；
只有在「半年内要成品」时才走 **方案 B：fork codewhale 并砍到最小**。

---

## 1. 评估标准（从零选底座，不看现项目兼容性）

| # | 维度 | 判定问题 |
|---|---|---|
| 1 | 是否自有主循环 | 模型调用 → 工具 → 回填 的循环在自己仓库里，还是外包给依赖库/子进程 |
| 2 | 会话模型 | 有没有 session / turn / item 三层；resume、fork、rewind 是否真实现 |
| 3 | 外部协议 | 有没有能被桌面/编辑器驱动的稳定协议；method 数量；有没有版本策略 |
| 4 | 并发 | 运行中插话（steer）、中断、工具并行、子 agent 真并行 |
| 5 | 安全 | OS 级 sandbox、审批流、MCP |
| 6 | 压缩 | 策略与触发条件是否与主循环耦合正确 |
| 7 | 体量 | 核心源码行数；能不能裁出最小核 |
| 8 | 许可 | 能不能闭源分发、能不能商用、有没有 rider |

---

## 2. 全量评估矩阵

| 项目 | 语言/许可 | 自有循环 | 会话模型 | 外部协议 | 并发 | 安全 | 内核体量 | 判定 |
|---|---|---|---|---|---|---|---|---|
| **codex** (openai) | Rust / Apache-2.0 | ✅ 双层 turn 循环 | thread/turn/item 全齐 + resume/fork/revert | app-server 288 method，无版本承诺 | steer/queue/interrupt/并行工具/子 agent 全齐 | seatbelt+landlock+execpolicy+审批+MCP | core 14.6 万行（非测试） | **规格源**（不作底座） |
| **codewhale** | Rust / MIT | ✅ | Thread/Session/Turn/Message 四层 + fork/rewind | codex v2 硬分叉，40 method + HTTP/SSE 274 route + ACP | 真 steer（带回执）/并行工具/多 agent/嵌套 | sandbox(seatbelt+bwrap)/execpolicy/审批/snapshot/MCP | engine 55.8 万行单 crate | **备选底座**（方案 B） |
| **jcode** | Rust / MIT | ✅ 两条循环 | Session/messages，无 Turn 实体；resume/fork/rewind 齐 | 自有 NDJSON + harness-api（v1.11） | soft interrupt 注入点/batch 并行/同进程多 session | 无 OS sandbox，bash 仅启发式 | 100 万行 / 89 crate | 参考 |
| **goose** | Rust / Apache-2.0 | ✅ 状态机 | Session/Conversation，无 Turn；fork=copy+truncate，无 rewind | **ACP**（8 标准 + 112 自定义），有版本协商 | steer/cancel/并行工具/子 agent 真并行 | 无 sandbox；审批三档 + LLM 判定；MCP 原生 | goose 19.4 万行 | 参考（ACP 抄它） |
| **grok-build** | Rust / Apache-2.0 | ✅ | session/turn/item + fork/rewind | ACP + 274 个 `x.ai/*`；leader socket | interject/queue/并行工具/子 agent | Landlock+Seatbelt+seccomp+权限规则 | 197 万行 / 89 crate | 参考（已停更） |
| **pi_agent_rust** | Rust / MIT+Rider | ✅ | append-only entry 流 + resume/fork/rewind | **ACP over stdio**（含 steering/fork/resume） | steering 队列/并行工具/子进程 subagent | 无 sandbox；审批三档 + dual confirm | 109 万行 | **参考（许可 blocked）** |
| **prime-agent** | Rust / MIT | ✅ 流式 | JSONL entry + parent_id 树 → fork/rewind 免费 | ACP v1（8 method）+ MCP client | steer/abort/并行工具/递归子 agent（3 层） | 无 sandbox、无审批门 | 51.2 万行 | **备选底座**（方案 C） |
| **openhuman** | Rust / GPL-3.0 | ❌ 循环在子模块 | thread/turn_state；resume/fork，无 rewind | JSON-RPC 2.0 + 523 控制器 + SSE | steer/并行工具/子 agent 3 层 | 审批三档 + egress + 注入防护 | core 75.5 万行 | 参考（GPL + 循环不在本仓） |
| **herdr** | Rust / Apache-2.0 | ❌ 无模型调用 | Workspace/Tab/Pane + session 快照 | NDJSON socket，102 method，协议版本 22 | 只有 pane 级进程并行 | socket 0600 无 token | 29.1 万行 | 参考（PTY/attach 层） |
| **dim** | Rust / MIT | ✅ | Session/Branch/Turn/Item 时间线 | JSON-RPC 2.0，113 method，协议 v4 | steer/interrupt/分支并行 | sandbox 默认关；审批事件化 | 38.9 万行 | 参考（时间线语义） |
| **codeg** | Rust+TS / Apache-2.0 | ❌ 驱动外部 agent | UnifiedMessage/Turn | ACP v1（含 steering 扩展） | 每 agent 独立子进程 + worktree | 代执行 fs/terminal + 透传审批 | Rust 67.4 万行 | 参考（多 agent 编排） |
| **OpenBitFun** | Rust+TS / MIT | ✅ | Session/Turn/ModelRound/Message | ACP（5 个 method 未实现）+ App Server v3 + IPC | steer/cancel/并行工具/子 agent | 无 sandbox | core 39 万行 | 参考 |
| **PiG** | Go / MIT | ✅ | JSONL entry + parentId 分支 | 纯 CLI | steer/并行工具 + 写操作串行化/子 agent | 未见 sandbox | 253 万行 | 参考（Go 侧最完整） |
| **pigo** | Go / MIT | ✅ 最清晰的 loop | 一 session 一 jsonl | WebSocket 远程控制 + 扩展 JSON-RPC | steer/并行工具/子进程 subagent | 无 | 7.6 万行 | **参考（循环教科书）** |
| **crush** | Go / **FSL-1.1** | ❌ 循环在 fantasy 库 | SQLite session | REST + SSE + unix socket | steer 最扎实/并行工具/子 agent | 有审批 | 965 文件 | **放弃（FSL 竞争窗 + 循环外包）** |
| **LoopForge** | Go / MIT | ❌ 纯 DAG | 状态外包给外部 CLI | HTTP API + MCP server | 分支并行/租约调度 | 沙箱 + human gate | 28.2 万行 | 参考（L6 编排） |
| **opencode** | TS / MIT | ✅ | session/message/part | server/client 分层 + plugin host | part 增量/权限持久化 | 审批持久化 + LSP | 5862 TS 文件 | 参考（数据形状） |
| **pi** | TS / MIT | ✅ | entry 流 + tree | ACP | outer loop(follow-up) + inner loop(tools+steering) | 审批 hook | 3549 TS 文件 | **参考（循环骨架）** |

---

## 3. 三个硬约束，以及为什么它们互斥

**约束一：体量可维护。**
能当底座的仓库里，内核最小的也是 51 万行（prime-agent），最大的 197 万行（grok-build）。
唯一「小」的是 pigo（7.6 万行，Go），但它没有协议面、没有 sandbox。
`codewhale` 的 engine 是 **单 crate 55.8 万行且无 feature gate**（`crates/engine/Cargo.toml:45-70`），
想裁出最小核只能手动删源码。

**约束二：协议可驱动。**
你的桌面是 GPUI（orbit 血统），它需要的是「能被外部客户端驱动的内核」。
这一条把候选分成两半：有协议面的（codex / codewhale / goose / grok-build / dim / pi_agent_rust / prime-agent），
和只有 CLI 的（PiG / pigo / 以及一堆 TUI）。

**约束三：安全可兜底。**
**只有两个项目有真正的 OS 级 sandbox**：codex（seatbelt + landlock + seccomp）与
codewhale（sandbox crate，seatbelt + bwrap）。grok-build 有但已停更。
其余全是「审批 + 启发式」：jcode 自己文档写着 *"defense in depth, not a sandbox"*
（`crates/jcode-command-risk/src/lib.rs:3-34`），goose / prime-agent / OpenBitFun / PiG 完全没有。

三者交集为空：
- 有 sandbox 的两个（codex / codewhale）＝ 体量最大
- 体量最小的（pigo）＝ 无协议无 sandbox
- 协议最干净的（goose ACP）＝ 无 sandbox + 19 万行产品层

**所以：要么接受一个大仓库，要么自己拼。**

---

## 4. 三个候选方案

### 方案 A（推荐）：自建内核，逐块移植规格

不 fork 任何仓库。内核自己写，但**每一块的规格来自某个具体项目的具体文件**，
按 `AGENTS.md` 第一原则「先调研 → 再对比 → 挑最优 → 记来源」执行。

| 内核部件 | 移植来源 | 规格文件 |
|---|---|---|
| 会话模型 thread/turn/item | codex | `codex-rs/app-server-protocol/src/protocol/v2/thread_data.rs:205/392`、`v2/item.rs:258` |
| 事件日志 + 投影 | 现项目 L0 已对齐 dim 的时间线语义 | `dim/docs/architecture/rust-agent/time-bus.md` |
| 协议形状（JSON-RPC 无 jsonrpc 字段、反向 request、通知） | codex app-server | `app-server-protocol/src/rpc.rs:1-2`、`protocol/common.rs` |
| 协议版本与兼容测试方法 | codewhale | `app-server-protocol/src/methods.rs:1-4` + `tests/contract.rs:26` |
| 编辑器/桌面接入面 | goose ACP | `goose/crates/goose/src/acp/server/dispatch.rs:31-434` |
| steer 注入点 | jcode / pi | `jcode/crates/jcode-app-core/src/agent/turn_streaming_mpsc.rs:1431-1450`；`pi/packages/agent/src/agent-loop.ts:184-313` |
| 工具并行分批 | codewhale | `engine/src/core/engine/turn_loop.rs:3407-3563` |
| 子 agent 递归 | prime-agent / codex | `prime-agent/.../rlm_max_depth`；`codex core/src/tools/handlers/multi_agents_v2/` |
| sandbox | codex | `sandboxing/src/seatbelt_base_policy.sbpl`、`linux-sandbox/src/seccomp.rs` |
| 命令策略 DSL | codex execpolicy | `execpolicy/src/lib.rs:1-35` |
| 快照/回滚 | codewhale snapshot | `crates/snapshot/src/lib.rs:1-45` |
| 压缩与缓存守卫 | openhuman + jcode | `openhuman/.../PromptCacheGuardMiddleware`；`jcode/crates/jcode-compaction-core/src/lib.rs:6-22` |

**体量目标：8–15k 行核心 Rust**（对比：codex core 14.6 万、codewhale engine 55.8 万），
一个人能读懂、能改、能保证质量。

**工期（一人）：** M1 3 天 / M2 4 天 / M3 5 天 / M4 4 天 / M5 5 天 / M6 3 天 ≈ **24 个工作日**。

**风险：** 自建意味着没有上游帮你修 bug；协议是自己定的，没有生态。

### 方案 B：fork codewhale，砍到最小

- **好处**：MIT、协议最接近 codex、sandbox / execpolicy / 审批 / snapshot / MCP / 多 agent 全有，
  单二进制，Go 插件层可以继续用 JSON-RPC 接。
- **代价**：engine 55.8 万行单 crate、无 feature gate（`crates/engine/Cargo.toml:45-70`），
  三套并行 wire（`crates/protocol`、`app-server-protocol/v2.rs` codegen、engine 内部 Op/Event +
  2025 行穷尽 match 的 `protocol_parity.rs`）。改任何 wire 都是链式改动。
  协议锁死上游 `CODEX_PROTOCOL_TAG="rust-v0.166.0"`（`methods.rs:3-4`）。
- **工期**：裁剪 2 周 + 换 UI/模型 2 周 ≈ **4 周**到能用，但之后每次上游同步都是一次成本。
- **适合**：半年内要成品、且愿意长期跟上游。

### 方案 C：fork prime-agent

- **好处**：MIT、内核六件齐（流式循环 / 树形会话 / 并行工具 / 递归子 agent / 压缩 / ACP），
  树形会话让 fork/rewind 免费（`pa-types/src/session.rs:362` + `pa-core/src/session/tree.rs:16`）。
- **代价**：工具面是「Python REPL 为唯一工具」的 RLM 范式，做编码 agent 要整体重写工具层；
  无 sandbox、无审批门；51 万行。
- **工期**：重写工具层 + 补安全层 ≈ **5–6 周**。

---

## 5. 推荐：方案 A

理由三条，都可证伪：

1. **没有一个候选同时满足三条硬约束**（第 3 节）。fork 哪个都要接受它的短板，
   而短板恰好是「安全」或「体量」这两条最贵的。
2. **内核真正难的部分都已被写成规格**。协议形状、steer 注入点、sandbox 策略、
   压缩触发、快照回滚——每个都有确定的源文件可抄。自建的边际成本主要在
   胶水与测试，不在发明。
3. **你要的是「非常强」，不是「很快有」**。fork 一个 55 万行的仓库，
   半年后你会花大量时间读代码而不是改代码；自建 1 万行，你能记住每一行。

### 「非常强」的可验收定义（先定标准，再写代码）

| 能力 | 验收方式 | 对标 |
|---|---|---|
| 多开不卡 | 10 个会话常驻，单会话增量内存 < 5MB | jcode 3.3MB / Claude Code 169MB |
| 运行中插话 | 流式输出途中注入消息，模型当轮即改方向 | codex `turn/steer` |
| 崩溃可续 | `kill -9` 后 resume，已决定的不重问、未决的阻塞 | kiso 三契约 |
| 安全兜底 | macOS seatbelt 下 deny-by-default；越权命令被拒且可解释 | codex sandbox |
| 协议可驱动 | 桌面/手机/编辑器三个客户端走同一协议，无伪造 method | ACP / app-server |
| 缓存命中 | 长会话稳态 >90%，miss 必须可归因 | Deva 99.9% |
| 守卫不误杀 | 轮询、迭代重试、长等待三种合法长任务不被拦 | maka / Raven / 天枢 |

---

## 6. 方案 A 的里程碑

| 里程碑 | 内容 | 验收 | 工期 |
|---|---|---|---|
| **M1 内核骨架** | 事件日志 + thread/turn/item + 单 turn 流式循环 | 真任务跑通，`kill -9` 能续 | 3 天 |
| **M2 协议面** | JSON-RPC（无 jsonrpc 字段）+ 反向 request + 通知流 | 桌面客户端纯协议驱动，无伪造 | 4 天 |
| **M3 并发** | steer 注入点 + interrupt + 工具分批并行 | 流式中途插话生效；并行工具不乱序 | 5 天 |
| **M4 安全** | seatbelt（macOS）+ execpolicy + 审批流 + MCP | 越权命令被拒且给理由 | 4 天 |
| **M5 上下文** | 压缩（阈值 + 保留窗口）+ 缓存守卫 + 三段式降级 | 长会话不爆，稳态命中 >90% | 5 天 |
| **M6 多 agent** | 子 agent 递归 + 深度上限 + 结果汇总 | 一个任务拆 3 个子 agent 并行完成 | 3 天 |

M1 必须先用真实任务跑一遍再进 M2——这是 `IMPLEMENTATION.md` 已经写对的纪律。

---

## 7. 许可风险表（二开前必须知道）

| 许可 | 项目 | 你能做什么 / 不能做什么 |
|---|---|---|
| MIT | codewhale, jcode, prime-agent, dim, OpenBitFun, PiG, pigo, opencode, pi | 自由使用/修改/闭源分发（保留版权声明） |
| Apache-2.0 | codex, goose, herdr, grok-build, codeg | 同上 + 专利授权 + 必须保留 NOTICE |
| **GPL-3.0** | openhuman | 分发必须开源；自用/内部/SaaS 不触发 |
| **FSL-1.1** | crush | 禁止「竞争使用」；自用允许；满两年自动转 MIT |
| **MIT+Rider** | pi_agent_rust | OpenAI/Anthropic 及其关联方一律不授权；含并入训练语料；违约终止+销毁 |
| AGPL-3.0 | CodexPlusPlus, ClaudeMom | 网络使用也触发开源义务 |

**结论：能放心 fork 的只有 MIT 与 Apache-2.0 那一批。** codewhale 的
`app-server-protocol` 是 `MIT AND Apache-2.0`（改编自 codex），保留 NOTICE 即可。

---

## 8. X 那条线的状态（未完成，需要你定）

试过的通道全部失败：
- `x.com` 直连返回 403；`x.com/search` 内置浏览器 20s 超时
- Bing / DuckDuckGo HTML 端点被墙（000）
- r.jina.ai 代理 000
- Chrome relay 扩展未连接（`Chrome extension is not connected`）

三个可选替代，选一个我就继续：
1. 你在 Chrome 里装 Alma 的 relay 扩展 → 我用你已登录的 X 会话搜
2. 你直接贴 X 上看到的项目链接（我按同一套标准评估）
3. 放弃 X，把 GitHub topic（`agent`, `coding-agent`, `llm-agent`）按 star 排序扫一遍补漏

---

## 9. 下一步（等你拍板）

1. **选方案**：A（自建+移植规格）/ B（fork codewhale）/ C（fork prime-agent）
2. **定「强」的边界**：第 5 节那张表要不要加/减项（比如要不要 LSP、要不要浏览器工具、要不要多机）
3. **X 补漏**：第 8 节三选一

定了这三件，我就把它拆成可执行的 sprint，按 `AGENTS.md` 的「不许自发明」逐块落地。
