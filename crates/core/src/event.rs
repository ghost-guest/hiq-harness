//! L0 事件日志。
//!
//! 三个契约（来自 kiso）：
//! 1. 轨迹即产物：append-only、带 seq。模型看到的消息是日志的纯函数。
//! 2. 裁决即事实：审批结果持久化，kill -9 后已决定的不重问。
//! 3. 副作用留收据：开始了但没报告的是 Uncertain，必须阻塞等人。
//!
//! 性能纪律（来自 pi 实测）：不要每条事件单独 fsync。
//! 组提交实测把 26.8 runs/s 提到 250 runs/s。

use serde::{Deserialize, Serialize};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// 日志事件。seq 由单写入者单调递增。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    /// Unix 毫秒。跨写入者排序靠它，seq 只做 tiebreak。
    pub ts: i64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
        blocks: Vec<Block>,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        #[serde(flatten)]
        outcome: ToolOutcome,
    },
    /// 压缩检查点。覆盖边界用 through_seq 表达。
    Compaction {
        checkpoint_id: String,
        summary: String,
        /// 这个检查点覆盖到哪一条事件为止。
        through_seq: u64,
        source_digest: String,
    },
    /// 审批裁决是持久事实。
    PermissionDecided {
        call_id: String,
        verdict: Verdict,
        decided_by: String,
    },
    GoalSet {
        objective: String,
    },
    GoalDone {
        summary: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub kind: String,
    pub text: String,
}

/// 工具执行结果。三种形态，第三种是「不确定」。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok {
        output: String,
    },
    Err {
        error: String,
    },
    /// 开始了但没收到报告。必须阻塞等人决定，绝不静默重试。
    Uncertain {
        execution_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allow,
    Deny,
    Rerun,
    Abandon,
}

/// append-only 日志的文件后端。
///
/// 用 BufWriter + 显式 flush 实现组提交：多次 append 合并成一次写。
pub struct EventLog {
    path: PathBuf,
    writer: BufWriter<std::fs::File>,
    next_seq: u64,
    buf: Vec<Event>,
}

impl EventLog {
    /// 打开或创建日志。若已存在，从最后一条的 seq 继续。
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let existing = Self::read_all(&path)?;
        let next_seq = existing.last().map(|e| e.seq + 1).unwrap_or(0);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            next_seq,
            buf: Vec::new(),
        })
    }

    /// 追加一条事件到缓冲，不立即落盘。
    pub fn append(&mut self, kind: EventKind) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.buf.push(Event {
            seq,
            ts: now_ms(),
            kind,
        });
        seq
    }

    /// 提交缓冲区。这是一次真正的写 + fsync。
    ///
    /// 调用频率应远低于 append —— 这就是组提交。
    pub fn commit(&mut self) -> anyhow::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        for ev in &self.buf {
            serde_json::to_writer(&mut self.writer, ev)?;
            self.writer.write_all(b"\n")?;
        }
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        self.buf.clear();
        Ok(())
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 下一条事件将使用的 seq。用于检查点记录覆盖边界。
    pub fn next_seq_hint(&self) -> u64 {
        self.next_seq
    }

    /// 读回全部事件。坏行（崩溃留下的半行）跳过，不猜测。
    pub fn read_all(path: impl AsRef<Path>) -> anyhow::Result<Vec<Event>> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(path)?;
        let mut out = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(line) {
                Ok(ev) => out.push(ev),
                Err(e) => {
                    // 末尾半行是崩溃的正常残留，跳过；中间坏行说明数据损坏。
                    tracing::warn!("skip malformed event line: {e}");
                }
            }
        }
        Ok(out)
    }
}

/// 日志的投影：把事件折成模型可见的消息列表。
///
/// 这是「消息是日志的纯函数」的实现。
/// 压缩只影响投影，不改日志。
pub struct Projection<'a> {
    events: &'a [Event],
    /// 压缩检查点替换掉它覆盖的那段历史。
    checkpoints: Vec<(&'a str, u64)>,
}

