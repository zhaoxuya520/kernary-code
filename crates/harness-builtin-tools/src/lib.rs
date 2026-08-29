#![forbid(unsafe_code)]

//! Builtin File Tools 与 Workspace path enforcement。

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use harness_permission::PermissionAction;
use harness_sandbox::ProcessSandbox;
use harness_tool::{
    SandboxPort, ToolDescriptor, ToolEffectClass, ToolError, ToolExecutionInput,
    ToolInvocationJournal, ToolInvocationPatch, ToolInvocationStatus, ToolPromptLoading,
    ToolProvider, ToolRegistry, ToolSource,
};
use harness_types::ToolInvocationId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use process_wrap::std::CommandWrap;
#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;

#[derive(Clone, Debug)]
pub struct WorkspacePathGuard {
    canonical_root: PathBuf,
}

impl WorkspacePathGuard {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let canonical_root = fs::canonicalize(root.as_ref())
            .map_err(|error| io_error("workspace-root-canonicalize", error))?;
        if !canonical_root.is_dir() {
            return Err(ToolError::new(
                "workspace-root-not-directory",
                canonical_root.display().to_string(),
            ));
        }
        Ok(Self { canonical_root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn resolve_read(&self, requested: &Path) -> Result<PathBuf, ToolError> {
        let lexical = self.lexical_target(requested)?;
        let canonical = fs::canonicalize(&lexical)
            .map_err(|error| io_error("workspace-read-canonicalize", error))?;
        self.ensure_inside(&canonical)?;
        Ok(canonical)
    }

    pub fn resolve_write(&self, requested: &Path) -> Result<PathBuf, ToolError> {
        let lexical = self.lexical_target(requested)?;
        if lexical.exists() {
            let canonical = fs::canonicalize(&lexical)
                .map_err(|error| io_error("workspace-write-canonicalize", error))?;
            self.ensure_inside(&canonical)?;
            return Ok(canonical);
        }
        let mut existing = lexical.as_path();
        while !existing.exists() {
            existing = existing.parent().ok_or_else(|| {
                ToolError::new(
                    "workspace-write-parent-missing",
                    lexical.display().to_string(),
                )
            })?;
        }
        let canonical_parent = fs::canonicalize(existing)
            .map_err(|error| io_error("workspace-parent-canonicalize", error))?;
        self.ensure_inside(&canonical_parent)?;
        let suffix = lexical.strip_prefix(existing).map_err(|_| {
            ToolError::new(
                "workspace-write-suffix-invalid",
                lexical.display().to_string(),
            )
        })?;
        Ok(canonical_parent.join(suffix))
    }

    fn lexical_target(&self, requested: &Path) -> Result<PathBuf, ToolError> {
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.canonical_root.join(requested)
        };
        normalize_lexical(&joined)
    }

    fn ensure_inside(&self, target: &Path) -> Result<(), ToolError> {
        if is_inside(&self.canonical_root, target) {
            Ok(())
        } else {
            Err(ToolError::new(
                "workspace-path-escape",
                target.display().to_string(),
            ))
        }
    }
}

/// 只声明当前实际具备的技术隔离能力。
pub struct WorkspaceSandbox {
    guard: WorkspacePathGuard,
    allowed_executables: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PatchStatus {
    Prepared,
    Applied,
    Undone,
    Aborted,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PatchRecord {
    pub id: String,
    pub path: PathBuf,
    pub before_sha256: Option<String>,
    pub after_sha256: String,
    pub status: PatchStatus,
    pub created_at_millis: i64,
    pub updated_at_millis: i64,
}

pub struct PatchStore {
    root: PathBuf,
    guard: WorkspacePathGuard,
    sequence: AtomicU64,
}

impl PatchStore {
    pub fn open(root: impl AsRef<Path>, guard: WorkspacePathGuard) -> Result<Self, ToolError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("blobs"))
            .map_err(|error| io_error("patch-store-create", error))?;
        fs::create_dir_all(root.join("records"))
            .map_err(|error| io_error("patch-store-create", error))?;
        Ok(Self {
            root,
            guard,
            sequence: AtomicU64::new(1),
        })
    }

    pub fn prepare(
        &self,
        invocation_id: &ToolInvocationId,
        path: &Path,
        before: Option<&[u8]>,
        after: &[u8],
        now_millis: i64,
    ) -> Result<PatchRecord, ToolError> {
        self.prepare_with_id(invocation_id.to_string(), path, before, after, now_millis)
    }

    pub fn prepare_with_id(
        &self,
        id: impl Into<String>,
        path: &Path,
        before: Option<&[u8]>,
        after: &[u8],
        now_millis: i64,
    ) -> Result<PatchRecord, ToolError> {
        let path = self.guard.resolve_write(path)?;
        let id = id.into();
        if id.is_empty() || id.len() > 512 {
            return Err(ToolError::new("patch-id-invalid", id));
        }
        let before_sha256 = before.map(sha256);
        if let (Some(hash), Some(bytes)) = (&before_sha256, before) {
            let blob = self.root.join("blobs").join(hash);
            if !blob.exists() {
                fs::write(&blob, bytes).map_err(|error| io_error("patch-blob-write", error))?;
            } else {
                let existing =
                    fs::read(&blob).map_err(|error| io_error("patch-blob-read", error))?;
                if sha256(&existing) != *hash {
                    return Err(ToolError::new("patch-blob-corrupt", hash.clone()));
                }
            }
        }
        let record = PatchRecord {
            id,
            path,
            before_sha256,
            after_sha256: sha256(after),
            status: PatchStatus::Prepared,
            created_at_millis: now_millis,
            updated_at_millis: now_millis,
        };
        let record_path = self.record_path(&record.id);
        if record_path.exists() {
            let mut existing = self.load(&record.id)?;
            if matches!(existing.status, PatchStatus::Aborted | PatchStatus::Undone)
                && existing.path == record.path
                && existing.before_sha256 == record.before_sha256
                && existing.after_sha256 == record.after_sha256
            {
                let current_hash = if existing.path.exists() {
                    Some(sha256(
                        &fs::read(self.guard.resolve_read(&existing.path)?)
                            .map_err(|error| io_error("patch-current-read", error))?,
                    ))
                } else {
                    None
                };
                if current_hash != existing.before_sha256 {
                    return Err(ToolError::new(
                        "patch-reprepare-current-mismatch",
                        existing.path.display().to_string(),
                    ));
                }
                existing.status = PatchStatus::Prepared;
                existing.updated_at_millis = now_millis;
                self.write_record(&existing)?;
                return Ok(existing);
            }
            return Err(ToolError::new("patch-record-exists", record.id));
        }
        self.write_record(&record)?;
        Ok(record)
    }

