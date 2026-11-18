//! 真实 agent 运行器：模型 → 工具 → 回填 → 守卫。
//!
//! 这是 L1 循环的实际形态。与 `harness_core::agent_loop` 的区别是
//! 它保留结构化的工具调用链（API 需要），而不是纯文本投影。

use crate::durable::{CallId, DurableSession, EffectOutcome, ExecutionId, InvocationSeq};
use crate::llm::OpenAiClient;
use crate::memory::ProjectMemory;
use crate::tools::LocalTools;
use harness_context::cache::{CacheStats, HeaderFingerprint};
use harness_context::effort::{plan_effort, Effort, EffortPlan};
use harness_context::ToolResultTier;
use harness_core::event::{EventKind, EventLog, ToolOutcome};
use harness_guards::{GuardSet, PostCallVerdict, PreCallVerdict, Signature};
use serde_json::{json, Value};

pub struct RunConfig {
    /// 最大步数，0 表示无上限
    pub max_steps: u32,
    /// 请求的推理强度。会按 provider 能力映射后再发。
    pub effort: Option<Effort>,
    /// provider 实际支持的档位。空表示未知，原样透传。
    pub supported_efforts: Vec<Effort>,
    /// 系统提示
    pub system: String,
    /// 会话里已有的历史消息（不含 system）。
    ///
    /// 按 OpenAI 的 chat 格式给：`[{"role":"user","content":"…"}, …]`。
    /// **必须只追加、不改写**——前缀一变，服务端的 KV 缓存就整段失效
    /// （DeepSeek 的缓存以「完整前缀单元」为单位匹配）。所以这里拿到的
    /// 历史会原样拼在 system 之后、当前 task 之前，不做任何重排或去重。
    pub history: Vec<Value>,
    /// 摘要阈值：工具结果降到「摘要」档的字符数。
    /// 来自 Deva 的三段式：Full → Summary → DiskRef。
    pub summary_threshold: usize,
    /// 工具结果降到「磁盘引用」档的阈值（字符数）。
    pub disk_threshold: usize,
    /// 落盘目录。降级到 DiskRef 时原文存这里，模型可再读回来。
    pub spill_dir: std::path::PathBuf,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_steps: 0,
            effort: Some(Effort::High),
            supported_efforts: Vec::new(),
            history: Vec::new(),
            system: "你是一个编程助手。用工具完成用户的任务，不要臆造文件内容。\
                     完成后用一句话说明你做了什么。"
                .into(),
            // Deva 实测：802 个工具结果里 21% 降到第三档。
            // 阈值按「一行摘要约 100 字符、磁盘引用约 150 字符」倒推，
            // 取能让大文件读取（数千字符）降级、小结果保留全文的值。
            summary_threshold: 2_000,
            disk_threshold: 6_000,
            spill_dir: std::path::PathBuf::from(".harness/tool-results"),
        }
    }
}

pub struct RunOutcome {
    pub steps: u32,
    pub guard_events: Vec<String>,
    pub final_text: String,
    /// 缓存统计（含逐轮命中率、稳态命中率）
    pub cache: CacheStats,
    /// 档位映射结果，便于确认「请求了什么、实际发了什么」
    pub effort: Option<harness_context::effort::EffortPlan>,
}

/// 跑一次真实的 agent 任务。
/// 运行过程中的事件。流式输出用。
///
/// 全部来自已有的执行流程，不新增逻辑 —— 只是把内部状态外化。
#[derive(Debug, Clone)]
pub enum RunEvent {
    /// 开始了第 n 步
    StepStarted { step: u32 },
    /// 模型决定调用工具
    ToolCalling { name: String, summary: String },
    /// 工具返回
    ToolDone {
        name: String,
        ok: bool,
        chars: usize,
    },
    /// 守卫介入
    GuardNote { note: String },
    /// 模型给出最终文本
    FinalText { text: String },
    /// 流式增量文本（模型逐段返回，界面边收边显示）。
    ///
    /// 与 `FinalText` 的区别：这条是补充显示用，最终落盘仍以 `FinalText`
    /// 为准。两者都会出现（增量在前、最终文本在后）。
    TextDelta { text: String },
    /// 思考/中间文本。预留给 token 级流式（模型侧逐段返回时用）。
    #[allow(dead_code)]
    Thinking { text: String },
}

/// 事件回调。返回 false 表示中止。
pub type EventSink<'a> = &'a mut dyn FnMut(RunEvent);

