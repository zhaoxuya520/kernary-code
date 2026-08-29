#![forbid(unsafe_code)]

//! Anthropic Messages API Adapter；只向上暴露 text、summary、tool、usage。

use std::collections::{BTreeMap, VecDeque};
use std::io::BufRead;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{
    HttpTransportError, StreamingHttpRequest, StreamingHttpResponse, StreamingHttpTransport,
    UreqStreamingTransport,
};
use harness_model::{
    CancellationToken, CompletionStatus, ModelCapability, ModelError, ModelErrorKind, ModelEvent,
    ModelEventStream, ModelInputItem, ModelMessageRole, ModelProvider, ModelRequest, ModelUsage,
    ReasoningAdapter, ReasoningLevel, ResponseFormat,
};
use harness_types::{ModelId, ProviderId, ResponseId, ToolCallId};

const ANTHROPIC_MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    pub credential_id: CredentialId,
    pub models: Vec<ModelCapability>,
}

/// Claude/Anthropic Messages wire protocol 的第三方中转配置。
#[derive(Clone, Debug)]
pub struct AnthropicCompatibleConfig {
    pub provider_id: ProviderId,
    pub endpoint: String,
    pub credential_id: CredentialId,
    pub anthropic_version: String,
    pub models: Vec<ModelCapability>,
}

impl AnthropicCompatibleConfig {
    fn validate(&self) -> Result<(), ModelError> {
        validate_messages_endpoint(&self.endpoint)?;
        if self.provider_id.as_str().trim().is_empty()
            || self.anthropic_version.trim().is_empty()
            || self.models.is_empty()
            || self
                .models
                .iter()
                .any(|model| model.provider_id != self.provider_id)
        {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "anthropic-compatible-config-invalid",
                self.provider_id.to_string(),
            ));
        }
        Ok(())
    }
}

impl AnthropicConfig {
    fn validate(&self) -> Result<(), ModelError> {
        if self.models.is_empty()
            || self
                .models
                .iter()
                .any(|model| model.provider_id != ProviderId::from("anthropic"))
        {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "anthropic-config-invalid",
                "Anthropic model capability 为空或 Provider ID 不匹配",
            ));
        }
        Ok(())
    }
}

pub struct AnthropicProvider {
    provider_id: ProviderId,
    endpoint: String,
    credential_id: CredentialId,
    anthropic_version: String,
    models: Vec<ModelCapability>,
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
}

impl AnthropicProvider {
    pub fn new(
        config: AnthropicConfig,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Result<Self, ModelError> {
        config.validate()?;
        Ok(Self {
            provider_id: ProviderId::from("anthropic"),
            endpoint: ANTHROPIC_MESSAGES_ENDPOINT.to_owned(),
            credential_id: config.credential_id,
            anthropic_version: ANTHROPIC_VERSION.to_owned(),
            models: config.models,
            credentials,
            transport,
        })
    }

    pub fn compatible(
        config: AnthropicCompatibleConfig,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Result<Self, ModelError> {
        config.validate()?;
        Ok(Self {
            provider_id: config.provider_id,
            endpoint: config.endpoint,
            credential_id: config.credential_id,
            anthropic_version: config.anthropic_version,
            models: config.models,
            credentials,
            transport,
        })
    }

    pub fn compatible_with_ureq(
        config: AnthropicCompatibleConfig,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, ModelError> {
        Self::compatible(
            config,
            credentials,
            Arc::new(UreqStreamingTransport::default()),
        )
    }

    pub fn with_ureq(
        config: AnthropicConfig,
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
                    "anthropic-model-not-configured",
                    model_id.to_string(),
                )
            })
    }

    fn api_key(&self) -> Result<SecretString, ModelError> {
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
                    "anthropic-api-key-missing",
                    format!("请先连接 {} credential", self.provider_id),
                )
            })
    }
}

