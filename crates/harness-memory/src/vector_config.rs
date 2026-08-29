use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::MemoryError;

pub const VECTOR_CONFIG_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_VECTOR_PROVIDER: &str = "default";
pub const DEFAULT_VECTOR_CREDENTIAL_ID: &str = "vector:default";
const MAX_VECTOR_CONFIG_BYTES: u64 = 64 * 1024;

/// 每个项目只允许一个 Embedding Provider；Key 仍只存在 OS Credential Store。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct VectorProviderConfig {
    pub schema_version: u32,
    pub endpoint: String,
    pub model: String,
    pub credential_id: String,
    pub dimensions: usize,
    pub verified_at_millis: i64,
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
            verified_at_millis,
        };
        config.validate()?;
        Ok(config)
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
        if self.schema_version != VECTOR_CONFIG_SCHEMA_VERSION {
            return Err(MemoryError::new(
                "vector-config-schema-unsupported",
                self.schema_version.to_string(),
            ));
        }
        if self.endpoint.is_empty()
            || self.model.is_empty()
            || self.credential_id != DEFAULT_VECTOR_CREDENTIAL_ID
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
}
