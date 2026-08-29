use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use process_wrap::std::{ChildWrapper, CommandWrap};
use serde::{Deserialize, Serialize};

use crate::protocol::{McpError, McpTransport};

type PendingSender = mpsc::SyncSender<Result<serde_json::Value, McpError>>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpStdioConfig {
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub inherit_env: Vec<String>,
    pub request_timeout_millis: Option<u64>,
    pub max_message_bytes: Option<usize>,
}

pub struct StdioMcpTransport {
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Box<dyn ChildWrapper>>,
    pending: Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: Arc<Mutex<VecDeque<serde_json::Value>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    request_timeout: Duration,
    max_message_bytes: usize,
    protocol_version: Mutex<Option<String>>,
}

impl StdioMcpTransport {
    pub fn spawn(config: &McpStdioConfig, project_root: &Path) -> Result<Arc<Self>, McpError> {
        let project_root =
            fs::canonicalize(project_root).map_err(|error| io_error("mcp-project-root", error))?;
        let command_path =
            fs::canonicalize(&config.command).map_err(|error| io_error("mcp-command", error))?;
        if !command_path.is_file() {
            return Err(McpError::new(
                "mcp-command-not-file",
                command_path.display().to_string(),
            ));
        }
        let cwd = config.cwd.as_ref().map_or_else(
            || Ok(project_root.clone()),
            |cwd| fs::canonicalize(cwd).map_err(|error| io_error("mcp-cwd", error)),
        )?;
        if !cwd.is_dir() || !is_inside(&project_root, &cwd) {
            return Err(McpError::new(
                "mcp-cwd-outside-project",
                cwd.display().to_string(),
            ));
        }
        let request_timeout =
            Duration::from_millis(config.request_timeout_millis.unwrap_or(10_000));
        if request_timeout == Duration::ZERO || request_timeout > Duration::from_secs(300) {
            return Err(McpError::new(
                "mcp-request-timeout-invalid",
                format!("{request_timeout:?}"),
            ));
        }
        let max_message_bytes = config.max_message_bytes.unwrap_or(1024 * 1024);
        if !(1024..=16 * 1024 * 1024).contains(&max_message_bytes) {
            return Err(McpError::new(
                "mcp-message-limit-invalid",
                max_message_bytes.to_string(),
            ));
        }

        let mut command = Command::new(command_path);
        command
            .args(&config.args)
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in [
            "PATH",
            "Path",
            "PATHEXT",
            "SystemRoot",
            "SYSTEMROOT",
            "WINDIR",
            "TEMP",
            "TMP",
            "HOME",
            "USERPROFILE",
            "LANG",
            "LC_ALL",
        ]
        .into_iter()
        .chain(config.inherit_env.iter().map(String::as_str))
        {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut wrapped = CommandWrap::from(command);
        #[cfg(windows)]
        wrapped.wrap(JobObject);
        #[cfg(unix)]
        wrapped.wrap(ProcessGroup::leader());
        let mut child = wrapped
            .spawn()
            .map_err(|error| io_error("mcp-spawn", error))?;
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| McpError::new("mcp-stdin-missing", "child stdin"))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| McpError::new("mcp-stdout-missing", "child stdout"))?;
        let stderr = child.stderr().take();
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let notifications = Arc::new(Mutex::new(VecDeque::new()));
        spawn_stdout_reader(
            stdout,
            pending.clone(),
            notifications.clone(),
            max_message_bytes,
        );
        if let Some(stderr) = stderr {
            spawn_stderr_drain(stderr);
        }
        Ok(Arc::new(Self {
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(child),
            pending,
            notifications,
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            request_timeout,
            max_message_bytes,
            protocol_version: Mutex::new(None),
        }))
    }

    fn send_json(&self, value: &serde_json::Value) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::new("mcp-transport-closed", "stdio"));
        }
        let mut bytes = serde_json::to_vec(value)
            .map_err(|error| McpError::new("mcp-json-encode", error.to_string()))?;
        if bytes.len() > self.max_message_bytes {
            return Err(McpError::new(
                "mcp-message-too-large",
                bytes.len().to_string(),
            ));
        }
        bytes.push(b'\n');
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| McpError::new("mcp-stdin-poisoned", "lock"))?;
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| McpError::new("mcp-transport-closed", "stdin"))?;
        stdin
            .write_all(&bytes)
            .and_then(|()| stdin.flush())
            .map_err(|error| io_error("mcp-stdin-write", error))
    }

    fn fail_pending(&self, error: McpError) {
        fail_all(&self.pending, error);
    }
}

