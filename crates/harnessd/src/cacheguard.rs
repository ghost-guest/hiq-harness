//! Cache guard, from openhuman's `PromptCacheGuardMiddleware`.
//!
//! The rule that matters: compacting a conversation that is still hitting cache
//! throws away the cheapest tokens you have. So when the window is full *and* the
//! prefix is still hot, we freeze the prefix and let the engine compact; when the
//! hit rate has already collapsed, compacting again would only make it worse.

pub const HIT_FLOOR: f64 = 0.90;
pub const PRESSURE: f64 = 0.85;

#[derive(Debug, Clone, PartialEq)]
pub enum Advice {
    Ok,
    /// Do not touch system prompt or tool specs this turn.
    FreezePrefix { reason: String },
    /// Safe to compact: the prefix is already cached, so rewriting costs little.
    CompactNow { reason: String },
}

#[derive(Debug, Default)]
pub struct CacheGuard {
    window: u64,
    prompt_tokens: u64,
    cached_tokens: u64,
}

impl CacheGuard {
    pub fn new(window: u64) -> Self {
        Self { window, ..Default::default() }
    }

    pub fn observe(&mut self, prompt_tokens: u64, cached_tokens: u64) {
        self.prompt_tokens = prompt_tokens;
        self.cached_tokens = cached_tokens.min(prompt_tokens);
    }

    pub fn hit_rate(&self) -> f64 {
        if self.prompt_tokens == 0 {
            return 1.0;
        }
        self.cached_tokens as f64 / self.prompt_tokens as f64
    }

    pub fn advise(&self) -> Advice {
        if self.window == 0 {
            return Advice::Ok;
        }
        let used = self.prompt_tokens as f64 / self.window as f64;
        if used < PRESSURE {
            return Advice::Ok;
        }
        let hit = self.hit_rate();
        if hit >= HIT_FLOOR {
            Advice::CompactNow {
                reason: format!("window {:.0}% full, prefix cached at {:.1}%", used * 100.0, hit * 100.0),
            }
        } else {
            Advice::FreezePrefix {
                reason: format!("window {:.0}% full but hit only {:.1}% — compacting again would make it worse", used * 100.0, hit * 100.0),
            }
        }
    }

    /// Called after a compaction: the prefix is new, so nothing is cached yet.
    pub fn after_compaction(&mut self) {
        self.cached_tokens = 0;
        self.prompt_tokens = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_pressure_is_ok() {
        let mut g = CacheGuard::new(128_000);
        g.observe(40_000, 38_000);
        assert_eq!(g.advise(), Advice::Ok);
    }

    #[test]
    fn full_window_with_hot_prefix_compacts() {
        let mut g = CacheGuard::new(128_000);
        g.observe(120_000, 118_000);
        assert!(matches!(g.advise(), Advice::CompactNow { .. }));
    }

    #[test]
    fn full_window_with_cold_prefix_freezes_instead() {
        let mut g = CacheGuard::new(128_000);
        g.observe(120_000, 40_000);
        assert!(matches!(g.advise(), Advice::FreezePrefix { .. }),
                "compacting a cold prefix cannot pay for itself");
    }

    #[test]
    fn compaction_resets_the_measurement() {
        let mut g = CacheGuard::new(128_000);
        g.observe(120_000, 118_000);
        g.after_compaction();
        assert_eq!(g.advise(), Advice::Ok);
    }

    #[test]
    fn zero_tokens_counts_as_cached() {
        let g = CacheGuard::new(0);
        assert_eq!(g.hit_rate(), 1.0);
        assert_eq!(g.advise(), Advice::Ok);
    }
}
