//! 最小 HTTP + WebSocket 服务。
//!
//! 不引入 web 框架：只用 std 的 TcpListener + 手写 WS 握手与帧解析。
//! 理由与调研一致 —— kiso 的「零依赖扩展」、Cindy host 的「哑中继」，
//! 都倾向最小实现而非堆框架。
//!
//! 只绑 127.0.0.1：不对外暴露。跨设备走隧道（协议已预留）。

use harness_core::wire::{Envelope, FrameError};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

/// 服务配置。
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub bind: String,
    pub port: u16,
}

impl Default for ServeConfig {
    fn default() -> Self {
        // 只绑回环地址。Cindy 的 host 也是这个做法。
        Self {
            bind: "127.0.0.1".into(),
            port: 7788,
        }
    }
}

/// 服务句柄。drop 时停止。
pub struct Server {
    stop: Arc<AtomicBool>,
    addr: String,
}

impl Server {
    /// 启动服务（阻塞）。
    ///
    /// `on_envelope` 处理收到的帧，返回要回发的帧。
    pub fn serve<F>(
        cfg: ServeConfig,
        state: Arc<Mutex<crate::host_service::HostState>>,
        on_envelope: F,
    ) -> anyhow::Result<Server>
    where
        F: Fn(Envelope, &str) -> Option<Envelope> + Send + Sync + 'static,
    {
        let addr = format!("{}:{}", cfg.bind, cfg.port);
        let listener = TcpListener::bind(&addr)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let handler = Arc::new(on_envelope);

        // 设备清理：TTL = 心跳间隔 × 3（Cindy 的语义）
        {
            let st = state.clone();
            let stop = stop_clone.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    let ttl = harness_core::wire::HEARTBEAT_SECS * 3;
                    let dead = st.lock().unwrap().registry.reap(now, ttl);
                    if !dead.is_empty() {
                        eprintln!("[host] 清理超时设备: {dead:?}");
                    }
                }
            });
        }

        std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        let st = state.clone();
                        let h = handler.clone();
                        std::thread::spawn(move || {
                            let _ = handle_conn(stream, st, h);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Server { stop, addr })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn handle_conn<F>(
    mut stream: TcpStream,
    state: Arc<Mutex<crate::host_service::HostState>>,
    on_envelope: Arc<F>,
) -> anyhow::Result<()>
where
    F: Fn(Envelope, &str) -> Option<Envelope> + Send + Sync,
{
    // 不设 read_timeout：WS 是长连接，需要阻塞读。
    // 半帧超时会让 read_exact 丢弃已读字节，导致帧错位。
    // 连接生命周期由 EOF（对端关闭）或服务停止决定。
    let _ = stream.set_nodelay(true);

    // 读请求行与头部
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.trim().is_empty() {
        return Ok(());
    }

    let mut headers = Vec::new();
    let mut head_bytes = request_line.clone();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        head_bytes.push_str(&line);
        let t = line.trim_end().to_string();
        if t.is_empty() {
            break;
        }
        headers.push(t);
    }

    let is_ws = headers
        .iter()
        .any(|h| h.to_lowercase().starts_with("upgrade: websocket"));

    if is_ws {
        // 已经读过的请求行 + 头部按原字节重放给 tungstenite：
        // 握手、SHA-1、base64、帧编解码都由库负责，我们不再自己实现协议。
        let head = head_bytes.into_bytes();
        let replay = Replay { head, pos: 0, inner: stream };
        let ws = tungstenite::accept(replay)
            .map_err(|e| anyhow::anyhow!("WS 握手失败：{e}"))?;
        ws_loop(ws, on_envelope)?;
    } else {
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        let method = parts.first().copied().unwrap_or("GET");
        let path = parts.get(1).copied().unwrap_or("/");

        // 读请求体（若有 Content-Length）
        let clen: usize = headers
            .iter()
            .find(|h| h.to_lowercase().starts_with("content-length:"))
            .and_then(|h| h.split(':').nth(1))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let mut body_bytes = vec![0u8; clen];
        if clen > 0 {
            reader.read_exact(&mut body_bytes)?;
        }
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();

        http_dispatch(&mut stream, method, path, &body_str, &state)?;
    }
    Ok(())
}

/// 已解析的字节流 + 剩余 socket：把读过的头部原样"倒带"给 tungstenite。
///
/// tungstenite 的 `accept` 要自己读 HTTP 升级请求，而我们为了区分
/// WS / 普通 HTTP 已经先读了一遍头部。这里把读到的字节重放一遍，
/// 就不需要自己实现握手、SHA-1、base64 和帧编解码了。
struct Replay {
    head: Vec<u8>,
    pos: usize,
    inner: TcpStream,
}

