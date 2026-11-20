//! harnessd CLI.
//!
//! `serve`  — orbit-facing loop on stdio (line-delimited JSON, one command per line)
//! `selftest` — end-to-end check with no model, no network, no API key
//! `replay`  — print the projection of a time-bus log (proves the log is the truth)
//! `doctor`  — environment check before wiring a real engine

use anyhow::{Context, Result};
use harnessd::engine::{Engine, JsonRpcEngine, MockEngine};
use harnessd::http_engine::HttpEngine;
use harnessd::protocol::{encode, Frame, UiEvent};
use harnessd::server::Supervisor;
use harnessd::timebus::{read_entries, Log};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Receiver};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("replay") => replay(args.get(1).context("usage: harnessd replay <log.ndjson>")?),
        Some("doctor") => doctor(),
        Some("selftest") | None => selftest(),
        Some(other) => {
            eprintln!("unknown command `{other}`");
            eprintln!("usage: harnessd <serve|selftest|replay|doctor>");
            std::process::exit(2);
        }
    }
}

fn serve(args: &[String]) -> Result<()> {
    // Defaults come from the environment so an embedding app (orbit) can hand us
    // an engine without building a command line.
    let mut program = std::env::var("HARNESSD_ENGINE").ok().filter(|v| !v.trim().is_empty());
    let mut engine_args: Vec<String> = std::env::var("HARNESSD_ENGINE_ARGS")
        .ok()
        .map(|v| v.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let mut log_path = std::env::var("HARNESSD_LOG").unwrap_or_else(|_| ".harness/timebus.ndjson".into());
    let mut window: u64 = std::env::var("HARNESSD_WINDOW").ok().and_then(|v| v.parse().ok()).unwrap_or(128_000);
    let mut plugin_paths: Vec<String> = Vec::new();
    let mut compat: Option<String> = None;
    let mut http_base: Option<String> =
        std::env::var("HARNESSD_HTTP_ENGINE").ok().filter(|v| !v.trim().is_empty());

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--engine" => program = it.next().cloned(),
            "--log" => log_path = it.next().cloned().unwrap_or(log_path),
            "--window" => window = it.next().and_then(|v| v.parse().ok()).unwrap_or(window),
            "--compat" => compat = it.next().cloned(),
            "--http-engine" => http_base = it.next().cloned(),
            "--plugin" => {
                if let Some(p) = it.next() {
                    plugin_paths.push(p.clone());
                }
            }
            other => engine_args.push(other.to_string()),
        }
    }

    let log = Log::open(&log_path)?;
    if let Some(base) = http_base.clone() {
        // HTTP is the only transport with tools and approvals (see http_engine.rs).
        let token = std::env::var("CODEWHALE_RUNTIME_TOKEN").ok();
        let mut sup = Supervisor::new(HttpEngine::new(base, token), log, window);
        sup.prepare()?;
        eprintln!("engine ready (http): {}", sup.engine.name());
        load_plugins(&mut sup, &plugin_paths)?;
        return if compat.as_deref() == Some("orbit") {
            serve_orbit(&mut sup)
        } else {
            serve_stdio(&mut sup)
        };
    }
    let engine = match program {
        Some(p) => JsonRpcEngine::spawn(&p, &engine_args)?,
        None => {
            eprintln!("no --engine given; run `harnessd selftest` for the offline path");
            std::process::exit(2);
        }
    };
    let mut sup = Supervisor::new(engine, log, window);

    for path in &plugin_paths {
        let spec = harnessd::plugins::PluginSpec::new(
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin"),
            path,
            Vec::new(),
        );
        match sup.plugins.load(&spec) {
            Ok(()) => eprintln!("plugin loaded: {} {:?}", spec.name, sup.plugins.tools()),
            Err(e) => {
                // Rule 1: a bad plugin fails the load, loudly, and never half-works.
                eprintln!("plugin `{}` refused: {e}", spec.name);
                std::process::exit(3);
            }
        }
    }

    sup.prepare()?;
    eprintln!(
        "engine ready: {} methods, tool_free={}, full_api={:?}",
        sup.state.caps.methods.len(),
        sup.state.caps.tool_free,
        sup.state.caps.full_api
    );

    if compat.as_deref() == Some("orbit") {
        return serve_orbit(&mut sup);
    }

    serve_stdio(&mut sup)
}


