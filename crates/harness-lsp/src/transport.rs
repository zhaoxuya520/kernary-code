use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
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

use crate::{LspError, LspServerConfig};

type PendingSender = mpsc::SyncSender<Result<serde_json::Value, LspError>>;

pub(crate) struct LspTransport {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    child: Mutex<Box<dyn ChildWrapper>>,
    pending: Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: Arc<Mutex<VecDeque<serde_json::Value>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    request_timeout: Duration,
    max_message_bytes: usize,
}

impl LspTransport {
    pub(crate) fn spawn(
        config: &LspServerConfig,
        project_root: &Path,
        root_uri: &str,
    ) -> Result<Arc<Self>, LspError> {
        let project_root = fs::canonicalize(project_root)
            .map_err(|error| LspError::new("lsp-project-root", error.to_string()))?;
        if !config.command.is_absolute() {
            return Err(LspError::new(
                "lsp-command-not-absolute",
                config.command.display().to_string(),
            ));
        }
        let command = fs::canonicalize(&config.command)
            .map_err(|error| LspError::new("lsp-command", error.to_string()))?;
        if !command.is_file() {
            return Err(LspError::new(
                "lsp-command-not-file",
                command.display().to_string(),
            ));
        }
        let cwd = resolve_cwd(&project_root, config.cwd.as_deref())?;
        let request_timeout =
            Duration::from_millis(config.request_timeout_millis.unwrap_or(15_000));
        if request_timeout.is_zero() || request_timeout > Duration::from_secs(300) {
            return Err(LspError::new(
                "lsp-request-timeout-invalid",
                format!("{request_timeout:?}"),
            ));
        }
        let max_message_bytes = config.max_message_bytes.unwrap_or(8 * 1024 * 1024);
        if !(1024..=32 * 1024 * 1024).contains(&max_message_bytes) {
            return Err(LspError::new(
                "lsp-message-limit-invalid",
                max_message_bytes.to_string(),
            ));
        }

        let mut process = Command::new(command);
        process
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
                process.env(name, value);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            process.creation_flags(CREATE_NO_WINDOW);
        }
        let mut wrapped = CommandWrap::from(process);
        #[cfg(windows)]
        wrapped.wrap(JobObject);
        #[cfg(unix)]
        wrapped.wrap(ProcessGroup::leader());
        let mut child = wrapped
            .spawn()
            .map_err(|error| LspError::new("lsp-spawn", error.to_string()))?;
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| LspError::new("lsp-stdin-missing", "child stdin"))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| LspError::new("lsp-stdout-missing", "child stdout"))?;
        let stderr = child.stderr().take();
        let stdin = Arc::new(Mutex::new(Some(stdin)));
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let notifications = Arc::new(Mutex::new(VecDeque::new()));
        spawn_stdout_reader(
            stdout,
            stdin.clone(),
            pending.clone(),
            notifications.clone(),
            max_message_bytes,
            root_uri.to_owned(),
            project_root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace")
                .to_owned(),
        );
        if let Some(stderr) = stderr {
            spawn_stderr_drain(stderr);
        }
        Ok(Arc::new(Self {
            stdin,
            child: Mutex::new(child),
            pending,
            notifications,
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            request_timeout,
            max_message_bytes,
        }))
    }

    pub(crate) fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .map_err(|_| LspError::new("lsp-pending-poisoned", "lock"))?
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
                let _ = self.notify("$/cancelRequest", serde_json::json!({"id":id}));
                Err(LspError::new("lsp-request-timeout", method).retryable(true))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(LspError::new("lsp-response-channel-closed", method))
            }
        }
    }

    pub(crate) fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), LspError> {
        self.send_json(&serde_json::json!({
            "jsonrpc":"2.0",
            "method":method,
            "params":params
        }))
    }

    pub(crate) fn poll_notifications(&self) -> Result<Vec<serde_json::Value>, LspError> {
        let mut notifications = self
            .notifications
            .lock()
            .map_err(|_| LspError::new("lsp-notifications-poisoned", "lock"))?;
        Ok(notifications.drain(..).collect())
    }

    pub(crate) fn close(&self) -> Result<(), LspError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.stdin
            .lock()
            .map_err(|_| LspError::new("lsp-stdin-poisoned", "lock"))?
            .take();
        let started = Instant::now();
        let mut child = self
            .child
            .lock()
            .map_err(|_| LspError::new("lsp-child-poisoned", "lock"))?;
        while started.elapsed() < Duration::from_millis(500) {
            if child
                .try_wait()
                .map_err(|error| LspError::new("lsp-child-wait", error.to_string()))?
                .is_some()
            {
                fail_all(
                    &self.pending,
                    LspError::new("lsp-transport-closed", "stdio"),
                );
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        child
            .kill()
            .map_err(|error| LspError::new("lsp-child-kill", error.to_string()))?;
        fail_all(
            &self.pending,
            LspError::new("lsp-transport-closed", "stdio"),
        );
        Ok(())
    }

    fn send_json(&self, value: &serde_json::Value) -> Result<(), LspError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(LspError::new("lsp-transport-closed", "stdio"));
        }
        send_shared(&self.stdin, value, self.max_message_bytes)
    }
}

