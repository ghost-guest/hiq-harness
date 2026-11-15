//! 推理强度档位。
//!
//! 各家 provider 的档位体系不同：
//! - OpenAI 风格：minimal / low / medium / high / xhigh
//! - DeepSeek（经 Magpie）：low / high / max，部分含 none
//!
//! 本模块把统一档位映射到 provider 实际支持的档位，
//! 并把映射结果记下来 —— 这样「我请求了 medium，实际跑的是 low」
//! 是可观测的，不会静默降级。
//!
//! 实测结论（2026-11，auto/deepseek-v4.1-flash）：
//! reported levels = low / high / max。
//! medium 与 xhigh 被上游接受但不产生独立行为（中位推理量差异在噪声内）。

use serde::{Deserialize, Serialize};

/// 统一档位（OpenAI 风格命名，覆盖常见叫法）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    /// 从字符串解析。未知值报错而不是静默取默认 ——
    /// 一个拼错的档位名不该悄悄变成 low。
    pub fn parse(s: &str) -> Result<Self, EffortError> {
        match s.trim().to_lowercase().as_str() {
            "none" | "off" | "0" => Ok(Self::None),
            "minimal" | "min" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" | "med" | "mid" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "extra-high" | "extra_high" | "veryhigh" => Ok(Self::XHigh),
            "max" | "maximum" => Ok(Self::Max),
            other => Err(EffortError::Unknown(other.to_string())),
        }
    }

    /// 数值序，用于比较与映射。
    pub fn rank(&self) -> u8 {
        match self {
            Self::None => 0,
            Self::Minimal => 1,
            Self::Low => 2,
            Self::Medium => 3,
            Self::High => 4,
            Self::XHigh => 5,
            Self::Max => 6,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortError {
    Unknown(String),
}

impl std::fmt::Display for EffortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(s) => write!(
                f,
                "未知的推理档位 {s}。可用：none/minimal/low/medium/high/xhigh/max"
            ),
        }
    }
}

impl std::error::Error for EffortError {}

/// 映射结果。必须能回答「实际发的是什么」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortPlan {
    /// 用户请求的档位
    pub requested: Effort,
    /// 实际发给 provider 的档位
    pub effective: Effort,
    /// 是否发生了映射（请求值不被上游支持）
    pub downgraded: bool,
    /// 映射理由，便于日志排查
    pub note: Option<String>,
}

impl EffortPlan {
    /// 是否等价（没有映射）。
    pub fn is_exact(&self) -> bool {
        !self.downgraded
    }
}

/// 把请求档位映射到 provider 支持的档位集合。
///
/// 规则：**向最近的受支持档位取整**，同等距离取较低档
/// （保守：宁可推理少一点，也不要意外的成本与延迟）。
///
/// 若 provider 没报支持列表，原样透传，不做猜测。
pub fn plan_effort(requested: Effort, supported: &[Effort]) -> EffortPlan {
    if supported.is_empty() {
        return EffortPlan {
            requested,
            effective: requested,
            downgraded: false,
            note: Some("provider 未声明支持的档位，原样透传".into()),
        };
    }

    if supported.contains(&requested) {
        return EffortPlan {
            requested,
            effective: requested,
            downgraded: false,
            note: None,
        };
    }

    // 找最近的
    let target_rank = requested.rank();
    let mut best: Option<Effort> = None;
    let mut best_dist = u8::MAX;
    for s in supported {
        let d = s.rank().abs_diff(target_rank);
        // 距离更近，或距离相同但档位更低（保守）
        let better = match best {
            None => true,
            Some(b) => d < best_dist || (d == best_dist && s.rank() < b.rank()),
        };
        if better {
            best = Some(*s);
            best_dist = d;
        }
    }

    let effective = best.unwrap_or(requested);
    EffortPlan {
        requested,
        effective,
        downgraded: effective != requested,
        note: Some(format!(
            "上游只支持 {}，{} 映射为 {}",
            supported
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("/"),
            requested.as_str(),
            effective.as_str()
        )),
    }
}

/// DeepSeek 经 Magpie 的档位集（实测）。
pub const DEEPSEEK_LEVELS: &[Effort] = &[Effort::Low, Effort::High, Effort::Max];

