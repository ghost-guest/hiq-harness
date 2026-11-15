//! L5 持久化。
//!
//! 设计来自：
//! - kiso：副作用留收据；不确定的阻塞等人
//! - LoopForge：租约 + Generation 围栏；从检查点续而非标失败
//! - HugAgentOS：效果回执 + 强一致精确查询（重放不重复加强）
//! - Codewhale：**记忆失效则拒绝交出 state**；恢复状态不恢复权限
//!
//! 最关键的一条：记忆不成立，恢复出的状态就是错的。宁可不恢复。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type ExecutionId = String;
pub type CheckpointId = String;
pub type MemoryId = String;

// ─────────────────────────────────────────────────────────────
// 副作用收据（kiso）
// ─────────────────────────────────────────────────────────────

/// 一次工具执行的收据。
///
/// 三种状态里 `Uncertain` 是一等公民：
/// 开始了但没收到报告，绝不静默重试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Receipt {
    /// 已确认成功。绝不重跑。
    Confirmed { at: i64 },
    /// 已确认失败。可以重试。
    Failed { at: i64, error: String },
    /// 开始了但没报告。必须阻塞等人判定。
    Uncertain { started_at: i64 },
}

impl Receipt {
    pub fn is_settled(&self) -> bool {
        matches!(self, Self::Confirmed { .. } | Self::Failed { .. })
    }
}

/// 收据账本。按 executionId 索引。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReceiptLedger {
    receipts: HashMap<ExecutionId, Receipt>,
}

impl ReceiptLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&mut self, exec_id: impl Into<ExecutionId>, now: i64) {
        self.receipts
            .insert(exec_id.into(), Receipt::Uncertain { started_at: now });
    }

    pub fn confirm(&mut self, exec_id: &str, now: i64) -> Result<(), PersistError> {
        match self.receipts.get(exec_id) {
            Some(Receipt::Confirmed { .. }) => {
                // 已确认的不能再确认 —— 说明重复执行了
                Err(PersistError::AlreadyConfirmed(exec_id.to_string()))
            }
            Some(_) | None => {
                self.receipts
                    .insert(exec_id.to_string(), Receipt::Confirmed { at: now });
                Ok(())
            }
        }
    }

    pub fn fail(&mut self, exec_id: &str, error: &str, now: i64) -> Result<(), PersistError> {
        if let Some(Receipt::Confirmed { .. }) = self.receipts.get(exec_id) {
            return Err(PersistError::AlreadyConfirmed(exec_id.to_string()));
        }
        self.receipts.insert(
            exec_id.to_string(),
            Receipt::Failed {
                at: now,
                error: error.to_string(),
            },
        );
        Ok(())
    }

    /// 人裁决后把 uncertain 定性。
    pub fn resolve_uncertain(
        &mut self,
        exec_id: &str,
        applied: bool,
        now: i64,
    ) -> Result<(), PersistError> {
        match self.receipts.get(exec_id) {
            Some(Receipt::Uncertain { .. }) => {
                self.receipts.insert(
                    exec_id.to_string(),
                    if applied {
                        Receipt::Confirmed { at: now }
                    } else {
                        Receipt::Failed {
                            at: now,
                            error: "人工裁决：副作用未生效".into(),
                        }
                    },
                );
                Ok(())
            }
            Some(_) => Err(PersistError::AlreadySettled(exec_id.to_string())),
            None => Err(PersistError::NotFound(exec_id.to_string())),
        }
    }

    pub fn get(&self, exec_id: &str) -> Option<&Receipt> {
        self.receipts.get(exec_id)
    }

    /// 全部收据，用于持久化。
    pub fn all(&self) -> impl Iterator<Item = (&ExecutionId, &Receipt)> {
        self.receipts.iter()
    }

    /// 所有未决的 uncertain。恢复时必须先处理这些。
    pub fn pending_uncertain(&self) -> Vec<ExecutionId> {
        self.receipts
            .iter()
            .filter(|(_, r)| matches!(r, Receipt::Uncertain { .. }))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 能否安全重试一个执行。已确认的不能，未决的不能。
    pub fn can_retry(&self, exec_id: &str) -> bool {
        match self.receipts.get(exec_id) {
            None => true,
            Some(Receipt::Failed { .. }) => true,
            Some(Receipt::Confirmed { .. }) => false,
            Some(Receipt::Uncertain { .. }) => false,
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 检查点 + 记忆依赖（Codewhale）
// ─────────────────────────────────────────────────────────────

/// 检查点引用的记忆版本。
///
/// 恢复时用它验证「这条记忆还是我当初依赖的那个版本吗」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRef {
    pub id: MemoryId,
    pub revision: u64,
}

/// 一个检查点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    /// 覆盖到日志的哪一条
    pub through_seq: u64,
    pub created_at: i64,
    /// 状态负载
    pub payload: String,
    /// 依赖的记忆
    pub memory_refs: Vec<MemoryRef>,
}

/// 当前记忆库的 revision 视图。
#[derive(Debug, Clone, Default)]
pub struct MemoryIndex {
    revisions: HashMap<MemoryId, u64>,
}

impl MemoryIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, id: impl Into<MemoryId>, revision: u64) {
        self.revisions.insert(id.into(), revision);
    }

    pub fn bump(&mut self, id: &str) {
        let e = self.revisions.entry(id.to_string()).or_insert(0);
        *e += 1;
    }

    /// 记忆被遗忘时移出索引。
    pub fn forget(&mut self, id: &str) {
        self.revisions.remove(id);
    }

    pub fn revision_of(&self, id: &str) -> Option<u64> {
        self.revisions.get(id).copied()
    }
}

