//! CLI 入口。
//!
//! 当前只实现本地可验证的部分：事件日志、守卫自检。
//! 模型调用与工具执行留了 trait，下一步接 OpenAI 兼容协议。

use anyhow::Result;
use clap::{Parser, Subcommand};
use harness_core::event::{EventKind, EventLog};
use harness_guards::{GuardSet, PostCallVerdict, PreCallVerdict, Signature};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "harness", version, about = "personal-AgentHarness")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 初始化工作区的日志目录
    Init,
    /// 往日志里追加一条消息并回放
    Say {
        text: String,
        #[arg(long, default_value = ".harness")]
        dir: PathBuf,
    },
    /// 回放当前日志投影出的消息
    Replay {
        #[arg(long, default_value = ".harness")]
        dir: PathBuf,
    },
    /// 守卫自检：用内置场景验证三个守卫的行为
    Selftest,
}

fn log_path(dir: &PathBuf) -> PathBuf {
    dir.join("events.jsonl")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().cmd {
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
            for m in harness_core::event::Projection::new(&EventLog::read_all(log_path(&dir))?)
                .messages()
            {
                println!("  {:?}: {}", m.role, first_line(&m.text));
            }
        }

        Cmd::Replay { dir } => {
            let events = EventLog::read_all(log_path(&dir))?;
            let proj = harness_core::event::Projection::new(&events);
            println!(
                "{} events, {} projected messages",
                events.len(),
                proj.messages().len()
            );
            for m in proj.messages() {
                println!("  {:?}: {}", m.role, first_line(&m.text));
            }
            if proj.has_unresolved_uncertain() {
                println!("!! 存在未决的 uncertain 副作用，需人工裁决");
            }
        }

        Cmd::Selftest => selftest()?,
    }
    Ok(())
}

fn first_line(s: &str) -> String {
    let l = s.lines().next().unwrap_or("");
    if l.chars().count() > 70 {
        format!("{}…", l.chars().take(70).collect::<String>())
    } else {
        l.to_string()
    }
}

/// 守卫自检：把调研里的关键场景跑一遍，打印结果。
fn selftest() -> Result<()> {
    println!("=== 守卫自检 ===\n");

    // 场景 1：相同失败连续 3 次 → 熔断
    let mut g = GuardSet::new();
    let s = Signature::new("bash", "pytest", "boom", "");
    for i in 1..=3 {
        let v = g.after_call(&s, false);
        println!("相同失败第 {i} 次 → {v:?}");
        if i == 3 {
            assert!(
                matches!(v, PostCallVerdict::Note { .. }),
                "第 3 次应触发熔断提示"
            );
        }
    }

    // 场景 2：轮询（结果在变）→ 不熔断
    let mut g = GuardSet::new();
    let mut blocked = false;
    for i in 0..20 {
        let s = Signature::new("bash", "check", &format!("running {i}%"), "");
        if let PostCallVerdict::Note { note } = g.after_call(&s, false) {
            if note.contains("连续失败") {
                blocked = true;
            }
        }
    }
    assert!(!blocked, "轮询不应触发失败熔断");
    println!("\n轮询 20 次（结果每次不同）→ 未触发失败熔断 ok");

    // 场景 3：只读螺旋
    // 注意：必须用全新的 GuardSet，且只走 before_call，
    // 否则前面场景残留的 failure 状态会干扰。
    let mut g = GuardSet::new();
    let mut advisory_at = None;
    let mut blocked_at = None;
    for i in 1..=16 {
        match g.before_call("read_file") {
            PreCallVerdict::ProceedWithNote { .. } if advisory_at.is_none() => {
                advisory_at = Some(i)
            }
            PreCallVerdict::Block { .. } if blocked_at.is_none() => blocked_at = Some(i),
            _ => {}
        }
    }
    println!("只读螺旋：第 {advisory_at:?} 次提示，第 {blocked_at:?} 次阻塞");
    assert_eq!(advisory_at, Some(12), "第 12 次应提示");
    assert_eq!(blocked_at, Some(15), "第 15 次应阻塞");

    // 场景 4：写入后重置
    let mut g = GuardSet::new();
    for _ in 0..11 {
        g.before_call("read_file");
    }
    g.before_call("write_file");
    let v = g.before_call("read_file");
    assert!(matches!(v, PreCallVerdict::Proceed), "写入后应重置螺旋计数");
    println!("写入后只读计数重置 ok");

    println!("\n全部通过。");
    Ok(())
}
