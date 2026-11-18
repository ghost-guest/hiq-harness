//! Byte-level compatibility with orbit's `pi --mode rpc` JSONL protocol.
//!
//! orbit must not be rewritten (see KERNEL-FUSION-PLAN §3, seam ①). It sends
//! `{"id":"…","type":"prompt","message":"…"}` and expects
//! `{"type":"response",…}` plus events like `message_update` / `agent_end`
//! (`apps/orbit/crates/orbit-rpc/src/types.rs:206`). This module is the only
//! place that knows both vocabularies.

use serde_json::{json, Value};

use crate::protocol::UiEvent;

/// orbit command name → our command name. `None` means "we do not have it".
pub fn command_of(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "prompt" => "Prompt",
        "steer" => "Steer",
        "follow_up" => "FollowUp",
        "abort" => "Abort",
        "clear_queue" => "ClearQueue",
        "compact" => "Compact",
        "set_auto_compaction" => "SetAutoCompaction",
        "set_auto_retry" => "SetAutoRetry",
        "set_session_name" => "SetSessionName",
        "new_session" => "NewSession",
        "switch_session" => "SwitchSession",
        "get_state" => "GetState",
        "get_messages" => "GetMessages",
        "get_available_models" => "GetAvailableModels",
        "set_model" => "SetModel",
        "cycle_model" => "CycleModel",
        "get_available_thinking_levels" => "GetAvailableThinkingLevels",
        "set_thinking_level" => "SetThinkingLevel",
        "cycle_thinking_level" => "CycleThinkingLevel",
        "get_commands" => "GetCommands",
        "get_session_stats" => "GetSessionStats",
        "get_fork_messages" => "GetForkMessages",
        "get_entries" => "GetEntries",
        "fork" => "Fork",
        "clone_session" => "CloneSession",
        "auth_list" => "AuthList",
        "auth_status" => "AuthStatus",
        "auth_login" => "AuthLogin",
        "auth_logout" => "AuthLogout",
        "auth_cancel" => "AuthCancel",
        "quota_list" => "QuotaList",
        "set_steering_mode" => "SetSteeringMode",
        "set_follow_up_mode" => "SetFollowUpMode",
        "abort_retry" => "AbortRetry",
        "raw" => "Raw",
        _ => return None,
    })
}

/// `message` is orbit's word for the prompt text; the rest of the keys pass through.
pub fn params_of(v: &Value) -> Value {
    let mut out = v.clone();
    if let Some(m) = v.get("message") {
        out["text"] = m.clone();
    }
    out
}

/// orbit 的事件解析器（`Event::from_value`）把**整行**当成 `value`，
/// 所以字段必须平铺在顶层；再套一层 `"value"` 它就读不到了。
/// 这类键名错不会报错，只会让界面上一片空白——只有在真引擎下跑一轮才暴露。
fn flat(kind: &str, data: &Value) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("type".into(), Value::String(kind.into()));
    match data {
        Value::Object(m) => {
            for (k, v) in m {
                if k == "type" {
                    continue;
                }
                out.insert(k.clone(), v.clone());
            }
        }
        other => {
            out.insert("value".into(), other.clone());
        }
    }
    Value::Object(out)
}

/// 工具事件有两种方言：
///   HTTP/SSE：`{"item":{id, kind, metadata:{tool_name, tool_input, status, exit_code}}}`
///   stdio   ：`{toolCallId, toolName, args, result}`
/// 界面读的是后者那套键（`transcript.rs:2350` 起）。这里统一成界面认的形状。
fn tool_fields(ty: &str, data: &Value) -> Value {
    let item = data.get("item").unwrap_or(data);
    let meta = item.get("metadata").unwrap_or(&Value::Null);
    let first = |keys: &[&str]| -> Option<Value> {
        for src in [item, meta, data] {
            for k in keys {
                if let Some(v) = src.get(*k) {
                    if !v.is_null() {
                        return Some(v.clone());
                    }
                }
            }
        }
        None
    };
    let mut out = serde_json::Map::new();
    out.insert("type".into(), Value::String(ty.into()));
    if let Some(v) = first(&["toolCallId", "id"]) {
        out.insert("toolCallId".into(), v);
    }
    if let Some(v) = first(&["toolName", "tool_name", "tool"]) {
        out.insert("toolName".into(), v);
    }
    if let Some(v) = first(&["args", "tool_input", "input", "arguments"]) {
        out.insert("args".into(), v);
    }
    if let Some(v) = first(&["status"]) {
        out.insert("status".into(), v);
    }
    if let Some(v) = first(&["exitCode", "exit_code"]) {
        out.insert("exitCode".into(), v);
    }
    if let Some(v) = first(&["result", "output"]) {
        out.insert("result".into(), v);
    }
    Value::Object(out)
}

