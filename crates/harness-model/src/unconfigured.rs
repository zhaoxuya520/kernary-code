use harness_types::{ModelId, ProviderId, ReasoningLevel};

use crate::{
    CancellationToken, ModelCapability, ModelError, ModelErrorKind, ModelEventStream,
    ModelProvider, ModelRequest,
};

pub const UNCONFIGURED_PROVIDER_ID: &str = "kernary-internal";
pub const UNCONFIGURED_MODEL_ID: &str = "unconfigured";

/// ModelRuntime 启动所需的显式“未配置”状态。
///
/// 它不是测试模型，也不生成任何文本或 Usage；所有调用都稳定失败。
pub struct UnconfiguredModelProvider;

impl ModelProvider for UnconfiguredModelProvider {
    fn provider_id(&self) -> ProviderId {
        ProviderId::from(UNCONFIGURED_PROVIDER_ID)
    }

    fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
        Ok(vec![ModelCapability {
            provider_id: ProviderId::from(UNCONFIGURED_PROVIDER_ID),
            model_id: ModelId::from(UNCONFIGURED_MODEL_ID),
            streaming: false,
            tool_calling: false,
            structured_output: false,
            image_input: false,
            prompt_cache_metrics: false,
            conversation_continuation: false,
            provider_compaction: false,
            context_window_tokens: 8_192,
            max_output_tokens: 2_048,
            reasoning_levels: [ReasoningLevel::Off].into_iter().collect(),
        }])
    }

    fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "model-not-configured",
            "MODEL_NOT_CONFIGURED: 先连接 Provider 并选择真实或本地模型",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{ModelInputItem, ResponseFormat};

    #[test]
    fn unconfigured_provider_never_generates_output() {
        let provider = UnconfiguredModelProvider;
        let error = provider
            .stream(
                ModelRequest {
                    model_id: ModelId::from(UNCONFIGURED_MODEL_ID),
                    instructions: String::new(),
                    input: vec![ModelInputItem::Message {
                        role: crate::ModelMessageRole::User,
                        content: "must not echo".to_owned(),
                    }],
                    tools: Vec::new(),
                    reasoning: ReasoningLevel::Off,
                    response_format: ResponseFormat::Text,
                    max_output_tokens: 32,
                    previous_response_id: None,
                    store: false,
                    timeout: Duration::from_secs(1),
                },
                CancellationToken::new(),
            )
            .err()
            .expect("unconfigured provider fails closed");
        assert_eq!(error.code, "model-not-configured");
        assert!(!error.retryable);
    }
}
