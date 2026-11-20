//! 变异扫描，Rust 版。
//!
//! 原来是 `scripts/mutate.py`（267 行 Python）。仓库语言约束是 Rust + Go，
//! 所以收编进 xtask；**变异表放在 `scripts/mutants.json`**——那是数据，不是代码，
//! 加新变异体不用碰编译。
//!
//! 语义和 Python 版一致：
//!   - 原文找不到 = STALE（算失败，不是通过）
//!   - 编译不过 = compile-error（不算抓到，必须排除）
//!   - 测试红了 = KILLED；测试全绿 = SURVIVED
//!   - 全程持锁，别人并发跑 cargo test 看到假红
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(serde::Deserialize)]
pub struct Mutant {
    pub name: String,
    pub file: String,
    pub old: String,
    pub new: String,
    #[serde(rename = "crate")]
    pub krate: String,
    pub desc: String,
}

pub fn sweep(root: &Path, only: Option<&str>) {
    let table: Vec<Mutant> = serde_json::from_str(
        &std::fs::read_to_string(root.join("scripts/mutants.json")).expect("read mutants.json"),
    )
    .expect("mutants.json 解析失败");

    let picked: Vec<&Mutant> = table
        .iter()
        .filter(|m| only.map(|o| m.name.contains(o)).unwrap_or(true))
        .collect();

    let lock = root.join(".mutate.lock");
    if lock.exists() {
        // 扫描期间工作区里是"被改坏"的代码。别人这时候跑测试会看到假红。
        println!("!! 已经有一个扫描在跑（锁 {}）。等它结束，或删掉它。", lock.display());
        std::process::exit(2);
    }
    std::fs::write(&lock, std::process::id().to_string()).ok();
    println!("# 变异扫描：{} 个变异体（锁：.mutate.lock）\n", picked.len());

    let mut rows: Vec<(String, &'static str, String)> = Vec::new();
    let mut bad: Vec<String> = Vec::new();

    for m in picked {
        let path = root.join(&m.file);
        let original = std::fs::read_to_string(&path).unwrap_or_default();
        if !original.contains(&m.old) {
            rows.push((m.name.clone(), "**STALE**", "原文没找到，变异体过期了".into()));
            bad.push(format!("{}：{}（变异体过期）", m.name, m.desc));
            println!("  !! STALE {}：{}", m.name, m.desc);
            continue;
        }
        let mutated = original.replacen(&m.old, &m.new, 1);
        // 直接写=更新 mtime。Python 版用 copy2 保留旧时间戳，导致 cargo 认为
        // 源码没变、继续用变异版产物，扫描全绿而实际是假的——这里天然没有这个问题。
        let restore = || {
            let _ = std::fs::write(&path, &original);
        };
        std::fs::write(&path, mutated).expect("写变异体");
        let (verdict, out) = suite(root, &m.krate);
        restore();

        match verdict {
            "compile-error" => {
                rows.push((m.name.clone(), "**COMPILE**", "编译没过，不算抓到".into()));
                bad.push(format!("{}：编译不过（变异体无效）", m.name));
                println!("  !! COMPILE {}：{}", m.name, m.desc);
            }
            "pass" => {
                rows.push((m.name.clone(), "**SURVIVED**", "测试全绿，没抓到".into()));
                bad.push(format!("{}：{}", m.name, m.desc));
                println!("  !! SURVIVED {}：{}", m.name, m.desc);
            }
            "fail" => {
                rows.push((m.name.clone(), "KILLED", "测试红了".to_string()));
                println!("  KILLED {}：{}", m.name, m.desc);
            }
            other => {
                let _ = out;
                rows.push((m.name.clone(), "**NO-RESULT**", other.to_string()));
                bad.push(format!("{}：{}", m.name, other));
            }
        }
    }

    let _ = std::fs::remove_file(&lock);
    println!("\n| 变异体 | 含义 | 判定 | 说明 |");
    println!("|---|---|---|---|");
    for (n, v, why) in &rows {
        println!("| {n} | {} | {v} | {why} |", table.iter().find(|m| &m.name == n).map(|m| m.desc.as_str()).unwrap_or(""));
    }
    if bad.is_empty() {
        println!("\n全部 {} 个变异体都被抓到（没有 STALE，也没有 SURVIVED）。", rows.len());
    } else {
        println!("\n## !! {} 个变异体没被抓住 —— 这些地方测试抓不住", bad.len());
        for b in &bad {
            println!("- {b}");
        }
        std::process::exit(1);
    }
}

fn suite(root: &Path, krate: &str) -> (&'static str, String) {
    let out = Command::new("cargo")
        .args(["test", "-q", "-p", krate])
        .current_dir(root)
        .output()
        .expect("run cargo test");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if text.contains("error[E") || text.contains("could not compile") {
        return ("compile-error", text);
    }
    // `test result: ok. 78 passed; 0 failed; 0 ignored; …`
    // 一次 cargo test 会有多行（lib + 各个 bin/test target），failures 要累加。
    let mut failed: u32 = 0;
    let mut seen_result = false;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("test result:") {
            continue;
        }
        seen_result = true;
        for part in line.split(';') {
            let p = part.trim();
            if let Some(n) = p.strip_suffix("failed") {
                if let Ok(v) = n.trim().parse::<u32>() {
                    failed += v;
                }
            }
        }
    }
    if !seen_result {
        return ("no-result", text);
    }
    if failed > 0 {
        return ("fail", text);
    }
    return ("pass", text);
}

pub fn mutants_path(root: &Path) -> PathBuf {
    root.join("scripts/mutants.json")
}
