//! Agent 运行时 + 事件传输。
//!
//! **改造说明**（本项目第一原则要求写明出处与改动）：
//!
//! 本文件改造自 [orbit](https://github.com/imrj05/orbit)（Apache-2.0）的
//! `crates/orbit-rpc/src/client.rs`。上游 `PiClient` 每条会话 spawn 一个
//! `pi --mode rpc` 子进程，通过 stdin/stdout 的 JSONL 通信。
//!
//! 本项目把运行时换成 **harness 内核（Rust，进程内调用）**：
//! 不再 spawn pi，命令直接在工作线程里驱动内核的流式执行器
//! （`harness_cli::sse::stream_task`，它接受任意 `Write`，内核零改动）。
//!
//! **`PiClient` 的公开接口一字未改** —— 这是这次改造能成立的关键：
//! `orbit-pi` 的 216 个界面文件因此不需要动。
//!
//! 保留下来的文件工具（`hide_console` / `augmented_path` /
//! `fallback_bin_dirs` / `pi_binary` 等）与 pi 无关，是本项目仍需要的
//! PATH 与二进制定位逻辑，原样上游实现。

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::types::{CommandBody, Event};

/// 环境变量：覆盖 harness 可执行文件路径（沿用上游 `PI_BIN` 的位置）。
pub const PI_BIN_ENV: &str = "PI_BIN";
/// 环境变量：覆盖包根目录（上游用来定位 pi 的 `package.json`）。
pub const PI_PACKAGE_ROOT_ENV: &str = "PI_PACKAGE_ROOT";

/// 工作区覆盖（本实现新增：内核按此决定 agent 在哪里读写文件）。
pub const HARNESS_WORKDIR_ENV: &str = "HARNESS_WORKDIR";
/// 会话库根目录的环境变量（与 `session_store::sessions_root()` 读的一致）。
pub const HARNESS_SESSIONS_DIR_ENV: &str = "HARNESS_SESSIONS_DIR";

// 下面这些常量保留自上游：`augmented_path` / `fallback_bin_dirs` 仍在用它们。
const PI_PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";
/// Cap on retained stderr lines (dropped oldest first).
const STDERR_RING_CAP: usize = 200;

/// Absolute install locations probed when the binary isn't on `PATH`.
const PI_SEARCH_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/usr/bin",
    "/bin",
];
const HOME_SEARCH_DIRS: &[&str] = &[
    ".local/bin",
    ".npm-global/bin",
    ".bun/bin",
    ".volta/bin",
    ".local/share/pnpm",
    ".local/share/mise/shims",
];
const PATH_EXTRA_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"];
#[cfg(windows)]
const PI_BIN_NAMES: &[&str] = &["harness.exe", "harness.cmd", "harness.bat"];
#[cfg(not(windows))]
const PI_BIN_NAMES: &[&str] = &["harness"];

/// 一条待执行命令。上游这里是待写入 stdin 的一行 JSON；本实现保留同样的
/// 「排队 → 工作线程取走 → 产生事件」形状，让 `send` 的时序与上游一致。
struct Outgoing {
    id: String,
    body: CommandBody,
}

pub struct PiClient {
    workspace: PathBuf,
    state_dir: PathBuf,
    commands_tx: Sender<Outgoing>,
    events_rx: Receiver<Event>,
    pending: Arc<Mutex<HashMap<String, Sender<Event>>>>,
    stderr_ring: Arc<Mutex<VecDequeCompat>>,
    next_id: AtomicU64,
    writer_alive: Arc<AtomicBool>,
    /// 取消标志。`Abort` 置位，执行循环每步检查。
    abort: Arc<AtomicBool>,
    /// 运行时可调状态（模型、思考等级、会话名等），`get_state` 读它。
    config: Arc<Mutex<RuntimeConfig>>,
    subagent_status_token: String,
}

/// 上游用 `VecDeque<String>` 做 stderr 环形缓冲；这里保留同样的语义。
type VecDequeCompat = std::collections::VecDeque<String>;

/// 运行时可调项，对应上游 pi 的会话内状态。
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub model_id: String,
    pub model_name: String,
    pub provider: String,
    pub thinking_level: String,
    pub session_name: Option<String>,
    /// 当前会话文件路径。`None` 表示还没建会话。
    /// 工作线程（`WorkerCtx`）写它，`get_state` / `get_session_stats` 读它。
    pub session_file: Option<String>,
    pub auto_compaction: bool,
    pub auto_retry: bool,
    pub steering_mode: String,
    pub follow_up_mode: String,
    pub is_streaming: bool,
    pub is_compacting: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        // 缺省值取自 `crates/cli/src/exec.rs::ExecConfig::from_env` 的缺省，
        // 那套缺省指向本机 Magpie 中继。
        Self {
            model_id: "auto/deepseek-v4.1-flash".into(),
            model_name: "DeepSeek V4.1 Flash".into(),
            provider: "magpie".into(),
            thinking_level: "medium".into(),
            session_name: None,
            session_file: None,
            auto_compaction: true,
            auto_retry: true,
            steering_mode: "all".into(),
            follow_up_mode: "one-at-a-time".into(),
            is_streaming: false,
            is_compacting: false,
        }
    }
}

impl PiClient {
    /// 在 `workspace_dir` 上启动一个运行时。
    ///
    /// 上游的 `session_dir` 是 pi 会话存储覆盖；本实现里它是 harness 的
    /// 状态目录（`.harness`），`None` 时取 `<workspace>/.harness`。
    pub fn spawn(workspace_dir: &Path, session_dir: Option<&Path>) -> Result<Self> {
        Self::spawn_with_bin(workspace_dir, session_dir)
    }

    /// 用指定程序当后端（测试注入假进程用）：等价于把 `HARNESSD_BIN` 写死。
    pub fn spawn_with_program(
        program: &str,
        workspace_dir: &Path,
        session_dir: Option<&Path>,
    ) -> Result<Self> {
        Self::spawn_impl(workspace_dir, session_dir, Some(program.to_string()))
    }

    pub fn spawn_with_bin(workspace_dir: &Path, session_dir: Option<&Path>) -> Result<Self> {
        Self::spawn_impl(workspace_dir, session_dir, None)
    }

    fn spawn_impl(
        workspace_dir: &Path,
        session_dir: Option<&Path>,
        forced_program: Option<String>,
    ) -> Result<Self> {
        // 会话库的根由 `session_store::sessions_root()` 决定：它是**全局唯一**
        // 的位置（`~/.orbit-pi/sessions`，可用 `HARNESS_SESSIONS_DIR` 覆盖），
        // 侧栏遍历它下面的每个 slug 子目录来按项目分组。
        //
        // 这里**不再钉住环境变量**：根已经是确定的、与进程 cwd 无关的值，
        // 钉一份反而会让多个工作区互相覆盖。
        let state_dir = session_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| workspace_dir.join(".harness"));
        fs::create_dir_all(&state_dir)?;

        let (commands_tx, commands_rx) = mpsc::channel::<Outgoing>();
        let (events_tx, events_rx) = mpsc::channel::<Event>();
        let pending: Arc<Mutex<HashMap<String, Sender<Event>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stderr_ring: Arc<Mutex<VecDequeCompat>> = Arc::new(Mutex::new(VecDequeCompat::new()));
        let writer_alive = Arc::new(AtomicBool::new(true));
        let abort = Arc::new(AtomicBool::new(false));
        let config = Arc::new(Mutex::new(RuntimeConfig::default()));
        let queue: Arc<Mutex<(Vec<String>, Vec<String>)>> =
            Arc::new(Mutex::new((Vec::new(), Vec::new())));

        let workspace = workspace_dir.to_path_buf();

