//! 项目状态维护：跨进程锁、一致性 SQLite 备份、校验、恢复与中断回滚。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const BACKUP_SCHEMA_VERSION: u32 = 1;
const MANIFEST_NAME: &str = "manifest.json";
const MANIFEST_HASH_NAME: &str = "manifest.sha256";
const RESTORE_STAGE_NAME: &str = ".restore-stage";
const RESTORE_ROLLBACK_NAME: &str = ".restore-rollback";
const RESTORE_JOURNAL_NAME: &str = ".restore-transaction.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceError {
    pub code: String,
    pub message: String,
}

impl MaintenanceError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for MaintenanceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for MaintenanceError {}

/// 同一项目只允许一个状态 Owner。锁文件保留在磁盘，真正所有权由 OS file lock 决定。
#[derive(Debug)]
pub struct ProjectStateLock {
    state_directory: PathBuf,
    file: File,
}

impl ProjectStateLock {
    pub fn acquire(state_directory: impl AsRef<Path>) -> Result<Self, MaintenanceError> {
        let state_directory = absolute_clean(state_directory.as_ref())?;
        fs::create_dir_all(&state_directory).map_err(io_error)?;
        let state_metadata = fs::symlink_metadata(&state_directory).map_err(io_error)?;
        if state_metadata.file_type().is_symlink() || !state_metadata.is_dir() {
            return Err(MaintenanceError::new(
                "project-state-directory-unsafe",
                state_directory.display().to_string(),
            ));
        }
        let state_directory = fs::canonicalize(state_directory).map_err(io_error)?;
        let lock_path = state_directory.join("runtime.lock");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(io_error)?;
        file.try_lock().map_err(|error| {
            MaintenanceError::new(
                "project-state-locked",
                format!("{}: {error}", state_directory.display()),
            )
        })?;
        file.set_len(0).map_err(io_error)?;
        writeln!(file, "pid={}", std::process::id()).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        recover_interrupted_restore(&state_directory)?;
        Ok(Self {
            state_directory,
            file,
        })
    }

    #[must_use]
    pub fn state_directory(&self) -> &Path {
        &self.state_directory
    }
}

