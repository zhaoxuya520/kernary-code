#![forbid(unsafe_code)]

//! Scope-safe multi-level Cache Engine。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use harness_types::{
    ConfidentialityLabel, ContentHash, InformationFlowLabel, ProjectId, SessionId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Cache 数据类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheNamespace {
    ToolResult,
    RepositoryScan,
    FileSummary,
    Embedding,
    MemoryRetrieval,
    PromptSegment,
    ModelCapability,
    McpSchema,
    PluginMetadata,
}

/// Cache key 的隔离域。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheScope {
    pub project_id: Option<ProjectId>,
    pub session_id: Option<SessionId>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Effectful 结果默认禁止缓存。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheEffectClass {
    Pure,
    ReadOnly,
    Idempotent,
    Effectful,
}

/// Canonical cache key。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheKey {
    pub namespace: CacheNamespace,
    pub scope: CacheScope,
    pub input_hash: ContentHash,
    pub schema_version: String,
    pub information_flow: InformationFlowLabel,
}

impl CacheKey {
    pub fn fingerprint(&self) -> Result<ContentHash, CacheError> {
        let json = serde_json::to_string(self)
            .map_err(|error| CacheError::new("cache-key-json", error.to_string()))?;
        Ok(hash_bytes(json.as_bytes()))
    }
}

/// Cache value 和生命周期。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheEntry {
    pub key: CacheKey,
    pub value: serde_json::Value,
    pub effect_class: CacheEffectClass,
    pub created_at_millis: i64,
    pub expires_at_millis: Option<i64>,
}

impl CacheEntry {
    #[must_use]
    pub fn expired_at(&self, now_millis: i64) -> bool {
        self.expires_at_millis
            .is_some_and(|expires| now_millis > expires)
    }
}

/// Cache policy。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachePolicy {
    pub max_entries: usize,
    pub max_bytes: usize,
    pub allowed_effect_classes: BTreeSet<CacheEffectClass>,
}

