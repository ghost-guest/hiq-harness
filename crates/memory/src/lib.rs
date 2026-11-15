//! L4 记忆。
//!
//! 设计来自：
//! - ownmem：存项目内不存家目录；配额棘轮只降不升
//! - Codewhale：Access 不可反序列化；遗忘递归闭包 + 墓碑；内容不可变更
//! - Alma：睡眠整合按五类原因归档；`merged_into` 审计链
//! - 天枢：失效用封口不删除；召回只返回叶子
//! - agentmemory：三路 RRF 融合

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ─────────────────────────────────────────────────────────────
// 数据模型
// ─────────────────────────────────────────────────────────────

pub type MemoryId = String;

/// 记忆条目的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Decision,
    Constraint,
    Preference,
    Lesson,
    /// 用户显式要求记住的
    UserConstraint,
}

/// 条目状态。用封口代替删除（天枢）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MemoryState {
    Active,
    /// 被更新版本取代，但保留可追溯
    Superseded {
        by: MemoryId,
    },
    /// 已封口（过期/孤儿），召回不返回
    Sealed {
        at: i64,
    },
}

/// 一条记忆。
///
/// `content` 一旦写入不可修改 —— 要改就出新版本（Codewhale 的触发器语义）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: MemoryId,
    pub kind: MemoryKind,
    pub content: String,
    /// 适用范围。回答不了「这条能迁移到哪」的内容不配进库。
    pub transferable_to: Vec<String>,
    pub state: MemoryState,
    pub created_at: i64,
    pub updated_at: i64,
    /// 命中次数，用于衰减与排序
    pub hits: u32,
    /// 证据：这条记忆是基于什么得出的
    pub evidence: Vec<String>,
}

impl Memory {
    pub fn is_recallable(&self) -> bool {
        matches!(self.state, MemoryState::Active)
    }
}

/// 归档原因（Alma 的五类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveReason {
    /// 完全重复
    Exact,
    /// 过期
    Expired,
    /// 孤儿：来源已不存在
    Orphan,
    /// 相似需合并
    Similarity,
    /// 模型判断该归档
    Llm,
}

/// 归档条目。必须留 `merged_into` 以便追溯。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedMemory {
    pub id: MemoryId,
    pub content: String,
    pub reason: ArchiveReason,
    /// 谁归档的：human / scheduler / model
    pub archived_by: String,
    /// 被合并到哪条了。别家都没有这个字段。
    pub merged_into: Option<MemoryId>,
    pub archived_at: i64,
}

/// 一次睡眠整合的运行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SleepRun {
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub examined: usize,
    /// 按原因分类的归档计数
    pub archived: HashMap<ArchiveReason, usize>,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub error: Option<String>,
}

impl SleepRun {
    pub fn new(started_at: i64) -> Self {
        Self {
            started_at,
            ended_at: None,
            examined: 0,
            archived: HashMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            error: None,
        }
    }

    pub fn total_archived(&self) -> usize {
        self.archived.values().sum()
    }
}

// ─────────────────────────────────────────────────────────────
// 准入控制（Codewhale）
// ─────────────────────────────────────────────────────────────

/// 记忆操作能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cap {
    Read,
    Propose,
    Review,
    Correct,
    Forget,
    Checkpoint,
}

/// 访问凭证。
///
/// **刻意不实现 `Deserialize`** —— 理由：请求体不能创造或放大授权。
/// 只能由可信宿主代码构造。
#[derive(Debug, Clone)]
pub struct Access {
    caps: HashSet<Cap>,
    scopes: Vec<String>,
}

impl Access {
    /// 只有宿主能造。
    pub fn grant(caps: &[Cap], scopes: &[String]) -> Self {
        Self {
            caps: caps.iter().copied().collect(),
            scopes: scopes.to_vec(),
        }
    }

    /// 全部能力，用于单用户本地场景。
    pub fn full(scope: &str) -> Self {
        Self::grant(
            &[
                Cap::Read,
                Cap::Propose,
                Cap::Review,
                Cap::Correct,
                Cap::Forget,
                Cap::Checkpoint,
            ],
            &[scope.to_string()],
        )
    }

    pub fn can(&self, cap: Cap) -> bool {
        self.caps.contains(&cap)
    }

