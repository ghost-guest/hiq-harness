//! 真引擎的审批通道探针（Rust 版，替代 `scripts/probe-approval.py`）。
//!
//! 这条通道曾经整条是坏的：id 硬编码 0、命令没摊平、词表抄错、重复投递。
//! 坏掉时内核不报"审批通道坏了"，它报的是 turn 死了——所以必须单独探。
//!
//! 流程：起引擎 → 建线程（auto_approve=false）→ 起一轮 → 等 approval 事件
//! → 用事件里的 id 答 deny → 断言引擎 200 且这一轮正常收尾。
//!
//! `PROBE_AUTO_APPROVE=1` 时线程自己批命令，探针**应该**报 FAIL——
//! 否则它就是个永远绿的空测试。
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 0");
    l.local_addr().unwrap().port()
}

fn call(base: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let url = format!("{base}{path}");
    let m = reqwest::Method::from_bytes(method.as_bytes())?;
    let mut req = client.request(m, &url);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req.send()?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok((status, v))
}

pub fn run(engine: &str, home: &str) -> Result<()> {
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = std::fs::File::create("/tmp/probe-approval-engine.log")?;
    let mut child: Child = Command::new(engine)
        .args(["app-server", "--http", "--insecure-no-auth", "--port", &port.to_string()])
        .env("CODEWHALE_HOME", home)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("spawn {engine}"))?;

    let result = (|| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if call(&base, "GET", "/healthz", None).map(|(c, _)| c == 200).unwrap_or(false) {
                break;
            }
            if Instant::now() > deadline {
                bail!("engine never became healthy");
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        let auto = std::env::var("PROBE_AUTO_APPROVE").as_deref() == Ok("1");
        let (code, thread) = call(
            &base,
            "POST",
            "/v1/threads",
            Some(&json!({
                "auto_approve": auto,
                "model_provider_id": std::env::var("HARNESS_PROVIDER").unwrap_or_else(|_| "magipe".into()),
                "model": std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "auto/deepseek-v4.1-flash".into()),
            })),
        )?;
        let tid = thread.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if code != 201 || tid.is_empty() {
            bail!("create thread → {code} {thread}");
        }

        // 先订阅再发回合：引擎的 SSE 是单订阅者，没有订阅者时回合会停住。
        let rx = subscribe(&base, &tid)?;
        std::thread::sleep(Duration::from_secs(1));

        let (code, _) = call(&base, "POST", &format!("/v1/threads/{tid}/turns"), Some(&json!({ "prompt": "运行 echo hi" })))?;
        if code != 200 && code != 201 {
            bail!("start turn → {code}");
        }

        let approval = wait_for(&rx, 60.0, |ev| {
            ev.get("kind").and_then(Value::as_str).map(|k| k.contains("approval")).unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("no approval.required arrived (auto_approve off?)"))?;

        let pl = approval.get("payload").cloned().unwrap_or(Value::Null);
        let aid = pl.get("approval_id").or_else(|| pl.get("id")).and_then(Value::as_str).unwrap_or_default().to_string();
        let tool = pl.get("tool_name").and_then(Value::as_str).unwrap_or_default().to_string();
        let cmd = pl.get("input").and_then(|i| i.get("command")).and_then(Value::as_str).unwrap_or_default().to_string();
        println!("approval: id={aid} tool={tool} command={cmd:?}");
        if aid.is_empty() || tool.is_empty() || cmd.is_empty() {
            bail!("the event is missing id/tool/command — this is what broke the channel before");
        }

        let (code, body) = call(&base, "POST", &format!("/v1/approvals/{aid}"), Some(&json!({ "decision": "deny" })))?;
        println!("POST /v1/approvals/{aid} {{'decision':'deny'}} → {code}");
        if code != 200 {
            bail!("engine refused the denial (wrong id or wrong word?): {body}");
        }

        let end = wait_for(&rx, 60.0, |ev| {
            matches!(
                ev.get("kind").and_then(Value::as_str),
                Some("turn.completed") | Some("turn.failed") | Some("turn.cancelled")
            )
        });
        let kind = end
            .as_ref()
            .and_then(|e| e.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_string();
        println!("turn end: {kind}");
        if kind != "turn.completed" {
            bail!("the turn did not finish cleanly after a denial ({kind})");
        }
        println!("PASS 审批通道：id 对、命令对、deny 被接受、这一轮正常收尾");
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    result
}

fn subscribe(base: &str, tid: &str) -> Result<Receiver<Value>> {
    let (tx, rx) = channel();
    let url = format!("{base}/v1/threads/{tid}/events");
    std::thread::spawn(move || {
        let Ok(resp) = reqwest::blocking::get(url) else { return };
        let reader = BufReader::new(resp);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Some(data) = line.strip_prefix("data:") else { continue };
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    Ok(rx)
}

fn wait_for<F: Fn(&Value) -> bool>(rx: &Receiver<Value>, secs: f32, pred: F) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs_f32(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        match rx.recv_timeout(left.min(Duration::from_millis(250))) {
            Ok(v) => {
                if pred(&v) {
                    return Some(v);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return None,
        }
    }
}
