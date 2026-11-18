//! 端到端：连续两轮对话，验证「记得上文」与「流式增量」。
//!
//! 上一版的缺陷：每轮只把当前这条消息发给内核，历史为空 ——
//! 模型不记得上一轮，请求前缀只有几十 token，缓存也永远不命中。
//! 这个 example 同时验证：
//!   1. 第二轮能引用第一轮说的内容（历史真的进去了）；
//!   2. 出现了 `text_delta`（流式增量，不再等整包）；
//!   3. 出现了 `message_end`（界面能算出 elapsed，不再只显示「已完成」）。
use std::time::{Duration, Instant};

use orbit_rpc::{CommandBody, Event, PiClient};

#[derive(Default)]
struct Captured {
    assistant: String,
    deltas: usize,
    ends: usize,
}

fn main() {
    let ws = std::env::temp_dir().join(format!("e2e-hist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).unwrap();

    let client = PiClient::spawn(&ws, None).expect("启动失败");

    // 先把事件通道取在手上（主线程收，不跨线程）。
    let rx = client.events();

    // 建会话
    let session_path = match client.send_blocking(CommandBody::NewSession, Duration::from_secs(10))
    {
        Ok(Event::Response { data: Some(d), .. }) => {
            d["sessionFile"].as_str().map(str::to_string)
        }
        other => panic!("新建会话失败: {other:?}"),
    };
    println!("会话文件: {session_path:?}");

    // 发一条任务，然后在主线程上收事件直到这一轮结束。
    let mut run = |task: &str, cap: &mut Captured| {
        match client.send_blocking(
            CommandBody::Prompt {
                message: task.to_string(),
                images: None,
                streaming_behavior: None,
            },
            Duration::from_secs(20),
        ) {
            Ok(Event::Response { success, error, .. }) => {
                assert!(success, "prompt 失败: {error:?}")
            }
            other => panic!("期待 Response，得到 {other:?}"),
        }

        cap.assistant.clear();
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut ended = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Event::MessageUpdate {
                    assistant: Some(orbit_rpc::types::AssistantMessageEvent::TextDelta { delta }),
                    ..
                }) => {
                    cap.deltas += 1;
                    cap.assistant.push_str(&delta);
                }
                Ok(Event::MessageEnd { .. }) => {
                    cap.ends += 1;
                    ended = true;
                    // message_end 之后 settle，再等一下收干净
                    break;
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if ended {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(ended, "这一轮没有 message_end");
    };

    let mut cap = Captured::default();

    println!("\n--- 第一轮 ---");
    run("请记住这个暗号：BLUE-7749。只回复「记住了」三个字。", &mut cap);
    let r1 = cap.assistant.clone();
    println!("回复1: {r1}");

    println!("\n--- 第二轮 ---");
    run("我刚才让你记住的暗号是什么？只回复暗号本身。", &mut cap);
    let r2 = cap.assistant.clone();
    println!("回复2: {r2}");

    println!("\n=== 验证 ===");
    println!("流式增量事件: {}（>0 说明真流式）", cap.deltas);
    println!("message_end 事件: {}（>0 说明界面能算 elapsed）", cap.ends);
    let remembered = r2.contains("BLUE");
    println!("记得上文: {}", if remembered { "是 ✅" } else { "否 ❌" });

    if let Some(path) = &session_path {
        let body = std::fs::read_to_string(path).unwrap_or_default();
        let bad = body
            .lines()
            .filter(|l| serde_json::from_str::<serde_json::Value>(l).is_err())
            .count();
        println!("会话文件 {} 行，坏行 {}", body.lines().count(), bad);
        assert_eq!(bad, 0, "会话文件有坏行");
    }

    assert!(cap.deltas > 0, "没有收到流式增量");
    assert!(remembered, "第二轮没记住第一轮的内容");

    let _ = std::fs::remove_dir_all(&ws);
    println!("\n全部通过");
}