impl Drop for ProjectStateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackupEntryKind {
    Sqlite,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupEntry {
    pub relative_path: String,
    pub kind: BackupEntryKind,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupManifest {
    pub schema_version: u32,
    pub product_version: String,
    pub created_at_millis: i64,
    pub entries: Vec<BackupEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreReport {
    pub restored_entries: usize,
    pub recovery_point: Option<PathBuf>,
}

/// 所有方法都要求调用者持有项目锁，避免跨数据库快照被另一个 Harness 并发修改。
pub struct ProjectMaintenance<'a> {
    lock: &'a ProjectStateLock,
}

impl<'a> ProjectMaintenance<'a> {
    #[must_use]
    pub fn new(lock: &'a ProjectStateLock) -> Self {
        Self { lock }
    }

    pub fn create_backup(
        &self,
        output_directory: impl AsRef<Path>,
        product_version: &str,
        now_millis: i64,
    ) -> Result<BackupManifest, MaintenanceError> {
        let product_version = Version::parse(product_version)
            .map_err(|error| MaintenanceError::new("backup-version-invalid", error.to_string()))?;
        let output_directory = absolute_clean(output_directory.as_ref())?;
        if output_directory.exists() {
            return Err(MaintenanceError::new(
                "backup-output-exists",
                output_directory.display().to_string(),
            ));
        }
        let parent = output_directory.parent().ok_or_else(|| {
            MaintenanceError::new(
                "backup-output-parent-missing",
                output_directory.display().to_string(),
            )
        })?;
        fs::create_dir_all(parent).map_err(io_error)?;
        let staging = parent.join(format!(
            ".harness-backup-stage-{}-{now_millis}",
            std::process::id()
        ));
        if staging.exists() {
            return Err(MaintenanceError::new(
                "backup-staging-exists",
                staging.display().to_string(),
            ));
        }
        fs::create_dir(&staging).map_err(io_error)?;
        let result = self.create_backup_into(&staging, &product_version, now_millis);
        match result {
            Ok(manifest) => {
                fs::rename(&staging, &output_directory).map_err(io_error)?;
                Ok(manifest)
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                Err(error)
            }
        }
    }

    pub fn verify_backup(
        &self,
        backup_directory: impl AsRef<Path>,
    ) -> Result<BackupManifest, MaintenanceError> {
        verify_project_backup(backup_directory.as_ref())
    }

    pub fn restore_backup(
        &self,
        backup_directory: impl AsRef<Path>,
        current_product_version: &str,
        now_millis: i64,
    ) -> Result<RestoreReport, MaintenanceError> {
        let backup_directory = absolute_clean(backup_directory.as_ref())?;
        let manifest = verify_project_backup(&backup_directory)?;
        let backup_version = Version::parse(&manifest.product_version)
            .map_err(|error| MaintenanceError::new("backup-version-invalid", error.to_string()))?;
        let current_version = Version::parse(current_product_version)
            .map_err(|error| MaintenanceError::new("restore-version-invalid", error.to_string()))?;
        if backup_version > current_version {
            return Err(MaintenanceError::new(
                "restore-backup-from-newer-version",
                format!("backup={backup_version}, current={current_version}"),
            ));
        }

        let recovery_point = if durable_sqlite_paths(self.lock.state_directory())?.is_empty() {
            None
        } else {
            let directory = unique_recovery_point(self.lock.state_directory(), now_millis);
            self.create_backup(&directory, current_product_version, now_millis)?;
            Some(directory)
        };

        let state = self.lock.state_directory();
        let stage = state.join(RESTORE_STAGE_NAME);
        let rollback = state.join(RESTORE_ROLLBACK_NAME);
        remove_internal_directory_if_present(state, &stage)?;
        remove_internal_directory_if_present(state, &rollback)?;
        fs::create_dir(&stage).map_err(io_error)?;
        fs::create_dir(&rollback).map_err(io_error)?;

        // 先完整复制并校验新旧两边，再写 durable journal；此时尚未改动当前数据库。
        for entry in &manifest.entries {
            let source = backup_directory.join(&entry.relative_path);
            let staged = stage.join(&entry.relative_path);
            copy_synced(&source, &staged)?;
            verify_entry(&stage, entry)?;
        }
        let original_files = current_database_and_sidecar_names(state)?;
        for name in &original_files {
            copy_synced(&state.join(name), &rollback.join(name))?;
        }
        let transaction = RestoreTransaction {
            schema_version: 1,
            phase: RestorePhase::Swapping,
            original_files,
            restored_files: manifest
                .entries
                .iter()
                .map(|entry| entry.relative_path.clone())
                .collect(),
        };
        write_restore_transaction(state, &transaction)?;

        let swap_result = (|| {
            for name in transaction
                .original_files
                .iter()
                .chain(transaction.restored_files.iter())
            {
                remove_internal_file_if_present(state, &state.join(name))?;
            }
            for entry in &manifest.entries {
                fs::rename(
                    stage.join(&entry.relative_path),
                    state.join(&entry.relative_path),
                )
                .map_err(io_error)?;
                verify_entry(state, entry)?;
            }
            Ok::<(), MaintenanceError>(())
        })();

        if let Err(error) = swap_result {
            rollback_interrupted_restore(state, &transaction)?;
            return Err(error);
        }
        write_restore_transaction(
            state,
            &RestoreTransaction {
                phase: RestorePhase::Committed,
                ..transaction.clone()
            },
        )?;
        cleanup_restore_transaction(state)?;
        Ok(RestoreReport {
            restored_entries: manifest.entries.len(),
            recovery_point,
        })
    }

    fn create_backup_into(
        &self,
        directory: &Path,
        product_version: &Version,
        now_millis: i64,
    ) -> Result<BackupManifest, MaintenanceError> {
        let sources = durable_sqlite_paths(self.lock.state_directory())?;
        if sources.is_empty() {
            return Err(MaintenanceError::new(
                "backup-no-durable-databases",
                self.lock.state_directory().display().to_string(),
            ));
        }
        let mut entries = Vec::with_capacity(sources.len());
        for source in sources {
            verify_sqlite(&source)?;
            let name = file_name(&source)?;
            let destination = directory.join(&name);
            let connection = Connection::open_with_flags(
                &source,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(sql_error)?;
            connection
                .backup("main", &destination, None)
                .map_err(sql_error)?;
            sync_file(&destination)?;
            verify_sqlite(&destination)?;
            entries.push(BackupEntry {
                relative_path: name,
                kind: BackupEntryKind::Sqlite,
                bytes: fs::metadata(&destination).map_err(io_error)?.len(),
                sha256: sha256_file(&destination)?,
            });
        }
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let manifest = BackupManifest {
            schema_version: BACKUP_SCHEMA_VERSION,
            product_version: product_version.to_string(),
            created_at_millis: now_millis,
            entries,
        };
        write_manifest(directory, &manifest)?;
        Ok(manifest)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RestorePhase {
    Swapping,
    Committed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreTransaction {
    schema_version: u32,
    phase: RestorePhase,
    original_files: Vec<String>,
    restored_files: Vec<String>,
}

fn durable_sqlite_paths(state: &Path) -> Result<Vec<PathBuf>, MaintenanceError> {
    let mut paths = fs::read_dir(state)
        .map_err(io_error)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            (file_type.is_file()
                && name.ends_with(".sqlite")
                && !matches!(name, "doctor.sqlite" | "file-leases.sqlite"))
            .then(|| entry.path())
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn current_database_and_sidecar_names(state: &Path) -> Result<Vec<String>, MaintenanceError> {
    let mut names = fs::read_dir(state)
        .map_err(io_error)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            let name = entry.file_name().to_str()?.to_owned();
            (file_type.is_file()
                && (name.ends_with(".sqlite")
                    || name.ends_with(".sqlite-wal")
                    || name.ends_with(".sqlite-shm")))
            .then_some(name)
        })
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

pub fn verify_project_backup(
    directory: impl AsRef<Path>,
) -> Result<BackupManifest, MaintenanceError> {
    let directory = absolute_clean(directory.as_ref())?;
    let manifest_path = directory.join(MANIFEST_NAME);
    let hash_path = directory.join(MANIFEST_HASH_NAME);
    let manifest_bytes = read_bounded(&manifest_path, 4 * 1024 * 1024)?;
    let expected_hash_text = String::from_utf8(read_bounded(&hash_path, 256)?)
        .map_err(|error| MaintenanceError::new("backup-manifest-hash-utf8", error.to_string()))?;
    let expected_hash = expected_hash_text
        .split_whitespace()
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| {
            MaintenanceError::new("backup-manifest-hash-invalid", expected_hash_text.clone())
        })?;
    let actual_hash = format!("{:x}", Sha256::digest(&manifest_bytes));
    if !actual_hash.eq_ignore_ascii_case(expected_hash) {
        return Err(MaintenanceError::new(
            "backup-manifest-hash-mismatch",
            manifest_path.display().to_string(),
        ));
    }
    let manifest: BackupManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| MaintenanceError::new("backup-manifest-json", error.to_string()))?;
    if manifest.schema_version != BACKUP_SCHEMA_VERSION {
        return Err(MaintenanceError::new(
            "backup-schema-unsupported",
            manifest.schema_version.to_string(),
        ));
    }
    Version::parse(&manifest.product_version)
        .map_err(|error| MaintenanceError::new("backup-version-invalid", error.to_string()))?;
    if manifest.entries.is_empty() {
        return Err(MaintenanceError::new(
            "backup-entries-empty",
            directory.display().to_string(),
        ));
    }
    let mut previous = None;
    let mut unique = BTreeSet::new();
    for entry in &manifest.entries {
        validate_relative_database_name(&entry.relative_path)?;
        if previous.is_some_and(|value: &str| value >= entry.relative_path.as_str())
            || !unique.insert(entry.relative_path.clone())
        {
            return Err(MaintenanceError::new(
                "backup-entry-order-or-duplicate",
                entry.relative_path.clone(),
            ));
        }
        previous = Some(entry.relative_path.as_str());
        verify_entry(&directory, entry)?;
    }
    Ok(manifest)
}

fn verify_entry(directory: &Path, entry: &BackupEntry) -> Result<(), MaintenanceError> {
    validate_relative_database_name(&entry.relative_path)?;
    let path = directory.join(&entry.relative_path);
    let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
    if !metadata.file_type().is_file() || metadata.len() != entry.bytes {
        return Err(MaintenanceError::new(
            "backup-entry-size-or-type",
            entry.relative_path.clone(),
        ));
    }
    if sha256_file(&path)? != entry.sha256 {
        return Err(MaintenanceError::new(
            "backup-entry-hash-mismatch",
            entry.relative_path.clone(),
        ));
    }
    match entry.kind {
        BackupEntryKind::Sqlite => verify_sqlite(&path),
    }
}

fn verify_sqlite(path: &Path) -> Result<(), MaintenanceError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql_error)?;
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(sql_error)?;
    if result == "ok" {
        Ok(())
    } else {
        Err(MaintenanceError::new(
            "sqlite-quick-check-failed",
            format!("{}: {result}", path.display()),
        ))
    }
}

fn write_manifest(directory: &Path, manifest: &BackupManifest) -> Result<(), MaintenanceError> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| MaintenanceError::new("backup-manifest-json", error.to_string()))?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    write_synced(&directory.join(MANIFEST_NAME), &bytes)?;
    write_synced(
        &directory.join(MANIFEST_HASH_NAME),
        format!("{hash}  {MANIFEST_NAME}\n").as_bytes(),
    )
}

