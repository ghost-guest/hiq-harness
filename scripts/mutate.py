#!/usr/bin/env python3
"""变异扫描：把内核代码故意改坏，看测试抓不抓得住。

为什么需要它：这一夜反复出现同一类问题——测试名字说有，内容里没有
（`nudges_then_refuses_then_stops` 从没验过 stop）。名字、注释、文档都能骗人，
只有"把它改坏，看它红不红"骗不了人。

用法：
    python3 scripts/mutate.py                 # 全部变异体
    python3 scripts/mutate.py --only guards   # 只跑某个 crate
退出码：0 = 每个变异体都被抓到；1 = 有"存活"（改了代码测试还是绿）。

每个变异体跑完一定还原文件——`finally` 里做，不用等人工清理。
"""
import argparse
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
LOCK = ROOT / ".mutate.lock"

# 每个变异体：文件、原文、改成什么、跑哪个 crate、描述。
# 选的都是**语义关键点**，不是格式噪声：守卫的升降档、缓存的判定线、
# 审批的词表、回合的闭合、映射的兜底。
MUTANTS = [
    # ---- 守卫：阶梯三档 ----
    ("guards-stop-to-note", "crates/guards/src/lib.rs",
     "NoProgressDecision::Stop { reason } => PostCallVerdict::Stop { reason },",
     "NoProgressDecision::Stop { reason } => PostCallVerdict::Note { note: reason },",
     "harness-guards", "守卫最后一档 Stop 降级成 Note"),
    ("guards-refuse-to-note", "crates/guards/src/lib.rs",
     "NoProgressDecision::Refuse { reason } => PostCallVerdict::ReplaceResult { reason },",
     "NoProgressDecision::Refuse { reason } => PostCallVerdict::Note { note: reason },",
     "harness-guards", "拒跑降级成提示"),
    ("guards-nudge-to-ok", "crates/guards/src/lib.rs",
     "NoProgressDecision::Nudge { note } => PostCallVerdict::Note { note },",
     "NoProgressDecision::Nudge { note } => PostCallVerdict::Note { note: format!(\"\") },",
     "harness-guards", "提示的理由被清空（UI 看到空提示）"),
    ("guards-failure-gate-threshold", "crates/guards/src/lib.rs",
     "if self.streak >= self.threshold {",
     "if self.streak >= 9999 {",
     "harness-guards", "相同失败永远到不了阈值"),

    # ---- 缓存守卫：两条线 ----
    ("cache-hit-floor", "crates/harnessd/src/cacheguard.rs",
     "pub const HIT_FLOOR: f64 = 0.90;",
     "pub const HIT_FLOOR: f64 = 0.00;",
     "harnessd", "命中率下限归零（冷前缀也压缩）"),
    ("cache-pressure", "crates/harnessd/src/cacheguard.rs",
     "pub const PRESSURE: f64 = 0.85;",
     "pub const PRESSURE: f64 = 0.00;",
     "harnessd", "压力线归零（永远觉得窗口满）"),

    # ---- 审批：词表 ----
    ("approval-allow-word", "crates/harnessd/src/http_engine.rs",
     '"accept" | "allow" | "acceptForSession" | "acceptWithExecpolicyAmendment" => "allow",',
     '"accept" | "acceptForSession" | "acceptWithExecpolicyAmendment" => "allow",',
     "harnessd", "裸 \"allow\" 不再被认（引擎会 400）"),

    # ---- 回合闭合 ----
    ("turn-completed-dropped", "crates/harnessd/src/http_engine.rs",
     '"turn.completed" | "turn.failed" | "turn.cancelled" => "turn/completed".into(),',
     '"turn.failed" | "turn.cancelled" => "turn/completed".into(),',
     "harnessd", "正常完成的回合不再被认作完成"),

    # ---- 映射兜底 ----
    # ---- 这一夜修过的每个缺陷，都问一遍：有测试守着吗 ----
    ("silent-drop-guard", "crates/harnessd/src/server.rs",
     '"engine rejected this turn: no turn id, no output (prompt dropped)"',
     '"engine accepted this turn (downgraded)"',
     "harnessd", "被拒的回合又被当成正常（错误文案都被改了）"),
    ("compact-failure-silent", "crates/harnessd/src/server.rs",
     'out.push(UiEvent::new(\n                                        "compact_failed",',
     'out.push(UiEvent::new(\n                                        "compact_ok",',
     "harnessd", "压缩失败被记成成功（坏消息翻译成好消息）"),
    ("protocol-turn-end", "crates/harnessd/src/protocol.rs",
     '"turn/completed" => "turn_end",',
     '"turn/completed" => "turn_end_DISABLED",',
     "harnessd", "protocol 层不认 turn.completed"),
    ("orbit-flat-nesting", "crates/harnessd/src/orbit_compat.rs",
     '        other => {\n            out.insert("value".into(), other.clone());\n        }',
     '        other => {\n            out.insert("value".into(), json!({ "value": other.clone() }));\n        }',
     "harnessd", "扁平化变成嵌套（界面会读不到）"),
    ("cache-observed-values", "crates/harnessd/src/server.rs",
     "self.cache.observe(input, cached);",
     "self.cache.observe(0, 0);",
     "harnessd", "缓存守卫永远拿到 0/0（永远\"没问题\"）"),
    # GetEntries：引擎返回 items，orbit 读 entries
    ("history-items-entries", "crates/harnessd/src/http_engine.rs",
     'obj.insert("entries".into(), entries);',
     'let _ = entries;',
     "harnessd", "items 不翻译成 entries（界面看到空历史）"),

    # steer 的数组方言：mapping.rs 产出 input 数组，不是字符串
    ("steer-array-dialect", "crates/harnessd/src/http_engine.rs",
     '        .or_else(|| {\n            params.get("input").and_then(Value::as_array).and_then(|a| {\n                a.iter()\n                    .filter_map(|i| i.get("text").and_then(Value::as_str))\n                    .next()\n            })\n        })\n',
     '',
     "harnessd", "steer 数组方言读不到 → 引擎 400 prompt is required"),

    # 回合结束只发 agent_end，不发 agent_settled。界面永远认为回合在跑，
    # 下一条消息一直排队转圈。
    ("turn-end-without-settle", "crates/harnessd/src/orbit_compat.rs",
     'vec![v, json!({ "type": "agent_settled", "aborted": false })]',
     'vec![v]',
     "harnessd", "回合结束时没有 agent_settled"),

    # HTTP/SSE 的 item 把种类放 `kind`，stdio 放 `type`。只认 type，
    # 每条 assistant 回答都不进日志——回放/分叉/压缩全都没有内容。
    ("http-item-kind-not-a-message", "crates/harnessd/src/server.rs",
     'kind_field == "agent_message"',
     'false',
     "harnessd", "HTTP 方言的 agent_message 不再被认成回答"),

    # 不流式的 provider 只有成品文本、没有 delta。不补一条 text_delta，
    # 界面按 delta 渲染就是一个字都不显示。
    ("nonstreaming-answer-never-reaches-ui", "crates/harnessd/src/server.rs",
     'if !detail.trim().is_empty() {\n                            out.insert(0, UiEvent::new("text_delta", json!({ "delta": detail })));',
     'if false {\n                            out.insert(0, UiEvent::new("text_delta", json!({ "delta": detail })));',
     "harnessd", "非流式回答不再发给界面"),

    # fork 之后当前线程必须跟着换。只认 thread/start 的话，fork 后
    # state.thread_id 还是旧线程：回合发到新线程，get_entries 却读旧线程。
    ("fork-thread-state-stale", "crates/harnessd/src/server.rs",
     '"thread/start" | "thread/fork" | "thread/resume"',
     '"thread/start"',
     "harnessd", "fork 之后 Supervisor 的当前线程没跟着换"),

    # 测试替身丢掉 params。丢了的代价是"发给引擎的 threadId 是什么"看不见，
    # 投影类 bug（读错线程、key 名不对）单测全抓不到。
    ("mock-engine-drops-params", "crates/harnessd/src/engine.rs",
     'self.requests.push((method.to_string(), params.clone()));',
     'self.requests.push((method.to_string(), Value::Null));',
     "harnessd", "测试替身不再记录参数"),

    # 回合 id 必须能穿过 handle 的包装；丢了它，第 2 回合会被误判成"被拒"
    ("turn-id-dropped-in-wrap", "crates/harnessd/src/server.rs",
     '"turn_id": out.get("turn_id"),',
     '"turn_id": Value::Null,',
     "harnessd", "包装时丢掉回合 id（多回合被误判成被拒）"),

    ("mapping-unknown-command", "crates/harnessd/src/mapping.rs",
     '"Prompt" => {',
     '"Prompt-DISABLED" => {',
     "harnessd", "Prompt 命令掉进兜底"),
]


