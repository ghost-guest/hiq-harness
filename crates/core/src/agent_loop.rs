//! L1 主循环：模型 → 工具 → 回填 → 退出。
//!
//! 设计要点（来自调研）：
//! - 不设硬上限：步数不是进展的度量，终止靠守卫
//! - 末轮禁工具但保留工具定义，只为不打碎前缀缓存
//! - uncertain 的副作用必须阻塞，绝不静默重试

use crate::event::{EventKind, EventLog, Projection, ToolOutcome};
use harness_guards::{GuardSet, PostCallVerdict, PreCallVerdict, Signature};

/// 模型的一次响应。
#[derive(Debug, Clone)]
pub enum ModelResponse {
    /// 纯文本，结束这一轮
    Text(String),
    /// 要调工具
    ToolCalls(Vec<ToolRequest>),
    /// 输出被长度截断
    Length(String),
}

#[derive(Debug, Clone)]
pub struct ToolRequest {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub output: String,
    pub success: bool,
    /// 结果里有没有 blocks（图片/渲染）。
    /// 指纹必须带上它，否则「重绘图片再检查」会被判死循环。
    pub blocks_digest: String,
}

/// 模型接口。实际实现接 OpenAI 兼容协议。
#[allow(async_fn_in_trait)]
pub trait ModelClient {
    async fn call(&self, messages: &[String]) -> anyhow::Result<ModelResponse>;
}

/// 工具执行接口。
#[allow(async_fn_in_trait)]
pub trait ToolExecutor {
    async fn execute(&self, req: &ToolRequest) -> anyhow::Result<ToolResult>;
}

/// 一轮的最大步数。0 表示无上限（推荐）。
#[derive(Debug, Clone, Copy)]
pub struct LoopConfig {
    pub max_steps: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        // 不设硬上限：靠守卫判断进展
        Self { max_steps: 0 }
    }
}

pub struct AgentLoop<'a> {
    log: &'a mut EventLog,
    guards: GuardSet,
    cfg: LoopConfig,
}

impl<'a> AgentLoop<'a> {
    pub fn new(log: &'a mut EventLog, cfg: LoopConfig) -> Self {
        Self {
            log,
            guards: GuardSet::new(),
            cfg,
        }
    }

    pub async fn run(
        &mut self,
        input: &str,
        model: &impl ModelClient,
        tools: &impl ToolExecutor,
    ) -> anyhow::Result<RunOutcome> {
        self.log.append(EventKind::UserMessage {
            text: input.to_string(),
        });
        self.log.commit()?;

        let mut step: u32 = 0;
        loop {
            // 存在未决的 uncertain 副作用 → 必须停下来等人
            let events = EventLog::read_all(self.log.path())?;
            if Projection::new(&events).has_unresolved_uncertain() {
                return Ok(RunOutcome::BlockedOnUncertain);
            }

            // 到上限（若设了）：禁工具但保留定义
            let at_limit = self.cfg.max_steps > 0 && step >= self.cfg.max_steps;

            self.guards.begin_turn();
            let messages = self.project_messages()?;
            let resp = model.call(&messages).await?;

            match resp {
                ModelResponse::Text(t) => {
                    self.log.append(EventKind::AssistantMessage {
                        text: t,
                        blocks: vec![],
                    });
                    self.log.commit()?;
                    return Ok(RunOutcome::Completed);
                }
                ModelResponse::Length(t) => {
                    // 截断续写
                    self.log.append(EventKind::AssistantMessage {
                        text: t,
                        blocks: vec![],
                    });
                    self.log.commit()?;
                }
                ModelResponse::ToolCalls(calls) => {
                    if at_limit {
                        // 到上限：不接受新的工具链，要求收尾
                        self.log.append(EventKind::AssistantMessage {
                            text: "[步数已达上限，请基于现有证据给出最终答复]".into(),
                            blocks: vec![],
                        });
                        self.log.commit()?;
                        return Ok(RunOutcome::MaxSteps);
                    }

                    for req in calls {
                        // 执行前：只读螺旋
                        match self.guards.before_call(&req.name) {
                            PreCallVerdict::Block { reason } => {
                                self.log.append(EventKind::ToolResult {
                                    call_id: req.id.clone(),
                                    outcome: ToolOutcome::Err {
                                        error: format!("[被守卫拦下] {reason}"),
                                    },
                                });
                                continue;
                            }
                            PreCallVerdict::ProceedWithNote { note } => {
                                tracing::info!("{note}");
                            }
                            PreCallVerdict::Proceed => {}
                        }

                        self.log.append(EventKind::ToolCall {
                            id: req.id.clone(),
                            name: req.name.clone(),
                            input: req.input.clone(),
                        });

                        let res = tools.execute(&req).await?;
                        let sig = Signature::new(
                            &req.name,
                            &serde_json::to_string(&req.input).unwrap_or_default(),
                            &res.output,
                            &res.blocks_digest,
                        );

                        // 执行后：熔断 + 无进展
                        match self.guards.after_call(&sig, res.success) {
                            PostCallVerdict::OK => {
                                self.log.append(EventKind::ToolResult {
                                    call_id: req.id.clone(),
                                    outcome: if res.success {
                                        ToolOutcome::Ok { output: res.output }
                                    } else {
                                        ToolOutcome::Err { error: res.output }
                                    },
                                });
                            }
                            PostCallVerdict::Note { note } => {
                                // 提示追加在结果尾部，不改变成功语义
                                let mut out = res.output;
                                out.push_str("\n\n");
                                out.push_str(&note);
                                self.log.append(EventKind::ToolResult {
                                    call_id: req.id.clone(),
                                    outcome: if res.success {
                                        ToolOutcome::Ok { output: out }
                                    } else {
                                        ToolOutcome::Err { error: out }
                                    },
                                });
                            }
                            PostCallVerdict::ReplaceResult { reason } => {
                                // 拒跑：用拒绝文案替换结果，模型读到 error 而非 success
                                self.log.append(EventKind::ToolResult {
                                    call_id: req.id.clone(),
                                    outcome: ToolOutcome::Err {
                                        error: format!("[未执行] {reason}"),
                                    },
                                });
                            }
                            PostCallVerdict::Stop { reason } => {
                                self.log.append(EventKind::ToolResult {
                                    call_id: req.id.clone(),
                                    outcome: ToolOutcome::Err {
                                        error: reason.clone(),
                                    },
                                });
                                self.log.commit()?;
                                return Ok(RunOutcome::StoppedByGuard(reason));
                            }
                        }
                        self.log.commit()?;
                    }
                    step += 1;
                }
            }
        }
    }

    fn project_messages(&self) -> anyhow::Result<Vec<String>> {
        let events = EventLog::read_all(self.log.path())?;
        Ok(Projection::new(&events)
            .messages()
            .into_iter()
            .map(|m| m.text)
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    MaxSteps,
    StoppedByGuard(String),
    BlockedOnUncertain,
}
