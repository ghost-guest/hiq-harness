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
            approvals: 0,
            denials: 0,
        }
    }

    /// One orbit command in, one reply plus any events the engine produced.
    pub fn handle(&mut self, cmd: &str, params: Value) -> Result<(Value, Vec<UiEvent>)> {
        match map(cmd, &params, &mut self.state) {
            Action::Local { reply } => Ok((reply, Vec::new())),
            Action::Unsupported { reason } => bail!("{reason}"),
            Action::Call { method, params } => {
                if method == "turn/start" {
                    self.guards.begin_turn();
                    self.text_buffer.clear();
                    self.log.append("turn.start", json!({ "thread": self.state.thread_id }))?;
                    self.plugins.broadcast("turn.start", json!({ "thread": self.state.thread_id }));
                }
                let reply = self.engine.request(&method, params)?;

                // thread/start is the only place a new thread id appears.
                if method == "thread/start" {
                    if let Some(id) = reply
                        .pointer("/thread/id")
                        .or_else(|| reply.get("threadId"))
                        .and_then(Value::as_str)
                    {
                        self.state.thread_id = Some(id.to_string());
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
        let tool = frame
            .params
            .as_ref()
            .and_then(|p| p.get("tool").or_else(|| p.get("toolName")))
            .and_then(Value::as_str)
            .unwrap_or(&method)
            .to_string();
        let args = frame.params.clone().unwrap_or(Value::Null).to_string();

        let verdict = self.guards.before(&tool, &args);
        let decision = match verdict {
            Verdict::Deny(reason) => {
                self.denials += 1;
                self.log.append(
                    "guard.deny",
                    json!({ "tool": tool, "reason": reason, "digest": digest(&args) }),
                )?;
                self.plugins.broadcast("guard.deny", json!({ "tool": tool, "reason": reason }));
                json!({ "decision": "denied", "reason": reason })
            }
            Verdict::AllowWithNote(note) => {
                self.approvals += 1;
                self.plugins.broadcast("tool.pre", json!({ "tool": tool, "note": note }));
                json!({ "decision": "approved", "note": note })
            }
            Verdict::Allow | Verdict::Stop(_) => {
                self.approvals += 1;
                self.plugins.broadcast("tool.pre", json!({ "tool": tool }));
                json!({ "decision": "approved" })
            }
        };
        Ok(frame.id.map(|id| (id, decision)))
    }

    fn on_notification(&mut self, method: &str, params: Value) -> Result<Vec<UiEvent>> {
        let mut out = vec![UiEvent::new(ui_kind_for(method), params.clone())];

        match method {
            "item/agentMessage/delta" => {
                if let Some(d) = params.get("delta").and_then(Value::as_str) {
                    self.text_buffer.push_str(d);
                }
            }
            "item/completed" => {
                let item = params.get("item").cloned().unwrap_or(Value::Null);
                let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
                if kind.contains("agentMessage") && !self.text_buffer.is_empty() {
                    let text = std::mem::take(&mut self.text_buffer);
                    self.log.append("message", json!({ "role": "assistant", "text": text }))?;
                }
                if kind.contains("commandExecution") || kind.contains("mcpToolCall") {
                    let call_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let tool = item
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or(kind)
                        .to_string();
                    // Rule 5 of the time-bus: an effect we did not observe is
                    // `uncertain` and blocks, it is never assumed to have happened.
                    let status = match item.get("status").and_then(Value::as_str) {
                        Some("completed") | Some("success") => "committed",
                        Some("failed") => "committed",
                        _ => "uncertain",
                    };
                    self.log
                        .append("tool.call", json!({ "call_id": call_id, "tool": tool, "status": status }))?;
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
            "thread/tokenUsage/updated" => {
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
        assert_eq!(last.1["decision"], "denied", "the veto must reach the engine as a denial");
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
}
