#![forbid(unsafe_code)]

//! OpenAI-compatible `/chat/completions` Provider Adapter。

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatibleReasoningField {
    Omit,
    ReasoningEffort,
}

#[derive(Clone, Debug)]
pub struct CompatibleProviderConfig {
    pub provider_id: ProviderId,
    pub endpoint: String,
    pub credential_id: Option<CredentialId>,
    pub models: Vec<ModelCapability>,
    pub reasoning_field: CompatibleReasoningField,
    pub headers: BTreeMap<String, String>,
}

impl CompatibleProviderConfig {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_endpoint(&self.endpoint)?;
        if self.provider_id.as_str().trim().is_empty() || self.models.is_empty() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "compatible-config-empty",
                "Provider ID 和 Model capabilities 不能为空",
            ));
        }
        if self
            .models
            .iter()
            .any(|model| model.provider_id != self.provider_id)
        {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "compatible-provider-id-mismatch",
                self.provider_id.to_string(),
            ));
        }
        if self.headers.keys().any(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "proxy-authorization" | "x-api-key" | "cookie" | "set-cookie"
            )
        }) {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "compatible-sensitive-header-misclassified",
                "敏感 Header 必须通过 CredentialStore/SensitiveHeader 配置",
            ));
        }
        Ok(())
    }
}

pub struct CompatibleProvider {
    config: CompatibleProviderConfig,
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
}

impl CompatibleProvider {
    pub fn new(
        config: CompatibleProviderConfig,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Result<Self, ModelError> {
        config.validate()?;
        Ok(Self {
            config,
            credentials,
            transport,
        })
    }

    pub fn with_ureq(
        config: CompatibleProviderConfig,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, ModelError> {
        Self::new(
            config,
            credentials,
            Arc::new(UreqStreamingTransport::default()),
        )
    }

    fn capability(&self, model_id: &ModelId) -> Result<&ModelCapability, ModelError> {
        self.config
            .models
            .iter()
            .find(|model| &model.model_id == model_id)
            .ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "compatible-model-not-configured",
                    model_id.to_string(),
                )
            })
    }

    fn authorization(&self) -> Result<Option<SecretString>, ModelError> {
        let Some(credential_id) = &self.config.credential_id else {
            return Ok(None);
        };
        let secret = self
            .credentials
            .get(credential_id)
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
                    "compatible-api-key-missing",
                    format!("缺少 {} credential", self.config.provider_id),
                )
            })?;
        Ok(Some(SecretString::new(format!(
            "Bearer {}",
            secret.expose_secret().map_err(|error| ModelError::new(
                ModelErrorKind::Auth,
                error.code,
                error.message
            ))?
        ))))
    }
}

impl ModelProvider for CompatibleProvider {
    fn provider_id(&self) -> ProviderId {
        self.config.provider_id.clone()
    }

    fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
        Ok(self.config.models.clone())
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
        if request.previous_response_id.is_some() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "compatible-continuation-unsupported",
                "Chat Completions Adapter 不接受 previous_response_id；请传回本地 Context Items",
            ));
        }
        let effective = ReasoningAdapter
            .resolve(request.reasoning, capability)
            .effective;
        let body = build_request(&request, effective, self.config.reasoning_field)?;
        let authorization = self.authorization()?;
        let mut http_request =
            StreamingHttpRequest::json(&self.config.endpoint, body, request.timeout);
        for (name, value) in &self.config.headers {
            http_request = http_request.with_header(name, value);
        }
        if let Some(authorization) = authorization {
            http_request = http_request.with_sensitive_header("Authorization", authorization);
        }
        let transport = self.transport.clone();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::sync_channel(128);
        thread::Builder::new()
            .name(format!("harness-{}-stream", self.config.provider_id))
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
                for event in
                    ChatCompletionsSseStream::new(response.body, worker_cancellation.clone())
                {
                    if sender.send(event).is_err() || worker_cancellation.is_cancelled() {
                        break;
                    }
                }
            })
            .map_err(|error| {
                ModelError::new(
                    ModelErrorKind::Transport,
                    "compatible-stream-thread",
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

fn validate_endpoint(endpoint: &str) -> Result<(), ModelError> {
    let (scheme, remainder) = endpoint
        .strip_prefix("https://")
        .map(|remainder| ("https", remainder))
        .or_else(|| {
            endpoint
                .strip_prefix("http://")
                .map(|remainder| ("http", remainder))
        })
        .ok_or_else(|| {
            ModelError::new(
                ModelErrorKind::InvalidRequest,
                "compatible-endpoint-invalid",
                "Endpoint 必须以 https:// 或 http:// 开头",
            )
        })?;
    let (authority, path) = remainder.split_once('/').ok_or_else(|| {
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            "compatible-endpoint-path",
            "Endpoint 缺少 /chat/completions path",
        )
    })?;
    if authority.is_empty()
        || authority.contains('@')
        || endpoint.contains('?')
        || endpoint.contains('#')
    {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "compatible-endpoint-invalid",
            "Endpoint 不允许空主机、userinfo、query 或 fragment",
        ));
    }
    let host = if let Some(ipv6) = authority.strip_prefix('[') {
        ipv6.split_once(']')
            .map(|(host, _)| host)
            .unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if scheme != "https" && !(scheme == "http" && loopback) {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "compatible-endpoint-insecure",
            "远程 Provider 必须使用 HTTPS；HTTP 只允许 loopback",
        ));
    }
    if !path.ends_with("chat/completions") {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "compatible-endpoint-path",
            "Endpoint 必须指向 /chat/completions",
        ));
    }
    Ok(())
}

