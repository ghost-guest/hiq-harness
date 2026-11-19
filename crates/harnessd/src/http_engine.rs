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
use std::collections::{HashMap, HashSet, VecDeque};
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
    /// 合成 id → 引擎的 approval id。
    ///
    /// 反向请求在这个传输上不是真的"请求"：引擎把审批放在 SSE 事件里
    /// （`approval.required`），我们只能给它编一个单调 id 交上去，答的时候
    /// 再换回真 id 去 POST。没有这张表就只能回 `id = 0`，而 0 答不了任何东西。
    pending_approvals: HashMap<u64, String>,
    next_approval: u64,
    /// 已经交出去的审批 id。
    ///
    /// 引擎会把同一个 `approval.required` 发**两次**（实测 seq 675 / 676 同一个
    /// approval id）。答第二次时引擎回 404「no pending approval」——而这条错误
    /// 会把整个 turn 打死。所以按 approval id 去重：见过就不再交，也不再答。
    seen_approvals: HashSet<String>,
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
            pending_approvals: HashMap::new(),
            next_approval: 0,
            seen_approvals: HashSet::new(),
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

/// SSE 事件名 → 我们内部的 method 名。
///
/// 抽成纯函数是为了能被测：原来这段内联在 `notifications()` 里，
/// **一行测试都没有**。变异扫描把 `"turn.completed"` 从这个分支删掉，
/// 整个 crate 的测试全绿——而"回合不闭合"正是这一夜最贵的那个 bug。
fn sse_method_for(kind: &str, v: &Value) -> String {
    match kind {
        "turn.completed" | "turn.failed" | "turn.cancelled" => "turn/completed".into(),
        "turn.started" => "turn/started".into(),
        "error" => "error".into(),
        "item.started" => "item/started".into(),
        "item.completed" => "item/completed".into(),
        "item.delta" => {
            let inner = v
                .pointer("/payload/kind")
                .and_then(Value::as_str)
                .unwrap_or("");
            if inner == "agent_message" {
                "item/agentMessage/delta".into()
            } else {
                "item/commandExecution/outputDelta".into()
            }
        }
        // 认不出的**带名字**转发。注释一直说"带名字转发，不丢"，
        // 但代码原来一律返回 `engine/event`——名字被压扁了，
        // 下游只能看到"有事件"，看不到是哪个。文档说的和代码做的不是一回事。
        other => out_method(other),
    }
}

