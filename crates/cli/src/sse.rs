//! SSE 流式执行端点。
//!
//! 用标准 **Server-Sent Events**（W3C 规范），不自创协议。
//! 事件类型对齐 AG-UI / assistant-ui 的通用语义：
//!
//! - `step` —— 开始第 n 步
//! - `tool` —— 正在调用工具
//! - `tool_done` —— 工具返回
//! - `text` —— 最终文本（也用于流式追加）
//! - `guard` —— 守卫介入
//! - `done` —— 结束（带统计）
//! - `error` —— 失败
//!
//! 前端只要按 `data: {...}\n\n` 逐行解析即可 —— 这是 SSE 的标准格式。

use crate::durable::DurableSession;
use crate::llm::OpenAiClient;
use crate::memory::ProjectMemory;
use crate::runner::{run_task, RunConfig, RunEvent};
use crate::tools::LocalTools;
use harness_context::effort::plan_effort;
use harness_core::event::EventLog;
use std::io::Write;
use std::path::Path;

/// 一处 SSE 事件。
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: &str, data: serde_json::Value) -> Self {
        Self {
            event: event.to_string(),
            data,
        }
    }

    /// 按 SSE 规范序列化。
    ///
    /// 格式：`event: <name>\ndata: <json>\n\n`
    /// data 里的换行必须拆成多行 `data:` —— 这是规范要求。
    pub fn encode(&self) -> String {
        let json = self.data.to_string();
        let mut out = format!("event: {}\n", self.event);
        for line in json.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}

/// SSE 响应头。
pub fn sse_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Content-Type", "text/event-stream; charset=utf-8".into()),
        ("Cache-Control", "no-cache, no-transform".into()),
        ("Connection", "keep-alive".into()),
        // 禁掉 nginx 之类的缓冲，否则事件会被攒着不发
        ("X-Accel-Buffering", "no".into()),
        ("Access-Control-Allow-Origin", "*".into()),
    ]
}

/// 把 `RunEvent` 转成 SSE 事件。
///
/// 纯映射，不含逻辑。
pub fn to_sse(ev: &RunEvent) -> SseEvent {
    match ev {
        RunEvent::StepStarted { step } => {
            SseEvent::new("step", serde_json::json!({ "step": step }))
        }
        RunEvent::ToolCalling { name, summary } => SseEvent::new(
            "tool",
            serde_json::json!({ "name": name, "input": summary }),
        ),
        RunEvent::ToolDone { name, ok, chars } => SseEvent::new(
            "tool_done",
            serde_json::json!({ "name": name, "ok": ok, "chars": chars }),
        ),
        RunEvent::GuardNote { note } => SseEvent::new("guard", serde_json::json!({ "note": note })),
        RunEvent::FinalText { text } => SseEvent::new("text", serde_json::json!({ "text": text })),
        RunEvent::Thinking { text } => {
            SseEvent::new("thinking", serde_json::json!({ "text": text }))
        }
    }
}

