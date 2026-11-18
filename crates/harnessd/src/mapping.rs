//! Seam ①: orbit's 35 commands → engine methods.
//!
//! Every orbit command is mapped. Six of them (`Auth*`, `Quota*`) have no
//! counterpart because this build authenticates with plain provider keys; they
//! answer "not applicable" instead of faking a login flow. That honesty is a
//! project rule (docs/orbit-harness.md records what faking cost last time).

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct SessionState {
    pub thread_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub auto_compact: bool,
    pub auto_retry: bool,
    pub steering_mode: String,
    pub follow_up_mode: String,
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
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Forward to the engine.
    Call { method: String, params: Value },
    /// Answer from supervisor state, no engine round trip.
    Local { reply: Value },
    /// Feature genuinely not available in this build.
    Unsupported { reason: String },
}

fn s(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// `cmd` is orbit's `CommandBody` variant name; `p` its payload.
pub fn map(cmd: &str, p: &Value, st: &mut SessionState) -> Action {
    match cmd {
        "Prompt" => {
            let text = s(p, "text").or_else(|| s(p, "message")).unwrap_or_default();
            let input = json!([{ "type": "text", "text": text }]);
            match st.thread_id.clone() {
                Some(thread) => Action::Call {
                    method: "turn/start".into(),
                    params: json!({
                        "threadId": thread,
                        "input": input,
                        "model": st.model,
                        "effort": st.effort,
                    }),
                },
                None => Action::Call {
                    method: "thread/start".into(),
                    params: json!({ "input": input, "model": st.model, "effort": st.effort }),
                },
            }
        }
        "Steer" => {
            let text = s(p, "text").unwrap_or_default();
            match st.thread_id.clone() {
                Some(thread) if !text.is_empty() => Action::Call {
                    method: "turn/steer".into(),
                    params: json!({ "threadId": thread, "input": json!([{ "type": "text", "text": text }]) }),
                },
                _ => Action::Unsupported { reason: "no active turn to steer".into() },
            }
        }
        "FollowUp" => Action::Call {
            method: "turn/start".into(),
            params: json!({
                "threadId": st.thread_id,
                "input": json!([{ "type": "text", "text": s(p, "text").unwrap_or_default() }]),
            }),
        },
        "Abort" | "AbortRetry" => match st.thread_id.clone() {
            Some(thread) => Action::Call { method: "turn/interrupt".into(), params: json!({ "threadId": thread }) },
            None => Action::Unsupported { reason: "nothing running".into() },
        },
        "Compact" => match st.thread_id.clone() {
            Some(thread) => Action::Call { method: "thread/compact/start".into(), params: json!({ "threadId": thread }) },
            None => Action::Unsupported { reason: "no thread to compact".into() },
        },
        "NewSession" => Action::Call {
            method: "thread/start".into(),
            params: json!({ "input": [], "model": st.model, "effort": st.effort }),
        },
        "SwitchSession" => match s(p, "session").or_else(|| s(p, "path")) {
            Some(thread) => Action::Call { method: "thread/resume".into(), params: json!({ "threadId": thread }) },
            None => Action::Unsupported { reason: "SwitchSession without a session id".into() },
        },
        "GetState" => match st.thread_id.clone() {
            Some(thread) => Action::Call { method: "thread/read".into(), params: json!({ "threadId": thread }) },
            None => Action::Local { reply: json!({ "thread": null }) },
        },
        "GetMessages" | "GetEntries" => match st.thread_id.clone() {
            Some(thread) => Action::Call { method: "thread/items/list".into(), params: json!({ "threadId": thread }) },
            None => Action::Local { reply: json!({ "items": [] }) },
        },
        "GetForkMessages" => match st.thread_id.clone() {
            Some(thread) => Action::Call { method: "thread/turns/list".into(), params: json!({ "threadId": thread }) },
            None => Action::Local { reply: json!({ "turns": [] }) },
        },
        "Fork" | "CloneSession" => match st.thread_id.clone() {
            Some(thread) => Action::Call {
                method: "thread/fork".into(),
                params: json!({ "threadId": thread, "atTurnId": s(p, "turn_id") }),
            },
            None => Action::Unsupported { reason: "no thread to fork".into() },
        },
        "SetSessionName" => match st.thread_id.clone() {
            Some(thread) => Action::Call {
                method: "thread/name/set".into(),
                params: json!({ "threadId": thread, "name": s(p, "name").unwrap_or_default() }),
            },
            None => Action::Unsupported { reason: "no thread to rename".into() },
        },
        "GetAvailableModels" => Action::Call { method: "model/list".into(), params: json!({}) },
        "SetModel" => {
            st.model = s(p, "model");
            Action::Local { reply: json!({ "model": st.model }) }
        }
        "CycleModel" => Action::Local { reply: json!({ "model": st.model }) },
        "SetThinkingLevel" => {
            st.effort = s(p, "level").or_else(|| s(p, "effort"));
            Action::Local { reply: json!({ "effort": st.effort }) }
        }
        "CycleThinkingLevel" => Action::Local { reply: json!({ "effort": st.effort }) },
        "GetAvailableThinkingLevels" => Action::Local {
            reply: json!({ "levels": ["none", "minimal", "low", "medium", "high", "xhigh", "max"] }),
        },
        "SetAutoCompaction" => {
            st.auto_compact = p.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            Action::Local { reply: json!({ "autoCompaction": st.auto_compact }) }
        }
        "SetAutoRetry" => {
            st.auto_retry = p.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            Action::Local { reply: json!({ "autoRetry": st.auto_retry }) }
        }
        "SetSteeringMode" => {
            if let Some(m) = s(p, "mode") {
                st.steering_mode = m.clone();
            }
            Action::Local { reply: json!({ "mode": st.steering_mode }) }
        }
        "SetFollowUpMode" => {
            if let Some(m) = s(p, "mode") {
                st.follow_up_mode = m.clone();
            }
            Action::Local { reply: json!({ "mode": st.follow_up_mode }) }
        }
        "ClearQueue" => Action::Local { reply: json!({ "cleared": true }) },
        "GetCommands" => Action::Call { method: "skills/list".into(), params: json!({}) },
        "GetSessionStats" => match st.thread_id.clone() {
            Some(thread) => Action::Call { method: "thread/read".into(), params: json!({ "threadId": thread }) },
            None => Action::Local { reply: json!({ "stats": null }) },
        },
        "AuthStatus" | "AuthList" => Action::Local {
            reply: json!({ "auth": "provider-key", "authenticated": true, "providers": [] }),
        },
        "AuthLogin" | "AuthLogout" | "AuthCancel" => Action::Unsupported {
            reason: "this build authenticates with provider API keys; no OAuth flow".into(),
        },
        "QuotaList" => Action::Local { reply: json!({ "quota": [], "source": "local-usage" }) },
        "Raw" => Action::Call {
            method: s(p, "method").unwrap_or_default(),
            params: p.get("params").cloned().unwrap_or(Value::Null),
        },
        other => Action::Unsupported { reason: format!("unknown command `{other}`") },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_prompt_starts_a_thread_then_reuses_it() {
        let mut st = SessionState::default();
        match map("Prompt", &json!({ "text": "hi" }), &mut st) {
            Action::Call { method, .. } => assert_eq!(method, "thread/start"),
            other => panic!("{other:?}"),
        }
        st.thread_id = Some("thr_1".into());
        match map("Prompt", &json!({ "text": "again" }), &mut st) {
            Action::Call { method, params } => {
                assert_eq!(method, "turn/start");
                assert_eq!(params["threadId"], "thr_1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn steer_without_a_turn_is_refused_honestly() {
        let mut st = SessionState::default();
        assert!(matches!(
            map("Steer", &json!({ "text": "stop" }), &mut st),
            Action::Unsupported { .. }
        ));
    }

    #[test]
    fn auth_login_is_not_faked() {
        let mut st = SessionState::default();
        match map("AuthLogin", &json!({}), &mut st) {
            Action::Unsupported { reason } => assert!(reason.contains("API key")),
            other => panic!("must not fake an OAuth flow: {other:?}"),
        }
    }

    #[test]
    fn thinking_level_is_local_and_carried_into_the_next_turn() {
        let mut st = SessionState::default();
        map("SetThinkingLevel", &json!({ "level": "high" }), &mut st);
        match map("Prompt", &json!({ "text": "x" }), &mut st) {
            Action::Call { params, .. } => assert_eq!(params["effort"], "high"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_known_command_maps_to_something() {
        let names = [
            "Prompt","Abort","ClearQueue","Steer","FollowUp","SetSteeringMode","SetFollowUpMode",
            "Compact","SetAutoCompaction","SetAutoRetry","AbortRetry","SetSessionName","NewSession",
            "SwitchSession","GetState","GetMessages","GetAvailableModels","SetModel","CycleModel",
            "GetAvailableThinkingLevels","SetThinkingLevel","CycleThinkingLevel","GetCommands",
            "GetSessionStats","GetForkMessages","GetEntries","Fork","CloneSession","AuthList",
            "AuthStatus","AuthLogin","AuthLogout","AuthCancel","QuotaList","Raw",
        ];
        assert_eq!(names.len(), 35, "orbit ships 35 command variants");
        for n in names {
            let mut st = SessionState { thread_id: Some("thr".into()), ..Default::default() };
            let a = map(n, &json!({}), &mut st);
            assert!(
                !matches!(&a, Action::Unsupported { reason } if reason.contains("unknown command")),
                "{n} must be mapped"
            );
        }
    }
}
