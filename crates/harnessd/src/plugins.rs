//! Seam ④: out-of-process plugins over stdio JSON-RPC.
//!
//! Five rules, each one a failure that has actually happened in this project's
//! history (see docs/KERNEL-FUSION-PLAN.md §3):
//!   1. `plugin/hello` must carry a protocol version; mismatch fails the load
//!   2. plugin `caps` are reviewed here; a plugin cannot widen exec policy
//!   3. a crashed plugin is contained and disabled after three strikes
//!   4. `uncertain` from a plugin blocks the turn (never silently retried)
//!   5. plugins return values only — they never write our log or state

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

pub const PLUGIN_PROTOCOL: u32 = 1;
pub const MAX_STRIKES: u32 = 3;

/// The six events a plugin may observe. Anything more and plugins start encoding
/// kernel policy, which is what this seam exists to prevent.
pub const EVENTS: [&str; 6] = [
    "tool.pre",
    "tool.post",
    "turn.start",
    "turn.end",
    "compact.pre",
    "guard.deny",
];

#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

impl PluginSpec {
    pub fn new(name: impl Into<String>, program: impl Into<String>, args: Vec<String>) -> Self {
        Self { name: name.into(), program: program.into(), args }
    }
}

pub struct Plugin {
    pub name: String,
    pub tools: Vec<String>,
    pub caps: BTreeSet<String>,
    pub strikes: u32,
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    next_id: u64,
}

impl Plugin {
    pub fn spawn(spec: &PluginSpec) -> Result<Self> {
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn plugin `{}` ({})", spec.name, spec.program))?;

        let stdin = child.stdin.take().context("plugin stdin")?;
        let stdout = child.stdout.take().context("plugin stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("plugin-{}", spec.name))
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })?;

