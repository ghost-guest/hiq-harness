# 内核方案 V3：按「成功率 / 稳定 / 不返工」重排

> V2 的两条自设约束（R13 Go 插件、R14 内核要小）在本版被撤销。理由见 §0。
> 判据换成你的真实目标：**新项目开发与旧项目维护的成功率高、够稳、不返工。**

---

## 0. 先证伪我自己：R13 / R14 为什么该撤

**R14「内核要小」——撤销。**
它是「可读性」偏好，不是成功率的因。反证：`final-plan.html` 里的关键数据是
**只换外层循环，114 个任务 PassRate 51.8 → 80.7**，而 codex 的 core 是 14.6 万行（非测试）。
体量与成功率没有因果关系；把内核压到 2 万行也不会提高一个点的通过率。

**R13「Go 插件」——降级为偏好，不是硬门槛。**
插件用什么语言写，不改变工具质量、不改变上下文管理、不改变安全边界。
它是「扩展怎么分发」的问题，不是「任务能不能做成」的问题。
保留：真要做扩展层时，Go 仍是首选（单二进制、跨平台、你熟）。

**撤销后的代价与收益：**
- 代价：候选池从 6 个扩到 12 个；dim / jcode / grok-build 重新入池。
- 收益：不再用架构洁癖筛掉本来最能提成功率的东西。

---

## 1. 把「成功率高」拆成 7 个可验证机制

不能直接说「这个项目成功率高」——那是结论不是证据。拆成机制后每一项都能指认代码。

| # | 机制 | 为什么它决定成功率 | 怎么验 |
|---|---|---|---|
| M1 | 安全执行环境（OS sandbox） | 没有沙箱，agent 不敢跑 build/test，只能靠猜 → 旧项目最容易改坏 | 构造越权命令，看是否被拒且可解释 |
| M2 | 每轮工作区快照/回滚 | 改坏了能退回去，才敢做多步重构 | 连续 10 轮改动后 `/restore` 能精确回到第 3 轮 |
| M3 | 精确编辑（apply_patch + 模糊匹配 + LSP/diagnostics） | 整文件重写是小项目玩具；真实仓库要按 hunk 改，改完立刻知道类型错误 | 在 500 文件仓库上改 3 处，编译错误数为 0 |
| M4 | 上下文工程（压缩 + 缓存前缀 + 大结果外置） | 长任务 90% 的失败是「上下文烂尾」，不是模型不行 | 长会话稳态缓存命中 >90%，压缩后任务继续跑完 |
| M5 | 崩溃恢复（收据 + 检查点 + uncertain 阻塞） | 一崩就重跑 = 重复副作用 = 数据损坏 | `kill -9` 后 resume，不重复执行已确认的工具 |
| M6 | 验证回路（plan / review / 测试结果回灌） | 模型必须看到自己的错，否则一次错一路错 | 故意让测试失败，看下一轮是否针对失败点修 |
| M7 | 并行探索（子 agent / 批量工具） | 大仓库定位慢是主要的非模型瓶颈 | 3 个子 agent 并行改不同模块不冲突 |

**这 7 条才是「强 harness」的定义。** 缺任何一条，成功率都会被它封顶。

---

## 2. 机制矩阵（这次把所有分析过的都列上）

✓ = 有且是内核级；△ = 部分/非默认；✗ = 没有；? = 未验证（不敢写 ✓）