impl CachePolicy {
    #[must_use]
    pub fn safe_default(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            max_entries,
            max_bytes,
            allowed_effect_classes: [CacheEffectClass::Pure, CacheEffectClass::ReadOnly]
                .into_iter()
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheMetrics {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub evictions: u64,
    pub rejected_writes: u64,
}

impl CacheMetrics {
    #[must_use]
    pub fn hit_rate_percent(&self) -> Option<u8> {
        let total = self.hits.saturating_add(self.misses);
        if total == 0 {
            return None;
        }
        Some(u8::try_from(self.hits.saturating_mul(100) / total).unwrap_or(100))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheError {
    pub code: &'static str,
    pub message: String,
}

impl CacheError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Display for CacheError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for CacheError {}

fn validate_write(entry: &CacheEntry, policy: &CachePolicy) -> Result<(), CacheError> {
    if entry.key.information_flow.confidentiality == ConfidentialityLabel::UserSecret {
        return Err(CacheError::new(
            "secret-not-cacheable",
            "UserSecret 不能进入 Cache",
        ));
    }
    if !policy.allowed_effect_classes.contains(&entry.effect_class) {
        return Err(CacheError::new(
            "effect-not-cacheable",
            format!("{:?} 不在 Cache policy allowlist", entry.effect_class),
        ));
    }
    Ok(())
}

fn entry_size(entry: &CacheEntry) -> Result<usize, CacheError> {
    serde_json::to_vec(entry)
        .map(|value| value.len())
        .map_err(|error| CacheError::new("cache-entry-json", error.to_string()))
}

/// L1 bounded memory cache。
pub struct MemoryCache {
    policy: CachePolicy,
    entries: BTreeMap<ContentHash, CacheEntry>,
    order: VecDeque<ContentHash>,
    bytes: usize,
    metrics: CacheMetrics,
}

impl MemoryCache {
    #[must_use]
    pub fn new(policy: CachePolicy) -> Self {
        Self {
            policy,
            entries: BTreeMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            metrics: CacheMetrics::default(),
        }
    }

    pub fn put(&mut self, entry: CacheEntry) -> Result<(), CacheError> {
        if let Err(error) = validate_write(&entry, &self.policy) {
            self.metrics.rejected_writes += 1;
            return Err(error);
        }
        let fingerprint = entry.key.fingerprint()?;
        let size = entry_size(&entry)?;
        if size > self.policy.max_bytes || self.policy.max_entries == 0 {
            self.metrics.rejected_writes += 1;
            return Err(CacheError::new(
                "cache-entry-too-large",
                format!("entry={size}, max={}", self.policy.max_bytes),
            ));
        }
        if let Some(previous) = self.entries.remove(&fingerprint) {
            self.bytes = self.bytes.saturating_sub(entry_size(&previous)?);
            self.order.retain(|key| key != &fingerprint);
        }
        self.bytes = self.bytes.saturating_add(size);
        self.order.push_back(fingerprint.clone());
        self.entries.insert(fingerprint, entry);
        self.metrics.writes += 1;
        self.evict_to_policy()?;
        Ok(())
    }

    pub fn get(
        &mut self,
        key: &CacheKey,
        now_millis: i64,
    ) -> Result<Option<CacheEntry>, CacheError> {
        let fingerprint = key.fingerprint()?;
        let Some(entry) = self.entries.get(&fingerprint).cloned() else {
            self.metrics.misses += 1;
            return Ok(None);
        };
        if entry.expired_at(now_millis) {
            self.remove(&fingerprint)?;
            self.metrics.misses += 1;
            return Ok(None);
        }
        self.order.retain(|candidate| candidate != &fingerprint);
        self.order.push_back(fingerprint);
        self.metrics.hits += 1;
        Ok(Some(entry))
    }

    #[must_use]
    pub const fn metrics(&self) -> CacheMetrics {
        self.metrics
    }

    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    fn evict_to_policy(&mut self) -> Result<(), CacheError> {
        while self.entries.len() > self.policy.max_entries || self.bytes > self.policy.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove(&oldest)?;
            self.metrics.evictions += 1;
        }
        Ok(())
    }

    fn remove(&mut self, fingerprint: &ContentHash) -> Result<(), CacheError> {
        if let Some(entry) = self.entries.remove(fingerprint) {
            self.bytes = self.bytes.saturating_sub(entry_size(&entry)?);
        }
        self.order.retain(|candidate| candidate != fingerprint);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiskMetadata {
    blob_hash: ContentHash,
    expires_at_millis: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiskIndex {
    entries: BTreeMap<ContentHash, DiskMetadata>,
}

/// L2 content-addressed disk cache。
pub struct DiskCache {
    root: PathBuf,
    policy: CachePolicy,
    index: DiskIndex,
    metrics: CacheMetrics,
}

impl DiskCache {
    pub fn open(root: impl AsRef<Path>, policy: CachePolicy) -> Result<Self, CacheError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("blobs"))
            .map_err(|error| io_error("create-cache-dir", error))?;
        let index_path = root.join("index.json");
        let index = if index_path.exists() {
            let bytes =
                fs::read(&index_path).map_err(|error| io_error("read-cache-index", error))?;
            serde_json::from_slice(&bytes)
                .map_err(|error| CacheError::new("cache-index-json", error.to_string()))?
        } else {
            DiskIndex::default()
        };
        Ok(Self {
            root,
            policy,
            index,
            metrics: CacheMetrics::default(),
        })
    }

    pub fn put(&mut self, entry: &CacheEntry) -> Result<(), CacheError> {
        if let Err(error) = validate_write(entry, &self.policy) {
            self.metrics.rejected_writes += 1;
            return Err(error);
        }
        let bytes = serde_json::to_vec(entry)
            .map_err(|error| CacheError::new("cache-entry-json", error.to_string()))?;
        if bytes.len() > self.policy.max_bytes {
            self.metrics.rejected_writes += 1;
            return Err(CacheError::new(
                "cache-entry-too-large",
                bytes.len().to_string(),
            ));
        }
        let fingerprint = entry.key.fingerprint()?;
        let blob_hash = hash_bytes(&bytes);
        let blob_path = self.blob_path(&blob_hash);
        if !blob_path.exists() {
            atomic_write(&blob_path, &bytes)?;
        }
        self.index.entries.insert(
            fingerprint,
            DiskMetadata {
                blob_hash,
                expires_at_millis: entry.expires_at_millis,
            },
        );
        self.metrics.writes += 1;
        self.enforce_entry_limit();
        self.save_index()
    }

    pub fn get(
        &mut self,
        key: &CacheKey,
        now_millis: i64,
    ) -> Result<Option<CacheEntry>, CacheError> {
        let fingerprint = key.fingerprint()?;
        let Some(metadata) = self.index.entries.get(&fingerprint).cloned() else {
            self.metrics.misses += 1;
            return Ok(None);
        };
        if metadata
            .expires_at_millis
            .is_some_and(|expires| now_millis > expires)
        {
            self.index.entries.remove(&fingerprint);
            self.metrics.misses += 1;
            self.save_index()?;
            return Ok(None);
        }
        let bytes = fs::read(self.blob_path(&metadata.blob_hash))
            .map_err(|error| io_error("read-cache-blob", error))?;
        if hash_bytes(&bytes) != metadata.blob_hash {
            return Err(CacheError::new(
                "cache-blob-hash-mismatch",
                metadata.blob_hash.to_string(),
            ));
        }
        let entry: CacheEntry = serde_json::from_slice(&bytes)
            .map_err(|error| CacheError::new("cache-entry-json", error.to_string()))?;
        if entry.key != *key {
            return Err(CacheError::new(
                "cache-key-mismatch",
                fingerprint.to_string(),
            ));
        }
        self.metrics.hits += 1;
        Ok(Some(entry))
    }

    #[must_use]
    pub const fn metrics(&self) -> CacheMetrics {
        self.metrics
    }

    fn enforce_entry_limit(&mut self) {
        while self.index.entries.len() > self.policy.max_entries {
            let Some(oldest) = self.index.entries.keys().next().cloned() else {
                break;
            };
            self.index.entries.remove(&oldest);
            self.metrics.evictions += 1;
        }
    }

    fn blob_path(&self, hash: &ContentHash) -> PathBuf {
        self.root
            .join("blobs")
            .join(format!("{}.json", hash.as_str()))
    }

    fn save_index(&self) -> Result<(), CacheError> {
        let bytes = serde_json::to_vec_pretty(&self.index)
            .map_err(|error| CacheError::new("cache-index-json", error.to_string()))?;
        atomic_write(&self.root.join("index.json"), &bytes)
    }
}

/// L1 miss 后查询 L2 并回填 L1。
pub struct CacheEngine {
    l1: MemoryCache,
    l2: Option<DiskCache>,
}

impl CacheEngine {
    #[must_use]
    pub fn new(l1: MemoryCache, l2: Option<DiskCache>) -> Self {
        Self { l1, l2 }
    }

    pub fn put(&mut self, entry: CacheEntry) -> Result<(), CacheError> {
        if let Some(l2) = &mut self.l2 {
            l2.put(&entry)?;
        }
        self.l1.put(entry)
    }

    pub fn get(
        &mut self,
        key: &CacheKey,
        now_millis: i64,
    ) -> Result<Option<CacheEntry>, CacheError> {
        if let Some(entry) = self.l1.get(key, now_millis)? {
            return Ok(Some(entry));
        }
        let Some(l2) = &mut self.l2 else {
            return Ok(None);
        };
        let Some(entry) = l2.get(key, now_millis)? else {
            return Ok(None);
        };
        self.l1.put(entry.clone())?;
        Ok(Some(entry))
    }

    #[must_use]
    pub const fn l1_metrics(&self) -> CacheMetrics {
        self.l1.metrics()
    }

    #[must_use]
    pub fn l2_metrics(&self) -> Option<CacheMetrics> {
        self.l2.as_ref().map(DiskCache::metrics)
    }
}

fn hash_bytes(bytes: &[u8]) -> ContentHash {
    ContentHash::from(format!("{:x}", Sha256::digest(bytes)))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).map_err(|error| io_error("write-cache-temp", error))?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| io_error("replace-cache-file", error))?;
    }
    fs::rename(&temporary, path).map_err(|error| io_error("rename-cache-temp", error))
}

fn io_error(context: &'static str, error: std::io::Error) -> CacheError {
    CacheError::new(context, error.to_string())
}

#[cfg(test)]
mod tests {
    use harness_types::{IntegrityLabel, JsonValue};
    use tempfile::tempdir;

