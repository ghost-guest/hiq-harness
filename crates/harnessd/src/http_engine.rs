//! Seam ② over HTTP: the only transport where the engine is *not* tool-free.
//!
//! Why this exists: `codewhale app-server --stdio` is a compatibility control
//! channel and rejects anything that needs tools or approvals
//! ("tool-free turn requests require explicit allowed_tools: []"). The runtime API
//! with tools, approvals and events is `app-server --http` on `/v1/*`.
//!
//! Verified against codewhale 0.10.3 by hand before writing this:
//!   GET  /healthz                    → {"status":"ok","protocol":"v2",…}
//!   GET  /v1/models                  → model catalogue
//!   POST /v1/threads {}              → {"id":"thr_…", "model":"deepseek-flash", …}
//!   POST /v1/threads/{id}/turns      → {"prompt":"…"} accepted; provider error if no key
//!   POST /v1/threads/{id}/turns/{t}/interrupt, …/steer, GET …/events (SSE)
//!
//! `curl` is used instead of an HTTP crate on purpose: this crate stays free of
//! network dependencies, and a supervisor that can be debugged with one shell
//! command is a supervisor that can be debugged at 3am.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::engine::Engine;
use crate::protocol::Frame;

pub struct HttpEngine {
    base: String,
    token: Option<String>,
    /// Extra fields for `POST /v1/threads` (`model`, `auto_approve`, `mode`,
    /// `permission_posture`). Comes from `HARNESSD_THREAD_BODY` so an operator
    /// can pick the posture without a code change — the runtime API rejects
    /// unknown keys loudly, so keeping this data-driven beats hardcoding.
    thread_body: Value,
    thread_id: Option<String>,
    last_turn: Option<String>,
    inbox: VecDeque<Frame>,
    sse: Option<(Child, Receiver<String>)>,
}

impl HttpEngine {
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        let base = base.into();
        Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            thread_body: std::env::var("HARNESSD_THREAD_BODY")
                .ok()
                .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                .unwrap_or_else(|| json!({})),
            thread_id: None,
            last_turn: None,
            inbox: VecDeque::new(),
            sse: None,
        }
    }

    fn curl(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let mut cmd = Command::new("curl");
        cmd.args(["--silent", "--show-error", "--max-time", "120", "-X", method])
            .args(["-H", "content-type: application/json"])
            .args(["-w", "\n%{http_code}"]);
        if let Some(t) = &self.token {
            cmd.args(["-H", &format!("authorization: Bearer {t}")]);
        }
        if let Some(b) = body {
            cmd.args(["-d", &serde_json::to_string(b)?]);
        }
        cmd.arg(&url);

        let out = cmd.output().context("run curl")?;
        let raw = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("{method} {path} failed: {err} {}", truncate(&raw));
        }
        // The status is the only thing that separates "empty success" from 404,
        // and a 404 body is empty — so never trust the body alone.
        let (body, status) = match raw.rsplit_once('\n') {
            Some((b, code)) => (b.trim().to_string(), code.trim().to_string()),
            None => (raw.trim().to_string(), "000".to_string()),
        };
        if !(status.starts_with('2')) {
            bail!("{method} {path} → HTTP {status} {}", truncate(&body));
        }
        if body.is_empty() {
            bail!("{method} {path} → HTTP {status} with an empty body");
        }
        serde_json::from_str(&body)
            .with_context(|| format!("{method} {path} returned non-JSON: {}", truncate(&body)))
    }

    fn ensure_thread(&mut self) -> Result<String> {
        if let Some(t) = &self.thread_id {
            return Ok(t.clone());
        }
        let v = self.curl("POST", "/v1/threads", Some(&self.thread_body))?;
        let id = v
            .get("id")
            .or_else(|| v.pointer("/thread/id"))
            .and_then(Value::as_str)
            .context("thread creation returned no id")?
            .to_string();
        self.thread_id = Some(id.clone());
        Ok(id)
    }

    /// Lazily attach to the thread's SSE stream. Events are forwarded verbatim:
    /// we do not yet know every event's schema, and guessing would be worse than
    /// passing them through under one honest kind.
    fn ensure_stream(&mut self) {
        if self.sse.is_some() {
            return;
        }
        let Some(thread) = self.thread_id.clone() else { return };
        let url = format!("{}/v1/threads/{}/events", self.base, thread);
        let mut cmd = Command::new("curl");
        cmd.args(["--silent", "--no-buffer", "--max-time", "3600"]);
        if let Some(t) = &self.token {
            cmd.args(["-H", &format!("authorization: Bearer {t}")]);
        }
        cmd.arg(url).stdout(Stdio::piped()).stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else { return };
        let Some(stdout) = child.stdout.take() else { return };
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("http-sse".into())
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
            })
            .ok();
        self.sse = Some((child, rx));
    }
}

