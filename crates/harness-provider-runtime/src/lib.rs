#![forbid(unsafe_code)]

//! Provider Catalog 的运行时组合、显式模型发现与版本化缓存。
//!
//! 构造 Provider 时只读本地 metadata/cache；只有 `ModelProvider::refresh` 才会联网。

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{StreamingHttpRequest, StreamingHttpTransport, UreqStreamingTransport};
use harness_model::{
    CancellationToken, ModelCapability, ModelError, ModelErrorKind, ModelEventStream,
    ModelProvider, ModelRegistry, ModelRequest, ProtocolMuxProvider,
};
use harness_provider_anthropic::{AnthropicCompatibleConfig, AnthropicProvider};
use harness_provider_catalog::{
    ProviderCatalog, ProviderDefinition, ProviderDiscoveryAuth, ProviderDiscoveryDefinition,
    ProviderDiscoveryFormat, ProviderModelCache, ProviderModelCacheEntry, ProviderProtocol,
    merge_discovered_models, provider_discovery_fingerprint, provider_with_cached_models,
};
use harness_provider_compatible::{
    CompatibleProvider, CompatibleProviderConfig, CompatibleReasoningField,
};
use harness_provider_openai::{OpenAiCompatibleResponsesConfig, OpenAiResponsesProvider};
use harness_types::{ModelId, ProviderId, ReasoningLevel};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DISCOVERY_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DISCOVERED_MODELS: usize = 5_000;

#[derive(Clone)]
pub struct CatalogProviderRuntime {
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
    cache: Arc<SharedCache>,
    timeout: Duration,
    isolated_cache_entries: Arc<Vec<String>>,
}

struct SharedCache {
    path: PathBuf,
    value: Mutex<ProviderModelCache>,
}

