//! Onboarding / setup dependency check.
//!
//! Orbit drives the `pi` CLI as a child process. Before the app is usable,
//! the runtime pieces it needs must be installed: the `pi` coding agent
//! itself, the `node` runtime that runs it (pi is a Node script), and `git`
//! for the branch picker / diff panel. This module probes for each binary,
//! reports which are missing, and carries the install command to show the
//! user.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute install dirs probed when a binary isn't on `PATH`. A bundled
/// `.app` launches with a minimal PATH, so we also check Homebrew-style dirs.
const SEARCH_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/usr/bin",
    "/bin",
];

/// Home-relative shim dirs (version managers, pnpm, bun, yarn, …). These are
/// where a manager puts a `node` launcher even though it isn't a real binary
/// in a system dir, so a bundled `.app` can't see it on PATH.
const HOME_SEARCH_SUBDIRS: &[&str] = &[
    ".volta/bin",
    ".local/share/mise/shims",
    ".asdf/shims",
    ".local/bin",
    "Library/pnpm/bin",
    ".bun/bin",
    ".npm-global/bin",
    // pi's own shim dir (it also carries `fd`); a recent pi may install its
    // launcher here, and it is on the user's shell PATH but not a `.app`'s.
    ".pi/agent/bin",
    ".yarn/bin",
];

/// Version managers that keep each Node version in its own numbered dir.
/// Each entry is `(base, subdir)`; the binary is `base/<version>/subdir/node`.
const VERSION_MANAGER_GLOBS: &[(&str, &str)] = &[
    (".nvm/versions/node", "bin"),
    (".local/share/fnm/node-versions", "installation/bin"),
];

/// Executable suffixes to probe on Windows, most-preferred first.
///
/// Windows resolves executables by extension, and npm's global installs ship
/// three files per command: an extensionless `pi` that is a POSIX *shell*
/// script, the `pi.cmd` shim `Command` can actually spawn, and `pi.ps1`.
/// Probing the bare name first would therefore find a `pi` that can never
/// report a version, and would miss `node.exe` entirely.
#[cfg(windows)]
const WINDOWS_EXEC_SUFFIXES: &[&str] = &["exe", "cmd", "bat"];

/// One runtime dependency checked at startup.
#[derive(Debug, Clone)]
pub struct Dependency {
    /// Human name shown in the list.
    pub name: &'static str,
    /// Executable to probe on disk (may differ from `name`, e.g. `node`).
    /// Read by tests; in the UI the install hint already names the binary.
    #[allow(dead_code)]
    pub bin: &'static str,
    /// True when Orbit can't run without it (vs. a nice-to-have).
    pub required: bool,
    /// Whether the binary was found.
    pub installed: bool,
    /// First line of `<bin> --version`, when installed.
    pub version: Option<String>,
    /// Shell command that installs it *on this host* (see [`install_hint`]).
    pub install_hint: &'static str,
    /// Translation key for the one-line explanation. Stored as a key (not the
    /// translated text) so the setup page follows a language change without
    /// re-probing the host.
    pub detail_key: &'static str,
}

/// A read-only fact about the machine, shown under the dependency list.
///
/// Not a dependency — there is nothing to install — but the OS, and the two
/// directories Orbit and pi read and write, are what the install commands
/// above have to work against, and they are the first thing to check when the
/// app comes up with an empty sidebar.
pub struct HostFact {
    pub label: String,
    pub value: String,
    /// Trailing state, e.g. `not created yet`.
    pub note: Option<String>,
    /// True when the fact is a problem rather than a statement.
    pub alert: bool,
}

/// Probe every known dependency, in display order (required first).
pub fn check_dependencies() -> Vec<Dependency> {
    vec![
        embedded_runtime("pi", "onboarding.pi_detail"),
        dependency("node", "Node.js", true, "onboarding.node_detail"),
        dependency("git", "git", false, "onboarding.git_detail"),
    ]
}

