//! 运行时来源（原 `pi_install`：pi 的安装情况）。
//!
//! **改造说明**：本文件替换上游 orbit 的 `crates/orbit-pi/src/pi_install.rs`
//! （944 行）。上游那个模块查的是「pi 装在哪里、怎么装的、能不能就地升级」——
//! 探测 npm 全局包、managed install 标记、`pi --version` 输出等。
//!
//! 本项目**没有外部 agent 二进制**：运行时是编译进这个应用的 harness 内核
//! （见 `orbit_rpc::bridge`），进程内调用，没有子进程、没有包管理器痕迹、
//! 也没有版本可探测。
//!
//! 因此这里保留上游的类型与函数**签名**（onboarding / settings / view 都在用，
//! 改签名会连带改十几个界面文件），但实现如实反映「内嵌」这个事实：
//! `InstallKind::Embedded` 是唯一可能的取值，`bin` 恒为 `None`
//! （没有独立可执行文件），版本号取自本应用自身的 `CARGO_PKG_VERSION`。
//!
//! 上游 AGENTS.md 第 5 条：协议给不出的能力显示为不可用，不伪造。
//! 这里同理 —— 不假装探测出一个不存在的安装。

// 上游的若干符号（`is_managed` / `is_npm` / `install_command_for` /
// `NPM_INSTALL_COMMAND` 等）在本项目里没有调用点 —— 内嵌内核不区分
// npm / managed / 包管理器这些安装形态。保留它们是为了让仍在引用
// 本模块的界面代码（onboarding、settings）改动最小，
// 也便于日后对照上游 diff。有意的死代码。
#![allow(dead_code)]

use std::path::PathBuf;

/// 上游的「装 pi 的一行命令」。本项目没有要装的运行时，
/// 保留常量只因为 `onboarding.rs` 的测试与提示文案引用它。
pub const INSTALL_COMMAND: &str = "";
pub const INSTALL_COMMAND_WINDOWS: &str = "";
pub const NPM_INSTALL_COMMAND: &str = "";

/// 运行时是怎么来的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// 唯一取值：内核编译在本应用里，没有外部安装。
    Embedded,
}

impl InstallKind {
    pub fn is_managed(&self) -> bool {
        false
    }

    pub fn is_npm(&self) -> bool {
        false
    }

    /// 界面上展示的来源描述。
    pub fn label(&self) -> &'static str {
        "内嵌（随应用编译）"
    }
}

/// 运行时安装情况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    /// 上游这里是 pi 的可执行文件路径。内嵌内核没有独立可执行文件，
    /// 恒为 `None` —— 界面据此不显示「打开所在目录」这类操作。
    pub bin: Option<PathBuf>,
    /// 版本号：取本应用自身的版本（内核与界面同属一个二进制）。
    pub version: Option<String>,
    pub kind: InstallKind,
    /// 上游问「PATH 上的 pi 是否就是这个安装」。内嵌内核与 PATH 无关，恒 `true`。
    pub on_path: bool,
}

impl Installation {
    /// 内嵌运行时的唯一形态。
    pub fn embedded() -> Self {
        Self {
            bin: None,
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            kind: InstallKind::Embedded,
            on_path: true,
        }
    }

    /// 运行时是否可用。内嵌内核永远可用 —— 没有「没装」这个状态，
    /// 所以界面上的「去安装」提示一律不出现。
    pub fn installed(&self) -> bool {
        true
    }

    /// 上游测试用的「没装」形态；这里等价于内嵌（永远可用）。
    #[cfg(test)]
    pub fn not_installed() -> Self {
        Self::embedded()
    }
}

impl Default for Installation {
    fn default() -> Self {
        Self::embedded()
    }
}

/// 上游按可执行文件路径判断安装形态。内嵌内核不看路径，直接回内嵌。
pub fn detect_from_bin(_bin: Option<PathBuf>) -> Installation {
    Installation::embedded()
}

/// 上游返回「装 pi 的命令」。没有要装的东西，回空串，
/// 调用方（onboarding 的提示行）会据此显示为空。
pub fn install_command() -> &'static str {
    INSTALL_COMMAND
}

/// 上游按平台给安装命令；同样为空。
pub fn install_command_for(_os: &str) -> &'static str {
    INSTALL_COMMAND
}

/// 上游刷新 pi 的模型目录（读它的 provider 配置）。
///
/// harness 的模型来自 `ExecConfig`（base_url + model 环境变量），
/// 没有目录可刷新，所以这是空操作。保留签名供 `providers.rs` 调用。
pub fn run_model_catalog_refresh() -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_reports_embedded() {
        let i = detect_from_bin(Some(PathBuf::from("/usr/local/bin/pi")));
        assert_eq!(i.kind, InstallKind::Embedded);
        assert!(i.bin.is_none(), "内嵌内核没有独立可执行文件");
        assert!(i.on_path);
    }

    #[test]
    fn version_follows_the_app() {
        let i = Installation::embedded();
        assert_eq!(i.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn never_claims_npm_or_managed() {
        let k = InstallKind::Embedded;
        assert!(!k.is_npm());
        assert!(!k.is_managed());
    }

    #[test]
    fn install_commands_are_empty() {
        // 没有要装的运行时：文案为空，界面不会显示一条装 pi 的命令
        assert!(install_command().is_empty());
        assert!(install_command_for("macos").is_empty());
    }
}
