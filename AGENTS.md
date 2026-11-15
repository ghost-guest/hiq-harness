# AGENTS.md

本项目的开发约定。任何改动前先读这里。

## 项目定位

自研 agent harness，个人使用。Rust 核心 + Go 插件层，桌面端后续用 Tauri。

设计依据是 69 个开源 harness 项目的源码调研，完整结论在 `IMPLEMENTATION.md`。

## 语言边界（硬约束）

只用两种语言：

- **Rust**：核心（日志、循环、守卫、上下文、记忆、持久化、编排）
- **Go**：插件层

两者之间只通过**进程边界 + JSON-RPC over stdio** 通信。不要引入 FFI / CGO / WASM。

不要引入第三种语言。前端的 HTML/CSS/JS 属于 Tauri 范畴，不计入，但当前阶段不做。

## 构建与测试

```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

cargo build            # 编译
cargo test             # 全部测试
cargo fmt              # 格式化
cargo clippy --all-targets   # 静态检查
cargo run -p harness-cli -- selftest   # 守卫行为自检

cd plugins && go vet ./... && go build ./...
```

改动后必须跑 `cargo test` 和 `cargo run -p harness-cli -- selftest`，两者都过才算完成。

## 代码约定

**注释写「为什么」不写「是什么」。** 尤其是：

- 阈值数字的来源（哪个项目、什么场景调的）
- 反直觉的设计（比如「为什么这里要重置」）
- 踩过的坑

**不要写防御性代码。** 项目内不出现 `unwrap_or_default()` 吞错误、`if x.is_some()` 加 `unwrap()` 这类模式。错误往上抛，由调用方决定。

**错误类型**：库层用 `thiserror` 定义明确错误，应用层用 `anyhow`。

## 七个机制的核心纪律

改任何一层前先读这条：

| 层 | 不可违反的纪律 |
|---|---|
| L0 日志 | 消息是日志的**纯函数**；不要每条 fsync（用组提交）；uncertain 必须阻塞 |
| L1 循环 | 不设硬上限；末轮禁工具但**保留工具定义**（保前缀缓存） |
| L2 守卫 | 只对**相同的失败**熔断；成功即重置；指纹必须含 `blocks` |
| L3 压缩 | 压缩只改投影不改日志；缓存快到期才动手 |
| L4 记忆 | 存项目内不存家目录；记忆内容不可改只能新版本 |
| L5 持久 | 记忆失效则**拒绝交出 state**；恢复状态不恢复权限 |
| L6 编排 | 节点就绪要求所有上游 `succeeded`；运行前冻结工作流快照 |
| L7 扩展 | 项目扩展**不能**覆盖内建；坏了启动即失败 |

## 测试要求

**守卫必须有反向用例。** 每个守卫至少三种测试：

1. 应该拦的（A-A-A 循环）
2. **不该拦的**（轮询、迭代-重试、长等待）
3. 边界（第 N 次恰好触发）

第 2 类最重要，也最容易漏。守卫写错的代价是杀掉合法长任务。

**历史 bug 要留回归测试。** 例如 `spiral_default_has_valid_window` —— 曾经用 `derive(Default)` 导致守卫永远不触发。

## 提交约定

提交信息用中文，格式：

```
<type>: <一句话概括>

<为什么这么改>
```

type 用 `feat` / `fix` / `refactor` / `docs` / `test`。

## 不要做的事

明确排除，防止范围失控：

- 多智能体协作（先把单 agent 做对；子智能体用同一循环 + 缩小权限）
- Web UI / 移动端（桌面是终态）
- 向量数据库（SQLite FTS5 + 可选 embedding 够用）
- 自研模型适配层（用 OpenAI 兼容协议）
- 插件市场（先有机制再说分发）
- 跨机器编排（单机做透再扩展）

## 当前进度

M1 完成：L0 + L1 + L2 + Go 插件协议。

下一步按 `IMPLEMENTATION.md` 第 3 节：

1. L3 上下文（token 校准 + 三段式降级 + 压缩触发）
2. L4 记忆
3. L5 持久化
4. L6 编排
5. L7 扩展宿主

**扩展放最后。** 前五个机制形状稳定后钩子面自然清楚，反过来会全部返工。
