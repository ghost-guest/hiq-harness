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
        if !methods.is_empty() {
            // The thread surface is not always advertised, but we need it.
            for m in ["thread/start", "thread/read", "app/models"] {
                methods.insert(m.to_string());
            }
            self.state.caps.methods = methods;
        }
        self.pump()?;
        Ok(())
    }

    /// One orbit command in, one reply plus any events the engine produced.
    pub fn handle(&mut self, cmd: &str, params: Value) -> Result<(Value, Vec<UiEvent>)> {
        match map(cmd, &params, &mut self.state) {
            Action::Local { reply } => Ok((reply, Vec::new())),
            Action::Unsupported { reason } => bail!("{reason}"),
            Action::Call { method, params } => {
                // A turn starts on either dialect: codex-style `turn/start` or the
                // native `prompt/run`. Missing this line means the log has a turn
                // that never opened — the projection would then be wrong.
                if method == "turn/start" || method.starts_with("prompt/") {
                    self.guards.begin_turn();
                    self.text_buffer.clear();
                    self.tool_seen.clear();
                    self.log.append("turn.start", json!({ "thread": self.state.thread_id }))?;
                    self.plugins.broadcast("turn.start", json!({ "thread": self.state.thread_id }));
                }
                let mut reply = self.engine.request(&method, params)?;

                // thread/start is the only place a new thread id appears.
                if method == "thread/start" {
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
                        });
                    }
                }
                let events = self.pump()?;
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
            "item/agentMessage/delta" => {
                if let Some(d) = params.get("delta").and_then(Value::as_str) {
                    self.text_buffer.push_str(d);
                }
            }
            "item/completed" => {
                let item = params.get("item").cloned().unwrap_or(Value::Null);
                let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
                // stdio 叫 agentMessage，HTTP/SSE 叫 agent_message——两种都认。
                if (kind.contains("agentMessage") || kind == "agent_message")
                    && !self.text_buffer.is_empty()
                {
                    let text = std::mem::take(&mut self.text_buffer);
                    self.log.append("message", json!({ "role": "assistant", "text": text }))?;
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
                let prompt = params.get("promptTokens").and_then(Value::as_u64).unwrap_or(0);
                let cached = params.get("cachedTokens").and_then(Value::as_u64).unwrap_or(0);
                self.cache.observe(prompt, cached);
                match self.cache.advise() {
                    Advice::Ok => {}
                    Advice::FreezePrefix { reason } => {
                        out.push(UiEvent::new("cache_freeze", json!({ "reason": reason })));
                    }
                    Advice::CompactNow { reason } => {
                        if self.state.auto_compact {
                            if let Some(thread) = self.state.thread_id.clone() {
                                self.plugins.broadcast("compact.pre", json!({ "reason": reason }));
                                self.engine.request(
                                    "thread/compact/start",
                                    json!({ "threadId": thread, "reason": reason }),
                                )?;
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
