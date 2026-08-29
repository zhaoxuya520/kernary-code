//! Playwright 子进程适配器：用 JSONL 传输结构化命令，并负责整棵进程树的回收。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use process_wrap::std::{ChildWrapper, CommandWrap};

use serde::Deserialize;

use crate::{BrowserAdapter, BrowserCommand, BrowserError, BrowserResult, BrowserSessionConfig};

const WORKER_SOURCE: &str = include_str!("../scripts/playwright_worker.py");

pub struct PlaywrightProcessAdapter {
    python_executable: PathBuf,
    state: Mutex<Option<WorkerProcess>>,
    next_id: AtomicU64,
}

impl PlaywrightProcessAdapter {
    pub fn new(python_executable: impl Into<PathBuf>) -> Result<Self, BrowserError> {
        let python_executable = python_executable.into();
        if !python_executable.is_absolute() {
            return Err(BrowserError::new(
                "browser-python-path-not-absolute",
                python_executable.display().to_string(),
            ));
        }
        Ok(Self {
            python_executable,
            state: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    fn send(
        &self,
        action: &str,
        payload: serde_json::Value,
        timeout: Duration,
    ) -> Result<BrowserResult, BrowserError> {
        // 请求 ID 严格单调递增，避免超时后的旧响应被误配给下一条动作。
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().map_err(lock_error)?;
        let worker = state
            .as_mut()
            .ok_or_else(|| BrowserError::new("browser-worker-not-running", action.to_owned()))?;
        if worker.child.try_wait().map_err(io_error)?.is_some() {
            return Err(BrowserError::new(
                "browser-worker-exited",
                worker.stderr_tail(),
            ));
        }
        let request = serde_json::json!({"id":id,"action":action,"payload":payload});
        serde_json::to_writer(&mut worker.stdin, &request)
            .map_err(|error| BrowserError::new("browser-worker-request-json", error.to_string()))?;
        worker.stdin.write_all(b"\n").map_err(io_error)?;
        worker.stdin.flush().map_err(io_error)?;
        let line = match worker.responses.recv_timeout(timeout) {
            Ok(line) => line,
            Err(error) => {
                let message = format!("{action}: {error}; stderr={}", worker.stderr_tail());
                worker.terminate();
                return Err(BrowserError::new(
                    "browser-worker-response-timeout",
                    message,
                ));
            }
        };
        let response: WorkerResponse = serde_json::from_str(&line).map_err(|error| {
            BrowserError::new("browser-worker-response-json", error.to_string())
        })?;
        if response.id != id {
            return Err(BrowserError::new(
                "browser-worker-response-id",
                format!("expected={id}, actual={}", response.id),
            ));
        }
        if response.ok {
            return serde_json::from_value(response.result.unwrap_or(serde_json::Value::Null))
                .map_err(|error| {
                    BrowserError::new("browser-worker-result-json", error.to_string())
                });
        }
        Err(BrowserError::new(
            response
                .error_code
                .unwrap_or_else(|| "browser-worker-error".to_owned()),
            response.error.unwrap_or_else(|| action.to_owned()),
        ))
    }

    fn launch_worker(&self, config: &BrowserSessionConfig) -> Result<WorkerProcess, BrowserError> {
        let python = std::fs::canonicalize(&self.python_executable).map_err(io_error)?;
        let browser = std::fs::canonicalize(&config.browser_executable).map_err(io_error)?;
        std::fs::create_dir_all(&config.profile_directory).map_err(io_error)?;
        std::fs::create_dir_all(&config.artifact_directory).map_err(io_error)?;
        std::fs::create_dir_all(&config.download_directory).map_err(io_error)?;
        let worker_path = config
            .profile_directory
            .join("harness-playwright-worker.py");
        write_worker(&worker_path)?;
        let mut command = Command::new(python);
        command
            .arg("-u")
            .arg(&worker_path)
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Windows Job Object / Unix Process Group 保证取消时 Chrome 后代进程不会成为孤儿。
        let mut wrapped = CommandWrap::from(command);
        #[cfg(windows)]
        wrapped.wrap(JobObject);
        #[cfg(unix)]
        wrapped.wrap(ProcessGroup::leader());
        let mut child = wrapped.spawn().map_err(io_error)?;
        let stdin = child.stdin().take().ok_or_else(|| {
            BrowserError::new(
                "browser-worker-stdin-missing",
                worker_path.display().to_string(),
            )
        })?;
        let stdout = child.stdout().take().ok_or_else(|| {
            BrowserError::new(
                "browser-worker-stdout-missing",
                worker_path.display().to_string(),
            )
        })?;
        let stderr = child.stderr().take().ok_or_else(|| {
            BrowserError::new(
                "browser-worker-stderr-missing",
                worker_path.display().to_string(),
            )
        })?;
        let (sender, responses) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let stderr_output = stderr_tail.clone();
        let stderr_reader = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut output = stderr_output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                output.push_str(&line);
                output.push('\n');
                if output.len() > 8_192 {
                    let drain = output.len() - 8_192;
                    output.drain(..drain);
                }
            }
        });
        let mut worker = WorkerProcess {
            child,
            stdin,
            responses,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            stderr_tail,
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = serde_json::json!({
            "id":id,
            "action":"open",
            "payload":{
                "browserExecutable":browser,
                "profileDirectory":config.profile_directory,
                "artifactDirectory":config.artifact_directory,
                "downloadDirectory":config.download_directory,
                "headless":config.headless,
                "allowedOrigins":config.allowed_origins,
                "allowUploads":config.allow_uploads,
                "allowDownloads":config.allow_downloads,
                "timeoutMillis":config.timeout_millis
            }
        });
        serde_json::to_writer(&mut worker.stdin, &request)
            .map_err(|error| BrowserError::new("browser-worker-request-json", error.to_string()))?;
        worker.stdin.write_all(b"\n").map_err(io_error)?;
        worker.stdin.flush().map_err(io_error)?;
        let line = worker
            .responses
            .recv_timeout(Duration::from_millis(config.timeout_millis))
            .map_err(|error| {
                BrowserError::new(
                    "browser-worker-open-timeout",
                    format!("{error}; stderr={}", worker.stderr_tail()),
                )
            })?;
        let response: WorkerResponse = serde_json::from_str(&line).map_err(|error| {
            BrowserError::new("browser-worker-response-json", error.to_string())
        })?;
        if response.id != id || !response.ok {
            worker.terminate();
            return Err(BrowserError::new(
                response
                    .error_code
                    .unwrap_or_else(|| "browser-worker-open-failed".to_owned()),
                response.error.unwrap_or_else(|| worker.stderr_tail()),
            ));
        }
        Ok(worker)
    }
}

impl BrowserAdapter for PlaywrightProcessAdapter {
    fn launch(&self, config: &BrowserSessionConfig) -> Result<(), BrowserError> {
        let mut state = self.state.lock().map_err(lock_error)?;
        if state
            .as_mut()
            .is_some_and(|worker| worker.child.try_wait().ok().flatten().is_none())
        {
            return Ok(());
        }
        *state = Some(self.launch_worker(config)?);
        Ok(())
    }

