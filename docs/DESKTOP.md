# 桌面版

> 纯 Rust + GPUI，无 webview、无 Node。3.3MB。

## 为什么是 GPUI

用户约束：**项目只用 Rust 和 Go**。

这直接排除了 Tauri —— 它的 UI 层必然要 HTML/CSS/JS。剩下唯一符合的是 **GPUI**：
Zed 编辑器的 GPU 渲染框架，纯 Rust 绘制。

调研中 orbit 用的就是它（`imrj05/orbit`，152★，Apache-2.0），
`README` 自述「a chat-style GUI rendered entirely in Rust on GPUI... no browser, no webview, no Node」。
我们的界面结构与设计令牌都参考了它。

## 落地

### vendored GPUI

`vendor/gpui/`（3.3MB，132 个源文件），做法与 orbit 一致 ——
它也是把 gpui 0.2.2 vendor 进 `vendor/gpui`。

**必须用默认 Metal 后端，不能用 `macos-blade`。**
实测：`--features macos-blade` 能编译（绕开 Metal Toolchain 依赖），
但窗口不显示；默认后端才能正常出窗口。

### 设计令牌（`crates/desktop/src/theme.rs`）

移植自 orbit 的 `theme/tokens.rs`。orbit 的注释写明：

> **Zed's design tokens, ported onto Orbit's Theme.**
> A one-to-one port of the sizing vocabulary in Zed's `ui` crate:
> the `styles/{units,spacing,typography,elevation,animation}.rs` modules...

所以这套尺寸体系是 **Zed → orbit → 我们**，有明确出处。
只取了需要的部分（间距 / 字号 / 圆角 / 语义色），未整份复制。

配色取 GitHub 深色，与 harness 现有界面一致。

### SSE 客户端（`crates/desktop/src/sse_client.rs`）

连后端 `/run/stream`，标准 W3C Server-Sent Events 解析：

- `event:` / `data:` 累积，空行成事件
- 支持 CRLF、多行 data、连续事件、无 data 的空行忽略
- 用 curl 子进程逐行读，不引入 HTTP 库（与 `llm.rs` 取向一致）

### 界面（`crates/desktop/src/main.rs`）

布局参考 orbit 的 app 结构：左侧导航 + 主区 + 输入区。

| 区域 | 内容 |
|---|---|
| 侧栏 | 对话 / 事件 / 记忆 / SOP |
| 标题栏 | 标题 + 后端地址 |
| 对话区 | 用户任务、步骤、回复、错误（分类着色） |
| 输入区 | 输入框 + 运行按钮 |

执行时后台线程跑 SSE，每 100ms 把事件回灌 UI 并重绘 ——
这是 GPUI 的常规做法（用 timer 驱动，不用阻塞主线程）。

## 打包

```bash
cargo build --release -p harness-desktop
./scripts/bundle-macos.sh
open dist/hiq-harness.app
```

产物 `dist/hiq-harness.app`，**3.3MB**。

`.app` 结构是必需的 —— orbit 的 `run-bundled.sh` 注释写明：

> **Notifications need an app bundle identity**: without one macOS...

裸 binary 在 macOS 上窗口与通知行为不完整。

## 实测

| 项 | 结果 |
|---|---|
| 编译 | 通过，零警告 |
| 窗口 | 正常显示，深色主题，标题栏/侧栏/输入区完整 |
| 打包 | 3.3MB，`open` 可启动 |
| 进程 | 稳定运行，无崩溃 |

## 已知限制

**键盘输入**：已接 `KeyDownEvent`（enter 提交、backspace 删除、space、普通字符），
窗口打开时显式 `window.focus(&handle)`。但自动化验证受屏幕锁屏限制，
需要手动确认打字是否正常。

**其余视图**：事件 / 记忆 / SOP 三个导航项目前是占位，
点击不切换内容（当前只有对话视图有数据）。

**流式渲染**：事件已按顺序回灌，但还没做逐字追加动画。

## 后续

1. 补齐事件 / 记忆 / SOP 三个视图（后端 REST 端点已就绪）
2. 输入框做成真正的受控组件（光标、选区、粘贴）
3. 接入 Tauri 之外的分发方式（Developer ID 签名）