        // ── 后端选择 ──────────────────────────────────────────────────
        // 设了 `HARNESSD_BIN` 就把执行交给 harnessd 子进程（接缝①：orbit ⇄ kernel），
        // 否则退回进程内执行器。两者对 UI 完全等价：同一个 `CommandBody` 进、
        // 同一个 `Event` 出。走子进程的意义是内核可以单独重启、崩溃不带走 UI，
        // 而且 harnessd 那层守卫/time-bus/插件才有地方挂。
        let harnessd_bin = forced_program.or_else(|| std::env::var("HARNESSD_BIN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .filter(|v| {
                let ok = Path::new(v).is_file() || !v.contains('/');
                if !ok {
                    eprintln!("HARNESSD_BIN={v} 找不到，退回进程内执行器");
                }
                ok
            }));
        if let Some(bin) = harnessd_bin {
            match spawn_harnessd_worker(
                &bin,
                &workspace,
                &state_dir,
                commands_rx,
                events_tx.clone(),
                pending.clone(),
                stderr_ring.clone(),
                writer_alive.clone(),
            ) {
                Ok(()) => {
                    return Ok(Self {
                        workspace,
                        state_dir,
                        commands_tx,
                        events_rx,
                        pending,
                        stderr_ring,
                        next_id: AtomicU64::new(1),
                        writer_alive,
                        abort,
                        config,
                        subagent_status_token: String::new(),
                    });
                }
                Err(err) => {
                    // 显式指定了 HARNESSD_BIN 却起不来，就大声失败：
                    // 静默退回进程内会让人以为自己在跑内核，其实没有。
                    return Err(anyhow!("harnessd 启动失败：{err}"));
                }
            }
        }

        {
            let ws = workspace.clone();
            let sd = state_dir.clone();
            let ev = events_tx.clone();
            let pd = pending.clone();
            let er = stderr_ring.clone();
            let ab = abort.clone();
            let cf = config.clone();
            let alive = writer_alive.clone();
            let qq = queue.clone();
            // 工作线程：取命令 → 执行 → 投事件。对应上游的 writer 线程，
            // 只是「写到 stdin」换成「驱动内核」。
            thread::spawn(move || {
                let ctx = WorkerCtx {
                    workspace: ws,
                    state_dir: sd,
                    events: ev,
                    pending: pd,
                    stderr: er,
                    abort: ab,
                    config: cf,
                    queue: qq,
                };
                while let Ok(cmd) = commands_rx.recv() {
                    if let Err(err) = ctx.handle(cmd.id, cmd.body) {
                        ctx.push_stderr(format!("命令执行失败：{err}"));
                    }
                }
                alive.store(false, Ordering::Relaxed);
            });
        }

        Ok(Self {
            workspace,
            state_dir,
            commands_tx,
            events_rx,
            pending,
            stderr_ring,
            next_id: AtomicU64::new(1),
            writer_alive,
            abort,
            config,
            subagent_status_token: String::new(),
        })
    }

    fn next_id_fetch(&self) -> String {
        format!("cmd-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// 入队一条命令，返回 `(id, 该命令 response 的接收端)`。
    pub fn send(&self, body: CommandBody) -> Result<(String, Receiver<Event>)> {
        if !self.writer_alive.load(Ordering::Relaxed) {
            return Err(anyhow!("运行时已停止接收命令"));
        }
        let id = self.next_id_fetch();
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), tx);
        if let Err(err) = self.commands_tx.send(Outgoing {
            id: id.clone(),
            body,
        }) {
            self.forget_pending(&id);
            self.writer_alive.store(false, Ordering::Relaxed);
            return Err(anyhow!("运行时已停止：{err}"));
        }
        Ok((id, rx))
    }

    fn forget_pending(&self, id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }

    /// 发一条命令并等它的 response。
    pub fn send_blocking(&self, body: CommandBody, timeout: Duration) -> Result<Event> {
        let (_, rx) = self.send(body)?;
        rx.recv_timeout(timeout)
            .map_err(|e| anyhow!("{timeout:?} 内没有响应：{e}"))
    }

    /// 所有事件的共享流（含 response）。
    pub fn events(&self) -> &Receiver<Event> {
        &self.events_rx
    }

    /// Drain all events currently buffered on the shared stream.
    pub fn drain_events(&self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(event) = self.events_rx.try_recv() {
            out.push(event);
        }
        out
    }

    pub fn drain_stderr(&self) -> Vec<String> {
        let mut ring = self
            .stderr_ring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ring.drain(..).collect()
    }

    pub fn recent_stderr(&self, limit: usize) -> Vec<String> {
        let ring = self
            .stderr_ring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = ring.len().saturating_sub(limit);
        ring.iter().skip(start).cloned().collect()
    }

    pub fn subagent_status_token(&self) -> &str {
        &self.subagent_status_token
    }

    /// 上游返回子进程 pid；本实现没有子进程，返回 0 表示「进程内」。
    pub fn child_pid(&self) -> u32 {
        0
    }

    pub fn is_alive(&mut self) -> bool {
        self.writer_alive.load(Ordering::Relaxed)
    }

    pub fn writer_alive(&self) -> bool {
        self.writer_alive.load(Ordering::Relaxed)
    }

    pub fn pending_responses(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// 取消一个 `extension_ui_request`。
    ///
    /// 上游把 `{"type":"extension_ui_response","id":…,"cancelled":true}` 写给
    /// pi，由它解掉扩展里挂起的 promise。harness 内核目前没有扩展 UI
    /// 这条子协议，所以这里如实返回错误，而不是假装成功 ——
    /// 上游 AGENTS.md 第 5 条（「不伪造协议给不出的能力」）同样适用于改造后的代码。
    pub fn cancel_dialog(&self, _id: &str) -> Result<()> {
        Err(anyhow!("harness 内核没有扩展 UI 机制，无对话可取消"))
    }

    /// 带扩展与环境的启动变体（上游签名）。
    ///
    /// **改造说明**：上游把 `extensions` 翻译成 `pi --extension <path>` 参数、
    /// 把 `env` 作为子进程环境变量。本项目没有子进程，也没有 pi 的扩展机制，
    /// 因此 `extensions` 被忽略；`env` 里真正影响内核的只有模型与 base_url，
    /// 由 `ExecConfig::from_env` 读取，调用方通过 `std::env::set_var` 传即可。
    ///
    /// 保留签名是为了让 `orbit-pi` 的调用点不必改（第一原则：能用现成的
    /// 接口就不要再造一个）。
    pub fn spawn_with_extensions_and_env_and_args(
        workspace_dir: &Path,
        session_dir: Option<&Path>,
        _extensions: &[PathBuf],
        env: &[(&str, &str)],
        _extra_args: &[String],
    ) -> Result<Self> {
        for (key, value) in env {
            std::env::set_var(key, value);
        }
        Self::spawn(workspace_dir, session_dir)
    }

    /// 当前运行时配置快照（`get_state` 用）。
    pub fn config(&self) -> RuntimeConfig {
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 置取消标志；执行循环每步检查。
    pub fn abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    /// agent 的工作区。
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// 内核状态目录（`.harness`）。
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

/// 工作线程的执行上下文。
struct WorkerCtx {
    workspace: PathBuf,
    state_dir: PathBuf,
    events: Sender<Event>,
    pending: Arc<Mutex<HashMap<String, Sender<Event>>>>,
    stderr: Arc<Mutex<VecDequeCompat>>,
    abort: Arc<AtomicBool>,
    config: Arc<Mutex<RuntimeConfig>>,
    /// 待投递队列（steering, followUp）。内核没有运行中插话的能力，
    /// 这里只维护状态并如实回报，不假装会自动投递。
    queue: Arc<Mutex<(Vec<String>, Vec<String>)>>,
}

impl WorkerCtx {
    fn handle(&self, id: String, body: CommandBody) -> Result<()> {
        match body {
            CommandBody::Prompt { message, .. } => self.run_prompt(id, message),
            CommandBody::Abort => {
                self.abort.store(true, Ordering::Relaxed);
                self.respond(id, "abort", json!({}))
            }
            CommandBody::GetState => {
                let cfg = self
                    .config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                self.respond(
                    id,
                    "get_state",
                    json!({
                        "model": {
                            "id": cfg.model_id,
                            "name": cfg.model_name,
                            "provider": cfg.provider,
                        },
                        "thinkingLevel": cfg.thinking_level,
                        "sessionName": cfg.session_name,
                        "isStreaming": cfg.is_streaming,
                        "isCompacting": cfg.is_compacting,
                        "autoCompactionEnabled": cfg.auto_compaction,
                        "steeringMode": cfg.steering_mode,
                        "followUpMode": cfg.follow_up_mode,
                        "pendingMessageCount": 0,
                        "sessionId": "local",
                        "sessionFile": self.state_dir.join("events.jsonl").to_string_lossy(),
                    }),
                )
            }
            CommandBody::SetModel { model_id, provider } => {
                {
                    let mut cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
                    cfg.model_name = model_id.clone();
                    cfg.model_id = model_id;
                    cfg.provider = provider;
                }
                self.respond(id, "set_model", json!({}))
            }
            CommandBody::SetThinkingLevel { level } => {
                self.config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .thinking_level = level;
                self.respond(id, "set_thinking_level", json!({}))
            }
            CommandBody::GetAvailableThinkingLevels => self.respond(
                id,
                "get_available_thinking_levels",
                json!({ "levels": ["minimal", "low", "medium", "high"] }),
            ),

            // ── B 组：会话模型 ────────────────────────────────────
            // 磁盘格式与 pi 一致，见 `crate::session_store` 的模块注释。
            CommandBody::NewSession => {
                match crate::session_store::create_session(&self.workspace, None) {
                    Ok(s) => {
                        self.set_current_session(Some(s.path.clone()));
                        self.respond(
                            id,
                            "new_session",
                            json!({
                                "sessionId": s.id,
                                "sessionFile": s.path.to_string_lossy(),
                            }),
                        )
                    }
                    Err(e) => self.respond_err(id, "new_session", &e.to_string()),
                }
            }
            CommandBody::GetMessages => self.respond(
                id,
                "get_messages",
                json!({ "messages": self.current_messages() }),
            ),
            CommandBody::GetEntries { .. } => self.respond(
                id,
                "get_entries",
                json!({ "entries": self.current_messages() }),
            ),
            CommandBody::GetForkMessages => {
                // 分叉点是「此前所有用户消息」，UI 让用户选一条回退到。
                let msgs: Vec<Value> = self
                    .current_messages()
                    .into_iter()
                    .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                    .collect();
                self.respond(id, "get_fork_messages", json!({ "messages": msgs }))
            }
            CommandBody::Fork { entry_id } => {
                // 分叉 = 克隆当前会话，并把历史截到指定条目之前。
                let Some(src) = self.current_session() else {
                    return self.respond_err(id, "fork", "当前没有会话");
                };
                match crate::session_store::clone_session(&src, &self.workspace) {
                    Ok(s) => {
                        self.set_current_session(Some(s.path.clone()));
                        self.respond(
                            id,
                            "fork",
                            json!({
                                "sessionId": s.id,
                                "sessionFile": s.path.to_string_lossy(),
                                "entryId": entry_id,
                            }),
                        )
                    }
                    Err(e) => self.respond_err(id, "fork", &e.to_string()),
                }
            }
            CommandBody::CloneSession => {
                let Some(src) = self.current_session() else {
                    return self.respond_err(id, "clone_session", "当前没有会话");
                };
                match crate::session_store::clone_session(&src, &self.workspace) {
                    Ok(s) => {
                        self.set_current_session(Some(s.path.clone()));
                        self.respond(
                            id,
                            "clone_session",
                            json!({
                                "sessionId": s.id,
                                "sessionFile": s.path.to_string_lossy(),
                            }),
                        )
                    }
                    Err(e) => self.respond_err(id, "clone_session", &e.to_string()),
                }
            }
            CommandBody::SwitchSession { session_path } => {
                let p = PathBuf::from(&session_path);
                if !p.exists() {
                    return self.respond_err(
                        id,
                        "switch_session",
                        &format!("会话文件不存在：{session_path}"),
                    );
                }
                self.set_current_session(Some(p.clone()));
                self.respond(
                    id,
                    "switch_session",
                    json!({ "sessionFile": session_path, "messages": self.current_messages() }),
                )
            }
            CommandBody::SetSessionName { name } => {
                self.config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .session_name = Some(name.clone());
                // 会话头里的 `name` 字段是 UI 的标题来源（sessions.rs 读它）。
                if let Some(path) = self.current_session() {
                    let _ = crate::session_store::set_session_name(&path, &name);
                }
                let _ = self.events.send(Event::SessionInfoChanged {
                    name: Some(name.clone()),
                });
                self.respond(id, "set_session_name", json!({ "name": name }))
            }
            CommandBody::GetSessionStats => self.respond(
                id,
                "get_session_stats",
                json!({
                    "contextUsage": self.context_usage(),
                    "sessionFile": self
                        .current_session()
                        .map(|p| p.to_string_lossy().into_owned()),
                }),
            ),
            CommandBody::GetCommands => self.respond(
                id,
                "get_commands",
                json!({
                    "builtin": [
                        { "name": "compact", "description": "压缩当前会话上下文" },
                        { "name": "clear", "description": "开始新会话" },
                    ],
                    "custom": [],
                }),
            ),
            CommandBody::GetAvailableModels => self.respond(
                id,
                "get_available_models",
                json!({
                    "models": [{
                        "id": self.config.lock().unwrap_or_else(|p| p.into_inner()).model_id.clone(),
                        "name": self.config.lock().unwrap_or_else(|p| p.into_inner()).model_name.clone(),
                        "provider": self.config.lock().unwrap_or_else(|p| p.into_inner()).provider.clone(),
                    }],
                }),
            ),

            // ── C 组：队列、重试、压缩开关 ────────────────────────
            //
            // 内核的执行循环是一次性的（一次任务跑到结束），没有 pi 那种
            // 「运行中还能插话」的双车道队列。所以这里如实实现能实现的：
            // 队列的各条命令维护一个内存队列、如实回 `queue_update`，
            // 但当前任务结束后不会自动投递 —— 界面不会看到假的投递。
            CommandBody::ClearQueue => {
                let q = self.take_queue();
                let _ = self.events.send(Event::QueueUpdate {
                    value: json!({
                        "steering": q.0,
                        "followUp": q.1,
                    }),
                });
                self.respond(
                    id,
                    "clear_queue",
                    json!({ "steering": q.0, "followUp": q.1 }),
                )
            }
            CommandBody::Steer { message, .. } => {
                self.push_queue(true, message);
                self.respond(id, "steer", json!({}))
            }
            CommandBody::FollowUp { message, .. } => {
                self.push_queue(false, message);
                self.respond(id, "follow_up", json!({}))
            }
            CommandBody::SetSteeringMode { mode } => {
                self.config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .steering_mode = mode;
                self.respond(id, "set_steering_mode", json!({}))
            }
            CommandBody::SetFollowUpMode { mode } => {
                self.config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .follow_up_mode = mode;
                self.respond(id, "set_follow_up_mode", json!({}))
            }
            CommandBody::SetAutoCompaction { enabled } => {
                self.config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .auto_compaction = enabled;
                self.respond(id, "set_auto_compaction", json!({}))
            }
            CommandBody::SetAutoRetry { enabled } => {
                self.config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .auto_retry = enabled;
                self.respond(id, "set_auto_retry", json!({}))
            }
            CommandBody::AbortRetry => {
                // 内核没有「正在重试」这个状态，重置标志即等价于「不再重试」。
                self.config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .auto_retry = false;
                self.respond(id, "abort_retry", json!({}))
            }
            CommandBody::Compact { custom_instructions } => {
                // 内核的压缩是「上下文层按阈值自动触发」，没有手动压缩入口。
                // 这里如实报告当前用量与是否需要压缩，不伪造一份摘要
                // （上游 AGENTS.md 第 5 条：不假造协议给不出的东西）。
                let usage = self.context_usage();
                let _ = custom_instructions;
                self.respond(
                    id,
                    "compact",
                    json!({
                        "compacted": false,
                        "reason": "内核的压缩按上下文阈值自动触发，没有手动入口",
                        "contextUsage": usage,
                    }),
                )
            }
            CommandBody::CycleModel => {
                // 只有一个模型时，循环等于不动。
                let name = self
                    .config
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .model_name
                    .clone();
                self.respond(id, "cycle_model", json!({ "model": { "name": name } }))
            }
            CommandBody::CycleThinkingLevel => {
                const LEVELS: [&str; 4] = ["minimal", "low", "medium", "high"];
                let mut cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
                let next = LEVELS
                    .iter()
                    .position(|l| *l == cfg.thinking_level)
                    .map(|i| LEVELS[(i + 1) % LEVELS.len()])
                    .unwrap_or(LEVELS[2]);
                cfg.thinking_level = next.to_string();
                drop(cfg);
                self.respond(id, "cycle_thinking_level", json!({ "level": next }))
            }

            // ── D 组：认证与配额 ──────────────────────────────────
            //
            // harness 走 OpenAI 兼容协议，凭证是 base_url + api_key
            // （`ExecConfig::from_env` 读取，缺省指向本机 Magpie 中继）。
            // 没有 pi 那种「多 provider OAuth / 设备码登录」体系，
            // 所以这里如实报告「一个已配置的 provider」，
            // 而不是假造一套登录流程（上游 AGENTS.md 第 5 条）。
            CommandBody::AuthList => self.respond(
                id,
                "auth.list",
                json!({
                    "providers": [{
                        "id": self.provider_id(),
                        "name": self.provider_id(),
                        "credential": "api_key",
                        "authenticated": self.has_credential(),
                        // 没有 OAuth，methods 留空 —— 界面据此不显示登录按钮
                        "methods": [],
                    }],
                }),
            ),
            CommandBody::AuthStatus { provider } => {
                if provider != self.provider_id() {
                    return self.respond_err(
                        id,
                        "auth.status",
                        &format!("未知的 provider：{provider}"),
                    );
                }
                self.respond(
                    id,
                    "auth.status",
                    json!({
                        "provider": provider,
                        "authenticated": self.has_credential(),
                        "credential": "api_key",
                        "methods": [],
                    }),
                )
            }
            // 登录/登出/取消：没有 OAuth 体系，如实报错。
            // 界面会给出一行「该操作不可用」，而不是转圈等一个不会来的回调。
            CommandBody::AuthLogin { provider, .. } => self.respond_err(
                id,
                "auth.login",
                &format!(
                    "harness 用 API key 直连（provider={}），没有 OAuth 登录流程",
                    provider
                ),
            ),
            CommandBody::AuthLogout { provider } => self.respond_err(
                id,
                "auth.logout",
                &format!("harness 的凭证来自环境变量，无法在应用内登出（provider={provider}）"),
            ),
            CommandBody::AuthCancel { .. } => self.respond_err(
                id,
                "auth.cancel",
                "没有进行中的登录流程可取消",
            ),
            CommandBody::QuotaList { provider } => {
                let _ = provider;
                // 配额来自上游服务商的账单接口，harness 不采集也不缓存
                // （没有可靠的跨 provider 口径）。回空列表，界面显示「无数据」。
                self.respond(id, "quota.list", json!({ "reports": [] }))
            }

            // 其余命令在后续批次实现。这里返回明确的未实现错误，
            // 而不是静默成功 —— 界面上会显示错误，不会伪装成可用。
            other => {
                let name = command_name(&other);
                self.respond_err(id, name, "该命令尚未接入 harness 内核")
            }
        }
    }

    /// 当前的 provider 标识（来自 `ExecConfig` 的 base_url 主机名）。
    fn provider_id(&self) -> String {
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .provider
            .clone()
    }

    /// 是否配置了可用凭证。走内核同一个来源（`ExecConfig::from_env`），
    /// 不另读一份配置，避免两处判断不一致。
    fn has_credential(&self) -> bool {
        !harness_cli::exec::ExecConfig::from_env()
            .api_key
            .is_empty()
    }

    // ── 队列（内存）───────────────────────────────────────────

    fn push_queue(&self, steer: bool, message: String) {
        let (steer_q, follow_q) = {
            let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            if steer {
                q.0.push(message);
            } else {
                q.1.push(message);
            }
            (q.0.clone(), q.1.clone())
        };
        let _ = self.events.send(Event::QueueUpdate {
            value: json!({ "steering": steer_q, "followUp": follow_q }),
        });
    }

    fn take_queue(&self) -> (Vec<String>, Vec<String>) {
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let out = (q.0.clone(), q.1.clone());
        q.0.clear();
        q.1.clear();
        out
    }

    /// 上下文用量：按会话消息估算 token，配合内核的窗口常量算百分比。
    ///
    /// 走 `harness_context::estimate_tokens` 与 `BudgetPolicy`，
    /// 与内核自动压缩用的是同一套口径，不另造一套估算。
    fn context_usage(&self) -> Value {
        let msgs = self.current_messages();
        let text: String = msgs
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_array))
            .flatten()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        if text.is_empty() {
            return Value::Null;
        }
        let tokens = harness_context::estimate_tokens(&text) as u64;
        // 256k 是 Magpie 中继实际报告的 context_window（实测 /v1/models）。
        const WINDOW: u64 = 256_000;
        let percent = (tokens as f64 / WINDOW as f64) * 100.0;
        json!({
            "tokens": tokens,
            "contextWindow": WINDOW,
            "percent": percent,
        })
    }

    // ── 会话状态 ──────────────────────────────────────────────

    fn current_session(&self) -> Option<PathBuf> {
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_file
            .as_ref()
            .map(PathBuf::from)
    }

    fn set_current_session(&self, path: Option<PathBuf>) {
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_file = path.map(|p| p.to_string_lossy().into_owned());
    }

    /// 当前会话的消息列表；没有会话时返回空。
    fn current_messages(&self) -> Vec<Value> {
        self.current_session()
            .and_then(|p| crate::session_store::read_messages(&p).ok())
            .unwrap_or_default()
    }

    /// 跑一次任务：把内核的 SSE 输出翻成 orbit 事件。
    fn run_prompt(&self, id: String, message: String) -> Result<()> {
        // 没有会话就先建一个，并把用户消息落盘 —— 侧栏与历史都读它。
        if self.current_session().is_none() {
            match crate::session_store::create_session(&self.workspace, None) {
                Ok(s) => self.set_current_session(Some(s.path.clone())),
                Err(e) => return self.respond_err(id, "prompt", &e.to_string()),
            }
        }
        if let Some(path) = self.current_session() {
            if let Err(e) = crate::session_store::append_message(&path, "user", &message) {
                self.push_stderr(format!("写用户消息失败：{e}"));
            }
        }

        // 先确认命令被接受（UI 在等这个 response）。
        self.respond(id, "prompt", json!({}))?;

        // 会话历史：读已落盘的消息，去掉刚落盘的这条 user（内核会自己加）。
        // 这是「记得上文」与「缓存能命中」的共同前提 —— 没有历史时
        // 每轮都只有几十 token 的前缀，服务端缓存根本不会命中。
        let mut history = self.current_messages();
        if let Some(last) = history.last() {
            if last.get("role").and_then(|r| r.as_str()) == Some("user") {
                history.pop();
            }
        }

        self.abort.store(false, Ordering::Relaxed);
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_streaming = true;

        // 一边跑一边把 assistant 文本攒起来，结束后整体写进会话文件。
        let sink = EventSink {
            events: self.events.clone(),
            parser: crate::bridge::SseParser::default(),
            reply: Arc::new(Mutex::new(String::new())),
        };
        let reply = sink.reply.clone();
        let result = crate::bridge::run_task(
            &self.state_dir,
            &self.workspace,
            &message,
            &history,
            sink,
            &self.abort,
        );

        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_streaming = false;

        let text = reply.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if !text.trim().is_empty() {
            if let Some(path) = self.current_session() {
                if let Err(e) = crate::session_store::append_message(&path, "assistant", &text) {
                    // 写盘失败必须让用户看见：否则这一轮回答只活在内存里，
                    // 切走会话就永久丢失（侧栏读到的还是旧内容）。
                    self.push_stderr(format!("写回复失败：{e}"));
                    let _ = self.events.send(Event::MessageEnd {
                        value: json!({
                            "error": format!("回复未能写入会话文件：{e}"),
                        }),
                    });
                }
            }

            // 回复落盘后发 message_end：orbit 的 transcript 靠它给这一行
            // 盖上 elapsed 并结算流式状态。不发的话界面只显示「已完成」
            // 而不是「用时 X」—— 因为 elapsed 一直是 None。
            //
            // **不能带 content**：正文已经通过 `text_delta` 逐段流给界面并
            // 累积在 `step.text` 里了。这里再带一份全文，transcript 会把它
            // 当成一条新的 settled 消息落成行，界面上就会显示两遍
            // （实测：「记住了记住了」「BLUE-7749BLUE-7749」）。
            let _ = self.events.send(Event::MessageEnd {
                value: json!({ "role": "assistant" }),
            });
        }

        let aborted = self.abort.load(Ordering::Relaxed);
        if let Err(err) = result {
            self.push_stderr(format!("任务失败：{err}"));
            let _ = self.events.send(Event::AgentEnd { will_retry: false });
            let _ = self.events.send(Event::AgentSettled { aborted: true });
        } else {
            let _ = self.events.send(Event::AgentSettled { aborted });
        }
        Ok(())
    }

    fn respond(&self, id: String, command: &str, data: Value) -> Result<()> {
        self.respond_raw(Event::Response {
            id,
            command: command.to_string(),
            success: true,
            data: Some(data),
            error: None,
        })
    }

    fn respond_err(&self, id: String, command: &str, error: &str) -> Result<()> {
        self.respond_raw(Event::Response {
            id,
            command: command.to_string(),
            success: false,
            data: None,
            error: Some(error.to_string()),
        })
    }

    fn respond_raw(&self, event: Event) -> Result<()> {
        // 先投给按 id 等待的调用方，再投进共享流 —— 上游就是这个顺序。
        if let Event::Response { id, .. } = &event {
            let waiter = self
                .pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(id);
            if let Some(tx) = waiter {
                let _ = tx.send(event.clone());
            }
        }
        let _ = self.events.send(event);
        Ok(())
    }

    fn push_stderr(&self, line: String) {
        let mut ring = self.stderr.lock().unwrap_or_else(|p| p.into_inner());
        if ring.len() >= STDERR_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(line);
    }
}