impl std::io::Read for Replay {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.head.len() {
            let n = (self.head.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.head[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

impl std::io::Write for Replay {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// WS 会话：收发都交给 tungstenite。
fn ws_loop<F>(mut ws: tungstenite::WebSocket<Replay>, on_envelope: Arc<F>) -> anyhow::Result<()>
where
    F: Fn(Envelope, &str) -> Option<Envelope> + Send + Sync,
{
    use tungstenite::Message;
    let mut device_id = String::from("anon");
    loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::Protocol(_)) => return Ok(()),
            Err(e) => {
                eprintln!("[ws] 连接结束：{e}");
                return Ok(());
            }
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => return Ok(()),
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => continue,
        };

        let env = match Envelope::decode(&text) {
            Ok(e) => e,
            Err(FrameError::PayloadTooLarge { .. })
            | Err(FrameError::VersionMismatch { .. })
            | Err(FrameError::Decode(_)) => {
                // 协议错误：回错误帧但不断连（Cindy 的做法）
                let err = Envelope::new(harness_core::wire::EnvelopeKind::RelayError)
                    .with_payload(serde_json::json!({"error": "protocol"}));
                if let Ok(s) = err.encode() {
                    ws.send(Message::Text(s.into()))?;
                }
                continue;
            }
            Err(_) => continue,
        };

        if env.kind == harness_core::wire::EnvelopeKind::Hello {
            if let Some(reply) = on_envelope(env, &device_id) {
                if let Some(p) = &reply.payload {
                    if let Some(id) = p.get("device_id").and_then(|v| v.as_str()) {
                        device_id = id.to_string();
                    }
                }
                if let Ok(s) = reply.encode() {
                    ws.send(Message::Text(s.into()))?;
                }
            }
            continue;
        }

        if let Some(reply) = on_envelope(env, &device_id) {
            if let Ok(s) = reply.encode() {
                ws.send(Message::Text(s.into()))?;
            }
        }
    }
}

fn http_dispatch(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    body: &str,
    state: &Arc<Mutex<crate::host_service::HostState>>,
) -> anyhow::Result<()> {
    use crate::rest::RestHandler;
    use crate::routes::Route;

    let route = Route::parse(method, path);

    // UI 由 apps/ui（Next.js + assistant-ui）提供，这里只返回指引
    if route == Route::Ui {
        let body = serde_json::json!({
            "service": "hiq-harness host",
            "note": "界面在 apps/ui（Next.js + assistant-ui）。在此目录跑 pnpm dev 打开。",
            "endpoints": ["/health", "/devices", "/events", "/memory",
                          "/pending", "/resolve", "/run", "/sop", "ws://.../ws"],
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(resp.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    let devices = state.lock().unwrap().registry.count();
    let handler = RestHandler::new(".harness");
    let parsed: serde_json::Value = if body.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(body).unwrap_or(serde_json::json!({}))
    };

    let (code, json) = match route {
        Route::Health => handler.health(devices),
        Route::Devices => {
            let s = state.lock().unwrap();
            (
                200,
                serde_json::to_value(s.registry.all()).unwrap_or(serde_json::json!([])),
            )
        }
        Route::Events => handler.events(path),
        Route::Memory => {
            if method == "POST" {
                handler.add_memory(&parsed)
            } else {
                handler.memory()
            }
        }
        Route::Pending => handler.pending(),
        Route::Resolve => handler.resolve(&parsed),
        Route::RunStream => {
            // SSE 流式：直接写 socket，不走 JSON 返回值
            let task = parsed
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let workdir = std::env::var("HARNESS_WORKDIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let cfg = crate::exec::ExecConfig::from_env();

            let headers = crate::sse::sse_headers();
            let mut resp = String::from("HTTP/1.1 200 OK\r\n");
            for (k, v) in headers {
                resp.push_str(&format!("{k}: {v}\r\n"));
            }
            resp.push_str("Connection: close\r\n\r\n");
            stream.write_all(resp.as_bytes())?;
            stream.flush()?;

            crate::sse::stream_task(
                stream,
                std::path::Path::new(".harness"),
                &workdir,
                task,
                &[],
                &cfg,
            )?;
            return Ok(());
        }
        Route::Run => {
            // 非流式：一次性返回 JSON
            let task = parsed
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let workdir = std::env::var("HARNESS_WORKDIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let cfg = crate::exec::ExecConfig::from_env();
            crate::exec::execute_task(std::path::Path::new(".harness"), &workdir, task, &cfg)
        }
        Route::Sop => (
            202,
            serde_json::json!({
                "accepted": true,
                "name": parsed.get("name").cloned().unwrap_or(serde_json::Value::Null),
                "note": "SOP 已接收。",
            }),
        ),
        Route::NotFound => (
            404,
            serde_json::json!({
                "error": "not found",
                "endpoints": ["/", "/health", "/devices", "/events", "/memory",
                              "/pending", "/resolve", "/run", "/sop", "ws://.../ws"],
            }),
        ),
        Route::Ui => unreachable!(),
    };

    let status_text = match code {
        200 => "200 OK",
        202 => "202 Accepted",
        400 => "400 Bad Request",
        404 => "404 Not Found",
        _ => "500 Internal Server Error",
    };
    let body_str = json.to_string();
    let resp = format!(
        "HTTP/1.1 {status_text}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\r\n{body_str}",
        body_str.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_binds_loopback_only() {
        // 安全要求：默认只绑回环，不对局域网暴露
        let cfg = ServeConfig::default();
        assert_eq!(cfg.bind, "127.0.0.1");
    }
}