    /// 校验 before/after hash 后应用 Prepared patch；不允许调用方绕过记录。
    pub fn apply_prepared(
        &self,
        id: &str,
        after: &[u8],
        now_millis: i64,
    ) -> Result<PatchRecord, ToolError> {
        let record = self.load(id)?;
        if record.status != PatchStatus::Prepared {
            return Err(ToolError::new(
                "patch-not-prepared",
                format!("id={id}, status={:?}", record.status),
            ));
        }
        if sha256(after) != record.after_sha256 {
            return Err(ToolError::new("patch-after-bytes-mismatch", id));
        }
        let current = if record.path.exists() {
            Some(
                fs::read(self.guard.resolve_read(&record.path)?)
                    .map_err(|error| io_error("patch-current-read", error))?,
            )
        } else {
            self.guard.resolve_write(&record.path)?;
            None
        };
        if current.as_deref().map(sha256) != record.before_sha256 {
            return Err(ToolError::new(
                "patch-before-hash-mismatch",
                record.path.display().to_string(),
            ));
        }
        replace_file(
            &record.path,
            after,
            self.sequence.fetch_add(1, Ordering::SeqCst),
        )?;
        self.mark_applied(id, now_millis)
    }

    pub fn abort_prepared(&self, id: &str, now_millis: i64) -> Result<PatchRecord, ToolError> {
        let mut record = self.load(id)?;
        if record.status != PatchStatus::Prepared {
            return Err(ToolError::new(
                "patch-not-prepared",
                format!("id={id}, status={:?}", record.status),
            ));
        }
        let current_hash = if record.path.exists() {
            Some(sha256(
                &fs::read(self.guard.resolve_read(&record.path)?)
                    .map_err(|error| io_error("patch-current-read", error))?,
            ))
        } else {
            None
        };
        if current_hash != record.before_sha256 {
            return Err(ToolError::new(
                "patch-abort-current-mismatch",
                record.path.display().to_string(),
            ));
        }
        record.status = PatchStatus::Aborted;
        record.updated_at_millis = now_millis;
        self.write_record(&record)?;
        Ok(record)
    }

    pub fn verify_undoable(&self, id: &str) -> Result<PatchRecord, ToolError> {
        let record = self.load(id)?;
        if record.status != PatchStatus::Applied {
            return Err(ToolError::new(
                "patch-not-applied",
                format!("id={id}, status={:?}", record.status),
            ));
        }
        let current = fs::read(self.guard.resolve_read(&record.path)?)
            .map_err(|error| io_error("patch-current-read", error))?;
        if sha256(&current) != record.after_sha256 {
            return Err(ToolError::new(
                "patch-current-hash-mismatch",
                record.path.display().to_string(),
            ));
        }
        if let Some(hash) = &record.before_sha256 {
            let bytes = fs::read(self.root.join("blobs").join(hash))
                .map_err(|error| io_error("patch-blob-read", error))?;
            if sha256(&bytes) != *hash {
                return Err(ToolError::new("patch-blob-corrupt", hash));
            }
        }
        Ok(record)
    }

    pub fn mark_applied(&self, id: &str, now_millis: i64) -> Result<PatchRecord, ToolError> {
        let mut record = self.load(id)?;
        if record.status != PatchStatus::Prepared {
            return Err(ToolError::new(
                "patch-not-prepared",
                format!("id={id}, status={:?}", record.status),
            ));
        }
        let current = fs::read(self.guard.resolve_read(&record.path)?)
            .map_err(|error| io_error("patch-verify-read", error))?;
        if sha256(&current) != record.after_sha256 {
            return Err(ToolError::new("patch-after-hash-mismatch", id));
        }
        record.status = PatchStatus::Applied;
        record.updated_at_millis = now_millis;
        self.write_record(&record)?;
        Ok(record)
    }

    pub fn undo(&self, id: &str, now_millis: i64) -> Result<PatchRecord, ToolError> {
        let mut record = self.load(id)?;
        if record.status != PatchStatus::Applied {
            return Err(ToolError::new(
                "patch-not-applied",
                format!("id={id}, status={:?}", record.status),
            ));
        }
        let path = self.guard.resolve_read(&record.path)?;
        let current = fs::read(&path).map_err(|error| io_error("patch-current-read", error))?;
        if sha256(&current) != record.after_sha256 {
            return Err(ToolError::new(
                "patch-current-hash-mismatch",
                "文件在 Harness patch 后已被修改，拒绝 Undo",
            ));
        }
        match &record.before_sha256 {
            Some(hash) => {
                let bytes = fs::read(self.root.join("blobs").join(hash))
                    .map_err(|error| io_error("patch-blob-read", error))?;
                if sha256(&bytes) != *hash {
                    return Err(ToolError::new("patch-blob-corrupt", hash.clone()));
                }
                replace_file(&path, &bytes, self.sequence.fetch_add(1, Ordering::SeqCst))?;
            }
            None => {
                fs::remove_file(&path).map_err(|error| io_error("patch-remove-created", error))?;
            }
        }
        record.status = PatchStatus::Undone;
        record.updated_at_millis = now_millis;
        self.write_record(&record)?;
        Ok(record)
    }

