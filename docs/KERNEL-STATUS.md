# 融合内核实施状态（S0–S5）

> 方案：`docs/KERNEL-FUSION-PLAN.md`。本文件只记**已落地并可复跑**的东西。
> 代码：`crates/harnessd`（Rust 监督层）、`sdk/go/harnessplugin`（插件 SDK）、`bench/`（20 任务评测集）。

---

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

## S5 评测集

- `bench/tasks/*.json` 20 个任务：10 新建（greenfield）+ 10 维护（brownfield）。
- `bench/run.py --selftest` 对每个任务验证两件事：**修前必失败**、**修后必通过**。
  只满足其一的检查会被判 `INVALID`——防止"永远通过"的假判据。
- `bench/run.py --solver '<cmd>'` 用真实 harness 跑分，指标：通过率、新建/维护分别通过率、守卫拒绝数、阻塞轮数、耗时。

## 按方案原文逐条对账（S0–S5 验收表）

方案里每行都写了「验收」。下面是**原文**口径，不是我的改写。

| 验收原文 | 状态 | 证据 |
|---|---|---|
| S0 两仓独立构建，harnessd 能调 `ExecPolicyEngine` | 改为进程依赖（§0.1） | `check-engine.sh` exit 0；`JsonRpcEngine` 只走 JSON-RPC，从不链接引擎 crate |
| S1 orbit 界面能发一条消息，看到流式回复和工具调用 | 数据层已验证（含真模型会话），**界面仍需人眼看一次** | 两段真跑输出回放：`a_real_session_parses_into_the_events_the_ui_renders`（脚本化 + DeepSeek 真会话）都解析出文本增量、`tool_execution_start/end` 成对、`agent_end` |
| S2 切换模型/档位生效；fork 出新会话能跑到不同结论 | **通过** | 真模型路由切换 200；`fork` → 201 新 id；档位实测：`low`/`high` → 201，非法值 → 400 并列出全部取值（`auto, off, minimal, low, medium, high, xhigh, ultra, max`，实测 9 档，不是方案里写的 7 档） |
| S3 流式中途插话生效；10 个长任务不被误杀；压缩前后缓存命中 >90% | **通过** | steer 实测 `steer_count=1`；误杀 0/10；真模型 20 题测得缓存命中 **95.6%**（`prompt_cache_hit_tokens / input_tokens`） |
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

### GUI 这一条为什么卡住（有证据）

我试着把界面跑起来：`cargo build -p orbit-pi` 成功，进程能起、能连上 harnessd
（`HARNESSD_BIN` + `HARNESSD_HTTP_ENGINE`），但取证失败：
`screencapture` 一开始拿到纯黑图（缩略图只有 1 种颜色），用 `caffeinate -d -u` 唤醒后
再截，OCR 读出来的是——

```
11月19日周四
03:08
Mac
使用触控ID 或输入密码
```

也就是**锁屏界面**。窗口在锁屏后面，截不到；解锁需要指纹或密码，那不是我能做的。
所以「界面能不能看到流式回复」不是代码没通，是**机器锁着**。这一条只能等人开屏点一次。

（另一个坑：`orbit-pi` 用的是 GPUI 原生窗口，不是终端 TUI，`tmux capture-pane`
抓到的永远是空白——我第一次的判断也错了。）

### 一个不是我造成、但我得说清楚的坑

`apps/orbit` 里 `orbit-pi`（TUI 二进制）的 19 个测试文件仍按老签名
`spawn_with_bin(bin, workspace, session)` 三参调用，而 `orbit-rpc` 的签名是两参
——**在我接手之前就编译不过**（`84425a2` 那版已是两参）。`orbit-rpc` 全绿，
`orbit-pi` 待修；不要把它当绿。

### 指标口径（这次真的能算了）

`bench/run.py` 现在从 time-bus 日志直接算三个数：

- **通过率**：`passed / total`
- **返工率**：`repeats / tool_calls`，即「同一调用在一轮里被执行第二次」的占比。
  首跑不算返工，重跑才算 —— 这是我能自证的口径，不是你定义的，随时可换。
- **误杀率**：带 `guard.deny` 的任务占比
- **缓存命中率**：`Σcached / Σinput`，来源是引擎的 usage 事件

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

## 已知缺口（不粉饰）

1. ~~工具执行还没接~~ **已接 HTTP 传输**（`crates/harnessd/src/http_engine.rs`）：
   `POST /v1/threads` 建线程 → `POST /v1/threads/{id}/turns {prompt}` 起轮（工具/审批在这一面），
   `GET /v1/threads/{id}/events` SSE 流式、`POST …/interrupt`、`…/steer`、`POST /v1/approvals/{id}` 审批。
   模型目录走 `GET /v1/providers` + `/v1/providers/{id}/models`（没有 `/v1/models`，404 已实测）。
2. ~~审批应答的字段名未对真引擎验证过~~ **已按源码对正**：
   `CommandExecutionRequestApprovalResponse { decision }`，
   取值 `accept / acceptForSession / acceptWithExecpolicyAmendment / applyNetworkPolicyAmendment / decline / cancel`
   （`app-server-protocol/src/v2.rs:542-560`）；
   请求侧是 `command` + `commandActions[]`（`read|listFiles|search|unknown`）+ `itemId`，
   **没有 tool 字段**（同文件 `:584-608`）。
   我们已把 `commandActions` 映射成守卫认识的 `read_file/list_dir/grep`，应答改用 `accept/decline/cancel`，
   并由 `real_engine_payload_maps_command_actions_to_our_tool_names` 与
   `approval_accepts_with_the_engines_own_wording` 两个测试钉住。
3. **未跑过任何真实任务**。二进制能编、能握手、能列模型、能起轮；
   起轮一路走到 provider 后返回 `400 DeepSeek API key not found`（错误原文已透传，不是假成功）。
   缺 key 是唯一阻塞，`bench --solver` 因此仍无基线数字。
4. **orbit 的 `client.rs` 还没指向 harnessd**。协议层已兼容（`--compat orbit`），
   但 orbit 目前仍在进程内直接驱动旧内核；把它改成 spawn `harnessd serve --compat orbit` 是最后一步接线。
5. 守卫的三级阶梯阈值沿用现项目 `harness-guards`，没有在 20 任务上标定过误杀率。
6. `thread/message`（steer 与 HTTP 侧交互）的参数形状未验证：stdio 有这个方法，但没探到参数结构。

## 下一步（按顺序）

1. 加 HTTP 传输（`app-server --http` → `/v1/*`，SSE），把工具、审批、流式事件真正接上；
   已确认入口：`/healthz`、`/v1/sessions`，默认端口 7878，本地可用 `--insecure-no-auth`
2. 配 provider key（`DEEPSEEK_API_KEY` 或 `codewhale auth`）→ 跑 `bench --solver` 出第一版基线
2. orbit `client.rs` 换后端（SSE → app-server JSON-RPC），复用现有 `bridge.rs` 的事件映射
3. 用基线数字标定守卫阈值（误杀率目标：合法长任务 0 误杀）
