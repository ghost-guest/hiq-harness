# 桌面 UI 选型调研

> 日期：2026-11-15
> 约束：不许自发明。必须先调研、对比、搬最优的开源项目。

## 候选对比

| 项目 | 星 | 技术栈 | 许可 | 定位 | 可否嵌入 |
|---|---|---|---|---|---|
| **assistant-ui** | 13226 | React + radix-ui + zustand | **MIT** | AI 聊天 UI 组件库 | ✅ 组件级嵌入 |
| shadcn/ui | 128216 | React + Tailwind + radix | MIT | 通用 UI 组件 | ✅ 但无聊天组件 |
| ai-chatbot (vercel) | 21053 | Next.js + ai-sdk | **NOASSERTION** | 完整应用模板 | ⚠️ 需整体接管 |
| lobe-chat | 85548 | Next.js | **NOASSERTION** | 完整 Chat 应用 | ⚠️ 体量大，强绑定 |
| open-webui | 158696 | Svelte | **NOASSERTION** | 完整 Web UI | ❌ 语言栈不符 |

## 为什么选 assistant-ui

**一句话**：它是唯一「专门为 AI agent 聊天设计、可组件级嵌入、MIT 许可」的项目。

具体理由：

### 1. 定位完全对口

它的 README 第一句：

> **The UX of ChatGPT in your React app** — an open-source TypeScript/React library
> to build production-grade AI chat experiences fast.

我们要的正是「一个能跑 agent 的聊天界面」，不是「一个通用 UI 库」，也不是「一个要接管整个应用的模板」。

### 2. 许可干净

MIT。且不像 ai-chatbot / lobe-chat / open-webui 那样是 NOASSERTION（需要逐个确认条款）。

### 3. 组件齐全且都是原语

已核实 `packages/react/src/primitives/` 下有：

| 组件 | 作用 |
|---|---|
| `Thread` | 消息列表、滚动、加载更多 |
| `Composer` | 输入框、附件、快捷操作 |
| `Message` | 单条消息渲染 |
| `Attachment` | 文件附件 |
| `ChainOfThought` | 思考过程折叠展示 |
| `ActionBar` | 消息操作（复制/重试） |
| `BranchPicker` | 分支切换 |
| `ThreadSuggestion` | 建议提示 |

**这些正是 agent UI 需要的**，不需要自己造。

### 4. 不接管应用

lobe-chat / ai-chatbot 是「整个应用」，要搬就得接管路由、数据库、鉴权。
assistant-ui 是「组件库」，我们保留自己的 Rust 后端，只把前端交给它。

### 5. 依赖轻

`@assistant-ui/react` 的依赖只有：`radix-ui`、`zustand`、`zod`、`react-textarea-autosize`、`safe-content-frame`。没有重型框架。

## 与 shadcn/ui 的关系

不是二选一。shadcn/ui 是通用组件（Button/Card/Dialog），assistant-ui 是聊天专用组件。

**组合方案**：assistant-ui 做聊天主体，shadcn/ui 补外围（设置页、对话框）。两者都用 radix-ui，风格天然一致。

## 放弃的候选

| 项目 | 放弃原因 |
|---|---|
| ai-chatbot | Next.js 应用模板，要接管路由与 API；许可需逐条确认 |
| lobe-chat | 完整应用，85K 星但强绑定其架构；许可需确认 |
| open-webui | Svelte，与我们的 React 技术栈不符 |
| 自写 HTML+CSS | **违反第一原则**（已删除，见 git 历史） |

## 实施方式

```bash
# 1. 用 assistant-ui 的脚手架创建
npx create-assistant-ui@latest ui

# 2. 接自己的后端
#    assistant-ui 通过 Runtime 适配器接数据源
#    我们写一个 adapter 指向 harness 的 /events + WS

# 3. Tauri 打包
cargo install tauri-cli
```

关键点：**不手写 UI 组件**，只写「适配层」（把 harness 的事件流喂给 assistant-ui）。

## 待确认

- assistant-ui 的 Runtime 适配器接口需要读文档确认（`packages/react/src/adapters/`）
- Tauri 需要额外约 420 个依赖包
- 手机端复用同一套 React 应用，通过响应式布局适配
