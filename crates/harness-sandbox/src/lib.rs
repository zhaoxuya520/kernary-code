//! Kernary 子进程沙箱。
//!
//! Sandbox 与 Approval 是两层独立控制：前者提供操作系统边界，后者决定何时询问用户。

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod windows;

/// 与 Codex 对齐的三种文件系统边界。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
}

impl Display for SandboxMode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        })
    }
}

impl SandboxMode {
    pub fn parse(value: &str) -> Result<Self, SandboxError> {
        match value {
            "read-only" | "readonly" => Ok(Self::ReadOnly),
            "workspace-write" | "workspace" => Ok(Self::WorkspaceWrite),
            "danger-full-access" | "danger" => Ok(Self::DangerFullAccess),
            _ => Err(SandboxError::new(
                "sandbox-mode-invalid",
                format!("不支持的 Sandbox mode：{value}"),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    pub mode: SandboxMode,
    pub backend: String,
    pub filesystem: String,
    pub network: String,
    pub process_tree: String,
    pub available: bool,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxError {
    pub code: String,
    pub message: String,
}

impl SandboxError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for SandboxError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for SandboxError {}

#[derive(Clone, Copy, Debug)]
struct RuntimePolicy {
    mode: SandboxMode,
    network_access: bool,
}

/// 进程工具共享的动态沙箱控制器；切换设置时不会重建 Tool Runtime。
pub struct ProcessSandbox {
    root: PathBuf,
    policy: RwLock<RuntimePolicy>,
}

impl ProcessSandbox {
    pub fn new(
        root: impl AsRef<Path>,
        mode: SandboxMode,
        network_access: bool,
    ) -> Result<Self, SandboxError> {
        let root = fs::canonicalize(root.as_ref())
            .map_err(|error| SandboxError::new("sandbox-root-canonicalize", error.to_string()))?;
        if !root.is_dir() {
            return Err(SandboxError::new(
                "sandbox-root-not-directory",
                root.display().to_string(),
            ));
        }
        Ok(Self {
            root,
            policy: RwLock::new(RuntimePolicy {
                mode,
                network_access,
            }),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn set_policy(&self, mode: SandboxMode, network_access: bool) -> Result<(), SandboxError> {
        let mut policy = self.policy.write().map_err(|_| {
            SandboxError::new("sandbox-policy-poisoned", "Sandbox policy lock poisoned")
        })?;
        *policy = RuntimePolicy {
            mode,
            network_access,
        };
        Ok(())
    }

    pub fn mode(&self) -> Result<SandboxMode, SandboxError> {
        Ok(self
            .policy
            .read()
            .map_err(|_| {
                SandboxError::new("sandbox-policy-poisoned", "Sandbox policy lock poisoned")
            })?
            .mode)
    }

    pub fn status(&self) -> Result<SandboxStatus, SandboxError> {
        let policy = *self.policy.read().map_err(|_| {
            SandboxError::new("sandbox-policy-poisoned", "Sandbox policy lock poisoned")
        })?;
        Ok(platform_status(policy, &self.root))
    }

    /// 在调用方 `env_clear` 之后重放网络策略，避免代理兼容层被清除。
    pub fn apply_environment(&self, command: &mut Command) -> Result<(), SandboxError> {
        let policy = *self.policy.read().map_err(|_| {
            SandboxError::new("sandbox-policy-poisoned", "Sandbox policy lock poisoned")
        })?;
        if policy.mode != SandboxMode::DangerFullAccess {
            apply_offline_environment(command, policy.network_access);
        }
        Ok(())
    }

    /// 构造真正承载目标进程的命令。受限模式绝不静默退化成 unrestricted。
    pub fn command(
        &self,
        executable: &Path,
        arguments: &[String],
        _cwd: &Path,
    ) -> Result<Command, SandboxError> {
        let policy = *self.policy.read().map_err(|_| {
            SandboxError::new("sandbox-policy-poisoned", "Sandbox policy lock poisoned")
        })?;
        if policy.mode == SandboxMode::DangerFullAccess {
            return Ok(Command::new(executable).tap_args(arguments));
        }

        #[cfg(windows)]
        {
            let launcher = std::env::current_exe().map_err(|error| {
                SandboxError::new("sandbox-launcher-current-exe", error.to_string())
            })?;
            let mut command = Command::new(launcher);
            command
                .arg("__kernary-sandbox-exec")
                .arg("--mode")
                .arg(policy.mode.to_string())
                .arg("--root")
                .arg(&self.root)
                .arg("--cwd")
                .arg(_cwd)
                .arg("--")
                .arg(executable)
                .args(arguments);
            apply_offline_environment(&mut command, policy.network_access);
            Ok(command)
        }

        #[cfg(target_os = "linux")]
        {
            let bwrap = find_trusted_bwrap(&self.root).ok_or_else(|| {
                SandboxError::new(
                    "sandbox-bwrap-unavailable",
                    "Linux Sandbox 需要系统 PATH 中、且不位于项目目录内的 bubblewrap (bwrap)",
                )
            })?;
            let mut command = Command::new(bwrap);
            command.args(["--die-with-parent", "--new-session", "--unshare-all"]);
            if policy.network_access {
                command.arg("--share-net");
            }
            command.args(["--ro-bind", "/", "/", "--proc", "/proc", "--dev", "/dev"]);
            if policy.mode == SandboxMode::WorkspaceWrite {
                command.arg("--bind").arg(&self.root).arg(&self.root);
                for protected in [self.root.join(".git"), self.root.join(".harness")] {
                    if protected.exists() {
                        command.arg("--ro-bind").arg(&protected).arg(&protected);
                    }
                }
            }
            command
                .arg("--tmpfs")
                .arg("/tmp")
                .arg("--chdir")
                .arg(_cwd)
                .arg("--")
                .arg(executable)
                .args(arguments);
            apply_offline_environment(&mut command, policy.network_access);
            Ok(command)
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        Err(SandboxError::new(
            "sandbox-platform-unsupported",
            "当前平台尚无 Kernary 系统级 Sandbox；为避免假隔离，命令已拒绝执行",
        ))
    }
}

trait CommandArgsExt {
    fn tap_args(self, arguments: &[String]) -> Self;
}

impl CommandArgsExt for Command {
    fn tap_args(mut self, arguments: &[String]) -> Self {
        self.args(arguments);
        self
    }
}

fn apply_offline_environment(command: &mut Command, network_access: bool) {
    if network_access {
        return;
    }
    // Windows unelevated 后端无法安装按 Token 生效的 WFP 规则；这些变量是明确标注的兼容层，
    // Linux 则同时由 network namespace 做内核级阻断。
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(name, "http://127.0.0.1:9");
    }
    command.env("NO_PROXY", "").env("no_proxy", "");
}

fn platform_status(policy: RuntimePolicy, _workspace: &Path) -> SandboxStatus {
    if policy.mode == SandboxMode::DangerFullAccess {
        return SandboxStatus {
            mode: policy.mode,
            backend: "none".to_owned(),
            filesystem: "unrestricted".to_owned(),
            network: "unrestricted".to_owned(),
            process_tree: process_tree_name().to_owned(),
            available: true,
            warning: Some("危险模式：子进程不受文件系统或网络边界约束".to_owned()),
        };
    }

    #[cfg(windows)]
    {
        SandboxStatus {
            mode: policy.mode,
            backend: "windows-restricted-token".to_owned(),
            filesystem: match policy.mode {
                SandboxMode::ReadOnly => "kernel-enforced project read-only + isolated temp write".to_owned(),
                SandboxMode::WorkspaceWrite => "kernel-enforced workspace + isolated temp writes; .git/.harness protected".to_owned(),
                SandboxMode::DangerFullAccess => unreachable!(),
            },
            network: if policy.network_access {
                "allowed".to_owned()
            } else {
                "environment-offline (not WFP-enforced)".to_owned()
            },
            process_tree: process_tree_name().to_owned(),
            available: true,
            warning: (!policy.network_access).then(|| {
                "Windows 当前采用类似 Codex unelevated 的兼容后端：写边界由 Token+ACL 强制，断网不是防火墙级隔离"
                    .to_owned()
            }),
        }
    }

    #[cfg(target_os = "linux")]
    {
        let bwrap = find_trusted_bwrap(_workspace);
        SandboxStatus {
            mode: policy.mode,
            backend: "linux-bubblewrap".to_owned(),
            filesystem: "mount namespace + read-only root + explicit workspace bind".to_owned(),
            network: if policy.network_access {
                "allowed".to_owned()
            } else {
                "network namespace isolated".to_owned()
            },
            process_tree: process_tree_name().to_owned(),
            available: bwrap.is_some(),
            warning: bwrap.is_none().then(|| {
                "未找到可信 bwrap；受限命令将 fail closed，请先安装 bubblewrap".to_owned()
            }),
        }
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    SandboxStatus {
        mode: policy.mode,
        backend: "unsupported".to_owned(),
        filesystem: "unavailable".to_owned(),
        network: "unavailable".to_owned(),
        process_tree: process_tree_name().to_owned(),
        available: false,
        warning: Some("当前平台受限命令会 fail closed".to_owned()),
    }
}

const fn process_tree_name() -> &'static str {
    if cfg!(windows) {
        "Windows Job Object"
    } else if cfg!(unix) {
        "POSIX Process Group"
    } else {
        "unavailable"
    }
}

#[cfg(target_os = "linux")]
fn find_trusted_bwrap(workspace: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|directory| {
        let candidate = directory.join("bwrap");
        let canonical = fs::canonicalize(candidate).ok()?;
        let metadata = fs::metadata(&canonical).ok()?;
        let trusted = !canonical.starts_with(workspace)
            && metadata.is_file()
            && metadata.uid() == 0
            && metadata.mode() & 0o022 == 0
            && metadata.mode() & 0o111 != 0;
        trusted.then_some(canonical)
    })
}

/// 在 Clap 解析之前识别内部 Windows Sandbox launcher。
#[must_use]
pub fn is_internal_helper_invocation() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__kernary-sandbox-exec"))
}

/// 运行内部 launcher，并返回目标进程 exit code。
pub fn run_internal_helper() -> Result<i32, SandboxError> {
    #[cfg(windows)]
    {
        let result = windows::run_helper(std::env::args_os().skip(2).collect());
        if let Some(path) = std::env::var_os("KERNARY_SANDBOX_DIAGNOSTIC_PATH") {
            // 只在用户显式指定路径时写入；诊断内容不含命令参数和环境变量。
            let detail = match &result {
                Ok(code) => format!("child-exit:{code}"),
                Err(error) => error.to_string(),
            };
            let _ = fs::write(path, detail);
        }
        result
    }
    #[cfg(not(windows))]
    Err(SandboxError::new(
        "sandbox-helper-platform-invalid",
        "内部 Windows Sandbox launcher 不能在当前平台运行",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_stable_and_strict() {
        assert_eq!(
            SandboxMode::parse("read-only").expect("mode"),
            SandboxMode::ReadOnly
        );
        assert_eq!(
            SandboxMode::parse("workspace").expect("mode"),
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            SandboxMode::parse("danger").expect("mode"),
            SandboxMode::DangerFullAccess
        );
        assert!(SandboxMode::parse("fake").is_err());
    }

    #[test]
    fn danger_mode_is_explicitly_reported_as_unrestricted() {
        let root = tempfile::tempdir().expect("root");
        let sandbox =
            ProcessSandbox::new(root.path(), SandboxMode::DangerFullAccess, true).expect("sandbox");
        let status = sandbox.status().expect("status");
        assert_eq!(status.backend, "none");
        assert!(status.warning.is_some());
    }
}