    /// 列出 PatchQueue。记录文件名必须与内容中的 ID 对应，避免伪造记录混入队列。
    pub fn list(&self) -> Result<Vec<PatchRecord>, ToolError> {
        let mut records = Vec::new();
        for entry in fs::read_dir(self.root.join("records"))
            .map_err(|error| io_error("patch-record-list", error))?
        {
            let entry = entry.map_err(|error| io_error("patch-record-list", error))?;
            if !entry
                .file_type()
                .map_err(|error| io_error("patch-record-type", error))?
                .is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let bytes =
                fs::read(entry.path()).map_err(|error| io_error("patch-record-read", error))?;
            let record: PatchRecord = serde_json::from_slice(&bytes)
                .map_err(|error| ToolError::new("patch-record-json", error.to_string()))?;
            if self.record_path(&record.id) != entry.path() {
                return Err(ToolError::new(
                    "patch-record-id-mismatch",
                    entry.path().display().to_string(),
                ));
            }
            records.push(record);
        }
        records.sort_by(|left, right| {
            left.created_at_millis
                .cmp(&right.created_at_millis)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(records)
    }

    pub fn undo_latest(&self, now_millis: i64) -> Result<PatchRecord, ToolError> {
        let record = self
            .list()?
            .into_iter()
            .rev()
            .find(|record| record.status == PatchStatus::Applied)
            .ok_or_else(|| ToolError::new("patch-undo-empty", "没有可撤销的 Harness Patch"))?;
        self.undo(&record.id, now_millis)
    }

    /// 崩溃可能发生在文件替换与 `mark_applied` 之间。只按内容 hash 恢复，绝不猜测。
    pub fn reconcile_prepared(&self, now_millis: i64) -> Result<Vec<PatchRecord>, ToolError> {
        let prepared = self
            .list()?
            .into_iter()
            .filter(|record| record.status == PatchStatus::Prepared)
            .collect::<Vec<_>>();
        let mut reconciled = Vec::with_capacity(prepared.len());
        for mut record in prepared {
            let current_hash = if record.path.exists() {
                let path = self.guard.resolve_read(&record.path)?;
                let bytes =
                    fs::read(path).map_err(|error| io_error("patch-reconcile-read", error))?;
                Some(sha256(&bytes))
            } else {
                self.guard.resolve_write(&record.path)?;
                None
            };
            record.status = if current_hash.as_deref() == Some(record.after_sha256.as_str()) {
                PatchStatus::Applied
            } else if current_hash == record.before_sha256 {
                PatchStatus::Aborted
            } else {
                PatchStatus::Uncertain
            };
            record.updated_at_millis = now_millis;
            self.write_record(&record)?;
            reconciled.push(record);
        }
        Ok(reconciled)
    }

    pub fn load(&self, id: &str) -> Result<PatchRecord, ToolError> {
        let bytes =
            fs::read(self.record_path(id)).map_err(|error| io_error("patch-record-read", error))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| ToolError::new("patch-record-json", error.to_string()))
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.root
            .join("records")
            .join(format!("{}.json", sha256(id.as_bytes())))
    }

    fn write_record(&self, record: &PatchRecord) -> Result<(), ToolError> {
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| ToolError::new("patch-record-json", error.to_string()))?;
        replace_file(
            &self.record_path(&record.id),
            &bytes,
            self.sequence.fetch_add(1, Ordering::SeqCst),
        )
    }
}

/// 用 Patch 内容证据收敛被进程崩溃打断的 `files.write` Journal 状态。
pub fn reconcile_patch_invocations(
    journal: &dyn ToolInvocationJournal,
    patches: &[PatchRecord],
    now_millis: i64,
) -> Result<Vec<harness_tool::ToolInvocationRecord>, ToolError> {
    let mut reconciled = Vec::new();
    for patch in patches {
        if !matches!(patch.status, PatchStatus::Applied | PatchStatus::Aborted) {
            continue;
        }
        let invocation_id = ToolInvocationId::from(patch.id.clone());
        let Some(invocation) = journal.get(&invocation_id)? else {
            continue;
        };
        if invocation.tool_name != "files.write"
            || invocation.status != ToolInvocationStatus::Uncertain
        {
            continue;
        }
        let (status, result, error) = match patch.status {
            PatchStatus::Applied => (
                ToolInvocationStatus::Completed,
                Some(serde_json::json!({
                    "path":patch.path.clone(),
                    "created":patch.before_sha256.is_none(),
                    "beforeSha256":patch.before_sha256.clone(),
                    "afterSha256":patch.after_sha256.clone(),
                    "patchId":patch.id.clone()
                })),
                None,
            ),
            PatchStatus::Aborted => (
                ToolInvocationStatus::Failed,
                None,
                Some("patch-reconciled-not-applied".to_owned()),
            ),
            _ => unreachable!("status filtered above"),
        };
        reconciled.push(journal.update(
            &invocation_id,
            ToolInvocationPatch {
                expected_status: ToolInvocationStatus::Uncertain,
                status,
                approval_request_id: invocation.approval_request_id,
                result,
                error,
                updated_at_millis: now_millis,
            },
        )?);
    }
    Ok(reconciled)
}

impl WorkspaceSandbox {
    #[must_use]
    pub fn new(guard: WorkspacePathGuard) -> Self {
        Self {
            guard,
            allowed_executables: vec![],
        }
    }