    use super::*;

    fn key(project: &str, confidentiality: ConfidentialityLabel) -> CacheKey {
        CacheKey {
            namespace: CacheNamespace::RepositoryScan,
            scope: CacheScope {
                project_id: Some(ProjectId::from(project)),
                session_id: None,
                provider: None,
                model: None,
            },
            input_hash: ContentHash::from("input:1"),
            schema_version: "1".to_owned(),
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality,
            },
        }
    }

    fn entry(key: CacheKey, value: JsonValue) -> CacheEntry {
        CacheEntry {
            key,
            value,
            effect_class: CacheEffectClass::ReadOnly,
            created_at_millis: 0,
            expires_at_millis: Some(10),
        }
    }

    #[test]
    fn project_scope_changes_fingerprint() {
        assert_ne!(
            key("project:a", ConfidentialityLabel::ProjectPrivate)
                .fingerprint()
                .expect("a"),
            key("project:b", ConfidentialityLabel::ProjectPrivate)
                .fingerprint()
                .expect("b")
        );
    }

    #[test]
    fn memory_cache_tracks_hit_miss_ttl_and_eviction() {
        let mut cache = MemoryCache::new(CachePolicy::safe_default(1, 10_000));
        let first_key = key("project:a", ConfidentialityLabel::ProjectPrivate);
        cache
            .put(entry(first_key.clone(), serde_json::json!({"value":1})))
            .expect("put first");
        assert!(cache.get(&first_key, 5).expect("get first").is_some());
        assert!(cache.get(&first_key, 11).expect("expired").is_none());
        cache
            .put(entry(first_key.clone(), serde_json::json!({"value":1})))
            .expect("put again");
        let second_key = CacheKey {
            input_hash: ContentHash::from("input:2"),
            ..first_key.clone()
        };
        cache
            .put(entry(second_key, serde_json::json!({"value":2})))
            .expect("put second");
        assert!(cache.get(&first_key, 5).expect("evicted").is_none());
        assert!(cache.metrics().evictions >= 1);
    }

