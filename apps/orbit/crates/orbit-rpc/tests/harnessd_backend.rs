//! 接缝①的真链路：orbit 的 `PiClient` → `harnessd` 子进程 → 引擎。
//!
//! 需要外部环境（缺任何一个就跳过，不让 CI 变红）：
//!   HARNESSD_BIN   harnessd 可执行文件
//!   HARNESSD_ENGINE 引擎可执行文件（如 codewhale）
//!   HARNESSD_ENGINE_ARGS 引擎参数（如 "app-server"）
//!
//! 断言的是**方言对齐**：orbit 的 `CommandBody` 写成一行 JSON，harnessd 用
//! `--compat orbit` 读它，回一行 `{"type":"response",…}`，orbit 的
//! `Event::parse_line` 再把它读回 `Event::Response`。任何一段错位都会失败。

use std::path::PathBuf;
use std::time::Duration;

use orbit_rpc::client::PiClient;
use orbit_rpc::types::{CommandBody, Event};

#[test]
fn orbit_talks_to_harnessd_over_the_orbit_dialect() {
    let (Ok(bin), Ok(engine)) = (
        std::env::var("HARNESSD_BIN"),
        std::env::var("HARNESSD_ENGINE"),
    ) else {
        eprintln!("skipped: HARNESSD_BIN / HARNESSD_ENGINE not set");
        return;
    };
    std::env::set_var("HARNESSD_BIN", &bin);
    std::env::set_var("HARNESSD_ENGINE", &engine);

    let ws: PathBuf = std::env::temp_dir().join(format!("orbit-harnessd-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();

    let client = PiClient::spawn(&ws, None).expect("spawn client");
    let (id, rx) = client.send(CommandBody::GetAvailableModels).expect("send");
    assert!(!id.is_empty());

    let event = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("harnessd answered nothing within 60s");
    match event {
        Event::Response { success, data, error, .. } => {
            assert!(success, "harnessd reported failure: {error:?}");
            assert!(data.is_some(), "response carried no data");
        }
        other => panic!("expected a Response, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&ws);
}