impl ModelProvider for AnthropicProvider {
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
        if request.max_output_tokens > capability.max_output_tokens {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "max-output-tokens-exceeded",
                request.max_output_tokens.to_string(),
            ));
        }
        if matches!(request.response_format, ResponseFormat::JsonSchema { .. }) {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "anthropic-structured-output-not-configured",
                "当前 Adapter 尚未配置 Anthropic Structured Output profile",
            ));
        }
        if request.previous_response_id.is_some() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "anthropic-continuation-unsupported",
                "请把历史作为本地 Context Items 传回 Messages API",
            ));
        }
        let reasoning = ReasoningAdapter
            .resolve(request.reasoning, capability)
            .effective;
        let body = build_request(&request, reasoning)?;
        let api_key = self.api_key()?;
        let http_request = StreamingHttpRequest::json(&self.endpoint, body, request.timeout)
            .with_header("anthropic-version", &self.anthropic_version)
            .with_sensitive_header("x-api-key", api_key);
        let transport = self.transport.clone();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::sync_channel(128);
        thread::Builder::new()
            .name("harness-anthropic-stream".to_owned())
            .spawn(move || {
                let response = match transport.send(http_request) {
                    Ok(response) => response,
                    Err(error) => {
                        let _ = sender.send(Err(map_transport_error(error)));
                        return;
                    }
                };
                if !(200..300).contains(&response.status) {
                    let _ = sender.send(Err(map_http_error(response)));
                    return;
                }
                for event in AnthropicSseStream::new(response.body, worker_cancellation.clone()) {
                    if sender.send(event).is_err() || worker_cancellation.is_cancelled() {
                        break;
                    }
                }
            })
            .map_err(|error| {
                ModelError::new(
                    ModelErrorKind::Transport,
                    "anthropic-stream-thread",
                    error.to_string(),
                )
            })?;
        Ok(Box::new(ChannelStream {
            receiver,
            cancellation,
            cancellation_emitted: false,
        }))
    }
}

pub fn build_request(
    request: &ModelRequest,
    effective_reasoning: Option<ReasoningLevel>,
) -> Result<serde_json::Value, ModelError> {
    let mut messages = Vec::new();
    for item in &request.input {
        match item {
            ModelInputItem::Message { role, content } => messages.push(serde_json::json!({
                "role": if *role == ModelMessageRole::Assistant {"assistant"} else {"user"},
                "content":content
            })),
            ModelInputItem::ToolResult { call_id, output } => {
                messages.push(serde_json::json!({
                    "role":"user",
                    "content":[{
                        "type":"tool_result",
                        "tool_use_id":call_id,
                        "content":serde_json::to_string(output).map_err(|error| ModelError::new(
                            ModelErrorKind::InvalidRequest,
                            "tool-result-json",
                            error.to_string()
                        ))?
                    }]
                }));
            }
            ModelInputItem::ToolCall {
                call_id,
                name,
                arguments,
            } => messages.push(serde_json::json!({
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":call_id,
                    "name":name,
                    "input":arguments
                }]
            })),
        }
    }
    let mut tools = request.tools.clone();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let tools = tools
        .into_iter()
        .map(|tool| {
            serde_json::json!({
                "name":tool.name,
                "description":tool.description,
                "input_schema":tool.input_schema
            })
        })
        .collect::<Vec<_>>();
    let mut body = serde_json::json!({
        "model":request.model_id,
        "system":request.instructions,
        "messages":messages,
        "tools":tools,
        "max_tokens":request.max_output_tokens,
        "stream":true
    });
    if effective_reasoning.is_some_and(|level| level != ReasoningLevel::Off) {
        body["thinking"] = serde_json::json!({
            "type":"adaptive",
            "display":"summarized"
        });
    }
    Ok(body)
}

struct ChannelStream {
    receiver: Receiver<Result<ModelEvent, ModelError>>,
    cancellation: CancellationToken,
    cancellation_emitted: bool,
}