/// 引擎的通知是**之后**到的（HTTP/SSE 尤其如此）。命令处理完就把它们取出来，
/// 一直取到**终态事件**为止——不是「静默一秒就撒手」。
///
/// 这条区别很重要：静默判活会在模型慢的时候提前收工，把还在跑的轮次丢掉；
/// 而终态（`turn_end` / `turn.failed` / `agent_end` / `process_exited`）
/// 是引擎自己说的「这轮结束了」。超时兜底由 `HARNESSD_TURN_TIMEOUT` 控制。
/// `steer_rx`：orbit compat 在回合进行中收到的命令。
/// 回合期间 stdin 主循环被本函数占住，用户此时发的 steer 命令
/// 会堆在 channel 里。每轮 pump 前后捞一次，把 steer 注入 `sup.steer`，
/// 由 pump 现有的安全点逻辑在 item 边界释放给引擎。
///
/// 这是 S3 "流式中途插话生效" 的关键一环：机制（SteerGate + 安全点）
/// 早就在了，缺的只是让回合进行中的命令能进到 gate 里。
fn drain_async_events<E: harnessd::engine::Engine>(
    sup: &mut Supervisor<E>,
    steer_rx: Option<&Receiver<String>>,
) -> Result<(Vec<harnessd::protocol::UiEvent>, Vec<String>)> {
    let timeout: u64 = std::env::var("HARNESSD_TURN_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let started = std::time::Instant::now();
    let mut all = Vec::new();
    let mut deferred: Vec<String> = Vec::new();
    let mut terminal = false;
    let mut saw_any = false;
    let mut idle = 0;
    loop {
        // 回合进行中，非阻塞地捞走用户插话，注入 SteerGate。
        // 用 try_recv 而非 recv：绝不能因为等命令而卡住回合。
        if let Some(rx) = steer_rx {
            while let Ok(line) = rx.try_recv() {
                if let Some(text) = steer_text_from_line(&line) {
                    // 插话注入 gate，由 pump 的安全点逻辑释放给引擎。
                    sup.steer.push(text);
                } else {
                    // 其他命令这一轮不处理，交还主循环按正常顺序处理。
                    // 绝不能丢——比如回合中发的第二个 prompt 必须排队。
                    deferred.push(line);
                }
            }
        }
        let batch = sup.pump()?;
        if batch.is_empty() {
            if terminal {
                break;
            }
            idle += 1;
            if !saw_any && idle >= 20 {
                // 引擎一句话都没说：别把命令卡死在这儿。
                break;
            }
            if started.elapsed().as_secs() >= timeout {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        } else {
            saw_any = true;
            idle = 0;
            for event in &batch {
                if is_terminal(&event.kind) {
                    terminal = true;
                }
            }
            all.extend(batch);
        }
    }
    Ok((all, deferred))
}

fn is_terminal(kind: &str) -> bool {
    matches!(
        kind,
        "turn_end" | "agent_end" | "agent_settled" | "process_exited" | "error"
    ) || kind.contains("turn.completed")
        || kind.contains("turn.failed")
        || kind.contains("agent_end")
}


/// 把发给界面的一条事件写出去。
///
/// `HARNESSD_ORBIT_DUMP=/path` 时**同时**写一份到文件：界面显示不出东西时，
/// 靠"我们到底发没发"来判断是投影丢了还是渲染丢了——不用再猜。
fn emit_orbit<W: std::io::Write>(out: &mut W, e: &Value) -> anyhow::Result<()> {
    write!(out, "{}\n", serde_json::to_string(e)?)?;
    if let Ok(path) = std::env::var("HARNESSD_ORBIT_DUMP") {
        if !path.trim().is_empty() {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(f, "{}", serde_json::to_string(e).unwrap_or_default());
            }
        }
    }
    Ok(())
}

fn starts_a_turn(method: &str) -> bool {
    matches!(method, "Prompt" | "turn/start" | "prompt/run" | "prompt/request")
}

fn load_plugins<E: harnessd::engine::Engine>(sup: &mut Supervisor<E>, paths: &[String]) -> Result<()> {
    for path in paths {
        let spec = harnessd::plugins::PluginSpec::new(
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin"),
            path,
            Vec::new(),
        );
        match sup.plugins.load(&spec) {
            Ok(()) => eprintln!("plugin loaded: {} {:?}", spec.name, sup.plugins.tools()),
            Err(e) => {
                eprintln!("plugin `{}` refused: {e}", spec.name);
                std::process::exit(3);
            }
        }
    }
    Ok(())
}

fn serve_stdio<E: harnessd::engine::Engine>(sup: &mut Supervisor<E>) -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let frame = match harnessd::protocol::decode(&line) {
            Ok(f) => f,
            Err(e) => {
                write!(out, "{}", encode(&Frame::error_response(0, -32700, e.to_string())))?;
                out.flush()?;
                continue;
            }
        };
        let id = frame.id.unwrap_or(0);
        let method = frame.method.clone().unwrap_or_default();
        match sup.handle(&method, frame.params.unwrap_or(Value::Null)) {
            Ok((reply, events)) => {
                write!(out, "{}", encode(&Frame::response(id, reply)))?;
                for e in events {
                    write!(out, "{}", encode(&Frame::notification("event", serde_json::to_value(e)?)))?;
                }
                if starts_a_turn(&method) {
                    let (events, _deferred) = drain_async_events(sup, None)?;
                    for e in events {
                        write!(out, "{}", encode(&Frame::notification("event", serde_json::to_value(e)?)))?;
                    }
                }
            }
            Err(e) => {
                write!(out, "{}", encode(&Frame::error_response(id, -32601, e.to_string())))?;
            }
        }
        out.flush()?;
    }
    Ok(())
}

