use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use harness_types::{ModelId, ProviderId};

use crate::{
    CancellationToken, ModelCapability, ModelEventStream, ModelProvider, ModelRegistry,
    ModelRequest, ModelRouter, ReasoningLevel, ReasoningMapping,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRuntimeView {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub reasoning_requested: ReasoningLevel,
    pub reasoning_effective: Option<ReasoningLevel>,
    pub reasoning_mapping: ReasoningMapping,
    pub capability: ModelCapability,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRuntimeError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailoverTarget {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
}

/// Failover 默认关闭；开启时必须精确 allowlist 且确认成本范围。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelRoutePolicy {
    pub enabled: bool,
    pub user_confirmed_cost_scope: bool,
    pub allowlist: Vec<FailoverTarget>,
}

impl Display for ModelRuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ModelRuntimeError {}

/// 当前 Session 的显式 Provider/Model/Reasoning 选择。
#[derive(Clone)]
pub struct ModelRuntime {
    registry: ModelRegistry,
    provider_id: ProviderId,
    model_id: ModelId,
    reasoning: ReasoningLevel,
    failover_policy: ModelRoutePolicy,
}

impl ModelRuntime {
    pub fn new(
        registry: ModelRegistry,
        provider_id: ProviderId,
        model_id: ModelId,
        reasoning: ReasoningLevel,
    ) -> Result<Self, ModelRuntimeError> {
        let runtime = Self {
            registry,
            provider_id,
            model_id,
            reasoning,
            failover_policy: ModelRoutePolicy::default(),
        };
        runtime.view()?;
        Ok(runtime)
    }

    pub fn select(
        &mut self,
        provider_id: ProviderId,
        model_id: ModelId,
    ) -> Result<ModelRuntimeView, ModelRuntimeError> {
        ModelRouter
            .resolve(
                &self.registry,
                &provider_id,
                &model_id,
                self.reasoning,
                false,
                false,
            )
            .map_err(runtime_router_error)?;
        self.provider_id = provider_id;
        self.model_id = model_id;
        self.view()
    }

    pub fn set_reasoning(
        &mut self,
        reasoning: ReasoningLevel,
    ) -> Result<ModelRuntimeView, ModelRuntimeError> {
        self.reasoning = reasoning;
        self.view()
    }

    pub fn set_failover_policy(
        &mut self,
        policy: ModelRoutePolicy,
    ) -> Result<(), ModelRuntimeError> {
        if policy.enabled && !policy.user_confirmed_cost_scope {
            return Err(ModelRuntimeError {
                code: "failover-cost-not-authorized".to_owned(),
                message: "启用 Failover 前必须确认成本范围".to_owned(),
            });
        }
        for target in &policy.allowlist {
            ModelRouter
                .resolve(
                    &self.registry,
                    &target.provider_id,
                    &target.model_id,
                    self.reasoning,
                    false,
                    false,
                )
                .map_err(runtime_router_error)?;
        }
        self.failover_policy = policy;
        Ok(())
    }

    #[must_use]
    pub fn failover_policy(&self) -> ModelRoutePolicy {
        self.failover_policy.clone()
    }

    pub fn refresh_provider(&mut self, provider_id: &ProviderId) -> Result<(), ModelRuntimeError> {
        let mut candidate = self.registry.clone();
        candidate
            .refresh(provider_id)
            .map_err(|error| ModelRuntimeError {
                code: error.code.to_owned(),
                message: error.message,
            })?;
        ModelRouter
            .resolve(
                &candidate,
                &self.provider_id,
                &self.model_id,
                self.reasoning,
                false,
                false,
            )
            .map_err(runtime_router_error)?;
        self.registry = candidate;
        Ok(())
    }

    /// 原子加入一个运行时 Provider；失败时不污染现有 Registry。
    pub fn register_provider(
        &mut self,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<(), ModelRuntimeError> {
        let mut candidate = self.registry.clone();
        candidate
            .register(provider)
            .map_err(|error| ModelRuntimeError {
                code: error.code.to_owned(),
                message: error.message,
            })?;
        // 新 Provider 不能破坏当前选择或 Failover allowlist。
        ModelRouter
            .resolve(
                &candidate,
                &self.provider_id,
                &self.model_id,
                self.reasoning,
                false,
                false,
            )
            .map_err(runtime_router_error)?;
        for target in &self.failover_policy.allowlist {
            ModelRouter
                .resolve(
                    &candidate,
                    &target.provider_id,
                    &target.model_id,
                    self.reasoning,
                    false,
                    false,
                )
                .map_err(runtime_router_error)?;
        }
        self.registry = candidate;
        Ok(())
    }

    pub fn view(&self) -> Result<ModelRuntimeView, ModelRuntimeError> {
        let (_, capability, resolution) = ModelRouter
            .resolve(
                &self.registry,
                &self.provider_id,
                &self.model_id,
                self.reasoning,
                false,
                false,
            )
            .map_err(runtime_router_error)?;
        Ok(ModelRuntimeView {
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
            reasoning_requested: resolution.requested,
            reasoning_effective: resolution.effective,
            reasoning_mapping: resolution.mapping,
            capability,
        })
    }

    #[must_use]
    pub fn models(&self) -> Vec<ModelCapability> {
        self.registry.list()
    }

    pub fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelRuntimeError> {
        let require_tools = !request.tools.is_empty();
        let require_structured = !matches!(request.response_format, crate::ResponseFormat::Text);
        let mut targets = vec![FailoverTarget {
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
        }];
        if self.failover_policy.enabled {
            targets.extend(self.failover_policy.allowlist.clone());
        }
        let last_index = targets.len().saturating_sub(1);
        for (index, target) in targets.into_iter().enumerate() {
            let (provider, _, resolution) = ModelRouter
                .resolve(
                    &self.registry,
                    &target.provider_id,
                    &target.model_id,
                    self.reasoning,
                    require_tools,
                    require_structured,
                )
                .map_err(runtime_router_error)?;
            let mut attempt = request.clone();
            attempt.model_id = target.model_id;
            attempt.reasoning = resolution.effective.unwrap_or(ReasoningLevel::Off);
            match provider.stream(attempt, cancellation.clone()) {
                Ok(stream) => return Ok(stream),
                Err(error) if error.retryable && index < last_index => {}
                Err(error) => {
                    return Err(ModelRuntimeError {
                        code: error.code,
                        message: error.message,
                    });
                }
            }
        }
        Err(ModelRuntimeError {
            code: "model-route-exhausted".to_owned(),
            message: "没有可用 Model route".to_owned(),
        })
    }
}

fn runtime_router_error(error: crate::ModelRouterError) -> ModelRuntimeError {
    ModelRuntimeError {
        code: error.code.to_owned(),
        message: error.message,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use std::collections::BTreeSet;

    use crate::{
        FakeModelProvider, FakeScenario, ModelError, ModelErrorKind, ModelEventStream,
        ModelInputItem, ModelMessageRole, ModelProvider, ModelUsage, ResponseFormat,
    };
    use std::time::Duration;

    use super::*;

    #[test]
    fn model_and_reasoning_changes_are_validated_before_commit() {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(FakeModelProvider::standard(vec![
                FakeScenario::text(&["ok"], ModelUsage::default()),
            ])))
            .expect("register");
        let mut runtime = ModelRuntime::new(
            registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Medium,
        )
        .expect("runtime");
        let max = runtime
            .set_reasoning(ReasoningLevel::Max)
            .expect("reasoning");
        assert_eq!(max.reasoning_effective, Some(ReasoningLevel::High));
        assert_eq!(max.reasoning_mapping, ReasoningMapping::ClampedDown);
        let error = runtime
            .select(ProviderId::from("missing"), ModelId::from("missing"))
            .expect_err("missing");
        assert_eq!(error.code, "model-not-found");
        assert_eq!(
            runtime.view().expect("selection unchanged").provider_id,
            ProviderId::from("fake")
        );
    }

    struct ImmediateFailure;

    impl ModelProvider for ImmediateFailure {
        fn provider_id(&self) -> ProviderId {
            ProviderId::from("failing")
        }

        fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
            Ok(vec![ModelCapability {
                provider_id: ProviderId::from("failing"),
                model_id: ModelId::from("primary"),
                streaming: true,
                tool_calling: false,
                structured_output: false,
                image_input: false,
                prompt_cache_metrics: false,
                conversation_continuation: false,
                provider_compaction: false,
                context_window_tokens: 8_192,
                max_output_tokens: 1_024,
                reasoning_summary: false,
                reasoning_levels: BTreeSet::new(),
            }])
        }

        fn stream(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            Err(ModelError::new(
                ModelErrorKind::Transport,
                "primary-offline",
                "offline",
            ))
        }
    }

    #[test]
    fn runtime_provider_registration_is_atomic_and_preserves_selection() {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(FakeModelProvider::echo()))
            .expect("fake");
        let mut runtime = ModelRuntime::new(
            registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("runtime");
        runtime
            .register_provider(Arc::new(ImmediateFailure))
            .expect("register");
        assert!(runtime.models().iter().any(|model| {
            model.provider_id.as_str() == "failing" && model.model_id.as_str() == "primary"
        }));
        assert_eq!(
            runtime.view().expect("view").provider_id,
            ProviderId::from("fake")
        );
        let error = runtime
            .register_provider(Arc::new(ImmediateFailure))
            .expect_err("duplicate rejected");
        assert_eq!(error.code, "provider-conflict");
        assert_eq!(
            runtime.view().expect("unchanged").provider_id,
            ProviderId::from("fake")
        );
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model_id: ModelId::from("primary"),
            instructions: String::new(),
            input: vec![ModelInputItem::Message {
                role: ModelMessageRole::User,
                content: "hello".to_owned(),
            }],
            tools: vec![],
            reasoning: ReasoningLevel::Off,
            response_format: ResponseFormat::Text,
            max_output_tokens: 10,
            previous_response_id: None,
            prompt_cache: None,
            store: false,
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn failover_requires_cost_confirmation_and_exact_allowlist() {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(ImmediateFailure))
            .expect("primary");
        registry
            .register(Arc::new(FakeModelProvider::standard(vec![
                FakeScenario::text(&["fallback"], ModelUsage::default()),
            ])))
            .expect("fallback");
        let mut runtime = ModelRuntime::new(
            registry,
            ProviderId::from("failing"),
            ModelId::from("primary"),
            ReasoningLevel::Off,
        )
        .expect("runtime");
        assert_eq!(
            runtime
                .stream(request(), CancellationToken::new())
                .err()
                .expect("default off")
                .code,
            "primary-offline"
        );
        assert_eq!(
            runtime
                .set_failover_policy(ModelRoutePolicy {
                    enabled: true,
                    user_confirmed_cost_scope: false,
                    allowlist: vec![],
                })
                .expect_err("cost gate")
                .code,
            "failover-cost-not-authorized"
        );
        runtime
            .set_failover_policy(ModelRoutePolicy {
                enabled: true,
                user_confirmed_cost_scope: true,
                allowlist: vec![FailoverTarget {
                    provider_id: ProviderId::from("fake"),
                    model_id: ModelId::from("deterministic"),
                }],
            })
            .expect("policy");
        let events = runtime
            .stream(request(), CancellationToken::new())
            .expect("fallback")
            .collect::<Result<Vec<_>, _>>()
            .expect("events");
        assert!(events.iter().any(|event| matches!(
            event,
            crate::ModelEvent::TextDelta { delta } if delta == "fallback"
        )));
    }

    fn refresh_capability(model: &str) -> ModelCapability {
        ModelCapability {
            provider_id: ProviderId::from("refreshable"),
            model_id: ModelId::from(model),
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

    struct RefreshedWithoutSelected;

    impl ModelProvider for RefreshedWithoutSelected {
        fn provider_id(&self) -> ProviderId {
            ProviderId::from("refreshable")
        }

        fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
            Ok(vec![refresh_capability("other")])
        }

        fn stream(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            unreachable!("测试不执行模型")
        }
    }

    struct RefreshDropsSelected;

    impl ModelProvider for RefreshDropsSelected {
        fn provider_id(&self) -> ProviderId {
            ProviderId::from("refreshable")
        }

        fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
            Ok(vec![refresh_capability("selected")])
        }

        fn refresh(&self) -> Result<Option<Arc<dyn ModelProvider>>, ModelError> {
            Ok(Some(Arc::new(RefreshedWithoutSelected)))
        }

        fn stream(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            unreachable!("测试不执行模型")
        }
    }

    #[test]
    fn refresh_that_drops_current_selection_rolls_back_entire_registry() {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(RefreshDropsSelected))
            .expect("register");
        let mut runtime = ModelRuntime::new(
            registry,
            ProviderId::from("refreshable"),
            ModelId::from("selected"),
            ReasoningLevel::Off,
        )
        .expect("runtime");
        let error = runtime
            .refresh_provider(&ProviderId::from("refreshable"))
            .expect_err("selected model must survive refresh");
        assert_eq!(error.code, "model-not-found");
        assert_eq!(
            runtime.view().expect("old view").model_id,
            ModelId::from("selected")
        );
        assert!(
            runtime
                .models()
                .iter()
                .any(|model| model.model_id == ModelId::from("selected"))
        );
    }
}
