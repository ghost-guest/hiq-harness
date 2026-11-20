//! 接缝① 的真实验收：**orbit 的客户端 → harnessd → 真引擎（HTTP）**。
//!
//! 现有测试里，`render_tests` 用的是假 pi 子进程，`program_injection` 用的是脚本后端。
//! 两者都证明"事件能进来"，但证明不了"这条链能对着真引擎跑完一轮并把流式文本和工具
//! 调用接上"。这个 example 就是补这一格。
//!
//! 跑法（见 scripts/evidence.sh §6）：
//!   HARNESS_ENGINE_BIN=/path/to/codewhale HARNESSD_BIN=target/debug/harnessd \
//!   CODEWHALE_HOME=/tmp/some-home cargo run -p orbit-rpc --example seam1_real_engine
use orbit_rpc::client::PiClient;
use orbit_rpc::types::{AssistantMessageEvent, CommandBody, Event};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn env_required(key: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            println!("need {key}");
            std::process::exit(2);
        }
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().unwrap().port()
}

fn health(port: u16) -> bool {
    use std::io::{Read, Write};
    let mut s = match std::net::TcpStream::connect(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = write!(s, "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.contains("\"ok\"")
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() {
    let engine = env_required("HARNESS_ENGINE_BIN");
    let harnessd = env_required("HARNESSD_BIN");
    let home = env_required("CODEWHALE_HOME");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let child = Command::new(&engine)
        .args(["app-server", "--http", "--insecure-no-auth", "--port", &port.to_string()])
        .env("CODEWHALE_HOME", &home)
        .stdout(std::fs::File::create("/tmp/seam1-engine.log").unwrap())
        .stderr(std::fs::File::create("/tmp/seam1-engine.err").unwrap())
        .spawn()
        .expect("spawn engine");
    let _guard = Kill(child);

    let deadline = Instant::now() + Duration::from_secs(30);
    while !health(port) {
        if Instant::now() > deadline {
            println!("FAIL engine never became healthy on {base}");
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    // 工作区用临时目录：这一格验的是链路，不碰任何真实项目。
    let ws = PathBuf::from(std::env::temp_dir()).join(format!("seam1-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("mkdir ws");

    // orbit 启动 harnessd 时不带命令行参数，全靠环境变量——这正是界面那条路。
    std::env::set_var("HARNESSD_HTTP_ENGINE", &base);
    std::env::set_var("HARNESSD_BIN", &harnessd);
    std::env::set_var("HARNESSD_LOG", ws.join(".harness/timebus.ndjson").to_string_lossy().to_string());
    std::env::set_var(
        "HARNESSD_THREAD_BODY",
        r#"{"auto_approve":true,"model_provider_id":"magipe","model":"auto/deepseek-v4.1-flash"}"#,
    );

    let client = PiClient::spawn_with_bin(&ws, None).expect("spawn harnessd client");
    // 默认是这条链的最小验收题；`SEAM1_PROMPT` 可以换成任何题面，
    // 用来在同一链路上验更重的任务（比如"Rust 和 Go 各写一份快排"）。
    let prompt_owned = std::env::var("SEAM1_PROMPT").unwrap_or_else(|_| {
        "用 bash 工具执行 echo seam1，把输出原样贴出来，然后结束。".to_string()
    });
    let prompt = prompt_owned.as_str();
    client
        .send(CommandBody::Prompt { message: prompt.to_string(), images: None, streaming_behavior: None })
        .expect("send prompt");

    let mut text = String::new();
    let mut tools: Vec<String> = Vec::new();
    let mut saw_end = false;
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        match client.events().recv_timeout(Duration::from_secs(5)) {
            Ok(Event::MessageUpdate { assistant: Some(AssistantMessageEvent::TextDelta { delta }), .. }) => {
                text.push_str(&delta);
            }
            Ok(Event::MessageUpdate { assistant: Some(AssistantMessageEvent::ToolcallStart { value }), .. }) => {
                let name = value.get("toolName").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                tools.push(name);
            }
            Ok(Event::ToolExecutionStart { value }) => {
                let name = value
                    .get("toolName")
                    .or_else(|| value.get("tool"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                if !tools.contains(&name) {
                    tools.push(name);
                }
            }
            Ok(Event::AgentEnd { .. }) => {
                saw_end = true;
                break;
            }
            Ok(_) => {}
            Err(_) => {
                if saw_end {
                    break;
                }
            }
        }
    }

    println!("文本长度: {}", text.chars().count());
    println!("文本片段: {:?}", text.chars().take(80).collect::<String>());
    println!("工具调用: {tools:?}");
    println!("agent_end: {saw_end}");

    let mut problems = Vec::new();
    if text.trim().is_empty() {
        problems.push("没有收到任何流式文本");
    }
    if tools.is_empty() {
        problems.push("没有收到任何工具调用事件");
    }
    // 只有事件、没有名字，等于界面上空一行——这是"看起来接上了"的典型假绿。
    if tools.iter().any(|t| t == "?") || (tools.is_empty() && text.is_empty()) {
        problems.push("工具事件里没有工具名（界面会渲染成空行）");
    }
    if !saw_end {
        problems.push("没有收到 agent_end（这一轮没收尾）");
    }
    if !problems.is_empty() {
        println!("FAIL 接缝① 真引擎链路：{}", problems.join("；"));
        std::process::exit(1);
    }
    println!("PASS 接缝① 真引擎：orbit 客户端 → harnessd → 引擎，流式文本 + 工具调用 + 收尾都在");
    let _ = std::fs::remove_dir_all(&ws);
}