impl Iterator for ChannelStream {
    type Item = Result<ModelEvent, ModelError>;

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
                    "Anthropic stream 已取消",
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

struct ToolState {
    call_id: ToolCallId,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct UsageState {
    input_tokens: u64,
    cache_read: u64,
    cache_write: u64,
    output_tokens: u64,
}

struct AnthropicSseStream {
    lines: std::io::Lines<Box<dyn BufRead + Send>>,
    cancellation: CancellationToken,
    queued: VecDeque<Result<ModelEvent, ModelError>>,
    data_lines: Vec<String>,
    tools: BTreeMap<u64, ToolState>,
    usage: UsageState,
    started: bool,
    stopped: bool,
    completion: Option<(CompletionStatus, Option<String>)>,
    eof_emitted: bool,
}

impl AnthropicSseStream {
    fn new(body: Box<dyn BufRead + Send>, cancellation: CancellationToken) -> Self {
        Self {
            lines: body.lines(),
            cancellation,
            queued: VecDeque::new(),
            data_lines: vec![],
            tools: BTreeMap::new(),
            usage: UsageState::default(),
            started: false,
            stopped: false,
            completion: None,
            eof_emitted: false,
        }
    }

    fn flush(&mut self) {
        if self.data_lines.is_empty() {
            return;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        let value = match serde_json::from_str::<serde_json::Value>(&data) {
            Ok(value) => value,
            Err(error) => {
                self.queued.push_back(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "anthropic-sse-json",
                    error.to_string(),
                )));
                return;
            }
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => {
                self.started = true;
                let message = &value["message"];
                self.usage.input_tokens = u64_at(message, "/usage/input_tokens");
                self.usage.cache_read = u64_at(message, "/usage/cache_read_input_tokens");
                self.usage.cache_write = u64_at(message, "/usage/cache_creation_input_tokens");
                self.usage.output_tokens = u64_at(message, "/usage/output_tokens");
                if let (Some(id), Some(model)) =
                    (string_at(message, "/id"), string_at(message, "/model"))
                {
                    self.queued.push_back(Ok(ModelEvent::Started {
                        response_id: ResponseId::from(id),
                        model_id: ModelId::from(model),
                    }));
                }
            }
            Some("content_block_start") => {
                if string_at(&value, "/content_block/type") == Some("tool_use") {
                    let index = u64_at(&value, "/index");
                    if let (Some(id), Some(name)) = (
                        string_at(&value, "/content_block/id"),
                        string_at(&value, "/content_block/name"),
                    ) {
                        self.tools.insert(
                            index,
                            ToolState {
                                call_id: ToolCallId::from(id),
                                name: name.to_owned(),
                                arguments: String::new(),
                            },
                        );
                    }
                }
            }
            Some("content_block_delta") => match string_at(&value, "/delta/type") {
                Some("text_delta") => self.queued.push_back(Ok(ModelEvent::TextDelta {
                    delta: string_at(&value, "/delta/text")
                        .unwrap_or_default()
                        .to_owned(),
                })),
                Some("input_json_delta") => {
                    if let Some(tool) = self.tools.get_mut(&u64_at(&value, "/index")) {
                        tool.arguments
                            .push_str(string_at(&value, "/delta/partial_json").unwrap_or_default());
                    }
                }
                // 只有请求 display=summarized 时才开启 thinking，因此这里是公开摘要。
                Some("thinking_delta") => {
                    self.queued.push_back(Ok(ModelEvent::ReasoningSummaryDelta {
                        delta: string_at(&value, "/delta/thinking")
                            .unwrap_or_default()
                            .to_owned(),
                    }))
                }
                _ => {}
            },
            Some("content_block_stop") => {
                if let Some(tool) = self.tools.remove(&u64_at(&value, "/index")) {
                    match serde_json::from_str(&tool.arguments) {
                        Ok(arguments) => self.queued.push_back(Ok(ModelEvent::ToolCall {
                            call_id: tool.call_id,
                            name: tool.name,
                            arguments,
                        })),
                        Err(error) => self.queued.push_back(Err(ModelError::new(
                            ModelErrorKind::Protocol,
                            "anthropic-tool-arguments-json",
                            error.to_string(),
                        ))),
                    }
                }
            }
            Some("message_delta") => {
                self.usage.output_tokens =
                    u64_at(&value, "/usage/output_tokens").max(self.usage.output_tokens);
                if let Some(reason) = string_at(&value, "/delta/stop_reason") {
                    self.completion = Some((
                        if reason == "max_tokens" {
                            CompletionStatus::Incomplete
                        } else {
                            CompletionStatus::Completed
                        },
                        (reason == "max_tokens").then(|| reason.to_owned()),
                    ));
                }
            }
            Some("message_stop") => self.finish(),
            Some("error") => {
                self.stopped = true;
                self.queued.push_back(Err(ModelError::new(
                    ModelErrorKind::Provider,
                    "anthropic-stream-error",
                    format!(
                        "Anthropic stream returned an error ({})",
                        string_at(&value, "/error/type").unwrap_or("unknown")
                    ),
                )));
            }
            _ => {}
        }
    }

