//! The supervisor: where all four seams meet.
//!
//! It is deliberately a *client* of the engine, which is what lets guard veto work
//! without patching 558k lines: codewhale asks its client for approval before a
//! command runs (`item/commandExecution/requestApproval`), and our answer to that
//! reverse request is the veto.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::cacheguard::{Advice, CacheGuard};
use crate::engine::Engine;
use crate::guards::{GuardBridge, Verdict};
use crate::mapping::{map, Action, SessionState};
use crate::plugins::PluginHost;
use crate::protocol::{ui_kind_for, Frame, Kind, UiEvent};
use crate::steer::{SafePoint, SteerGate, SteerMode};
use crate::timebus::{digest, Log};

pub struct Supervisor<E: Engine> {
    pub engine: E,
    pub log: Log,
    pub guards: GuardBridge,
    pub steer: SteerGate,
    pub cache: CacheGuard,
    pub plugins: PluginHost,
    pub state: SessionState,
    text_buffer: String,
    /// 思考增量。单独一条缓冲：它不进 assistant 正文的日志条目，
    /// 但回放时要能和正文分开（正文是结论，思考是过程）。
    reasoning_buffer: String,
    /// 本轮内出现过的工具调用签名 → 次数。返工率就是从这里量的：
    /// 同一条命令在一轮里被执行两次，才是真的"重做"。
    tool_seen: std::collections::HashMap<String, u32>,
    approvals: u64,
    denials: u64,
}

impl<E: Engine> Supervisor<E> {
    pub fn new(engine: E, log: Log, window_tokens: u64) -> Self {
        Self {
            engine,
            log,
            guards: GuardBridge::new(),
            steer: SteerGate::new(SteerMode::Immediate),
            cache: CacheGuard::new(window_tokens),
            plugins: PluginHost::new(),
            state: SessionState::default(),
            text_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_seen: std::collections::HashMap::new(),
            approvals: 0,
            denials: 0,
        }
    }

    /// Handshake. The shipped app-server answers everything with "Not initialized"
    /// until `initialize` carries `clientInfo`, and its capability endpoints are
    /// the only truthful description of what this build can do.
    pub fn prepare(&mut self) -> Result<()> {
        let version = env!("CARGO_PKG_VERSION");
        self.engine.request(
            "initialize",
            json!({ "clientInfo": { "name": "harnessd", "version": version } }),
        )?;

        let mut methods = std::collections::BTreeSet::new();
        for probe in ["app/capabilities", "thread/capabilities", "prompt/capabilities"] {
            let Ok(v) = self.engine.request(probe, json!({})) else { continue };
            if let Some(list) = v.get("methods").and_then(Value::as_array) {
                for m in list.iter().filter_map(Value::as_str) {
                    methods.insert(m.to_string());
                }
            }
            if let Some(tf) = v.get("tool_free_turns").and_then(Value::as_bool) {
                self.state.caps.tool_free = tf;
            }
            if let Some(fa) = v.get("full_api_entrypoint").and_then(Value::as_str) {
                self.state.caps.full_api = Some(fa.to_string());
            }
        }
        // 兜底用"这条传输自己能路由的"，不是靠引擎探测（探测经常全失败）。
        match self.engine.supported_methods() {
            Some(list) => {
                for m in list {
                    methods.insert((*m).to_string());
                }
            }
            None => {
                for m in ["thread/start", "thread/read", "app/models"] {
                    methods.insert(m.to_string());
                }
            }
        }
        // 能力集**永远**收敛到"本传输真能路由的"，不管探测成没成。
        //
        // 这里踩过一个连环坑，值得记下来：
        //   1. 引擎没有 `/v1/prompt/capabilities` 这个端点，探测三个全失败；
        //   2. 以前整块写在 `if !methods.is_empty()` 里，所以探测失败时
        //      `caps.methods` 保持**空**；
        //   3. 而 `has()` 把空集解释成"什么都能做"（is_empty() || contains），
        //      于是 `GetCommands` 挑中了 `prompt/capabilities` 去调，
        //      得到 "'prompt/capabilities' is not mapped on the HTTP transport"，
        //      界面上是一整条红：`Get commands 失败`。
        // 空集意味着"不知道"，不该意味着"全都能"。至少要把我们确定支持的那些填进去。
        let before = methods.len();
        methods.retain(|m| self.engine.supports(m));
        if methods.len() != before {
            eprintln!(
                "capabilities: 丢弃 {} 个本传输路由不了的方法",
                before - methods.len()
            );
        }
        self.state.caps.methods = methods;
        self.pump()?;
        Ok(())
    }

