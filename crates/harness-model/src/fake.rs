use std::collections::VecDeque;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use harness_types::{ModelId, ProviderId, ResponseId, ToolCallId};

use crate::{
    CancellationToken, CompletionStatus, ModelCapability, ModelError, ModelErrorKind, ModelEvent,
    ModelEventStream, ModelInputItem, ModelProvider, ModelRequest, ModelUsage, ResponseFormat,
};

/// Fake Provider 的一组确定性 stream 事件。
#[derive(Clone, Debug)]
pub struct FakeScenario {
    events: Vec<Result<ModelEvent, ModelError>>,
}

impl FakeScenario {
    #[must_use]
    pub fn text(chunks: &[&str], usage: ModelUsage) -> Self {
        let mut events = vec![Ok(ModelEvent::Started {
            response_id: ResponseId::from("response:fake"),
            model_id: ModelId::from("deterministic"),
        })];
        events.extend(chunks.iter().map(|chunk| {
            Ok(ModelEvent::TextDelta {
                delta: (*chunk).to_owned(),
            })
        }));
        events.push(Ok(ModelEvent::Usage { usage }));
        events.push(Ok(ModelEvent::Completed {
            status: CompletionStatus::Completed,
            incomplete_reason: None,
        }));
        Self { events }
    }

    #[must_use]
    pub fn tool(name: &str, arguments: serde_json::Value, usage: ModelUsage) -> Self {
        Self {
            events: vec![
                Ok(ModelEvent::Started {
                    response_id: ResponseId::from("response:fake-tool"),
                    model_id: ModelId::from("deterministic"),
                }),
                Ok(ModelEvent::ToolCall {
                    call_id: ToolCallId::from("call:fake"),
                    name: name.to_owned(),
                    arguments,
                }),
                Ok(ModelEvent::Usage { usage }),
                Ok(ModelEvent::Completed {
                    status: CompletionStatus::Completed,
                    incomplete_reason: None,
                }),
            ],
        }
    }

    #[must_use]
    pub fn error(error: ModelError) -> Self {
        Self {
            events: vec![Err(error)],
        }
    }
}

/// 合同测试与离线开发使用的 deterministic Provider。
pub struct FakeModelProvider {
    capability: ModelCapability,
    scenarios: Mutex<VecDeque<FakeScenario>>,
    requests: Mutex<Vec<ModelRequest>>,
    echo_when_empty: bool,
    event_delay: Duration,
}

impl FakeModelProvider {
    #[must_use]
    pub fn new(capability: ModelCapability, scenarios: Vec<FakeScenario>) -> Self {
        Self {
            capability,
            scenarios: Mutex::new(scenarios.into()),
            requests: Mutex::new(Vec::new()),
            echo_when_empty: false,
            event_delay: Duration::ZERO,
        }
    }

    pub fn standard(scenarios: Vec<FakeScenario>) -> Self {
        use crate::ReasoningLevel;

        Self::new(
            ModelCapability {
                provider_id: ProviderId::from("fake"),
                model_id: ModelId::from("deterministic"),
                streaming: true,
                tool_calling: true,
                structured_output: true,
                image_input: false,
                prompt_cache_metrics: true,
                conversation_continuation: true,
                provider_compaction: false,
                context_window_tokens: 8_192,
                max_output_tokens: 2_048,
                reasoning_levels: [
                    ReasoningLevel::Off,
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                ]
                .into_iter()
                .collect(),
            },
            scenarios,
        )
    }

    #[must_use]
    pub fn echo() -> Self {
        let mut provider = Self::standard(vec![]);
        provider.echo_when_empty = true;
        provider
    }

    #[must_use]
    pub fn echo_with_delay(event_delay: Duration) -> Self {
        let mut provider = Self::echo();
        provider.event_delay = event_delay;
        provider
    }

    pub fn requests(&self) -> Result<Vec<ModelRequest>, ModelError> {
        self.requests
            .lock()
            .map_err(|_| {
                ModelError::new(
                    ModelErrorKind::Provider,
                    "fake-request-log-poisoned",
                    "Fake request log poisoned",
                )
            })
            .map(|requests| requests.clone())
    }

