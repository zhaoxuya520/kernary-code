#![forbid(unsafe_code)]

//! LSP WorkspaceEdit 的持久 Preview、FileLease 与 PatchStore 协调层。

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use harness_agent::{FileLease, FileLeaseManager};
use harness_builtin_tools::{PatchStatus, PatchStore, WorkspacePathGuard};
use harness_lsp::{HumanPosition, HumanRange, LspComputedWorkspaceEdit, LspManager};
use harness_permission::PermissionAction;
use harness_tool::{
    ToolDescriptor, ToolEffectClass, ToolError, ToolExecutionInput, ToolPromptLoading,
    ToolProvider, ToolRegistry, ToolSource,
};
use harness_types::{RunId, ToolInvocationId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const PREVIEW_SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LspPatchStatus {
    Ready,
    Applying,
    Applied,
    Undoing,
    Undone,
    RolledBack,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspPatchFile {
    pub path: String,
    pub before_hash: String,
    pub after_hash: String,
    pub after_blob: String,
    pub edit_count: usize,
    pub added_bytes: usize,
    pub removed_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspPatchPreview {
    pub schema_version: u32,
    pub id: String,
    pub source_server: String,
    pub source_method: String,
    pub title: String,
    pub source_document_hash: String,
    pub fingerprint: String,
    pub files: Vec<LspPatchFile>,
    pub total_edits: usize,
    pub status: LspPatchStatus,
    pub patch_ids: Vec<String>,
    pub created_at_millis: i64,
    pub updated_at_millis: i64,
}

pub struct LspPatchStore {
    root: PathBuf,
    guard: WorkspacePathGuard,
    sequence: AtomicU64,
}

impl LspPatchStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, guard: WorkspacePathGuard) -> Self {
        Self {
            root: root.into(),
            guard,
            sequence: AtomicU64::new(1),
        }
    }

    pub fn save_computed(
        &self,
        computed: LspComputedWorkspaceEdit,
        now_millis: i64,
    ) -> Result<LspPatchPreview, ToolError> {
        self.ensure_directories()?;
        if computed.files.is_empty()
            || computed.files.len() > 64
            || computed.total_edits > 2048
            || !is_sha256(&computed.fingerprint)
            || !is_sha256(&computed.source_document.file_hash)
            || computed.title.len() > 512
            || computed.source_method.len() > 64
            || computed.server_id.len() > 96
        {
            return Err(ToolError::new(
                "lsp-preview-size-invalid",
                computed.files.len().to_string(),
            ));
        }
        let id = format!("lsp-preview:{}", &computed.fingerprint[..32]);
        if self.record_path(&id).exists() {
            let existing = self.load(&id)?;
            if existing.fingerprint == computed.fingerprint {
                return Ok(existing);
            }
            return Err(ToolError::new("lsp-preview-id-conflict", id));
        }
        let mut files = Vec::with_capacity(computed.files.len());
        for file in computed.files {
            let path = self.guard.resolve_read(Path::new(&file.path))?;
            if !path.is_file()
                || file.before_hash.len() != 64
                || file.after_hash.len() != 64
                || sha256(file.after_text.as_bytes()) != file.after_hash
            {
                return Err(ToolError::new("lsp-preview-file-invalid", file.path));
            }
            let relative = path
                .strip_prefix(self.guard.root())
                .map_err(|_| {
                    ToolError::new("lsp-preview-relative-path", path.display().to_string())
                })?
                .to_string_lossy()
                .replace('\\', "/");
            self.write_blob(&file.after_hash, file.after_text.as_bytes())?;
            files.push(LspPatchFile {
                path: relative,
                before_hash: file.before_hash,
                after_hash: file.after_hash.clone(),
                after_blob: file.after_hash,
                edit_count: file.edit_count,
                added_bytes: file.added_bytes,
                removed_bytes: file.removed_bytes,
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let preview = LspPatchPreview {
            schema_version: PREVIEW_SCHEMA_VERSION,
            id,
            source_server: computed.server_id,
            source_method: computed.source_method,
            title: computed.title,
            source_document_hash: computed.source_document.file_hash,
            fingerprint: computed.fingerprint,
            files,
            total_edits: computed.total_edits,
            status: LspPatchStatus::Ready,
            patch_ids: vec![],
            created_at_millis: now_millis,
            updated_at_millis: now_millis,
        };
        self.write_record(&preview)?;
        Ok(preview)
    }

    pub fn load(&self, id: &str) -> Result<LspPatchPreview, ToolError> {
        let path = self.record_path(id);
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| io_error("lsp-preview-read", error))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_RECORD_BYTES {
            return Err(ToolError::new(
                "lsp-preview-size-or-type",
                path.display().to_string(),
            ));
        }
        let preview: LspPatchPreview = serde_json::from_slice(
            &fs::read(&path).map_err(|error| io_error("lsp-preview-read", error))?,
        )
        .map_err(|error| ToolError::new("lsp-preview-json", error.to_string()))?;
        self.validate(&preview)?;
        if self.record_path(&preview.id) != path {
            return Err(ToolError::new("lsp-preview-id-mismatch", preview.id));
        }
        Ok(preview)
    }

    pub fn list(&self) -> Result<Vec<LspPatchPreview>, ToolError> {
        let records = self.root.join("records");
        if !records.exists() {
            return Ok(vec![]);
        }
        let mut previews = Vec::new();
        for entry in fs::read_dir(records).map_err(|error| io_error("lsp-preview-list", error))? {
            let entry = entry.map_err(|error| io_error("lsp-preview-list", error))?;
            if entry
                .file_type()
                .map_err(|error| io_error("lsp-preview-list", error))?
                .is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            {
                let preview: LspPatchPreview = serde_json::from_slice(
                    &fs::read(entry.path()).map_err(|error| io_error("lsp-preview-read", error))?,
                )
                .map_err(|error| ToolError::new("lsp-preview-json", error.to_string()))?;
                self.validate(&preview)?;
                previews.push(preview);
            }
        }
        previews.sort_by(|left, right| {
            left.created_at_millis
                .cmp(&right.created_at_millis)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(previews)
    }

    pub fn transition(
        &self,
        id: &str,
        expected: &[LspPatchStatus],
        status: LspPatchStatus,
        patch_ids: Option<Vec<String>>,
        now_millis: i64,
    ) -> Result<LspPatchPreview, ToolError> {
        let mut preview = self.load(id)?;
        if !expected.contains(&preview.status) {
            return Err(ToolError::new(
                "lsp-preview-status-conflict",
                format!("expected={expected:?}, actual={:?}", preview.status),
            ));
        }
        preview.status = status;
        if let Some(patch_ids) = patch_ids {
            preview.patch_ids = patch_ids;
        }
        preview.updated_at_millis = now_millis;
        self.write_record(&preview)?;
        Ok(preview)
    }

    pub fn after_bytes(&self, file: &LspPatchFile) -> Result<Vec<u8>, ToolError> {
        let bytes = fs::read(self.root.join("blobs").join(&file.after_blob))
            .map_err(|error| io_error("lsp-preview-blob-read", error))?;
        if sha256(&bytes) != file.after_hash {
            return Err(ToolError::new(
                "lsp-preview-blob-corrupt",
                file.after_blob.clone(),
            ));
        }
        Ok(bytes)
    }

    #[must_use]
    pub fn absolute_paths(&self, preview: &LspPatchPreview) -> Vec<PathBuf> {
        preview
            .files
            .iter()
            .map(|file| self.guard.root().join(&file.path))
            .collect()
    }

    fn validate(&self, preview: &LspPatchPreview) -> Result<(), ToolError> {
        if preview.schema_version != PREVIEW_SCHEMA_VERSION
            || preview.id.is_empty()
            || preview.files.is_empty()
            || preview.files.len() > 64
            || !is_sha256(&preview.fingerprint)
            || !is_sha256(&preview.source_document_hash)
            || preview.id != format!("lsp-preview:{}", &preview.fingerprint[..32])
            || preview.patch_ids.len() > preview.files.len()
        {
            return Err(ToolError::new("lsp-preview-invalid", preview.id.clone()));
        }
        if preview.patch_ids.iter().collect::<BTreeSet<_>>().len() != preview.patch_ids.len() {
            return Err(ToolError::new(
                "lsp-preview-patch-id-conflict",
                preview.id.clone(),
            ));
        }
        let mut paths = BTreeSet::new();
        for file in &preview.files {
            if !paths.insert(file.path.clone())
                || file.before_hash.len() != 64
                || !is_sha256(&file.before_hash)
                || !is_sha256(&file.after_hash)
                || file.after_blob != file.after_hash
            {
                return Err(ToolError::new(
                    "lsp-preview-file-invalid",
                    file.path.clone(),
                ));
            }
            self.guard.resolve_write(Path::new(&file.path))?;
        }
        Ok(())
    }

    fn ensure_directories(&self) -> Result<(), ToolError> {
        fs::create_dir_all(self.root.join("records"))
            .and_then(|()| fs::create_dir_all(self.root.join("blobs")))
            .map_err(|error| io_error("lsp-preview-create", error))
    }

    fn write_blob(&self, hash: &str, bytes: &[u8]) -> Result<(), ToolError> {
        let path = self.root.join("blobs").join(hash);
        if path.exists() {
            let existing =
                fs::read(&path).map_err(|error| io_error("lsp-preview-blob-read", error))?;
            if sha256(&existing) != hash {
                return Err(ToolError::new("lsp-preview-blob-corrupt", hash));
            }
            return Ok(());
        }
        atomic_write(&path, bytes, self.sequence.fetch_add(1, Ordering::SeqCst))
    }

    fn write_record(&self, preview: &LspPatchPreview) -> Result<(), ToolError> {
        self.ensure_directories()?;
        self.validate(preview)?;
        let mut bytes = serde_json::to_vec_pretty(preview)
            .map_err(|error| ToolError::new("lsp-preview-json", error.to_string()))?;
        bytes.push(b'\n');
        atomic_write(
            &self.record_path(&preview.id),
            &bytes,
            self.sequence.fetch_add(1, Ordering::SeqCst),
        )
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.root
            .join("records")
            .join(format!("{}.json", sha256(id.as_bytes())))
    }
}

pub struct LspPatchCoordinator {
    project_root: PathBuf,
    previews: Arc<LspPatchStore>,
    patches: Arc<PatchStore>,
    leases: Mutex<FileLeaseManager>,
}

impl LspPatchCoordinator {
    pub fn new(
        project_root: impl AsRef<Path>,
        lease_database: impl AsRef<Path>,
        previews: Arc<LspPatchStore>,
        patches: Arc<PatchStore>,
    ) -> Result<Self, ToolError> {
        let project_root =
            fs::canonicalize(project_root).map_err(|error| io_error("lsp-patch-root", error))?;
        let leases =
            FileLeaseManager::open(&project_root, lease_database).map_err(agent_tool_error)?;
        Ok(Self {
            project_root,
            previews,
            patches,
            leases: Mutex::new(leases),
        })
    }

    pub fn apply(
        &self,
        preview_id: &str,
        invocation_id: &ToolInvocationId,
        owner_run: &RunId,
        now_millis: i64,
    ) -> Result<LspPatchPreview, ToolError> {
        let preview = self.previews.load(preview_id)?;
        if preview.status == LspPatchStatus::Applied {
            return Ok(preview);
        }
        if !matches!(
            preview.status,
            LspPatchStatus::Ready | LspPatchStatus::RolledBack | LspPatchStatus::Undone
        ) {
            return Err(ToolError::new(
                "lsp-preview-not-applicable",
                format!("{preview_id}:{:?}", preview.status),
            ));
        }
        let leases = self.acquire_leases(&preview, owner_run, now_millis)?;
        let result = self.apply_with_leases(preview, invocation_id, now_millis);
        let release = self.release_leases(&leases);
        result.and_then(|preview| release.map(|()| preview))
    }

    fn apply_with_leases(
        &self,
        preview: LspPatchPreview,
        invocation_id: &ToolInvocationId,
        now_millis: i64,
    ) -> Result<LspPatchPreview, ToolError> {
        let mut after_bytes = Vec::with_capacity(preview.files.len());
        for file in &preview.files {
            let path = self.project_root.join(&file.path);
            let current = fs::read(&path).map_err(|error| io_error("lsp-patch-read", error))?;
            if sha256(&current) != file.before_hash {
                return Err(ToolError::new("lsp-patch-stale-before", file.path.clone()));
            }
            after_bytes.push(self.previews.after_bytes(file)?);
        }
        let patch_ids = preview
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                format!(
                    "{}:lsp-file:{index}:{}",
                    invocation_id,
                    &file.after_hash[..12]
                )
            })
            .collect::<Vec<_>>();
        self.previews.transition(
            &preview.id,
            &[preview.status],
            LspPatchStatus::Applying,
            Some(patch_ids.clone()),
            now_millis,
        )?;
        let mut prepared = Vec::new();
        for ((file, after), patch_id) in preview.files.iter().zip(&after_bytes).zip(&patch_ids) {
            let path = self.project_root.join(&file.path);
            let before = fs::read(&path).map_err(|error| io_error("lsp-patch-read", error))?;
            match self.patches.prepare_with_id(
                patch_id.clone(),
                &path,
                Some(&before),
                after,
                now_millis,
            ) {
                Ok(record) => prepared.push(record.id),
                Err(error) => {
                    let rollback_ok = self.abort_or_undo(&prepared, now_millis);
                    let status = if rollback_ok {
                        LspPatchStatus::RolledBack
                    } else {
                        LspPatchStatus::Uncertain
                    };
                    let _ = self.previews.transition(
                        &preview.id,
                        &[LspPatchStatus::Applying],
                        status,
                        None,
                        now_millis,
                    );
                    return Err(error);
                }
            }
        }
        let mut applied = Vec::new();
        for (patch_id, after) in patch_ids.iter().zip(&after_bytes) {
            match self.patches.apply_prepared(patch_id, after, now_millis) {
                Ok(_) => applied.push(patch_id.clone()),
                Err(error) => {
                    let mut touched = prepared.clone();
                    touched.sort_by_key(|id| !applied.contains(id));
                    let rollback_ok = self.abort_or_undo(&touched, now_millis);
                    let status = if rollback_ok {
                        LspPatchStatus::RolledBack
                    } else {
                        LspPatchStatus::Uncertain
                    };
                    let _ = self.previews.transition(
                        &preview.id,
                        &[LspPatchStatus::Applying],
                        status,
                        None,
                        now_millis,
                    );
                    return Err(error);
                }
            }
        }
        self.previews.transition(
            &preview.id,
            &[LspPatchStatus::Applying],
            LspPatchStatus::Applied,
            None,
            now_millis,
        )
    }

    pub fn undo(
        &self,
        preview_id: &str,
        owner_run: &RunId,
        now_millis: i64,
    ) -> Result<LspPatchPreview, ToolError> {
        let preview = self.previews.load(preview_id)?;
        if preview.status == LspPatchStatus::Undone {
            return Ok(preview);
        }
        if preview.status != LspPatchStatus::Applied {
            return Err(ToolError::new(
                "lsp-preview-not-undoable",
                format!("{preview_id}:{:?}", preview.status),
            ));
        }
        let leases = self.acquire_leases(&preview, owner_run, now_millis)?;
        let result = (|| {
            for patch_id in &preview.patch_ids {
                self.patches.verify_undoable(patch_id)?;
            }
            self.previews.transition(
                preview_id,
                &[LspPatchStatus::Applied],
                LspPatchStatus::Undoing,
                None,
                now_millis,
            )?;
            for patch_id in preview.patch_ids.iter().rev() {
                if let Err(error) = self.patches.undo(patch_id, now_millis) {
                    let _ = self.previews.transition(
                        preview_id,
                        &[LspPatchStatus::Undoing],
                        LspPatchStatus::Uncertain,
                        None,
                        now_millis,
                    );
                    return Err(error);
                }
            }
            self.previews.transition(
                preview_id,
                &[LspPatchStatus::Undoing],
                LspPatchStatus::Undone,
                None,
                now_millis,
            )
        })();
        let release = self.release_leases(&leases);
        result.and_then(|preview| release.map(|()| preview))
    }

    pub fn reconcile(&self, now_millis: i64) -> Result<Vec<LspPatchPreview>, ToolError> {
        self.patches.reconcile_prepared(now_millis)?;
        let mut reconciled = Vec::new();
        for preview in self.previews.list()? {
            if !matches!(
                preview.status,
                LspPatchStatus::Applying | LspPatchStatus::Undoing | LspPatchStatus::Uncertain
            ) {
                continue;
            }
            let mut uncertain = false;
            for patch_id in preview.patch_ids.iter().rev() {
                let record = match self.patches.load(patch_id) {
                    Ok(record) => record,
                    Err(_) => continue,
                };
                match record.status {
                    PatchStatus::Applied => {
                        if self.patches.undo(patch_id, now_millis).is_err() {
                            uncertain = true;
                        }
                    }
                    PatchStatus::Uncertain => uncertain = true,
                    PatchStatus::Prepared => {
                        if self.patches.abort_prepared(patch_id, now_millis).is_err() {
                            uncertain = true;
                        }
                    }
                    PatchStatus::Aborted | PatchStatus::Undone => {}
                }
            }
            let target = if uncertain {
                LspPatchStatus::Uncertain
            } else if preview.status == LspPatchStatus::Undoing {
                LspPatchStatus::Undone
            } else {
                LspPatchStatus::RolledBack
            };
            reconciled.push(self.previews.transition(
                &preview.id,
                &[preview.status],
                target,
                None,
                now_millis,
            )?);
        }
        Ok(reconciled)
    }

    fn abort_or_undo(&self, patch_ids: &[String], now_millis: i64) -> bool {
        let mut ok = true;
        for patch_id in patch_ids.iter().rev() {
            let result = match self.patches.load(patch_id).map(|record| record.status) {
                Ok(PatchStatus::Applied) => self.patches.undo(patch_id, now_millis).map(|_| ()),
                Ok(PatchStatus::Prepared) => self
                    .patches
                    .abort_prepared(patch_id, now_millis)
                    .map(|_| ()),
                Ok(PatchStatus::Aborted | PatchStatus::Undone) => Ok(()),
                Ok(PatchStatus::Uncertain) | Err(_) => {
                    Err(ToolError::new("lsp-patch-rollback-uncertain", patch_id))
                }
            };
            ok &= result.is_ok();
        }
        ok
    }

    fn acquire_leases(
        &self,
        preview: &LspPatchPreview,
        owner_run: &RunId,
        now_millis: i64,
    ) -> Result<Vec<FileLease>, ToolError> {
        let mut paths = self.previews.absolute_paths(preview);
        paths.sort();
        paths.dedup();
        let mut manager = self
            .leases
            .lock()
            .map_err(|_| ToolError::new("lsp-patch-leases-poisoned", "lock"))?;
        let mut leases = Vec::with_capacity(paths.len());
        for path in paths {
            match manager.acquire(&path, owner_run.clone(), now_millis, 120_000) {
                Ok(lease) => leases.push(lease),
                Err(error) => {
                    for lease in leases.iter().rev() {
                        let _ = manager.release(lease);
                    }
                    return Err(agent_tool_error(error));
                }
            }
        }
        Ok(leases)
    }

    fn release_leases(&self, leases: &[FileLease]) -> Result<(), ToolError> {
        let manager = self
            .leases
            .lock()
            .map_err(|_| ToolError::new("lsp-patch-leases-poisoned", "lock"))?;
        for lease in leases.iter().rev() {
            if !manager.release(lease).map_err(agent_tool_error)? {
                return Err(ToolError::new(
                    "lsp-patch-lease-stale-release",
                    lease.path.display().to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum PreviewToolKind {
    Rename,
    CodeAction,
}

struct PreviewTool {
    manager: LspManager,
    store: Arc<LspPatchStore>,
    kind: PreviewToolKind,
}

impl ToolProvider for PreviewTool {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let server = required_string(value, "serverId")?;
        required_string(value, "path")?;
        self.manager.process_spec(server).map_err(lsp_tool_error)?;
        match self.kind {
            PreviewToolKind::Rename => {
                position(value, "line")?;
                position(value, "character")?;
                required_string(value, "newName")?;
            }
            PreviewToolKind::CodeAction => {
                for field in ["startLine", "startCharacter", "endLine", "endCharacter"] {
                    position(value, field)?;
                }
                value
                    .get("actionIndex")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|value| *value <= 255)
                    .ok_or_else(|| ToolError::new("lsp-preview-action-index", "actionIndex"))?;
            }
        }
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if value
            .get("previewId")
            .and_then(serde_json::Value::as_str)
            .is_none()
            || value
                .get("fingerprint")
                .and_then(serde_json::Value::as_str)
                .is_none()
            || value
                .get("files")
                .and_then(serde_json::Value::as_array)
                .is_none()
        {
            return Err(ToolError::new("lsp-preview-result-invalid", "result"));
        }
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        let spec = self
            .manager
            .process_spec(required_string(args, "serverId")?)
            .map_err(lsp_tool_error)?;
        Ok(PermissionAction::ProcessSpawn {
            executable: spec.executable,
            arguments: spec.arguments,
            cwd: spec.cwd,
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        if input.cancellation.is_cancelled() {
            return Err(ToolError::new("tool-cancelled", "LSP preview"));
        }
        let server = required_string(&input.args, "serverId")?;
        let path = Path::new(required_string(&input.args, "path")?);
        let computed = match self.kind {
            PreviewToolKind::Rename => self.manager.rename_edit(
                server,
                path,
                position(&input.args, "line")?,
                position(&input.args, "character")?,
                required_string(&input.args, "newName")?,
            ),
            PreviewToolKind::CodeAction => self.manager.code_action_edit(
                server,
                path,
                HumanRange {
                    start: HumanPosition {
                        line: position(&input.args, "startLine")?,
                        character: position(&input.args, "startCharacter")?,
                    },
                    end: HumanPosition {
                        line: position(&input.args, "endLine")?,
                        character: position(&input.args, "endCharacter")?,
                    },
                },
                input.args["actionIndex"]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .expect("validated actionIndex"),
                input.args.get("only").and_then(serde_json::Value::as_str),
            ),
        }
        .map_err(lsp_tool_error)?;
        let preview = self.store.save_computed(computed, input.now_millis)?;
        Ok(preview_summary(&preview))
    }
}

#[derive(Clone, Copy)]
enum ApplyToolKind {
    Apply,
    Undo,
}

struct ApplyTool {
    store: Arc<LspPatchStore>,
    coordinator: Arc<LspPatchCoordinator>,
    kind: ApplyToolKind,
}

impl ToolProvider for ApplyTool {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let preview = self.store.load(required_string(value, "previewId")?)?;
        match self.kind {
            ApplyToolKind::Apply
                if !matches!(
                    preview.status,
                    LspPatchStatus::Ready | LspPatchStatus::RolledBack | LspPatchStatus::Undone
                ) =>
            {
                return Err(ToolError::new(
                    "lsp-preview-not-applicable",
                    format!("{:?}", preview.status),
                ));
            }
            ApplyToolKind::Undo if preview.status != LspPatchStatus::Applied => {
                return Err(ToolError::new(
                    "lsp-preview-not-undoable",
                    format!("{:?}", preview.status),
                ));
            }
            _ => {}
        }
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if value
            .get("previewId")
            .and_then(serde_json::Value::as_str)
            .is_none()
            || value
                .get("status")
                .and_then(serde_json::Value::as_str)
                .is_none()
            || value
                .get("patchIds")
                .and_then(serde_json::Value::as_array)
                .is_none()
        {
            return Err(ToolError::new("lsp-patch-result-invalid", "result"));
        }
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        let preview = self.store.load(required_string(args, "previewId")?)?;
        let paths = self.store.absolute_paths(&preview);
        Ok(PermissionAction::WorkspacePatchApply {
            operation: match self.kind {
                ApplyToolKind::Apply => "apply",
                ApplyToolKind::Undo => "undo",
            }
            .to_owned(),
            preview_id: preview.id,
            preview_fingerprint: preview.fingerprint,
            paths,
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        let run_id = input
            .envelope
            .run_id
            .as_ref()
            .ok_or_else(|| ToolError::new("lsp-patch-run-required", "runId"))?;
        let preview_id = required_string(&input.args, "previewId")?;
        let preview = match self.kind {
            ApplyToolKind::Apply => self.coordinator.apply(
                preview_id,
                &input.invocation_id,
                run_id,
                input.now_millis,
            )?,
            ApplyToolKind::Undo => self
                .coordinator
                .undo(preview_id, run_id, input.now_millis)?,
        };
        Ok(serde_json::json!({
            "previewId":preview.id,
            "status":preview.status,
            "patchIds":preview.patch_ids,
            "paths":preview.files.iter().map(|file|file.path.clone()).collect::<Vec<_>>()
        }))
    }
}

pub fn register_lsp_patch_tools(
    registry: &ToolRegistry,
    manager: LspManager,
    store: Arc<LspPatchStore>,
    coordinator: Arc<LspPatchCoordinator>,
) -> Result<(), ToolError> {
    for (name, kind, description, schema) in [
        (
            "lsp.rename.preview",
            PreviewToolKind::Rename,
            "计算 rename WorkspaceEdit 并持久化只读 Patch Preview；不修改文件",
            serde_json::json!({
                "type":"object",
                "properties":{
                    "serverId":{"type":"string"},"path":{"type":"string"},
                    "line":{"type":"integer","minimum":1},"character":{"type":"integer","minimum":1},
                    "newName":{"type":"string","minLength":1,"maxLength":512}
                },
                "required":["serverId","path","line","character","newName"],"additionalProperties":false
            }),
        ),
        (
            "lsp.code-action.preview",
            PreviewToolKind::CodeAction,
            "选择 CodeAction edit 并持久化只读 Patch Preview；拒绝 command/resource/snippet",
            serde_json::json!({
                "type":"object",
                "properties":{
                    "serverId":{"type":"string"},"path":{"type":"string"},
                    "startLine":{"type":"integer","minimum":1},"startCharacter":{"type":"integer","minimum":1},
                    "endLine":{"type":"integer","minimum":1},"endCharacter":{"type":"integer","minimum":1},
                    "actionIndex":{"type":"integer","minimum":0,"maximum":255},"only":{"type":"string"}
                },
                "required":["serverId","path","startLine","startCharacter","endLine","endCharacter","actionIndex"],"additionalProperties":false
            }),
        ),
    ] {
        registry.register(
            ToolDescriptor {
                canonical_name: name.to_owned(),
                version: "1".to_owned(),
                description: description.to_owned(),
                effect_class: ToolEffectClass::ReadOnlyRetryable,
                source: ToolSource::Builtin,
                prompt_loading: ToolPromptLoading::OnDemand,
                keywords: vec![
                    "lsp".to_owned(),
                    "rename".to_owned(),
                    "refactor".to_owned(),
                    "preview".to_owned(),
                    "重命名".to_owned(),
                    "预览".to_owned(),
                ],
                input_schema: schema,
                output_schema: preview_output_schema(),
            },
            Arc::new(PreviewTool {
                manager: manager.clone(),
                store: store.clone(),
                kind,
            }),
        )?;
    }
    for (name, kind, description) in [
        (
            "lsp.patch.apply",
            ApplyToolKind::Apply,
            "二次审批后以 FileLease + PatchStore 原子应用 LSP Preview",
        ),
        (
            "lsp.patch.undo",
            ApplyToolKind::Undo,
            "二次审批后按 PatchSet 逆序撤销全部子 Patch",
        ),
    ] {
        registry.register(
            ToolDescriptor {
                canonical_name: name.to_owned(),
                version: "1".to_owned(),
                description: description.to_owned(),
                effect_class: ToolEffectClass::IdempotentEffect,
                source: ToolSource::Builtin,
                prompt_loading: ToolPromptLoading::OnDemand,
                keywords: vec!["lsp".to_owned(), "patch".to_owned(), "apply".to_owned(), "undo".to_owned(), "应用".to_owned(), "撤销".to_owned()],
                input_schema: serde_json::json!({
                    "type":"object","properties":{"previewId":{"type":"string"}},
                    "required":["previewId"],"additionalProperties":false
                }),
                output_schema: serde_json::json!({
                    "type":"object","properties":{
                        "previewId":{"type":"string"},"status":{"type":"string"},
                        "patchIds":{"type":"array"},"paths":{"type":"array"}
                    },"required":["previewId","status","patchIds","paths"],"additionalProperties":false
                }),
            },
            Arc::new(ApplyTool {
                store: store.clone(),
                coordinator: coordinator.clone(),
                kind,
            }),
        )?;
    }
    Ok(())
}

fn preview_summary(preview: &LspPatchPreview) -> serde_json::Value {
    serde_json::json!({
        "previewId":preview.id,
        "fingerprint":preview.fingerprint,
        "title":preview.title,
        "sourceMethod":preview.source_method,
        "status":preview.status,
        "totalEdits":preview.total_edits,
        "files":preview.files.iter().map(|file|serde_json::json!({
            "path":file.path,"beforeHash":file.before_hash,"afterHash":file.after_hash,
            "editCount":file.edit_count,"addedBytes":file.added_bytes,"removedBytes":file.removed_bytes
        })).collect::<Vec<_>>(),
        "source":"lsp","integrity":"untrusted"
    })
}

fn preview_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type":"object","properties":{
            "previewId":{"type":"string"},"fingerprint":{"type":"string"},
            "title":{"type":"string"},"sourceMethod":{"type":"string"},
            "status":{"type":"string"},"totalEdits":{"type":"integer"},
            "files":{"type":"array"},"source":{"const":"lsp"},"integrity":{"const":"untrusted"}
        },"required":["previewId","fingerprint","title","sourceMethod","status","totalEdits","files","source","integrity"],
        "additionalProperties":false
    })
}

fn required_string<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str, ToolError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::new("lsp-patch-argument-missing", field))
}

fn position(value: &serde_json::Value, field: &str) -> Result<u32, ToolError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| ToolError::new("lsp-patch-position-invalid", field))
}

fn atomic_write(path: &Path, bytes: &[u8], sequence: u64) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::new("lsp-preview-parent-missing", path.display().to_string()))?;
    fs::create_dir_all(parent).map_err(|error| io_error("lsp-preview-create", error))?;
    let temporary = parent.join(format!(
        ".lsp-preview-{}-{sequence}.tmp",
        std::process::id()
    ));
    let backup = parent.join(format!(
        ".lsp-preview-{}-{sequence}.bak",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| io_error("lsp-preview-temp", error))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| io_error("lsp-preview-temp", error))?;
        if path.exists() {
            fs::rename(path, &backup).map_err(|error| io_error("lsp-preview-backup", error))?;
        }
        if let Err(error) = fs::rename(&temporary, path) {
            if backup.exists() {
                let _ = fs::rename(&backup, path);
            }
            return Err(io_error("lsp-preview-commit", error));
        }
        if backup.exists() {
            let _ = fs::remove_file(backup);
        }
        Ok(())
    })();
    if result.is_err() && temporary.exists() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn io_error(code: &'static str, error: std::io::Error) -> ToolError {
    ToolError::new(code, error.to_string())
}

fn lsp_tool_error(error: harness_lsp::LspError) -> ToolError {
    ToolError::new(error.code, error.message)
}

fn agent_tool_error(error: harness_agent::AgentError) -> ToolError {
    ToolError::new(error.code, error.message)
}
