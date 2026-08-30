use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use harness_types::{ModelId, ProviderId};

use crate::{
    ModelCapability, ModelError, ModelProvider, ReasoningAdapter, ReasoningLevel,
    ReasoningResolution,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRegistryError {
    pub code: &'static str,
    pub message: String,
}

impl Display for ModelRegistryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ModelRegistryError {}

#[derive(Clone, Default)]
pub struct ModelRegistry {
    providers: BTreeMap<ProviderId, Arc<dyn ModelProvider>>,
    capabilities: BTreeMap<(ProviderId, ModelId), ModelCapability>,
}

impl ModelRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn ModelProvider>) -> Result<(), ModelRegistryError> {
        let provider_id = provider.provider_id();
        if self.providers.contains_key(&provider_id) {
            return Err(ModelRegistryError {
                code: "provider-conflict",
                message: provider_id.to_string(),
            });
        }
        let capabilities = provider
            .capabilities()
            .map_err(|error| registry_provider_error("provider-capabilities", error))?;
        if capabilities.is_empty() {
            return Err(ModelRegistryError {
                code: "provider-has-no-models",
                message: provider_id.to_string(),
            });
        }
        let mut validated = BTreeMap::new();
        for capability in capabilities {
            if capability.provider_id != provider_id {
                return Err(ModelRegistryError {
                    code: "capability-provider-mismatch",
                    message: capability.model_id.to_string(),
                });
            }
            let key = (provider_id.clone(), capability.model_id.clone());
            if self.capabilities.contains_key(&key) || validated.insert(key, capability).is_some() {
                return Err(ModelRegistryError {
                    code: "model-capability-conflict",
                    message: provider_id.to_string(),
                });
            }
        }
        self.capabilities.extend(validated);
        self.providers.insert(provider_id, provider);
        Ok(())
    }

    pub fn refresh(&mut self, provider_id: &ProviderId) -> Result<(), ModelRegistryError> {
        let current =
            self.providers
                .get(provider_id)
                .cloned()
                .ok_or_else(|| ModelRegistryError {
                    code: "provider-not-found",
                    message: provider_id.to_string(),
                })?;
        let provider = current
            .refresh()
            .map_err(|error| registry_provider_error("provider-refresh", error))?
            .unwrap_or(current);
        if provider.provider_id() != *provider_id {
            return Err(ModelRegistryError {
                code: "provider-refresh-id-mismatch",
                message: format!("expected={provider_id}, actual={}", provider.provider_id()),
            });
        }
        let capabilities = provider
            .capabilities()
            .map_err(|error| registry_provider_error("provider-capabilities", error))?;
        if capabilities.is_empty() {
            return Err(ModelRegistryError {
                code: "provider-has-no-models",
                message: provider_id.to_string(),
            });
        }
        let mut replacement = BTreeMap::new();
        for capability in capabilities {
            if capability.provider_id != *provider_id {
                return Err(ModelRegistryError {
                    code: "capability-provider-mismatch",
                    message: capability.model_id.to_string(),
                });
            }
            if replacement
                .insert(
                    (provider_id.clone(), capability.model_id.clone()),
                    capability,
                )
                .is_some()
            {
                return Err(ModelRegistryError {
                    code: "model-capability-conflict",
                    message: provider_id.to_string(),
                });
            }
        }
        self.capabilities
            .retain(|(candidate, _), _| candidate != provider_id);
        self.capabilities.extend(replacement);
        self.providers.insert(provider_id.clone(), provider);
        Ok(())
    }

    #[must_use]
    pub fn list(&self) -> Vec<ModelCapability> {
        self.capabilities.values().cloned().collect()
    }

    #[must_use]
    pub fn capability(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
    ) -> Option<&ModelCapability> {
        self.capabilities
            .get(&(provider_id.clone(), model_id.clone()))
    }

    #[must_use]
    pub fn provider(&self, provider_id: &ProviderId) -> Option<Arc<dyn ModelProvider>> {
        self.providers.get(provider_id).cloned()
    }
}

