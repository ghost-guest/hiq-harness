//! harnessd — the fusion supervisor.
//!
//! Sits between orbit (client) and a codewhale app-server (engine) and owns the
//! pieces codewhale does not: guard veto, time-bus log, steer gate, cache guard,
//! and the out-of-process plugin host.
//!
//! Design rule (see docs/KERNEL-FUSION-PLAN.md): the engine is reached over
//! JSON-RPC only. We never link its crates, so an upstream bump cannot force a
//! rewrite of this crate.

pub mod cacheguard;
pub mod engine;
pub mod guards;
pub mod mapping;
pub mod orbit_compat;
pub mod plugins;
pub mod protocol;
pub mod server;
pub mod steer;
pub mod timebus;