/// 未知事件也要带名字转发（`engine/<kind>`），否则 UI 只能看到一团。
fn out_method(kind: &str) -> &'static str {
    match kind {
        "turn.lifecycle" => "engine/turn.lifecycle",
        "turn.usage" => "thread/tokenUsage/updated",
        "pre_turn" | "post_turn" => "engine/turn.phase",
        "thread.started" => "engine/thread.started",
        "user_message" => "engine/user.message",
        "agent_message" => "item/completed",
        "file" => "engine/file",
        "status" => "thread/status/changed",
        _ => "engine/event",
    }
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.len() <= 300 { s.to_string() } else { format!("{}…", &s[..300]) }
}

impl Engine for HttpEngine {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        match method {
            // The stdio handshake has no HTTP equivalent; /healthz is the check.
            "initialize" => {
                let v = self.curl("GET", "/healthz", None)?;
                Ok(json!({ "userAgent": v.get("service").cloned().unwrap_or(Value::Null), "protocol": v.get("protocol") }))
            }
            // There is no /v1/models: the catalogue hangs off the provider.
            "app/models" | "model/list" => {
                let providers = self.curl("GET", "/v1/providers", None)?;
                let current = providers
                    .get("current_provider_id")
                    .and_then(Value::as_str)
                    .unwrap_or("deepseek")
                    .to_string();
                let entry = providers
                    .get("providers")
                    .and_then(Value::as_array)
                    .and_then(|a| a.iter().find(|p| p.get("id").and_then(Value::as_str) == Some(current.as_str())))
                    .cloned()
                    .unwrap_or(Value::Null);
                let models = self
                    .curl("GET", &format!("/v1/providers/{current}/models"), None)
                    .unwrap_or(Value::Null);
                Ok(json!({
                    "provider": current,
                    "credential": entry.get("credentialState").cloned().unwrap_or(Value::Null),
                    "models": models.get("models").cloned().unwrap_or(json!([])),
                }))
            }
            "thread/start" => {
                let v = self.curl("POST", "/v1/threads", Some(&self.thread_body))?;
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    self.thread_id = Some(id.to_string());
                }
                Ok(v)
            }
            "thread/read" => {
                let id = params.get("threadId").and_then(Value::as_str).map(str::to_string)
                    .or_else(|| self.thread_id.clone())
                    .context("thread/read without a thread id")?;
                self.curl("GET", &format!("/v1/threads/{id}"), None)
            }
            // Tool-enabled turn: this is the whole reason the HTTP transport exists.
            "prompt/run" | "prompt/request" => {
                let id = self.ensure_thread()?;
                let prompt = params
                    .get("prompt")
                    .or_else(|| params.get("text"))
                    .and_then(Value::as_str)
                    .context("prompt/run without a prompt")?
                    .to_string();
                let mut body = json!({ "prompt": prompt });
                if let Some(m) = params.get("model").and_then(Value::as_str) {
                    body["model"] = json!(m);
                }
                // 引擎的字段名是 `reasoning_effort`（StartTurnRequest）；
                // 我们的抽象叫 effort，翻译在这里。
                if let Some(e) = params.get("effort").and_then(Value::as_str) {
                    body["reasoning_effort"] = json!(e);
                }
                let v = self.curl("POST", &format!("/v1/threads/{id}/turns"), Some(&body))?;
                if let Some(t) = v.get("turn_id").or_else(|| v.pointer("/turn/id")).and_then(Value::as_str) {
                    self.last_turn = Some(t.to_string());
                }
                self.ensure_stream();
                Ok(json!({ "thread": { "id": id }, "turn": v }))
            }
            // 分叉：`Fork`（回到某条消息之前）与 `CloneSession`（原样复制）都走这一条。
            "thread/fork" => {
                let id = self
                    .thread_id
                    .clone()
                    .or_else(|| params.get("threadId").and_then(Value::as_str).map(str::to_string))
                    .context("fork without a thread")?;
                let v = self.curl("POST", &format!("/v1/threads/{id}/fork"), Some(&json!({})))?;
                if let Some(new_id) = v
                    .get("id")
                    .or_else(|| v.pointer("/thread/id"))
                    .and_then(Value::as_str)
                {
                    self.thread_id = Some(new_id.to_string());
                }
                Ok(v)
            }
            // 流式中途插话：HTTP 面按 turn 走，stdio 面的 `thread/message` 在这里翻译。
            "thread/message" | "turn/steer" => {
                let thread = self.thread_id.clone().context("steer without a thread")?;
                let turn = self
                    .last_turn
                    .clone()
                    .or_else(|| params.get("turnId").and_then(Value::as_str).map(str::to_string))
                    .context("steer without a turn")?;
                let text = params
                    .get("text")
                    .or_else(|| params.get("input"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.curl(
                    "POST",
                    &format!("/v1/threads/{thread}/turns/{turn}/steer"),
                    // 实测：steer 要的是 `prompt`，不是 OpenAI 风格的 input 数组。
                    Some(&json!({ "prompt": text })),
                )
            }
            "thread/interrupt" | "turn/interrupt" => {
                let thread = self
                    .thread_id
                    .clone()
                    .or_else(|| params.get("threadId").and_then(Value::as_str).map(str::to_string))
                    .context("interrupt without a thread")?;
                let turn = self
                    .last_turn
                    .clone()
                    .or_else(|| params.get("turnId").and_then(Value::as_str).map(str::to_string))
                    .context("interrupt without a turn id")?;
                self.curl("POST", &format!("/v1/threads/{thread}/turns/{turn}/interrupt"), Some(&json!({})))
            }
            other => bail!("`{other}` is not mapped on the HTTP transport"),
        }
    }