    /// One orbit command in, one reply plus any events the engine produced.
    pub fn handle(&mut self, cmd: &str, params: Value) -> Result<(Value, Vec<UiEvent>)> {
        match map(cmd, &params, &mut self.state) {
            Action::Local { reply } => Ok((reply, Vec::new())),
            Action::Unsupported { reason } => bail!("{reason}"),
            Action::Call { method, params } => {
                // 这一轮**开始前**的条目数。用来判断引擎到底跑没跑——
                // 光看 `turn_open` 不行：`handle` 进来时就把它置位了。
                let entries_before = self.log.entries().len();
                // A turn starts on either dialect: codex-style `turn/start` or the
                // native `prompt/run`. Missing this line means the log has a turn
                // that never opened — the projection would then be wrong.
                if method == "turn/start" || method.starts_with("prompt/") {
                    self.guards.begin_turn();
                    self.text_buffer.clear();
                        self.reasoning_buffer.clear();
                    self.tool_seen.clear();
                    self.log.append("turn.start", json!({ "thread": self.state.thread_id }))?;
                    self.plugins.broadcast("turn.start", json!({ "thread": self.state.thread_id }));
                }
                let mut reply = self.engine.request(&method, params)?;

                // 哪些方法会让「当前线程」变。
                //
                // 以前这里只认 `thread/start`，注释还写着"新线程 id 只会在这里出现"。
                // 那句话是错的：`thread/fork` 也创建线程。
                //
                // 踩过的后果——fork 之后出现**两个真相**：
                //   - http_engine 自己把 thread_id 换成新线程（回合发到新线程）
                //   - Supervisor 的 state.thread_id 还是旧线程（get_entries 读旧线程）
                // 于是「在新线程上跑的回合，读回来的历史是旧线程的」。
                // get_entries / get_messages / 再次 fork / set_session_name 全都读错线程。
                // 这条只能靠真引擎探针抓：单测里 mock 不区分线程。
                if matches!(
                    method.as_str(),
                    "thread/start" | "thread/fork" | "thread/resume"
                ) {
                    // Two dialects: stdio nests the id under `thread`, the HTTP
                    // runtime returns the thread object at the top level.
                    if let Some(id) = reply
                        .pointer("/thread/id")
                        .or_else(|| reply.get("threadId"))
                        .or_else(|| reply.get("id"))
                        .and_then(Value::as_str)
                    {
                        self.state.thread_id = Some(id.to_string());
                    }
                }
                // A prompt stashed while the thread was being created runs now,
                // and its thread id is the one the caller gets back.
                if let Some(text) = self.state.pending_prompt.take() {
                    if self.state.thread_id.is_none() {
                        // Never swallow a prompt because a thread id went missing.
                        anyhow::bail!("engine created no thread id; prompt not sent: {text:?}");
                    }
                    if let Some(thread) = self.state.thread_id.clone() {
                        self.guards.begin_turn();
                        self.text_buffer.clear();
                        self.log.append("turn.start", json!({ "thread": thread }))?;
                        self.plugins.broadcast("turn.start", json!({ "thread": thread }));
                        let mut params =
                            json!({ "prompt": text, "thread_id": thread, "allowed_tools": [] });
                        if let Some(m) = self.state.model.clone() {
                            params["model"] = json!(m);
                        }
                        let out = self.engine.request("prompt/run", params)?;
                        reply = json!({
                            "thread": { "id": thread },
                            "output": out.get("output"),
                            "model": out.get("model"),
                            // 回合 id 必须带着，否则上层判据（见下方 `accepted`）
                            // 拿不到证据，会把好回合误判成"被拒"。
                            "turn_id": out.get("turn_id"),
                        });
                    }
                }
                let events = self.pump()?;
                // 引擎可能**根本没跑这一轮**（上一轮还没结束、或者它自己拒了）。
                // 判据：既没有 output，也没有任何回合事件。这种情况必须报错——
                // 回 success 会让 orbit 以为消息发出去了，而它静默消失了。
                // 这不是假想：连续发 4 条 prompt 时只有第 1 条真的跑了，另外 3 条
                // 都拿到了 `{"output": null}` + `success: true`。
                // 判"引擎到底接没接这一轮"，唯一可靠的同步证据是**回合 id**。
                //
                // HTTP 上 `POST /turns` 成功就带 `turn_id`；被拒（上一轮还没结束、
                // 线程状态不对）则没有。stdout 方言同理，`prompt/run` 成功会带
                // `output` 或回合事件。
                //
                // 不能用 `turn.start`：那一行是**我们自己**在发请求前写的，永远在——
                // 用它判断"引擎跑没跑"等于自己给自己作证。
                // 也不能用"这一瞬间 pump 到了什么"：HTTP 回合是异步的，
                // 201 返回时引擎还没产出事件，那一瞬间永远是空的。
                // （踩过：第 1 回合正常，第 2 回合被这条误判成"被拒"。）
                let accepted = reply
                    .pointer("/turn/turn_id")
                    .or_else(|| reply.pointer("/turn/id"))
                    .or_else(|| reply.get("turn_id"))
                    .and_then(Value::as_str)
                    .is_some()
                    || reply.get("output").map(|v| !v.is_null()).unwrap_or(false)
                    || reply.get("model").is_some();
                if (method == "prompt/run" || method == "prompt/request" || method == "turn/start")
                    && !accepted
                {
                    anyhow::bail!(
                        "engine rejected this turn: no turn id, no output (prompt dropped)"
                    );
                }

                Ok((reply, events))
            }
        }
    }

    /// Drain engine output: apply guards, steer, cache advice, and log facts.
    pub fn pump(&mut self) -> Result<Vec<UiEvent>> {
        let frames = self.engine.notifications();
        let mut events = Vec::new();
        for frame in frames {
            match frame.kind() {
                Kind::Request => {
                    if let Some((id, resp)) = self.on_reverse_request(frame)? {
                        self.engine.respond(id, resp)?;
                    }
                }
                _ => {
                    if let Some(method) = frame.method.clone() {
                        events.extend(self.on_notification(&method, frame.params.unwrap_or(Value::Null))?);
                    }
                }
            }
        }
        Ok(events)
    }