def run(cmd, cwd=ROOT, timeout=600):
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                          timeout=timeout, shell=isinstance(cmd, str))


def suite(crate):
    r = run(["cargo", "test", "-q", "-p", crate])
    out = r.stdout + r.stderr
    if "error[E" in out or "could not compile" in out:
        return "compile-error", out
    m = re.search(r"(\d+) passed; (\d+) failed", out)
    if not m:
        return "no-result", out
    return ("fail" if int(m.group(2)) > 0 else "pass"), out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", help="只跑名字里含这个串的变异体")
    args = ap.parse_args()

    picked = [m for m in MUTANTS if not args.only or args.only in m[0]]

    # 扫描期间工作区里是"被改坏"的代码。别人这时候编译/测试会看到假的红
    # （我踩过：扫描跑着，我并发跑 cargo test，看到 `failure_gate` 红了，
    # 查了半小时才发现是扫描器正在改文件、还没还原）。
    if LOCK.exists():
        pid = LOCK.read_text().strip()
        print(f"!! 已经有一个扫描在跑（pid {pid}）。等它结束，或删掉 {LOCK}。")
        return 2
    LOCK.write_text(str(__import__("os").getpid()))
    print(f"# 变异扫描：{len(picked)} 个变异体（锁：{LOCK.name}）\n")
    try:
        return _sweep(picked)
    finally:
        # 异常也要解锁，否则下一次扫描永远说"已经有一个在跑"。
        LOCK.unlink(missing_ok=True)


