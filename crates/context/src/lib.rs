//! L3 上下文：token 预算、三段式降级、压缩触发与摘要校验。
//!
//! 关键常量与规则来自：
//! - opencode：渐进收缩、预压缩提前量、缓存感知
//! - maka：摘要格式单一权威、回显不算内容、大折叠下限、截断检测
//! - Deva：工具结果三段式降级
//! - Codewhale：缓存铁律（header 冻结、历史只增）

pub mod cache;
pub mod effort;

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────
// 预算常量（opencode）
// ─────────────────────────────────────────────────────────────

/// 被 provider 拒绝「太长」后的收缩阶梯。
/// 不用猜该压多少，被拒一次就退一档，实测驱动。
pub const SHRINK_STEPS: &[f64] = &[0.85, 0.7, 0.5, 0.35];

/// 压缩前预留的窗口比例与下限。
pub const RESERVE_SHARE: f64 = 0.15;
pub const RESERVE_MIN: usize = 16_000;

/// 后台摘要提前量。
/// 摘要要 30–60 秒，这期间中位数涨几千 token，
/// 但 1/20 的情况涨 25k 以上（大量文件读取同时到达）。
/// 提前量取决于增长速度而非窗口大小，所以取固定值。
pub const PRECOMPUTE_LEAD: usize = 60_000;

/// 只在缓存寿命还剩这么多时才启动后台摘要 —— 反正要失效了才动。
pub const CACHE_MARGIN: usize = 30_000;

/// 摘要至少给的输出额度。
pub const SUMMARY_OUTPUT_FLOOR: usize = 32_000;

/// 请求级预留（octet）。
pub const TURN_RESERVE: usize = 16_000;

/// 自适应余量：窗口的 1%，钳制在 256–4096。
/// 理由：provider 用自己的 tokenizer 算 token，必须留余量。
pub fn adaptive_margin(context_window: usize) -> usize {
    ((context_window as f64 * 0.01) as usize).clamp(256, 4096)
}

// ─────────────────────────────────────────────────────────────
// 三段式工具结果（Deva）
// ─────────────────────────────────────────────────────────────

/// 单条工具结果的三种形态。
///
/// 比「整段压缩」粒度更细：逐个结果降级。
/// Deva 实测 802 个工具结果里 21% 已降到第三档。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum ToolResultTier {
    /// 原始全文
    Full { text: String },
    /// 一行摘要
    Summary { text: String },
    /// 只留磁盘引用，可随时取回
    DiskRef { path: String, bytes: usize },
}

impl ToolResultTier {
    pub fn text_for_model(&self) -> String {
        match self {
            Self::Full { text } => text.clone(),
            Self::Summary { text } => format!("[摘要] {text}"),
            Self::DiskRef { path, bytes } => {
                // 文案对齐 Deva 实测格式：必须给出「怎么取回来」的指引，
                // 否则模型不知道该用 read_file 去读，会重复调工具。
                format!(
                    "[较早的工具结果已清理以节省上下文] 原结果 {bytes} 字符，已保存到：{path}。\
                     需要时用 read_file 查看；文件、搜索类结果也可以重新调用工具获取最新内容。"
                )
            }
        }
    }

    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.text_for_model())
    }

    /// 降到下一档。Full → Summary → DiskRef。
    ///
    /// `summary` 与 `disk_path` 由调用方提供：摘要要模型生成，
    /// 磁盘路径要真的落盘后才有效。
    pub fn degrade(&self, summary: &str, disk_path: &str) -> Self {
        match self {
            Self::Full { text: _ } => Self::Summary {
                text: summary.to_string(),
            },
            Self::Summary { .. } => Self::DiskRef {
                path: disk_path.to_string(),
                bytes: 0,
            },
            Self::DiskRef { .. } => self.clone(),
        }
    }
}

/// 降级时的目标形态。按年龄从新到旧依次降。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradePolicy {
    /// 保留全文
    KeepFull,
    /// 降成摘要
    ToSummary,
    /// 降成磁盘引用
    ToDiskRef,
}

