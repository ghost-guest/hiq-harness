//! 仓库级 lint：把踩过的坑变成会红的规则。
//!
//! 不是风格检查，是**事故检查**——每一条都对应一次真实故障。
use std::path::Path;
use std::process::Command;

fn sh(cmd: &str) -> String {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

pub fn run(root: &Path) -> i32 {
    let mut problems: Vec<String> = Vec::new();

    // 规则 1：建线程必须显式给 auto_approve。
    //
    // 事故：不显式给 → 引擎默认 false → 模型一调工具就发 approval.required
    // 然后一直等，回合永远 in_progress，界面显示"已工作 0s"，后续消息排队。
    let engine = std::fs::read_to_string(root.join("crates/harnessd/src/http_engine.rs"))
        .unwrap_or_default();
    let body_fn = engine
        .find("fn thread_body(&self) -> Value {")
        .map(|i| &engine[i..(i + 1200).min(engine.len())])
        .unwrap_or("");
    // 只认**赋值语句**：注释里出现 auto_approve 不算数
    //（第一版就是这么被自己的注释骗过去的）。
    if !body_fn.contains("body[\"auto_approve\"]") {
        problems.push(
            "http_engine::thread_body() 没有兜底 auto_approve：引擎会默认 false，\
             工具一调用就无限等审批（回合永不闭合）"
                .to_string(),
        );
    }

    // 规则 2：订阅必须跟着线程走（单订阅者引擎）。
    if !engine.contains("same_thread") {
        problems.push(
            "http_engine::ensure_stream() 里没有「订阅线程 == 当前线程」的判断：\
             换会话后事件收不到，回合不闭合"
                .to_string(),
        );
    }

    // 规则 3：回合事件必须边产生边写出去（否则界面只在结束时一次性收到）。
    let main = std::fs::read_to_string(root.join("crates/harnessd/src/main.rs")).unwrap_or_default();
    if !main.contains("on_batch") {
        problems.push(
            "drain_async_events 没有 on_batch 回调：事件会被攒到回合结束才吐出去，\
             界面运行中看不到任何过程"
                .to_string(),
        );
    }

    // 规则 4：起界面时线程体里不许是 auto_approve=false（探针除外）。
    let bad_launchers = sh("grep -rln 'HARNESSD_THREAD_BODY=.*auto_approve.*false' scripts 2>/dev/null \
         | grep -v '^scripts/probe-' || true");
    for f in bad_launchers.lines().filter(|l| !l.trim().is_empty()) {
        problems.push(format!(
            "{f} 用 auto_approve=false 起界面：界面显示不出审批框，回合会卡死（探针请放 scripts/probe-*）"
        ));
    }

    if problems.is_empty() {
        println!("ok lint：{} 条事故规则全部满足", 4);
        0
    } else {
        for p in &problems {
            println!("!! {p}");
        }
        println!("FAIL lint：{} 条规则没满足", problems.len());
        1
    }
}
