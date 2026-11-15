//! 多端会话协议。
//!
//! 设计来自 Cindy 的 `device-link-protocol`（`packages/device-link-protocol/src/protocol.ts`）：
//!
//! - **信封统一**：所有帧走同一个 `Envelope`，含 `v`（协议版本）/ `kind` / `id` / `src` / `dst`
//! - **协议版本只升不降**：不兼容改动才 +1
//! - **帧上限**：`MAX_FRAME_BYTES = 2 MiB`，超过即拒并回错误码，不断连
//! - **分层**：连接层（client ↔ server）与隧道层（controller ↔ target）分开
//! - **src 由服务端填**：客户端传的值会被覆盖，防伪造
//!
//! 与 Cindy 的区别：它用 relay 做跨设备中继，我们先用**局域网直连**，
//! 协议保持一致，将来加 relay 时客户端不用改。

use serde::{Deserialize, Serialize};

/// 协议版本。整数，只升不降；不兼容改动 +1。
pub const PROTOCOL_VERSION: u32 = 1;

/// 单帧最大字节数。超过即回 `PayloadTooLarge` 并丢弃，**不断连**。
///
/// 发送方应先行拒绝或裁剪 —— 断开连接是最后手段。
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// 应用层心跳间隔（秒）。
///
/// Cindy 用 20 秒，服务端借此刷新路由 TTL。
pub const HEARTBEAT_SECS: u64 = 20;

/// 帧类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvelopeKind {
    // ── 连接层 ──
    /// client → server：上线注册
    Hello,
    /// server → client：注册确认
    HelloAck,
    /// client → server：部分更新在线状态
    PresenceSet,
    /// server → 同账号在线设备广播
    PresenceChanged,
    /// client → server：应用层心跳
    Ping,
    /// server → client
    Pong,

    // ── 隧道层（controller ↔ target）──
    /// 请求建立控制通道
    LinkOpen,
    /// 接受
    LinkAccept,
    /// 关闭
    LinkClose,
    /// 发起一次调用
    Invoke,
    /// 调用结果
    InvokeResult,
    /// 服务端主动推送（事件流）
    Push,

    // ── 错误 ──
    RelayError,
}

/// 统一信封。
///
/// `src` 由服务端填充 —— 客户端传入会被覆盖，防伪造。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// 协议版本
    pub v: u32,
    pub kind: EnvelopeKind,
    /// 请求/响应配对 id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 源 deviceId（服务端填）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    /// 目标 deviceId（隧道层必填）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dst: Option<String>,
    /// 负载。对中继不透明。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

impl Envelope {
    pub fn new(kind: EnvelopeKind) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            kind,
            id: None,
            src: None,
            dst: None,
            payload: None,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn with_dst(mut self, dst: impl Into<String>) -> Self {
        self.dst = Some(dst.into());
        self
    }

    pub fn with_payload(mut self, p: serde_json::Value) -> Self {
        self.payload = Some(p);
        self
    }

    /// 序列化并检查帧大小。
    ///
    /// 超过 `MAX_FRAME_BYTES` 返回错误，**不发送** ——
    /// 让发送方先裁剪，而不是让接收方断连。
    pub fn encode(&self) -> Result<String, FrameError> {
        let s = serde_json::to_string(self).map_err(|e| FrameError::Encode(e.to_string()))?;
        if s.len() > MAX_FRAME_BYTES {
            return Err(FrameError::PayloadTooLarge {
                bytes: s.len(),
                max: MAX_FRAME_BYTES,
            });
        }
        Ok(s)
    }

    /// 解码并校验版本。
    pub fn decode(s: &str) -> Result<Self, FrameError> {
        if s.len() > MAX_FRAME_BYTES {
            return Err(FrameError::PayloadTooLarge {
                bytes: s.len(),
                max: MAX_FRAME_BYTES,
            });
        }
        let env: Envelope =
            serde_json::from_str(s).map_err(|e| FrameError::Decode(e.to_string()))?;
        if env.v != PROTOCOL_VERSION {
            return Err(FrameError::VersionMismatch {
                got: env.v,
                want: PROTOCOL_VERSION,
            });
        }
        Ok(env)
    }

