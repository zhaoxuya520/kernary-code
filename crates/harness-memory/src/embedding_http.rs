use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{StreamingHttpRequest, StreamingHttpTransport};
use serde::{Deserialize, Serialize};

use crate::{EmbeddingProfile, EmbeddingProvider, EmbeddingProviderFactory, MemoryError};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpEmbeddingConfig {
    pub provider: String,
    pub endpoint: String,
    pub credential_id: Option<String>,
    pub allow_remote_project_private: bool,
    pub timeout_millis: Option<u64>,
}

pub struct HttpEmbeddingFactory {
    config: HttpEmbeddingConfig,
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
}
impl HttpEmbeddingFactory {
    pub fn new(
        config: HttpEmbeddingConfig,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Result<Self, MemoryError> {
        validate_endpoint(&config.endpoint)?;
        let remote = !is_loopback(&config.endpoint);
        if remote && !config.allow_remote_project_private {
            return Err(MemoryError::new(
                "embedding-egress-not-approved",
                "remote project-private embedding requires explicit approval",
            ));
        }
        Ok(Self {
            config,
            credentials,
            transport,
        })
    }
}
impl EmbeddingProviderFactory for HttpEmbeddingFactory {
    fn create(
        &self,
        profile: &EmbeddingProfile,
    ) -> Result<Arc<dyn EmbeddingProvider>, MemoryError> {
        if profile.provider != self.config.provider {
            return Err(MemoryError::new(
                "embedding-provider-mismatch",
                format!("{} != {}", profile.provider, self.config.provider),
            ));
        }
        Ok(Arc::new(HttpEmbeddingProvider {
            profile: profile.clone(),
            config: self.config.clone(),
            credentials: self.credentials.clone(),
            transport: self.transport.clone(),
        }))
    }
}
struct HttpEmbeddingProvider {
    profile: EmbeddingProfile,
    config: HttpEmbeddingConfig,
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
}
impl EmbeddingProvider for HttpEmbeddingProvider {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let input = text.trim();
        if input.is_empty() {
            return Err(MemoryError::new("embedding-input-empty", "input"));
        }
        let mut request=StreamingHttpRequest::json(self.config.endpoint.clone(),serde_json::json!({"input":input,"model":self.profile.model,"dimensions":self.profile.dimensions,"encoding_format":"float"}),Duration::from_millis(self.config.timeout_millis.unwrap_or(30_000))).with_header("Accept","application/json");
        if let Some(id) = &self.config.credential_id {
            let secret = self
                .credentials
                .get(&CredentialId::new(id.clone()))
                .map_err(|e| MemoryError::new(e.code, e.message))?
                .ok_or_else(|| MemoryError::new("embedding-credential-missing", id))?;
            let bearer = secret
                .expose_secret()
                .map(|value| SecretString::new(format!("Bearer {value}")))
                .map_err(|e| MemoryError::new(e.code, e.message))?;
            request = request.with_sensitive_header("Authorization", bearer);
        }
        let response = self
            .transport
            .send(request)
            .map_err(|e| MemoryError::new(e.code, e.message))?;
        if !(200..300).contains(&response.status) {
            return Err(MemoryError::new(
                "embedding-http-status",
                response.status.to_string(),
            ));
        }
        let mut bytes = Vec::new();
        response
            .body
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| MemoryError::new("embedding-http-read", e.to_string()))?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(MemoryError::new(
                "embedding-response-too-large",
                bytes.len().to_string(),
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| MemoryError::new("embedding-response-json", e.to_string()))?;
        let data = value
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| MemoryError::new("embedding-data-missing", "data"))?;
        if data.len() != 1 {
            return Err(MemoryError::new(
                "embedding-data-count",
                data.len().to_string(),
            ));
        }
        let vector = data[0]
            .get("embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| MemoryError::new("embedding-vector-missing", "embedding"))?
            .iter()
            .map(|value| {
                value
                    .as_f64()
                    .map(|v| v as f32)
                    .ok_or_else(|| MemoryError::new("embedding-vector-value", value.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(vector)
    }
}
fn validate_endpoint(endpoint: &str) -> Result<(), MemoryError> {
    if !(endpoint.starts_with("https://") || is_loopback(endpoint)) {
        return Err(MemoryError::new("embedding-endpoint-insecure", endpoint));
    }
    if endpoint.contains('@') || endpoint.contains('#') {
        return Err(MemoryError::new("embedding-endpoint-invalid", endpoint));
    }
    Ok(())
}
fn is_loopback(endpoint: &str) -> bool {
    endpoint.starts_with("http://127.0.0.1")
        || endpoint.starts_with("http://localhost")
        || endpoint.starts_with("http://[::1]")
}