fn write_restore_transaction(
    state: &Path,
    transaction: &RestoreTransaction,
) -> Result<(), MaintenanceError> {
    let bytes = serde_json::to_vec_pretty(transaction)
        .map_err(|error| MaintenanceError::new("restore-journal-json", error.to_string()))?;
    let temporary = state.join(format!("{RESTORE_JOURNAL_NAME}.new"));
    write_synced(&temporary, &bytes)?;
    let target = state.join(RESTORE_JOURNAL_NAME);
    if target.exists() {
        fs::remove_file(&target).map_err(io_error)?;
    }
    fs::rename(temporary, target).map_err(io_error)
}

fn recover_interrupted_restore(state: &Path) -> Result<(), MaintenanceError> {
    let journal_path = state.join(RESTORE_JOURNAL_NAME);
    if !journal_path.exists() {
        remove_internal_directory_if_present(state, &state.join(RESTORE_STAGE_NAME))?;
        remove_internal_directory_if_present(state, &state.join(RESTORE_ROLLBACK_NAME))?;
        return Ok(());
    }
    let transaction: RestoreTransaction =
        serde_json::from_slice(&read_bounded(&journal_path, 1024 * 1024)?)
            .map_err(|error| MaintenanceError::new("restore-journal-json", error.to_string()))?;
    if transaction.schema_version != 1 {
        return Err(MaintenanceError::new(
            "restore-journal-schema-unsupported",
            transaction.schema_version.to_string(),
        ));
    }
    for name in transaction
        .original_files
        .iter()
        .chain(transaction.restored_files.iter())
    {
        validate_state_file_name(name)?;
    }
    match transaction.phase {
        RestorePhase::Swapping => rollback_interrupted_restore(state, &transaction),
        RestorePhase::Committed => cleanup_restore_transaction(state),
    }
}

