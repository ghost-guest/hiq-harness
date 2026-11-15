//! L6 编排：SOP 五节点 DAG。
//!
//! 设计来自 LoopForge：
//! - 五种节点：Agent / Script / HumanGate / SubSop / PreviewBuild
//! - 分支一满足依赖就跑，**不等同批最慢的**
//! - 就绪判定：节点 pending 且所有上游 succeeded
//! - 前置能力声明：缺失在首次运行前暴露，不在流水线中途失败
//! - 运行前冻结工作流快照
//! - Script 节点「No AI where AI is not needed」

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

pub type NodeId = String;

// ─────────────────────────────────────────────────────────────
// 节点
// ─────────────────────────────────────────────────────────────

/// 五种节点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// 角色：提示词 + 执行器 + 模型 + 规则
    Agent { role: String },
    /// 确定性 shell 步骤。不需要 AI 的地方就不要 AI。
    Script { command: String },
    /// 人工网关：暂停等人批准或打回
    HumanGate { ask: String },
    /// 子工作流
    SubSop { workflow_id: String },
    /// 构建并归档供审阅
    PreviewBuild { command: String },
}

impl NodeKind {
    /// 是否确定性（不需要模型参与）。
    pub fn is_deterministic(&self) -> bool {
        matches!(self, Self::Script { .. } | Self::PreviewBuild { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Agent { .. } => "agent",
            Self::Script { .. } => "script",
            Self::HumanGate { .. } => "human_gate",
            Self::SubSop { .. } => "sub_sop",
            Self::PreviewBuild { .. } => "preview_build",
        }
    }
}

/// 失败后的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    /// 循环重试当前节点
    Loop,
    /// 打回上游
    Reject,
    /// 失败终止整个 run
    Fail,
    /// 交人决定
    Human,
}

/// 返工策略。两种质量不同的形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryStrategy {
    /// 定向修：只改坏的部分
    TargetedFix,
    /// 全重做
    FullRedo,
}

/// 一个节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub name: String,
    pub kind: NodeKind,
    /// 失败策略
    pub on_failure: Option<OnFailure>,
    /// 打回到哪个节点
    pub reject_to: Option<NodeId>,
    /// 返工策略
    pub retry: Option<RetryStrategy>,
    /// 打回时携带哪些上下文
    pub carry_over: Vec<String>,
    pub max_attempts: u32,
    pub max_reworks: u32,
}

impl Node {
    pub fn agent(id: &str, name: &str, role: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: NodeKind::Agent { role: role.into() },
            on_failure: None,
            reject_to: None,
            retry: None,
            carry_over: vec![],
            max_attempts: 3,
            max_reworks: 2,
        }
    }

    pub fn script(id: &str, name: &str, command: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: NodeKind::Script {
                command: command.into(),
            },
            on_failure: None,
            reject_to: None,
            retry: None,
            carry_over: vec![],
            max_attempts: 2,
            max_reworks: 0,
        }
    }

    pub fn human_gate(id: &str, name: &str, ask: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: NodeKind::HumanGate { ask: ask.into() },
            on_failure: None,
            reject_to: None,
            retry: None,
            carry_over: vec![],
            max_attempts: 1,
            max_reworks: 1,
        }
    }
}

/// 边。只有 success 一种条件（与 LoopForge 一致）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
}

/// 前置能力声明。
///
/// 缺失的 Git Token / MCP 配置在**首次运行前**暴露，
/// 而不是在流水线中途失败。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub key: String,
    pub label: String,
    pub hint: String,
}

/// 产物合同。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactContract {
    pub name: String,
    pub path: String,
    pub required: bool,
}

/// 工作流定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub name: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub requirements: Vec<Requirement>,
}

