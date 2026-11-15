//! L7 扩展宿主。
//!
//! 设计来自 kiso 与 Codewhale：
//! - 一个插件同时承载 tools / hooks / approvals / compaction / systemPrompt
//! - 加载必须大声：坏文件启动即失败，不能静默改变行为
//! - **用户扩展可覆盖内建，项目扩展不可**
//! - 策略链单调：deny > allow > ask，append 只追加不替换
//! - 抛异常算 ask，无审批通道时退化为诚实拒绝
//!
//! Rust 侧是本模块，Go 侧是独立进程（JSON-RPC over stdio）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ─────────────────────────────────────────────────────────────
// 插件声明
// ─────────────────────────────────────────────────────────────

/// 插件来源。决定覆盖权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    /// 内建，随二进制发布
    Builtin,
    /// 用户级（家目录），可覆盖内建
    User,
    /// 项目级（仓库内），**不可覆盖内建**
    Project,
}

impl PluginScope {
    /// 权限等级：数值越小越优先。
    pub fn precedence(&self) -> u8 {
        match self {
            Self::Project => 0,
            Self::User => 1,
            Self::Builtin => 2,
        }
    }

    pub fn can_override_builtin(&self) -> bool {
        // 只有用户级可以覆盖内建。
        // 项目级不行：你 clone 下来的仓库不该悄悄替换 CLI 自身的行为。
        !matches!(self, Self::Project)
    }
}

/// 插件提供的能力。显式枚举，便于审计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Tools,
    Hooks,
    Approvals,
    Compaction,
    SystemPrompt,
    /// 原生代码执行 —— 默认不启用
    Native,
}

/// 一个工具声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// 只读工具可与其它只读并行
    pub read_only: bool,
}

/// 插件清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    /// 入口命令（Go 编译出的可执行文件）
    pub entry: String,
    pub capabilities: Vec<Capability>,
    /// 插件自己声明的工具
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// 追加到系统提示的文本。只追加不替换。
    #[serde(default)]
    pub system_prompt_append: Option<String>,
}

impl PluginManifest {
    pub fn has(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }
}

/// 已加载的插件。
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub scope: PluginScope,
    /// 源文件路径，用于报错定位
    pub source: PathBuf,
}

// ─────────────────────────────────────────────────────────────
// 审批策略链
// ─────────────────────────────────────────────────────────────

/// 单个策略的判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny { reason: String },
    Ask,
}

/// 一次工具调用的上下文，交给策略判定。
#[derive(Debug, Clone)]
pub struct CallContext {
    pub tool: String,
    pub args: serde_json::Value,
}

/// 策略链。
///
/// 组合规则单调：**deny > allow > ask**。
/// 任何 deny 胜出（第一个 deny 的理由传给模型）；
/// 否则任何 allow 胜出（后面的 allow 能压过前面的 ask）；
/// 都没人 allow 但有人 ask → 交给人。
///
/// 抛异常的策略算 ask（不是默认允许）。
#[derive(Default)]
pub struct ApprovalChain {
    policies: Vec<(String, PolicyFn)>,
}

type PolicyFn = Box<dyn Fn(&CallContext) -> Verdict + Send + Sync>;

impl ApprovalChain {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(
        &mut self,
        plugin: impl Into<String>,
        f: impl Fn(&CallContext) -> Verdict + Send + Sync + 'static,
    ) {
        self.policies.push((plugin.into(), Box::new(f)));
    }

    pub fn len(&self) -> usize {
        self.policies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// 跑完整条链。
    ///
    /// `has_approval_channel` 为 false 时，`Ask` 退化为诚实拒绝 ——
    /// 没有人能回答的问题不该悬着。
    pub fn decide(&self, call: &CallContext, has_approval_channel: bool) -> ChainVerdict {
        let mut first_deny: Option<(String, String)> = None;
        let mut any_allow = false;
        let mut any_ask = false;

        for (name, policy) in &self.policies {
            // 抛异常算 ask
            let v = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| policy(call)))
                .unwrap_or(Verdict::Ask);

            match v {
                Verdict::Deny { reason } => {
                    // 第一个 deny 的理由保留（后面的不覆盖）
                    if first_deny.is_none() {
                        first_deny = Some((name.clone(), reason));
                    }
                }
                Verdict::Allow => any_allow = true,
                Verdict::Ask => any_ask = true,
            }
        }

