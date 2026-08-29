#![forbid(unsafe_code)]

//! 官方 Codex CLI delegated adapter；不读取官方客户端私有凭证。

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexProcessOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexBridgeError {
    pub code: String,
    pub message: String,
}

impl CodexBridgeError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for CodexBridgeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for CodexBridgeError {}

pub trait CodexProcessRunner: Send + Sync {
    fn run(
        &self,
        arguments: &[&str],
        interactive: bool,
    ) -> Result<CodexProcessOutput, CodexBridgeError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCodexProcessRunner;

impl CodexProcessRunner for SystemCodexProcessRunner {
    fn run(
        &self,
        arguments: &[&str],
        interactive: bool,
    ) -> Result<CodexProcessOutput, CodexBridgeError> {
        let mut command = Command::new("codex");
        command.args(arguments);
        if interactive {
            let status = command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(process_error)?;
            return Ok(CodexProcessOutput {
                success: status.success(),
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let output = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(process_error)?;
        Ok(CodexProcessOutput {
            success: output.status.success(),
            stdout: truncate_output(String::from_utf8_lossy(&output.stdout).into_owned()),
            stderr: truncate_output(String::from_utf8_lossy(&output.stderr).into_owned()),
        })
    }
}

fn process_error(error: std::io::Error) -> CodexBridgeError {
    CodexBridgeError::new(
        "codex-cli-unavailable",
        format!("无法启动官方 codex CLI：{error}"),
    )
}

fn truncate_output(value: String) -> String {
    value.chars().take(4_096).collect()
}

pub struct CodexAuthBridge<R> {
    runner: R,
}

impl<R: CodexProcessRunner> CodexAuthBridge<R> {
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    pub fn login(&self) -> Result<(), CodexBridgeError> {
        let output = self.runner.run(&["login"], true)?;
        if output.success {
            Ok(())
        } else {
            Err(CodexBridgeError::new(
                "codex-login-failed",
                "官方 codex login 返回失败",
            ))
        }
    }

    pub fn status(&self) -> Result<String, CodexBridgeError> {
        let output = self.runner.run(&["login", "status"], false)?;
        if output.success {
            let text = if output.stdout.trim().is_empty() {
                output.stderr
            } else {
                output.stdout
            };
            Ok(text.trim().to_owned())
        } else {
            Err(CodexBridgeError::new(
                "codex-status-failed",
                if output.stderr.trim().is_empty() {
                    "官方 codex login status 返回失败".to_owned()
                } else {
                    output.stderr
                },
            ))
        }
    }

    pub fn logout(&self) -> Result<(), CodexBridgeError> {
        let output = self.runner.run(&["logout"], false)?;
        if output.success {
            Ok(())
        } else {
            Err(CodexBridgeError::new(
                "codex-logout-failed",
                if output.stderr.trim().is_empty() {
                    "官方 codex logout 返回失败".to_owned()
                } else {
                    output.stderr
                },
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeRunner {
        calls: Mutex<Vec<(Vec<String>, bool)>>,
    }

    impl CodexProcessRunner for FakeRunner {
        fn run(
            &self,
            arguments: &[&str],
            interactive: bool,
        ) -> Result<CodexProcessOutput, CodexBridgeError> {
            self.calls.lock().expect("calls").push((
                arguments.iter().map(|value| (*value).to_owned()).collect(),
                interactive,
            ));
            Ok(CodexProcessOutput {
                success: true,
                stdout: "Logged in".to_owned(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn bridge_only_invokes_documented_auth_commands() {
        let bridge = CodexAuthBridge::new(FakeRunner {
            calls: Mutex::new(vec![]),
        });
        bridge.login().expect("login");
        assert_eq!(bridge.status().expect("status"), "Logged in");
        bridge.logout().expect("logout");
        let calls = bridge.runner.calls.lock().expect("calls");
        assert_eq!(
            *calls,
            vec![
                (vec!["login".to_owned()], true),
                (vec!["login".to_owned(), "status".to_owned()], false),
                (vec!["logout".to_owned()], false),
            ]
        );
    }
}