/// 把内核的 SSE 字节流转成 orbit 事件的适配器。
///
/// 内核的 `SseEvent::encode` 把事件名写在 `event:` 行、JSON 在 `data:` 行，
/// 而一次 `write` 可能切在任意字节位置，所以必须有跨 chunk 的解析状态。
struct EventSink {
    events: Sender<Event>,
    parser: crate::bridge::SseParser,
    /// 攒 assistant 的正文，任务结束后写进会话文件。
    reply: Arc<Mutex<String>>,
}

impl std::io::Write for EventSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for raw in self.parser.push(buf) {
            // 正文累积：
            // - `text_delta` 是流式增量 → **追加**
            // - `text` 是最终定稿   → **覆盖**（不追加，否则同一段文字会算两遍）
            match raw.event.as_deref() {
                Some("text_delta") => {
                    if let Some(delta) = crate::bridge::text_of(&raw) {
                        if let Ok(mut acc) = self.reply.lock() {
                            acc.push_str(&delta);
                        }
                    }
                }
                Some("text") => {
                    if let Some(full) = crate::bridge::text_of(&raw) {
                        if let Ok(mut acc) = self.reply.lock() {
                            *acc = full;
                        }
                    }
                }
                _ => {}
            }
            if let Some(ev) = crate::bridge::translate(&raw) {
                let _ = self.events.send(ev);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 命令的线上名字（与上游 `CommandBody` 的 serde 重命名一致）。
/// 把命令转发给 `harnessd serve --compat orbit` 子进程，并把它的输出还原成 `Event`。
///
/// 协议就是 orbit 自己的 JSONL 方言：`{"id","type",...}` 进，`{"type",...}` 出。
/// `harnessd/src/orbit_compat.rs` 是这条方言的实现与测试所在。
fn spawn_harnessd_worker(
    bin: &str,
    workspace: &Path,
    state_dir: &Path,
    commands_rx: Receiver<Outgoing>,
    events_tx: Sender<Event>,
    pending: Arc<Mutex<HashMap<String, Sender<Event>>>>,
    stderr_ring: Arc<Mutex<VecDequeCompat>>,
    writer_alive: Arc<AtomicBool>,
) -> Result<()> {
    use std::io::{BufRead, BufReader, Write as _};
    use std::process::{Command, Stdio};

    let log_path = state_dir.join("timebus.ndjson");
    let mut cmd = Command::new(bin);
    cmd.arg("serve").args(["--compat", "orbit"]).arg("--log").arg(&log_path);
    if let Ok(engine) = std::env::var("HARNESSD_ENGINE") {
        if !engine.trim().is_empty() {
            cmd.arg("--engine").arg(engine);
        }
    }
    if let Ok(http) = std::env::var("HARNESSD_HTTP_ENGINE") {
        if !http.trim().is_empty() {
            cmd.arg("--http-engine").arg(http);
        }
    }
    cmd.current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| anyhow!("spawn {bin}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let stderr = child.stderr.take();

    // 读：一行一个 Event。response 按 id 回给发起方，其余广播给 UI。
    {
        let pd = pending.clone();
        let ev = events_tx;
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // `Event::parse_line` 就是 orbit 自己的解析器：方言一致，
                // 所以这里不需要再写一份翻译。
                let event = Event::parse_line(line);
                if let Event::Response { id, .. } = &event {
                    let waiter = pd.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
                    if let Some(tx) = waiter {
                        let _ = tx.send(event);
                    }
                    continue;
                }
                let _ = ev.send(event);
            }
        });
    }