    pub fn with_processes(
        guard: WorkspacePathGuard,
        allowed_executables: Vec<PathBuf>,
    ) -> Result<Self, ToolError> {
        let allowed_executables = allowed_executables
            .into_iter()
            .map(|path| {
                fs::canonicalize(&path).map_err(|error| io_error("sandbox-executable", error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            guard,
            allowed_executables,
        })
    }
}

impl SandboxPort for WorkspaceSandbox {
    fn execute(
        &self,
        descriptor: &ToolDescriptor,
        permission_action: &PermissionAction,
        provider: &dyn ToolProvider,
        input: ToolExecutionInput,
    ) -> Result<serde_json::Value, ToolError> {
        match permission_action {
            PermissionAction::InternalCompute { .. } => {}
            PermissionAction::FilesystemRead { path } => {
                self.guard.resolve_read(path)?;
            }
            PermissionAction::FilesystemWrite { path } => {
                self.guard.resolve_write(path)?;
            }
            PermissionAction::WorkspacePatchApply { paths, .. } => {
                if !matches!(
                    descriptor.canonical_name.as_str(),
                    "lsp.patch.apply" | "lsp.patch.undo"
                ) || descriptor.source != ToolSource::Builtin
                {
                    return Err(ToolError::new(
                        "sandbox-workspace-patch-source-mismatch",
                        descriptor.canonical_name.clone(),
                    ));
                }
                if paths.is_empty() {
                    return Err(ToolError::new(
                        "sandbox-workspace-patch-empty",
                        descriptor.canonical_name.clone(),
                    ));
                }
                for path in paths {
                    self.guard.resolve_write(path)?;
                }
            }
            PermissionAction::ProcessSpawn {
                executable, cwd, ..
            } => {
                let executable = fs::canonicalize(executable)
                    .map_err(|error| io_error("sandbox-executable", error))?;
                if !self
                    .allowed_executables
                    .iter()
                    .any(|allowed| paths_equal(allowed, &executable))
                {
                    return Err(ToolError::new(
                        "sandbox-executable-denied",
                        executable.display().to_string(),
                    ));
                }
                let cwd = self.guard.resolve_read(cwd)?;
                if !cwd.is_dir() {
                    return Err(ToolError::new(
                        "sandbox-process-cwd-not-directory",
                        cwd.display().to_string(),
                    ));
                }
            }
            PermissionAction::McpCall {
                server_id,
                tool_name,
                ..
            } => match &descriptor.source {
                ToolSource::Mcp {
                    server_id: source_server,
                } if source_server.as_str() == server_id.as_str()
                    && descriptor.canonical_name == format!("mcp.{server_id}.{tool_name}") => {}
                _ => {
                    return Err(ToolError::new(
                        "sandbox-mcp-source-mismatch",
                        descriptor.canonical_name.clone(),
                    ));
                }
            },
            PermissionAction::PluginCall {
                plugin_id,
                capability,
                ..
            } => match &descriptor.source {
                ToolSource::Plugin {
                    plugin_id: source_plugin,
                } if source_plugin.as_str() == plugin_id.as_str()
                    && descriptor.canonical_name == format!("plugin.{plugin_id}.{capability}") => {}
                _ => {
                    return Err(ToolError::new(
                        "sandbox-plugin-source-mismatch",
                        descriptor.canonical_name.clone(),
                    ));
                }
            },
            PermissionAction::BrowserUpload { path, .. } => {
                self.guard.resolve_read(path)?;
                ensure_browser_tool_source(descriptor)?;
            }
            PermissionAction::BrowserOpen { .. }
            | PermissionAction::BrowserSnapshot { .. }
            | PermissionAction::BrowserAct { .. }
            | PermissionAction::BrowserDownload { .. } => {
                ensure_browser_tool_source(descriptor)?;
            }
            _ => {
                return Err(ToolError::new(
                    "sandbox-capability-unavailable",
                    "WorkspaceSandbox 只实现 internal/filesystem/allowlisted process/typed extension proxy；Network/Browser 需要专用 Sandbox",
                ));
            }
        }
        provider.execute(input)
    }
}

fn ensure_browser_tool_source(descriptor: &ToolDescriptor) -> Result<(), ToolError> {
    if descriptor.source == ToolSource::Internal
        && descriptor.canonical_name.starts_with("browser.")
    {
        Ok(())
    } else {
        Err(ToolError::new(
            "sandbox-browser-source-mismatch",
            descriptor.canonical_name.clone(),
        ))
    }
}

pub fn register_process_tool(
    registry: &mut ToolRegistry,
    guard: WorkspacePathGuard,
    process_sandbox: Arc<ProcessSandbox>,
    allowed_executables: Vec<PathBuf>,
    max_timeout: Duration,
    max_output_bytes: usize,
) -> Result<(), ToolError> {
    let allowed_executables = allowed_executables
        .into_iter()
        .map(|path| fs::canonicalize(path).map_err(|error| io_error("process-executable", error)))
        .collect::<Result<Vec<_>, _>>()?;
    let executable_values = allowed_executables
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    registry.register(
        ToolDescriptor {
            canonical_name: "process.exec".to_owned(),
            version: "1".to_owned(),
            description: "直接执行 allowlisted executable + argv；不经过 shell 拼接".to_owned(),
            effect_class: ToolEffectClass::NonRepeatableEffect,
            source: ToolSource::Builtin,
            prompt_loading: ToolPromptLoading::Eager,
            keywords: vec![
                "process".to_owned(),
                "shell".to_owned(),
                "command".to_owned(),
            ],
            input_schema: serde_json::json!({
                "type":"object",
                "properties":{
                    "executable":{"type":"string","enum":executable_values},
                    "arguments":{"type":"array","items":{"type":"string"}},
                    "cwd":{"type":"string"},
                    "timeoutMs":{"type":"integer","minimum":1}
                },
                "required":["executable","arguments"],
                "additionalProperties":false
            }),
            output_schema: serde_json::json!({"type":"object"}),
        },
        Arc::new(ProcessExecTool {
            guard,
            process_sandbox,
            allowed_executables,
            max_timeout,
            max_output_bytes,
        }),
    )
}

pub fn register_file_tools(
    registry: &mut ToolRegistry,
    guard: WorkspacePathGuard,
    max_read_bytes: usize,
) -> Result<(), ToolError> {
    register_file_tools_with_patch_store(registry, guard, max_read_bytes, None)
}

pub fn register_file_tools_with_patch_store(
    registry: &mut ToolRegistry,
    guard: WorkspacePathGuard,
    max_read_bytes: usize,
    patch_store: Option<Arc<PatchStore>>,
) -> Result<(), ToolError> {
    let write_effect_class = if patch_store.is_some() {
        ToolEffectClass::VerifiableEffect
    } else {
        ToolEffectClass::IdempotentEffect
    };
    registry.register(
        ToolDescriptor {
            canonical_name: "files.read".to_owned(),
            version: "1".to_owned(),
            description: "读取 workspace 内 UTF-8 文件".to_owned(),
            effect_class: ToolEffectClass::ReadOnlyRetryable,
            source: ToolSource::Builtin,
            prompt_loading: ToolPromptLoading::Eager,
            keywords: vec!["read".to_owned(), "file".to_owned()],
            input_schema: serde_json::json!({
                "type":"object",
                "properties":{"path":{"type":"string"}},
                "required":["path"],
                "additionalProperties":false
            }),
            output_schema: serde_json::json!({"type":"object"}),
        },
        Arc::new(FileReadTool {
            guard: guard.clone(),
            max_read_bytes,
        }),
    )?;
    registry.register(
        ToolDescriptor {
            canonical_name: "files.write".to_owned(),
            version: "1".to_owned(),
            description: "原子写入 workspace 内 UTF-8 文件".to_owned(),
            effect_class: write_effect_class,
            source: ToolSource::Builtin,
            prompt_loading: ToolPromptLoading::Eager,
            keywords: vec!["write".to_owned(), "file".to_owned(), "edit".to_owned()],
            input_schema: serde_json::json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "content":{"type":"string"},
                    "createParents":{"type":"boolean"}
                },
                "required":["path","content"],
                "additionalProperties":false
            }),
            output_schema: serde_json::json!({"type":"object"}),
        },
        Arc::new(FileWriteTool {
            guard,
            sequence: AtomicU64::new(1),
            patch_store,
        }),
    )?;
    Ok(())
}

struct FileReadTool {
    guard: WorkspacePathGuard,
    max_read_bytes: usize,
}

