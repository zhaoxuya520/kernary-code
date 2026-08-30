use std::io::Read;
use std::time::Duration;

use harness_model::{
    ModelError, ModelErrorKind, ModelInputItem, ModelMessageRole, ModelRequest, ReasoningLevel,
    ResponseFormat,
};

use crate::OpenAiHttpResponse;

/// 把 Provider-neutral request 翻译为官方 Responses API JSON。
pub fn build_responses_request(
    request: &ModelRequest,
    effective_reasoning: Option<ReasoningLevel>,
) -> Result<serde_json::Value, ModelError> {
    let input = request
        .input
        .iter()
        .map(|item| match item {
            ModelInputItem::Message { role, content } => Ok(serde_json::json!({
                "type": "message",
                "role": role_name(*role),
                "content": [{
                    "type": if *role == ModelMessageRole::Assistant {
                        "output_text"
                    } else {
                        "input_text"
                    },
                    "text": content
                }]
            })),
            ModelInputItem::ToolResult { call_id, output } => {
                let output = serde_json::to_string(output).map_err(|error| {
                    ModelError::new(
                        ModelErrorKind::InvalidRequest,
                        "tool-result-json",
                        error.to_string(),
                    )
                })?;
                Ok(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output
                }))
            }
            ModelInputItem::ToolCall {
                call_id,
                name,
                arguments,
            } => Ok(serde_json::json!({
                "type":"function_call",
                "call_id":call_id,
                "name":name,
                "arguments":serde_json::to_string(arguments).map_err(|error| ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "tool-call-json",
                    error.to_string()
                ))?
            })),
        })
        .collect::<Result<Vec<_>, ModelError>>()?;
    let mut tools = request.tools.clone();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    if tools.windows(2).any(|pair| pair[0].name == pair[1].name) {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "duplicate-tool-name",
            "Tool 名称必须唯一",
        ));
    }
    let tools = tools
        .into_iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": tool.strict
            })
        })
        .collect::<Vec<_>>();
    let text = match &request.response_format {
        ResponseFormat::Text => serde_json::json!({"format":{"type":"text"}}),
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => serde_json::json!({
            "format": {
                "type": "json_schema",
                "name": name,
                "schema": schema,
                "strict": strict
            }
        }),
    };
    let mut body = serde_json::json!({
        "model": request.model_id,
        "instructions": request.instructions,
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "max_output_tokens": request.max_output_tokens,
        "text": text,
        "stream": true,
        "store": request.store
    });
    if let Some(reasoning) = effective_reasoning {
        body["reasoning"] = serde_json::json!({"effort": reasoning_name(reasoning)});
    }
    if let Some(previous_response_id) = &request.previous_response_id {
        body["previous_response_id"] = serde_json::json!(previous_response_id);
    }
    if let Some(prompt_cache) = &request.prompt_cache {
        // 先验证 ABI 边界，避免把动态任务误标为稳定前缀。
        let _ = prompt_cache.dynamic_tail(&request.instructions)?;
        body["prompt_cache_key"] = serde_json::json!(prompt_cache.key);
        if request.model_id.as_str().starts_with("gpt-5.6") {
            // GPT-5.6+ 明确声明隐式断点；旧模型只发送兼容的 cache key。
            body["prompt_cache_options"] = serde_json::json!({
                "mode": "implicit",
                "ttl": "30m"
            });
        }
    }
    Ok(body)
}

const fn role_name(role: ModelMessageRole) -> &'static str {
    match role {
        ModelMessageRole::Developer => "developer",
        ModelMessageRole::User => "user",
        ModelMessageRole::Assistant => "assistant",
    }
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

/// HTTP 非 2xx → 稳定 ModelError；原始 Header/Body 不向上透传。
pub fn map_http_error(mut response: OpenAiHttpResponse) -> ModelError {
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
        .unwrap_or("openai-http-error");
    let kind = match response.status {
        401 | 403 => ModelErrorKind::Auth,
        408 | 504 => ModelErrorKind::Timeout,
        429 => ModelErrorKind::RateLimit,
        400 if provider_code.contains("context") => ModelErrorKind::ContextLimit,
        400..=499 => ModelErrorKind::InvalidRequest,
        _ => ModelErrorKind::Provider,
    };
    let mut error = ModelError::new(
        kind,
        format!("openai-{provider_code}"),
        format!("OpenAI HTTP {} ({provider_code})", response.status),
    );
    if response.status >= 500 {
        error.retryable = true;
    }
    if response.status == 429
        && let Some(seconds) = response
            .headers
            .get("retry-after")
            .and_then(|value| value.parse::<u64>().ok())
    {
        error.retry_after = Some(Duration::from_secs(seconds));
    }
    error
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use harness_model::{
        ModelInputItem, ModelMessageRole, PromptCachePolicy, ReasoningLevel, ResponseFormat,
        ToolDefinition,
    };
    use harness_types::{ModelId, ResponseId, ToolCallId};

    use super::*;

    fn request() -> ModelRequest {
        ModelRequest {
            model_id: ModelId::from("gpt-test"),
            instructions: "stable".to_owned(),
            input: vec![
                ModelInputItem::Message {
                    role: ModelMessageRole::User,
                    content: "hello".to_owned(),
                },
                ModelInputItem::ToolResult {
                    call_id: ToolCallId::from("call:1"),
                    output: serde_json::json!({"ok":true}),
                },
            ],
            tools: vec![ToolDefinition {
                name: "z_tool".to_owned(),
                description: "z".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
                strict: true,
            }],
            reasoning: ReasoningLevel::Off,
            response_format: ResponseFormat::Text,
            max_output_tokens: 100,
            previous_response_id: Some(ResponseId::from("response:1")),
            prompt_cache: None,
            store: false,
            timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn request_uses_typed_items_and_off_maps_to_none() {
        let body = build_responses_request(&request(), Some(ReasoningLevel::Off)).expect("body");
        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["previous_response_id"], "response:1");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
    }

    #[test]
    fn gpt_56_request_uses_stable_cache_key_and_implicit_breakpoint() {
        let mut request = request();
        request.model_id = ModelId::from("gpt-5.6");
        request.prompt_cache = Some(
            PromptCachePolicy::for_request(&request.instructions, &request.tools)
                .expect("cache policy"),
        );
        let body = build_responses_request(&request, Some(ReasoningLevel::Off)).expect("body");
        assert_eq!(
            body["prompt_cache_key"],
            request.prompt_cache.as_ref().expect("policy").key
        );
        assert_eq!(body["prompt_cache_options"]["mode"], "implicit");
        assert_eq!(body["prompt_cache_options"]["ttl"], "30m");
    }

    #[test]
    fn http_auth_and_rate_limit_are_typed_without_raw_body() {
        let auth = map_http_error(OpenAiHttpResponse {
            status: 401,
            headers: Default::default(),
            body: Box::new(std::io::BufReader::new(std::io::Cursor::new(
                br#"{"error":{"code":"invalid_api_key","message":"bad key"}}"#.to_vec(),
            ))),
        });
        assert_eq!(auth.kind, ModelErrorKind::Auth);
        assert!(!auth.message.contains("sk-"));

        let rate = map_http_error(OpenAiHttpResponse {
            status: 429,
            headers: [("retry-after".to_owned(), "7".to_owned())]
                .into_iter()
                .collect(),
            body: Box::new(std::io::BufReader::new(std::io::Cursor::new(
                br#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#.to_vec(),
            ))),
        });
        assert_eq!(rate.kind, ModelErrorKind::RateLimit);
        assert_eq!(rate.retry_after, Some(Duration::from_secs(7)));
        assert!(rate.retryable);
    }
}