/// 恢复结果。
///
/// **`state_usable` 为 false 时 `state` 必须是 None。**
/// 理由：记忆不成立，恢复出的状态就是错的。宁可不恢复。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeResult {
    pub checkpoint_id: CheckpointId,
    pub state_usable: bool,
    pub state: Option<String>,
    /// 哪些依赖的记忆失效了
    pub invalidated_memory_ids: Vec<MemoryId>,
    /// 恒为 false：恢复状态不恢复权限
    pub restores_permissions: bool,
    /// 需要对账未决操作
    pub reconcile_pending_operations: bool,
}

impl ResumeResult {
    pub fn blocked(checkpoint_id: &str, invalidated: Vec<MemoryId>) -> Self {
        Self {
            checkpoint_id: checkpoint_id.to_string(),
            state_usable: false,
            state: None,
            invalidated_memory_ids: invalidated,
            restores_permissions: false,
            reconcile_pending_operations: true,
        }
    }

    pub fn ok(checkpoint_id: &str, state: &str) -> Self {
        Self {
            checkpoint_id: checkpoint_id.to_string(),
            state_usable: true,
            state: Some(state.to_string()),
            invalidated_memory_ids: Vec::new(),
            restores_permissions: false,
            reconcile_pending_operations: true,
        }
    }
}

/// 从检查点恢复。
///
/// 逐个核对依赖的记忆版本。任一对不上 → 拒绝交出 state。
pub fn resume(checkpoint: &Checkpoint, index: &MemoryIndex) -> ResumeResult {
    let mut invalidated = Vec::new();
    for r in &checkpoint.memory_refs {
        match index.revision_of(&r.id) {
            Some(cur) if cur == r.revision => {}
            // 不存在（被遗忘）或版本不同（被修改）
            _ => invalidated.push(r.id.clone()),
        }
    }
    if invalidated.is_empty() {
        ResumeResult::ok(&checkpoint.id, &checkpoint.payload)
    } else {
        ResumeResult::blocked(&checkpoint.id, invalidated)
    }
}

// ─────────────────────────────────────────────────────────────
// 租约 + 代次围栏（LoopForge）
// ─────────────────────────────────────────────────────────────

/// 租约。`generation` 是围栏：防旧实例复活后写乱。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub owner: String,
    pub until: i64,
    pub generation: u64,
}

