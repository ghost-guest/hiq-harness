//! `orbit-rpc` — 事件模型 + agent 运行时客户端。
//!
//! **改造说明**：本 crate 来自 orbit（github.com/imrj05/orbit，Apache-2.0）。
//! 上游是 pi CLI 的 RPC 客户端（`pi --mode rpc` 子进程 + stdio JSONL）；
//! 本项目把运行时换成自己的 harness 内核（Rust，进程内调用），
//! 但**保留全部类型定义与 `PiClient` 的公开接口**，因此 `orbit-pi` 的
//! 界面代码无需改动。
//!
//! 设计规则（沿用上游 AGENTS.md）：
//! - 信封宽松解析；未知形状透传为原始 JSON
//! - 客户端从不阻塞 UI 帧：取事件一律 `try_recv`

pub mod bridge;
pub mod client;
pub mod session_store;
pub mod types;

pub use bridge::run_task;
pub use client::{
    augmented_path, fallback_bin_dirs, hide_console, pi_bin_candidates, pi_binary,
    pi_package_root, pi_package_root_from, PiClient, RuntimeConfig, HARNESS_WORKDIR_ENV,
    PI_BIN_ENV, PI_PACKAGE_ROOT_ENV,
};
pub use types::{
    is_auth_event, parse_quota_reports, AssistantMessageEvent, AuthErrorCode, AuthEvent,
    AuthMethodCapability, AuthProvider, Command, CommandBody, ContextUsage, Event, MessageUsage,
    PendingQueue, QueueLane, QuotaBalance, QuotaKind, QuotaReport, QuotaWindow, SessionState,
    SessionUsage,
};