    fn finish(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let input_tokens = self
            .usage
            .input_tokens
            .saturating_add(self.usage.cache_read)
            .saturating_add(self.usage.cache_write);
        let usage = ModelUsage {
            input_tokens,
            cached_input_tokens: self.usage.cache_read,
            cache_write_tokens: self.usage.cache_write,
            output_tokens: self.usage.output_tokens,
            reasoning_tokens: 0,
            total_tokens: input_tokens.saturating_add(self.usage.output_tokens),
        };
        self.queued
            .push_back(usage.validate().map(|usage| ModelEvent::Usage { usage }));
        let (status, incomplete_reason) = self
            .completion
            .take()
            .unwrap_or((CompletionStatus::Completed, None));
        self.queued.push_back(Ok(ModelEvent::Completed {
            status,
            incomplete_reason,
        }));
    }
}

impl Iterator for AnthropicSseStream {
    type Item = Result<ModelEvent, ModelError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Some(event);
            }
            if self.cancellation.is_cancelled() {
                return Some(Err(ModelError::new(
                    ModelErrorKind::Cancelled,
                    "model-cancelled",
                    "Anthropic stream 已取消",
                )));
            }
            match self.lines.next() {
                Some(Ok(line)) if line.is_empty() => self.flush(),
                Some(Ok(line)) => {
                    if let Some(data) = line.strip_prefix("data:") {
                        self.data_lines.push(data.trim_start().to_owned());
                    }
                }
                Some(Err(error)) => {
                    return Some(Err(ModelError::new(
                        ModelErrorKind::Transport,
                        "anthropic-sse-read",
                        error.to_string(),
                    )));
                }
                None => {
                    self.flush();
                    if let Some(event) = self.queued.pop_front() {
                        return Some(event);
                    }
                    if !self.eof_emitted && !self.stopped {
                        self.eof_emitted = true;
                        return Some(Err(ModelError::new(
                            ModelErrorKind::Protocol,
                            "anthropic-stream-truncated",
                            "SSE 在 message_stop 前结束",
                        )));
                    }
                    return None;
                }
            }
        }
    }
}

fn string_at<'a>(value: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(serde_json::Value::as_str)
}

