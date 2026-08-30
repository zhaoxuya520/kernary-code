#![forbid(unsafe_code)]

//! OpenAI Responses API 的协议 Adapter。
//!
//! 只处理官方 API Key + `/v1/responses`；ChatGPT subscription 由 Codex delegated adapter 负责。

mod protocol;
mod sse;

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{StreamingHttpTransport, UreqStreamingTransport};
use harness_model::{
    CancellationToken, ModelCapability, ModelError, ModelErrorKind, ModelEventStream,
    ModelProvider, ModelRequest, ReasoningAdapter, ReasoningLevel, ResponseFormat,
};
use harness_types::{ModelId, ProviderId};

pub use harness_http::{
    HttpTransportError as TransportError, StreamingHttpRequest as OpenAiHttpRequest,
    StreamingHttpResponse as OpenAiHttpResponse, StreamingHttpTransport as OpenAiTransport,
    UreqStreamingTransport as UreqOpenAiTransport,
};
pub use protocol::build_responses_request;
pub use sse::OpenAiSseStream;

const OFFICIAL_RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";

#[derive(Clone, Debug)]
pub struct OpenAiResponsesConfig {
    pub credential_id: CredentialId,
    pub models: Vec<ModelCapability>,
}

/// OpenAI Responses wire protocol 的第三方网关配置。
#[derive(Clone, Debug)]
pub struct OpenAiCompatibleResponsesConfig {
    pub provider_id: ProviderId,
    pub endpoint: String,
    pub credential_id: CredentialId,
    pub models: Vec<ModelCapability>,
}

impl OpenAiCompatibleResponsesConfig {
    fn validate(&self) -> Result<(), ModelError> {
        validate_responses_endpoint(&self.endpoint)?;
        if self.provider_id.as_str().trim().is_empty()
            || self.models.is_empty()
            || self
                .models
                .iter()
                .any(|model| model.provider_id != self.provider_id)
        {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "openai-compatible-responses-config-invalid",
                self.provider_id.to_string(),
            ));
        }
        Ok(())
    }
}

impl OpenAiResponsesConfig {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.models.is_empty() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "openai-models-empty",
                "OpenAI Adapter 至少需要一个显式 Model capability",
            ));
        }
        for model in &self.models {
            if model.provider_id != ProviderId::from("openai") {
                return Err(ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "openai-provider-id-mismatch",
                    model.model_id.to_string(),
                ));
            }
        }
        Ok(())
    }
}

pub struct OpenAiResponsesProvider {
    provider_id: ProviderId,
    endpoint: String,
    credential_id: CredentialId,
    models: Vec<ModelCapability>,
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
}

impl OpenAiResponsesProvider {
    pub fn new(
        config: OpenAiResponsesConfig,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Result<Self, ModelError> {
        config.validate()?;
        Ok(Self {
            provider_id: ProviderId::from("openai"),
            endpoint: OFFICIAL_RESPONSES_ENDPOINT.to_owned(),
            credential_id: config.credential_id,
            models: config.models,
            credentials,
            transport,
        })
    }

    pub fn compatible(
        config: OpenAiCompatibleResponsesConfig,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Result<Self, ModelError> {
        config.validate()?;
        Ok(Self {
            provider_id: config.provider_id,
            endpoint: config.endpoint,
            credential_id: config.credential_id,
            models: config.models,
            credentials,
            transport,
        })
    }

    pub fn compatible_with_ureq(
        config: OpenAiCompatibleResponsesConfig,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, ModelError> {
        Self::compatible(
            config,
            credentials,
            Arc::new(UreqStreamingTransport::default()),
        )
    }

    pub fn official(
        config: OpenAiResponsesConfig,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, ModelError> {
        Self::new(
            config,
            credentials,
            Arc::new(UreqStreamingTransport::default()),
        )
    }

    fn capability(&self, model_id: &ModelId) -> Result<&ModelCapability, ModelError> {
        self.models
            .iter()
            .find(|model| &model.model_id == model_id)
            .ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "openai-model-not-configured",
                    model_id.to_string(),
                )
            })
    }

    fn validate_request(
        &self,
        request: &ModelRequest,
        capability: &ModelCapability,
    ) -> Result<Option<ReasoningLevel>, ModelError> {
        if request.timeout == Duration::ZERO {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "model-timeout-zero",
                "Model timeout 必须大于 0",
            ));
        }
        if request.max_output_tokens > capability.max_output_tokens {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "max-output-tokens-exceeded",
                request.max_output_tokens.to_string(),
            ));
        }
        if !request.tools.is_empty() && !capability.tool_calling {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "tool-calling-unsupported",
                request.model_id.to_string(),
            ));
        }
        if matches!(request.response_format, ResponseFormat::JsonSchema { .. })
            && !capability.structured_output
        {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "structured-output-unsupported",
                request.model_id.to_string(),
            ));
        }
        if request.previous_response_id.is_some() && !capability.conversation_continuation {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "continuation-unsupported",
                request.model_id.to_string(),
            ));
        }
        Ok(ReasoningAdapter
            .resolve(request.reasoning, capability)
            .effective)
    }

    fn credential(&self) -> Result<SecretString, ModelError> {
        self.credentials
            .get(&self.credential_id)
            .map_err(|error| {
                ModelError::new(
                    ModelErrorKind::Auth,
                    "credential-store-error",
                    error.message,
                )
            })?
            .ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::Auth,
                    "openai-api-key-missing",
                    format!("请先连接 {} credential", self.provider_id),
                )
            })
    }
}