    fn validate_request(&self, request: &ModelRequest) -> Result<(), ModelError> {
        if request.model_id != self.capability.model_id {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "model-not-found",
                request.model_id.to_string(),
            ));
        }
        if request.max_output_tokens > self.capability.max_output_tokens {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "max-output-tokens-exceeded",
                request.max_output_tokens.to_string(),
            ));
        }
        let input_tokens = estimate_request_tokens(request);
        let available = self
            .capability
            .context_window_tokens
            .saturating_sub(request.max_output_tokens);
        if input_tokens > available {
            return Err(ModelError::new(
                ModelErrorKind::ContextLimit,
                "context-limit-exceeded",
                format!("input={input_tokens}, available={available}"),
            ));
        }
        if !request.tools.is_empty() && !self.capability.tool_calling {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "tool-calling-unsupported",
                "当前模型不支持 Tool Calling",
            ));
        }
        if matches!(request.response_format, ResponseFormat::JsonSchema { .. })
            && !self.capability.structured_output
        {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "structured-output-unsupported",
                "当前模型不支持 Structured Output",
            ));
        }
        if request.previous_response_id.is_some() && !self.capability.conversation_continuation {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "continuation-unsupported",
                "当前模型不支持 Provider continuation",
            ));
        }
        Ok(())
    }
}

impl ModelProvider for FakeModelProvider {
    fn provider_id(&self) -> ProviderId {
        self.capability.provider_id.clone()
    }

    fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
        Ok(vec![self.capability.clone()])
    }

    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.validate_request(&request)?;
        self.requests
            .lock()
            .map_err(|_| {
                ModelError::new(
                    ModelErrorKind::Provider,
                    "fake-request-log-poisoned",
                    "Fake request log poisoned",
                )
            })?
            .push(request.clone());
        let scenario = self
            .scenarios
            .lock()
            .map_err(|_| {
                ModelError::new(
                    ModelErrorKind::Provider,
                    "fake-provider-poisoned",
                    "Fake scenario lock poisoned",
                )
            })?
            .pop_front();
        let scenario = match scenario {
            Some(scenario) => scenario,
            None if self.echo_when_empty => echo_scenario(&request),
            None => {
                return Err(ModelError::new(
                    ModelErrorKind::Provider,
                    "fake-scenario-exhausted",
                    "没有剩余 Fake scenario",
                ));
            }
        };
        Ok(Box::new(FakeStream {
            events: scenario.events.into(),
            cancellation,
            cancellation_emitted: false,
            event_delay: self.event_delay,
        }))
    }
}

fn echo_scenario(request: &ModelRequest) -> FakeScenario {
    let text = request
        .input
        .iter()
        .rev()
        .find_map(|item| match item {
            ModelInputItem::Message { content, .. } => Some(content.as_str()),
            ModelInputItem::ToolResult { .. } | ModelInputItem::ToolCall { .. } => None,
        })
        .unwrap_or("empty");
    let output = format!("deterministic:{text}");
    let input_tokens = u64::from(estimate_request_tokens(request));
    let output_tokens =
        u64::try_from(output.chars().count().saturating_add(3) / 4).unwrap_or(u64::MAX);
    let chunks = [output.as_str()];
    FakeScenario::text(
        &chunks,
        ModelUsage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            ..ModelUsage::default()
        },
    )
}

struct FakeStream {
    events: VecDeque<Result<ModelEvent, ModelError>>,
    cancellation: CancellationToken,
    cancellation_emitted: bool,
    event_delay: Duration,
}

impl Iterator for FakeStream {
    type Item = Result<ModelEvent, ModelError>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.event_delay.is_zero() {
            thread::sleep(self.event_delay);
        }
        if self.cancellation.is_cancelled() {
            if self.cancellation_emitted {
                return None;
            }
            self.cancellation_emitted = true;
            return Some(Err(ModelError::new(
                ModelErrorKind::Cancelled,
                "model-cancelled",
                "Model stream 已取消",
            )));
        }
        self.events.pop_front().map(|event| {
            event.and_then(|event| match event {
                ModelEvent::Usage { usage } => {
                    usage.validate().map(|usage| ModelEvent::Usage { usage })
                }
                event => Ok(event),
            })
        })
    }
}

