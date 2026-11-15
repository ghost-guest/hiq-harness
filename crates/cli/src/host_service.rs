//! Host 服务：本地 HTTP + WebSocket，供桌面与手机共享。
//!
//! 架构参考 Cindy（`apps/host`）：
//! - **只绑 127.0.0.1**：不对外暴露，跨设备走隧道
//! - **HTTP + WS 同端口**：HTTP 供简单查询，WS 供事件流
//! - **会话即信封**：所有 WS 帧走 `Envelope`（见 `harness_core::wire`）
//!
//! 手机连上来的方式（初版）：
//! 1. 同一局域网 → 直连 `<desktop-ip>:<port>`
//! 2. 跨网段 → 后续加中继（协议已预留 `dst`）

use harness_core::wire::{
    Envelope, EnvelopeKind, HelloAckPayload, HelloPayload, InvokePayload, InvokeResultPayload,
    Presence, PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 在线设备表。多端共享。
#[derive(Debug, Default)]
pub struct DeviceRegistry {
    devices: HashMap<String, Presence>,
    next_id: u64,
}

impl DeviceRegistry {
    /// 显式构造。`Default` 也提供，这里保留便于阅读时一眼看出意图。
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一台设备，返回分配的 device id。
    pub fn register(&mut self, hello: &HelloPayload, now: i64) -> String {
        let id = format!("dev-{:04}", self.next_id);
        self.next_id += 1;
        self.devices.insert(
            id.clone(),
            Presence {
                device_id: id.clone(),
                device_name: hello.device_name.clone(),
                platform: hello.platform.clone(),
                remote_control_enabled: hello.remote_control_enabled,
                busy: hello.busy,
                last_seen: now,
            },
        );
        id
    }

    pub fn touch(&mut self, id: &str, now: i64) {
        if let Some(d) = self.devices.get_mut(id) {
            d.last_seen = now;
        }
    }

    pub fn set_busy(&mut self, id: &str, busy: bool) {
        if let Some(d) = self.devices.get_mut(id) {
            d.busy = busy;
        }
    }

    pub fn get(&self, id: &str) -> Option<&Presence> {
        self.devices.get(id)
    }

    pub fn all(&self) -> Vec<&Presence> {
        let mut v: Vec<&Presence> = self.devices.values().collect();
        v.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        v
    }

    pub fn count(&self) -> usize {
        self.devices.len()
    }

    /// 清理超时设备。TTL = 心跳间隔的 3 倍。
    pub fn reap(&mut self, now: i64, ttl_secs: u64) -> Vec<String> {
        let cutoff = now - (ttl_secs * 1000) as i64;
        let dead: Vec<String> = self
            .devices
            .iter()
            .filter(|(_, p)| p.last_seen < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for id in &dead {
            self.devices.remove(id);
        }
        dead
    }
}

/// 可被远程调用的通道。
///
/// 手机通过 `InvokePayload { channel, args }` 触发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// 提交一个任务
    Run,
    /// 查询运行状态
    Status,
    /// 读取事件日志
    Events,
    /// 查询记忆
    Memory,
    /// 查看未决副作用
    Pending,
}

impl Channel {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "harness.run" => Some(Self::Run),
            "harness.status" => Some(Self::Status),
            "harness.events" => Some(Self::Events),
            "harness.memory" => Some(Self::Memory),
            "harness.pending" => Some(Self::Pending),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Run => "harness.run",
            Self::Status => "harness.status",
            Self::Events => "harness.events",
            Self::Memory => "harness.memory",
            Self::Pending => "harness.pending",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    NotAllowed,
    Unknown,
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAllowed => write!(f, "该操作需要被控端允许远程控制"),
            Self::Unknown => write!(f, "未知通道"),
        }
    }
}

/// 检查一次 invoke 是否允许。
///
/// 远程控制是否启用由**被控端**决定，手机不能自己开启 ——
/// 这是 Cindy `remoteControlEnabled` 的语义。
pub fn authorize(target: &Presence, channel: Channel) -> Result<(), ChannelError> {
    match channel {
        // 只读操作不要求远程控制
        Channel::Status | Channel::Events | Channel::Memory | Channel::Pending => Ok(()),
        // 会改变世界的操作必须被控端允许
        Channel::Run => {
            if target.remote_control_enabled {
                Ok(())
            } else {
                Err(ChannelError::NotAllowed)
            }
        }
    }
}

