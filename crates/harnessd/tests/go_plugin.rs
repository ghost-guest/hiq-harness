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
