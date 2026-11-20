//! SSE 客户端：连 harness 后端的 `/run/stream`。
//!
//! 传输用开源库：`reqwest` 发请求、`reqwest-eventsource` 解 SSE。
//! 以前是 spawn `curl` 子进程 + 自己按行解帧——那既违反"不自研"的约定，
//! 也让"连接还活着吗"没有答案（子进程悄悄退出，我们当成还连着）。
//! 同一课在 harnessd 上已经上过一次：界面一直转圈，查到最后是死连接被当成活的。

use futures::StreamExt as _;

/// 一条 SSE 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

impl SseEvent {
    /// 解析 data 里的 JSON，取一个字符串字段。
    pub fn data_str(&self, key: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(&self.data).ok()?;
        v.get(key)?.as_str().map(|s| s.to_string())
    }

    pub fn data_u64(&self, key: &str) -> Option<u64> {
        let v: serde_json::Value = serde_json::from_str(&self.data).ok()?;
        v.get(key)?.as_u64()
    }

    pub fn data_bool(&self, key: &str) -> Option<bool> {
        let v: serde_json::Value = serde_json::from_str(&self.data).ok()?;
        v.get(key)?.as_bool()
    }
}

/// 逐行解析 SSE 流。
///
/// 按规范：`event:` 与 `data:` 累积，空行表示一条完整事件。
pub struct SseParser {
    event: String,
    data: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            event: String::new(),
            data: String::new(),
        }
    }

    /// 喂一行，若凑成完整事件则返回。
    pub fn push(&mut self, line: &str) -> Option<SseEvent> {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if self.data.is_empty() && self.event.is_empty() {
                return None;
            }
            let ev = SseEvent {
                event: if self.event.is_empty() {
                    "message".into()
                } else {
                    std::mem::take(&mut self.event)
                },
                data: std::mem::take(&mut self.data),
            };
            return Some(ev);
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.trim_start());
        }
        None
    }
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

/// 发起一次流式任务，每个事件回调一次。
///
/// 阻塞调用，应在后台线程里跑。内部起一个 current-thread runtime 跑
/// `reqwest-eventsource`——断线、keep-alive、`last-event-id` 由库负责。
pub fn stream_task<F>(base_url: &str, task: &str, mut on_event: F) -> anyhow::Result<()>
where
    F: FnMut(SseEvent),
{
    let url = format!("{}/run/stream", base_url.trim_end_matches('/'));
    let payload = serde_json::json!({ "task": task });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let client = reqwest::Client::new();
        let mut es = reqwest_eventsource::EventSource::new(client.post(&url).json(&payload))
            .map_err(|e| anyhow::anyhow!("SSE 建立失败：{e}"))?;
        while let Some(item) = es.next().await {
            match item {
                Ok(reqwest_eventsource::Event::Message(m)) => on_event(SseEvent {
                    event: if m.event.is_empty() { "message".into() } else { m.event },
                    data: m.data,
                }),
                Ok(reqwest_eventsource::Event::Open) => {}
                Err(_) => break,
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_event() {
        let mut p = SseParser::new();
        assert_eq!(p.push("event: step"), None);
        assert_eq!(p.push("data: {\"step\":1}"), None);
        let ev = p.push("").unwrap();
        assert_eq!(ev.event, "step");
        assert_eq!(ev.data, "{\"step\":1}");
    }

    #[test]
    fn parses_consecutive_events() {
        let mut p = SseParser::new();
        let mut out = Vec::new();
        for line in [
            "event: start",
            "data: {\"task\":\"x\"}",
            "",
            "event: done",
            "data: {\"steps\":3}",
            "",
        ] {
            if let Some(ev) = p.push(line) {
                out.push(ev);
            }
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event, "start");
        assert_eq!(out[1].event, "done");
    }

    #[test]
    fn multiline_data_is_joined() {
        // SSE 规范允许多行 data，合并时用 \n
        let mut p = SseParser::new();
        p.push("event: text");
        p.push("data: line1");
        p.push("data: line2");
        let ev = p.push("").unwrap();
        assert_eq!(ev.data, "line1\nline2");
    }

    #[test]
    fn blank_lines_without_data_are_ignored() {
        let mut p = SseParser::new();
        assert_eq!(p.push(""), None);
        assert_eq!(p.push(""), None);
    }

    #[test]
    fn extracts_json_fields() {
        let ev = SseEvent {
            event: "tool".into(),
            data: r#"{"name":"read_file","ok":true,"chars":120}"#.into(),
        };
        assert_eq!(ev.data_str("name").as_deref(), Some("read_file"));
        assert_eq!(ev.data_bool("ok"), Some(true));
        assert_eq!(ev.data_u64("chars"), Some(120));
        assert_eq!(ev.data_str("missing"), None);
    }

    #[test]
    fn crlf_is_handled() {
        let mut p = SseParser::new();
        p.push("event: ping\r");
        p.push("data: {}\r");
        let ev = p.push("\r").unwrap();
        assert_eq!(ev.event, "ping");
    }
}
