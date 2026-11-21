# 融合内核实施状态（S0–S5）

> 方案：`docs/KERNEL-FUSION-PLAN.md`。本文件只记**已落地并可复跑**的东西。
> 代码：`crates/harnessd`（Rust 监督层）、`sdk/go/harnessplugin`（插件 SDK）、`bench/`（20 任务评测集）。

---

## 逐条核对 S0–S5：矩阵之前漏了 S2 的 GetEntries

没有继续"再挖一个 bug"，而是回到 `KERNEL-FUSION-PLAN.md` 的 S0–S5 **原始定义**
逐条核对验收矩阵覆盖。发现 S2 的一条从没验过：**`GetEntries` + session 持久化**。

真引擎实测：引擎 `GET /v1/threads/{id}` 返回的是 `items`（完整历史，含我们的标记），
但 orbit 的 `GetEntries` 读 `entries`。不翻译 → 引擎有历史、界面显示空。
又一个"数据在、投影错"。修 `with_entries_from_items()`，单测 + 变异体 + 真引擎探针钉住。
矩阵 17 → 18 行。

### 矩阵自己也会用旧二进制

改完 GetEntries 直接跑矩阵，S2 会话历史那行红了，单跑却 PASS。
原因：矩阵假设二进制是最新的，**没有保证**。加了开头 `cargo build -q -p harnessd`——
"现场跑"的前提是现场编译。这是矩阵自己第 N 次暴露自身缺陷。

### S3 的"流式中途插话"——已打通（上一轮标未达成，这轮攻下来了）

`KERNEL-FUSION-PLAN.md` 的 S3 验收白纸黑字："**流式中途插话生效**"。
上一轮我把它标成未达成缺口、留给用户决策。这轮认真攻了一次，发现改动
其实**可控**，不该用"稳定优先"当借口回避硬指标。

根因（机制半条在）：
- `SteerGate` + `sup.pump()` 安全点释放（`server.rs:365-371`）早就实现；
- 但 orbit 的 steer 命令在 `drain_async_events()` 期间进不来——
  stdin 是阻塞迭代器，回合进行时主循环被占住。

修法（三个小改，标准库，没加依赖）：
1. `serve_orbit` 把 stdin 读入放进独立线程 → `mpsc::channel`；主循环从
   channel 收命令，`Supervisor` 仍是主线程独占，**无锁、无跨线程共享**。
2. `drain_async_events` 加 `steer_rx`：每轮 `try_recv` 非阻塞捞命令，
   steer 注入 `sup.steer`，由现成的安全点逻辑释放给引擎。
3. 回合中捞到的非 steer 命令放进 `deferred` 交还主循环，不丢命令
   （回合中发的第二个 prompt 仍能排队）。

**红方向验证过**：把 `sup.steer.push(text)` 去掉，标记进输出从 4 → 0，
探针变红。证明 PASS 是修复带来的，不是自欺。

### 主循环改动要验的不止 steer 一条

拆 stdin 线程时还新开了一条路：回合中捞到的**非 steer** 命令进 deferred，
回合结束后逆序插回 pending。这是我新写的代码，一开始一行没验过——
"不丢、顺序不乱"只是我自己脑里的假设。

`probe-deferred.sh` 在回合进行中连发 `get_state` + `get_entries`，
验两条都回、且按发送顺序回。红方向：删掉 deferred 重放 → 两条都被吞。
确认这条新路不是空转。

> 动了主循环之后，不能只验"我想要的那个功能"，
> 还要验"我顺手改的那部分没把别的东西弄坏"。

`probe-steer-midturn.sh` 真引擎实测：慢回合进行中发 steer →
`回合 start=1 end=1 ｜ 插话标记进输出 4~5 处`。S3 这一条从缺口变成 PASS。

## steer：两层问题，别混为一谈

`steer_targets_the_live_turn` 一直是绿的。真引擎探针一跑，两个问题一起冒出来：

**① 真 bug（已修）：body 方言对不上。**
`mapping.rs` 的 `Steer` 把文本翻成 `input` **数组**
（`[{type:"text",text:"..."}]`），但 `http_engine` 只 `as_str()` → 拿到空串 →
`{"prompt": ""}` → 引擎 400 `prompt is required`。
mock 里只有 `{"text": "..."}` 那一种，所以从没测出来。
抽成 `steer_text()` 认三种方言，单测 + 变异体钉住。

**② 架构限制（当时未修，现已修）：compat 层做不了中途插话。**
当时的问题：orbit compat 是**单线程同步循环**，`prompt` 命令会
`drain_async_events()` 一直阻塞到回合结束，此时 steer 命令根本进不来。
而**引擎自己的 steer API 是好的**——直连实测回合进行中发 steer 返回 200。
限制在我们这一层，不在引擎。

当时我没有收工，而是如实标了 `LIMIT`。后来把它攻下来了：stdin 拆成
读线程 + mpsc，`drain_async_events` 用 `try_recv` 非阻塞捞 steer 注入
`SteerGate`，由现成的安全点逻辑释放。`probe-steer-midturn.sh` 真引擎实测：
`回合 start=1 end=1 ｜ 插话标记进输出 4~5 处`；红方向：去掉注入 → 4 → 0。
详见本节上方的"流式中途插话——已打通"。

### fork 的 409 是引擎约束，不是内核缺陷

fork 探针偶发红：`HTTP 409 Workspace already has an active turn`。
`turn.end` 落账只说明 harnessd 看见了，引擎自己还可能慢一拍。
改成重试 5 次（实测第 5 次才成功），而不是一次 409 就判红。
这和"命中率改观察项"是同一个原则：**外部的不确定性，不该变成内核的红**。

## §9 界面探针：重建后的桌面版 PASS 过两次

- 2026-11-20 20:14 CST：重构传输层（reqwest + reqwest-eventsource）之后，
  `evidence.sh` 里 §9 首次 **PASS**——`OCR 读到 44 行 ｜ PASS 界面渲染（项目区 / 会话搜索 / 任务输入框 都在）`。
 这次是**新二进制、新传输层**跑出来的，不是旧构建的余温。
- 2026-11-20 20:36 CST：重跑时屏幕锁了，按设计 SKIP。

证据文件只写当轮真正验到的东西，所以 `docs/EVIDENCE.md` 里那份写的是 SKIP；
PASS 那轮的结果记在这里，附时间戳。

## §9 界面探针：解锁时真的 PASS 过一次

2026-11-19 21:54 CST 屏幕解锁期间，`HARNESS_ENGINE_BIN=… CODEWHALE_HOME=… bash scripts/probe-ui.sh`
实跑拿到 `PASS 界面渲染（项目区 / 会话搜索 / 任务输入框 都在）`，
截图 `/tmp/orbit-ui-probe.png`。

之后屏幕重新锁上（`CGSSessionScreenIsLocked=True`）。23:11 重新生成
`docs/EVIDENCE.md` 时探针按设计报 SKIP——证据只写当轮真正验到的东西，
不拿旧截图冒充本轮通过。两次结论不矛盾，是屏幕状态变了。

## 切会话：能力集收敛后暴露的第三个洞

`SwitchSession` 走 `thread/resume`。HTTP 传输里**从来没有这个分支**，
以前靠"能力集是空 = 什么都能做"蒙过去：`st.thread_id` 先被赋值，
调用随后报 `not mapped`——错在明处，状态却已经换对了，所以看着能用。

能力集按传输层能力收敛之后，这条捷径没了：mapping 见 `has("thread/resume")`
为假，直接回 `na("session switching")`，**状态根本不换**。结果是切回原会话失败，
`GetEntries` 继续读上一个线程——界面上就是"另一个会话的消息跑进了这个会话的历史"。

修法不是把它绕过，而是**真的实现**：HTTP 上"恢复会话"是本地动作——
换目标线程 + 换 SSE 订阅。于是 `thread/resume` 进了 `supported_methods()`，
`request()` 里有对应分支。

**教训**：能力集从"空 = 全支持"改成"只有登记过的才算"之后，
所有靠蒙过日子的分支会一次性露出来。这不是回归，是把假绿变真红。

## relay 补上了端到端测试，顺手抓到两个真 bug

换 `tungstenite` 之后的 WS 握手一直失败。先写测试再修，两个问题都是**换实现时引入的**：

1. **`BufReader` 抢帧**：为了区分 WS / 普通 HTTP，我先把请求头读进 `BufReader`；
   它会顺手把后面的 WS 帧一起预读进自己的缓冲区，而 tungstenite 接手的是原始
   socket——那些字节就丢了。表现：握手成功，然后 `Protocol(ResetWithoutClosingHandshake)`。
   修法：逐字节读到 `\r\n\r\n`，不套 `BufReader`。
2. **非阻塞 socket**：监听器是 non-blocking 的，接手的 socket 跟着非阻塞，
   tungstenite 读帧立刻拿到 `EAGAIN`。修法：`set_nonblocking(false)`。

新增 `crates/cli/tests/relay_e2e.rs` 两条端到端：`GET /health` 回 200 JSON、
WS 握手 → hello → 收到带 `device_id` 的 `hello_ack`。这两条以前**根本不存在**
——所以刚才那两个 bug 没有任何东西会替我抓住。

**注意**：这两条测试是"先有验收再动实现"的第一步，`tiny_http` 那半还没动。

## 线程生命周期 × 订阅生命周期：一张表，逐格说明谁在盯着

同一个 bug 踩了三次（fork / thread/resume / thread/start），根因都是
"换了线程但没换订阅"。引擎的 SSE 是**单订阅者**：订错线程 = 一条事件都收不到，
症状永远是"回合起了、`turn_id` 也有、但 `turn.completed` 不来，界面一直转"。

现在判据在 `ensure_stream()` 里（订阅线程 != 当前线程就重订），下面是**每一格
的覆盖情况**——没覆盖的我不写成"应该没问题"。

| # | 从 → 到 | 订阅该做什么 | 谁在验证 | 状态 |
|---|---|---|---|---|
| 1 | 无会话 → 建会话 | 订新线程 | `probe-multilang` / `probe-tool-followup`（首回合） | ✅ 真引擎 |
| 2 | 会话 A → 新会话 B（NewSession） | 丢 A，订 B | `probe-switch-session`（B 的回合必须闭合） | ✅ 真引擎 |
| 3 | A → B（SwitchSession） | 丢 A，订 B | `probe-switch-session`（读回各自的历史） | ✅ 真引擎 |
| 4 | A → fork 出 A' | 丢 A，订 A' | `probe-fork-branch` | ✅ 真引擎 |
| 5 | 回合进行中切会话 | 同上，但旧回合还在跑 | 无 | ❌ **未覆盖** |
| 6 | 引擎进程重启后重连 | 订阅会断，需要重订 | `dead_subscription_is_replaced_on_next_ensure_stream`（假 SSE 服务：回完头就断） | ✅ 单测（真引擎下等价于 7） |
| 7 | 订阅对端静默断开 | 下一次 `ensure_stream` 应发现并重订 | 同上（红方向验过：去掉 `is_closed()` 就红） | ✅ 单测 |

### 第 5 格：量出来了，是个真缺陷（2026-11-21 01:30）

写了 `scripts/probe-switch-midturn.sh` 去**量**，不猜。结果比预想的严重：

```
回执: {'p1': (True, '')}
观察：切走又切回来之后，第二条 prompt **没有回执**（卡住了）
观察：本轮共闭合 0 个回合
```

也就是说：**回合进行中切换会话，会把那个会话弄成半死**——旧回合的事件源被
换掉，它永远不会闭合（timebus 里连一条 `turn.end` 都没有），之后在这个会话上
发消息也没有回执。

这是一条**真缺陷**，不是"未验证"。三种可能的行为里，静默切走最坏（已验证），
所以正确方向是"有回合在跑就拒切"或"先中断再切"。

**这次没修成，如实记下：** 我加了 `turn_active` 标记 + 在 `handle()` 入口拦截
`SwitchSession`，但调试输出显示标记在两个命令之间翻回 `false`
（`cmd=Prompt active=true` → `cmd=NewSession active=false`），而同一份日志里
**一条 `turn.end` 都没有**。两件事互相矛盾，说明我对"这一轮什么时候算结束"
的模型还是错的。凌晨 1:35、fatigue 30 的状态下继续改内核是在赌，
所以回退了这次尝试（`server.rs` 回到 `7b98430`），把测量结果和证据留下。

### 判据查清了（2026-11-21 01:45，只读调研）

**权威信号是引擎自己发出的 `turn.completed`**（HTTP/SSE 上 `turn.failed` /
`turn.cancelled` 在 `http_engine.rs:264` 归一后同义）。内核在
`server.rs:461` 收到它的那一刻，才同时落下日志的 `turn.end`（`:462`）和界面的
`turn_end`——这是**唯一同时被日志和界面承认**的回合结束。其余全是它的投影或副产品：