impl Drop for LspTransport {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn resolve_cwd(project_root: &Path, configured: Option<&Path>) -> Result<PathBuf, LspError> {
    let candidate = configured.map_or_else(
        || project_root.to_path_buf(),
        |path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                project_root.join(path)
            }
        },
    );
    let cwd =
        fs::canonicalize(candidate).map_err(|error| LspError::new("lsp-cwd", error.to_string()))?;
    if !cwd.is_dir() || !is_inside(project_root, &cwd) {
        return Err(LspError::new(
            "lsp-cwd-outside-project",
            cwd.display().to_string(),
        ));
    }
    Ok(cwd)
}

fn send_shared(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    value: &serde_json::Value,
    max_message_bytes: usize,
) -> Result<(), LspError> {
    let body = serde_json::to_vec(value)
        .map_err(|error| LspError::new("lsp-json-encode", error.to_string()))?;
    if body.len() > max_message_bytes {
        return Err(LspError::new(
            "lsp-message-too-large",
            body.len().to_string(),
        ));
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut stdin = stdin
        .lock()
        .map_err(|_| LspError::new("lsp-stdin-poisoned", "lock"))?;
    let stdin = stdin
        .as_mut()
        .ok_or_else(|| LspError::new("lsp-transport-closed", "stdin"))?;
    stdin
        .write_all(header.as_bytes())
        .and_then(|()| stdin.write_all(&body))
        .and_then(|()| stdin.flush())
        .map_err(|error| LspError::new("lsp-stdin-write", error.to_string()))
}

fn spawn_stdout_reader(
    stdout: std::process::ChildStdout,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pending: Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: Arc<Mutex<VecDeque<serde_json::Value>>>,
    max_message_bytes: usize,
    root_uri: String,
    root_name: String,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_message(&mut reader, max_message_bytes) {
                Ok(Some(message)) => dispatch_message(
                    &stdin,
                    &pending,
                    &notifications,
                    message,
                    max_message_bytes,
                    &root_uri,
                    &root_name,
                ),
                Ok(None) => {
                    fail_all(
                        &pending,
                        LspError::new("lsp-process-exited", "stdout closed"),
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

fn read_message<R: BufRead>(
    reader: &mut R,
    max_message_bytes: usize,
) -> Result<Option<serde_json::Value>, LspError> {
    let mut content_length = None;
    let mut total_header_bytes = 0_usize;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| LspError::new("lsp-header-read", error.to_string()))?;
        if read == 0 {
            return if total_header_bytes == 0 {
                Ok(None)
            } else {
                Err(LspError::new("lsp-header-truncated", "stdout closed"))
            };
        }
        total_header_bytes = total_header_bytes.saturating_add(read);
        if total_header_bytes > 16 * 1024 {
            return Err(LspError::new(
                "lsp-header-too-large",
                total_header_bytes.to_string(),
            ));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            if content_length.is_some() {
                return Err(LspError::new(
                    "lsp-content-length-duplicate",
                    "duplicate header",
                ));
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| LspError::new("lsp-content-length-invalid", value.trim()))?,
            );
        }
    }
    let content_length =
        content_length.ok_or_else(|| LspError::new("lsp-content-length-missing", "header"))?;
    if content_length == 0 || content_length > max_message_bytes {
        return Err(LspError::new(
            "lsp-message-size-invalid",
            content_length.to_string(),
        ));
    }
    let mut body = vec![0_u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|error| LspError::new("lsp-body-read", error.to_string()))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| LspError::new("lsp-invalid-json", error.to_string()))
}

fn dispatch_message(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    pending: &Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: &Arc<Mutex<VecDeque<serde_json::Value>>>,
    message: serde_json::Value,
    max_message_bytes: usize,
    root_uri: &str,
    root_name: &str,
) {
    let Some(object) = message.as_object() else {
        fail_all(pending, LspError::new("lsp-message-not-object", "stdio"));
        return;
    };
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        fail_all(
            pending,
            LspError::new("lsp-jsonrpc-version-invalid", "stdio"),
        );
        return;
    }
    if let (Some(id), Some(method)) = (
        object.get("id"),
        object.get("method").and_then(serde_json::Value::as_str),
    ) {
        let response = server_request_response(
            id.clone(),
            method,
            object.get("params"),
            root_uri,
            root_name,
        );
        let _ = send_shared(stdin, &response, max_message_bytes);
        return;
    }
    if let Some(id) = object.get("id").and_then(serde_json::Value::as_u64) {
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
                .unwrap_or("LSP server error");
            Err(LspError::new(
                format!("lsp-jsonrpc-error-{code}"),
                message.chars().take(512).collect::<String>(),
            ))
        } else {
            object
                .get("result")
                .cloned()
                .ok_or_else(|| LspError::new("lsp-response-result-missing", id.to_string()))
        };
        let _ = sender.send(result);
        return;
    }
    if object
        .get("method")
        .and_then(serde_json::Value::as_str)
        .is_some()
    {
        let mut queue = notifications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.len() == 1024 {
            queue.pop_front();
        }
        queue.push_back(message);
    }
}

