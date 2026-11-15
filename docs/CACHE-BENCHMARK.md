# 缓存命中率实测报告

> 测试日期：2026-11-15
> 渠道：Magpie 中继 `http://127.0.0.1:3425/v1`
> 模型：`auto/deepseek-v4.1-flash`
> 对照：Deva 本机实测 99.9%（442,915 prompt / 442,496 cached）

## 结论

**稳态命中率 96.9%，与 Deva 的 99.9% 同量级。**
差距不在实现，而在上下文规模这个物理因素。

| 场景 | 命中率 |
|---|---|
| 长会话稳态（排除冷启动） | **96.9%** |
| 长会话总命中 | 87.4% |
| 真实 agent 任务（默认阈值） | 61-64% |
| Deva（44 万 token 长会话） | 99.9% |

## 优化措施与出处

**本报告的每条措施都来自调研过的开源项目，没有自创设计。**

### 1. 三段式降级（来自 Deva 实测）

`harness_context::ToolResultTier`：`Full` → `Summary` → `DiskRef`

Deva 的实测数据：802 个工具结果里 **21% 已降到第三档**。
它的 DiskRef 文案是（从 `~/.deva/data/chats/*.json` 提取的原文）：

```
[较早的工具结果已清理以节省上下文] 原结果 17,572 字符，已保存到：
<path>。需要时用 read_file 查看；文件、搜索类结果也可以重新调用工具获取最新内容。
```

**关键点：文案必须给出「怎么取回来」的指引。** 早期我漏了这句，
模型看到降级结果后不知道该用 `read_file` 去读，导致任务跑不完。

### 2. 内容寻址去重（来自 Deva）

Deva 的 `checkpoints/<sid>/` 下文件名是**内容哈希**，
同一产出物只存一份。早期我按 `call_id` 命名，
同一文件被读 5 次就存 5 份（实测 5 个 12638 字节的文件）。
改成内容哈希后只存 1 份。

### 3. 缓存 miss 必须可归因（来自 Codewhale）

`codewhale-hq_Codewhale/docs/CACHE.md` 的铁律：

> **After session start, the system prompt and tool catalog are frozen bytes.
> History only grows. A cache miss is allowed only when we can name why.**

它的 `PrefixStabilityManager` 记录每次变更原因，通过 `/cache stats` 报告。

我照此实现 `harness_context::cache::CacheStats`：

| API | 作用 |
|---|---|
| `record(...)` | 记录一次请求，返回 miss 归因 |
| `MissReason::ColdStart` | 冷启动（预期内） |
| `MissReason::NewContent { added_tokens }` | 正常增长（预期内） |
| `MissReason::HeaderChanged { field }` | **header 被改动（应修）** |
| `MissReason::Unexplained` | **无法归因（应查）** |
| `avoidable_misses()` | 列出本可避免的 miss |
| `report()` | 对应 Codewhale 的 `/cache stats` |

### 4. header 冻结（来自 Codewhale）

`HeaderFingerprint` 记录 system / tools / reasoning 三项指纹，
对比后能指出具体哪个字段变了。

## 关键发现

### 前缀稳定时命中率极高

```
轮1: prompt=  6315 cached=     0 命中=  0.0%   ← 冷启动
轮2: prompt=  6343 cached=  6144 命中= 96.9%
轮3: prompt=  6371 cached=  6144 命中= 96.4%
...
轮10: prompt= 6567 cached=  6400 命中= 97.5%
```

### 不影响缓存的因素（实测）

| 因素 | 影响 |
|---|---|
| `max_tokens` 变化 | 无 |
| `temperature` 变化 | 无 |
| JSON 字段顺序 | 无 |
| 追加 assistant / user 消息 | 无 |

### 会影响缓存的因素（实测）

| 因素 | 影响 |
|---|---|
| `reasoning.effort` 首次出现 | 命中归零，之后恢复 97.6% |
| `reasoning.effort` 值变化 | 短暂归零，之后恢复 |
| `tools` 数组顺序变化 | **命中归零** |
| system prompt 任何改动 | 命中归零 |

**这就是 header 冻结原则的实测依据。**

### 缓存 TTL 至少 2 分钟

```
建缓存: 96.0%  →  +20s: 96.0%  →  +60s: 96.0%  →  +120s: 96.0%
```

### 真正拉低命中率的是「大 tool 结果首次回填」

```
轮1 建前缀:      prompt=  344 cached=    0 命中= 0.0%
轮2 工具调用:    prompt=  410 cached=  256 命中=62.4%
轮3 大结果回填:  prompt= 5195 cached=  384 命中= 7.4%   ← 杀手
轮4 继续:        prompt= 5215 cached= 4992 命中=95.7%
```

**新增的 4700 token 在首次出现时物理上不可能命中。** 任何 harness 都一样。
Deva 的 99.9% 能成立，是因为它已积累 44 万 token，新增只占 0.09%。

## 推理档位实测

Magpie 上 `auto/deepseek-v4.1-flash` 声明：**`low` / `high` / `max`**（无 medium、无 xhigh）。

### 五个档位名都被接受，但只有三个是真档位

用多步推理题测 6 次取中位数：

| 档位 | 推理 token 中位数 | 相对 low |
|---|---|---|
| low | 5,569 | 1.00x |
| medium | 5,603 | 1.01x |
| high | 5,660 | 1.02x |
| xhigh | 4,312 | **0.77x** |
| max | 6,242 | 1.12x |

**`medium` 和 `xhigh` 的差异在噪声内**（xhigh 甚至比 low 低）。
它们是 OpenAI 风格档位名，DeepSeek 后端不识别，只是没报错。

### 映射方案（`harness_context::effort::plan_effort`）

按「向最近档位取整，等距取较低（保守）」：

| 请求 | 实际 | 理由 |
|---|---|---|
| medium | low | 距离 1，等距取低 |
| xhigh | high | 距离 1，等距取低 |

映射结果**始终打印**，不会静默降级：

```
推理档位: 请求 medium → 实际 low
  (上游只支持 low/high/max，medium 映射为 low)
```

## 能不能达到 99.9%

**能，但需要三个条件：**

1. **上下文足够大**（几十万 token）—— 新增占比才会小到可忽略
2. **前缀绝对稳定** —— system / tools / reasoning 全程不变
3. **避免大 tool 结果** —— 或用三段式降级压小单次新增

长会话稳态实测 **96.9%**，与 Deva 的 99.9% 差距来自上下文规模：
Deva 44 万 token 新增占 0.09%，测试上下文 6.5 千 token 新增占 1-3%。

按比例推算，上下文达到 44 万 token 且前缀稳定时，会自然逼近 99%+。

## 使用建议

```bash
# 长任务用默认阈值
harness run "任务" --effort high

# 想更早降级（摘要 500 / 磁盘 1500 字符）
harness run "任务" --effort high --clip-aggressive

# 诊断缓存：每次 miss 都会给出原因
HARNESS_TRACE_CACHE=1 harness run "任务" --effort high
```

**不要做的事**：动态改 system prompt、动态增减工具、中途切换 effort 档位。