| 东西 | 位置 | 它其实是什么 |
|---|---|---|
| 日志 `turn.end` | `server.rs:462` | `turn.completed` 的投影，**唯一写入点** |
| `SafePoint::TurnDone` | `server.rs:467` | `turn.end` 的**下游副作用**（放行排队插话），不是判据 |
| `guards.begin_turn()` | `guards/src/lib.rs:410` | 只是 `nudges_this_turn = 0` 的限流计数重置 |
| drain 的 idle/超时退出 | `main.rs:171` / `main.rs:175` | **一条日志都不写** |
| `is_terminal()` 里的 `agent_end`/`error` | `main.rs:196-199` | 也不写 turn.end |
| 引擎侧 409 active turn | `http_engine.rs:410` | 引擎自己的状态，与我们无关 |

**为什么我上次观测到"矛盾"**：`NewSession` 是纯本地动作（`mapping.rs:186-189`
只把 `thread_id` 置 None，`server.rs:122` 直接返回、连 `pump()` 都不调），
它读到的 `false` 是**别人留下的**；而 drain 的三条退出里只有 `terminal`
会写 `turn.end`，另外两类退出（idle≥20 且零事件、600s 超时）以及
`is_terminal` 认的 `agent_end`/`error` **一个字都不写日志**。
所以"active=false + 零条 turn.end"不是矛盾——是两套东西本来就没同步过。

**结论**：我加的 `turn_active` 是手写的**第二份真相**，没有任何一行代码保证它与
日志同步，所以必然分歧。要做"当前有回合在跑"的判断，只有一种有事实支撑的实现：
读 `self.log.project().turn_open`（`timebus.rs:130-131` 定义，纯函数、可从磁盘重建）。

**但它有"假开"**：`turn.start` 在 `engine.request()` **之前**写（`server.rs:131-137`），
引擎随后拒单（`server.rs:213-222` 直接 bail）时那条 `turn.start` 已落账、
永远等不到 `turn.end`。所以它只能用作**守卫的保守近似**（宁可少切一次），
不能当作"回合真的结束了"。

下次接着做需要先回答一个问题：**内核认为"一轮结束"的判据是什么**——
`turn/completed` 通知？日志里的 `turn.end`？还是守卫的 `at_safe_point`？
这个问题没有清楚答案之前，`turn_active` 的置位/清位必然是错的。

第 5 格仍是**已知未验证**。第 6、7 格这一轮补上了单测，
红方向也验过（把 `is_closed()` 去掉，测试立刻红），并进了变异表。

## 已知缺口：CLI 的 HTTP 那一半还是手写的

`crates/cli/src/server.rs` 里，**WebSocket 已经换成 `tungstenite`**，
但同一个端口上的普通 HTTP 仍是我们自己解析的：读请求行、读头、
按 `Content-Length` 读体，响应也是拼字符串写 `HTTP/1.1 200 OK`。

为什么今晚没改：这条路径要同时服务 WS 升级和普通 HTTP，正确做法是
`tiny_http`（它支持 `Request::upgrade()` 把原始 socket 交出来）+ tungstenite，
但 `http_dispatch` 现在是直接往 `TcpStream` 写响应的，改成"返回 (status, body)"
再交给库，是一次实打实的重构；而这条 relay 只有 72 条单测兜底，
没有端到端验收覆盖。改完我无法当场证明它没坏——那就不是"补上差异"，是"赌一把"。

落地方案（下一轮，已核对过 `tiny_http` 的能力）：

1. 依赖换成 `tiny_http`；`Server::server()` 起服务，`recv_timeout` 循环。
2. WS 分支：`request.upgrade("websocket", Response::empty(101))` 拿回原始流
   → `tungstenite::server::accept`（`upgrade` 正是为此设计的）。
3. 普通路由：`http_dispatch` 改成返回 `(u16, serde_json::Value)`，由库写响应。
4. **`/run/stream` 是最麻烦的一个**：现在它拿 `&mut TcpStream` 直接写
   SSE 头和事件。换成库要么让 `stream_task` 写进一个 `mpsc`，再实现一个
   从 channel 拉字节的 `Read`（`tiny_http` 在 `data_length: None` 时用
   chunked，正好吃这种按需产出的 body），要么把执行器改成回调式。
   前者约 40 行，后者动 `exec`/`sse` 两个模块。
5. 验收：`tests/relay_e2e.rs` 已经有 HTTP + WS 两条；**要再加一条
   `/run/stream` 的 SSE 断言**，否则第 4 步没有护栏。**先补这条，再动实现。**

> 现状盘点（2026-11-21 00:05）：第 1–3 步是纯机械替换，第 4 步需要一个
> 流式 body 适配器，第 5 步的 SSE 端到端还没有。所以这一项**不能**用
> "改完编译过"来结案。

### 试过 `tiny_http` 了：这条路不通（2026-11-21 00:35）

真按方案改了一版，`relay_e2e` 当场判红：

```
WS 握手: Protocol(SecWebSocketAcceptKeyMismatch)
```

原因是两个库抢同一件事：`tiny_http::Request::upgrade(protocol, response)`
会**自己写出 101 响应**，而 tungstenite 的服务端握手要自己算
`Sec-WebSocket-Accept` 并写响应。tiny_http 先写过之后，tungstenite 拿到的
请求已经被消费掉，握手根本没法完成（服务端日志：`Handshake not finished`）。

可选出路只有两条，都不轻：

1. **换 `axum` + `tokio-tungstenite`**：async 重构，整个 relay 从
   "每连接一线程"改成 tokio 任务；`/run/stream` 也要改成 async stream。
2. **两个端口**：HTTP 走 tiny_http，WS 单独一个原始端口 —— 但这会改动
   对外接口（`/health` 与 `ws://.../ws` 不再同端口），下游要跟着改。

所以这一项**不是"换个库"那么小**，是一个需要单独排期的重构。
已经做好的护栏保留：`dispatch()` 拆分 + 三条路由单测 + `relay_e2e` 两条
端到端（HTTP/WS），换任何方案都用它们判红。

## 自研传输清完了：desktop 的 curl、CLI 的手写 WebSocket

方案里"不要自研"不是只针对 harnessd。这一轮把剩下的两处也换掉了：

| 位置 | 原来 | 现在 |
|---|---|---|
| `crates/desktop/src/sse_client.rs` | spawn `curl`，自己按行解 `event:`/`data:` | `reqwest` + `reqwest-eventsource` |
| `crates/cli/src/server.rs` WS | 手写握手 + **自己实现 SHA-1 和 base64** + 帧编解码 | `tungstenite`（握手/帧/ping-pong 全交给库） |

CLI 那套最刺眼：`sha1()`、`base64()`、`ws_accept_key()`、`read_frame()`、`write_frame()`
全是自己写的，连 RFC 里的示例向量都得手工对一遍。删掉两百多行，行为不变
（文本帧 → `on_envelope`，hello 分配 device id，协议错回错误帧但不断连）。

删掉的 4 条测试（`sha1_known_vector` 等）是**自研 crypto 的自测**，随实现一起退场；
真正要保的行为由 `tungstenite` 负责，不再需要我们用测试去守别人的协议正确性。

`grep -rn 'Command::new("curl")'` 在整个仓库（排除 target/vendor）现在**为空**。

## 探针必须先构建：一次假红换来的规矩

验收跑到一半失败过一条：`unknown command `Prompt``——看着像内核不认识 Prompt 了，
实际是探针拿了一个**旧二进制**（gate 的构建和我自己的构建并发，产物时间戳对不上）。
这类假红最贵：它会让人去改本来没错的代码。

规矩：凡是靠 `HARNESSD_BIN` 跑真引擎的探针，开头先 `cargo build -q -p harnessd`，
除非显式 `SKIP_BUILD=1`。六个探针都加上了。

## 语言约束落地：仓库只剩 Rust + Go

方案定的是 Rust 做内核与界面、Go 做插件层。审计命令
`cargo run -p xtask -- lang-audit` 把这条变成会红的门禁——106 个非 Rust/Go
源码文件，逐个处理到 **0**：

| 处理 | 做法 |
|---|---|
| 58 个上游开发脚本（py/swift/ps1） | 删除（不参与构建） |
| `scripts/ocr.swift` | → `xtask ocr`（objc2-vision） |
| `scripts/mutate.py` | → `xtask mutate`（表外置成 mutants.json） |
| `scripts/probe-approval.py` | → `xtask approve-probe`（reqwest + SSE） |
| `bench/run.py` + `mock_provider.py` | → `xtask bench`；20 道题自测判定与 Python 版**逐题一致** |
| 36 个 `contrib/*.js|mjs`（13k 行，被 `include_str!` 编进二进制） | 移除；`install_*()` 返回 `None` |

**JS 扩展移除的代价，明说：** 额度桥、自动标题、建议、子代理、工作流这几个
界面扩展会退化成"不可用"，对应控件本来就有降级分支（标题用首条消息、访问模式
控件如实显示无守卫）。**安全边界没有变松**：真正的判定在内核——harnessd 的三守卫
和 Go 插件审批（`go_plugin_can_deny_a_command` 是绿的）。这符合方案里"S4 插件层"
的方向：审批判定属于内核侧，不属于界面里的脚本。

保留的唯一白名单是 `bench/fixtures/`：那里是**被测数据**（故意用别的语言写的坏代码），
删掉等于删掉评测集。审计输出会把它单独列出来，不假装不存在。

## 传输层不再手写：curl 子进程 → reqwest + reqwest-eventsource

方案里写的是"融合只发生在接缝上，不自研"。`http_engine.rs` 原来一直 spawn
`curl` 当 HTTP 客户端——这是自研，而且它今天咬了我们一口：

```
回合 1 结束 → 引擎关掉 SSE → curl 安静退出
             → 我们的 `sse: Some((Child, rx))` 还挂着"已连接"
回合 2 → ensure_stream() 看见 is_some() 就返回 → POST 发给了不存在的订阅者
       → 引擎把这一轮挂住：timebus 里只有我们自己写的 turn.start
       → 界面「已工作 4m22s」，永远转圈
```

先按方案要求调研，而不是直接改代码：

| 候选 | 下载量 | 同步 API | 结论 |
|---|---|---|---|
| `reqwest` | 9.37 亿 | 有（blocking） | 用它做普通请求 |
| `reqwest-eventsource` | 1291 万 | 无（async） | **选它**：依赖的正是 reqwest 0.12，接口对得上 |
| `eventsource-client`（LaunchDarkly） | 990 万 | 无（0.18 只有 async） | 排除 |
| `sse-client` | 2 万 | 有 | 太旧太小，排除 |

落地方式：后台线程里跑一个 current-thread tokio runtime + `reqwest-eventsource`，
把事件行推进原来的 `mpsc`——**`Engine` trait 一个签名都没改**，
所以 server / supervisor / 探针全部不用动。订阅的存活状态由 `SseHandle`
（`closed: AtomicBool`）回答，那是线程真的结束了没有，不是我们还记不记得它。

顺带修掉一条自己挖的坑：能力集收敛时只兜底了三个线程面方法，把 `prompt/run`
挡在外面，第二回合直接被拒（`engine exposes none of prompt/run, prompt/request`）。
现在 `supported_methods()` 是"这条传输能路由什么"的唯一事实来源，
`supports()` 和 `prepare()` 都从它推——两边不会再各写一份。

## 一打开界面就一条红 —— 空集被当成了"什么都能做"

界面一启动就调 `get_commands`，以前必定失败：

```
Get commands 失败：'prompt/capabilities' is not mapped on the HTTP transport
```

三个坑串成一条：

1. 引擎根本没有 `/v1/prompt/capabilities` 这个端点，`prepare()` 里那组能力探测**全部失败**；
2. 整块能力收敛逻辑写在 `if !methods.is_empty()` 里面，探测失败时 `caps.methods` 保持**空**；
3. 而 `has()` 的写法是 `is_empty() || contains()`——**空集被解释成"什么都能做"**。
   于是 `GetCommands` 挑中 `prompt/capabilities` 去调，得到一个我们没实现的传输路由，
   界面收到一条红。

两处一起改：

- `Capabilities` 的语义：空集意味着"**不知道**"，不该意味着"全都能"。现在
  `prepare()` **无条件**把三条线程面方法填进去，再按传输层能力收敛，所以它永远非空。
- `Engine` trait 新增 `supports(&self, method)`：引擎的清单是"它能做什么"，
  传输层的 `supports` 是"我们这条线能不能转发"。能用的方法必须是两者的**交集**。
  `HttpEngine` 的实现与 `request()` 的 match 分支一一对应，两边不同步就会重演这条红。

**这一条是装完新二进制、真打开界面才发现的。** 内核测试全绿、验收 23 项全过的时候，
界面上还挂着一条红——因为没有任何一条验收问过"界面启动时调的那个命令成功了吗"。
现在 `probe-capabilities.sh` 专门问这一句，并进了验收矩阵。

## 第二回合永远转圈 —— 事件词汇表少了一个词

