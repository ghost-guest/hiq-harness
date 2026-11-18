//! Steering, with jcode's discipline: inject only at safe points.
//!
//! jcode's comment (crates/jcode-app-core/src/agent/turn_streaming_mpsc.rs) is the
//! whole design: a message arriving mid-token cannot be delivered without either
//! discarding the in-flight response or corrupting the transcript. So we queue,
//! and release at a boundary the engine has actually reached.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerMode {
    /// As soon as any safe point appears (item boundary).
    Immediate,
    /// Only after a tool call finishes.
    AfterItem,
    /// Only between turns.
    AfterTurn,
}

impl Default for SteerMode {
    fn default() -> Self {
        Self::Immediate
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafePoint {
    ItemDone,
    TurnDone,
}

#[derive(Debug, Default)]
pub struct SteerGate {
    mode: SteerMode,
    queue: VecDeque<String>,
    released: u64,
}

impl SteerGate {
    pub fn new(mode: SteerMode) -> Self {
        Self { mode, queue: VecDeque::new(), released: 0 }
    }

    pub fn mode(&self) -> SteerMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: SteerMode) {
        self.mode = mode;
    }

    pub fn push(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.trim().is_empty() {
            self.queue.push_back(text);
        }
    }

    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    pub fn released(&self) -> u64 {
        self.released
    }

    /// Returns the message to inject now, if this safe point is allowed to carry one.
    pub fn at_safe_point(&mut self, point: SafePoint) -> Option<String> {
        let allowed = match self.mode {
            SteerMode::Immediate => true,
            SteerMode::AfterItem => point == SafePoint::ItemDone,
            SteerMode::AfterTurn => point == SafePoint::TurnDone,
        };
        if !allowed {
            return None;
        }
        let msg = self.queue.pop_front()?;
        self.released += 1;
        Some(msg)
    }

    /// Turn ending with messages still queued: the engine must be told to keep going.
    pub fn has_follow_up(&self) -> bool {
        !self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_releases_at_item_boundary() {
        let mut g = SteerGate::new(SteerMode::Immediate);
        g.push("actually use postgres");
        assert_eq!(g.at_safe_point(SafePoint::ItemDone).as_deref(), Some("actually use postgres"));
        assert_eq!(g.pending(), 0);
    }

    #[test]
    fn after_turn_holds_until_turn_end() {
        let mut g = SteerGate::new(SteerMode::AfterTurn);
        g.push("next do X");
        assert_eq!(g.at_safe_point(SafePoint::ItemDone), None, "must not interrupt the turn");
        assert_eq!(g.at_safe_point(SafePoint::TurnDone).as_deref(), Some("next do X"));
    }

    #[test]
    fn queue_is_fifo_and_never_drops() {
        let mut g = SteerGate::new(SteerMode::Immediate);
        g.push("one");
        g.push("two");
        assert_eq!(g.at_safe_point(SafePoint::ItemDone).as_deref(), Some("one"));
        assert_eq!(g.at_safe_point(SafePoint::ItemDone).as_deref(), Some("two"));
        assert!(g.has_follow_up() == false);
        assert_eq!(g.released(), 2);
    }

    #[test]
    fn blank_messages_are_dropped() {
        let mut g = SteerGate::new(SteerMode::Immediate);
        g.push("   ");
        assert_eq!(g.pending(), 0);
    }
}
