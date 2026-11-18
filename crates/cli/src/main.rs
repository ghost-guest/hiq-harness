//! CLI 入口。把七层串起来。
//!
//! 当前可运行的部分：日志、循环骨架、守卫、预算、记忆、持久化、编排、插件注册表。
//! 模型调用与工具执行通过 trait 注入，实际对接留待下一步。

use anyhow::Result;
use clap::{Parser, Subcommand};
use harness_cli::durable::{self, DurableSession};
use harness_cli::{
    exec, host, host_service, llm, memory as memory, rest, routes, runner, server, sop, sse, tools,
};
use harness_context::effort::Effort;
use harness_context::{validate_summary, BudgetPolicy, SummaryDefect, ToolResultTier};
use harness_core::event::{EventKind, EventLog, Projection};
use harness_guards::{GuardSet, PostCallVerdict, PreCallVerdict, Signature};
use harness_memory::{ArchiveReason, MemoryKind, MemoryStore, SleepRun};
use harness_orchestrate::{next_batch, Node, RunState, Schedule, Workflow};
use harness_persist::{
    resume, CheckpointStore, LeaseManager, MemoryIndex, MemoryRef, ReceiptLedger,
};
use harness_plugin::{
    ApprovalChain, CallContext, Capability, ChainVerdict, PluginManifest, PluginScope, Registry,
    Verdict,
};
use std::path::PathBuf;