    /// 服务端转发时重写 src（防伪造）。
    pub fn stamp_src(mut self, src: &str) -> Self {
        self.src = Some(src.to_string());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Encode(String),
    Decode(String),
    PayloadTooLarge { bytes: usize, max: usize },
    VersionMismatch { got: u32, want: u32 },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode(e) => write!(f, "编码失败：{e}"),
            Self::Decode(e) => write!(f, "解码失败：{e}"),
            Self::PayloadTooLarge { bytes, max } => {
                write!(f, "帧过大：{bytes} 字节，上限 {max}")
            }
            Self::VersionMismatch { got, want } => {
                write!(f, "协议版本不匹配：收到 {got}，期望 {want}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

// ─────────────────────────────────────────────────────────────
// 负载类型
// ─────────────────────────────────────────────────────────────

/// hello 负载：上线注册。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloPayload {
    pub device_name: String,
    pub platform: String,
    pub app_version: String,
    /// 是否允许被远程控制
    #[serde(default)]
    pub remote_control_enabled: bool,
    #[serde(default)]
    pub busy: bool,
}

/// hello-ack 负载。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAckPayload {
    pub server_protocol_version: u32,
    pub device_id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// presence 快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub remote_control_enabled: bool,
    pub busy: bool,
    /// Unix 毫秒
    pub last_seen: i64,
}

/// 一次 invoke 的请求。
///
/// 手机端发起的操作都走这里：提交任务、查状态、读日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokePayload {
    /// 通道名，如 "harness.run" / "harness.status"
    pub channel: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// invoke 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeResultPayload {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip() {
        let env = Envelope::new(EnvelopeKind::Ping).with_id("r1");
        let s = env.encode().unwrap();
        let back = Envelope::decode(&s).unwrap();
        assert_eq!(back.kind, EnvelopeKind::Ping);
        assert_eq!(back.id.as_deref(), Some("r1"));
        assert_eq!(back.v, PROTOCOL_VERSION);
    }

    #[test]
    fn kind_uses_kebab_case() {
        let env = Envelope::new(EnvelopeKind::HelloAck);
        let s = env.encode().unwrap();
        assert!(s.contains("hello-ack"), "帧类型应为 kebab-case：{s}");
        // 解析回来也认
        assert_eq!(Envelope::decode(&s).unwrap().kind, EnvelopeKind::HelloAck);
    }

    #[test]
    fn oversized_frame_is_rejected_before_send() {
        // Cindy：超过即回错误并丢弃，不断连。
        // 这里是发送方先行拒绝。
        let big = "x".repeat(MAX_FRAME_BYTES + 10);
        let env = Envelope::new(EnvelopeKind::Push).with_payload(serde_json::json!({"data": big}));
        match env.encode() {
            Err(FrameError::PayloadTooLarge { bytes, max }) => {
                assert!(bytes > max);
            }
            other => panic!("应报帧过大，实际 {other:?}"),
        }
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let mut env = Envelope::new(EnvelopeKind::Ping);
        env.v = 999;
        let s = serde_json::to_string(&env).unwrap();
        match Envelope::decode(&s) {
            Err(FrameError::VersionMismatch { got, want }) => {
                assert_eq!(got, 999);
                assert_eq!(want, PROTOCOL_VERSION);
            }
            other => panic!("应报版本不匹配，实际 {other:?}"),
        }
    }

    #[test]
    fn src_is_overwritten_by_server() {
        // 防伪造：客户端传的 src 会被服务端覆盖
        let env = Envelope::new(EnvelopeKind::Invoke).with_payload(serde_json::json!({}));
        let mut forged = env.clone();
        forged.src = Some("pretend-to-be-someone".into());
        let stamped = forged.stamp_src("real-device-id");
        assert_eq!(stamped.src.as_deref(), Some("real-device-id"));
    }

    #[test]
    fn optional_fields_are_omitted() {
        // 空字段省略，不要序列化成 null
        let env = Envelope::new(EnvelopeKind::Pong);
        let s = env.encode().unwrap();
        assert!(!s.contains("null"), "空字段应省略：{s}");
        assert!(!s.contains("\"src\""));
    }

    #[test]
    fn hello_payload_roundtrip() {
        let p = HelloPayload {
            device_name: "iPhone".into(),
            platform: "ios".into(),
            app_version: "1.0".into(),
            remote_control_enabled: true,
            busy: false,
        };
        let env =
            Envelope::new(EnvelopeKind::Hello).with_payload(serde_json::to_value(&p).unwrap());
        let s = env.encode().unwrap();
        let back = Envelope::decode(&s).unwrap();
        let parsed: HelloPayload = serde_json::from_value(back.payload.unwrap()).unwrap();
        assert_eq!(parsed.device_name, "iPhone");
        assert!(parsed.remote_control_enabled);
    }

    #[test]
    fn invoke_roundtrip() {
        let req = InvokePayload {
            channel: "harness.run".into(),
            args: serde_json::json!({"task": "修 bug"}),
        };
        let env = Envelope::new(EnvelopeKind::Invoke)
            .with_id("q1")
            .with_dst("desktop-1")
            .with_payload(serde_json::to_value(&req).unwrap());
        let s = env.encode().unwrap();
        let back = Envelope::decode(&s).unwrap();
        assert_eq!(back.dst.as_deref(), Some("desktop-1"));
        let parsed: InvokePayload = serde_json::from_value(back.payload.unwrap()).unwrap();
        assert_eq!(parsed.channel, "harness.run");
    }

    #[test]
    fn invoke_result_carries_error_when_failed() {
        let r = InvokeResultPayload {
            ok: false,
            value: serde_json::Value::Null,
            error: Some("任务失败".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("任务失败"));
    }
}