fn rollback_interrupted_restore(
    state: &Path,
    transaction: &RestoreTransaction,
) -> Result<(), MaintenanceError> {
    let rollback = state.join(RESTORE_ROLLBACK_NAME);
    for name in transaction
        .original_files
        .iter()
        .chain(transaction.restored_files.iter())
    {
        validate_state_file_name(name)?;
        remove_internal_file_if_present(state, &state.join(name))?;
    }
    for name in &transaction.original_files {
        let source = rollback.join(name);
        if !source.is_file() {
            return Err(MaintenanceError::new(
                "restore-rollback-file-missing",
                source.display().to_string(),
            ));
        }
        copy_synced(&source, &state.join(name))?;
    }
    cleanup_restore_transaction(state)
}

fn cleanup_restore_transaction(state: &Path) -> Result<(), MaintenanceError> {
    remove_internal_directory_if_present(state, &state.join(RESTORE_STAGE_NAME))?;
    remove_internal_directory_if_present(state, &state.join(RESTORE_ROLLBACK_NAME))?;
    remove_internal_file_if_present(state, &state.join(RESTORE_JOURNAL_NAME))?;
    remove_internal_file_if_present(state, &state.join(format!("{RESTORE_JOURNAL_NAME}.new")))
}

fn unique_recovery_point(state: &Path, now_millis: i64) -> PathBuf {
    let root = state.join("backups");
    let base = root.join(format!("pre-restore-{now_millis}"));
    if !base.exists() {
        base
    } else {
        root.join(format!("pre-restore-{now_millis}-{}", std::process::id()))
    }
}

fn validate_relative_database_name(value: &str) -> Result<(), MaintenanceError> {
    validate_state_file_name(value)?;
    if !value.ends_with(".sqlite") {
        return Err(MaintenanceError::new(
            "backup-entry-not-sqlite",
            value.to_owned(),
        ));
    }
    Ok(())
}

fn validate_state_file_name(value: &str) -> Result<(), MaintenanceError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(MaintenanceError::new(
            "maintenance-path-unsafe",
            value.to_owned(),
        ));
    }
    Ok(())
}

