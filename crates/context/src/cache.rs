//! 缓存健康度与 miss 归因。
//!
//! 全部来自调研，不含自创设计：
//!
//! - **前缀冻结铁律**（Codewhale `docs/CACHE.md`）：
//!   "After session start, the system prompt and tool catalog are frozen bytes.
//!    History only grows. **A cache miss is allowed only when we can name why.**"
//! - **miss 必须可归因**（Codewhale `PrefixStabilityManager`）：
//!   每次变更记录原因，通过 `/cache stats` 报告。
//! - **三段式降级**（Deva 实测）：Full / Summary / DiskRef。
//!   实测 802 个工具结果里 21% 已降到第三档。
//!
//! 因此本模块只做两件事：
//! 1. 记录并归因每一次缓存 miss
//! 2. 统计命中率（含稳态口径，对齐 Deva 的测量方式）

use serde::{Deserialize, Serialize};

/// 缓存 miss 的原因。
///
/// 必须能回答「为什么这次没命中」，而不是只知道「没命中」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum MissReason {
    /// 会话首次请求，缓存尚未建立
    ColdStart,
    /// 新增了内容（正常增长，物理上必然 miss）
    NewContent { added_tokens: u64 },
    /// header 被改动：system / tools / reasoning 变化
    HeaderChanged { field: String },
    /// 无法归因（应当避免出现）
    Unexplained,
}

impl MissReason {
    pub fn is_expected(&self) -> bool {
        // 冷启动和新内容都是物理必然，不算异常
        matches!(self, Self::ColdStart | Self::NewContent { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Self::ColdStart => "会话首次请求，缓存未建立".into(),
            Self::NewContent { added_tokens } => {
                format!("新增 {added_tokens} token（正常增长，首现无法命中）")
            }
            Self::HeaderChanged { field } => {
                format!("header 被改动：{field} —— 这会重建整个缓存")
            }
            Self::Unexplained => "无法归因的 miss，需检查请求构造".into(),
        }
    }
}

/// header 的指纹。用来判断「冻结的部分有没有变」。
///
/// 对应 Codewhale 的 frozen bytes：system prompt + tool catalog
/// （以及影响后端的 reasoning 档位）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderFingerprint {
    pub system_hash: u64,
    pub tools_hash: u64,
    pub reasoning: Option<String>,
}

impl HeaderFingerprint {
    pub fn new(system: &str, tools_json: &str, reasoning: Option<&str>) -> Self {
        Self {
            system_hash: fnv1a(system),
            tools_hash: fnv1a(tools_json),
            reasoning: reasoning.map(|s| s.to_string()),
        }
    }

    /// 对比两个指纹，返回变化了的字段。
    ///
    /// 实测（2026-11）：system / tools 顺序 / reasoning 值
    /// 任一变化都会让缓存归零。
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut changed = Vec::new();
        if self.system_hash != other.system_hash {
            changed.push("system prompt".to_string());
        }
        if self.tools_hash != other.tools_hash {
            changed.push("tools".to_string());
        }
        if self.reasoning != other.reasoning {
            changed.push("reasoning".to_string());
        }
        changed
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 一次请求的缓存记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheRecord {
    pub step: u32,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub miss_reason: Option<MissReason>,
}

impl CacheRecord {
    pub fn hit_rate(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            self.cached_tokens as f64 / self.prompt_tokens as f64
        }
    }

    pub fn is_hit(&self) -> bool {
        self.cached_tokens > 0
    }
}

/// 缓存统计与归因。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStats {
    pub records: Vec<CacheRecord>,
    /// 上一次的 header 指纹，用于检测改动
    last_header: Option<HeaderFingerprint>,
    /// 上一次的 prompt token 数，用于估算新增量
    last_prompt: u64,
}