/// 协议层的一次交互处理。
///
/// 返回要回给发送方的帧。`None` 表示不需要回。
pub fn handle(
    env: Envelope,
    from_device: &str,
    registry: &mut DeviceRegistry,
    now: i64,
) -> Option<Envelope> {
    match env.kind {
        EnvelopeKind::Ping => {
            registry.touch(from_device, now);
            Some(Envelope::new(EnvelopeKind::Pong).with_id(env.id.unwrap_or_default()))
        }

        EnvelopeKind::Hello => {
            let hello: HelloPayload = serde_json::from_value(env.payload?).ok()?;
            let id = registry.register(&hello, now);
            let ack = HelloAckPayload {
                server_protocol_version: PROTOCOL_VERSION,
                device_id: id,
                capabilities: vec![
                    "harness.run".into(),
                    "harness.status".into(),
                    "harness.events".into(),
                    "harness.memory".into(),
                    "harness.pending".into(),
                ],
            };
            Some(
                Envelope::new(EnvelopeKind::HelloAck)
                    .with_payload(serde_json::to_value(ack).unwrap()),
            )
        }

        EnvelopeKind::PresenceSet => {
            if let Some(busy) = env.payload.as_ref().and_then(|p| p.get("busy")) {
                if let Some(b) = busy.as_bool() {
                    registry.set_busy(from_device, b);
                }
            }
            None
        }

        EnvelopeKind::Invoke => {
            let req: InvokePayload = serde_json::from_value(env.payload.clone()?).ok()?;
            let id = env.id.clone().unwrap_or_default();
            let channel = match Channel::parse(&req.channel) {
                Some(c) => c,
                None => {
                    return Some(
                        Envelope::new(EnvelopeKind::InvokeResult)
                            .with_id(id)
                            .with_payload(
                                serde_json::to_value(InvokeResultPayload {
                                    ok: false,
                                    value: serde_json::Value::Null,
                                    error: Some(ChannelError::Unknown.to_string()),
                                })
                                .unwrap(),
                            ),
                    );
                }
            };

            // 权限检查：被控端必须允许
            let target_id = env.dst.as_deref().unwrap_or(from_device);
            let allowed = registry
                .get(target_id)
                .map(|t| authorize(t, channel).is_ok())
                .unwrap_or(false);

            let result = if allowed {
                InvokeResultPayload {
                    ok: true,
                    value: serde_json::json!({"channel": channel.name(), "accepted": true}),
                    error: None,
                }
            } else {
                InvokeResultPayload {
                    ok: false,
                    value: serde_json::Value::Null,
                    error: Some(ChannelError::NotAllowed.to_string()),
                }
            };

            Some(
                Envelope::new(EnvelopeKind::InvokeResult)
                    .with_id(id)
                    .with_payload(serde_json::to_value(result).unwrap()),
            )
        }

        // 隧道层的转发由连接管理负责，这里不处理
        EnvelopeKind::LinkOpen
        | EnvelopeKind::LinkAccept
        | EnvelopeKind::LinkClose
        | EnvelopeKind::Push
        | EnvelopeKind::InvokeResult
        | EnvelopeKind::HelloAck
        | EnvelopeKind::PresenceChanged
        | EnvelopeKind::Pong
        | EnvelopeKind::RelayError => None,
    }
}

/// 共享状态。HTTP 与 WS 处理器都持有它。
pub type SharedState = Arc<Mutex<HostState>>;

#[derive(Debug, Default)]
pub struct HostState {
    pub registry: DeviceRegistry,
}