impl McpTransport for StdioMcpTransport {
    fn kind(&self) -> &'static str {
        "stdio"
    }

    fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .map_err(|_| McpError::new("mcp-pending-poisoned", "lock"))?
            .insert(id, sender);
        if let Err(error) = self.send_json(&serde_json::json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":method,
            "params":params
        })) {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(error);
        }
        match receiver.recv_timeout(self.request_timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                let _ = self.notify(
                    "notifications/cancelled",
                    serde_json::json!({"requestId":id,"reason":"client timeout"}),
                );
                Err(McpError::new("mcp-request-timeout", method).retryable(true))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(McpError::new("mcp-response-channel-closed", method))
            }
        }
    }

    fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        self.send_json(&serde_json::json!({
            "jsonrpc":"2.0",
            "method":method,
            "params":params
        }))
    }

    fn set_protocol_version(&self, protocol_version: &str) -> Result<(), McpError> {
        *self
            .protocol_version
            .lock()
            .map_err(|_| McpError::new("mcp-version-poisoned", "lock"))? =
            Some(protocol_version.to_owned());
        Ok(())
    }

    fn poll_notifications(&self) -> Result<Vec<serde_json::Value>, McpError> {
        let mut notifications = self
            .notifications
            .lock()
            .map_err(|_| McpError::new("mcp-notifications-poisoned", "stdio"))?;
        Ok(notifications.drain(..).collect())
    }

    fn close(&self) -> Result<(), McpError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.stdin
            .lock()
            .map_err(|_| McpError::new("mcp-stdin-poisoned", "lock"))?
            .take();
        let started = Instant::now();
        let mut child = self
            .child
            .lock()
            .map_err(|_| McpError::new("mcp-child-poisoned", "lock"))?;
        while started.elapsed() < Duration::from_millis(500) {
            if child
                .try_wait()
                .map_err(|error| io_error("mcp-child-wait", error))?
                .is_some()
            {
                self.fail_pending(McpError::new("mcp-transport-closed", "stdio"));
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        child
            .kill()
            .map_err(|error| io_error("mcp-child-kill", error))?;
        self.fail_pending(McpError::new("mcp-transport-closed", "stdio"));
        Ok(())
    }
}

impl Drop for StdioMcpTransport {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn spawn_stdout_reader(
    stdout: std::process::ChildStdout,
    pending: Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: Arc<Mutex<VecDeque<serde_json::Value>>>,
    max_message_bytes: usize,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_capped_line(&mut reader, max_message_bytes) {
                Ok(Some(line)) => match serde_json::from_slice::<serde_json::Value>(&line) {
                    Ok(message) => dispatch_message(&pending, &notifications, message),
                    Err(error) => {
                        fail_all(
                            &pending,
                            McpError::new("mcp-invalid-json-line", error.to_string()),
                        );
                        return;
                    }
                },
                Ok(None) => {
                    fail_all(
                        &pending,
                        McpError::new("mcp-process-exited", "stdout closed"),
                    );
                    return;
                }
                Err(error) => {
                    fail_all(&pending, error);
                    return;
                }
            }
        }
    });
}

fn spawn_stderr_drain(stderr: std::process::ChildStderr) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buffer = [0_u8; 8 * 1024];
        while let Ok(read) = std::io::Read::read(&mut reader, &mut buffer) {
            if read == 0 {
                break;
            }
        }
    });
}

fn dispatch_message(
    pending: &Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: &Arc<Mutex<VecDeque<serde_json::Value>>>,
    message: serde_json::Value,
) {
    let Some(object) = message.as_object() else {
        fail_all(pending, McpError::new("mcp-message-not-object", "stdio"));
        return;
    };
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        fail_all(
            pending,
            McpError::new("mcp-jsonrpc-version-invalid", "stdio"),
        );
        return;
    }
    let Some(id) = object.get("id").and_then(serde_json::Value::as_u64) else {
        if object
            .get("method")
            .and_then(serde_json::Value::as_str)
            .is_some()
        {
            let mut queued = notifications
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if queued.len() == 1024 {
                queued.pop_front();
            }
            queued.push_back(message);
        }
        return;
    };
    let sender = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
    let Some(sender) = sender else {
        return;
    };
    let result = if let Some(error) = object.get("error") {
        let code = error
            .get("code")
            .map_or_else(|| "unknown".to_owned(), serde_json::Value::to_string);
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("MCP server error");
        Err(McpError::new(
            format!("mcp-jsonrpc-error-{code}"),
            message.chars().take(512).collect::<String>(),
        ))
    } else {
        object
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::new("mcp-response-result-missing", id.to_string()))
    };
    let _ = sender.send(result);
}

fn fail_all(pending: &Arc<Mutex<BTreeMap<u64, PendingSender>>>, error: McpError) {
    let senders = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .split_off(&0);
    for sender in senders.into_values() {
        let _ = sender.send(Err(error.clone()));
    }
}

fn read_capped_line<R: BufRead>(
    reader: &mut R,
    max_message_bytes: usize,
) -> Result<Option<Vec<u8>>, McpError> {
    let mut output = Vec::new();
    loop {
        let buffer = reader
            .fill_buf()
            .map_err(|error| io_error("mcp-stdout-read", error))?;
        if buffer.is_empty() {
            return if output.is_empty() {
                Ok(None)
            } else {
                Err(McpError::new(
                    "mcp-truncated-json-line",
                    output.len().to_string(),
                ))
            };
        }
        let take = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |position| position + 1);
        if output.len().saturating_add(take) > max_message_bytes {
            return Err(McpError::new(
                "mcp-message-too-large",
                max_message_bytes.to_string(),
            ));
        }
        output.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if output.last() == Some(&b'\n') {
            output.pop();
            if output.last() == Some(&b'\r') {
                output.pop();
            }
            if output.is_empty() {
                continue;
            }
            return Ok(Some(output));
        }
    }
}

fn is_inside(root: &Path, target: &Path) -> bool {
    let root = normalized(root);
    let target = normalized(target);
    target == root || target.starts_with(&format!("{root}{}", std::path::MAIN_SEPARATOR))
}

fn normalized(path: &Path) -> String {
    let path = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path
    }
}

fn io_error(code: &'static str, error: std::io::Error) -> McpError {
    McpError::new(code, error.to_string()).retryable(true)
}
