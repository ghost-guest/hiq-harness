//! REST 处理器：把请求转成 CLI 已有的能力。
//!
//! 每个处理器都调用 `harness-*` crate 的真实 API，不重造逻辑。

use crate::durable::DurableSession;
use crate::memory::ProjectMemory;
use crate::routes::{kind_name, parse_limit, summarize, EventView};
use harness_core::event::EventLog;
use serde_json::{json, Value};
use std::path::Path;

/// 处理一次 REST 请求，返回 (状态码, JSON)。
pub struct RestHandler {
    pub dir: std::path::PathBuf,
}

impl RestHandler {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn log_path(&self) -> std::path::PathBuf {
        self.dir.join("events.jsonl")
    }

    pub fn health(&self, devices: usize) -> (u16, Value) {
        (
            200,
            json!({
                "ok": true,
                "protocol_version": harness_core::wire::PROTOCOL_VERSION,
                "devices": devices,
                "heartbeat_secs": harness_core::wire::HEARTBEAT_SECS,
            }),
        )
    }

    pub fn events(&self, path: &str) -> (u16, Value) {
        let limit = parse_limit(path, 50);
        match EventLog::read_all(self.log_path()) {
            Ok(all) => {
                let start = all.len().saturating_sub(limit);
                let events: Vec<EventView> = all[start..]
                    .iter()
                    .map(|e| EventView {
                        seq: e.seq,
                        kind: kind_name(&e.kind).to_string(),
                        summary: summarize(&e.kind),
                    })
                    .collect();
                (200, json!({ "events": events, "total": all.len() }))
            }
            Err(e) => (500, json!({ "error": e.to_string() })),
        }
    }

    pub fn memory(&self) -> (u16, Value) {
        match ProjectMemory::open(&self.dir) {
            Ok(m) => {
                let (active, archived) = m.stats();
                let entries: Vec<Value> = m
                    .entries_view()
                    .into_iter()
                    .map(|(kind, content)| json!({"kind": kind, "content": content}))
                    .collect();
                (
                    200,
                    json!({"active": active, "archived": archived, "entries": entries}),
                )
            }
            Err(e) => (500, json!({ "error": e.to_string() })),
        }
    }

    pub fn add_memory(&self, body: &Value) -> (u16, Value) {
        let Some(content) = body.get("content").and_then(|v| v.as_str()) else {
            return (400, json!({"error": "缺少 content"}));
        };
        let kind = match body.get("kind").and_then(|v| v.as_str()) {
            Some("decision") => harness_memory::MemoryKind::Decision,
            Some("constraint") => harness_memory::MemoryKind::Constraint,
            Some("preference") => harness_memory::MemoryKind::Preference,
            Some("lesson") => harness_memory::MemoryKind::Lesson,
            _ => harness_memory::MemoryKind::Fact,
        };
        match ProjectMemory::open(&self.dir) {
            Ok(mut m) => {
                if m.remember(kind, content, &["project".to_string()]) {
                    (200, json!({"ok": true}))
                } else {
                    (
                        400,
                        json!({"error": "被拒绝：可能含敏感信息，或未说明可迁移范围"}),
                    )
                }
            }
            Err(e) => (500, json!({ "error": e.to_string() })),
        }
    }

    pub fn pending(&self) -> (u16, Value) {
        match DurableSession::open(&self.dir) {
            Ok(s) => {
                let p = s.pending_uncertain();
                (200, json!({"pending": p, "count": p.len()}))
            }
            Err(e) => (500, json!({ "error": e.to_string() })),
        }
    }

    pub fn resolve(&self, body: &Value) -> (u16, Value) {
        let Some(exec_id) = body.get("exec_id").and_then(|v| v.as_str()) else {
            return (400, json!({"error": "缺少 exec_id"}));
        };
        let verdict = match body.get("verdict").and_then(|v| v.as_str()) {
            Some("rerun") => crate::durable::Verdict::Rerun,
            Some("abandon") => crate::durable::Verdict::Abandon,
            _ => return (400, json!({"error": "verdict 必须是 rerun 或 abandon"})),
        };
        match DurableSession::open(&self.dir) {
            Ok(mut s) => match s.resolve(exec_id, verdict) {
                Ok(_) => (200, json!({"ok": true})),
                Err(e) => (400, json!({"error": e.to_string()})),
            },
            Err(e) => (500, json!({ "error": e.to_string() })),
        }
    }
}

/// 判断工作区里是否有可用的 Go 插件目录。
///
/// 预留给 UI 的插件页（尚未接入）。
#[allow(dead_code)]
pub fn has_plugins(workdir: &Path) -> bool {
    workdir.join("plugins").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rest-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn health_reports_protocol() {
        let dir = tmpdir("health");
        let h = RestHandler::new(&dir);
        let (code, body) = h.health(2);
        assert_eq!(code, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["devices"], 2);
        assert_eq!(body["protocol_version"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_empty_is_ok() {
        let dir = tmpdir("events");
        let h = RestHandler::new(&dir);
        let (code, body) = h.events("/events");
        assert_eq!(code, 200);
        assert_eq!(body["total"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_respects_limit() {
        let dir = tmpdir("limit");
        let path = dir.join("events.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        for i in 0..10 {
            log.append(harness_core::event::EventKind::UserMessage {
                text: format!("msg {i}"),
            });
        }
        log.commit().unwrap();

        let h = RestHandler::new(&dir);
        let (code, body) = h.events("/events?limit=3");
        assert_eq!(code, 200);
        assert_eq!(body["events"].as_array().unwrap().len(), 3);
        assert_eq!(body["total"], 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_add_and_list() {
        let dir = tmpdir("mem");
        let h = RestHandler::new(&dir);
        let (code, _) = h.add_memory(&json!({"content": "项目用 rust", "kind": "decision"}));
        assert_eq!(code, 200);
        let (code, body) = h.memory();
        assert_eq!(code, 200);
        assert_eq!(body["active"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_rejects_secret() {
        let dir = tmpdir("secret");
        let h = RestHandler::new(&dir);
        // 用 AKIA 前缀（AWS key），不与占位符机制冲突
        let (code, body) = h.add_memory(&json!({"content": "aws AKIAIOSFODNN7EXAMPLE"}));
        assert_eq!(code, 400);
        assert!(body["error"].as_str().unwrap().contains("拒绝"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_empty() {
        let dir = tmpdir("pending");
        let h = RestHandler::new(&dir);
        let (code, body) = h.pending();
        assert_eq!(code, 200);
        assert_eq!(body["count"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_validates_input() {
        let dir = tmpdir("resolve");
        let h = RestHandler::new(&dir);
        let (code, _) = h.resolve(&json!({}));
        assert_eq!(code, 400, "缺 exec_id 应报 400");
        let (code, _) = h.resolve(&json!({"exec_id": "e1", "verdict": "bogus"}));
        assert_eq!(code, 400, "非法 verdict 应报 400");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