fn estimate_request_tokens(request: &ModelRequest) -> u32 {
    let input_units = request.instructions.chars().count().saturating_add(
        request
            .input
            .iter()
            .map(|item| match item {
                ModelInputItem::Message { content, .. } => content.chars().count(),
                ModelInputItem::ToolResult { output, .. } => output.to_string().len(),
                ModelInputItem::ToolCall { arguments, .. } => arguments.to_string().len(),
            })
            .sum::<usize>(),
    );
    u32::try_from(input_units.saturating_add(3) / 4).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        ModelInputItem, ModelMessageRole, ReasoningLevel, ResponseFormat, ToolDefinition,
        validate_event_contract,
    };

    use super::*;

    fn request() -> ModelRequest {
        ModelRequest {
            model_id: ModelId::from("deterministic"),
            instructions: "stable instructions".to_owned(),
            input: vec![ModelInputItem::Message {
                role: ModelMessageRole::User,
                content: "hello".to_owned(),
            }],
            tools: vec![],
            reasoning: ReasoningLevel::Low,
            response_format: ResponseFormat::Text,
            max_output_tokens: 100,
            previous_response_id: None,
            prompt_cache: None,
            store: false,
            timeout: Duration::from_secs(10),
        }
    }

    fn usage() -> ModelUsage {
        ModelUsage {
            input_tokens: 20,
            cached_input_tokens: 10,
            cache_write_tokens: 0,
            output_tokens: 5,
            reasoning_tokens: 2,
            total_tokens: 25,
        }
    }

    #[test]
    fn contract_streams_text_usage_and_completion() {
        let provider =
            FakeModelProvider::standard(vec![FakeScenario::text(&["hel", "lo"], usage())]);
        let events = provider
            .stream(request(), CancellationToken::new())
            .expect("stream")
            .collect::<Result<Vec<_>, _>>()
            .expect("events");
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    ModelEvent::TextDelta { delta } => Some(delta.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "hello"
        );
        assert!(matches!(events.last(), Some(ModelEvent::Completed { .. })));
        validate_event_contract(&events).expect("shared contract");
    }

    #[test]
    fn contract_normalizes_tool_call_and_result_continuation() {
        let provider = FakeModelProvider::standard(vec![FakeScenario::tool(
            "read_file",
            serde_json::json!({"path":"src/main.rs"}),
            usage(),
        )]);
        let mut with_tool = request();
        with_tool.tools.push(ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read one file".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
            strict: true,
        });
        let events = provider
            .stream(with_tool, CancellationToken::new())
            .expect("stream")
            .collect::<Result<Vec<_>, _>>()
            .expect("events");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCall { call_id, name, .. }
                if call_id.as_str() == "call:fake" && name == "read_file"
        )));

        let provider = FakeModelProvider::standard(vec![FakeScenario::text(&["done"], usage())]);
        let mut continuation = request();
        continuation.previous_response_id = Some(ResponseId::from("response:fake-tool"));
        continuation.input = vec![ModelInputItem::ToolResult {
            call_id: ToolCallId::from("call:fake"),
            output: serde_json::json!({"content":"fn main() {}"}),
        }];
        assert!(
            provider
                .stream(continuation, CancellationToken::new())
                .is_ok()
        );
    }

    #[test]
    fn contract_cancellation_context_auth_rate_limit_and_timeout_are_typed() {
        let provider = FakeModelProvider::standard(vec![FakeScenario::text(&["late"], usage())]);
        let cancellation = CancellationToken::new();
        let mut stream = provider
            .stream(request(), cancellation.clone())
            .expect("stream");
        cancellation.cancel();
        assert_eq!(
            stream
                .next()
                .expect("cancel event")
                .expect_err("cancelled")
                .kind,
            ModelErrorKind::Cancelled
        );

        let provider = FakeModelProvider::standard(vec![]);
        let mut oversized = request();
        oversized.input = vec![ModelInputItem::Message {
            role: ModelMessageRole::User,
            content: "x".repeat(40_000),
        }];
        assert_eq!(
            provider
                .stream(oversized, CancellationToken::new())
                .err()
                .expect("context error")
                .kind,
            ModelErrorKind::ContextLimit
        );

        for kind in [
            ModelErrorKind::Auth,
            ModelErrorKind::RateLimit,
            ModelErrorKind::Timeout,
        ] {
            let provider = FakeModelProvider::standard(vec![FakeScenario::error(ModelError::new(
                kind,
                "expected-error",
                "redacted",
            ))]);
            let error = provider
                .stream(request(), CancellationToken::new())
                .expect("stream created")
                .next()
                .expect("error event")
                .expect_err("expected error");
            assert_eq!(error.kind, kind);
        }
    }

    #[test]
    fn invalid_usage_is_rejected_at_stream_boundary() {
        let provider = FakeModelProvider::standard(vec![FakeScenario::text(
            &["bad"],
            ModelUsage {
                input_tokens: 2,
                cached_input_tokens: 3,
                output_tokens: 1,
                total_tokens: 3,
                ..ModelUsage::default()
            },
        )]);
        let error = provider
            .stream(request(), CancellationToken::new())
            .expect("stream")
            .find_map(Result::err)
            .expect("usage error");
        assert_eq!(error.code, "usage-cached-exceeds-input");
    }
}