        // deny 优先
        if let Some((plugin, reason)) = first_deny {
            return ChainVerdict::Deny {
                decided_by: plugin,
                reason,
            };
        }
        // 其次 allow（能压过 ask）
        if any_allow {
            return ChainVerdict::Allow;
        }
        // 再次 ask
        if any_ask {
            if has_approval_channel {
                return ChainVerdict::AskHuman;
            }
            // 没人能回答 → 诚实拒绝
            return ChainVerdict::Deny {
                decided_by: "chain".into(),
                reason: "需要人工审批，但当前没有可用的审批通道".into(),
            };
        }
        ChainVerdict::Allow
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainVerdict {
    Allow,
    Deny { decided_by: String, reason: String },
    AskHuman,
}

// ─────────────────────────────────────────────────────────────
// 系统提示组装：只追加不替换
// ─────────────────────────────────────────────────────────────

/// 组装系统提示。
///
/// **只追加，绝不替换** —— 加一个扩展永远不会移除已有指导。
/// 同样的单调性也适用于策略链。
///
/// 顺序确定：按传入顺序，用空行连接，逐字节可复现。
pub fn compose_system_prompt(base: &str, appends: &[String]) -> String {
    let mut out = base.to_string();
    for a in appends {
        if a.trim().is_empty() {
            continue;
        }
        out.push_str("\n\n");
        out.push_str(a);
    }
    out
}

// ─────────────────────────────────────────────────────────────
// 加载器
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// 坏文件 —— 必须大声失败，不能静默跳过
    BadManifest { path: PathBuf, reason: String },
    /// 重名
    Duplicate { name: String },
    /// 项目级试图覆盖内建
    ProjectOverrideBuiltin { name: String },
    /// 声明了没启用的能力
    CapabilityNotEnabled { name: String, cap: Capability },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadManifest { path, reason } => {
                write!(f, "插件清单无效（{}）：{reason}", path.display())
            }
            Self::Duplicate { name } => write!(f, "插件重名：{name}"),
            Self::ProjectOverrideBuiltin { name } => write!(
                f,
                "项目级插件 {name} 试图覆盖内建。你 clone 下来的仓库不能替换 CLI 自身的行为。"
            ),
            Self::CapabilityNotEnabled { name, cap } => {
                write!(f, "插件 {name} 声明了未启用的能力 {cap:?}")
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// 插件注册表。
#[derive(Default)]
pub struct Registry {
    plugins: HashMap<String, LoadedPlugin>,
    /// 本次启用哪些能力。默认不含 Native。
    enabled: Vec<Capability>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
            // 默认只放行安全的能力
            enabled: vec![
                Capability::Tools,
                Capability::Hooks,
                Capability::Approvals,
                Capability::Compaction,
                Capability::SystemPrompt,
            ],
        }
    }

    pub fn enable(&mut self, cap: Capability) {
        if !self.enabled.contains(&cap) {
            self.enabled.push(cap);
        }
    }

    pub fn is_enabled(&self, cap: Capability) -> bool {
        self.enabled.contains(&cap)
    }

