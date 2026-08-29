use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use harness_tool::ToolCancellationToken;
use process_wrap::std::CommandWrap;
#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;

use crate::PluginError;

type ReaderResult = Result<(Vec<u8>, bool), std::io::Error>;

pub(crate) fn run_plugin_process(
    plugin_root: &Path,
    entry: &Path,
    operation: &str,
    request: &serde_json::Value,
    timeout: Duration,
    max_output_bytes: usize,
    cancellation: Option<&ToolCancellationToken>,
) -> Result<serde_json::Value, PluginError> {
    let plugin_root = fs::canonicalize(plugin_root)
        .map_err(|error| io_error("plugin-root-canonicalize", error))?;
    let entry =
        fs::canonicalize(entry).map_err(|error| io_error("plugin-entry-canonicalize", error))?;
    if !entry.is_file() || !is_inside(&plugin_root, &entry) {
        return Err(PluginError::new(
            "plugin-entry-outside-root",
            entry.display().to_string(),
        ));
    }
    if timeout == Duration::ZERO || timeout > Duration::from_secs(300) {
        return Err(PluginError::new(
            "plugin-timeout-invalid",
            format!("{timeout:?}"),
        ));
    }
    let request = serde_json::to_vec(request)
        .map_err(|error| PluginError::new("plugin-request-json", error.to_string()))?;
    if request.len() > 4 * 1024 * 1024 {
        return Err(PluginError::new(
            "plugin-request-too-large",
            request.len().to_string(),
        ));
    }
    let mut command = Command::new(entry);
    command
        .arg(operation)
        .current_dir(plugin_root)
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
    ] {
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
        .map_err(|error| io_error("plugin-process-spawn", error))?;
    let mut stdin = child
        .stdin()
        .take()
        .ok_or_else(|| PluginError::new("plugin-process-stdin-missing", operation))?;
    stdin
        .write_all(&request)
        .and_then(|()| stdin.flush())
        .map_err(|error| io_error("plugin-process-stdin-write", error))?;
    drop(stdin);
    let stdout = child.stdout().take();
    let stderr = child.stderr().take();
    let stdout_reader = spawn_reader(stdout, max_output_bytes);
    let stderr_reader = spawn_reader(stderr, 64 * 1024);
    let started = Instant::now();
    let status = loop {
        if cancellation.is_some_and(ToolCancellationToken::is_cancelled) {
            child
                .kill()
                .map_err(|error| io_error("plugin-process-cancel", error))?;
            return Err(PluginError::new("plugin-process-cancelled", operation));
        }
        if started.elapsed() >= timeout {
            child
                .kill()
                .map_err(|error| io_error("plugin-process-timeout-kill", error))?;
            return Err(PluginError::new("plugin-process-timeout", operation));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| io_error("plugin-process-wait", error))?
        {
            break status;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stdout_truncated) = join_reader(stdout_reader)?;
    let (_stderr, stderr_truncated) = join_reader(stderr_reader)?;
    if !status.success() {
        return Err(PluginError::new(
            "plugin-process-failed",
            format!(
                "operation={operation}, exit={:?}, stderrTruncated={stderr_truncated}",
                status.code()
            ),
        ));
    }
    if stdout_truncated {
        return Err(PluginError::new(
            "plugin-response-too-large",
            max_output_bytes.to_string(),
        ));
    }
    serde_json::from_slice(&stdout)
        .map_err(|error| PluginError::new("plugin-response-json", error.to_string()))
}

fn spawn_reader<R: Read + Send + 'static>(
    reader: Option<R>,
    limit: usize,
) -> Option<thread::JoinHandle<ReaderResult>> {
    reader.map(|mut reader| {
        thread::spawn(move || {
            let mut captured = Vec::new();
            let mut buffer = [0_u8; 8 * 1024];
            let mut truncated = false;
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                let available = limit.saturating_sub(captured.len());
                let keep = read.min(available);
                captured.extend_from_slice(&buffer[..keep]);
                truncated |= keep < read;
            }
            Ok((captured, truncated))
        })
    })
}

fn join_reader(
    reader: Option<thread::JoinHandle<ReaderResult>>,
) -> Result<(Vec<u8>, bool), PluginError> {
    reader.map_or(Ok((vec![], false)), |reader| {
        reader
            .join()
            .map_err(|_| PluginError::new("plugin-output-reader-panicked", "reader"))?
            .map_err(|error| io_error("plugin-output-read", error))
    })
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

fn io_error(code: &'static str, error: std::io::Error) -> PluginError {
    PluginError::new(code, error.to_string())
}

pub(crate) fn resolve_inside(root: &Path, relative: &Path) -> Result<PathBuf, PluginError> {
    if relative.is_absolute() {
        return Err(PluginError::new(
            "plugin-relative-path-required",
            relative.display().to_string(),
        ));
    }
    let root =
        fs::canonicalize(root).map_err(|error| io_error("plugin-root-canonicalize", error))?;
    let path = fs::canonicalize(root.join(relative))
        .map_err(|error| io_error("plugin-path-canonicalize", error))?;
    if !is_inside(&root, &path) {
        return Err(PluginError::new(
            "plugin-path-outside-root",
            path.display().to_string(),
        ));
    }
    Ok(path)
}