用户实测：第一回合（用了 uname）答完，紧接着追问一句 → 界面一直转圈，没有输出。

分层排查的结果很干净：

- **内核侧全对**。同一线程两回合：`turn_id` 都有、都闭合、日志里两条回答都在，
  `probe-second-turn.sh` 直接 PASS。所以不是引擎、不是回执、不是多回合限制。
- **界面侧缺一个事件**。orbit 的回合状态机认的是
  `agent_start` / `agent_end` / `agent_settled` / `turn_start`；
  我们只发了 `turn_start` 和 `agent_end`。

关键差别在语义，不是名字：`agent_end` 只是"这一轮循环结束了"——界面据此清掉
流式状态，但**故意把回合留着不关**，因为 agent 循环可能还要再来一轮。
真正关回合的是 `agent_settled`（界面的 `end_live_run()` 只挂在它上面）。
少了它，界面永远认为回合在跑，下一条消息只能排队等——就是那个转圈。

修法：把发出的序列对齐 `orbit-pi` 测试里 FakePi 捕获的那份标准序列——

```
agent_start → turn_start → … → agent_end → agent_settled
```

`turn_start` 同时发 `agent_start`，`turn_end` 同时发 `agent_settled`。
两条单测分别钉住一头，探针额外断言"每个 agent_end 都有配对的 agent_settled"，
红方向都验过。真引擎跑出来的界面序列：

```
agent_start turn_start agent_end agent_settled  ×2
```

**教训**：这类"内核绿、产品红"的 bug，判据是**对端认的词汇表**，不是我们自己
发出去的条数。事件一条不少照样能用不了。以后改事件映射，先把对端的
`parse_line` 分支列出来对账，而不是看我们自己的测试。

## "问一句就一个命令，然后没有下文" —— 两个方言的坑只修了一半

用户实测：在界面里问"你的内核是什么？" → 跑了一个命令 → 回合结束，没有回答。

复现（`probe-tool-followup.sh`，真引擎 + 真 harnessd）：

```
turn.start → usage → tool.call → usage → tool.call → usage → turn.end
```

**没有任何 assistant 文本**。而引擎侧其实发得好好的：

```
tool_execution_start → tool_execution_end → ... → text_delta → agent_end
```

所以内核接线是通的，坏在投影。两处，同一个根因：

1. **日志只认 `item.type`**。stdio 方言写 `type: "agentMessage"`，HTTP/SSE 写
   `kind: "agent_message"`。代码只看了前者，于是 HTTP 路径下**每一条 assistant
   回答都不进日志**。而"日志是事实，其余都是投影"是这个内核的第一条原则——
   日志里没有的，回放、分叉、压缩、历史读回来全都没有。
2. **不流式的 provider 界面一个字都不显示**。界面按 `text_delta` 渲染，
   但有的 provider 一条 delta 都不发，只在 `item.detail` 里给成品文本。
   这种情况以前直接丢掉了。

修法：`type`/`kind` 两种都认；delta 为空时用 `item.detail` 补一条 `text_delta`
发给界面。单测 `http_shaped_agent_message_reaches_the_log` 同时钉住
"进日志"和"发给界面"两件事，两条变异体对应，红方向验过。

**教训**：文件上方那段注释自己写着"两种方言都认"，但它只改到了工具那一支，
回答那一支没改。以后凡是"两种方言"，得把同一份数据的所有分支一起列出来对账。

修复后的时间线：

```
turn.start → usage → tool.call → usage → message → turn.end
```

## fork 的第二层：id 换了，但"当前线程"有两个真相

`probe-fork.sh` 证明 fork 后新线程能跑完回合。但 S2 原文要的是
"fork 出新会话能跑到**不同结论**"——真正的分叉应该是数据隔离。

`probe-fork-branch.sh` 的做法：原线程发 MA → fork → **新线程**发 MB →
切回原线程读历史。结果 MB 在新线程的历史里找不到。

根因是两个真相源在 fork 之后分家：

- `http_engine` 的 `self.thread_id` 在 fork 时换成新线程——**回合发到新线程**；
- Supervisor 的 `state.thread_id` 没换——`get_entries` / `get_messages` /
  再次 fork / `set_session_name` **全读旧线程**。

`handle` 里那句注释写着"thread/start 是唯一出现新线程 id 的地方"。
那句话是错的：fork 也创建线程。改成 start/fork/resume 都同步 state。

**顺带修了测试替身**：`MockEngine::request` 以前把 params 丢了（`_params`），
闭包永远只看到 `Null`。这就是这类 bug 单测抓不住的原因——测试根本看不到
"发给引擎的 threadId 是什么"。现在 `requests()` 记录 (方法, 参数)，
新单测 `fork_moves_the_current_thread_so_later_reads_follow_it` 钉住
state 和真实的 threadId 两个层面，红方向验过（退回只认 start → 测试红）。

变异扫描 17 → 19（新增 `fork-thread-state-stale` / `mock-engine-drops-params`）。

## fork：单元测试只验了"id 换了"，没验"还能跑"

`fork_switches_to_the_new_thread` 一直是绿的。但真引擎上 fork 之后，
新线程的回合**永远不闭合**——三条命令全是 `success:true`，什么都没跑。

根因：`thread/fork` 改了 `self.thread_id`，**没动 `self.sse`**。
`ensure_stream()` 见 `sse.is_some()` 就直接返回，旧订阅还挂在原线程上，
新线程没有订阅者 → 引擎让回合停住（和"先订阅再发回合"是同一家病）。

单元测试里没有 SSE，这类缺陷天生看不见。加 `probe-fork.sh`：
原线程跑完 → fork → **新线程上再跑一个回合** → 验它闭合、验线程 id 真换了。
修完实测：`thr_b693… → thr_1f07…`，两个回合都闭合。

### 探针第一次跑，连前置回合都没过

报 `DeepSeek API key not found`。查下来是**我自己的测试改坏了被测环境**：
上一轮修 §8 时写的 `cp "$HOME_DIR/config.toml" "$CMP_HOME/config.toml"`，
在 CMP_HOME 指向共享引擎 home 时，把 0 字节的空配置**覆盖**了进去。

修法：补配置是"缺了才补"（`[ ! -s target ]`），不是"有也覆盖"。
测试不能改坏被测环境——这条和"断言不能自欺"是同一个道理的两面。

### 命中率不该当硬门禁

多回合探针原来断言"第二回合命中率必须高于第一回合"，忽然红了：`0 0 83`。
真相还是那个——命中率由上游代理给，同一份代码实测 `0 98 97` / `0 83 82` / `0 0 83`。
改成**观察项**，硬门禁只留"回合都闭合"。
把不能控制的东西当门禁，等于给自己制造随机红。

## 证据新鲜度：一份 3 小时前的证据不算证据

`docs/EVIDENCE.md` 停在 10:35，代码停在 13:24。中间修了 `turn_id` 丢失、
加了两个真引擎探针——证据里完全没有这些。**证据必须和代码同一时刻**，
否则它证明的是旧构建。

重跑时 §8「真压缩」红了。查了一圈，发现自己的排查方向一直是错的：

1. 先怀疑 `turn_id` 改动打破了 usage 解析 → 写真实信封单测，过。
2. 再怀疑 timebus 没写 usage → 翻日志发现 `cache_freeze` 明明在跑，
   reason 是 `window 304% full but hit only 75.0%`。**usage 一直收到了**。
3. 又怀疑是 home 太新、没有缓存历史 → 换成有历史的 home，**还是 68-78%**。

真相：命中率是**上游代理给的**，我们控制不了。同一份代码，有时 2 次就到
90%，有时 6 次都在 68-78%。之前 PASS 是环境给的运气。

修法不是提高尝试次数自欺，而是把它从"硬门禁"改成**观察项**：
没触发就打印实测命中率（69.6 / 74.0 / 78.5），明说条件没满足，
同时指出内核侧判定由 5 条单测覆盖。**红要红得可解释，绿要绿得可复现。**

顺带给探针加了诊断：失败时打印真实命中率，不再只说"命中率不够"——
没有数字就只能猜，有数字一眼定。

## 变异扫描：测试到底抓不抓得住

`python3 scripts/mutate.py` —— 把内核代码故意改坏，看测试红不红。

为什么需要它：这一夜反复出现同一类问题——**名字说有，内容里没有**。
`nudges_then_refuses_then_stops` 从没验过 stop。名字、注释、文档都能骗人，
只有"改坏它，看它红不红"骗不了人。

最近一次全量（`MUTATE=1 bash scripts/gate.sh` 里跑）——**15 个，全部是测试挡的**：

| 变异体 | 改坏了什么 | 结果 | 说明 |
|---|---|---|---|
| guards-stop-to-note | 守卫最后一档 Stop 降级成 Note | KILLED | 测试红了 |
| guards-refuse-to-note | 拒跑降级成提示 | KILLED | 测试红了 |
| guards-nudge-to-ok | 提示的理由被清空（UI 看到空提示） | KILLED | 测试红了 |
| guards-failure-gate-threshold | 相同失败永远到不了阈值 | KILLED | 测试红了 |
| cache-hit-floor | 命中率下限归零（冷前缀也压缩） | KILLED | 测试红了 |
| cache-pressure | 压力线归零（永远觉得窗口满） | KILLED | 测试红了 |
| approval-allow-word | 裸 "allow" 不再被认（引擎会 400） | KILLED | 测试红了 |
| turn-completed-dropped | 正常完成的回合不再被认作完成 | KILLED | 测试红了 |
| silent-drop-guard | 被拒的回合又被当成正常（错误文案都被改了） | KILLED | 测试红了 |
| compact-failure-silent | 压缩失败被记成成功（坏消息翻译成好消息） | KILLED | 测试红了 |
| protocol-turn-end | protocol 层不认 turn.completed | KILLED | 测试红了 |
| orbit-flat-nesting | 扁平化变成嵌套（界面会读不到） | KILLED | 测试红了 |
| cache-observed-values | 缓存守卫永远拿到 0/0（永远"没问题"） | KILLED | 测试红了 |
| turn-id-dropped-in-wrap | 包装时丢掉回合 id（多回合被误判成被拒） | KILLED | 测试红了 |
| mapping-unknown-command | Prompt 命令掉进兜底 | KILLED | 测试红了 |

全部 15 个变异体都被抓到（没有 STALE，也没有 SURVIVED）。

### 多回合探针的条件：守卫是关着的

`probe-multiturn.sh` 用的是 `auto_approve:true`。这个开关会**关掉整条守卫/审批/插件路径**——
所以"3 回合全闭合"这个证据是在守卫关着的条件下拿到的。

补 `probe-guarded-turns.sh`：`auto_approve:false`，每轮真跑一个写文件的命令，
验两轮都闭合、每轮工具数一致、文件真的写了（2/2）。

做这条时踩了两个坑，都值得记：

1. 一开始用 `echo hello` 当工具——引擎把只读命令自动放行，**根本不触发审批**。
   探针"0 审批"不是缺陷，是我的探针没测到东西。换成 `echo > file` 才触发
   `approval.required`（直连引擎实测确认）。
2. 想数审批事件，但**审批 Request 帧在 `on_reverse_request` 里被消费，
   不写 timebus**，stdout 也看不到 `requestApproval`。改成用"文件真的写了"
   作判据——工具跑完本身就证明审批通道通了。
3. 红向验证第一次做错了：把审批回答器关掉，探针照样 PASS。因为 harnessd
   的守卫在审批通道上**自己就 accept 了**，HTTP 审批队列根本没走到。
   真正的红向是换假引擎/假 harnessd（都验证过会红，且退出码 1）。

### 变异扫描证明不了的东西：多回合

`mutate.py` 跑的是单元测试。有一类缺陷它**天生看不见**——
"先订阅再发回合"和"回合 id 带回顶层"都只在真引擎上才显形。
它们的假象是：**第 1 回合正常，第 2 回合被拒**。单元测试里没有引擎，
永远看不到。

加 `scripts/probe-multiturn.sh`，断言三件事：同一线程连着跑 N 轮、
每轮 `turn.start`/`turn.end` 数量相等、命中率跨轮上涨（前缀真的复用）。

它一跑就红了，挖出一条**真缺陷**：

> 引擎 HTTP 响应是 `{"thread":…, "turn":{"id":"turn_…"}}`，
> harnessd 的 `handle` 手工包装成 `{"thread":…,"output":…,"model":…}` 时
> 把 `turn` 整个丢掉了。上层判断"引擎接没接这一轮"唯一可靠的同步证据
> 就是这个 id——id 没了，第 2 回合被误判成"被拒"。

旧守卫的判据更糟：它看 `turn.start`，而那一行是**我们自己**在发请求前写的，
永远在——等于自己给自己作证。

修完实测：**3 回合全闭合，命中率 0 → 98 → 97**。前缀复用这条
现在也进了验收矩阵（S1 多回合，14/14）。

### 扩到"这一夜修过的每个缺陷"