/// 根据上下文压力决定每个工具结果降到哪一档。
///
/// 越老的越先降。返回与输入等长的策略列表。
pub fn plan_degradation(
    tiers: &[ToolResultTier],
    used_tokens: usize,
    context_window: usize,
) -> Vec<DegradePolicy> {
    let usable = context_window.saturating_sub(TURN_RESERVE + adaptive_margin(context_window));
    if used_tokens <= usable {
        return vec![DegradePolicy::KeepFull; tiers.len()];
    }

    // 压力 = 超出的比例。用超出量占「可用量」的比例，
    // 但下限要给足，否则刚过线时一条都不降，等于没生效。
    let overshoot = used_tokens - usable;
    let raw = overshoot as f64 / usable.max(1) as f64;
    // 只要过线，至少按 20% 压力起步；上限 100%
    let pressure = (raw.max(0.2)).min(1.0);

    let n = tiers.len();
    let mut plan = vec![DegradePolicy::KeepFull; n];

    // 先降最老的（索引小的）：一部分降到磁盘，一部分降到摘要，最新的保留
    let to_summary = ((n as f64) * pressure * 0.5).ceil() as usize;
    let to_disk = ((n as f64) * pressure * 0.3).ceil() as usize;

    for item in plan.iter_mut().take(to_disk.min(n)) {
        *item = DegradePolicy::ToDiskRef;
    }
    for i in to_disk..(to_disk + to_summary).min(n) {
        plan[i] = DegradePolicy::ToSummary;
    }
    plan
}

// ─────────────────────────────────────────────────────────────
// token 估算与校准
// ─────────────────────────────────────────────────────────────

/// 估算一段文本的 token 数。保守：宁可高估。
pub fn estimate_tokens(text: &str) -> usize {
    let cjk = text.chars().filter(|c| is_cjk(*c)).count();
    let rest = text.chars().count().saturating_sub(cjk);
    // 中文约 1.5 字/token，英文约 4 字符/token
    ((cjk as f64) / 1.5).round() as usize + rest / 4 + 1
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3000..=0x303F | 0xFF00..=0xFFEF)
}

/// 用 provider 回报的真实值校准本地估算（Codewhale）。
///
/// 只在 `reported > estimated` 时更新 —— 保守，宁可高估。
/// 低估会导致请求被拒，高估只浪费一点余量。
#[derive(Debug, Default, Clone)]
pub struct TokenCalibration {
    reported: usize,
    estimated: usize,
}

impl TokenCalibration {
    pub fn observe(&mut self, reported: usize, estimated: usize) {
        if estimated == 0 {
            return;
        }
        // 只有实际比例更大才更新
        if (reported as u128) * (self.estimated.max(1) as u128)
            > (self.reported as u128) * (estimated as u128)
        {
            self.reported = reported;
            self.estimated = estimated;
        }
    }

    pub fn calibrate(&self, estimate: usize) -> usize {
        if self.reported <= self.estimated || self.estimated == 0 {
            return estimate;
        }
        (estimate as u128 * self.reported as u128).div_ceil(self.estimated as u128) as usize
    }

    pub fn ratio(&self) -> Option<f64> {
        if self.estimated == 0 {
            None
        } else {
            Some(self.reported as f64 / self.estimated as f64)
        }
    }
}

/// 预算策略。
#[derive(Debug, Clone)]
pub struct BudgetPolicy {
    pub context_window: usize,
    pub calibration: TokenCalibration,
}

impl BudgetPolicy {
    pub fn new(context_window: usize) -> Self {
        Self {
            context_window,
            calibration: TokenCalibration::default(),
        }
    }

    /// 压缩前必须留出的余量。
    pub fn reserve(&self) -> usize {
        ((self.context_window as f64 * RESERVE_SHARE) as usize).max(RESERVE_MIN)
    }

    /// 当前用量校准后是否该压缩。
    pub fn should_compact(&self, estimated: usize) -> bool {
        let real = self.calibration.calibrate(estimated);
        real + self.reserve() + TURN_RESERVE >= self.context_window
    }

    /// 是否该提前开始写后台摘要。
    pub fn should_precompute(&self, estimated: usize) -> bool {
        let real = self.calibration.calibrate(estimated);
        real + self.reserve() + PRECOMPUTE_LEAD >= self.context_window
    }

