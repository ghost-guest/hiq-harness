//! 会话存储。
//!
//! **来源与改动**（本项目第一原则要求写明出处）：
//!
//! 会话文件的**磁盘格式照搬 pi**，因为 orbit 的 `crates/orbit-pi/src/sessions.rs`
//! 已经在按它读取（首行 `{"type":"session","id":…,"cwd":…}`，
//! 其后是 `{"type":"message","message":{"role":"user","content":…}}`）。
//! 保持格式一致意味着 **`sessions.rs` 的解析与侧栏渲染一行都不用改**，
//! 只需把扫描目录从 `~/.pi/agent/sessions` 换成本项目的目录。
//!
//! 本文件是新增的写入端：harness 内核原本只写 `.harness/events.jsonl`
//! （事件流，不是对话），这里把它另外落成 pi 格式的会话文件，
//! 让侧栏、历史消息、分叉、克隆这些界面能力有数据可读。
//!
//! 目录布局（与 pi 一致）：`<sessions_root>/<workspace-slug>/<timestamp>_<id>.jsonl`

use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

/// 会话文件里的一行。
///
/// pi 的 entry 类型很多（`message` / `model_change` / `custom` / …），
/// 这里只产出 UI 实际读取的两种：会话头与消息。
const ENTRY_KIND_SESSION: &str = "session";
const ENTRY_KIND_MESSAGE: &str = "message";

/// 一个会话在磁盘上的位置与元信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFile {
    pub path: PathBuf,
    pub id: String,
    pub cwd: PathBuf,
}

/// 会话库的根目录。
///
/// 与 orbit 的 `sessions::sessions_dir()` 配对：那边读，这边写，
/// 必须指向同一个位置，否则侧栏看不到新会话。
///
/// 测试通过 [`create_session_in`] / [`list_sessions_in`] 等显式传根目录的
/// 变体注入临时目录 —— 环境变量是进程级共享状态，并行测试互相覆盖会
/// 造成假失败，所以核心逻辑都接受 `root` 参数，只有面向生产的两三个
/// 便捷函数才读全局。
pub fn sessions_root() -> PathBuf {
    // 放在项目的 `.harness/sessions` 下，与内核的其它状态同处一地
    // （AGENTS.md：记忆与状态存项目内，不存家目录）。
    std::env::var("HARNESS_SESSIONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let base = std::env::var("HARNESS_WORKDIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            base.join(".harness/sessions")
        })
}

/// 工作区在会话库里的目录名。
///
/// pi 的做法是「工作区绝对路径把 `/` 换成 `-`」，`sessions.rs` 的模块注释
/// 写明了这一点（`<workspace-slug>` is the workspace absolute path with
/// `/` → `-`）。照抄，保证两边算出同一个目录。
pub fn workspace_slug(workspace: &Path) -> String {
    workspace.to_string_lossy().replace('/', "-")
}

/// 新建一个会话文件，写入会话头。
pub fn create_session(workspace: &Path, name: Option<&str>) -> std::io::Result<SessionFile> {
    create_session_in(&sessions_root(), workspace, name)
}

/// [`create_session`] 的显式根目录变体（测试用，避免依赖全局状态）。
pub fn create_session_in(
    sessions_root: &Path,
    workspace: &Path,
    name: Option<&str>,
) -> std::io::Result<SessionFile> {
    let root = sessions_root.join(workspace_slug(workspace));
    fs::create_dir_all(&root)?;

    let id = new_session_id();
    let path = root.join(format!("{}_{}.jsonl", now_ms(), id));
    let mut file = fs::File::create(&path)?;

    let header = json!({
        "type": ENTRY_KIND_SESSION,
        "version": 3,
        "id": id,
        "timestamp": now_rfc3339(),
        "cwd": workspace.to_string_lossy(),
        "name": name,
    });
    writeln!(file, "{header}")?;
    file.flush()?;

    Ok(SessionFile {
        path,
        id,
        cwd: workspace.to_path_buf(),
    })
}

/// 往会话文件追加一条消息。
///
/// `role` 取 `user` / `assistant`；`content` 是 pi 的 content block 数组形态。
pub fn append_message(path: &Path, role: &str, text: &str) -> std::io::Result<()> {
    let entry = json!({
        "type": ENTRY_KIND_MESSAGE,
        "timestamp": now_rfc3339(),
        "message": {
            "role": role,
            "content": [{ "type": "text", "text": text }],
            "timestamp": now_ms(),
        }
    });
    let mut file = OpenOptions::new().append(true).open(path)?;
    writeln!(file, "{entry}")?;
    file.flush()
}

/// 读一个会话文件里的消息（跳过会话头与无法解析的行）。
///
/// 返回的形状与 pi 一致：`[{"role":"user","content":[…],"timestamp":…}, …]`。
pub fn read_messages(path: &Path) -> std::io::Result<Vec<Value>> {
    let file = fs::File::open(path)?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some(ENTRY_KIND_MESSAGE) {
            if let Some(m) = v.get("message") {
                out.push(m.clone());
            }
        }
    }
    Ok(out)
}

/// 读会话头（`id` / `cwd` / `name`）。
pub fn read_header(path: &Path) -> std::io::Result<Value> {
    let file = fs::File::open(path)?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first)?;
    let v: Value = serde_json::from_str(first.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(v)
}