impl Lease {
    pub fn is_valid_at(&self, now: i64) -> bool {
        now < self.until
    }
}

#[derive(Debug, Default)]
pub struct LeaseManager {
    lease: Option<Lease>,
    next_generation: u64,
}

impl LeaseManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 尝试获取租约。若已被他人持有且未过期则失败。
    pub fn acquire(&mut self, owner: &str, now: i64, ttl: i64) -> Result<Lease, PersistError> {
        if let Some(cur) = &self.lease {
            if cur.is_valid_at(now) && cur.owner != owner {
                return Err(PersistError::LeaseHeldBy(cur.owner.clone()));
            }
        }
        self.next_generation += 1;
        let lease = Lease {
            owner: owner.to_string(),
            until: now + ttl,
            generation: self.next_generation,
        };
        self.lease = Some(lease.clone());
        Ok(lease)
    }

    /// 续租。必须持有当前代次。
    pub fn renew(
        &mut self,
        owner: &str,
        generation: u64,
        now: i64,
        ttl: i64,
    ) -> Result<Lease, PersistError> {
        let cur = self.lease.as_ref().ok_or(PersistError::NoLease)?;
        if cur.generation != generation {
            return Err(PersistError::StaleGeneration {
                expected: cur.generation,
                got: generation,
            });
        }
        if cur.owner != owner {
            return Err(PersistError::LeaseHeldBy(cur.owner.clone()));
        }
        if !cur.is_valid_at(now) {
            return Err(PersistError::LeaseExpired);
        }
        let lease = Lease {
            owner: owner.to_string(),
            until: now + ttl,
            generation,
        };
        self.lease = Some(lease.clone());
        Ok(lease)
    }

    /// 校验写权限。旧代次的写必须被拒绝。
    pub fn check_writer(&self, owner: &str, generation: u64) -> Result<(), PersistError> {
        let cur = self.lease.as_ref().ok_or(PersistError::NoLease)?;
        if cur.generation != generation {
            return Err(PersistError::StaleGeneration {
                expected: cur.generation,
                got: generation,
            });
        }
        if cur.owner != owner {
            return Err(PersistError::LeaseHeldBy(cur.owner.clone()));
        }
        Ok(())
    }

    pub fn current(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    /// 从磁盘恢复一个未过期的租约。
    ///
    /// 不递增代次 —— 原实例可能还活着，递增会让它立即失效。
    /// 只有确认过期才该走 `acquire` 接管（LoopForge 的做法）。
    pub fn restore(&mut self, lease: Lease) {
        self.next_generation = self.next_generation.max(lease.generation);
        self.lease = Some(lease);
    }

    pub fn release(&mut self) {
        self.lease = None;
    }
}

// ─────────────────────────────────────────────────────────────
// 检查点仓库
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct CheckpointStore {
    checkpoints: HashMap<CheckpointId, Checkpoint>,
    next_id: u64,
}