fn remove_internal_directory_if_present(state: &Path, path: &Path) -> Result<(), MaintenanceError> {
    ensure_direct_child(state, path)?;
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(io_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(MaintenanceError::new(
                "maintenance-target-not-directory",
                path.display().to_string(),
            ));
        }
        fs::remove_dir_all(path).map_err(io_error)?;
    }
    Ok(())
}

fn remove_internal_file_if_present(state: &Path, path: &Path) -> Result<(), MaintenanceError> {
    ensure_direct_child(state, path)?;
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(io_error)?;
        if !metadata.file_type().is_file() {
            return Err(MaintenanceError::new(
                "maintenance-target-not-file",
                path.display().to_string(),
            ));
        }
        fs::remove_file(path).map_err(io_error)?;
    }
    Ok(())
}

fn ensure_direct_child(state: &Path, path: &Path) -> Result<(), MaintenanceError> {
    if path.parent() != Some(state) || path.file_name().is_none() {
        return Err(MaintenanceError::new(
            "maintenance-internal-path-unsafe",
            path.display().to_string(),
        ));
    }
    Ok(())
}

fn absolute_clean(path: &Path) -> Result<PathBuf, MaintenanceError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(MaintenanceError::new(
            "maintenance-path-not-absolute-clean",
            path.display().to_string(),
        ));
    }
    Ok(path.to_path_buf())
}

fn file_name(path: &Path) -> Result<String, MaintenanceError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            MaintenanceError::new("maintenance-file-name-invalid", path.display().to_string())
        })
}

fn copy_synced(source: &Path, destination: &Path) -> Result<(), MaintenanceError> {
    let metadata = fs::symlink_metadata(source).map_err(io_error)?;
    if !metadata.file_type().is_file() {
        return Err(MaintenanceError::new(
            "maintenance-source-not-file",
            source.display().to_string(),
        ));
    }
    fs::copy(source, destination).map_err(io_error)?;
    sync_file(destination)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), MaintenanceError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn sync_file(path: &Path) -> Result<(), MaintenanceError> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io_error)?
        .sync_all()
        .map_err(io_error)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, MaintenanceError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.file_type().is_file() || metadata.len() > limit as u64 {
        return Err(MaintenanceError::new(
            "maintenance-file-size-or-type",
            path.display().to_string(),
        ));
    }
    let mut reader = BufReader::new(File::open(path).map_err(io_error)?).take(limit as u64 + 1);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    reader.read_to_end(&mut bytes).map_err(io_error)?;
    if bytes.len() > limit {
        return Err(MaintenanceError::new(
            "maintenance-file-too-large",
            path.display().to_string(),
        ));
    }
    Ok(bytes)
}