    fn execute(
        &self,
        config: &BrowserSessionConfig,
        command: &BrowserCommand,
    ) -> Result<BrowserResult, BrowserError> {
        self.send(
            "execute",
            serde_json::to_value(command).map_err(|error| {
                BrowserError::new("browser-worker-command-json", error.to_string())
            })?,
            Duration::from_millis(config.timeout_millis),
        )
    }

    fn close(&self, config: &BrowserSessionConfig) -> Result<(), BrowserError> {
        let result = if self.is_alive() {
            self.send(
                "close",
                serde_json::json!({}),
                Duration::from_millis(config.timeout_millis.min(10_000)),
            )
            .map(|_| ())
        } else {
            Ok(())
        };
        if let Some(mut worker) = self.state.lock().map_err(lock_error)?.take() {
            worker.terminate();
        }
        result
    }

    fn handoff(&self, config: &BrowserSessionConfig) -> Result<(), BrowserError> {
        self.close(config)?;
        let mut visible = config.clone();
        visible.headless = false;
        self.launch(&visible)
    }

    fn is_alive(&self) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| {
                state
                    .as_mut()
                    .map(|worker| worker.child.try_wait().ok().flatten().is_none())
            })
            .unwrap_or(false)
    }
}

impl Drop for PlaywrightProcessAdapter {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut()
            && let Some(mut worker) = state.take()
        {
            worker.terminate();
        }
    }
}

struct WorkerProcess {
    child: Box<dyn ChildWrapper>,
    stdin: ChildStdin,
    responses: Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
    stderr_tail: Arc<Mutex<String>>,
}

impl WorkerProcess {
    fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|output| output.clone())
            .unwrap_or_else(|_| "stderr unavailable".to_owned())
    }

    fn terminate(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkerResponse {
    id: u64,
    ok: bool,
    result: Option<serde_json::Value>,
    error_code: Option<String>,
    error: Option<String>,
}

fn write_worker(path: &Path) -> Result<(), BrowserError> {
    let write = std::fs::read_to_string(path)
        .map(|existing| existing != WORKER_SOURCE)
        .unwrap_or(true);
    if write {
        std::fs::write(path, WORKER_SOURCE).map_err(io_error)?;
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> BrowserError {
    BrowserError::new("browser-process-io", error.to_string())
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> BrowserError {
    BrowserError::new("browser-process-poisoned", "worker")
}
