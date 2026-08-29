use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{StreamingHttpRequest, StreamingHttpTransport};
use serde::{Deserialize, Serialize};

use crate::{EmbeddingProfile, EmbeddingProvider, EmbeddingProviderFactory, MemoryError};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingDimensionField {
    #[default]
    Dimensions,
    OutputDimension,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingRequestDialect {
    #[default]
    OpenAi,
    Voyage,
    Jina,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpEmbeddingConfig {
    pub provider: String,
    pub endpoint: String,
    pub credential_id: Option<String>,
    /// 固定维度模型为 false；只有显式协商可变维度时才发送 dimensions。
    pub send_dimensions: bool,
    pub dimension_field: EmbeddingDimensionField,
    pub request_dialect: EmbeddingRequestDialect,
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

    /// 配置向导的一次性可用性验证；不创建向量表或 generation。
    pub fn probe(&self, model: &str, text: &str) -> Result<Vec<f32>, MemoryError> {
        let model = model.trim();
        if model.is_empty() {
            return Err(MemoryError::new("embedding-model-empty", "model"));
        }
        request_embedding(
            &self.config,
            self.credentials.as_ref(),
            self.transport.as_ref(),
            model,
            None,
            text,
        )
    }

    /// 自动探测失败后的显式维度验证；Provider 必须返回完全相同的向量长度。
    pub fn probe_with_dimensions(
        &self,
        model: &str,
        dimensions: usize,
        text: &str,
    ) -> Result<Vec<f32>, MemoryError> {
        let model = model.trim();
        if model.is_empty() {
            return Err(MemoryError::new("embedding-model-empty", "model"));
        }
        if !(1..=65_536).contains(&dimensions) {
            return Err(MemoryError::new(
                "embedding-dimensions-invalid",
                dimensions.to_string(),
            ));
        }
        let vector = request_embedding(
            &self.config,
            self.credentials.as_ref(),
            self.transport.as_ref(),
            model,
            Some(dimensions),
            text,
        )?;
        if vector.len() != dimensions {
            return Err(MemoryError::new(
                "embedding-dimensions-mismatch",
                format!("requested={dimensions}, actual={}", vector.len()),
            ));
        }
        Ok(vector)
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
        request_embedding(
            &self.config,
            self.credentials.as_ref(),
            self.transport.as_ref(),
            &self.profile.model,
            self.config
                .send_dimensions
                .then_some(self.profile.dimensions),
            text,
        )
    }
}

fn request_embedding(
    config: &HttpEmbeddingConfig,
    credentials: &dyn CredentialStore,
    transport: &dyn StreamingHttpTransport,
    model: &str,
    dimensions: Option<usize>,
    text: &str,
) -> Result<Vec<f32>, MemoryError> {
    let input = text.trim();
    if input.is_empty() {
        return Err(MemoryError::new("embedding-input-empty", "input"));
    }
    let mut body = serde_json::json!({
        "input": [input],
        "model": model
    });
    match config.request_dialect {
        EmbeddingRequestDialect::OpenAi => {
            body["encoding_format"] = serde_json::json!("float");
        }
        EmbeddingRequestDialect::Voyage => {
            body["output_dtype"] = serde_json::json!("float");
        }
        EmbeddingRequestDialect::Jina => {
            body["normalized"] = serde_json::json!(true);
            body["embedding_type"] = serde_json::json!("float");
        }
    }
    if let Some(dimensions) = dimensions {
        let field = match config.dimension_field {
            EmbeddingDimensionField::Dimensions => "dimensions",
            EmbeddingDimensionField::OutputDimension => "output_dimension",
        };
        body[field] = serde_json::json!(dimensions);
    }
    let mut request = StreamingHttpRequest::json(
        config.endpoint.clone(),
        body,
        Duration::from_millis(config.timeout_millis.unwrap_or(30_000)),
    )
    .with_header("Accept", "application/json");
    if let Some(id) = &config.credential_id {
        let secret = credentials
            .get(&CredentialId::new(id.clone()))
            .map_err(|e| MemoryError::new(e.code, e.message))?
            .ok_or_else(|| MemoryError::new("embedding-credential-missing", id))?;
        let bearer = secret
            .expose_secret()
            .map(|value| SecretString::new(format!("Bearer {value}")))
            .map_err(|e| MemoryError::new(e.code, e.message))?;
        request = request.with_sensitive_header("Authorization", bearer);
    }
    let response = transport
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
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return Err(MemoryError::new(
            "embedding-vector-invalid",
            vector.len().to_string(),
        ));
    }
    Ok(vector)
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