/// 改会话头里的 `name`（UI 的标题来源）。
///
/// 会话头必须在第一行，所以这里整体读入、改第一行、写回。
/// 会话文件通常只有几百 KB，这个开销可以接受。
pub fn set_session_name(path: &Path, name: &str) -> std::io::Result<()> {
    let body = fs::read_to_string(path)?;
    let mut lines = body.lines();
    let first = lines.next().unwrap_or_default();
    let rest: String = lines.map(|l| format!("{l}\n")).collect();

    let mut header: Value = serde_json::from_str(first)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    header["name"] = Value::String(name.to_string());

    let mut out = String::new();
    out.push_str(&serde_json::to_string(&header).unwrap_or_default());
    out.push('\n');
    out.push_str(&rest);
    fs::write(path, out)
}

/// 列出所有会话文件，最新的在前。
pub fn list_sessions() -> Vec<SessionFile> {
    list_sessions_in(&sessions_root())
}

/// [`list_sessions`] 的显式根目录变体。
pub fn list_sessions_in(sessions_root: &Path) -> Vec<SessionFile> {
    let mut out = Vec::new();
    let Ok(groups) = fs::read_dir(sessions_root) else {
        return out;
    };
    for group in groups.flatten() {
        let Ok(files) = fs::read_dir(group.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                if let Ok(h) = read_header(&path) {
                    let id = h
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let cwd = h
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                        .unwrap_or_default();
                    out.push(SessionFile { path, id, cwd });
                }
            }
        }
    }
    // 文件名以毫秒时间戳开头，字典序即时间序。
    out.sort_by(|a, b| b.path.cmp(&a.path));
    out
}

/// 克隆一个会话：复制全部内容到新文件，换新 id。
pub fn clone_session(src: &Path, workspace: &Path) -> std::io::Result<SessionFile> {
    let created = create_session(workspace, None)?;
    let body = fs::read_to_string(src)?;
    // 丢掉源文件的第一行（会话头），其余原样追加。
    let mut lines = body.lines();
    lines.next();
    let rest: String = lines.map(|l| format!("{l}\n")).collect();
    let mut file = OpenOptions::new().append(true).open(&created.path)?;
    file.write_all(rest.as_bytes())?;
    file.flush()?;
    Ok(created)
}

fn new_session_id() -> String {
    // 时间戳 + 进程 id，够唯一（pi 用 uuid；这里不引入依赖）。
    format!("{}-{}", now_ms(), std::process::id())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn now_rfc3339() -> String {
    // 只用于展示与排序，用 epoch 秒的可读形态即可。
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例用自己的临时根目录，不碰进程级环境变量 ——
    /// 并行测试共享 `std::env`，互相覆盖会造成假失败。
    fn fresh_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sess-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn slug_replaces_slashes_like_pi() {
        // sessions.rs 的注释：workspace 绝对路径 `/` → `-`
        assert_eq!(workspace_slug(Path::new("/Users/a/proj")), "-Users-a-proj");
    }

    #[test]
    fn create_writes_pi_compatible_header() {
        let root = fresh_root("create");
        let ws = fresh_root("create-ws");
        let s = create_session_in(&root, &ws, Some("测试")).unwrap();

        let h = read_header(&s.path).unwrap();
        assert_eq!(h["type"], "session");
        assert_eq!(h["cwd"], ws.to_string_lossy().as_ref());
        assert!(h["id"].as_str().unwrap().len() > 3);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn messages_round_trip_in_pi_shape() {
        let root = fresh_root("msg");
        let ws = fresh_root("msg-ws");
        let s = create_session_in(&root, &ws, None).unwrap();

        append_message(&s.path, "user", "你好").unwrap();
        append_message(&s.path, "assistant", "收到").unwrap();

        let msgs = read_messages(&s.path).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        // content 必须是 content block 数组（sessions.rs 的 first_user_text 依赖它）
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "你好");
        assert_eq!(msgs[1]["role"], "assistant");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn list_returns_newest_first() {
        let root = fresh_root("list");
        let ws = fresh_root("list-ws");
        let a = create_session_in(&root, &ws, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = create_session_in(&root, &ws, None).unwrap();

        let listed = list_sessions_in(&root);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, b.id, "最新的排在最前");
        assert_eq!(listed[1].id, a.id);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn clone_copies_messages_but_not_header() {
        let root = fresh_root("clone");
        let ws = fresh_root("clone-ws");
        let src = create_session_in(&root, &ws, None).unwrap();
        append_message(&src.path, "user", "原始内容").unwrap();

        let copy = clone_session(&src.path, &ws).unwrap();
        assert_ne!(copy.id, src.id);
        let msgs = read_messages(&copy.path).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"][0]["text"], "原始内容");
        let heads = fs::read_to_string(&copy.path)
            .unwrap()
            .lines()
            .filter(|l| l.contains("\"type\":\"session\""))
            .count();
        assert_eq!(heads, 1, "副本只能有一个会话头");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let root = fresh_root("bad");
        let ws = fresh_root("bad-ws");
        let s = create_session_in(&root, &ws, None).unwrap();
        append_message(&s.path, "user", "ok").unwrap();
        let mut f = OpenOptions::new().append(true).open(&s.path).unwrap();
        writeln!(f, "{{not json").unwrap();
        f.flush().unwrap();

        let msgs = read_messages(&s.path).unwrap();
        assert_eq!(msgs.len(), 1, "坏行被跳过，好行仍可读");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn set_name_rewrites_only_the_header() {
        let root = fresh_root("name");
        let ws = fresh_root("name-ws");
        let s = create_session_in(&root, &ws, None).unwrap();
        append_message(&s.path, "user", "正文不动").unwrap();
        set_session_name(&s.path, "新标题").unwrap();

        let h = read_header(&s.path).unwrap();
        assert_eq!(h["name"], "新标题");
        let msgs = read_messages(&s.path).unwrap();
        assert_eq!(msgs.len(), 1, "改名字不能丢消息");
        assert_eq!(msgs[0]["content"][0]["text"], "正文不动");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&ws);
    }
}