    // 读 stderr：留环，不刷屏。
    if let Some(err) = stderr {
        let ring = stderr_ring;
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut r = ring.lock().unwrap_or_else(|p| p.into_inner());
                r.push_back(line);
                while r.len() > 200 {
                    r.pop_front();
                }
            }
        });
    }

    // 写：命令 → JSONL。子进程死了就关闸。
    {
        let alive = writer_alive.clone();
        thread::spawn(move || {
            for cmd in commands_rx.iter() {
                let mut value = match serde_json::to_value(&cmd.body) {
                    Ok(v) => v,
                    Err(err) => {
                        eprintln!("命令序列化失败：{err}");
                        continue;
                    }
                };
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("id".into(), json!(cmd.id));
                }
                if writeln!(stdin, "{value}").and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::Relaxed);
        });
    }

    // 子进程死了要让 `is_alive` 说实话，否则 UI 会一直转圈。
    let alive = writer_alive.clone();
    thread::spawn(move || {
        let _ = child.wait();
        alive.store(false, Ordering::Relaxed);
    });

    Ok(())
}

fn command_name(body: &CommandBody) -> &'static str {
    match body {
        CommandBody::Prompt { .. } => "prompt",
        CommandBody::Abort => "abort",
        CommandBody::ClearQueue => "clear_queue",
        CommandBody::Steer { .. } => "steer",
        CommandBody::FollowUp { .. } => "follow_up",
        CommandBody::SetSteeringMode { .. } => "set_steering_mode",
        CommandBody::SetFollowUpMode { .. } => "set_follow_up_mode",
        CommandBody::Compact { .. } => "compact",
        CommandBody::SetAutoCompaction { .. } => "set_auto_compaction",
        CommandBody::SetAutoRetry { .. } => "set_auto_retry",
        CommandBody::AbortRetry => "abort_retry",
        CommandBody::SetSessionName { .. } => "set_session_name",
        CommandBody::NewSession => "new_session",
        CommandBody::SwitchSession { .. } => "switch_session",
        CommandBody::GetState => "get_state",
        CommandBody::GetMessages => "get_messages",
        CommandBody::GetAvailableModels => "get_available_models",
        CommandBody::SetModel { .. } => "set_model",
        CommandBody::CycleModel => "cycle_model",
        CommandBody::GetAvailableThinkingLevels => "get_available_thinking_levels",
        CommandBody::SetThinkingLevel { .. } => "set_thinking_level",
        CommandBody::CycleThinkingLevel => "cycle_thinking_level",
        CommandBody::GetCommands => "get_commands",
        CommandBody::GetSessionStats => "get_session_stats",
        CommandBody::GetForkMessages => "get_fork_messages",
        CommandBody::GetEntries { .. } => "get_entries",
        CommandBody::Fork { .. } => "fork",
        CommandBody::CloneSession => "clone_session",
        CommandBody::AuthList => "auth.list",
        CommandBody::AuthStatus { .. } => "auth.status",
        CommandBody::AuthLogin { .. } => "auth.login",
        CommandBody::AuthLogout { .. } => "auth.logout",
        CommandBody::AuthCancel { .. } => "auth.cancel",
        CommandBody::QuotaList { .. } => "quota.list",
        CommandBody::Raw(_) => "raw",
    }
}