    /// Answers to approvals are POSTed, not returned on a pipe: there is no
    /// reverse-request channel here.
    fn respond(&mut self, id: u64, result: Value) -> Result<()> {
        let decision = result.get("decision").and_then(Value::as_str).unwrap_or("decline");
        let approval_id = self
            .inbox
            .iter()
            .find(|f| f.id == Some(id))
            .and_then(|f| f.params.as_ref())
            .and_then(|p| p.get("approval_id").or_else(|| p.get("approvalId")))
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(approval_id) = approval_id else {
            bail!("no approval {id} to answer");
        };
        self.curl(
            "POST",
            &format!("/v1/approvals/{approval_id}"),
            Some(&json!({ "decision": decision })),
        )?;
        Ok(())
    }

    fn notifications(&mut self) -> Vec<Frame> {
        self.ensure_stream();
        let mut out = Vec::new();
        if let Some((_, rx)) = &self.sse {
            loop {
                match rx.try_recv() {
                    Ok(line) => {
                        let line = line.trim();
                        if let Some(data) = line.strip_prefix("data:") {
                            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                                let kind = v
                                    .get("kind")
                                    .or_else(|| v.get("event"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("event")
                                    .to_string();
                                // 事件名直接对上 `ui_kind_for`，UI 才能拿到
                                // `turn_end` / `text_delta` 这类认识的种类；
                                // 认不出的照样转发，不丢。
                                let method = match kind.as_str() {
                                    "turn.completed" | "turn.failed" | "turn.cancelled" => "turn/completed",
                                    "turn.started" => "turn/started",
                                    "error" => "error",
                                    "item.started" => "item/started",
                                    "item.completed" => "item/completed",
                                    "item.delta" => {
                                        let inner = v
                                            .pointer("/payload/kind")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        if inner == "agent_message" {
                                            "item/agentMessage/delta"
                                        } else {
                                            "item/commandExecution/outputDelta"
                                        }
                                    }
                                    other => {
                                        // 借用一下 string：未知种类带自己的名字转发。
                                        out_method(other)
                                    }
                                };
                                if kind.contains("approval") {
                                    let approval_id = v
                                        .get("approval_id")
                                        .or_else(|| v.get("approvalId"))
                                        .cloned()
                                        .unwrap_or(Value::Null);
                                    let mut f = Frame::request(
                                        0,
                                        "item/commandExecution/requestApproval",
                                        json!({ "approval_id": approval_id, "raw": v }),
                                    );
                                    f.id = Some(0);
                                    out.push(f);
                                } else {
                                    // 事件的内容在 `payload` 里，信封只是外壳。
                                    // 把整个信封往下传，下游就永远看见 item=None，
                                    // 工具调用一条也记不到——指标全绿但什么都没量。
                                    let payload = v
                                        .get("payload")
                                        .cloned()
                                        .unwrap_or_else(|| v.clone());
                                    out.push(Frame::notification(method, payload));
                                }
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.sse = None;
                        break;
                    }
                }
            }
        }
        out.extend(self.inbox.drain(..));
        out
    }

    fn name(&self) -> String {
        format!("http:{}", self.base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Minimal HTTP server: enough to assert the transport's routing and bodies.
    /// Serves the given bodies in order, one connection each.
    fn serve_seq(bodies: Vec<&'static str>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in bodies {
                if let Ok((mut s, _)) = l.accept() {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                }
            }
        });
        format!("http://{addr}")
    }

    fn serve_once(status: &'static str, body: &'static str) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn thread_start_reads_the_top_level_id() {
        let base = serve_once("200 OK", r#"{"id":"thr_1","model":"deepseek-flash"}"#);
        let mut e = HttpEngine::new(base, None);
        let v = e.request("thread/start", json!({})).unwrap();
        assert_eq!(v["id"], "thr_1");
        assert_eq!(e.thread_id.as_deref(), Some("thr_1"));
    }

    #[test]
    fn models_come_from_the_provider_catalogue() {
        let base = serve_seq(vec![
            r#"{"current_provider_id":"deepseek","providers":[{"id":"deepseek","credentialState":"missing"}]}"#,
            r#"{"provider":"deepseek","models":[{"id":"deepseek-v4-pro"}]}"#,
        ]);
        let mut e = HttpEngine::new(base, None);
        let v = e.request("app/models", json!({})).unwrap();
        assert_eq!(v["models"][0]["id"], "deepseek-v4-pro");
        assert_eq!(v["credential"], "missing", "a missing key must be visible to the UI");
    }

    #[test]
    fn a_404_is_an_error_not_an_empty_success() {
        let base = serve_once("404 Not Found", "");
        let mut e = HttpEngine::new(base, None);
        let err = e.request("app/models", json!({})).unwrap_err().to_string();
        assert!(err.contains("404"), "{err}");
    }

    /// 只在请求体里出现 `needle` 时才回 200——这样「字段有没有送出去」是可断言的。
    fn serve_expect(needle: &'static str, reply: &'static str) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = if req.contains(needle) {
                    ("200 OK", reply)
                } else {
                    ("400 Bad Request", r#"{"error":"missing expected field"}"#)
                };
                let resp = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn effort_is_sent_as_reasoning_effort() {
        let base = serve_expect("reasoning_effort", r#"{"turn_id":"turn_1"}"#);
        let mut e = HttpEngine::new(base, None);
        e.thread_id = Some("thr_1".into());
        let v = e
            .request("prompt/run", json!({"prompt":"hi","effort":"high"}))
            .expect("effort must reach the engine as reasoning_effort");
        assert_eq!(v["thread"]["id"], "thr_1");
    }

    #[test]
    fn fork_switches_to_the_new_thread() {
        let base = serve_expect("/v1/threads/thr_1/fork", r#"{"id":"thr_2"}"#);
        let mut e = HttpEngine::new(base, None);
        e.thread_id = Some("thr_1".into());
        let v = e.request("thread/fork", json!({})).unwrap();
        assert_eq!(v["id"], "thr_2");
        assert_eq!(e.thread_id.as_deref(), Some("thr_2"), "fork must move the session");
    }

    #[test]
    fn steer_targets_the_live_turn() {
        let base = serve_expect("/turns/turn_9/steer", r#"{"ok":true}"#);
        let mut e = HttpEngine::new(base, None);
        e.thread_id = Some("thr_1".into());
        e.last_turn = Some("turn_9".into());
        let v = e.request("thread/message", json!({"text":"wait, use the other file"})).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn unmapped_method_fails_loudly() {
        let mut e = HttpEngine::new("http://127.0.0.1:1", None);
        let err = e.request("auth/login", json!({})).unwrap_err().to_string();
        assert!(err.contains("not mapped on the HTTP transport"), "{err}");
    }
}