pub fn build_request(
    request: &ModelRequest,
    effective_reasoning: Option<ReasoningLevel>,
    reasoning_field: CompatibleReasoningField,
) -> Result<serde_json::Value, ModelError> {
    let mut messages = vec![serde_json::json!({
        "role":"system",
        "content":request.instructions
    })];
    for item in &request.input {
        match item {
            ModelInputItem::Message { role, content } => messages.push(serde_json::json!({
                "role": match role {
                    ModelMessageRole::Developer => "system",
                    ModelMessageRole::User => "user",
                    ModelMessageRole::Assistant => "assistant",
                },
                "content":content
            })),
            ModelInputItem::ToolResult { call_id, output } => {
                messages.push(serde_json::json!({
                    "role":"tool",
                    "tool_call_id":call_id,
                    "content":serde_json::to_string(output).map_err(|error| ModelError::new(
                        ModelErrorKind::InvalidRequest,
                        "tool-result-json",
                        error.to_string()
                    ))?
                }));
            }
            ModelInputItem::ToolCall {
                call_id,
                name,
                arguments,
            } => messages.push(serde_json::json!({
                "role":"assistant",
                "content":serde_json::Value::Null,
                "tool_calls":[{
                    "id":call_id,
                    "type":"function",
                    "function":{
                        "name":name,
                        "arguments":serde_json::to_string(arguments).map_err(|error| ModelError::new(
                            ModelErrorKind::InvalidRequest,
                            "tool-call-json",
                            error.to_string()
                        ))?
                    }
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
                "type":"function",
                "function":{
                    "name":tool.name,
                    "description":tool.description,
                    "parameters":tool.input_schema,
                    "strict":tool.strict
                }
            })
        })
        .collect::<Vec<_>>();
    let mut body = serde_json::json!({
        "model":request.model_id,
        "messages":messages,
        "tools":tools,
        "tool_choice":"auto",
        "max_tokens":request.max_output_tokens,
        "stream":true,
        "stream_options":{"include_usage":true}
    });
    if let ResponseFormat::JsonSchema {
        name,
        schema,
        strict,
    } = &request.response_format
    {
        body["response_format"] = serde_json::json!({
            "type":"json_schema",
            "json_schema":{"name":name,"schema":schema,"strict":strict}
        });
    }
    if reasoning_field == CompatibleReasoningField::ReasoningEffort
        && let Some(reasoning) = effective_reasoning
    {
        body["reasoning_effort"] = serde_json::json!(reasoning_name(reasoning));
    }
    Ok(body)
}

const fn reasoning_name(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Off => "none",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::Xhigh => "xhigh",
        ReasoningLevel::Max => "max",
    }
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
                    "Compatible Provider stream 已取消",
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

struct PendingToolCall {
    call_id: Option<ToolCallId>,
    name: String,
    arguments: String,
}

struct ChatCompletionsSseStream {
    lines: std::io::Lines<Box<dyn BufRead + Send>>,
    cancellation: CancellationToken,
    queued: VecDeque<Result<ModelEvent, ModelError>>,
    started: bool,
    terminal: bool,
    calls: BTreeMap<u64, PendingToolCall>,
    latest_usage: Option<serde_json::Value>,
    pending_completion: Option<(CompletionStatus, Option<String>)>,
    data_lines: Vec<String>,
    eof_emitted: bool,
}

impl ChatCompletionsSseStream {
    fn new(body: Box<dyn BufRead + Send>, cancellation: CancellationToken) -> Self {
        Self {
            lines: body.lines(),
            cancellation,
            queued: VecDeque::new(),
            started: false,
            terminal: false,
            calls: BTreeMap::new(),
            latest_usage: None,
            pending_completion: None,
            data_lines: vec![],
            eof_emitted: false,
        }
    }

    fn flush(&mut self) {
        if self.data_lines.is_empty() {
            return;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        if data == "[DONE]" {
            let (status, reason) = self
                .pending_completion
                .take()
                .unwrap_or((CompletionStatus::Completed, None));
            self.finish(status, reason);
            return;
        }
        let value = match serde_json::from_str::<serde_json::Value>(&data) {
            Ok(value) => value,
            Err(error) => {
                self.queued.push_back(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "compatible-sse-json",
                    error.to_string(),
                )));
                return;
            }
        };
        if value.get("error").is_some() {
            self.terminal = true;
            self.queued.push_back(Err(ModelError::new(
                ModelErrorKind::Provider,
                "compatible-stream-error",
                value
                    .pointer("/error/message")
                    .and_then(serde_json::Value::as_str)
                    .map_or(
                        "Provider stream error",
                        |_| "Provider stream returned an error",
                    ),
            )));
            return;
        }
        if !self.started
            && let (Some(id), Some(model)) = (
                value.get("id").and_then(serde_json::Value::as_str),
                value.get("model").and_then(serde_json::Value::as_str),
            )
        {
            self.started = true;
            self.queued.push_back(Ok(ModelEvent::Started {
                response_id: ResponseId::from(id),
                model_id: ModelId::from(model),
            }));
        }
        if let Some(usage) = value.get("usage") {
            self.latest_usage = Some(usage.clone());
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return;
        };
        if let Some(reasoning) = choice
            .pointer("/delta/reasoning_content")
            .and_then(serde_json::Value::as_str)
            && !reasoning.is_empty()
        {
            // DeepSeek/GLM 等 Chat 兼容模型会把用户可见的推理流放在独立字段。
            // 它仍与最终正文隔离，交给 TUI 的 live reasoning cell，而不是混进回答。
            self.queued.push_back(Ok(ModelEvent::ReasoningSummaryDelta {
                delta: reasoning.to_owned(),
            }));
        }
        if let Some(content) = choice
            .pointer("/delta/content")
            .and_then(serde_json::Value::as_str)
            && !content.is_empty()
        {
            self.queued.push_back(Ok(ModelEvent::TextDelta {
                delta: content.to_owned(),
            }));
        }
        if let Some(tool_calls) = choice
            .pointer("/delta/tool_calls")
            .and_then(serde_json::Value::as_array)
        {
            for tool_call in tool_calls {
                let index = tool_call
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let pending = self.calls.entry(index).or_insert_with(|| PendingToolCall {
                    call_id: None,
                    name: String::new(),
                    arguments: String::new(),
                });
                if let Some(id) = tool_call.get("id").and_then(serde_json::Value::as_str) {
                    pending.call_id = Some(ToolCallId::from(id));
                }
                if let Some(name) = tool_call
                    .pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
                {
                    pending.name.push_str(name);
                }
                if let Some(arguments) = tool_call
                    .pointer("/function/arguments")
                    .and_then(serde_json::Value::as_str)
                {
                    pending.arguments.push_str(arguments);
                }
            }
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
        {
            self.pending_completion = Some((
                if reason == "length" {
                    CompletionStatus::Incomplete
                } else {
                    CompletionStatus::Completed
                },
                (reason == "length").then(|| reason.to_owned()),
            ));
        }
    }

    fn finish(&mut self, status: CompletionStatus, reason: Option<String>) {
        if self.terminal {
            return;
        }
        for (_, pending) in std::mem::take(&mut self.calls) {
            let Some(call_id) = pending.call_id else {
                self.queued.push_back(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "compatible-tool-call-id-missing",
                    pending.name,
                )));
                continue;
            };
            match serde_json::from_str(&pending.arguments) {
                Ok(arguments) => self.queued.push_back(Ok(ModelEvent::ToolCall {
                    call_id,
                    name: pending.name,
                    arguments,
                })),
                Err(error) => self.queued.push_back(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "compatible-tool-arguments-json",
                    error.to_string(),
                ))),
            }
        }
        self.queued.push_back(match self.latest_usage.take() {
            Some(usage) => parse_usage(&usage).map(|usage| ModelEvent::Usage { usage }),
            None => Err(ModelError::new(
                ModelErrorKind::Protocol,
                "compatible-usage-missing",
                "Provider 未返回请求的 stream usage",
            )),
        });
        self.terminal = true;
        self.queued.push_back(Ok(ModelEvent::Completed {
            status,
            incomplete_reason: reason,
        }));
    }
}

