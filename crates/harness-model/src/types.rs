use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use harness_types::{ModelId, ProviderId, ReasoningLevel, ResponseId, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Reasoning capability 映射结果，降级必须可见。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningMapping {
    Exact,
    ClampedDown,
    ClampedUp,
    UnsupportedIgnored,
}

/// Provider-neutral 模型能力。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCapability {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub streaming: bool,
    pub tool_calling: bool,
    pub structured_output: bool,
    pub image_input: bool,
    pub prompt_cache_metrics: bool,
    pub conversation_continuation: bool,
    pub provider_compaction: bool,
    pub context_window_tokens: u32,
    pub max_output_tokens: u32,
    /// Provider 是否能像 Codex Responses/Claude 一样流式返回可公开的推理摘要。
    /// 旧模型缓存缺少此字段时按 false 处理，绝不把私有思维链冒充摘要。
    #[serde(default)]
    pub reasoning_summary: bool,
    pub reasoning_levels: BTreeSet<ReasoningLevel>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelMessageRole {
    Developer,
    User,
    Assistant,
}

/// Responses 风格 typed input；不是松散的 `messages[]`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ModelInputItem {
    Message {
        role: ModelMessageRole,
        content: String,
    },
    ToolResult {
        call_id: ToolCallId,
        output: serde_json::Value,
    },
    ToolCall {
        call_id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub strict: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ResponseFormat {
    Text,
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        strict: bool,
    },
}

/// Provider-neutral Prompt Cache 策略。
///
/// `stable_prefix` 必须是 `ModelRequest.instructions` 的完整前缀；动态任务数据只能放在
/// 该边界之后。Provider 可以据此生成原生 cache breakpoint，而无需理解 Harness Prompt。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptCachePolicy {
    /// 不含用户、Session 或 Run 明文的稳定散列；满足 OpenAI 64 字符上限。
    pub key: String,
    /// 可跨任务复用、字节级稳定的指令前缀。
    pub stable_prefix: String,
}

impl PromptCachePolicy {
    /// 从稳定指令和有序 Tool ABI 生成可跨 Session 复用的缓存身份。
    pub fn for_request(
        stable_prefix: impl Into<String>,
        tools: &[ToolDefinition],
    ) -> Result<Self, ModelError> {
        let stable_prefix = stable_prefix.into();
        if stable_prefix.trim().is_empty() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "prompt-cache-prefix-empty",
                "Prompt Cache 稳定前缀不能为空",
            ));
        }
        let mut tools = tools.to_vec();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        let tools = tools
            .into_iter()
            .map(|tool| {
                serde_json::json!({
                    "description": tool.description,
                    "inputSchema": canonical_json(&tool.input_schema),
                    "name": tool.name,
                    "strict": tool.strict,
                })
            })
            .collect::<Vec<_>>();
        let abi = serde_json::to_vec(&serde_json::json!({
            "schema": "kernary-prompt-cache-v2",
            "stablePrefix": stable_prefix,
            "tools": tools,
        }))
        .map_err(|error| {
            ModelError::new(
                ModelErrorKind::InvalidRequest,
                "prompt-cache-key-json",
                error.to_string(),
            )
        })?;
        let key = format!("{:x}", Sha256::digest(abi));
        Ok(Self { key, stable_prefix })
    }

    /// 验证缓存边界没有越过动态内容，并返回边界之后的尾部。
    pub fn dynamic_tail<'a>(&self, instructions: &'a str) -> Result<&'a str, ModelError> {
        if self.key.len() > 64 || self.key.is_empty() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "prompt-cache-key-invalid",
                "Prompt Cache key 必须为 1..64 字符",
            ));
        }
        let Some(tail) = instructions.strip_prefix(&self.stable_prefix) else {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "prompt-cache-prefix-mismatch",
                "Prompt Cache 稳定前缀与 instructions 不一致",
            ));
        };
        Ok(tail.strip_prefix('\n').unwrap_or(tail))
    }
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            let mut canonical = Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonical_json(&object[key]));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

