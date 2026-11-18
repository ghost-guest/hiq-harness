//! S4 acceptance: a real Go plugin, built and loaded by the real host.
//!
//! Skipped when `go` is absent so a Rust-only checkout still tests green.

use harnessd::plugins::{PluginHost, PluginSpec, PLUGIN_PROTOCOL};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;

fn go_available() -> bool {
    Command::new("go").arg("version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

#[test]
fn go_plugin_handshakes_and_serves_tools() {
    if !go_available() {
        eprintln!("go not installed — skipping");
        return;
    }
    let root = repo_root();
    let plugin_dir = root.join("plugins/example-echo");
    assert!(plugin_dir.exists(), "missing {}", plugin_dir.display());

    let mut bin = std::env::temp_dir();
    bin.push(format!("example-echo-{}", std::process::id()));
    let build = Command::new("go")
        .current_dir(&plugin_dir)
        .args(["build", "-o"])
        .arg(&bin)
        .arg(".")
        .output()
        .expect("run go build");
    assert!(
        build.status.success(),
        "go build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    let mut host = PluginHost::new();
    host.load(&PluginSpec::new("example-echo", bin.to_string_lossy(), vec![]))
        .expect("plugin must load");

    assert_eq!(host.names(), ["example-echo"]);
    let tools = host.tools();
    assert!(tools.iter().any(|(t, _)| t == "echo.run"), "tools: {tools:?}");

    let echoed = host
        .call_tool("echo.run", json!({"hello":"world"}))
        .expect("tool is registered")
        .expect("call succeeds");
    assert_eq!(echoed["content"], json!({"hello":"world"}));

    let denial = host
        .call_tool("policy.check", json!({"command":"rm -rf /"}))
        .expect("tool is registered")
        .expect("call succeeds");
    assert_eq!(denial["content"]["decision"], "deny");

    assert_eq!(PLUGIN_PROTOCOL, 1);
    let _ = std::fs::remove_file(&bin);
}

#[test]
fn unknown_tool_is_reported_not_panicked() {
    if !go_available() {
        return;
    }
    assert!(PluginHost::new().call_tool("nope", json!({})).is_none());
}

/// S4 验收原句：「写一个 Go 插件，deny 一条命令，内核记录并继续」。
///
/// 这里跑的是真插件进程 + 真审批通道：插件说 deny，内核必须
/// ①在线上回 `decline`（引擎那边是真拒绝，不是建议）②把 `guard.deny` 记进日志
/// ③广播给其它插件 ④这条命令之外的东西照常跑（不 panic、不中断）。
#[test]
fn go_plugin_can_deny_a_command_and_the_kernel_records_it() {
    if !go_available() {
        eprintln!("go not installed — skipping");
        return;
    }
    use harnessd::engine::MockEngine;
    use harnessd::plugins::PluginSpec;
    use harnessd::server::Supervisor;
    use harnessd::timebus::Log;
    use harnessd::protocol::Frame;

    let root = repo_root();
    let plugin_dir = root.join("plugins/example-echo");
    let mut bin = std::env::temp_dir();
    bin.push(format!("example-echo-deny-{}", std::process::id()));
    let build = Command::new("go")
        .current_dir(&plugin_dir)
        .args(["build", "-o"])
        .arg(&bin)
        .arg(".")
        .output()
        .expect("run go build");
    assert!(build.status.success(), "go build: {}", String::from_utf8_lossy(&build.stderr));

    let engine = MockEngine::new(|method, _| match method {
        "thread/start" => json!({"thread": {"id": "thr_mock"}}),
        _ => json!({}),
    });
    let mut log_path = std::env::temp_dir();
    log_path.push(format!("deny-{}.ndjson", std::process::id()));
    let log = Log::open(&log_path).expect("open log");
    let mut sup = Supervisor::new(engine, log, 128_000);
    sup.plugins
        .load(&PluginSpec::new("example-echo", bin.to_str().unwrap(), Vec::new()))
        .expect("load plugin");

    // 引擎在命令执行前问我们；插件的声音必须能改变答案。
    sup.engine.emit_request(
        7,
        "item/commandExecution/requestApproval",
        json!({ "command": "rm -rf / --no-preserve-root", "commandActions": [{"type": "unknown"}] }),
    );
    let _ = sup.pump().expect("pump");

    let answers = sup.engine.responses();
    let declined = answers.iter().any(|(_, v)| v.get("decision").and_then(|d| d.as_str()) == Some("decline"));
    assert!(declined, "plugin deny did not turn into a wire-level decline: {answers:?}");

    let logged = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(logged.contains("guard.deny"), "kernel did not record the denial: {logged}");
    assert!(logged.contains("plugin"), "denial did not name its source: {logged}");

    // 「并继续」：同一进程里下一条命令仍然照常讨论。
    sup.engine
        .emit_request(8, "item/commandExecution/requestApproval", json!({ "command": "ls -la", "commandActions": [{"type": "listFiles"}] }));
    let _ = sup.pump().expect("pump again");
    let answers = sup.engine.responses();
    assert!(
        answers.iter().any(|(_, v)| v.get("decision").and_then(|d| d.as_str()) == Some("accept")),
        "kernel stopped answering after a denial: {answers:?}"
    );

    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&bin);
}