/// 部分 DeepSeek 型号还支持 none。
pub const DEEPSEEK_LEVELS_WITH_NONE: &[Effort] =
    &[Effort::None, Effort::Low, Effort::High, Effort::Max];

/// OpenAI 风格全集。
pub const OPENAI_LEVELS: &[Effort] = &[
    Effort::Minimal,
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_aliases() {
        for (s, want) in [
            ("low", Effort::Low),
            ("LOW", Effort::Low),
            ("medium", Effort::Medium),
            ("med", Effort::Medium),
            ("mid", Effort::Medium),
            ("high", Effort::High),
            ("xhigh", Effort::XHigh),
            ("extra-high", Effort::XHigh),
            ("max", Effort::Max),
            ("none", Effort::None),
            ("off", Effort::None),
            ("minimal", Effort::Minimal),
            ("min", Effort::Minimal),
        ] {
            assert_eq!(Effort::parse(s).unwrap(), want, "解析 {s}");
        }
    }

    #[test]
    fn parse_unknown_errors() {
        assert!(matches!(
            Effort::parse("superduper"),
            Err(EffortError::Unknown(_))
        ));
    }

    #[test]
    fn rank_is_ordered() {
        let order = [
            Effort::None,
            Effort::Minimal,
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ];
        for w in order.windows(2) {
            assert!(w[0].rank() < w[1].rank());
        }
    }

    #[test]
    fn deepseek_supports_low_high_max() {
        let p = plan_effort(Effort::Low, DEEPSEEK_LEVELS);
        assert!(p.is_exact());
        let p = plan_effort(Effort::High, DEEPSEEK_LEVELS);
        assert!(p.is_exact());
        let p = plan_effort(Effort::Max, DEEPSEEK_LEVELS);
        assert!(p.is_exact());
    }

    #[test]
    fn medium_maps_up_to_high_on_deepseek() {
        // medium(3) → low(2) 距离1，high(4) 距离1
        // 等距离取较低 → low
        let p = plan_effort(Effort::Medium, DEEPSEEK_LEVELS);
        assert!(p.downgraded);
        assert_eq!(p.effective, Effort::Low, "等距离应取保守的低档");
        assert!(p.note.unwrap().contains("medium"));
    }

    #[test]
    fn xhigh_maps_to_max() {
        // xhigh(5) → max(6) 距离1，high(4) 距离1
        // 等距离取较低 → high
        let p = plan_effort(Effort::XHigh, DEEPSEEK_LEVELS);
        assert!(p.downgraded);
        assert_eq!(p.effective, Effort::High);
    }

    #[test]
    fn minimal_maps_to_low() {
        let p = plan_effort(Effort::Minimal, DEEPSEEK_LEVELS);
        assert_eq!(p.effective, Effort::Low);
    }

    #[test]
    fn none_maps_to_low_when_unsupported() {
        let p = plan_effort(Effort::None, DEEPSEEK_LEVELS);
        assert_eq!(p.effective, Effort::Low);
        assert!(p.downgraded);
    }

    #[test]
    fn none_stays_none_when_supported() {
        let p = plan_effort(Effort::None, DEEPSEEK_LEVELS_WITH_NONE);
        assert!(p.is_exact());
        assert_eq!(p.effective, Effort::None);
    }

    #[test]
    fn empty_supported_passes_through() {
        let p = plan_effort(Effort::XHigh, &[]);
        assert!(!p.downgraded, "无声明时不猜，原样透传");
        assert_eq!(p.effective, Effort::XHigh);
        assert!(p.note.is_some());
    }

    #[test]
    fn openai_levels_keep_all_distinct() {
        for e in [
            Effort::Minimal,
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
        ] {
            let p = plan_effort(e, OPENAI_LEVELS);
            assert!(p.is_exact(), "{:?} 应原样保留", e);
        }
    }

    #[test]
    fn mapping_is_deterministic() {
        let a = plan_effort(Effort::Medium, DEEPSEEK_LEVELS);
        let b = plan_effort(Effort::Medium, DEEPSEEK_LEVELS);
        assert_eq!(a, b);
    }
}
