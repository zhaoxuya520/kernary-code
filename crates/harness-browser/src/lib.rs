#![forbid(unsafe_code)]

//! BrowserRuntime 只暴露结构化动作；底层 Playwright/CDP 永不进入 Agent Tool Catalog。

mod journal;
mod process;
mod tools;

pub use journal::{BrowserActionJournal, SqliteBrowserJournal};
pub use process::PlaywrightProcessAdapter;
pub use tools::register_browser_tools;

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use harness_types::{BrowserActionId, BrowserSessionId, ConfidentialityLabel, ContentHash};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserError {
    pub code: String,
    pub message: String,
}

impl BrowserError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for BrowserError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for BrowserError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserSessionStatus {
    #[default]
    Closed,
    Starting,
    Ready,
    /// 可见浏览器已交给用户，Agent 动作在重新接管前全部拒绝。
    UserControl,
    Closing,
    Failed,
    NeedsReconciliation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserSessionConfig {
    pub id: BrowserSessionId,
    pub browser_executable: PathBuf,
    pub profile_directory: PathBuf,
    pub artifact_directory: PathBuf,
    pub download_directory: PathBuf,
    pub headless: bool,
    pub allowed_origins: BTreeSet<String>,
    pub upload_roots: Vec<PathBuf>,
    pub allow_uploads: bool,
    pub allow_downloads: bool,
    pub timeout_millis: u64,
}

impl BrowserSessionConfig {
    pub fn validate(mut self) -> Result<Self, BrowserError> {
        if self.id.as_str().trim().is_empty()
            || !self.browser_executable.is_absolute()
            || !self.profile_directory.is_absolute()
            || !self.artifact_directory.is_absolute()
            || !self.download_directory.is_absolute()
            || self.timeout_millis == 0
        {
            return Err(BrowserError::new(
                "browser-config-invalid",
                self.id.to_string(),
            ));
        }
        self.allowed_origins = self
            .allowed_origins
            .iter()
            .map(|origin| canonical_origin(origin))
            .collect::<Result<_, _>>()?;
        if self.allowed_origins.is_empty() {
            return Err(BrowserError::new(
                "browser-origins-empty",
                self.id.to_string(),
            ));
        }
        self.upload_roots = self
            .upload_roots
            .iter()
            .map(|root| {
                std::fs::canonicalize(root)
                    .map_err(|error| BrowserError::new("browser-upload-root", error.to_string()))
            })
            .collect::<Result<_, _>>()?;
        if self.allow_uploads && self.upload_roots.is_empty() {
            return Err(BrowserError::new(
                "browser-upload-roots-empty",
                self.id.to_string(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserActionKind {
    Open,
    Navigate,
    Snapshot,
    Click,
    Type,
    Read,
    Inspect,
    Wait,
    Screenshot,
    Upload,
    Download,
    Close,
    Handoff,
    Reclaim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserActionStatus {
    Running,
    Completed,
    Failed,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserActionRecord {
    pub id: BrowserActionId,
    pub session_id: BrowserSessionId,
    pub sequence: u64,
    pub action: BrowserActionKind,
    pub status: BrowserActionStatus,
    pub origin: Option<String>,
    pub target: Option<String>,
    pub arguments_sha256: ContentHash,
    pub result_summary: Option<String>,
    pub error: Option<String>,
    pub started_at_millis: i64,
    pub completed_at_millis: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserSnapshotNode {
    pub ref_id: Option<String>,
    pub role: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub sensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserSnapshot {
    pub url: String,
    pub title: String,
    pub generation: u64,
    pub nodes: Vec<BrowserSnapshotNode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserArtifactRef {
    pub id: String,
    pub path: PathBuf,
    pub mime_type: String,
    pub bytes: u64,
    pub sha256: ContentHash,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserInspectResult {
    pub ref_id: String,
    pub role: String,
    pub name: String,
    pub tag: Option<String>,
    pub attributes: serde_json::Value,
    pub bounds: Option<[f64; 4]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum BrowserWait {
    Millis { millis: u64 },
    Ref { ref_id: String },
    Load { state: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum BrowserCommand {
    Navigate {
        url: String,
    },
    Snapshot,
    Click {
        ref_id: String,
    },
    Type {
        ref_id: String,
        text: String,
        classification: ConfidentialityLabel,
    },
    Read {
        ref_id: String,
    },
    Inspect {
        ref_id: String,
    },
    Wait {
        wait: BrowserWait,
    },
    Screenshot,
    Upload {
        ref_id: String,
        path: PathBuf,
    },
    Download {
        ref_id: String,
    },
}

impl BrowserCommand {
    #[must_use]
    pub const fn kind(&self) -> BrowserActionKind {
        match self {
            Self::Navigate { .. } => BrowserActionKind::Navigate,
            Self::Snapshot => BrowserActionKind::Snapshot,
            Self::Click { .. } => BrowserActionKind::Click,
            Self::Type { .. } => BrowserActionKind::Type,
            Self::Read { .. } => BrowserActionKind::Read,
            Self::Inspect { .. } => BrowserActionKind::Inspect,
            Self::Wait { .. } => BrowserActionKind::Wait,
            Self::Screenshot => BrowserActionKind::Screenshot,
            Self::Upload { .. } => BrowserActionKind::Upload,
            Self::Download { .. } => BrowserActionKind::Download,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BrowserResult {
    Unit,
    Snapshot { snapshot: BrowserSnapshot },
    Text { text: String },
    Inspect { result: BrowserInspectResult },
    Artifact { artifact: BrowserArtifactRef },
}

pub trait BrowserAdapter: Send + Sync {
    fn launch(&self, config: &BrowserSessionConfig) -> Result<(), BrowserError>;
    fn execute(
        &self,
        config: &BrowserSessionConfig,
        command: &BrowserCommand,
    ) -> Result<BrowserResult, BrowserError>;
    fn close(&self, config: &BrowserSessionConfig) -> Result<(), BrowserError>;
    fn handoff(&self, config: &BrowserSessionConfig) -> Result<(), BrowserError>;
    fn is_alive(&self) -> bool;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserRuntimeView {
    pub session_id: BrowserSessionId,
    pub status: BrowserSessionStatus,
    pub current_origin: Option<String>,
    pub snapshot_generation: u64,
    pub action_count: usize,
    pub adapter_alive: bool,
}

#[derive(Default)]
struct BrowserRuntimeState {
    status: BrowserSessionStatus,
    current_origin: Option<String>,
    snapshot_generation: u64,
}

pub struct BrowserRuntime {
    config: BrowserSessionConfig,
    adapter: Arc<dyn BrowserAdapter>,
    journal: Arc<dyn BrowserActionJournal>,
    state: Mutex<BrowserRuntimeState>,
}

impl BrowserRuntime {
    pub fn new(
        config: BrowserSessionConfig,
        adapter: Arc<dyn BrowserAdapter>,
        journal: Arc<dyn BrowserActionJournal>,
    ) -> Result<Self, BrowserError> {
        Ok(Self {
            config: config.validate()?,
            adapter,
            journal,
            state: Mutex::new(BrowserRuntimeState::default()),
        })
    }

    pub fn open(&self, now_millis: i64) -> Result<BrowserRuntimeView, BrowserError> {
        {
            let mut state = self.state.lock().map_err(lock_error)?;
            match state.status {
                BrowserSessionStatus::Ready => {
                    drop(state);
                    return self.view();
                }
                BrowserSessionStatus::UserControl => {
                    return Err(BrowserError::new(
                        "browser-user-control-active-use-reclaim",
                        self.config.id.to_string(),
                    ));
                }
                BrowserSessionStatus::Starting | BrowserSessionStatus::Closing => {
                    return Err(BrowserError::new(
                        "browser-session-transition-active",
                        self.config.id.to_string(),
                    ));
                }
                BrowserSessionStatus::Closed
                | BrowserSessionStatus::Failed
                | BrowserSessionStatus::NeedsReconciliation => {}
            }
            state.status = BrowserSessionStatus::Starting;
        }
        self.journal
            .upsert_session(&self.config.id, BrowserSessionStatus::Starting, now_millis)?;
        match self.adapter.launch(&self.config) {
            Ok(()) => {
                let mut state = self.state.lock().map_err(lock_error)?;
                state.status = BrowserSessionStatus::Ready;
                state.current_origin = None;
                self.journal.upsert_session(
                    &self.config.id,
                    BrowserSessionStatus::Ready,
                    now_millis,
                )?;
                drop(state);
                self.view()
            }
            Err(error) => {
                self.state.lock().map_err(lock_error)?.status = BrowserSessionStatus::Failed;
                self.journal.upsert_session(
                    &self.config.id,
                    BrowserSessionStatus::Failed,
                    now_millis,
                )?;
                Err(error)
            }
        }
    }

    pub fn execute(
        &self,
        action_id: BrowserActionId,
        command: BrowserCommand,
        now_millis: i64,
    ) -> Result<BrowserResult, BrowserError> {
        let (origin, target) = command_metadata(&command, self.current_origin());
        let arguments = serde_json::to_vec(&command)
            .map_err(|error| BrowserError::new("browser-command-json", error.to_string()))?;
        let record = self.journal.begin(BrowserActionRecord {
            id: action_id,
            session_id: self.config.id.clone(),
            sequence: 0,
            action: command.kind(),
            status: BrowserActionStatus::Running,
            origin,
            target,
            arguments_sha256: ContentHash::from(format!("{:x}", Sha256::digest(arguments))),
            result_summary: None,
            error: None,
            started_at_millis: now_millis,
            completed_at_millis: None,
        })?;
        if let Err(error) = self.validate_command(&command) {
            self.journal
                .fail(&record.id, error.to_string(), now_millis)?;
            return Err(error);
        }
        match self.adapter.execute(&self.config, &command) {
            Ok(result) => {
                // Adapter 输出仍是不可信边界；结果形状或状态迁移失败也必须关闭 Journal 记录。
                match self.validate_result(&command, result).and_then(|result| {
                    self.apply_result_state(&command, &result)?;
                    Ok(result)
                }) {
                    Ok(result) => {
                        self.journal.complete(
                            &record.id,
                            format!("{:?}", command.kind()).to_lowercase(),
                            now_millis,
                        )?;
                        Ok(result)
                    }
                    Err(error) => {
                        self.journal
                            .fail(&record.id, error.to_string(), now_millis)?;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.state.lock().map_err(lock_error)?.status =
                    BrowserSessionStatus::NeedsReconciliation;
                self.journal.upsert_session(
                    &self.config.id,
                    BrowserSessionStatus::NeedsReconciliation,
                    now_millis,
                )?;
                self.journal
                    .fail(&record.id, error.to_string(), now_millis)?;
                Err(error)
            }
        }
    }

    pub fn close(&self, now_millis: i64) -> Result<BrowserRuntimeView, BrowserError> {
        {
            let mut state = self.state.lock().map_err(lock_error)?;
            if state.status == BrowserSessionStatus::Closed {
                drop(state);
                return self.view();
            }
            state.status = BrowserSessionStatus::Closing;
        }
        let close_result = self.adapter.close(&self.config);
        let mut state = self.state.lock().map_err(lock_error)?;
        state.status = if close_result.is_ok() {
            BrowserSessionStatus::Closed
        } else {
            BrowserSessionStatus::NeedsReconciliation
        };
        state.current_origin = None;
        self.journal
            .upsert_session(&self.config.id, state.status, now_millis)?;
        drop(state);
        close_result?;
        self.view()
    }

    /// 关闭 headless worker，并用同一独立 profile 重启可见浏览器交给用户操作。
    pub fn handoff(
        &self,
        action_id: BrowserActionId,
        now_millis: i64,
    ) -> Result<BrowserRuntimeView, BrowserError> {
        let record = self.journal.begin(BrowserActionRecord {
            id: action_id,
            session_id: self.config.id.clone(),
            sequence: 0,
            action: BrowserActionKind::Handoff,
            status: BrowserActionStatus::Running,
            origin: self
                .state
                .lock()
                .map_err(lock_error)?
                .current_origin
                .clone(),
            target: None,
            arguments_sha256: ContentHash::from(format!("{:x}", Sha256::digest(b"handoff"))),
            result_summary: None,
            error: None,
            started_at_millis: now_millis,
            completed_at_millis: None,
        })?;
        if self.state.lock().map_err(lock_error)?.status != BrowserSessionStatus::Ready
            || !self.adapter.is_alive()
        {
            let error = BrowserError::new("browser-session-not-ready", self.config.id.to_string());
            self.journal
                .fail(&record.id, error.to_string(), now_millis)?;
            return Err(error);
        }
        match self.adapter.handoff(&self.config) {
            Ok(()) => {
                let mut state = self.state.lock().map_err(lock_error)?;
                state.status = BrowserSessionStatus::UserControl;
                state.current_origin = None;
                state.snapshot_generation = state.snapshot_generation.saturating_add(1);
                self.journal.upsert_session(
                    &self.config.id,
                    BrowserSessionStatus::UserControl,
                    now_millis,
                )?;
                drop(state);
                self.journal
                    .complete(&record.id, "visible-user-control".to_owned(), now_millis)?;
                self.view()
            }
            Err(error) => {
                self.state.lock().map_err(lock_error)?.status =
                    BrowserSessionStatus::NeedsReconciliation;
                self.journal.upsert_session(
                    &self.config.id,
                    BrowserSessionStatus::NeedsReconciliation,
                    now_millis,
                )?;
                self.journal
                    .fail(&record.id, error.to_string(), now_millis)?;
                Err(error)
            }
        }
    }

    /// 用户完成密码、验证码等敏感操作后，重启 headless Worker 并恢复 Agent 控制。
    pub fn reclaim(
        &self,
        action_id: BrowserActionId,
        now_millis: i64,
    ) -> Result<BrowserRuntimeView, BrowserError> {
        if self.state.lock().map_err(lock_error)?.status != BrowserSessionStatus::UserControl {
            return Err(BrowserError::new(
                "browser-not-in-user-control",
                self.config.id.to_string(),
            ));
        }
        let record = self.journal.begin(BrowserActionRecord {
            id: action_id,
            session_id: self.config.id.clone(),
            sequence: 0,
            action: BrowserActionKind::Reclaim,
            status: BrowserActionStatus::Running,
            origin: None,
            target: None,
            arguments_sha256: ContentHash::from(format!("{:x}", Sha256::digest(b"reclaim"))),
            result_summary: None,
            error: None,
            started_at_millis: now_millis,
            completed_at_millis: None,
        })?;
        let result = self
            .adapter
            .close(&self.config)
            .and_then(|()| self.adapter.launch(&self.config));
        match result {
            Ok(()) => {
                let mut state = self.state.lock().map_err(lock_error)?;
                state.status = BrowserSessionStatus::Ready;
                state.current_origin = None;
                state.snapshot_generation = state.snapshot_generation.saturating_add(1);
                self.journal.upsert_session(
                    &self.config.id,
                    BrowserSessionStatus::Ready,
                    now_millis,
                )?;
                drop(state);
                self.journal.complete(
                    &record.id,
                    "agent-control-restored".to_owned(),
                    now_millis,
                )?;
                self.view()
            }
            Err(error) => {
                self.state.lock().map_err(lock_error)?.status =
                    BrowserSessionStatus::NeedsReconciliation;
                self.journal.upsert_session(
                    &self.config.id,
                    BrowserSessionStatus::NeedsReconciliation,
                    now_millis,
                )?;
                self.journal
                    .fail(&record.id, error.to_string(), now_millis)?;
                Err(error)
            }
        }
    }

    pub fn view(&self) -> Result<BrowserRuntimeView, BrowserError> {
        let state = self.state.lock().map_err(lock_error)?;
        Ok(BrowserRuntimeView {
            session_id: self.config.id.clone(),
            status: state.status,
            current_origin: state.current_origin.clone(),
            snapshot_generation: state.snapshot_generation,
            action_count: self.journal.list(&self.config.id)?.len(),
            adapter_alive: self.adapter.is_alive(),
        })
    }

    pub fn actions(&self) -> Result<Vec<BrowserActionRecord>, BrowserError> {
        self.journal.list(&self.config.id)
    }

    pub fn current_origin(&self) -> Result<String, BrowserError> {
        self.state
            .lock()
            .map_err(lock_error)?
            .current_origin
            .clone()
            .ok_or_else(|| BrowserError::new("browser-origin-unavailable", "navigate first"))
    }

    fn validate_command(&self, command: &BrowserCommand) -> Result<(), BrowserError> {
        if self.state.lock().map_err(lock_error)?.status != BrowserSessionStatus::Ready
            || !self.adapter.is_alive()
        {
            return Err(BrowserError::new(
                "browser-session-not-ready",
                self.config.id.to_string(),
            ));
        }
        match command {
            BrowserCommand::Navigate { url } => {
                let origin = origin_from_url(url)?;
                if !self.config.allowed_origins.contains(&origin) {
                    return Err(BrowserError::new("browser-origin-not-allowed", origin));
                }
            }
            BrowserCommand::Type {
                text,
                classification,
                ..
            } => {
                if *classification == ConfidentialityLabel::UserSecret {
                    return Err(BrowserError::new(
                        "browser-secret-input-requires-user-handoff",
                        "password/verification code must not enter Agent args",
                    ));
                }
                if text.chars().count() > 16_384 || text.contains('\0') {
                    return Err(BrowserError::new("browser-text-invalid", "length/control"));
                }
                self.require_origin()?;
            }
            BrowserCommand::Upload { path, .. } => {
                if !self.config.allow_uploads {
                    return Err(BrowserError::new(
                        "browser-upload-disabled",
                        path.display().to_string(),
                    ));
                }
                let canonical = std::fs::canonicalize(path)
                    .map_err(|error| BrowserError::new("browser-upload-path", error.to_string()))?;
                if !self
                    .config
                    .upload_roots
                    .iter()
                    .any(|root| canonical.starts_with(root))
                {
                    return Err(BrowserError::new(
                        "browser-upload-outside-roots",
                        canonical.display().to_string(),
                    ));
                }
                self.require_origin()?;
            }
            BrowserCommand::Download { .. } => {
                if !self.config.allow_downloads {
                    return Err(BrowserError::new("browser-download-disabled", "policy"));
                }
                self.require_origin()?;
            }
            _ => self.require_origin()?,
        }
        Ok(())
    }

    fn validate_result(
        &self,
        command: &BrowserCommand,
        result: BrowserResult,
    ) -> Result<BrowserResult, BrowserError> {
        match (command, &result) {
            (BrowserCommand::Snapshot, BrowserResult::Snapshot { snapshot })
                if snapshot.nodes.len() <= 500 => {}
            (BrowserCommand::Read { .. }, BrowserResult::Text { text })
                if text.chars().count() <= 1_000_000 => {}
            (BrowserCommand::Inspect { .. }, BrowserResult::Inspect { .. }) => {}
            (BrowserCommand::Screenshot, BrowserResult::Artifact { artifact })
                if artifact.mime_type == "image/png"
                    && artifact.path.starts_with(&self.config.artifact_directory) => {}
            (BrowserCommand::Download { .. }, BrowserResult::Artifact { artifact })
                if artifact.path.starts_with(&self.config.download_directory) => {}
            (BrowserCommand::Navigate { .. }, BrowserResult::Unit)
            | (BrowserCommand::Click { .. }, BrowserResult::Unit)
            | (BrowserCommand::Type { .. }, BrowserResult::Unit)
            | (BrowserCommand::Wait { .. }, BrowserResult::Unit)
            | (BrowserCommand::Upload { .. }, BrowserResult::Unit) => {}
            _ => {
                return Err(BrowserError::new(
                    "browser-result-mismatch",
                    format!("{:?}", command.kind()),
                ));
            }
        }
        Ok(result)
    }

    fn apply_result_state(
        &self,
        command: &BrowserCommand,
        result: &BrowserResult,
    ) -> Result<(), BrowserError> {
        let mut state = self.state.lock().map_err(lock_error)?;
        match (command, result) {
            (BrowserCommand::Navigate { url }, _) => {
                state.current_origin = Some(origin_from_url(url)?);
                state.snapshot_generation = state.snapshot_generation.saturating_add(1);
            }
            (BrowserCommand::Snapshot, BrowserResult::Snapshot { snapshot }) => {
                state.snapshot_generation = snapshot.generation;
            }
            (BrowserCommand::Click { .. } | BrowserCommand::Type { .. }, _) => {
                state.snapshot_generation = state.snapshot_generation.saturating_add(1);
            }
            _ => {}
        }
        Ok(())
    }

    fn require_origin(&self) -> Result<(), BrowserError> {
        let origin = self.current_origin()?;
        if self.config.allowed_origins.contains(&origin) {
            Ok(())
        } else {
            Err(BrowserError::new("browser-origin-not-allowed", origin))
        }
    }
}

fn command_metadata(
    command: &BrowserCommand,
    current_origin: Result<String, BrowserError>,
) -> (Option<String>, Option<String>) {
    match command {
        BrowserCommand::Navigate { url } => (origin_from_url(url).ok(), redacted_url_target(url)),
        BrowserCommand::Click { ref_id }
        | BrowserCommand::Read { ref_id }
        | BrowserCommand::Inspect { ref_id }
        | BrowserCommand::Download { ref_id } => (current_origin.ok(), Some(ref_id.clone())),
        BrowserCommand::Type { ref_id, .. } | BrowserCommand::Upload { ref_id, .. } => {
            (current_origin.ok(), Some(ref_id.clone()))
        }
        BrowserCommand::Wait { .. } | BrowserCommand::Snapshot | BrowserCommand::Screenshot => {
            (current_origin.ok(), None)
        }
    }
}

pub fn canonical_origin(value: &str) -> Result<String, BrowserError> {
    let url = Url::parse(value)
        .map_err(|error| BrowserError::new("browser-origin-invalid", error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(BrowserError::new("browser-origin-scheme", value));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BrowserError::new(
            "browser-url-credentials-denied",
            "userinfo must not enter Browser URL",
        ));
    }
    Ok(url.origin().ascii_serialization())
}

pub fn origin_from_url(value: &str) -> Result<String, BrowserError> {
    canonical_origin(value)
}

pub fn sha256_file(path: &Path) -> Result<ContentHash, BrowserError> {
    let bytes = std::fs::read(path)
        .map_err(|error| BrowserError::new("browser-artifact-read", error.to_string()))?;
    Ok(ContentHash::from(format!("{:x}", Sha256::digest(bytes))))
}

fn redacted_url_target(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    Some(format!(
        "{}{}",
        url.origin().ascii_serialization(),
        url.path()
    ))
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> BrowserError {
    BrowserError::new("browser-runtime-poisoned", "state")
}
