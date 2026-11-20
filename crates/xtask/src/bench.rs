//! 20 道题的跑分驱动（Rust 版，替代 `bench/run.py`）。
//!
//! 语义与 Python 版逐条对齐：
//!   - 每道题一个临时目录，先铺 fixture
//!   - `--selftest`：修之前必须红、修之后必须绿，两边都成立才算有效题
//!   - `--solver`：shell 命令，`$TASK_PROMPT` 是题面
//!   - 从 `.harness/timebus.ndjson` 读工具调用/守卫/用量，算返工率与缓存命中
//!
//! 题目本身（fixture 里的坏代码、check 脚本）是**被测数据**，不改。
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(serde::Deserialize, Clone)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub prompt: String,
    #[serde(default)]
    pub fixture: Option<String>,
    pub check: String,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    #[serde(default)]
    pub gold: BTreeMap<String, String>,
}

fn default_timeout() -> u64 {
    180
}

fn repo_root() -> PathBuf {
    // xtask 在 crates/xtask 下，仓库根是再上两级
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
}

fn load_tasks(only: Option<&str>) -> Result<Vec<Task>> {
    let dir = repo_root().join("bench/tasks");
    let mut out = Vec::new();
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("读 {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    names.sort();
    for p in names {
        let t: Task = serde_json::from_str(&std::fs::read_to_string(&p)?)
            .with_context(|| format!("解析 {}", p.display()))?;
        if only.map(|o| t.id != o).unwrap_or(false) {
            continue;
        }
        out.push(t);
    }
    Ok(out)
}

fn fresh_dir(t: &Task) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("bench-{}-{}", t.id, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    if let Some(fx) = &t.fixture {
        let src = repo_root().join(fx);
        if src.exists() {
            copy_tree(&src, &dir)?;
        }
    }
    Ok(dir)
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), &dst)?;
        }
    }
    Ok(())
}

fn sh(cmd: &str, cwd: &Path, extra_env: &[(&str, String)], timeout_s: u64) -> (bool, String, f64) {
    let t0 = Instant::now();
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd).current_dir(cwd);
    c.env("PYTHONDONTWRITEBYTECODE", "1");
    for (k, v) in extra_env {
        c.env(k, v);
    }
    // 超时靠外部 `timeout` 不存在（macOS 没有），这里用轮询 + kill。
    let mut child = match c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn() {
        Ok(ch) => ch,
        Err(e) => return (false, format!("spawn: {e}"), 0.0),
    };
    let deadline = Instant::now() + std::time::Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (false, format!("timeout after {timeout_s}s"), t0.elapsed().as_secs_f64());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return (false, format!("wait: {e}"), t0.elapsed().as_secs_f64()),
        }
    }
    let out = child.wait_with_output().unwrap_or_else(|_| unreachable!());
    let ok = out.status.success();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (ok, text.trim().to_string(), t0.elapsed().as_secs_f64())
}

fn apply_gold(t: &Task, dir: &Path) -> Result<()> {
    for (name, script) in &t.gold {
        if name == "__script__" {
            let st = Command::new("python3").arg("-c").arg(script).current_dir(dir).output()?;
            if !st.status.success() {
                bail!("gold failed for {}: {}", t.id, String::from_utf8_lossy(&st.stderr));
            }
        } else {
            std::fs::write(dir.join(name), script)?;
        }
    }
    Ok(())
}

fn run_check(t: &Task, dir: &Path) -> (bool, String, f64) {
    sh(&t.check, dir, &[], t.timeout_s)
}