/// 运行时那一行：内核编译在本应用里（见 `pi_install`），**天然就在**。
///
/// 之前这一行走的是 `dependency("pi", ..)` —— 它去磁盘找 `pi` 二进制。
/// 内嵌内核没有独立可执行文件，于是得到「未安装 + 空安装命令」：
/// 设置页显示一个必装依赖缺失，给的却是一个**空代码框**和一个只会
/// 复制空串的按钮。用户既看不懂也没法修。
///
/// 诚实的模型是：它不需要安装，因为它在。版本号取本应用自己的版本。
fn embedded_runtime(name: &'static str, detail_key: &'static str) -> Dependency {
    Dependency {
        name,
        bin: name,
        required: true,
        installed: true,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        // 没有可安装的东西，所以没有命令。UI 在 `installed: true` 时
        // 走的是对勾那一支，不会渲染这个空串。
        install_hint: "",
        detail_key,
    }
}

/// The host and the paths Orbit depends on, for the setup page. Takes the
/// host the caller already probed ([`crate::platform::host`]) so the page
/// never runs a probe while rendering.
pub fn host_facts(host: &crate::platform::Host) -> Vec<HostFact> {
    let platform = HostFact {
        label: tr!("onboarding.platform"),
        value: format!("{} · {}", host.label, host.arch),
        alert: host.unsupported.is_some(),
        note: host.unsupported.clone(),
    };
    vec![
        platform,
        path_fact(
            tr!("onboarding.pi_sessions"),
            crate::sessions::sessions_dir(),
        ),
        path_fact(
            tr!("onboarding.orbit_config"),
            crate::platform::home_dir().join(".orbit-pi"),
        ),
    ]
}

/// Rows for the pi CLI install itself: how it was installed, where it
/// resolves, and which version. The method is the first thing to check when
/// an update refuses to apply (npm vs. the official managed install vs. a
/// package manager Orbit must not touch).
pub fn pi_install_facts(installation: &crate::pi_install::Installation) -> Vec<HostFact> {
    // 改造：运行时来源只有「内嵌」一种（见 `pi_install` 模块头），
    // 上游那套 npm / managed / 其他包管理器的分支不复存在。
    let method = installation.kind.label().to_string();
    let path = installation
        .bin
        .as_ref()
        .map(|bin| crate::usage::format::short_path(&bin.to_string_lossy()).replace('\\', "/"))
        .unwrap_or_else(|| "—".to_string());
    let version = installation
        .version
        .clone()
        .unwrap_or_else(|| "—".to_string());
    vec![
        HostFact {
            label: tr!("onboarding.pi_install"),
            value: method,
            note: None,
            alert: !installation.installed(),
        },
        HostFact {
            label: tr!("onboarding.pi_path"),
            value: path,
            note: None,
            alert: false,
        },
        HostFact {
            label: tr!("onboarding.pi_version"),
            value: version,
            note: None,
            alert: false,
        },
    ]
}

/// A row for a directory the app expects to exist. A missing one is a
/// statement, not an error: pi and Orbit both create theirs on first use.
fn path_fact(label: String, path: PathBuf) -> HostFact {
    let note = (!path.is_dir()).then(|| tr!("onboarding.not_created_yet"));
    HostFact {
        label,
        // Displayed with forward slashes on every platform — the store path is
        // built as `.pi/agent/sessions`, so Windows would otherwise render a
        // mix of both separators.
        value: crate::usage::format::short_path(&path.to_string_lossy()).replace('\\', "/"),
        note,
        alert: false,
    }
}

/// True when every required dependency is installed.
pub fn all_required_installed(deps: &[Dependency]) -> bool {
    deps.iter().filter(|d| d.required).all(|d| d.installed)
}

/// Number of required dependencies that are still missing.
pub fn missing_required_count(deps: &[Dependency]) -> usize {
    deps.iter().filter(|d| d.required && !d.installed).count()
}

fn dependency(
    bin: &'static str,
    name: &'static str,
    required: bool,
    detail_key: &'static str,
) -> Dependency {
    let found = locate(bin);
    let version = found.as_deref().and_then(version_of);
    Dependency {
        name,
        bin,
        required,
        installed: found.is_some(),
        version,
        install_hint: install_hint(bin),
        detail_key,
    }
}

