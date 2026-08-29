use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::BufRead;

use harness_model::{
    CancellationToken, CompletionStatus, ModelError, ModelErrorKind, ModelEvent, ModelUsage,
};
use harness_types::{ModelId, ResponseId, ToolCallId};

pub struct OpenAiSseStream {
    lines: std::io::Lines<Box<dyn BufRead + Send>>,
    cancellation: CancellationToken,
    queued: VecDeque<Result<ModelEvent, ModelError>>,
    data_lines: Vec<String>,
    state: StreamState,
    cancellation_emitted: bool,
    eof_emitted: bool,
}

#[derive(Default)]
struct StreamState {
    started: bool,
    terminal: bool,
    pending_calls: BTreeMap<String, (ToolCallId, String)>,
    emitted_calls: BTreeSet<ToolCallId>,
}

impl OpenAiSseStream {
    #[must_use]
    pub fn new(body: Box<dyn BufRead + Send>, cancellation: CancellationToken) -> Self {
        Self {
            lines: body.lines(),
            cancellation,
            queued: VecDeque::new(),
            data_lines: Vec::new(),
            state: StreamState::default(),
            cancellation_emitted: false,
            eof_emitted: false,
        }
    }

    fn flush_data(&mut self) {
        if self.data_lines.is_empty() {
            return;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        if data == "[DONE]" {
            if !self.state.terminal {
                self.queued.push_back(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "openai-stream-ended-before-terminal",
                    "SSE [DONE] 前没有 response.completed/incomplete",
                )));
            }
            self.state.terminal = true;
            return;
        }
        match serde_json::from_str::<serde_json::Value>(&data) {
            Ok(value) => {
                for event in parse_event(&mut self.state, &value) {
                    self.queued.push_back(event);
                }
            }
            Err(error) => self.queued.push_back(Err(ModelError::new(
                ModelErrorKind::Protocol,
                "openai-sse-json",
                error.to_string(),
            ))),
        }
    }
}

impl Iterator for OpenAiSseStream {
    type Item = Result<ModelEvent, ModelError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Some(event);
            }
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
            match self.lines.next() {
                Some(Ok(line)) if line.is_empty() => self.flush_data(),
                Some(Ok(line)) => {
                    if let Some(data) = line.strip_prefix("data:") {
                        self.data_lines.push(data.trim_start().to_owned());
                    }
                }
                Some(Err(error)) => {
                    return Some(Err(ModelError::new(
                        ModelErrorKind::Transport,
                        "openai-sse-read",
                        error.to_string(),
                    )));
                }
                None => {
                    self.flush_data();
                    if let Some(event) = self.queued.pop_front() {
                        return Some(event);
                    }
                    if !self.eof_emitted && !self.state.terminal {
                        self.eof_emitted = true;
                        return Some(Err(ModelError::new(
                            ModelErrorKind::Protocol,
                            "openai-stream-truncated",
                            "OpenAI SSE 在 terminal event 前结束",
                        )));
                    }
                    return None;
                }
            }
        }
    }
}

fn parse_event(
    state: &mut StreamState,
    value: &serde_json::Value,
) -> Vec<Result<ModelEvent, ModelError>> {
    let event_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    match event_type {
        "response.created" => {
            if state.started {
                return vec![protocol_error(
                    "openai-started-twice",
                    "response.created 重复",
                )];
            }
            let Some(response) = value.get("response") else {
                return vec![protocol_error(
                    "openai-response-missing",
                    "response.created 缺少 response",
                )];
            };
            let Some(id) = string_at(response, "/id") else {
                return vec![protocol_error(
                    "openai-response-id-missing",
                    "缺少 response.id",
                )];
            };
            let Some(model) = string_at(response, "/model") else {
                return vec![protocol_error(
                    "openai-model-missing",
                    "缺少 response.model",
                )];
            };
            state.started = true;
            vec![Ok(ModelEvent::Started {
                response_id: ResponseId::from(id),
                model_id: ModelId::from(model),
            })]
        }
        "response.output_text.delta" => started_event(
            state,
            ModelEvent::TextDelta {
                delta: string_at(value, "/delta").unwrap_or_default().to_owned(),
            },
        ),
        "response.reasoning_summary_text.delta" => started_event(
            state,
            ModelEvent::ReasoningSummaryDelta {
                delta: string_at(value, "/delta").unwrap_or_default().to_owned(),
            },
        ),
        "response.output_item.added" => {
            if let Some(item) = value.get("item")
                && string_at(item, "/type") == Some("function_call")
                && let (Some(item_id), Some(call_id), Some(name)) = (
                    string_at(item, "/id"),
                    string_at(item, "/call_id"),
                    string_at(item, "/name"),
                )
            {
                state.pending_calls.insert(
                    item_id.to_owned(),
                    (ToolCallId::from(call_id), name.to_owned()),
                );
            }
            vec![]
        }
        "response.function_call_arguments.done" => {
            let Some(item_id) = string_at(value, "/item_id") else {
                return vec![protocol_error(
                    "openai-tool-item-id-missing",
                    "缺少 item_id",
                )];
            };
            let Some((call_id, name)) = state.pending_calls.remove(item_id) else {
                return vec![protocol_error(
                    "openai-tool-call-missing",
                    "arguments.done 没有对应 function_call item",
                )];
            };
            tool_call_event(
                state,
                call_id,
                name,
                string_at(value, "/arguments").unwrap_or("{}"),
            )
        }
        "response.output_item.done" => {
            let Some(item) = value.get("item") else {
                return vec![];
            };
            if string_at(item, "/type") != Some("function_call") {
                return vec![];
            }
            let (Some(call_id), Some(name)) =
                (string_at(item, "/call_id"), string_at(item, "/name"))
            else {
                return vec![protocol_error(
                    "openai-tool-fields-missing",
                    "function_call 缺少 call_id/name",
                )];
            };
            tool_call_event(
                state,
                ToolCallId::from(call_id),
                name.to_owned(),
                string_at(item, "/arguments").unwrap_or("{}"),
            )
        }
        "response.completed" => terminal_events(state, value, CompletionStatus::Completed),
        "response.incomplete" => terminal_events(state, value, CompletionStatus::Incomplete),
        "response.failed" | "error" => {
            state.terminal = true;
            let provider_code = string_at(value, "/error/code")
                .or_else(|| string_at(value, "/code"))
                .unwrap_or("unknown");
            vec![Err(ModelError::new(
                ModelErrorKind::Provider,
                "openai-stream-error",
                format!("OpenAI stream returned an error ({provider_code})"),
            ))]
        }
        _ => vec![],
    }
}

