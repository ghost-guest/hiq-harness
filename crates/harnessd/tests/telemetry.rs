//! 指标得从真事件里量出来，而不是「我觉得它在量」。
//!
//! 这里喂的是引擎真实吐出的形状（HTTP/SSE 的 `item.completed` + `turn.usage`），
//! 检查 time-bus 日志里能不能读出：工具调用、返工标记、缓存字段。

use harnessd::engine::MockEngine;
use harnessd::server::Supervisor;
use harnessd::timebus::Log;
use serde_json::json;

fn temp_log(tag: &str) -> (std::path::PathBuf, Log) {
    let mut p = std::env::temp_dir();
    p.push(format!("telemetry-{tag}-{}.ndjson", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let log = Log::open(&p).expect("open log");
    (p, log)
}

fn read(p: &std::path::Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn tool_calls_and_rework_are_measured_from_http_shaped_events() {
    let (path, log) = temp_log("tools");
    let engine = MockEngine::new(|_, _| json!({}));
    let mut sup = Supervisor::new(engine, log, 128_000);

    let call = json!({
        "item": {
            "id": "item_1",
            "kind": "tool_call",
            "status": "completed",
            "metadata": { "tool_name": "bash", "tool_input": "{\"command\":\"cargo test\"}" }
        }
    });
    sup.engine.emit("item/completed", call.clone());
    let _ = sup.pump().unwrap();

    let once = read(&path);
    assert!(once.contains("\"kind\":\"tool.call\""), "tool call not logged: {once}");
    assert!(once.contains("\"repeat\":false"), "first call must not be a rework: {once}");

    // 同一条命令再来一次 —— 这才是返工。
    sup.engine.emit("item/completed", call);
    let _ = sup.pump().unwrap();

    let twice = read(&path);
    assert!(
        twice.contains("\"repeat\":true"),
        "the second identical call must be marked as rework: {twice}"
    );
    let calls = twice.matches("\"kind\":\"tool.call\"").count();
    assert_eq!(calls, 2, "both calls must be present: {twice}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn cache_hit_inputs_are_recorded_when_the_engine_reports_them() {
    let (path, log) = temp_log("usage");
    let engine = MockEngine::new(|_, _| json!({}));
    let mut sup = Supervisor::new(engine, log, 128_000);

    // HTTP/SSE 形状（蛇形）
    sup.engine.emit("thread/tokenUsage/updated", json!({ "input_tokens": 2000, "cached_tokens": 1800 }));
    let _ = sup.pump().unwrap();
    let body = read(&path);
    assert!(body.contains("\"kind\":\"usage\""), "usage event not logged: {body}");
    assert!(body.contains("\"cached_tokens\":1800"), "cached tokens dropped: {body}");

    // stdio 形状（驼峰）
    sup.engine.emit("thread/tokenUsage/updated", json!({ "promptTokens": 1000, "cachedTokens": 900 }));
    let _ = sup.pump().unwrap();
    let body = read(&path);
    assert_eq!(body.matches("\"kind\":\"usage\"").count(), 2, "camelCase usage dropped: {body}");

    let _ = std::fs::remove_file(&path);
}
