//! personal-AgentHarness 核心 crate。
//!
//! L0 事件日志 + L1 主循环。

pub mod agent_loop;
pub mod event;
pub mod wire;

pub use agent_loop::{AgentLoop, LoopConfig, ModelClient, ModelResponse, RunOutcome, ToolExecutor};
pub use event::{Event, EventKind, EventLog, Projection, ToolOutcome, Verdict};
pub use wire::{Envelope, EnvelopeKind, FrameError, PROTOCOL_VERSION};