impl ToolProvider for FileReadTool {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        path_arg(value)?;
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .is_none()
            || value
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .is_none()
        {
            return Err(ToolError::new(
                "files-read-result-invalid",
                "content/hash missing",
            ));
        }
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        Ok(PermissionAction::FilesystemRead {
            path: self.guard.resolve_read(&path_arg(args)?)?,
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        if input.cancellation.is_cancelled() {
            return Err(ToolError::new("tool-cancelled", "files.read cancelled"));
        }
        let path = self.guard.resolve_read(&path_arg(&input.args)?)?;
        let metadata =
            fs::metadata(&path).map_err(|error| io_error("files-read-metadata", error))?;
        if !metadata.is_file() {
            return Err(ToolError::new(
                "files-read-not-file",
                path.display().to_string(),
            ));
        }
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if size > self.max_read_bytes {
            return Err(ToolError::new(
                "files-read-too-large",
                format!("size={size}, max={}", self.max_read_bytes),
            ));
        }
        let bytes = fs::read(&path).map_err(|error| io_error("files-read", error))?;
        let content_hash = sha256(&bytes);
        let byte_count = bytes.len();
        let content = String::from_utf8(bytes)
            .map_err(|_| ToolError::new("files-read-not-utf8", path.display().to_string()))?;
        Ok(serde_json::json!({
            "path":path,
            "content":content,
            "bytes":byte_count,
            "sha256":content_hash
        }))
    }
}

struct FileWriteTool {
    guard: WorkspacePathGuard,
    sequence: AtomicU64,
    patch_store: Option<Arc<PatchStore>>,
}

struct ProcessExecTool {
    guard: WorkspacePathGuard,
    process_sandbox: Arc<ProcessSandbox>,
    allowed_executables: Vec<PathBuf>,
    max_timeout: Duration,
    max_output_bytes: usize,
}

type CapturedOutput = Result<(Vec<u8>, bool), std::io::Error>;
type OutputReader = Option<thread::JoinHandle<CapturedOutput>>;

impl ToolProvider for ProcessExecTool {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let executable = value
            .get("executable")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::new("process-args-invalid", "executable missing"))?;
        if !Path::new(executable).is_absolute() {
            return Err(ToolError::new(
                "process-executable-not-absolute",
                executable,
            ));
        }
        let canonical_executable =
            fs::canonicalize(executable).map_err(|error| io_error("process-executable", error))?;
        if !self
            .allowed_executables
            .iter()
            .any(|allowed| paths_equal(allowed, &canonical_executable))
        {
            return Err(ToolError::new(
                "process-executable-not-allowlisted",
                executable,
            ));
        }
        if value
            .get("arguments")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|arguments| arguments.iter().any(|arg| !arg.is_string()))
        {
            return Err(ToolError::new(
                "process-args-invalid",
                "arguments 必须是字符串数组",
            ));
        }
        let timeout = value
            .get("timeoutMs")
            .and_then(serde_json::Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(self.max_timeout);
        if timeout == Duration::ZERO || timeout > self.max_timeout {
            return Err(ToolError::new(
                "process-timeout-invalid",
                format!("timeout={timeout:?}, max={:?}", self.max_timeout),
            ));
        }
        if let Some(cwd) = value.get("cwd").and_then(serde_json::Value::as_str) {
            let cwd = self.guard.resolve_read(Path::new(cwd))?;
            if !cwd.is_dir() {
                return Err(ToolError::new(
                    "process-cwd-not-directory",
                    cwd.display().to_string(),
                ));
            }
        }
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if value
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .is_none()
            || value
                .get("stdout")
                .and_then(serde_json::Value::as_str)
                .is_none()
            || value
                .get("stderr")
                .and_then(serde_json::Value::as_str)
                .is_none()
        {
            return Err(ToolError::new("process-result-invalid", "missing fields"));
        }
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        let executable = PathBuf::from(
            args.get("executable")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolError::new("process-args-invalid", "executable"))?,
        );
        let arguments = args["arguments"]
            .as_array()
            .expect("validated arguments")
            .iter()
            .map(|value| value.as_str().expect("validated argument").to_owned())
            .collect();
        let cwd = args
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || Ok(self.guard.root().to_path_buf()),
                |cwd| self.guard.resolve_read(Path::new(cwd)),
            )?;
        Ok(PermissionAction::ProcessSpawn {
            executable: fs::canonicalize(executable)
                .map_err(|error| io_error("process-executable", error))?,
            arguments,
            cwd,
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        let executable = fs::canonicalize(PathBuf::from(
            input.args["executable"]
                .as_str()
                .expect("validated executable"),
        ))
        .map_err(|error| io_error("process-executable", error))?;
        let arguments = input.args["arguments"]
            .as_array()
            .expect("validated arguments")
            .iter()
            .map(|value| value.as_str().expect("validated argument").to_owned())
            .collect::<Vec<_>>();
        let cwd = input
            .args
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || Ok(self.guard.root().to_path_buf()),
                |cwd| self.guard.resolve_read(Path::new(cwd)),
            )?;
        let timeout = input
            .args
            .get("timeoutMs")
            .and_then(serde_json::Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(self.max_timeout);
        let mut command = self
            .process_sandbox
            .command(&executable, &arguments, &cwd)
            .map_err(|error| ToolError::new(error.code, error.message))?;
        command
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in [
            "PATH",
            "SystemRoot",
            "SYSTEMROOT",
            "TEMP",
            "TMP",
            "LANG",
            "LC_ALL",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        self.process_sandbox
            .apply_environment(&mut command)
            .map_err(|error| ToolError::new(error.code, error.message))?;
        let mut wrapped = CommandWrap::from(command);
        #[cfg(windows)]
        wrapped.wrap(JobObject);
        #[cfg(unix)]
        wrapped.wrap(ProcessGroup::leader());
        let mut child = wrapped
            .spawn()
            .map_err(|error| io_error("process-spawn", error))?;
        let stdout = child.stdout().take();
        let stderr = child.stderr().take();
        let stdout_reader = spawn_reader(stdout, self.max_output_bytes);
        let stderr_reader = spawn_reader(stderr, self.max_output_bytes);
        let started = Instant::now();
        let mut timed_out = false;
        let mut cancelled = false;
        let status = loop {
            if input.cancellation.is_cancelled() {
                cancelled = true;
                child
                    .kill()
                    .map_err(|error| io_error("process-tree-kill", error))?;
                break child
                    .try_wait()
                    .map_err(|error| io_error("process-wait", error))?
                    .ok_or_else(|| ToolError::new("process-kill-not-confirmed", "cancel"))?;
            }
            if started.elapsed() >= timeout {
                timed_out = true;
                child
                    .kill()
                    .map_err(|error| io_error("process-tree-kill", error))?;
                break child
                    .try_wait()
                    .map_err(|error| io_error("process-wait", error))?
                    .ok_or_else(|| ToolError::new("process-kill-not-confirmed", "timeout"))?;
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|error| io_error("process-try-wait", error))?
            {
                break status;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let (stdout, stdout_truncated) = join_reader(stdout_reader)?;
        let (stderr, stderr_truncated) = join_reader(stderr_reader)?;
        Ok(serde_json::json!({
            "success":status.success() && !timed_out && !cancelled,
            "exitCode":status.code(),
            "timedOut":timed_out,
            "cancelled":cancelled,
            "stdout":String::from_utf8_lossy(&stdout),
            "stderr":String::from_utf8_lossy(&stderr),
            "stdoutTruncated":stdout_truncated,
            "stderrTruncated":stderr_truncated,
            "durationMs":u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        }))
    }
}

fn spawn_reader<R: Read + Send + 'static>(reader: Option<R>, limit: usize) -> OutputReader {
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

fn join_reader(reader: OutputReader) -> Result<(Vec<u8>, bool), ToolError> {
    reader.map_or(Ok((vec![], false)), |reader| {
        reader
            .join()
            .map_err(|_| ToolError::new("process-reader-panicked", "reader thread"))?
            .map_err(|error| io_error("process-output-read", error))
    })
}

impl ToolProvider for FileWriteTool {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        path_arg(value)?;
        if value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return Err(ToolError::new(
                "files-write-args-invalid",
                "content missing",
            ));
        }
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if value
            .get("afterSha256")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return Err(ToolError::new(
                "files-write-result-invalid",
                "after hash missing",
            ));
        }
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        Ok(PermissionAction::FilesystemWrite {
            path: self.guard.resolve_write(&path_arg(args)?)?,
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        if input.cancellation.is_cancelled() {
            return Err(ToolError::new("tool-cancelled", "files.write cancelled"));
        }
        let path = self.guard.resolve_write(&path_arg(&input.args)?)?;
        let content = input.args["content"].as_str().expect("validated content");
        let create_parents = input
            .args
            .get("createParents")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let parent = path.parent().ok_or_else(|| {
            ToolError::new("files-write-parent-missing", path.display().to_string())
        })?;
        if create_parents {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("files-write-create-parent", error))?;
            self.guard.resolve_write(&path)?;
        } else if !parent.exists() {
            return Err(ToolError::new(
                "files-write-parent-missing",
                parent.display().to_string(),
            ));
        }
        let before = if path.exists() {
            Some(fs::read(&path).map_err(|error| io_error("files-write-read-before", error))?)
        } else {
            None
        };
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let after = content.as_bytes();
        let patch = self
            .patch_store
            .as_ref()
            .map(|store| {
                store.prepare(
                    &input.invocation_id,
                    &path,
                    before.as_deref(),
                    after,
                    input.now_millis,
                )
            })
            .transpose()?;
        replace_file(&path, after, sequence)?;
        if let (Some(store), Some(patch)) = (&self.patch_store, &patch) {
            store.mark_applied(&patch.id, input.now_millis)?;
        }
        Ok(serde_json::json!({
            "path":path,
            "created":before.is_none(),
            "bytes":after.len(),
            "beforeSha256":before.as_deref().map(sha256),
            "afterSha256":sha256(after),
            "patchId":patch.map(|patch| patch.id)
        }))
    }
}

fn path_arg(value: &serde_json::Value) -> Result<PathBuf, ToolError> {
    value
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| ToolError::new("tool-path-missing", "path must be string"))
}