impl CatalogProviderRuntime {
    pub fn with_ureq(
        cache_path: impl Into<PathBuf>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, ModelError> {
        Self::new(
            cache_path,
            credentials,
            Arc::new(UreqStreamingTransport::default()),
            DEFAULT_DISCOVERY_TIMEOUT,
        )
    }

    pub fn new(
        cache_path: impl Into<PathBuf>,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
        timeout: Duration,
    ) -> Result<Self, ModelError> {
        if timeout.is_zero() {
            return Err(model_error(
                ModelErrorKind::InvalidRequest,
                "provider-discovery-timeout-zero",
                "模型发现 timeout 必须大于 0",
            ));
        }
        let cache_path = cache_path.into();
        let loaded = ProviderModelCache::load_isolated(&cache_path).map_err(catalog_error)?;
        Ok(Self {
            credentials,
            transport,
            cache: Arc::new(SharedCache {
                path: cache_path,
                value: Mutex::new(loaded.cache),
            }),
            timeout,
            isolated_cache_entries: Arc::new(loaded.isolated_entries),
        })
    }

    #[must_use]
    pub fn isolated_cache_entries(&self) -> &[String] {
        self.isolated_cache_entries.as_slice()
    }

    pub fn register_all(
        &self,
        registry: &mut ModelRegistry,
        catalog: &ProviderCatalog,
    ) -> Result<(), ModelError> {
        for provider in catalog.list() {
            if registry.provider(&provider.id).is_some() {
                continue;
            }
            registry.register(self.build(provider)?).map_err(|error| {
                model_error(ModelErrorKind::InvalidRequest, error.code, error.message)
            })?;
        }
        Ok(())
    }

    pub fn build(
        &self,
        definition: ProviderDefinition,
    ) -> Result<Arc<dyn ModelProvider>, ModelError> {
        let cache = self
            .cache
            .value
            .lock()
            .map_err(|_| cache_poisoned())?
            .clone();
        let effective = provider_with_cached_models(&definition, &cache).map_err(catalog_error)?;
        let inner =
            build_protocol_provider(&effective, self.credentials.clone(), self.transport.clone())?;
        Ok(Arc::new(RefreshableCatalogProvider {
            snapshot: definition,
            inner,
            runtime: self.clone(),
        }))
    }

    /// 向导专用的一次性模型目录验证；不修改缓存或 Registry。
    pub fn discover_models(
        &self,
        provider: &ProviderDefinition,
    ) -> Result<Vec<ModelId>, ModelError> {
        let discovery = provider.discovery.as_ref().ok_or_else(|| {
            model_error(
                ModelErrorKind::InvalidRequest,
                "provider-discovery-not-configured",
                provider.id.to_string(),
            )
        })?;
        self.discover(provider, discovery)
            .map(|response| response.models)
    }

    fn refresh_provider(
        &self,
        snapshot: &ProviderDefinition,
    ) -> Result<Arc<dyn ModelProvider>, ModelError> {
        let discovery = snapshot.discovery.as_ref().ok_or_else(|| {
            model_error(
                ModelErrorKind::InvalidRequest,
                "provider-discovery-not-configured",
                snapshot.id.to_string(),
            )
        })?;
        let response = self.discover(snapshot, discovery)?;
        let (effective, routable_models) =
            merge_discovered_models(snapshot, &response.models).map_err(catalog_error)?;
        let inner =
            build_protocol_provider(&effective, self.credentials.clone(), self.transport.clone())?;
        let entry = ProviderModelCacheEntry {
            provider_id: snapshot.id.clone(),
            endpoint_fingerprint: provider_discovery_fingerprint(snapshot),
            fetched_at_millis: unix_millis()?,
            response_sha256: response.sha256,
            discovered_models: response.models,
            routable_models,
        };
        self.commit_cache(entry)?;
        Ok(Arc::new(RefreshableCatalogProvider {
            snapshot: snapshot.clone(),
            inner,
            runtime: self.clone(),
        }))
    }

    fn commit_cache(&self, entry: ProviderModelCacheEntry) -> Result<(), ModelError> {
        let mut guard = self.cache.value.lock().map_err(|_| cache_poisoned())?;
        let mut candidate = guard.clone();
        candidate.upsert(entry).map_err(catalog_error)?;
        candidate.save(&self.cache.path).map_err(catalog_error)?;
        *guard = candidate;
        Ok(())
    }

    fn discover(
        &self,
        provider: &ProviderDefinition,
        discovery: &ProviderDiscoveryDefinition,
    ) -> Result<DiscoveryResponse, ModelError> {
        let endpoint = if discovery.format == ProviderDiscoveryFormat::AnthropicModels {
            format!("{}?limit=1000", discovery.endpoint)
        } else {
            discovery.endpoint.clone()
        };
        let mut request = StreamingHttpRequest::get(endpoint, self.timeout)
            .with_header("Accept", "application/json");
        match discovery.auth {
            ProviderDiscoveryAuth::None => {}
            ProviderDiscoveryAuth::Bearer => {
                let secret = provider_secret(provider, self.credentials.as_ref())?;
                let value = secret.expose_secret().map_err(auth_error)?;
                request = request.with_sensitive_header(
                    "Authorization",
                    SecretString::new(format!("Bearer {value}")),
                );
            }
            ProviderDiscoveryAuth::AnthropicApiKey => {
                let secret = provider_secret(provider, self.credentials.as_ref())?;
                request = request
                    .with_header("anthropic-version", "2023-06-01")
                    .with_sensitive_header("x-api-key", secret);
            }
        }
        let mut response = self.transport.send(request).map_err(|error| {
            model_error(
                if error.timeout {
                    ModelErrorKind::Timeout
                } else {
                    ModelErrorKind::Transport
                },
                "provider-discovery-transport",
                error.code,
            )
        })?;
        if !(200..300).contains(&response.status) {
            let kind = match response.status {
                401 | 403 => ModelErrorKind::Auth,
                429 => ModelErrorKind::RateLimit,
                408 | 504 => ModelErrorKind::Timeout,
                _ if response.status >= 500 => ModelErrorKind::Transport,
                _ => ModelErrorKind::Protocol,
            };
            return Err(model_error(
                kind,
                "provider-discovery-http",
                response.status.to_string(),
            ));
        }
        let mut limited = response
            .body
            .by_ref()
            .take(MAX_DISCOVERY_RESPONSE_BYTES + 1);
        let mut bytes = Vec::new();
        limited.read_to_end(&mut bytes).map_err(|error| {
            model_error(
                ModelErrorKind::Protocol,
                "provider-discovery-read",
                error.to_string(),
            )
        })?;
        if bytes.len() as u64 > MAX_DISCOVERY_RESPONSE_BYTES {
            return Err(model_error(
                ModelErrorKind::Protocol,
                "provider-discovery-response-too-large",
                bytes.len().to_string(),
            ));
        }
        let mut models = parse_models(discovery.format, &bytes)?;
        models.sort();
        models.dedup();
        if models.is_empty() || models.len() > MAX_DISCOVERED_MODELS {
            return Err(model_error(
                ModelErrorKind::Protocol,
                "provider-discovery-model-count",
                models.len().to_string(),
            ));
        }
        Ok(DiscoveryResponse {
            models,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        })
    }
}

struct RefreshableCatalogProvider {
    snapshot: ProviderDefinition,
    inner: Arc<dyn ModelProvider>,
    runtime: CatalogProviderRuntime,
}

impl ModelProvider for RefreshableCatalogProvider {
    fn provider_id(&self) -> ProviderId {
        self.snapshot.id.clone()
    }

    fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
        self.inner.capabilities()
    }

    fn refresh(&self) -> Result<Option<Arc<dyn ModelProvider>>, ModelError> {
        if self.snapshot.discovery.is_none() {
            Ok(None)
        } else {
            self.runtime.refresh_provider(&self.snapshot).map(Some)
        }
    }

    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.inner.stream(request, cancellation)
    }
}

struct DiscoveryResponse {
    models: Vec<ModelId>,
    sha256: String,
}

#[derive(Deserialize)]
struct OpenAiModelList {
    #[serde(default)]
    data: Vec<OpenAiModelItem>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct OpenAiModelItem {
    id: String,
}

#[derive(Deserialize)]
struct OllamaModelList {
    #[serde(default)]
    models: Vec<OllamaModelItem>,
}

#[derive(Deserialize)]
struct OllamaModelItem {
    #[serde(default)]
    model: String,
    #[serde(default)]
    name: String,
}

fn parse_models(format: ProviderDiscoveryFormat, bytes: &[u8]) -> Result<Vec<ModelId>, ModelError> {
    let values = match format {
        ProviderDiscoveryFormat::OpenaiModels => {
            let response: OpenAiModelList = serde_json::from_slice(bytes).map_err(json_error)?;
            response
                .data
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>()
        }
        ProviderDiscoveryFormat::AnthropicModels => {
            let response: OpenAiModelList = serde_json::from_slice(bytes).map_err(json_error)?;
            if response.has_more {
                return Err(model_error(
                    ModelErrorKind::Protocol,
                    "provider-discovery-pagination-required",
                    "Anthropic Models API 返回 has_more=true；拒绝写入不完整目录",
                ));
            }
            response
                .data
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>()
        }
        ProviderDiscoveryFormat::OllamaTags => {
            let response: OllamaModelList = serde_json::from_slice(bytes).map_err(json_error)?;
            response
                .models
                .into_iter()
                .map(|item| {
                    if item.model.is_empty() {
                        item.name
                    } else {
                        item.model
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    let mut models = Vec::with_capacity(values.len());
    for value in values {
        if value.trim().is_empty() || value.len() > 192 {
            return Err(model_error(
                ModelErrorKind::Protocol,
                "provider-discovery-model-id-invalid",
                "empty-or-too-long",
            ));
        }
        models.push(ModelId::from(value));
    }
    Ok(models)
}

fn build_protocol_provider(
    provider: &ProviderDefinition,
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
) -> Result<Arc<dyn ModelProvider>, ModelError> {
    let multiplexed = provider.routes.len() > 1;
    let mut mux_routes = Vec::new();
    let mut direct = None;
    for (route_index, route) in provider.routes.iter().enumerate() {
        let adapter_id = if multiplexed {
            ProviderId::from(format!("{}:route-{route_index}", provider.id))
        } else {
            provider.id.clone()
        };
        let models = route
            .models
            .iter()
            .map(|model| catalog_capability(&adapter_id, model, route.protocol))
            .collect::<Vec<_>>();
        let adapter: Arc<dyn ModelProvider> = match route.protocol {
            ProviderProtocol::OpenaiChat => Arc::new(CompatibleProvider::new(
                CompatibleProviderConfig {
                    provider_id: adapter_id,
                    endpoint: route.endpoint.clone(),
                    credential_id: provider.credential_id.as_deref().map(CredentialId::new),
                    models,
                    reasoning_field: if route.reasoning_field.as_deref() == Some("reasoning-effort")
                    {
                        CompatibleReasoningField::ReasoningEffort
                    } else {
                        CompatibleReasoningField::Omit
                    },
                    headers: BTreeMap::new(),
                },
                credentials.clone(),
                transport.clone(),
            )?),
            ProviderProtocol::OpenaiResponses => Arc::new(OpenAiResponsesProvider::compatible(
                OpenAiCompatibleResponsesConfig {
                    provider_id: adapter_id,
                    endpoint: route.endpoint.clone(),
                    credential_id: required_credential(provider)?,
                    models,
                },
                credentials.clone(),
                transport.clone(),
            )?),
            ProviderProtocol::AnthropicMessages => Arc::new(AnthropicProvider::compatible(
                AnthropicCompatibleConfig {
                    provider_id: adapter_id,
                    endpoint: route.endpoint.clone(),
                    credential_id: required_credential(provider)?,
                    anthropic_version: "2023-06-01".to_owned(),
                    models,
                },
                credentials.clone(),
                transport.clone(),
            )?),
        };
        if multiplexed {
            mux_routes.extend(
                route
                    .models
                    .iter()
                    .cloned()
                    .map(|model| (model, adapter.clone())),
            );
        } else {
            direct = Some(adapter);
        }
    }
    if multiplexed {
        Ok(Arc::new(ProtocolMuxProvider::new(
            provider.id.clone(),
            mux_routes,
        )?))
    } else {
        direct.ok_or_else(|| {
            model_error(
                ModelErrorKind::InvalidRequest,
                "provider-routes-empty",
                provider.id.to_string(),
            )
        })
    }
}

fn catalog_capability(
    provider_id: &ProviderId,
    model_id: &ModelId,
    protocol: ProviderProtocol,
) -> ModelCapability {
    let mut capability = ModelCapability {
        provider_id: provider_id.clone(),
        model_id: model_id.clone(),
        streaming: true,
        tool_calling: true,
        structured_output: true,
        image_input: false,
        prompt_cache_metrics: true,
        conversation_continuation: false,
        provider_compaction: false,
        context_window_tokens: 32_768,
        max_output_tokens: 4_096,
        reasoning_levels: BTreeSet::new(),
    };
    match protocol {
        ProviderProtocol::OpenaiResponses => {
            capability.conversation_continuation = true;
            capability.provider_compaction = true;
            capability.reasoning_levels = [
                ReasoningLevel::Off,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ]
            .into_iter()
            .collect();
        }
        ProviderProtocol::OpenaiChat => {}
        ProviderProtocol::AnthropicMessages => {
            capability.structured_output = false;
            capability.context_window_tokens = 100_000;
            capability.reasoning_levels = [ReasoningLevel::Off, ReasoningLevel::Medium]
                .into_iter()
                .collect();
        }
    }
    capability
}

fn required_credential(provider: &ProviderDefinition) -> Result<CredentialId, ModelError> {
    provider
        .credential_id
        .as_deref()
        .map(CredentialId::new)
        .ok_or_else(|| {
            model_error(
                ModelErrorKind::Auth,
                "provider-credential-id-missing",
                provider.id.to_string(),
            )
        })
}

fn provider_secret(
    provider: &ProviderDefinition,
    credentials: &dyn CredentialStore,
) -> Result<SecretString, ModelError> {
    let credential_id = required_credential(provider)?;
    credentials
        .get(&credential_id)
        .map_err(auth_error)?
        .ok_or_else(|| {
            model_error(
                ModelErrorKind::Auth,
                "provider-discovery-credential-missing",
                format!("请先运行 `kernary connect {}`", provider.id),
            )
        })
}

fn unix_millis() -> Result<i64, ModelError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            model_error(
                ModelErrorKind::Protocol,
                "provider-discovery-clock",
                error.to_string(),
            )
        })?
        .as_millis()
        .try_into()
        .map_err(|error: std::num::TryFromIntError| {
            model_error(
                ModelErrorKind::Protocol,
                "provider-discovery-clock-overflow",
                error.to_string(),
            )
        })
}

fn model_error(
    kind: ModelErrorKind,
    code: impl Into<String>,
    message: impl Into<String>,
) -> ModelError {
    ModelError::new(kind, code, message)
}

fn catalog_error(error: impl std::fmt::Display) -> ModelError {
    model_error(
        ModelErrorKind::Protocol,
        "provider-catalog",
        error.to_string(),
    )
}

fn auth_error(error: impl std::fmt::Display) -> ModelError {
    model_error(
        ModelErrorKind::Auth,
        "provider-discovery-auth",
        error.to_string(),
    )
}

fn json_error(error: serde_json::Error) -> ModelError {
    model_error(
        ModelErrorKind::Protocol,
        "provider-discovery-json",
        error.to_string(),
    )
}

fn cache_poisoned() -> ModelError {
    model_error(
        ModelErrorKind::Protocol,
        "provider-cache-poisoned",
        "Provider cache lock poisoned",
    )
}

#[must_use]
pub fn cache_path_for_project(project_root: &Path) -> PathBuf {
    harness_provider_catalog::default_model_cache_path(project_root)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{BufReader, Cursor};

    use harness_auth::MemoryCredentialStore;
    use harness_http::{HttpTransportError, StreamingHttpResponse};
    use harness_provider_catalog::{
        ProviderDiscoveryRouting, ProviderRouteDefinition, ProviderSource,
    };
    use tempfile::tempdir;

    use super::*;

    type MockResponses = Arc<Mutex<VecDeque<(u16, Vec<u8>)>>>;

    #[derive(Clone)]
    struct MockTransport {
        responses: MockResponses,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockTransport {
        fn new(responses: impl IntoIterator<Item = (u16, &'static str)>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(
                    responses
                        .into_iter()
                        .map(|(status, body)| (status, body.as_bytes().to_vec()))
                        .collect(),
                )),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn request_debug(&self) -> Vec<String> {
            self.requests.lock().expect("requests").clone()
        }
    }

    impl StreamingHttpTransport for MockTransport {
        fn send(
            &self,
            request: StreamingHttpRequest,
        ) -> Result<StreamingHttpResponse, HttpTransportError> {
            self.requests
                .lock()
                .expect("requests")
                .push(format!("{request:?}"));
            let (status, body) = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .ok_or_else(|| HttpTransportError {
                    code: "mock-response-missing".to_owned(),
                    message: "missing".to_owned(),
                    timeout: false,
                })?;
            Ok(StreamingHttpResponse {
                status,
                headers: BTreeMap::new(),
                body: Box::new(BufReader::new(Cursor::new(body))),
            })
        }
    }

    fn single_route_provider(
        id: &str,
        format: ProviderDiscoveryFormat,
        auth: ProviderDiscoveryAuth,
    ) -> ProviderDefinition {
        ProviderDefinition {
            id: ProviderId::from(id),
            display_name: "Test Relay".to_owned(),
            credential_id: (auth != ProviderDiscoveryAuth::None).then(|| format!("{id}:default")),
            credential_required: auth != ProviderDiscoveryAuth::None,
            routes: vec![ProviderRouteDefinition {
                protocol: ProviderProtocol::OpenaiChat,
                endpoint: "https://relay.example/v1/chat/completions".to_owned(),
                models: vec![ModelId::from("snapshot-model")],
                reasoning_field: None,
            }],
            default_model: Some(ModelId::from("snapshot-model")),
            discovery: Some(ProviderDiscoveryDefinition {
                format,
                endpoint: "https://relay.example/v1/models".to_owned(),
                auth,
                routing: ProviderDiscoveryRouting::SingleRouteAdditive,
            }),
            source: ProviderSource::BuiltIn,
        }
    }

    #[test]
    fn construction_is_zero_network_and_single_route_refresh_is_cached_and_routable() {
        let temporary = tempdir().expect("tempdir");
        let cache_path = temporary.path().join(".harness/provider-models-v1.json");
        let credentials = Arc::new(MemoryCredentialStore::new());
        credentials
            .put(
                &CredentialId::new("relay:default"),
                SecretString::new("super-secret-key"),
            )
            .expect("credential");
        let transport = MockTransport::new([(
            200,
            r#"{"object":"list","data":[{"id":"snapshot-model"},{"id":"new-coder"}]}"#,
        )]);
        let runtime = CatalogProviderRuntime::new(
            &cache_path,
            credentials,
            Arc::new(transport.clone()),
            Duration::from_secs(1),
        )
        .expect("runtime");
        let provider = runtime
            .build(single_route_provider(
                "relay",
                ProviderDiscoveryFormat::OpenaiModels,
                ProviderDiscoveryAuth::Bearer,
            ))
            .expect("provider");
        assert!(transport.request_debug().is_empty(), "构造阶段不得联网");
        assert_eq!(provider.capabilities().expect("caps").len(), 1);

        let refreshed = provider.refresh().expect("refresh").expect("replacement");
        let models = refreshed
            .capabilities()
            .expect("caps")
            .into_iter()
            .map(|capability| capability.model_id)
            .collect::<BTreeSet<_>>();
        assert!(models.contains(&ModelId::from("new-coder")));
        let requests = transport.request_debug();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("Authorization"));
        assert!(requests[0].contains("[REDACTED]"));
        assert!(!requests[0].contains("super-secret-key"));
        let cache = std::fs::read_to_string(cache_path).expect("cache");
        assert!(cache.contains("new-coder"));
        assert!(!cache.contains("super-secret-key"));
    }

    #[test]
    fn multi_protocol_opencode_discovery_never_guesses_unknown_model_route() {
        let temporary = tempdir().expect("tempdir");
        let transport = MockTransport::new([(
            200,
            r#"{"object":"list","data":[{"id":"kimi-k3"},{"id":"brand-new-unknown"}]}"#,
        )]);
        let runtime = CatalogProviderRuntime::new(
            temporary.path().join("cache.json"),
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(transport),
            Duration::from_secs(1),
        )
        .expect("runtime");
        let catalog = ProviderCatalog::built_in().expect("catalog");
        let provider = runtime
            .build(
                catalog
                    .get(&ProviderId::from("opencode-go"))
                    .expect("opencode-go")
                    .clone(),
            )
            .expect("provider");
        let refreshed = provider.refresh().expect("refresh").expect("replacement");
        let models = refreshed
            .capabilities()
            .expect("caps")
            .into_iter()
            .map(|capability| capability.model_id)
            .collect::<BTreeSet<_>>();
        assert!(models.contains(&ModelId::from("kimi-k3")));
        assert!(!models.contains(&ModelId::from("brand-new-unknown")));
    }

    #[test]
    fn ollama_tags_add_models_but_anthropic_partial_page_is_rejected() {
        let temporary = tempdir().expect("tempdir");
        let ollama_transport = MockTransport::new([(
            200,
            r#"{"models":[{"model":"qwen3-coder:30b"},{"name":"deepseek-r1:8b"}]}"#,
        )]);
        let runtime = CatalogProviderRuntime::new(
            temporary.path().join("ollama.json"),
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(ollama_transport),
            Duration::from_secs(1),
        )
        .expect("runtime");
        let catalog = ProviderCatalog::built_in().expect("catalog");
        let ollama = runtime
            .build(
                catalog
                    .get(&ProviderId::from("ollama"))
                    .expect("ollama")
                    .clone(),
            )
            .expect("provider")
            .refresh()
            .expect("refresh")
            .expect("replacement");
        assert!(
            ollama
                .capabilities()
                .expect("caps")
                .iter()
                .any(|model| model.model_id == ModelId::from("qwen3-coder:30b"))
        );

        let credentials = Arc::new(MemoryCredentialStore::new());
        credentials
            .put(
                &CredentialId::new("anthropic-test:default"),
                SecretString::new("secret"),
            )
            .expect("credential");
        let anthropic_transport =
            MockTransport::new([(200, r#"{"data":[{"id":"claude-new"}],"has_more":true}"#)]);
        let mut anthropic = single_route_provider(
            "anthropic-test",
            ProviderDiscoveryFormat::AnthropicModels,
            ProviderDiscoveryAuth::AnthropicApiKey,
        );
        anthropic.credential_id = Some("anthropic-test:default".to_owned());
        let cache_path = temporary.path().join("anthropic.json");
        let runtime = CatalogProviderRuntime::new(
            &cache_path,
            credentials,
            Arc::new(anthropic_transport),
            Duration::from_secs(1),
        )
        .expect("runtime");
        let provider = runtime.build(anthropic).expect("provider");
        let error = match provider.refresh() {
            Err(error) => error,
            Ok(_) => panic!("partial page rejected"),
        };
        assert_eq!(error.code, "provider-discovery-pagination-required");
        assert!(!cache_path.exists());
    }

    #[test]
    fn missing_credential_fails_before_network() {
        let temporary = tempdir().expect("tempdir");
        let transport = MockTransport::new([(200, r#"{"data":[{"id":"model"}]}"#)]);
        let runtime = CatalogProviderRuntime::new(
            temporary.path().join("cache.json"),
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(transport.clone()),
            Duration::from_secs(1),
        )
        .expect("runtime");
        let provider = runtime
            .build(single_route_provider(
                "relay",
                ProviderDiscoveryFormat::OpenaiModels,
                ProviderDiscoveryAuth::Bearer,
            ))
            .expect("provider");
        let error = match provider.refresh() {
            Err(error) => error,
            Ok(_) => panic!("missing credential"),
        };
        assert_eq!(error.code, "provider-discovery-credential-missing");
        assert!(transport.request_debug().is_empty());
    }

    #[test]
    fn failed_refresh_preserves_last_good_provider_and_cache() {
        let temporary = tempdir().expect("tempdir");
        let cache_path = temporary.path().join("cache.json");
        let transport = MockTransport::new([
            (200, r#"{"data":[{"id":"first-good"}]}"#),
            (500, r#"{"error":"secret upstream detail"}"#),
        ]);
        let runtime = CatalogProviderRuntime::new(
            &cache_path,
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(transport),
            Duration::from_secs(1),
        )
        .expect("runtime");
        let provider = runtime
            .build(single_route_provider(
                "relay",
                ProviderDiscoveryFormat::OpenaiModels,
                ProviderDiscoveryAuth::None,
            ))
            .expect("provider");
        let refreshed = provider
            .refresh()
            .expect("first refresh")
            .expect("replacement");
        let before = std::fs::read(&cache_path).expect("cache");
        let error = match refreshed.refresh() {
            Err(error) => error,
            Ok(_) => panic!("500 refresh must fail"),
        };
        assert_eq!(error.code, "provider-discovery-http");
        assert_eq!(std::fs::read(&cache_path).expect("cache"), before);
        assert!(
            refreshed
                .capabilities()
                .expect("old capabilities")
                .iter()
                .any(|model| model.model_id == ModelId::from("first-good"))
        );
        let cache = String::from_utf8(before).expect("utf8");
        assert!(!cache.contains("secret upstream detail"));
    }
}
