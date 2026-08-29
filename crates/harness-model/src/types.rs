use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use harness_types::{ModelId, ProviderId, ReasoningLevel, ResponseId, ToolCallId};
use serde::{Deserialize, Serialize};

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
