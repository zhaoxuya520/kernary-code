#![forbid(unsafe_code)]

//! 统一 Tool Registry/Runtime；Permission allow 后仍必须进入 SandboxPort。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use harness_permission::{
    ApprovalPolicy, ExecutionEnvelope, GrantScope, PermissionAction, PermissionDecision,
    PermissionEngine, PermissionRule,
};
use harness_types::{PermissionGrantId, PermissionRequestId, ToolInvocationId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolEffectClass {
    ReadOnlyRetryable,
    IdempotentEffect,
    VerifiableEffect,
    NonRepeatableEffect,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ToolSource {
    Builtin,
    Mcp { server_id: String },
    Plugin { plugin_id: String },
    Internal,
    Test,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolPromptLoading {
    Eager,
    OnDemand,
    UserOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolInvocationStatus {
    Requested,
    WaitingApproval,
    Running,
    Completed,
    Denied,
    Failed,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolDescriptor {
    pub canonical_name: String,
    pub version: String,
    pub description: String,
    pub effect_class: ToolEffectClass,
    pub source: ToolSource,
    pub prompt_loading: ToolPromptLoading,
    pub keywords: Vec<String>,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolInvocationRecord {
    pub id: ToolInvocationId,
    pub idempotency_key: String,
    pub envelope: ExecutionEnvelope,
    pub tool_name: String,
    pub tool_version: String,
    pub effect_class: ToolEffectClass,
    pub status: ToolInvocationStatus,
    pub args: serde_json::Value,
    pub permission_action: PermissionAction,
    pub approval_request_id: Option<PermissionRequestId>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub created_at_millis: i64,
    pub updated_at_millis: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolError {
    pub code: String,
    pub message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ToolError {}

#[derive(Clone, Debug, Default)]
pub struct ToolCancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl ToolCancellationToken {
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

#[derive(Clone, Debug)]
pub struct ToolExecutionInput {
    pub invocation_id: ToolInvocationId,
    pub envelope: ExecutionEnvelope,
    pub args: serde_json::Value,
    pub cancellation: ToolCancellationToken,
    pub now_millis: i64,
}

pub trait ToolProvider: Send + Sync {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError>;
    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError>;
    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError>;
    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError>;
}

/// Permission 与 Sandbox 的硬边界：Runtime 不直接执行 ToolProvider。
pub trait SandboxPort: Send + Sync {
    fn execute(
        &self,
        descriptor: &ToolDescriptor,
        permission_action: &PermissionAction,
        provider: &dyn ToolProvider,
        input: ToolExecutionInput,
    ) -> Result<serde_json::Value, ToolError>;
}

#[derive(Clone)]
pub struct RegisteredTool {
    pub descriptor: ToolDescriptor,
    pub provider: Arc<dyn ToolProvider>,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, RegisteredTool>>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        descriptor: ToolDescriptor,
        provider: Arc<dyn ToolProvider>,
    ) -> Result<(), ToolError> {
        if descriptor.canonical_name.trim().is_empty() || descriptor.version.trim().is_empty() {
            return Err(ToolError::new(
                "tool-descriptor-invalid",
                "Tool name/version 不能为空",
            ));
        }
        let mut tools = self
            .tools
            .write()
            .map_err(|_| ToolError::new("tool-registry-poisoned", "write"))?;
        if tools.contains_key(&descriptor.canonical_name) {
            return Err(ToolError::new(
                "tool-already-registered",
                descriptor.canonical_name,
            ));
        }
        tools.insert(
            descriptor.canonical_name.clone(),
            RegisteredTool {
                descriptor,
                provider,
            },
        );
        Ok(())
    }

    pub fn resolve(&self, canonical_name: &str) -> Result<RegisteredTool, ToolError> {
        self.tools
            .read()
            .map_err(|_| ToolError::new("tool-registry-poisoned", "read"))?
            .get(canonical_name)
            .cloned()
            .ok_or_else(|| ToolError::new("tool-not-found", canonical_name))
    }

    #[must_use]
    pub fn list(&self) -> Vec<ToolDescriptor> {
        self.tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|tool| tool.descriptor.clone())
            .collect()
    }

    pub fn try_list(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
        self.tools
            .read()
            .map_err(|_| ToolError::new("tool-registry-poisoned", "read"))
            .map(|tools| tools.values().map(|tool| tool.descriptor.clone()).collect())
    }

    pub fn unregister(
        &self,
        canonical_name: &str,
        expected_source: &ToolSource,
    ) -> Result<bool, ToolError> {
        let mut tools = self
            .tools
            .write()
            .map_err(|_| ToolError::new("tool-registry-poisoned", "write"))?;
        let Some(registered) = tools.get(canonical_name) else {
            return Ok(false);
        };
        if &registered.descriptor.source != expected_source {
            return Err(ToolError::new(
                "tool-unregister-source-mismatch",
                canonical_name,
            ));
        }
        tools.remove(canonical_name);
        Ok(true)
    }

    pub fn select_for_prompt(
        &self,
        query: &str,
        max_on_demand: usize,
    ) -> Result<Vec<ToolDescriptor>, ToolError> {
        let tools = self
            .tools
            .read()
            .map_err(|_| ToolError::new("tool-registry-poisoned", "read"))?;
        let query_tokens = search_tokens(query);
        let mut eager = Vec::new();
        let mut on_demand = Vec::new();
        for tool in tools.values() {
            match tool.descriptor.prompt_loading {
                ToolPromptLoading::Eager => eager.push(tool.descriptor.clone()),
                ToolPromptLoading::OnDemand => {
                    let score = tool_search_score(&tool.descriptor, &query_tokens);
                    if score > 0 {
                        on_demand.push((score, tool.descriptor.clone()));
                    }
                }
                ToolPromptLoading::UserOnly => {}
            }
        }
        on_demand.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.canonical_name.cmp(&right.1.canonical_name))
        });
        eager.extend(
            on_demand
                .into_iter()
                .take(max_on_demand)
                .map(|(_, descriptor)| descriptor),
        );
        eager.sort_by(|left, right| left.canonical_name.cmp(&right.canonical_name));
        Ok(eager)
    }
}

fn search_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })
        .filter(|token| token.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

fn tool_search_score(descriptor: &ToolDescriptor, query_tokens: &[String]) -> usize {
    if query_tokens.is_empty() {
        return 0;
    }
    let name = descriptor.canonical_name.to_lowercase();
    let description = descriptor.description.to_lowercase();
    let keywords = descriptor
        .keywords
        .iter()
        .map(|keyword| keyword.to_lowercase())
        .collect::<Vec<_>>();
    query_tokens
        .iter()
        .map(|token| {
            usize::from(name.contains(token)) * 4
                + usize::from(keywords.iter().any(|keyword| keyword.contains(token))) * 2
                + usize::from(description.contains(token))
        })
        .sum()
}

pub trait ToolInvocationJournal: Send + Sync {
    fn create(&self, record: ToolInvocationRecord) -> Result<(), ToolError>;
    fn update(
        &self,
        id: &ToolInvocationId,
        patch: ToolInvocationPatch,
    ) -> Result<ToolInvocationRecord, ToolError>;
    fn get(&self, id: &ToolInvocationId) -> Result<Option<ToolInvocationRecord>, ToolError>;
    fn find_by_idempotency_key(&self, key: &str)
    -> Result<Option<ToolInvocationRecord>, ToolError>;
    fn list(&self) -> Result<Vec<ToolInvocationRecord>, ToolError>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolInvocationPatch {
    pub expected_status: ToolInvocationStatus,
    pub status: ToolInvocationStatus,
    pub approval_request_id: Option<PermissionRequestId>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub updated_at_millis: i64,
}

#[derive(Default)]
pub struct MemoryToolJournal {
    records: Mutex<BTreeMap<ToolInvocationId, ToolInvocationRecord>>,
    by_key: Mutex<BTreeMap<String, ToolInvocationId>>,
}

impl MemoryToolJournal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ToolInvocationJournal for MemoryToolJournal {
    fn create(&self, record: ToolInvocationRecord) -> Result<(), ToolError> {
        let mut records = self
            .records
            .lock()
            .map_err(|_| ToolError::new("tool-journal-poisoned", "records"))?;
        let mut by_key = self
            .by_key
            .lock()
            .map_err(|_| ToolError::new("tool-journal-poisoned", "by-key"))?;
        if records.contains_key(&record.id) || by_key.contains_key(&record.idempotency_key) {
            return Err(ToolError::new(
                "tool-invocation-conflict",
                record.id.to_string(),
            ));
        }
        by_key.insert(record.idempotency_key.clone(), record.id.clone());
        records.insert(record.id.clone(), record);
        Ok(())
    }

    fn update(
        &self,
        id: &ToolInvocationId,
        patch: ToolInvocationPatch,
    ) -> Result<ToolInvocationRecord, ToolError> {
        let mut records = self
            .records
            .lock()
            .map_err(|_| ToolError::new("tool-journal-poisoned", "records"))?;
        let record = records
            .get_mut(id)
            .ok_or_else(|| ToolError::new("tool-invocation-not-found", id.to_string()))?;
        if record.status != patch.expected_status {
            return Err(ToolError::new(
                "tool-invocation-update-conflict",
                format!(
                    "expected={:?}, actual={:?}",
                    patch.expected_status, record.status
                ),
            ));
        }
        record.status = patch.status;
        record.approval_request_id = patch.approval_request_id;
        record.result = patch.result;
        record.error = patch.error;
        record.updated_at_millis = patch.updated_at_millis;
        Ok(record.clone())
    }

    fn get(&self, id: &ToolInvocationId) -> Result<Option<ToolInvocationRecord>, ToolError> {
        self.records
            .lock()
            .map_err(|_| ToolError::new("tool-journal-poisoned", "records"))
            .map(|records| records.get(id).cloned())
    }

    fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<ToolInvocationRecord>, ToolError> {
        let id = self
            .by_key
            .lock()
            .map_err(|_| ToolError::new("tool-journal-poisoned", "by-key"))?
            .get(key)
            .cloned();
        id.map_or(Ok(None), |id| self.get(&id))
    }

    fn list(&self) -> Result<Vec<ToolInvocationRecord>, ToolError> {
        self.records
            .lock()
            .map_err(|_| ToolError::new("tool-journal-poisoned", "records"))
            .map(|records| records.values().cloned().collect())
    }
}

#[derive(Clone, Debug)]
pub struct ToolInvokeRequest {
    pub invocation_id: ToolInvocationId,
    pub approval_request_id: PermissionRequestId,
    pub idempotency_key: String,
    pub envelope: ExecutionEnvelope,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub now_millis: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolInvokeResponse {
    pub invocation: ToolInvocationRecord,
    pub needs_approval: bool,
    pub retryable: bool,
}

pub struct ToolRuntime {
    registry: ToolRegistry,
    permissions: Mutex<PermissionEngine>,
    journal: Arc<dyn ToolInvocationJournal>,
    sandbox: Arc<dyn SandboxPort>,
    active_cancellations: Mutex<BTreeMap<ToolInvocationId, ToolCancellationToken>>,
}

impl ToolRuntime {
    #[must_use]
    pub fn new(
        registry: ToolRegistry,
        permissions: PermissionEngine,
        journal: Arc<dyn ToolInvocationJournal>,
        sandbox: Arc<dyn SandboxPort>,
    ) -> Self {
        Self {
            registry,
            permissions: Mutex::new(permissions),
            journal,
            sandbox,
            active_cancellations: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn invoke(&self, request: ToolInvokeRequest) -> Result<ToolInvokeResponse, ToolError> {
        if let Some(existing) = self
            .journal
            .find_by_idempotency_key(&request.idempotency_key)?
        {
            return Ok(response(existing));
        }
        let registered = self.registry.resolve(&request.tool_name)?;
        let args = registered.provider.validate_args(&request.args)?;
        let permission_action = registered.provider.permission_action(&args)?;
        let invocation = ToolInvocationRecord {
            id: request.invocation_id,
            idempotency_key: request.idempotency_key,
            envelope: request.envelope.clone(),
            tool_name: registered.descriptor.canonical_name.clone(),
            tool_version: registered.descriptor.version.clone(),
            effect_class: registered.descriptor.effect_class,
            status: ToolInvocationStatus::Requested,
            args,
            permission_action,
            approval_request_id: None,
            result: None,
            error: None,
            created_at_millis: request.now_millis,
            updated_at_millis: request.now_millis,
        };
        self.journal.create(invocation.clone())?;
        self.authorize_and_maybe_execute(
            invocation,
            registered,
            request.approval_request_id,
            request.now_millis,
        )
    }

    #[must_use]
    pub fn tools(&self) -> Vec<ToolDescriptor> {
        self.registry.list()
    }

    pub fn model_tools(
        &self,
        prompt: &str,
        max_on_demand: usize,
    ) -> Result<Vec<ToolDescriptor>, ToolError> {
        self.registry.select_for_prompt(prompt, max_on_demand)
    }

    pub fn pending_approvals(&self) -> Result<Vec<harness_permission::ApprovalRequest>, ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))
            .map(|permissions| permissions.pending_requests())
    }

    pub fn active_grants(&self) -> Result<Vec<harness_permission::PermissionGrant>, ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))
            .map(|permissions| permissions.active_grants())
    }

    pub fn approval_policy(&self) -> Result<ApprovalPolicy, ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))
            .map(|permissions| permissions.approval_policy())
    }

    pub fn set_approval_policy(&self, policy: ApprovalPolicy) -> Result<(), ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?
            .set_approval_policy(policy);
        Ok(())
    }

    pub fn allow_mcp_server(&self, server_id: &str) -> Result<(), ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?
            .allow_mcp_server(server_id)
            .map_err(|error| ToolError::new(error.code, error.message))
    }

    pub fn remove_mcp_server(&self, server_id: &str) -> Result<(), ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?
            .remove_mcp_server(server_id);
        Ok(())
    }

    pub fn permission_rules(&self) -> Result<Vec<PermissionRule>, ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))
            .map(|permissions| permissions.rules())
    }

    pub fn replace_permission_rules(&self, rules: Vec<PermissionRule>) -> Result<(), ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?
            .replace_rules(rules)
            .map_err(|error| ToolError::new(error.code, error.message))
    }

    pub fn add_permission_rule(&self, rule: PermissionRule) -> Result<(), ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?
            .add_rule(rule)
            .map_err(|error| ToolError::new(error.code, error.message))
    }

    pub fn remove_permission_rule(&self, rule_id: &str) -> Result<bool, ToolError> {
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))
            .map(|mut permissions| permissions.remove_rule(rule_id))
    }

    /// 请求取消一个正在执行的 Tool。真正的进程树终止由 Sandbox/Provider 响应 token 完成。
    pub fn cancel(&self, invocation_id: &ToolInvocationId) -> Result<bool, ToolError> {
        let active = self
            .active_cancellations
            .lock()
            .map_err(|_| ToolError::new("tool-cancellation-poisoned", "active"))?;
        Ok(active.get(invocation_id).is_some_and(|token| {
            token.cancel();
            true
        }))
    }

    pub fn active_invocations(&self) -> Result<Vec<ToolInvocationId>, ToolError> {
        self.active_cancellations
            .lock()
            .map_err(|_| ToolError::new("tool-cancellation-poisoned", "active"))
            .map(|active| active.keys().cloned().collect())
    }

    pub fn recover_interrupted(
        &self,
        now_millis: i64,
    ) -> Result<Vec<ToolInvocationRecord>, ToolError> {
        let running = self
            .journal
            .list()?
            .into_iter()
            .filter(|record| record.status == ToolInvocationStatus::Running)
            .collect::<Vec<_>>();
        running
            .into_iter()
            .map(|record| {
                let uncertain = matches!(
                    record.effect_class,
                    ToolEffectClass::VerifiableEffect | ToolEffectClass::NonRepeatableEffect
                );
                self.journal.update(
                    &record.id,
                    ToolInvocationPatch {
                        expected_status: ToolInvocationStatus::Running,
                        status: if uncertain {
                            ToolInvocationStatus::Uncertain
                        } else {
                            ToolInvocationStatus::Failed
                        },
                        approval_request_id: record.approval_request_id,
                        result: None,
                        error: Some("interrupted-before-result".to_owned()),
                        updated_at_millis: now_millis,
                    },
                )
            })
            .collect()
    }

    pub fn rehydrate_pending_approvals(&self) -> Result<usize, ToolError> {
        let waiting = self
            .journal
            .list()?
            .into_iter()
            .filter(|record| record.status == ToolInvocationStatus::WaitingApproval)
            .collect::<Vec<_>>();
        let mut permissions = self
            .permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?;
        let mut restored = 0;
        for record in &waiting {
            let approval_id = record.approval_request_id.clone().ok_or_else(|| {
                ToolError::new("tool-approval-request-missing", record.id.to_string())
            })?;
            match permissions.restore_pending_request(
                approval_id,
                record.permission_action.clone(),
                record.envelope.clone(),
                record.updated_at_millis,
            ) {
                Ok(_) => restored += 1,
                Err(error) if error.code == "approval-now-hard-denied" => {
                    self.journal.update(
                        &record.id,
                        ToolInvocationPatch {
                            expected_status: ToolInvocationStatus::WaitingApproval,
                            status: ToolInvocationStatus::Denied,
                            approval_request_id: record.approval_request_id.clone(),
                            result: None,
                            error: Some(error.message),
                            updated_at_millis: record.updated_at_millis,
                        },
                    )?;
                }
                Err(error) => return Err(permission_error(error)),
            }
        }
        Ok(restored)
    }

    pub fn resume_after_approval(
        &self,
        invocation_id: &ToolInvocationId,
        envelope: &ExecutionEnvelope,
        scope: GrantScope,
        grant_id: PermissionGrantId,
        next_request_id: PermissionRequestId,
        now_millis: i64,
    ) -> Result<ToolInvokeResponse, ToolError> {
        let invocation = self.journal.get(invocation_id)?.ok_or_else(|| {
            ToolError::new("tool-invocation-not-found", invocation_id.to_string())
        })?;
        if invocation.status != ToolInvocationStatus::WaitingApproval {
            return Err(ToolError::new(
                "tool-invocation-not-waiting-approval",
                invocation_id.to_string(),
            ));
        }
        if &invocation.envelope != envelope {
            return Err(ToolError::new(
                "tool-invocation-envelope-mismatch",
                invocation_id.to_string(),
            ));
        }
        let approval_id = invocation.approval_request_id.clone().ok_or_else(|| {
            ToolError::new("tool-approval-request-missing", invocation_id.to_string())
        })?;
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?
            .respond(&approval_id, true, scope, grant_id, now_millis)
            .map_err(permission_error)?;
        let registered = self.registry.resolve(&invocation.tool_name)?;
        self.authorize_and_maybe_execute(invocation, registered, next_request_id, now_millis)
    }

    pub fn deny_approval(
        &self,
        invocation_id: &ToolInvocationId,
        grant_id: PermissionGrantId,
        now_millis: i64,
    ) -> Result<ToolInvokeResponse, ToolError> {
        let invocation = self.journal.get(invocation_id)?.ok_or_else(|| {
            ToolError::new("tool-invocation-not-found", invocation_id.to_string())
        })?;
        if invocation.status != ToolInvocationStatus::WaitingApproval {
            return Err(ToolError::new(
                "tool-invocation-not-waiting-approval",
                invocation_id.to_string(),
            ));
        }
        let approval_id = invocation.approval_request_id.clone().ok_or_else(|| {
            ToolError::new("tool-approval-request-missing", invocation_id.to_string())
        })?;
        self.permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?
            .respond(&approval_id, false, GrantScope::Once, grant_id, now_millis)
            .map_err(permission_error)?;
        let denied = self.journal.update(
            invocation_id,
            ToolInvocationPatch {
                expected_status: ToolInvocationStatus::WaitingApproval,
                status: ToolInvocationStatus::Denied,
                approval_request_id: Some(approval_id),
                result: None,
                error: Some("user-denied-approval".to_owned()),
                updated_at_millis: now_millis,
            },
        )?;
        Ok(response(denied))
    }

    pub fn retry(
        &self,
        invocation_id: &ToolInvocationId,
        envelope: &ExecutionEnvelope,
        approval_request_id: PermissionRequestId,
        now_millis: i64,
    ) -> Result<ToolInvokeResponse, ToolError> {
        let invocation = self.journal.get(invocation_id)?.ok_or_else(|| {
            ToolError::new("tool-invocation-not-found", invocation_id.to_string())
        })?;
        if invocation.status != ToolInvocationStatus::Failed
            || !matches!(
                invocation.effect_class,
                ToolEffectClass::ReadOnlyRetryable | ToolEffectClass::IdempotentEffect
            )
        {
            return Err(ToolError::new(
                "tool-invocation-not-retryable",
                format!(
                    "status={:?}, effect={:?}",
                    invocation.status, invocation.effect_class
                ),
            ));
        }
        if &invocation.envelope != envelope {
            return Err(ToolError::new(
                "tool-invocation-envelope-mismatch",
                invocation_id.to_string(),
            ));
        }
        let requested = self.journal.update(
            invocation_id,
            ToolInvocationPatch {
                expected_status: ToolInvocationStatus::Failed,
                status: ToolInvocationStatus::Requested,
                approval_request_id: None,
                result: None,
                error: None,
                updated_at_millis: now_millis,
            },
        )?;
        let registered = self.registry.resolve(&requested.tool_name)?;
        self.authorize_and_maybe_execute(requested, registered, approval_request_id, now_millis)
    }

    fn authorize_and_maybe_execute(
        &self,
        invocation: ToolInvocationRecord,
        registered: RegisteredTool,
        request_id: PermissionRequestId,
        now_millis: i64,
    ) -> Result<ToolInvokeResponse, ToolError> {
        let decision = self
            .permissions
            .lock()
            .map_err(|_| ToolError::new("permission-engine-poisoned", "lock"))?
            .evaluate(
                invocation.permission_action.clone(),
                &invocation.envelope,
                request_id,
                now_millis,
            )
            .map_err(permission_error)?;
        match decision {
            PermissionDecision::Deny { reason, .. } => {
                let denied = self.journal.update(
                    &invocation.id,
                    ToolInvocationPatch {
                        expected_status: invocation.status,
                        status: ToolInvocationStatus::Denied,
                        approval_request_id: None,
                        result: None,
                        error: Some(reason),
                        updated_at_millis: now_millis,
                    },
                )?;
                Ok(response(denied))
            }
            PermissionDecision::RequestApproval(request) => {
                let waiting = self.journal.update(
                    &invocation.id,
                    ToolInvocationPatch {
                        expected_status: invocation.status,
                        status: ToolInvocationStatus::WaitingApproval,
                        approval_request_id: Some(request.id),
                        result: None,
                        error: None,
                        updated_at_millis: now_millis,
                    },
                )?;
                Ok(response(waiting))
            }
            PermissionDecision::Allow { .. } => self.execute(invocation, registered, now_millis),
        }
    }

    fn execute(
        &self,
        invocation: ToolInvocationRecord,
        registered: RegisteredTool,
        now_millis: i64,
    ) -> Result<ToolInvokeResponse, ToolError> {
        let running = self.journal.update(
            &invocation.id,
            ToolInvocationPatch {
                expected_status: invocation.status,
                status: ToolInvocationStatus::Running,
                approval_request_id: invocation.approval_request_id.clone(),
                result: None,
                error: None,
                updated_at_millis: now_millis,
            },
        )?;
        let cancellation = ToolCancellationToken::new();
        self.active_cancellations
            .lock()
            .map_err(|_| ToolError::new("tool-cancellation-poisoned", "active"))?
            .insert(invocation.id.clone(), cancellation.clone());
        let execution = self
            .sandbox
            .execute(
                &registered.descriptor,
                &invocation.permission_action,
                registered.provider.as_ref(),
                ToolExecutionInput {
                    invocation_id: invocation.id.clone(),
                    envelope: invocation.envelope,
                    args: invocation.args,
                    cancellation: cancellation.clone(),
                    now_millis,
                },
            )
            .and_then(|value| {
                if cancellation.is_cancelled() {
                    Err(ToolError::new("tool-cancelled", invocation.id.to_string()))
                } else {
                    registered.provider.validate_result(&value)
                }
            });
        self.active_cancellations
            .lock()
            .map_err(|_| ToolError::new("tool-cancellation-poisoned", "active"))?
            .remove(&invocation.id);
        match execution {
            Ok(result) => {
                let completed = self.journal.update(
                    &running.id,
                    ToolInvocationPatch {
                        expected_status: ToolInvocationStatus::Running,
                        status: ToolInvocationStatus::Completed,
                        approval_request_id: running.approval_request_id,
                        result: Some(result),
                        error: None,
                        updated_at_millis: now_millis,
                    },
                )?;
                Ok(response(completed))
            }
            Err(error) => {
                let uncertain = matches!(
                    running.effect_class,
                    ToolEffectClass::VerifiableEffect | ToolEffectClass::NonRepeatableEffect
                );
                let failed = self.journal.update(
                    &running.id,
                    ToolInvocationPatch {
                        expected_status: ToolInvocationStatus::Running,
                        status: if uncertain {
                            ToolInvocationStatus::Uncertain
                        } else {
                            ToolInvocationStatus::Failed
                        },
                        approval_request_id: running.approval_request_id,
                        result: None,
                        error: Some(error.to_string()),
                        updated_at_millis: now_millis,
                    },
                )?;
                Ok(response(failed))
            }
        }
    }

    #[must_use]
    pub fn journal(&self) -> &Arc<dyn ToolInvocationJournal> {
        &self.journal
    }
}

