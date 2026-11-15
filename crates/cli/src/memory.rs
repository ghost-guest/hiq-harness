//! 记忆的持久化与注入。
//!
//! 全部来自调研：
//! - **存项目内不存家目录**（ownmem、天枢）：`.harness/memory.json`，
//!   跟着仓库走，换机不丢。
//! - **准入控制**（Codewhale `auth.rs`）：密钥正则拦截，命中即拒写。
//! - **封口不删除**（天枢）：取代用 `superseded`，召回只返回叶子。
//! - **配额棘轮只降不升**（ownmem `memory-audit.mjs`）。
//! - **睡眠整合五类归档**（Alma `memory_sleep_runs`）。
//! - **RRF 三路检索**（agentmemory `hybrid-search.ts`）。
//!
//! 记忆失效时连带撤销检查点（Codewhale `protocol.rs:278`）。

use harness_memory::{Access, ArchiveReason, MemoryKind, MemoryState, MemoryStore};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 磁盘上的记忆文件。
#[derive(Debug, Serialize, Deserialize, Default)]
struct MemoryFile {
    version: u32,
    entries: Vec<EntryRecord>,
    archive: Vec<ArchiveRecord>,
    tombstones: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EntryRecord {
    id: String,
    kind: String,
    content: String,
    transferable_to: Vec<String>,
    state: String,
    created_at: i64,
    updated_at: i64,
    hits: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct ArchiveRecord {
    id: String,
    content: String,
    reason: String,
    archived_by: String,
    merged_into: Option<String>,
    archived_at: i64,
}

/// 项目内记忆库。
pub struct ProjectMemory {
    path: PathBuf,
    store: MemoryStore,
}

impl ProjectMemory {
    /// 打开或创建。默认位置 `.harness/memory.json`（项目内）。
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("memory.json");
        let mut store = MemoryStore::new();

        if path.exists() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(file) = serde_json::from_str::<MemoryFile>(&text) {
                    store = Self::rebuild(file);
                }
            }
        }

