#![forbid(unsafe_code)]

//! Provider-neutral Model Runtime contracts。
//!
//! Kernel 不依赖本 crate；Application/Agent Runtime 通过这些 Port 调用模型。

mod contract;
mod fake;
mod provider;
mod reasoning;
mod registry;
mod runtime;
mod types;
mod unconfigured;

pub use contract::validate_event_contract;
pub use fake::{FakeModelProvider, FakeScenario};
pub use harness_types::ReasoningLevel;
pub use provider::{CancellationToken, ModelEventStream, ModelProvider, ProtocolMuxProvider};
pub use reasoning::{ReasoningAdapter, ReasoningResolution};
pub use registry::{ModelRegistry, ModelRegistryError, ModelRouter, ModelRouterError};
pub use runtime::{
    FailoverTarget, ModelRoutePolicy, ModelRuntime, ModelRuntimeError, ModelRuntimeView,
};
pub use types::{
    CompletionStatus, ModelCapability, ModelError, ModelErrorKind, ModelEvent, ModelInputItem,
    ModelMessageRole, ModelRequest, ModelUsage, PromptCachePolicy, ReasoningMapping,
    ResponseFormat, ToolDefinition,
};
pub use unconfigured::{
    UNCONFIGURED_MODEL_ID, UNCONFIGURED_PROVIDER_ID, UnconfiguredModelProvider,
};
