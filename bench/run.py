#!/usr/bin/env python3
"""Bench driver: 20 tasks, executable pass criteria.

    ./run.py --list
    ./run.py --selftest                 # proves every check fails before the fix and passes after
    ./run.py --solver './harnessd run --prompt "$TASK_PROMPT"'
    ./run.py --task b01_off_by_one --solver '...'

A task is only admitted to the set if `--selftest` shows both directions:
  * without the fix the check FAILS  (the check is not vacuous)
  * with the fix    the check PASSES (the criterion is reachable)
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent


def load_tasks(only=None):
    out = []
    for f in sorted((ROOT / "tasks").glob("*.json")):
        t = json.loads(f.read_text(encoding="utf-8"))
        if only and t["id"] != only:
            continue
        out.append(t)
    return out


def fresh_dir(t):
    d = Path(tempfile.mkdtemp(prefix=f"bench-{t['id']}-"))
    if t.get("fixture") and (REPO / t["fixture"]).exists():
        shutil.copytree(REPO / t["fixture"], d, dirs_exist_ok=True)
    return d


def sh(cmd, cwd, env=None, timeout=None):
    e = dict(os.environ)
    if env:
        e.update(env)
    t0 = time.time()
    try:
        p = subprocess.run(cmd, cwd=cwd, shell=True, capture_output=True, text=True,
                           timeout=timeout or 180, env=e)
        return p.returncode == 0, (p.stdout + p.stderr).strip(), time.time() - t0
    except subprocess.TimeoutExpired:
        return False, f"timeout after {timeout or 180}s", time.time() - t0


def run_check(t, d):
    # A fix that keeps the file the same byte length would otherwise be masked by a
    # stale .pyc (CPython validates on mtime-seconds + size).
    return sh(t["check"], d, env={"PYTHONDONTWRITEBYTECODE": "1"}, timeout=t.get("timeout_s", 180))


def apply_gold(t, d):
    gold = t.get("gold") or {}
    for name, script in gold.items():
        if name == "__script__":
            code = subprocess.run(["python3", "-c", script], cwd=d, capture_output=True, text=True)
            if code.returncode != 0:
                raise RuntimeError(f"gold failed for {t['id']}: {code.stderr}")
        else:
            (d / name).write_text(script, encoding="utf-8")


def read_log(d):
    p = d / ".harness" / "timebus.ndjson"
    if not p.exists():
        return {}
    calls = denied = 0
    blocked = False
    for line in p.read_text(encoding="utf-8").splitlines():
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("kind") == "tool.call":
            calls += 1
            if (e.get("payload") or {}).get("status") == "uncertain":
                blocked = True
        elif e.get("kind") == "guard.deny":
            denied += 1
    return {"tool_calls": calls, "denials": denied, "blocked": blocked}


def selftest(tasks):
    bad = []
    for t in tasks:
        d = fresh_dir(t)
        ok_before, out_before, _ = run_check(t, d)
        apply_gold(t, d)
        ok_after, out_after, _ = run_check(t, d)
        verdict = "valid" if (not ok_before and ok_after) else "INVALID"
        if verdict == "INVALID":
            bad.append((t["id"], ok_before, ok_after, out_before, out_after))
        print(f"{t['id']:<22} {t['kind']:<11} before={'PASS' if ok_before else 'fail'} "
              f"after={'PASS' if ok_after else 'fail'}  {verdict}")
        shutil.rmtree(d, ignore_errors=True)
    print()
    if bad:
        for b in bad:
            print("INVALID:", b[0], "before_pass=", b[1], "after_pass=", b[2])
            print("  before:", b[3][:200])
            print("  after :", b[4][:200])
        return 1
    print(f"selftest ok: {len(tasks)} tasks, every check fails before the fix and passes after")
    return 0


def run(tasks, solver, out_path):
    results = []
    for t in tasks:
        d = fresh_dir(t)
        if solver == "gold":
            # A perfect agent: proves the scoring pipeline itself reaches 100%.
            apply_gold(t, d)
            ok_solve, solve_out, dt_solve = True, "gold", 0.0
        elif solver:
            ok_solve, solve_out, dt_solve = sh(solver, d, env={"TASK_PROMPT": t["prompt"]},
                                               timeout=t.get("timeout_s", 180) + 120)
        else:
            ok_solve, solve_out, dt_solve = True, "no solver (check-only)", 0.0
        ok, out, dt = run_check(t, d)
        res = {"id": t["id"], "kind": t["kind"], "pass": ok,
               "duration_s": round(dt_solve + dt, 2), "detail": out[:400], **read_log(d)}
        results.append(res)
        print(f"{t['id']:<22} {t['kind']:<11} {'PASS' if ok else 'FAIL':<4} {res['duration_s']:>6}s "
              f"tools={res.get('tool_calls', 0)} denied={res.get('denials', 0)}")
        shutil.rmtree(d, ignore_errors=True)

    n = len(results)
    passed = sum(1 for r in results if r["pass"])
    summary = {
        "total": n,
        "passed": passed,
        "pass_rate": round(passed / n, 3) if n else 0.0,
        "greenfield_pass_rate": round(
            sum(1 for r in results if r["kind"] == "greenfield" and r["pass"]) /
            max(1, sum(1 for r in results if r["kind"] == "greenfield")), 3),
        "brownfield_pass_rate": round(
            sum(1 for r in results if r["kind"] == "brownfield" and r["pass"]) /
            max(1, sum(1 for r in results if r["kind"] == "brownfield")), 3),
        "guard_denials": sum(r.get("denials", 0) for r in results),
        "blocked_turns": sum(1 for r in results if r.get("blocked")),
        "mean_duration_s": round(sum(r["duration_s"] for r in results) / n, 2) if n else 0.0,
    }
    print("\n" + json.dumps(summary, ensure_ascii=False, indent=2))
    if out_path:
        Path(out_path).write_text(json.dumps({"summary": summary, "results": results},
                                             ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"written: {out_path}")
    return 0 if passed == n else 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--solver", default=None, help="shell command; $TASK_PROMPT holds the task text")
    ap.add_argument("--task", default=None)
    ap.add_argument("--out", default=None)
    a = ap.parse_args()

    tasks = load_tasks(a.task)
    if not tasks:
        print("no tasks", file=sys.stderr)
        return 2
    if a.list:
        for t in tasks:
            print(f"{t['id']:<22} {t['kind']:<11} {t['prompt'][:70]}")
        return 0
    if a.selftest or not a.solver:
        return selftest(tasks)
    return run(tasks, a.solver, a.out)


if __name__ == "__main__":
    sys.exit(main())