    /// 请求是否装得下。
    pub fn fits(&self, estimated: usize) -> bool {
        let real = self.calibration.calibrate(estimated);
        real + TURN_RESERVE + adaptive_margin(self.context_window) <= self.context_window
    }

    /// 被 provider 拒绝后，下一次请求该压到多少。
    ///
    /// `rejected_size` 是首次被拒请求的实际大小。
    pub fn shrink_target(&self, rejected_size: usize, attempt: usize) -> usize {
        let step = SHRINK_STEPS
            .get(attempt)
            .copied()
            .unwrap_or(*SHRINK_STEPS.last().unwrap_or(&0.35));
        (rejected_size as f64 * step) as usize
    }
}

// ─────────────────────────────────────────────────────────────
// 摘要格式与校验（maka）
// ─────────────────────────────────────────────────────────────

/// 摘要必须有的段落。
///
/// `Critical Context` 必填的原因：真实事故里丢了文件、命令和错误。
pub const REQUIRED_SECTIONS: &[&str] = &[
    "## Goal",
    "## Progress",
    "## Next Steps",
    "## Critical Context",
];

/// 完整的摘要模板。提示词构建与校验共用这一份，
/// 格式不可能漂移。
pub const SUMMARY_TEMPLATE: &[&str] = &[
    "## Goal",
    "[What the user is trying to accomplish]",
    "",
    "## Progress",
    "### Done",
    "- [Completed work and changes]",
    "### In Progress",
    "- [Current work]",
    "",
    "## Key Decisions",
    "- **[Decision]**: [Brief rationale]",
    "",
    "## Next Steps",
    "1. [Ordered list of what should happen next]",
    "",
    "## Critical Context",
    "- [Files, commands/results, errors, anything needed to continue; or \"(none)\"]",
];