/// 把会话历史里的一条消息转成可发出去的 chat 消息。
///
/// 会话文件里混着会话头、工具块、空内容等，只有带 `role` 且内容非空的
/// 才适合回填。`assistant` 的空回复也要丢掉 —— 发一个空 content 的
/// assistant 消息会让部分服务端直接 400。
fn sanitize_history_message(v: &Value) -> Option<Value> {
    let role = v.get("role")?.as_str()?;
    if !matches!(role, "user" | "assistant" | "system") {
        return None;
    }
    let content = v.get("content")?;
    let empty = match content {
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Null => true,
        _ => false,
    };
    if empty {
        return None;
    }
    Some(json!({ "role": role, "content": content }))
}

pub fn run_task(
    client: &OpenAiClient,
    tools: &LocalTools,
    log: &mut EventLog,
    session: &mut DurableSession,
    memory: &mut ProjectMemory,
    task: &str,
    cfg: &RunConfig,
    sink: EventSink<'_>,
) -> anyhow::Result<RunOutcome> {
    let mut guards = GuardSet::new();
    let tool_specs = LocalTools::tool_specs();

    // 开场召回相关记忆并注入。
    // 放在 system 之后、user 之前 —— 这样前缀仍然只增不改。
    let recalled = memory.recall_block(task, 5);
    let mut history_msgs: Vec<Value> = vec![json!({"role": "system", "content": cfg.system})];
    if let Some(block) = &recalled {
        history_msgs.push(json!({"role": "system", "content": block}));
    }
    // 会话历史：原样接在 system 之后、本次 task 之前。
    // 只追加、不改写 —— 服务端的 KV 缓存按「完整前缀单元」匹配，
    // 任何重排都会让前面整段缓存作废（实测：无历史时命中率 0%，
    // 同前缀连打三次可到 94.2%）。
    history_msgs.extend(cfg.history.iter().filter_map(sanitize_history_message));
    history_msgs.push(json!({"role": "user", "content": task}));

    log.append(EventKind::UserMessage {
        text: task.to_string(),
    });
    log.commit()?;

    let mut steps = 0u32;
    // 序号从会话已有的最大值继续（kiso 的 ex-<seq> 不跨运行重置）
    let invocation_base = session.next_invocation_seq();
    let mut invocation_in_step;
    let mut guard_events = Vec::new();
    let mut cache_stats = CacheStats::new();

    // 档位映射只算一次：请求了什么、实际发什么，都要可观测
    let effort_plan: Option<EffortPlan> =
        cfg.effort.map(|e| plan_effort(e, &cfg.supported_efforts));
    let effective_effort = effort_plan.as_ref().map(|p| p.effective);

    loop {
        // 未决的 uncertain 副作用 → 必须停。
        // 用持久化账本判断，崩溃恢复后也能拦住。
        let events = EventLog::read_all(log.path())?;
        let pending = session.pending_uncertain();
        if !pending.is_empty()
            || harness_core::event::Projection::new(&events).has_unresolved_uncertain()
        {
            let notice = format!("存在未决的副作用，需人工裁决：{}", pending.join(", "));
            // 必须把停止原因发出去：不发的话界面上只剩工具卡片，
            // 用户看不到「为什么停了」（实测就是这样：整轮无 text 事件）。
            sink(RunEvent::FinalText {
                text: notice.clone(),
            });
            log.append(EventKind::AssistantMessage {
                text: notice.clone(),
                blocks: vec![],
            });
            log.commit()?;
            return Ok(RunOutcome {
                steps,
                guard_events,
                final_text: notice,
                cache: cache_stats.clone(),
                effort: effort_plan.clone(),
            });
        }

        if cfg.max_steps > 0 && steps >= cfg.max_steps {
            // 同理：跑满步数也要告诉用户，否则界面一片空白。
            let notice = format!("已达步数上限（{} 步），任务未完成。", cfg.max_steps);
            sink(RunEvent::FinalText {
                text: notice.clone(),
            });
            log.append(EventKind::AssistantMessage {
                text: notice.clone(),
                blocks: vec![],
            });
            log.commit()?;
            return Ok(RunOutcome {
                steps,
                guard_events,
                final_text: notice,
                cache: cache_stats.clone(),
                effort: effort_plan.clone(),
            });
        }

        guards.begin_turn();
        invocation_in_step = 0u64;
        sink(RunEvent::StepStarted { step: steps });

        // 一次模型调用（真流式）。
        //
        // `reasoning` 由 transport 的 `with_effort` 注入：不带时完全省略，
        // 不发出 "reasoning": null —— 多余字段会改变请求字节、破坏前缀缓存。
        //
        // 正文增量逐段推给界面，不等整轮结束 —— 这是原先
        // curl + .output() 做不到的（必须等进程退出才能拿到任何字节）。
        //
        // 边发边攒：这段正文是模型本步的「思考痕迹」，下一步必须原样
        // 回灌进历史。只推给界面、不记历史的话，模型看不到自己上一步说过
        // 什么，会换个说法把同一句话再讲一遍（实测：同一句出现两遍）。
        let mut streamed_text = String::new();
        let done = client.complete_with_tools(
            history_msgs.clone(),
            tool_specs.clone(),
            8192,
            |delta| {
                streamed_text.push_str(delta);
                sink(RunEvent::TextDelta {
                    text: delta.to_string(),
                });
            },
        )?;

        let usage = done.usage;
        let p_tok = usage.prompt_tokens.unwrap_or(0);
        let c_tok = usage.cached_tokens.unwrap_or(0);

        // 记录 header 指纹，用于归因 miss。
        // Codewhale 铁律：miss 必须能说出理由。
        let header_fp = HeaderFingerprint::new(
            &cfg.system,
            &serde_json::to_string(&tool_specs).unwrap_or_default(),
            effective_effort.map(|e| e.as_str()),
        );
        let miss = cache_stats.record(steps, p_tok, c_tok, header_fp);

        if std::env::var("HARNESS_TRACE_CACHE").is_ok() {
            let rate = if p_tok > 0 {
                c_tok as f64 / p_tok as f64 * 100.0
            } else {
                0.0
            };
            let why = miss
                .as_ref()
                .map(|m| format!("  ← {}", m.describe()))
                .unwrap_or_default();
            eprintln!(
                "[cache] step={} prompt={} cached={} 命中={:.1}%{}",
                steps, p_tok, c_tok, rate, why
            );
        }

        // 传输层已经给出结构化结果，不需要再解析原始 JSON。
        let model_resp = if !done.tool_calls.is_empty() {
            harness_core::agent_loop::ModelResponse::ToolCalls(done.tool_calls.clone())
        } else {
            let text = done.text.clone().unwrap_or_default();
            if done.stop_reason.as_deref() == Some("Length") {
                harness_core::agent_loop::ModelResponse::Length(text)
            } else {
                harness_core::agent_loop::ModelResponse::Text(text)
            }
        };

        match model_resp {
            harness_core::agent_loop::ModelResponse::Text(t) => {
                sink(RunEvent::FinalText { text: t.clone() });
                log.append(EventKind::AssistantMessage {
                    text: t.clone(),
                    blocks: vec![],
                });
                log.commit()?;
                return Ok(RunOutcome {
                    steps,
                    guard_events,
                    final_text: t,
                    cache: cache_stats.clone(),
                    effort: effort_plan.clone(),
                });
            }
            harness_core::agent_loop::ModelResponse::Length(t) => {
                log.append(EventKind::AssistantMessage {
                    text: t.clone(),
                    blocks: vec![],
                });
                log.commit()?;
                // 截断 → 要求续写
                history_msgs.push(json!({"role": "assistant", "content": t}));
                history_msgs.push(json!({
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
                // assistant 消息带上本步已流出的正文（可能为空）。
                // 空正文时仍要发 `null` —— 带 tool_calls 的 assistant 消息
                // 允许 content 为空，但把空串发出去会多占字节、改变前缀。
                history_msgs.push(json!({
                    "role": "assistant",
                    "content": if streamed_text.is_empty() {
                        Value::Null
                    } else {
                        Value::String(streamed_text.clone())
                    },
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
                            history_msgs.push(json!({
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

                    // kiso 的 Stable Intent Identity：
                    // CallId 只对应 tool_call/tool_result，ExecutionId 用于副作用去重。
                    let call_id = CallId(req.id.clone());
                    let invocation =
                        InvocationSeq(invocation_base + steps as u64 * 1000 + invocation_in_step);
                    invocation_in_step += 1;
                    let exec_id = ExecutionId::derive(invocation);

                    sink(RunEvent::ToolCalling {
                        name: req.name.clone(),
                        summary: req.input.to_string().chars().take(60).collect(),
                    });

                    log.append(EventKind::ToolCall {
                        id: call_id.0.clone(),
                        name: req.name.clone(),
                        input: req.input.clone(),
                    });

                    // 按 kiso 契约执行：先落盘 STARTED 收据，再跑工具。
                    // 崩溃在中间 → 收据停在 Uncertain，恢复时必须人裁决。

                    let res = match session.run_effect(
                        &exec_id,
                        true,
                        || tools.execute_sync(&req).map_err(|e| e.to_string()),
                        || None,
                    ) {
                        EffectOutcome::Completed(r) => r,
                        EffectOutcome::Replayed(r) => r,
                        EffectOutcome::FailedRetryable(e) | EffectOutcome::Failed(e) => {
                            anyhow::bail!("工具执行失败: {e}")
                        }
                        EffectOutcome::Blocked { reason } => {
                            anyhow::bail!("被持久化契约拦住: {reason}")
                        }
                    };
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
                            sink(RunEvent::GuardNote { note: note.clone() });
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
                                cache: cache_stats.clone(),
                                effort: effort_plan.clone(),
                            });
                        }
                    }

                    // 三段式降级（Deva）：Full → Summary → DiskRef。
                    // 超过 disk_threshold 的落盘，超过 summary_threshold 的降成一行摘要。
                    let orig_chars = out_text.chars().count();
                    if orig_chars > cfg.disk_threshold {
                        let path = spill_to_disk(&cfg.spill_dir, &out_text)?;
                        let tier = ToolResultTier::DiskRef {
                            path,
                            bytes: orig_chars,
                        };
                        let _ = &tier;
                        out_text = tier.text_for_model();
                    } else if orig_chars > cfg.summary_threshold {
                        let tier = ToolResultTier::Summary {
                            text: first_line_summary(&out_text),
                        };
                        out_text = tier.text_for_model();
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

                    sink(RunEvent::ToolDone {
                        name: req.name.clone(),
                        ok: res.success && !replaced,
                        chars: out_text.chars().count(),
                    });

                    history_msgs.push(json!({
                        "role": "tool",
                        "tool_call_id": req.id,
                        "content": out_text,
                    }));
                }
                // 每轮结束保存检查点：崩溃后可从最近检查点续跑。
                // 检查点带上当前记忆版本，恢复时若记忆失效会拒绝交出 state。
                steps += 1;
                let _ = session.save_checkpoint(
                    log.next_seq_hint(),
                    &format!("step {steps} 完成，历史 {} 条消息", history_msgs.len()),
                    Vec::new(),
                );
            }
        }
    }
}

/// 把工具结果原文落盘，返回路径。
///
/// 用**内容哈希**命名而不是 call_id —— 同一个文件被读多次时
/// 只存一份。这是 Deva 的做法（`checkpoints/<sid>/` 下的文件
/// 以内容哈希命名，同一产出物只存一份）。
fn spill_to_disk(dir: &std::path::Path, text: &str) -> anyhow::Result<String> {
    std::fs::create_dir_all(dir)?;
    let digest = content_digest(text);
    let path = dir.join(format!("{digest}.txt"));
    // 已存在就不重复写
    if !path.exists() {
        std::fs::write(&path, text)?;
    }
    Ok(path.display().to_string())
}

/// FNV-1a 内容指纹。
fn content_digest(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// 一行摘要。取首行并截断。
///
/// Deva 的 summaries 就是这种形态：要么是内容首行，
/// 要么是「N 字符」这样的占位。
fn first_line_summary(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return format!("{} 字符", text.chars().count());
    }
    if line.chars().count() > 100 {
        let t: String = line.chars().take(100).collect();
        format!("{t}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_history_message;
    use serde_json::json;

    /// 会话历史必须能原样拼进请求。
    ///
    /// 这是「记得上文」与「缓存命中」的共同前提：没有历史时每轮只有
    /// 几十 token 的前缀，服务端缓存单元根本不会命中（实测 0%；
    /// 同前缀连打三次可到 94.2%）。
    #[test]
    fn history_messages_survive_sanitizing() {
        let user = json!({"role": "user", "content": "第一句"});
        let assistant = json!({"role": "assistant", "content": "第一答"});
        for m in [&user, &assistant] {
            let out = sanitize_history_message(m).expect("应保留");
            assert_eq!(out["role"], m["role"]);
            assert_eq!(out["content"], m["content"]);
        }
    }

    /// 空回复、无 role、tool 块都不能发出去。
    ///
    /// 发一条 `content: ""` 的 assistant 消息，部分服务端直接 400；
    /// 这也是会话文件里那些半成品条目最容易造成的问题。
    #[test]
    fn broken_history_entries_are_dropped() {
        let cases = [
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": "   "}),
            json!({"role": "assistant", "content": []}),
            json!({"role": "assistant", "content": null}),
            json!({"content": "没有 role"}),
            json!({"role": "tool", "content": "工具结果"}),
            json!({"role": "system"}),
        ];
        for c in cases {
            assert!(
                sanitize_history_message(&c).is_none(),
                "不该发出去：{c}"
            );
        }
    }

    /// sanitize 不能改动内容字节 —— 前缀一变缓存全失效。
    #[test]
    fn sanitizing_preserves_content_bytes() {
        let text = "含中文与 emoji 的回复 🎯，不能删改任何字符";
        let m = json!({"role": "assistant", "content": text});
        let out = sanitize_history_message(&m).unwrap();
        assert_eq!(out["content"].as_str().unwrap(), text);
    }
}