第一版只有 9 个变异体，覆盖的是守卫/缓存/审批/回合。后来把范围改成
**"这一夜修过的每个 bug，都问一遍：有测试守着吗"**，加进来 5 个，抓到两个漏洞：

- **`orbit-flat-nesting`**：把 `flat()` 改成嵌套，测试全绿。因为所有测试都走
  `text_delta` 那条路径，**那条不经过 `flat`**。而"界面一片空白"正是当年
  这个 bug 的症状——orbit 把整行当 value，套一层就什么都读不到，还不报错。
  补 `flat_keeps_fields_at_the_top_level`。
- **`compact-failure-silent`**：把 `compact_failed` 改成 `compact_ok`，测试全绿。
  又是"坏消息被翻译成好消息"——用户会以为上下文被整理了，其实没有。
  补进 `a_failed_compaction_does_not_swallow_turn_end`。

这两条合起来说明一件事：**修复本身不会自动带上测试**。修的时候想的是
"让它不再坏"，不是"证明它不再坏"。所以修完还得问一句：把它改回去，谁会红？

**扫描器自己也被验过会红**，抓出三个它自己的假绿：

1. `SKIP` 被算成"抓到"。原文没找到时打印 `SKIP` 然后 `continue`，
   最后照样说"全部抓到"——这是最讽刺的一种：**一个用来抓假绿的工具自己在假绿**。
   现在 `SKIP` 改名 `STALE`，并且计入失败。
2. 两个变异体是**编译器**挡的，不是测试挡的（`GateDecision::Allow` 类型不匹配）。
   那不算数——编译器挡的是语法，不是语义。改成语义等价但能编译的破坏
   （`>= self.threshold` → `>= 9999`），才是真在测行为。
3. 扫描期间工作区里是"被改坏"的代码。我并发跑了 `cargo test`，看到
   `failure_gate_blocks_identical_failures` 红了，查了半小时才发现是扫描器
   正在改文件、还没还原。现在加了 `.mutate.lock`，并且 `finally` 里一定解锁。

### 扫描器自己的第四个假绿：缓存

扫描全绿之后紧接着跑 `cargo test`，红了两条不相关的测试
（`every_orbit_command_maps_to_something`、`repeated_identical_failure_produces_a_verdict`）。
源码是**对的**，`git status` 也是干净的。

原因：`shutil.copy2` 还原文件时**保留备份的 mtime**，而备份是"改坏之前"那一刻创建的。
cargo 一比时间戳，认为源文件没变，继续用**变异版**的旧产物。

这类缺陷最坏的地方是它的**指向**：它让你怀疑源码、怀疑测试、怀疑并发，
而真凶是"文件内容对了但时间戳说它没变"。现在还原后显式 `os.utime`。
修完验证：扫描 9/9 KILLED，紧接着 `cargo test --workspace` → **404 通过 / 0 失败**。

### 它抓到的一个真缺陷

`turn.completed` 从 SSE 映射里删掉 → **整个 crate 的测试全绿**。
而"回合不闭合"正是这一夜最贵的那个 bug（timebus 里只有 `turn.start` + `usage`，
投影全错）。这段映射原来内联在 `notifications()` 里，**一行测试都没有**。
现在抽成 `sse_method_for`，`every_sse_event_maps_to_its_own_method` 钉住每个事件名。

顺带发现文档骗人：注释写"未知事件带名字转发，不丢"，
代码却一律返回固定的 `engine/event`——名字被压扁了。
改成真的 `format!("engine/{other}")`。

## S0–S5 验收矩阵（现场跑，一条命令）

`bash scripts/acceptance.sh` —— 27 条，每条打印「期望 / 实际 / 判定」，有红就退出码 1。
下面是最近一次真跑的输出（`HARNESS_ENGINE_BIN` + `CODEWHALE_HOME` 都给了，含真引擎项）：

| 验收项 | 期望 | 实际 | 判定 |
|---|---|---|---|
| S0 进程依赖 | 没有任何 crate 依赖引擎 | ok 没有任何 crate 依赖引擎（进程依赖成立）；path 依赖都在仓库内 | PASS |
| S0 检查会红 | 越界的 path 依赖必须被判红 | 注入 ../../../acc-outside 后确实报错 | PASS |
| S1 接缝① | 流式文本 + 工具调用 + 收尾 | PASS 接缝① 真引擎：orbit 客户端 → harnessd → 引擎，流式文本 + 工具调用 + 收尾都在 | PASS |
| S1 多回合 | 同一线程连续回合，每轮闭合、前缀复用 | PASS 多回合：3 个回合都闭合 ｜ 前缀复用：96 88 97 | PASS |
| S2 会话历史 | GetEntries 在真引擎上读回非空历史 | PASS GetEntries：entries 非空（2 条），历史里找到标记 HISTORY_MARK_18593 | PASS |
| S2 steer | 引擎接受插话 + body 方言翻译正确 | 引擎 steer API：200 —— 回合进行中插话被接受 PASS steer：引擎侧接受 + harnessd body 翻译正确  | PASS |
| S3 deferred 命令 | 回合中非 steer 命令不丢、顺序不乱 | PASS 回合中发的两条非 steer 命令都回来了，顺序 ['g1', 'g2'] | PASS |
| S1 跨语言任务 | Rust 与 Go 各写一份快排，都跑通、有结论 | 工具调用 5 次 ｜ .rs 1 个 ｜ .go 1 个 | PASS |
| S2 切会话 | 开新会话后回合闭合；切回去只看到各自的消息 | PASS 切会话：A 只看到 A 的消息，B 只看到 B 的消息 | PASS |
| S1 跨语言（界面客户端） | orbit 客户端跑跨语言题，流式+工具+收尾都在 | 文本长度: 483 工具调用: ["bash"] PASS 接缝① 真引擎：orbit 客户端 → harnessd → 引擎，流式文本 + 工具调用 + 收尾都在  | PASS |
| S4 能力探测 | get_commands 成功（能力集收敛到可路由方法） | PASS get_commands 正常返回（能力清单已按传输层能力收敛） | PASS |
| S3 第二回合 | 同线程追问能闭合、能答、且发 agent_settled | PASS 两回合都闭合且都有回答 ｜ 第二回合：`Darwin` | PASS |
| S1 工具后收尾 | 工具跑完模型必须给出最终回答（进日志） | PASS 工具 1 次，之后模型给出了最终回答：这个目录里有 4 个条目：README.md、in.fifo、main.rs、timebus.ndjson；其中 in. | PASS |
| S2 fork 真分叉 | 新线程的消息不漏进原线程历史 | PASS 分叉成立：新线程有自己的消息 MB，原线程历史干净（不含 MB） | PASS |
| S3 中途插话 | 回合进行中插话，模型输出含插话标记 | PASS 中途插话：回合进行中注入 steer → 模型输出含插话标记（5 处） | PASS |
| S2 fork 后回合 | fork 出新线程，新线程上回合闭合 | PASS fork：原线程跑完 → fork 出新线程 → 新线程上回合也闭合 | PASS |
| S3 守卫跨回合 | auto_approve=false 下每轮工具真跑、回合闭合 | PASS 守卫跨回合：2 轮都闭合，每轮工具数一致（1 1），审批通道 2/2 都通 | PASS |
| S1 命令覆盖 | orbit 声明的命令与事件都有映射 | every_orbit_command_maps_to_something + every_orbit_type_we_claim_is_mapped 都通过 | PASS |
| S2 档位 | effort 以 reasoning_effort 送到引擎 | effort_is_sent_as_reasoning_effort 通过 | PASS |
| S2 fork | fork 后会话切到新线程 | fork_switches_to_the_new_thread 通过 | PASS |
| S3 守卫 | 三件守卫语义正确 | 17 条守卫测试通过 | PASS |
| S3 缓存守卫 | 命中率与压力两条线都判对 | 5 条缓存测试通过 | PASS |
| S3 回合闭合 | 压缩失败也要写 turn.end | a_failed_compaction_does_not_swallow_turn_end 通过 | PASS |
| S4 插件 | 真 Go 插件能 deny 一条命令 | test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.47s；test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.78s | PASS |
| S4 不空转 | 没有 go 时必须红，不能静默跳过 | 插件测试 ignored=0 | PASS |
| S5 基线 | 通过率/返工率/误杀率有基线数字 | pass 1.0 / rework 0.01 / false-kill 0.0 / solver_fail 0 / cache 0.901 | PASS |
| S5 门禁 | 四条门禁同时成立 | 门禁通过 | PASS |

全部成立（SKIP 的不算成立，但也不算失败——它明说了自己没验）。

**这张表自己也被验过会红。** 写它的过程中抓到三个问题：

1. 我引用了 `every_command_body_variant_is_mapped` 这个测试名——**它不存在**。
   真实的叫 `every_orbit_command_maps_to_something`。矩阵第一版有两条永远红。
2. `S3 缓存守卫` 用 `grep -c "^test .* ok$"` 数测试——`cargo test -q` 根本不打印那些行，
   数出来恒为 0。**判定写错比不判还糟**：它会稳定地告诉你"没做"。
3. `gate.sh` 会调 `acceptance.sh`，而 `acceptance.sh` 又调 `gate.sh` —— **无限递归**。
   表现是"脚本跑了几百秒、输出 0 行"，看起来像慢，其实是自我 fork。
   修法：矩阵调门禁时设 `SKIP_ACCEPTANCE=1`。400 秒 → 4 秒。

## 守卫的最后一档：名字比内容诚实

有一条测试叫 `no_progress_nudges_then_refuses_then_stops`。我试着把
`NoProgressDecision::Stop => PostCallVerdict::Stop` 悄悄降级成 `Note` ——
**14 条守卫测试全绿**。

因为那条测试只验了 Nudge 和 Refuse 两档，第三档从没被断言过。
名字里有 `stops`，内容里没有。这是最阴的一类空洞：名字让人以为验过了。

补了 `every_no_progress_rung_survives_after_call`，把三档各自从 `after_call` 走一遍。
再降级 → 立刻红（`拒跑累积到上限后 after_call 必须给出 Stop`）。守卫 14 → 16 条。

## 一句话

**引擎作为进程通过 JSON-RPC 接入，融合层全在我们自己的 crate 里。**
`cargo test -p harnessd` 55 项（50 lib + 2 Go 插件 + 3 真进程 stdio）、基准集 `--selftest` 20/20 全部通过，零编译警告。

---

## S0 地基

| 项 | 落点 | 验证 |
|---|---|---|
| 新 crate | `crates/harnessd`（lib + bin） | `cargo check -p harnessd` |
| 引擎接法 | 进程 + stdio JSON-RPC（`engine.rs`） | `JsonRpcEngine` / `MockEngine` 双实现 |
| 禁止改引擎 | `scripts/check-engine.sh` + `vendor/ENGINE_PIN` | 有改动即 exit 1 |
| CI | `.github/workflows/kernel.yml` | rust + go + bench + pin 四步 |

**为什么不是 lib 依赖**：codewhale 的 engine 是单 crate 55.8 万行、无 feature gate。
编译进我们的树意味着每次上游变动都要重建，这正是"返工"的来源。走协议则上游随便改，
只要 method 不变我们就活着。

## S1 接缝①（orbit ⇄ kernel）

- `mapping.rs`：orbit 的 **35 个命令全部映射**，`every_known_command_maps_to_something` 断言数量等于 35。
- 映射**不是**按文档写的，是按二进制探出来的：`initialize{clientInfo}` 必发，然后探
  `app/capabilities`+`thread/capabilities`+`prompt/capabilities`，只把**对方声明过**的 method 记进 `caps`。
- `Prompt` → `thread/start`（首次）→ `prompt/run{prompt, thread_id, allowed_tools: [], model}`。
  文档里的 `turn/start` 在 0.10.3 上是 `-32601`，已废弃。
- `protocol.rs`：`ui_kind_for` 把引擎通知翻译成 UI 事件；不可识别的走 `engine/<method>` **转发而不是丢弃**（静默丢弃是 UI bug 变隐形的根源）。
- `Auth*` / `Quota*` 六条**诚实返回不适用**，不伪造 OAuth。

## S2 模型与会话

- `SessionState` 持有 thread/model/effort/auto-compact/auto-retry/steer 模式。
- `Fork`/`CloneSession` → `thread/fork`；`SwitchSession` → `thread/resume`；`SetSessionName` → `thread/name/set`。
- 七档 effort 由 `GetAvailableThinkingLevels` 暴露，`turn/start.effort` 生效。

## S3 融合件（这是方案的核心）

