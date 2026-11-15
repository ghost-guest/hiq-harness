//! REST 端点：给桌面与手机 UI 用。
//!
//! 与 WS 协议并行：UI 用 REST 做一次性查询，WS 做实时事件流。
//! 这也是 Cindy host 的做法（HTTP + WS 同端口）。

use serde::Serialize;

/// 端点路由表。`method + path` → 处理器名。
///
/// 单独抽出来便于测试与文档生成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Health,
    Devices,
    Events,
    Memory,
    Pending,
    Resolve,
    Run,
    Sop,
    Ui,
    NotFound,
}

impl Route {
    /// 解析请求。
    pub fn parse(method: &str, path: &str) -> Self {
        // 去掉 query
        let p = path.split('?').next().unwrap_or(path);
        match (method, p) {
            ("GET", "/health") => Self::Health,
            ("GET", "/devices") => Self::Devices,
            ("GET", "/events") => Self::Events,
            ("GET", "/memory") => Self::Memory,
            ("POST", "/memory") => Self::Memory,
            ("GET", "/pending") => Self::Pending,
            ("POST", "/resolve") => Self::Resolve,
            ("POST", "/run") => Self::Run,
            ("POST", "/sop") => Self::Sop,
            ("GET", "/") | ("GET", "/ui") | ("GET", "/index.html") => Self::Ui,
            _ => Self::NotFound,
        }
    }

    /// 是否需要请求体。
    ///
    /// 供将来的请求校验用（当前 handle_conn 已统一读取）。
    #[allow(dead_code)]
    pub fn has_body(&self) -> bool {
        matches!(self, Self::Memory | Self::Resolve | Self::Run | Self::Sop)
    }
}

/// 事件列表项。
#[derive(Debug, Serialize)]
pub struct EventView {
    pub seq: u64,
    pub kind: String,
    pub summary: String,
}

/// 从 `EventKind` 生成一行摘要。
///
/// UI 用，不追求完整。
pub fn summarize(kind: &harness_core::event::EventKind) -> String {
    use harness_core::event::EventKind::*;
    match kind {
        UserMessage { text } => truncate(text, 80),
        AssistantMessage { text, .. } => truncate(text, 80),
        ToolCall { name, input, .. } => {
            format!("{name} {}", truncate(&input.to_string(), 60))
        }
        ToolResult { call_id, .. } => truncate(call_id, 60),
        Compaction { checkpoint_id, .. } => format!("检查点 {checkpoint_id}"),
        PermissionDecided { verdict, .. } => format!("{verdict:?}"),
        GoalSet { objective } => truncate(objective, 80),
        GoalDone { summary } => truncate(summary, 80),
    }
}

pub fn kind_name(kind: &harness_core::event::EventKind) -> &'static str {
    use harness_core::event::EventKind::*;
    match kind {
        UserMessage { .. } => "user_message",
        AssistantMessage { .. } => "assistant_message",
        ToolCall { .. } => "tool_call",
        ToolResult { .. } => "tool_result",
        Compaction { .. } => "compaction",
        PermissionDecided { .. } => "permission_decided",
        GoalSet { .. } => "goal_set",
        GoalDone { .. } => "goal_done",
    }
}

fn truncate(s: &str, n: usize) -> String {
    let one = s.lines().next().unwrap_or("");
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one.to_string()
    }
}

/// 读取查询参数里的 limit。
pub fn parse_limit(path: &str, default: usize) -> usize {
    path.split("limit=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_parse() {
        assert_eq!(Route::parse("GET", "/health"), Route::Health);
        assert_eq!(Route::parse("GET", "/devices"), Route::Devices);
        assert_eq!(Route::parse("GET", "/events?limit=10"), Route::Events);
        assert_eq!(Route::parse("POST", "/run"), Route::Run);
        assert_eq!(Route::parse("POST", "/sop"), Route::Sop);
        assert_eq!(Route::parse("GET", "/"), Route::Ui);
        assert_eq!(Route::parse("GET", "/nope"), Route::NotFound);
    }

    #[test]
    fn method_matters() {
        // GET /run 不存在，只有 POST
        assert_eq!(Route::parse("GET", "/run"), Route::NotFound);
        assert_eq!(Route::parse("POST", "/health"), Route::NotFound);
    }

    #[test]
    fn body_required_for_mutations() {
        assert!(Route::Run.has_body());
        assert!(Route::Resolve.has_body());
        assert!(!Route::Health.has_body());
        assert!(!Route::Events.has_body());
    }

    #[test]
    fn limit_parsed_from_query() {
        assert_eq!(parse_limit("/events?limit=10", 50), 10);
        assert_eq!(parse_limit("/events", 50), 50);
        assert_eq!(parse_limit("/events?foo=1&limit=5", 50), 5);
    }

    #[test]
    fn summarize_covers_all_kinds() {
        use harness_core::event::EventKind;
        let kinds = vec![
            EventKind::UserMessage { text: "hi".into() },
            EventKind::AssistantMessage {
                text: "hello".into(),
                blocks: vec![],
            },
            EventKind::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "a"}),
            },
            EventKind::GoalSet {
                objective: "修 bug".into(),
            },
        ];
        for k in kinds {
            let s = summarize(&k);
            assert!(!s.is_empty());
            assert!(!kind_name(&k).is_empty());
        }
    }

    #[test]
    fn long_text_is_truncated() {
        let long = "x".repeat(200);
        let s = summarize(&harness_core::event::EventKind::UserMessage { text: long });
        assert!(
            s.chars().count() <= 81,
            "应截断到 80 字符：{}",
            s.chars().count()
        );
        assert!(s.ends_with('…'));
    }
}
