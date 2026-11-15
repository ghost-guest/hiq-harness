//! 持久化会话：把收据账本与检查点存到磁盘，支持崩溃恢复。
//!
//! 设计来自调研：
//! - **副作用收据**（kiso `durability.md`）：已确认的绝不重跑，
//!   开始了但没报告的是 `Uncertain`，必须阻塞等人。
//! - **检查点 + 记忆依赖**（Codewhale `protocol.rs:278`）：
//!   记忆失效则拒绝交出 state。
//! - **租约 + 代次围栏**（LoopForge `run_recovery.go`）：
//!   防旧实例复活后写乱。
//!
//! 存储形态是单个 JSON 文件（`<dir>/session.json`），
//! 原子写：先写临时文件再 rename，避免半截文件。
//!
//! 契约（kiso `docs/durability.md` 的六条）：
//! - **Committed Intent Before Effect**：工具调用在产生任何效果前
//!   就已决定并持久化；审批是持久事实，不是内存状态。
//! - **Durable Start Before Side Effect**：处理器绝不在 STARTED 收据
//!   落盘之前运行 —— 崩溃不能留下未报告的副作用。
//! - **Ambiguity Never Auto-Repeats**：开始了但没报告的，
//!   永远由人决定，绝不自动重跑，也绝不静默重问。
//! - **Stable Intent Identity**：callId / invocationSeq / executionId
//!   三个身份永不混用；派生状态永不持久化。

