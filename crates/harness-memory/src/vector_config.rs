use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::MemoryError;

pub const VECTOR_CONFIG_SCHEMA_VERSION: u32 = 1;
pub const VECTOR_CATALOG_SCHEMA_VERSION: u32 = 2;
pub const DEFAULT_VECTOR_PROVIDER: &str = "default";
pub const DEFAULT_VECTOR_CREDENTIAL_ID: &str = "vector:default";
const MAX_VECTOR_CONFIG_BYTES: u64 = 64 * 1024;

/// 维度协商模式：固定维度模型不发送 dimensions，可变维度模型显式发送用户选择。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VectorDimensionMode {
    AutoDetected,
    /// 兼容旧配置：旧版运行时始终发送 dimensions。
    #[default]
    Requested,
}

/// 旧版单 Provider 配置；schema v2 加载时会迁移进 `VectorCatalogConfig`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct VectorProviderConfig {
    pub schema_version: u32,
    pub endpoint: String,
    pub model: String,
    pub credential_id: String,
    pub dimensions: usize,
    #[serde(default)]
    pub dimension_mode: VectorDimensionMode,
    pub verified_at_millis: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VectorProviderKind {
    Voyage,
    Jina,
    Custom,
    Legacy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct VectorModelConfig {
    pub id: String,
    pub dimensions: Option<usize>,
    pub dimension_mode: Option<VectorDimensionMode>,
    pub verified_at_millis: Option<i64>,
}

impl VectorModelConfig {
    #[must_use]
    pub fn unverified(id: impl Into<String>) -> Self {
        Self {
            id: id.into().trim().to_owned(),
            dimensions: None,
            dimension_mode: None,
            verified_at_millis: None,
        }
    }

    #[must_use]
    pub fn verified(
        id: impl Into<String>,
        dimensions: usize,
        dimension_mode: VectorDimensionMode,
        verified_at_millis: i64,
    ) -> Self {
        Self {
            id: id.into().trim().to_owned(),
            dimensions: Some(dimensions),
            dimension_mode: Some(dimension_mode),
            verified_at_millis: Some(verified_at_millis),
        }
    }

    pub fn mark_verified(
        &mut self,
        dimensions: usize,
        dimension_mode: VectorDimensionMode,
        verified_at_millis: i64,
    ) -> Result<(), MemoryError> {
        if !(1..=65_536).contains(&dimensions) {
            return Err(MemoryError::new(
                "vector-model-dimensions-invalid",
                dimensions.to_string(),
            ));
        }
        self.dimensions = Some(dimensions);
        self.dimension_mode = Some(dimension_mode);
        self.verified_at_millis = Some(verified_at_millis);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct VectorProviderDefinition {
    pub id: String,
    pub display_name: String,
    pub kind: VectorProviderKind,
    pub endpoint: String,
    pub credential_id: String,
    pub models: Vec<VectorModelConfig>,
    pub active_model: Option<String>,
}

impl VectorProviderDefinition {
    pub fn model(&self, id: &str) -> Option<&VectorModelConfig> {
        self.models.iter().find(|model| model.id == id)
    }

    pub fn model_mut(&mut self, id: &str) -> Option<&mut VectorModelConfig> {
        self.models.iter_mut().find(|model| model.id == id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct VectorCatalogConfig {
    pub schema_version: u32,
    pub active_provider: Option<String>,
    pub providers: Vec<VectorProviderDefinition>,
}

impl Default for VectorCatalogConfig {
    fn default() -> Self {
        Self {
            schema_version: VECTOR_CATALOG_SCHEMA_VERSION,
            active_provider: None,
            providers: Vec::new(),
        }
    }
}

impl VectorCatalogConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| MemoryError::new("vector-config-io", error.to_string()))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_VECTOR_CONFIG_BYTES {
            return Err(MemoryError::new(
                "vector-config-size-or-type",
                path.display().to_string(),
            ));
        }
        let source = fs::read_to_string(path)
            .map_err(|error| MemoryError::new("vector-config-io", error.to_string()))?;
        let value: toml::Value = toml::from_str(&source)
            .map_err(|error| MemoryError::new("vector-config-toml", error.to_string()))?;
        let schema_version = value
            .get("schema_version")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                MemoryError::new("vector-config-schema-missing", path.display().to_string())
            })?;
        let catalog = match schema_version {
            VECTOR_CATALOG_SCHEMA_VERSION => toml::from_str::<Self>(&source)
                .map_err(|error| MemoryError::new("vector-config-toml", error.to_string()))?,
            VECTOR_CONFIG_SCHEMA_VERSION => {
                let legacy = toml::from_str::<VectorProviderConfig>(&source)
                    .map_err(|error| MemoryError::new("vector-config-toml", error.to_string()))?;
                legacy.validate()?;
                let model = VectorModelConfig::verified(
                    legacy.model.clone(),
                    legacy.dimensions,
                    legacy.dimension_mode,
                    legacy.verified_at_millis,
                );
                Self {
                    schema_version: VECTOR_CATALOG_SCHEMA_VERSION,
                    active_provider: Some("custom-legacy".to_owned()),
                    providers: vec![VectorProviderDefinition {
                        id: "custom-legacy".to_owned(),
                        display_name: "Migrated Vector Provider".to_owned(),
                        kind: VectorProviderKind::Legacy,
                        endpoint: legacy.endpoint,
                        credential_id: legacy.credential_id,
                        models: vec![model],
                        active_model: Some(legacy.model),
                    }],
                }
            }
            _ => {
                return Err(MemoryError::new(
                    "vector-config-schema-unsupported",
                    schema_version.to_string(),
                ));
            }
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), MemoryError> {
        self.validate()?;
        let mut bytes = toml::to_string_pretty(self)
            .map_err(|error| MemoryError::new("vector-config-toml", error.to_string()))?
            .into_bytes();
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_VECTOR_CONFIG_BYTES {
            return Err(MemoryError::new(
                "vector-config-too-large",
                bytes.len().to_string(),
            ));
        }
        atomic_write(path.as_ref(), &bytes)
    }

    pub fn provider(&self, id: &str) -> Option<&VectorProviderDefinition> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub fn provider_mut(&mut self, id: &str) -> Option<&mut VectorProviderDefinition> {
        self.providers.iter_mut().find(|provider| provider.id == id)
    }

    pub fn upsert_provider(
        &mut self,
        provider: VectorProviderDefinition,
    ) -> Result<(), MemoryError> {
        validate_provider(&provider)?;
        if let Some(existing) = self.provider_mut(&provider.id) {
            *existing = provider;
        } else {
            if self.providers.len() >= 32 {
                return Err(MemoryError::new("vector-provider-limit", "32"));
            }
            self.providers.push(provider);
            self.providers.sort_by(|left, right| left.id.cmp(&right.id));
        }
        Ok(())
    }

    pub fn activate(&mut self, provider_id: &str, model_id: &str) -> Result<(), MemoryError> {
        let provider = self
            .provider_mut(provider_id)
            .ok_or_else(|| MemoryError::new("vector-provider-missing", provider_id))?;
        let model = provider.model(model_id).ok_or_else(|| {
            MemoryError::new("vector-model-missing", format!("{provider_id}/{model_id}"))
        })?;
        if model.dimensions.is_none() || model.dimension_mode.is_none() {
            return Err(MemoryError::new(
                "vector-model-unverified",
                format!("{provider_id}/{model_id}"),
            ));
        }
        provider.active_model = Some(model_id.to_owned());
        self.active_provider = Some(provider_id.to_owned());
        Ok(())
    }

    pub fn resolved_active(&self) -> Result<Option<(String, VectorProviderConfig)>, MemoryError> {
        let Some(provider_id) = self.active_provider.as_deref() else {
            return Ok(None);
        };
        let provider = self
            .provider(provider_id)
            .ok_or_else(|| MemoryError::new("vector-active-provider-missing", provider_id))?;
        let model_id = provider
            .active_model
            .as_deref()
            .ok_or_else(|| MemoryError::new("vector-active-model-missing", provider_id))?;
        let model = provider.model(model_id).ok_or_else(|| {
            MemoryError::new(
                "vector-active-model-missing",
                format!("{provider_id}/{model_id}"),
            )
        })?;
        let dimensions = model.dimensions.ok_or_else(|| {
            MemoryError::new(
                "vector-model-unverified",
                format!("{provider_id}/{model_id}"),
            )
        })?;
        let dimension_mode = model.dimension_mode.ok_or_else(|| {
            MemoryError::new(
                "vector-model-unverified",
                format!("{provider_id}/{model_id}"),
            )
        })?;
        let mut config = VectorProviderConfig::new_with_credential(
            provider.endpoint.clone(),
            model.id.clone(),
            provider.credential_id.clone(),
            dimensions,
            dimension_mode,
            model.verified_at_millis.unwrap_or_default(),
        )?;
        config.schema_version = VECTOR_CONFIG_SCHEMA_VERSION;
        Ok(Some((provider.id.clone(), config)))
    }

    fn validate(&self) -> Result<(), MemoryError> {
        if self.schema_version != VECTOR_CATALOG_SCHEMA_VERSION || self.providers.len() > 32 {
            return Err(MemoryError::new(
                "vector-catalog-invalid",
                "schema/providers",
            ));
        }
        let mut provider_ids = std::collections::BTreeSet::new();
        for provider in &self.providers {
            validate_provider(provider)?;
            if !provider_ids.insert(provider.id.as_str()) {
                return Err(MemoryError::new(
                    "vector-provider-duplicate",
                    provider.id.clone(),
                ));
            }
        }
        if let Some(active) = self.active_provider.as_deref()
            && self.provider(active).is_none()
        {
            return Err(MemoryError::new("vector-active-provider-missing", active));
        }
        Ok(())
    }
}

fn validate_provider(provider: &VectorProviderDefinition) -> Result<(), MemoryError> {
    if !valid_identifier(&provider.id)
        || provider.display_name.trim().is_empty()
        || provider.display_name.len() > 128
        || provider.endpoint.trim().is_empty()
        || provider.credential_id.trim().is_empty()
        || provider.models.is_empty()
        || provider.models.len() > 128
    {
        return Err(MemoryError::new(
            "vector-provider-invalid",
            provider.id.clone(),
        ));
    }
    let mut model_ids = std::collections::BTreeSet::new();
    for model in &provider.models {
        if model.id.is_empty()
            || model.id.len() > 256
            || model.id.chars().any(char::is_whitespace)
            || !model_ids.insert(model.id.as_str())
            || model
                .dimensions
                .is_some_and(|value| !(1..=65_536).contains(&value))
            || model.dimensions.is_some() != model.dimension_mode.is_some()
            || model.dimensions.is_some() != model.verified_at_millis.is_some()
        {
            return Err(MemoryError::new(
                "vector-model-invalid",
                format!("{}/{}", provider.id, model.id),
            ));
        }
    }
    if let Some(active) = provider.active_model.as_deref()
        && !provider.model(active).is_some_and(|model| {
            model.dimensions.is_some()
                && model.dimension_mode.is_some()
                && model.verified_at_millis.is_some()
        })
    {
        return Err(MemoryError::new(
            "vector-active-model-missing-or-unverified",
            format!("{}/{}", provider.id, active),
        ));
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

impl VectorProviderConfig {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        dimensions: usize,
        verified_at_millis: i64,
    ) -> Result<Self, MemoryError> {
        let config = Self {
            schema_version: VECTOR_CONFIG_SCHEMA_VERSION,
            endpoint: endpoint.into().trim().to_owned(),
            model: model.into().trim().to_owned(),
            credential_id: DEFAULT_VECTOR_CREDENTIAL_ID.to_owned(),
            dimensions,
            dimension_mode: VectorDimensionMode::AutoDetected,
            verified_at_millis,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn new_requested(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        dimensions: usize,
        verified_at_millis: i64,
    ) -> Result<Self, MemoryError> {
        let mut config = Self::new(endpoint, model, dimensions, verified_at_millis)?;
        config.dimension_mode = VectorDimensionMode::Requested;
        Ok(config)
    }

    pub fn new_with_credential(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        credential_id: impl Into<String>,
        dimensions: usize,
        dimension_mode: VectorDimensionMode,
        verified_at_millis: i64,
    ) -> Result<Self, MemoryError> {
        let config = Self {
            schema_version: VECTOR_CONFIG_SCHEMA_VERSION,
            endpoint: endpoint.into().trim().to_owned(),
            model: model.into().trim().to_owned(),
            credential_id: credential_id.into().trim().to_owned(),
            dimensions,
            dimension_mode,
            verified_at_millis,
        };
        config.validate_with_any_credential()?;
        Ok(config)
    }

    #[must_use]
    pub const fn sends_dimensions(&self) -> bool {
        matches!(self.dimension_mode, VectorDimensionMode::Requested)
    }

    pub fn refresh_detected_dimensions(
        &mut self,
        dimensions: usize,
        verified_at_millis: i64,
    ) -> Result<(), MemoryError> {
        self.dimensions = dimensions;
        self.verified_at_millis = verified_at_millis;
        self.validate_with_any_credential()
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Option<Self>, MemoryError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| MemoryError::new("vector-config-io", error.to_string()))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_VECTOR_CONFIG_BYTES {
            return Err(MemoryError::new(
                "vector-config-size-or-type",
                path.display().to_string(),
            ));
        }
        let source = fs::read_to_string(path)
            .map_err(|error| MemoryError::new("vector-config-io", error.to_string()))?;
        let config: Self = toml::from_str(&source)
            .map_err(|error| MemoryError::new("vector-config-toml", error.to_string()))?;
        config.validate()?;
        Ok(Some(config))
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), MemoryError> {
        self.validate()?;
        let mut bytes = toml::to_string_pretty(self)
            .map_err(|error| MemoryError::new("vector-config-toml", error.to_string()))?
            .into_bytes();
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_VECTOR_CONFIG_BYTES {
            return Err(MemoryError::new(
                "vector-config-too-large",
                bytes.len().to_string(),
            ));
        }
        atomic_write(path.as_ref(), &bytes)
    }

    fn validate(&self) -> Result<(), MemoryError> {
        self.validate_with_any_credential()?;
        if self.credential_id != DEFAULT_VECTOR_CREDENTIAL_ID {
            return Err(MemoryError::new(
                "vector-config-invalid",
                "legacy credential",
            ));
        }
        Ok(())
    }

    fn validate_with_any_credential(&self) -> Result<(), MemoryError> {
        if self.schema_version != VECTOR_CONFIG_SCHEMA_VERSION {
            return Err(MemoryError::new(
                "vector-config-schema-unsupported",
                self.schema_version.to_string(),
            ));
        }
        if self.endpoint.is_empty()
            || self.model.is_empty()
            || self.credential_id.is_empty()
            || self.dimensions == 0
            || self.dimensions > 65_536
        {
            return Err(MemoryError::new(
                "vector-config-invalid",
                "endpoint/model/credential/dimensions",
            ));
        }
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    let parent = path.parent().ok_or_else(|| {
        MemoryError::new("vector-config-parent-missing", path.display().to_string())
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| MemoryError::new("vector-config-io", error.to_string()))?;
    if path.exists()
        && !fs::symlink_metadata(path)
            .map_err(|error| MemoryError::new("vector-config-io", error.to_string()))?
            .file_type()
            .is_file()
    {
        return Err(MemoryError::new(
            "vector-config-target-type",
            path.display().to_string(),
        ));
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let backup = path.with_extension(format!("{}.previous", std::process::id()));
    if temporary.exists() || backup.exists() {
        return Err(MemoryError::new(
            "vector-config-staging-exists",
            path.display().to_string(),
        ));
    }
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| MemoryError::new("vector-config-io", error.to_string()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| MemoryError::new("vector-config-io", error.to_string()))?;
        if path.exists() {
            fs::rename(path, &backup)
                .map_err(|error| MemoryError::new("vector-config-backup", error.to_string()))?;
        }
        if let Err(error) = fs::rename(&temporary, path) {
            if backup.exists() {
                let _ = fs::rename(&backup, path);
            }
            return Err(MemoryError::new("vector-config-commit", error.to_string()));
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn single_provider_config_round_trips_without_secret() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("kernary.vector.toml");
        let config = VectorProviderConfig::new(
            "https://relay.example/v1/embeddings",
            "embedding-model",
            1_024,
            42,
        )
        .expect("config");
        config.save(&path).expect("save");
        assert_eq!(
            VectorProviderConfig::load(&path).expect("load"),
            Some(config)
        );
        let source = fs::read_to_string(path).expect("source");
        assert!(!source.to_ascii_lowercase().contains("api_key"));
    }

    #[test]
    fn requested_and_detected_dimensions_have_distinct_wire_contracts() {
        let detected =
            VectorProviderConfig::new("https://relay.example/v1/embeddings", "fixed-model", 768, 1)
                .expect("detected");
        let requested = VectorProviderConfig::new_requested(
            "https://relay.example/v1/embeddings",
            "variable-model",
            1_024,
            2,
        )
        .expect("requested");
        assert!(!detected.sends_dimensions());
        assert!(requested.sends_dimensions());
    }

    #[test]
    fn legacy_config_without_dimension_mode_preserves_requested_wire_behavior() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("legacy-vector.toml");
        fs::write(
            &path,
            "schema_version = 1\nendpoint = \"https://relay.example/v1/embeddings\"\nmodel = \"legacy\"\ncredential_id = \"vector:default\"\ndimensions = 1536\nverified_at_millis = 1\n",
        )
        .expect("legacy config");
        let loaded = VectorProviderConfig::load(path)
            .expect("load")
            .expect("config");
        assert_eq!(loaded.dimension_mode, VectorDimensionMode::Requested);
        assert!(loaded.sends_dimensions());
    }

    #[test]
    fn catalog_round_trips_multiple_named_providers_and_switches_active_model() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("vector-catalog.toml");
        let mut catalog = VectorCatalogConfig::default();
        for (id, display_name, kind, credential, model) in [
            (
                "voyage",
                "Voyage AI",
                VectorProviderKind::Voyage,
                "vector:voyage",
                "voyage-4-lite",
            ),
            (
                "jina",
                "Jina AI",
                VectorProviderKind::Jina,
                "vector:jina",
                "jina-embeddings-v5-text-small",
            ),
        ] {
            catalog
                .upsert_provider(VectorProviderDefinition {
                    id: id.to_owned(),
                    display_name: display_name.to_owned(),
                    kind,
                    endpoint: format!("https://{id}.example/v1/embeddings"),
                    credential_id: credential.to_owned(),
                    models: vec![VectorModelConfig::verified(
                        model,
                        1_024,
                        VectorDimensionMode::AutoDetected,
                        42,
                    )],
                    active_model: None,
                })
                .expect("provider");
        }
        catalog
            .activate("jina", "jina-embeddings-v5-text-small")
            .expect("activate");
        catalog.save(&path).expect("save");
        let loaded = VectorCatalogConfig::load(&path).expect("load");
        assert_eq!(loaded, catalog);
        let (provider, active) = loaded.resolved_active().expect("resolve").expect("active");
        assert_eq!(provider, "jina");
        assert_eq!(active.model, "jina-embeddings-v5-text-small");
        assert_eq!(active.credential_id, "vector:jina");
    }

    #[test]
    fn legacy_single_provider_migrates_into_named_catalog() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("legacy-vector.toml");
        fs::write(
            &path,
            "schema_version = 1\nendpoint = \"https://relay.example/v1/embeddings\"\nmodel = \"legacy-embedding\"\ncredential_id = \"vector:default\"\ndimensions = 1536\nverified_at_millis = 7\n",
        )
        .expect("legacy config");
        let catalog = VectorCatalogConfig::load(path).expect("migrate");
        assert_eq!(catalog.schema_version, VECTOR_CATALOG_SCHEMA_VERSION);
        assert_eq!(catalog.active_provider.as_deref(), Some("custom-legacy"));
        let (_, active) = catalog.resolved_active().expect("resolve").expect("active");
        assert_eq!(active.model, "legacy-embedding");
        assert_eq!(active.dimension_mode, VectorDimensionMode::Requested);
    }

    #[test]
    fn catalog_refuses_to_activate_unverified_model() {
        let mut catalog = VectorCatalogConfig::default();
        catalog
            .upsert_provider(VectorProviderDefinition {
                id: "custom-example".to_owned(),
                display_name: "Example".to_owned(),
                kind: VectorProviderKind::Custom,
                endpoint: "https://example.test/v1/embeddings".to_owned(),
                credential_id: "vector:custom-example".to_owned(),
                models: vec![VectorModelConfig::unverified("chat-model")],
                active_model: None,
            })
            .expect("provider");
        let error = catalog
            .activate("custom-example", "chat-model")
            .expect_err("unverified model must fail");
        assert_eq!(error.code, "vector-model-unverified");
        assert!(catalog.active_provider.is_none());
    }
}
