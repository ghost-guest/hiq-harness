# hiq-harness

自研 agent harness。Rust 核心 + Go 插件层。
基于 69 个开源 harness 项目的源码调研（见 `IMPLEMENTATION.md`）。

核心原则：**日志是事实，其余都是投影。**

## 状态：七层全部完成，已通过真实模型端到端验证

| 层 | 内容 | 测试数 |
|---|---|---|
| L0 事件日志 | append-only + seq、组提交、投影、uncertain 阻塞 | 6 |
| L1 主循环 | 不设硬上限、末轮禁工具留定义、真实工具调用 | — |
| L2 守卫 | 失败熔断、三级阶梯、只读螺旋 | 13 |
| L3 上下文 | 预算校准、三段式降级、摘要校验 | 21 |
| L4 记忆 | 准入、封口、配额棘轮、睡眠整合、RRF 检索 | 18 |
| L5 持久化 | 收据、检查点、租约围栏、拒绝交出失效状态 | 15 |
| L6 编排 | SOP 五节点 DAG、人工网关、前置能力 | 21 |
| L7 扩展 | 策略链、作用域覆盖规则、系统提示只追加 | 24 |
| Go 插件 | JSON-RPC over stdio | — |

**117 个测试全部通过。**

## 快速开始

```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

cargo build --release
cargo test

# 七层自检
./target/release/harness selftest
./target/release/harness demo
```

## 真实运行

已用 Magpie 中继（`auto/deepseek-v4.1-flash`）验证过完整链路。

```bash
# 测试连通性
./target/release/harness ping \
  --base-url http://127.0.0.1:3425/v1 \
  --api-key magpie \
  --model auto/deepseek-v4.1-flash

# 真实跑一个任务
./target/release/harness run "修复 math.py 里的 bug 并跑测试确认" \
  --workdir /path/to/project \
  --dir .harness \
  --max-steps 12 \
  --effort high
```

## 实测结果

三次真实任务，全部成功：

| 任务 | 步数 | 输入 token | 结果 |
|---|---|---|---|
| 修 bug + 跑测试验证 | 5 | 5,434 | ✅ 文件被正确修复，测试通过 |
| 多文件重构（改问候语） | 6 | 5,733 | ✅ 输出正确变更 |
| 故意触发死循环（读 20 次） | 2 | 2,066 | ✅ 守卫在第 12 次拒跑、第 3 次拒跑后终止 |

第三个任务最能说明守卫有效：模型被要求重复读同一文件 20 次，实际行为是：

```
第 8、9 次  → 无进展提示
第 12 次    → 拒跑（不执行，用拒绝文案替换结果）
第 13、14 次 → 只读螺旋提示
拒跑累计 3 次 → 终止
```

## 目录

```
crates/
  core/         L0 事件日志 + L1 主循环
  guards/       L2 守卫
  context/      L3 预算、三段式降级、摘要校验
  memory/       L4 记忆（准入/封口/棘轮/睡眠整合/检索）
  persist/      L5 收据、检查点、租约
  orchestrate/  L6 SOP DAG 编排
  plugin/       L7 扩展宿主与策略链
  cli/          可执行入口（含真实模型客户端与工具）
plugins/        Go 插件（JSON-RPC over stdio）
```

## 设计要点

**日志是事实。** 模型看到的消息是事件日志的纯函数。压缩只改变投影，不改日志。

**不要每条事件 fsync。** 组提交实测把吞吐从 26.8 runs/s 提到 250 runs/s。

**守卫的难点是不杀合法长任务。** 只对「相同的失败」熔断 —— 轮询和迭代-重试天然不受影响，不需要白名单。指纹必须含结果，否则轮询会被误杀。

**不确定的副作用必须阻塞。** 开始了但没报告的 `uncertain`，绝不静默重试。

**记忆失效则拒绝交出状态。** 恢复出的状态若建立在过期记忆上就是错的，宁可不恢复。

**扩展：项目级不能覆盖内建。** 你 clone 下来的仓库不该悄悄替换 CLI 自身的行为。

详见 `IMPLEMENTATION.md` 与 `AGENTS.md`。