fn registry_provider_error(code: &'static str, error: ModelError) -> ModelRegistryError {
    ModelRegistryError {
        code,
        message: format!("{}: {}", error.code, error.message),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRouterError {
    pub code: &'static str,
    pub message: String,
}

impl Display for ModelRouterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ModelRouterError {}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModelRouter;

impl ModelRouter {
    pub fn resolve(
        &self,
        registry: &ModelRegistry,
        provider_id: &ProviderId,
        model_id: &ModelId,
        reasoning: ReasoningLevel,
        require_tools: bool,
        require_structured_output: bool,
    ) -> Result<(Arc<dyn ModelProvider>, ModelCapability, ReasoningResolution), ModelRouterError>
    {
        let capability = registry
            .capability(provider_id, model_id)
            .cloned()
            .ok_or_else(|| ModelRouterError {
                code: "model-not-found",
                message: format!("{provider_id}/{model_id}"),
            })?;
        if require_tools && !capability.tool_calling {
            return Err(ModelRouterError {
                code: "model-tool-calling-required",
                message: model_id.to_string(),
            });
        }
        if require_structured_output && !capability.structured_output {
            return Err(ModelRouterError {
                code: "model-structured-output-required",
                message: model_id.to_string(),
            });
        }
        let provider = registry
            .provider(provider_id)
            .ok_or_else(|| ModelRouterError {
                code: "provider-not-found",
                message: provider_id.to_string(),
            })?;
        let resolution = ReasoningAdapter.resolve(reasoning, &capability);
        Ok((provider, capability, resolution))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::{
        CancellationToken, FakeModelProvider, FakeScenario, ModelError, ModelEventStream,
        ModelRequest, ModelUsage, ReasoningMapping,
    };

    use super::*;

    #[test]
    fn explicit_route_never_silently_fails_over() {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(FakeModelProvider::standard(vec![
                FakeScenario::text(&["ok"], ModelUsage::default()),
            ])))
            .expect("register");
        let (_, capability, reasoning) = ModelRouter
            .resolve(
                &registry,
                &ProviderId::from("fake"),
                &ModelId::from("deterministic"),
                ReasoningLevel::Max,
                true,
                true,
            )
            .expect("route");
        assert_eq!(capability.provider_id, ProviderId::from("fake"));
        assert_eq!(reasoning.effective, Some(ReasoningLevel::High));
        assert_eq!(reasoning.mapping, ReasoningMapping::ClampedDown);

        let missing = ModelRouter
            .resolve(
                &registry,
                &ProviderId::from("other"),
                &ModelId::from("missing"),
                ReasoningLevel::Low,
                false,
                false,
            )
            .err()
            .expect("no implicit failover");
        assert_eq!(missing.code, "model-not-found");
    }

    struct BrokenReplacement;

    impl ModelProvider for BrokenReplacement {
        fn provider_id(&self) -> ProviderId {
            ProviderId::from("refreshable")
        }

        fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
            Ok(vec![ModelCapability {
                provider_id: ProviderId::from("wrong-provider"),
                model_id: ModelId::from("replacement"),
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
            }])
        }

        fn stream(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            unreachable!("测试不执行模型")
        }
    }

    struct RefreshableOriginal;

    impl ModelProvider for RefreshableOriginal {
        fn provider_id(&self) -> ProviderId {
            ProviderId::from("refreshable")
        }

        fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
            let mut capability = BrokenReplacement.capabilities()?.remove(0);
            capability.provider_id = ProviderId::from("refreshable");
            capability.model_id = ModelId::from("original");
            Ok(vec![capability])
        }

        fn refresh(&self) -> Result<Option<Arc<dyn ModelProvider>>, ModelError> {
            Ok(Some(Arc::new(BrokenReplacement)))
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
    fn invalid_refresh_is_atomic_and_preserves_original_provider() {
        let mut invalid_registry = ModelRegistry::new();
        let register_error = invalid_registry
            .register(Arc::new(BrokenReplacement))
            .expect_err("invalid initial provider rejected");
        assert_eq!(register_error.code, "capability-provider-mismatch");
        assert!(invalid_registry.list().is_empty());
        assert!(
            invalid_registry
                .provider(&ProviderId::from("refreshable"))
                .is_none()
        );

        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(RefreshableOriginal))
            .expect("register");
        let error = registry
            .refresh(&ProviderId::from("refreshable"))
            .expect_err("mismatched capability rejected");
        assert_eq!(error.code, "capability-provider-mismatch");
        assert!(
            registry
                .capability(&ProviderId::from("refreshable"), &ModelId::from("original"))
                .is_some()
        );
        assert!(
            registry
                .capability(
                    &ProviderId::from("refreshable"),
                    &ModelId::from("replacement")
                )
                .is_none()
        );
    }
}