fn u64_at(value: &serde_json::Value, pointer: &str) -> u64 {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn map_transport_error(error: HttpTransportError) -> ModelError {
    ModelError::new(
        if error.timeout {
            ModelErrorKind::Timeout
        } else {
            ModelErrorKind::Transport
        },
        error.code,
        error.message,
    )
}

fn map_http_error(mut response: StreamingHttpResponse) -> ModelError {
    use std::io::Read;

    let mut body = String::new();
    let _ = response
        .body
        .by_ref()
        .take(64 * 1024)
        .read_to_string(&mut body);
    let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
    let provider_type = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let kind = match response.status {
        401 | 403 => ModelErrorKind::Auth,
        408 | 504 => ModelErrorKind::Timeout,
        429 => ModelErrorKind::RateLimit,
        400..=499 => ModelErrorKind::InvalidRequest,
        _ => ModelErrorKind::Provider,
    };
    let mut error = ModelError::new(
        kind,
        format!("anthropic-http-{}", response.status),
        format!("Anthropic HTTP {} ({provider_type})", response.status),
    );
    if response.status >= 500 {
        error.retryable = true;
    }
    if response.status == 429
        && let Some(seconds) = response
            .headers
            .get("retry-after")
            .and_then(|value| value.parse().ok())
    {
        error.retry_after = Some(Duration::from_secs(seconds));
    }
    error
}

fn validate_messages_endpoint(endpoint: &str) -> Result<(), ModelError> {
    let parsed = url::Url::parse(endpoint).map_err(|error| {
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            "anthropic-endpoint-invalid",
            error.to_string(),
        )
    })?;
    let host = parsed.host_str().ok_or_else(|| {
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            "anthropic-endpoint-host-missing",
            endpoint,
        )
    })?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.path().ends_with("/messages")
    {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "anthropic-endpoint-shape",
            "endpoint 必须是无 userinfo/query/fragment 的 /messages URL",
        ));
    }
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "anthropic-endpoint-insecure",
            "远程 Messages Provider 必须 HTTPS；HTTP 只允许 loopback",
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
    use harness_model::{ToolDefinition, validate_event_contract};

    use super::*;

    struct FakeTransport {
        response: Mutex<Option<StreamingHttpResponse>>,
        captured: Mutex<Option<StreamingHttpRequest>>,
    }

    impl StreamingHttpTransport for FakeTransport {
        fn send(
            &self,
            request: StreamingHttpRequest,
        ) -> Result<StreamingHttpResponse, HttpTransportError> {
            *self.captured.lock().expect("capture") = Some(request);
            self.response
                .lock()
                .expect("response")
                .take()
                .ok_or_else(|| HttpTransportError {
                    code: "missing".to_owned(),
                    message: "missing".to_owned(),
                    timeout: false,
                })
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model_id: ModelId::from("claude-test"),
            instructions: "system".to_owned(),
            input: vec![ModelInputItem::Message {
                role: ModelMessageRole::User,
                content: "hello".to_owned(),
            }],
            tools: vec![ToolDefinition {
                name: "weather".to_owned(),
                description: "weather".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
                strict: true,
            }],
            reasoning: ReasoningLevel::Medium,
            response_format: ResponseFormat::Text,
            max_output_tokens: 100,
            previous_response_id: None,
            store: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn request_enables_summarized_thinking_only() {
        let body = build_request(&request(), Some(ReasoningLevel::Medium)).expect("body");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["tools"][0]["name"], "weather");
    }

    #[test]
    fn stream_normalizes_text_tool_summary_and_cumulative_usage() {
        let values = [
            serde_json::json!({"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":10,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":1}}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"summary"}}),
            serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}}),
            serde_json::json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"weather","input":{}}}),
            serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"Paris\"}"}}),
            serde_json::json!({"type":"content_block_stop","index":2}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
            serde_json::json!({"type":"message_stop"}),
        ];
        let bytes = values
            .iter()
            .map(|value| format!("event: ignored\ndata: {value}\n\n"))
            .collect::<String>()
            .into_bytes();
        let events = AnthropicSseStream::new(
            Box::new(BufReader::new(Cursor::new(bytes))),
            CancellationToken::new(),
        )
        .collect::<Result<Vec<_>, _>>()
        .expect("events");
        validate_event_contract(&events).expect("shared contract");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ReasoningSummaryDelta { delta } if delta == "summary"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCall { arguments, .. } if arguments["city"] == "Paris"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage { usage }
                if usage.input_tokens == 15 && usage.cached_input_tokens == 3 && usage.output_tokens == 9
        )));
    }

    #[test]
    fn capability_shape_can_represent_summary_only_reasoning() {
        let capability = ModelCapability {
            provider_id: ProviderId::from("anthropic"),
            model_id: ModelId::from("claude-test"),
            streaming: true,
            tool_calling: true,
            structured_output: false,
            image_input: false,
            prompt_cache_metrics: true,
            conversation_continuation: false,
            provider_compaction: false,
            context_window_tokens: 100_000,
            max_output_tokens: 4_096,
            reasoning_levels: [ReasoningLevel::Off, ReasoningLevel::Medium]
                .into_iter()
                .collect::<BTreeSet<_>>(),
        };
        assert!(
            capability
                .reasoning_levels
                .contains(&ReasoningLevel::Medium)
        );
    }

    #[test]
    fn provider_uses_x_api_key_and_passes_shared_contract() {
        let values = [
            serde_json::json!({"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":2,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            serde_json::json!({"type":"message_stop"}),
        ];
        let bytes = values
            .iter()
            .map(|value| format!("data: {value}\n\n"))
            .collect::<String>()
            .into_bytes();
        let transport = Arc::new(FakeTransport {
            response: Mutex::new(Some(StreamingHttpResponse {
                status: 200,
                headers: Default::default(),
                body: Box::new(BufReader::new(Cursor::new(bytes))),
            })),
            captured: Mutex::new(None),
        });
        let credentials = Arc::new(MemoryCredentialStore::new());
        let credential_id = CredentialId::new("anthropic:test");
        credentials
            .put(&credential_id, SecretString::new("sk-ant-test"))
            .expect("credential");
        let provider = AnthropicProvider::new(
            AnthropicConfig {
                credential_id,
                models: vec![ModelCapability {
                    provider_id: ProviderId::from("anthropic"),
                    model_id: ModelId::from("claude-test"),
                    streaming: true,
                    tool_calling: true,
                    structured_output: false,
                    image_input: false,
                    prompt_cache_metrics: true,
                    conversation_continuation: false,
                    provider_compaction: false,
                    context_window_tokens: 100_000,
                    max_output_tokens: 4_096,
                    reasoning_levels: [ReasoningLevel::Off, ReasoningLevel::Medium]
                        .into_iter()
                        .collect(),
                }],
            },
            credentials,
            transport.clone(),
        )
        .expect("provider");
        let events = provider
            .stream(request(), CancellationToken::new())
            .expect("stream")
            .collect::<Result<Vec<_>, _>>()
            .expect("events");
        validate_event_contract(&events).expect("shared contract");
        let captured = transport
            .captured
            .lock()
            .expect("capture")
            .take()
            .expect("request");
        assert!(
            captured
                .sensitive_headers
                .iter()
                .any(|header| header.name == "x-api-key")
        );
        assert!(!format!("{captured:?}").contains("sk-ant-test"));
    }

    #[test]
    fn compatible_messages_provider_uses_custom_identity_and_secure_endpoint_policy() {
        let credentials = Arc::new(MemoryCredentialStore::new());
        let provider_id = ProviderId::from("company-relay");
        let provider = AnthropicProvider::compatible(
            AnthropicCompatibleConfig {
                provider_id: provider_id.clone(),
                endpoint: "https://relay.example.com/v1/messages".to_owned(),
                credential_id: CredentialId::new("provider:company-relay"),
                anthropic_version: ANTHROPIC_VERSION.to_owned(),
                models: vec![ModelCapability {
                    provider_id: provider_id.clone(),
                    model_id: ModelId::from("claude-relay"),
                    streaming: true,
                    tool_calling: true,
                    structured_output: false,
                    image_input: false,
                    prompt_cache_metrics: true,
                    conversation_continuation: false,
                    provider_compaction: false,
                    context_window_tokens: 100_000,
                    max_output_tokens: 4_096,
                    reasoning_levels: BTreeSet::new(),
                }],
            },
            credentials,
            Arc::new(FakeTransport {
                response: Mutex::new(None),
                captured: Mutex::new(None),
            }),
        )
        .expect("compatible provider");
        assert_eq!(provider.provider_id(), provider_id);
        assert_eq!(
            provider.capabilities().expect("capabilities")[0].model_id,
            ModelId::from("claude-relay")
        );
        assert!(validate_messages_endpoint("http://127.0.0.1:9000/v1/messages").is_ok());
        assert!(validate_messages_endpoint("http://relay.example.com/v1/messages").is_err());
        assert!(validate_messages_endpoint("https://user@relay.example.com/v1/messages").is_err());
    }
}