impl Workflow {
    pub fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            nodes: vec![],
            edges: vec![],
            requirements: vec![],
        }
    }

    pub fn add_node(mut self, n: Node) -> Self {
        self.nodes.push(n);
        self
    }

    pub fn add_edge(mut self, from: &str, to: &str) -> Self {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
        });
        self
    }

    pub fn add_requirement(mut self, key: &str, label: &str, hint: &str) -> Self {
        self.requirements.push(Requirement {
            key: key.into(),
            label: label.into(),
            hint: hint.into(),
        });
        self
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// 每个节点的上游集合。
    fn deps_map(&self) -> HashMap<NodeId, Vec<NodeId>> {
        let mut m: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for n in &self.nodes {
            m.entry(n.id.clone()).or_default();
        }
        for e in &self.edges {
            m.entry(e.to.clone()).or_default().push(e.from.clone());
        }
        m
    }

    /// 拓扑检查：有环就报错。
    pub fn validate(&self) -> Result<(), PlanError> {
        let deps = self.deps_map();

        // 边引用的节点必须存在
        for e in &self.edges {
            if self.node(&e.from).is_none() {
                return Err(PlanError::UnknownNode(e.from.clone()));
            }
            if self.node(&e.to).is_none() {
                return Err(PlanError::UnknownNode(e.to.clone()));
            }
        }

        // Kahn 拓扑排序检测环
        let mut indeg: HashMap<&str, usize> = HashMap::new();
        for n in &self.nodes {
            indeg.insert(n.id.as_str(), deps.get(&n.id).map(|v| v.len()).unwrap_or(0));
        }
        let mut q: VecDeque<&str> = indeg
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut seen = 0;
        while let Some(cur) = q.pop_front() {
            seen += 1;
            for e in &self.edges {
                if e.from == cur {
                    if let Some(d) = indeg.get_mut(e.to.as_str()) {
                        *d -= 1;
                        if *d == 0 {
                            q.push_back(e.to.as_str());
                        }
                    }
                }
            }
        }
        if seen != self.nodes.len() {
            return Err(PlanError::Cycle);
        }
        Ok(())
    }

    /// 就绪节点：状态 pending 且所有上游 succeeded。
    ///
    /// 注意：上游 `skipped` 不算成功 —— 这是保守选择。
    pub fn ready_nodes(&self, state: &RunState) -> Vec<&Node> {
        let deps = self.deps_map();
        self.nodes
            .iter()
            .filter(|n| {
                state.status_of(&n.id) == NodeStatus::Pending
                    && deps
                        .get(&n.id)
                        .map(|ds| {
                            ds.iter()
                                .all(|d| state.status_of(d) == NodeStatus::Succeeded)
                        })
                        .unwrap_or(true)
            })
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────
// 运行状态
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    /// 人工网关等待中
    WaitingHuman,
    /// 被跳过（上游失败且策略允许）
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// 单个节点的运行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRun {
    pub id: NodeId,
    pub status: NodeStatus,
    pub attempts: u32,
    pub reworks: u32,
    /// 失败原因
    pub error: Option<String>,
}

/// 一次运行的状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub id: String,
    /// 冻结的工作流快照 —— 后续编辑不影响已在跑的 run
    pub snapshot: Workflow,
    pub status: RunStatus,
    pub nodes: HashMap<NodeId, NodeRun>,
    /// 返工统计
    pub total_reworks: u32,
}

impl RunState {
    /// 开始一次运行。**冻结工作流快照**：后续编辑不影响它。
    pub fn start(workflow: &Workflow, run_id: &str) -> Result<Self, PlanError> {
        workflow.validate()?;
        let mut nodes = HashMap::new();
        for n in &workflow.nodes {
            nodes.insert(
                n.id.clone(),
                NodeRun {
                    id: n.id.clone(),
                    status: NodeStatus::Pending,
                    attempts: 0,
                    reworks: 0,
                    error: None,
                },
            );
        }
        Ok(Self {
            id: run_id.into(),
            snapshot: workflow.clone(),
            status: RunStatus::Running,
            nodes,
            total_reworks: 0,
        })
    }

    pub fn status_of(&self, id: &str) -> NodeStatus {
        self.nodes
            .get(id)
            .map(|n| n.status)
            .unwrap_or(NodeStatus::Pending)
    }

