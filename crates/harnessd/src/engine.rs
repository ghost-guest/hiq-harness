//! Seam ②: the engine.
//!
//! `JsonRpcEngine` talks to a codewhale app-server over line-delimited JSON on
//! stdio. Deliberately *not* a library dependency: a 558k-line single-crate engine
//! compiled into our binary would make every upstream bump a rebuild of our tree.
//! `MockEngine` is a scripted stand-in so the supervisor's tests never need a
//! model, a network, or an API key.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use crate::protocol::{self, Frame, Kind};

pub trait Engine {
    fn request(&mut self, method: &str, params: Value) -> Result<Value>;
    /// Answer a reverse request (approval) the engine is blocked on.
    fn respond(&mut self, id: u64, result: Value) -> Result<()>;
    /// Non-blocking drain of notifications seen since the last call.
    fn notifications(&mut self) -> Vec<Frame>;
    fn name(&self) -> String;
}

pub struct JsonRpcEngine {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    inbox: VecDeque<Frame>,
    next_id: u64,
    timeout: Duration,
    /// How long to wait for the first notification after a reply. A response and
    /// the notifications that follow it arrive as separate lines; draining
    /// non-blockingly would strand every one of them.
    notify_grace: Duration,
    /// How long a gap ends the drain. Frames of one turn are written back to
    /// back, but the last one still lands a few microseconds after the rest.
    notify_idle: Duration,
}

impl JsonRpcEngine {
    pub fn spawn(program: &str, args: &[String]) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn engine `{program}`"))?;

        let stdin = child.stdin.take().context("engine stdin")?;
        let stdout = child.stdout.take().context("engine stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("engine-reader".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            child,
            stdin,
            rx,
            inbox: VecDeque::new(),
            next_id: 1,
            timeout: Duration::from_secs(600),
            notify_grace: Duration::from_millis(50),
            notify_idle: Duration::from_millis(20),
        })
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    fn read_frame(&mut self) -> Result<Frame> {
        let line = self.rx.recv_timeout(self.timeout).map_err(|e| match e {
            RecvTimeoutError::Timeout => anyhow::anyhow!("engine timed out"),
            RecvTimeoutError::Disconnected => anyhow::anyhow!("engine closed the pipe"),
        })?;
        protocol::decode(&line)
    }
}

impl Engine for JsonRpcEngine {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let line = protocol::encode(&Frame::request(id, method, params));
        self.stdin.write_all(line.as_bytes()).context("write to engine")?;
        self.stdin.flush()?;

        loop {
            let frame = self.read_frame()?;
            match frame.kind() {
                Kind::Response if frame.id == Some(id) => {
                    if let Some(err) = frame.error {
                        bail!("engine error {}: {}", err.code, err.message);
                    }
                    return Ok(frame.result.unwrap_or(Value::Null));
                }
                _ => self.inbox.push_back(frame),
            }
        }
    }

    fn respond(&mut self, id: u64, result: Value) -> Result<()> {
        let line = protocol::encode(&Frame::response(id, result));
        self.stdin.write_all(line.as_bytes()).context("write response")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn notifications(&mut self) -> Vec<Frame> {
        // Anything the engine wrote after the response we were waiting for is
        // still in the pipe. Wait once (bounded) so a reply's trailing events are
        // not lost, then drain whatever else is ready.
        if self.inbox.is_empty() {
            if let Ok(line) = self.rx.recv_timeout(self.notify_grace) {
                if let Ok(frame) = protocol::decode(&line) {
                    self.inbox.push_back(frame);
                }
            }
        }
        // Drain until the pipe goes quiet: one turn's events are several writes,
        // and stopping after a single read loses the last of them.
        while self.inbox.len() < 512 {
            match self.rx.recv_timeout(self.notify_idle) {
                Ok(line) => {
                    if let Ok(frame) = protocol::decode(&line) {
                        self.inbox.push_back(frame);
                    }
                }
                Err(_) => break,
            }
        }
        self.inbox.drain(..).collect()
    }

    fn name(&self) -> String {
        format!("jsonrpc:{}", self.child.id())
    }
}

impl Drop for JsonRpcEngine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Scripted engine. Every method it does not know returns `{}` so a new mapping
/// does not break existing tests.
pub struct MockEngine {
    calls: Vec<String>,
    /// (方法, 参数) —— `calls` 只留方法名，参数以前被直接丢掉。
    /// 丢参数的代价：测试替身**永远看不到**发给引擎的 threadId / key 叫什么，
    /// 于是"读错了线程""key 名对不上"这类投影 bug 单测全抓不到（真踩过）。
    requests: Vec<(String, Value)>,
    outbox: VecDeque<Frame>,
    responses: Vec<(u64, Value)>,
    reply: Box<dyn Fn(&str, &Value) -> Value + Send>,
}

impl Default for MockEngine {
    fn default() -> Self {
        Self::new(|method, _params| match method {
            "thread/start" => json!({ "thread": { "id": "thr_mock" } }),
            "thread/read" => json!({ "thread": { "id": "thr_mock", "turns": [] } }),
            "prompt/run" | "prompt/request" => json!({ "output": "mock", "model": "deepseek-flash", "events": [] }),
            "app/models" => json!({ "models": [ { "id": "deepseek-v4-pro" } ] }),
            "thread/fork" => json!({ "thread": { "id": "thr_fork" } }),
            "thread/items/list" => json!({ "items": [] }),
            "model/list" => json!({ "models": [ { "id": "deepseek-v4-pro" } ] }),
            _ => json!({}),
        })
    }
}

impl MockEngine {
    pub fn new(reply: impl Fn(&str, &Value) -> Value + Send + 'static) -> Self {
        Self {
            calls: Vec::new(),
            requests: Vec::new(),
            outbox: VecDeque::new(),
            responses: Vec::new(),
            reply: Box::new(reply),
        }
    }

    pub fn calls(&self) -> &[String] {
        &self.calls
    }

    /// 方法 + 参数。要看"发给引擎的 threadId 是不是新线程"，用这个。
    pub fn requests(&self) -> &[(String, Value)] {
        &self.requests
    }

    /// Answers this mock gave to reverse requests (approvals).
    pub fn responses(&self) -> &[(u64, Value)] {
        &self.responses
    }

    /// Push a notification the engine "emitted" while a turn was running.
    pub fn emit(&mut self, method: &str, params: Value) {
        self.outbox.push_back(Frame::notification(method, params));
    }

    /// Push a reverse request — the channel an engine uses to block on approval.
    pub fn emit_request(&mut self, id: u64, method: &str, params: Value) {
        self.outbox.push_back(Frame::request(id, method, params));
    }
}

impl Engine for MockEngine {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.calls.push(method.to_string());
        self.requests.push((method.to_string(), params.clone()));
        Ok((self.reply)(method, &params))
    }

    fn respond(&mut self, id: u64, result: Value) -> Result<()> {
        self.responses.push((id, result));
        Ok(())
    }

    fn notifications(&mut self) -> Vec<Frame> {
        self.outbox.drain(..).collect()
    }

    fn name(&self) -> String {
        "mock".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_records_calls_in_order() {
        let mut e = MockEngine::default();
        e.request("thread/start", json!({})).unwrap();
        e.request("turn/start", json!({})).unwrap();
        assert_eq!(e.calls(), ["thread/start", "turn/start"]);
    }

    #[test]
    fn notifications_drain_once() {
        let mut e = MockEngine::default();
        e.emit("item/agentMessage/delta", json!({"delta":"hi"}));
        assert_eq!(e.notifications().len(), 1);
        assert_eq!(e.notifications().len(), 0);
    }
}
