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
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
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
        // 关键：必须复用同一个 BufReader。
        // BufReader 可能已把 WS 帧的字节预读进缓冲区，
        // 若改用原始 stream 读，这些字节就丢了 —— 握手成功但收不到帧。
        ws_handshake(&mut stream, &headers)?;
        ws_loop(&mut reader, &mut stream, on_envelope)?;
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

/// WebSocket 握手。返回 101。
fn ws_handshake(stream: &mut TcpStream, headers: &[String]) -> anyhow::Result<()> {
    let key = headers
        .iter()
        .find(|h| h.to_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|h| h.split(':').nth(1))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("缺少 Sec-WebSocket-Key"))?;

    let accept = ws_accept_key(&key);
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// 按 RFC 6455 计算 accept key：base64(sha1(key + GUID))。
fn ws_accept_key(key: &str) -> String {
    let mut input = key.as_bytes().to_vec();
    input.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64(&sha1(&input))
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for i in 0..5 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// WS 主循环：读帧 → 处理 → 回帧。
fn ws_loop<F>(
    reader: &mut BufReader<TcpStream>,
    stream: &mut TcpStream,
    on_envelope: Arc<F>,
) -> anyhow::Result<()>
where
    F: Fn(Envelope, &str) -> Option<Envelope> + Send + Sync,
{
    let mut device_id = String::from("anon");
    loop {
        let (opcode, payload) = match read_frame(reader) {
            Ok((op, payload)) => {
                eprintln!("[ws] 收到帧 opcode={op} len={}", payload.len());
                (op, payload)
            }
            Err(e) => {
                // WouldBlock/TimedOut 表示这一轮没数据，继续等；
                // 其他错误（对端关闭）才退出。
                // 注意：set_read_timeout 会让 socket 在无数据时返回 WouldBlock。
                let kind = e.downcast_ref::<std::io::Error>().map(|io| io.kind());
                match kind {
                    Some(std::io::ErrorKind::WouldBlock) | Some(std::io::ErrorKind::TimedOut) => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    _ => {
                        eprintln!("[ws] 连接结束：{e}");
                        return Ok(());
                    }
                }
            }
        };

        // 只处理文本帧与 close
        if opcode == 0x8 {
            return Ok(());
        }
        if opcode == 0x9 {
            // ping → pong
            write_frame(stream, 0xA, &payload)?;
            continue;
        }
        if opcode != 0x1 {
            continue;
        }

        let text = match String::from_utf8(payload) {
            Ok(t) => t,
            Err(_) => continue,
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
                    write_frame(stream, 0x1, s.as_bytes())?;
                }
                continue;
            }
            Err(_) => continue,
        };

        // hello 时分配 device id
        if env.kind == harness_core::wire::EnvelopeKind::Hello {
            if let Some(reply) = on_envelope(env, &device_id) {
                if let Some(p) = &reply.payload {
                    if let Some(id) = p.get("device_id").and_then(|v| v.as_str()) {
                        device_id = id.to_string();
                    }
                }
                if let Ok(s) = reply.encode() {
                    write_frame(stream, 0x1, s.as_bytes())?;
                }
            }
            continue;
        }

        if let Some(reply) = on_envelope(env, &device_id) {
            if let Ok(s) = reply.encode() {
                write_frame(stream, 0x1, s.as_bytes())?;
            }
        }
    }
}

/// 读一个 WS 帧（客户端帧带掩码）。
fn read_frame<R: Read>(stream: &mut R) -> anyhow::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr)?;
    let opcode = hdr[0] & 0x0F;
    let masked = (hdr[1] & 0x80) != 0;
    let mut len = (hdr[1] & 0x7F) as usize;

    if len == 126 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0u8; 8];
        stream.read_exact(&mut b)?;
        len = u64::from_be_bytes(b) as usize;
    }

    if len > harness_core::wire::MAX_FRAME_BYTES {
        anyhow::bail!("帧过大");
    }

    let mut mask = [0u8; 4];
    if masked {
        stream.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok((opcode, payload))
}

/// 写一个 WS 帧（服务端帧不带掩码）。
fn write_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> anyhow::Result<()> {
    let mut frame = vec![0x80 | opcode];
    let len = payload.len();
    if len < 126 {
        frame.push(len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}

/// HTTP 分发：路由 → 处理器 → 响应。
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
        Route::Run => {
            // 真正执行任务（同步返回最终文本）
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
    fn sha1_known_vector() {
        // "abc" 的 SHA-1 是公开测试向量
        let d = sha1(b"abc");
        let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn sha1_empty() {
        let d = sha1(b"");
        let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn ws_accept_key_matches_rfc_example() {
        // RFC 6455 的示例：key 为 "dGhlIHNhbXBsZSBub25jZQ=="
        // 期望 accept "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        let k = ws_accept_key("dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(k, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=", "WS 握手必须符合 RFC");
    }

    #[test]
    fn default_binds_loopback_only() {
        // 安全要求：默认只绑回环，不对局域网暴露
        let cfg = ServeConfig::default();
        assert_eq!(cfg.bind, "127.0.0.1");
    }
}
