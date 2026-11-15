# 桌面与手机

> 参考 Cindy（`apps/host` + `packages/device-link-protocol`）的架构。
> UI 用 **assistant-ui**（MIT，13K 星），不自写。

## 架构

```
apps/ui（Next.js + assistant-ui）
   │  /api/chat（适配层，只做转发）
   ↓
harness host（Rust，HTTP + WS，绑 127.0.0.1）
   │  协议：crates/core/src/wire.rs（照搬 Cindy device-link）
   ↓
内核七层（日志 / 循环 / 守卫 / 上下文 / 记忆 / 持久 / 编排）
```

**关键决策：host 是唯一的内核持有者。** 桌面和手机都是客户端，会话/记忆/检查点只有一份。

## UI 为什么用 assistant-ui

调研了 5 个候选（见 `docs/UI-RESEARCH.md`），选它是因为：

| 理由 | 依据 |
|---|---|
| 定位对口 | README 首句：「The UX of ChatGPT in your React app」 |
| 许可干净 | MIT（其他候选多为 NOASSERTION） |
| 组件齐全 | Thread / Composer / Message / Attachment / ChainOfThought / ActionBar / BranchPicker |
| 不接管应用 | 是组件库，不是应用模板；保留自己的 Rust 后端 |
| 依赖轻 | radix-ui + zustand + zod，无重型框架 |

**没有自写任何 UI 组件。** 界面的每一部分都来自 assistant-ui 的 `minimal` 模板。

## 启动

```bash
# 1. 后端
cargo build --release
HARNESS_WORKDIR=/path/to/project \
  ./target/release/harness --dir .harness serve --port 7788

# 2. 界面
cd apps/ui
pnpm install
HARNESS_URL=http://127.0.0.1:7788 pnpm dev
# 打开 http://localhost:3000
```

后端的环境变量：

| 变量 | 默认 | 作用 |
|---|---|---|
| `HARNESS_BASE_URL` | `http://127.0.0.1:3425/v1` | 模型 endpoint |
| `HARNESS_API_KEY` | `magpie` | API key |
| `HARNESS_MODEL` | `auto/deepseek-v4.1-flash` | 模型 |
| `HARNESS_MAX_STEPS` | 10 | 单任务最大步数 |
| `HARNESS_EFFORT` | high | 推理档位 |
| `HARNESS_WORKDIR` | `.` | 任务工作区 |

## 适配层只做转发

`apps/ui/app/api/chat/route.ts` 的全部逻辑：

1. 从 UIMessage 抽出最后一条用户文本
2. POST 到 harness 的 `/run`
3. 用 ai-sdk 官方的 `createUIMessageStream` 把结果包成 UI 消息流

**不实现任何业务逻辑** —— 那都在 Rust 里。协议格式由 ai-sdk 保证，不自己拼 SSE。

## 协议层（`crates/core/src/wire.rs`）

照搬 Cindy 的 `device-link-protocol`：

| 设计 | 值 | 理由 |
|---|---|---|
| `PROTOCOL_VERSION` | 1 | 只升不降 |
| `MAX_FRAME_BYTES` | 2 MiB | 超过即拒但**不断连** |
| `HEARTBEAT_SECS` | 20 | 服务端刷新路由 TTL |
| 信封字段 | `v` / `kind` / `id` / `src` / `dst` / `payload` | `src` 服务端填，防伪造 |

帧类型分三层：连接层（hello/ping/presence）、隧道层（link-open/invoke/push）、错误（relay-error）。
**隧道层对中继不透明** —— 将来加中继时，服务端看不到内容。

## 通道与权限

| 通道 | 需要远程控制 |
|---|---|
| `harness.status` / `events` / `memory` / `pending` | 否（只读） |
| `harness.run` | **是** |

这是 Cindy `remoteControlEnabled` 的语义：会改变世界的操作由被控端授权，手机不能自己开。

## HTTP 端点

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/health` | 健康检查 |
| GET | `/devices` | 在线设备 |
| GET | `/events?limit=N` | 事件日志 |
| GET/POST | `/memory` | 读/写记忆 |
| GET | `/pending` | 未决副作用 |
| POST | `/resolve` | 裁决未决副作用 |
| POST | `/run` | **真实执行任务**（同步返回） |
| POST | `/sop` | 接收 SOP |
| WS | `/ws` | 协议帧通道 |

## 实测记录

### WS 协议（六项全过）

```
✓ 握手 101（RFC 6455，accept key 有向量测试）
✓ hello → hello-ack，分配 dev-0000
✓ ping → pong
✓ invoke harness.run → ok
✓ invoke harness.status → ok（只读不需授权）
✓ 版本 999 → relay-error（未断连）
✓ 错误后仍可通信
```

### 端到端任务（真实模型）

```
POST /api/chat
→ harness 执行 4 步
→ 缓存命中 82%
→ 文件真实被修改：新增 mul(a,b) = a*b
→ 验证 mul(6,7) = 42 ✓
```

### 一次完整修复任务

```
任务：s.py 的 sub 应该是减法
结果：3 步，缓存 70%
文件：return a+b → return a-b
验证：sub(10,3) = 7 ✓
```

## 手机连接

```bash
# 桌面监听局域网
./target/release/harness serve --bind 0.0.0.0 --port 7788

# 手机浏览器访问
http://<桌面局域网IP>:7788
```

协议层已为跨网段中继预留 `dst` 字段，但中继服务需要公网部署，暂未实现。

## 未做的部分

| 项目 | 原因 |
|---|---|
| Tauri 打包 | 需要额外约 420 个依赖包；当前「浏览器打开」已可用 |
| 跨网段中继 | 需要公网部署；协议已预留 |
| 手机原生 App | 手机浏览器访问已够用 |
| 流式输出 | 当前 `/run` 同步返回；assistant-ui 已支持流式，改后端即可 |

**这四项不影响当前使用**，且协议和 UI 都已为它们留好位置。
