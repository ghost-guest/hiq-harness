//! 真实工具执行器。
//!
//! 只实现最小集：read_file / write_file / list_dir / bash。
//! 每个工具都在工作区内运作，路径要校验不越界。

use harness_core::agent_loop::{ToolExecutor, ToolRequest, ToolResult};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct LocalTools {
    /// 工作区根。所有相对路径以此为基准，且不允许越界。
    pub root: PathBuf,
}

impl LocalTools {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 把相对路径解析成绝对路径，并校验不越出工作区。
    fn resolve(&self, p: &str) -> anyhow::Result<PathBuf> {
        let candidate = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.root.join(p)
        };
        // 词法归一化，不碰文件系统（避免符号链接探测）
        let mut out = PathBuf::new();
        for comp in candidate.components() {
            use std::path::Component::*;
            match comp {
                ParentDir => {
                    out.pop();
                }
                CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        let root_norm = {
            let mut r = PathBuf::new();
            for comp in self.root.components() {
                use std::path::Component::*;
                match comp {
                    ParentDir => {
                        r.pop();
                    }
                    CurDir => {}
                    other => r.push(other.as_os_str()),
                }
            }
            r
        };
        if !out.starts_with(&root_norm) {
            anyhow::bail!("路径越出工作区: {p}");
        }
        Ok(out)
    }

    /// 工具的 JSON schema，供模型使用。
    pub fn tool_specs() -> serde_json::Value {
        serde_json::json!([
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "读取工作区内的文件内容",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "相对工作区的路径"}
                        },
                        "required": ["path"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "把内容写入工作区内的文件（覆盖）",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"}
                        },
                        "required": ["path", "content"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "list_dir",
                    "description": "列出工作区内某个目录的条目",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "相对路径，留空表示根"}
                        }
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "在工作区执行一条 shell 命令（非交互）",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string"}
                        },
                        "required": ["command"]
                    }
                }
            }
        ])
    }
}

impl LocalTools {
    /// 同步执行入口。runner 是同步的，直接用它。
    pub fn execute_sync(&self, req: &ToolRequest) -> anyhow::Result<ToolResult> {
        // 复用 async 实现的逻辑：这里手工展开，避免引入 runtime。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(<Self as ToolExecutor>::execute(self, req))
    }
}

impl ToolExecutor for LocalTools {
    async fn execute(&self, req: &ToolRequest) -> anyhow::Result<ToolResult> {
        match req.name.as_str() {
            "read_file" => {
                let p = req.input.get("path").and_then(|v| v.as_str()).unwrap_or("");
                match self.resolve(p) {
                    Ok(abs) => match std::fs::read_to_string(&abs) {
                        Ok(text) => Ok(ToolResult {
                            output: text,
                            success: true,
                            blocks_digest: String::new(),
                        }),
                        Err(e) => Ok(ToolResult {
                            output: format!("读取失败: {e}"),
                            success: false,
                            blocks_digest: String::new(),
                        }),
                    },
                    Err(e) => Ok(ToolResult {
                        output: e.to_string(),
                        success: false,
                        blocks_digest: String::new(),
                    }),
                }
            }

            "write_file" => {
                let p = req.input.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let content = req
                    .input
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match self.resolve(p) {
                    Ok(abs) => {
                        if let Some(parent) = abs.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        match std::fs::write(&abs, content) {
                            Ok(_) => Ok(ToolResult {
                                output: format!("已写入 {} 字节到 {p}", content.len()),
                                success: true,
                                blocks_digest: String::new(),
                            }),
                            Err(e) => Ok(ToolResult {
                                output: format!("写入失败: {e}"),
                                success: false,
                                blocks_digest: String::new(),
                            }),
                        }
                    }
                    Err(e) => Ok(ToolResult {
                        output: e.to_string(),
                        success: false,
                        blocks_digest: String::new(),
                    }),
                }
            }

            "list_dir" => {
                let p = req.input.get("path").and_then(|v| v.as_str()).unwrap_or("");
                match self.resolve(if p.is_empty() { "." } else { p }) {
                    Ok(abs) => match std::fs::read_dir(&abs) {
                        Ok(rd) => {
                            let mut names: Vec<String> = rd
                                .filter_map(|e| e.ok())
                                .map(|e| {
                                    let n = e.file_name().to_string_lossy().to_string();
                                    if e.path().is_dir() {
                                        format!("{n}/")
                                    } else {
                                        n
                                    }
                                })
                                .collect();
                            names.sort();
                            Ok(ToolResult {
                                output: names.join("\n"),
                                success: true,
                                blocks_digest: String::new(),
                            })
                        }
                        Err(e) => Ok(ToolResult {
                            output: format!("列目录失败: {e}"),
                            success: false,
                            blocks_digest: String::new(),
                        }),
                    },
                    Err(e) => Ok(ToolResult {
                        output: e.to_string(),
                        success: false,
                        blocks_digest: String::new(),
                    }),
                }
            }

            "bash" => {
                let cmd = req
                    .input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // 危险命令直接拒绝（演示守卫与工具层的双重防护）
                if is_dangerous(cmd) {
                    return Ok(ToolResult {
                        output: "拒绝执行：命令包含递归删除或格式化操作".into(),
                        success: false,
                        blocks_digest: String::new(),
                    });
                }
                let out = Command::new("bash")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(&self.root)
                    .output();
                match out {
                    Ok(o) => {
                        let mut text = String::from_utf8_lossy(&o.stdout).to_string();
                        let err = String::from_utf8_lossy(&o.stderr);
                        if !err.is_empty() {
                            text.push_str("\n[stderr] ");
                            text.push_str(&err);
                        }
                        let ok = o.status.success();
                        if !ok {
                            text.push_str(&format!("\n[退出码] {:?}", o.status.code()));
                        }
                        Ok(ToolResult {
                            output: truncate(text, 8000),
                            success: ok,
                            blocks_digest: String::new(),
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        output: format!("执行失败: {e}"),
                        success: false,
                        blocks_digest: String::new(),
                    }),
                }
            }

            other => Ok(ToolResult {
                output: format!("未知工具: {other}"),
                success: false,
                blocks_digest: String::new(),
            }),
        }
    }
}

fn is_dangerous(cmd: &str) -> bool {
    let c = cmd.trim();
    c.contains("rm -rf /")
        || c.contains("rm -rf /*")
        || c.contains("mkfs")
        || c.contains("dd if=")
        || c.contains(":(){:|:&};:")
}

fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}\n…[截断，原文 {} 字符]", s.chars().count())
    }
}
