# 桌面端：orbit 界面 + harness 内核

本文说明 `apps/orbit/` 这个改造过的分支做了什么、为什么这么做、以及哪些地方
有意不实现。改这个目录前先读这里。

## 一句话

上游 [orbit](https://github.com/imrj05/orbit)（Apache-2.0，GPUI 原生桌面端）
的**界面一行没改**，把它的 `orbit-rpc` 客户端从「拉起 pi 子进程、走 stdio JSON-RPC」
换成「直接驱动本仓库的 harness 内核」。

## 边界

| 层 | 来源 | 是否改动 |
| --- | --- | --- |
| 216 个界面文件（`crates/orbit-pi/`） | orbit 上游 | 只做了**删除 pi 专属功能**（见下） |
| `crates/orbit-rpc/src/client.rs` | orbit 上游 | **整体重写**（保留 8 个公开方法签名） |
| `crates/orbit-rpc/src/bridge.rs` | 新增 | SSE 解析器照搬 jcode，事件翻译对齐 pi `docs/json.md` |
| `crates/orbit-rpc/src/session_store.rs` | 新增 | 会话磁盘格式照搬 pi |
| 运行时 | 本仓库 `crates/cli` | 拆出 `lib.rs`（`harness_cli`），**内核零改动** |

没有 pi 协议兼容层。`orbit-rpc` 的客户端实现整体换掉，不是适配。

## 为什么保留 pi 的会话格式

`apps/orbit/crates/orbit-pi/src/sessions.rs` 的解析逻辑**一行没动**，靠
`session_store` 按 pi 的格式落盘来对上：

```
{"type":"session","id":"…","cwd":"…","version":3}     ← 首行
{"type":"message","message":{"role":"user","content":[…],"timestamp":…}}
```

好处是侧栏、搜索、历史、分叉全部无需修改。改格式会连锁改十几个界面文件。

## 34 个 RPC 命令

全部实现（`CommandBody` 共 35 个变体，其中 `Raw` 是透传兜底，不是独立命令）。
覆盖测试在 `crates/orbit-rpc/tests/commands.rs`，逐条发命令并断言不是
「尚未接入 harness 内核」。

### 有意不伪造的部分

内核或缺协议、或没有对应机制时，这些命令**如实报告不可用**，而不是假装成功：

| 命令 | 真实情况 |
| --- | --- |
| `compact` | 内核按上下文阈值自动压缩，没有手动入口。回 `{compacted:false, 原因}` |
| `auth.login` / `auth.logout` / `auth.cancel` | harness 用 `base_url`+`api_key` 直连，没有 OAuth 体系。返回明确错误 |
| `quota.list` | 配额来自服务商账单接口，harness 不采集。回空列表，不是假数字 |
| `steer` / `follow_up` | 内核执行循环是一次性的，没有「运行中插话」的双车道队列。维护内存队列并如实回报，但不假装会自动投递 |
| MCP 探测 | 内核没有「外部运行时持有 MCP 连接」的模型，探测恒为默认值、零进程 |

上游 `orbit/AGENTS.md` 第 5 条要求「不伪造协议给不出的能力」，这里照此执行。

## 删掉的 pi 专属代码

| 文件/模块 | 原行数 | 处理 |
| --- | --- | --- |
| `rpc_patches.rs` | 658 | 删除。给 pi 的 Node 包打补丁用，没有 Node 包可打 |
| `app/pi_update_ui.rs` | 271 | 删除。「pi 有新版本」的提示 |
| `app/view.rs::render_pi_migration` | ~60 | 删除。npm → 官方安装的迁移面板 |
| `pi_install.rs` | 944 → 130 | **重写**。保留类型/签名（十几个界面文件在用），实现如实返回「内嵌」 |
| `updater.rs` 的安装机制 | — | **禁用**。见下 |

### 为什么必须禁掉应用自更新

上游的更新源编译期钉死：

```rust
"https://github.com/imrj05/orbit/releases/latest/download/appcast-…xml"
```

一旦让用户点「更新」，下载回来的是**上游 Orbit 的二进制**，会把这里的
harness 内核覆盖掉。这不是「功能缺失」，是会损坏安装，所以
`run_install_helper()` 直接返回 `None`（不再执行任何二进制替换），
`Updater::init()` 也不再初始化。

要接自己的更新渠道，改 `FEED_URL` 并配好 ed25519 公钥后再恢复。

## 修过的坑（别再踩）

### 1. 无限自我复制

`pi_binary()` 返回自身路径 + `resolve_pi_bin` 兜底 `current_exe()` + MCP 探测每心跳
一次 → Dock 里冒出几十个图标。三处必须同时修，缺一处就会复发。

### 2. 会话行的短写截断（2026-11 修）

原先用 `writeln!(file, "{entry}")` 写会话行。含中文的长行会**短写且静默丢尾** ——
会话文件留下半行 JSON（断在 `"text":` 之后），侧栏打开该会话直接解析失败。

修法（`session_store.rs::append_message`）：正文和换行拼成**一次** `write_all`。
分两次写在 `O_APPEND` 下不保证相邻，实测会出现两条消息首尾相连成一行。

回归测试：`session_store::tests::multibyte_reply_is_written_whole`。

### 3. 会话库的根不能跟进程 cwd 走

`session_store::sessions_root()` 依次读 `HARNESS_SESSIONS_DIR` →
`HARNESS_WORKDIR`+`.harness/sessions` → 进程 cwd。GUI 的 cwd 是 `/`（或应用包内
路径），与界面里的工作区无关，不钉住的话会话写到意外位置、侧栏永远读不到。

`PiClient::spawn_with_bin` 在首次创建时设置 `HARNESS_SESSIONS_DIR`。根是**全局**
的：侧栏遍历根下每个「工作区 slug」子目录来分组。

### 4. SSE 解析的格式假设

`harness_cli::sse::SseEvent::encode` 把事件名写在 `event:` 行、JSON 在 `data:` 行。
解析器不能假设 JSON 里有 `event` 字段（两种写法都要认，见 `bridge::translate`）。

## 测试与验证

```bash
cd apps/orbit
cargo test -p orbit-rpc --lib          # 63 passed
cargo test -p orbit-rpc --test commands # 18 passed（34 命令覆盖）
cargo run -p orbit-rpc --example e2e_prompt   # 端到端：真任务 → 真落盘
```

`e2e_prompt` 是「界面能不能用」的最后一层证据：macOS 拦截第三方 keystroke
与 AX 注入（需要辅助功能授权，弹框会阻塞脚本），界面里的输入没法自动化，
所以直接驱动同一个 `PiClient`。

### 界面内发任务仍需手动验证

RPC 层（34 个命令 + 端到端链路）已自动化覆盖，但「在输入框打字 → 点发送 →
看到流式回答」这一整条**界面交互**没有自动化验证：macOS 会拦掉
AppleScript keystroke，AX API 会因缺少辅助功能授权而阻塞。
新装或升级后请手动发一条任务确认。


## 打包

```bash
cd apps/orbit
SIGNING=0 ./scripts/make-dmg.sh arm64
```

产物在 `target/package/arm64/Orbit Pi.app`、`dist/`。

`Finder layout failed — shipping an unstyled DMG`（`-1712`）不影响 `.app`，
只是 DMG 窗口图标没排版。

设置 `NOTARY_PROFILE` 才会公证，否则别的 Mac 上 Gatekeeper 会警告。