| 融合件 | 机制 | 验证测试 |
|---|---|---|
| **守卫否决** | 引擎的审批反向 request（`item/commandExecution/requestApproval`）→ 守卫裁决 → 回 `accept` / `decline` / `cancel`（真实枚举） | `guard_veto_turns_into_a_denial_on_the_approval_channel` |
| **time-bus 六条** | append-only + seq、投影纯函数、只记影响行为的事实、副作用不重放、uncertain 阻塞 | `timebus::tests`（6 项） |
| **steer 安全点** | 只在 item/turn 边界注入，不打断飞行中的响应 | `steer_waits_for_item_boundary_then_reaches_the_engine` |
| **缓存守卫** | 窗口 85% + 命中 90% 两阈值：热前缀→压缩，冷前缀→冻结 | `full_window_with_hot_prefix_triggers_compaction` |
| **不确定即阻塞** | 工具结果无法确认时记 `Uncertain`，绝不重试 | `uncertain_tool_result_blocks_the_turn` |

### S1 补充：orbit 零改动接入（`--compat orbit`）

orbit 说的是 `pi --mode rpc` 的 JSONL 方言：`{"id","type":"prompt","message":…}` →
`{"type":"response",…}` + `message_update` / `agent_end`
（`apps/orbit/crates/orbit-rpc/src/types.rs:206`）。

`crates/harnessd/src/orbit_compat.rs` 做双向翻译，`harnessd serve --compat orbit` 直接吃这套方言。
**orbit 的 UI 一行都不用改**，`client.rs` 的 spawn 点也已接上（`HARNESSD_BIN` 分支）：

```
HARNESSD_BIN=<harnessd> HARNESSD_ENGINE=<codewhale> HARNESSD_ENGINE_ARGS=app-server \
  cargo test -p orbit-rpc --test harnessd_backend     # 1 passed，1.56s，真进程真方言
```

`HARNESSD_BIN` 设置后命令走子进程（内核崩溃不带走 UI）；`HARNESSD_HTTP_ENGINE` 则把同一层
接到工具/审批面。未设置时退回进程内执行器——两条路对 UI 等价，同一个 `CommandBody` 进、同一个 `Event` 出。
端到端由 `orbit_dialect_end_to_end` 覆盖（真二进制、真帧格式、假引擎）。

## S4 插件层（Go）

- `sdk/go/harnessplugin`：单文件 SDK，`Serve(name, WithTool(...), WithEvent(...))`。
- 五条规则落地：协议版本不匹配→装载失败；`policy.bypass` cap 被内核剥离；崩溃三振出局；`uncertain`→阻塞；插件只返值不写状态。
- 六个事件：`tool.pre/tool.post/turn.start/turn.end/compact.pre/guard.deny`。
- 端到端：`crates/harnessd/tests/go_plugin.rs` 真编译 `plugins/example-echo` 并调用工具。
- **第二个真插件 `plugins/guard-safety`**：禁 force push、禁改 `.env`、禁删根、禁 fork 炸弹，
  外加三条 agent 真会干的事——`chmod 777 /`、往 `/etc` 写、`git reset --hard` 丢未提交改动（共 7 条）。
  规则是纯函数（`checkCommand`），Go 单测 22 个用例（含"不该拦的必须放行"：`cat /etc/hosts`、
  `chmod 777 ./scripts`、`git reset --soft`）；Rust 侧
  `crates/harnessd/tests/go_plugin_guard.rs` 两条——一条验规则（含"正常命令必须放行"），
  一条验**拒绝理由真的走到内核日志里**（`guard.deny` 里带 `force` 字样）。
  第一个插件证明"插件能否决"，这一个证明"否决规则本身站得住"。
- **插件可配置**：`PluginSpec` 新增 `env` / `with_env`（内核把环境变量送进插件进程），
  guard-safety 用 `GUARD_SAFETY_CONFIG` 指向一个 JSON 就能关规则、加受保护文件，不用重编。
  `a_plugin_can_be_configured_without_rebuilding_it` 验的是"内核真把它送进去了"：
  关掉 force-push 后同一条命令从 deny 变 allow，而 `safety.rules` 从 4 条变 3 条。

## S5 评测集

- `bench/tasks/*.json` 20 个任务：10 新建（greenfield）+ 10 维护（brownfield）。
- `bench/run.py --selftest` 对每个任务验证两件事：**修前必失败**、**修后必通过**。
  只满足其一的检查会被判 `INVALID`——防止"永远通过"的假判据。
- `bench/run.py --solver '<cmd>'` 用真实 harness 跑分，指标：通过率、新建/维护分别通过率、守卫拒绝数、阻塞轮数、耗时。

## 按方案原文逐条对账（S0–S5 验收表）

方案里每行都写了「验收」。下面是**原文**口径，不是我的改写。

| 验收原文 | 状态 | 证据 |
|---|---|---|
| S0 两仓独立构建，harnessd 能调 `ExecPolicyEngine` | 改为进程依赖（§0.1 / 方案 §7） | `check-engine.sh`：扫描全部 `Cargo.toml`，任何地方提到引擎 crate 或 path 依赖逃出仓库就红（**两个方向都验过会红**），已并进 `gate.sh` |
| S1 orbit 界面能发一条消息，看到流式回复和工具调用 | **通过（机器可判）**；排版好不好看仍需人一眼 | `app::render_tests::a_streamed_reply_and_its_tool_call_land_in_the_transcript`：真子进程喂流式事件 → 断言语义上的**可见对话导出**里有回复文本、工具行也进同一份投影；另有真会话回放两段 | 两段真跑输出回放：`a_real_session_parses_into_the_events_the_ui_renders`（脚本化 + DeepSeek 真会话）都解析出文本增量、`tool_execution_start/end` 成对、`agent_end` |
| S2 切换模型/档位生效；fork 出新会话能跑到不同结论 | **通过** | 真模型路由切换 200；`fork` → 201 新 id；档位实测：`low`/`high` → 201，非法值 → 400 并列出全部取值（`auto, off, minimal, low, medium, high, xhigh, ultra, max`，实测 9 档，不是方案里写的 7 档） |
| S3 流式中途插话生效；10 个长任务不被误杀；压缩前后缓存命中 >90% | **通过** | 中途插话：`probe-steer-midturn.sh` 真引擎实测，慢回合进行中发 steer → `回合 start=1 end=1 ｜ 插话标记进输出 4~5 处`；红方向去掉注入 → 0（见「流式中途插话——已打通」）。误杀 0/10。缓存命中：真模型 20 题测得 **95.6%**，但命中率由上游代理给出、内核不可控，故 §8 与多回合命中率只作**观察项**，不作门禁 |
| S4 写一个 Go 插件，deny 一条命令，内核记录并继续 | **通过** | `go_plugin_can_deny_a_command_and_the_kernel_records_it`：线上回 decline、日志记 `guard.deny`（带 `source: plugin`）、下一问照常 accept |
| S5 通过率/返工率/误杀率有基线数字 | **通过** | 真模型（DeepSeek V4.1 Flash）20/20 = 1.0；返工率 1.3%；误杀率 0.0%；缓存命中 95.6%；平均 20.0s |

### 这一轮修掉的六个真缺陷

1. **守卫的阶梯被自己挡住**：`after_call` 在失败熔断命中时直接 `return Note`，
   于是「同一条命令失败 12 次」永远只拿到温馨提示，升不到拒跑/终止——守卫形同摆设，
   返工照样发生。现在两个守卫都跑、取更强裁决，第 12 次真的 `Deny`
   （`the_same_failure_over_and_over_is_still_caught` 钉住）。
2. **插件只能看不能否**：`policy.check` 从来没被内核调用过，
   `guard.deny` 只是单向广播。现在审批前先问插件，插件 deny 即 `decline`；
   SDK 返回的 `{"content": {...}}` 包装层也一并解掉了。
3. **HTTP 路径下的工具调用从来没进过日志**：SSE 把**整个信封**当参数往下传，
   而 item 在 `payload` 里，于是 `item=None`、一条 `tool.call` 也不记。
   20/20 的基线里「tools=0」本该是红灯，却没人看。修完真跑：每题 tools=1。
4. **assistant 消息同样漏记**：stdio 叫 `agentMessage`、HTTP 叫 `agent_message`，
   只认前者。现在两种都认。
5. **工具调用在界面上不是工具**：`item_start/item_end` 一律翻成 message_start/end，
   界面只会看到"消息"，看不到"在跑 bash"。现在按 item 种类细分成
   `tool_execution_start/end`。
6. **文本增量键名与 orbit 解析器不一致**：我发 `assistant`，orbit 读
   `assistantMessageEvent`（`types.rs:934`）——键名写错不报错，界面就是一片空白。
   已对齐，并用真跑输出回放钉住。

### GUI 这一条：机器能判的都判了，剩下的只有审美

**为什么截不到图（三次独立取证）。** 界面能构建、能启动、能连上 harnessd：

- `cargo build -p orbit-pi` 成功；
- `CGWindowListCopyWindowInfo` 报出窗口 `orbit-pi / "hiq" / 1412×800`；
- `ps` 里能看到它拉起的子进程 `harnessd serve --compat orbit`。

但取不到画面，三种办法结论一致：

1. 全屏 `screencapture` → 纯黑（缩略图只有 1 种颜色）；
2. `caffeinate -d -u` 唤醒后重截 → OCR 读出的是锁屏：
   `11月19日周四 / 03:08 / Mac / 使用触控ID 或输入密码`；
3. 绕过屏幕按窗口抓 `CGWindowListCreateImage` → 2824×1600 的图，**仍然只有 1 种颜色**
   —— 锁屏时系统会把窗口内容一并抹掉。

解锁需要指纹或密码，那不是我能做的。

**能判的那半，已经用测试判了。**
`app::render_tests::a_streamed_reply_and_its_tool_call_land_in_the_transcript`
起一个真子进程，按 orbit 方言喂 `agent_start / turn_start / message_start /
message_update(text_delta) / message_end / agent_end`，然后断言**用户可见的那份对话导出**
（`export_visible_markdown`）里确实有「这是流式回复」；再把 `toolcall_start` 喂进同一份
投影，断言工具行（`bash`）也进得去。

所以 S1 这条现在是「机器可判」而不是「等人看图」。人要做的那一下只是审美：排版顺不顺眼。

### 界面层测试：从「编译不过」到 1931 通过 / 6 红

`orbit-pi` 的测试在我接手前**根本编译不过**（19 个测试文件仍按三参调用
`spawn_with_bin(bin, ws, session)`，而签名早已是两参）。做的三件事：

1. `orbit-rpc` 增加 `spawn_with_program(program, ws, session)`，测试能把假进程注入进来；
2. 19 个测试文件改用它；
3. 删掉两条测**已被移除功能**的用例（外部 Managed/Npm 安装方式、`restart_pi_for_catalog`）。

结果：`cargo test -p orbit-pi` → **1931 passed / 6 failed / 9 ignored**（43 秒）。

剩下 6 条红，逐条说清楚：

| 用例 | 性质 |
|---|---|
| `onboarding::tests` × 3 | 断言的是**旧产品模型**（外部 pi 安装方式、npm 升级、`pi sessions` 面板），已被内嵌内核取代；改它等于改上游产品定义 |
| `git_accounts` × 1 | 临时目录里没有 git 身份，`State::Detected` 断言失败——环境相关 |
| `backdrop` × 1 | 背景版式的长宽比断言（2.0–2.4），与内核无关 |
| `session_park::parked_session_cancels_custom_ui_with_a_top_level_flag` × 1 | **设计差异**：它要求 app 往内核写 `extension_ui_response` 取消一个自定义 UI，而新内核**按设计拒绝 Raw 直通**（`mapping.rs:286`），`cancel_dialog` 明确返回「没有扩展 UI 机制」而不是假成功。要它绿就得给内核加一套扩展 UI 协议——那是产品决定，不是修 bug |

### 界面层测试替我们抓到的三个**真 bug**（真人用界面时也会中）

1. **回执被吞**：子进程路径里 `Event::Response` 投给按 id 等待的调用方之后就 `continue` 了。
   上游的规矩是「先给等待者，**再**投共享流」。而 `send_tracked` 会丢掉自己的 receiver，
   于是 UI 永远看不见回执——fork、切会话这类操作会一直转圈。修好后一次消掉 21 条红。
2. **进程死了没人告诉界面**：`child.wait()` 只把 `is_alive` 翻成 false，没有发
   `Event::ProcessExited`。内核崩了，界面那条「运行中」永不结束，退出清理也不触发。
3. **不认 `PI_BIN`**：上游用它指定可执行文件（发行版和测试都靠它）。内嵌之后它指向的
   仍然是「跑内核的那个程序」，所以并进优先级：显式注入 > `HARNESSD_BIN` > `PI_BIN`。
   进程死后重启/重连的用例因此转绿。

### 指标口径（这次真的能算了）

`bench/run.py` 现在从 time-bus 日志直接算三个数：

- **通过率**：`passed / total`
- **返工率**：`repeats / tool_calls`，即「同一调用在一轮里被执行第二次」的占比。
  首跑不算返工，重跑才算 —— 这是我能自证的口径，不是你定义的，随时可换。
- **误杀率**：带 `guard.deny` 的任务占比
- **缓存命中率**：`Σcached / Σinput`，来源是引擎的 usage 事件

