# hiq-harness

自研 agent harness。Rust 核心 + Go 插件层。

基于 69 个开源 harness 项目的源码调研（详见 `IMPLEMENTATION.md`）。
核心原则：**日志是事实，其余都是投影。**

## 状态

M1 骨架可运行：事件日志 + 最小循环 + 三个守卫。

- ✅ L0 事件日志（append-only、seq、组提交、投影）
- ✅ L1 主循环（不设硬上限、uncertain 阻塞）
- ✅ L2 守卫三件套（失败熔断 / 三级阶梯 / 只读螺旋）
- ✅ Go 插件协议（JSON-RPC over stdio）
- ⬜ L3 上下文压缩
- ⬜ L4 记忆
- ⬜ L5 持久化
- ⬜ L6 编排
- ⬜ L7 扩展宿主

## 快速开始

```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

cargo build
cargo test

# 初始化日志
./target/debug/harness init

# 追加消息并回放
./target/debug/harness say "你好"

# 回放当前投影
./target/debug/harness replay

# 守卫自检：验证三个守卫的实际行为
./target/debug/harness selftest
```

## 目录

```
crates/
  core/         L0 事件日志 + L1 主循环
  guards/       L2 守卫
  context/      L3 预算与压缩
  memory/       L4 记忆
  persist/      L5 持久化
  orchestrate/  L6 编排
  plugin/       L7 扩展宿主
  cli/          可执行入口
plugins/        Go 插件
```

## 设计要点

**日志是事实。** 模型看到的消息是事件日志的纯函数。压缩只改变投影，不改日志。

**不要每条事件 fsync。** 组提交实测把吞吐从 26.8 runs/s 提到 250 runs/s。

**守卫的难点是不杀合法长任务。** 只对「相同的失败」熔断 —— 轮询和迭代-重试天然不受影响，不需要白名单。

**不确定的副作用必须阻塞。** 工具调用带 executionId，开始了但没报告的是 `uncertain`，绝不静默重试。

详见 `IMPLEMENTATION.md`。
