//! harness 内核桥接：SSE 解析 + 事件翻译。
//!
//! **来源说明**（本项目第一原则要求写明出处）：
//!
//! 1. **SSE 解析器**照搬 [jcode](https://github.com/1jehuang/jcode) 的
//!    `crates/jcode-base/src/mcp/remote.rs::SseParser`（约 966 行起）。
//!    它按 WHATWG 的 Server-Sent Events 规范增量解析：LF / CRLF / 裸 CR
//!    都算换行、吃 BOM、跳过注释行、支持多行 `data:` 累积。
//!    那是 Rust 生态里少见的、不依赖 async 运行时的正确实现。
//!
//! 2. **事件翻译**照 [pi](https://github.com/badlogic/pi-mono) 的
//!    `docs/json.md`（JSON Event Stream，规范文档）逐字段对齐：
//!    `text_delta` 必须带 `contentIndex`、`tool_execution_end` 必带
//!    `isError` 与 `durationMs`、`agent_end` 带 `messages` 与 `willRetry`。
//!    orbit 的 `types.rs` 就是照这份文档定义的，所以对齐文档即对齐 UI。
//!
//! 本文件是新增的适配层，不是 orbit 原有代码。

use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

use crate::types::Event;

// ─────────────────────────────────────────────────────────────────────
// SSE 解析（搬自 jcode 的 SseParser）
// ─────────────────────────────────────────────────────────────────────

/// 一条解析出来的 SSE 记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSseEvent {
    /// `event:` 字段；缺省按规范为 `None`（调用方视作 `message`）。
    pub event: Option<String>,
    pub data: String,
}

/// 增量 UTF-8 SSE 解析器。
///
/// 搬自 jcode `crates/jcode-base/src/mcp/remote.rs`，去掉了它的
/// `anyhow` 错误类型与字节上限（我们是进程内调用，不面对不可信的远端字节流）。
/// 行为保持不变：LF / CRLF / 裸 CR 都算换行、吃 BOM、跳过注释、
/// 支持多行 `data:` 累积。
#[derive(Default)]
pub struct SseParser {
    line: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
    skip_lf: bool,
    first_line: bool,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<RawSseEvent> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\n' || byte == b'\r' {
                self.skip_lf = byte == b'\r';
                if let Some(event) = self.finish_line() {
                    events.push(event);
                }
            } else {
                self.line.push(byte);
            }
        }
        events
    }

    fn finish_line(&mut self) -> Option<RawSseEvent> {
        let bytes = std::mem::take(&mut self.line);
        // 内核写出的永远是合法 UTF-8；非 UTF-8 视为协议错误，丢弃该行。
        let mut line = String::from_utf8(bytes).ok()?;
        if !self.first_line {
            self.first_line = true;
            if let Some(rest) = line.strip_prefix('\u{feff}') {
                line = rest.to_string();
            }
        }
        if line.is_empty() {
            let event = self.event.take();
            if self.has_data {
                self.has_data = false;
                // 去掉最后一条 data 行留下的换行
                self.data.pop();
                return Some(RawSseEvent {
                    event,
                    data: std::mem::take(&mut self.data),
                });
            }
            return None;
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            // `id` / `retry` 对本地进程内流无意义，按规范忽略。
            _ => {}
        }
        None
    }
}

// ─────────────────────────────────────────────────────────────────────
// 内核执行
// ─────────────────────────────────────────────────────────────────────

/// 跑一次任务，事件通过 `sink` 流出。
///
/// `abort` 是取消标志：内核的流式执行器本身不读它，所以这里在每次写出时
/// 检查一次 —— 置位后立刻返回错误，内核的 `write_all` 随之向上冒泡并退出，
/// 效果等同上游「kill 掉 pi 子进程」。检查点选在写出边界，因为内核每一步
/// 都会推一个 SSE 事件，粒度足够细。
pub fn run_task<W: Write>(
    state_dir: &Path,
    workdir: &Path,
    task: &str,
    sink: W,
    abort: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<()> {
    let cfg = harness_cli::exec::ExecConfig::from_env();
    let mut sink = AbortableSink { inner: sink, abort };
    harness_cli::sse::stream_task(&mut sink, state_dir, workdir, task, &cfg)
}

/// 包一层写入端：置位后拒绝继续写出，内核的 `write_all` 会因此报错并退出。
struct AbortableSink<'a, W: Write> {
    inner: W,
    abort: &'a std::sync::atomic::AtomicBool,
}

impl<W: Write> Write for AbortableSink<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use std::sync::atomic::Ordering;
        if self.abort.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "已取消",
            ));
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ─────────────────────────────────────────────────────────────────────
// 事件翻译：harness SSE → orbit Event
// ─────────────────────────────────────────────────────────────────────

/// 取一条 `text` 事件的正文（供调用方累积完整回复）。
pub fn text_of(raw: &RawSseEvent) -> Option<String> {
    let v: Value = serde_json::from_str(&raw.data).ok()?;
    v.get("text").and_then(Value::as_str).map(str::to_owned)
}

