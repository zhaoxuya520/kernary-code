#![forbid(unsafe_code)]

//! Provider identity/catalog 与 wire protocol 分离；配置只保存 credential reference，不保存 Key。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use harness_types::{ModelId, ProviderId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CATALOG_SCHEMA_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MODEL_CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_MODEL_CACHE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DISCOVERED_MODELS: usize = 5_000;
pub const DEFAULT_DISCOVERY_FRESH_MILLIS: i64 = 24 * 60 * 60 * 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCatalogError {
    pub code: String,
    pub message: String,
}

impl ProviderCatalogError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for ProviderCatalogError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ProviderCatalogError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    OpenaiResponses,
    OpenaiChat,
    AnthropicMessages,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderSource {
    BuiltIn,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderDiscoveryFormat {
    OpenaiModels,
    AnthropicModels,
    OllamaTags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderDiscoveryAuth {
    None,
    Bearer,
    AnthropicApiKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderDiscoveryRouting {
    KnownRoutesOnly,
    SingleRouteAdditive,
}

/// Provider 的显式模型目录；启动时只读 metadata，不会自动发起网络请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProviderDiscoveryDefinition {
    pub format: ProviderDiscoveryFormat,
    pub endpoint: String,
    pub auth: ProviderDiscoveryAuth,
    pub routing: ProviderDiscoveryRouting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProviderRouteDefinition {
    pub protocol: ProviderProtocol,
    pub endpoint: String,
    pub models: Vec<ModelId>,
    #[serde(default)]
    pub reasoning_field: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProviderDefinition {
    pub id: ProviderId,
    pub display_name: String,
    pub credential_id: Option<String>,
    pub credential_required: bool,
    pub routes: Vec<ProviderRouteDefinition>,
    #[serde(default)]
    pub discovery: Option<ProviderDiscoveryDefinition>,
    #[serde(default = "built_in_source")]
    pub source: ProviderSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderModelCacheEntry {
    pub provider_id: ProviderId,
    pub endpoint_fingerprint: String,
    pub fetched_at_millis: i64,
    pub response_sha256: String,
    pub discovered_models: Vec<ModelId>,
    pub routable_models: Vec<ModelId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderModelCache {
    #[serde(default = "model_cache_schema_version")]
    schema_version: u32,
    #[serde(default)]
    entries: BTreeMap<ProviderId, ProviderModelCacheEntry>,
}

impl Default for ProviderModelCache {
    fn default() -> Self {
        Self {
            schema_version: MODEL_CACHE_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

fn model_cache_schema_version() -> u32 {
    MODEL_CACHE_SCHEMA_VERSION
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderModelCacheLoad {
    pub cache: ProviderModelCache,
    pub isolated_entries: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderDiscoveryStatus {
    NotConfigured,
    Never,
    Fresh,
    Stale,
    EndpointChanged,
}

impl Display for ProviderDiscoveryStatus {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotConfigured => "not-configured",
            Self::Never => "never",
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::EndpointChanged => "endpoint-changed",
        })
    }
}

fn built_in_source() -> ProviderSource {
    ProviderSource::BuiltIn
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct ProviderCatalogFile {
    schema_version: u32,
    #[serde(default)]
    providers: Vec<ProviderDefinition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedProviderRoute {
    pub provider: ProviderDefinition,
    pub route: ProviderRouteDefinition,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderCatalog {
    providers: BTreeMap<ProviderId, ProviderDefinition>,
}

impl ProviderCatalog {
    pub fn built_in() -> Result<Self, ProviderCatalogError> {
        let mut catalog = Self::default();
        for provider in built_in_providers() {
            catalog.insert(provider)?;
        }
        Ok(catalog)
    }

    pub fn load_project_file(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<usize, ProviderCatalogError> {
        let path = path.as_ref();
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| ProviderCatalogError::new("provider-config-io", error.to_string()))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Err(ProviderCatalogError::new(
                "provider-config-size-or-type",
                path.display().to_string(),
            ));
        }
        let source = std::fs::read_to_string(path)
            .map_err(|error| ProviderCatalogError::new("provider-config-io", error.to_string()))?;
        let mut file: ProviderCatalogFile = toml::from_str(&source).map_err(|error| {
            ProviderCatalogError::new("provider-config-toml", error.to_string())
        })?;
        if file.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(ProviderCatalogError::new(
                "provider-config-schema-unsupported",
                file.schema_version.to_string(),
            ));
        }
        let count = file.providers.len();
        let mut validated = Vec::with_capacity(count);
        let mut file_ids = BTreeSet::new();
        for provider in &mut file.providers {
            provider.source = ProviderSource::Project;
            validate_provider(provider)?;
            if self.providers.contains_key(&provider.id) {
                return Err(ProviderCatalogError::new(
                    "provider-builtin-override-denied",
                    provider.id.to_string(),
                ));
            }
            if !file_ids.insert(provider.id.clone()) {
                return Err(ProviderCatalogError::new(
                    "provider-id-conflict",
                    provider.id.to_string(),
                ));
            }
            validated.push(provider.clone());
        }
        for provider in validated {
            self.providers.insert(provider.id.clone(), provider);
        }
        Ok(count)
    }

    pub fn insert(&mut self, mut provider: ProviderDefinition) -> Result<(), ProviderCatalogError> {
        validate_provider(&mut provider)?;
        if self.providers.contains_key(&provider.id) {
            return Err(ProviderCatalogError::new(
                "provider-id-conflict",
                provider.id.to_string(),
            ));
        }
        self.providers.insert(provider.id.clone(), provider);
        Ok(())
    }

    #[must_use]
    pub fn list(&self) -> Vec<ProviderDefinition> {
        self.providers.values().cloned().collect()
    }

    #[must_use]
    pub fn get(&self, id: &ProviderId) -> Option<&ProviderDefinition> {
        self.providers.get(id)
    }

    pub fn resolve(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
    ) -> Result<ResolvedProviderRoute, ProviderCatalogError> {
        let provider = self.providers.get(provider_id).ok_or_else(|| {
            ProviderCatalogError::new("provider-not-found", provider_id.to_string())
        })?;
        let route = provider
            .routes
            .iter()
            .find(|route| route.models.contains(model_id))
            .cloned()
            .ok_or_else(|| {
                ProviderCatalogError::new(
                    "provider-model-not-routable",
                    format!("{provider_id}/{model_id}"),
                )
            })?;
        Ok(ResolvedProviderRoute {
            provider: provider.clone(),
            route,
        })
    }

    /// 用户显式选择单协议 Provider 时，可把该 Model 加到唯一已声明路由。
    /// 多协议 Provider 仍必须由受信目录明确路由，不能按名字猜协议。
    pub fn extend_explicit_single_route_model(
        &mut self,
        provider_id: &ProviderId,
        model_id: ModelId,
    ) -> Result<bool, ProviderCatalogError> {
        validate_model_id(model_id.as_str())?;
        let current = self.providers.get(provider_id).ok_or_else(|| {
            ProviderCatalogError::new("provider-not-found", provider_id.to_string())
        })?;
        if current
            .routes
            .iter()
            .any(|route| route.models.contains(&model_id))
        {
            return Ok(false);
        }
        if current.routes.len() != 1 {
            return Err(ProviderCatalogError::new(
                "provider-explicit-model-route-ambiguous",
                format!("{provider_id}/{model_id}"),
            ));
        }
        let mut candidate = current.clone();
        candidate.routes[0].models.push(model_id);
        validate_provider(&mut candidate)?;
        self.providers.insert(provider_id.clone(), candidate);
        Ok(true)
    }
}

impl ProviderModelCache {
    pub fn load_isolated(
        path: impl AsRef<Path>,
    ) -> Result<ProviderModelCacheLoad, ProviderCatalogError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(ProviderModelCacheLoad {
                cache: Self::default(),
                isolated_entries: vec![],
            });
        }
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| ProviderCatalogError::new("provider-cache-io", error.to_string()))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_MODEL_CACHE_BYTES {
            return Err(ProviderCatalogError::new(
                "provider-cache-size-or-type",
                path.display().to_string(),
            ));
        }
        let bytes = fs::read(path)
            .map_err(|error| ProviderCatalogError::new("provider-cache-io", error.to_string()))?;
        let mut parsed: Self = serde_json::from_slice(&bytes)
            .map_err(|error| ProviderCatalogError::new("provider-cache-json", error.to_string()))?;
        if parsed.schema_version != MODEL_CACHE_SCHEMA_VERSION {
            return Err(ProviderCatalogError::new(
                "provider-cache-schema-unsupported",
                parsed.schema_version.to_string(),
            ));
        }
        let mut isolated_entries = Vec::new();
        parsed.entries.retain(|provider_id, entry| {
            match validate_cache_entry(provider_id, entry) {
                Ok(()) => true,
                Err(error) => {
                    isolated_entries.push(format!("{provider_id}:{}", error.code));
                    false
                }
            }
        });
        Ok(ProviderModelCacheLoad {
            cache: parsed,
            isolated_entries,
        })
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ProviderCatalogError> {
        if self.schema_version != MODEL_CACHE_SCHEMA_VERSION {
            return Err(ProviderCatalogError::new(
                "provider-cache-schema-unsupported",
                self.schema_version.to_string(),
            ));
        }
        for (provider_id, entry) in &self.entries {
            validate_cache_entry(provider_id, entry)?;
        }
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| ProviderCatalogError::new("provider-cache-json", error.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_MODEL_CACHE_BYTES {
            return Err(ProviderCatalogError::new(
                "provider-cache-too-large",
                bytes.len().to_string(),
            ));
        }
        atomic_write(path.as_ref(), &bytes)
    }

    pub fn upsert(&mut self, entry: ProviderModelCacheEntry) -> Result<(), ProviderCatalogError> {
        validate_cache_entry(&entry.provider_id, &entry)?;
        self.entries.insert(entry.provider_id.clone(), entry);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, provider_id: &ProviderId) -> Option<&ProviderModelCacheEntry> {
        self.entries.get(provider_id)
    }

    #[must_use]
    pub fn status(
        &self,
        provider: &ProviderDefinition,
        now_millis: i64,
    ) -> ProviderDiscoveryStatus {
        if provider.discovery.is_none() {
            return ProviderDiscoveryStatus::NotConfigured;
        }
        let Some(entry) = self.entries.get(&provider.id) else {
            return ProviderDiscoveryStatus::Never;
        };
        if entry.endpoint_fingerprint != provider_discovery_fingerprint(provider) {
            return ProviderDiscoveryStatus::EndpointChanged;
        }
        let age = now_millis.saturating_sub(entry.fetched_at_millis);
        if (0..=DEFAULT_DISCOVERY_FRESH_MILLIS).contains(&age) {
            ProviderDiscoveryStatus::Fresh
        } else {
            ProviderDiscoveryStatus::Stale
        }
    }
}

/// 把受信缓存合并到 Provider 快照。endpoint fingerprint 不一致时完全忽略缓存。
pub fn provider_with_cached_models(
    provider: &ProviderDefinition,
    cache: &ProviderModelCache,
) -> Result<ProviderDefinition, ProviderCatalogError> {
    let Some(entry) = cache.get(&provider.id) else {
        return Ok(provider.clone());
    };
    if entry.endpoint_fingerprint != provider_discovery_fingerprint(provider) {
        return Ok(provider.clone());
    }
    merge_discovered_models(provider, &entry.discovered_models).map(|(provider, _)| provider)
}

/// 根据显式路由策略计算可执行模型；多协议 Provider 永远不会按名字猜协议。
pub fn merge_discovered_models(
    provider: &ProviderDefinition,
    discovered: &[ModelId],
) -> Result<(ProviderDefinition, Vec<ModelId>), ProviderCatalogError> {
    let Some(discovery) = &provider.discovery else {
        return Err(ProviderCatalogError::new(
            "provider-discovery-not-configured",
            provider.id.to_string(),
        ));
    };
    let mut discovered = discovered.to_vec();
    discovered.sort();
    discovered.dedup();
    if discovered.len() > MAX_DISCOVERED_MODELS {
        return Err(ProviderCatalogError::new(
            "provider-discovery-model-limit",
            discovered.len().to_string(),
        ));
    }
    for model in &discovered {
        validate_model_id(model.as_str())?;
    }
    let mut effective = provider.clone();
    let routable = match discovery.routing {
        ProviderDiscoveryRouting::KnownRoutesOnly => {
            let known = provider
                .routes
                .iter()
                .flat_map(|route| route.models.iter().cloned())
                .collect::<BTreeSet<_>>();
            discovered
                .iter()
                .filter(|model| known.contains(*model))
                .cloned()
                .collect::<Vec<_>>()
        }
        ProviderDiscoveryRouting::SingleRouteAdditive => {
            if effective.routes.len() != 1 {
                return Err(ProviderCatalogError::new(
                    "provider-discovery-routing-ambiguous",
                    provider.id.to_string(),
                ));
            }
            effective.routes[0].models.extend(discovered.clone());
            effective.routes[0].models.sort();
            effective.routes[0].models.dedup();
            discovered.clone()
        }
    };
    validate_provider(&mut effective)?;
    Ok((effective, routable))
}

#[must_use]
pub fn provider_discovery_fingerprint(provider: &ProviderDefinition) -> String {
    let mut parts = provider
        .routes
        .iter()
        .map(|route| format!("route|{:?}|{}", route.protocol, route.endpoint))
        .collect::<Vec<_>>();
    if let Some(discovery) = &provider.discovery {
        parts.push(format!(
            "discovery|{:?}|{:?}|{:?}|{}",
            discovery.format, discovery.auth, discovery.routing, discovery.endpoint
        ));
    }
    parts.sort();
    format!("{:x}", Sha256::digest(parts.join("\n").as_bytes()))
}

fn validate_cache_entry(
    provider_id: &ProviderId,
    entry: &ProviderModelCacheEntry,
) -> Result<(), ProviderCatalogError> {
    if provider_id != &entry.provider_id {
        return Err(ProviderCatalogError::new(
            "provider-cache-id-mismatch",
            format!("{provider_id}/{}", entry.provider_id),
        ));
    }
    validate_id(provider_id.as_str(), "provider-cache-id-invalid")?;
    if entry.fetched_at_millis < 0
        || entry.endpoint_fingerprint.len() != 64
        || entry.response_sha256.len() != 64
        || !entry
            .endpoint_fingerprint
            .bytes()
            .chain(entry.response_sha256.bytes())
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ProviderCatalogError::new(
            "provider-cache-metadata-invalid",
            provider_id.to_string(),
        ));
    }
    if entry.discovered_models.len() > MAX_DISCOVERED_MODELS
        || entry.routable_models.len() > MAX_DISCOVERED_MODELS
    {
        return Err(ProviderCatalogError::new(
            "provider-cache-model-limit",
            provider_id.to_string(),
        ));
    }
    let discovered = entry.discovered_models.iter().collect::<BTreeSet<_>>();
    for model in entry
        .discovered_models
        .iter()
        .chain(entry.routable_models.iter())
    {
        validate_model_id(model.as_str())?;
    }
    if entry
        .routable_models
        .iter()
        .any(|model| !discovered.contains(model))
    {
        return Err(ProviderCatalogError::new(
            "provider-cache-routable-not-discovered",
            provider_id.to_string(),
        ));
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProviderCatalogError> {
    let parent = path.parent().ok_or_else(|| {
        ProviderCatalogError::new("provider-cache-parent-missing", path.display().to_string())
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| ProviderCatalogError::new("provider-cache-io", error.to_string()))?;
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| ProviderCatalogError::new("provider-cache-io", error.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(ProviderCatalogError::new(
                "provider-cache-target-type",
                path.display().to_string(),
            ));
        }
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let backup = path.with_extension(format!("{}.previous", std::process::id()));
    if temporary.exists() || backup.exists() {
        return Err(ProviderCatalogError::new(
            "provider-cache-staging-exists",
            path.display().to_string(),
        ));
    }
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| ProviderCatalogError::new("provider-cache-io", error.to_string()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| ProviderCatalogError::new("provider-cache-io", error.to_string()))?;
        if path.exists() {
            fs::rename(path, &backup).map_err(|error| {
                ProviderCatalogError::new("provider-cache-backup", error.to_string())
            })?;
        }
        if let Err(error) = fs::rename(&temporary, path) {
            if backup.exists() {
                let _ = fs::rename(&backup, path);
            }
            return Err(ProviderCatalogError::new(
                "provider-cache-commit",
                error.to_string(),
            ));
        }
        if backup.exists() {
            // 新缓存已经原子生效；旧备份清理失败不应把成功提交报告成失败。
            let _ = fs::remove_file(&backup);
        }
        Ok(())
    })();
    if result.is_err() && temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn validate_provider(provider: &mut ProviderDefinition) -> Result<(), ProviderCatalogError> {
    validate_id(provider.id.as_str(), "provider-id-invalid")?;
    provider.display_name = provider.display_name.trim().to_owned();
    if provider.display_name.is_empty() || provider.routes.is_empty() {
        return Err(ProviderCatalogError::new(
            "provider-required-field-empty",
            provider.id.to_string(),
        ));
    }
    let mut models = BTreeSet::new();
    for route in &mut provider.routes {
        validate_endpoint(&route.endpoint, route.protocol)?;
        route.models.sort();
        route.models.dedup();
        if route.models.is_empty() {
            return Err(ProviderCatalogError::new(
                "provider-route-models-empty",
                provider.id.to_string(),
            ));
        }
        for model in &route.models {
            validate_model_id(model.as_str())?;
            if !models.insert(model.clone()) {
                return Err(ProviderCatalogError::new(
                    "provider-model-route-conflict",
                    format!("{}/{}", provider.id, model),
                ));
            }
        }
        if route
            .reasoning_field
            .as_deref()
            .is_some_and(|field| !matches!(field, "omit" | "reasoning-effort"))
        {
            return Err(ProviderCatalogError::new(
                "provider-reasoning-field-invalid",
                route.reasoning_field.clone().unwrap_or_default(),
            ));
        }
    }
    if let Some(discovery) = &provider.discovery {
        validate_discovery(provider, discovery)?;
    }
    if provider.source == ProviderSource::Project {
        let expected = project_credential_scope(provider);
        match &provider.credential_id {
            Some(actual) if actual != &expected => {
                return Err(ProviderCatalogError::new(
                    "provider-credential-scope-mismatch",
                    format!("expected={expected}, actual={actual}"),
                ));
            }
            None if provider.credential_required => provider.credential_id = Some(expected),
            _ => {}
        }
    }
    if provider.credential_required && provider.credential_id.is_none() {
        return Err(ProviderCatalogError::new(
            "provider-credential-id-missing",
            provider.id.to_string(),
        ));
    }
    if let Some(credential_id) = &provider.credential_id {
        validate_id(credential_id, "provider-credential-id-invalid")?;
    }
    Ok(())
}

fn project_credential_scope(provider: &ProviderDefinition) -> String {
    let mut routes = provider
        .routes
        .iter()
        .map(|route| format!("{:?}|{}", route.protocol, route.endpoint))
        .collect::<Vec<_>>();
    if let Some(discovery) = &provider.discovery {
        routes.push(format!(
            "discovery|{:?}|{:?}|{}",
            discovery.format, discovery.auth, discovery.endpoint
        ));
    }
    routes.sort();
    let digest = format!("{:x}", Sha256::digest(routes.join("\n").as_bytes()));
    format!("provider:{}:{}", provider.id, &digest[..16])
}

fn validate_id(value: &str, code: &str) -> Result<(), ProviderCatalogError> {
    let valid = !value.is_empty()
        && value.len() <= 96
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-' | b':'))
        });
    if valid {
        Ok(())
    } else {
        Err(ProviderCatalogError::new(code, value))
    }
}

fn validate_model_id(value: &str) -> Result<(), ProviderCatalogError> {
    let valid = !value.is_empty()
        && value.len() <= 192
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-' | b':' | b'/' | b'@'))
        });
    if valid {
        Ok(())
    } else {
        Err(ProviderCatalogError::new(
            "provider-model-id-invalid",
            value,
        ))
    }
}

fn validate_endpoint(
    endpoint: &str,
    protocol: ProviderProtocol,
) -> Result<(), ProviderCatalogError> {
    let url = url::Url::parse(endpoint).map_err(|error| {
        ProviderCatalogError::new("provider-endpoint-invalid", error.to_string())
    })?;
    let host = url
        .host_str()
        .ok_or_else(|| ProviderCatalogError::new("provider-endpoint-host-missing", endpoint))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderCatalogError::new(
            "provider-endpoint-sensitive-or-ambiguous",
            endpoint,
        ));
    }
    let required_suffix = match protocol {
        ProviderProtocol::OpenaiResponses => "/responses",
        ProviderProtocol::OpenaiChat => "/chat/completions",
        ProviderProtocol::AnthropicMessages => "/messages",
    };
    if !url.path().ends_with(required_suffix) {
        return Err(ProviderCatalogError::new(
            "provider-endpoint-protocol-mismatch",
            format!("expected suffix {required_suffix}: {endpoint}"),
        ));
    }
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(ProviderCatalogError::new(
            "provider-endpoint-insecure",
            endpoint,
        ));
    }
    Ok(())
}

fn validate_discovery(
    provider: &ProviderDefinition,
    discovery: &ProviderDiscoveryDefinition,
) -> Result<(), ProviderCatalogError> {
    let discovery_url = validate_safe_url(&discovery.endpoint, "provider-discovery-endpoint")?;
    if discovery.routing == ProviderDiscoveryRouting::SingleRouteAdditive
        && provider.routes.len() != 1
    {
        return Err(ProviderCatalogError::new(
            "provider-discovery-routing-ambiguous",
            provider.id.to_string(),
        ));
    }
    if discovery.auth != ProviderDiscoveryAuth::None && !provider.credential_required {
        return Err(ProviderCatalogError::new(
            "provider-discovery-auth-without-credential",
            provider.id.to_string(),
        ));
    }
    if discovery.format == ProviderDiscoveryFormat::OllamaTags
        && discovery.auth != ProviderDiscoveryAuth::None
    {
        return Err(ProviderCatalogError::new(
            "provider-discovery-ollama-auth-invalid",
            provider.id.to_string(),
        ));
    }
    let discovery_origin = discovery_url.origin().ascii_serialization();
    for route in &provider.routes {
        let route_url = validate_safe_url(&route.endpoint, "provider-route-endpoint")?;
        if route_url.origin().ascii_serialization() != discovery_origin {
            return Err(ProviderCatalogError::new(
                "provider-discovery-cross-origin-denied",
                provider.id.to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_safe_url(endpoint: &str, code: &str) -> Result<url::Url, ProviderCatalogError> {
    let url = url::Url::parse(endpoint)
        .map_err(|error| ProviderCatalogError::new(code, error.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| ProviderCatalogError::new(code, "missing host"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderCatalogError::new(
            code,
            "userinfo/query/fragment denied",
        ));
    }
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(ProviderCatalogError::new(code, "remote HTTPS required"));
    }
    Ok(url)
}

fn built_in_providers() -> Vec<ProviderDefinition> {
    vec![
        with_discovery(
            provider(
                "openai",
                "OpenAI",
                Some("openai:default"),
                true,
                vec![route(
                    ProviderProtocol::OpenaiResponses,
                    "https://api.openai.com/v1/responses",
                    &["gpt-5.6-sol", "gpt-5.6-terra"],
                )],
            ),
            ProviderDiscoveryFormat::OpenaiModels,
            "https://api.openai.com/v1/models",
            ProviderDiscoveryAuth::Bearer,
            ProviderDiscoveryRouting::KnownRoutesOnly,
        ),
        with_discovery(
            provider(
                "anthropic",
                "Anthropic / Claude",
                Some("anthropic:default"),
                true,
                vec![route(
                    ProviderProtocol::AnthropicMessages,
                    "https://api.anthropic.com/v1/messages",
                    &["claude-sonnet-4-6", "claude-opus-4-6"],
                )],
            ),
            ProviderDiscoveryFormat::AnthropicModels,
            "https://api.anthropic.com/v1/models",
            ProviderDiscoveryAuth::AnthropicApiKey,
            ProviderDiscoveryRouting::SingleRouteAdditive,
        ),
        with_discovery(
            provider(
                "opencode-go",
                "OpenCode Go",
                Some("opencode-go:default"),
                true,
                vec![
                    route(
                        ProviderProtocol::OpenaiResponses,
                        "https://opencode.ai/zen/go/v1/responses",
                        &["grok-4.6", "gpt-5.6-luna", "muse-spark-1.2-contributor"],
                    ),
                    route(
                        ProviderProtocol::OpenaiChat,
                        "https://opencode.ai/zen/go/v1/chat/completions",
                        &[
                            "glm-5.3-flash",
                            "glm-5.3",
                            "glm-5.2",
                            "glm-5.1",
                            "kimi-k3",
                            "kimi-k2.7-code",
                            "kimi-k2.6",
                            "longcat-2.0",
                            "deepseek-v4-pro",
                            "deepseek-v4-flash",
                            "deepseek-v4-flash-vision-exp",
                            "mimo-v2.5",
                            "mimo-v2.5-pro",
                            "hy4-preview",
                            "hy3",
                        ],
                    ),
                    route(
                        ProviderProtocol::AnthropicMessages,
                        "https://opencode.ai/zen/go/v1/messages",
                        &[
                            "minimax-m3",
                            "minimax-m2.7",
                            "minimax-m2.5",
                            "qwen3.8-max",
                            "qwen3.8-flash",
                            "qwen3.7-max",
                            "qwen3.7-plus",
                            "qwen3.6-plus",
                        ],
                    ),
                ],
            ),
            ProviderDiscoveryFormat::OpenaiModels,
            "https://opencode.ai/zen/go/v1/models",
            ProviderDiscoveryAuth::None,
            ProviderDiscoveryRouting::KnownRoutesOnly,
        ),
        with_discovery(
            provider(
                "opencode-zen",
                "OpenCode Zen",
                Some("opencode-zen:default"),
                true,
                vec![
                    route(
                        ProviderProtocol::OpenaiResponses,
                        "https://opencode.ai/zen/v1/responses",
                        &[
                            "gpt-5.6-sol",
                            "gpt-5.6-terra",
                            "gpt-5.6-luna",
                            "gpt-5.5",
                            "gpt-5.5-pro",
                            "gpt-5.4",
                            "gpt-5.4-pro",
                            "gpt-5.4-mini",
                            "gpt-5.4-nano",
                            "gpt-5.3-codex",
                            "gpt-5.3-codex-spark",
                            "gpt-5.2",
                            "gpt-5.2-codex",
                            "gpt-5.1",
                            "gpt-5.1-codex",
                            "gpt-5.1-codex-max",
                            "gpt-5.1-codex-mini",
                            "gpt-5",
                            "gpt-5-codex",
                            "gpt-5-nano",
                            "grok-4.6",
                            "grok-4.5",
                            "grok-build-0.1",
                            "muse-spark-1.2",
                            "muse-spark-1.2-contributor-free",
                        ],
                    ),
                    route(
                        ProviderProtocol::AnthropicMessages,
                        "https://opencode.ai/zen/v1/messages",
                        &[
                            "claude-fable-5",
                            "claude-opus-5",
                            "claude-opus-4-8",
                            "claude-opus-4-7",
                            "claude-opus-4-6",
                            "claude-opus-4-5",
                            "claude-sonnet-5",
                            "claude-sonnet-4-6",
                            "claude-sonnet-4-5",
                            "claude-haiku-4-5",
                            "qwen3.7-max",
                            "qwen3.7-plus",
                            "qwen3.6-plus",
                            "qwen3.5-plus",
                        ],
                    ),
                    route(
                        ProviderProtocol::OpenaiChat,
                        "https://opencode.ai/zen/v1/chat/completions",
                        &[
                            "deepseek-v4-pro",
                            "deepseek-v4-flash",
                            "minimax-m3",
                            "minimax-m2.7",
                            "minimax-m2.5",
                            "glm-5.2",
                            "glm-5.1",
                            "glm-5",
                            "kimi-k2.5",
                            "kimi-k2.6",
                            "kimi-k2.7-code",
                            "kimi-k3",
                            "big-pickle",
                            "mimo-v2.5-free",
                            "hy3-free",
                            "ling-3.0-flash-fin-free",
                            "nemotron-3-ultra-free",
                            "nemotron-3.5-lightning-free",
                        ],
                    ),
                ],
            ),
            ProviderDiscoveryFormat::OpenaiModels,
            "https://opencode.ai/zen/v1/models",
            ProviderDiscoveryAuth::None,
            ProviderDiscoveryRouting::KnownRoutesOnly,
        ),
        provider(
            "groq",
            "Groq",
            Some("groq:default"),
            true,
            vec![route(
                ProviderProtocol::OpenaiChat,
                "https://api.groq.com/openai/v1/chat/completions",
                &["openai/gpt-oss-120b", "qwen/qwen3.6-27b"],
            )],
        ),
        provider(
            "xai",
            "xAI / Grok",
            Some("xai:default"),
            true,
            vec![route(
                ProviderProtocol::OpenaiChat,
                "https://api.x.ai/v1/chat/completions",
                &["grok-4.6"],
            )],
        ),
        provider(
            "together",
            "Together AI",
            Some("together:default"),
            true,
            vec![route(
                ProviderProtocol::OpenaiChat,
                "https://api.together.ai/v1/chat/completions",
                &["openai/gpt-oss-20b"],
            )],
        ),
        provider(
            "gemini",
            "Google Gemini (OpenAI compatibility)",
            Some("gemini:default"),
            true,
            vec![route(
                ProviderProtocol::OpenaiChat,
                "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
                &["gemini-2.5-pro", "gemini-2.5-flash"],
            )],
        ),
        provider(
            "deepseek",
            "DeepSeek",
            Some("deepseek:default"),
            true,
            vec![route(
                ProviderProtocol::OpenaiChat,
                "https://api.deepseek.com/chat/completions",
                &["deepseek-chat", "deepseek-reasoner"],
            )],
        ),
        provider(
            "openrouter",
            "OpenRouter",
            Some("openrouter:default"),
            true,
            vec![route(
                ProviderProtocol::OpenaiChat,
                "https://openrouter.ai/api/v1/chat/completions",
                &["openai/gpt-5.6-sol", "anthropic/claude-sonnet-4.6"],
            )],
        ),
        with_discovery(
            provider(
                "ollama",
                "Ollama",
                None,
                false,
                vec![route(
                    ProviderProtocol::OpenaiChat,
                    "http://127.0.0.1:11434/v1/chat/completions",
                    &["qwen3-coder"],
                )],
            ),
            ProviderDiscoveryFormat::OllamaTags,
            "http://127.0.0.1:11434/api/tags",
            ProviderDiscoveryAuth::None,
            ProviderDiscoveryRouting::SingleRouteAdditive,
        ),
        with_discovery(
            provider(
                "lmstudio",
                "LM Studio",
                None,
                false,
                vec![route(
                    ProviderProtocol::OpenaiChat,
                    "http://127.0.0.1:1234/v1/chat/completions",
                    &["local-model"],
                )],
            ),
            ProviderDiscoveryFormat::OpenaiModels,
            "http://127.0.0.1:1234/v1/models",
            ProviderDiscoveryAuth::None,
            ProviderDiscoveryRouting::SingleRouteAdditive,
        ),
        with_discovery(
            provider(
                "vllm",
                "vLLM (local)",
                None,
                false,
                vec![route(
                    ProviderProtocol::OpenaiChat,
                    "http://127.0.0.1:8000/v1/chat/completions",
                    &["local-model"],
                )],
            ),
            ProviderDiscoveryFormat::OpenaiModels,
            "http://127.0.0.1:8000/v1/models",
            ProviderDiscoveryAuth::None,
            ProviderDiscoveryRouting::SingleRouteAdditive,
        ),
        with_discovery(
            provider(
                "llamacpp",
                "llama.cpp server",
                None,
                false,
                vec![route(
                    ProviderProtocol::OpenaiChat,
                    "http://127.0.0.1:8080/v1/chat/completions",
                    &["local-model"],
                )],
            ),
            ProviderDiscoveryFormat::OpenaiModels,
            "http://127.0.0.1:8080/v1/models",
            ProviderDiscoveryAuth::None,
            ProviderDiscoveryRouting::SingleRouteAdditive,
        ),
    ]
}

fn provider(
    id: &str,
    display_name: &str,
    credential_id: Option<&str>,
    credential_required: bool,
    routes: Vec<ProviderRouteDefinition>,
) -> ProviderDefinition {
    ProviderDefinition {
        id: ProviderId::from(id),
        display_name: display_name.to_owned(),
        credential_id: credential_id.map(str::to_owned),
        credential_required,
        routes,
        discovery: None,
        source: ProviderSource::BuiltIn,
    }
}

fn with_discovery(
    mut provider: ProviderDefinition,
    format: ProviderDiscoveryFormat,
    endpoint: &str,
    auth: ProviderDiscoveryAuth,
    routing: ProviderDiscoveryRouting,
) -> ProviderDefinition {
    provider.discovery = Some(ProviderDiscoveryDefinition {
        format,
        endpoint: endpoint.to_owned(),
        auth,
        routing,
    });
    provider
}

fn route(protocol: ProviderProtocol, endpoint: &str, models: &[&str]) -> ProviderRouteDefinition {
    ProviderRouteDefinition {
        protocol,
        endpoint: endpoint.to_owned(),
        models: models.iter().copied().map(ModelId::from).collect(),
        reasoning_field: None,
    }
}

#[must_use]
pub fn default_project_catalog_path(project_root: &Path) -> PathBuf {
    project_root.join("kernary.providers.toml")
}

#[must_use]
pub fn default_model_cache_path(project_root: &Path) -> PathBuf {
    project_root.join(".harness/provider-models-v1.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn builtins_resolve_opencode_go_models_to_distinct_protocols() {
        let catalog = ProviderCatalog::built_in().expect("catalog");
        assert_eq!(
            catalog
                .resolve(&ProviderId::from("opencode-go"), &ModelId::from("kimi-k3"))
                .expect("chat")
                .route
                .protocol,
            ProviderProtocol::OpenaiChat
        );
        assert_eq!(
            catalog
                .resolve(
                    &ProviderId::from("opencode-go"),
                    &ModelId::from("minimax-m3")
                )
                .expect("messages")
                .route
                .protocol,
            ProviderProtocol::AnthropicMessages
        );
    }

    #[test]
    fn custom_file_is_strict_secure_and_cannot_override_builtin() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("providers.toml");
        std::fs::write(
            &path,
            r#"
schema_version = 1

[[providers]]
id = "company-relay"
display_name = "Company Relay"
credential_required = true

[[providers.routes]]
protocol = "openai-chat"
endpoint = "https://relay.example.com/v1/chat/completions"
models = ["company-coder"]
reasoning_field = "reasoning-effort"
"#,
        )
        .expect("config");
        let mut catalog = ProviderCatalog::built_in().expect("catalog");
        assert_eq!(catalog.load_project_file(&path).expect("load"), 1);
        let provider = catalog
            .get(&ProviderId::from("company-relay"))
            .expect("provider");
        assert!(
            provider
                .credential_id
                .as_deref()
                .is_some_and(|id| id.starts_with("provider:company-relay:"))
        );

        std::fs::write(
            &path,
            r#"
schema_version = 1
[[providers]]
id = "openai"
display_name = "Override"
credential_required = false
[[providers.routes]]
protocol = "openai-chat"
endpoint = "https://evil.example/v1/chat/completions"
models = ["gpt"]
"#,
        )
        .expect("override config");
        assert_eq!(
            catalog
                .load_project_file(&path)
                .expect_err("override denied")
                .code,
            "provider-builtin-override-denied"
        );
    }

    #[test]
    fn endpoint_and_model_route_validation_fail_closed() {
        let mut catalog = ProviderCatalog::default();
        let error = catalog
            .insert(provider(
                "relay",
                "Relay",
                None,
                false,
                vec![route(
                    ProviderProtocol::OpenaiChat,
                    "http://relay.example.com/v1/chat/completions",
                    &["model"],
                )],
            ))
            .expect_err("remote http denied");
        assert_eq!(error.code, "provider-endpoint-insecure");

        let error = catalog
            .insert(ProviderDefinition {
                id: ProviderId::from("relay"),
                display_name: "Relay".to_owned(),
                credential_id: Some("openai:default".to_owned()),
                credential_required: true,
                routes: vec![route(
                    ProviderProtocol::OpenaiChat,
                    "https://relay.example.com/v1/chat/completions",
                    &["model"],
                )],
                discovery: None,
                source: ProviderSource::Project,
            })
            .expect_err("credential reuse denied");
        assert_eq!(error.code, "provider-credential-scope-mismatch");
    }

    #[test]
    fn invalid_project_file_is_transactional_and_partially_activates_nothing() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("providers.toml");
        std::fs::write(
            &path,
            r#"
schema_version = 1
[[providers]]
id = "would-have-been-valid"
display_name = "Valid First"
credential_required = false
[[providers.routes]]
protocol = "openai-chat"
endpoint = "http://127.0.0.1:9000/v1/chat/completions"
models = ["model"]

[[providers]]
id = "openai"
display_name = "Forbidden Override"
credential_required = false
[[providers.routes]]
protocol = "openai-chat"
endpoint = "https://relay.example.com/v1/chat/completions"
models = ["model"]
"#,
        )
        .expect("config");
        let mut catalog = ProviderCatalog::built_in().expect("catalog");
        assert!(catalog.load_project_file(&path).is_err());
        assert!(
            catalog
                .get(&ProviderId::from("would-have-been-valid"))
                .is_none()
        );
    }

    #[test]
    fn duplicate_insert_is_transactional_and_preserves_original_provider() {
        let mut catalog = ProviderCatalog::default();
        catalog
            .insert(provider(
                "relay",
                "Original Relay",
                None,
                false,
                vec![route(
                    ProviderProtocol::OpenaiChat,
                    "http://127.0.0.1:9000/v1/chat/completions",
                    &["original-model"],
                )],
            ))
            .expect("首次插入成功");

        let error = catalog
            .insert(provider(
                "relay",
                "Replacement Relay",
                None,
                false,
                vec![route(
                    ProviderProtocol::OpenaiChat,
                    "http://127.0.0.1:9001/v1/chat/completions",
                    &["replacement-model"],
                )],
            ))
            .expect_err("重复 Provider ID 必须拒绝");

        assert_eq!(error.code, "provider-id-conflict");
        let original = catalog
            .get(&ProviderId::from("relay"))
            .expect("原 Provider 仍存在");
        assert_eq!(original.display_name, "Original Relay");
        assert_eq!(
            original.routes[0].models,
            vec![ModelId::from("original-model")]
        );
    }

    #[test]
    fn discovery_requires_same_origin_and_unambiguous_routing() {
        let mut catalog = ProviderCatalog::default();
        let mut relay = provider(
            "relay",
            "Relay",
            Some("relay:default"),
            true,
            vec![route(
                ProviderProtocol::OpenaiChat,
                "https://relay.example/v1/chat/completions",
                &["model"],
            )],
        );
        relay.discovery = Some(ProviderDiscoveryDefinition {
            format: ProviderDiscoveryFormat::OpenaiModels,
            endpoint: "https://attacker.example/v1/models".to_owned(),
            auth: ProviderDiscoveryAuth::Bearer,
            routing: ProviderDiscoveryRouting::SingleRouteAdditive,
        });
        let error = catalog.insert(relay).expect_err("cross origin denied");
        assert_eq!(error.code, "provider-discovery-cross-origin-denied");

        let mut opencode = ProviderCatalog::built_in()
            .expect("catalog")
            .get(&ProviderId::from("opencode-go"))
            .expect("provider")
            .clone();
        opencode.discovery.as_mut().expect("discovery").routing =
            ProviderDiscoveryRouting::SingleRouteAdditive;
        let error = ProviderCatalog::default()
            .insert(opencode)
            .expect_err("ambiguous multi route denied");
        assert_eq!(error.code, "provider-discovery-routing-ambiguous");
    }

    #[test]
    fn known_routes_never_guess_and_single_route_adds_discovered_models() {
        let mut catalog = ProviderCatalog::built_in().expect("catalog");
        let opencode = catalog
            .get(&ProviderId::from("opencode-go"))
            .expect("opencode");
        let (effective, routable) = merge_discovered_models(
            opencode,
            &[ModelId::from("kimi-k3"), ModelId::from("unknown-model")],
        )
        .expect("merge");
        assert_eq!(effective, *opencode);
        assert_eq!(routable, vec![ModelId::from("kimi-k3")]);

        let ollama = catalog.get(&ProviderId::from("ollama")).expect("ollama");
        let (effective, routable) =
            merge_discovered_models(ollama, &[ModelId::from("new-local:8b")]).expect("merge");
        assert!(
            effective.routes[0]
                .models
                .contains(&ModelId::from("new-local:8b"))
        );
        assert_eq!(routable, vec![ModelId::from("new-local:8b")]);

        assert!(
            catalog
                .extend_explicit_single_route_model(
                    &ProviderId::from("ollama"),
                    ModelId::from("explicit-local")
                )
                .expect("single route")
        );
        let error = catalog
            .extend_explicit_single_route_model(
                &ProviderId::from("opencode-go"),
                ModelId::from("unknown-multi-route"),
            )
            .expect_err("multi route remains ambiguous");
        assert_eq!(error.code, "provider-explicit-model-route-ambiguous");
    }

    #[test]
    fn model_cache_is_versioned_atomic_and_isolates_invalid_entries() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("state/provider-models-v1.json");
        let catalog = ProviderCatalog::built_in().expect("catalog");
        let ollama = catalog.get(&ProviderId::from("ollama")).expect("ollama");
        let mut cache = ProviderModelCache::default();
        cache
            .upsert(ProviderModelCacheEntry {
                provider_id: ProviderId::from("ollama"),
                endpoint_fingerprint: provider_discovery_fingerprint(ollama),
                fetched_at_millis: 123,
                response_sha256: "a".repeat(64),
                discovered_models: vec![ModelId::from("qwen:8b")],
                routable_models: vec![ModelId::from("qwen:8b")],
            })
            .expect("entry");
        cache.save(&path).expect("save");
        let loaded = ProviderModelCache::load_isolated(&path).expect("load");
        assert!(loaded.isolated_entries.is_empty());
        assert_eq!(
            loaded
                .cache
                .get(&ProviderId::from("ollama"))
                .expect("entry")
                .discovered_models,
            vec![ModelId::from("qwen:8b")]
        );

        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        let entry = json["entries"]["ollama"].clone();
        json["entries"]["broken"] = entry;
        std::fs::write(&path, serde_json::to_vec(&json).expect("serialize")).expect("corrupt");
        let loaded = ProviderModelCache::load_isolated(&path).expect("load isolated");
        assert_eq!(loaded.isolated_entries.len(), 1);
        assert!(loaded.cache.get(&ProviderId::from("ollama")).is_some());
        assert!(loaded.cache.get(&ProviderId::from("broken")).is_none());
    }
}
