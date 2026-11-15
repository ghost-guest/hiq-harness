//! SOP 编排的真实执行器。
//!
//! 把 `harness_orchestrate` 的 DAG 调度接到真实 agent 循环上。
//!
//! 设计全部来自 LoopForge：
//! - **分支一满足依赖就跑**，不等同批最慢的（`node_execute.go:58`）
//! - **Script 节点不调模型**：「No AI where AI is not needed」
//! - **人工网关暂停等人**（`HumanGate`）
//! - **前置能力在运行前检查**（`WorkflowRequirement`）
//! - **运行前冻结工作流快照**（`workflow_graph.go:41-47`）

use crate::durable::DurableSession;
use crate::llm::OpenAiClient;
use crate::memory::ProjectMemory;
use crate::runner::{run_task, RunConfig};
use crate::tools::LocalTools;
use harness_core::event::EventLog;
use harness_orchestrate::{
    check_requirements, next_batch, FailureAction, NodeKind, RunState, Schedule, Workflow,
};
use std::collections::HashSet;
use std::path::Path;

/// 一次 SOP 执行的结果。
pub struct SopOutcome {
    pub run_id: String,
    pub succeeded: usize,
    pub failed: usize,
    pub reworks: u32,
    pub log: Vec<String>,
}

/// 执行一个 SOP。
///
/// `available` 是已具备的外部能力 key，用于前置检查。
pub fn run_sop(
    wf: &Workflow,
    available: &HashSet<String>,
    client: &OpenAiClient,
    tools: &LocalTools,
    log: &mut EventLog,
    session: &mut DurableSession,
    memory: &mut ProjectMemory,
    workdir: &Path,
) -> anyhow::Result<SopOutcome> {
    let mut trace = Vec::new();

    // 前置能力检查：缺失在**首次运行前**暴露，不在流水线中途失败
    let missing = check_requirements(wf, available);
    if !missing.is_empty() {
        let keys: Vec<&str> = missing.iter().map(|r| r.key.as_str()).collect();
        anyhow::bail!(
            "缺少前置能力：{}。请在运行前配置，而不是让流水线中途失败。",
            keys.join(", ")
        );
    }

    // 冻结快照（RunState::start 内部已克隆）
    let mut state = RunState::start(wf, &format!("sop-{}", wf.id))?;
    trace.push(format!("开始 {}（{} 个节点）", wf.name, wf.nodes.len()));

    loop {
        match next_batch(&state) {
            Schedule::Done => {
                trace.push("全部节点完成".into());
                break;
            }
            Schedule::Failed => {
                trace.push("调度失败：无可执行节点".into());
                break;
            }
            Schedule::WaitingHuman(ids) => {
                // 真实场景该阻塞等人；这里如实报告并停下
                trace.push(format!("等待人工裁决：{}", ids.join(", ")));
                break;
            }
            Schedule::Start(ids) => {
                for id in ids {
                    let node = state
                        .snapshot
                        .node(&id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("未知节点 {id}"))?;

                    state.begin_node(&id)?;
                    trace.push(format!("▶ {}（{}）", node.name, node.kind.label()));

                    let result = execute_node(
                        &node.kind, &node.name, client, tools, log, session, memory, workdir,
                    );

                    match result {
                        Ok(()) => {
                            state.succeed_node(&id);
                            trace.push(format!("✓ {}", node.name));
                        }
                        Err(e) => {
                            let action = state.fail_node(&id, &e.to_string())?;
                            trace.push(format!("✗ {}：{e}", node.name));
                            match action {
                                FailureAction::Abort => {
                                    trace.push("按策略终止".into());
                                    return Ok(finish(state, trace));
                                }
                                FailureAction::RetrySelf => {
                                    trace.push("按策略重试本节点".into());
                                }
                                FailureAction::Reject { to } => {
                                    trace.push(format!("按策略打回 {to}"));
                                }
                                FailureAction::AskHuman => {
                                    trace.push("按策略交人".into());
                                    return Ok(finish(state, trace));
                                }
                                FailureAction::Continue => {}
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(finish(state, trace))
}

fn finish(state: RunState, log: Vec<String>) -> SopOutcome {
    SopOutcome {
        run_id: state.id.clone(),
        succeeded: state.succeeded_count(),
        failed: state
            .nodes
            .values()
            .filter(|n| n.status == harness_orchestrate::NodeStatus::Failed)
            .count(),
        reworks: state.total_reworks,
        log,
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_node(
    kind: &NodeKind,
    name: &str,
    client: &OpenAiClient,
    tools: &LocalTools,
    log: &mut EventLog,
    session: &mut DurableSession,
    memory: &mut ProjectMemory,
    workdir: &Path,
) -> anyhow::Result<()> {
    match kind {
        // Script：确定性步骤，不调模型。
        // LoopForge 的原话：「No AI where AI is not needed」
        NodeKind::Script { command } | NodeKind::PreviewBuild { command } => {
            let out = std::process::Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(workdir)
                .output()?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                anyhow::bail!("脚本失败（退出码 {:?}）：{}", out.status.code(), err.trim());
            }
            Ok(())
        }

        // Agent：跑一个完整的 agent 回合
        NodeKind::Agent { role } => {
            let task = format!("作为 {role}，完成节点「{name}」。");
            let cfg = RunConfig {
                max_steps: 8,
                effort: Some(harness_context::effort::Effort::High),
                supported_efforts: vec![
                    harness_context::effort::Effort::Low,
                    harness_context::effort::Effort::High,
                    harness_context::effort::Effort::Max,
                ],
                system: format!("你是 {role}。完成分配给你的节点任务。"),
                ..Default::default()
            };
            let outcome = run_task(client, tools, log, session, memory, &task, &cfg)?;
            if outcome.steps == 0 {
                anyhow::bail!("agent 未执行任何步骤");
            }
            Ok(())
        }

        // 人工网关：真实场景该阻塞。这里返回需要人处理。
        NodeKind::HumanGate { ask } => {
            anyhow::bail!("需要人工确认：{ask}")
        }

        // 子流程：真实实现应递归调用 run_sop
        NodeKind::SubSop { workflow_id } => {
            anyhow::bail!("子流程 {workflow_id} 未内联，需外层展开")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_orchestrate::Node;

    fn wf() -> Workflow {
        Workflow::new("t", "测试流程")
            .add_node(Node::script("build", "构建", "echo build-ok"))
            .add_node(Node::script("test", "测试", "echo test-ok"))
            .add_edge("build", "test")
    }

    #[test]
    fn missing_requirement_fails_before_run() {
        let wf = wf().add_requirement("git-token", "Git Token", "配置它");
        let available: HashSet<String> = HashSet::new();
        let missing = check_requirements(&wf, &available);
        assert_eq!(missing.len(), 1);
        // run_sop 会在这里 bail，不会进入执行
    }

    #[test]
    fn satisfied_requirement_passes_check() {
        let wf = wf().add_requirement("git-token", "Git Token", "配置它");
        let mut available = HashSet::new();
        available.insert("git-token".to_string());
        assert!(check_requirements(&wf, &available).is_empty());
    }

    #[test]
    fn script_node_is_deterministic() {
        // Script 节点不调模型 —— LoopForge 的「No AI where AI is not needed」
        let n = Node::script("s", "步骤", "echo hi");
        assert!(n.kind.is_deterministic());
        let a = Node::agent("a", "角色", "coder");
        assert!(!a.kind.is_deterministic());
    }

    #[test]
    fn schedule_returns_parallel_branches() {
        // 分支一满足依赖就跑，不等同批最慢
        let w = Workflow::new("p", "并行")
            .add_node(Node::script("root", "根", "echo"))
            .add_node(Node::script("a", "a", "echo a"))
            .add_node(Node::script("b", "b", "echo b"))
            .add_edge("root", "a")
            .add_edge("root", "b");
        let mut st = RunState::start(&w, "r").unwrap();
        st.succeed_node("root");
        match next_batch(&st) {
            Schedule::Start(ids) => assert_eq!(ids.len(), 2, "两个分支应同时就绪"),
            other => panic!("实际 {other:?}"),
        }
    }

    #[test]
    fn human_gate_reports_waiting() {
        let mut n = Node::human_gate("gate", "评审", "检查改动");
        n.on_failure = Some(harness_orchestrate::OnFailure::Human);
        let w = Workflow::new("h", "h").add_node(n);
        let mut st = RunState::start(&w, "r").unwrap();
        st.begin_node("gate").unwrap();
        st.fail_node("gate", "需要人看").unwrap();
        assert!(matches!(next_batch(&st), Schedule::WaitingHuman(_)));
    }
}