/// Our UI events → orbit events. Unknown kinds are forwarded as `unknown` so a
/// new engine notification shows up in the UI instead of vanishing.
pub fn events_to_orbit(events: &[UiEvent]) -> Vec<Value> {
    events
        .iter()
        .map(|e| match e.kind.as_str() {
            // orbit 读的是 `assistantMessageEvent`（types.rs:934），不是 `assistant`。
            // 键名写错不会报错，只会让界面一个字都不显示——所以这里对齐解析器的键。
            "text_delta" | "reasoning_delta" => {
                let text = e.data.get("delta").cloned().unwrap_or(Value::Null);
                let kind = if e.kind == "reasoning_delta" { "thinking_delta" } else { "text_delta" };
                json!({
                    "type": "message_update",
                    "assistantMessageEvent": { "type": kind, "delta": text, "text": text }
                })
            }
            "tool_output_delta" => tool_fields("tool_execution_update", &e.data),
            "item_start" => flat("message_start", &e.data),
            "item_end" => flat("message_end", &e.data),
            "tool_call" | "tool_start" => tool_fields("tool_execution_start", &e.data),
            "tool_end" => tool_fields("tool_execution_end", &e.data),
            "turn_start" => json!({ "type": "turn_start" }),
            "turn_end" => json!({ "type": "agent_end", "willRetry": false }),
            "usage" => json!({ "type": "message_update", "usage": e.data }),
            "compacted" | "compact_requested" => flat("compaction_end", &e.data),
            "cache_freeze" => flat("queue_update", &e.data),
            "steer_sent" | "follow_up_started" => flat("queue_update", &e.data),
            "error" => flat("extension_error", &e.data),
            other => json!({ "type": "unknown", "value": { "kind": other, "data": e.data } }),
        })
        .collect()
}

/// orbit's reply envelope.
pub fn response_of(id: &str, command: &str, success: bool, data: Value, error: Option<String>) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": command,
        "success": success,
        "data": if success { data } else { Value::Null },
        "error": error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_maps_and_renames_message_to_text() {
        let v = json!({"id": "1", "type": "prompt", "message": "hi"});
        assert_eq!(command_of("prompt"), Some("Prompt"));
        assert_eq!(params_of(&v)["text"], "hi");
    }

    #[test]
    fn every_orbit_type_we_claim_is_mapped() {
        for k in [
            "prompt", "steer", "follow_up", "abort", "clear_queue", "compact",
            "set_session_name", "new_session", "switch_session", "fork", "clone_session",
            "get_state", "get_messages", "set_model", "set_thinking_level", "quota_list",
        ] {
            assert!(command_of(k).is_some(), "{k} unmapped");
        }
        assert_eq!(command_of("nonsense"), None);
    }

    #[test]
    fn text_delta_becomes_message_update() {
        // 键名必须与 orbit 的解析器一致（`assistantMessageEvent`）；写错不报错，
        // 界面只会一片空白，所以这条断言盯的是**解析器读的那个键**。
        let out = events_to_orbit(&[UiEvent::new("text_delta", json!({"delta": "hello"}))]);
        assert_eq!(out[0]["type"], "message_update");
        assert_eq!(out[0]["assistantMessageEvent"]["type"], "text_delta");
        assert_eq!(out[0]["assistantMessageEvent"]["delta"], "hello");
        assert!(out[0].get("assistant").is_none(), "orbit does not read `assistant`");
    }

    #[test]
    fn turn_end_becomes_agent_end() {
        let out = events_to_orbit(&[UiEvent::new("turn_end", json!({}))]);
        assert_eq!(out[0]["type"], "agent_end");
        assert_eq!(out[0]["willRetry"], false, "orbit reads `willRetry`, not `will_retry`");
    }

    #[test]
    fn unknown_events_are_forwarded_not_dropped() {
        let out = events_to_orbit(&[UiEvent::new("engine/thread/goal/updated", json!({"x":1}))]);
        assert_eq!(out[0]["type"], "unknown");
    }
}