fn sha256_file(path: &Path) -> Result<String, MaintenanceError> {
    let mut reader = BufReader::new(File::open(path).map_err(io_error)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn io_error(error: std::io::Error) -> MaintenanceError {
    MaintenanceError::new("maintenance-io", error.to_string())
}

fn sql_error(error: rusqlite::Error) -> MaintenanceError {
    MaintenanceError::new("maintenance-sqlite", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sqlite(path: &Path, value: &str) {
        let connection = Connection::open(path).expect("sqlite");
        connection
            .execute_batch("CREATE TABLE value(data TEXT NOT NULL);")
            .expect("schema");
        connection
            .execute("INSERT INTO value VALUES(?1)", [value])
            .expect("insert");
    }

    fn read_value(path: &Path) -> String {
        Connection::open(path)
            .expect("open")
            .query_row("SELECT data FROM value", [], |row| row.get(0))
            .expect("value")
    }

    #[test]
    fn project_lock_is_exclusive_and_recovers_interrupted_restore() {
        let temporary = tempdir().expect("tempdir");
        let state = temporary.path().join(".harness");
        let lock = ProjectStateLock::acquire(&state).expect("first lock");
        assert_eq!(
            ProjectStateLock::acquire(&state)
                .expect_err("second lock must fail")
                .code,
            "project-state-locked"
        );
        drop(lock);

        sqlite(&state.join("kernel.sqlite"), "original");
        let rollback = state.join(RESTORE_ROLLBACK_NAME);
        fs::create_dir(&rollback).expect("rollback");
        fs::copy(state.join("kernel.sqlite"), rollback.join("kernel.sqlite"))
            .expect("rollback copy");
        fs::remove_file(state.join("kernel.sqlite")).expect("remove original");
        sqlite(&state.join("kernel.sqlite"), "replacement");
        write_restore_transaction(
            &state,
            &RestoreTransaction {
                schema_version: 1,
                phase: RestorePhase::Swapping,
                original_files: vec!["kernel.sqlite".to_owned()],
                restored_files: vec!["kernel.sqlite".to_owned()],
            },
        )
        .expect("journal");
        let _recovered = ProjectStateLock::acquire(&state).expect("recovered lock");
        assert_eq!(read_value(&state.join("kernel.sqlite")), "original");
        assert!(!state.join(RESTORE_JOURNAL_NAME).exists());
    }

    #[test]
    fn backup_verify_restore_is_hashed_versioned_and_creates_recovery_point() {
        let temporary = tempdir().expect("tempdir");
        let state = temporary.path().join("project/.harness");
        let backup = temporary.path().join("exports/backup-one");
        let lock = ProjectStateLock::acquire(&state).expect("lock");
        sqlite(&state.join("kernel.sqlite"), "before");
        sqlite(&state.join("memory.sqlite"), "memory");
        sqlite(&state.join("doctor.sqlite"), "excluded");
        sqlite(&state.join("file-leases.sqlite"), "excluded");
        let maintenance = ProjectMaintenance::new(&lock);
        let manifest = maintenance
            .create_backup(&backup, "0.1.0", 100)
            .expect("backup");
        assert_eq!(
            manifest
                .entries
                .iter()
                .map(|entry| entry.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec!["kernel.sqlite", "memory.sqlite"]
        );
        assert_eq!(
            maintenance.verify_backup(&backup).expect("verify"),
            manifest
        );

        fs::remove_file(state.join("kernel.sqlite")).expect("replace db");
        sqlite(&state.join("kernel.sqlite"), "after");
        let restored = maintenance
            .restore_backup(&backup, "0.1.0", 200)
            .expect("restore");
        assert_eq!(restored.restored_entries, 2);
        assert!(restored.recovery_point.expect("recovery point").is_dir());
        assert_eq!(read_value(&state.join("kernel.sqlite")), "before");
        assert_eq!(read_value(&state.join("memory.sqlite")), "memory");
        assert!(!state.join("doctor.sqlite").exists());
        assert!(!state.join("file-leases.sqlite").exists());
    }

    #[test]
    fn verify_rejects_corruption_and_restore_rejects_newer_backup() {
        let temporary = tempdir().expect("tempdir");
        let state = temporary.path().join("project/.harness");
        let backup = temporary.path().join("backup");
        let lock = ProjectStateLock::acquire(&state).expect("lock");
        sqlite(&state.join("kernel.sqlite"), "value");
        let maintenance = ProjectMaintenance::new(&lock);
        maintenance
            .create_backup(&backup, "9.0.0", 100)
            .expect("backup");
        assert_eq!(
            maintenance
                .restore_backup(&backup, "0.1.0", 200)
                .expect_err("newer backup")
                .code,
            "restore-backup-from-newer-version"
        );
        let mut bytes = fs::read(backup.join("kernel.sqlite")).expect("read");
        bytes[0] ^= 0xff;
        fs::write(backup.join("kernel.sqlite"), bytes).expect("corrupt");
        assert_eq!(
            maintenance
                .verify_backup(&backup)
                .expect_err("hash mismatch")
                .code,
            "backup-entry-hash-mismatch"
        );
    }

    #[test]
    fn verify_rejects_manifest_path_traversal_even_with_matching_manifest_hash() {
        let temporary = tempdir().expect("tempdir");
        let state = temporary.path().join("project/.harness");
        let backup = temporary.path().join("backup");
        let lock = ProjectStateLock::acquire(&state).expect("lock");
        sqlite(&state.join("kernel.sqlite"), "value");
        let maintenance = ProjectMaintenance::new(&lock);
        let mut manifest = maintenance
            .create_backup(&backup, "0.1.0", 100)
            .expect("backup");
        fs::remove_file(backup.join(MANIFEST_NAME)).expect("remove manifest");
        fs::remove_file(backup.join(MANIFEST_HASH_NAME)).expect("remove hash");
        manifest.entries[0].relative_path = "../outside.sqlite".to_owned();
        write_manifest(&backup, &manifest).expect("rewrite manifest and hash");
        assert_eq!(
            maintenance
                .verify_backup(&backup)
                .expect_err("path traversal")
                .code,
            "maintenance-path-unsafe"
        );
    }
}