### 真引擎下的插件拦截（S4 的"真"验收）

一键复现：`python3 scripts/probe-approval.py`（需要 `HARNESS_ENGINE_BIN` 和一个干净的
`CODEWHALE_HOME`）。它会自己起引擎、建线程、起轮、等审批、答 deny、检查这一轮正常收尾。
**探针本身验过会红**：`PROBE_AUTO_APPROVE=1` 时它报
`FAIL no approval.required arrived`、退出码 1。

`auto_approve:true` 会让引擎**自己批**命令，审批请求根本不到内核——插件和守卫等于被拔掉。
把默认改成 `false`（`scripts/bench-solve.sh`、`evidence.sh`、`real-task.sh` 都改了）之后，
同一个审批通道暴露了三个真缺陷，全部修掉并用真事件复测：

| 缺陷 | 现象 | 修法 |
|---|---|---|
| 审批帧硬编码 `id = 0`，且 `respond` 去 `inbox` 找一个从没进过 `inbox` 的帧 | `Error: no approval 0 to answer`，turn 直接死 | 单调合成 id + `pending_approvals` 表；按真事件形状摊平 `tool` / `command` |
| 只搬 `approval_id`，命令塞在 `payload.input.command` 里没取出来 | 日志写 `tool:"unknown"`，插件认不出是什么命令 | `approval_frame()` 按实测形状取值，配两条单测（含"command 不能是整坨 JSON"） |
| 审批词表抄 stdio 的 `accept/decline` | 引擎回 400 `expected "allow" or "deny"` | `wire_decision()` 翻译；`cancel` 按 deny 处理（宁可不动，不替用户放行） |
| 同一个 `approval.required` 引擎发两遍 | 第二次应答 404，整个 turn 死 | 按 approval id 去重（`first_sighting`），三条单测 |

修完的真引擎实测（`git reset --hard`）：

```text
guard.deny {"tool":"bash","reason":"git reset --hard discards uncommitted work without a copy","source":"plugin"}
tool.call  {"tool":"bash","status":"committed"}   ← agent 被拒后继续跑 git log
turn.end                                         ← 正常收尾，不是报错
```

这就是 S4 原文"插件 deny 一条命令，内核记录并继续"，第一次在 HTTP 传输上真的成立。

修完又跑了一遍完整的 20 题，确认**走审批通道没有回归**：

| | 旧（`auto_approve:true`，守卫被绕过） | 新（`auto_approve:false`，每步都过策略） |
|---|---|---|
| 通过率 | 20/20 = 1.0 | **20/20 = 1.0** |
| 返工率 | 1.3% | **1.0%** |
| 误杀率 | 0.0 | **0.0** |
| 缓存命中 | 95.6% | **90.1%** |
| 平均耗时 | 20.0 s | **20.05 s** |
| 守卫拒绝次数 | 0（因为根本没被问） | **0**（问了 20 轮，一条都没拦——正常代码不该被拦） |

延迟多了 0.05 秒，换来的是一条命令都不绕过策略。

### 真实仓库实测（不是自造题）

20 题基线是自造的，天花板效应明显。所以又拿**本仓库自己**当维护对象跑了两题：
克隆到 `/tmp`，让它改真文件，用 `cargo test` 当验收门。脚本：`scripts/real-task.sh`。

| 任务 | 改动 | 验收 | 工具调用 | 返工 |
|---|---|---|---|---|
| A：给 `GuardSet` 加 `describe()` + 测试（单文件） | 1 文件 +18 行 | `cargo test -p harness-guards` **15 passed** | 3 | 0 |
| B：加 `describe()` 并再接进 `crates/cli` 的自检输出（跨 crate） | 2 文件 +54 行 | `cargo test --workspace` **全绿**（83/76/47/14/10/15 …） | 20 | **0** |

上面两组是**自动批**跑的。把默认改成走审批之后重跑了一遍，结果一样（`scripts/real-task.sh`）：

| 任务（走策略） | 改动 | 验收 | 工具调用 | 返工 |
|---|---|---|---|---|
| A | 1 文件 +21 行 | `cargo test -p harness-guards` **15 passed** | — | 0 |
| B | 2 文件 +106 行 | `cargo test --workspace` **396 passed / 0 failed** | 15（B 段） | **0** |

走的每一轮都经过 `harnessd` 的策略判断（插件 → 守卫 → 决定），一条命令都没被误拦。

两题都是一轮做完、没有返工、没有守卫拒绝。B 题值得单独说：它要跨两个 crate 读代码、
找到 CLI 里合适的位置、再接上，最后自己跑全量测试——这是"维护旧项目"最典型的形状。

### 真模型基线（这一格终于有真数了）

本机 3425 是 magipe 的 OpenAI 兼容代理（`alma providers` 里的 custom 路由），
把它配成引擎的一个 provider 就能跑真模型，**不需要外部 key**：

```toml
[providers.magipe]
kind = "openai-compatible"
model = "auto/deepseek-v4.1-flash"
api_key = "local-proxy"          # 代理不校验
base_url = "http://127.0.0.1:3425/v1"
context_window = 256000
```

起轮时要带 `model_provider_id = "magipe"`（`/v1/providers/magipe/switch` 会被
引擎拒绝并明确告诉你用这个字段）。

| 模型 | 通过率 | 返工率 | 误杀率 | 缓存命中 | 平均耗时 |
|---|---|---|---|---|---|
| DeepSeek V4.1 Flash（本地代理） | **20/20 = 1.0** | **1.3%** | 0.0% | **95.6%** | 20.0 s |
| 脚本化 provider（人工金标） | 20/20 = 1.0 | 0.0% | 0.0% | null（无 usage） | 3.5 s |
| qwen2.5-coder:1.5b（本地 ollama） | 0/20 | 0.0% | 0.0% | null | 7.3 s |

真模型每题工具调用 2–4 次（`tools=2..4`），返工率 1.3% 意味着约 76 次调用里有 1 次
是同一调用重跑。**这三行合起来读**：管道本身能满分，弱模型一分不得，强模型也能满分
——瓶颈在模型，不在内核。

> 又一条踩出来的教训：`CODEWHALE_HOME` 不能复制。把带 `state.db`/receipt 的目录
> 拷一份会得到 `error: selected owner receipt is invalid`，20 题全 21.5s 失败，
> 而且基线数字看起来「像模型不行」。**只复制 `config.toml`。**

最近一次脚本化真跑（20 题，fresh `CODEWHALE_HOME`）：
通过率 **1.0**、返工率 **0.0**、误杀率 **0.0**、平均 **3.49 s**、每题工具调用 **1**，
缓存命中率 **null** —— 引擎对脚本化 provider 不上报 usage（`input_tokens=0`），
这一格只能等真 provider 才能填。

### S5 基线：真管道、真工具、真改文件

`bench/run.py --solver scripts/bench-solve.sh` 现在跑的是**整条链**：
`harnessd serve --compat orbit` → `codewhale app-server --http` → 工具（bash/read/edit）→ 改工作区 → 自测。

| 跑法 | 通过率 | 新建/维护 | 均值 | 含义 |
|---|---|---|---|---|
| 脚本化 provider（`bench/mock_provider.py`） | 20/20 = 1.0 | 10/10 · 10/10 | 3.74 s | **管道上限**：模型永远做对时，harness 能不能把活干完 |
| 本地 qwen2.5-coder:1.5b（ollama，32k） | 0/20 = 0.0 | 0/10 · 0/10 | 7.34 s | **模型太弱**：它只回文本，从不发 tool_call，文件一个没改 |

复跑：

```
HARNESSD_BIN=./target/debug/harnessd \
HARNESS_ENGINE_BIN=/path/to/codewhale \
CODEWHALE_HOME=/tmp/cw-bench \
python3 bench/run.py --solver "$PWD/scripts/bench-solve.sh"
```

`mock_provider.py` 只做一件事：把 bench 的 gold 变成**真的 tool_call**（`cat > 文件`），
让引擎自己执行、自己写盘。所以 20/20 证明的是接缝②③和评测器，不是模型能力。

两条数字合起来读才对：管道能到 100%，1.5B 本地模型到 0%（它连工具都不肯调）。
也就是说**瓶颈现在是模型，不是 harness**——这正是 S5 要的那个基线。
换 7B 以上、会调工具的模型，数字才有意义；这一步需要一次性下载或一个 provider key。

### 这一轮实测到的三个硬事实（都写进代码注释了）

1. **SSE 是单订阅的**：harnessd 订阅之后，第二个 `curl -N /events` 只收到 `thread.started`。
   所以「轮次结束」不能靠旁路再开一条流判定，必须由 harnessd 自己泵到终态事件再返回。
2. **`ask` 姿态下 shell 安全校验先于审批**：多行命令直接 `BLOCKED: Command contains multiple lines`，
   根本走不到审批那一步；`auto_approve` 才放行多行。守卫否决因此只能拦「引擎愿意问」的命令。
3. **持久化状态会污染下一轮**：同一 `CODEWHALE_HOME` 跑过被拒命令后，后续任务全变慢/全失败；
   干净 home 立刻恢复。基线必须每次用干净的 `CODEWHALE_HOME`。

---

### HTTP 传输实测（`scripts/probe-http.sh` 可复跑）

```
engine ready (http): http:http://127.0.0.1:7883
get_available_models → {"credential":"missing","provider":"deepseek","models":[{"id":"deepseek-chat", …}, …]}
prompt               → error: POST /v1/threads/thr_df30…/turns → HTTP 400
                       {"error":{"message":"… DeepSeek API key not found …"}}
time-bus             → {"seq":0,"kind":"turn.start","payload":{"thread":"thr_df30…"}}
```

三件事被钉住：状态码不被当成 body 解析（404 不会再变成"空成功"）、
引擎错误原样上抛、thread id 在两种方言（`thread.id` / 顶层 `id`）下都能捕获。

## 怎么跑

```bash
cargo test -p harnessd                      # 37 tests
cargo run -p harnessd -- selftest           # 8 项端到端（无模型、无网络、无 key）
cd sdk/go && go test ./...                  # 插件 SDK
python3 bench/run.py --selftest             # 20 任务判据自检
python3 bench/run.py --list                 # 看待跑任务

# 接真引擎：
harnessd serve --engine /path/to/codewhale app-server --log .harness/timebus.ndjson   # stdio，tool-free
harnessd serve --http-engine http://127.0.0.1:7878 --log .harness/timebus.ndjson      # 工具/审批面
./scripts/probe-engine.sh /path/to/codewhale                                          # 协议探针
./scripts/probe-http.sh  /path/to/codewhale                                           # HTTP 冒烟
harnessd replay .harness/timebus.ndjson     # 日志是事实：任何状态可从它重建
harnessd doctor                             # 环境检查
```

## 真二进制核对（2026-11-18，codewhale 0.10.3，自编 debug 版）

**文档说的协议 ≠ 二进制实现的协议。** 这一条是本次最大的修正：

| 依据 | 说法 |
|---|---|
| `crates/app-server-protocol`（codex v2 协议 crate） | `turn/start`、`model/list`、`thread/items/list`、40+ 方法、审批反向 request |
| 真二进制（`app-server` stdio） | 只实现 28 个方法；`turn/start` / `model/list` / `thread/items/list` / `config/read` / `skills/list` 全部 `-32601` |

真二进制的实际方法面：

```
app/{capabilities,config/get,config/list,config/set,config/unset,config/reload,models,request}
prompt/{capabilities,request,run}
thread/{archive,capabilities,create,fork,goal/clear,goal/get,goal/set,interrupt,list,
        loaded/list,message,name/set,read,request,resume,start,unarchive}
```

跑出来的四条硬约束：

1. **必须先 `initialize` 且带 `clientInfo`**，否则一切返回 `Not initialized`。
2. **stdio 是 tool-free**：`prompt/capabilities` 自己写着 `tool_free_turns: true`，
   且提示 `full_api_entrypoint: "codewhale app-server --http"`。
   不带 `allowed_tools: []` 的 prompt 会被拒：
   *"Compatibility transports cannot answer approvals… tool-free turn requests require explicit allowed_tools: []"*。
3. **`thread/start` 拒绝 `approvalPolicy` / `sandbox`**：
   *"threads are durable and tool-free with a fixed read-only sandbox"*。
4. **工具执行与审批只在 HTTP 面**（`/thread`、`/app`、`/prompt`、`/v1/chat/completions`）。

我们据此改了：
- 映射层从「硬编码 codex 方法表」改成 **握手时问 `app/|thread/|prompt/capabilities`，按自报能力选方法**，
  缺能力时返回带探测清单的诚实错误（`crates/harnessd/src/mapping.rs` 的 `Capabilities`）。
- `Prompt` → `thread/start`（首次）→ `prompt/run{prompt, thread_id, allowed_tools: []}`。
- `GetAvailableModels` → `app/models`；`Abort` → `thread/interrupt`；`GetState` → `thread/read`；
  `Compact` / `Auth*` / `QuotaList` → 诚实不适用。