fn server_request_response(
    id: serde_json::Value,
    method: &str,
    params: Option<&serde_json::Value>,
    root_uri: &str,
    root_name: &str,
) -> serde_json::Value {
    let result = match method {
        "workspace/configuration" => {
            let count = params
                .and_then(|value| value.get("items"))
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            Some(serde_json::Value::Array(vec![
                serde_json::Value::Null;
                count
            ]))
        }
        "workspace/workspaceFolders" => Some(serde_json::json!([{
            "uri":root_uri,
            "name":root_name
        }])),
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create"
        | "window/showMessageRequest" => Some(serde_json::Value::Null),
        "workspace/applyEdit" => Some(serde_json::json!({
            "applied":false,
            "failureReason":"Kernary LSP Bridge is read-only"
        })),
        "window/showDocument" => Some(serde_json::json!({"success":false})),
        _ => None,
    };
    result.map_or_else(
        || {
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":id,
                "error":{"code":-32601,"message":"Method not found"}
            })
        },
        |result| serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
    )
}

fn spawn_stderr_drain(stderr: std::process::ChildStderr) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buffer = [0_u8; 8 * 1024];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
        }
    });
}

fn fail_all(pending: &Arc<Mutex<BTreeMap<u64, PendingSender>>>, error: LspError) {
    let senders = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .split_off(&0);
    for sender in senders.into_values() {
        let _ = sender.send(Err(error.clone()));
    }
}

fn is_inside(root: &Path, target: &Path) -> bool {
    target == root || target.starts_with(root)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn content_length_framing_is_strict_and_bounded() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let framed = format!("Content-Length: {}\r\n\r\n", body.len())
            .bytes()
            .chain(body.iter().copied())
            .collect::<Vec<_>>();
        let value = read_message(&mut Cursor::new(framed), 1024)
            .expect("message")
            .expect("value");
        assert_eq!(value["id"], 1);

        let duplicate = b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(
            read_message(&mut Cursor::new(duplicate), 1024)
                .expect_err("duplicate")
                .code,
            "lsp-content-length-duplicate"
        );
        let oversized = b"Content-Length: 9999\r\n\r\n";
        assert_eq!(
            read_message(&mut Cursor::new(oversized), 1024)
                .expect_err("oversized")
                .code,
            "lsp-message-size-invalid"
        );
    }
}