    fn set(&mut self, id: &str, st: NodeStatus) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.status = st;
        }
    }

    /// 标记节点开始执行。
    pub fn begin_node(&mut self, id: &str) -> Result<(), PlanError> {
        let n = self
            .nodes
            .get_mut(id)
            .ok_or(PlanError::UnknownNode(id.into()))?;
        if n.status != NodeStatus::Pending {
            return Err(PlanError::NotPending(id.into()));
        }
        n.status = NodeStatus::Running;
        n.attempts += 1;
        Ok(())
    }

    /// 标记节点成功。
    pub fn succeed_node(&mut self, id: &str) {
        self.set(id, NodeStatus::Succeeded);
    }

    /// 标记节点失败，按策略处理。
    ///
    /// 返回下一步该做什么。
    pub fn fail_node(&mut self, id: &str, error: &str) -> Result<FailureAction, PlanError> {
        let node = self
            .snapshot
            .node(id)
            .ok_or(PlanError::UnknownNode(id.into()))?
            .clone();
        if let Some(n) = self.nodes.get_mut(id) {
            n.status = NodeStatus::Failed;
            n.error = Some(error.to_string());
        }

        let policy = node.on_failure.unwrap_or(OnFailure::Fail);
        match policy {
            OnFailure::Loop => {
                let run = self.nodes.get(id).unwrap();
                if run.attempts >= node.max_attempts {
                    // 重试耗尽 → 按 fail 处理
                    self.status = RunStatus::Failed;
                    return Ok(FailureAction::Abort);
                }
                self.set(id, NodeStatus::Pending);
                Ok(FailureAction::RetrySelf)
            }
            OnFailure::Reject => {
                let target = node.reject_to.ok_or(PlanError::NoRejectTarget(id.into()))?;
                let run = self.nodes.get(id).unwrap();
                if run.reworks >= node.max_reworks {
                    self.status = RunStatus::Failed;
                    return Ok(FailureAction::Abort);
                }
                // 打回：目标节点重置为 pending，本节点也重置
                self.set(&target, NodeStatus::Pending);
                self.set(id, NodeStatus::Pending);
                if let Some(r) = self.nodes.get_mut(id) {
                    r.reworks += 1;
                }
                self.total_reworks += 1;
                Ok(FailureAction::Reject { to: target })
            }
            OnFailure::Human => {
                self.set(id, NodeStatus::WaitingHuman);
                Ok(FailureAction::AskHuman)
            }
            OnFailure::Fail => {
                self.status = RunStatus::Failed;
                Ok(FailureAction::Abort)
            }
        }
    }

    /// 人工裁决。
    pub fn human_verdict(&mut self, id: &str, approve: bool) -> Result<FailureAction, PlanError> {
        if self.status_of(id) != NodeStatus::WaitingHuman {
            return Err(PlanError::NotWaitingHuman(id.into()));
        }
        if approve {
            self.set(id, NodeStatus::Succeeded);
            Ok(FailureAction::Continue)
        } else {
            let node = self
                .snapshot
                .node(id)
                .ok_or(PlanError::UnknownNode(id.into()))?
                .clone();
            match node.reject_to {
                Some(target) => {
                    self.set(&target, NodeStatus::Pending);
                    self.set(id, NodeStatus::Pending);
                    if let Some(r) = self.nodes.get_mut(id) {
                        r.reworks += 1;
                    }
                    self.total_reworks += 1;
                    Ok(FailureAction::Reject { to: target })
                }
                None => {
                    self.status = RunStatus::Failed;
                    Ok(FailureAction::Abort)
                }
            }
        }
    }

    /// 检查是否全部完成。
    pub fn is_complete(&self) -> bool {
        self.nodes
            .values()
            .all(|n| matches!(n.status, NodeStatus::Succeeded | NodeStatus::Skipped))
    }

    /// 已成功的节点数。
    pub fn succeeded_count(&self) -> usize {
        self.nodes
            .values()
            .filter(|n| n.status == NodeStatus::Succeeded)
            .count()
    }

    /// 返工率。
    pub fn rework_rate(&self) -> f64 {
        let done = self.succeeded_count();
        if done == 0 {
            0.0
        } else {
            self.total_reworks as f64 / done as f64
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureAction {
    /// 重试自己
    RetrySelf,
    /// 打回到某节点
    Reject { to: NodeId },
    /// 交人
    AskHuman,
    /// 终止
    Abort,
    /// 继续（人批准了）
    Continue,
}

// ─────────────────────────────────────────────────────────────
// 调度器：分支一满足依赖就跑
// ─────────────────────────────────────────────────────────────

/// 调度决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// 可以开始这些节点（可并行）
    Start(Vec<NodeId>),
    /// 有人在等
    WaitingHuman(Vec<NodeId>),
    /// 全部完成
    Done,
    /// 失败终止
    Failed,
}

/// 计算下一步该启动哪些节点。
///
/// **不等同批最慢的**：只要有节点就绪就返回，
/// 调用方可以并发执行它们。
pub fn next_batch(state: &RunState) -> Schedule {
    if state.status == RunStatus::Failed {
        return Schedule::Failed;
    }
    let waiting: Vec<NodeId> = state
        .nodes
        .values()
        .filter(|n| n.status == NodeStatus::WaitingHuman)
        .map(|n| n.id.clone())
        .collect();
    if !waiting.is_empty() {
        return Schedule::WaitingHuman(waiting);
    }
    if state.is_complete() {
        return Schedule::Done;
    }
    let ready: Vec<NodeId> = state
        .snapshot
        .ready_nodes(state)
        .into_iter()
        .map(|n| n.id.clone())
        .collect();
    if ready.is_empty() {
        // 没有就绪但也没完成 → 卡住了
        if state
            .nodes
            .values()
            .all(|n| n.status == NodeStatus::Running)
        {
            return Schedule::Done; // 全部在跑，等同批结束
        }
        return Schedule::Failed;
    }
    Schedule::Start(ready)
}

// ─────────────────────────────────────────────────────────────
// 前置能力检查
// ─────────────────────────────────────────────────────────────

/// 运行前检查前置能力。
///
/// 返回缺失的要求。非空时**不应开始运行** ——
/// 让它在首次运行前暴露，而不是流水线中途失败。
pub fn check_requirements<'a>(
    workflow: &'a Workflow,
    available: &HashSet<String>,
) -> Vec<&'a Requirement> {
    workflow
        .requirements
        .iter()
        .filter(|r| !available.contains(&r.key))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    UnknownNode(NodeId),
    Cycle,
    NotPending(NodeId),
    NotWaitingHuman(NodeId),
    NoRejectTarget(NodeId),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownNode(id) => write!(f, "未知节点 {id}"),
            Self::Cycle => write!(f, "工作流有环"),
            Self::NotPending(id) => write!(f, "节点 {id} 不在 pending 状态"),
            Self::NotWaitingHuman(id) => write!(f, "节点 {id} 不在等人状态"),
            Self::NoRejectTarget(id) => write!(f, "节点 {id} 的 Reject 策略缺少目标"),
        }
    }
}