pub fn hide_console(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // `CREATE_NO_WINDOW`: run the child without a visible console.
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// 本项目的 harness 可执行文件路径。
///
/// **改造说明**：上游返回 `pi` 的路径，供「一次性子进程」类调用使用
/// （扩展安装、更新探测、提交信息生成等）。
///
/// 改造后运行时是内嵌的 harness 内核，**这些调用点绝大多数应当消失**：
/// pi 的扩展、配额、版本更新在 harness 里都没有对应机制。返回自身路径
/// 会让它们 spawn 自己 —— 子进程启动后又走同样的探测，形成无限递归
/// （实测会把 Dock 塞满几十个图标）。
///
/// 因此这里返回一个**不存在的路径**：调用点拿到它必然启动失败，
/// 从而如实暴露「这个能力不存在」，而不是自我递归或假装成功。
/// 仍然需要一次性文本生成的调用点（提交信息、issue 草稿）
/// 已在 `commit_message::run_pi` / `issue_message::run_pi` 里改成直接调内核。
pub fn pi_binary() -> String {
    String::from("/nonexistent/harness-has-no-host-binary")
}

/// Resolve the `pi` executable to spawn.
///
/// Every plausible `pi` launcher, most-preferred first: `PI_BIN`, then each
/// `PATH` dir, then the platform search dirs. Public so a caller that needs to
/// *inspect* the install (the RPC patcher) can, not just spawn it.
pub fn pi_bin_candidates() -> Vec<PathBuf> {
    let mut bins: Vec<PathBuf> = Vec::new();
    if let Ok(bin) = std::env::var(PI_BIN_ENV) {
        if !bin.is_empty() {
            push_unique(&mut bins, PathBuf::from(bin));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in PI_BIN_NAMES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    push_unique(&mut bins, candidate);
                }
            }
        }
    }
    for dir in search_dirs() {
        for name in PI_BIN_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                push_unique(&mut bins, candidate);
            }
        }
    }
    bins
}