fn normalize_lexical(path: &Path) -> Result<PathBuf, ToolError> {
    if !path.is_absolute() {
        return Err(ToolError::new(
            "workspace-path-not-absolute",
            path.display().to_string(),
        ));
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                output.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return Err(ToolError::new(
                        "workspace-path-parent-overflow",
                        path.display().to_string(),
                    ));
                }
            }
        }
    }
    Ok(output)
}

fn is_inside(root: &Path, target: &Path) -> bool {
    let root = normalized_compare(root);
    let target = normalized_compare(target);
    target == root || target.starts_with(&format!("{root}{}", std::path::MAIN_SEPARATOR))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    normalized_compare(left) == normalized_compare(right)
}

fn normalized_compare(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn replace_file(path: &Path, bytes: &[u8], sequence: u64) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::new("replace-file-parent-missing", path.display().to_string()))?;
    fs::create_dir_all(parent).map_err(|error| io_error("replace-file-create-parent", error))?;
    let temporary = parent.join(format!(
        ".harness-write-{}-{sequence}.tmp",
        std::process::id()
    ));
    let backup = parent.join(format!(
        ".harness-backup-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| io_error("replace-file-temp-open", error))?;
        file.write_all(bytes)
            .map_err(|error| io_error("replace-file-temp-write", error))?;
        file.sync_all()
            .map_err(|error| io_error("replace-file-temp-sync", error))?;
        let replaced_existing = path.exists();
        if replaced_existing {
            fs::rename(path, &backup).map_err(|error| io_error("replace-file-backup", error))?;
        }
        if let Err(error) = fs::rename(&temporary, path) {
            if replaced_existing && backup.exists() {
                let _ = fs::rename(&backup, path);
            }
            return Err(io_error("replace-file-rename", error));
        }
        if backup.exists() {
            let _ = fs::remove_file(&backup);
        }
        Ok(())
    })();
    if result.is_err() && temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn io_error(code: &'static str, error: std::io::Error) -> ToolError {
    ToolError::new(code, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use harness_permission::{
        ApprovalPolicy, ExecutionEnvelope, PermissionEngine, workspace_write_profile,
    };
    use harness_tool::{MemoryToolJournal, ToolInvokeRequest, ToolRuntime};
    use harness_types::{
        ActorId, ConfidentialityLabel, InformationFlowLabel, IntegrityLabel, MissionId,
        PermissionRequestId, ProjectId, RunId, ToolInvocationId,
    };
    use tempfile::tempdir;

    use super::*;
    use harness_sandbox::SandboxMode;

    struct NoopBrowserProvider;
    impl ToolProvider for NoopBrowserProvider {
        fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(value.clone())
        }
        fn validate_result(
            &self,
            value: &serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            Ok(value.clone())
        }
        fn permission_action(
            &self,
            _args: &serde_json::Value,
        ) -> Result<PermissionAction, ToolError> {
            Ok(PermissionAction::BrowserSnapshot {
                origin: "http://127.0.0.1:4173".to_owned(),
            })
        }
        fn execute(&self, _input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"ok":true}))
        }
    }

    fn envelope() -> ExecutionEnvelope {
        ExecutionEnvelope {
            project_id: ProjectId::from("project:files"),
            mission_id: MissionId::from("mission:files"),
            run_id: Some(RunId::from("run:files")),
            actor_id: ActorId::from("agent:files"),
            origin: harness_permission::InvocationOrigin::Agent,
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality: ConfidentialityLabel::ProjectPrivate,
            },
        }
    }

    #[test]
    fn guard_rejects_traversal_and_similar_prefix() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().join("project");
        fs::create_dir_all(&root).expect("root");
        let guard = WorkspacePathGuard::new(&root).expect("guard");
        assert!(guard.resolve_write(Path::new("../escape.txt")).is_err());
        assert!(
            guard
                .resolve_write(&temporary.path().join("project-other/file.txt"))
                .is_err()
        );
    }

    #[test]
    fn workspace_sandbox_only_proxies_typed_internal_browser_tools() {
        let temporary = tempdir().expect("tempdir");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let sandbox = WorkspaceSandbox::new(guard);
        let descriptor = ToolDescriptor {
            canonical_name: "browser.snapshot".to_owned(),
            version: "1".to_owned(),
            description: "snapshot".to_owned(),
            effect_class: ToolEffectClass::ReadOnlyRetryable,
            source: ToolSource::Internal,
            prompt_loading: ToolPromptLoading::OnDemand,
            keywords: vec!["browser".to_owned()],
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: serde_json::json!({"type":"object"}),
        };
        let input = ToolExecutionInput {
            invocation_id: ToolInvocationId::from("tool:browser"),
            envelope: envelope(),
            args: serde_json::json!({}),
            cancellation: harness_tool::ToolCancellationToken::new(),
            now_millis: 1,
        };
        let result = sandbox
            .execute(
                &descriptor,
                &PermissionAction::BrowserSnapshot {
                    origin: "http://127.0.0.1:4173".to_owned(),
                },
                &NoopBrowserProvider,
                input.clone(),
            )
            .expect("browser proxy");
        assert_eq!(result["ok"], true);
        let mut wrong = descriptor;
        wrong.source = ToolSource::Test;
        assert_eq!(
            sandbox
                .execute(
                    &wrong,
                    &PermissionAction::BrowserSnapshot {
                        origin: "http://127.0.0.1:4173".to_owned(),
                    },
                    &NoopBrowserProvider,
                    input,
                )
                .expect_err("wrong source")
                .code,
            "sandbox-browser-source-mismatch"
        );
    }

    #[test]
    fn file_tools_execute_through_permission_and_sandbox() {
        let temporary = tempdir().expect("tempdir");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let mut registry = ToolRegistry::new();
        register_file_tools(&mut registry, guard.clone(), 1024).expect("register");
        let runtime = ToolRuntime::new(
            registry,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::NeverWithinSandbox,
            ),
            Arc::new(MemoryToolJournal::new()),
            Arc::new(WorkspaceSandbox::new(guard)),
        );
        let write = runtime
            .invoke(ToolInvokeRequest {
                invocation_id: ToolInvocationId::from("invocation:write"),
                approval_request_id: PermissionRequestId::from("approval:write"),
                idempotency_key: "write:1".to_owned(),
                envelope: envelope(),
                tool_name: "files.write".to_owned(),
                args: serde_json::json!({"path":"hello.txt","content":"hello"}),
                now_millis: 1,
            })
            .expect("write");
        assert_eq!(
            write.invocation.status,
            harness_tool::ToolInvocationStatus::Completed
        );
        let read = runtime
            .invoke(ToolInvokeRequest {
                invocation_id: ToolInvocationId::from("invocation:read"),
                approval_request_id: PermissionRequestId::from("approval:read"),
                idempotency_key: "read:1".to_owned(),
                envelope: envelope(),
                tool_name: "files.read".to_owned(),
                args: serde_json::json!({"path":"hello.txt"}),
                now_millis: 2,
            })
            .expect("read");
        assert_eq!(read.invocation.result.expect("result")["content"], "hello");
    }

    fn shell_executable() -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
                .join("System32/cmd.exe")
        }
        #[cfg(unix)]
        {
            PathBuf::from("/bin/sh")
        }
    }

    fn process_runtime(root: &Path, max_timeout: Duration) -> (ToolRuntime, PathBuf) {
        let guard = WorkspacePathGuard::new(root).expect("guard");
        let executable = fs::canonicalize(shell_executable()).expect("shell executable");
        let mut registry = ToolRegistry::new();
        register_process_tool(
            &mut registry,
            guard.clone(),
            Arc::new(
                ProcessSandbox::new(guard.root(), SandboxMode::DangerFullAccess, true)
                    .expect("process sandbox"),
            ),
            vec![executable.clone()],
            max_timeout,
            1024,
        )
        .expect("register");
        let mut profile = workspace_write_profile(guard.root().to_path_buf());
        profile.subprocess.allowed_executables = vec![executable.clone()];
        let runtime = ToolRuntime::new(
            registry,
            PermissionEngine::new(profile, ApprovalPolicy::NeverWithinSandbox),
            Arc::new(MemoryToolJournal::new()),
            Arc::new(
                WorkspaceSandbox::with_processes(guard, vec![executable.clone()]).expect("sandbox"),
            ),
        );
        (runtime, executable)
    }

    #[test]
    fn process_tool_runs_in_group_with_bounded_output() {
        let temporary = tempdir().expect("tempdir");
        let (runtime, executable) = process_runtime(temporary.path(), Duration::from_secs(2));
        #[cfg(windows)]
        let arguments = serde_json::json!(["/D", "/S", "/C", "echo process-ok"]);
        #[cfg(unix)]
        let arguments = serde_json::json!(["-c", "printf process-ok"]);
        let result = runtime
            .invoke(ToolInvokeRequest {
                invocation_id: ToolInvocationId::from("invocation:process"),
                approval_request_id: PermissionRequestId::from("approval:process"),
                idempotency_key: "process:1".to_owned(),
                envelope: envelope(),
                tool_name: "process.exec".to_owned(),
                args: serde_json::json!({
                    "executable":executable,
                    "arguments":arguments,
                    "cwd":temporary.path(),
                    "timeoutMs":1000
                }),
                now_millis: 1,
            })
            .expect("process");
        assert_eq!(
            result.invocation.status,
            harness_tool::ToolInvocationStatus::Completed
        );
        let output = result.invocation.result.expect("result");
        assert!(
            output["stdout"]
                .as_str()
                .expect("stdout")
                .contains("process-ok")
        );
        assert_eq!(output["timedOut"], false);
    }

    #[test]
    fn timeout_kills_process_group_and_returns_known_result() {
        let temporary = tempdir().expect("tempdir");
        let (runtime, executable) = process_runtime(temporary.path(), Duration::from_secs(2));
        #[cfg(windows)]
        let arguments = serde_json::json!(["/D", "/S", "/C", "ping -n 6 127.0.0.1 >NUL"]);
        #[cfg(unix)]
        let arguments = serde_json::json!(["-c", "sleep 5 & wait"]);
        let started = Instant::now();
        let result = runtime
            .invoke(ToolInvokeRequest {
                invocation_id: ToolInvocationId::from("invocation:timeout"),
                approval_request_id: PermissionRequestId::from("approval:timeout"),
                idempotency_key: "process:timeout".to_owned(),
                envelope: envelope(),
                tool_name: "process.exec".to_owned(),
                args: serde_json::json!({
                    "executable":executable,
                    "arguments":arguments,
                    "cwd":temporary.path(),
                    "timeoutMs":100
                }),
                now_millis: 1,
            })
            .expect("timeout result");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            result.invocation.status,
            harness_tool::ToolInvocationStatus::Completed
        );
        assert_eq!(result.invocation.result.expect("result")["timedOut"], true);
    }

    #[test]
    fn runtime_cancel_kills_process_tree_and_marks_nonrepeatable_uncertain() {
        let temporary = tempdir().expect("tempdir");
        let (runtime, executable) = process_runtime(temporary.path(), Duration::from_secs(10));
        let runtime = Arc::new(runtime);
        let invocation_id = ToolInvocationId::from("invocation:cancel-process");
        #[cfg(windows)]
        let arguments = serde_json::json!(["/D", "/S", "/C", "ping -n 20 127.0.0.1 >NUL"]);
        #[cfg(unix)]
        let arguments = serde_json::json!(["-c", "sleep 5 & wait"]);
        let worker_runtime = runtime.clone();
        let worker_id = invocation_id.clone();
        let root = temporary.path().to_path_buf();
        let worker = thread::spawn(move || {
            worker_runtime.invoke(ToolInvokeRequest {
                invocation_id: worker_id,
                approval_request_id: PermissionRequestId::from("approval:cancel-process"),
                idempotency_key: "process:cancel".to_owned(),
                envelope: envelope(),
                tool_name: "process.exec".to_owned(),
                args: serde_json::json!({
                    "executable":executable,
                    "arguments":arguments,
                    "cwd":root,
                    "timeoutMs":9_000
                }),
                now_millis: 1,
            })
        });
        let started = Instant::now();
        while runtime
            .active_invocations()
            .expect("active invocations")
            .is_empty()
        {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "tool did not start"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(runtime.cancel(&invocation_id).expect("cancel"));
        let result = worker.join().expect("worker").expect("invoke");
        assert_eq!(
            result.invocation.status,
            harness_tool::ToolInvocationStatus::Uncertain
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(runtime.active_invocations().expect("active").is_empty());
    }

    #[test]
    fn patch_store_undo_restores_only_unchanged_harness_write() {
        let temporary = tempdir().expect("tempdir");
        let file = temporary.path().join("tracked.txt");
        fs::write(&file, "before").expect("before");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let patch_store = Arc::new(
            PatchStore::open(temporary.path().join(".harness/patches"), guard.clone())
                .expect("patch store"),
        );
        let mut registry = ToolRegistry::new();
        register_file_tools_with_patch_store(
            &mut registry,
            guard.clone(),
            1024,
            Some(patch_store.clone()),
        )
        .expect("register");
        let runtime = ToolRuntime::new(
            registry,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::NeverWithinSandbox,
            ),
            Arc::new(MemoryToolJournal::new()),
            Arc::new(WorkspaceSandbox::new(guard)),
        );
        let write = runtime
            .invoke(ToolInvokeRequest {
                invocation_id: ToolInvocationId::from("invocation:patch"),
                approval_request_id: PermissionRequestId::from("approval:patch"),
                idempotency_key: "patch:1".to_owned(),
                envelope: envelope(),
                tool_name: "files.write".to_owned(),
                args: serde_json::json!({"path":file,"content":"after"}),
                now_millis: 10,
            })
            .expect("write");
        let patch_id = write.invocation.result.expect("result")["patchId"]
            .as_str()
            .expect("patch id")
            .to_owned();
        assert_eq!(fs::read_to_string(&file).expect("after"), "after");
        let undone = patch_store.undo(&patch_id, 20).expect("undo");
        assert_eq!(undone.status, PatchStatus::Undone);
        assert_eq!(fs::read_to_string(&file).expect("restored"), "before");

        let prepared = patch_store
            .prepare(
                &ToolInvocationId::from("invocation:conflict"),
                &file,
                Some(b"before"),
                b"expected-after",
                30,
            )
            .expect("prepare conflict");
        replace_file(&file, b"expected-after", 99).expect("apply");
        patch_store.mark_applied(&prepared.id, 31).expect("mark");
        fs::write(&file, "user-change").expect("user change");
        assert_eq!(
            patch_store
                .undo(&prepared.id, 32)
                .expect_err("hash conflict")
                .code,
            "patch-current-hash-mismatch"
        );
        assert_eq!(fs::read_to_string(&file).expect("kept"), "user-change");
    }

    #[test]
    fn patch_recovery_classifies_prepared_records_without_guessing() {
        let temporary = tempdir().expect("tempdir");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let store = PatchStore::open(temporary.path().join(".harness/patches"), guard)
            .expect("patch store");

        let applied_path = temporary.path().join("applied.txt");
        fs::write(&applied_path, "before").expect("before");
        store
            .prepare(
                &ToolInvocationId::from("invocation:prepared-applied"),
                &applied_path,
                Some(b"before"),
                b"after",
                1,
            )
            .expect("prepare applied");
        fs::write(&applied_path, "after").expect("simulate applied write");

        let aborted_path = temporary.path().join("aborted.txt");
        store
            .prepare(
                &ToolInvocationId::from("invocation:prepared-aborted"),
                &aborted_path,
                None,
                b"never-written",
                2,
            )
            .expect("prepare aborted");

        let uncertain_path = temporary.path().join("uncertain.txt");
        fs::write(&uncertain_path, "before").expect("before uncertain");
        store
            .prepare(
                &ToolInvocationId::from("invocation:prepared-uncertain"),
                &uncertain_path,
                Some(b"before"),
                b"expected-after",
                3,
            )
            .expect("prepare uncertain");
        fs::write(&uncertain_path, "external-change").expect("external change");

        let reconciled = store.reconcile_prepared(10).expect("reconcile");
        assert_eq!(reconciled.len(), 3);
        assert_eq!(
            store
                .load("invocation:prepared-applied")
                .expect("applied")
                .status,
            PatchStatus::Applied
        );
        assert_eq!(
            store
                .load("invocation:prepared-aborted")
                .expect("aborted")
                .status,
            PatchStatus::Aborted
        );
        assert_eq!(
            store
                .load("invocation:prepared-uncertain")
                .expect("uncertain")
                .status,
            PatchStatus::Uncertain
        );
        let journal = MemoryToolJournal::new();
        for (id, path) in [
            ("invocation:prepared-applied", applied_path.clone()),
            ("invocation:prepared-aborted", aborted_path),
            ("invocation:prepared-uncertain", uncertain_path),
        ] {
            journal
                .create(harness_tool::ToolInvocationRecord {
                    id: ToolInvocationId::from(id),
                    idempotency_key: format!("key:{id}"),
                    envelope: envelope(),
                    tool_name: "files.write".to_owned(),
                    tool_version: "1".to_owned(),
                    effect_class: ToolEffectClass::VerifiableEffect,
                    status: harness_tool::ToolInvocationStatus::Uncertain,
                    args: serde_json::json!({"path":path,"content":"after"}),
                    permission_action: PermissionAction::FilesystemWrite { path },
                    approval_request_id: None,
                    result: None,
                    error: Some("interrupted-before-result".to_owned()),
                    created_at_millis: 1,
                    updated_at_millis: 10,
                })
                .expect("journal record");
        }
        let journal_reconciled =
            reconcile_patch_invocations(&journal, &reconciled, 11).expect("journal reconcile");
        assert_eq!(journal_reconciled.len(), 2);
        assert_eq!(
            journal
                .get(&ToolInvocationId::from("invocation:prepared-applied"))
                .expect("journal")
                .expect("applied")
                .status,
            harness_tool::ToolInvocationStatus::Completed
        );
        assert_eq!(
            journal
                .get(&ToolInvocationId::from("invocation:prepared-aborted"))
                .expect("journal")
                .expect("aborted")
                .status,
            harness_tool::ToolInvocationStatus::Failed
        );
        assert_eq!(
            journal
                .get(&ToolInvocationId::from("invocation:prepared-uncertain"))
                .expect("journal")
                .expect("uncertain")
                .status,
            harness_tool::ToolInvocationStatus::Uncertain
        );
        let undone = store.undo_latest(11).expect("undo latest applied");
        assert_eq!(undone.id, "invocation:prepared-applied");
        assert_eq!(
            fs::read_to_string(applied_path).expect("restored"),
            "before"
        );
    }
}