fn read_log(dir: &Path) -> Value {
    let p = dir.join(".harness/timebus.ndjson");
    let Ok(text) = std::fs::read_to_string(&p) else {
        return json!({});
    };
    let (mut calls, mut denied, mut repeats, mut input, mut cached) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut blocked = false;
    for line in text.lines() {
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let payload = e.get("payload").cloned().unwrap_or(Value::Null);
        match e.get("kind").and_then(Value::as_str) {
            Some("tool.call") => {
                calls += 1;
                if payload.get("repeat").and_then(Value::as_bool).unwrap_or(false) {
                    repeats += 1;
                }
                if payload.get("status").and_then(Value::as_str) == Some("uncertain") {
                    blocked = true;
                }
            }
            Some("guard.deny") => denied += 1,
            Some("usage") => {
                input += payload.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                cached += payload.get("cached_tokens").and_then(Value::as_i64).unwrap_or(0);
            }
            _ => {}
        }
    }
    let rework = if calls > 0 { (repeats as f64 / calls as f64 * 1000.0).round() / 1000.0 } else { 0.0 };
    json!({
        "tool_calls": calls,
        "denials": denied,
        "blocked": blocked,
        "repeats": repeats,
        "rework_rate": rework,
        "input_tokens": input,
        "cache_hit_rate": if input > 0 { Some((cached as f64 / input as f64 * 1000.0).round() / 1000.0) } else { None },
    })
}

pub fn main_cmd(args: &[String]) -> Result<i32> {
    let has = |f: &str| args.iter().any(|a| a == f);
    let val = |f: &str| -> Option<String> {
        args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned()
    };
    let tasks = load_tasks(val("--task").as_deref())?;
    if tasks.is_empty() {
        bail!("no tasks");
    }
    if has("--list") {
        for t in &tasks {
            println!("{:<22} {:<11} {}", t.id, t.kind, t.prompt.chars().take(70).collect::<String>());
        }
        return Ok(0);
    }
    let solver = val("--solver");
    if has("--selftest") || solver.is_none() {
        return selftest(&tasks);
    }
    run(&tasks, solver.unwrap(), val("--out"))
}

fn selftest(tasks: &[Task]) -> Result<i32> {
    let mut bad = Vec::new();
    for t in tasks {
        let d = fresh_dir(t)?;
        let (ok_before, out_before, _) = run_check(t, &d);
        apply_gold(t, &d)?;
        let (ok_after, out_after, _) = run_check(t, &d);
        let valid = !ok_before && ok_after;
        println!(
            "{:<22} {:<11} before={} after={}  {}",
            t.id,
            t.kind,
            if ok_before { "PASS" } else { "fail" },
            if ok_after { "PASS" } else { "fail" },
            if valid { "valid" } else { "INVALID" }
        );
        if !valid {
            bad.push((t.id.clone(), ok_before, ok_after, out_before, out_after));
        }
        let _ = std::fs::remove_dir_all(&d);
    }
    println!();
    if !bad.is_empty() {
        for b in &bad {
            println!("INVALID: {} before_pass={} after_pass={}", b.0, b.1, b.2);
            println!("  before: {}", b.3.chars().take(200).collect::<String>());
            println!("  after : {}", b.4.chars().take(200).collect::<String>());
        }
        return Ok(1);
    }
    println!(
        "selftest ok: {} tasks, every check fails before the fix and passes after",
        tasks.len()
    );
    Ok(0)
}

