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
        .map(|e| -> Value { match e.kind.as_str() {
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
            "item_start" => message_event("message_start", &e.data),
            "item_end" => message_event("message_end", &e.data),
            "tool_call" | "tool_start" => tool_fields("tool_execution_start", &e.data),
            "tool_end" => tool_fields("tool_execution_end", &e.data),
            "turn_start" => json!({ "type": "turn_start" }),
            // 回合结束要发**两**条：`agent_end` 只是"这一轮循环结束了"，
            // 界面据此清掉流式状态但**故意把回合留着不关**（agent 循环可能还要
            // 再来一轮）。真正关回合的是 `agent_settled`——界面的
            // `end_live_run()` 只挂在它上面。
            //
            // 以前只发 `agent_end`。表现：第一回合答完，界面仍认为回合在跑，
            // 于是下一条消息一直排队转圈，永远等不到输出。
            // 这条只能接真界面才看得出来——内核侧事件一条不少、回执也对。
            "turn_end" => json!({ "type": "agent_end", "willRetry": false }),
            "usage" => json!({ "type": "message_update", "usage": e.data }),
            "compacted" | "compact_requested" => flat("compaction_end", &e.data),
            "cache_freeze" => flat("queue_update", &e.data),
            "steer_sent" | "follow_up_started" => flat("queue_update", &e.data),
            "error" => flat("extension_error", &e.data),
            other => json!({ "type": "unknown", "value": { "kind": other, "data": e.data } }),
        } })
        // 一条内核事件可能要变成界面上的多条。界面认的是 pi 的那套词汇表
        // （见 `orbit-pi` 测试里 FakePi 捕获的标准序列）：
        //   agent_start → turn_start → message_start → … → agent_end → agent_settled
        // 早先只发了 `turn_start` 和 `agent_end`，缺的两头都会让界面卡住：
        //   - 缺 agent_start：界面不认为有一轮在跑；
        //   - 缺 agent_settled：`agent_end` 只清流式状态、**故意不关回合**，
        //     界面于是永远认为回合没结束，下一条消息只能排队转圈。
        .flat_map(|v| match v.get("type").and_then(Value::as_str) {
            Some("turn_start") => vec![json!({ "type": "agent_start" }), v],
            Some("agent_end") => vec![v, json!({ "type": "agent_settled", "aborted": false })],
            _ => vec![v],
        })
        .collect()
}


/// `message_start` / `message_end`：**必须带 `role`**。
///
/// 界面建消息行时先看 `role`（`ChatMessage::from_value` 里 `value.get("role")?`），
/// 没有 role 就直接丢弃。我们以前只把引擎的 `item` 摊平，role 落在 item 里面，
/// 顶层没有——于是运行期间一条消息行都建不出来：界面上只剩一个计时器，
/// 思考、工具、文本全要等回合结束、从历史重载时才铺出来。
/// 这就是"运行中没有过程"的根。
fn message_event(ty: &str, data: &Value) -> Value {
    let mut out = flat(ty, data);
    let item = data.get("item").unwrap_or(data);
    let kind = item
        .get("kind")
        .or_else(|| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let role = match kind {
        "user_message" | "userMessage" => Some("user"),
        "agent_message" | "agentMessage" | "agent_reasoning" | "agentReasoning" => Some("assistant"),
        _ => None,
    };
    if let (Some(role), Value::Object(map)) = (role, &mut out) {
        use serde_json::json;
        map.entry("role".to_string()).or_insert_with(|| json!(role));
        // 界面读正文读的是 `content` 数组（pi 的方言），我们这边成品文本在
        // `item.detail` 里。不翻译的话：日志里有回答，界面渲染不出——因为
        // `ChatMessage::from_value` 拿到的是一个没有 content 的 assistant 消息。
        if role == "assistant" {
            let detail = item
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !detail.trim().is_empty() && !map.contains_key("content") {
                map.insert(
                    "content".to_string(),
                    json!([{ "type": "text", "text": detail }]),
                );
            }
        }
    }
    out
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
    /// 扁平化：字段必须**直接**在顶层，不能套一层。
    ///
    /// orbit 的事件解析用的是**整行**做 value（`Event::from_value`），
    /// 它按 `{"type": ..., <字段直接在顶层>}` 去读。套一层 `"value"` 之后
    /// 它读不到任何字段——表现为**界面一片空白**，而且不报错。
    ///
    /// 变异扫描抓到的：把 `flat` 改成嵌套，整个 crate 的测试全绿。
    /// 因为所有测试都走 `text_delta` 那条路径，那条**不经过 `flat`**。
    #[test]
    fn flat_keeps_fields_at_the_top_level() {
        let out = flat("message_start", &json!({ "id": "m1", "role": "assistant" }));
        assert_eq!(out["type"], "message_start");
        assert_eq!(out["id"], "m1", "字段必须在顶层，orbit 不会往里层找");
        assert_eq!(out["role"], "assistant");
        assert!(
            out.get("value").is_none(),
            "不能再套一层 value —— orbit 把整行当 value，嵌套了就什么都读不到（界面空白）"
        );

        // 非对象也要能转，且内容不能丢。
        let arr = flat("x", &json!([1, 2, 3]));
        assert_eq!(arr["type"], "x");
        assert_eq!(arr["value"], json!([1, 2, 3]), "非对象时放 value，但只在顶层");

        // `type` 由 kind 决定，data 里的 type 不能覆盖它。
        let clash = flat("real", &json!({ "type": "fake", "a": 1 }));
        assert_eq!(clash["type"], "real", "data 里的 type 不能盖掉 kind");
        assert_eq!(clash["a"], 1);
    }

    /// 回合结束必须发**两条**：`agent_end` 只清流式状态，界面故意把回合留着
    /// 不关（agent 循环可能还要再来一轮）。真正关回合的是 `agent_settled`。
    ///
    /// 只发 `agent_end` 的表现：第一回合答完，界面仍认为回合在跑，
    /// 下一条消息永远排队转圈。内核侧事件一条不少、回执也对——只有接真界面
    /// 才看得出来，所以这条单测钉的是"两条都要有、顺序对"。
    #[test]
    fn turn_end_emits_agent_end_then_agent_settled() {
        let out = events_to_orbit(&[UiEvent::new("turn_end", json!({}))]);
        let types: Vec<&str> = out
            .iter()
            .filter_map(|v| v.get("type").and_then(Value::as_str))
            .collect();
        assert_eq!(types, vec!["agent_end", "agent_settled"], "回合没真正收尾");
        assert_eq!(out[1]["aborted"], json!(false));
    }

    /// 界面的标准序列是 `agent_start → turn_start → … → agent_end → agent_settled`
    /// （`orbit-pi` 的 FakePi 就是照捕获的 pi 会话写的）。两头缺一个都会卡。
    #[test]
    fn turn_start_also_announces_agent_start() {
        let out = events_to_orbit(&[UiEvent::new("turn_start", json!({}))]);
        let types: Vec<&str> = out
            .iter()
            .filter_map(|v| v.get("type").and_then(Value::as_str))
            .collect();
        assert_eq!(types, vec!["agent_start", "turn_start"]);
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
