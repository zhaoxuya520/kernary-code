use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use harness_types::{ModelId, ProviderId};

use crate::{ModelCapability, ModelError, ModelErrorKind, ModelEvent, ModelRequest};

/// 可跨线程共享的协作式取消令牌。
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

pub type ModelEventStream = Box<dyn Iterator<Item = Result<ModelEvent, ModelError>> + Send>;

/// Model Provider 的唯一同步 Port。
///
/// 网络 Adapter 可在内部线程执行 I/O；调用方只消费 typed stream。
pub trait ModelProvider: Send + Sync {
    fn provider_id(&self) -> ProviderId;

    fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError>;

    /// 显式模型目录刷新钩子。默认 Provider 没有网络目录，因此返回 `None`。
    ///
    /// 可刷新 Provider 返回一个完整 replacement；Registry 会先验证 replacement，
    /// 再原子替换旧 Provider，避免失败刷新污染当前 Session。
    fn refresh(&self) -> Result<Option<Arc<dyn ModelProvider>>, ModelError> {
        Ok(None)
    }

    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError>;
}

/// 同一展示 Provider 可以按 model 路由到不同 wire protocol Adapter。
///
/// OpenCode Zen/Go 这类网关同时提供 Responses、Chat Completions 和 Messages；
/// Registry 仍只看到一个 Provider ID，具体协议不会污染 Kernel/Agent。
pub struct ProtocolMuxProvider {
    provider_id: ProviderId,
    capabilities: Vec<ModelCapability>,
    routes: BTreeMap<ModelId, Arc<dyn ModelProvider>>,
}

impl ProtocolMuxProvider {
    pub fn new(
        provider_id: ProviderId,
        routes: Vec<(ModelId, Arc<dyn ModelProvider>)>,
    ) -> Result<Self, ModelError> {
        if provider_id.as_str().trim().is_empty() || routes.is_empty() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "protocol-mux-empty",
                provider_id.to_string(),
            ));
        }
        let mut indexed = BTreeMap::new();
        let mut capabilities = Vec::with_capacity(routes.len());
        for (model_id, provider) in routes {
            if indexed.contains_key(&model_id) {
                return Err(ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "protocol-mux-model-conflict",
                    model_id.to_string(),
                ));
            }
            let mut capability = provider
                .capabilities()?
                .into_iter()
                .find(|candidate| candidate.model_id == model_id)
                .ok_or_else(|| {
                    ModelError::new(
                        ModelErrorKind::InvalidRequest,
                        "protocol-mux-capability-missing",
                        model_id.to_string(),
                    )
                })?;
            capability.provider_id = provider_id.clone();
            capabilities.push(capability);
            indexed.insert(model_id, provider);
        }
        capabilities.sort_by(|left, right| left.model_id.cmp(&right.model_id));
        Ok(Self {
            provider_id,
            capabilities,
            routes: indexed,
        })
    }
}

impl ModelProvider for ProtocolMuxProvider {
    fn provider_id(&self) -> ProviderId {
        self.provider_id.clone()
    }

    fn capabilities(&self) -> Result<Vec<ModelCapability>, ModelError> {
        Ok(self.capabilities.clone())
    }

    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.routes
            .get(&request.model_id)
            .ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "protocol-mux-model-not-routable",
                    request.model_id.to_string(),
                )
            })?
            .stream(request, cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeModelProvider;

    #[test]
    fn protocol_mux_rewrites_public_provider_and_rejects_duplicate_model_routes() {
        let child: Arc<dyn ModelProvider> = Arc::new(FakeModelProvider::echo());
        let model = ModelId::from("deterministic");
        let mux = ProtocolMuxProvider::new(
            ProviderId::from("opencode-go"),
            vec![(model.clone(), child.clone())],
        )
        .expect("mux");
        let capability = mux.capabilities().expect("capabilities").remove(0);
        assert_eq!(capability.provider_id, ProviderId::from("opencode-go"));
        assert_eq!(capability.model_id, model);

        let duplicate = ProtocolMuxProvider::new(
            ProviderId::from("opencode-go"),
            vec![
                (ModelId::from("deterministic"), child.clone()),
                (ModelId::from("deterministic"), child),
            ],
        )
        .err()
        .expect("duplicate rejected");
        assert_eq!(duplicate.code, "protocol-mux-model-conflict");
    }
}
