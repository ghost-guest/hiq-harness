//! L2 守卫。
//!
//! 这一层的难点不是「数重复次数」，而是「怎么不杀掉合法的长任务」。
//! 三个守卫按优先级排列：
//!
//! 1. 只对相同的**失败**熔断（maka）—— 轮询和迭代-重试天然不受影响
//! 2. 三级阶梯 + 可解冻（Raven）—— 提示 / 拒跑 / 终止，冻结可被新证据解冻
//! 3. 只读螺旋（天枢）—— 连续只读不写，12 提示 15 阻塞
//!
//! 两个必须处理的盲区：
//! - 指纹要含 `blocks`（图片读取的像素在其中，否则重绘再检查会被误判）
//! - 不同文本不算进展（模型换说法太容易）

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// 一个工具调用的可比较签名。
///
/// `blocks_digest` 不能省：图片/渲染类工具返回的文本部分是稳定元数据，
/// 真正的变化在 blocks 里。只用文本做指纹会把真进展判成死循环。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Signature {
    pub tool: String,
    pub args: String,
    pub result: String,
    pub blocks_digest: String,
}

impl Signature {
    pub fn new(tool: &str, args: &str, result: &str, blocks_digest: &str) -> Self {
        Self {
            tool: tool.to_string(),
            args: args.to_string(),
            result: result.to_string(),
            blocks_digest: blocks_digest.to_string(),
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 守卫一：只对相同的失败熔断（maka，阈值 3）
// ─────────────────────────────────────────────────────────────

/// 只在「同一个失败连续发生」时熔断。
///
/// 关键设计：**成功一次或换调用即重置 streak**。
/// 因此：
/// - 轮询（反复查 status 直到通过）—— 每次结果不同，不会累积
/// - 迭代-重试（改文件后重跑同一个失败测试）—— 中间有成功写入，会重置
/// 两者天然不受影响，**不需要工具白名单**。
#[derive(Debug)]
pub struct FailureGate {
    last_failed: Option<String>,
    streak: u32,
    threshold: u32,
}

impl Default for FailureGate {
    fn default() -> Self {
        Self::new(3)
    }
}

impl FailureGate {
    pub fn new(threshold: u32) -> Self {
        Self {
            last_failed: None,
            streak: 0,
            threshold: threshold.max(1),
        }
    }

    /// 记录一次调用结果。返回是否应当拦截下一次相同失败。
    pub fn record(&mut self, sig: &Signature, success: bool) -> GateDecision {
        if success {
            // 成功即重置 —— 这是「不杀合法长任务」的核心
            self.last_failed = None;
            self.streak = 0;
            return GateDecision::Allow;
        }
        let key = failure_key(sig);
        if self.last_failed.as_deref() == Some(key.as_str()) {
            self.streak += 1;
        } else {
            self.last_failed = Some(key);
            self.streak = 1;
        }
        if self.streak >= self.threshold {
            GateDecision::Block {
                reason: format!(
                    "这个 {} 调用（参数完全相同）已连续失败 {} 次，且期间没有任何变化，\
                     再跑一次结果也一样。改变参数或换一步。",
                    sig.tool, self.streak
                ),
            }
        } else {
            GateDecision::Allow
        }
    }
}

fn failure_key(sig: &Signature) -> String {
    // 必须包含 result：轮询时同一工具同参数但结果在变，
    // 那是等待而非死循环。只用 tool+args 会把轮询误杀。
    format!("{}\u{0}{}\u{0}{}", sig.tool, sig.args, sig.result)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Block { reason: String },
}

// ─────────────────────────────────────────────────────────────
// 守卫二：三级阶梯 + 可解冻（Raven）
// ─────────────────────────────────────────────────────────────

/// 阈值（Raven 实测值）。
#[derive(Debug, Clone, Copy)]
pub struct NoProgressSemantics {
    /// 多少条相同答案后开始提示
    pub nudge_at: u32,
    /// 多少条后拒跑
    pub refuse_at: u32,
    /// 拒跑几次后终止
    pub stop_after_refusals: u32,
}

impl Default for NoProgressSemantics {
    fn default() -> Self {
        // 8 / 12 / 3
        Self {
            nudge_at: 8,
            refuse_at: 12,
            stop_after_refusals: 3,
        }
    }
}

#[derive(Debug, Clone)]
struct Frozen {
    /// 冻结时的全局新证据计数。用于判断是否已被新证据解冻。
    novelty: u64,
}

/// 三级阶梯守卫。
///
/// 两个计数器**故意不合并**：
/// - `seen` 累计（宽松），驱动提示
/// - `streak` 严格，问「自上次这个调用给出这个答案以来，别的调用有没有答出新东西」
///
/// 模型在两次检查之间插一个 `sleep`，`sleep` 的答案永远相同。
/// 只用一个计数器这个 `sleep` 会被判死循环；
/// 但旁边的检查在变化，所以 `sleep` 的 streak 每轮重置，只有 `seen` 在爬。
#[derive(Debug)]
pub struct NoProgressGuard {
    sem: NoProgressSemantics,
    seen: HashMap<String, u32>,
    streak: HashMap<String, u32>,
    frozen: HashMap<String, Frozen>,
    /// 全局「新证据」计数：某个 (调用, 答案) 组合首次出现时 +1
    novelty: u64,
    known_keys: std::collections::HashSet<String>,
    refusals: u32,
    /// 解冻次数也计入预算，否则「每 N 次产出 1 条新答案」的循环永远有界不了
    thaws: u32,
    nudges_this_turn: u32,
    stopped: bool,
}

impl Default for NoProgressGuard {
    fn default() -> Self {
        Self::new(NoProgressSemantics::default())
    }
}

impl NoProgressGuard {
    pub fn new(sem: NoProgressSemantics) -> Self {
        Self {
            sem,
            seen: HashMap::new(),
            streak: HashMap::new(),
            frozen: HashMap::new(),
            novelty: 0,
            known_keys: Default::default(),
            refusals: 0,
            thaws: 0,
            nudges_this_turn: 0,
            stopped: false,
        }
    }

    fn key(sig: &Signature) -> String {
        format!(
            "{}\u{0}{}\u{0}{}\u{0}{}",
            sig.tool, sig.args, sig.result, sig.blocks_digest
        )
    }

    /// 每轮开始时重置「本轮提示次数」。
    pub fn begin_turn(&mut self) {
        self.nudges_this_turn = 0;
    }

    /// 记录一次调用结果，返回处置决定。
    pub fn record(&mut self, sig: &Signature) -> NoProgressDecision {
        if self.stopped {
            return NoProgressDecision::Stop {
                reason: "已因无进展终止".into(),
            };
        }
        let key = Self::key(sig);

        // 全局新证据：这个 (调用, 答案) 组合是否首次出现
        if self.known_keys.insert(key.clone()) {
            self.novelty += 1;
        }

        let seen = self.seen.entry(key.clone()).or_insert(0);
        *seen += 1;
        let seen_n = *seen;

        // 解冻：冻结时的 novelty 与当前不同，说明别处答出了新东西
        if let Some(f) = self.frozen.get(&key) {
            if f.novelty != self.novelty {
                self.frozen.remove(&key);
                self.thaws += 1;
                self.streak.insert(key.clone(), 0);
            }
        }

        let streak = self.streak.entry(key.clone()).or_insert(0);
        *streak += 1;
        let streak_n = *streak;

        // 拒跑
        if streak_n >= self.sem.refuse_at {
            self.refusals += 1;
            self.frozen.insert(
                key.clone(),
                Frozen {
                    novelty: self.novelty,
                },
            );
            let used = self.used_budget();
            if self.refusals >= self.sem.stop_after_refusals || used >= self.sem.stop_after_refusals
            {
                self.stopped = true;
                return NoProgressDecision::Stop {
                    reason: format!(
                        "无进展终止：拒绝执行 {} 次，其中被其它进展解冻重跑 {} 次。",
                        self.refusals, self.thaws
                    ),
                };
            }
            return NoProgressDecision::Refuse {
                reason: format!(
                    "这个 {} 调用（相同参数、相同结果）已重复 {} 次，且在此期间没有任何其它调用答出新东西，\
                     所以这次不执行 —— 结果不会变。\
                     换一个不同长度的等待是一次不同的调用；只要别处答出新东西，这个调用会再被允许。",
                    sig.tool, streak_n
                ),
            };
        }

        // 提示
        if seen_n >= self.sem.nudge_at && self.nudges_this_turn < 2 {
            self.nudges_this_turn += 1;
            return NoProgressDecision::Nudge {
                note: format!(
                    "（运行时提示，非工具自身输出）这个 {} 调用已累计 {} 次相同结果。\
                     如果它在等外部进度，换个不同的等待或先做别的；如果它本来就是结论，换个步骤。",
                    sig.tool, seen_n
                ),
            };
        }

        NoProgressDecision::Allow
    }

    /// 已花掉的预算。解冻也算 —— 否则守卫有漏洞。
    fn used_budget(&self) -> u32 {
        self.refusals + self.thaws.saturating_sub(1)
    }

    pub fn stats(&self) -> GuardStats {
        GuardStats {
            refusals: self.refusals,
            thaws: self.thaws,
            novelty: self.novelty,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoProgressDecision {
    Allow,
    Nudge { note: String },
    Refuse { reason: String },
    Stop { reason: String },
}

#[derive(Debug, Clone, Copy)]
pub struct GuardStats {
    pub refusals: u32,
    pub thaws: u32,
    pub novelty: u64,
}

// ─────────────────────────────────────────────────────────────
// 守卫三：只读螺旋（天枢，12 / 15）
// ─────────────────────────────────────────────────────────────

pub const EXPLORATION_TOOLS: &[&str] = &[
    "grep",
    "glob",
    "read_file",
    "read_section",
    "semantic_search",
    "repo_map",
    "inspect_symbol",
    "find_references",
    "file_info",
    "related_tests",
    "diff",
];

pub const ADVISORY_THRESHOLD: u32 = 12;
pub const HARD_THRESHOLD: u32 = 15;

/// 检测「只探索不产出」。
///
/// 注释原话：只读不写就是没在干活。
#[derive(Debug)]
pub struct ReadOnlySpiral {
    history: VecDeque<String>,
    window: usize,
}

impl Default for ReadOnlySpiral {
    fn default() -> Self {
        // 必须手动实现：derive 会让 window=0，
        // 那样每 push 一条就被 pop 掉，守卫永远不触发。
        Self::new()
    }
}

impl ReadOnlySpiral {
    pub fn new() -> Self {
        Self {
            history: VecDeque::new(),
            window: HARD_THRESHOLD as usize,
        }
    }

    pub fn record(&mut self, tool: &str) -> SpiralDecision {
        let is_explore = EXPLORATION_TOOLS.contains(&tool);
        if !is_explore {
            self.history.clear();
            return SpiralDecision::Allow;
        }
        self.history.push_back(tool.to_string());
        if self.history.len() > self.window {
            self.history.pop_front();
        }
        let n = self.history.len() as u32;
        if n >= HARD_THRESHOLD {
            SpiralDecision::Block {
                reason: format!(
                    "已连续 {} 次只读操作，没有任何写入。这不像是还在推进，\
                     更像在原地探索。停下来基于已有信息做判断，或者动手改。",
                    n
                ),
            }
        } else if n >= ADVISORY_THRESHOLD {
            SpiralDecision::Advisory {
                note: format!("（运行时提示）已连续 {n} 次只读操作。如果信息够了，该动手了。"),
            }
        } else {
            SpiralDecision::Allow
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpiralDecision {
    Allow,
    Advisory { note: String },
    Block { reason: String },
}

// ─────────────────────────────────────────────────────────────
// 组合
// ─────────────────────────────────────────────────────────────

/// 把三个守卫合成一个入口。
#[derive(Debug, Default)]
pub struct GuardSet {
    pub failure: FailureGate,
    pub no_progress: NoProgressGuard,
    pub spiral: ReadOnlySpiral,
}

impl GuardSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_turn(&mut self) {
        self.no_progress.begin_turn();
    }

    /// 在**执行工具之前**调用。
    pub fn before_call(&mut self, tool: &str) -> PreCallVerdict {
        // 只读螺旋优先级最高：它不需要等结果就能判断
        match self.spiral.record(tool) {
            SpiralDecision::Block { reason } => PreCallVerdict::Block { reason },
            SpiralDecision::Advisory { note } => PreCallVerdict::ProceedWithNote { note },
            SpiralDecision::Allow => PreCallVerdict::Proceed,
        }
    }

    /// 在**拿到结果之后**调用。
    pub fn after_call(&mut self, sig: &Signature, success: bool) -> PostCallVerdict {
        // 失败熔断优先
        if let GateDecision::Block { reason } = self.failure.record(sig, success) {
            return PostCallVerdict::Note { note: reason };
        }
        match self.no_progress.record(sig) {
            NoProgressDecision::Allow => PostCallVerdict::OK,
            NoProgressDecision::Nudge { note } => PostCallVerdict::Note { note },
            NoProgressDecision::Refuse { reason } => PostCallVerdict::ReplaceResult { reason },
            NoProgressDecision::Stop { reason } => PostCallVerdict::Stop { reason },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCallVerdict {
    Proceed,
    ProceedWithNote { note: String },
    Block { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostCallVerdict {
    OK,
    /// 在结果尾部追加提示
    Note {
        note: String,
    },
    /// 用拒绝文案替换结果（模型读到 error 而非 success）
    ReplaceResult {
        reason: String,
    },
    Stop {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(tool: &str, args: &str, result: &str) -> Signature {
        Signature::new(tool, args, result, "")
    }

    // ── 守卫一 ──

    #[test]
    fn failure_gate_blocks_identical_failures() {
        let mut g = FailureGate::default();
        let s = sig("bash", "pytest", "boom");
        assert_eq!(g.record(&s, false), GateDecision::Allow);
        assert_eq!(g.record(&s, false), GateDecision::Allow);
        // 第 3 次才拦
        match g.record(&s, false) {
            GateDecision::Block { reason } => assert!(reason.contains("3")),
            _ => panic!("第三次相同失败应被拦"),
        }
    }

    #[test]
    fn failure_gate_resets_on_success() {
        let mut g = FailureGate::default();
        let s = sig("bash", "pytest", "boom");
        g.record(&s, false);
        g.record(&s, false);
        // 中间成功一次（比如改完文件跑通了别的）
        assert_eq!(g.record(&s, true), GateDecision::Allow);
        // 重置后重新计数，不应该立刻拦
        assert_eq!(g.record(&s, false), GateDecision::Allow);
    }

    #[test]
    fn failure_gate_allows_polling() {
        // 轮询：同一个工具同参数，但结果在变 —— 这是迭代-重试/等待，不该被拦
        let mut g = FailureGate::default();
        for i in 0..20 {
            let s = sig("bash", "check_status", &format!("running {i}%"));
            assert_eq!(
                g.record(&s, false),
                GateDecision::Allow,
                "结果变化的失败不应触发熔断"
            );
        }
    }

    #[test]
    fn failure_gate_allows_different_args() {
        let mut g = FailureGate::default();
        for i in 0..10 {
            let s = sig("bash", &format!("pytest -k test{i}"), "boom");
            assert_eq!(g.record(&s, false), GateDecision::Allow);
        }
    }

    // ── 守卫二 ──

    #[test]
    fn no_progress_nudges_then_refuses_then_stops() {
        let mut g = NoProgressGuard::default();
        let s = sig("bash", "sleep 5", "done");
        let mut saw_nudge = false;
        let mut saw_refuse = false;
        for i in 1..=12 {
            match g.record(&s) {
                NoProgressDecision::Nudge { .. } => saw_nudge = true,
                NoProgressDecision::Refuse { .. } => {
                    saw_refuse = true;
                    assert_eq!(i, 12, "拒跑应发生在第 12 次");
                }
                _ => {}
            }
        }
        assert!(saw_nudge, "第 8 次应有提示");
        assert!(saw_refuse, "第 12 次应拒跑");
    }

    #[test]
    fn no_progress_sleep_not_killed_when_other_work_progresses() {
        // 核心场景：模型反复 sleep，但旁边的检查在推进
        let mut g = NoProgressGuard::default();
        for round in 0..30 {
            // sleep 的答案永远相同
            let sleep = sig("bash", "sleep 2", "done");
            let d = g.record(&sleep);
            assert!(
                !matches!(d, NoProgressDecision::Stop { .. }),
                "旁边的检查在推进时，sleep 不该被终止 (round {round})"
            );
            // 旁边的检查每轮都有新答案
            let check = sig("bash", "cat log", &format!("entry {round}"));
            g.record(&check);
        }
    }

    #[test]
    fn no_progress_thaw_consumes_budget() {
        // 每 13 次产出 1 条新答案的循环，最终仍应被终止
        let mut g = NoProgressGuard::default();
        let mut stopped = false;
        for round in 0..100 {
            let s = sig("bash", "same", "same");
            if matches!(g.record(&s), NoProgressDecision::Stop { .. }) {
                stopped = true;
                break;
            }
            // 隔一段插一条新答案
            if round % 13 == 0 {
                g.record(&sig("bash", "probe", &format!("new {round}")));
            }
        }
        assert!(stopped, "解冻次数计入预算后，这种循环必须能终止");
    }

    #[test]
    fn blocks_digest_changes_break_repetition() {
        // 盲区：图片读取文本相同、像素不同 → 必须视为进展
        let mut g = NoProgressGuard::default();
        for i in 0..20 {
            let s = Signature::new("render_edit", "slide1", "same text", &format!("px{i}"));
            let d = g.record(&s);
            assert!(
                !matches!(d, NoProgressDecision::Stop { .. }),
                "像素变化应算进展"
            );
        }
    }

    // ── 守卫三 ──

    #[test]
    fn spiral_advisory_then_block() {
        let mut s = ReadOnlySpiral::new();
        for _ in 0..11 {
            assert_eq!(s.record("read_file"), SpiralDecision::Allow);
        }
        assert!(matches!(
            s.record("read_file"),
            SpiralDecision::Advisory { .. }
        ));
        for _ in 13..15 {
            let _ = s.record("read_file");
        }
        assert!(matches!(
            s.record("read_file"),
            SpiralDecision::Block { .. }
        ));
    }

    #[test]
    fn spiral_resets_on_write() {
        let mut s = ReadOnlySpiral::new();
        for _ in 0..11 {
            s.record("read_file");
        }
        // 写一次即清零
        assert_eq!(s.record("write_file"), SpiralDecision::Allow);
        assert_eq!(s.record("read_file"), SpiralDecision::Allow);
    }

    #[test]
    fn spiral_ignores_non_exploration_tools() {
        let mut s = ReadOnlySpiral::new();
        for _ in 0..20 {
            assert_eq!(s.record("bash"), SpiralDecision::Allow);
        }
    }

    #[test]
    fn spiral_default_has_valid_window() {
        // 回归：曾经用 derive(Default) 导致 window=0，守卫永远不触发
        let mut s = ReadOnlySpiral::default();
        let mut saw_advisory = false;
        for _ in 0..ADVISORY_THRESHOLD {
            if matches!(s.record("read_file"), SpiralDecision::Advisory { .. }) {
                saw_advisory = true;
            }
        }
        assert!(saw_advisory, "default() 构造的螺旋守卫必须真的会触发");
    }

    #[test]
    fn guard_set_spiral_works() {
        // 回归：GuardSet::default() 里的 spiral 必须工作
        let mut g = GuardSet::new();
        let mut saw = false;
        for _ in 0..HARD_THRESHOLD {
            if !matches!(g.before_call("read_file"), PreCallVerdict::Proceed) {
                saw = true;
            }
        }
        assert!(saw, "GuardSet 的只读螺旋必须真的会触发");
    }
}
