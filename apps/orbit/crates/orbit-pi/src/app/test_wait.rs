//! 测试里的等待预算。
//!
//! 界面的无头测试靠"泵到某个状态出现"，每个都写死了一个截止时间（5s / 2s）。
//! 这张机器上单独跑没问题，可 `cargo test -p orbit-pi` 是十几路并行 + 每路都要
//! 起假进程，满载时 5 秒不够——于是出现"同一用例单跑 6/6 过、满载偶尔红"的抖动。
//!
//! 抖动最坏的地方不是慢，是它让整份证据不可信：没人知道那次红是回归还是排队。
//! 所以这里不调某个用例的数字，给所有等待一个**统一的预算系数**：
//! 默认 ×4，可用 `ORBIT_TEST_TIMEOUT_SCALE` 再放大或收回。
use std::time::Duration;

pub fn budget(base: Duration) -> Duration {
    let scale: f64 = std::env::var("ORBIT_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4.0);
    base.mul_f64(scale)
}