/// The command that installs `bin` on this host. `pi` uses the project's
/// official installer wherever pi ships one — macOS and Linux through
/// `install.sh`, Windows through the documented `install.ps1` PowerShell
/// one-liner — and pi's documented npm form on anything else. Orbit never runs
/// npm itself, so that last one is only ever a hint. `node` and `git` come from
/// whatever package manager the OS ships: `winget` on Windows 10/11, Homebrew
/// on macOS (the manager a Mac user with neither already has), and apt on
/// Linux.
fn install_hint(bin: &str) -> &'static str {
    let pi = crate::pi_install::install_command();
    #[cfg(target_os = "macos")]
    return match bin {
        "pi" => pi,
        "node" => "brew install node",
        _ => "brew install git",
    };
    #[cfg(windows)]
    return match bin {
        "pi" => pi,
        "node" => "winget install OpenJS.NodeJS.LTS",
        _ => "winget install Git.Git",
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    return match bin {
        "pi" => pi,
        "node" => "sudo apt install nodejs npm",
        _ => "sudo apt install git",
    };
}

/// Locate a binary by name, honoring `PI_BIN` for `pi`, then `PATH`, then
/// the common install dirs, Home-relative shim dirs, version-manager
/// version dirs, and the Windows fallbacks. This keeps detection working
/// from a bundled `.app` whose PATH doesn't include a user's node install
/// (e.g. nvm, volta, mise, Scoop).
pub(crate) fn locate(name: &str) -> Option<PathBuf> {
    if name == "pi" {
        if let Ok(bin) = std::env::var("PI_BIN") {
            if !bin.is_empty() {
                let p = PathBuf::from(&bin);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if let Some(found) = existing_in(&dir, name) {
                return Some(found);
            }
        }
    }
    for dir in SEARCH_DIRS {
        if let Some(found) = existing_in(Path::new(dir), name) {
            return Some(found);
        }
    }
    if let Some(home) = home_dir() {
        if let Some(found) = locate_in_home(&home, name) {
            return Some(found);
        }
    }
    // Windows package-manager dirs (Scoop, Chocolatey, nvm-windows, fnm) and
    // the registry PATH, which sees an install made after Orbit launched —
    // without it Refresh could never clear a missing row (issue #124).
    orbit_rpc::fallback_bin_dirs()
        .iter()
        .find_map(|dir| existing_in(dir, name))
}

/// Search the home-relative shim dirs and version-manager version dirs for a
/// binary. Split out so tests can point it at a temp dir.
fn locate_in_home(home: &Path, name: &str) -> Option<PathBuf> {
    for sub in HOME_SEARCH_SUBDIRS {
        if let Some(found) = existing_in(&home.join(sub), name) {
            return Some(found);
        }
    }
    for (base, sub) in VERSION_MANAGER_GLOBS {
        let base = home.join(base);
        let Ok(versions) = std::fs::read_dir(&base) else {
            continue;
        };
        for version in versions.flatten() {
            if let Some(found) = existing_in(&version.path().join(sub), name) {
                return Some(found);
            }
        }
    }
    None
}

/// `dir/name` as it exists on disk, trying the Windows executable suffixes
/// first there (see [`WINDOWS_EXEC_SUFFIXES`]) and the bare name last.
fn existing_in(dir: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    for suffix in WINDOWS_EXEC_SUFFIXES {
        let candidate = dir.join(format!("{name}.{suffix}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

/// The user's home directory.
fn home_dir() -> Option<PathBuf> {
    crate::platform::home_dir_opt()
}

/// First line of `<bin> --version`, trimmed; `None` if it fails or is empty.
pub(crate) fn version_of(bin: &Path) -> Option<String> {
    let mut command = Command::new(bin);
    command.arg("--version");
    // pi's launcher is `#!/usr/bin/env node`, so the probe needs the same
    // augmented PATH as spawns: a bundled `.app` PATH has no `node` dir, and
    // the raw `env: node: ...` stderr would otherwise read as a version.
    command.env("PATH", orbit_rpc::augmented_path(bin.parent()));
    orbit_rpc::hide_console(&mut command);
    let output = command.output().ok()?;
    let text = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    String::from_utf8_lossy(&text)
        .lines()
        .next()
        .map(str::trim)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hint is what a user copies, so it has to be a command for the OS
    /// they are on — the page used to offer Homebrew on Windows.
    #[test]
    fn install_hints_match_the_host() {
        // `pi` installs through its official installer wherever pi ships one;
        // Orbit must never advertise an npm install on those platforms.
        assert_eq!(install_hint("pi"), crate::pi_install::install_command());
        assert!(
            !install_hint("pi").contains("npm install"),
            "{}",
            install_hint("pi")
        );
        #[cfg(target_os = "macos")]
        assert_eq!(install_hint("pi"), crate::pi_install::INSTALL_COMMAND);
        #[cfg(windows)]
        assert_eq!(
            install_hint("pi"),
            crate::pi_install::INSTALL_COMMAND_WINDOWS
        );
        #[cfg(target_os = "macos")]
        {
            assert_eq!(install_hint("node"), "brew install node");
            assert_eq!(install_hint("git"), "brew install git");
        }
        #[cfg(windows)]
        {
            assert!(install_hint("node").starts_with("winget install"));
            assert!(install_hint("git").starts_with("winget install"));
        }
        // 运行时不走安装命令（内嵌），其余依赖必须有。
        assert!(
            install_hint("pi").trim().is_empty(),
            "内嵌运行时不该有安装命令，实际：{}",
            install_hint("pi")
        );
        for bin in ["node", "git"] {
            assert!(!install_hint(bin).trim().is_empty(), "{bin} 缺安装命令");
        }
    }

    /// 只有**可能要装**的依赖才带安装命令，而且命令里要点名那个二进制
    /// （否则用户复制了另一行的命令）。内嵌运行时是例外：它在，不用装。
    #[test]
    fn only_installable_dependencies_carry_an_install_hint() {
        for dep in check_dependencies() {
            if dep.installed && dep.install_hint.is_empty() {
                // 内嵌运行时：必须已安装，且没有命令。有命令反而是编的。
                assert_eq!(dep.name, "pi", "{} 没装又没安装命令", dep.name);
                continue;
            }
            assert!(!dep.install_hint.trim().is_empty(), "{}", dep.name);
            // Case-insensitively: installers spell the package, not the
            // executable (`winget install Git.Git` for `git`).
            assert!(
                dep.install_hint.to_lowercase().contains(dep.bin),
                "{}: {}",
                dep.name,
                dep.install_hint
            );
        }
    }

    /// 内嵌运行时必须是「已安装」，否则设置页会给一个空安装命令。
    #[test]
    fn the_embedded_runtime_is_always_present() {
        let pi = check_dependencies()
            .into_iter()
            .find(|d| d.name == "pi")
            .expect("必须有 pi 这一行");
        assert!(pi.installed, "内嵌运行时永远在，不该显示成未安装");
        assert!(pi.version.is_some(), "版本取本应用版本");
        assert!(pi.install_hint.is_empty(), "没有可安装的东西");
    }

    /// The setup page reports the platform and the two directories the app
    /// needs to read and write — the paths that decide whether the sidebar
    /// has anything in it.
    /// The Pi install panel reports the method honestly, including the npm
    /// case the updater has to migrate and the missing case the setup page
    /// exists for.
    #[test]
    fn host_facts_cover_the_platform_and_the_stores() {
        let host = crate::platform::host();
        let facts = host_facts(&host);
        assert_eq!(facts.len(), 3);

        let platform = &facts[0];
        assert_eq!(platform.label, "Platform");
        assert!(platform.value.contains(&host.label), "{}", platform.value);
        assert!(platform.value.contains(host.arch), "{}", platform.value);
        assert_eq!(platform.alert, host.unsupported.is_some());

        // 用翻译键取值，别写死。产品从 Orbit 改名成 hiq 之后，
        // 写死 `"Orbit config"` 的那条就永远找不到了。
        let sessions = facts
            .iter()
            .find(|f| f.label == tr!("onboarding.pi_sessions"))
            .expect("必须有 sessions 那一行");
        assert!(sessions.value.contains("sessions"), "{}", sessions.value);
        // Displayed, not used: separators are normalized so a Windows user
        // never sees `.pi/agent\sessions`.
        assert!(!sessions.value.contains('\\'), "{}", sessions.value);
        let config = facts
            .iter()
            .find(|f| f.label == tr!("onboarding.orbit_config"))
            .expect("必须有配置目录那一行");
        assert!(config.value.contains(".orbit-pi"), "{}", config.value);
        // A missing directory is reported, not treated as an error.
        for fact in facts.iter().skip(1) {
            assert!(!fact.alert);
            if let Some(note) = &fact.note {
                assert_eq!(note, "not created yet");
            }
        }
    }

    #[test]
    fn reports_expected_dependencies() {
        let deps = check_dependencies();
        assert_eq!(deps.len(), 3);
        assert!(deps.iter().any(|d| d.name == "pi"));
        assert!(deps.iter().any(|d| d.name == "Node.js"));
        assert!(deps.iter().any(|d| d.name == "git"));
        // Only pi and Node.js are required.
        assert!(deps
            .iter()
            .filter(|d| d.required)
            .all(|d| d.name == "pi" || d.name == "Node.js"));
        // Every installed dep has a version line from `<bin> --version`.
        for d in &deps {
            if d.installed {
                assert!(d.version.is_some(), "{} missing version", d.name);
            } else {
                assert!(!d.install_hint.is_empty());
            }
        }
    }

    #[test]
    fn node_probes_the_node_binary() {
        // The display name is "Node.js" but the executable is "node". If the
        // binary name were used as the probe name, node would always look
        // missing even when installed.
        let deps = check_dependencies();
        let node = deps.iter().find(|d| d.name == "Node.js").unwrap();
        assert_eq!(node.bin, "node");
        // With node on PATH, the probe must resolve it.
        assert!(
            locate("node").is_some(),
            "expected to find node on this machine"
        );
    }

    #[test]
    fn ready_only_when_all_required_installed() {
        let mut deps = check_dependencies();
        for d in deps.iter_mut() {
            d.installed = true;
        }
        assert!(all_required_installed(&deps));
        assert_eq!(missing_required_count(&deps), 0);

        // Missing a required dep blocks readiness.
        deps.iter_mut().find(|d| d.name == "pi").unwrap().installed = false;
        assert!(!all_required_installed(&deps));
        assert_eq!(missing_required_count(&deps), 1);

        // A missing optional dep (git) never blocks readiness.
        deps.iter_mut().find(|d| d.name == "pi").unwrap().installed = true;
        deps.iter_mut().find(|d| d.name == "git").unwrap().installed = false;
        assert!(all_required_installed(&deps));
        assert_eq!(missing_required_count(&deps), 0);
    }

    /// A `pi` shim under the npm global prefix (the one `~/.npmrc` can point
    /// at) or pi's own `~/.pi/agent/bin` must be found without PATH — a
    /// bundled `.app` never inherits the shell PATH that contains them.
    #[test]
    fn finds_pi_in_npm_global_and_pi_shim_dirs() {
        let home = std::env::temp_dir().join("orbit-onboarding-pi-dirs-test");
        let _ = std::fs::remove_dir_all(&home);
        for sub in [".npm-global/bin", ".pi/agent/bin"] {
            let dir = home.join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            let pi = dir.join("pi");
            std::fs::write(&pi, "#!/bin/sh\necho pi\n").unwrap();
            assert_eq!(locate_in_home(&home, "pi"), Some(pi), "{sub}");
            std::fs::remove_file(dir.join("pi")).unwrap();
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn finds_node_in_version_manager_dirs() {
        // A fake HOME with an nvm-style version dir: node at
        // ~/.nvm/versions/node/v20.0.0/bin/node.
        let home = std::env::temp_dir().join("orbit-onboarding-test");
        let _ = std::fs::remove_dir_all(&home);
        let nvm = home
            .join(".nvm")
            .join("versions")
            .join("node")
            .join("v20.0.0")
            .join("bin");
        std::fs::create_dir_all(&nvm).unwrap();
        let node = nvm.join("node");
        std::fs::write(&node, "#!/bin/sh\necho node\n").unwrap();

        assert_eq!(locate_in_home(&home, "node"), Some(node));

        let _ = std::fs::remove_dir_all(&home);
    }
}
