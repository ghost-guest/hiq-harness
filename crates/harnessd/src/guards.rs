//! L2 guards, fused onto the engine's approval seam.
//!
//! The three guards themselves are `harness-guards` (already proven on this repo's
//! own sessions). What is new here is the *bridge*: codewhale asks its client for
//! approval before running a command (`item/commandExecution/requestApproval`),
//! so a guard verdict becomes a denial on that channel. No engine patch needed —
//! this is fusion depth D2, not D4.

use harness_guards::{GuardSet, PostCallVerdict, PreCallVerdict, Signature};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Run, but append a note to the tool result so the model sees it.
    AllowWithNote(String),
    /// Refuse this call. The reason goes back to the model *and* the UI.
    Deny(String),
    /// End the turn: the loop is not converging.
    Stop(String),
}

pub struct GuardBridge {
    set: GuardSet,
    denials: u64,
    stops: u64,
}

impl Default for GuardBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl GuardBridge {
    pub fn new() -> Self {
        Self { set: GuardSet::new(), denials: 0, stops: 0 }
    }

    pub fn begin_turn(&mut self) {
        self.set.begin_turn();
    }

    pub fn before(&mut self, tool: &str, _args: &str) -> Verdict {
        match self.set.before_call(tool) {
            PreCallVerdict::Proceed => Verdict::Allow,
            PreCallVerdict::ProceedWithNote { note } => Verdict::AllowWithNote(note),
            PreCallVerdict::Block { reason } => {
                self.denials += 1;
                Verdict::Deny(reason)
            }
        }
    }

    pub fn after(&mut self, tool: &str, args: &str, result: &str, blocks: &str, success: bool) -> Verdict {
        let sig = Signature::new(tool, args, result, blocks);
        match self.set.after_call(&sig, success) {
            PostCallVerdict::OK => Verdict::Allow,
            PostCallVerdict::Note { note } => Verdict::AllowWithNote(note),
            PostCallVerdict::ReplaceResult { reason } => {
                self.denials += 1;
                Verdict::Deny(reason)
            }
            PostCallVerdict::Stop { reason } => {
                self.stops += 1;
                Verdict::Stop(reason)
            }
        }
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.denials, self.stops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_identical_failure_produces_a_verdict() {
        let mut g = GuardBridge::new();
        g.begin_turn();
        let mut last = Verdict::Allow;
        for _ in 0..4 {
            last = g.after("shell", "cargo test", "error[E0308]", "", false);
        }
        assert_ne!(last, Verdict::Allow, "same failure four times must be noticed");
    }

    #[test]
    fn success_resets_the_failure_streak() {
        let mut g = GuardBridge::new();
        g.begin_turn();
        for _ in 0..2 {
            g.after("shell", "cargo test", "error", "", false);
        }
        assert_eq!(g.after("shell", "cargo test", "ok", "", true), Verdict::Allow);
        assert_eq!(g.after("shell", "cargo test", "error", "", false), Verdict::Allow,
                   "streak restarted after a success");
    }

    #[test]
    fn exploration_spiral_is_blocked_before_the_call() {
        let mut g = GuardBridge::new();
        g.begin_turn();
        let mut blocked = false;
        for _ in 0..20 {
            if let Verdict::Deny(_) = g.before("list_dir", "{}") {
                blocked = true;
                break;
            }
        }
        assert!(blocked, "read-only spiral must eventually deny the call");
        assert!(g.stats().0 >= 1);
    }
}