#[derive(Parser)]
#[command(name = "harness", version, about = "personal-AgentHarness")]
struct Cli {
    /// 数据目录（日志、会话、记忆都放这里）
    #[arg(long, global = true, default_value = ".harness")]
    dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum PluginCmd {
    /// 从目录发现插件并列出
    List {
        /// 插件目录
        #[arg(default_value = "plugins")]
        path: PathBuf,
    },
    /// 启动插件并调用它的工具
    Call {
        /// 插件目录
        #[arg(default_value = "plugins")]
        path: PathBuf,
        /// 工具名
        tool: String,
        /// 输入
        #[arg(default_value = "")]
        input: String,
    },
}

#[derive(Subcommand)]
enum MemCmd {
    /// 查看记忆统计
    Stats,
    /// 手动记住一条
    Add {
        content: String,
        /// fact / decision / constraint / preference / lesson
        #[arg(long, default_value = "fact")]
        kind: String,
    },
    /// 搜索记忆
    Search { query: String },
    /// 遗忘一条（按内容精确匹配）
    Forget { content: String },
}

#[derive(Subcommand)]
enum Cmd {
    /// 初始化工作区
    Init,
    /// 追加一条消息并回放
    Say {
        text: String,
        #[arg(long, default_value = ".harness")]
        dir: PathBuf,
    },
    /// 回放当前投影
    Replay {
        #[arg(long, default_value = ".harness")]
        dir: PathBuf,
    },
    /// 守卫自检
    Selftest,
    /// 全层演示
    Demo,
    /// 启动桌面界面（本地服务 + 自动打开浏览器）
    Desktop {
        /// 监听端口
        #[arg(long, default_value_t = 7788)]
        port: u16,
        /// 允许局域网访问（手机连接需要）
        #[arg(long, default_value_t = false)]
        lan: bool,
    },
    /// 启动 host 服务（供桌面与手机连接）
    Serve {
        /// 监听端口
        #[arg(long, default_value_t = 7788)]
        port: u16,
        /// 绑定地址。默认只绑回环；手机同局域网可改 0.0.0.0
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
    /// 加载并探测插件
    Plugin {
        #[command(subcommand)]
        action: PluginCmd,
    },
    /// 运行一个 SOP（演示内置流程）
    Sop {
        /// 流程名：build-test / fix-bug
        #[arg(default_value = "build-test")]
        name: String,
        #[arg(long, default_value = ".")]
        workdir: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:3425/v1")]
        base_url: String,
        #[arg(long, default_value = "magpie")]
        api_key: String,
        #[arg(long, default_value = "auto/deepseek-v4.1-flash")]
        model: String,
    },
    /// 管理项目记忆
    Mem {
        #[command(subcommand)]
        action: MemCmd,
    },
    /// 查看检查点并尝试恢复
    Resume {
        #[arg(long, default_value = ".harness")]
        dir: PathBuf,
    },
    /// 查看未决副作用
    Pending {
        #[arg(long, default_value = ".harness")]
        dir: PathBuf,
    },
    /// 裁决未决副作用（kiso 的 rerun / abandon）
    Resolve {
        /// 执行 id，或用 "all" 处理全部
        exec_id: String,
        /// rerun（允许重跑）或 abandon（不再重跑）
        #[arg(long, default_value = "abandon")]
        verdict: String,
        #[arg(long, default_value = ".harness")]
        dir: PathBuf,
    },
    /// 测试模型连通性
    Ping {
        #[arg(long, default_value = "http://127.0.0.1:3425/v1")]
        base_url: String,
        #[arg(long, default_value = "magpie")]
        api_key: String,
        #[arg(long, default_value = "auto/deepseek-v4.1-flash")]
        model: String,
    },
    /// 真实跑一个任务
    Run {
        /// 任务描述
        task: String,
        /// 工作区根目录
        #[arg(long, default_value = ".")]
        workdir: PathBuf,
        /// 日志目录
        #[arg(long, default_value = ".harness")]
        dir: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:3425/v1")]
        base_url: String,
        #[arg(long, default_value = "magpie")]
        api_key: String,
        #[arg(long, default_value = "auto/deepseek-v4.1-flash")]
        model: String,
        /// 最大步数，0 表示无上限
        #[arg(long, default_value_t = 12)]
        max_steps: u32,
        /// 请求的推理强度：none/minimal/low/medium/high/xhigh/max
        #[arg(long, default_value = "high")]
        effort: String,
        /// provider 实际支持的档位（逗号分隔）。留空表示未知、原样透传。
        /// 例：--supported low,high,max
        #[arg(long, default_value = "low,high,max")]
        supported: String,
        /// 更早降级工具结果（摘要 500 / 磁盘 1500 字符），
        /// 减少进入上下文的新 token，从而减少缓存 miss 的规模
        #[arg(long, default_value_t = false)]
        clip_aggressive: bool,
    },
}

fn log_path(dir: &PathBuf) -> PathBuf {
    dir.join("events.jsonl")
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let dir = cli.dir;
    match cli.cmd {
        Cmd::Init => {
            let dir = PathBuf::from(".harness");
            std::fs::create_dir_all(&dir)?;
            let mut log = EventLog::open(log_path(&dir))?;
            log.commit()?;
            println!("initialized: {}", dir.display());
        }

        Cmd::Say { text, dir } => {
            let mut log = EventLog::open(log_path(&dir))?;
            log.append(EventKind::UserMessage { text });
            log.commit()?;
            println!("appended. replay:");
            for m in Projection::new(&EventLog::read_all(log_path(&dir))?).messages() {
                println!("  {}: {}", role(&m), first_line(&m.text));
            }
        }

        Cmd::Replay { dir } => {
            let events = EventLog::read_all(log_path(&dir))?;
            let proj = Projection::new(&events);
            println!(
                "{} events, {} projected messages",
                events.len(),
                proj.messages().len()
            );
            for m in proj.messages() {
                println!("  {}: {}", role(&m), first_line(&m.text));
            }
            if proj.has_unresolved_uncertain() {
                println!("!! 存在未决的 uncertain 副作用，需人工裁决");
            }
        }

        Cmd::Desktop { port, lan } => {
            use harness_core::wire::EnvelopeKind;

            // 桌面模式：绑 0.0.0.0 让手机可连，否则只绑回环
            let bind = if lan {
                "0.0.0.0".to_string()
            } else {
                "127.0.0.1".into()
            };
            let state = host_service::shared();
            let cfg = server::ServeConfig {
                bind: bind.clone(),
                port,
            };
            let st = state.clone();

            let url = format!("http://127.0.0.1:{port}/");
            println!("hiq-harness 桌面版");
            println!("  界面: {url}");
            if lan {
                // 打印局域网地址，方便手机输入
                if let Ok(ip) = local_ip() {
                    println!("  手机访问: http://{ip}:{port}/");
                }
            } else {
                println!("  手机连接请加 --lan（监听局域网）");
            }
            println!();

            let srv = server::Server::serve(cfg, st, move |env, from| {
                let mut g = state.lock().unwrap();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let env = env.stamp_src(from);
                let is_hello = env.kind == EnvelopeKind::Hello;
                let reply = host_service::handle(env, from, &mut g.registry, now);
                if is_hello {
                    if let Some(r) = &reply {
                        if let Some(p) = &r.payload {
                            if let Some(id) = p.get("device_id").and_then(|v| v.as_str()) {
                                g.registry.touch(id, now);
                            }
                        }
                    }
                }
                reply
            })?;

            println!("已就绪：{}", srv.addr());
            open_browser(&url);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }

        Cmd::Serve { port, bind } => {
            use harness_core::wire::EnvelopeKind;
            let state = host_service::shared();
            let cfg = server::ServeConfig {
                bind: bind.clone(),
                port,
            };
            let st = state.clone();

            println!("host 服务启动中…");
            println!("  绑定: {bind}:{port}");
            println!("  HTTP: http://{bind}:{port}/health");
            println!("  WS:   ws://{bind}:{port}/ws");
            if bind == "127.0.0.1" {
                println!("  注意：只绑回环，手机无法直连。同局域网请用 --bind 0.0.0.0");
            }
            println!("\n按 Ctrl+C 停止。\n");

            let srv = server::Server::serve(cfg, st, move |env, from| {
                let mut g = state.lock().unwrap();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                // 服务端重写 src，防伪造
                let env = env.stamp_src(from);
                let is_hello = env.kind == EnvelopeKind::Hello;
                let reply = host_service::handle(env, from, &mut g.registry, now);
                if is_hello {
                    if let Some(r) = &reply {
                        if let Some(p) = &r.payload {
                            if let Some(id) = p.get("device_id").and_then(|v| v.as_str()) {
                                g.registry.touch(id, now);
                            }
                        }
                    }
                }
                reply
            })?;

            println!("已就绪：{}", srv.addr());
            // 阻塞等 Ctrl+C
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }

        Cmd::Plugin { action } => {
            use harness_plugin::{PluginScope, Registry};
            match action {
                PluginCmd::List { path } => {
                    let mut reg = Registry::new();
                    let n = host::discover_and_register(&mut reg, &path, PluginScope::User)?;
                    println!("发现 {n} 个插件（目录 {}）", path.display());
                    for p in reg.ordered() {
                        println!(
                            "  {} v{} [{}] 能力={:?}",
                            p.manifest.name,
                            p.manifest.version,
                            format!("{:?}", p.scope).to_lowercase(),
                            p.manifest.capabilities
                        );
                        for t in &p.manifest.tools {
                            println!("    工具: {}（{}）", t.name, t.description);
                        }
                    }
                    // 启动它们，验证「加载必须大声」
                    let procs = host::load_plugins(&reg, &path)?;
                    println!("\n成功启动 {} 个插件进程。", procs.len());
                }
                PluginCmd::Call { path, tool, input } => {
                    let mut reg = Registry::new();
                    host::discover_and_register(&mut reg, &path, PluginScope::User)?;
                    let mut procs = host::load_plugins(&reg, &path)?;
                    let Some(proc) = procs.first_mut() else {
                        anyhow::bail!("没有可用的插件");
                    };
                    let info = proc.describe()?;
                    println!("插件: {info}");
                    let out = proc.call(
                        "tool.call",
                        serde_json::json!({"name": tool, "input": input}),
                    )?;
                    println!("调用 {tool} → {out}");
                }
            }
        }

        Cmd::Sop {
            name,
            workdir,
            base_url,
            api_key,
            model,
        } => {
            use harness_orchestrate::{Node, Workflow};
            let wf = match name.as_str() {
                "build-test" => Workflow::new("build-test", "构建并测试")
                    .add_node(Node::script("build", "构建", "cargo build 2>&1 | tail -3"))
                    .add_node(Node::script("test", "测试", "cargo test 2>&1 | tail -5"))
                    .add_edge("build", "test"),
                "fix-bug" => Workflow::new("fix-bug", "修复并验证")
                    .add_node(Node::agent("locate", "定位", "debugger"))
                    .add_node(Node::agent("fix", "修复", "coder"))
                    .add_node(Node::script("verify", "验证", "echo verified"))
                    .add_edge("locate", "fix")
                    .add_edge("fix", "verify"),
                other => anyhow::bail!("未知流程 {other}（可用 build-test / fix-bug）"),
            };

            let workdir = workdir.canonicalize().unwrap_or(workdir);
            let mut log = EventLog::open(log_path(&dir))?;
            let mut session = durable::DurableSession::open(&dir)?;
            let mut memory = memory::ProjectMemory::open(&dir)?;
            let client = llm::OpenAiClient {
                base_url,
                api_key,
                model,
                effort: None,
            };
            let tools = tools::LocalTools::new(workdir.clone());

            println!("流程: {}（{} 个节点）", wf.name, wf.nodes.len());
            println!("工作区: {}\n", workdir.display());

            let available: std::collections::HashSet<String> = Default::default();
            let outcome = sop::run_sop(
                &wf,
                &available,
                &client,
                &tools,
                &mut log,
                &mut session,
                &mut memory,
                &workdir,
            )?;

            for line in &outcome.log {
                println!("{line}");
            }
            println!(
                "\n结果: 运行 {} ｜ 成功 {} / 失败 {} / 返工 {}",
                outcome.run_id, outcome.succeeded, outcome.failed, outcome.reworks
            );
        }

        Cmd::Mem { action } => {
            let mut m = memory::ProjectMemory::open(&dir)?;
            match action {
                MemCmd::Stats => {
                    let (active, archived) = m.stats();
                    println!(
                        "活跃记忆 {active} 条（active_count={}），归档 {archived} 条",
                        m.active_count()
                    );
                    println!("位置: {}", dir.join("memory.json").display());
                }
                MemCmd::Add { content, kind } => {
                    let k = match kind.as_str() {
                        "fact" => harness_memory::MemoryKind::Fact,
                        "decision" => harness_memory::MemoryKind::Decision,
                        "constraint" => harness_memory::MemoryKind::Constraint,
                        "preference" => harness_memory::MemoryKind::Preference,
                        "lesson" => harness_memory::MemoryKind::Lesson,
                        other => anyhow::bail!("未知类型 {other}"),
                    };
                    if m.remember(k, &content, &["project".to_string()]) {
                        println!("已记住。");
                    } else {
                        println!("被拒绝（可能含敏感信息，或未说明可迁移范围）。");
                    }
                }
                MemCmd::Search { query } => match m.recall_block(&query, 5) {
                    Some(block) => println!("{block}"),
                    None => println!("没有相关记忆。"),
                },
                MemCmd::Forget { content } => {
                    let n = m.forget(&content, harness_memory::ArchiveReason::Llm);
                    println!("遗忘 {n} 条（已进归档，可追溯）。");
                    // 连带撤销引用它的检查点（Codewhale protocol.rs:278）：
                    // 记忆不成立，基于它的检查点也不能再用。
                    let mut session = durable::DurableSession::open(&dir)?;
                    let revoked = session.on_memory_forgotten(&content)?;
                    if revoked > 0 {
                        println!("连带撤销 {revoked} 个引用它的检查点。");
                    }
                }
            }
        }

        Cmd::Selftest => selftest()?,
        Cmd::Demo => demo()?,

        Cmd::Resume { dir } => {
            let session = durable::DurableSession::open(&dir)?;
            match session.latest_checkpoint() {
                None => println!("没有检查点。"),
                Some(cp) => {
                    println!("最近检查点: {}", cp.id);
                    println!("  覆盖到 seq {}", cp.through_seq);
                    println!("  依赖 {} 条记忆", cp.memory_refs.len());
                    println!("  内容: {}", cp.payload);

                    // 尝试恢复：记忆失效会拒绝交出 state
                    let idx = harness_persist::MemoryIndex::new();
                    match session.resume(&idx) {
                        Some(r) if r.state_usable => {
                            println!("\n✓ 可恢复：state = {:?}", r.state);
                            println!("  restores_permissions = {}", r.restores_permissions);
                        }
                        Some(r) => {
                            println!("\n✗ 拒绝恢复：依赖的记忆已失效");
                            for m in &r.invalidated_memory_ids {
                                println!("    失效: {m}");
                            }
                        }
                        None => println!("无法恢复"),
                    }
                }
            }
        }

        Cmd::Pending { dir } => {
            let session = durable::DurableSession::open(&dir)?;
            let pending = session.pending_uncertain();
            if pending.is_empty() {
                println!("没有未决副作用。");
            } else {
                println!("{} 个未决副作用：", pending.len());
                for p in pending {
                    let retryable = session.can_retry(&p);
                    println!("  {p}（{}）", if retryable { "可重跑" } else { "需裁决" });
                }
                println!("\n用 harness resolve <id> --verdict rerun|abandon 裁决");
            }
        }

        Cmd::Resolve {
            exec_id,
            verdict,
            dir,
        } => {
            let mut session = durable::DurableSession::open(&dir)?;
            let v = match verdict.as_str() {
                "rerun" => durable::Verdict::Rerun,
                "abandon" => durable::Verdict::Abandon,
                other => anyhow::bail!("未知裁决 {other}，用 rerun 或 abandon"),
            };
            let targets: Vec<String> = if exec_id == "all" {
                session.pending_uncertain()
            } else {
                vec![exec_id]
            };
            for t in &targets {
                session.resolve(t, v)?;
                println!("已裁决 {t} → {verdict}");
            }
        }

        Cmd::Ping {
            base_url,
            api_key,
            model,
        } => {
            let client = llm::OpenAiClient::new(base_url.clone(), api_key, model.clone());
            println!("请求 {base_url} 模型 {model} …");
            let (text, usage) = llm::ping(&client, "用一句话回答：你是什么模型？")?;
            println!("回复: {}", text.trim());
            println!(
                "用量: prompt={:?} completion={:?} cached={:?}",
                usage.prompt_tokens, usage.completion_tokens, usage.cached_tokens
            );
        }

        Cmd::Run {
            task,
            workdir,
            dir,
            base_url,
            api_key,
            model,
            max_steps,
            effort,
            supported,
            clip_aggressive,
        } => {
            let workdir = workdir.canonicalize().unwrap_or(workdir);
            let mut log = EventLog::open(log_path(&dir))?;
            let client = llm::OpenAiClient {
                base_url,
                api_key,
                model: model.clone(),
                effort: None, // 档位改由 runner 按 provider 能力映射
            };
            let tools = tools::LocalTools::new(workdir.clone());

            println!("工作区: {}", workdir.display());
            println!("模型: {model} (effort={effort})");
            println!("任务: {task}\n");

            let requested = Effort::parse(&effort)?;
            let supported: Vec<Effort> = supported
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .filter_map(|s| Effort::parse(s).ok())
                .collect();

            let plan = harness_context::effort::plan_effort(requested, &supported);
            println!(
                "推理档位: 请求 {} → 实际 {}",
                plan.requested.as_str(),
                plan.effective.as_str()
            );
            if let Some(note) = &plan.note {
                println!("  ({note})");
            }
            println!();

            let cfg = runner::RunConfig {
                max_steps,
                effort: Some(requested),
                supported_efforts: supported,
                system: "你是一个编程助手，在给定的工作区内完成任务。\
                         用工具读写文件、执行命令。不要臆造文件内容 —— 先读再改。\
                         任务完成后，用一句话总结你做了什么。"
                    .into(),
                // 三段式阈值（Deva）：更早降到摘要/磁盘，减少进入上下文的新 token
                summary_threshold: if clip_aggressive { 500 } else { 2_000 },
                disk_threshold: if clip_aggressive { 1_500 } else { 6_000 },
                spill_dir: dir.join("tool-results"),
                // CLI 单次任务没有跨轮会话，历史为空。
                history: Vec::new(),
            };

            let mut session = durable::DurableSession::open(&dir)?;
            let mut memory = memory::ProjectMemory::open(&dir)?;
            let generation = session.acquire_lease("cli")?;
            session.check_writer("cli", generation)?;

            // 恢复提示：有未决副作用就先报出来
            let pending = session.pending_uncertain();
            if !pending.is_empty() {
                println!("⚠ 上次运行留下 {} 个未决副作用：", pending.len());
                for p in &pending {
                    println!("    {p}");
                }
                println!("  需人工裁决后才能继续。\n");
            }

            let outcome = runner::run_task(
                &client,
                &tools,
                &mut log,
                &mut session,
                &mut memory,
                &task,
                &cfg,
                &mut |_| {}, // CLI 同步输出，不需要事件流
            )?;

            println!("\n=== 结果 ===");
            if let Some(plan) = &outcome.effort {
                println!(
                    "推理档位: 请求 {} → 实际 {}{}",
                    plan.requested.as_str(),
                    plan.effective.as_str(),
                    if plan.downgraded {
                        "（已映射）"
                    } else {
                        ""
                    }
                );
            }
            println!("步数: {}", outcome.steps);
            // 任务成功 → 写入一条项目记忆（决策类）。
            // 准入在 remember 内部做：含密钥会被拒，未说明可迁移也会被拒。
            if !outcome.final_text.trim().is_empty() && outcome.steps > 0 {
                let record = format!(
                    "任务「{}」已完成（{} 步）：{}",
                    task.chars().take(60).collect::<String>(),
                    outcome.steps,
                    outcome
                        .final_text
                        .trim()
                        .chars()
                        .take(200)
                        .collect::<String>()
                );
                if memory.remember(
                    harness_memory::MemoryKind::Lesson,
                    &record,
                    &["project".to_string()],
                ) {
                    println!("已写入项目记忆（跨会话可召回）。");
                }
            }

            print!("{}", outcome.cache.report());
            if !outcome.guard_events.is_empty() {
                println!("守卫事件:");
                for e in &outcome.guard_events {
                    println!("  {e}");
                }
            }
            println!("最终回复: {}", outcome.final_text.trim());

            // 回放日志，验证持久化
            let events = EventLog::read_all(log_path(&dir))?;
            println!("\n日志: {} 条事件", events.len());
        }
    }
    Ok(())
}

fn role(m: &harness_core::event::ProjectedMessage) -> &'static str {
    use harness_core::event::Role;
    match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn first_line(s: &str) -> String {
    let l = s.lines().next().unwrap_or("");
    if l.chars().count() > 70 {
        format!("{}…", l.chars().take(70).collect::<String>())
    } else {
        l.to_string()
    }
}

// ─────────────────────────────────────────────────────────────
// 守卫自检
// ─────────────────────────────────────────────────────────────

fn selftest() -> Result<()> {
    println!("=== 守卫自检 ===\n");

    // 1. 相同失败连续 3 次 → 熔断
    let mut g = GuardSet::new();
    let s = Signature::new("bash", "pytest", "boom", "");
    for i in 1..=3 {
        let v = g.after_call(&s, false);
        if i == 3 {
            assert!(
                matches!(v, PostCallVerdict::Note { .. }),
                "第 3 次应触发熔断"
            );
        }
    }
    println!("[L2] 相同失败第 3 次 → 熔断 ok");

    // 2. 轮询（结果在变）→ 不熔断
    let mut g = GuardSet::new();
    for i in 0..20 {
        let s = Signature::new("bash", "check", &format!("running {i}%"), "");
        if let PostCallVerdict::Note { note } = g.after_call(&s, false) {
            assert!(!note.contains("连续失败"), "轮询不该触发失败熔断");
        }
    }
    println!("[L2] 轮询 20 次（结果每次不同）→ 未误杀 ok");

    // 3. 只读螺旋
    let mut g = GuardSet::new();
    let mut adv = None;
    let mut blk = None;
    for i in 1..=16 {
        match g.before_call("read_file") {
            PreCallVerdict::ProceedWithNote { .. } if adv.is_none() => adv = Some(i),
            PreCallVerdict::Block { .. } if blk.is_none() => blk = Some(i),
            _ => {}
        }
    }
    assert_eq!(adv, Some(12), "第 12 次应提示");
    assert_eq!(blk, Some(15), "第 15 次应阻塞");
    println!("[L2] 只读螺旋：第 12 次提示、第 15 次阻塞 ok");

    // 4. 写入后重置
    let mut g = GuardSet::new();
    for _ in 0..11 {
        g.before_call("read_file");
    }
    g.before_call("write_file");
    assert!(matches!(
        g.before_call("read_file"),
        PreCallVerdict::Proceed
    ));
    println!("[L2] 写入后只读计数重置 ok");

    println!("\n全部通过。");
    Ok(())
}

// ─────────────────────────────────────────────────────────────
// 全层演示
// ─────────────────────────────────────────────────────────────

fn demo() -> Result<()> {
    println!("=== 七层演示 ===\n");

    // L0 日志
    let dir = std::env::temp_dir().join(format!("harness-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("events.jsonl");

    let mut log = EventLog::open(&path)?;
    log.append(EventKind::UserMessage {
        text: "修好登录 bug".into(),
    });
    log.append(EventKind::ToolCall {
        id: "c1".into(),
        name: "read_file".into(),
        input: serde_json::json!({"path": "src/auth.rs"}),
    });
    log.append(EventKind::ToolResult {
        call_id: "c1".into(),
        outcome: harness_core::event::ToolOutcome::Ok {
            output: "fn login() {...}".into(),
        },
    });
    log.commit()?;
    let events = EventLog::read_all(&path)?;
    println!(
        "[L0] 日志 {} 条，投影 {} 条消息",
        events.len(),
        Projection::new(&events).messages().len()
    );

    // L0 压缩投影
    let mut log = EventLog::open(&path)?;
    log.append(EventKind::Compaction {
        checkpoint_id: "cp1".into(),
        summary: "前面读过 auth.rs，发现 login 没校验 token".into(),
        through_seq: 2,
        source_digest: "d1".into(),
    });
    log.append(EventKind::UserMessage {
        text: "继续".into(),
    });
    log.commit()?;
    let events = EventLog::read_all(&path)?;
    let projected = Projection::new(&events).messages();
    println!(
        "[L0] 压缩后投影 {} 条（{} 条历史折叠为摘要）",
        projected.len(),
        3
    );

    // L3 预算与摘要校验
    let mut policy = BudgetPolicy::new(200_000);
    policy.calibration.observe(120, 100); // 实际是估算的 1.2 倍
    println!(
        "[L3] 校准后 100k 估算 → {} tokens；是否该压缩={}",
        policy.calibration.calibrate(100_000),
        policy.should_compact(100_000)
    );

    let good = "## Goal\n修登录\n\n## Progress\n### Done\n- 定位问题\n\
                ### In Progress\n- 改代码\n\n## Next Steps\n1. 测试\n\n\
                ## Critical Context\n- src/auth.rs:42, cargo test 失败";
    let defects = validate_summary(good, 5_000);
    println!("[L3] 合格摘要缺陷数 = {}", defects.len());

    let echo = harness_context::SUMMARY_TEMPLATE.join("\n");
    let echo_defects = validate_summary(&echo, 5_000);
    println!(
        "[L3] 模板回显被识别 = {}",
        echo_defects.contains(&SummaryDefect::TemplateEcho)
    );

    let tier = ToolResultTier::Full {
        text: "x".repeat(2000),
    };
    let degraded = tier.degrade("一行摘要", "/tmp/r1");
    println!(
        "[L3] 三段式降级: Full → {:?}",
        std::mem::discriminant(&degraded)
    );

    // L4 记忆
    let mut ms = MemoryStore::with_quota(10);
    let acc = harness_memory::Access::full("proj");
    let scope = vec!["proj".to_string()];
    let m1 = ms.remember(&acc, MemoryKind::Fact, "项目用 postgres", &scope, 0)?;
    let m2 = ms.remember(&acc, MemoryKind::Preference, "偏好紧凑排版", &scope, 0)?;
    println!(
        "[L4] 写入 {} 条，召回『postgres』{} 条",
        ms.active_count(),
        ms.recall(&acc, "postgres", 5).len()
    );

    // 密钥拒写
    let secret_err = ms.remember(&acc, MemoryKind::Fact, "key=sk-abcdefghij", &scope, 0);
    println!("[L4] 密钥拒写 = {}", secret_err.is_err());

    // 封口
    ms.supersede(&acc, &m1, "项目用 mysql", &scope, 1)?;
    println!("[L4] 取代后活跃 {} 条（旧条封口但保留）", ms.active_count());

    // 睡眠整合
    let run: SleepRun = ms.sleep(&acc, 10, |m| {
        if m.content.contains("偏好") {
            Some(ArchiveReason::Expired)
        } else {
            None
        }
    })?;
    println!(
        "[L4] 睡眠整合：检查 {} 条，归档 {} 条（原因 {:?}）",
        run.examined,
        run.total_archived(),
        run.archived.keys().collect::<Vec<_>>()
    );
    let _ = m2;

    // L5 持久化
    let mut ledger = ReceiptLedger::new();
    ledger.begin("exec-1", 0);
    ledger.begin("exec-2", 0);
    ledger.confirm("exec-2", 1)?;
    println!("[L5] 未决 uncertain = {:?}", ledger.pending_uncertain());
    ledger.resolve_uncertain("exec-1", false, 2)?;
    println!("[L5] 裁决后未决 = {}", ledger.pending_uncertain().len());

    // 检查点 + 记忆失效
    let mut cps = CheckpointStore::new();
    let cp_id = cps.save(
        5,
        "state-payload",
        vec![MemoryRef {
            id: m1.clone(),
            revision: 1,
        }],
        0,
    );
    let mut idx = MemoryIndex::new();
    idx.set(&m1, 1);
    let r_ok = resume(cps.get(&cp_id).unwrap(), &idx);
    println!("[L5] 记忆一致时恢复可用 = {}", r_ok.state_usable);

    idx.forget(&m1);
    let r_bad = resume(cps.get(&cp_id).unwrap(), &idx);
    println!(
        "[L5] 记忆被遗忘后恢复可用 = {}，失效项 {:?}，state={:?}",
        r_bad.state_usable, r_bad.invalidated_memory_ids, r_bad.state
    );
    cps.invalidate_by_memory(&m1);
    println!("[L5] 连带撤销检查点后剩余 = {}", cps.len());

    // 租约围栏
    let mut lm = LeaseManager::new();
    let old = lm.acquire("instance-a", 0, 5)?;
    let new = lm.acquire("instance-b", 10, 100)?;
    println!(
        "[L5] 旧实例写被拒 = {}（代次 {} vs {}）",
        lm.check_writer("instance-a", old.generation).is_err(),
        old.generation,
        new.generation
    );

    // L6 编排
    let wf = Workflow::new("deliver", "交付流程")
        .add_node(Node::agent("design", "设计", "architect"))
        .add_node(Node::script("fast", "快分支", "echo fast"))
        .add_node(Node::script("slow", "慢分支", "sleep 1"))
        .add_node(Node::script("test", "测试", "cargo test"))
        .add_node(Node::human_gate("review", "评审", "检查改动"))
        .add_edge("design", "fast")
        .add_edge("design", "slow")
        .add_edge("fast", "test")
        .add_edge("slow", "test")
        .add_edge("test", "review")
        .add_requirement("git-token", "Git Token", "在设置里配置");

    let mut st = RunState::start(&wf, "run-1")?;
    println!(
        "[L6] 初始就绪 = {:?}",
        match next_batch(&st) {
            Schedule::Start(ids) => ids,
            _ => vec![],
        }
    );
    st.succeed_node("design");
    println!(
        "[L6] 设计完成后两个分支同时就绪 = {:?}",
        match next_batch(&st) {
            Schedule::Start(ids) => ids,
            _ => vec![],
        }
    );

    let missing = harness_orchestrate::check_requirements(&wf, &Default::default());
    println!(
        "[L6] 前置能力缺失（运行前暴露）= {:?}",
        missing.iter().map(|r| r.key.as_str()).collect::<Vec<_>>()
    );

    // L7 扩展
    let mut reg = Registry::new();
    let mut m = PluginManifest {
        name: "example".into(),
        version: "0.1.0".into(),
        entry: "./example".into(),
        capabilities: vec![Capability::Tools],
        tools: vec![harness_plugin::ToolSpec {
            name: "echo".into(),
            description: "原样返回".into(),
            read_only: true,
        }],
        system_prompt_append: Some("插件提示：回答简洁。".into()),
    };
    reg.register(m.clone(), PluginScope::Builtin, "builtin")?;
    let proj_override = reg.register(m.clone(), PluginScope::Project, "repo");
    println!("[L7] 项目级覆盖内建被拒 = {}", proj_override.is_err());

    m.name = "user-version".into();
    reg.register(m, PluginScope::User, "~/.harness")?;
    println!(
        "[L7] 注册 {} 个插件，工具 {} 个",
        reg.len(),
        reg.tools().len()
    );

    let mut chain = ApprovalChain::new();
    // 这条策略只对危险命令表态，其余返回 Ask（不表态）
    chain.push("danger-guard", |c: &CallContext| {
        if c.tool == "bash" && c.args.to_string().contains("rm -rf") {
            Verdict::Deny {
                reason: "禁止递归删除".into(),
            }
        } else {
            Verdict::Ask
        }
    });
    // 这条只对写文件要求审批，其余不表态
    chain.push("ask-write", |c: &CallContext| {
        if c.tool == "write_file" {
            Verdict::Ask
        } else {
            Verdict::Ask
        }
    });

    let dangerous = CallContext {
        tool: "bash".into(),
        args: serde_json::json!({"cmd": "rm -rf /"}),
    };
    match chain.decide(&dangerous, true) {
        ChainVerdict::Deny { decided_by, reason } => {
            println!("[L7] 危险命令 → Deny（由 {decided_by}）：{reason}")
        }
        other => println!("[L7] 意外：{other:?}"),
    }

    let ask = CallContext {
        tool: "write_file".into(),
        args: serde_json::json!({}),
    };
    match chain.decide(&ask, true) {
        ChainVerdict::AskHuman => println!("[L7] 写文件（有审批通道）→ AskHuman ok"),
        other => println!("[L7] 意外：{other:?}"),
    }
    match chain.decide(&ask, false) {
        ChainVerdict::Deny { reason, .. } => {
            println!("[L7] 写文件（无审批通道）→ 诚实拒绝：{reason}")
        }
        other => println!("[L7] 意外：{other:?}"),
    }

    // 演示 allow 压过 ask（单调性）
    let mut chain2 = ApprovalChain::new();
    chain2.push("tier", |_: &CallContext| Verdict::Ask);
    chain2.push("dont-ask-again", |_: &CallContext| Verdict::Allow);
    println!(
        "[L7] allow 压过 ask（『不再询问』生效）= {:?}",
        chain2.decide(&ask, true)
    );

    let base = "你是助手。";
    let composed = harness_plugin::compose_system_prompt(base, &reg.system_prompt_appends());
    println!(
        "[L7] 系统提示只追加 = {}",
        composed.starts_with(base) && composed.len() > base.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
    println!("\n七层全部演示完成。");
    Ok(())
}

/// 取本机局域网 IP（用于提示手机访问地址）。
fn local_ip() -> anyhow::Result<String> {
    let out = std::process::Command::new("bash")
        .args([
            "-c",
            "ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null",
        ])
        .output()?;
    let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if ip.is_empty() {
        anyhow::bail!("未找到局域网地址");
    }
    Ok(ip)
}

/// 用系统默认浏览器打开界面。
fn open_browser(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}
