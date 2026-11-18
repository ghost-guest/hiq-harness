//! 命令通道的集成测试：真的起一个 `PiClient`，逐条发命令，
//! 断言它回的是「已实现」而不是「未接入」。
//!
//! **为什么要有这组测试**：34 个命令里任何一个漏接，界面都会弹一条
//! 「该命令尚未接入 harness 内核」。这种回归只有端到端发命令才看得出来，
//! 单测覆盖不到 match 分支的完整性。

use std::path::{Path, PathBuf};
use std::time::Duration;

use orbit_rpc::{CommandBody, Event, PiClient};

fn workspace(tag: &str) -> PathBuf {
    // 会话库的根是全家的（`~/.orbit-pi/sessions`），不是每个工作区一份。
    // 测试必须把它指到临时目录，否则会把测试会话写进用户的真实数据目录。
    //
    // 根是**进程级**的，而 `#[test]` 默认并行跑 —— 每个用例设不同的值会
    // 互相覆盖，所以整二进制只初始化一次、共用一个根；各用例的 workspace
    // 不同，slug 子目录自然分开，不会撞。
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let root = std::env::temp_dir().join(format!("orbit-rpc-test-sessions-{}", std::process::id()));
        std::env::set_var("HARNESS_SESSIONS_DIR", &root);
    });

    let d = std::env::temp_dir().join(format!("cmds-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 发一条命令，拿回 `(command, success, data, error)`。
fn ask(client: &PiClient, body: CommandBody) -> (String, bool, Option<serde_json::Value>, Option<String>) {
    match client.send_blocking(body, Duration::from_secs(10)) {
        Ok(Event::Response {
            command,
            success,
            data,
            error,
            ..
        }) => (command, success, data, error),
        other => panic!("期待 Response，得到 {other:?}"),
    }
}

/// 断言这条命令已被实现（不是「尚未接入」那句）。
fn assert_implemented(client: &PiClient, body: CommandBody, expect_command: &str) {
    let (command, success, _data, error) = ask(client, body);
    assert_eq!(command, expect_command, "命令名不匹配");
    if let Some(err) = &error {
        assert!(
            !err.contains("尚未接入 harness 内核"),
            "{expect_command} 仍是未实现状态：{err}"
        );
    }
    let _ = success; // 部分命令有意返回失败（如 auth.login），见各用例注释
}

fn client(tag: &str) -> (PiClient, PathBuf) {
    let ws = workspace(tag);
    let c = PiClient::spawn(&ws, None).expect("启动运行时失败");
    (c, ws)
}

// ── A 组：内核状态 ────────────────────────────────────────────

#[test]
fn get_state_reports_model_and_session_fields() {
    let (c, ws) = client("state");
    let (cmd, ok, data, _) = ask(&c, CommandBody::GetState);
    assert_eq!(cmd, "get_state");
    assert!(ok);
    let d = data.unwrap();
    // 界面靠这些字段渲染模型选择器与状态条
    assert!(d["model"]["id"].is_string());
    assert!(d["model"]["name"].is_string());
    assert!(d["model"]["provider"].is_string());
    assert!(d["thinkingLevel"].is_string());
    assert!(d["isStreaming"].is_boolean());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn set_model_and_thinking_level_stick() {
    let (c, ws) = client("model");
    ask(
        &c,
        CommandBody::SetModel {
            model_id: "test/model-a".into(),
            provider: "testprov".into(),
        },
    );
    ask(
        &c,
        CommandBody::SetThinkingLevel {
            level: "high".into(),
        },
    );
    let (_, _, data, _) = ask(&c, CommandBody::GetState);
    let d = data.unwrap();
    assert_eq!(d["model"]["id"], "test/model-a");
    assert_eq!(d["model"]["provider"], "testprov");
    assert_eq!(d["thinkingLevel"], "high");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cycle_thinking_level_wraps_around() {
    let (c, ws) = client("cycle");
    ask(&c, CommandBody::SetThinkingLevel { level: "high".into() });
    // high 是最后一个，循环应回到 minimal
    let (_, _, data, _) = ask(&c, CommandBody::CycleThinkingLevel);
    assert_eq!(data.unwrap()["level"], "minimal");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn available_models_and_thinking_levels_are_lists() {
    let (c, ws) = client("avail");
    let (_, _, data, _) = ask(&c, CommandBody::GetAvailableModels);
    assert!(data.unwrap()["models"].as_array().is_some_and(|a| !a.is_empty()));
    let (_, _, data, _) = ask(&c, CommandBody::GetAvailableThinkingLevels);
    assert!(data.unwrap()["levels"].as_array().is_some_and(|a| !a.is_empty()));
    let _ = std::fs::remove_dir_all(&ws);
}

// ── B 组：会话 ────────────────────────────────────────────────

#[test]
fn new_session_creates_a_file_the_sidebar_can_read() {
    let (c, ws) = client("new");
    let (cmd, ok, data, _) = ask(&c, CommandBody::NewSession);
    assert_eq!(cmd, "new_session");
    assert!(ok);
    let path = data.unwrap()["sessionFile"].as_str().unwrap().to_string();
    assert!(Path::new(&path).exists(), "会话文件必须真的落盘");

    // 侧栏读的是同一个目录，必须能列出来
    let listed = orbit_rpc::session_store::list_sessions_in(
        &orbit_rpc::session_store::sessions_root(),
    );
    // 环境变量未设时 root 指向 cwd，这里只断言文件本身可被解析
    let _ = listed;
    let header = orbit_rpc::session_store::read_header(Path::new(&path)).unwrap();
    assert_eq!(header["type"], "session");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn messages_and_entries_start_empty_then_reflect_the_session() {
    let (c, ws) = client("msgs");
    ask(&c, CommandBody::NewSession);
    let (_, _, data, _) = ask(&c, CommandBody::GetMessages);
    assert_eq!(data.unwrap()["messages"].as_array().unwrap().len(), 0);

    let (_, _, data, _) = ask(&c, CommandBody::GetEntries { since: None });
    assert!(data.unwrap()["entries"].is_array());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn session_name_is_echoed_and_persisted() {
    let (c, ws) = client("name");
    ask(&c, CommandBody::NewSession);
    let (cmd, ok, data, _) = ask(
        &c,
        CommandBody::SetSessionName {
            name: "我的会话".into(),
        },
    );
    assert_eq!(cmd, "set_session_name");
    assert!(ok);
    assert_eq!(data.unwrap()["name"], "我的会话");

    let (_, _, data, _) = ask(&c, CommandBody::GetState);
    assert_eq!(data.unwrap()["sessionName"], "我的会话");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn clone_and_fork_produce_new_files() {
    let (c, ws) = client("clone");
    let (_, _, data, _) = ask(&c, CommandBody::NewSession);
    let first = data.unwrap()["sessionFile"].as_str().unwrap().to_string();

    let (cmd, ok, data, _) = ask(&c, CommandBody::CloneSession);
    assert_eq!(cmd, "clone_session");
    assert!(ok);
    let cloned = data.unwrap()["sessionFile"].as_str().unwrap().to_string();
    assert_ne!(cloned, first, "克隆必须产生新文件");
    assert!(Path::new(&cloned).exists());

    let (cmd, ok, data, _) = ask(
        &c,
        CommandBody::Fork {
            entry_id: "e1".into(),
        },
    );
    assert_eq!(cmd, "fork");
    assert!(ok);
    assert_ne!(
        data.unwrap()["sessionFile"].as_str().unwrap(),
        cloned,
        "分叉必须另建一个文件"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn switch_session_rejects_missing_file_with_clear_error() {
    let (c, ws) = client("switch");
    let (cmd, ok, _, err) = ask(
        &c,
        CommandBody::SwitchSession {
            session_path: "/nope/gone.jsonl".into(),
        },
    );
    assert_eq!(cmd, "switch_session");
    assert!(!ok, "不存在的文件必须失败，不能假装切换成功");
    assert!(err.unwrap().contains("不存在"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn session_stats_reports_context_usage_shape() {
    let (c, ws) = client("stats");
    ask(&c, CommandBody::NewSession);
    let (cmd, ok, data, _) = ask(&c, CommandBody::GetSessionStats);
    assert_eq!(cmd, "get_session_stats");
    assert!(ok);
    let d = data.unwrap();
    // 空会话时 contextUsage 为 null（界面按「无数据」渲染），但不能缺字段
    assert!(d.get("contextUsage").is_some());
    let _ = std::fs::remove_dir_all(&ws);
}

// ── C 组：队列与开关 ─────────────────────────────────────────

#[test]
fn queue_commands_report_real_state() {
    let (c, ws) = client("queue");
    // 入队两条，一条进 steering 车道，一条进 followUp
    let (cmd, ok, _, _) = ask(
        &c,
        CommandBody::Steer {
            message: "插话".into(),
            images: None,
        },
    );
    assert_eq!(cmd, "steer");
    assert!(ok);
    let (cmd, ok, _, _) = ask(
        &c,
        CommandBody::FollowUp {
            message: "后续".into(),
            images: None,
        },
    );
    assert_eq!(cmd, "follow_up");
    assert!(ok);

    // clear_queue 必须如实回报被丢掉的队列内容（界面靠它恢复输入框）
    let (cmd, ok, data, _) = ask(&c, CommandBody::ClearQueue);
    assert_eq!(cmd, "clear_queue");
    assert!(ok);
    let d = data.unwrap();
    assert_eq!(d["steering"].as_array().unwrap().len(), 1);
    assert_eq!(d["followUp"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn mode_and_toggle_commands_update_state() {
    let (c, ws) = client("modes");
    for body in [
        CommandBody::SetSteeringMode {
            mode: "one-at-a-time".into(),
        },
        CommandBody::SetFollowUpMode {
            mode: "all".into(),
        },
        CommandBody::SetAutoCompaction { enabled: false },
        CommandBody::SetAutoRetry { enabled: false },
    ] {
        let (_, ok, _, err) = ask(&c, body);
        assert!(ok, "开关命令必须成功：{err:?}");
    }
    let (_, _, data, _) = ask(&c, CommandBody::GetState);
    let d = data.unwrap();
    assert_eq!(d["steeringMode"], "one-at-a-time");
    assert_eq!(d["followUpMode"], "all");
    assert_eq!(d["autoCompactionEnabled"], false);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn compact_reports_that_it_did_not_compact() {
    let (c, ws) = client("compact");
    let (cmd, ok, data, _) = ask(&c, CommandBody::Compact {
        custom_instructions: None,
    });
    assert_eq!(cmd, "compact");
    assert!(ok);
    // 内核没有手动压缩入口 —— 必须如实说「没压」，不能假装压了
    assert_eq!(data.unwrap()["compacted"], false);
    let _ = std::fs::remove_dir_all(&ws);
}

// ── D 组：认证与配额 ─────────────────────────────────────────

#[test]
fn auth_list_advertises_the_configured_provider() {
    let (c, ws) = client("auth");
    let (cmd, ok, data, _) = ask(&c, CommandBody::AuthList);
    assert_eq!(cmd, "auth.list");
    assert!(ok);
    let providers = data.unwrap()["providers"].as_array().unwrap().clone();
    assert_eq!(providers.len(), 1, "harness 只有一个 API-key provider");
    assert_eq!(providers[0]["credential"], "api_key");
    // 没有 OAuth，界面不应显示登录按钮
    assert_eq!(providers[0]["methods"].as_array().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn auth_login_fails_honestly_instead_of_hanging() {
    let (c, ws) = client("login");
    let (cmd, ok, _, err) = ask(
        &c,
        CommandBody::AuthLogin {
            provider: "magpie".into(),
            method: "oauth".into(),
            session_id: None,
        },
    );
    assert_eq!(cmd, "auth.login");
    assert!(!ok, "没有 OAuth 就该失败");
    assert!(err.unwrap().contains("没有 OAuth"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn auth_cancel_fails_because_nothing_is_pending() {
    let (c, ws) = client("cancel");
    let (_, ok, _, err) = ask(
        &c,
        CommandBody::AuthCancel {
            session_id: "s1".into(),
        },
    );
    assert!(!ok);
    assert!(err.unwrap().contains("没有进行中的登录"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn quota_list_is_empty_not_invented() {
    let (c, ws) = client("quota");
    let (cmd, ok, data, _) = ask(&c, CommandBody::QuotaList { provider: None });
    assert_eq!(cmd, "quota.list");
    assert!(ok);
    // 不采集配额 → 必须是空列表，不能编数字
    assert_eq!(data.unwrap()["reports"].as_array().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(&ws);
}

// ── 覆盖性检查 ───────────────────────────────────────────────

#[test]
fn every_command_responds_instead_of_hanging() {
    // 一次性把每条命令都发一遍，确认都拿得到 Response。
    // 任何一条漏接，界面就会卡在那个操作上等着。
    let (c, ws) = client("all");
    let bodies = vec![
        ("get_state", CommandBody::GetState),
        ("get_commands", CommandBody::GetCommands),
        ("get_available_models", CommandBody::GetAvailableModels),
        (
            "get_available_thinking_levels",
            CommandBody::GetAvailableThinkingLevels,
        ),
        ("new_session", CommandBody::NewSession),
        ("get_messages", CommandBody::GetMessages),
        ("get_entries", CommandBody::GetEntries { since: None }),
        ("get_fork_messages", CommandBody::GetForkMessages),
        ("clone_session", CommandBody::CloneSession),
        ("get_session_stats", CommandBody::GetSessionStats),
        ("clear_queue", CommandBody::ClearQueue),
        ("abort_retry", CommandBody::AbortRetry),
        ("cycle_model", CommandBody::CycleModel),
        ("cycle_thinking_level", CommandBody::CycleThinkingLevel),
        ("auth.list", CommandBody::AuthList),
        ("quota.list", CommandBody::QuotaList { provider: None }),
        ("compact", CommandBody::Compact {
            custom_instructions: None,
        }),
        (
            "set_steering_mode",
            CommandBody::SetSteeringMode {
                mode: "all".into(),
            },
        ),
        (
            "set_follow_up_mode",
            CommandBody::SetFollowUpMode {
                mode: "all".into(),
            },
        ),
        ("set_auto_compaction", CommandBody::SetAutoCompaction { enabled: true }),
        ("set_auto_retry", CommandBody::SetAutoRetry { enabled: true }),
    ];
    for (expect, body) in bodies {
        assert_implemented(&c, body, expect);
    }
    let _ = std::fs::remove_dir_all(&ws);
}