impl<'a> Projection<'a> {
    pub fn new(events: &'a [Event]) -> Self {
        let mut checkpoints = Vec::new();
        for ev in events {
            if let EventKind::Compaction {
                summary,
                through_seq,
                ..
            } = &ev.kind
            {
                checkpoints.push((summary.as_str(), *through_seq));
            }
        }
        Self {
            events,
            checkpoints,
        }
    }

    /// 生成模型可见的消息。被检查点覆盖的历史折成一条摘要。
    pub fn messages(&self) -> Vec<ProjectedMessage> {
        // 没有压缩时 covered 为 None —— 不能用 0，否则 seq=0 的第一条会被误跳过。
        let covered = self.checkpoints.iter().map(|(_, s)| *s).max();
        let mut out = Vec::new();

        if let Some(through) = covered {
            if let Some((summary, _)) = self.checkpoints.last() {
                let more = self.checkpoints.len().saturating_sub(1);
                let note = if more > 0 {
                    format!("{summary}\n\n（此前另有 {more} 次压缩）")
                } else {
                    summary.to_string()
                };
                out.push(ProjectedMessage {
                    role: Role::User,
                    text: format!("[先前对话的摘要]\n{note}"),
                });
            }
            let _ = through;
        }

        for ev in self.events.iter().filter(|e| match covered {
            Some(through) => e.seq > through,
            None => true,
        }) {
            match &ev.kind {
                EventKind::UserMessage { text } => out.push(ProjectedMessage {
                    role: Role::User,
                    text: text.clone(),
                }),
                EventKind::AssistantMessage { text, .. } => out.push(ProjectedMessage {
                    role: Role::Assistant,
                    text: text.clone(),
                }),
                EventKind::ToolCall { name, input, .. } => out.push(ProjectedMessage {
                    role: Role::Assistant,
                    text: format!("[调用工具 {name}] {}", compact_json(input)),
                }),
                EventKind::ToolResult {
                    call_id,
                    outcome: ToolOutcome::Ok { output },
                } => out.push(ProjectedMessage {
                    role: Role::User,
                    text: format!("[工具结果 {call_id}]\n{output}"),
                }),
                EventKind::ToolResult {
                    call_id,
                    outcome: ToolOutcome::Err { error },
                } => out.push(ProjectedMessage {
                    role: Role::User,
                    text: format!("[工具失败 {call_id}]\n{error}"),
                }),
                EventKind::ToolResult {
                    call_id,
                    outcome: ToolOutcome::Uncertain { execution_id },
                } => out.push(ProjectedMessage {
                    role: Role::User,
                    text: format!(
                        "[工具结果未知 {call_id}]\n\
                         该调用的副作用是否生效未知（execution_id={execution_id}）。\n\
                         在此确认之前不得重复执行。"
                    ),
                }),
                EventKind::GoalSet { objective } => out.push(ProjectedMessage {
                    role: Role::User,
                    text: format!("[目标] {objective}"),
                }),
                EventKind::PermissionDecided { .. }
                | EventKind::Compaction { .. }
                | EventKind::GoalDone { .. } => {}
            }
        }
        out
    }

