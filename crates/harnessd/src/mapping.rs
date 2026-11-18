//! Seam ①: orbit's 35 commands → whatever the engine actually answers.
//!
//! **Discovered, not assumed.** The shipped app-server speaks a smaller, native
//! dialect than the `app-server-protocol` crate advertises: `prompt/run` instead
//! of `turn/start`, `app/models` instead of `model/list`, and the stdio transport
//! is deliberately *tool-free* (`prompt/capabilities` says so, and points at
//! `app-server --http` for tool-enabled work). Hardcoding the crate's method list
//! would have produced a supervisor that 404s every call.
//!
//! So: ask the engine what it can do (`app/|thread/|prompt/capabilities`), store
//! that in `Capabilities`, and pick the first candidate method it admits to.

use serde_json::{json, Value};
use std::collections::BTreeSet;

/// What the engine said it supports. `assume_native()` is the set the 0.10.3
/// binary actually answers; an empty set means "unknown, try everything".
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub methods: BTreeSet<String>,
    pub tool_free: bool,
    pub full_api: Option<String>,
}

impl Capabilities {
    pub fn assume_native() -> Self {
        let methods = [
            "app/capabilities",
            "app/config/get",
            "app/config/list",
            "app/config/set",
            "app/config/unset",
            "app/config/reload",
            "app/models",
            "app/request",
            "prompt/capabilities",
            "prompt/request",
            "prompt/run",
            "thread/archive",
            "thread/capabilities",
            "thread/create",
            "thread/fork",
            "thread/goal/clear",
            "thread/goal/get",
            "thread/goal/set",
            "thread/interrupt",
            "thread/list",
            "thread/loaded/list",
            "thread/message",
            "thread/name/set",
            "thread/read",
            "thread/request",
            "thread/resume",
            "thread/start",
            "thread/unarchive",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        Self { methods, tool_free: true, full_api: None }
    }

    pub fn has(&self, method: &str) -> bool {
        self.methods.is_empty() || self.methods.contains(method)
    }

    /// First candidate the engine admits to.
    pub fn pick(&self, candidates: &[&str]) -> Option<String> {
        candidates.iter().find(|m| self.has(m)).map(|m| m.to_string())
    }

    pub fn probe_list(&self, candidates: &[&str]) -> String {
        candidates.join(", ")
    }
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub thread_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub auto_compact: bool,
    pub auto_retry: bool,
    pub steering_mode: String,
    pub follow_up_mode: String,
    pub caps: Capabilities,
    /// A prompt waiting for its thread: `prompt/run` keys off a thread id.
    pub pending_prompt: Option<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            thread_id: None,
            model: None,
            effort: None,
            auto_compact: true,
            auto_retry: true,
            steering_mode: "immediate".into(),
            follow_up_mode: "after_turn".into(),
            caps: Capabilities::assume_native(),
            pending_prompt: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Call { method: String, params: Value },
    Local { reply: Value },
    Unsupported { reason: String },
}

fn s(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn na(feature: &str, why: &str) -> Action {
    Action::Unsupported { reason: format!("{feature} is not available: {why}") }
}

/// `cmd` is orbit's `CommandBody` variant name; `p` its payload.
pub fn map(cmd: &str, p: &Value, st: &mut SessionState) -> Action {
    let caps = st.caps.clone();
    match cmd {
        // One prompt = one `prompt/run`. The thread id is optional to the engine;
        // we pass ours when we have one and learn it from the reply when we don't.
        "Prompt" => {
            let text = s(p, "text").or_else(|| s(p, "message")).unwrap_or_default();
            if text.trim().is_empty() {
                return Action::Unsupported { reason: "empty prompt".into() };
            }
            match st.thread_id.clone() {
                None => {
                    // Thread first: prompt/run keys off a thread id, and
                    // thread/start is the only method that mints one.
                    st.pending_prompt = Some(text);
                    let mut params = json!({});
                    if let Some(m) = st.model.clone() {
                        params["model"] = json!(m);
                    }
                    Action::Call { method: "thread/start".into(), params }
                }
                Some(_) => match caps.pick(&["prompt/run", "prompt/request"]) {
                    Some(method) => {
                        // Stdio turns are tool-free by contract: the engine refuses
                        // a prompt without an explicit (empty) allow-list.
                        let mut params = json!({ "prompt": text, "allowed_tools": [] });
                        params["thread_id"] = json!(st.thread_id.clone().unwrap());
                        if let Some(m) = st.model.clone() {
                            params["model"] = json!(m);
                        }
                        Action::Call { method, params }
                    }
                    None => na("prompt execution", &format!("engine exposes none of {}", caps.probe_list(&["prompt/run", "prompt/request"]))),
                },
            }
        }

        "Steer" => {
            let text = s(p, "text").unwrap_or_default();
            match (&st.thread_id, text.trim().is_empty()) {
                (Some(thread), false) if caps.has("thread/message") => Action::Call {
                    method: "thread/message".into(),
                    params: json!({ "threadId": thread, "input": [{ "type": "text", "text": text }] }),
                },
                (_, true) => Action::Unsupported { reason: "empty steer message".into() },
                _ => na("steering", "this build exposes no thread/message"),
            }
        }

        "Abort" => match st.thread_id.clone() {
            Some(thread) if caps.has("thread/interrupt") => Action::Call {
                method: "thread/interrupt".into(),
                params: json!({ "threadId": thread }),
            },
            Some(_) => na("interrupt", "engine exposes no thread/interrupt"),
            None => Action::Local { reply: json!({ "aborted": false, "reason": "no active turn" }) },
        },

        "NewSession" => {
            st.thread_id = None;
            Action::Local { reply: json!({ "session": null }) }
        }

        "SwitchSession" => match s(p, "sessionId").or_else(|| s(p, "threadId")) {
            Some(id) if caps.has("thread/resume") => {
                st.thread_id = Some(id.clone());
                Action::Call { method: "thread/resume".into(), params: json!({ "threadId": id }) }
            }
            Some(_) => na("session switching", "engine exposes no thread/resume"),
            None => Action::Unsupported { reason: "SwitchSession needs a sessionId".into() },
        },

        "Fork" => match st.thread_id.clone() {
            Some(thread) if caps.has("thread/fork") => Action::Call {
                method: "thread/fork".into(),
                params: json!({ "threadId": thread }),
            },
            Some(_) => na("forking", "engine exposes no thread/fork"),
            None => Action::Unsupported { reason: "Fork needs an open thread".into() },
        },

        "CloneSession" => Action::Local {
            reply: json!({ "session": st.thread_id, "clone": null }),
        },

        "SetSessionName" => match st.thread_id.clone() {
            Some(thread) if caps.has("thread/name/set") => Action::Call {
                method: "thread/name/set".into(),
                params: json!({ "threadId": thread, "name": s(p, "name").unwrap_or_default() }),
            },
            Some(_) => na("renaming", "engine exposes no thread/name/set"),
            None => Action::Unsupported { reason: "no open thread".into() },
        },

        "GetState" => match st.thread_id.clone() {
            Some(thread) if caps.has("thread/read") => Action::Call {
                method: "thread/read".into(),
                params: json!({ "threadId": thread }),
            },
            Some(_) => Action::Local { reply: local_state(st) },
            None => Action::Local { reply: local_state(st) },
        },

        "GetMessages" | "GetEntries" | "GetForkMessages" => match st.thread_id.clone() {
            Some(thread) if caps.has("thread/read") => Action::Call {
                method: "thread/read".into(),
                params: json!({ "threadId": thread }),
            },
            _ => Action::Local { reply: json!({ "messages": [], "entries": [] }) },
        },

        "GetAvailableModels" | "SetModel" | "CycleModel" => match caps.pick(&["app/models", "model/list"]) {
            Some(method) => Action::Call { method, params: json!({}) },
            None => Action::Local {
                reply: json!({ "models": [], "note": "engine exposes no model listing" }),
            },
        },

        "GetAvailableThinkingLevels" | "SetThinkingLevel" | "CycleThinkingLevel" => Action::Local {
            reply: json!({ "levels": [], "effort": st.effort, "note": "effort is passed per prompt" }),
        },

        "GetCommands" => match caps.pick(&["prompt/capabilities", "app/capabilities"]) {
            Some(method) => Action::Call { method, params: json!({}) },
            None => Action::Local { reply: json!({ "commands": [] }) },
        },

        "GetSessionStats" => Action::Local {
            reply: json!({ "thread": st.thread_id, "model": st.model, "effort": st.effort }),
        },

        "SetSteeringMode" => {
            st.steering_mode = s(p, "mode").unwrap_or_else(|| "immediate".into());
            Action::Local { reply: json!({ "mode": st.steering_mode }) }
        }
        "SetFollowUpMode" => {
            st.follow_up_mode = s(p, "mode").unwrap_or_else(|| "after_turn".into());
            Action::Local { reply: json!({ "mode": st.follow_up_mode }) }
        }
        "SetAutoCompaction" => {
            st.auto_compact = p.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            Action::Local { reply: json!({ "enabled": st.auto_compact }) }
        }
        "SetAutoRetry" => {
            st.auto_retry = p.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            Action::Local { reply: json!({ "enabled": st.auto_retry }) }
        }
        "AbortRetry" | "ClearQueue" | "FollowUp" => Action::Local {
            reply: json!({ "ok": true, "note": "handled by the supervisor queue" }),
        },

        // No counterpart in this build: it authenticates with plain provider keys.
        "AuthList" | "AuthStatus" | "AuthLogin" | "AuthLogout" | "AuthCancel" => {
            na("provider OAuth", "this build authenticates with provider keys, not OAuth")
        }
        "QuotaList" => na("quota reporting", "provider quota is not exposed by the app-server"),
        "Compact" => na("manual compaction", "no compact method; compaction is engine-internal"),

        "Raw" => Action::Unsupported { reason: "Raw passthrough is refused by design".into() },
        other => Action::Unsupported { reason: format!("unknown command `{other}`") },
    }
}

fn local_state(st: &SessionState) -> Value {
    json!({
        "thread": st.thread_id,
        "model": st.model,
        "effort": st.effort,
        "autoCompaction": st.auto_compact,
        "autoRetry": st.auto_retry,
        "steeringMode": st.steering_mode,
        "followUpMode": st.follow_up_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORBIT_COMMANDS: [&str; 35] = [
        "Prompt", "Abort", "ClearQueue", "Steer", "FollowUp", "SetSteeringMode", "SetFollowUpMode",
        "Compact", "SetAutoCompaction", "SetAutoRetry", "AbortRetry", "SetSessionName", "NewSession",
        "SwitchSession", "GetState", "GetMessages", "GetAvailableModels", "SetModel", "CycleModel",
        "GetAvailableThinkingLevels", "SetThinkingLevel", "CycleThinkingLevel", "GetCommands",
        "GetSessionStats", "GetForkMessages", "GetEntries", "Fork", "CloneSession", "AuthList",
        "AuthStatus", "AuthLogin", "AuthLogout", "AuthCancel", "QuotaList", "Raw",
    ];

    #[test]
    fn every_orbit_command_maps_to_something() {
        let mut st = SessionState::default();
        for cmd in ORBIT_COMMANDS.iter() {
            let action = map(cmd, &json!({"text":"x","message":"x","name":"n","sessionId":"t","mode":"immediate","enabled":true}), &mut st);
            assert!(
                !matches!(action, Action::Unsupported { ref reason } if reason.contains("unknown command")),
                "{cmd} fell through to unknown"
            );
        }
    }

    #[test]
    fn prompt_uses_prompt_run_with_a_bare_string_prompt() {
        let mut st = SessionState { thread_id: Some("thr_1".into()), ..Default::default() };
        match map("Prompt", &json!({"text":"你好"}), &mut st) {
            Action::Call { method, params } => {
                assert_eq!(method, "prompt/run");
                assert_eq!(params["prompt"], "你好", "engine takes `prompt`, not `input`");
                assert!(params.get("input").is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn prompt_carries_the_thread_once_it_exists() {
        let mut st = SessionState { thread_id: Some("thr_1".into()), ..Default::default() };
        match map("Prompt", &json!({"text":"again"}), &mut st) {
            Action::Call { params, .. } => assert_eq!(params["thread_id"], "thr_1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn models_come_from_app_models_not_model_list() {
        let mut st = SessionState::default();
        match map("GetAvailableModels", &json!({}), &mut st) {
            Action::Call { method, .. } => assert_eq!(method, "app/models"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn capabilities_without_prompt_report_what_was_probed() {
        let mut st = SessionState { thread_id: Some("thr_1".into()), ..Default::default() };
        st.caps.methods = ["thread/list".to_string()].into_iter().collect();
        match map("Prompt", &json!({"text":"x"}), &mut st) {
            Action::Unsupported { reason } => {
                assert!(reason.contains("prompt/run"), "{reason}");
                assert!(reason.contains("prompt/request"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn auth_is_honest_not_faked() {
        let mut st = SessionState::default();
        for cmd in ["AuthList", "AuthStatus", "AuthLogin", "AuthLogout", "AuthCancel"] {
            assert!(matches!(map(cmd, &json!({}), &mut st), Action::Unsupported { .. }), "{cmd}");
        }
    }

    #[test]
    fn abort_interrupts_the_thread() {
        let mut st = SessionState { thread_id: Some("thr_1".into()), ..Default::default() };
        match map("Abort", &json!({}), &mut st) {
            Action::Call { method, params } => {
                assert_eq!(method, "thread/interrupt");
                assert_eq!(params["threadId"], "thr_1");
            }
            other => panic!("{other:?}"),
        }
    }
}