HTTP 面（`app-server --http`，默认 127.0.0.1:7878）实测：

```
GET /healthz      → {"status":"ok","protocol":"v2","service":"deepseek-app-server"}
GET /v1/sessions  → 真实会话列表（id/title/runtime_store/created_at…）
```

也就是说 **codex-v2 协议没错，它只是活在 HTTP 面**；stdio 是兼容控制面（tool-free）。
工具、审批、流式事件都在 `/v1/*`（sessions / threads / turns / approvals / events / usage）。

**活体验证**（`harnessd serve --compat orbit --engine <真二进制> app-server`）：

- `get_available_models` → 真模型表（`deepseek-v4-pro`、`deepseek-flash`、`deepseek-v3.2` 等，带 aliases/capabilities）
- `get_state` → 真状态（autoCompaction/autoRetry/steeringMode…）
- `prompt` → 走到 provider 调用后返回 `-32603 Runtime owner 400`，因为本机 **没有配 provider key**
  （`codewhale auth status`：`not signed in`，provider 表为空）。这不是代码问题，是环境缺 key。

## S1 的真引擎验收（这轮补上的）

之前"界面能发一条消息、看到流式回复和工具调用"这条验收，证据只有 `render_tests` 里的
**假 pi 子进程**——它能证明事件进得来，证明不了这条链对着真引擎跑得通。

补的方式：`crates/orbit-rpc/examples/seam1_real_engine.rs`。orbit 自己的 `PiClient`
拉起 harnessd（靠 `HARNESSD_HTTP_ENGINE` 指向真引擎，和界面走的是同一条路），发一句
要求它真的调用 bash 的 prompt，然后卡三件事：**有流式文本 / 工具调用带真名字 / 收到收尾**。

第一次跑就抓到两个真缺陷：

1. **事件多包了一层**。harnessd 发 `{"type":"tool_execution_start","value":{…}}`，
   而 orbit 的解析器（`Event::from_value`）把**整行**当 `value` 用，于是界面读到的是
   `value.value.item`——工具行渲染成空白。改成平铺（`flat()`）。
2. **工具字段是引擎方言**。HTTP 引擎给的是 `{"item":{metadata:{tool_name,tool_input}}}`，
   界面读的是 `toolCallId` / `toolName` / `args`。加了 `tool_fields()` 把两种方言归一，
   并补了一个测试 `a_tool_row_can_be_opened_by_tool_execution_alone`：**只靠**
   `tool_execution_start` 也要能开出工具行（真引擎下没有 `toolCall` 内容块）。

修完跑出来的是：

```text
文本长度: 13   文本片段: "```\nseam1\n```"
工具调用: ["bash"]
agent_end: true
PASS 接缝① 真引擎
```

这里最该记住的一点：工具那条第一次是 `["?"]`——**事件到了，名字没到**。
`text.is_empty()` 和 `tools.is_empty()` 两个判断都会放它过去，而界面上是一行空白。
现在探针会专门断言"工具名不能是 `?`"。

## S4 的"6 个事件"到底几个

方案里写插件层有 6 个事件。`grep broadcast` 能数出 6 种，但**接了线不等于发得出来**，
所以写了个只会记录的插件（`plugins/event-tap`）真跑一轮，把它收到的每种事件打一行。
`scripts/probe-events.sh` 就是这件事，跑出来是：

```text
收到的事件：tool.post tool.pre turn.end turn.start
条件触发（这一轮没有发生，不是缺陷）：guard.deny（守卫拒绝时才发）、compact.pre（压缩时才发）
```

| 事件 | 一次普通回合 | 证据在哪 |
|---|---|---|
| `turn.start` / `tool.pre` / `tool.post` / `turn.end` | ✅ 都在 | 探针实测 |
| `guard.deny` | 条件触发 | `go_plugin_guard` 测试的真日志：`guard.deny {"reason":"git push --force rewrites shared history…","source":"plugin"}` |
| `compact.pre` | ✅ 端到端跑通了 | `scripts/probe-compact.sh`：真引擎 + 真 harnessd + 真插件，`compact.pre` 真的到达插件 |

**6 种事件现在全部有运行证据**：4 种在普通回合里实测到，`guard.deny` 由真引擎的拒绝场景实测，
`compact.pre` 由 `probe-compact.sh` 端到端实测。

### 为什么 `compact.pre` 之前怎么都触发不了——是个真 bug

缓存守卫的规则是"窗口占用 ≥85% **且** 前缀缓存命中 ≥90% 才建议压缩"。
我把 `HARNESSD_WINDOW` 调到 9000、跑 1.2 万 token，它一声没吭。查下来：

```rust
let input  = inner.get("input_tokens").or(promptTokens)…;   // 方言容错，解析对了
let cached = inner.get("cached_tokens").or(prompt_cache_hit_tokens)…;
…
let prompt = params.get("promptTokens")…;   // ← 又读了一遍驼峰键
self.cache.observe(prompt, cached);
```

日志解析做了方言容错，**守卫看到的却是 0/0**——HTTP/SSE 报的是 `input_tokens`，
驼峰键永远取不到。于是 `advise()` 永远返回 Ok：`cache_freeze` 和 `compact.pre`
在 HTTP 传输下**整条是死的**，而它看起来一切正常。

这和当初"HTTP 下 tools 永远是 0"是同一个病：**两种命名被当成了两种数据**。
修法就一行（用已经解析好的值），另加了一个测试 `the_cache_guard_reads_the_http_usage_dialect`
把它钉住。修完立刻就能触发，说明之前不是"条件苛刻"，是根本没在判断。

### SSE 的顺序：先订阅，再发回合

引擎的 SSE 是**单订阅者**，而且**没有订阅者时它不推进回合**。实测：
`POST /v1/threads/{id}/turns` 返回 201，然后 60 秒内 `items=0`，
连 `turn.started` 都不出现；再发一回合直接 409 `Thread already has an active turn`。

harnessd 原来的顺序是"发完回合再订阅"，于是**第一回合永远不结束**。
改成先订阅再发。这不是风格问题，是这一层唯一的正确顺序。

写测试时还发现：测试里的 mock server 只 `accept()` 一次，SSE 订阅会把唯一的名额
吃掉，于是"发回合"那一步连不上。mock 也得能接多次连接——测试替身的形状
本身就是一条断言。

### 一个失败的建议，把整条事件流打断了

`thread/compact/start` 在 HTTP 传输上**没有实现**。它当时用 `?` 调用，
而调用点在 SSE 事件处理里：一次失败就让 `on_notification` 返回 Err，
后面真正的 `turn.completed` 再也读不到。

结果：timebus 里只有 `turn.start` + `usage` 两条，**回合永远不闭合**。
`turn_open` 永远是 true，所有基于它的投影都错。

修法：压缩是**建议**，不是回合的一部分。失败要记下来（`compact_failed` 事件），
但不能拖垮事件流。`a_failed_compaction_does_not_swallow_turn_end` 钉住这条，
先验过它会红（把 `?` 放回去 → `turn.completed` 不再变成 `turn_end`）。

修完 timebus 正常闭合：`turn.start, usage, turn.end, turn.start`。

### 界面这一格：从"没人看过"变成可复跑

以前 `docs/KERNEL-STATUS.md` 里写着"GUI 没人看过"——那是诚实，但不是证据。
现在 `scripts/probe-ui.sh`：起引擎 + 起 orbit + 截图 + OCR，断言侧栏项目区、
会话搜索框、任务输入框都在。锁屏时明确 `SKIP`（截到的是登录框，不是界面），
不装通过。

08:29 屏幕解锁时实测通过，OCR 读到 `项目 / 搜索会话 / 描述任务 / DeepSeek V4.1 Flash / 已连接`。

红向也验过两次：
- `/bin/sleep` 冒充 orbit → `FAIL 探针起的 orbit 已经退出`（第一版这里是**假绿**：
  屏幕上有上一轮残留的窗口，OCR 照样读到了界面。所以探针现在先清场，再检查自己的进程还活着。）
- 喂一张纯白图 → 三个缺失项逐条列出来

### 又一个"看着成功、其实丢了"

`probe-compact.sh` 一次塞 4 条 prompt 进去，结果只有第 1 条真的跑了，另外 3 条
拿到的是 `{"output": null}` + `success: true`——**消息静默消失**，调用方还以为发出去了。
引擎在「上一轮还没结束」时就是这个回应，而 harnessd 把它当成功转发。

判据改成：本轮写了 `turn.start`、但没有 `turn.end`、也没有 `output` → 报错。
`a_rejected_prompt_is_an_error_not_a_silent_drop` 钉住这条（先验过它会红：
`reply={"output":null}` 拿到 `success: true`）。

这类 bug 的共同点是**坏消息被翻译成了好消息**——和之前缓存守卫读到 0/0、
`check-engine` 打印 `nothing to check` 是同一种病。

### 改完之后重跑真仓库（回归确认）

这一夜改了事件平铺、工具字段方言、缓存守卫、测试等待预算。改完必须在**真仓库**上确认
成功率没被带坏，而不是只看单测绿。`scripts/real-task.sh` 重跑结果：

```text
== 任务 A：单文件加一个方法 + 测试 ==
 crates/guards/src/lib.rs | 17 +++++++++++++++++
 test result: ok. 15 passed; 0 failed        (harness-guards)

== 任务 B：跨 crate 接线 + 全量测试 ==
 crates/cli/src/main.rs   |  2 ++
 crates/guards/src/lib.rs | 31 +++++++++++++++++++++++++++++++
 workspace: 399 passed, 0 failed

== 指标（从 timebus 算，不看模型自述） ==
{"tool_calls": 35, "repeats": 0, "rework_rate": 0.0}
```

两道题都一次过、零返工，全量测试 399 条全绿。**注意任务 B 的 35 次工具调用**（上一轮同题是 15 次）：
题目没变，模型自己决定多跑了几轮测试——这才是"返工率"该量的东西（重复同一调用），
而不是"调用次数多"。所以口径保持不变：`repeats: 0`。

### 假红的两个真实来源（都不是产品 bug）

`compact.pre` 通了之后，连续两次证据跑出了**不一样的失败集**：一次多出
`sidepane_full_width_tests`，一次 `orbit-rpc` 还多一个。查下来两条都不是产品问题：

1. **残留引擎进程。** 探针脚本起了引擎但没收拾干净，机器上常驻 2–3 个
   `app-server --http`，占着端口和 CPU。时序类测试（心跳重绘、布局）在这种环境下会间歇失败。
   修法：`probe-compact.sh` 的 trap 改成按端口兜底清理；`evidence.sh` 在开头记下
   "已在跑的引擎"、结束时只清**这次新起的**，并把残留数量打进报告。
   现在跑完残留是 0。
2. **测试自己依赖渲染顺序。** `expanding_review_opens_the_sessions_sidebar` 把
   "侧栏初始是关的"这句断言写在 `draw()` **之后**，而侧栏的上升沿正是在 render 里处理的——
   于是"这一帧算不算上升沿"变成了运气。断言初始状态就不该让一帧渲染夹在中间，
   挪到 draw 之前，连跑 8 次全过。

这两条加起来说明一件事：**红点检查（known-red）报出来的"未登记失败"，先别当成回归，
但也别当成噪声——先确认机器是干净的、测试是不是自己没写稳。**

## 基线噪声：红点也分"红"和"抖"

`orbit-pi` 有一次在满载下报了 32 个红（平时 6 个）。量了一下才知道区别在哪——
同一条用例单独重复跑 6 次：

| 用例 | 6 次结果 | 判定 |
|---|---|---|
| `a_custom_surface_closes_when_its_process_dies` | **6/6 PASS** | **抖**，不是红（满载时偶发） |
| `parked_session_cancels_custom_ui_with_a_top_level_flag` | 6/6 FAIL | 确定性红（设计差异） |
| `mark_placement_and_small_fallback` | 6/6 FAIL | 确定性红（字体度量） |

根因找到了：这些无头测试用"泵到某个状态出现"的循环，截止时间写死 5 秒（少数 2 秒）。
单独跑够用，满载（十几路并行 + 每路起假进程）时不够——**慢被当成了坏**。
修法不是逐个调数字，而是给所有等待一个统一预算：`app/test_wait.rs` 的 `budget()`
默认 ×4，可用 `ORBIT_TEST_TIMEOUT_SCALE` 调。8 个文件、11 处截止统一走它。

改完连跑两遍全量：`1932 passed; 6 failed` ×2，失败集**一字不差**就是那 6 条登记过的。

所以那条抖动用例**没有**进 `bench/known-red.txt`：把抖动登记成"已知红"，等于给它发免死金牌，
以后真出问题也不会有人知道。不登记，它一抖就被判"未登记的失败"。
`known-red.txt` 里写了这段说明。