| 项目 | M1 沙箱 | M2 快照 | M3 精确编辑 | M4 上下文 | M5 恢复 | M6 验证回路 | M7 并行 | 许可 |
|---|---|---|---|---|---|---|---|---|
| **codewhale** | ✓ `crates/sandbox` | ✓ `crates/snapshot`（每轮 pre/post，side-git） | ✓ apply_patch 2.8k + `crates/lsp` + diagnostics + cargo 失败摘要 | ✓ compaction + cache + 大输出路由 | ✓ checkpoints + receipts | ✓ plan/review + `dev_server_readiness` | ✓ subagent / fleet | MIT |
| **codex** | ✓ seatbelt+landlock+seccomp | △ `thread/revert` 只回退线程，不动工作区 | ✓ apply_patch 4.8k；LSP 未见证据 → △ | ✓ 双实现压缩 | ✓ rollout + 收据 | ✓ `review/start` + guardian | ✓ 子 agent 真并行 | Apache-2.0 |
| **grok-build** | ✓ Landlock/Seatbelt + 逐子进程 seccomp | ✓ rewind 文件状态回滚 | ✓ | ✓ 两遍压缩 | △ | △ | ✓ | Apache-2.0 |
| **jcode** | ✗（自述 "not a sandbox"） | ✗（只有会话快照，不是工作区） | ✓ apply_patch 837 行 | ✓ compaction-core | ✓ journal 重放 | △ | ✓ 同进程多 session | MIT |
| **dim** | △ opt-in 默认关 | ? | △ | ✓ context_policy | ✓ 检查点 | △ | ✓ 分支并行 | MIT |
| **prime-agent** | ✗ | ✗ | △（Python REPL 为唯一工具） | ✓ | ✓ entry 流 | △ | ✓ 递归子 agent | MIT |
| **goose** | ✗ | ✗ | △ | ✓ | ✓ | △ | ✓ | Apache-2.0 |
| **PiG** | ✗ | ✗ | ✓ | ✓ | ✓ | △ | ✓ | MIT |
| **pigo** | ✗ | ✗ | ? | ✓ | ✓ | △ | ✓ | MIT |
| **crush** | ✗ | ✗ | ✓ | ✓ | ✓ | △ | ✓ | FSL-1.1 |
| **opencode** | △ | ✓ 快照 | ✓ | ✓ | ✓ | ✓ | ✓ | MIT |
| **pi** | ✗ | △ | ✓ | ✓ | ✓ | △ | ✓ | MIT |

### 这张表回答了你上一个问题

**为什么上一版没有 grok-build / jcode？**
因为上一版用「能不能当底座」做筛选器，把「机制贡献者」过滤掉了——方法错误。
grok-build 在 M1/M2 上其实和 codex 同级（Landlock + 逐子进程 seccomp + 文件状态 rewind），
jcode 在 M4/M5/M7 上是同类里最好的。它们不该被排除，只是**不适合当底座**（见下）。

**但它们仍不是底座**，原因是交付形态不是能力：
- grok-build：最后提交 2026-09-29，单次快照同步、明确不接受外部贡献 → 你 fork 了也没有上游
- jcode：缺 M1/M2 两条，而这两条恰好是「旧项目维护」的命门；补它 = 自己写出 codewhale 的 sandbox+snapshot

---

## 3. 全绿只有一个：codewhale

七项全 ✓ 的只有 **codewhale**（opencode 是 TS，且 M1 只是 △）。

它为什么是最强候选，四条硬理由：

1. **MIT**，可闭源分发；`app-server-protocol` 是 `MIT AND Apache-2.0`（改编自 codex），带 NOTICE 即可。
2. **协议是 codex v2 的硬分叉**（`CONTRACT_NAME="codewhale-codex-app-server-v2"`，`methods.rs:1-4`）
   → 你的桌面客户端、编辑器、手机都按这个协议接，生态里现成的客户端实现可直接用。
3. **DeepSeek 原生**：`crates/config/src/auto_model.rs:19` 里 `PRO_MODEL = "deepseek-v4-pro"`，
   模型路由按任务复杂度分档（pro / flash）。你的 Magpie/DeepSeek 环境不用写适配器。
4. **M2 是它独有的**：每轮 `pre-turn:<seq>` / `post-turn:<seq>` 侧仓快照
   （`crates/snapshot/src/lib.rs:1-45`），永远不碰用户 `.git`，没有 git 的目录也能快照。
   这是「旧项目维护不返工」最直接的保险。

**它的真实风险（我在 V2 里也写了）：**
- engine 是单 crate 55.8 万行、无 feature gate（`crates/engine/Cargo.toml:45-70`）
- 三套并行 wire：`crates/protocol` / `app-server-protocol/v2.rs`（codegen）/ engine 内部 Op/Event +
  2,025 行穷尽 match 的 `protocol_parity.rs`
- 协议 pin 在 codex `rust-v0.166.0`

**缓解策略（这是「不返工」的关键）：不要重构它的 engine。**
只在外围改：provider 接入、协议导出、UI、插件。engine 内部一行不动。
这样上游升级是 `git merge`，不是重写。

---

## 4. 方案：先测后选，不靠读代码拍板