fn run(tasks: &[Task], solver: String, out_path: Option<String>) -> Result<i32> {
    let mut results = Vec::new();
    let mut solver_failures = Vec::new();
    for t in tasks {
        let d = fresh_dir(t)?;
        let (ok_solve, solve_out, dt_solve) = if solver == "gold" {
            apply_gold(t, &d)?;
            (true, "gold".to_string(), 0.0)
        } else {
            let r = sh(&solver, &d, &[("TASK_PROMPT", t.prompt.clone())], t.timeout_s + 120);
            if !r.0 {
                solver_failures.push(json!({
                    "id": t.id,
                    "seconds": (r.2 * 100.0).round() / 100.0,
                    "output": r.1.lines().take(3).collect::<Vec<_>>(),
                }));
                println!(
                    "!! solver failed on {} after {:.1}s: {}",
                    t.id,
                    r.2,
                    r.1.lines().next().unwrap_or("(no output)").chars().take(160).collect::<String>()
                );
            }
            r
        };
        let (ok, out, dt) = run_check(t, &d);
        let mut res = json!({
            "id": t.id,
            "kind": t.kind,
            "pass": ok,
            "duration_s": ((dt_solve + dt) * 100.0).round() / 100.0,
            "detail": out.chars().take(400).collect::<String>(),
        });
        let log = read_log(&d);
        if let (Some(obj), Some(l)) = (res.as_object_mut(), log.as_object()) {
            for (k, v) in l {
                obj.insert(k.clone(), v.clone());
            }
        }
        println!(
            "{:<22} {:<11} {:<4} {:>6}s tools={} denied={} rework={}",
            t.id, t.kind, if ok { "PASS" } else { "FAIL" },
            res["duration_s"], res["tool_calls"], res["denials"], res["rework_rate"]
        );
        results.push(res);
        let _ = std::fs::remove_dir_all(&d);
        let _ = ok_solve; let _ = solve_out;
    }

    let n = results.len() as f64;
    let passed = results.iter().filter(|r| r["pass"].as_bool().unwrap_or(false)).count();
    let sum = |k: &str| -> f64 { results.iter().map(|r| r[k].as_f64().unwrap_or(0.0)).sum() };
    let input_total = sum("input_tokens");
    let greenfield: Vec<_> = results.iter().filter(|r| r["kind"] == "greenfield").collect();
    let brownfield: Vec<_> = results.iter().filter(|r| r["kind"] == "brownfield").collect();
    let pass_rate = |v: &[&Value]| -> f64 {
        if v.is_empty() { 0.0 } else {
            let p = v.iter().filter(|r| r["pass"].as_bool().unwrap_or(false)).count() as f64;
            (p / v.len() as f64 * 1000.0).round() / 1000.0
        }
    };
    let summary = json!({
        "total": results.len(),
        "rework_rate": ((sum("rework_rate") / n) * 1000.0).round() / 1000.0,
        "false_kill_rate": ((results.iter().filter(|r| r["denials"].as_i64().unwrap_or(0) > 0).count() as f64 / n) * 1000.0).round() / 1000.0,
        "cache_hit_rate": if input_total > 0.0 {
            let weighted: f64 = results.iter()
                .map(|r| r["input_tokens"].as_f64().unwrap_or(0.0) * r["cache_hit_rate"].as_f64().unwrap_or(0.0))
                .sum();
            Some(((weighted / input_total) * 1000.0).round() / 1000.0)
        } else { None },
        "passed": passed,
        "pass_rate": ((passed as f64 / n) * 1000.0).round() / 1000.0,
        "greenfield_pass_rate": pass_rate(&greenfield),
        "brownfield_pass_rate": pass_rate(&brownfield),
        "guard_denials": sum("denials"),
        "blocked_turns": results.iter().filter(|r| r["blocked"].as_bool().unwrap_or(false)).count(),
        "solver_failures": solver_failures.len(),
        "mean_duration_s": ((sum("duration_s") / n) * 100.0).round() / 100.0,
    });

    if !solver_failures.is_empty() {
        println!("\n!! {} 道题的 solver 自己失败了——这些分数不说明模型能力，先查 solver：", solver_failures.len());
        for f in solver_failures.iter().take(3) {
            println!("   - {} ({}s): {:?}", f["id"], f["seconds"], f["output"].get(0));
        }
    }
    println!("\n{}", serde_json::to_string_pretty(&summary)?);
    if let Some(p) = out_path {
        std::fs::write(&p, serde_json::to_string_pretty(&json!({
            "summary": summary, "results": results, "solver_failures": solver_failures
        }))?)?;
        println!("written: {p}");
    }
    Ok(if passed == results.len() { 0 } else { 1 })
}