/// 从一行 orbit JSONL 里认出 steer 命令，取它的文本。
/// 只认 `{"type":"steer"}`；其他命令返回 None。解析失败也返回 None
/// （主循环会给出正式的解析错误，这里不重复报错）。
fn steer_text_from_line(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("steer") {
        return None;
    }
    let text = v
        .get("text")
        .or_else(|| v.get("input"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// orbit's JSONL dialect: `{"id","type",...}` in, `{"type",...}` out.
fn serve_orbit<E: harnessd::engine::Engine>(sup: &mut Supervisor<E>) -> Result<()> {
    use harnessd::orbit_compat::{command_of, events_to_orbit, params_of, response_of};

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();

    // 把 stdin 读入放进独立线程 → channel。
    // 这样回合进行中（drain_async_events 占着主循环时），用户发的 steer
    // 命令仍能进 channel，被 drain 里的 try_recv 捞走注入 SteerGate。
    // 主线程仍是唯一的命令处理者——Supervisor 没有跨线程共享，无锁。
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break; // 主循环退了
            }
        }
        // tx 在这里 drop → channel 关闭 → rx.recv() 收到 Err → 主循环退出。
    });

    // 回合结束后要处理的暂存命令（drain 期间被捞走的非 steer 命令）。
    let mut pending: Vec<String> = Vec::new();
    loop {
        let line = if !pending.is_empty() {
            pending.remove(0)
        } else {
            match rx.recv() {
                Ok(line) => line,
                Err(_) => break, // stdin EOF，channel 关闭
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let r = response_of("", "", false, Value::Null, Some(e.to_string()));
                write!(out, "{}\n", serde_json::to_string(&r)?)?;
                out.flush()?;
                continue;
            }
        };
        let id = v.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("").to_string();

        let Some(cmd) = command_of(&kind) else {
            let r = response_of(&id, &kind, false, Value::Null, Some(format!("unsupported command `{kind}`")));
            write!(out, "{}\n", serde_json::to_string(&r)?)?;
            out.flush()?;
            continue;
        };

        match sup.handle(cmd, params_of(&v)) {
            Ok((reply, events)) => {
                write!(out, "{}\n", serde_json::to_string(&response_of(&id, &kind, true, reply, None))?)?;
                for e in events_to_orbit(&events) {
                    emit_orbit(&mut out, &e)?;
                }
                if starts_a_turn(cmd) {
                    let (events, deferred) = drain_async_events(sup, Some(&rx))?;
                    for e in events {
                        for out_event in events_to_orbit(&[e]) {
                            emit_orbit(&mut out, &out_event)?;
                        }
                    }
                    // 回合中收到的非 steer 命令，放回队首下次处理。
                    for line in deferred.into_iter().rev() {
                        pending.insert(0, line);
                    }
                }
            }
            Err(e) => {
                let r = response_of(&id, &kind, false, Value::Null, Some(e.to_string()));
                write!(out, "{}\n", serde_json::to_string(&r)?)?;
            }
        }
        out.flush()?;
    }
    Ok(())
}