fn push_unique(bins: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !bins.contains(&candidate) {
        bins.push(candidate);
    }
}

/// Resolve the installed pi **package root** — the directory whose
/// `package.json` is named [`PI_PACKAGE_NAME`].
///
/// Both install layouts Orbit sees are handled, on every platform:
///
/// - **npm/pnpm/bun globals** — walk up from the launcher's real path to the
///   enclosing `node_modules/@earendil-works/pi-coding-agent`.
/// - **pi's managed installer** — the launcher is a script at `<agent>/bin/pi`,
///   so the package sits under
///   `<agent>/install/releases/<version>/node_modules/@earendil-works/pi-coding-agent`.
///   `PI_MANAGED_INSTALL_ROOT` and `install/current-version` are honoured.
///
/// `PI_PACKAGE_ROOT` wins when set. Otherwise every candidate launcher is tried
/// (`PI_BIN`, `PATH`, then the platform search dirs), then the managed install
/// roots directly. Callers persist a success so later launches skip the search.
pub fn pi_package_root() -> Result<PathBuf, String> {
    if let Some(root) = env_package_root()? {
        return Ok(root);
    }

    let mut launchers = Vec::new();
    for bin in pi_bin_candidates() {
        if let Some(root) = pi_package_root_from(&bin) {
            return Ok(root);
        }
        launchers.push(bin);
    }
    for install in install_root_candidates() {
        if let Some(root) = managed_package_root(&install) {
            return Ok(root);
        }
    }

    Err(if launchers.is_empty() {
        "pi package not found (no pi on PATH or in the usual install dirs)".to_string()
    } else {
        let looked = launchers
            .iter()
            .take(3)
            .map(|bin| bin.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("pi package not found from {looked}")
    })
}

/// The pi package root for a known path: the root itself, or a `pi` launcher
/// anywhere inside it. `None` when the path is not a pi install. Validates a
/// path saved by a previous launch without trusting it blindly.
pub fn pi_package_root_from(path: &Path) -> Option<PathBuf> {
    if is_pi_package(path) {
        return Some(path.to_path_buf());
    }
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(root) = walk_up_to_package_root(&real) {
        return Some(root);
    }
    // pi's managed launcher: `<agent>/bin/pi` → `<agent>/install/releases/<v>/…`.
    let dir = if real.is_dir() {
        real.as_path()
    } else {
        real.parent()?
    };
    dir.ancestors()
        .take(3)
        .find_map(|ancestor| managed_package_root(&ancestor.join("install")))
}

/// A `PI_PACKAGE_ROOT` value, validated. An explicitly set but invalid path is
/// an error rather than a silent fall-through, so a typo is visible.
fn env_package_root() -> Result<Option<PathBuf>, String> {
    let Some(value) = std::env::var_os(PI_PACKAGE_ROOT_ENV).filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    pi_package_root_from(&path)
        .map(Some)
        .ok_or_else(|| format!("{} is not a pi package root", path.display()))
}

/// Walk up from `start` to a directory whose `package.json` names the pi
/// package. Handles the npm/pnpm/bun global layout.
///
/// Windows npm exposes `pi.cmd` as a *sibling* of `node_modules` rather than a
/// symlink into the package, so each ancestor's own `node_modules` is checked
/// as well as the ancestor itself.
fn walk_up_to_package_root(start: &Path) -> Option<PathBuf> {
    // A path that is not on disk is not a pi install. Without this, any
    // nonexistent `<prefix>/pi` would resolve through `<prefix>/node_modules`
    // on the nested check below.
    if !start.exists() {
        return None;
    }
    let start = if start.is_dir() {
        start
    } else {
        start.parent()?
    };
    let mut dir = start.to_path_buf();
    for _ in 0..12 {
        if is_pi_package(&dir) {
            return Some(dir);
        }
        let nested = dir.join("node_modules").join(PI_PACKAGE_NAME);
        if is_pi_package(&nested) {
            return Some(nested);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

fn is_pi_package(dir: &Path) -> bool {
    fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|name| name == PI_PACKAGE_NAME)
}

/// The package root inside a managed install dir (`…/.pi/agent/install`),
/// preferring the recorded `current-version` and falling back to the newest
/// release still carrying the pi package.
fn managed_package_root(install: &Path) -> Option<PathBuf> {
    let package = |version: &str| {
        install
            .join("releases")
            .join(version)
            .join("node_modules")
            .join(PI_PACKAGE_NAME)
    };
    if let Ok(raw) = fs::read_to_string(install.join("current-version")) {
        let version = raw.trim();
        if !version.is_empty() {
            let root = package(version);
            if is_pi_package(&root) {
                return Some(root);
            }
        }
    }
    let mut versions: Vec<String> = fs::read_dir(install.join("releases"))
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    versions.sort_by_key(|version| version_key(version));
    versions.iter().rev().find_map(|version| {
        let root = package(version);
        is_pi_package(&root).then_some(root)
    })
}

/// Managed-install roots to probe when no launcher could be resolved.
fn install_root_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(env) = std::env::var_os("PI_MANAGED_INSTALL_ROOT").filter(|value| !value.is_empty())
    {
        roots.push(PathBuf::from(env));
    }
    if let Some(home) = home_dir() {
        let root = home.join(".pi").join("agent").join("install");
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// `1.0.10` sorts after `1.0.9`, unlike a plain string compare.
fn version_key(version: &str) -> Vec<u64> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// Install dirs probed for `pi` when it isn't on `PATH`: the absolute
/// [`PI_SEARCH_DIRS`], the home-relative [`HOME_SEARCH_DIRS`], and the
/// [`fallback_bin_dirs`] (on Windows, the npm/pnpm/bun global dirs and the
/// registry PATH).
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = PI_SEARCH_DIRS.iter().map(PathBuf::from).collect();
    if let Some(home) = home_dir() {
        dirs.extend(HOME_SEARCH_DIRS.iter().map(|sub| home.join(sub)));
    }
    dirs.extend(fallback_bin_dirs());
    dirs
}

/// Install dirs a GUI launch may not see on its own `PATH`, beyond the
/// Unix-style dirs every caller already probes. On Windows: the package- and
/// version-manager dirs, then the user's *current* PATH read from the
/// registry. A process keeps the PATH it was launched with, so a tool
/// installed (or a PATH entry added) after Orbit started would otherwise stay
/// invisible to the setup page's Refresh (issue #124). Empty elsewhere.
pub fn fallback_bin_dirs() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut dirs = windows_search_dirs();
        dirs.extend(registry_path_dirs());
        dirs
    }
    #[cfg(not(windows))]
    Vec::new()
}

#[cfg(windows)]
fn windows_search_dirs() -> Vec<PathBuf> {
    windows_search_dirs_from(|name| std::env::var_os(name))
}

/// npm and friends put their global launchers under `%APPDATA%` or
/// `%LOCALAPPDATA%` on Windows, none of which appear in [`HOME_SEARCH_DIRS`];
/// Node.js itself lands wherever its Windows installer or package manager
/// puts it. Takes the variable lookup so tests can fake a Windows host.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_search_dirs_from(var: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let var = |name: &str| {
        var(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let mut dirs = Vec::new();
    if let Some(appdata) = var("APPDATA") {
        dirs.push(appdata.join("npm"));
    }
    if let Some(local) = var("LOCALAPPDATA") {
        dirs.push(local.join("pnpm"));
        dirs.push(local.join("Volta").join("bin"));
        dirs.push(local.join("Yarn").join("bin"));
        dirs.push(local.join("mise").join("shims"));
        dirs.push(local.join("Programs").join("bun").join("bin"));
    }
    if let Some(profile) = var("USERPROFILE") {
        dirs.push(profile.join(".bun").join("bin"));
        dirs.push(profile.join(".volta").join("bin"));
    }
    // Scoop puts `apps\nodejs\current` itself on the user PATH rather than
    // shimming `node.exe`, for both user and global (`-g`) installs.
    let scoop = var("SCOOP").or_else(|| var("USERPROFILE").map(|p| p.join("scoop")));
    let scoop_global = var("SCOOP_GLOBAL").or_else(|| var("ProgramData").map(|p| p.join("scoop")));
    for root in [scoop, scoop_global].into_iter().flatten() {
        dirs.push(root.join("shims"));
        for app in ["nodejs", "nodejs-lts"] {
            dirs.push(root.join("apps").join(app).join("current"));
        }
    }
    // Chocolatey shims every package's executables into one bin dir.
    let choco =
        var("ChocolateyInstall").or_else(|| var("ProgramData").map(|p| p.join("chocolatey")));
    if let Some(choco) = choco {
        dirs.push(choco.join("bin"));
    }
    // nvm-windows links the active version here; it defaults to the same
    // `Program Files\nodejs` the official installer and winget use.
    if let Some(symlink) = var("NVM_SYMLINK") {
        dirs.push(symlink);
    }
    if let Some(program_files) = var("ProgramFiles") {
        dirs.push(program_files.join("nodejs"));
    }
    // fnm on Windows keeps `node.exe` directly in each version's
    // `installation` dir, with no `bin` beneath it.
    let fnm = var("FNM_DIR").or_else(|| var("APPDATA").map(|p| p.join("fnm")));
    if let Some(fnm) = fnm {
        dirs.extend(newest_version_bin(
            &fnm.join("node-versions"),
            Path::new("installation"),
        ));
    }
    dirs
}

/// The machine and user `Path` as the registry holds them *now*, system
/// first as Windows builds a new process's PATH. `%VAR%` references are
/// expanded against the process environment, then against the registry's own
/// variables — one a package manager just added (Scoop's `SCOOP`) is not in
/// a process environment that predates it.
#[cfg(windows)]
fn registry_path_dirs() -> Vec<PathBuf> {
    use windows_registry::{CURRENT_USER, LOCAL_MACHINE};
    const SYSTEM_ENVIRONMENT: &str =
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    let system = LOCAL_MACHINE.open(SYSTEM_ENVIRONMENT).ok();
    let user = CURRENT_USER.open("Environment").ok();
    let lookup = |name: &str| {
        std::env::var(name)
            .ok()
            .or_else(|| user.as_ref()?.get_string(name).ok())
            .or_else(|| system.as_ref()?.get_string(name).ok())
    };
    [system.as_ref(), user.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|key| key.get_string("Path").ok())
        .flat_map(|raw| std::env::split_paths(&expand_env_refs(&raw, lookup)).collect::<Vec<_>>())
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect()
}

/// Expand `%NAME%` references the way Windows expands a `REG_EXPAND_SZ`
/// value. An unknown name, an empty `%%` or an unpaired `%` is kept as
/// written.
#[cfg_attr(not(windows), allow(dead_code))]
fn expand_env_refs(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            expanded.push_str(&rest[start..]);
            return expanded;
        };
        let name = &after[..end];
        match (!name.is_empty()).then(|| lookup(name)).flatten() {
            Some(found) => expanded.push_str(&found),
            None => expanded.push_str(&rest[start..start + end + 2]),
        }
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);
    expanded
}