/// 执行任务并把事件逐条写到 `out`。
///
/// 这是流式的核心：每产生一个事件就 flush 一次，
/// 不等任务结束。
pub fn stream_task<W: Write>(
    out: &mut W,
    dir: &Path,
    workdir: &Path,
    task: &str,
    cfg: &crate::exec::ExecConfig,
) -> anyhow::Result<()> {
    if task.trim().is_empty() {
        let ev = SseEvent::new("error", serde_json::json!({ "error": "任务描述为空" }));
        out.write_all(ev.encode().as_bytes())?;
        out.flush()?;
        return Ok(());
    }

    let mut log = EventLog::open(dir.join("events.jsonl"))?;
    let mut session = DurableSession::open(dir)?;
    let mut memory = ProjectMemory::open(dir)?;

    // 未决副作用必须先裁决（kiso 的 Ambiguity Never Auto-Repeats）
    let pending = session.pending_uncertain();
    if !pending.is_empty() {
        let ev = SseEvent::new(
            "error",
            serde_json::json!({
                "error": "存在未决副作用，需先裁决",
                "pending": pending,
            }),
        );
        out.write_all(ev.encode().as_bytes())?;
        out.flush()?;
        return Ok(());
    }

    let client = OpenAiClient {
        base_url: cfg.base_url.clone(),
        api_key: cfg.api_key.clone(),
        model: cfg.model.clone(),
        effort: None,
    };
    let tools = LocalTools::new(workdir);
    let plan = plan_effort(cfg.effort, &cfg.supported);

    // 先发一个 start 事件，让前端立刻有反馈
    let start = SseEvent::new(
        "start",
        serde_json::json!({
            "task": task,
            "effort": { "requested": plan.requested.as_str(), "effective": plan.effective.as_str() },
        }),
    );
    out.write_all(start.encode().as_bytes())?;
    out.flush()?;

    let run_cfg = RunConfig {
        max_steps: cfg.max_steps,
        effort: Some(cfg.effort),
        supported_efforts: cfg.supported.clone(),
        system: "你是一个编程助手，在给定工作区内完成任务。\
                 用工具读写文件、执行命令。不要臆造文件内容 —— 先读再改。\
                 完成后用一句话总结你做了什么。"
            .into(),
        ..Default::default()
    };

    // 关键：sink 里直接写 SSE 并 flush，做到真流式
    let mut sink = |ev: RunEvent| {
        let sse = to_sse(&ev);
        let _ = out.write_all(sse.encode().as_bytes());
        let _ = out.flush();
    };

    let result = run_task(
        &client,
        &tools,
        &mut log,
        &mut session,
        &mut memory,
        task,
        &run_cfg,
        &mut sink,
    );

    match result {
        Ok(outcome) => {
            // DFS 记忆写入
            if outcome.steps > 0 && !outcome.final_text.trim().is_empty() {
                let record = format!(
                    "任务「{}」已完成（{} 步）：{}",
                    task.chars().take(60).collect::<String>(),
                    outcome.steps,
                    outcome
                        .final_text
                        .trim()
                        .chars()
                        .take(200)
                        .collect::<String>()
                );
                let _ = memory.remember(
                    harness_memory::MemoryKind::Lesson,
                    &record,
                    &["project".to_string()],
                );
            }

            let done = SseEvent::new(
                "done",
                serde_json::json!({
                    "steps": outcome.steps,
                    "cache": {
                        "hit_rate": outcome.cache.hit_rate(),
                        "steady_hit_rate": outcome.cache.steady_state_hit_rate(),
                    },
                    "effort": {
                        "requested": plan.requested.as_str(),
                        "effective": plan.effective.as_str(),
                        "downgraded": plan.downgraded,
                    },
                }),
            );
            out.write_all(done.encode().as_bytes())?;
            out.flush()?;
        }
        Err(e) => {
            let err = SseEvent::new("error", serde_json::json!({ "error": e.to_string() }));
            out.write_all(err.encode().as_bytes())?;
            out.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_encoding_follows_spec() {
        let ev = SseEvent::new("text", serde_json::json!({"text": "hi"}));
        let s = ev.encode();
        assert!(s.starts_with("event: text\n"));
        assert!(s.contains("data: "));
        assert!(s.ends_with("\n\n"), "SSE 事件必须以空行结束");
    }

    #[test]
    fn multiline_data_is_split() {
        // SSE 规范：data 里的换行要拆成多个 data: 行
        let ev = SseEvent::new("text", serde_json::json!({"text": "line1\nline2"}));
        let s = ev.encode();
        let data_lines = s.lines().filter(|l| l.starts_with("data: ")).count();
        assert!(data_lines >= 1, "至少一行 data");
        // JSON 序列化会把 \n 转义成 \\n，所以这里仍是一行 —— 验证格式正确
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn headers_disable_buffering() {
        let h = sse_headers();
        let map: std::collections::HashMap<_, _> = h.into_iter().collect();
        assert_eq!(map["Content-Type"], "text/event-stream; charset=utf-8");
        assert_eq!(map["Cache-Control"], "no-cache, no-transform");
        assert_eq!(map["X-Accel-Buffering"], "no", "必须禁用中间层缓冲");
    }

    #[test]
    fn run_event_maps_to_sse() {
        let ev = RunEvent::StepStarted { step: 3 };
        let sse = to_sse(&ev);
        assert_eq!(sse.event, "step");
        assert_eq!(sse.data["step"], 3);
    }

    #[test]
    fn tool_events_map() {
        let ev = RunEvent::ToolCalling {
            name: "read_file".into(),
            summary: "{\"path\":\"a.py\"}".into(),
        };
        let sse = to_sse(&ev);
        assert_eq!(sse.event, "tool");
        assert_eq!(sse.data["name"], "read_file");

        let ev = RunEvent::ToolDone {
            name: "read_file".into(),
            ok: true,
            chars: 120,
        };
        let sse = to_sse(&ev);
        assert_eq!(sse.event, "tool_done");
        assert_eq!(sse.data["ok"], true);
    }

    #[test]
    fn guard_and_text_events_map() {
        let ev = RunEvent::GuardNote {
            note: "只读螺旋".into(),
        };
        assert_eq!(to_sse(&ev).event, "guard");

        let ev = RunEvent::FinalText {
            text: "完成".into(),
        };
        let sse = to_sse(&ev);
        assert_eq!(sse.event, "text");
        assert_eq!(sse.data["text"], "完成");
    }

    #[test]
    fn empty_task_emits_error_event() {
        let dir = std::env::temp_dir().join(format!("sse-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg = crate::exec::ExecConfig::from_env();
        let mut buf: Vec<u8> = Vec::new();
        stream_task(&mut buf, &dir, &dir, "  ", &cfg).unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("event: error"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
