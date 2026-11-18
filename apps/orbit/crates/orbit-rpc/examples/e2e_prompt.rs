//! 端到端：起真运行时、发一个真任务、看事件流与落盘。
//!
//! 这是「界面能不能用」的最后一层证据 —— 界面里的输入没法自动化
//! （macOS 拦第三方 keystroke），所以直接驱动同一个 `PiClient`。
use std::time::Duration;

use orbit_rpc::{CommandBody, Event, PiClient};

fn main() {
    let ws = std::env::temp_dir().join(format!("e2e-{}", std::process::id()));
    // 保留现场便于排查
    println!("会话目录保留在：{}", ws.display());
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();

    let client = PiClient::spawn(&ws, None).expect("启动失败");
    let (_, ok, data, err) = match client.send_blocking(CommandBody::NewSession, Duration::from_secs(10)) {
        Ok(Event::Response { success, data, error, .. }) => ((), success, data, error),
        other => panic!("{other:?}"),
    };
    println!("new_session ok={ok} err={err:?}");
    let path = data.unwrap()["sessionFile"].as_str().unwrap().to_string();

    let rx = client.events();
    let task = "看 calc.py，add 函数有什么 bug？一句话说明。";
    let (_, ok, data, _) = match client.send_blocking(
        CommandBody::Prompt { message: task.into(), images: None, streaming_behavior: None },
        Duration::from_secs(10),
    ) {
        Ok(Event::Response { success, data, error, .. }) => ((), success, data, error),
        other => panic!("{other:?}"),
    };
    println!("prompt 已受理 ok={ok} disposition={:?}", data.map(|d| d["disposition"].clone()));

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut text = String::new();
    let mut saw_end = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(ev) => match ev {
                Event::MessageUpdate { assistant, .. } => {
                    if let Some(orbit_rpc::AssistantMessageEvent::TextDelta { delta }) = assistant {
                        text.push_str(&delta);
                    }
                }
                // 等 `agent_settled`：`agent_end` 之后还有收尾工作，
                // 包括把 assistant 回复写进会话文件。在 `agent_end` 就读
                // 文件会漏掉最后一行。
                Event::AgentSettled { .. } => { saw_end = true; break; }
                Event::ToolExecutionStart { value } => {
                    println!("[工具] {}", value["toolName"].as_str().unwrap_or("?"))
                }
                Event::ToolExecutionEnd { value } => println!(
                    "[工具结束] {} isError={}",
                    value["toolName"].as_str().unwrap_or("?"),
                    value["isError"]
                ),
                _ => {}
            },
            Err(_) => if saw_end { break },
        }
    }
    println!("--- 模型回答 ---\n{text}\n--- agent_end={saw_end} ---");

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<_> = content.lines().collect();
    println!("会话文件 {} 行：", lines.len());
    for (i, l) in lines.iter().enumerate() {
        match serde_json::from_str::<serde_json::Value>(l) {
            Ok(t) => println!("  {i}: type={}", t["type"]),
            Err(e) => println!("  {i}: 解析失败 {e} | 原文前 120 字符: {:?}", &l[..l.len().min(120)]),
        }
    }
    // 保留现场便于排查
    println!("会话目录保留在：{}", ws.display());
}
