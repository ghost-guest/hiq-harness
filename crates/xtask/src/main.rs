//! 仓库自己的开发工具。**只用 Rust**：这个 crate 的存在本身就是为了
//! 把 `.py` / `.swift` 这类脚本收编进来，不再往仓库里添别的语言。
//!
//! 用法：
//!   cargo run -p xtask -- lang-audit      # 仓库里有别的语言的源码就红
//!   cargo run -p xtask -- lang-list       # 只列出来，不判红
use std::collections::BTreeMap;
use std::process::Command;

#[cfg(target_os = "macos")]
mod ocr;
mod mutate;

/// 本项目的实现语言。方案定的是 Rust 做内核与界面、Go 做插件层。
const LANGS: &[(&str, &str)] = &[
    ("rs", "Rust"),
    ("go", "Go"),
];

/// 非源码，不需要管。
const NOT_SOURCE: &[&str] = &[
    "md", "json", "toml", "yml", "yaml", "txt", "lock", "gitignore", "svg", "png", "jpg", "jpeg",
    "ttf", "otf", "woff", "woff2", "ico", "icns", "xml", "plist", "html", "css", "csv", "patch",
    "diff", "sh", "zsh", "mod", "sum", "license", "rc", "hlsl", "wgsl", "metal", "spv", "bin",
    "dat", "ndjson", "log", "snap", "bak", "orig", "rej", "envrc", "gitattributes", "editorconfig",
    // 打包配置与图片，不是源码。
    "tiff", "iss", "manifest", "h",
];

/// 允许的例外，必须写清楚为什么。
const ALLOWED_DIRS: &[(&str, &str)] = &[
    // 这是**被测数据**，不是我们的实现：跑分任务里故意放各种语言的坏代码，
    // 让 harness 去修。删掉等于删掉评测集。
    ("bench/fixtures/", "评测集的被测样例（故意用别的语言写的坏代码）"),
    // vendored gpui 的着色器是 GPU 语言，改不了。
    ("apps/orbit/vendor/", "vendored 上游代码（含 GPU 着色器）"),
    ("vendor/", "vendored 上游代码"),
];

fn tracked_files() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files"])
        .output()
        .expect("run git ls-files");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn ext_of(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).map(|e| Box::leak(e.into_boxed_str()) as &str)
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "lang-audit".into());
    if cmd == "mutate" {
        let root = std::env::current_dir().expect("cwd");
        let only = std::env::args().nth(2);
        mutate::sweep(&root, only.as_deref());
        return;
    }
    if cmd == "ocr" {
        #[cfg(target_os = "macos")]
        {
            let path = std::env::args().nth(2).unwrap_or_else(|| {
                eprintln!("用法：xtask ocr <图片>");
                std::process::exit(2);
            });
            match ocr::recognize(&path) {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    return;
                }
                Err(e) => {
                    eprintln!("ERR {e}");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            eprintln!("ocr 只在 macOS 上可用");
            std::process::exit(1);
        }
    }
    let files = tracked_files();
    let known: Vec<&str> = LANGS.iter().map(|(e, _)| *e).collect();

    let mut bad: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();

    for f in &files {
        let Some(ext) = ext_of(f) else { continue };
        if known.contains(&ext) || NOT_SOURCE.contains(&ext) {
            continue;
        }
        let allowed = ALLOWED_DIRS.iter().find(|(d, _)| f.starts_with(d));
        match allowed {
            Some((d, why)) => {
                let _ = why;
                *skipped.entry(d).or_default() += 1;
            }
            None => bad.entry(ext).or_default().push(f.as_str()),
        }
    }

    if cmd == "lang-list" || cmd == "lang-audit" {
        if skipped.is_empty() && bad.is_empty() {
            println!("ok 仓库里只有 Rust 和 Go 的源码");
        }
        for (d, n) in &skipped {
            let why = ALLOWED_DIRS.iter().find(|(p, _)| p == d).map(|(_, w)| *w).unwrap_or("");
            println!("允许 {d}（{n} 个文件）：{why}");
        }
        for (ext, list) in &bad {
            println!("!! .{ext} 共 {} 个：", list.len());
            for f in list.iter().take(8) {
                println!("   {f}");
            }
            if list.len() > 8 {
                println!("   …还有 {} 个", list.len() - 8);
            }
        }
    }

    if cmd == "lang-audit" && !bad.is_empty() {
        let total: usize = bad.values().map(Vec::len).sum();
        println!("FAIL 仓库里有 {total} 个非 Rust/Go 的源码文件（方案要求只用 Rust + Go）");
        std::process::exit(1);
    }
}