impl HostState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub fn shared() -> SharedState {
    Arc::new(Mutex::new(HostState::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(name: &str, remote: bool) -> HelloPayload {
        HelloPayload {
            device_name: name.into(),
            platform: "macos".into(),
            app_version: "0.1.0".into(),
            remote_control_enabled: remote,
            busy: false,
        }
    }

    #[test]
    fn register_and_count() {
        let mut r = DeviceRegistry::new();
        let id = r.register(&hello("MacBook", true), 1000);
        assert_eq!(r.count(), 1);
        assert_eq!(r.get(&id).unwrap().device_name, "MacBook");
    }

    #[test]
    fn heartbeat_touches_last_seen() {
        let mut r = DeviceRegistry::new();
        let id = r.register(&hello("Mac", true), 1000);
        r.touch(&id, 5000);
        assert_eq!(r.get(&id).unwrap().last_seen, 5000);
    }

    #[test]
    fn reap_removes_stale_devices() {
        let mut r = DeviceRegistry::new();
        let a = r.register(&hello("a", true), 0);
        let b = r.register(&hello("b", true), 60_000);
        // TTL 60 秒，当前 120 秒 → a 超时，b 还在
        let dead = r.reap(120_000, 60);
        assert_eq!(dead, vec![a]);
        assert_eq!(r.count(), 1);
        assert!(r.get(&b).is_some());
    }

    #[test]
    fn run_requires_remote_control() {
        let target = Presence {
            device_id: "d1".into(),
            device_name: "Mac".into(),
            platform: "macos".into(),
            remote_control_enabled: false,
            busy: false,
            last_seen: 0,
        };
        assert_eq!(
            authorize(&target, Channel::Run),
            Err(ChannelError::NotAllowed),
            "未开启远程控制时不能提交任务"
        );
        // 只读通道不要求
        assert!(authorize(&target, Channel::Status).is_ok());
        assert!(authorize(&target, Channel::Events).is_ok());
    }

    #[test]
    fn run_allowed_when_remote_enabled() {
        let target = Presence {
            device_id: "d1".into(),
            device_name: "Mac".into(),
            platform: "macos".into(),
            remote_control_enabled: true,
            busy: false,
            last_seen: 0,
        };
        assert!(authorize(&target, Channel::Run).is_ok());
    }

    #[test]
    fn ping_gets_pong() {
        let mut r = DeviceRegistry::new();
        let id = r.register(&hello("d", true), 0);
        let reply = handle(
            Envelope::new(EnvelopeKind::Ping).with_id("p1"),
            &id,
            &mut r,
            100,
        )
        .unwrap();
        assert_eq!(reply.kind, EnvelopeKind::Pong);
        assert_eq!(reply.id.as_deref(), Some("p1"));
    }

    #[test]
    fn hello_gets_ack_with_device_id() {
        let mut r = DeviceRegistry::new();
        let env = Envelope::new(EnvelopeKind::Hello)
            .with_payload(serde_json::to_value(hello("iPhone", true)).unwrap());
        let reply = handle(env, "unknown", &mut r, 0).unwrap();
        assert_eq!(reply.kind, EnvelopeKind::HelloAck);
        let ack: HelloAckPayload = serde_json::from_value(reply.payload.unwrap()).unwrap();
        assert!(!ack.device_id.is_empty());
        assert_eq!(ack.server_protocol_version, PROTOCOL_VERSION);
        assert!(ack.capabilities.contains(&"harness.run".to_string()));
    }

    #[test]
    fn invoke_rejected_when_remote_disabled() {
        let mut r = DeviceRegistry::new();
        let desktop = r.register(&hello("Mac", false), 0);
        let req = InvokePayload {
            channel: "harness.run".into(),
            args: serde_json::json!({"task": "x"}),
        };
        let env = Envelope::new(EnvelopeKind::Invoke)
            .with_id("q1")
            .with_dst(&desktop)
            .with_payload(serde_json::to_value(req).unwrap());
        let reply = handle(env, "phone", &mut r, 0).unwrap();
        let res: InvokeResultPayload = serde_json::from_value(reply.payload.unwrap()).unwrap();
        assert!(!res.ok);
        assert!(res.error.unwrap().contains("远程控制"));
    }

    #[test]
    fn invoke_allowed_when_remote_enabled() {
        let mut r = DeviceRegistry::new();
        let desktop = r.register(&hello("Mac", true), 0);
        let req = InvokePayload {
            channel: "harness.run".into(),
            args: serde_json::json!({"task": "x"}),
        };
        let env = Envelope::new(EnvelopeKind::Invoke)
            .with_id("q2")
            .with_dst(&desktop)
            .with_payload(serde_json::to_value(req).unwrap());
        let reply = handle(env, "phone", &mut r, 0).unwrap();
        let res: InvokeResultPayload = serde_json::from_value(reply.payload.unwrap()).unwrap();
        assert!(res.ok, "开启后应允许，实际 {:?}", res.error);
    }

    #[test]
    fn unknown_channel_is_rejected() {
        let mut r = DeviceRegistry::new();
        let d = r.register(&hello("Mac", true), 0);
        let req = InvokePayload {
            channel: "harness.destroy".into(),
            args: serde_json::Value::Null,
        };
        let env = Envelope::new(EnvelopeKind::Invoke)
            .with_id("q3")
            .with_dst(&d)
            .with_payload(serde_json::to_value(req).unwrap());
        let reply = handle(env, "phone", &mut r, 0).unwrap();
        let res: InvokeResultPayload = serde_json::from_value(reply.payload.unwrap()).unwrap();
        assert!(!res.ok);
        assert!(res.error.unwrap().contains("未知通道"));
    }

    #[test]
    fn channel_names_roundtrip() {
        for c in [
            Channel::Run,
            Channel::Status,
            Channel::Events,
            Channel::Memory,
            Channel::Pending,
        ] {
            assert_eq!(Channel::parse(c.name()), Some(c));
        }
    }
}