/// 一次 Provider 调用的完整、可验证请求。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelRequest {
    pub model_id: ModelId,
    pub instructions: String,
    pub input: Vec<ModelInputItem>,
    pub tools: Vec<ToolDefinition>,
    pub reasoning: ReasoningLevel,
    pub response_format: ResponseFormat,
    pub max_output_tokens: u32,
    pub previous_response_id: Option<ResponseId>,
    /// `None` 表示禁用 Harness 主动缓存控制；Provider 自带的隐式缓存仍可工作。
    pub prompt_cache: Option<PromptCachePolicy>,
    pub store: bool,
    #[serde(with = "duration_millis")]
    pub timeout: Duration,
}

/// 标准化 Token 与 Prompt Cache 用量。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

impl ModelUsage {
    #[must_use]
    pub fn prompt_cache_hit_rate_percent(self) -> Option<u8> {
        (self.input_tokens != 0).then(|| {
            u8::try_from(self.cached_input_tokens.saturating_mul(100) / self.input_tokens)
                .unwrap_or(100)
        })
    }

    pub fn validate(self) -> Result<Self, ModelError> {
        if self.cached_input_tokens > self.input_tokens {
            return Err(ModelError::new(
                ModelErrorKind::Protocol,
                "usage-cached-exceeds-input",
                "cached input tokens 不能超过 input tokens",
            ));
        }
        let expected = self.input_tokens.saturating_add(self.output_tokens);
        if self.total_tokens != expected {
            return Err(ModelError::new(
                ModelErrorKind::Protocol,
                "usage-total-mismatch",
                format!("expected={expected}, actual={}", self.total_tokens),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompletionStatus {
    Completed,
    Incomplete,
}

/// 所有 Provider stream 都归一化为这些公开事件；不暴露隐藏 Chain-of-Thought。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ModelEvent {
    Started {
        response_id: ResponseId,
        model_id: ModelId,
    },
    TextDelta {
        delta: String,
    },
    ReasoningSummaryDelta {
        delta: String,
    },
    ToolCall {
        call_id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    Usage {
        usage: ModelUsage,
    },
    Completed {
        status: CompletionStatus,
        incomplete_reason: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelErrorKind {
    Auth,
    RateLimit,
    ContextLimit,
    Timeout,
    Cancelled,
    InvalidRequest,
    Transport,
    Provider,
    Protocol,
}

/// Provider 错误不包含 secret、完整请求体或原始敏感 Header。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
}

impl ModelError {
    #[must_use]
    pub fn new(kind: ModelErrorKind, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
            retryable: matches!(
                kind,
                ModelErrorKind::RateLimit | ModelErrorKind::Timeout | ModelErrorKind::Transport
            ),
            retry_after: None,
        }
    }

    #[must_use]
    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }
}

impl Display for ModelError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ModelError {}

mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, schema: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.to_owned(),
            description: format!("{name} tool"),
            input_schema: schema,
            strict: true,
        }
    }

    #[test]
    fn prompt_cache_key_is_stable_across_tool_and_json_object_order() {
        let first = PromptCachePolicy::for_request(
            "stable",
            &[
                tool("z", serde_json::json!({"b":2,"a":1})),
                tool("a", serde_json::json!({"type":"object"})),
            ],
        )
        .expect("first");
        let second = PromptCachePolicy::for_request(
            "stable",
            &[
                tool("a", serde_json::json!({"type":"object"})),
                tool("z", serde_json::json!({"a":1,"b":2})),
            ],
        )
        .expect("second");
        assert_eq!(first.key, second.key);
        assert_eq!(first.key.len(), 64);
    }

    #[test]
    fn prompt_cache_boundary_rejects_mismatch_and_returns_dynamic_tail() {
        let policy = PromptCachePolicy::for_request("stable", &[]).expect("policy");
        assert_eq!(
            policy.dynamic_tail("stable\ndynamic").expect("tail"),
            "dynamic"
        );
        let error = policy.dynamic_tail("changed").expect_err("mismatch");
        assert_eq!(error.code, "prompt-cache-prefix-mismatch");
    }

    #[test]
    fn prompt_cache_hit_rate_is_normalized() {
        let usage = ModelUsage {
            input_tokens: 200,
            cached_input_tokens: 150,
            ..ModelUsage::default()
        };
        assert_eq!(usage.prompt_cache_hit_rate_percent(), Some(75));
        assert_eq!(ModelUsage::default().prompt_cache_hit_rate_percent(), None);
    }
}
