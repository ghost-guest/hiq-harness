//! Real process plumbing: a fake app-server on the other end of a pipe.
//!
//! MockEngine covers the logic; this covers the parts logic tests cannot reach —
//! framing, id matching, reverse requests, and the `harnessd serve` binary itself.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const FAKE_ENGINE: &str = r#"
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    m = json.loads(line)
    method = m.get("method")
    if method is None:
        continue                      # a response to our reverse request
    mid = m.get("id")
    def reply(result):
        print(json.dumps({"id": mid, "result": result}), flush=True)
    if method == "initialize":
        reply({"serverInfo": {"name": "fake"}})
    elif method == "thread/start":
        reply({"thread": {"id": "thr_fake"}})
    elif method == "turn/start":
        reply({"turn": {"id": "turn_1"}})
        print(json.dumps({"id": 9000, "method": "item/commandExecution/requestApproval",
                          "params": {"tool": "list_dir"}}), flush=True)
        print(json.dumps({"method": "item/agentMessage/delta",
                          "params": {"delta": "hello"}}), flush=True)
        print(json.dumps({"method": "item/completed",
                          "params": {"item": {"type": "agentMessage", "id": "i1"}}}), flush=True)
        print(json.dumps({"method": "turn/completed", "params": {}}), flush=True)
    elif method == "model/list":
        reply({"models": [{"id": "deepseek-v4-pro"}]})
    else:
        reply({})
"#;

/// One file per test: the two tests run in parallel in the same process, and a
/// shared path lets the faster one delete the script the other is about to spawn.
fn fake_script(label: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("fake-engine-{}-{}.py", std::process::id(), label));
    std::fs::write(&p, FAKE_ENGINE).unwrap();
    p
}

#[test]
fn jsonrpc_engine_talks_to_a_real_process() {
    let script = fake_script("jsonrpc");
    let mut engine =
        harnessd::engine::JsonRpcEngine::spawn("python3", &["-u".into(), script.to_string_lossy().into()])
            .expect("spawn fake engine");

    use harnessd::engine::Engine;
    let out = engine.request("initialize", json!({})).expect("initialize");
    assert_eq!(out["serverInfo"]["name"], "fake");

    let out = engine.request("thread/start", json!({"input": []})).expect("thread/start");
    assert_eq!(out["thread"]["id"], "thr_fake");

    // This turn emits two notifications and one reverse request before completing.
    let out = engine.request("turn/start", json!({"threadId": "thr_fake"})).expect("turn/start");
    assert_eq!(out["turn"]["id"], "turn_1");

    let notes = engine.notifications();
    // reverse approval + text delta + item/completed + turn/completed
    assert_eq!(notes.len(), 4, "reverse request must not be swallowed: {notes:?}");
    assert!(notes.iter().any(|n| n.method.as_deref() == Some("item/agentMessage/delta")));

    // Answering the reverse request must not deadlock the session.
    engine.respond(9000, json!({"decision": "approved"})).expect("respond");
    assert_eq!(engine.request("model/list", json!({})).expect("model/list")["models"][0]["id"],
               "deepseek-v4-pro");

    let _ = std::fs::remove_file(&script);
}

#[test]
fn serve_binary_streams_events_for_one_prompt() {
    let script = fake_script("serve");
    let log = std::env::temp_dir().join(format!("harnessd-serve-{}.ndjson", std::process::id()));
    let _ = std::fs::remove_file(&log);

    let mut child = Command::new(env!("CARGO_BIN_EXE_harnessd"))
        .args(["serve", "--engine", "python3", "-u"])
        .arg(&script)
        .arg("--log")
        .arg(&log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start harnessd");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    // First prompt opens the thread, second one runs a turn.
    writeln!(stdin, r#"{{"id":1,"method":"Prompt","params":{{"text":"hi"}}}}"#).unwrap();
    writeln!(stdin, r#"{{"id":2,"method":"Prompt","params":{{"text":"again"}}}}"#).unwrap();
    stdin.flush().unwrap();
    drop(stdin); // EOF lets serve finish instead of blocking the reader forever

    let mut lines = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        lines.push(line);
        if lines.len() > 32 {
            break;
        }
    }
    let _ = child.wait();

    let parsed: Vec<Value> = lines.iter().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect();
    assert!(parsed.iter().any(|f| f["result"]["thread"]["id"] == "thr_fake"), "no thread reply: {lines:?}");
    assert!(parsed.iter().any(|f| f["result"]["turn"]["id"] == "turn_1"), "no turn reply: {lines:?}");
    assert!(
        parsed.iter().any(|f| f["method"] == "event" && f["params"]["kind"] == "text_delta"),
        "no text delta reached the client: {lines:?}"
    );
    assert!(
        parsed.iter().any(|f| f["method"] == "event" && f["params"]["kind"] == "turn_end"),
        "turn never completed: {lines:?}"
    );
}