/// The user's home directory (`HOME`, falling back to `USERPROFILE`).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// The PATH handed to a pi child: the resolved binary's dir plus common
/// install dirs prepended to whatever PATH the parent process has. Public so
/// every direct pi spawn (updater, version probe) resolves `node` from a
/// bundled `.app`, not just the RPC session.
pub fn augmented_path(bin_dir: Option<&Path>) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = bin_dir {
        if !dir.as_os_str().is_empty() {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs.extend(PATH_EXTRA_DIRS.iter().map(PathBuf::from));
    if let Some(home) = home_dir() {
        dirs.extend(home_bin_dirs(&home));
    }
    #[cfg(windows)]
    dirs.extend(windows_search_dirs());
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    // Last, so the PATH the app was launched with keeps precedence: a `node`
    // added to the user PATH since then (Scoop, issue #124) still resolves.
    #[cfg(windows)]
    dirs.extend(registry_path_dirs());
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// The home-relative dirs a GUI launch cannot see: the same install dirs
/// [`pi_bin_candidates`] probes, plus the `node` a version manager keeps
/// outside `PATH`. pi's launcher ends in `#!/usr/bin/env node`, so a child
/// PATH without one of these dies with `env: node: No such file or
/// directory` — instantly, and with nothing on stdout (issue #81).
fn home_bin_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = HOME_SEARCH_DIRS.iter().map(|sub| home.join(sub)).collect();
    // pi's managed installer ships its own node. Its launcher prepends that
    // dir itself, but every other pi invocation (update, mcp probe, patches)
    // needs it on PATH too.
    dirs.push(home.join(".local/share/pi-node/current/bin"));
    // nvm and fnm keep node under a version directory, not a stable path.
    // Newest first: an old node cannot run a current pi.
    dirs.extend(newest_version_bin(
        &home.join(".nvm/versions/node"),
        Path::new("bin"),
    ));
    for root in [
        home.join("Library/Application Support/fnm/node-versions"),
        home.join(".local/share/fnm/node-versions"),
    ] {
        dirs.extend(newest_version_bin(&root, Path::new("installation/bin")));
    }
    dirs.retain(|dir| dir.is_dir());
    dirs
}

/// The `bin` dir of the newest version under a version-manager root, when the
/// root exists. Versions are compared numerically: lexically `v9` sorts above
/// `v24`.
fn newest_version_bin(root: &Path, suffix: &Path) -> Option<PathBuf> {
    let mut newest: Option<(Vec<u64>, PathBuf)> = None;
    for entry in fs::read_dir(root).ok()?.flatten() {
        let version = version_key(&entry.file_name().to_string_lossy());
        if version.is_empty() {
            continue;
        }
        let dir = entry.path().join(suffix);
        if !dir.is_dir() {
            continue;
        }
        if newest.as_ref().is_none_or(|(best, _)| version > *best) {
            newest = Some((version, dir));
        }
    }
    newest.map(|(_, dir)| dir)
}

#[cfg(test)]
mod augmented_path_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_home() -> PathBuf {
        let unique = format!(
            "orbit-rpc-home-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(unique);
        fs::create_dir_all(&home).unwrap();
        home
    }

    /// A bundled `.app` cannot see nvm/fnm: their node lives under a version
    /// dir no `PATH` entry points at. pi's launcher is `#!/usr/bin/env node`,
    /// so the child PATH has to carry the newest one or pi dies before it can
    /// say why (issue #81).
    #[test]
    fn the_node_a_version_manager_hides_is_on_the_path() {
        let home = scratch_home();
        for version in ["v9.11.2", "v24.1.0"] {
            fs::create_dir_all(home.join(".nvm/versions/node").join(version).join("bin")).unwrap();
        }
        fs::create_dir_all(
            home.join("Library/Application Support/fnm/node-versions/v22.4.1/installation/bin"),
        )
        .unwrap();

        let dirs = home_bin_dirs(&home);
        assert!(
            dirs.contains(&home.join(".nvm/versions/node/v24.1.0/bin")),
            "the newest nvm node is offered: {dirs:?}"
        );
        assert!(
            !dirs.contains(&home.join(".nvm/versions/node/v9.11.2/bin")),
            "v9 sorts above v24 lexically but must not win: {dirs:?}"
        );
        assert!(dirs.contains(
            &home.join("Library/Application Support/fnm/node-versions/v22.4.1/installation/bin")
        ));
        let _ = fs::remove_dir_all(&home);
    }

    /// A Scoop Node.js is never shimmed: Scoop puts `apps\nodejs\current` on
    /// the user PATH instead, which a GUI launch may not have (issue #124).
    /// Chocolatey, the official installer and fnm each have their own dir.
    #[test]
    fn windows_node_installs_are_searched() {
        let home = scratch_home();
        let fnm = home.join("fnm/node-versions");
        for version in ["v20.11.0", "v26.13.1"] {
            fs::create_dir_all(fnm.join(version).join("installation")).unwrap();
        }
        let profile = PathBuf::from(r"C:\Users\me");
        let program_data = PathBuf::from(r"C:\ProgramData");
        let vars = |name: &str| -> Option<OsString> {
            match name {
                "USERPROFILE" => Some(profile.clone().into()),
                "ProgramData" => Some(program_data.clone().into()),
                "ProgramFiles" => Some(r"C:\Program Files".into()),
                "FNM_DIR" => Some(home.join("fnm").into()),
                // Set but empty is the same as unset.
                "SCOOP_GLOBAL" => Some(OsString::new()),
                _ => None,
            }
        };
        let dirs = windows_search_dirs_from(vars);
        for expected in [
            profile
                .join("scoop")
                .join("apps")
                .join("nodejs")
                .join("current"),
            profile
                .join("scoop")
                .join("apps")
                .join("nodejs-lts")
                .join("current"),
            profile.join("scoop").join("shims"),
            program_data
                .join("scoop")
                .join("apps")
                .join("nodejs")
                .join("current"),
            program_data.join("chocolatey").join("bin"),
            PathBuf::from(r"C:\Program Files").join("nodejs"),
            fnm.join("v26.13.1").join("installation"),
        ] {
            assert!(dirs.contains(&expected), "{expected:?} missing: {dirs:?}");
        }
        assert!(!dirs.contains(&fnm.join("v20.11.0").join("installation")));

        // An explicit `SCOOP` root replaces the default under the profile.
        let custom =
            |name: &str| -> Option<OsString> { (name == "SCOOP").then(|| r"D:\scoop".into()) };
        let dirs = windows_search_dirs_from(custom);
        assert!(dirs.contains(
            &PathBuf::from(r"D:\scoop")
                .join("apps")
                .join("nodejs")
                .join("current")
        ));
        assert!(
            !dirs.iter().any(|dir| dir.starts_with(&profile)),
            "{dirs:?}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// Registry PATH entries are `REG_EXPAND_SZ`: `%SCOOP%` and friends have
    /// to be expanded before a dir can be probed, and anything unresolvable
    /// is kept verbatim rather than silently mangled.
    #[test]
    fn env_refs_expand_like_windows() {
        let lookup = |name: &str| match name {
            "SCOOP" => Some(r"C:\Users\me\scoop".to_string()),
            "SystemRoot" => Some(r"C:\Windows".to_string()),
            _ => None,
        };
        assert_eq!(
            expand_env_refs(r"%SCOOP%\apps\nodejs\current;%SystemRoot%\system32", lookup),
            r"C:\Users\me\scoop\apps\nodejs\current;C:\Windows\system32"
        );
        assert_eq!(
            expand_env_refs(r"%MISSING%\bin;%SCOOP%\shims", lookup),
            r"%MISSING%\bin;C:\Users\me\scoop\shims"
        );
        assert_eq!(expand_env_refs("100%", lookup), "100%");
        assert_eq!(expand_env_refs("a%%b", lookup), "a%%b");
        assert_eq!(expand_env_refs(r"C:\plain", lookup), r"C:\plain");
    }

    /// Only real dirs go on a child's PATH, and a home with no version
    /// manager is not an error.
    #[test]
    fn absent_install_dirs_are_skipped() {
        let home = scratch_home();
        assert!(home_bin_dirs(&home).is_empty());
        let _ = fs::remove_dir_all(&home);
    }
}

#[cfg(test)]
mod package_root_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir() -> PathBuf {
        let unique = format!(
            "orbit-rpc-package-root-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pi_package(root: &Path) {
        fs::create_dir_all(root.join("dist").join("bundle")).unwrap();
        fs::write(
            root.join("package.json"),
            format!(r#"{{"name":"{PI_PACKAGE_NAME}","version":"0.0.0"}}"#),
        )
        .unwrap();
        fs::write(
            root.join("dist").join("bundle").join("cli.js"),
            "#!/usr/bin/env node",
        )
        .unwrap();
    }

    #[test]
    fn walks_up_to_an_npm_global_package() {
        let dir = scratch_dir();
        let root = dir
            .join("node_modules")
            .join("@earendil-works")
            .join("pi-coding-agent");
        write_pi_package(&root);
        let bin = root.join("dist").join("bundle").join("cli.js");

        assert_eq!(
            pi_package_root_from(&bin),
            Some(fs::canonicalize(&root).unwrap())
        );
        assert_eq!(pi_package_root_from(&dir.join("missing")), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolves_a_managed_install_from_its_launcher() {
        // pi's managed layout: the launcher at <agent>/bin/pi is a script, not
        // inside node_modules, so the package is only reachable via
        // <agent>/install/current-version.
        let dir = scratch_dir();
        let agent = dir.join("agent");
        let bin = agent.join("bin").join("pi");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, "#!/bin/sh\n").unwrap();

        let root = agent
            .join("install")
            .join("releases")
            .join("1.0.4")
            .join("node_modules")
            .join(PI_PACKAGE_NAME);
        write_pi_package(&root);
        fs::write(agent.join("install").join("current-version"), "1.0.4\n").unwrap();

        assert_eq!(
            pi_package_root_from(&bin),
            Some(fs::canonicalize(&root).unwrap())
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn managed_uses_current_version_then_newest_release() {
        let dir = scratch_dir();
        let install = dir.join("install");
        fs::create_dir_all(&install).unwrap();
        let install = fs::canonicalize(&install).unwrap();
        let root = |version: &str| {
            install
                .join("releases")
                .join(version)
                .join("node_modules")
                .join(PI_PACKAGE_NAME)
        };
        write_pi_package(&root("1.0.3"));
        write_pi_package(&root("1.0.10"));

        // No current-version: the newest release wins, compared numerically so
        // `1.0.10` beats `1.0.9` rather than sorting as a string.
        assert_eq!(managed_package_root(&install), Some(root("1.0.10")));

        // current-version wins when present.
        fs::write(install.join("current-version"), "1.0.3").unwrap();
        assert_eq!(managed_package_root(&install), Some(root("1.0.3")));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn version_key_orders_numerically() {
        let mut versions = vec![
            "1.0.9".to_string(),
            "1.0.10".to_string(),
            "1.0.2".to_string(),
        ];
        versions.sort_by_key(|version| version_key(version));
        assert_eq!(versions, vec!["1.0.2", "1.0.9", "1.0.10"]);
    }
}