def _sweep(picked):
        rows, survived = [], []
        for name, rel, old, new, crate, desc in picked:
            path = ROOT / rel
            if old not in path.read_text(encoding="utf-8"):
                # 原文没找到 = 这个变异体压根没测到。算通过就是自欺。
                rows.append((name, desc, "**STALE**", "原文没找到，变异体过期了"))
                survived.append((name, desc + "（变异体过期：原文没找到）"))
                print(f"  !! STALE {name}：{desc}（原文没找到——变异体过期，不是通过）")
                continue
            backup = pathlib.Path(tempfile.mkdtemp()) / "backup"
            shutil.copy2(path, backup)
            try:
                src = path.read_text(encoding="utf-8")
                path.write_text(src.replace(old, new, 1), encoding="utf-8")
                verdict, out = suite(crate)
            finally:
                # 还原后必须把 mtime 推新。`copy2` 会保留**备份文件**的 mtime，
                # 而备份是"改坏之前"那一刻创建的——cargo 一比时间戳，
                # 认为源文件没变，继续用**变异版**的旧产物。
                # 我踩过：扫描全绿，紧接着 cargo test 红了两条不相关的测试，
                # 源码却是对的，查了很久才发现是缓存。
                shutil.copy2(backup, path)
                _now = time.time()
                os.utime(path, (_now, _now))

            if verdict == "fail":
                rows.append((name, desc, "KILLED", "测试红了"))
                print(f"  KILLED {name}：{desc}")
            elif verdict == "compile-error":
                # 编译失败也算被挡住——但要说清是"编译器挡的"还是"测试挡的"。
                rows.append((name, desc, "KILLED", "编译失败（编译器挡的）"))
                print(f"  KILLED {name}：{desc}（编译失败）")
            else:
                rows.append((name, desc, "**SURVIVED**", f"{verdict}"))
                survived.append((name, desc))
                print(f"  !! SURVIVED {name}：{desc}")

        print()
        print("| 变异体 | 改坏了什么 | 结果 | 说明 |")
        print("|---|---|---|---|")
        for n, d, v, note in rows:
            print(f"| {n} | {d} | {v} | {note} |")
        print()
        if survived:
            print(f"## !! {len(survived)} 个变异体没被抓住 —— 这些地方测试抓不住")
            for n, d in survived:
                print(f"- {n}：{d}")
            return 1
        print(f"全部 {len(rows)} 个变异体都被抓到（没有 STALE，也没有 SURVIVED）。")
        return 0


if __name__ == "__main__":
    sys.exit(main())