    #[test]
    fn secret_and_effectful_entries_are_rejected() {
        let mut cache = MemoryCache::new(CachePolicy::safe_default(10, 10_000));
        let secret = entry(
            key("project:a", ConfidentialityLabel::UserSecret),
            serde_json::json!({"secret":"hidden"}),
        );
        assert_eq!(
            cache.put(secret).expect_err("secret").code,
            "secret-not-cacheable"
        );
        let mut effectful = entry(
            key("project:a", ConfidentialityLabel::ProjectPrivate),
            serde_json::json!({"sent":true}),
        );
        effectful.effect_class = CacheEffectClass::Effectful;
        assert_eq!(
            cache.put(effectful).expect_err("effectful").code,
            "effect-not-cacheable"
        );
    }

    #[test]
    fn disk_cas_round_trips_across_reopen_and_promotes_to_l1() {
        let temporary = tempdir().expect("tempdir");
        let policy = CachePolicy::safe_default(100, 100_000);
        let cache_key = key("project:a", ConfidentialityLabel::ProjectPrivate);
        let cached = entry(cache_key.clone(), serde_json::json!({"summary":"ok"}));
        {
            let mut disk = DiskCache::open(temporary.path(), policy.clone()).expect("open disk");
            disk.put(&cached).expect("disk put");
        }
        let disk = DiskCache::open(temporary.path(), policy.clone()).expect("reopen disk");
        let mut engine = CacheEngine::new(MemoryCache::new(policy), Some(disk));
        assert_eq!(engine.get(&cache_key, 5).expect("engine get"), Some(cached));
        assert!(engine.l1_metrics().writes >= 1);
        assert_eq!(engine.l2_metrics().expect("l2").hits, 1);
    }
}