        Ok(Self { path, store })
    }

    fn rebuild(file: MemoryFile) -> MemoryStore {
        let mut store = MemoryStore::new();
        let access = Access::full("project");
        // 重建条目（保持原 id 与状态需要底层支持，这里用公开 API 近似）
        for e in file.entries {
            let kind = match e.kind.as_str() {
                "fact" => MemoryKind::Fact,
                "decision" => MemoryKind::Decision,
                "constraint" => MemoryKind::Constraint,
                "preference" => MemoryKind::Preference,
                "lesson" => MemoryKind::Lesson,
                _ => MemoryKind::UserConstraint,
            };
            if let Ok(_id) =
                store.remember(&access, kind, &e.content, &e.transferable_to, e.created_at)
            {
                // 状态恢复：被取代/封口的条目重新封口
                if e.state != "active" {
                    let _ = store.forget(
                        &access,
                        &e.content,
                        ArchiveReason::Expired,
                        "restore",
                        e.updated_at,
                    );
                }
            }
        }

        // 恢复归档：审计链必须完整往返（Alma 的 merged_into 语义）
        for a in file.archive {
            let reason = match a.reason.as_str() {
                "exact" => ArchiveReason::Exact,
                "expired" => ArchiveReason::Expired,
                "orphan" => ArchiveReason::Orphan,
                "similarity" => ArchiveReason::Similarity,
                _ => ArchiveReason::Llm,
            };
            store.restore_archive(harness_memory::ArchivedMemory {
                id: a.id,
                content: a.content,
                reason,
                archived_by: a.archived_by,
                merged_into: a.merged_into,
                archived_at: a.archived_at,
            });
        }
        store
    }

    fn flush(&self) -> anyhow::Result<()> {
        let entries: Vec<EntryRecord> = self
            .store
            .all()
            .map(|m| EntryRecord {
                id: m.id.clone(),
                kind: format!("{:?}", m.kind).to_lowercase(),
                content: m.content.clone(),
                transferable_to: m.transferable_to.clone(),
                state: match &m.state {
                    MemoryState::Active => "active".into(),
                    MemoryState::Superseded { .. } => "superseded".into(),
                    MemoryState::Sealed { .. } => "sealed".into(),
                },
                created_at: m.created_at,
                updated_at: m.updated_at,
                hits: m.hits,
            })
            .collect();

        let archive: Vec<ArchiveRecord> = self
            .store
            .archive()
            .iter()
            .map(|a| ArchiveRecord {
                id: a.id.clone(),
                content: a.content.clone(),
                reason: format!("{:?}", a.reason).to_lowercase(),
                archived_by: a.archived_by.clone(),
                merged_into: a.merged_into.clone(),
                archived_at: a.archived_at,
            })
            .collect();

        let file = MemoryFile {
            version: 1,
            entries,
            archive,
            tombstones: Vec::new(),
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&file)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 召回相关记忆，返回注入用的文本块。
    ///
    /// 用 RRF 三路检索（agentmemory），取前 N 条。
    pub fn recall_block(&self, query: &str, limit: usize) -> Option<String> {
        let access = Access::full("project");
        let hits = self.store.recall(&access, query, limit);
        if hits.is_empty() {
            return None;
        }
        let mut out = String::from("[项目记忆]\n");
        for h in hits {
            out.push_str(&format!("- {}\n", h.content));
        }
        Some(out)
    }

    /// 从任务结果里提取值得记住的内容并写入。
    ///
    /// 准入在 `MemoryStore::remember` 内部做（密钥拦截 + 必须可迁移）。
    pub fn remember(&mut self, kind: MemoryKind, content: &str, scopes: &[String]) -> bool {
        let access = Access::full("project");
        let now = now_ms();
        if self
            .store
            .remember(&access, kind, content, scopes, now)
            .is_ok()
        {
            let _ = self.flush();
            true
        } else {
            false
        }
    }

    /// 遗忘一条，返回被遗忘的数量。
    pub fn forget(&mut self, content: &str, reason: ArchiveReason) -> usize {
        let access = Access::full("project");
        // 按内容找到 id
        let id = self
            .store
            .all()
            .find(|m| m.content == content)
            .map(|m| m.id.clone());
        let Some(id) = id else { return 0 };
        let n = self
            .store
            .forget(&access, &id, reason, "cli", now_ms())
            .unwrap_or(0);
        let _ = self.flush();
        n
    }

    pub fn active_count(&self) -> usize {
        self.store.active_count()
    }

    pub fn stats(&self) -> (usize, usize) {
        (self.store.active_count(), self.store.archive().len())
    }
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

    fn tmpdir(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("mem-{}-{}-{}", name, std::process::id(), now_ms()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn remember_and_recall() {
        let dir = tmpdir("recall");
        let mut m = ProjectMemory::open(&dir).unwrap();
        assert!(m.remember(
            MemoryKind::Fact,
            "项目用 postgres 数据库",
            &["project".into()]
        ));
        let block = m.recall_block("postgres", 5);
        assert!(block.is_some());
        assert!(block.unwrap().contains("postgres"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_is_rejected() {
        let dir = tmpdir("secret");
        let mut m = ProjectMemory::open(&dir).unwrap();
        // 准入拦截：密钥不能落库
        assert!(!m.remember(MemoryKind::Fact, "key=sk-abcdefghijklmnop", &["p".into()]));
        assert_eq!(m.active_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_persists_across_reopen() {
        let dir = tmpdir("persist");
        {
            let mut m = ProjectMemory::open(&dir).unwrap();
            m.remember(MemoryKind::Decision, "选了 rust 做核心", &["p".into()]);
        }
        let m = ProjectMemory::open(&dir).unwrap();
        assert_eq!(m.active_count(), 1, "记忆应跨进程持久");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_recall_returns_none() {
        let dir = tmpdir("empty");
        let m = ProjectMemory::open(&dir).unwrap();
        assert!(m.recall_block("完全不相关的查询", 5).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_survives_reopen() {
        // 回归：归档曾经没被 rebuild 恢复，stats 读出来是 0，
        // 「为什么被归档、被谁合并」的审计链就断了。
        let dir = tmpdir("archive-persist");
        {
            let mut m = ProjectMemory::open(&dir).unwrap();
            m.remember(MemoryKind::Fact, "待遗忘的事实", &["p".into()]);
            assert_eq!(m.forget("待遗忘的事实", ArchiveReason::Orphan), 1);
        }
        let m = ProjectMemory::open(&dir).unwrap();
        let (active, archived) = m.stats();
        assert_eq!(active, 0);
        assert_eq!(archived, 1, "归档必须能跨进程往返");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_moves_to_archive() {
        let dir = tmpdir("forget");
        let mut m = ProjectMemory::open(&dir).unwrap();
        m.remember(MemoryKind::Fact, "临时结论", &["p".into()]);
        assert_eq!(m.active_count(), 1);
        let n = m.forget("临时结论", ArchiveReason::Expired);
        assert_eq!(n, 1);
        assert_eq!(m.active_count(), 0);
        let (active, archived) = m.stats();
        assert_eq!(active, 0);
        assert!(archived >= 1, "应进归档而不是消失");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