    /// Guard veto lives here. codewhale blocks on our answer, so a `deny` here is
    /// a real refusal, not advice the model can ignore.
    fn on_reverse_request(&mut self, frame: Frame) -> Result<Option<(u64, Value)>> {
        let method = frame.method.clone().unwrap_or_default();
        let params = frame.params.clone().unwrap_or(Value::Null);
        let tool = tool_name(&params, &method);
        let args = params.to_string();

        // 插件可以先于守卫一票否决（S4 的验收：插件 deny 一条命令，内核记录并继续）。
        // 插件只在「引擎愿意问」的那一刻有发言权——它的位置是审批通道，不是命令行。
        // 插件要有话可说，就得看到**命令本身**，不是整个参数 JSON。
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or(args.as_str());
        if let Some(Ok(v)) = self.plugins.call_tool(
            "policy.check",
            json!({ "tool": tool, "command": command }),
        )
        {
            // SDK 把工具返回值包在 `content` 里（`{"content": {...}}`），
            // 两种形状都认，免得插件作者被包装层坑一次。
            let verdict = v.get("content").unwrap_or(&v);
            let denied = verdict.get("decision").and_then(Value::as_str) == Some("deny")
                || verdict.get("allow").and_then(Value::as_bool) == Some(false);
            if denied {
                let reason = verdict
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin denied this command")
                    .to_string();
                self.denials += 1;
                self.log.append(
                    "guard.deny",
                    json!({ "tool": tool, "reason": reason, "source": "plugin", "digest": digest(&args) }),
                )?;
                self.plugins.broadcast(
                    "guard.deny",
                    json!({ "tool": tool, "reason": reason, "source": "plugin" }),
                );
                return Ok(Some((frame.id.unwrap_or(0), json!({ "decision": "decline" }))));
            }
        }

        let verdict = self.guards.before(&tool, &args);
        let decision = match verdict {
            Verdict::Deny(reason) => {
                self.denials += 1;
                self.log.append(
                    "guard.deny",
                    json!({ "tool": tool, "reason": reason, "digest": digest(&args) }),
                )?;
                self.plugins.broadcast("guard.deny", json!({ "tool": tool, "reason": reason }));
                // Wire vocabulary is `accept` / `decline` / `cancel`, not approved/denied.
                json!({ "decision": "decline" })
            }
            Verdict::AllowWithNote(note) => {
                self.approvals += 1;
                self.plugins.broadcast("tool.pre", json!({ "tool": tool, "note": note }));
                // The note goes to the UI, not the wire: the engine's response
                // struct carries `decision` only.
                json!({ "decision": "accept" })
            }
            Verdict::Allow => {
                self.approvals += 1;
                self.plugins.broadcast("tool.pre", json!({ "tool": tool }));
                json!({ "decision": "accept" })
            }
            Verdict::Stop(_) => {
                self.approvals += 1;
                self.plugins.broadcast("tool.pre", json!({ "tool": tool }));
                json!({ "decision": "cancel" })
            }
        };
        Ok(frame.id.map(|id| (id, decision)))
    }