impl CacheStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次请求。
    ///
    /// `header` 是本次请求的 header 指纹。
    /// 返回本次 miss 的归因（命中时为 None）。
    pub fn record(
        &mut self,
        step: u32,
        prompt_tokens: u64,
        cached_tokens: u64,
        header: HeaderFingerprint,
    ) -> Option<MissReason> {
        let reason = if cached_tokens == 0 {
            Some(self.attribute_miss(prompt_tokens, &header))
        } else {
            None
        };

        self.records.push(CacheRecord {
            step,
            prompt_tokens,
            cached_tokens,
            miss_reason: reason.clone(),
        });
        self.last_header = Some(header);
        self.last_prompt = prompt_tokens;
        reason
    }

    /// 归因一次 miss。
    ///
    /// 顺序很重要：先查 header 是否变了（这是异常），
    /// 再判断是不是正常增长。
    fn attribute_miss(&self, prompt_tokens: u64, header: &HeaderFingerprint) -> MissReason {
        let Some(prev) = &self.last_header else {
            return MissReason::ColdStart;
        };

        let changed = prev.diff(header);
        if let Some(field) = changed.first() {
            return MissReason::HeaderChanged {
                field: field.clone(),
            };
        }

        // header 没变 → 正常增长
        let added = prompt_tokens.saturating_sub(self.last_prompt);
        if added > 0 {
            MissReason::NewContent {
                added_tokens: added,
            }
        } else {
            MissReason::Unexplained
        }
    }

    // ── 统计口径 ──

    pub fn requests(&self) -> usize {
        self.records.len()
    }

    pub fn prompt_tokens(&self) -> u64 {
        self.records.iter().map(|r| r.prompt_tokens).sum()
    }

    pub fn cached_tokens(&self) -> u64 {
        self.records.iter().map(|r| r.cached_tokens).sum()
    }

    /// 总命中率（含冷启动）。
    pub fn hit_rate(&self) -> f64 {
        let p = self.prompt_tokens();
        if p == 0 {
            0.0
        } else {
            self.cached_tokens() as f64 / p as f64
        }
    }

    /// 稳态命中率：排除首轮冷启动。
    ///
    /// Deva 的 99.9% 是在长会话稳定期测的，口径要对齐。
    pub fn steady_state_hit_rate(&self) -> f64 {
        if self.records.len() <= 1 {
            return self.hit_rate();
        }
        let rest = &self.records[1..];
        let p: u64 = rest.iter().map(|r| r.prompt_tokens).sum();
        let c: u64 = rest.iter().map(|r| r.cached_tokens).sum();
        if p == 0 {
            0.0
        } else {
            c as f64 / p as f64
        }
    }

    /// 有几次 miss 是**不该发生**的（header 被改动或无法归因）。
    ///
    /// Codewhale 的铁律：miss 必须能说出理由。
    /// 「header 被改动」和「无法归因」都是需要修的问题。
    pub fn avoidable_misses(&self) -> Vec<&CacheRecord> {
        self.records
            .iter()
            .filter(|r| {
                matches!(
                    r.miss_reason,
                    Some(MissReason::HeaderChanged { .. }) | Some(MissReason::Unexplained)
                )
            })
            .collect()
    }

    /// 生成可读报告，对应 Codewhale 的 `/cache stats`。
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "缓存：{} 次请求，prompt={} cached={}\n",
            self.requests(),
            self.prompt_tokens(),
            self.cached_tokens()
        ));
        out.push_str(&format!(
            "  总命中率 {:.1}%｜稳态命中率 {:.1}%（排除冷启动）\n",
            self.hit_rate() * 100.0,
            self.steady_state_hit_rate() * 100.0
        ));

        let avoidable = self.avoidable_misses();
        if avoidable.is_empty() {
            out.push_str("  所有 miss 均可解释（冷启动或正常增长）\n");
        } else {
            out.push_str(&format!("  ⚠ {} 次 miss 本可避免：\n", avoidable.len()));
            for r in avoidable {
                let why = r
                    .miss_reason
                    .as_ref()
                    .map(|m| m.describe())
                    .unwrap_or_default();
                out.push_str(&format!("    step {}: {why}\n", r.step));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(system: &str, tools: &str, reasoning: Option<&str>) -> HeaderFingerprint {
        HeaderFingerprint::new(system, tools, reasoning)
    }

    #[test]
    fn cold_start_is_explained() {
        let mut s = CacheStats::new();
        let r = s.record(0, 500, 0, hdr("sys", "tools", Some("high")));
        assert_eq!(r, Some(MissReason::ColdStart));
        assert!(r.unwrap().is_expected());
    }

    #[test]
    fn hit_has_no_miss_reason() {
        let mut s = CacheStats::new();
        s.record(0, 500, 0, hdr("sys", "t", Some("high")));
        let r = s.record(1, 1000, 900, hdr("sys", "t", Some("high")));
        assert_eq!(r, None, "命中不该有 miss 原因");
    }

    #[test]
    fn new_content_is_explained_and_expected() {
        let mut s = CacheStats::new();
        s.record(0, 500, 0, hdr("sys", "t", Some("high")));
        // 追加了 4700 token，全部首现 → miss
        let r = s.record(1, 5200, 0, hdr("sys", "t", Some("high")));
        match r {
            Some(MissReason::NewContent { added_tokens }) => {
                assert_eq!(added_tokens, 4700);
            }
            other => panic!("应归因为新内容，实际 {other:?}"),
        }
        assert!(r.unwrap().is_expected(), "正常增长不算异常");
    }

    #[test]
    fn system_change_is_flagged_as_avoidable() {
        let mut s = CacheStats::new();
        s.record(0, 500, 0, hdr("sys-v1", "t", Some("high")));
        let r = s.record(1, 500, 0, hdr("sys-v2", "t", Some("high")));
        match &r {
            Some(MissReason::HeaderChanged { field }) => assert_eq!(field, "system prompt"),
            other => panic!("应归因为 header 改动，实际 {other:?}"),
        }
        assert!(!r.unwrap().is_expected(), "header 改动不该发生");
        assert_eq!(s.avoidable_misses().len(), 1);
    }

    #[test]
    fn tools_change_is_flagged() {
        let mut s = CacheStats::new();
        s.record(0, 500, 0, hdr("sys", "tools-a", Some("high")));
        let r = s.record(1, 500, 0, hdr("sys", "tools-b", Some("high")));
        match &r {
            Some(MissReason::HeaderChanged { field }) => assert_eq!(field, "tools"),
            other => panic!("实际 {other:?}"),
        }
    }

    #[test]
    fn reasoning_change_is_flagged() {
        let mut s = CacheStats::new();
        s.record(0, 500, 0, hdr("sys", "t", Some("high")));
        let r = s.record(1, 500, 0, hdr("sys", "t", Some("low")));
        match &r {
            Some(MissReason::HeaderChanged { field }) => assert_eq!(field, "reasoning"),
            other => panic!("实际 {other:?}"),
        }
    }

    #[test]
    fn header_diff_lists_all_changes() {
        let a = hdr("sys1", "t1", Some("high"));
        let b = hdr("sys2", "t2", Some("low"));
        let d = a.diff(&b);
        assert_eq!(d.len(), 3);
        assert!(d.contains(&"system prompt".to_string()));
        assert!(d.contains(&"tools".to_string()));
        assert!(d.contains(&"reasoning".to_string()));
    }

    #[test]
    fn same_header_no_diff() {
        let a = hdr("sys", "t", Some("high"));
        let b = hdr("sys", "t", Some("high"));
        assert!(a.diff(&b).is_empty());
    }

    #[test]
    fn hit_rate_computation() {
        let mut s = CacheStats::new();
        s.record(0, 100, 0, hdr("s", "t", None)); // 冷启动
        s.record(1, 1000, 900, hdr("s", "t", None));
        s.record(2, 1000, 950, hdr("s", "t", None));
        assert_eq!(s.prompt_tokens(), 2100);
        assert_eq!(s.cached_tokens(), 1850);
        assert!((s.hit_rate() - 1850.0 / 2100.0).abs() < 1e-9);
    }

    #[test]
    fn steady_state_excludes_cold_start() {
        let mut s = CacheStats::new();
        s.record(0, 100, 0, hdr("s", "t", None));
        s.record(1, 1000, 950, hdr("s", "t", None));
        s.record(2, 1000, 970, hdr("s", "t", None));
        assert!((s.steady_state_hit_rate() - 0.96).abs() < 1e-9);
        assert!(s.hit_rate() < s.steady_state_hit_rate());
    }

    #[test]
    fn single_request_steady_equals_total() {
        let mut s = CacheStats::new();
        s.record(0, 100, 50, hdr("s", "t", None));
        assert!((s.steady_state_hit_rate() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn unexplained_miss_is_flagged() {
        let mut s = CacheStats::new();
        s.record(0, 500, 0, hdr("s", "t", None));
        // header 没变，prompt 也没增长，但仍 miss → 无法归因
        let r = s.record(1, 500, 0, hdr("s", "t", None));
        assert_eq!(r, Some(MissReason::Unexplained));
        assert_eq!(s.avoidable_misses().len(), 1);
    }

    #[test]
    fn report_mentions_avoidable() {
        let mut s = CacheStats::new();
        s.record(0, 500, 0, hdr("s1", "t", None));
        s.record(1, 500, 0, hdr("s2", "t", None));
        let rep = s.report();
        assert!(rep.contains("本可避免"));
        assert!(rep.contains("system prompt"));
    }

    #[test]
    fn report_clean_when_all_explained() {
        let mut s = CacheStats::new();
        s.record(0, 500, 0, hdr("s", "t", None));
        s.record(1, 5000, 0, hdr("s", "t", None)); // 新内容
        s.record(2, 5100, 4900, hdr("s", "t", None)); // 命中
        let rep = s.report();
        assert!(rep.contains("所有 miss 均可解释"), "实际: {rep}");
    }
}
