//! Go 插件宿主：子进程 + JSON-RPC over stdio。
//!
//! 设计来自调研：
//! - **进程边界而非 FFI**（kiso、Codewhale）：崩溃隔离、语言无关。
//! - **加载必须大声**（kiso `extensions.md`）：
//!   坏插件启动即失败，不能静默改变行为。
//! - **项目扩展不能覆盖内建**（kiso）：
//!   `a repo you cloned must never silently replace the CLI's own behavior`
//! - **能力枚举**（Codewhale `activation.rs`）：显式声明能做什么。

use harness_plugin::{Capability, PluginManifest, PluginScope, Registry, RpcRequest, RpcResponse};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// 一个运行中的 Go 插件进程。
pub struct PluginProcess {
    name: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl PluginProcess {
    /// 启动插件。
    ///
    /// `entry` 是可执行文件路径。启动失败必须报错，
    /// 不能静默跳过 —— 否则插件没生效却没人知道。
    pub fn spawn(manifest: &PluginManifest, base_dir: &Path) -> anyhow::Result<Self> {
        let entry = if Path::new(&manifest.entry).is_absolute() {
            PathBuf::from(&manifest.entry)
        } else {
            base_dir.join(&manifest.entry)
        };
        if !entry.exists() {
            anyhow::bail!(
                "插件 {} 的入口不存在：{}。加载失败必须报错，不能静默跳过。",
                manifest.name,
                entry.display()
            );
        }

        let mut child = Command::new(&entry)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("启动插件 {} 失败：{e}", manifest.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 stdout"))?;

        Ok(Self {
            name: manifest.name.clone(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    /// 发一次 RPC。
    pub fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = RpcRequest {
            id,
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&req)?;
        writeln!(self.stdin, "{line}")?;
        self.stdin.flush()?;

        let mut resp_line = String::new();
        let n = self.stdout.read_line(&mut resp_line)?;
        if n == 0 {
            anyhow::bail!("插件 {} 无响应（进程可能已退出）", self.name);
        }
        let resp: RpcResponse = serde_json::from_str(resp_line.trim())
            .map_err(|e| anyhow::anyhow!("插件 {} 返回非法 JSON：{e}", self.name))?;
        if let Some(err) = resp.error {
            anyhow::bail!("插件 {} 报错：{err}", self.name);
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }

    /// 询问插件提供哪些工具。
    pub fn describe(&mut self) -> anyhow::Result<serde_json::Value> {
        self.call("describe", serde_json::json!({}))
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 加载并启动一组插件。
///
/// 返回成功启动的进程。任何一个启动失败都会整体报错 ——
/// 这是 kiso 的「加载必须大声」。
pub fn load_plugins(registry: &Registry, base_dir: &Path) -> anyhow::Result<Vec<PluginProcess>> {
    let mut out = Vec::new();
    for p in registry.ordered() {
        // 只启动声明了 Tools 或 Hooks 的
        if !p.manifest.has(Capability::Tools) && !p.manifest.has(Capability::Hooks) {
            continue;
        }
        let proc = PluginProcess::spawn(&p.manifest, base_dir)?;
        out.push(proc);
    }
    Ok(out)
}

/// 从目录发现插件并注册。
///
/// 项目级目录的插件不能覆盖内建（kiso 的安全规则）。
pub fn discover_and_register(
    registry: &mut Registry,
    dir: &Path,
    scope: PluginScope,
) -> anyhow::Result<usize> {
    let found = harness_plugin::scan_dir(dir, scope)
        .map_err(|e| anyhow::anyhow!("扫描插件目录失败：{e}"))?;
    let mut n = 0;
    for (manifest, path) in found {
        registry
            .register(manifest, scope, path)
            .map_err(|e| anyhow::anyhow!("注册插件失败：{e}"))?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "plug-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_entry_fails_loudly() {
        // kiso 的「加载必须大声」：入口不存在必须报错
        let dir = tmpdir("missing");
        let m = PluginManifest {
            name: "ghost".into(),
            version: "0.1.0".into(),
            entry: "./does-not-exist".into(),
            capabilities: vec![Capability::Tools],
            tools: vec![],
            system_prompt_append: None,
        };
        let err = match PluginProcess::spawn(&m, &dir) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("入口不存在时必须报错"),
        };
        assert!(err.contains("不存在"), "应报入口不存在：{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_plugin_cannot_override_builtin() {
        let mut reg = Registry::new();
        let m = PluginManifest {
            name: "mcp".into(),
            version: "1".into(),
            entry: "./x".into(),
            capabilities: vec![Capability::Tools],
            tools: vec![],
            system_prompt_append: None,
        };
        reg.register(m.clone(), PluginScope::Builtin, "builtin")
            .unwrap();
        // 项目级覆盖内建 → 必须被拒
        assert!(reg.register(m, PluginScope::Project, "repo").is_err());
    }

    #[test]
    fn discover_empty_dir_is_ok() {
        let dir = tmpdir("empty");
        let mut reg = Registry::new();
        let n = discover_and_register(&mut reg, &dir, PluginScope::User).unwrap();
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_bad_json_fails_loudly() {
        let dir = tmpdir("badjson");
        std::fs::write(dir.join("bad.json"), "{ not json").unwrap();
        let mut reg = Registry::new();
        let err = discover_and_register(&mut reg, &dir, PluginScope::User).unwrap_err();
        assert!(err.to_string().contains("扫描"), "应报扫描失败：{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_registers_valid_manifest() {
        let dir = tmpdir("valid");
        let m = serde_json::json!({
            "name": "demo",
            "version": "0.1.0",
            "entry": "./demo",
            "capabilities": ["tools"],
            "tools": [{"name": "echo", "description": "回显", "read_only": true}]
        });
        std::fs::write(dir.join("demo.json"), m.to_string()).unwrap();
        let mut reg = Registry::new();
        let n = discover_and_register(&mut reg, &dir, PluginScope::User).unwrap();
        assert_eq!(n, 1);
        assert_eq!(reg.tools().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_capability_rejected_by_default() {
        // Codewhale 的能力枚举：Native 默认不启用
        let dir = tmpdir("native");
        let m = serde_json::json!({
            "name": "native-plugin",
            "version": "0.1.0",
            "entry": "./x",
            "capabilities": ["native"]
        });
        std::fs::write(dir.join("n.json"), m.to_string()).unwrap();
        let mut reg = Registry::new();
        assert!(discover_and_register(&mut reg, &dir, PluginScope::User).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