impl CheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn save(
        &mut self,
        through_seq: u64,
        payload: &str,
        memory_refs: Vec<MemoryRef>,
        now: i64,
    ) -> CheckpointId {
        let id = format!("cp_{:06}", self.next_id);
        self.next_id += 1;
        self.checkpoints.insert(
            id.clone(),
            Checkpoint {
                id: id.clone(),
                through_seq,
                created_at: now,
                payload: payload.to_string(),
                memory_refs,
            },
        );
        id
    }

    pub fn get(&self, id: &str) -> Option<&Checkpoint> {
        self.checkpoints.get(id)
    }

    pub fn latest(&self) -> Option<&Checkpoint> {
        self.checkpoints.values().max_by_key(|c| c.through_seq)
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    /// 全部检查点，用于持久化。
    pub fn all(&self) -> impl Iterator<Item = &Checkpoint> {
        self.checkpoints.values()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    /// 记忆失效时连带撤销引用它的检查点（Codewhale 的连带语义）。
    ///
    /// 返回被撤销的检查点数量。
    pub fn invalidate_by_memory(&mut self, memory_id: &str) -> usize {
        let doomed: Vec<CheckpointId> = self
            .checkpoints
            .iter()
            .filter(|(_, c)| c.memory_refs.iter().any(|r| r.id == memory_id))
            .map(|(id, _)| id.clone())
            .collect();
        let n = doomed.len();
        for id in doomed {
            self.checkpoints.remove(&id);
        }
        n
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistError {
    AlreadyConfirmed(ExecutionId),
    AlreadySettled(ExecutionId),
    NotFound(ExecutionId),
    LeaseHeldBy(String),
    LeaseExpired,
    NoLease,
    StaleGeneration { expected: u64, got: u64 },
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyConfirmed(id) => write!(f, "{id} 已确认成功，不可重复执行"),
            Self::AlreadySettled(id) => write!(f, "{id} 已有裁决，不可再裁决"),
            Self::NotFound(id) => write!(f, "{id} 不存在"),
            Self::LeaseHeldBy(o) => write!(f, "租约被 {o} 持有"),
            Self::LeaseExpired => write!(f, "租约已过期"),
            Self::NoLease => write!(f, "当前无租约"),
            Self::StaleGeneration { expected, got } => {
                write!(f, "代次不匹配（期望 {expected}，实际 {got}）")
            }
        }
    }
}

impl std::error::Error for PersistError {}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 收据 ──

    #[test]
    fn confirmed_is_never_rerun() {
        let mut l = ReceiptLedger::new();
        l.begin("e1", 0);
        assert!(!l.can_retry("e1"), "未决时不能重试");
        l.confirm("e1", 1).unwrap();
        assert!(!l.can_retry("e1"), "已确认的绝不重跑");
        assert_eq!(
            l.confirm("e1", 2),
            Err(PersistError::AlreadyConfirmed("e1".into()))
        );
    }

    #[test]
    fn failed_can_be_retried() {
        let mut l = ReceiptLedger::new();
        l.begin("e1", 0);
        l.fail("e1", "boom", 1).unwrap();
        assert!(l.can_retry("e1"), "确认失败可以重试");
    }

    #[test]
    fn uncertain_blocks_until_human_decides() {
        let mut l = ReceiptLedger::new();
        l.begin("e1", 0);
        l.begin("e2", 0);
        l.confirm("e2", 1).unwrap();

        let pending = l.pending_uncertain();
        assert_eq!(pending, vec!["e1".to_string()], "未决的必须被列出来");
        assert!(!l.can_retry("e1"), "未决时不能自动重试");

        l.resolve_uncertain("e1", true, 2).unwrap();
        assert!(l.pending_uncertain().is_empty());
        assert!(matches!(l.get("e1"), Some(Receipt::Confirmed { .. })));
    }

    #[test]
    fn cannot_settle_twice() {
        let mut l = ReceiptLedger::new();
        l.begin("e1", 0);
        l.resolve_uncertain("e1", false, 1).unwrap();
        assert_eq!(
            l.resolve_uncertain("e1", true, 2),
            Err(PersistError::AlreadySettled("e1".into()))
        );
    }

    // ── 检查点与记忆依赖 ──

    fn cp_with_refs() -> Checkpoint {
        Checkpoint {
            id: "cp1".into(),
            through_seq: 10,
            created_at: 0,
            payload: "state-data".into(),
            memory_refs: vec![
                MemoryRef {
                    id: "m1".into(),
                    revision: 2,
                },
                MemoryRef {
                    id: "m2".into(),
                    revision: 1,
                },
            ],
        }
    }

    #[test]
    fn resume_ok_when_memory_matches() {
        let mut idx = MemoryIndex::new();
        idx.set("m1", 2);
        idx.set("m2", 1);
        let r = resume(&cp_with_refs(), &idx);
        assert!(r.state_usable);
        assert_eq!(r.state.as_deref(), Some("state-data"));
        assert!(r.invalidated_memory_ids.is_empty());
    }

    #[test]
    fn resume_blocked_when_memory_forgotten() {
        let mut idx = MemoryIndex::new();
        idx.set("m1", 2);
        // m2 被遗忘，不在索引里
        let r = resume(&cp_with_refs(), &idx);
        assert!(!r.state_usable, "记忆失效必须拒绝交出状态");
        assert_eq!(r.state, None, "拒绝时必须返回 null");
        assert_eq!(r.invalidated_memory_ids, vec!["m2".to_string()]);
    }

    #[test]
    fn resume_blocked_when_memory_revised() {
        let mut idx = MemoryIndex::new();
        idx.set("m1", 3); // 版本变了
        idx.set("m2", 1);
        let r = resume(&cp_with_refs(), &idx);
        assert!(!r.state_usable);
        assert_eq!(r.invalidated_memory_ids, vec!["m1".to_string()]);
    }

    #[test]
    fn resume_never_restores_permissions() {
        let mut idx = MemoryIndex::new();
        idx.set("m1", 2);
        idx.set("m2", 1);
        let r = resume(&cp_with_refs(), &idx);
        assert!(!r.restores_permissions, "恢复状态不恢复权限");
        assert!(r.reconcile_pending_operations, "需要对账未决操作");
    }

    #[test]
    fn forget_cascades_to_checkpoints() {
        let mut store = CheckpointStore::new();
        store.save(
            10,
            "s1",
            vec![MemoryRef {
                id: "m1".into(),
                revision: 1,
            }],
            0,
        );
        store.save(
            20,
            "s2",
            vec![MemoryRef {
                id: "m9".into(),
                revision: 1,
            }],
            0,
        );
        assert_eq!(store.len(), 2);
        let n = store.invalidate_by_memory("m1");
        assert_eq!(n, 1, "应连带撤销引用它的检查点");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn latest_checkpoint_by_seq() {
        let mut store = CheckpointStore::new();
        store.save(5, "a", vec![], 0);
        store.save(50, "b", vec![], 0);
        store.save(20, "c", vec![], 0);
        assert_eq!(store.latest().unwrap().payload, "b");
    }

    // ── 租约 ──

    #[test]
    fn lease_excludes_other_owner() {
        let mut m = LeaseManager::new();
        let l = m.acquire("a", 0, 100).unwrap();
        assert_eq!(l.generation, 1);
        assert_eq!(
            m.acquire("b", 10, 100),
            Err(PersistError::LeaseHeldBy("a".into()))
        );
    }

    #[test]
    fn lease_expires_allowing_takeover() {
        let mut m = LeaseManager::new();
        m.acquire("a", 0, 10).unwrap();
        // 过期后可被接管，代次递增
        let l2 = m.acquire("b", 20, 10).unwrap();
        assert_eq!(l2.generation, 2);
    }

    #[test]
    fn stale_generation_write_is_rejected() {
        let mut m = LeaseManager::new();
        let old = m.acquire("a", 0, 5).unwrap();
        // a 过期，b 接管
        let new = m.acquire("b", 10, 100).unwrap();
        assert_eq!(new.generation, 2);

        // 旧实例复活后试图写 → 必须拒绝
        assert_eq!(
            m.check_writer("a", old.generation),
            Err(PersistError::StaleGeneration {
                expected: 2,
                got: 1
            })
        );
        assert!(m.check_writer("b", new.generation).is_ok());
    }

    #[test]
    fn renew_requires_current_generation() {
        let mut m = LeaseManager::new();
        let l = m.acquire("a", 0, 100).unwrap();
        assert!(m.renew("a", l.generation, 10, 100).is_ok());
        assert_eq!(
            m.renew("a", 999, 20, 100),
            Err(PersistError::StaleGeneration {
                expected: 1,
                got: 999
            })
        );
    }

    #[test]
    fn renew_fails_after_expiry() {
        let mut m = LeaseManager::new();
        let l = m.acquire("a", 0, 10).unwrap();
        assert_eq!(
            m.renew("a", l.generation, 20, 10),
            Err(PersistError::LeaseExpired)
        );
    }
}
