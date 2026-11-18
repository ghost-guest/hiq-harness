//! `spawn_with_program` 应该和「真的启动一个 orbit-pi」一样工作：
//! 写进去一行命令，读回来一串事件。这里用一个 shell 脚本当后端。

use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

#[test]
fn a_scripted_backend_speaks_the_same_dialect() {
    let dir = std::env::temp_dir().join(format!("orbit-prog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-pi");
    std::fs::write(
        &bin,
        "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"prompt\"'*)\n      printf '%s\\n' '{\"type\":\"agent_start\"}'\n      printf '%s\\n' '{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"delta\":\"hi\"}}'\n      printf '%s\\n' '{\"type\":\"agent_end\",\"willRetry\":false}'\n      ;;\n  esac\ndone\n",
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let client = orbit_rpc::client::PiClient::spawn_with_program(
        bin.to_str().unwrap(),
        &dir,
        None,
    )
    .expect("spawn");

    client
        .send(orbit_rpc::types::CommandBody::Prompt {
            message: "hello".into(),
            images: None,
            streaming_behavior: None,
        })
        .expect("send prompt");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_text = false;
    while Instant::now() < deadline && !saw_text {
        if let Ok(ev) = client.events().recv_timeout(Duration::from_millis(300)) {
            if let orbit_rpc::types::Event::MessageUpdate { assistant: Some(a), .. } = ev {
                saw_text = matches!(
                    a,
                    orbit_rpc::types::AssistantMessageEvent::TextDelta { .. }
                );
            }
        }
    }
    assert!(saw_text, "the scripted backend's text delta never reached the client");
    let _ = std::fs::remove_dir_all(&dir);
}