    fn on_notification(&mut self, method: &str, params: Value) -> Result<Vec<UiEvent>> {
        let mut out = vec![UiEvent::new(ui_kind_for(method), params.clone())];

        // 工具调用如果按 item_start/item_end 走，UI 只会把它当成一条消息，
        // 看不到"在跑什么工具"。按 item 的种类再细分一次。
        if method == "item/started" || method == "item/completed" {
            if let Some(kind) = params.pointer("/item/kind").and_then(Value::as_str) {
                if kind == "tool_call" {
                    out[0].kind = if method == "item/started" {
                        "tool_start".into()
                    } else {
                        "tool_end".into()
                    };
                }
            }
        }

        match method {
            // 注意：增量**不必**在这里 push 事件。函数开头那句
            //   `let mut out = vec![UiEvent::new(ui_kind_for(method), params.clone())]`
            // 已经把它按 `text_delta` 逐条发出去了——流式本来就是通的。
            // 我一开始以为"不流式"是这里只攒不发，加了第二份 push，
            // 结果每条增量发两遍（单测立刻抓到 ["一","一","二","二"]）。
            // 真正让界面"像一次性蹦出来"的是缺 `agent_start`：
            // 界面不认识"有一轮在跑"，就把增量憋着，等结算再一起放。
            "item/agentMessage/delta" => {
                if let Some(d) = params.get("delta").and_then(Value::as_str) {
                    self.text_buffer.push_str(d);
                }
            }
            // 思考增量单独走一条线（方法名由 http_engine 按 `payload.kind` 分出来）。
            // 以前它和工具输出混在一个分支里，界面上"思考"根本不是思考。
            "item/agentReasoning/delta" => {
                if let Some(d) = params.get("delta").and_then(Value::as_str) {
                    self.reasoning_buffer.push_str(d);
                }
            }
            "item/completed" => {
                let item = params.get("item").cloned().unwrap_or(Value::Null);
                let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
                // 两种方言：stdio 把种类放在 `type`（agentMessage），
                // HTTP/SSE 放在 `kind`（agent_message）。
                //
                // 以前这里只看 `type`。后果不是"少个字段"，是**HTTP 路径下
                // 每一条 assistant 回答都不进日志**——而"日志是事实，其余都是
                // 投影"是这个内核的第一条原则。日志里没有的东西，回放、分叉、
                // 压缩、历史读回来全都没有。用户看到的就是：跑了一个命令，
                // 然后没有下文。
                let kind_field = item.get("kind").and_then(Value::as_str).unwrap_or_default();
                let is_agent_message = kind.contains("agentMessage")
                    || kind == "agent_message"
                    || kind_field.contains("agentMessage")
                    || kind_field == "agent_message";
                if is_agent_message {
                    let streamed = std::mem::take(&mut self.text_buffer);
                    // 有的 provider 不流式：一条 delta 都没有，只有 item 里的
                    // 成品文本。界面按 delta 渲染的话，这时一个字都不显示。
                    // 所以 delta 为空时，用 item.detail 补一条 text_delta 出去。
                    let detail = item
                        .get("detail")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let text = if streamed.trim().is_empty() {
                        if !detail.trim().is_empty() {
                            out.insert(0, UiEvent::new("text_delta", json!({ "delta": detail })));
                        }
                        detail
                    } else {
                        streamed
                    };
                    if !text.trim().is_empty() {
                        self.log.append("message", json!({ "role": "assistant", "text": text }))?;
                    }
                }
                // 两种方言：stdio 的 item.type 是 commandExecution/mcpToolCall，
                // HTTP/SSE 的 item.kind 是 tool_call 且名字在 metadata.tool_name。
                // 只认前者会让 HTTP 跑分永远是 tools=0——指标看着绿，其实没量。
                let http_tool = item.get("kind").and_then(Value::as_str) == Some("tool_call");
                if kind.contains("commandExecution") || kind.contains("mcpToolCall") || http_tool {
                    let call_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let tool = item
                        .pointer("/metadata/tool_name")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("command").and_then(Value::as_str))
                        .unwrap_or(if http_tool { "tool" } else { kind })
                        .to_string();
                    let input = item
                        .pointer("/metadata/tool_input")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    // Rule 5 of the time-bus: an effect we did not observe is
                    // `uncertain` and blocks, it is never assumed to have happened.
                    let status = match item.get("status").and_then(Value::as_str) {
                        Some("completed") | Some("success") => "committed",
                        Some("failed") => "committed",
                        _ => "uncertain",
                    };
                    let signature = format!("{tool}\u{0}{input}");
                    let seen = self.tool_seen.entry(signature).or_insert(0);
                    *seen += 1;
                    let repeat = *seen > 1;
                    self.log.append(
                        "tool.call",
                        json!({
                            "call_id": call_id,
                            "tool": tool,
                            "status": status,
                            "repeat": repeat,
                            "digest": digest(&input),
                        }),
                    )?;
                    self.plugins.broadcast("tool.post", json!({ "tool": tool, "status": status }));
                }
                // Steer releases at an item boundary, never mid-token.
                if let Some(msg) = self.steer.at_safe_point(SafePoint::ItemDone) {
                    if let Some(thread) = self.state.thread_id.clone() {
                        self.engine.request(
                            "turn/steer",
                            json!({ "threadId": thread, "input": [{ "type": "text", "text": msg }] }),
                        )?;
                        out.push(UiEvent::new("steer_sent", json!({ "text": msg })));
                    }
                }
            }
            "turn/completed" => {
                self.log.append("turn.end", json!({}))?;
                self.plugins.broadcast("turn.end", json!({}));
                // Follow-ups queued during the turn continue the conversation
                // instead of being silently dropped.
                if self.steer.has_follow_up() {
                    if let Some(msg) = self.steer.at_safe_point(SafePoint::TurnDone) {
                        if let Some(thread) = self.state.thread_id.clone() {
                            self.engine.request(
                                "turn/start",
                                json!({ "threadId": thread, "input": [{ "type": "text", "text": msg }] }),
                            )?;
                            out.push(UiEvent::new("follow_up_started", json!({ "text": msg })));
                        }
                    }
                }
            }
            "thread/tokenUsage/updated" | "usage" | "turn/usage" => {
                // 缓存命中率只能从引擎报的 usage 里量：input 与 cached。
                // 两种命名都认（stdio 驼峰，HTTP/SSE 蛇形）。
                // turn.usage 的形状是 {"usage": {...}}，老形状是平铺的；
                // 两种都认，否则缓存命中率永远是 null。
                let inner = params.get("usage").unwrap_or(&params);
                let input = inner
                    .get("input_tokens")
                    .or_else(|| inner.get("promptTokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let cached = inner
                    .get("cached_tokens")
                    .or_else(|| inner.get("cachedTokens"))
                    // 引擎对 DeepSeek 系报的是这个字段名（实测 turn.usage）。
                    .or_else(|| inner.get("prompt_cache_hit_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if input > 0 {
                    self.log.append(
                        "usage",
                        json!({ "input_tokens": input, "cached_tokens": cached }),
                    )?;
                }
                // 用上面已经按方言解析好的值。**不要**在这里重新读驼峰键：
                // HTTP/SSE 报的是 `input_tokens`/`cached_tokens`，驼峰永远取不到，
                // 缓存守卫就会一直拿 0/0 判断（= 永远"没问题"），
                // `cache_freeze` 和 `compact.pre` 在 HTTP 上等于死路。
                // 这条和当年"HTTP 下 tools 永远是 0"是同一个病：命名不同 ≠ 没有数据。
                self.cache.observe(input, cached);
                match self.cache.advise() {
                    Advice::Ok => {}
                    Advice::FreezePrefix { reason } => {
                        out.push(UiEvent::new("cache_freeze", json!({ "reason": reason })));
                    }
                    Advice::CompactNow { reason } => {
                        if self.state.auto_compact {
                            if let Some(thread) = self.state.thread_id.clone() {
                                self.plugins.broadcast("compact.pre", json!({ "reason": reason }));
                                // 压缩请求**不能**用 `?`：这里正处在 SSE 事件流里，
                                // 一个失败的方法会中断整条流，`turn.completed` 就永远读不到，
                                // 回合在 timebus 里永远不闭合（实测就是这样：
                                //  timebus 只有 turn.start + usage 两条）。
                                // 压缩是建议，不是回合的一部分——失败要说出来，但别拖垮事件流。
                                if let Err(e) = self.engine.request(
                                    "thread/compact/start",
                                    json!({ "threadId": thread, "reason": reason }),
                                ) {
                                    out.push(UiEvent::new(
                                        "compact_failed",
                                        json!({ "reason": reason, "error": e.to_string() }),
                                    ));
                                }
                            }
                        }
                        out.push(UiEvent::new("compact_requested", json!({ "reason": reason })));
                    }
                }
            }
            "thread/compacted" => {
                self.log.append("compact", json!({}))?;
                self.cache.after_compaction();
            }
            _ => {}
        }
        Ok(out)
    }

    pub fn stats(&self) -> Value {
        json!({
            "thread": self.state.thread_id,
            "approvals": self.approvals,
            "denials": self.denials,
            "guard": { "denials": self.guards.stats().0, "stops": self.guards.stats().1 },
            "steer": { "pending": self.steer.pending(), "released": self.steer.released() },
            "cache": { "hit_rate": self.cache.hit_rate() },
            "plugins": self.plugins.names(),
            "log": { "entries": self.log.entries().len(), "next_seq": self.log.next_seq() },
        })
    }
}


/// The engine identifies a pending action by `command` + `commandActions`, not by a
/// tool name. Map its vocabulary onto ours so the guards see the same words they
/// were written against. (`CommandAction` variants: read / listFiles / search /
/// unknown — crates/app-server-protocol/src/v2.rs:405.)
fn tool_name(params: &Value, method: &str) -> String {
    if let Some(t) = params
        .get("tool")
        .or_else(|| params.get("toolName"))
        .and_then(Value::as_str)
    {
        return t.to_string();
    }
    if let Some(actions) = params.get("commandActions").and_then(Value::as_array) {
        if let Some(kind) = actions
            .first()
            .and_then(|a| a.get("type"))
            .and_then(Value::as_str)
        {
            return match kind {
                "listFiles" => "list_dir",
                "read" => "read_file",
                "search" => "grep",
                _ => "shell",
            }
            .to_string();
        }
    }
    if method.contains("fileChange") {
        return "apply_patch".to_string();
    }
    if params.get("command").is_some() || params.get("itemId").is_some() {
        return "shell".to_string();
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::MockEngine;
    use serde_json::json;

    fn tmp_log(name: &str) -> Log {
        let mut p = std::env::temp_dir();
        p.push(format!("harnessd-srv-{name}-{}.ndjson", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Log::open(p).unwrap()
    }

    #[test]
    fn prompt_creates_thread_logs_turn_and_streams_text() {
        let mut engine = MockEngine::default();
        engine.emit("item/agentMessage/delta", json!({"delta":"hello"}));
        engine.emit("item/completed", json!({"item":{"type":"agentMessage","id":"i1"}}));
        engine.emit("turn/completed", json!({}));
        let log = tmp_log("prompt");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(engine, log, 128_000);

        let (_reply, events) = sup.handle("Prompt", json!({"text":"hi"})).unwrap();
        assert_eq!(sup.state.thread_id.as_deref(), Some("thr_mock"));
        assert!(events.iter().any(|e| e.kind == "turn_end"));
        assert!(sup.engine.responses().is_empty());

        let proj = sup.log.project();
        assert_eq!(proj.messages.len(), 1);
        assert_eq!(proj.messages[0]["text"], "hello");
        assert!(proj.receipts.is_empty());
        assert!(!proj.turn_open);

        // replay: same projection from disk, no writes
        let replayed = crate::timebus::Log::open(&path).unwrap().project();
        assert_eq!(replayed.messages, proj.messages);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn http_shaped_thread_reply_still_runs_the_prompt() {
        // The HTTP runtime returns {"id": "thr_…"} at the top level, not {"thread":{"id":…}}.
        let engine = MockEngine::new(|method, _| match method {
            "thread/start" => json!({"id": "thr_http", "model": "deepseek-flash"}),
            "prompt/run" => json!({"output": "hi", "model": "deepseek-flash"}),
            _ => json!({}),
        });
        let log = tmp_log("http-shape");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(engine, log, 128_000);
        let (reply, _) = sup.handle("Prompt", json!({"text":"hi"})).unwrap();
        assert_eq!(sup.state.thread_id.as_deref(), Some("thr_http"));
        assert!(sup.engine.calls().iter().any(|c| c == "prompt/run"), "prompt was dropped");
        assert_eq!(reply["output"], "hi");
        let _ = std::fs::remove_file(&path);
    }

    /// fork 之后，**当前线程必须跟着换**。
    ///
    /// 踩过：`handle` 只认 `thread/start` 会给出新线程 id，注释还写着
    /// "新线程 id 只会在这里出现"。fork 也会创建线程，于是 fork 后出现两个真相：
    ///   - http_engine 自己换成新线程 → 回合发到新线程
    ///   - Supervisor 的 state.thread_id 还是旧线程 → get_entries 读旧线程
    /// 结果"新线程上跑的回合，读回来的历史是旧线程的"。
    /// 单测里 mock 不区分线程，真引擎探针（probe-fork-branch.sh）才抓到。
    #[test]
    fn fork_moves_the_current_thread_so_later_reads_follow_it() {
        let engine = MockEngine::new(|method, _| match method {
            "thread/start" => json!({ "id": "thr_old" }),
            "thread/fork" => json!({ "id": "thr_new" }),
            "thread/read" => json!({ "items": [] }),
            _ => json!({}),
        });
        let log = tmp_log("fork-branch");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(engine, log, 128_000);

        sup.handle("Prompt", json!({ "text": "hi" })).unwrap();
        assert_eq!(sup.state.thread_id.as_deref(), Some("thr_old"));

        sup.handle("Fork", json!({})).unwrap();
        assert_eq!(
            sup.state.thread_id.as_deref(),
            Some("thr_new"),
            "fork 之后当前线程必须换成新线程，否则后续读的还是旧线程"
        );

        // 而且真的按新线程去读——不是只有 state 好看。
        sup.handle("GetEntries", json!({})).unwrap();
        let reads: Vec<String> = sup
            .engine
            .requests()
            .iter()
            .filter(|(m, _)| m == "thread/read")
            .filter_map(|(_, p)| p.get("threadId").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert_eq!(
            reads.last().map(String::as_str),
            Some("thr_new"),
            "GetEntries 必须读新线程，实际读了：{reads:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// HTTP/SSE 的 item 把种类放在 `kind`，stdio 放在 `type`。
    ///
    /// 只看 `type` 的后果不是"少个字段"：HTTP 路径下**每一条 assistant 回答
    /// 都不进日志**。日志里没有，回放/分叉/压缩/历史全都没有，用户看到的就是
    /// "跑了一个命令，然后没有下文"。
    #[test]
    fn http_shaped_agent_message_reaches_the_log() {
        let mut engine = MockEngine::default();
        // 没有任何 delta——模拟不流式的 provider，只有成品文本。
        engine.emit(
            "item/completed",
            json!({ "item": { "kind": "agent_message", "id": "i9", "detail": "一共 19 项" } }),
        );
        let log = tmp_log("http-agent-message");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(engine, log, 128_000);

        let (_reply, events) = sup.handle("Prompt", json!({ "text": "hi" })).unwrap();

        let texts: Vec<String> = sup
            .log
            .entries()
            .iter()
            .filter(|e| e.kind == "message")
            .filter_map(|e| e.payload.get("text").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert_eq!(texts, vec!["一共 19 项".to_string()], "assistant 回答没进日志");

        // 不流式时界面也要能看到——按 delta 渲染的话这里会是空的。
        let streamed: String = events
            .iter()
            .filter(|e| e.kind == "text_delta")
            .filter_map(|e| e.data.get("delta").and_then(Value::as_str))
            .collect();
        assert_eq!(streamed, "一共 19 项", "非流式回答没有发给界面");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn native_prompt_opens_a_turn_in_the_log() {
        let log = tmp_log("native-turn");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(MockEngine::default(), log, 128_000);
        sup.handle("Prompt", json!({"text":"hi"})).unwrap();
        let kinds: Vec<_> = sup.log.entries().iter().map(|e| e.kind.clone()).collect();
        assert!(kinds.contains(&"turn.start".to_string()), "kinds: {kinds:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guard_veto_turns_into_a_denial_on_the_approval_channel() {
        let engine = MockEngine::default();
        let log = tmp_log("veto");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(engine, log, 128_000);

        let mut denied = false;
        for i in 0..30 {
            sup.engine.emit_request(
                100 + i,
                "item/commandExecution/requestApproval",
                json!({"tool": "list_dir", "attempt": i}),
            );
            sup.pump().unwrap();
            if sup.denials > 0 {
                denied = true;
                break;
            }
        }
        assert!(denied, "spiral guard must eventually veto");
        let last = sup.engine.responses().last().cloned().expect("engine got an answer");
        assert_eq!(last.1["decision"], "decline", "wire vocabulary is accept/decline");
        assert!(sup.log.project().receipts.is_empty(), "a denied call is not an effect");
        let kinds: Vec<_> = sup.log.entries().iter().map(|e| e.kind.clone()).collect();
        assert!(kinds.iter().any(|k| k == "guard.deny"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn steer_waits_for_item_boundary_then_reaches_the_engine() {
        let log = tmp_log("steer");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(MockEngine::default(), log, 128_000);
        sup.state.thread_id = Some("thr_1".into());
        sup.steer.push("use sqlite instead");

        sup.engine.emit("turn/completed", json!({}));
        let calls_before = sup.engine.calls().len();
        sup.pump().unwrap();
        // AfterTurn mode would hold; default is Immediate, and turn end is a safe point.
        assert!(sup.engine.calls().len() > calls_before);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn uncertain_tool_result_blocks_the_turn() {
        let log = tmp_log("uncertain");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(MockEngine::default(), log, 128_000);
        sup.engine.emit(
            "item/completed",
            json!({"item":{"type":"commandExecution","id":"c1","command":"rm -rf build","status":"inProgress"}}),
        );
        sup.pump().unwrap();
        let proj = sup.log.project();
        assert!(proj.blocked().is_some(), "unobserved effect must block");
        let _ = std::fs::remove_file(&path);
    }

    /// 压缩请求失败时，**回合仍然要能闭合**。
    ///
    /// 这个 bug 在真引擎上表现为：timebus 只有 `turn.start` + `usage` 两条，
    /// 没有 `turn.end`。原因是 `thread/compact/start` 在 HTTP 传输上没实现，
    /// 而它当时是用 `?` 调的——一个失败的建议把整条事件流打断了，
    /// 后面真正的 `turn.completed` 就再也读不到。
    /// 日志不闭合，投影就全错——"日志是事实"这条原则被悄悄违反。
    #[test]
    fn a_failed_compaction_does_not_swallow_turn_end() {
        let log = tmp_log("compact-fail");
        let path = log.path().to_path_buf();
        // 引擎：压缩请求一律失败（HTTP 上就是没这个方法），usage 报满窗口 + 热前缀。
        // 一个"压缩一定失败"的引擎。MockEngine 的 reply 只能返回 Value，
        // 所以这里用一个最小的自建引擎，专门让 thread/compact/start 报错。
        struct FailingCompact(MockEngine);
        impl crate::engine::Engine for FailingCompact {
            fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
                if method == "thread/compact/start" {
                    anyhow::bail!("this transport has no compaction endpoint");
                }
                self.0.request(method, params)
            }
            fn respond(&mut self, id: u64, result: Value) -> anyhow::Result<()> {
                self.0.respond(id, result)
            }
            fn notifications(&mut self) -> Vec<crate::protocol::Frame> {
                self.0.notifications()
            }
            fn name(&self) -> String {
                self.0.name()
            }
        }
        let mut sup = Supervisor::new(
            FailingCompact(MockEngine::new(|_, _| json!({}))),
            log,
            9000,
        );
        sup.state.thread_id = Some("thr_1".into());
        sup.state.auto_compact = true;
        sup.cache.observe(12_000, 11_800);

        // 直接喂一个 usage 通知，再喂一个 turn/completed——后者必须被投影看见。
        let events = sup
            .on_notification(
                "thread/tokenUsage/updated",
                json!({ "usage": { "input_tokens": 12000, "cached_tokens": 11800 } }),
            )
            .expect("usage 处理不该失败");
        assert!(
            events.iter().any(|e| e.kind == "compact_requested"),
            "满窗口 + 热前缀应当请求压缩"
        );
        // 失败必须**记成失败**。变异扫描抓到的：把 `compact_failed` 改成
        // `compact_ok`，测试全绿——因为没人看这个事件名。
        // 这是这一夜反复出现的同一类病：坏消息被翻译成好消息。
        // 压缩请求失败如果看起来像成功，用户会以为上下文被整理了，其实没有。
        let failed: Vec<_> = events.iter().filter(|e| e.kind.contains("compact")).collect();
        assert!(
            failed.iter().any(|e| e.kind == "compact_failed"),
            "压缩请求失败必须发 compact_failed，实际发的是 {:?}",
            failed.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
        assert!(
            !failed.iter().any(|e| e.kind == "compact_ok"),
            "失败不能记成成功"
        );

        let done = sup
            .on_notification("turn/completed", json!({}))
            .expect("压缩失败不能打断事件流");
        assert!(
            done.iter().any(|e| e.kind == "turn_end"),
            "turn.completed 必须变成 turn_end，否则 timebus 里的回合永远不闭合"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn full_window_with_hot_prefix_triggers_compaction() {
        let log = tmp_log("compact");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(MockEngine::default(), log, 100_000);
        sup.state.thread_id = Some("thr_1".into());
        sup.engine.emit("thread/tokenUsage/updated", json!({"promptTokens":95000,"cachedTokens":94000}));
        sup.pump().unwrap();
        assert!(sup.engine.calls().iter().any(|c| c == "thread/compact/start"));
        let _ = std::fs::remove_file(&path);
    }

    /// S4 里"6 个事件"包含 `compact.pre`，但这一条一直是**读代码**得出的结论：
    /// 探针跑普通回合只看得见 4 种，压缩又很难在真机上凑出来。
    /// 所以这里退一步，用真的插件进程验一次"广播确实到达插件"：
    /// 插件把收到的方法名写文件，我们读回来。
    #[test]
    fn the_cache_guard_tells_plugins_before_it_compacts() {
        let log = tmp_log("compact-plugin");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(MockEngine::default(), log, 100_000);
        sup.state.thread_id = Some("thr_1".into());

        let seen = std::env::temp_dir().join(format!(
            "compact-plugin-seen-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&seen);
        let script = format!(
            r#"
import json,sys
out=open({seen:?},'a',buffering=1)
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    m=json.loads(line)
    if m.get('method')=='plugin/hello':
        print(json.dumps({{'id':m['id'],'result':{{'name':'tap','protocol':1,'tools':[],'caps':[]}}}}),flush=True)
    elif 'method' in m:
        out.write(m['method']+'\n')
"#,
            seen = seen.to_string_lossy()
        );
        sup.plugins
            .load(&crate::plugins::PluginSpec::new("tap", "python3", vec!["-u".into(), "-c".into(), script]))
            .expect("load tap plugin");

        // 热前缀几乎全命中：缓存守卫会说"该压缩了"。
        sup.engine
            .emit("thread/tokenUsage/updated", json!({"promptTokens":95000,"cachedTokens":94000}));
        sup.pump().unwrap();
        assert!(sup.engine.calls().iter().any(|c| c == "thread/compact/start"));

        // 插件是另一个进程：给它一点时间把行刷出来。
        let mut got = String::new();
        for _ in 0..50 {
            got = std::fs::read_to_string(&seen).unwrap_or_default();
            if got.contains("compact.pre") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&seen);
        let _ = std::fs::remove_file(&path);
        assert!(
            got.contains("compact.pre"),
            "插件没有收到 compact.pre，实际收到：{got:?}"
        );
    }

    /// HTTP/SSE 报的是蛇形 `input_tokens` / `prompt_cache_hit_tokens`。
    /// 缓存守卫如果只认驼峰，就会永远拿 0/0 判断——永远"没问题"，也就永远不压缩。
    /// 这条不是理论：真机上把 window 调到 9000、跑 1.2 万 token 的回合，
    /// 守卫一声没吭，查下来就是它读不到数。
    #[test]
    fn the_cache_guard_reads_the_http_usage_dialect() {
        let log = tmp_log("compact-http");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(MockEngine::default(), log, 9_000);
        sup.state.thread_id = Some("thr_1".into());
        sup.state.auto_compact = true;
        sup.engine.emit(
            "turn/usage",
            json!({"usage":{"input_tokens":12467,"prompt_cache_hit_tokens":11600}}),
        );
        sup.pump().unwrap();
        assert!(
            sup.engine.calls().iter().any(|c| c == "thread/compact/start"),
            "HTTP 形状的 usage 也必须能推动缓存守卫请求压缩"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 和上面那条对称：窗口满了但**缓存命中率崩了**时，守卫必须说"冻结前缀"，
    /// 而不是压缩（再压只会更贵）。这条路之前同样因为读不到用量而永远不会走。
    #[test]
    fn the_cache_guard_freezes_the_prefix_when_the_hit_rate_collapses() {
        let log = tmp_log("freeze-http");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(MockEngine::default(), log, 9_000);
        sup.state.thread_id = Some("thr_1".into());
        sup.state.auto_compact = true;
        sup.engine.emit(
            "turn/usage",
            json!({"usage":{"input_tokens":12467,"prompt_cache_hit_tokens":1200}}),
        );
        let events = sup.pump().unwrap();
        let froze = events.iter().any(|e| e.kind == "cache_freeze");
        let compressed = sup.engine.calls().iter().any(|c| c == "thread/compact/start");
        let _ = std::fs::remove_file(&path);
        assert!(froze, "命中率只有 9.6% 时必须冻结前缀，实际事件：{events:?}");
        assert!(!compressed, "命中率崩了还去压缩，等于把最便宜的 token 扔了");
    }

    /// 连续发多条 prompt 时，引擎会拒绝"上一轮还没结束"的新回合，回应里既没有
    /// `output` 也没有任何回合事件。之前的实现把它原样当成功回给 orbit——
    /// **消息静默消失**，而调用方看到的是 `success: true`。
    /// 这条测试钉住：那种回应必须变成错误。
    #[test]
    /// 包装不能吃掉回合 id。
    ///
    /// 变异扫描抓到的：把 `turn_id` 换成 `Value::Null`，单元测试全绿。
    /// 但真引擎上第 2 回合会被判成"被拒"——因为上层唯一可靠的
    /// "引擎接没接这一轮"的证据就是这个 id。
    /// （真引擎实测：201 返回 `{"thread":…,"turn":{"id":"turn_…"}}`，
    ///   handle 手工包装时把 turn 整个丢了。）
    /// 「怎么不是流式显示呢」——判据是增量**什么时候**出去，不是有没有。
    /// 以前只攒不发，item 结束才补一条成品：事件条数看着对，界面是整段蹦出来。
    #[test]
    fn deltas_go_out_as_they_arrive_not_all_at_item_end() {
        let mut engine = MockEngine::new(Box::new(|m: &str, _p: &Value| {
            if m == "thread/start" {
                json!({ "thread": { "id": "thr_mock" } })
            } else {
                json!({ "output": "ok" })
            }
        }));
        engine.emit("item/agentMessage/delta", json!({ "delta": "一" }));
        engine.emit("item/agentMessage/delta", json!({ "delta": "二" }));
        engine.emit("item/agentReasoning/delta", json!({ "delta": "想" }));
        let mut sup = Supervisor::new(engine, tmp_log("streaming"), 128_000);

        let (_r, events) = sup.handle("Prompt", json!({ "text": "hi" })).unwrap();
        let texts: Vec<&str> = events
            .iter()
            .filter(|e| e.kind == "text_delta")
            .filter_map(|e| e.data.get("delta").and_then(Value::as_str))
            .collect();
        assert_eq!(texts, vec!["一", "二"], "增量没有边到边发");
        let think: Vec<&str> = events
            .iter()
            .filter(|e| e.kind == "reasoning_delta")
            .filter_map(|e| e.data.get("delta").and_then(Value::as_str))
            .collect();
        assert_eq!(think, vec!["想"], "思考增量没走自己那条线");
    }

    #[test]
    fn prompt_reply_keeps_the_turn_id_through_wrapping() {
        let log = tmp_log("turn-id-wrap");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(
            MockEngine::new(|method, _| {
                if method == "prompt/run" {
                    json!({ "turn_id": "turn_abc", "output": "ok" })
                } else {
                    json!({ "thread": { "id": "thr_1" } })
                }
            }),
            log,
            128_000,
        );
        sup.state.thread_id = Some("thr_1".into());
        sup.state.pending_prompt = Some("先存着的那条".into());

        let outcome = sup.handle("Prompt", json!({ "text": "不会真发出去" }));
        let _ = std::fs::remove_file(&path);
        let reply = outcome.expect("带着 turn_id 的回合不该被当成被拒");
        assert_eq!(
            reply.0.get("turn_id").and_then(Value::as_str),
            Some("turn_abc"),
            "包装后回合 id 必须还在——丢了它，真引擎多回合会被误判成被拒"
        );
    }

    #[test]
    fn a_rejected_prompt_is_an_error_not_a_silent_drop() {
        let log = tmp_log("prompt-drop");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(
            MockEngine::new(|method, _| {
                if method == "prompt/run" {
                    // 引擎在"上一轮还在跑"时的真实回应：什么都没有。
                    json!({ "output": null })
                } else {
                    json!({ "thread": { "id": "thr_1" } })
                }
            }),
            log,
            128_000,
        );
        sup.state.thread_id = Some("thr_1".into());

        let outcome = sup.handle("Prompt", json!({"text": "第二条"}));
        let _ = std::fs::remove_file(&path);
        let err = match outcome {
            Err(e) => e.to_string(),
            Ok((reply, _)) => panic!(
                "引擎没跑这一轮却回了 success，消息就此消失（reply={reply}）"
            ),
        };
        assert!(
            err.contains("no turn") || err.contains("rejected") || err.contains("没有回合"),
            "错误信息要说清是引擎没接受这一轮，实际：{err}"
        );
    }

    #[test]
    fn real_engine_payload_maps_command_actions_to_our_tool_names() {
        assert_eq!(
            tool_name(&json!({"commandActions":[{"type":"listFiles"}]}), "item/commandExecution/requestApproval"),
            "list_dir"
        );
        assert_eq!(
            tool_name(&json!({"commandActions":[{"type":"search","query":"x"}]}), "item/commandExecution/requestApproval"),
            "grep"
        );
        assert_eq!(tool_name(&json!({"command":"cargo test","itemId":"i1"}), ""), "shell");
        assert_eq!(tool_name(&json!({"itemId":"i1"}), "item/fileChange/requestApproval"), "apply_patch");
    }

    #[test]
    fn approval_accepts_with_the_engines_own_wording() {
        let log = tmp_log("accept");
        let path = log.path().to_path_buf();
        let mut sup = Supervisor::new(MockEngine::default(), log, 128_000);
        sup.engine.emit_request(
            7,
            "item/commandExecution/requestApproval",
            json!({"command":"cargo test","itemId":"i1"}),
        );
        sup.pump().unwrap();
        let (id, resp) = sup.engine.responses().last().cloned().unwrap();
        assert_eq!(id, 7);
        assert_eq!(resp, json!({"decision": "accept"}));
        let _ = std::fs::remove_file(&path);
    }
}
