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

/// 缺 Go 时**默认是失败**，不是跳过。
///
/// 这几个测试是 S4「插件层」唯一的机械证据。跳过 = 绿 = 没用装没装 Go 都显示通过，
/// 而这正是最危险的一种绿：读的人会以为插件层验过了。要显式跳过必须设
/// `HARNESS_SKIP_GO=1`，并且输出里会留一行脚印。
fn require_go(test: &str) {
    if go_available() {
        return;
    }
    if std::env::var("HARNESS_SKIP_GO").as_deref() == Ok("1") {
        eprintln!("[{test}] skipped on purpose: HARNESS_SKIP_GO=1 and no `go` in PATH");
        return;
    }
    panic!("[{test}] needs `go` in PATH: S4 的插件层证据只能由真插件产出。确实要跳过请设 HARNESS_SKIP_GO=1");
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn build_plugin(test: &str) -> Option<PathBuf> {
    if !go_available() {
        // 缺 Go：默认 panic；只有显式 HARNESS_SKIP_GO=1 才安静地跳过。
        require_go(test);
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
    let Some(bin) = build_plugin("go_plugin_guard") else { return };
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

    let chmod = decide(&mut host, "sudo chmod -R 777 /");
    assert_eq!(chmod["content"]["rule"], "root-chmod", "{chmod}");

    let etc = decide(&mut host, "echo 'x' > /etc/hosts");
    assert_eq!(etc["content"]["rule"], "etc-write", "{etc}");

    let reset = decide(&mut host, "git reset --hard HEAD~1");
    assert_eq!(reset["content"]["rule"], "hard-reset", "{reset}");

    // 正常命令必须放行：一个见谁都拦的守卫会把 agent 卡死。
    for ok in [
        "cargo test --workspace",
        "git push origin main",
        "cat .env",
        "cat /etc/hosts",
        "chmod 777 ./scripts",
        "git reset --soft HEAD~1",
        "rm -rf /tmp/scratch",
    ] {
        let verdict = decide(&mut host, ok);
        assert_eq!(verdict["content"]["decision"], "allow", "{ok} → {verdict}");
    }

    let rules = host
        .call_tool("safety.rules", json!({}))
        .expect("safety.rules is registered")
        .expect("safety.rules answers");
    assert_eq!(rules["content"]["rules"].as_array().map(Vec::len), Some(7), "{rules}");

    let _ = std::fs::remove_file(&bin);
}

/// 拒绝理由要能走到内核日志里——否则用户只看到"被拦"，不知道为什么。
#[test]
fn a_denial_reason_reaches_the_kernel_log() {
    let Some(bin) = build_plugin("go_plugin_guard") else { return };
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
    let Some(bin) = build_plugin("go_plugin_guard") else { return };
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
    assert_eq!(rules["content"]["rules"].as_array().map(Vec::len), Some(6), "{rules}");
    assert_eq!(
        rules["content"]["protected_env"][0], "secrets.yaml",
        "{rules}"
    );

    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&bin);
}
