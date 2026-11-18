//! 第二个真插件：`plugins/guard-safety`。
//!
//! 第一个（`example-echo`）证明"插件能拒绝一条命令"；这一个证明
//! **拒绝规则本身是站得住的**：force push、改 .env、删根、fork 炸弹，
//! 而且它给出的理由会一路走到内核日志里（用户看得到为什么被拦）。

use harnessd::plugins::{PluginHost, PluginSpec};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;

fn go_available() -> bool {
    Command::new("go")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn build_plugin() -> Option<PathBuf> {
    if !go_available() {
        eprintln!("go not installed — skipping");
        return None;
    }
    let dir = repo_root().join("plugins/guard-safety");
    assert!(dir.exists(), "missing {}", dir.display());
    let mut bin = std::env::temp_dir();
    bin.push(format!("guard-safety-{}", std::process::id()));
    let build = Command::new("go")
        .current_dir(&dir)
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
    Some(bin)
}

fn decide(host: &mut PluginHost, command: &str) -> serde_json::Value {
    host.call_tool("policy.check", json!({ "command": command }))
        .expect("policy.check is registered")
        .expect("policy.check answers")
}

#[test]
fn the_safety_guard_denies_the_four_dangerous_shapes() {
    let Some(bin) = build_plugin() else { return };
    let mut host = PluginHost::new();
    host.load(&PluginSpec::new(
        "guard-safety",
        bin.to_string_lossy(),
        vec![],
    ))
    .expect("plugin must load");

    let force = decide(&mut host, "git push --force origin main");
    assert_eq!(force["content"]["decision"], "deny", "{force}");
    assert_eq!(force["content"]["rule"], "force-push", "{force}");

    let dotenv = decide(&mut host, "printf 'A=1' > .env");
    assert_eq!(dotenv["content"]["decision"], "deny", "{dotenv}");
    assert_eq!(dotenv["content"]["rule"], "dotenv-write", "{dotenv}");

    let root = decide(&mut host, "sudo rm -rf / --no-preserve-root");
    assert_eq!(root["content"]["decision"], "deny", "{root}");
    assert_eq!(root["content"]["rule"], "root-delete", "{root}");

    let bomb = decide(&mut host, ":(){ :|:& };:");
    assert_eq!(bomb["content"]["decision"], "deny", "{bomb}");
    assert_eq!(bomb["content"]["rule"], "fork-bomb", "{bomb}");

    // 正常命令必须放行：一个见谁都拦的守卫会把 agent 卡死。
    for ok in ["cargo test --workspace", "git push origin main", "cat .env", "rm -rf /tmp/scratch"] {
        let verdict = decide(&mut host, ok);
        assert_eq!(verdict["content"]["decision"], "allow", "{ok} → {verdict}");
    }

    let rules = host
        .call_tool("safety.rules", json!({}))
        .expect("safety.rules is registered")
        .expect("safety.rules answers");
    assert_eq!(rules["content"]["rules"].as_array().map(Vec::len), Some(4), "{rules}");

    let _ = std::fs::remove_file(&bin);
}

/// 拒绝理由要能走到内核日志里——否则用户只看到"被拦"，不知道为什么。
#[test]
fn a_denial_reason_reaches_the_kernel_log() {
    let Some(bin) = build_plugin() else { return };
    use harnessd::engine::MockEngine;
    use harnessd::protocol::Frame;
    use harnessd::server::Supervisor;
    use harnessd::timebus::Log;

    let engine = MockEngine::new(|method, _| match method {
        "thread/start" => json!({ "thread": { "id": "thr_mock" } }),
        _ => json!({}),
    });
    let mut log_path = std::env::temp_dir();
    log_path.push(format!("guard-safety-{}.ndjson", std::process::id()));
    let log = Log::open(&log_path).expect("open log");
    let mut sup = Supervisor::new(engine, log, 128_000);
    sup.plugins
        .load(&PluginSpec::new("guard-safety", bin.to_string_lossy(), vec![]))
        .expect("load plugin");

    sup.engine.emit_request(
        11,
        "item/commandExecution/requestApproval",
        json!({ "command": "git push --force origin main", "commandActions": [{"type": "unknown"}] }),
    );
    let _ = sup.pump().expect("pump");

    let declined = sup
        .engine
        .responses()
        .iter()
        .any(|(_, v)| v.get("decision").and_then(|d| d.as_str()) == Some("decline"));
    assert!(declined, "the guard's deny did not become a wire decline");

    let logged = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(logged.contains("guard.deny"), "no denial in the log: {logged}");
    assert!(
        logged.contains("force"),
        "the denial did not carry the plugin's reason: {logged}"
    );
    let _ = Frame::notification("noop", json!({})); // keep the import honest

    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&bin);
}

/// 插件要能配置：一个跑起来的进程，换个环境变量就换一套规则。
/// 这条测的是"内核把配置送进去了"这件事，而不是插件自己怎么读。
#[test]
fn a_plugin_can_be_configured_without_rebuilding_it() {
    let Some(bin) = build_plugin() else { return };
    let mut cfg_path = std::env::temp_dir();
    cfg_path.push(format!("guard-safety-cfg-{}.json", std::process::id()));
    std::fs::write(&cfg_path, r#"{"disabled":["force-push"],"protected_env":["secrets.yaml"]}"#)
        .expect("write config");

    let mut host = PluginHost::new();
    host.load(
        &PluginSpec::new("guard-safety", bin.to_string_lossy(), vec![])
            .with_env("GUARD_SAFETY_CONFIG", cfg_path.to_string_lossy()),
    )
    .expect("plugin must load");

    // 被关掉的规则不再拦。
    let force = decide(&mut host, "git push --force origin main");
    assert_eq!(force["content"]["decision"], "allow", "{force}");

    // 没关的规则还在，新加的保护文件生效。
    let dotenv = decide(&mut host, "echo x > secrets.yaml");
    assert_eq!(dotenv["content"]["decision"], "deny", "{dotenv}");

    // 规则清单也跟着变，操作员能看到"现在到底还拦什么"。
    let rules = host
        .call_tool("safety.rules", json!({}))
        .expect("registered")
        .expect("answers");
    assert_eq!(rules["content"]["rules"].as_array().map(Vec::len), Some(3), "{rules}");
    assert_eq!(
        rules["content"]["protected_env"][0], "secrets.yaml",
        "{rules}"
    );

    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&bin);
}