        let mut p = Self {
            name: spec.name.clone(),
            tools: Vec::new(),
            caps: BTreeSet::new(),
            strikes: 0,
            child,
            stdin,
            rx,
            next_id: 1,
        };
        p.handshake()?;
        Ok(p)
    }

    fn handshake(&mut self) -> Result<()> {
        let reply = self.request(
            "plugin/hello",
            json!({ "protocol": PLUGIN_PROTOCOL, "events": EVENTS }),
            Duration::from_secs(5),
        )?;

        let proto = reply.get("protocol").and_then(Value::as_u64).unwrap_or(0);
        if proto != PLUGIN_PROTOCOL as u64 {
            bail!("plugin `{}` speaks protocol {proto}, we speak {PLUGIN_PROTOCOL}", self.name);
        }
        self.tools = reply
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        self.caps = reply
            .get("caps")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        // Rule 2: caps are ours to grant. A plugin asking to bypass policy loads,
        // but is stripped of that cap before it can be used.
        self.caps.remove("policy.bypass");
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&json!({ "id": id, "method": method, "params": params }))?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        loop {
            let raw = self.rx.recv_timeout(timeout).map_err(|e| {
                self.strikes += 1;
                match e {
                    RecvTimeoutError::Timeout => anyhow::anyhow!("plugin `{}` timed out", self.name),
                    RecvTimeoutError::Disconnected => anyhow::anyhow!("plugin `{}` died", self.name),
                }
            })?;
            let v: Value = serde_json::from_str(&raw)?;
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = v.get("error") {
                    bail!("plugin `{}` error: {err}", self.name);
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
            // Notification from the plugin: ignored on purpose (rule 5).
        }
    }

    pub fn call_tool(&mut self, tool: &str, input: Value) -> Result<Value> {
        let out = self.request(
            "tools/call",
            json!({ "tool": tool, "input": input }),
            Duration::from_secs(120),
        )?;
        // Rule 4: an uncertain effect is surfaced, never retried.
        if out.get("status").and_then(Value::as_str) == Some("uncertain") {
            bail!("plugin reported an uncertain side effect");
        }
        Ok(out)
    }

    pub fn notify(&mut self, event: &str, payload: Value) {
        if !EVENTS.contains(&event) {
            return;
        }
        let line = serde_json::to_string(&json!({ "method": event, "params": payload }));
        if let Ok(line) = line {
            let _ = self.stdin.write_all(line.as_bytes());
            let _ = self.stdin.write_all(b"\n");
            let _ = self.stdin.flush();
        }
    }

    pub fn healthy(&self) -> bool {
        self.strikes < MAX_STRIKES
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
pub struct PluginHost {
    plugins: Vec<Plugin>,
}

impl PluginHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rule 1: a plugin that fails the handshake fails the load loudly.
    pub fn load(&mut self, spec: &PluginSpec) -> Result<()> {
        let p = Plugin::spawn(spec)?;
        self.plugins.push(p);
        Ok(())
    }

    pub fn names(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name.as_str()).collect()
    }

    pub fn tools(&self) -> Vec<(String, String)> {
        self.plugins
            .iter()
            .filter(|p| p.healthy())
            .flat_map(|p| p.tools.iter().map(move |t| (t.clone(), p.name.clone())))
            .collect()
    }

    /// Rule 3: a crashed plugin degrades to "unavailable", never takes us down.
    pub fn call_tool(&mut self, tool: &str, input: Value) -> Option<Result<Value>> {
        let idx = self
            .plugins
            .iter()
            .position(|p| p.tools.iter().any(|t| t == tool))?;
        if !self.plugins[idx].healthy() {
            return Some(Err(anyhow::anyhow!("plugin `{}` is disabled", self.plugins[idx].name)));
        }
        let out = self.plugins[idx].call_tool(tool, input);
        if out.is_err() && !self.plugins[idx].healthy() {
            let name = self.plugins[idx].name.clone();
            return Some(Err(anyhow::anyhow!("plugin `{name}` disabled after {MAX_STRIKES} strikes")));
        }
        Some(out)
    }

    pub fn broadcast(&mut self, event: &str, payload: Value) {
        for p in self.plugins.iter_mut().filter(|p| p.healthy()) {
            p.notify(event, payload.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_plugin(hello_protocol: u32) -> PluginSpec {
        let script = format!(
            r#"
import json,sys
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    m=json.loads(line)
    if 'method' not in m: continue
    if m['method']=='plugin/hello':
        print(json.dumps({{'id':m['id'],'result':{{'name':'echo','protocol':{hello_protocol},
            'tools':['echo.run'],'caps':['policy.bypass']}}}}),flush=True)
    elif m['method']=='tools/call':
        print(json.dumps({{'id':m['id'],'result':{{'content':m['params']['input']}}}}),flush=True)
"#
        );
        PluginSpec::new("echo", "python3", vec!["-u".into(), "-c".into(), script])
    }

    fn have_python() -> bool {
        Command::new("python3").arg("--version").output().is_ok()
    }

    #[test]
    fn handshake_loads_tools_and_strips_the_bypass_cap() {
        if !have_python() {
            return;
        }
        let mut host = PluginHost::new();
        host.load(&python_plugin(PLUGIN_PROTOCOL)).unwrap();
        assert_eq!(host.names(), ["echo"]);
        assert_eq!(host.tools(), vec![("echo.run".to_string(), "echo".to_string())]);
        let p = &host.plugins[0];
        assert!(!p.caps.contains("policy.bypass"), "a plugin may not grant itself policy bypass");
    }

    #[test]
    fn protocol_mismatch_fails_the_load() {
        if !have_python() {
            return;
        }
        let mut host = PluginHost::new();
        let err = host.load(&python_plugin(99)).unwrap_err().to_string();
        assert!(err.contains("protocol"), "got: {err}");
        assert!(host.names().is_empty(), "a bad plugin must not be registered");
    }

    #[test]
    fn tool_call_round_trips() {
        if !have_python() {
            return;
        }
        let mut host = PluginHost::new();
        host.load(&python_plugin(PLUGIN_PROTOCOL)).unwrap();
        let out = host.call_tool("echo.run", json!({"x":1})).unwrap().unwrap();
        assert_eq!(out["content"], json!({"x":1}));
    }
}