    /// scope 精确匹配，无前缀、无通配。
    pub fn allows_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// 写入前的敏感信息检测。
///
/// 命中即拒写，不做替换 —— 记忆里不该出现密钥。
pub fn contains_secret(text: &str) -> Option<&'static str> {
    const PATTERNS: &[(&str, &str)] = &[
        ("-----BEGIN", "PEM 私钥"),
        ("sk-", "API key"),
        ("ghp_", "GitHub token"),
        ("gho_", "GitHub OAuth"),
        ("AKIA", "AWS access key"),
        ("xoxb-", "Slack token"),
        ("xoxp-", "Slack token"),
        ("api_key=", "裸露的 api_key"),
        ("apikey=", "裸露的 apikey"),
        ("Bearer ", "bearer token"),
        ("password=", "裸露的密码"),
    ];
    for (pat, name) in PATTERNS {
        if text.contains(pat) {
            return Some(name);
        }
    }
    // JWT: 三段 base64 用点连接
    if looks_like_jwt(text) {
        return Some("JWT");
    }
    None
}

fn looks_like_jwt(text: &str) -> bool {
    for token in text.split_whitespace() {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() == 3
            && parts[0].starts_with("eyJ")
            && parts.iter().all(|p| {
                p.len() > 8
                    && p.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
        {
            return true;
        }
    }
    false
}

// ─────────────────────────────────────────────────────────────
// 配额棘轮（ownmem）
// ─────────────────────────────────────────────────────────────

/// 配额棘轮：只降不升。
///
/// 增长到阈值后锁死。之后要新增，必须先合并或归档。
/// 字节上限被刻意不设 —— 一个每次被绑住就手动抬高的上限不是配额。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaRatchet {
    pub mode: RatchetMode,
    /// 现在的活跃条目上限
    pub max_active: usize,
    /// 处于 growth 模式时的阈值，达到即切换
    pub growth_threshold: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatchetMode {
    /// 自由增长
    Growth,
    /// 锁死，只降不升
    Ratchet,
}

impl Default for QuotaRatchet {
    fn default() -> Self {
        Self {
            mode: RatchetMode::Growth,
            max_active: 0,
            growth_threshold: 50,
        }
    }
}

impl QuotaRatchet {
    /// 检查并可能切换到棘轮模式。返回当前活跃数是否仍在配额内。
    pub fn observe(&mut self, active_count: usize) -> Result<(), QuotaError> {
        if self.mode == RatchetMode::Growth {
            if active_count >= self.growth_threshold {
                self.mode = RatchetMode::Ratchet;
                self.max_active = active_count;
            }
            return Ok(());
        }
        if active_count > self.max_active {
            return Err(QuotaError::Exceeded {
                active: active_count,
                max: self.max_active,
            });
        }
        // 收紧：只降
        self.max_active = active_count;
        Ok(())
    }

    /// 显式要求提高配额 —— 必须报错。
    pub fn try_raise(&self, new_max: usize) -> Result<(), QuotaError> {
        if self.mode == RatchetMode::Ratchet && new_max > self.max_active {
            return Err(QuotaError::RatchetWouldRaise);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaError {
    /// 超配额，必须先合并或归档
    Exceeded { active: usize, max: usize },
    /// 棘轮只降不升
    RatchetWouldRaise,
}

// ─────────────────────────────────────────────────────────────
// 记忆库
// ─────────────────────────────────────────────────────────────

/// 记忆库。存项目内，跟代码走。
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: HashMap<MemoryId, Memory>,
    archive: Vec<ArchivedMemory>,
    /// 墓碑：阻断自动重导
    tombstones: HashSet<String>,
    quota: QuotaRatchet,
    next_id: u64,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_quota(threshold: usize) -> Self {
        Self {
            quota: QuotaRatchet {
                growth_threshold: threshold,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn active_count(&self) -> usize {
        self.entries.values().filter(|m| m.is_recallable()).count()
    }

    pub fn get(&self, id: &str) -> Option<&Memory> {
        self.entries.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &Memory> {
        self.entries.values()
    }

    pub fn archive(&self) -> &[ArchivedMemory] {
        &self.archive
    }

    pub fn quota(&self) -> &QuotaRatchet {
        &self.quota
    }

    /// 写入一条新记忆。
    pub fn remember(
        &mut self,
        access: &Access,
        kind: MemoryKind,
        content: &str,
        transferable_to: &[String],
        now: i64,
    ) -> Result<MemoryId, MemoryError> {
        if !access.can(Cap::Propose) {
            return Err(MemoryError::Forbidden("缺少 Propose 能力"));
        }

        // 准入：敏感信息直接拒写
        if let Some(what) = contains_secret(content) {
            return Err(MemoryError::SecretDetected(what));
        }

        // 结构性硬闸：必须说明可迁移到哪
        if transferable_to.is_empty() {
            return Err(MemoryError::NotTransferable);
        }

        // 墓碑：同内容已被遗忘过，不得重新引入
        let digest = content_digest(content);
        if self.tombstones.contains(&digest) {
            return Err(MemoryError::Tombstoned);
        }

        // 配额
        self.quota.observe(self.active_count() + 1)?;

        let id = format!("m_{:08x}", self.next_id);
        self.next_id += 1;
        let mem = Memory {
            id: id.clone(),
            kind,
            content: content.to_string(),
            transferable_to: transferable_to.to_vec(),
            state: MemoryState::Active,
            created_at: now,
            updated_at: now,
            hits: 0,
            evidence: vec![],
        };
        self.entries.insert(id.clone(), mem);
        Ok(id)
    }

    /// 用新内容取代旧条目。旧条目被封口但保留。
    pub fn supersede(
        &mut self,
        access: &Access,
        old_id: &str,
        new_content: &str,
        transferable_to: &[String],
        now: i64,
    ) -> Result<MemoryId, MemoryError> {
        if !access.can(Cap::Correct) {
            return Err(MemoryError::Forbidden("缺少 Correct 能力"));
        }
        let old = self
            .entries
            .get(old_id)
            .ok_or(MemoryError::NotFound)?
            .clone();

        let new_id = self.remember(access, old.kind, new_content, transferable_to, now)?;

        if let Some(m) = self.entries.get_mut(old_id) {
            m.state = MemoryState::Superseded { by: new_id.clone() };
            m.updated_at = now;
        }
        Ok(new_id)
    }

    /// 沿 superseded 链找到叶子。召回只返回叶子。
    pub fn resolve_leaf(&self, id: &str) -> Option<&Memory> {
        let mut cur = self.entries.get(id)?;
        let mut guard = 0;
        while let MemoryState::Superseded { by } = &cur.state {
            if guard > 100 {
                return None; // 循环保护
            }
            guard += 1;
            cur = self.entries.get(by)?;
        }
        Some(cur)
    }

    /// 遗忘：递归闭包 + 墓碑 + 连带清理由此派生的记忆。
    pub fn forget(
        &mut self,
        access: &Access,
        id: &str,
        reason: ArchiveReason,
        by: &str,
        now: i64,
    ) -> Result<usize, MemoryError> {
        if !access.can(Cap::Forget) {
            return Err(MemoryError::Forbidden("缺少 Forget 能力"));
        }

        // 递归闭包：沿 supersede 链双向展开
        let mut closure: HashSet<MemoryId> = HashSet::new();
        let mut stack = vec![id.to_string()];
        while let Some(cur) = stack.pop() {
            if !closure.insert(cur.clone()) {
                continue;
            }
            if let Some(m) = self.entries.get(&cur) {
                if let MemoryState::Superseded { by } = &m.state {
                    stack.push(by.clone());
                }
            }
            // 反向：谁被它取代了
            for (other_id, other) in &self.entries {
                if let MemoryState::Superseded { by } = &other.state {
                    if by == &cur {
                        stack.push(other_id.clone());
                    }
                }
            }
        }

        let mut count = 0;
        for target in closure {
            if let Some(m) = self.entries.remove(&target) {
                // 墓碑：同内容不得自动重导
                self.tombstones.insert(content_digest(&m.content));
                self.archive.push(ArchivedMemory {
                    id: m.id,
                    content: m.content,
                    reason,
                    archived_by: by.to_string(),
                    merged_into: None,
                    archived_at: now,
                });
                count += 1;
            }
        }
        Ok(count)
    }

    /// 合并两条：把 `from` 合进 `into`，留 `merged_into` 审计链。
    pub fn merge(
        &mut self,
        access: &Access,
        from: &str,
        into: &str,
        now: i64,
    ) -> Result<(), MemoryError> {
        if !access.can(Cap::Correct) {
            return Err(MemoryError::Forbidden("缺少 Correct 能力"));
        }
        let m = self.entries.remove(from).ok_or(MemoryError::NotFound)?;
        self.archive.push(ArchivedMemory {
            id: m.id.clone(),
            content: m.content.clone(),
            reason: ArchiveReason::Similarity,
            archived_by: "merge".into(),
            merged_into: Some(into.to_string()),
            archived_at: now,
        });
        // 被合并方的内容进墓碑，避免又被重新引入
        self.tombstones.insert(content_digest(&m.content));
        Ok(())
    }

    /// 从持久化数据恢复一条归档记录。
    ///
    /// 用于启动时重建：归档必须能完整往返，
    /// 否则「为什么被归档、被谁合并」的审计链就断了（Alma 的 merged_into 语义）。
    pub fn restore_archive(&mut self, record: ArchivedMemory) {
        self.tombstones.insert(content_digest(&record.content));
        self.archive.push(record);
    }

    /// 睡眠整合：按五类原因归档。
    ///
    /// `should_archive` 由调用方提供判断（可以用模型，也可以用规则）。
    /// 返回本次运行记录。
    pub fn sleep(
        &mut self,
        access: &Access,
        now: i64,
        mut should_archive: impl FnMut(&Memory) -> Option<ArchiveReason>,
    ) -> Result<SleepRun, MemoryError> {
        if !access.can(Cap::Forget) {
            return Err(MemoryError::Forbidden("缺少 Forget 能力"));
        }
        let mut run = SleepRun::new(now);

        let ids: Vec<MemoryId> = self.entries.keys().cloned().collect();
        run.examined = ids.len();

        for id in ids {
            let Some(m) = self.entries.get(&id) else {
                continue;
            };
            if !m.is_recallable() {
                continue;
            }
            if let Some(reason) = should_archive(m) {
                if let Some(m) = self.entries.remove(&id) {
                    self.tombstones.insert(content_digest(&m.content));
                    self.archive.push(ArchivedMemory {
                        id: m.id,
                        content: m.content,
                        reason,
                        archived_by: "scheduler".into(),
                        merged_into: None,
                        archived_at: now,
                    });
                    *run.archived.entry(reason).or_insert(0) += 1;
                }
            }
        }

        run.ended_at = Some(now);
        // 整合后配额收紧
        let _ = self.quota.observe(self.active_count());
        Ok(run)
    }

    /// 检索：三路 RRF 融合（关键词 / 词形 / 图）。
    ///
    /// 权重 BM25 0.4、向量 0.6、图 0.3，K=60。
    pub fn recall(&self, access: &Access, query: &str, limit: usize) -> Vec<RecallHit> {
        if !access.can(Cap::Read) {
            return Vec::new();
        }
        const RRF_K: f64 = 60.0;
        const W_KEYWORD: f64 = 0.4;
        const W_SEMANTIC: f64 = 0.6;
        const W_GRAPH: f64 = 0.3;

        let terms = tokenize(query);
        if terms.is_empty() {
            return Vec::new();
        }

        // 只对叶子和 active 条目计分
        let mut candidates: Vec<&Memory> = Vec::new();
        for m in self.entries.values() {
            if let Some(leaf) = self.resolve_leaf(&m.id) {
                if leaf.is_recallable() && leaf.id == m.id {
                    candidates.push(m);
                }
            }
        }

        if candidates.is_empty() {
            return Vec::new();
        }

        // 第一路：关键词命中（子串）
        let mut keyword_rank: Vec<(&Memory, f64)> = candidates
            .iter()
            .map(|m| {
                let hay = m.content.to_lowercase();
                let hits = terms.iter().filter(|t| hay.contains(t.as_str())).count() as f64;
                let score = hits / terms.len() as f64;
                (*m, score)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();
        keyword_rank.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 第二路：词频（同一批词出现次数，近似语义相似）
        let mut semantic_rank: Vec<(&Memory, f64)> = candidates
            .iter()
            .map(|m| {
                let hay = m.content.to_lowercase();
                let total: usize = terms.iter().map(|t| hay.matches(t.as_str()).count()).sum();
                (*m, total as f64)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();
        semantic_rank.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 第三路：图（共享 transferable_to 的邻域）
        let query_scopes: HashSet<&str> = terms
            .iter()
            .map(|s| s.as_str())
            .filter(|t| {
                candidates.iter().any(|m| {
                    m.transferable_to
                        .iter()
                        .any(|s| s.to_lowercase().contains(t))
                })
            })
            .collect();
        let mut graph_rank: Vec<(&Memory, f64)> = candidates
            .iter()
            .map(|m| {
                let shared = m
                    .transferable_to
                    .iter()
                    .filter(|s| query_scopes.iter().any(|q| s.to_lowercase().contains(*q)))
                    .count() as f64;
                (*m, shared)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();
        graph_rank.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // RRF 融合
        let mut scores: HashMap<&str, f64> = HashMap::new();
        for (rank, (m, _)) in keyword_rank.iter().enumerate() {
            *scores.entry(m.id.as_str()).or_insert(0.0) +=
                W_KEYWORD * (1.0 / (RRF_K + rank as f64 + 1.0));
        }
        for (rank, (m, _)) in semantic_rank.iter().enumerate() {
            *scores.entry(m.id.as_str()).or_insert(0.0) +=
                W_SEMANTIC * (1.0 / (RRF_K + rank as f64 + 1.0));
        }
        for (rank, (m, _)) in graph_rank.iter().enumerate() {
            *scores.entry(m.id.as_str()).or_insert(0.0) +=
                W_GRAPH * (1.0 / (RRF_K + rank as f64 + 1.0));
        }

        let mut out: Vec<RecallHit> = scores
            .into_iter()
            .filter_map(|(id, score)| {
                self.entries.get(id).map(|m| RecallHit {
                    id: m.id.clone(),
                    content: m.content.clone(),
                    score,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(limit);
        out
    }
}

#[derive(Debug, Clone)]
pub struct RecallHit {
    pub id: MemoryId,
    pub content: String,
    pub score: f64,
}

fn content_digest(s: &str) -> String {
    // 简单稳定的内容指纹，不引入额外依赖
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn tokenize(q: &str) -> Vec<String> {
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && !is_cjk_char(c))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn is_cjk_char(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    Forbidden(&'static str),
    SecretDetected(&'static str),
    NotTransferable,
    Tombstoned,
    NotFound,
    Quota(QuotaError),
}

impl From<QuotaError> for MemoryError {
    fn from(e: QuotaError) -> Self {
        MemoryError::Quota(e)
    }
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden(w) => write!(f, "权限不足：{w}"),
            Self::SecretDetected(w) => write!(f, "检测到敏感信息（{w}），拒绝写入"),
            Self::NotTransferable => write!(f, "必须说明这条记忆可迁移到哪"),
            Self::Tombstoned => write!(f, "该内容已被遗忘，不得自动重导"),
            Self::NotFound => write!(f, "条目不存在"),
            Self::Quota(e) => write!(f, "配额：{e:?}"),
        }
    }
}

impl std::error::Error for MemoryError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc() -> Access {
        Access::full("proj")
    }

    fn scopes() -> Vec<String> {
        vec!["proj".into()]
    }

    // ── 准入 ──

    #[test]
    fn rejects_secrets() {
        let mut s = MemoryStore::new();
        let a = acc();
        for (text, what) in [
            ("key=sk-abcdefghijklmnop", "API key"),
            ("-----BEGIN RSA PRIVATE KEY-----", "PEM 私钥"),
            ("token ghp_abcdefghijklmnopqrst", "GitHub token"),
            ("aws=AKIAIOSFODNN7EXAMPLE", "AWS access key"),
        ] {
            match s.remember(&a, MemoryKind::Fact, text, &scopes(), 0) {
                Err(MemoryError::SecretDetected(got)) => assert_eq!(got, what),
                other => panic!("应拒绝 {text}，实际 {other:?}"),
            }
        }
        assert_eq!(s.active_count(), 0, "拒绝的内容不能落库");
    }

    #[test]
    fn rejects_jwt() {
        let mut s = MemoryStore::new();
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop";
        assert!(matches!(
            s.remember(&acc(), MemoryKind::Fact, jwt, &scopes(), 0),
            Err(MemoryError::SecretDetected("JWT"))
        ));
    }

    #[test]
    fn requires_transferable_to() {
        let mut s = MemoryStore::new();
        assert_eq!(
            s.remember(&acc(), MemoryKind::Fact, "某个事实", &[], 0),
            Err(MemoryError::NotTransferable)
        );
    }

    #[test]
    fn access_requires_capability() {
        let mut s = MemoryStore::new();
        let read_only = Access::grant(&[Cap::Read], &["proj".into()]);
        assert!(matches!(
            s.remember(&read_only, MemoryKind::Fact, "x", &scopes(), 0),
            Err(MemoryError::Forbidden(_))
        ));
    }

    // ── 封口与叶子 ──

    #[test]
    fn supersede_seals_but_keeps_history() {
        let mut s = MemoryStore::new();
        let a = acc();
        let old = s
            .remember(&a, MemoryKind::Fact, "端口是 8080", &scopes(), 0)
            .unwrap();
        let new = s.supersede(&a, &old, "端口是 9090", &scopes(), 1).unwrap();

        // 旧条目还在，但被封口
        assert!(s.get(&old).is_some(), "封口不删除");
        assert_eq!(
            s.get(&old).unwrap().state,
            MemoryState::Superseded { by: new.clone() }
        );
        // 叶子解析到新的
        assert_eq!(s.resolve_leaf(&old).unwrap().id, new);
        // 只有新的可召回
        assert_eq!(s.active_count(), 1);
    }

    #[test]
    fn recall_returns_leaf_not_superseded() {
        let mut s = MemoryStore::new();
        let a = acc();
        let old = s
            .remember(&a, MemoryKind::Fact, "数据库是 mysql", &scopes(), 0)
            .unwrap();
        s.supersede(&a, &old, "数据库是 postgres", &scopes(), 1)
            .unwrap();

        let hits = s.recall(&a, "数据库", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("postgres"), "应召回最新版本");
    }

    // ── 遗忘与墓碑 ──

    #[test]
    fn forget_removes_closure_and_leaves_tombstone() {
        let mut s = MemoryStore::new();
        let a = acc();
        let v1 = s
            .remember(&a, MemoryKind::Fact, "配置在 a.yml", &scopes(), 0)
            .unwrap();
        let v2 = s.supersede(&a, &v1, "配置在 b.yml", &scopes(), 1).unwrap();

        let n = s
            .forget(&a, &v1, ArchiveReason::Expired, "human", 2)
            .unwrap();
        assert_eq!(n, 2, "递归闭包应删除整条链");
        assert_eq!(s.active_count(), 0);

        // 墓碑：同内容不得重新引入
        assert_eq!(
            s.remember(&a, MemoryKind::Fact, "配置在 a.yml", &scopes(), 3),
            Err(MemoryError::Tombstoned)
        );
        let _ = v2;
    }

    #[test]
    fn archive_records_reason_and_who() {
        let mut s = MemoryStore::new();
        let a = acc();
        let id = s
            .remember(&a, MemoryKind::Fact, "临时结论", &scopes(), 0)
            .unwrap();
        s.forget(&a, &id, ArchiveReason::Orphan, "scheduler", 1)
            .unwrap();

        let ar = &s.archive()[0];
        assert_eq!(ar.reason, ArchiveReason::Orphan);
        assert_eq!(ar.archived_by, "scheduler");
        assert_eq!(ar.merged_into, None);
    }

    // ── 合并审计链 ──

    #[test]
    fn merge_records_merged_into() {
        let mut s = MemoryStore::new();
        let a = acc();
        let keep = s
            .remember(&a, MemoryKind::Fact, "端口 8080", &scopes(), 0)
            .unwrap();
        let dup = s
            .remember(&a, MemoryKind::Fact, "端口 8080（重复）", &scopes(), 0)
            .unwrap();

        s.merge(&a, &dup, &keep, 1).unwrap();

        let ar = s
            .archive()
            .iter()
            .find(|x| x.id == dup)
            .expect("应有归档记录");
        assert_eq!(ar.merged_into, Some(keep.clone()), "必须留合并去向");
        assert_eq!(ar.reason, ArchiveReason::Similarity);
    }

    // ── 配额棘轮 ──

    #[test]
    fn quota_ratchets_and_rejects_raise() {
        let mut q = QuotaRatchet {
            growth_threshold: 3,
            ..Default::default()
        };
        // growth 模式自由增长
        assert!(q.observe(3).is_ok());
        assert_eq!(q.mode, RatchetMode::Ratchet, "到阈值应切棘轮");
        assert_eq!(q.max_active, 3);

        // 超配额报错
        assert_eq!(
            q.observe(4),
            Err(QuotaError::Exceeded { active: 4, max: 3 })
        );
        // 只降：收紧到 2
        assert!(q.observe(2).is_ok());
        assert_eq!(q.max_active, 2);
        // 试图提高 → 报错
        assert_eq!(q.try_raise(5), Err(QuotaError::RatchetWouldRaise));
    }

    #[test]
    fn quota_blocks_new_memory_when_full() {
        let mut s = MemoryStore::with_quota(2);
        let a = acc();
        s.remember(&a, MemoryKind::Fact, "a", &scopes(), 0).unwrap();
        s.remember(&a, MemoryKind::Fact, "b", &scopes(), 0).unwrap();
        // 第三条超配额
        assert!(matches!(
            s.remember(&a, MemoryKind::Fact, "c", &scopes(), 0),
            Err(MemoryError::Quota(_))
        ));
    }

    // ── 睡眠整合 ──

    #[test]
    fn sleep_archives_by_reason_and_counts() {
        let mut s = MemoryStore::new();
        let a = acc();
        for i in 0..5 {
            s.remember(&a, MemoryKind::Fact, &format!("旧事实 {i}"), &scopes(), 0)
                .unwrap();
        }
        for i in 0..3 {
            s.remember(&a, MemoryKind::Fact, &format!("新事实 {i}"), &scopes(), 10)
                .unwrap();
        }

        let run = s
            .sleep(&a, 20, |m| {
                if m.content.starts_with("旧") {
                    Some(ArchiveReason::Expired)
                } else {
                    None
                }
            })
            .unwrap();

        assert_eq!(run.examined, 8);
        assert_eq!(run.total_archived(), 5);
        assert_eq!(run.archived.get(&ArchiveReason::Expired), Some(&5));
        assert_eq!(s.active_count(), 3);
        assert!(run.ended_at.is_some());
    }

    #[test]
    fn sleep_counts_each_reason_separately() {
        let mut s = MemoryStore::new();
        let a = acc();
        s.remember(&a, MemoryKind::Fact, "dup-a", &scopes(), 0)
            .unwrap();
        s.remember(&a, MemoryKind::Fact, "dup-b", &scopes(), 0)
            .unwrap();
        s.remember(&a, MemoryKind::Fact, "orphan", &scopes(), 0)
            .unwrap();
        s.remember(&a, MemoryKind::Fact, "keep", &scopes(), 0)
            .unwrap();

        let run = s
            .sleep(&a, 5, |m| match m.content.as_str() {
                "dup-a" | "dup-b" => Some(ArchiveReason::Exact),
                "orphan" => Some(ArchiveReason::Orphan),
                _ => None,
            })
            .unwrap();

        assert_eq!(run.archived.get(&ArchiveReason::Exact), Some(&2));
        assert_eq!(run.archived.get(&ArchiveReason::Orphan), Some(&1));
        assert_eq!(run.total_archived(), 3);
    }

    // ── 检索 ──

    #[test]
    fn recall_finds_relevant() {
        let mut s = MemoryStore::new();
        let a = acc();
        s.remember(&a, MemoryKind::Fact, "项目用 postgres 数据库", &scopes(), 0)
            .unwrap();
        s.remember(&a, MemoryKind::Fact, "前端用 react 框架", &scopes(), 0)
            .unwrap();

        let hits = s.recall(&a, "数据库", 5);
        assert!(!hits.is_empty());
        assert!(hits[0].content.contains("postgres"));
    }

    #[test]
    fn recall_requires_read_cap() {
        let mut s = MemoryStore::new();
        let a = acc();
        s.remember(&a, MemoryKind::Fact, "x", &scopes(), 0).unwrap();
        let no_read = Access::grant(&[Cap::Propose], &["proj".into()]);
        assert!(s.recall(&no_read, "x", 5).is_empty());
    }

    #[test]
    fn recall_respects_limit() {
        let mut s = MemoryStore::new();
        let a = acc();
        for i in 0..20 {
            s.remember(
                &a,
                MemoryKind::Fact,
                &format!("数据库配置 {i}"),
                &scopes(),
                0,
            )
            .unwrap();
        }
        assert_eq!(s.recall(&a, "数据库", 3).len(), 3);
    }

    #[test]
    fn recall_empty_query_returns_nothing() {
        let mut s = MemoryStore::new();
        s.remember(&acc(), MemoryKind::Fact, "x", &scopes(), 0)
            .unwrap();
        assert!(s.recall(&acc(), "", 5).is_empty());
    }

    #[test]
    fn scope_is_exact_match() {
        let a = Access::grant(&[Cap::Read], &["proj-a".into()]);
        assert!(a.allows_scope("proj-a"));
        assert!(
            !a.allows_scope("proj-a/sub"),
            "scope 必须精确匹配，不支持前缀"
        );
        assert!(!a.allows_scope("proj"));
    }
}