impl ModelProvider for OpenAiResponsesProvider {
    fn provider_id(&self) -> ProviderId {
        self.provider_id.clone()
    }

    fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
        Ok(self.models.clone())
    }

    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let capability = self.capability(&request.model_id)?;
        let effective_reasoning = self.validate_request(&request, capability)?;
        let body =
            build_responses_request(&request, effective_reasoning, capability.reasoning_summary)?;
        let api_key = self.credential()?;
        if cancellation.is_cancelled() {
            return Err(ModelError::new(
                ModelErrorKind::Cancelled,
                "model-cancelled",
                "请求在发送前已取消",
            ));
        }
        let timeout = request.timeout;
        let authorization = SecretString::new(format!(
            "Bearer {}",
            api_key.expose_secret().map_err(|error| ModelError::new(
                ModelErrorKind::Auth,
                error.code,
                error.message
            ))?
        ));
        let transport = self.transport.clone();
        let endpoint = self.endpoint.clone();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::sync_channel(128);
        thread::Builder::new()
            .name("harness-openai-stream".to_owned())
            .spawn(move || {
                let response = transport.send(
                    OpenAiHttpRequest::json(endpoint, body, timeout)
                        .with_sensitive_header("Authorization", authorization),
                );
                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        let _ = sender.send(Err(map_transport_error(error)));
                        return;
                    }
                };
                if !(200..300).contains(&response.status) {
                    let _ = sender.send(Err(protocol::map_http_error(response)));
                    return;
                }
                for event in OpenAiSseStream::new(response.body, worker_cancellation.clone()) {
                    if sender.send(event).is_err() || worker_cancellation.is_cancelled() {
                        break;
                    }
                }
            })
            .map_err(|error| {
                ModelError::new(
                    ModelErrorKind::Transport,
                    "openai-stream-thread",
                    error.to_string(),
                )
            })?;
        Ok(Box::new(ChannelModelStream {
            receiver,
            cancellation,
            cancellation_emitted: false,
        }))
    }
}

struct ChannelModelStream {
    receiver: Receiver<Result<harness_model::ModelEvent, ModelError>>,
    cancellation: CancellationToken,
    cancellation_emitted: bool,
}

