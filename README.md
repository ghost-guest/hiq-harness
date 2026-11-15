# hiq-harness

自研 agent harness。Rust 核心 + Go 插件层。
基于 69 个开源 harness 项目的源码调研（见 `IMPLEMENTATION.md`）。

核心原则：**日志是事实，其余都是投影。**

## 状态：七层完成 + 真实模型验证 + 缓存实测

| 层 | 内容 | 测试 |
|---|---|---|
| L0 事件日志 | append-only + seq、组提交、投影、uncertain 阻塞 | 6 |
| L1 主循环 | 不设硬上限、末轮禁工具留定义、真实工具调用 | — |
| L2 守卫 | 失败熔断、三级阶梯、只读螺旋 | 13 |
| L3 上下文 | 预算校准、三段式降级、摘要校验、**缓存优化** | 47 |
| L4 记忆 | 准入、封口、配额棘轮、睡眠整合、RRF 检索 | 18 |
| L5 持久化 | 收据、检查点、租约围栏、拒绝交出失效状态 | 15 |
| L6 编排 | SOP 五节点 DAG、人工网关、前置能力 | 21 |
| L7 扩展 | 策略链、作用域覆盖规则、系统提示只追加 | 24 |
| Go 插件 | JSON-RPC over stdio | — |

**143 个测试全部通过，零编译警告。**

## 快速开始

```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
cargo build --release
cargo test
```

## 真实运行

```bash
# 测连通性
./target/release/harness ping

# 跑任务（默认 Magpie 渠道）
./target/release/harness run "修复 bug 并验证" --workdir /path/to/project

# 指定推理档位
./target/release/harness run "任务" --effort medium
./target/release/harness run "任务" --effort xhigh

# 激进裁剪（压小缓存 miss）
./target/release/harness run "任务" --clip-aggressive

# 诊断缓存
HARNESS_TRACE_CACHE=1 ./target/release/harness run "任务"
```

## 推理档位

**支持 7 档**：`none` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`。

Magpie 上 `auto/deepseek-v4.1-flash` 只支持 `low` / `high` / `max`，
其余档位会**映射到最近的受支持档位**，且映射结果始终打印：

```
推理档位: 请求 medium → 实际 low
  (上游只支持 low/high/max，medium 映射为 low)
```

| 请求 | 实际 | 理由 |
|---|---|---|
| medium | low | 等距取较低（保守） |
| xhigh | high | 等距取较低（保守） |

实测 `medium` 与 `xhigh` 在 DeepSeek 后端**不是独立档位**（推理量差异在噪声内）。
详见 `docs/CACHE-BENCHMARK.md`。

## 缓存命中率

| 场景 | 命中率 |
|---|---|
| 长会话稳态（排除首轮） | **96.9%** |
| 长会话总命中 | 87.4% |
| 真实任务（默认裁剪） | 65.1% |
| 真实任务（激进裁剪） | **79.6%** |
| Deva 对照（44 万 token） | 99.9% |

**稳态 96.9% 与 Deva 同量级。** 差距来自上下文规模：Deva 44 万 token 里新增只占 0.09%，
测试上下文 6.5 千 token 里新增占 1-3%。

### 会影响缓存的因素（实测）

| 因素 | 影响 |
|---|---|
| `reasoning.effort` 首次出现或值变化 | 命中归零，之后恢复 |
| `tools` 数组顺序变化 | **命中归零** |
| system prompt 改动 | 命中归零 |
| `max_tokens` / `temperature` / 字段顺序 | 无 |

**不要动态改 system prompt、增减工具、中途切换 effort** —— 这三个都会重建缓存。

### 优化措施

- **三段式裁剪**：大 tool 结果裁到 8k（默认）或 2k（激进）字符
- **缓存健康度监控**：`CacheStats` 提供总命中率、稳态命中率、退化告警
- **档位映射可观测**：请求值与实际值都打印，不静默降级

## 目录

```
crates/
  core/         L0 事件日志 + L1 主循环
  guards/       L2 守卫
  context/      L3 预算、三段式降级、摘要校验、缓存优化、档位映射
  memory/       L4 记忆
  persist/      L5 收据、检查点、租约
  orchestrate/  L6 SOP DAG 编排
  plugin/       L7 扩展宿主与策略链
  cli/          可执行入口（真实模型客户端 + 工具）
plugins/        Go 插件（JSON-RPC over stdio）
docs/           实测报告
```

## 设计要点

**日志是事实。** 模型看到的消息是事件日志的纯函数。

**不要每条事件 fsync。** 组提交实测把吞吐从 26.8 提到 250 runs/s。

**守卫的难点是不杀合法长任务。** 只对「相同的失败」熔断，指纹必须含结果。

**不确定的副作用必须阻塞。** `uncertain` 绝不静默重试。

**记忆失效则拒绝交出状态。** 恢复出的状态若建立在过期记忆上就是错的。

**扩展：项目级不能覆盖内建。** clone 下来的仓库不该替换 CLI 的行为。

**缓存：前缀必须冻结。** 见 `docs/CACHE-BENCHMARK.md`。

详见 `IMPLEMENTATION.md` 与 `AGENTS.md`。
