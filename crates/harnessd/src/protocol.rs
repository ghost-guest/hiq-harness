//! Wire types for both seams.
//!
//! Engine side mirrors the codex/codewhale app-server convention: line-delimited
//! JSON, **no `jsonrpc` field**. Client side is orbit's `Command` / `Event` shape,
//! which we keep as loosely-typed JSON so the UI can evolve without breaking us.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Frame {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request,
    Notification,
    Response,
}

impl Frame {
    pub fn request(id: u64, method: &str, params: Value) -> Self {
        Self { id: Some(id), method: Some(method.into()), params: Some(params), ..Default::default() }
    }

    pub fn notification(method: &str, params: Value) -> Self {
        Self { method: Some(method.into()), params: Some(params), ..Default::default() }
    }

    pub fn response(id: u64, result: Value) -> Self {
        Self { id: Some(id), result: Some(result), ..Default::default() }
    }

    pub fn error_response(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self { id: Some(id), error: Some(RpcError { code, message: message.into() }), ..Default::default() }
    }

    pub fn kind(&self) -> Kind {
        match (&self.method, &self.id) {
            (Some(_), Some(_)) => Kind::Request,
            (Some(_), None) => Kind::Notification,
            _ => Kind::Response,
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

pub fn encode(frame: &Frame) -> String {
    let mut s = serde_json::to_string(frame).expect("frame is serialisable");
    s.push('\n');
    s
}

pub fn decode(line: &str) -> Result<Frame> {
    let line = line.trim();
    if line.is_empty() {
        bail!("empty frame");
    }
    Ok(serde_json::from_str(line)?)
}

/// One event pushed to the UI. `kind` stays a string so a new engine notification
/// never needs a code change here (see KERNEL-FUSION-PLAN §3, seam ①).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiEvent {
    pub kind: String,
    pub data: Value,
}

impl UiEvent {
    pub fn new(kind: impl Into<String>, data: Value) -> Self {
        Self { kind: kind.into(), data }
    }
}

/// Engine notification → UI event name. Unknown notifications are forwarded
/// verbatim under `engine/<method>` rather than dropped: a silent drop is how
/// UI bugs become invisible.
pub fn ui_kind_for(method: &str) -> String {
    match method {
        "item/agentMessage/delta" => "text_delta",
        "item/reasoning/textDelta" => "reasoning_delta",
        "item/started" => "item_start",
        "item/completed" => "item_end",
        "item/commandExecution/outputDelta" => "tool_output_delta",
        "turn/started" => "turn_start",
        "turn/completed" => "turn_end",
        "turn/plan/updated" => "plan_update",
        "turn/diff/updated" => "diff_update",
        "thread/tokenUsage/updated" => "usage",
        "thread/compacted" => "compacted",
        "thread/status/changed" => "status",
        "thread/queue/changed" => "queue",
        "error" => "error",
        other => return format!("engine/{other}"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_without_jsonrpc_field() {
        let line = encode(&Frame::request(7, "turn/start", serde_json::json!({"input": []})));
        assert!(!line.contains("jsonrpc"), "codex convention: no jsonrpc field");
        let back = decode(&line).unwrap();
        assert_eq!(back.kind(), Kind::Request);
        assert_eq!(back.id, Some(7));
    }

    #[test]
    fn unknown_notification_is_forwarded_not_dropped() {
        assert_eq!(ui_kind_for("thread/goal/updated"), "engine/thread/goal/updated");
        assert_eq!(ui_kind_for("item/agentMessage/delta"), "text_delta");
    }
}