fn response(invocation: ToolInvocationRecord) -> ToolInvokeResponse {
    ToolInvokeResponse {
        needs_approval: invocation.status == ToolInvocationStatus::WaitingApproval,
        retryable: invocation.status == ToolInvocationStatus::Failed
            && matches!(
                invocation.effect_class,
                ToolEffectClass::ReadOnlyRetryable | ToolEffectClass::IdempotentEffect
            ),
        invocation,
    }
}

fn permission_error(error: harness_permission::PermissionError) -> ToolError {
    ToolError::new(error.code, error.message)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use harness_permission::{ApprovalPolicy, workspace_write_profile};
    use harness_types::{
        ActorId, ConfidentialityLabel, InformationFlowLabel, IntegrityLabel, MissionId, ProjectId,
        RunId,
    };
    use tempfile::tempdir;

    use super::*;

    struct WriteTool {
        executions: Arc<AtomicUsize>,
        fail: bool,
    }

    impl ToolProvider for WriteTool {
        fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
            if value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .is_none()
            {
                return Err(ToolError::new("invalid-write-args", "path missing"));
            }
            Ok(value.clone())
        }

        fn validate_result(
            &self,
            value: &serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            if value.get("ok") != Some(&serde_json::Value::Bool(true)) {
                return Err(ToolError::new("invalid-write-result", "ok missing"));
            }
            Ok(value.clone())
        }

        fn permission_action(
            &self,
            args: &serde_json::Value,
        ) -> Result<PermissionAction, ToolError> {
            Ok(PermissionAction::FilesystemWrite {
                path: PathBuf::from(args["path"].as_str().expect("validated path")),
            })
        }

        fn execute(&self, _input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(ToolError::new("disk-busy", "redacted"))
            } else {
                Ok(serde_json::json!({"ok":true}))
            }
        }
    }

    struct TestSandbox;

    impl SandboxPort for TestSandbox {
        fn execute(
            &self,
            _descriptor: &ToolDescriptor,
            _permission_action: &PermissionAction,
            provider: &dyn ToolProvider,
            input: ToolExecutionInput,
        ) -> Result<serde_json::Value, ToolError> {
            provider.execute(input)
        }
    }

    fn envelope() -> ExecutionEnvelope {
        ExecutionEnvelope {
            project_id: ProjectId::from("project:tool"),
            mission_id: MissionId::from("mission:tool"),
            run_id: Some(RunId::from("run:tool")),
            actor_id: ActorId::from("agent:tool"),
            origin: harness_permission::InvocationOrigin::Agent,
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality: ConfidentialityLabel::ProjectPrivate,
            },
        }
    }

    fn runtime(
        root: PathBuf,
        approval: ApprovalPolicy,
        effect_class: ToolEffectClass,
        fail: bool,
    ) -> (ToolRuntime, Arc<AtomicUsize>, Arc<MemoryToolJournal>) {
        let executions = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::new();
        registry
            .register(
                ToolDescriptor {
                    canonical_name: "files.write".to_owned(),
                    version: "1".to_owned(),
                    description: "write".to_owned(),
                    effect_class,
                    source: ToolSource::Test,
                    prompt_loading: ToolPromptLoading::Eager,
                    keywords: vec!["write".to_owned()],
                    input_schema: serde_json::json!({"type":"object"}),
                    output_schema: serde_json::json!({"type":"object"}),
                },
                Arc::new(WriteTool {
                    executions: executions.clone(),
                    fail,
                }),
            )
            .expect("register");
        let journal = Arc::new(MemoryToolJournal::new());
        (
            ToolRuntime::new(
                registry,
                PermissionEngine::new(workspace_write_profile(root), approval),
                journal.clone(),
                Arc::new(TestSandbox),
            ),
            executions,
            journal,
        )
    }

    fn request(root: &std::path::Path, key: &str) -> ToolInvokeRequest {
        ToolInvokeRequest {
            invocation_id: ToolInvocationId::from(format!("invocation:{key}")),
            approval_request_id: PermissionRequestId::from(format!("approval:{key}")),
            idempotency_key: key.to_owned(),
            envelope: envelope(),
            tool_name: "files.write".to_owned(),
            args: serde_json::json!({"path":root.join("file.rs"),"content":"ok"}),
            now_millis: 1,
        }
    }

    #[test]
    fn safe_tool_executes_once_and_journal_deduplicates() {
        let temporary = tempdir().expect("tempdir");
        let (runtime, executions, _journal) = runtime(
            temporary.path().to_path_buf(),
            ApprovalPolicy::NeverWithinSandbox,
            ToolEffectClass::IdempotentEffect,
            false,
        );
        let first = runtime
            .invoke(request(temporary.path(), "safe"))
            .expect("first");
        let second = runtime
            .invoke(request(temporary.path(), "safe"))
            .expect("second");
        assert_eq!(first.invocation.status, ToolInvocationStatus::Completed);
        assert_eq!(first.invocation.id, second.invocation.id);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn approval_resume_executes_once() {
        let temporary = tempdir().expect("tempdir");
        let (runtime, executions, _journal) = runtime(
            temporary.path().to_path_buf(),
            ApprovalPolicy::Always,
            ToolEffectClass::IdempotentEffect,
            false,
        );
        let waiting = runtime
            .invoke(request(temporary.path(), "approval"))
            .expect("waiting");
        assert!(waiting.needs_approval);
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        let completed = runtime
            .resume_after_approval(
                &waiting.invocation.id,
                &envelope(),
                GrantScope::Once,
                PermissionGrantId::from("grant:1"),
                PermissionRequestId::from("approval:resume"),
                2,
            )
            .expect("resume");
        assert_eq!(completed.invocation.status, ToolInvocationStatus::Completed);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn effect_class_controls_failed_vs_uncertain() {
        let temporary = tempdir().expect("tempdir");
        let (retryable_runtime, _, _retryable_journal) = runtime(
            temporary.path().to_path_buf(),
            ApprovalPolicy::NeverWithinSandbox,
            ToolEffectClass::IdempotentEffect,
            true,
        );
        let (uncertain_runtime, _, _uncertain_journal) = runtime(
            temporary.path().to_path_buf(),
            ApprovalPolicy::NeverWithinSandbox,
            ToolEffectClass::NonRepeatableEffect,
            true,
        );
        let failed = retryable_runtime
            .invoke(request(temporary.path(), "retry"))
            .expect("failed");
        let uncertain = uncertain_runtime
            .invoke(request(temporary.path(), "uncertain"))
            .expect("uncertain");
        assert_eq!(failed.invocation.status, ToolInvocationStatus::Failed);
        assert!(failed.retryable);
        assert_eq!(uncertain.invocation.status, ToolInvocationStatus::Uncertain);
        assert!(!uncertain.retryable);
    }

    #[test]
    fn recovery_classifies_interrupted_running_by_effect_class() {
        let temporary = tempdir().expect("tempdir");
        let (runtime, _, journal) = runtime(
            temporary.path().to_path_buf(),
            ApprovalPolicy::NeverWithinSandbox,
            ToolEffectClass::IdempotentEffect,
            false,
        );
        let pending = request(temporary.path(), "interrupted");
        journal
            .create(ToolInvocationRecord {
                id: pending.invocation_id,
                idempotency_key: pending.idempotency_key,
                envelope: pending.envelope,
                tool_name: pending.tool_name,
                tool_version: "1".to_owned(),
                effect_class: ToolEffectClass::IdempotentEffect,
                status: ToolInvocationStatus::Running,
                args: pending.args,
                permission_action: PermissionAction::FilesystemWrite {
                    path: temporary.path().join("file.rs"),
                },
                approval_request_id: None,
                result: None,
                error: None,
                created_at_millis: 1,
                updated_at_millis: 1,
            })
            .expect("create running");
        let recovered = runtime.recover_interrupted(2).expect("recover");
        assert_eq!(recovered[0].status, ToolInvocationStatus::Failed);
        assert_eq!(
            recovered[0].error.as_deref(),
            Some("interrupted-before-result")
        );
    }

    #[test]
    fn failed_idempotent_invocation_can_retry_but_uncertain_cannot() {
        let temporary = tempdir().expect("tempdir");
        let (runtime, executions, _journal) = runtime(
            temporary.path().to_path_buf(),
            ApprovalPolicy::NeverWithinSandbox,
            ToolEffectClass::IdempotentEffect,
            true,
        );
        let failed = runtime
            .invoke(request(temporary.path(), "retry-explicit"))
            .expect("failed");
        assert!(failed.retryable);
        let retried = runtime
            .retry(
                &failed.invocation.id,
                &envelope(),
                PermissionRequestId::from("approval:retry"),
                2,
            )
            .expect("retry");
        assert_eq!(retried.invocation.status, ToolInvocationStatus::Failed);
        assert_eq!(executions.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rehydrate_converts_newly_hard_denied_approval_without_blocking_startup() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().to_path_buf();
        let (first, executions, journal) = runtime(
            root.clone(),
            ApprovalPolicy::Always,
            ToolEffectClass::IdempotentEffect,
            false,
        );
        let waiting = first
            .invoke(request(&root, "rehydrate-hard-deny"))
            .expect("waiting");
        assert_eq!(
            waiting.invocation.status,
            ToolInvocationStatus::WaitingApproval
        );

        let registry = ToolRegistry::new();
        registry
            .register(
                ToolDescriptor {
                    canonical_name: "files.write".to_owned(),
                    version: "1".to_owned(),
                    description: "write".to_owned(),
                    effect_class: ToolEffectClass::IdempotentEffect,
                    source: ToolSource::Test,
                    prompt_loading: ToolPromptLoading::Eager,
                    keywords: vec!["write".to_owned()],
                    input_schema: serde_json::json!({"type":"object"}),
                    output_schema: serde_json::json!({"type":"object"}),
                },
                Arc::new(WriteTool {
                    executions,
                    fail: false,
                }),
            )
            .expect("register");
        let mut stricter = workspace_write_profile(root.clone());
        stricter.filesystem.denied_roots = vec![root];
        let recovered = ToolRuntime::new(
            registry,
            PermissionEngine::new(stricter, ApprovalPolicy::Always),
            journal.clone(),
            Arc::new(TestSandbox),
        );
        assert_eq!(
            recovered.rehydrate_pending_approvals().expect("rehydrate"),
            0
        );
        assert_eq!(
            journal
                .get(&waiting.invocation.id)
                .expect("journal")
                .expect("record")
                .status,
            ToolInvocationStatus::Denied
        );
    }

    #[test]
    fn dynamic_tool_search_is_bounded_and_unregistration_checks_source() {
        let executions = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::new();
        for descriptor in [
            ToolDescriptor {
                canonical_name: "files.read".to_owned(),
                version: "1".to_owned(),
                description: "read file".to_owned(),
                effect_class: ToolEffectClass::ReadOnlyRetryable,
                source: ToolSource::Builtin,
                prompt_loading: ToolPromptLoading::Eager,
                keywords: vec!["file".to_owned()],
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: serde_json::json!({"type":"object"}),
            },
            ToolDescriptor {
                canonical_name: "mcp.github.pulls.list".to_owned(),
                version: "1".to_owned(),
                description: "list pull requests".to_owned(),
                effect_class: ToolEffectClass::ReadOnlyRetryable,
                source: ToolSource::Mcp {
                    server_id: "github".to_owned(),
                },
                prompt_loading: ToolPromptLoading::OnDemand,
                keywords: vec!["github".to_owned(), "pull request".to_owned()],
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: serde_json::json!({"type":"object"}),
            },
            ToolDescriptor {
                canonical_name: "process.admin".to_owned(),
                version: "1".to_owned(),
                description: "user only".to_owned(),
                effect_class: ToolEffectClass::NonRepeatableEffect,
                source: ToolSource::Internal,
                prompt_loading: ToolPromptLoading::UserOnly,
                keywords: vec!["admin".to_owned()],
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: serde_json::json!({"type":"object"}),
            },
        ] {
            registry
                .register(
                    descriptor,
                    Arc::new(WriteTool {
                        executions: executions.clone(),
                        fail: false,
                    }),
                )
                .expect("register");
        }
        assert_eq!(
            registry
                .select_for_prompt("explain architecture", 8)
                .expect("select")
                .into_iter()
                .map(|tool| tool.canonical_name)
                .collect::<Vec<_>>(),
            vec!["files.read"]
        );
        assert_eq!(
            registry
                .select_for_prompt("检查 github pull request", 1)
                .expect("select")
                .into_iter()
                .map(|tool| tool.canonical_name)
                .collect::<Vec<_>>(),
            vec!["files.read", "mcp.github.pulls.list"]
        );
        assert!(
            registry
                .unregister("mcp.github.pulls.list", &ToolSource::Builtin)
                .is_err()
        );
        assert!(
            registry
                .unregister(
                    "mcp.github.pulls.list",
                    &ToolSource::Mcp {
                        server_id: "github".to_owned()
                    }
                )
                .expect("unregister")
        );
    }
}