fn replay(path: &str) -> Result<()> {
    let entries = read_entries(path)?;
    let p = harnessd::timebus::project(&entries);
    println!("{}", serde_json::to_string_pretty(&json!({
        "entries": entries.len(),
        "next_seq": p.next_seq,
        "messages": p.messages.len(),
        "receipts": p.receipts.values().collect::<Vec<_>>(),
        "blocked": p.blocked().map(|r| r.call_id.clone()),
        "compacted_at": p.compacted_at,
        "turn_open": p.turn_open,
    }))?);
    Ok(())
}

fn doctor() -> Result<()> {
    let mut ok = true;
    match std::process::Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => println!("git      ok"),
        _ => {
            println!("git      MISSING (snapshot/restore needs it)");
            ok = false;
        }
    }
    for (name, var) in [("deepseek", "DEEPSEEK_API_KEY"), ("magpie", "MAGPIE_API_KEY")] {
        match std::env::var(var) {
            Ok(v) if !v.trim().is_empty() => println!("{name:<8} key set ({} chars)", v.len()),
            _ => println!("{name:<8} key absent ({var})"),
        }
    }
    let engine = std::env::var("HARNESSD_ENGINE").unwrap_or_default();
    if engine.is_empty() {
        println!("engine   not set (HARNESSD_ENGINE=path/to/codewhale-app-server)");
    } else {
        println!("engine   {engine}");
    }
    if ok {
        Ok(())
    } else {
        anyhow::bail!("environment incomplete")
    }
}

