//! `harness-cli` 的库入口。
//!
//! 二进制（`src/main.rs`）与桌面端共用同一套内核：把 `main.rs` 里的模块
//! 声明搬到这里，`main.rs` 通过 `use harness_cli::...` 引用。
//!
//! **为什么要有这个文件**：桌面端（`apps/orbit/crates/orbit-rpc`）需要
//! 在进程内调用内核的流式执行器，而不是 spawn 一个 `harness` 子进程。
//! 进程内调用省掉一次 IPC，也让取消、状态查询这些操作变成普通的函数调用。

pub mod durable;
pub mod exec;
pub mod host;
pub mod host_service;
pub mod llm;
pub mod memory;
pub mod rest;
pub mod routes;
pub mod runner;
pub mod server;
pub mod sop;
pub mod sse;
pub mod tools;