/// 大折叠的摘要下限：超过 10k 输入 token 的折叠，摘要至少 200 输出 token。
pub const LARGE_FOLD_INPUT_TOKENS: usize = 10_000;
pub const LARGE_FOLD_SUMMARY_TOKENS_FLOOR: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryDefect {
    /// 缺少必需段落
    MissingSection(&'static str),
    /// 只是把模板照抄回来，没有实际内容
    TemplateEcho,
    /// 结尾像被截断
    Truncated,
    /// 折叠量很大但摘要太短
    TooShort { tokens: usize, floor: usize },
}

/// 校验摘要是否符合契约。
///
/// `fold_input_tokens` 是被压缩掉的历史的 token 数，
/// 用于大折叠下限判断。
pub fn validate_summary(summary: &str, fold_input_tokens: usize) -> Vec<SummaryDefect> {
    let mut defects = Vec::new();

    for section in REQUIRED_SECTIONS {
        if !summary.contains(section) {
            defects.push(SummaryDefect::MissingSection(section));
        }
    }

    // 模板回显：如果去掉模板行之后几乎没有内容，就是照抄
    if is_template_echo(summary) {
        defects.push(SummaryDefect::TemplateEcho);
    }

    if looks_truncated(summary) {
        defects.push(SummaryDefect::Truncated);
    }

    let tokens = estimate_tokens(summary);
    if fold_input_tokens >= LARGE_FOLD_INPUT_TOKENS && tokens < LARGE_FOLD_SUMMARY_TOKENS_FLOOR {
        defects.push(SummaryDefect::TooShort {
            tokens,
            floor: LARGE_FOLD_SUMMARY_TOKENS_FLOOR,
        });
    }

    defects
}

/// 模板行不算内容 —— 模型照抄格式不能通过校验。
fn is_template_echo(summary: &str) -> bool {
    let template_lines: Vec<String> = SUMMARY_TEMPLATE
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let content_lines: Vec<String> = summary
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if content_lines.is_empty() {
        return true;
    }

    // 所有非空行都在模板里出现过 → 纯回显
    content_lines
        .iter()
        .all(|line| template_lines.contains(line))
}

/// 结尾是省略号、冒号、逗号、破折号 → 判中途停止。
///
/// 故意不含反引号：它可能是完整代码块的结尾。
fn looks_truncated(summary: &str) -> bool {
    let t = summary.trim_end();
    if t.is_empty() {
        return true;
    }
    let last = t.chars().last().unwrap();
    matches!(
        last,
        '…' | ':' | '：' | ',' | '，' | '、' | ';' | '；' | '—' | '(' | '（'
    ) || t.ends_with("...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrink_steps_monotonic() {
        for w in SHRINK_STEPS.windows(2) {
            assert!(w[0] > w[1], "收缩阶梯必须单调递减");
        }
    }

    #[test]
    fn estimate_handles_cjk_and_ascii() {
        let zh = estimate_tokens("这是一个中文句子测试");
        let en = estimate_tokens("this is an english sentence");
        assert!(zh >= 5, "中文估算 {zh} 偏低");
        assert!(en > 0);
    }

    #[test]
    fn calibration_only_moves_up() {
        let mut c = TokenCalibration::default();
        c.observe(100, 100);
        assert_eq!(c.calibrate(1000), 1000);
        c.observe(200, 100);
        assert_eq!(c.calibrate(1000), 2000);
        c.observe(50, 100);
        assert_eq!(c.calibrate(1000), 2000, "校准比率只增不减");
    }

    #[test]
    fn calibration_rounds_up() {
        let mut c = TokenCalibration::default();
        c.observe(3, 2);
        // 1000 * 3 / 2 = 1500
        assert_eq!(c.calibrate(1000), 1500);
        // 奇数时必须向上取整，不能低估
        assert_eq!(c.calibrate(1001), 1502);
    }

    #[test]
    fn budget_triggers_compaction_with_reserve() {
        let p = BudgetPolicy::new(100_000);
        assert!(p.should_compact(90_000));
        assert!(!p.should_compact(60_000));
    }

    #[test]
    fn precompute_starts_ahead_of_time() {
        let p = BudgetPolicy::new(200_000);
        assert!(p.should_precompute(200_000 - 30_000 - 60_000));
        assert!(!p.should_precompute(50_000));
    }

    #[test]
    fn shrink_target_degrades_by_step() {
        let p = BudgetPolicy::new(200_000);
        let rejected = 200_000;
        assert_eq!(p.shrink_target(rejected, 0), 170_000);
        assert_eq!(p.shrink_target(rejected, 1), 140_000);
        assert_eq!(p.shrink_target(rejected, 2), 100_000);
        assert_eq!(p.shrink_target(rejected, 3), 70_000);
        // 超出阶梯用最低档
        assert_eq!(p.shrink_target(rejected, 9), 70_000);
    }

    #[test]
    fn adaptive_margin_is_clamped() {
        assert_eq!(adaptive_margin(1_000), 256, "小窗口用下限");
        assert_eq!(adaptive_margin(100_000), 1000);
        assert_eq!(adaptive_margin(10_000_000), 4096, "大窗口用上限");
    }

    #[test]
    fn tiers_degrade_in_order() {
        let full = ToolResultTier::Full {
            text: "x".repeat(1000),
        };
        let sum = full.degrade("一句话摘要", "/tmp/x");
        assert!(matches!(sum, ToolResultTier::Summary { .. }));
        let disk = sum.degrade("ignored", "/tmp/x");
        assert!(matches!(disk, ToolResultTier::DiskRef { .. }));
        // DiskRef 不能再降
        let again = disk.degrade("a", "b");
        assert_eq!(again, disk);
    }

    #[test]
    fn disk_ref_renders_with_path() {
        let r = ToolResultTier::DiskRef {
            path: "/tmp/x".into(),
            bytes: 17572,
        };
        let s = r.text_for_model();
        assert!(s.contains("已清理") && s.contains("/tmp/x") && s.contains("17572"));
    }

    #[test]
    fn degradation_plan_respects_pressure() {
        let tiers: Vec<ToolResultTier> = (0..10)
            .map(|i| ToolResultTier::Full {
                text: format!("result {i}"),
            })
            .collect();

        // 无压力 → 全保留
        let plan = plan_degradation(&tiers, 1000, 200_000);
        assert!(plan.iter().all(|p| *p == DegradePolicy::KeepFull));

        // 中等压力 → 有降级但新的保留
        let plan = plan_degradation(&tiers, 190_000, 200_000);
        assert!(plan.iter().any(|p| *p != DegradePolicy::KeepFull));
        assert_eq!(
            *plan.last().unwrap(),
            DegradePolicy::KeepFull,
            "最新的不应被降"
        );
    }

    #[test]
    fn degradation_hits_oldest_first() {
        let tiers: Vec<ToolResultTier> = (0..10)
            .map(|i| ToolResultTier::Full {
                text: format!("r{i}"),
            })
            .collect();
        let plan = plan_degradation(&tiers, 250_000, 200_000);
        // 第一条必须降得最狠
        assert_eq!(plan[0], DegradePolicy::ToDiskRef);
        // 最后一条保留
        assert_eq!(plan[plan.len() - 1], DegradePolicy::KeepFull);
    }

    // ── 摘要校验 ──

    fn good_summary() -> String {
        "## Goal\n修好登录 bug\n\n## Progress\n### Done\n- 定位到 token 校验\n\
         ### In Progress\n- 改测试\n\n## Key Decisions\n- **保持兼容**: 不改接口\n\n\
         ## Next Steps\n1. 跑测试\n\n## Critical Context\n- src/auth.rs:42, cargo test 失败"
            .to_string()
    }

    #[test]
    fn good_summary_passes() {
        // fold 量在小折叠区间（<10k），这样短摘要是合格的
        let defects = validate_summary(&good_summary(), 5_000);
        assert!(defects.is_empty(), "合格摘要不该有缺陷: {defects:?}");
    }

    #[test]
    fn good_summary_passes_large_fold_when_long_enough() {
        // 大折叠但摘要够长 → 也应通过
        let mut s = good_summary();
        s.push_str("\n\n## 补充上下文\n");
        for i in 0..40 {
            s.push_str(&format!("- 第 {i} 条细节说明，包含文件与命令\n"));
        }
        let defects = validate_summary(&s, 50_000);
        assert!(!defects
            .iter()
            .any(|d| matches!(d, SummaryDefect::TooShort { .. })));
    }

    #[test]
    fn missing_section_is_rejected() {
        let s = "## Goal\nx\n\n## Progress\ny\n\n## Next Steps\nz\n";
        let defects = validate_summary(s, 1000);
        assert!(defects.contains(&SummaryDefect::MissingSection("## Critical Context")));
    }

    #[test]
    fn template_echo_is_rejected() {
        // 完全照抄模板，一个字没写
        let echo = SUMMARY_TEMPLATE.join("\n");
        let defects = validate_summary(&echo, 1000);
        assert!(defects.contains(&SummaryDefect::TemplateEcho));
    }

    #[test]
    fn template_echo_with_one_real_line_passes() {
        // 模板 + 一行真实内容 → 不算纯回显
        let mut s = SUMMARY_TEMPLATE.join("\n");
        s.push_str("\n- 真正的文件改动在这里");
        let defects = validate_summary(&s, 1000);
        assert!(!defects.contains(&SummaryDefect::TemplateEcho));
    }

    #[test]
    fn truncated_tail_is_detected() {
        let mut s = good_summary();
        s.push_str("接下来要看 src/auth");
        s.push(':');
        let defects = validate_summary(&s, 1000);
        assert!(defects.contains(&SummaryDefect::Truncated));
    }

    #[test]
    fn code_fence_tail_is_not_truncated() {
        let mut s = good_summary();
        s.push_str("\n```\nlet x = 1;\n```");
        let defects = validate_summary(&s, 1000);
        assert!(
            !defects.contains(&SummaryDefect::Truncated),
            "完整代码块结尾不算截断"
        );
    }

    #[test]
    fn large_fold_requires_min_output() {
        let tiny = "## Goal\nx\n## Progress\nx\n## Next Steps\nx\n## Critical Context\nx";
        let defects = validate_summary(tiny, 50_000);
        assert!(
            defects
                .iter()
                .any(|d| matches!(d, SummaryDefect::TooShort { .. })),
            "大折叠配超短摘要应被拒: {defects:?}"
        );
    }

    #[test]
    fn small_fold_allows_short_summary() {
        let tiny = "## Goal\nx\n## Progress\nx\n## Next Steps\nx\n## Critical Context\nx";
        let defects = validate_summary(tiny, 500);
        assert!(!defects
            .iter()
            .any(|d| matches!(d, SummaryDefect::TooShort { .. })));
    }
}