你的要求是「高成功率 + 稳定 + 不返工」，这三件事**只能测出来**，不能靠读代码推出来。
所以方案分三段。

### Phase 0：建评测集（3 天，先做这个）

20 个任务，全部来自你自己的真实仓库：

| 类型 | 数量 | 来源 | 通过判据 |
|---|---|---|---|
| 新项目 | 10 | 「从空目录做一个 X」类任务 | 能编译 + 有测试 + 测试过 |
| 旧项目维护 | 10 | 从 `personal-Agentharness` / `apps/orbit` 的真实 bug 与 TODO 里挑 | 指定测试从红变绿，且无新增失败 |

指标五个：**通过率 / 首次通过率 / 返工率 / 守卫误杀率 / token 与缓存命中**。

这一步的价值：它同时是「选型依据」和「以后的回归集」。**没有评测集，任何选型都是信仰。**

### Phase 1：两个候选并行跑（2 周）

| 候选 | 改动范围 | 工作量 |
|---|---|---|
| A. codewhale fork | 只换模型接 DeepSeek + 导出 app-server 协议给 orbit + 关掉 TUI/web/cloud | 1 周 |
| B. codex fork | 写 Chat Completions 适配器（`codex-api` 层）+ 摘掉 ChatGPT 账号路径 + 导出协议 | 1.5 周 |

B 作为对照组的意义：如果 codex 只靠改适配器就能跑过 A，那说明 A 的优势不成立，省下 55 万行的维护成本。

### Phase 2：按指标决策（不按喜好）

| 情况 | 决策 |
|---|---|
| A 的通过率 ≥ B，且 A 的返工率更低 | 选 A（codewhale fork），投入产品化 |
| B 明显更高 | 选 B，接受 Responses 适配器的维护成本 |
| 两者都低于你自己定的 60% 基线 | **止损**：说明「换底座」不是瓶颈，问题在提示词/工具/评测集，回到现项目修，不重开 |

**止损线是我主动加的。** 没有它，「重构内核」会变成无底洞——这正是你说的「反复返工」。

---

## 5. 「不返工」的四道闸（比选哪个底座更重要）

| 闸 | 做法 | 防的是什么 |
|---|---|---|
| 1 评测集即回归集 | 任何机制改动都必须跑完 20 个任务，指标不能退 | 改 A 坏 B |
| 2 协议冻结 | `app-server` 客户端协议发布后只做加法；method 加 `experimental` 标注 | 每接一个客户端返工一次 |
| 3 快照/回滚默认开 | 每轮 pre/post 快照，改坏了 `/restore` 回退 | 旧项目被改坏 |
| 4 来源注释 + 不许自发明 | 每个机制写明抄哪个项目哪个文件 | 自己发明的机制没人能维护 |

这四道闸里，**第 1 条是唯一能真正阻止返工的**。前三条是技术，第 4 条是纪律。

---

## 6. 反方举证：这个方案可能错在哪

1. **codewhale 可能扛不住长期维护。** 55.8 万行单 crate、20 个 2 万行以上文件
   （`runtime_threads/manager.rs` 20,937 行、`tools/subagent/mod.rs` 23,909 行）。
   缓解是「只改外围」，但外围改多了，merge 上游时一样痛。
2. **codex 可能被低估。** 它的上游质量、测试密度、协议设计都是最高的；
   Responses-only 是障碍，但不是不可越过的障碍——写一个适配器 ≈ 2 周。
3. **评测集本身可能不可靠。** 20 个任务太少，方差大；我自己写判据也可能偏。
   缓解：每个任务有可执行的通过判据（测试命令），不靠人读。

---

## 7. 结论与下一步

**方案：Phase 0 建评测集 → Phase 1 双候选跑 → Phase 2 按指标决策，带止损线。**
首选候选 **codewhale**（七项机制全绿、MIT、DeepSeek 原生、协议兼容 codex），
对照组 **codex**（验证「是否值得为上游质量付出适配器成本」）。

下一步（按顺序，我可以直接开工）：
1. **写 20 个任务的评测集**（`bench/` 目录 + 每任务的通过脚本 + 跑分器）
2. **codewhale fork 最小可用**（DeepSeek 接入 + 协议导出 + 关掉非必需面）
3. 跑第一轮基线，出数

要我开始第 1 步，还是先把评测集的任务清单列出来给你过一遍？