impl std::error::Error for PlanError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_wf() -> Workflow {
        Workflow::new("wf", "简单流程")
            .add_node(Node::agent("design", "设计", "architect"))
            .add_node(Node::agent("impl", "实现", "coder"))
            .add_node(Node::script("test", "测试", "cargo test"))
            .add_node(Node::human_gate("review", "评审", "检查改动"))
            .add_edge("design", "impl")
            .add_edge("impl", "test")
            .add_edge("test", "review")
    }

    // ── 结构校验 ──

    #[test]
    fn validates_acyclic() {
        assert!(simple_wf().validate().is_ok());
    }

    #[test]
    fn detects_cycle() {
        let wf = Workflow::new("c", "环")
            .add_node(Node::script("a", "a", "echo a"))
            .add_node(Node::script("b", "b", "echo b"))
            .add_edge("a", "b")
            .add_edge("b", "a");
        assert_eq!(wf.validate(), Err(PlanError::Cycle));
    }

    #[test]
    fn detects_unknown_edge_target() {
        let wf = Workflow::new("x", "x")
            .add_node(Node::script("a", "a", "echo"))
            .add_edge("a", "nonexistent");
        assert_eq!(
            wf.validate(),
            Err(PlanError::UnknownNode("nonexistent".into()))
        );
    }

    // ── 就绪判定 ──

    #[test]
    fn only_root_is_ready_initially() {
        let wf = simple_wf();
        let st = RunState::start(&wf, "r1").unwrap();
        let ready = wf.ready_nodes(&st);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "design");
    }

    #[test]
    fn requires_upstream_succeeded_not_skipped() {
        let wf = simple_wf();
        let mut st = RunState::start(&wf, "r1").unwrap();
        // 把 design 标为 skipped
        st.set("design", NodeStatus::Skipped);
        let ready = wf.ready_nodes(&st);
        assert!(ready.is_empty(), "skipped 不算成功，下游不该就绪");
    }

    #[test]
    fn downstream_ready_after_success() {
        let wf = simple_wf();
        let mut st = RunState::start(&wf, "r1").unwrap();
        st.succeed_node("design");
        let ready = wf.ready_nodes(&st);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "impl");
    }

    // ── 并行分支不等最慢 ──

    #[test]
    fn parallel_branches_run_independently() {
        // design → impl_a 和 impl_b 并行 → test
        let wf = Workflow::new("p", "并行")
            .add_node(Node::agent("design", "设计", "architect"))
            .add_node(Node::script("fast", "快分支", "sleep 0"))
            .add_node(Node::script("slow", "慢分支", "sleep 100"))
            .add_node(Node::script("test", "测试", "cargo test"))
            .add_edge("design", "fast")
            .add_edge("design", "slow")
            .add_edge("fast", "test")
            .add_edge("slow", "test");

        let mut st = RunState::start(&wf, "r1").unwrap();
        st.succeed_node("design");

        // 两个分支同时就绪 —— 这是「不等同批最慢」的关键
        let ready: Vec<String> = wf.ready_nodes(&st).iter().map(|n| n.id.clone()).collect();
        assert_eq!(ready.len(), 2, "两个分支应同时就绪");
        assert!(ready.contains(&"fast".to_string()));
        assert!(ready.contains(&"slow".to_string()));

        // fast 完成后，test 仍未就绪（因为 slow 还没完成）；
        // 此时就绪列表里只剩 slow（它还没跑）
        st.succeed_node("fast");
        let ready: Vec<String> = wf.ready_nodes(&st).iter().map(|n| n.id.clone()).collect();
        assert!(
            !ready.contains(&"test".to_string()),
            "test 不能只等 fast，必须等 slow"
        );

        // slow 完成后 test 才就绪
        st.succeed_node("slow");
        let ready: Vec<String> = wf.ready_nodes(&st).iter().map(|n| n.id.clone()).collect();
        assert_eq!(ready, vec!["test".to_string()], "两个上游都成功后才就绪");
    }

    #[test]
    fn batch_starts_all_ready_nodes() {
        let wf = Workflow::new("p", "并行")
            .add_node(Node::script("root", "根", "echo"))
            .add_node(Node::script("a", "a", "echo a"))
            .add_node(Node::script("b", "b", "echo b"))
            .add_edge("root", "a")
            .add_edge("root", "b");
        let mut st = RunState::start(&wf, "r1").unwrap();
        st.succeed_node("root");
        match next_batch(&st) {
            Schedule::Start(ids) => {
                assert_eq!(ids.len(), 2, "应一次返回两个可并行节点");
            }
            other => panic!("期望 Start，实际 {other:?}"),
        }
    }

    // ── 失败策略 ──

    #[test]
    fn on_failure_loop_retries_then_aborts() {
        let mut n = Node::script("s", "步骤", "false");
        n.on_failure = Some(OnFailure::Loop);
        n.max_attempts = 2;
        let wf = Workflow::new("l", "l").add_node(n);
        let mut st = RunState::start(&wf, "r1").unwrap();

        st.begin_node("s").unwrap();
        assert_eq!(st.fail_node("s", "boom").unwrap(), FailureAction::RetrySelf);
        assert_eq!(st.status_of("s"), NodeStatus::Pending);

        st.begin_node("s").unwrap();
        assert_eq!(
            st.fail_node("s", "boom again").unwrap(),
            FailureAction::Abort
        );
        assert_eq!(st.status, RunStatus::Failed);
    }

    #[test]
    fn on_failure_reject_sends_back_upstream() {
        let mut impl_node = Node::agent("impl", "实现", "coder");
        impl_node.on_failure = Some(OnFailure::Reject);
        impl_node.reject_to = Some("design".into());
        impl_node.max_reworks = 1;

        let wf = Workflow::new("rj", "打回")
            .add_node(Node::agent("design", "设计", "architect"))
            .add_node(impl_node)
            .add_edge("design", "impl");

        let mut st = RunState::start(&wf, "r1").unwrap();
        st.begin_node("design").unwrap();
        st.succeed_node("design");
        st.begin_node("impl").unwrap();

        match st.fail_node("impl", "方案不对").unwrap() {
            FailureAction::Reject { to } => assert_eq!(to, "design"),
            other => panic!("期望打回，实际 {other:?}"),
        }
        assert_eq!(st.status_of("design"), NodeStatus::Pending, "上游应重置");
        assert_eq!(st.total_reworks, 1);
    }

    #[test]
    fn rework_exhaustion_aborts() {
        let mut n = Node::agent("impl", "实现", "coder");
        n.on_failure = Some(OnFailure::Reject);
        n.reject_to = Some("design".into());
        n.max_reworks = 1;

        let wf = Workflow::new("rj2", "打回")
            .add_node(Node::agent("design", "设计", "architect"))
            .add_node(n)
            .add_edge("design", "impl");

        let mut st = RunState::start(&wf, "r1").unwrap();
        // 第一次打回成功
        st.begin_node("impl").unwrap();
        assert!(matches!(
            st.fail_node("impl", "e1").unwrap(),
            FailureAction::Reject { .. }
        ));
        // 第二次超出 max_reworks → 终止
        st.begin_node("impl").unwrap();
        assert_eq!(st.fail_node("impl", "e2").unwrap(), FailureAction::Abort);
    }

    #[test]
    fn on_failure_human_waits() {
        let mut n = Node::agent("impl", "实现", "coder");
        n.on_failure = Some(OnFailure::Human);
        let wf = Workflow::new("h", "h").add_node(n);
        let mut st = RunState::start(&wf, "r1").unwrap();
        st.begin_node("impl").unwrap();
        assert_eq!(
            st.fail_node("impl", "需要人看").unwrap(),
            FailureAction::AskHuman
        );
        assert_eq!(st.status_of("impl"), NodeStatus::WaitingHuman);
        assert!(matches!(next_batch(&st), Schedule::WaitingHuman(_)));
    }

    // ── 人工网关 ──

    #[test]
    fn human_approve_continues() {
        let wf = simple_wf();
        let mut st = RunState::start(&wf, "r1").unwrap();
        st.set("review", NodeStatus::WaitingHuman);
        assert_eq!(
            st.human_verdict("review", true).unwrap(),
            FailureAction::Continue
        );
        assert_eq!(st.status_of("review"), NodeStatus::Succeeded);
    }

    #[test]
    fn human_reject_sends_back() {
        let mut gw = Node::human_gate("review", "评审", "看改动");
        gw.reject_to = Some("impl".into());
        let wf = Workflow::new("hr", "hr")
            .add_node(Node::agent("impl", "实现", "coder"))
            .add_node(gw)
            .add_edge("impl", "review");

        let mut st = RunState::start(&wf, "r1").unwrap();
        st.begin_node("impl").unwrap();
        st.succeed_node("impl");
        st.set("review", NodeStatus::WaitingHuman);
        st.set("impl", NodeStatus::Succeeded);

        match st.human_verdict("review", false).unwrap() {
            FailureAction::Reject { to } => assert_eq!(to, "impl"),
            other => panic!("期望打回，实际 {other:?}"),
        }
        assert_eq!(st.status_of("impl"), NodeStatus::Pending);
    }

    #[test]
    fn verdict_requires_waiting_state() {
        let wf = simple_wf();
        let mut st = RunState::start(&wf, "r1").unwrap();
        assert_eq!(
            st.human_verdict("review", true),
            Err(PlanError::NotWaitingHuman("review".into()))
        );
    }

    // ── 前置能力 ──

    #[test]
    fn missing_requirements_reported_before_run() {
        let wf = simple_wf().add_requirement("git-token", "Git Token", "在设置里配置");
        let available: HashSet<String> = HashSet::new();
        let missing = check_requirements(&wf, &available);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].key, "git-token");
    }

    #[test]
    fn satisfied_requirements_pass() {
        let wf = simple_wf().add_requirement("git-token", "Git Token", "配置它");
        let mut available = HashSet::new();
        available.insert("git-token".to_string());
        assert!(check_requirements(&wf, &available).is_empty());
    }

    // ── 快照冻结 ──

    #[test]
    fn snapshot_is_frozen_at_start() {
        let wf = simple_wf();
        let mut st = RunState::start(&wf, "r1").unwrap();
        // 修改原工作流
        let mut edited = wf.clone();
        edited.nodes.clear();
        // 运行中的快照不受影响
        assert_eq!(st.snapshot.nodes.len(), 4);
        st.succeed_node("design");
        assert_eq!(st.status_of("design"), NodeStatus::Succeeded);
    }

    // ── 完成判定 ──

    #[test]
    fn complete_when_all_succeeded() {
        let wf = simple_wf();
        let mut st = RunState::start(&wf, "r1").unwrap();
        for id in ["design", "impl", "test", "review"] {
            st.succeed_node(id);
        }
        assert!(st.is_complete());
        assert_eq!(next_batch(&st), Schedule::Done);
        assert_eq!(st.succeeded_count(), 4);
    }

    #[test]
    fn rework_rate_is_reported() {
        let mut n = Node::agent("impl", "实现", "coder");
        n.on_failure = Some(OnFailure::Reject);
        n.reject_to = Some("design".into());
        n.max_reworks = 5;
        let wf = Workflow::new("rr", "rr")
            .add_node(Node::agent("design", "设计", "architect"))
            .add_node(n)
            .add_edge("design", "impl");

        let mut st = RunState::start(&wf, "r1").unwrap();
        st.succeed_node("design");
        st.begin_node("impl").unwrap();
        st.fail_node("impl", "e").unwrap();

        // 打回会把 design 也重置为 pending，所以此刻成功数为 0，
        // 返工率尚无意义（避免除零，返回 0.0）。
        assert_eq!(st.succeeded_count(), 0);
        assert_eq!(st.rework_rate(), 0.0);
        assert_eq!(st.total_reworks, 1, "返工次数要记下来");

        // 重跑完成后返工率才有值
        st.succeed_node("design");
        st.succeed_node("impl");
        // 1 次返工 / 2 个成功 = 0.5
        assert!(
            (st.rework_rate() - 0.5).abs() < 1e-9,
            "实际 {}",
            st.rework_rate()
        );
    }

    // ── 节点类型 ──

    #[test]
    fn script_nodes_are_deterministic() {
        let s = Node::script("a", "a", "echo");
        let p = Node {
            id: "p".into(),
            name: "p".into(),
            kind: NodeKind::PreviewBuild {
                command: "build".into(),
            },
            on_failure: None,
            reject_to: None,
            retry: None,
            carry_over: vec![],
            max_attempts: 1,
            max_reworks: 0,
        };
        let a = Node::agent("b", "b", "coder");
        assert!(s.kind.is_deterministic());
        assert!(p.kind.is_deterministic());
        assert!(!a.kind.is_deterministic(), "Agent 节点需要模型参与");
    }
}
