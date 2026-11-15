//! L3 上下文：token 预算与三段式降级。
//!
//! 关键常量来自 opencode 与 Deva：
//! - 渐进收缩被拒后逐级降
//! - 预压缩提前 60k token（因为 1/20 情况一次涨 25k）
//! - 工具结果三段式：原始 / 摘要 / 磁盘引用

use serde::{Deserialize, Serialize};

/// 被 provider 拒绝「太长」后的收缩阶梯（opencode）。
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
                format!("[较早的工具结果已清理以节省上下文] 约 {bytes} 字符，原文保留在 {path}")
            }
        }
    }
}

/// 估算一段文本的 token 数。
///
/// 保守：宁可高估。真实校准靠 provider 回报的实际值。
pub fn estimate_tokens(text: &str) -> usize {
    // 中文按 1.5 字符/token，英文按 4 字符/token 的粗估。
    let cjk = text.chars().filter(|c| is_cjk(*c)).count();
    let rest = text.chars().count().saturating_sub(cjk);
    (cjk * 2 / 3).max(cjk / 2) + rest / 4 + 1
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3000..=0x303F | 0xFF00..=0xFFEF)
}

/// 用 provider 回报的真实值校准本地估算（Codewhale）。
///
/// 只在 `reported > estimated` 时更新 —— 保守，宁可高估。
/// 低估会导致请求被拒，高估只是浪费一点余量。
#[derive(Debug, Default)]
pub struct TokenCalibration {
    reported: usize,
    estimated: usize,
}

impl TokenCalibration {
    pub fn observe(&mut self, reported: usize, estimated: usize) {
        if estimated == 0 {
            return;
        }
        // 只有实际值更大才更新
        if u128::from(reported as u64) * self.estimated.max(1) as u128
            > u128::from(self.reported as u64) * estimated as u128
        {
            self.reported = reported;
            self.estimated = estimated;
        }
    }

    pub fn calibrate(&self, estimate: usize) -> usize {
        if self.reported <= self.estimated || self.estimated == 0 {
            return estimate;
        }
        // 向上取整
        (estimate as u128 * self.reported as u128).div_ceil(self.estimated as u128) as usize
    }
}

/// 判断是否已经到该压缩的临界点。
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

    /// 当前用量按真实比例校准后，是否该压缩。
    pub fn should_compact(&self, estimated: usize) -> bool {
        let real = self.calibration.calibrate(estimated);
        real + self.reserve() >= self.context_window
    }

    /// 是否该提前开始写后台摘要。
    pub fn should_precompute(&self, estimated: usize) -> bool {
        let real = self.calibration.calibrate(estimated);
        real + self.reserve() + PRECOMPUTE_LEAD >= self.context_window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrink_steps_are_monotonic() {
        for w in SHRINK_STEPS.windows(2) {
            assert!(w[0] > w[1], "收缩阶梯必须单调递减");
        }
    }

    #[test]
    fn estimate_handles_cjk_and_ascii() {
        let zh = estimate_tokens("这是一个中文句子测试");
        let en = estimate_tokens("this is an english sentence");
        assert!(zh > 0 && en > 0);
        // 10 个中文字不应算成 10/4=2 个 token
        assert!(zh >= 5, "中文估算 {zh} 偏低");
    }

    #[test]
    fn calibration_only_moves_up() {
        let mut c = TokenCalibration::default();
        c.observe(100, 100); // 相等，不更新
        assert_eq!(c.calibrate(1000), 1000);
        c.observe(200, 100); // 实际是估算的 2 倍
        assert_eq!(c.calibrate(1000), 2000);
        // 再来一个更小的，不应回退
        c.observe(50, 100);
        assert_eq!(c.calibrate(1000), 2000, "校准比率只增不减");
    }

    #[test]
    fn budget_triggers_compaction_with_reserve() {
        let p = BudgetPolicy::new(100_000);
        // 90k 用量 + 15% 预留 = 105k > 100k → 该压缩
        assert!(p.should_compact(90_000));
        // 80k + 15k = 95k < 100k → 不压
        assert!(!p.should_compact(80_000));
    }

    #[test]
    fn precompute_starts_ahead_of_time() {
        let p = BudgetPolicy::new(200_000);
        // 提前 60k 就该动手
        assert!(p.should_precompute(200_000 - 30_000 - 60_000));
        assert!(!p.should_precompute(50_000));
    }

    #[test]
    fn tier_rendering() {
        let r = ToolResultTier::DiskRef {
            path: "/tmp/x".into(),
            bytes: 17572,
        };
        let s = r.text_for_model();
        assert!(s.contains("已清理") && s.contains("/tmp/x"));
    }
}
