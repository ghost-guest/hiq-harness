//! 真实 agent 运行器：模型 → 工具 → 回填 → 守卫。
//!
//! 这是 L1 循环的实际形态。与 `harness_core::agent_loop` 的区别是
//! 它保留结构化的工具调用链（API 需要），而不是纯文本投影。

use crate::llm::{parse_model_response, OpenAiClient};
use crate::tools::LocalTools;
use harness_core::event::{EventKind, EventLog, ToolOutcome};
use harness_guards::{GuardSet, PostCallVerdict, PreCallVerdict, Signature};
use serde_json::{json, Value};

pub struct RunConfig {
    /// 最大步数，0 表示无上限
    pub max_steps: u32,
    /// 推理强度
    pub effort: Option<String>,
    /// 系统提示
    pub system: String,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_steps: 0,
            effort: Some("high".into()),
            system: "你是一个编程助手。用工具完成用户的任务，不要臆造文件内容。\
                     完成后用一句话说明你做了什么。"
                .into(),
        }
    }
}

pub struct RunOutcome {
    pub steps: u32,
    pub guard_events: Vec<String>,
    pub final_text: String,
    pub total_input_tokens: u64,
}

/// 跑一次真实的 agent 任务。
pub fn run_task(
    client: &OpenAiClient,
    tools: &LocalTools,
    log: &mut EventLog,
    task: &str,
    cfg: &RunConfig,
) -> anyhow::Result<RunOutcome> {
    let mut guards = GuardSet::new();
    let tool_specs = LocalTools::tool_specs();

    // 结构化历史：必须保留 tool_calls 与 tool 结果，否则 API 会拒绝
    let mut history: Vec<Value> = vec![
        json!({"role": "system", "content": cfg.system}),
        json!({"role": "user", "content": task}),
    ];

    log.append(EventKind::UserMessage {
        text: task.to_string(),
    });
    log.commit()?;

    let mut steps = 0u32;
    let mut guard_events = Vec::new();
    let mut total_input = 0u64;

    loop {
        // 未决的 uncertain 副作用 → 必须停
        let events = EventLog::read_all(log.path())?;
        if harness_core::event::Projection::new(&events).has_unresolved_uncertain() {
            return Ok(RunOutcome {
                steps,
                guard_events,
                final_text: "存在未决的副作用，需人工裁决".into(),
                total_input_tokens: total_input,
            });
        }

        if cfg.max_steps > 0 && steps >= cfg.max_steps {
            return Ok(RunOutcome {
                steps,
                guard_events,
                final_text: "已达步数上限".into(),
                total_input_tokens: total_input,
            });
        }

        guards.begin_turn();

        // 一次模型调用
        let body = json!({
            "model": client.model,
            "messages": history,
            "tools": tool_specs,
            "max_tokens": 8192,
            "reasoning": cfg.effort.as_ref().map(|e| json!({"effort": e})),
        });
        let (resp, usage) = client.post_public(&body)?;
        if let Some(p) = usage.prompt_tokens {
            total_input += p;
        }

        let model_resp = parse_model_response(&resp)?;

        match model_resp {
            harness_core::agent_loop::ModelResponse::Text(t) => {
                log.append(EventKind::AssistantMessage {
                    text: t.clone(),
                    blocks: vec![],
                });
                log.commit()?;
                return Ok(RunOutcome {
                    steps,
                    guard_events,
                    final_text: t,
                    total_input_tokens: total_input,
                });
            }
            harness_core::agent_loop::ModelResponse::Length(t) => {
                log.append(EventKind::AssistantMessage {
                    text: t.clone(),
                    blocks: vec![],
                });
                log.commit()?;
                // 截断 → 要求续写
                history.push(json!({"role": "assistant", "content": t}));
                history.push(json!({
                    "role": "user",
                    "content": "上一条回复被长度截断，请从中断处继续，不要重复已说过的内容。"
                }));
                steps += 1;
                continue;
            }
            harness_core::agent_loop::ModelResponse::ToolCalls(calls) => {
                // 把 assistant 的 tool_calls 原样放进历史
                let api_calls: Vec<Value> = calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": serde_json::to_string(&c.input).unwrap_or_else(|_| "{}".into())
                            }
                        })
                    })
                    .collect();
                history.push(json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": api_calls,
                }));

                for req in calls {
                    // 执行前：只读螺旋
                    match guards.before_call(&req.name) {
                        PreCallVerdict::Block { reason } => {
                            guard_events.push(format!("[螺旋] 拦截 {}: {reason}", req.name));
                            log.append(EventKind::ToolResult {
                                call_id: req.id.clone(),
                                outcome: ToolOutcome::Err {
                                    error: format!("[被守卫拦下] {reason}"),
                                },
                            });
                            history.push(json!({
                                "role": "tool",
                                "tool_call_id": req.id,
                                "content": format!("[被守卫拦下] {reason}"),
                            }));
                            log.commit()?;
                            continue;
                        }
                        PreCallVerdict::ProceedWithNote { note } => {
                            guard_events.push(format!("[螺旋] 提示: {note}"));
                        }
                        PreCallVerdict::Proceed => {}
                    }

                    log.append(EventKind::ToolCall {
                        id: req.id.clone(),
                        name: req.name.clone(),
                        input: req.input.clone(),
                    });

                    let res = tools.execute_sync(&req)?;
                    let sig = Signature::new(
                        &req.name,
                        &serde_json::to_string(&req.input).unwrap_or_default(),
                        &res.output,
                        &res.blocks_digest,
                    );

                    let mut out_text = res.output.clone();
                    let mut replaced = false;

                    match guards.after_call(&sig, res.success) {
                        PostCallVerdict::OK => {}
                        PostCallVerdict::Note { note } => {
                            guard_events.push(format!("[守卫] 提示: {note}"));
                            out_text.push_str("\n\n");
                            out_text.push_str(&note);
                        }
                        PostCallVerdict::ReplaceResult { reason } => {
                            guard_events.push(format!("[守卫] 拒跑 {}: {reason}", req.name));
                            out_text = format!("[未执行] {reason}");
                            replaced = true;
                        }
                        PostCallVerdict::Stop { reason } => {
                            guard_events.push(format!("[守卫] 终止: {reason}"));
                            log.append(EventKind::ToolResult {
                                call_id: req.id.clone(),
                                outcome: ToolOutcome::Err {
                                    error: reason.clone(),
                                },
                            });
                            log.commit()?;
                            return Ok(RunOutcome {
                                steps,
                                guard_events,
                                final_text: reason,
                                total_input_tokens: total_input,
                            });
                        }
                    }

                    log.append(EventKind::ToolResult {
                        call_id: req.id.clone(),
                        outcome: if res.success && !replaced {
                            ToolOutcome::Ok {
                                output: out_text.clone(),
                            }
                        } else {
                            ToolOutcome::Err {
                                error: out_text.clone(),
                            }
                        },
                    });
                    log.commit()?;

                    history.push(json!({
                        "role": "tool",
                        "tool_call_id": req.id,
                        "content": out_text,
                    }));
                }
                steps += 1;
            }
        }
    }
}
