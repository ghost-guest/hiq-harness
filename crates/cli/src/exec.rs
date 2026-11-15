//! 任务执行端点：让 UI 能真正跑任务。
//!
//! 复用 `runner::run_task`，不重造逻辑。
//! 同步执行并返回最终文本 —— assistant-ui 那边按「一次性响应」处理。
//!
//! 未来要做流式，改这里即可（runner 已按步循环，吐事件不难）。

use crate::durable::DurableSession;
use crate::llm::OpenAiClient;
use crate::memory::ProjectMemory;
use crate::runner::{run_task, RunConfig};
use crate::tools::LocalTools;
use harness_context::effort::{plan_effort, Effort};
use harness_core::event::EventLog;
use serde_json::{json, Value};
use std::path::Path;

/// 执行配置。由环境变量控制，便于 UI 侧不改代码。
#[derive(Debug, Clone)]
pub struct ExecConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_steps: u32,
    pub effort: Effort,
    pub supported: Vec<Effort>,
}

impl ExecConfig {
    /// 从环境变量读，缺省用 Magpie 中继。
    pub fn from_env() -> Self {
        let base_url =
            std::env::var("HARNESS_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3425/v1".into());
        let api_key = std::env::var("HARNESS_API_KEY").unwrap_or_else(|_| "magpie".into());
        let model =
            std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "auto/deepseek-v4.1-flash".into());
        let max_steps = std::env::var("HARNESS_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        let effort = std::env::var("HARNESS_EFFORT")
            .ok()
            .and_then(|s| Effort::parse(&s).ok())
            .unwrap_or(Effort::High);

        Self {
            base_url,
            api_key,
            model,
            max_steps,
            effort,
            // Magpie 上这个模型只支持 low/high/max（实测）
            supported: vec![Effort::Low, Effort::High, Effort::Max],
        }
    }
}

/// 执行一个任务，返回给 UI 的 JSON。
///
/// 返回结构对齐 assistant-ui 期望的简单文本响应。
pub fn execute_task(dir: &Path, workdir: &Path, task: &str, cfg: &ExecConfig) -> (u16, Value) {
    if task.trim().is_empty() {
        return (400, json!({"error": "任务描述为空"}));
    }

    let mut log = match EventLog::open(dir.join("events.jsonl")) {
        Ok(l) => l,
        Err(e) => return (500, json!({"error": format!("打开日志失败：{e}")})),
    };
    let mut session = match DurableSession::open(dir) {
        Ok(s) => s,
        Err(e) => return (500, json!({"error": format!("打开会话失败：{e}")})),
    };
    let mut memory = match ProjectMemory::open(dir) {
        Ok(m) => m,
        Err(e) => return (500, json!({"error": format!("打开记忆失败：{e}")})),
    };

    // 未决副作用必须先裁决（kiso 的 Ambiguity Never Auto-Repeats）
    let pending = session.pending_uncertain();
    if !pending.is_empty() {
        return (
            409,
            json!({
                "error": "存在未决副作用，需先裁决",
                "pending": pending,
                "hint": "POST /resolve {exec_id, verdict}",
            }),
        );
    }

    let client = OpenAiClient {
        base_url: cfg.base_url.clone(),
        api_key: cfg.api_key.clone(),
        model: cfg.model.clone(),
        effort: None,
    };
    let tools = LocalTools::new(workdir);

    let plan = plan_effort(cfg.effort, &cfg.supported);
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

    match run_task(
        &client,
        &tools,
        &mut log,
        &mut session,
        &mut memory,
        task,
        &run_cfg,
    ) {
        Ok(outcome) => {
            // 成功的任务写入记忆（跨会话可召回）
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

            (
                200,
                json!({
                    "ok": true,
                    "output": outcome.final_text,
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
                    "guard_events": outcome.guard_events,
                }),
            )
        }
        Err(e) => (500, json!({"ok": false, "error": e.to_string()})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_task_is_rejected() {
        let dir = std::env::temp_dir().join(format!("exec-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg = ExecConfig::from_env();
        let (code, body) = execute_task(&dir, &dir, "   ", &cfg);
        assert_eq!(code, 400);
        assert!(body["error"].as_str().unwrap().contains("为空"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exec_config_reads_env_with_defaults() {
        let cfg = ExecConfig::from_env();
        assert!(!cfg.base_url.is_empty());
        assert!(!cfg.model.is_empty());
        assert!(cfg.max_steps > 0);
        assert!(!cfg.supported.is_empty(), "必须声明支持的档位");
    }

    #[test]
    fn pending_blocks_execution() {
        // kiso：未决副作用必须先裁决
        let dir = std::env::temp_dir().join(format!("exec-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        {
            let mut s = DurableSession::open(&dir).unwrap();
            s.begin_effect("risky-op").unwrap();
        }

        let cfg = ExecConfig::from_env();
        let (code, body) = execute_task(&dir, &dir, "做点什么", &cfg);
        assert_eq!(code, 409, "有未决副作用时应拒绝执行");
        assert!(body["pending"]
            .as_array()
            .unwrap()
            .contains(&json!("risky-op")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