impl Iterator for ChatCompletionsSseStream {
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
                    "Compatible stream 已取消",
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
                        "compatible-sse-read",
                        error.to_string(),
                    )));
                }
                None => {
                    self.flush();
                    if !self.terminal
                        && let Some((status, reason)) = self.pending_completion.take()
                    {
                        self.finish(status, reason);
                    }
                    if let Some(event) = self.queued.pop_front() {
                        return Some(event);
                    }
                    if !self.eof_emitted && !self.terminal {
                        self.eof_emitted = true;
                        return Some(Err(ModelError::new(
                            ModelErrorKind::Protocol,
                            "compatible-stream-truncated",
                            "SSE 在 terminal event 前结束",
                        )));
                    }
                    return None;
                }
            }
        }
    }
}

fn parse_usage(value: &serde_json::Value) -> Result<ModelUsage, ModelError> {
    ModelUsage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: 0,
        output_tokens: value
            .get("completion_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: value
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        total_tokens: value
            .get("total_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    }
    .validate()
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
    let provider_code = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/code"))
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
        format!("compatible-http-{}", response.status),
        format!(
            "Compatible Provider HTTP {} ({provider_code})",
            response.status
        ),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::{BufReader, Cursor};
    use std::sync::Mutex;

    use harness_auth::MemoryCredentialStore;
    use harness_model::{ModelMessageRole, ToolDefinition, validate_event_contract};

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
                    code: "fake-response-missing".to_owned(),
                    message: "missing".to_owned(),
                    timeout: false,
                })
        }
    }

    fn capability() -> ModelCapability {
        ModelCapability {
            provider_id: ProviderId::from("ollama"),
            model_id: ModelId::from("local-model"),
            streaming: true,
            tool_calling: true,
            structured_output: true,
            image_input: false,
            prompt_cache_metrics: false,
            conversation_continuation: false,
            provider_compaction: false,
            context_window_tokens: 8_192,
            max_output_tokens: 1_024,
            reasoning_summary: false,
            reasoning_levels: BTreeSet::new(),
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model_id: ModelId::from("local-model"),
            instructions: "system".to_owned(),
            input: vec![ModelInputItem::Message {
                role: ModelMessageRole::User,
                content: "hello".to_owned(),
            }],
            tools: vec![ToolDefinition {
                name: "read_file".to_owned(),
                description: "read".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
                strict: true,
            }],
            reasoning: ReasoningLevel::Off,
            response_format: ResponseFormat::Text,
            max_output_tokens: 100,
            previous_response_id: None,
            prompt_cache: None,
            store: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn endpoint_allows_https_and_loopback_http_only() {
        assert!(validate_endpoint("https://api.example.com/v1/chat/completions").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:11434/v1/chat/completions").is_ok());
        assert_eq!(
            validate_endpoint("http://api.example.com/v1/chat/completions")
                .expect_err("insecure")
                .code,
            "compatible-endpoint-insecure"
        );
        assert!(validate_endpoint("http://127.0.0.1.evil.test/v1/chat/completions").is_err());
        assert!(validate_endpoint("http://[::1]:11434/v1/chat/completions").is_ok());
        assert!(validate_endpoint("https://user@example.com/v1/chat/completions").is_err());
    }

    #[test]
    fn stream_accumulates_tool_arguments_and_usage() {
        let chunks = [
            serde_json::json!({"id":"chat_1","model":"model","choices":[{"delta":{"reasoning_content":"checking "},"finish_reason":null}]}),
            serde_json::json!({"id":"chat_1","model":"model","choices":[{"delta":{"reasoning_content":"facts"},"finish_reason":null}]}),
            serde_json::json!({"id":"chat_1","model":"model","choices":[{"delta":{"content":"hi"},"finish_reason":null}]}),
            serde_json::json!({"id":"chat_1","model":"model","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}),
            serde_json::json!({"id":"chat_1","model":"model","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_","arguments":"{\"path\":"}}]},"finish_reason":null}]}),
            serde_json::json!({"id":"chat_1","model":"model","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"\"a.rs\"}"}}]},"finish_reason":"tool_calls"}]}),
            serde_json::json!({"id":"chat_1","model":"model","choices":[],"usage":{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens":2,"total_tokens":12}}),
        ];
        let bytes = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>()
            .into_bytes();
        let events = ChatCompletionsSseStream::new(
            Box::new(BufReader::new(Cursor::new(bytes))),
            CancellationToken::new(),
        )
        .collect::<Result<Vec<_>, _>>()
        .expect("events");
        validate_event_contract(&events).expect("shared contract");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ReasoningSummaryDelta { delta } if delta == "checking "
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCall { name, arguments, .. }
                if name == "read_file" && arguments["path"] == "a.rs"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage { usage } if usage.cached_input_tokens == 3
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ModelEvent::Usage { .. }))
                .count(),
            1,
            "累计 usage 只能在 terminal 前上报一次"
        );
    }

    #[test]
    fn local_provider_needs_no_fake_api_key_and_uses_loopback_endpoint() {
        let bytes = [
            serde_json::json!({"id":"chat_1","model":"local-model","choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
            serde_json::json!({"id":"chat_1","model":"local-model","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
        ]
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
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
        let provider = CompatibleProvider::new(
            CompatibleProviderConfig {
                provider_id: ProviderId::from("ollama"),
                endpoint: "http://127.0.0.1:11434/v1/chat/completions".to_owned(),
                credential_id: None,
                models: vec![capability()],
                reasoning_field: CompatibleReasoningField::Omit,
                headers: BTreeMap::new(),
            },
            Arc::new(MemoryCredentialStore::new()),
            transport.clone(),
        )
        .expect("provider");
        let events = provider
            .stream(request(), CancellationToken::new())
            .expect("stream")
            .collect::<Result<Vec<_>, _>>()
            .expect("events");
        validate_event_contract(&events).expect("shared contract");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::TextDelta { delta } if delta == "ok"
        )));
        let captured = transport
            .captured
            .lock()
            .expect("capture")
            .take()
            .expect("request");
        assert!(captured.endpoint.starts_with("http://127.0.0.1"));
        assert!(captured.sensitive_headers.is_empty());
    }
}