顺便记一次自己的踩坑：这个测量我前两次都测了个空——第一次用了 `--lib`（一个用例都没选到），
第二次 `head -1` 抓到的是另一个测试目标的 `0 passed`。两次都"成功退出"。
现在脚本会显式断言"这次真的跑了至少 1 个用例"。

## 会"绿得发空"的检查，全改成了会红

这一类缺陷没有错误信号：检查照跑、结果照绿、什么都没验。一次只修一处，逐条改掉：

| 原来 | 现在 |
|---|---|
| `check-engine.sh`：引擎没 vendored → `nothing to check`，exit 0 | 扫描全部 `Cargo.toml`：提到引擎 crate 或 path 逃出仓库就红（两个方向都验过） |
| `evidence.sh`：某节没产出数字 → 表格空一格，exit 0 | 每格过形状检查，取不到就打印「证据不完整」+ 点名 + exit 1 |
| Go 插件测试：没装 Go → `eprintln` 一句然后 return（算通过） | 默认 **panic**；要跳过必须 `HARNESS_SKIP_GO=1`，且输出留一行脚印 |
| `bench/run.py`：solver 起不来 → 只显示 FAIL，看起来像模型不行 | 统计 `solver_failures`，打印那句真错误，并在结论里明说"这些分数不说明模型能力" |
| `gate.sh`：基线缺 `solver_failures` 字段 → 当作 0，静默通过 | 字段缺失即红；三份历史基线已显式补 0 |

## 证据本身也有自检

`evidence.sh` 产出的每一格现在都要过形状检查（`need`）：`cargo test` 的
`N passed / M failed`、基线必须是 `20 20`、`healthz` 必须是 `"status":"ok"`、
探针必须打出 `PASS`。**任何一格取不到值，脚本打印「证据不完整」并点名，然后 exit 1。**

验过会红：把 `HARNESS_ENGINE_BIN` 指向一个不存在的路径，输出是

```text
## !! 证据不完整
- 引擎 healthz → ''（期望匹配 /"status"\s*:\s*"ok"/）
- 审批通道探针 → 'need HARNESS_ENGINE_BIN'（期望匹配 /^PASS /）
```

为什么加这个：这份脚本以前是 `set -uo pipefail` 但没有 `-e`，某一节编译失败时
grep 抓不到 `test result`，表格里就留一格空白，而脚本照样 exit 0——读的人会以为那一格"没问题"。

## 干净克隆也能过门禁（不依赖本地临时状态）

一直在同一个工作目录上跑测试有个盲区：可能悄悄依赖了没进仓库的文件、
或者我手工留在 `/tmp` 的东西。验法很简单——把仓库 clone 到别处，
从零构建，跑同一条门禁：

```
git clone <repo> /tmp/clean-check && cd /tmp/clean-check
HARNESS_ENGINE_BIN=… CODEWHALE_HOME=… bash scripts/gate.sh
→ D4 检查：没有任何 crate 依赖引擎
→ 验收矩阵：20/20 全 PASS（没有一条 SKIP）
→ 门禁通过
```

结论：提交上去的这份代码是自洽的，不靠本地残留。

## 已知缺口（不粉饰）

按"是不是真的挡事"排序，每一条都写了为什么不挡。

### 挡事的三条

1. **界面排版没有人眼看过。** 机器能判的部分（流式文本、工具行、结束态、退出清理、
   重连）已经有测试钉住（见上面 `render_tests`/1931 条）。剩下的只是"好不好看"，
   需要一台醒着的机器点一次。
2. **上游 `orbit-pi` 只剩 1 条红**（这一轮从 5 减到 1）。

   上一版这里写着"4 条不属于内核，改它们等于改上游产品定义或环境假设"。
   **那句话是错的**，这一轮逐条查完被打脸——`bench/known-red.txt` 里早写过
   一句警告："红点挂太久会被自动归成环境"。实际是：

   - `onboarding::every_dependency_has_an_install_hint` /
     `install_hints_match_the_host`：不是产品模型之争。内嵌内核没有外部 pi 可装，
     `INSTALL_COMMAND` 故意是空串，但测试断言"必须非空"；更糟的是
     `check_dependencies()` 把 pi 当外部依赖去磁盘探测，探不到就得出
     「未安装 + 空安装命令」——**设置页给用户一个空代码框和只复制空串的按钮**。
     改成 `embedded_runtime()`（已安装、版本取本应用、没有命令），并加测试钉住。
   - `onboarding::host_facts_cover_the_platform_and_the_stores`：产品从 Orbit 改名
     hiq 后，测试还写死 `"Orbit config"`。改用 `tr!` 取键。
   - `git_accounts::workspace_local_identity_…`：**真 bug**。Apple Git 2.54 对
     `/etc/gitconfig` 的默认 helper 报 `scope=unknown`，`parse_helpers` 把
     `unknown` 当脏数据、整段 bail → macOS 用户的账号页读不到任何 helper 状态。
     `unknown` 是 git 的合法取值，加进白名单。

   剩下的一条是**真的 design-diff**，不是环境：
   `session_park_tests::parked_session_cancels_custom_ui_with_a_top_level_flag`
   需要内核回 `extension_ui_response{cancelled:true}`，而
   `orbit_rpc::client::cancel_dialog()` 按设计返回 Err（"harness 内核没有扩展 UI
   机制"）。要变绿就得给内核加扩展 UI 子协议——这是产品决策，不伪造。
3. **引擎二进制不在本仓库。** D1 是进程依赖：`harnessd` 只通过 JSON-RPC/HTTP 跟它说话，
   从不链接它的 crate（`check-engine.sh` 守着这条）。所以跑任何实测都要先
   `HARNESS_ENGINE_BIN=/path/to/codewhale`。这是**设计**，不是遗漏；代价是部署时要多带一个二进制。

### 不挡事、但要知道的

4. ~~`compact.pre` 没有端到端跑通过一次。~~ **已关**：先是查出缓存守卫在 HTTP 下
   读不到用量（见上），修完 `probe-compact.sh` 一次就通。
5. **X（Twitter）那一路调研没做。** 候选池只来自 GitHub stars（670 个，含 dim/pi-jax/PIG 校验）
   与手工对比。X 需要登录态，抓不了；结论因此只覆盖了一半来源。
5. **方案原文 S0 写的是"git submodule + engine 作为 lib 路径依赖"**，实施改成了进程依赖。
   原因写在 `KERNEL-FUSION-PLAN.md` §0.1：引擎是别人的代码，D4 禁改；链接进去等于把
   "升级引擎"变成"重新编译我们的内核"。**这里文档与实现有张力，已标注，不改成一致是为了
   保留当时的判断痕迹。**
6. **档位只验证了"引擎接受并透传"**（9 个合法取值、非法值报错、`effort → reasoning_effort`
   有单测）。没有一个模型宣称自己的 reasoning levels（DeepSeek 走代理时 `reasoning_effort_levels: []`），
   所以"不同档位产出不同质量"这件事只能等一个有档位的模型来测。

### 已经完全不是缺口的（之前记在这里，现已做掉）

- ~~工具执行没接~~ → HTTP 传输全通，真模型 20/20 跑过。
- ~~审批字段没对真引擎验证~~ → 按源码对正并有测试钉住。
- ~~没跑过任何真实任务~~ → 跑了 20 题，1.0 通过率。
- ~~orbit 的 client 还没指向 harnessd~~ → 已指向，`spawn_with_program` + `--compat orbit`，
  界面层 1931 条测试就是走这条路。
- ~~守卫阈值没有标定~~ → 10 个长任务误杀 0；同一条失败重复 12 次仍会被拦。
- ~~steer 参数形状未知~~ → `{"prompt": …}`，实测 `steer_count=1`。

## 下一步（按顺序）

上面 S0–S5 都收了，下面是把这套东西变成日常主力要做的四件事，按收益排序。

1. **用 20 题以外的工作负载再测一次稳定性。** 现在的基线是自造 20 题（10 绿 + 10 棕地），
   通过率 1.0 有天花板效应。要证明"不反复返工"，得拿真实仓库跑：一个已有测试的项目、
   一个需要跨多文件改的 bug、一次依赖升级。
2. ~~把返工率做成回归门禁~~ **已做**：`scripts/gate.sh`（通过率 ≥0.95、返工率 ≤0.05、
   误杀率 ==0），`--run` 可先重跑基线；`scripts/install-hooks.sh` 把它装成 pre-push 钩子，
   不达标就推不上去。**门禁本身验过会红**：把基线里的返工率改成 0.2，`gate.sh` 退出码 1。
3. ~~插件层补第二个真插件~~ **已做**：`plugins/guard-safety`（禁 force push / 改 `.env` /
   删根 / fork 炸弹），Go 单测 + Rust 集成测试，理由会写进 `guard.deny` 日志。
   下一步可以是给它加配置（规则开关从文件读，而不是写死在代码里）。
4. **决定要不要给内核加"扩展 UI"协议。** `session_park` 那条红的原因已经查清：它不是接缝
   没接对，而是新内核按设计拒绝 Raw 直通，所以"取消一个扩展自定义 UI"没有对应语义。
   要不要补这套协议，取决于以后插件要不要弹原生对话框（Go 插件目前只走 `tool.pre`）。


---

## 附录：方案 vs 现状的对账（2026-11-20）

| 方案里的约定 | 现状 | 判据 |
|---|---|---|
| 禁止 D4，引擎源码零改动 | 成立 | `scripts/check-engine.sh`：工作区没有任何 crate 依赖引擎 |
| orbit 的 35 条命令全部映射 | 成立 | `CommandBody` 35 个变体，mapping 覆盖 35，未覆盖 0 |
| effort 以 `reasoning_effort` 送引擎 | 成立 | `http_engine` 里有翻译分支 |
| 四接缝：orbit⇄harnessd / harnessd⇄engine / ⇄插件 / ⇄守卫·time-bus·缓存 | 成立 | `crates/guards`、`crates/plugin`、`crates/context`、`harnessd/src/timebus.rs` |
| D1 进程依赖 | 成立 | 引擎是外部二进制，`HARNESS_ENGINE_BIN` 指定 |
| **HTTP/SSE 传输不得自研** | 本次修好 | curl 子进程 → `reqwest` + `reqwest-eventsource` |
| 能力集 = 引擎说支持 ∩ 传输能路由 | 本次修好 | `supported_methods()` 是唯一事实来源 |

---

## 已知 flake：侧栏宽度那条在并发下会红

`app::sidepane_full_width_tests::expanding_fills_the_page_beside_the_sidebar`
在满载并发时偶尔失败，单独跑稳定 PASS；`--test-threads=1` 全量跑也不会红
（实测：单线程 1933 passed / 1 failed，唯一的红是 `session_park` 那条设计差异）。

原因没查到根上：gpui 的测试各自有 `TestAppContext`，但 56 个测试文件都往全局写
`Theme`，怀疑与共享的字体/图集缓存有关。**没有证据就不动它**——
`evidence.sh` 里本来就有"未登记的失败先单跑复核，只有单跑 PASS 才降级为观察项"
的机制，这条会被如实标成 flake，而不是悄悄洗白。

---

## 界面"一阵一阵"的定位与两次被否掉的改法（2026-11-21）

用户反馈：思考和工具调用不连续，思考完停一下才出工具调用，工具出来又停一下。

**机制定位**（不是时序问题，事件已经是实时的）：运行中活动组是展开的
（`transcript_view.rs` 的 `show_work = paint.live || paint.fold_open || !before_answer`），
组内每张工具卡片又按 `agent_calls.open_by_default(false)` 自动展开，
于是每来一个工具，组就长高一大块 —— 视觉上就是"一阵一阵"。

**改法 A（已回滚）**：运行中不渲染活动组，只留活动条。
→ 破 4 条测试，其中 `agent_call_display_sets_the_untouched_default` 明确断言
"follow_run opens a live group"，属上游产品意图，不能单方面改。

**改法 B（已回滚）**：运行中卡片不自动展开（`!live && open_by_default(false)`）。
→ `agent_call_display_sets_the_untouched_default` 转绿，但
`repeated_tools_with_short_thoughts_do_not_accumulate_bottom_space` 红：
指示器贴底距离 24px → 46px。**多画一帧仍 46px，不收敛**，
说明不是单帧测量残差，而是折叠后行高真的少算进了贴底算法。

**最终定位（同日）**：这两次失败都不是产品意图冲突，而是一个真 bug ——
`debug_selector("working-indicator")` 挂在了三行结构的**第一行**头上，
"指示器底部"因此落在第二行上方，贴底距离凭空多一行（24px → 46px）。
把标记挪到整个容器后，改法 B 直接通过。

**已修**：`tool_open = explicit_open || (!live && agent_calls.open_by_default(false))`
—— 运行中卡片只有一行标题（当前命令在活动条里），用户点开的照旧展开，回合结束按偏好显示。
`cargo test -p orbit-pi`：1933 passed / 1 failed（唯一红的是已知设计差异
`session_park_cancels_custom_ui_with_a_top_level_flag`）。