/// 把一条 SSE 记录翻成 orbit 事件。返回 `None` 表示不映射（如心跳注释）。
pub fn translate(raw: &RawSseEvent) -> Option<Event> {
    let v: Value = serde_json::from_str(&raw.data).ok()?;
    // harness 的 `SseEvent::encode` 把事件名放在 `event:` 行；
    // 同时兜底读 JSON 里的 `event` 字段（两种写法都见过）。
    let kind = raw
        .event
        .as_deref()
        .filter(|k| !k.is_empty() && *k != "message")
        .map(str::to_owned)
        .or_else(|| v.get("event").and_then(Value::as_str).map(str::to_owned))?;

    Some(match kind.as_str() {
        // ── 生命周期（pi `json.md` › Agent and turn events）─────────
        "start" => Event::AgentStart,
        "step" => Event::TurnStart,
        "done" => Event::AgentEnd {
            // pi 的 `agent_end` 带 `messages` 与 `willRetry`；
            // 内核的 done 只报步数，messages 留空由后续 message_end 补。
            will_retry: false,
        },
        "error" => Event::MessageEnd {
            value: json!({
                "error": v.get("error").and_then(Value::as_str).unwrap_or("未知错误"),
            }),
        },

        // ── 文本流（pi `json.md` › Reconstruct streaming messages）───
        // `text_delta` 必须带 `contentIndex`，orbit 的 transcript 靠它定位块。
        "text" => Event::MessageUpdate {
            usage: None,
            assistant: Some(crate::types::AssistantMessageEvent::TextDelta {
                delta: v.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            }),
        },
        "thinking" => Event::MessageUpdate {
            usage: None,
            assistant: Some(crate::types::AssistantMessageEvent::ThinkingDelta {
                delta: v.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            }),
        },

        // ── 工具执行（pi `json.md` › Tool execution events）──────────
        // 内核的 `tool` 事件字段是 name / input / summary。
        "tool" => Event::ToolExecutionStart {
            value: json!({
                "toolCallId": v.get("call").and_then(Value::as_str).unwrap_or(""),
                "toolName": v.get("name").and_then(Value::as_str).unwrap_or(""),
                "args": v.get("input").cloned().unwrap_or(Value::Null),
            }),
        },
        "tool_done" => Event::ToolExecutionEnd {
            value: json!({
                "toolCallId": v.get("call").and_then(Value::as_str).unwrap_or(""),
                "toolName": v.get("name").and_then(Value::as_str).unwrap_or(""),
                "isError": !v.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "result": json!({
                    "content": [{
                        "type": "text",
                        "text": v.get("chars").and_then(Value::as_u64)
                            .map(|n| format!("{n} 字符"))
                            .unwrap_or_default(),
                    }],
                }),
            }),
        },

        // ── 守卫提示：内核独有，pi 没有对应事件 ─────────────────────
        // 当作一段 assistant 文本投出去，UI 会正常显示在对话流里。
        "guard" => Event::MessageUpdate {
            usage: None,
            assistant: Some(crate::types::AssistantMessageEvent::TextDelta {
                delta: format!(
                    "\n\n⚠ {}\n\n",
                    v.get("note").and_then(Value::as_str).unwrap_or("")
                ),
            }),
        },

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&str]) -> Vec<RawSseEvent> {
        let mut p = SseParser::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.push(c.as_bytes()));
        }
        out
    }

    #[test]
    fn parses_one_event() {
        let evs = collect(&["event: text\ndata: {\"text\":\"hi\"}\n\n"]);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("text"));
        assert_eq!(evs[0].data, "{\"text\":\"hi\"}");
    }

    #[test]
    fn parses_split_chunks() {
        // 字节流会在任意位置断开，解析器必须能跨 chunk 续上
        let evs = collect(&["event: te", "xt\ndata: {\"te", "xt\":\"a\"}\n", "\n"]);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("text"));
    }

    #[test]
    fn handles_crlf_and_bare_cr() {
        let a = collect(&["event: done\r\ndata: {}\r\n\r\n"]);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].event.as_deref(), Some("done"));
        let b = collect(&["event: done\rdata: {}\r\r"]);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn skips_comments() {
        let evs = collect(&[": keep-alive\n\n", "event: x\ndata: {}\n\n"]);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("x"));
    }

    #[test]
    fn multiline_data_joins_with_newline() {
        let evs = collect(&["event: t\ndata: line1\ndata: line2\n\n"]);
        assert_eq!(evs[0].data, "line1\nline2");
    }

    fn tr(kind: &str, json_body: &str) -> Option<Event> {
        translate(&RawSseEvent {
            event: Some(kind.to_string()),
            data: json_body.to_string(),
        })
    }

    #[test]
    fn start_and_done() {
        assert!(matches!(tr("start", "{}"), Some(Event::AgentStart)));
        assert!(matches!(
            tr("done", "{\"steps\":3}"),
            Some(Event::AgentEnd { will_retry: false })
        ));
    }

    #[test]
    fn text_delta_carries_delta() {
        match tr("text", "{\"text\":\"你好\"}") {
            Some(Event::MessageUpdate {
                assistant: Some(crate::types::AssistantMessageEvent::TextDelta { delta }),
                ..
            }) => assert_eq!(delta, "你好"),
            other => panic!("意外的映射：{other:?}"),
        }
    }

    #[test]
    fn tool_start_uses_pi_field_names() {
        // pi 规范是 toolCallId / toolName / args
        match tr("tool", "{\"name\":\"read_file\",\"input\":{\"path\":\"a\"}}") {
            Some(Event::ToolExecutionStart { value }) => {
                assert_eq!(value.get("toolName").unwrap(), "read_file");
                assert_eq!(value.get("args").unwrap()["path"], "a");
            }
            other => panic!("意外的映射：{other:?}"),
        }
    }

    #[test]
    fn tool_end_marks_error() {
        match tr("tool_done", "{\"name\":\"bash\",\"ok\":false,\"chars\":9}") {
            Some(Event::ToolExecutionEnd { value }) => {
                assert_eq!(value.get("isError").unwrap(), true);
                assert_eq!(value.get("toolName").unwrap(), "bash");
            }
            other => panic!("意外的映射：{other:?}"),
        }
    }

    #[test]
    fn unknown_event_is_dropped() {
        assert!(tr("something-new", "{}").is_none());
    }

    #[test]
    fn malformed_json_is_dropped() {
        assert!(tr("text", "not json").is_none());
    }
}