    /// 注册一个插件。
    ///
    /// 冲突规则：
    /// - 项目级不能覆盖内建（直接报错）
    /// - 用户级可以覆盖内建（大声记录）
    /// - 同 scope 重名 → 报错（不允许静默替换）
    pub fn register(
        &mut self,
        manifest: PluginManifest,
        scope: PluginScope,
        source: impl Into<PathBuf>,
    ) -> Result<(), LoadError> {
        let name = manifest.name.clone();
        let source = source.into();

        // 基本校验
        if name.trim().is_empty() {
            return Err(LoadError::BadManifest {
                path: source,
                reason: "name 不能为空".into(),
            });
        }
        if manifest.entry.trim().is_empty() {
            return Err(LoadError::BadManifest {
                path: source,
                reason: "entry 不能为空".into(),
            });
        }

        // 能力必须已启用
        for cap in &manifest.capabilities {
            if !self.is_enabled(*cap) {
                return Err(LoadError::CapabilityNotEnabled {
                    name: name.clone(),
                    cap: *cap,
                });
            }
        }

        if let Some(existing) = self.plugins.get(&name) {
            // 项目级不能覆盖内建
            if scope == PluginScope::Project && existing.scope == PluginScope::Builtin {
                return Err(LoadError::ProjectOverrideBuiltin { name });
            }
            // 同 scope 重名 → 报错
            if existing.scope == scope {
                return Err(LoadError::Duplicate { name });
            }
            // 用户级覆盖内建：允许，但要大声
            if scope == PluginScope::User && existing.scope == PluginScope::Builtin {
                tracing::warn!("用户插件 {name} 覆盖了内建");
            }
        }

        self.plugins.insert(
            name,
            LoadedPlugin {
                manifest,
                scope,
                source,
            },
        );
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&LoadedPlugin> {
        self.plugins.get(name)
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// 所有插件按优先级排序（项目 > 用户 > 内建）。
    pub fn ordered(&self) -> Vec<&LoadedPlugin> {
        let mut v: Vec<&LoadedPlugin> = self.plugins.values().collect();
        v.sort_by(|a, b| {
            a.scope
                .precedence()
                .cmp(&b.scope.precedence())
                .then(a.manifest.name.cmp(&b.manifest.name))
        });
        v
    }

    /// 收集所有插件的系统提示追加，按优先级顺序。
    pub fn system_prompt_appends(&self) -> Vec<String> {
        self.ordered()
            .into_iter()
            .filter_map(|p| p.manifest.system_prompt_append.clone())
            .collect()
    }

    /// 收集所有工具。同名时高优先级胜出（已由 register 处理覆盖）。
    pub fn tools(&self) -> Vec<&ToolSpec> {
        let mut out: Vec<&ToolSpec> = Vec::new();
        for p in self.ordered() {
            for t in &p.manifest.tools {
                if !out.iter().any(|e| e.name == t.name) {
                    out.push(t);
                }
            }
        }
        out
    }
}

/// 从目录扫描插件清单。
///
/// 坏文件必须报错 —— 一个加载不了的扩展绝不能静默改变行为。
pub fn scan_dir(
    dir: &Path,
    scope: PluginScope,
) -> Result<Vec<(PluginManifest, PathBuf)>, LoadError> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let entries = std::fs::read_dir(dir).map_err(|e| LoadError::BadManifest {
        path: dir.to_path_buf(),
        reason: e.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| LoadError::BadManifest {
            path: dir.to_path_buf(),
            reason: e.to_string(),
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| LoadError::BadManifest {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        let manifest: PluginManifest =
            serde_json::from_str(&text).map_err(|e| LoadError::BadManifest {
                path: path.clone(),
                reason: e.to_string(),
            })?;
        out.push((manifest, path));
    }
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    let _ = scope;
    Ok(out)
}

// ─────────────────────────────────────────────────────────────
// JSON-RPC 协议（与 Go 插件通信）
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: u64,
    // 空字段直接省略，不要序列化成 null ——
    // 成功响应里出现 "error":null 会让调用方误判。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, msg: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(msg.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str) -> PluginManifest {
        PluginManifest {
            name: name.into(),
            version: "0.1.0".into(),
            entry: "plugin".into(),
            capabilities: vec![Capability::Tools],
            tools: vec![],
            system_prompt_append: None,
        }
    }

    fn call(tool: &str) -> CallContext {
        CallContext {
            tool: tool.into(),
            args: serde_json::json!({}),
        }
    }

    // ── 作用域覆盖规则 ──

    #[test]
    fn project_cannot_override_builtin() {
        let mut r = Registry::new();
        r.register(manifest("mcp"), PluginScope::Builtin, "builtin")
            .unwrap();
        let err = r
            .register(manifest("mcp"), PluginScope::Project, "repo/.harness")
            .unwrap_err();
        assert!(
            matches!(err, LoadError::ProjectOverrideBuiltin { .. }),
            "项目级不能覆盖内建：{err}"
        );
    }

    #[test]
    fn user_can_override_builtin() {
        let mut r = Registry::new();
        r.register(manifest("mcp"), PluginScope::Builtin, "builtin")
            .unwrap();
        assert!(r
            .register(manifest("mcp"), PluginScope::User, "~/.harness")
            .is_ok());
        assert_eq!(r.get("mcp").unwrap().scope, PluginScope::User);
    }

    #[test]
    fn same_scope_duplicate_is_rejected() {
        let mut r = Registry::new();
        r.register(manifest("a"), PluginScope::User, "p1").unwrap();
        assert_eq!(
            r.register(manifest("a"), PluginScope::User, "p2"),
            Err(LoadError::Duplicate { name: "a".into() }),
            "同 scope 重名必须报错，不能静默替换"
        );
    }

    #[test]
    fn bad_manifest_is_loud() {
        let mut r = Registry::new();
        let mut m = manifest("");
        m.name = "".into();
        let err = r.register(m, PluginScope::User, "x").unwrap_err();
        assert!(matches!(err, LoadError::BadManifest { .. }));
    }

    #[test]
    fn empty_entry_is_rejected() {
        let mut r = Registry::new();
        let mut m = manifest("x");
        m.entry = "  ".into();
        assert!(matches!(
            r.register(m, PluginScope::User, "p"),
            Err(LoadError::BadManifest { .. })
        ));
    }

    #[test]
    fn native_capability_disabled_by_default() {
        let mut r = Registry::new();
        let mut m = manifest("native-plugin");
        m.capabilities = vec![Capability::Native];
        let err = r.register(m, PluginScope::User, "p").unwrap_err();
        assert!(matches!(
            err,
            LoadError::CapabilityNotEnabled {
                cap: Capability::Native,
                ..
            }
        ));

        // 显式启用后才能装
        r.enable(Capability::Native);
        let mut m2 = manifest("native-plugin2");
        m2.capabilities = vec![Capability::Native];
        assert!(r.register(m2, PluginScope::User, "p2").is_ok());
    }

    // ── 策略链 ──

    #[test]
    fn deny_wins_over_allow() {
        let mut c = ApprovalChain::new();
        c.push("p1", |_| Verdict::Allow);
        c.push("p2", |_| Verdict::Deny {
            reason: "危险命令".into(),
        });
        match c.decide(&call("bash"), true) {
            ChainVerdict::Deny { decided_by, reason } => {
                assert_eq!(decided_by, "p2");
                assert_eq!(reason, "危险命令");
            }
            other => panic!("deny 应胜出，实际 {other:?}"),
        }
    }

    #[test]
    fn first_deny_reason_is_kept() {
        let mut c = ApprovalChain::new();
        c.push("first", |_| Verdict::Deny {
            reason: "第一个理由".into(),
        });
        c.push("second", |_| Verdict::Deny {
            reason: "第二个理由".into(),
        });
        match c.decide(&call("x"), true) {
            ChainVerdict::Deny { decided_by, reason } => {
                assert_eq!(decided_by, "first");
                assert_eq!(reason, "第一个理由", "第一个 deny 的理由传给模型");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn allow_overrides_earlier_ask() {
        // 「允许且不再询问」必须能压过模式档的 ask
        let mut c = ApprovalChain::new();
        c.push("tier", |_| Verdict::Ask);
        c.push("dont-ask-again", |_| Verdict::Allow);
        assert_eq!(c.decide(&call("read"), true), ChainVerdict::Allow);
    }

    #[test]
    fn ask_without_channel_degrades_to_deny() {
        let mut c = ApprovalChain::new();
        c.push("p", |_| Verdict::Ask);
        match c.decide(&call("x"), false) {
            ChainVerdict::Deny { reason, .. } => {
                assert!(
                    reason.contains("没有可用的审批通道"),
                    "应诚实拒绝：{reason}"
                );
            }
            other => panic!("无通道时应拒绝，实际 {other:?}"),
        }
        // 有通道时才是 AskHuman
        assert_eq!(c.decide(&call("x"), true), ChainVerdict::AskHuman);
    }

    #[test]
    fn panicking_policy_counts_as_ask() {
        let mut c = ApprovalChain::new();
        c.push("boom", |_| panic!("策略炸了"));
        // 单独一个抛异常的策略 → ask（有通道时）
        assert_eq!(c.decide(&call("x"), true), ChainVerdict::AskHuman);
    }

    #[test]
    fn empty_chain_allows() {
        let c = ApprovalChain::new();
        assert_eq!(c.decide(&call("x"), false), ChainVerdict::Allow);
    }

    #[test]
    fn deny_beats_panicking_policy() {
        let mut c = ApprovalChain::new();
        c.push("boom", |_| panic!("炸"));
        c.push("deny", |_| Verdict::Deny {
            reason: "明确禁止".into(),
        });
        assert!(matches!(
            c.decide(&call("x"), true),
            ChainVerdict::Deny { .. }
        ));
    }

    // ── 系统提示只追加 ──

    #[test]
    fn system_prompt_only_appends() {
        let base = "你是助手。";
        let out = compose_system_prompt(base, &["规则 A".into(), "规则 B".into()]);
        assert!(out.starts_with(base), "原有内容不能被移除");
        assert!(out.contains("规则 A") && out.contains("规则 B"));
    }

    #[test]
    fn system_prompt_is_deterministic() {
        let base = "b";
        let a = compose_system_prompt(base, &["x".into(), "y".into()]);
        let b = compose_system_prompt(base, &["x".into(), "y".into()]);
        assert_eq!(a, b, "同样输入必须逐字节一致");
    }

    #[test]
    fn empty_appends_ignored() {
        assert_eq!(
            compose_system_prompt("base", &["".into(), "  ".into()]),
            "base"
        );
    }

    // ── 注册表 ──

    #[test]
    fn ordered_by_precedence() {
        let mut r = Registry::new();
        r.register(manifest("z-builtin"), PluginScope::Builtin, "b")
            .unwrap();
        r.register(manifest("a-user"), PluginScope::User, "u")
            .unwrap();
        r.register(manifest("m-project"), PluginScope::Project, "p")
            .unwrap();

        let names: Vec<&str> = r
            .ordered()
            .iter()
            .map(|p| p.manifest.name.as_str())
            .collect();
        assert_eq!(names, vec!["m-project", "a-user", "z-builtin"]);
    }

    #[test]
    fn tools_deduplicated_by_priority() {
        let mut r = Registry::new();
        let mut low = manifest("low");
        low.tools = vec![ToolSpec {
            name: "shared".into(),
            description: "内建版".into(),
            read_only: true,
        }];
        r.register(low, PluginScope::Builtin, "b").unwrap();

        let mut high = manifest("high");
        high.tools = vec![ToolSpec {
            name: "shared".into(),
            description: "用户版".into(),
            read_only: true,
        }];
        r.register(high, PluginScope::User, "u").unwrap();

        let tools = r.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].description, "用户版", "高优先级胜出");
    }

    #[test]
    fn system_prompt_appends_follow_priority() {
        let mut r = Registry::new();
        let mut b = manifest("b");
        b.system_prompt_append = Some("builtin 规则".into());
        r.register(b, PluginScope::Builtin, "x").unwrap();
        let mut p = manifest("p");
        p.system_prompt_append = Some("project 规则".into());
        r.register(p, PluginScope::Project, "y").unwrap();

        let appends = r.system_prompt_appends();
        assert_eq!(appends, vec!["project 规则", "builtin 规则"]);
    }

    // ── 扫描 ──

    #[test]
    fn scan_missing_dir_is_ok() {
        let out = scan_dir(Path::new("/nonexistent/dir"), PluginScope::User).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn scan_bad_json_fails_loudly() {
        let dir = std::env::temp_dir().join(format!("plug-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.json"), "{ not json").unwrap();

        let err = scan_dir(&dir, PluginScope::User).unwrap_err();
        assert!(
            matches!(err, LoadError::BadManifest { .. }),
            "坏 JSON 必须报错"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_reads_valid_manifests() {
        let dir = std::env::temp_dir().join(format!("plug-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = serde_json::json!({
            "name": "demo",
            "version": "0.1.0",
            "entry": "./demo",
            "capabilities": ["tools"],
            "tools": []
        });
        std::fs::write(dir.join("demo.json"), m.to_string()).unwrap();

        let found = scan_dir(&dir, PluginScope::User).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.name, "demo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── RPC ──

    #[test]
    fn rpc_roundtrip() {
        let req = RpcRequest {
            id: 1,
            method: "describe".into(),
            params: serde_json::json!({}),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: RpcRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "describe");

        let resp = RpcResponse::ok(1, serde_json::json!({"name": "demo"}));
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("demo"));
        assert!(!s.contains("error\":"), "成功响应不该带 error");
    }

    #[test]
    fn rpc_error_response() {
        let r = RpcResponse::err(2, "boom");
        assert!(r.result.is_none());
        assert_eq!(r.error.as_deref(), Some("boom"));
    }
}