use harness_persist::{
    resume, CheckpointStore, LeaseManager, MemoryIndex, MemoryRef, Receipt, ReceiptLedger,
    ResumeResult,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 模型给出的调用 id。**只用于对应 tool_call 与 tool_result**，
/// 不用于副作用去重。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallId(pub String);

/// 本次会话内的调用序号。单调递增，用于排序与审计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct InvocationSeq(pub u64);

/// 副作用执行的持久身份。**收据按它索引**。
///
/// kiso 的 `Stable Intent Identity`：三者永不混用。
/// 同一个 callId 重试会得到新的 executionId —— 这正是
/// 「不确定的执行绝不被当成同一次执行」的实现基础。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecutionId(pub String);

impl ExecutionId {
    /// 由 invocationSeq 派生。确定性，便于审计。
    pub fn derive(seq: InvocationSeq) -> Self {
        Self(format!("exec-{:06}", seq.0))
    }
}

/// 磁盘上的会话状态。
#[derive(Debug, Serialize, Deserialize, Default)]
struct SessionFile {
    version: u32,
    /// 收据：executionId → 状态
    receipts: Vec<(String, Receipt)>,
    /// 检查点
    checkpoints: Vec<PersistedCheckpoint>,
    /// 租约
    lease: Option<harness_persist::Lease>,
    /// 下一个检查点序号
    next_checkpoint: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedCheckpoint {
    id: String,
    through_seq: u64,
    created_at: i64,
    payload: String,
    memory_refs: Vec<MemoryRef>,
}

/// 持久化会话。
pub struct DurableSession {
    path: PathBuf,
    ledger: ReceiptLedger,
    checkpoints: CheckpointStore,
    leases: LeaseManager,
}

impl DurableSession {
    /// 打开或创建。
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("session.json");
        let file = Self::load(&path)?;

        let mut ledger = ReceiptLedger::new();
        for (id, r) in file.receipts {
            match r {
                Receipt::Confirmed { at } => {
                    ledger.begin(&id, at);
                    ledger.confirm(&id, at)?;
                }
                Receipt::Failed { at, error } => {
                    ledger.begin(&id, at);
                    ledger.fail(&id, &error, at)?;
                }
                Receipt::Uncertain { started_at } => {
                    ledger.begin(&id, started_at);
                }
            }
        }

        let mut checkpoints = CheckpointStore::new();
        for cp in file.checkpoints {
            checkpoints.save(cp.through_seq, &cp.payload, cp.memory_refs, cp.created_at);
        }

        let mut leases = LeaseManager::new();
        if let Some(l) = file.lease {
            // 恢复租约（LoopForge `run_recovery.go:133`）：
            // **只有过期才允许接管**，未过期的说明原实例可能还活着。
            //
            // 这里只做状态恢复：过期时把 until 保留在过去的时刻，
            // 让调用方的 `acquire` 自然能接管（代次递增）。
            // 未过期时原样恢复，别人拿不到。
            leases.restore(l);
        }

        Ok(Self {
            path,
            ledger,
            checkpoints,
            leases,
        })
    }

    fn load(path: &Path) -> anyhow::Result<SessionFile> {
        if !path.exists() {
            return Ok(SessionFile {
                version: 1,
                ..Default::default()
            });
        }
        let text = std::fs::read_to_string(path)?;
        if text.trim().is_empty() {
            return Ok(SessionFile {
                version: 1,
                ..Default::default()
            });
        }
        Ok(serde_json::from_str(&text).unwrap_or(SessionFile {
            version: 1,
            ..Default::default()
        }))
    }

    /// 原子写回磁盘。
    fn flush(&self) -> anyhow::Result<()> {
        let receipts: Vec<(String, Receipt)> = self
            .ledger
            .all()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let checkpoints: Vec<PersistedCheckpoint> = self
            .checkpoints
            .all()
            .into_iter()
            .map(|c| PersistedCheckpoint {
                id: c.id.clone(),
                through_seq: c.through_seq,
                created_at: c.created_at,
                payload: c.payload.clone(),
                memory_refs: c.memory_refs.clone(),
            })
            .collect();
        let next_checkpoint = checkpoints.len() as u64;
        let file = SessionFile {
            version: 1,
            receipts,
            checkpoints,
            lease: self.leases.current().cloned(),
            next_checkpoint,
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&file)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    // ── 收据 ──

    /// 开始一次副作用。落盘后返回 execution id。
    ///
    /// 关键：**先落盘再执行**。这样崩溃后能知道「这个副作用可能发生了」。
    pub fn begin_effect(&mut self, exec_id: &str) -> anyhow::Result<()> {
        self.ledger.begin(exec_id, now_ms());
        self.flush()
    }

    /// 确认副作用成功。已确认的会报错。
    pub fn confirm_effect(&mut self, exec_id: &str) -> anyhow::Result<()> {
        self.ledger.confirm(exec_id, now_ms())?;
        self.flush()
    }

    /// 确认副作用失败。
    pub fn fail_effect(&mut self, exec_id: &str, err: &str) -> anyhow::Result<()> {
        self.ledger.fail(exec_id, err, now_ms())?;
        self.flush()
    }

    /// 下一个可用的 invocation 序号。
    ///
    /// kiso 的 `ex-<seq>` 方案里 seq 在会话内单调、不跨运行重置。
    /// 否则新一次运行会撞上上一轮已确认的 executionId，
    /// 被契约拦住（"已确认或未决，不可自动重跑"）。
    pub fn next_invocation_seq(&self) -> u64 {
        self.ledger
            .all()
            .filter_map(|(id, _)| id.strip_prefix("exec-"))
            .filter_map(|n| n.parse::<u64>().ok())
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
    }

    /// 未决的 uncertain 列表。恢复时必须先处理。
    pub fn pending_uncertain(&self) -> Vec<String> {
        self.ledger.pending_uncertain()
    }

    /// 能否安全重试某个执行。
    ///
    /// 供 CLI 查询（`harness pending` 的补充信息）。
    /// `run_effect` 内部也用它做守卫。
    pub fn can_retry(&self, exec_id: &str) -> bool {
        self.ledger.can_retry(exec_id)
    }

    /// 遗忘一条记忆时，连带撤销引用它的检查点。
    ///
    /// Codewhale 的连带语义：记忆不成立，基于它的检查点也不能再用。
    /// 供记忆层在 forget 后调用。
    pub fn on_memory_forgotten(&mut self, memory_id: &str) -> anyhow::Result<usize> {
        self.invalidate_by_memory(memory_id)
    }

    // ── 副作用执行（kiso 契约）──

    /// 按 kiso 契约执行一次副作用。
    ///
    /// 顺序严格：
    /// 1. **先落盘 STARTED 收据**（Durable Start Before Side Effect）
    /// 2. 再跑 handler
    /// 3. 落盘结果（成功 → Confirmed，失败 → Failed）
    ///
    /// 若进程在第 1 步和第 3 步之间崩溃，收据停在 `Uncertain`，
    /// 恢复时**必须由人裁决**（Ambiguity Never Auto-Repeats）。
    ///
    /// `retry_if_settled` 为 false 时，已 Confirmed 的执行直接返回缓存结果，
    /// 绝不重跑。
    pub fn run_effect<T>(
        &mut self,
        exec: &ExecutionId,
        retry_if_settled: bool,
        handler: impl FnOnce() -> Result<T, String>,
        cached: impl FnOnce() -> Option<T>,
    ) -> EffectOutcome<T> {
        // 已确认的：绝不重跑
        if !self.ledger.can_retry(&exec.0) {
            if let Some(v) = cached() {
                return EffectOutcome::Replayed(v);
            }
            return EffectOutcome::Blocked {
                reason: format!("{} 已确认或未决，不可自动重跑；需人工裁决", exec.0),
            };
        }

        // 1. 先落盘 STARTED
        if let Err(e) = self.begin_effect(&exec.0) {
            return EffectOutcome::Blocked {
                reason: format!("无法持久化 STARTED 收据：{e}"),
            };
        }

        // 2. 跑 handler
        match handler() {
            Ok(v) => {
                // 3a. 成功 → Confirmed
                let _ = self.confirm_effect(&exec.0);
                EffectOutcome::Completed(v)
            }
            Err(e) => {
                // 3b. 失败 → Failed（可重试）
                let _ = self.fail_effect(&exec.0, &e);
                if retry_if_settled {
                    EffectOutcome::FailedRetryable(e)
                } else {
                    EffectOutcome::Failed(e)
                }
            }
        }
    }

    /// 人工裁决一个未决副作用（kiso 的四种 verdict）。
    ///
    /// - `Rerun`：确认副作用未生效，允许重跑
    /// - `Abandon`：确认副作用已生效（或不再处理），不再重跑
    ///
    /// 裁决是**持久事实**：写盘后，重开也不会再问。
    pub fn resolve(&mut self, exec_id: &str, verdict: Verdict) -> anyhow::Result<()> {
        match verdict {
            Verdict::Rerun => {
                // 当成失败，允许重试
                self.ledger.resolve_uncertain(exec_id, false, now_ms())?;
            }
            Verdict::Abandon => {
                // 当成已生效，不再重跑
                self.ledger.resolve_uncertain(exec_id, true, now_ms())?;
            }
        }
        self.flush()
    }

    // ── 检查点 ──

    pub fn save_checkpoint(
        &mut self,
        through_seq: u64,
        payload: &str,
        memory_refs: Vec<MemoryRef>,
    ) -> anyhow::Result<String> {
        let id = self
            .checkpoints
            .save(through_seq, payload, memory_refs, now_ms());
        self.flush()?;
        Ok(id)
    }

    pub fn latest_checkpoint(&self) -> Option<&harness_persist::Checkpoint> {
        self.checkpoints.latest()
    }

    /// 从最新检查点恢复。
    ///
    /// 记忆失效则返回 `state_usable: false` 且 `state: None`。
    pub fn resume(&self, index: &MemoryIndex) -> Option<ResumeResult> {
        self.checkpoints.latest().map(|cp| resume(cp, index))
    }

    /// 记忆失效时连带撤销引用它的检查点。
    pub fn invalidate_by_memory(&mut self, memory_id: &str) -> anyhow::Result<usize> {
        let n = self.checkpoints.invalidate_by_memory(memory_id);
        if n > 0 {
            self.flush()?;
        }
        Ok(n)
    }

    // ── 租约 ──

    pub fn acquire_lease(&mut self, owner: &str) -> anyhow::Result<u64> {
        let prev = self.leases.current().cloned();
        let lease = self.leases.acquire(owner, now_ms(), 300_000)?;
        // 接管了别人的过期租约 → 记审计（LoopForge 的做法）
        if let Some(p) = prev {
            if p.owner != owner {
                tracing::info!(
                    "过期执行租约已接管（generation={}，previousOwner={}）",
                    lease.generation,
                    p.owner
                );
            }
        }
        self.flush()?;
        Ok(lease.generation)
    }

    pub fn check_writer(&self, owner: &str, generation: u64) -> anyhow::Result<()> {
        self.leases.check_writer(owner, generation)?;
        Ok(())
    }
}

/// 人工对未决副作用的裁决（kiso `durability.md:24`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 副作用没生效，允许重跑
    Rerun,
    /// 副作用已生效（或不再处理），不重跑
    Abandon,
}

/// 副作用执行结果。
#[derive(Debug)]
pub enum EffectOutcome<T> {
    /// 本次真的执行了
    Completed(T),
    /// 之前已执行过，直接返回缓存结果（没有重跑）
    Replayed(T),
    /// 执行失败，可重试
    FailedRetryable(String),
    /// 执行失败，不可重试
    Failed(String),
    /// 被契约拦住（已确认或未决）
    Blocked { reason: String },
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_persist::PersistError;

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "durable-{}-{}-{}",
            name,
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn receipt_survives_reopen() {
        let dir = tmpdir("receipt");
        {
            let mut s = DurableSession::open(&dir).unwrap();
            s.begin_effect("e1").unwrap();
            s.confirm_effect("e1").unwrap();
            s.begin_effect("e2").unwrap();
        }
        // 重开
        let s = DurableSession::open(&dir).unwrap();
        assert_eq!(s.pending_uncertain(), vec!["e2".to_string()]);
        assert!(!s.can_retry("e1"), "已确认的绝不重跑");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirmed_cannot_confirm_twice() {
        let dir = tmpdir("twice");
        let mut s = DurableSession::open(&dir).unwrap();
        s.begin_effect("e1").unwrap();
        s.confirm_effect("e1").unwrap();
        assert!(matches!(
            s.confirm_effect("e1"),
            Err(e) if matches!(e.downcast_ref::<PersistError>(), Some(PersistError::AlreadyConfirmed(_)))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uncertain_blocks_after_crash() {
        let dir = tmpdir("uncertain");
        {
            let mut s = DurableSession::open(&dir).unwrap();
            s.begin_effect("risky-write").unwrap();
            // 模拟崩溃：不确认也不失败
        }
        let s = DurableSession::open(&dir).unwrap();
        assert_eq!(s.pending_uncertain(), vec!["risky-write".to_string()]);
        assert!(!s.can_retry("risky-write"), "未决时不能自动重试");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_survives_reopen() {
        let dir = tmpdir("cp");
        {
            let mut s = DurableSession::open(&dir).unwrap();
            s.save_checkpoint(10, "state-a", vec![]).unwrap();
        }
        let s = DurableSession::open(&dir).unwrap();
        let cp = s.latest_checkpoint().expect("检查点应持久化");
        assert_eq!(cp.through_seq, 10);
        assert_eq!(cp.payload, "state-a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_blocked_when_memory_invalid() {
        let dir = tmpdir("resume");
        let mut s = DurableSession::open(&dir).unwrap();
        s.save_checkpoint(
            5,
            "state",
            vec![MemoryRef {
                id: "m1".into(),
                revision: 1,
            }],
        )
        .unwrap();

        let mut idx = MemoryIndex::new();
        idx.set("m1", 1);
        let r = s.resume(&idx).unwrap();
        assert!(r.state_usable);

        idx.forget("m1");
        let r = s.resume(&idx).unwrap();
        assert!(!r.state_usable, "记忆失效必须拒绝交出 state");
        assert_eq!(r.state, None);
        assert_eq!(r.invalidated_memory_ids, vec!["m1".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalidate_cascades() {
        let dir = tmpdir("cascade");
        let mut s = DurableSession::open(&dir).unwrap();
        s.save_checkpoint(
            1,
            "a",
            vec![MemoryRef {
                id: "m1".into(),
                revision: 1,
            }],
        )
        .unwrap();
        let n = s.invalidate_by_memory("m1").unwrap();
        assert_eq!(n, 1);
        assert!(s.latest_checkpoint().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unexpired_lease_is_preserved_on_reopen() {
        // LoopForge `run_recovery.go:133`：只有过期才接管。
        // 未过期的说明原实例可能还活着，抢租约会导致两个实例同时写。
        let dir = tmpdir("lease-keep");
        let g1 = {
            let mut s = DurableSession::open(&dir).unwrap();
            s.acquire_lease("instance-a").unwrap()
        };
        let mut s = DurableSession::open(&dir).unwrap();
        // 原租约仍有效 → 别人拿不到
        assert!(
            s.acquire_lease("instance-b").is_err(),
            "未过期的租约不该被抢"
        );
        // 原持有者仍可写
        assert!(s.check_writer("instance-a", g1).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_lease_is_taken_over_with_new_generation() {
        let dir = tmpdir("lease-takeover");
        // 手工写一个已过期的租约
        {
            let mut s = DurableSession::open(&dir).unwrap();
            s.acquire_lease("dead-instance").unwrap();
            // 篡改文件，把 until 改成过去
            let p = dir.join("session.json");
            let text = std::fs::read_to_string(&p).unwrap();
            let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
            v["lease"]["until"] = serde_json::json!(1);
            std::fs::write(&p, serde_json::to_string(&v).unwrap()).unwrap();
        }
        // 重开 → 应接管，代次递增
        let mut s = DurableSession::open(&dir).unwrap();
        let g2 = s.acquire_lease("new-instance").unwrap();
        assert!(g2 > 1, "接管后代次应递增，实际 {g2}");
        // 死实例的旧代次写必须被拒
        assert!(s.check_writer("dead-instance", 1).is_err());
        assert!(s.check_writer("new-instance", g2).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_leaves_no_tmp() {
        let dir = tmpdir("atomic");
        let mut s = DurableSession::open(&dir).unwrap();
        s.begin_effect("e1").unwrap();
        let tmp = dir.join("session.json.tmp");
        assert!(!tmp.exists(), "临时文件应已被 rename");
        assert!(dir.join("session.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