/// Offline end-to-end: proves the seams work without a model.
fn selftest() -> Result<()> {
    let mut dir = std::env::temp_dir();
    dir.push(format!("harnessd-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let log_path = dir.join("timebus.ndjson");
    let _ = std::fs::remove_file(&log_path);

    let mut engine = MockEngine::default();
    engine.emit("item/agentMessage/delta", json!({"delta":"hi from the engine"}));
    engine.emit("item/completed", json!({"item":{"type":"agentMessage","id":"i1"}}));
    engine.emit("turn/completed", json!({}));

    let mut sup = Supervisor::new(engine, Log::open(&log_path)?, 128_000);

    // 1. command path
    let (reply, events) = sup.handle("Prompt", json!({"text":"你好"}))?;
    assert_eq!(reply["thread"]["id"], "thr_mock", "first prompt opens a thread");
    assert!(events.iter().any(|e: &UiEvent| e.kind == "text_delta"));
    println!("1 command path       ok  ({} events)", events.len());

    // 2. thread reuse
    sup.handle("Prompt", json!({"text":"再来"})).unwrap();
    assert!(sup.engine.calls().iter().filter(|c| *c == "turn/start").count() >= 1);
    println!("2 thread reuse       ok  (thread={})", sup.state.thread_id.clone().unwrap_or_default());

    // 3. guard veto on the approval channel
    let mut denied = false;
    for i in 0..30u64 {
        sup.engine.emit_request(900 + i, "item/commandExecution/requestApproval", json!({"tool": "list_dir"}));
        sup.pump()?;
        if sup.stats()["denials"].as_u64().unwrap_or(0) > 0 {
            denied = true;
            break;
        }
    }
    assert!(denied, "spiral guard did not veto");
    println!("3 guard veto         ok  (approval -> denied)");

    // 4. steer at a safe point
    sup.steer.push("换个方案");
    let before = sup.engine.calls().len();
    sup.engine.emit("item/completed", json!({"item":{"type":"agentMessage","id":"i2"}}));
    sup.pump()?;
    assert!(sup.engine.calls().len() > before, "steer never reached the engine");
    println!("4 steer safe point   ok  (released={})", sup.steer.released());

    // 5. uncertain effect blocks
    sup.engine.emit(
        "item/completed",
        json!({"item":{"type":"commandExecution","id":"c9","command":"rm -rf build","status":"inProgress"}}),
    );
    sup.pump()?;
    assert!(sup.log.project().blocked().is_some());
    println!("5 uncertain blocks   ok  (no retry, no silent success)");

    // 6. compaction advice
    sup.engine.emit("thread/tokenUsage/updated", json!({"promptTokens":120000,"cachedTokens":118000}));
    sup.pump()?;
    assert!(sup.engine.calls().iter().any(|c| c == "thread/compact/start"));
    println!("6 cache guard        ok  (hot prefix -> compact)");

    // close the second turn before replaying
    sup.engine.emit("turn/completed", json!({}));
    sup.pump()?;

    // 7. replay from disk
    let proj = Log::open(&log_path)?.project();
    assert_eq!(proj.messages.len(), 1, "only the first turn logged a message");
    assert!(!proj.turn_open);
    println!("7 replay             ok  ({} entries, seq={})", proj.next_seq, proj.next_seq);

    // 8. the honest gaps stay honest
    let auth = sup.handle("AuthLogin", json!({}));
    assert!(auth.is_err(), "AuthLogin must not be faked");
    println!("8 honest gaps        ok  (AuthLogin -> not applicable)");

    println!("\nselftest passed: 8/8 seams");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[cfg(test)]
mod steer_line_tests {
    use super::steer_text_from_line;

    /// 回合进行中，orbit 发来的 steer 命令必须被认出来。
    /// 这是"中途插话"能进 SteerGate 的第一道门——认不出就白搭。
    #[test]
    fn recognizes_steer_from_orbit_line() {
        let line = r#"{"id":"s1","type":"steer","text":"别数了，改成这句"}"#;
        assert_eq!(
            steer_text_from_line(line).as_deref(),
            Some("别数了，改成这句")
        );
    }

    /// 兼容 input 字段（老 orbit 版本）。
    #[test]
    fn recognizes_steer_input_field() {
        let line = r#"{"id":"s2","type":"steer","input":"停下"}"#;
        assert_eq!(steer_text_from_line(line).as_deref(), Some("停下"));
    }

    /// 其他命令绝不能被当成 steer——否则回合中发的 prompt 会被
    /// 错误地注入 SteerGate 而丢失。
    #[test]
    fn other_commands_are_not_steer() {
        assert!(steer_text_from_line(r#"{"id":"p","type":"prompt","message":"hi"}"#).is_none());
        assert!(steer_text_from_line(r#"{"id":"a","type":"abort"}"#).is_none());
        assert!(steer_text_from_line("not json").is_none());
        // 空 steer 也不能注入（否则会往 gate 里塞空串）。
        assert!(steer_text_from_line(r#"{"type":"steer","text":"  "}"#).is_none());
    }
}