    /// 是否存在未决的 uncertain 副作用。
    pub fn has_unresolved_uncertain(&self) -> bool {
        let mut unresolved = Vec::new();
        for ev in self.events {
            match &ev.kind {
                EventKind::ToolResult {
                    call_id,
                    outcome: ToolOutcome::Uncertain { .. },
                } => unresolved.push(call_id.clone()),
                EventKind::PermissionDecided { call_id, .. } => {
                    unresolved.retain(|c| c != call_id);
                }
                _ => {}
            }
        }
        !unresolved.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct ProjectedMessage {
    pub role: Role,
    pub text: String,
}

fn compact_json(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    if s.len() > 200 {
        format!("{}…", &s[..200])
    } else {
        s
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_log() -> (tempdir_like::Dir, EventLog) {
        let dir = tempdir_like::Dir::new();
        let log = EventLog::open(dir.path().join("events.jsonl")).unwrap();
        (dir, log)
    }

    #[test]
    fn append_commit_and_replay() {
        let (_d, mut log) = tmp_log();
        log.append(EventKind::UserMessage { text: "hi".into() });
        log.append(EventKind::AssistantMessage {
            text: "hello".into(),
            blocks: vec![],
        });
        assert_eq!(log.pending(), 2);
        log.commit().unwrap();

        let events = EventLog::read_all(log.path()).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[1].seq, 1);

        let proj = Projection::new(&events);
        let msgs = proj.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
    }

    #[test]
    fn seq_continues_after_reopen() {
        let dir = tempdir_like::Dir::new();
        let path = dir.path().join("e.jsonl");
        {
            let mut log = EventLog::open(&path).unwrap();
            log.append(EventKind::UserMessage { text: "a".into() });
            log.commit().unwrap();
        }
        let mut log = EventLog::open(&path).unwrap();
        let seq = log.append(EventKind::UserMessage { text: "b".into() });
        assert_eq!(seq, 1, "重开后 seq 必须接着走");
    }

    #[test]
    fn compaction_replaces_covered_history() {
        let (_d, mut log) = tmp_log();
        log.append(EventKind::UserMessage {
            text: "old1".into(),
        });
        log.append(EventKind::UserMessage {
            text: "old2".into(),
        });
        log.append(EventKind::Compaction {
            checkpoint_id: "cp1".into(),
            summary: "前面聊了 old1 和 old2".into(),
            through_seq: 1,
            source_digest: "abc".into(),
        });
        log.append(EventKind::UserMessage { text: "new".into() });
        log.commit().unwrap();

        let events = EventLog::read_all(log.path()).unwrap();
        let msgs = Projection::new(&events).messages();
        // 摘要一条 + new 一条；old1/old2 被覆盖
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].text.contains("old1 和 old2"));
        assert_eq!(msgs[1].text, "new");
    }

    #[test]
    fn uncertain_blocks_until_decided() {
        let (_d, mut log) = tmp_log();
        log.append(EventKind::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({"path": "x"}),
        });
        log.append(EventKind::ToolResult {
            call_id: "c1".into(),
            outcome: ToolOutcome::Uncertain {
                execution_id: "exec-1".into(),
            },
        });
        log.commit().unwrap();
        let events = EventLog::read_all(log.path()).unwrap();
        assert!(Projection::new(&events).has_unresolved_uncertain());

        // 裁决后解除
        let mut log = EventLog::open(log.path()).unwrap();
        log.append(EventKind::PermissionDecided {
            call_id: "c1".into(),
            verdict: Verdict::Abandon,
            decided_by: "human".into(),
        });
        log.commit().unwrap();
        let events = EventLog::read_all(log.path()).unwrap();
        assert!(!Projection::new(&events).has_unresolved_uncertain());
    }

    #[test]
    fn broken_tail_line_is_skipped() {
        let dir = tempdir_like::Dir::new();
        let path = dir.path().join("e.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        log.append(EventKind::UserMessage { text: "ok".into() });
        log.commit().unwrap();
        // 模拟崩溃留下的半行
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"seq\":1,\"ts\":12")
            .unwrap();
        let events = EventLog::read_all(&path).unwrap();
        assert_eq!(events.len(), 1, "坏尾行应被跳过");
    }

    mod tempdir_like {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                // 计数器必须带上：now_ms() 在同一毫秒内会重复，
                // 共用同一目录会让测试互相看到对方的文件。
                let mut p = std::env::temp_dir();
                p.push(format!(
                    "harness-test-{}-{}-{}",
                    std::process::id(),
                    super::now_ms(),
                    SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
