//! relay 的端到端测试：**先有验收，再动实现**。
//!
//! 背景：`crates/cli/src/server.rs` 里同端口既服务 WebSocket 又服务普通 HTTP，
//! 而 HTTP 那一半是我们自己解析的（读请求行/头/体、拼 `HTTP/1.1 200 OK`）。
//! 要换成 `tiny_http` 之前，得先有东西证明"现在是什么行为、换完没变"——
//! 否则改完只能说"看起来还行"。
//!
//! 这条测试覆盖两件事：
//!   1. 普通 HTTP：`GET /health` 回 200 且带 JSON
//!   2. WebSocket：握手 → 发 hello → 收到带 device_id 的 hello_ack
use harness_cli::host_service::HostState;
use harness_cli::server::{ServeConfig, Server};
use harness_core::wire::{Envelope, EnvelopeKind};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn start_relay(port: u16) -> Server {
    let state = Arc::new(Mutex::new(HostState::default()));
    let host_guard = Arc::new(std::sync::Mutex::new(harness_cli::host_service::DeviceRegistry::default()));
    let guard = host_guard.clone();
    let handler = move |env: Envelope, from: &str| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut g = guard.lock().unwrap();
        harness_cli::host_service::handle(env, from, &mut g, now)
    };
    let cfg = ServeConfig { bind: "127.0.0.1".into(), port };
    let server = Server::serve(cfg, state, handler).expect("start relay");
    // 起来之前端口可能还没 listen 完
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    server
}

#[test]
fn http_health_returns_json() {
    let port = free_port();
    let _server = start_relay(port);
    let resp = reqwest::blocking::get(format!("http://127.0.0.1:{port}/health"))
        .expect("GET /health");
    assert_eq!(resp.status().as_u16(), 200, "健康检查必须 200");
    let text = resp.text().unwrap_or_default();
    assert!(text.contains("{"), "健康检查要回 JSON，实际：{text}");
}

#[test]
fn websocket_hello_gets_a_device_id() {
    let port = free_port();
    let _server = start_relay(port);

    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _resp) = tungstenite::connect(&url).expect("WS 握手");

    let hello = Envelope::new(EnvelopeKind::Hello).with_payload(serde_json::json!({
        "device_name": "relay-e2e",
        "platform": "test",
        "app_version": "0.0.0",
    }));
    ws.send(tungstenite::Message::Text(hello.encode().unwrap().into()))
        .expect("发 hello");

    let msg = ws.read().expect("读 hello_ack");
    let text = match msg {
        tungstenite::Message::Text(t) => t.to_string(),
        other => panic!("期望文本帧，收到 {other:?}"),
    };
    let env = Envelope::decode(&text).expect("解码回执");
    assert_eq!(env.kind, EnvelopeKind::HelloAck, "回的必须是 hello_ack");
    assert!(text.contains("device_id"), "hello_ack 必须带 device_id：{text}");
}