/// 未知事件也要带名字转发（`engine/<kind>`），否则 UI 只能看到一团。
fn out_method(kind: &str) -> String {
    match kind {
        "turn.lifecycle" => "engine/turn.lifecycle".into(),
        "turn.usage" => "thread/tokenUsage/updated".into(),
        "pre_turn" | "post_turn" => "engine/turn.phase".into(),
        "thread.started" => "engine/thread.started".into(),
        "user_message" => "engine/user.message".into(),
        "agent_message" => "item/completed".into(),
        "file" => "engine/file".into(),
        "status" => "thread/status/changed".into(),
        other => format!("engine/{other}"),
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
                // **先订阅，再发回合。** 这个顺序不是风格问题：
                // 引擎的 SSE 是单订阅者，而且在没有订阅者时它会让回合停住
                // （实测 `POST /turns` 返回 201，但 60 秒内 items=0、
                //  `turn.started` 都不出现；下一回合直接 409
                //  `Thread already has an active turn`）。
                // 之前的顺序是发完才订阅，于是第一回合永远不结束。
                self.ensure_stream();
                let v = self.curl("POST", &format!("/v1/threads/{id}/turns"), Some(&body))?;
                if let Some(t) = v.get("turn_id").or_else(|| v.pointer("/turn/id")).and_then(Value::as_str) {
                    self.last_turn = Some(t.to_string());
                }
                self.ensure_stream();
                // 回合 id 要带到顶层。引擎的响应是 `{"thread":…, "turn":{"id":…}}`，
                // 但上层 `handle` 会把返回包成 `{"thread":…, "output":…, "model":…}`，
                // `turn` 整个丢掉。丢掉之后上层**无法判断引擎到底接没接这一轮**——
                // 判据查不到，回合被误判成"被拒"。（真实踩过：第 1 回合正常，
                // 第 2 回合被拒，而引擎其实 201 返回了。）
                let turn_id = v
                    .get("id")
                    .or_else(|| v.pointer("/turn/id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Ok(json!({ "thread": { "id": id }, "turn": v, "turn_id": turn_id }))
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
            .pending_approvals
            .remove(&id)
            .or_else(|| {
                self.inbox
                    .iter()
                    .find(|f| f.id == Some(id))
                    .and_then(|f| f.params.as_ref())
                    .and_then(|p| p.get("approval_id").or_else(|| p.get("approvalId")))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let Some(approval_id) = approval_id else {
            bail!("no approval {id} to answer");
        };
        self.curl(
            "POST",
            &format!("/v1/approvals/{approval_id}"),
            Some(&json!({ "decision": wire_decision(decision) })),
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
                                let method = sse_method_for(&kind, &v);
                                if kind.contains("approval") {
                                    self.next_approval += 1;
                                    let id = self.next_approval;
                                    let f = approval_frame(id, &v);
                                    let aid = f
                                        .params
                                        .as_ref()
                                        .and_then(|p| p.get("approval_id"))
                                        .and_then(Value::as_str)
                                        .map(str::to_string);
                                    if first_sighting(&mut self.seen_approvals, &aid) {
                                        if let Some(aid) = aid {
                                            self.pending_approvals.insert(id, aid);
                                        }
                                        out.push(f);
                                    }
                                } else {
                                    // 事件的内容在 `payload` 里，信封只是外壳。
                                    // 把整个信封往下传，下游就永远看见 item=None，
                                    // 工具调用一条也记不到——指标全绿但什么都没量。
                                    let payload = v
                                        .get("payload")
                                        .cloned()
                                        .unwrap_or_else(|| v.clone());
                                    out.push(Frame::notification(&method, payload));
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
        // 要接**多次**连接：客户端在发回合之前会先挂上 SSE（引擎没有订阅者
        // 就不推进回合），那也是一条连接。只接一次的话，SSE 会把唯一的名额吃掉。
        std::thread::spawn(move || {
            for _ in 0..4 {
                let Ok((mut s, _)) = l.accept() else { return };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                if req.contains("/events") {
                    // 保持打开，别回 JSON：这是 SSE，测试不消费它的内容。
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
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

    /// 每一种 SSE 事件都必须映射到自己那一档。
    ///
    /// 这条是变异扫描逼出来的：把 `"turn.completed"` 从这个 match 里删掉，
    /// 整个 crate 的测试**全绿**——而"回合不闭合"正是这一夜最贵的那个 bug
    /// （timebus 里只有 turn.start + usage，投影全错）。
    /// 事件名是这一层的词汇表，词汇表错了，后面每一层都跟着错。
    /// 真实引擎的 turn.usage 信封必须能被认出来。
    /// 这是从直连 SSE 抓来的原文（seq 13283），不是编的。
    /// 压缩探针红了 6 次才查到：timebus 里一条 usage 都没有——
    /// 缓存守卫的命中率从这里来，usage 丢了就永远不会压缩。
    #[test]
    fn real_turn_usage_envelope_maps_to_token_usage() {
        let v = json!({
            "schema_version": 1, "seq": 13283, "event": "turn.usage",
            "kind": "turn.usage", "thread_id": "thr_x", "turn_id": "turn_x",
            "payload": { "usage": {
                "input_tokens": 13582, "output_tokens": 10,
                "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": 13582
            }}
        });
        let method = sse_method_for(
            v.get("kind").and_then(serde_json::Value::as_str).unwrap_or("event"), &v);
        assert_eq!(method, "thread/tokenUsage/updated");
        let payload = v.get("payload").cloned().unwrap_or(v.clone());
        let inner = payload.get("usage").unwrap_or(&payload);
        let input = inner.get("input_tokens").and_then(serde_json::Value::as_u64).unwrap_or(0);
        assert_eq!(input, 13582, "usage 必须能被解析出来，否则缓存命中率永远没有");
    }

    #[test]
    fn every_sse_event_maps_to_its_own_method() {
        let empty = json!({});
        // 回合结束的三种形态都必须认。
        for kind in ["turn.completed", "turn.failed", "turn.cancelled"] {
            assert_eq!(
                sse_method_for(kind, &empty),
                "turn/completed",
                "{kind} 必须映射成 turn/completed，否则 timebus 里的回合永远不闭合"
            );
        }
        assert_eq!(sse_method_for("turn.started", &empty), "turn/started");
        assert_eq!(sse_method_for("item.started", &empty), "item/started");
        assert_eq!(sse_method_for("item.completed", &empty), "item/completed");
        assert_eq!(sse_method_for("error", &empty), "error");

        // 增量的两种走向由 payload 里的 kind 决定。
        let text = json!({ "payload": { "kind": "agent_message" } });
        assert_eq!(
            sse_method_for("item.delta", &text),
            "item/agentMessage/delta"
        );
        let tool = json!({ "payload": { "kind": "tool_call" } });
        assert_eq!(
            sse_method_for("item.delta", &tool),
            "item/commandExecution/outputDelta"
        );

        // 认不出的要带名字转发，不能丢。
        assert_eq!(sse_method_for("turn.usage", &empty), "thread/tokenUsage/updated");
        assert_eq!(sse_method_for("weird.thing", &empty), "engine/weird.thing");
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

/// 把一个 `approval.required` 事件翻成内核认识的审批请求。
///
/// 真实形状（实测，不是猜的）：
/// ```text
/// {"event":"approval.required","payload":{
///    "id":"approval_…","approval_id":"approval_…","tool_name":"bash",
///    "input":{"command":"echo hi","description":"Run echo hi"},"summary":"…"}}
/// ```
/// 早先版本只搬 `approval_id`，结果内核看到 `tool:"unknown"`、命令也是一坨 JSON，
/// 插件因此永远认不出"这是不是一条危险命令"。这里把 tool / command 摊平。
pub fn approval_frame(id: u64, event: &Value) -> Frame {
    let payload = event.get("payload").cloned().unwrap_or_else(|| event.clone());
    let approval_id = payload
        .get("approval_id")
        .or_else(|| payload.get("approvalId"))
        .or_else(|| payload.get("id"))
        .or_else(|| event.get("approval_id"))
        .cloned()
        .unwrap_or(Value::Null);
    let tool = payload
        .get("tool_name")
        .or_else(|| payload.get("toolName"))
        .or_else(|| payload.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("bash")
        .to_string();
    let command = payload
        .get("input")
        .and_then(|i| i.get("command").or_else(|| i.get("cmd")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Frame::request(
        id,
        "item/commandExecution/requestApproval",
        json!({
            "approval_id": approval_id,
            "tool": tool,
            "command": command,
            "raw": event,
        }),
    )
}

#[cfg(test)]
mod approval_shape_tests {
    use super::*;

    /// 真事件（从引擎 SSE 里抓的），只裁掉了无关字段。
    fn real_event() -> Value {
        json!({
            "schema_version": 1,
            "seq": 10,
            "event": "approval.required",
            "kind": "approval.required",
            "thread_id": "thr_x",
            "payload": {
                "id": "approval_2ad1dc5c21aa46b3b9a7094e88a383d1",
                "approval_id": "approval_2ad1dc5c21aa46b3b9a7094e88a383d1",
                "tool_call_id": "396be749",
                "tool_name": "bash",
                "input": { "command": "echo hi", "description": "Run echo hi" },
                "summary": "Run `echo hi`"
            }
        })
    }

    #[test]
    fn the_frame_carries_the_real_approval_id_tool_and_command() {
        let f = approval_frame(3, &real_event());
        assert_eq!(f.id, Some(3));
        let p = f.params.clone().unwrap();
        assert_eq!(p["approval_id"], "approval_2ad1dc5c21aa46b3b9a7094e88a383d1");
        assert_eq!(p["tool"], "bash");
        assert_eq!(p["command"], "echo hi");
    }

    /// 插件要看到的就是这串命令：多包一层 JSON 它就不认识了。
    #[test]
    fn the_command_is_flat_not_a_json_blob() {
        let f = approval_frame(1, &real_event());
        let cmd = f.params.as_ref().unwrap()["command"].as_str().unwrap().to_string();
        assert_eq!(cmd, "echo hi");
        assert!(!cmd.starts_with('{'), "command must not be the whole params: {cmd}");
    }
}

/// 两种方言的审批词表不一样，抄错会被引擎当场骂回来（实测报错：
/// `invalid decision 'decline'; expected "allow" or "deny"`）。
///
/// - stdio / 内核内部：`accept` / `decline` / `cancel`
/// - HTTP `/v1/approvals/{id}`：`allow` / `deny`（只有这两个）
///
/// `cancel` 在 HTTP 面没有对应值，按 deny 处理最保守：宁可不动，也不要替用户
/// 放行一个他自己取消掉的命令。
pub fn wire_decision(decision: &str) -> &'static str {
    match decision {
        "accept" | "allow" | "acceptForSession" | "acceptWithExecpolicyAmendment" => "allow",
        _ => "deny",
    }
}

#[cfg(test)]
mod decision_words_tests {
    use super::*;

    #[test]
    fn the_two_dialects_are_translated() {
        assert_eq!(wire_decision("accept"), "allow");
        assert_eq!(wire_decision("allow"), "allow");
        assert_eq!(wire_decision("decline"), "deny");
        assert_eq!(wire_decision("deny"), "deny");
        // 取消不能变成放行。
        assert_eq!(wire_decision("cancel"), "deny");
        assert_eq!(wire_decision(""), "deny");
    }
}

/// 同一个审批只交一次。
///
/// 引擎实测会把同一个 `approval.required` 发两遍（seq 675 / 676 同一个 id）。
/// 交两遍的后果不是"多问一次"，而是第二次应答 404 之后整个 turn 直接死掉。
/// 没有 approval id 的事件（形状不认识的）一律放行，宁可多问也不要静默吞掉。
fn first_sighting(seen: &mut HashSet<String>, aid: &Option<String>) -> bool {
    match aid {
        Some(id) => seen.insert(id.clone()),
        None => true,
    }
}

#[cfg(test)]
mod approval_dedupe_tests {
    use super::*;

    #[test]
    fn the_same_approval_is_delivered_once() {
        let mut seen = HashSet::new();
        let aid = Some("approval_x".to_string());
        assert!(first_sighting(&mut seen, &aid), "第一次必须交上去");
        assert!(!first_sighting(&mut seen, &aid), "同一条不能交第二遍");
    }

    #[test]
    fn a_different_approval_still_gets_through() {
        let mut seen = HashSet::new();
        assert!(first_sighting(&mut seen, &Some("a".into())));
        assert!(first_sighting(&mut seen, &Some("b".into())));
    }

    #[test]
    fn an_unknown_shape_is_never_swallowed() {
        let mut seen = HashSet::new();
        assert!(first_sighting(&mut seen, &None));
        assert!(first_sighting(&mut seen, &None));
    }
}