fn started_event(state: &StreamState, event: ModelEvent) -> Vec<Result<ModelEvent, ModelError>> {
    if state.started {
        vec![Ok(event)]
    } else {
        vec![protocol_error(
            "openai-event-before-start",
            "Delta 出现在 response.created 之前",
        )]
    }
}

fn tool_call_event(
    state: &mut StreamState,
    call_id: ToolCallId,
    name: String,
    arguments: &str,
) -> Vec<Result<ModelEvent, ModelError>> {
    if !state.started {
        return vec![protocol_error(
            "openai-event-before-start",
            "Tool Call 出现在 response.created 之前",
        )];
    }
    if !state.emitted_calls.insert(call_id.clone()) {
        return vec![];
    }
    match serde_json::from_str(arguments) {
        Ok(arguments) => vec![Ok(ModelEvent::ToolCall {
            call_id,
            name,
            arguments,
        })],
        Err(error) => vec![Err(ModelError::new(
            ModelErrorKind::Protocol,
            "openai-tool-arguments-json",
            error.to_string(),
        ))],
    }
}

fn terminal_events(
    state: &mut StreamState,
    value: &serde_json::Value,
    status: CompletionStatus,
) -> Vec<Result<ModelEvent, ModelError>> {
    if state.terminal {
        return vec![protocol_error(
            "openai-terminal-twice",
            "Terminal event 重复",
        )];
    }
    state.terminal = true;
    let response = value.get("response").unwrap_or(value);
    let mut events = Vec::new();
    if let Some(usage) = response.get("usage") {
        events.push(parse_usage(usage).map(|usage| ModelEvent::Usage { usage }));
    }
    let incomplete_reason = response
        .pointer("/incomplete_details/reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    events.push(Ok(ModelEvent::Completed {
        status,
        incomplete_reason,
    }));
    events
}

fn parse_usage(value: &serde_json::Value) -> Result<ModelUsage, ModelError> {
    let usage = ModelUsage {
        input_tokens: u64_at(value, "/input_tokens"),
        cached_input_tokens: u64_at(value, "/input_tokens_details/cached_tokens"),
        cache_write_tokens: u64_at(value, "/input_tokens_details/cache_write_tokens"),
        output_tokens: u64_at(value, "/output_tokens"),
        reasoning_tokens: u64_at(value, "/output_tokens_details/reasoning_tokens"),
        total_tokens: u64_at(value, "/total_tokens"),
    };
    usage.validate()
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

fn protocol_error(code: &'static str, message: &'static str) -> Result<ModelEvent, ModelError> {
    Err(ModelError::new(ModelErrorKind::Protocol, code, message))
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    fn stream(events: &[serde_json::Value]) -> OpenAiSseStream {
        let bytes = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            .into_bytes();
        OpenAiSseStream::new(
            Box::new(BufReader::new(Cursor::new(bytes))),
            CancellationToken::new(),
        )
    }

    #[test]
    fn typed_sse_normalizes_text_tool_usage_and_completion() {
        let events = stream(&[
            serde_json::json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-test"}}),
            serde_json::json!({"type":"response.output_text.delta","delta":"hello"}),
            serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read_file"}}),
            serde_json::json!({"type":"response.function_call_arguments.done","item_id":"item_1","arguments":"{\"path\":\"src/main.rs\"}"}),
            serde_json::json!({
                "type":"response.completed",
                "response":{
                    "usage":{
                        "input_tokens":20,
                        "input_tokens_details":{"cached_tokens":10,"cache_write_tokens":2},
                        "output_tokens":5,
                        "output_tokens_details":{"reasoning_tokens":2},
                        "total_tokens":25
                    }
                }
            }),
        ])
        .collect::<Result<Vec<_>, _>>()
        .expect("events");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCall { call_id, .. } if call_id.as_str() == "call_1"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage { usage } if usage.cached_input_tokens == 10
        )));
        assert!(matches!(events.last(), Some(ModelEvent::Completed { .. })));
    }

    #[test]
    fn truncated_and_malformed_streams_fail_closed() {
        let truncated = stream(&[serde_json::json!({
            "type":"response.created",
            "response":{"id":"resp_1","model":"gpt-test"}
        })])
        .find_map(Result::err)
        .expect("truncated error");
        assert_eq!(truncated.code, "openai-stream-truncated");

        let bytes = b"data: not-json\n\n".to_vec();
        let malformed = OpenAiSseStream::new(
            Box::new(BufReader::new(Cursor::new(bytes))),
            CancellationToken::new(),
        )
        .next()
        .expect("event")
        .expect_err("malformed");
        assert_eq!(malformed.code, "openai-sse-json");
    }
}