impl Iterator for ChannelModelStream {
    type Item = Result<harness_model::ModelEvent, ModelError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.cancellation.is_cancelled() {
                if self.cancellation_emitted {
                    return None;
                }
                self.cancellation_emitted = true;
                return Some(Err(ModelError::new(
                    ModelErrorKind::Cancelled,
                    "model-cancelled",
                    "OpenAI stream 已取消",
                )));
            }
            match self.receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(event) => return Some(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

fn map_transport_error(error: TransportError) -> ModelError {
    let kind = if error.timeout {
        ModelErrorKind::Timeout
    } else {
        ModelErrorKind::Transport
    };
    ModelError::new(kind, error.code, error.message)
}

fn validate_responses_endpoint(endpoint: &str) -> Result<(), ModelError> {
    let parsed = url::Url::parse(endpoint).map_err(|error| {
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            "responses-endpoint-invalid",
            error.to_string(),
        )
    })?;
    let host = parsed.host_str().ok_or_else(|| {
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            "responses-endpoint-host-missing",
            endpoint,
        )
    })?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.path().ends_with("/responses")
    {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "responses-endpoint-shape",
            "endpoint 必须是无 userinfo/query/fragment 的 /responses URL",
        ));
    }
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "responses-endpoint-insecure",
            "远程 Responses Provider 必须 HTTPS；HTTP 只允许 loopback",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::{BufReader, Cursor};
    use std::sync::Mutex;

    use harness_auth::{CredentialStore, MemoryCredentialStore};
    use harness_model::{
        CompletionStatus, ModelEvent, ModelInputItem, ModelMessageRole, ModelUsage,
        validate_event_contract,
    };
    use harness_types::ResponseId;

    use super::*;

    struct FakeTransport {
        response: Mutex<Option<OpenAiHttpResponse>>,
        captured: Mutex<Option<(String, serde_json::Value, String)>>,
    }

    impl FakeTransport {
        fn sse(events: &[serde_json::Value]) -> Self {
            let bytes = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>()
                .into_bytes();
            Self {
                response: Mutex::new(Some(OpenAiHttpResponse {
                    status: 200,
                    headers: Default::default(),
                    body: Box::new(BufReader::new(Cursor::new(bytes))),
                })),
                captured: Mutex::new(None),
            }
        }
    }

    impl OpenAiTransport for FakeTransport {
        fn send(&self, request: OpenAiHttpRequest) -> Result<OpenAiHttpResponse, TransportError> {
            let debug = format!("{request:?}");
            let body = match request.body {
                harness_http::HttpBody::Json(body) => body,
                _ => panic!("OpenAI transport must use JSON body"),
            };
            *self.captured.lock().expect("capture lock") = Some((request.endpoint, body, debug));
            self.response
                .lock()
                .expect("response lock")
                .take()
                .ok_or_else(|| TransportError {
                    code: "fake-response-missing".to_owned(),
                    message: "missing".to_owned(),
                    timeout: false,
                })
        }
    }

    struct SlowTransport;

    impl OpenAiTransport for SlowTransport {
        fn send(&self, _request: OpenAiHttpRequest) -> Result<OpenAiHttpResponse, TransportError> {
            std::thread::sleep(Duration::from_millis(250));
            Ok(OpenAiHttpResponse {
                status: 200,
                headers: Default::default(),
                body: Box::new(BufReader::new(Cursor::new(Vec::<u8>::new()))),
            })
        }
    }

    fn capability(model_id: &str) -> ModelCapability {
        ModelCapability {
            provider_id: ProviderId::from("openai"),
            model_id: ModelId::from(model_id),
            streaming: true,
            tool_calling: true,
            structured_output: true,
            image_input: false,
            prompt_cache_metrics: true,
            conversation_continuation: true,
            provider_compaction: true,
            context_window_tokens: 100_000,
            max_output_tokens: 8_192,
            reasoning_summary: true,
            reasoning_levels: [ReasoningLevel::Low, ReasoningLevel::High]
                .into_iter()
                .collect::<BTreeSet<_>>(),
        }
    }

    fn request(model_id: &str) -> ModelRequest {
        ModelRequest {
            model_id: ModelId::from(model_id),
            instructions: "stable".to_owned(),
            input: vec![ModelInputItem::Message {
                role: ModelMessageRole::User,
                content: "hello".to_owned(),
            }],
            tools: vec![],
            reasoning: ReasoningLevel::Medium,
            response_format: ResponseFormat::Text,
            max_output_tokens: 100,
            previous_response_id: Some(ResponseId::from("resp_previous")),
            prompt_cache: None,
            store: false,
            timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn provider_translates_request_and_stream_without_exposing_key() {
        let credentials = Arc::new(MemoryCredentialStore::new());
        let credential_id = CredentialId::new("openai:test");
        credentials
            .put(&credential_id, SecretString::new("sk-test-never-log"))
            .expect("credential");
        let transport = Arc::new(FakeTransport::sse(&[
            serde_json::json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-test"}}),
            serde_json::json!({"type":"response.output_text.delta","delta":"hello"}),
            serde_json::json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":1},"total_tokens":12}}
            }),
        ]));
        let provider = OpenAiResponsesProvider::new(
            OpenAiResponsesConfig {
                credential_id,
                models: vec![capability("gpt-test")],
            },
            credentials,
            transport.clone(),
        )
        .expect("provider");
        let events = provider
            .stream(request("gpt-test"), CancellationToken::new())
            .expect("stream")
            .collect::<Result<Vec<_>, _>>()
            .expect("events");
        validate_event_contract(&events).expect("shared contract");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage {
                usage: ModelUsage {
                    cached_input_tokens: 4,
                    ..
                }
            }
        )));
        assert!(matches!(
            events.last(),
            Some(ModelEvent::Completed {
                status: CompletionStatus::Completed,
                ..
            })
        ));
        let captured = transport
            .captured
            .lock()
            .expect("capture")
            .take()
            .expect("captured request");
        assert_eq!(captured.0, OFFICIAL_RESPONSES_ENDPOINT);
        assert_eq!(captured.1["reasoning"]["effort"], "low");
        assert_eq!(captured.1["previous_response_id"], "resp_previous");
        assert!(!captured.2.contains("sk-test-never-log"));
        assert!(captured.2.contains("[REDACTED]"));
    }

    #[test]
    fn missing_api_key_fails_before_transport() {
        let provider = OpenAiResponsesProvider::new(
            OpenAiResponsesConfig {
                credential_id: CredentialId::new("missing"),
                models: vec![capability("gpt-test")],
            },
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(FakeTransport::sse(&[])),
        )
        .expect("provider");
        let error = provider
            .stream(request("gpt-test"), CancellationToken::new())
            .err()
            .expect("auth error");
        assert_eq!(error.kind, ModelErrorKind::Auth);
        assert_eq!(error.code, "openai-api-key-missing");
    }

    #[test]
    fn compatible_responses_provider_uses_custom_identity_and_endpoint_policy() {
        let provider_id = ProviderId::from("opencode-zen");
        let mut model = capability("gpt-relay");
        model.provider_id = provider_id.clone();
        let provider = OpenAiResponsesProvider::compatible(
            OpenAiCompatibleResponsesConfig {
                provider_id: provider_id.clone(),
                endpoint: "https://opencode.ai/zen/v1/responses".to_owned(),
                credential_id: CredentialId::new("provider:opencode-zen"),
                models: vec![model],
            },
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(FakeTransport::sse(&[])),
        )
        .expect("compatible provider");
        assert_eq!(provider.provider_id(), provider_id);
        assert!(validate_responses_endpoint("http://127.0.0.1:9000/v1/responses").is_ok());
        assert!(validate_responses_endpoint("http://relay.example.com/v1/responses").is_err());
        assert!(
            validate_responses_endpoint("https://user@relay.example.com/v1/responses").is_err()
        );
    }

    #[test]
    fn cancellation_returns_without_waiting_for_blocked_transport() {
        let credentials = Arc::new(MemoryCredentialStore::new());
        let credential_id = CredentialId::new("openai:cancel");
        credentials
            .put(&credential_id, SecretString::new("sk-test"))
            .expect("credential");
        let provider = OpenAiResponsesProvider::new(
            OpenAiResponsesConfig {
                credential_id,
                models: vec![capability("gpt-test")],
            },
            credentials,
            Arc::new(SlowTransport),
        )
        .expect("provider");
        let cancellation = CancellationToken::new();
        let mut stream = provider
            .stream(request("gpt-test"), cancellation.clone())
            .expect("stream");
        cancellation.cancel();
        let started = std::time::Instant::now();
        let error = stream.next().expect("cancel event").expect_err("cancelled");
        assert_eq!(error.kind, ModelErrorKind::Cancelled);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    #[ignore = "需要显式提供 HARNESS_OPENAI_LIVE=1、OPENAI_API_KEY、HARNESS_OPENAI_MODEL"]
    fn live_openai_stream_is_opt_in() {
        if std::env::var("HARNESS_OPENAI_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY required");
        let model = std::env::var("HARNESS_OPENAI_MODEL").expect("HARNESS_OPENAI_MODEL required");
        let credentials = Arc::new(MemoryCredentialStore::new());
        let credential_id = CredentialId::new("openai:live");
        credentials
            .put(&credential_id, SecretString::new(key))
            .expect("credential");
        let provider = OpenAiResponsesProvider::official(
            OpenAiResponsesConfig {
                credential_id,
                models: vec![capability(&model)],
            },
            credentials,
        )
        .expect("provider");
        let mut live_request = request(&model);
        live_request.previous_response_id = None;
        live_request.reasoning = ReasoningLevel::Low;
        live_request.input = vec![ModelInputItem::Message {
            role: ModelMessageRole::User,
            content: "Reply with exactly OK".to_owned(),
        }];
        let events = provider
            .stream(live_request, CancellationToken::new())
            .expect("stream")
            .collect::<Result<Vec<_>, _>>()
            .expect("events");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::TextDelta { .. }))
        );
    }
}
