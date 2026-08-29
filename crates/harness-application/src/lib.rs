#![forbid(unsafe_code)]

//! CLI/TUI 调用的 Application Use Cases。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use harness_agent::{
    AgentBudgetManager, AgentBudgetPolicy, AgentBudgetRequest, AgentCatalog, AgentDispatch,
    AgentEndpoint, AgentEndpointStatus, AgentExecutionMetrics, AgentExecutionOutcome,
    AgentExecutionRequest, AgentLifecycle, AgentMessage, AgentMessageBus, AgentMessageKind,
    AgentModelContinuation, AgentModelToolYield, AgentResult, AgentResultEnvelope, AgentRole,
    AgentScheduler, AgentSession, AgentSessionStatus, AgentStateStore, AgentTaskContract,
    AgentToolCall, AgentWorkingContext, BoundedAgentExecutor, BudgetEscrowStatus, Coordinator,
    Evidence, FileLease, FileLeaseManager, ModelAgentHandler, PlanningBudget, RunCancellationTree,
    SharedSteeringBuffer, StaffingAssignment, StaffingRouter, StaffingTask, SteeringAgentHandler,
    validate_required_evidence,
};
use harness_auth::{CredentialId, CredentialStore, OPENAI_API_KEY_CREDENTIAL_ID};
use harness_browser::{
    BrowserActionRecord, BrowserCommand, BrowserResult, BrowserRuntime, BrowserRuntimeView,
};
use harness_builtin_tools::{PatchRecord, PatchStore};
use harness_cache::{
    CacheEffectClass, CacheEngine, CacheEntry, CacheKey, CacheMetrics, CacheNamespace, CachePolicy,
    CacheScope, MemoryCache,
};
use harness_config::{
    ConfigLayer, ConfigManager, EffectiveConfigView, ModeProfile, PermissionMode, VectorMode,
};
use harness_context::{
    CacheClass, CompactionItem, ContextBroker, ContextBudget, ContextCheckpoint, ContextCompactor,
    ContextItem, ContextKind, ContextSeries, ContextStore, ContextTransition, HeuristicTokenizer,
    Priority, PromptCacheability, PromptCanonicalizer, PromptRole, PromptSegment, PromptSource,
    Role, StructuredSummary, SummaryProvider, Tokenizer, fork_context_series,
};
use harness_event::{EventBus, EventPriority, EventScope, HarnessEvent};
use harness_kernel::{
    ApprovalDecision, ApprovalStatus as KernelApprovalStatus, ClaimedEffect, CompletionFence,
    DomainEvent, EffectCompletion, EffectIntent, EffectOutcome, GoalRevision, KernelStore,
    MissionCommand, MissionEpoch, MissionState, MissionStatus, NewEffect, NodeKind, NodeStatus,
    RunFence, SessionCommand, SessionState, SessionStatus, WorkflowNodeDefinition, decide_mission,
    decide_session, find_ready_node_ids, reduce_session,
};
use harness_lsp::{LspDiagnostic, LspLocation, LspManager, LspServerView, LspSymbol};
use harness_mcp::{
    McpManager, McpOAuthStart, McpOAuthStatus, McpPromptDescriptor, McpResourceDescriptor,
    McpServerConfig, McpServerView, McpToolDescriptor,
};
use harness_memory::{
    LspFactBatch, MemoryKind, MemoryRecord, MemorySearchResponse, MemoryStatus, NewMemoryRecord,
    ProjectMemory, ProjectMemoryView, RepositoryIndex, RepositoryIndexView, RepositorySearchResult,
    RepositoryUpdateStats, RetrievalMode,
};
use harness_model::{
    CancellationToken, CompletionStatus, FailoverTarget, ModelCapability, ModelEvent,
    ModelInputItem, ModelMessageRole, ModelProvider, ModelRequest, ModelRoutePolicy, ModelRuntime,
    ModelRuntimeView, ReasoningMapping, ResponseFormat, ToolDefinition,
};
use harness_permission::{
    ApprovalPolicy, ExecutionEnvelope, GrantScope, InvocationOrigin, PermissionRule,
    PermissionRuleAction, PermissionRuleEffect,
};
use harness_plugin::{PluginManager, PluginPermissionReview, PluginView};
use harness_skill::{LoadedSkill, SkillRegistry, SkillSource, SkillView};
use harness_tool::{ToolInvocationRecord, ToolInvocationStatus, ToolInvokeRequest, ToolRuntime};
use harness_types::{
    ActorId, AgentDefinitionId, AgentEndpointId, AgentInstanceId, AgentSessionId, ApprovalId,
    ArtifactId, BrowserActionId, CheckpointId, ClaimToken, Clock, ConfidentialityLabel,
    ContentHash, ContextItemId, ContextSeriesId, EffectId, GoalRevisionId, IdGenerator,
    InformationFlowLabel, IntegrityLabel, MissionId, ModelId, PermissionGrantId,
    PermissionRequestId, ProjectId, PromptSegmentId, ProviderId, ReasoningLevel, ResponseId, RunId,
    SessionId, TaskId, ToolCallId, ToolInvocationId, TraceId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use harness_context::CompactionMode;

/// Application 边界的友好错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationError {
    pub code: String,
    pub message: String,
}

impl ApplicationError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for ApplicationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ApplicationError {}

/// `/status` 使用的 Terminal-neutral ViewModel。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusView {
    pub session_id: SessionId,
    pub session_status: SessionStatus,
    pub session_version: u64,
    pub goal: Option<String>,
    pub goal_locked: bool,
    pub active_mission_id: Option<MissionId>,
    pub mode: String,
    pub model: String,
    pub reasoning: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSummaryView {
    pub session_id: SessionId,
    pub current: bool,
    pub status: SessionStatus,
    pub version: u64,
    pub goal: Option<String>,
    pub parent_session_id: Option<SessionId>,
    pub forked_from_checkpoint_id: Option<CheckpointId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalHistoryView {
    pub current_revision_id: Option<GoalRevisionId>,
    pub locked: bool,
    pub revisions: Vec<GoalRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextResetView {
    pub checkpoint_id: CheckpointId,
    pub previous_series_id: ContextSeriesId,
    pub next_series_id: ContextSeriesId,
    pub removed_items: usize,
    pub retained_items: usize,
}

/// `/model`、`/reasoning` 共用的 ViewModel。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelView {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub reasoning_requested: ReasoningLevel,
    pub reasoning_effective: Option<ReasoningLevel>,
    pub reasoning_mapping: ReasoningMapping,
    pub context_window_tokens: u32,
    pub max_output_tokens: u32,
    pub tool_calling: bool,
    pub structured_output: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailoverView {
    pub enabled: bool,
    pub cost_confirmed: bool,
    pub targets: Vec<String>,
}

/// `/account` 的脱敏视图。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthView {
    pub provider_id: ProviderId,
    pub auth_method: String,
    pub configured: bool,
    pub storage: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolRuntimeView {
    pub tools: Vec<harness_tool::ToolDescriptor>,
    pub pending_approvals: usize,
    pub active_grants: usize,
    pub approval_policy: ApprovalPolicy,
    pub permission_rules: Vec<PermissionRule>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserCapabilityView {
    pub configured: bool,
    pub runtime: Option<BrowserRuntimeView>,
}

type PendingToolCall = (
    ToolCallId,
    String,
    serde_json::Value,
    Option<ToolInvocationId>,
);

#[derive(Clone)]
struct ModelLoopState {
    view: ModelRuntimeView,
    canonical_context: String,
    tools: Vec<ToolDefinition>,
    transcript: Vec<ModelInputItem>,
    next_input: Vec<ModelInputItem>,
    previous_response_id: Option<ResponseId>,
    output: String,
    next_turn: u8,
}

#[derive(Clone)]
struct PendingToolBatch {
    response_id: Option<ResponseId>,
    results: Vec<ModelInputItem>,
    calls: VecDeque<PendingToolCall>,
}

#[derive(Clone)]
struct PendingModelContinuation {
    mission_id: MissionId,
    run_id: RunId,
    state: ModelLoopState,
    batch: PendingToolBatch,
}

struct WaitingParallelTool {
    invocation: ToolInvocationRecord,
    continuation: AgentModelContinuation,
    waiting_call: AgentToolCall,
    remaining_calls: Vec<AgentToolCall>,
}

struct PendingParallelToolContinuation {
    request: AgentExecutionRequest,
    waiting: WaitingParallelTool,
    prompt: String,
}

#[derive(Default)]
struct PluginExtensionRegistration {
    skill_ids: Vec<String>,
    mcp_server_ids: Vec<String>,
}

/// `/plan` 的最小稳定 ViewModel。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanView {
    pub mission_id: Option<MissionId>,
    pub status: Option<MissionStatus>,
    pub accepted: usize,
    pub running: usize,
    pub pending: usize,
    pub blocked: usize,
}

/// Agent 能力目录的只读视图；完整能力说明不会注入主模型历史。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentView {
    pub id: AgentDefinitionId,
    pub name: String,
    pub roles: Vec<AgentRole>,
    pub capabilities: Vec<String>,
    pub lifecycle: AgentLifecycle,
    pub active: usize,
    pub max_concurrency: usize,
    pub control_plane: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentTeamView {
    pub total: usize,
    pub sleeping: usize,
    pub reserved: usize,
    pub running: usize,
    pub durable_messages: bool,
    pub file_leases: bool,
    pub active_run_controls: usize,
    pub recoverable_sessions: usize,
    pub agents: Vec<AgentView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentQueueItemView {
    pub task_id: TaskId,
    pub title: String,
    pub agent_definition_id: AgentDefinitionId,
    pub status: NodeStatus,
    pub ready: bool,
    pub priority: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentQueueView {
    pub mission_id: Option<MissionId>,
    pub items: Vec<AgentQueueItemView>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AdaptiveTeamProfile {
    security: bool,
    performance: bool,
    release: bool,
}

impl AdaptiveTeamProfile {
    fn classify(objective: &str) -> Self {
        let objective = objective.to_lowercase();
        let contains_any =
            |keywords: &[&str]| keywords.iter().any(|keyword| objective.contains(keyword));
        Self {
            security: contains_any(&[
                "security",
                "secure",
                "auth",
                "permission",
                "secret",
                "credential",
                "crypto",
                "injection",
                "supply chain",
                "安全",
                "鉴权",
                "认证",
                "授权",
                "权限",
                "密钥",
                "注入",
                "供应链",
            ]),
            performance: contains_any(&[
                "performance",
                "latency",
                "throughput",
                "benchmark",
                "profil",
                "memory leak",
                "optimize",
                "slow",
                "性能",
                "延迟",
                "吞吐",
                "基准",
                "内存泄漏",
                "优化",
                "卡顿",
            ]),
            release: contains_any(&[
                "release",
                "publish",
                "deploy",
                "package",
                "shipping",
                "version bump",
                "发布",
                "上线",
                "部署",
                "打包",
                "发版",
                "版本",
            ]),
        }
    }
}

struct AdaptiveNodeBlueprint {
    id: TaskId,
    title: String,
    kind: NodeKind,
    depends_on: Vec<TaskId>,
    required_capability: &'static str,
    preferred_role: AgentRole,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SteeringView {
    pub message_id: String,
    pub mission_id: MissionId,
    pub recipient: String,
    pub queued: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBudgetView {
    pub scope: String,
    pub max_agents: usize,
    pub max_parallel_agents: usize,
    pub max_total_tokens: u64,
    pub max_tool_calls: u64,
    pub max_runtime_millis: u64,
    pub max_retries: u32,
    pub max_cost_units: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoordinationView {
    pub conflict_count: usize,
    pub meeting_id: Option<String>,
    pub memory_id: Option<String>,
    pub merge_required: bool,
    pub merge_task_id: Option<TaskId>,
}

/// 后台线程只持有纯 Model batch；Kernel/SQLite 收尾始终回到前台 Application。
pub struct AgentTeamModelJob {
    executor: BoundedAgentExecutor,
    dispatches: Vec<AgentDispatch>,
    started_at_millis: i64,
    steering: SharedSteeringBuffer,
}

impl AgentTeamModelJob {
    #[must_use]
    pub fn cancellation_tokens(&self) -> Vec<CancellationToken> {
        self.dispatches
            .iter()
            .map(|dispatch| dispatch.cancellation.clone())
            .collect()
    }

    #[must_use]
    pub fn cancellation_controls(&self) -> Vec<(TaskId, CancellationToken)> {
        self.dispatches
            .iter()
            .map(|dispatch| {
                (
                    dispatch.request.contract.task_id.clone(),
                    dispatch.cancellation.clone(),
                )
            })
            .collect()
    }

    #[must_use]
    pub fn steering_buffer(&self) -> SharedSteeringBuffer {
        self.steering.clone()
    }

    pub fn execute(self) -> Result<Vec<AgentExecutionOutcome>, ApplicationError> {
        self.executor
            .execute_batch(self.dispatches, self.started_at_millis)
            .map_err(agent_error)
    }
}

pub struct AgentTeamContinuation {
    mission_id: MissionId,
    prompt: String,
    claimed_by_run: BTreeMap<RunId, ClaimedEffect>,
    requests_by_run: BTreeMap<RunId, AgentExecutionRequest>,
}

impl AgentTeamContinuation {
    #[must_use]
    pub fn mission_id(&self) -> &MissionId {
        &self.mission_id
    }
}

pub struct PreparedAgentTeam {
    pub job: AgentTeamModelJob,
    pub continuation: AgentTeamContinuation,
}

pub struct AgentTeamFinalizeStep {
    pub plan: PlanView,
    pub next: Option<PreparedAgentTeam>,
}

/// `/context` 的 Terminal-neutral ViewModel。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextView {
    pub series_id: ContextSeriesId,
    pub item_count: usize,
    pub selected_count: usize,
    pub excluded_count: usize,
    pub used_tokens: u32,
    pub max_tokens: u32,
    pub percent: u8,
    pub checkpoint_count: usize,
}

/// `/checkpoint` 与 `/rollback` 返回的恢复锚点摘要。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckpointView {
    pub checkpoint_id: CheckpointId,
    pub context_series_id: ContextSeriesId,
    pub created_at_millis: i64,
}

/// `/fork` 返回的 Child Session lineage。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForkView {
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub checkpoint_id: CheckpointId,
    pub child_context_series_id: ContextSeriesId,
}

/// `/compact` 的可审计结果摘要。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompactionView {
    pub mode: CompactionMode,
    pub previous_series_id: ContextSeriesId,
    pub next_series_id: ContextSeriesId,
    pub checkpoint_id: CheckpointId,
    pub token_cost_before: u32,
    pub token_cost_after: u32,
}

/// `/cache` 的 L1/L2 指标。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheView {
    pub l1: CacheMetrics,
    pub l2: Option<CacheMetrics>,
    pub effective_hit_rate_percent: Option<u8>,
}

/// `/profile` 的真实有界采样；不包含 prompt、secret 或 Chain-of-Thought。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileMetricView {
    pub name: String,
    pub count: usize,
    pub total_millis: u64,
    pub p50_millis: u64,
    pub p95_millis: u64,
    pub max_millis: u64,
    pub last_millis: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileView {
    pub uptime_millis: u64,
    pub metrics: Vec<ProfileMetricView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WhyToolEvidence {
    pub invocation_id: ToolInvocationId,
    pub tool_name: String,
    pub status: ToolInvocationStatus,
    pub updated_at_millis: i64,
}

/// `/why` 只暴露可审计依据，不输出模型私有思维链。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WhyView {
    pub goal: Option<String>,
    pub mission_id: Option<MissionId>,
    pub summary: String,
    pub context_sources: Vec<String>,
    pub recent_tools: Vec<WhyToolEvidence>,
}

const MAX_PROFILE_SAMPLES: usize = 512;

struct ProfileSpan {
    name: &'static str,
    started: Instant,
    samples: Arc<Mutex<BTreeMap<String, VecDeque<u64>>>>,
}

impl Drop for ProfileSpan {
    fn drop(&mut self) {
        if let Ok(mut samples) = self.samples.lock() {
            let values = samples.entry(self.name.to_owned()).or_default();
            values.push_back(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX));
            while values.len() > MAX_PROFILE_SAMPLES {
                values.pop_front();
            }
        }
    }
}

/// 使用系统时间的生产 Clock；不进入 Reducer。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
            })
    }
}

/// 进程内 ID Generator；时间 nonce 降低跨进程碰撞，持久化唯一约束是最终防线。
#[derive(Debug)]
pub struct ProcessIdGenerator {
    nonce: u64,
    sequence: AtomicU64,
}

impl ProcessIdGenerator {
    #[must_use]
    pub fn new(clock: &dyn Clock) -> Self {
        Self {
            nonce: u64::try_from(clock.now_unix_millis()).unwrap_or_default(),
            sequence: AtomicU64::new(1),
        }
    }
}

impl IdGenerator for ProcessIdGenerator {
    fn next_id(&self, prefix: &str) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}:{}:{sequence}", self.nonce)
    }
}

/// Stage 4 Application；真实 Provider 接入前使用 deterministic fake effect handler。
pub struct HarnessApplication<S, C, I>
where
    S: KernelStore + ContextStore,
    C: Clock,
    I: IdGenerator,
{
    store: S,
    events: EventBus,
    clock: C,
    ids: I,
    project_id: ProjectId,
    project_root: String,
    session_id: SessionId,
    active_mission_id: Option<MissionId>,
    cache: CacheEngine,
    model_runtime: Option<ModelRuntime>,
    credentials: Option<Arc<dyn CredentialStore>>,
    tool_runtime: Option<Arc<ToolRuntime>>,
    patch_store: Option<Arc<PatchStore>>,
    pending_model_continuations: Mutex<BTreeMap<ToolInvocationId, PendingModelContinuation>>,
    pending_parallel_tool_continuations:
        BTreeMap<ToolInvocationId, PendingParallelToolContinuation>,
    ready_parallel_resume: Option<PreparedAgentTeam>,
    mcp_manager: Option<Arc<McpManager>>,
    plugin_manager: Option<Arc<PluginManager>>,
    skill_registry: Option<Arc<SkillRegistry>>,
    plugin_extensions: BTreeMap<String, PluginExtensionRegistration>,
    memory: Option<ProjectMemory>,
    repository: Option<RepositoryIndex>,
    lsp: Option<LspManager>,
    browser_runtime: Option<Arc<BrowserRuntime>>,
    agent_catalog: Option<AgentCatalog>,
    agent_messages: Option<AgentMessageBus>,
    file_leases: Option<FileLeaseManager>,
    agent_state: Option<AgentStateStore>,
    agent_budgets: Option<AgentBudgetManager>,
    config: ConfigManager,
    effective_config: EffectiveConfigView,
    mode_profile: ModeProfile,
    agent_budget_policy: AgentBudgetPolicy,
    started_at: Instant,
    profile_samples: Arc<Mutex<BTreeMap<String, VecDeque<u64>>>>,
    run_controls: Mutex<RunCancellationTree>,
}

impl<S, C, I> HarnessApplication<S, C, I>
where
    S: KernelStore + ContextStore,
    C: Clock,
    I: IdGenerator,
{
    #[must_use]
    pub fn new(
        store: S,
        events: EventBus,
        clock: C,
        ids: I,
        project_id: ProjectId,
        project_root: String,
        session_id: SessionId,
    ) -> Self {
        let config = ConfigManager::default();
        let effective_config = config.effective();
        let mode_profile = ModeProfile::resolve(&effective_config.settings);
        let agent_budget_policy = mode_agent_budget(&mode_profile);
        Self {
            store,
            events,
            clock,
            ids,
            project_id,
            project_root,
            session_id,
            active_mission_id: None,
            cache: CacheEngine::new(
                MemoryCache::new(CachePolicy::safe_default(256, 8 * 1024 * 1024)),
                None,
            ),
            model_runtime: None,
            credentials: None,
            tool_runtime: None,
            patch_store: None,
            pending_model_continuations: Mutex::new(BTreeMap::new()),
            pending_parallel_tool_continuations: BTreeMap::new(),
            ready_parallel_resume: None,
            mcp_manager: None,
            plugin_manager: None,
            skill_registry: None,
            plugin_extensions: BTreeMap::new(),
            memory: None,
            repository: None,
            lsp: None,
            browser_runtime: None,
            agent_catalog: None,
            agent_messages: None,
            file_leases: None,
            agent_state: None,
            agent_budgets: None,
            config,
            effective_config,
            mode_profile,
            agent_budget_policy,
            started_at: Instant::now(),
            profile_samples: Arc::new(Mutex::new(BTreeMap::new())),
            run_controls: Mutex::new(RunCancellationTree::default()),
        }
    }

    /// Composition root 可注入带 L2 Disk CAS 的 CacheEngine。
    #[must_use]
    pub fn with_cache(mut self, cache: CacheEngine) -> Self {
        self.cache = cache;
        self
    }

    /// Composition root 注入已注册 Provider 的 Model Runtime。
    #[must_use]
    pub fn with_model_runtime(mut self, model_runtime: ModelRuntime) -> Self {
        self.model_runtime = Some(model_runtime);
        self
    }

    #[must_use]
    pub fn with_credentials(mut self, credentials: Arc<dyn CredentialStore>) -> Self {
        self.credentials = Some(credentials);
        self
    }

    #[must_use]
    pub fn with_tool_runtime(mut self, tool_runtime: Arc<ToolRuntime>) -> Self {
        self.tool_runtime = Some(tool_runtime);
        self
    }

    #[must_use]
    pub fn with_patch_store(mut self, patch_store: Arc<PatchStore>) -> Self {
        self.patch_store = Some(patch_store);
        self
    }

    #[must_use]
    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    #[must_use]
    pub fn with_plugin_manager(mut self, manager: Arc<PluginManager>) -> Self {
        self.plugin_manager = Some(manager);
        self
    }

    #[must_use]
    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.skill_registry = Some(registry);
        self
    }

    #[must_use]
    pub fn with_memory(mut self, memory: ProjectMemory) -> Self {
        self.memory = Some(memory);
        self
    }

    #[must_use]
    pub fn with_repository(mut self, repository: RepositoryIndex) -> Self {
        self.repository = Some(repository);
        self
    }

    /// LSP metadata 可启动时注入，但 Language Server 进程仍保持 sleeping。
    #[must_use]
    pub fn with_lsp(mut self, lsp: LspManager) -> Self {
        self.lsp = Some(lsp);
        self
    }

    #[must_use]
    pub fn with_browser_runtime(mut self, runtime: Arc<BrowserRuntime>) -> Self {
        self.browser_runtime = Some(runtime);
        self
    }

    #[must_use]
    pub fn with_agent_catalog(mut self, catalog: AgentCatalog) -> Self {
        self.agent_catalog = Some(catalog);
        self
    }

    #[must_use]
    pub fn with_agent_control_plane(
        mut self,
        messages: AgentMessageBus,
        file_leases: FileLeaseManager,
        state: AgentStateStore,
        budgets: AgentBudgetManager,
    ) -> Self {
        self.agent_messages = Some(messages);
        self.file_leases = Some(file_leases);
        self.agent_state = Some(state);
        self.agent_budgets = Some(budgets);
        self
    }

    /// 注入 Default < Global < Project < Session < Runtime 的有效配置。
    pub fn with_config(mut self, config: ConfigManager) -> Result<Self, ApplicationError> {
        let effective_config = config.effective();
        let mode_profile = ModeProfile::resolve(&effective_config.settings);
        self.agent_budget_policy = mode_agent_budget(&mode_profile);
        if let Some(runtime) = self.tool_runtime.as_ref() {
            runtime
                .set_approval_policy(permission_approval_policy(
                    effective_config.settings.permission_mode,
                ))
                .expect("composition-time ToolRuntime permission mutex must be available");
        }
        if let Some(runtime) = self.model_runtime.as_mut() {
            runtime
                .set_failover_policy(model_route_policy(&effective_config)?)
                .map_err(|error| ApplicationError::new(error.code, error.message))?;
        }
        self.config = config;
        self.effective_config = effective_config;
        self.mode_profile = mode_profile;
        Ok(self)
    }

    pub fn boot(&mut self) -> Result<StatusView, ApplicationError> {
        self.publish(
            HarnessEvent::SystemStarted {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                mode: self.mode_profile().mode.to_string(),
            },
            EventScope::default(),
            EventPriority::Critical,
        )?;
        let state = self.recover_session()?;
        if state.version == 0 {
            self.apply_session_command(SessionCommand::CreateSession {
                session_id: self.session_id.clone(),
                project_id: self.project_id.clone(),
            })?;
        }
        self.reconcile_model_session()?;
        self.ensure_context_series()?;
        self.reconcile_goal_context()?;
        self.reconcile_agent_runtime()?;
        self.publish(
            HarnessEvent::SystemReady {
                project_root: self.project_root.clone(),
            },
            self.session_scope(),
            EventPriority::Critical,
        )?;
        self.publish(
            HarnessEvent::SessionChanged {
                status: "active".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        self.status()
    }

    pub fn set_goal(&mut self, text: &str) -> Result<StatusView, ApplicationError> {
        let state = self.recover_session()?;
        let revision = GoalRevision {
            id: GoalRevisionId::from(self.ids.next_id("goal")),
            parent_revision_id: state.goal.current_revision_id.clone(),
            text: text.to_owned(),
            created_by: ActorId::from("user:terminal"),
            reason: "terminal-input".to_owned(),
            created_at_millis: self.clock.now_unix_millis(),
        };
        self.apply_session_command(SessionCommand::ReviseGoal {
            revision: revision.clone(),
        })?;
        self.replace_goal_context(&revision)?;
        self.publish_context_changed()?;
        let current = self.recover_session()?;
        self.publish(
            HarnessEvent::GoalChanged {
                revision_id: Some(revision.id),
                text: Some(revision.text),
                locked: current.goal.locked,
            },
            self.session_scope(),
            EventPriority::Critical,
        )?;
        self.status()
    }

    pub fn clear_goal(&mut self) -> Result<StatusView, ApplicationError> {
        self.apply_session_command(SessionCommand::ClearGoal {
            reason: "user-terminal-clear".to_owned(),
        })?;
        self.replace_context_source("goal:active", None)?;
        self.publish_context_changed()?;
        self.publish(
            HarnessEvent::GoalChanged {
                revision_id: None,
                text: None,
                locked: false,
            },
            self.session_scope(),
            EventPriority::Critical,
        )?;
        self.status()
    }

    pub fn goal_history(&self, limit: usize) -> Result<GoalHistoryView, ApplicationError> {
        let state = self.recover_session()?;
        let mut revisions = state.goal.revisions.values().cloned().collect::<Vec<_>>();
        revisions.sort_by(|left, right| {
            right
                .created_at_millis
                .cmp(&left.created_at_millis)
                .then_with(|| right.id.cmp(&left.id))
        });
        revisions.truncate(limit.clamp(1, 200));
        Ok(GoalHistoryView {
            current_revision_id: state.goal.current_revision_id,
            locked: state.goal.locked,
            revisions,
        })
    }

    pub fn sessions(&self) -> Result<Vec<SessionSummaryView>, ApplicationError> {
        self.store
            .list_session_ids()
            .map_err(storage_error)?
            .into_iter()
            .map(|session_id| {
                let state = self.recover_session_id(&session_id)?;
                let goal = state
                    .goal
                    .current_revision_id
                    .as_ref()
                    .and_then(|id| state.goal.revisions.get(id))
                    .map(|revision| revision.text.clone());
                Ok(SessionSummaryView {
                    current: session_id == self.session_id,
                    session_id,
                    status: state.status,
                    version: state.version,
                    goal,
                    parent_session_id: state.parent_session_id,
                    forked_from_checkpoint_id: state.forked_from_checkpoint_id,
                })
            })
            .collect()
    }

    /// 创建 checkpoint 后重置当前 Context；保留 Goal 与 hard-required/pinned 项。
    pub fn reset_context(&mut self) -> Result<ContextResetView, ApplicationError> {
        if let Some(mission_id) = self.active_mission_id.as_ref()
            && self.recover_mission(mission_id)?.status == MissionStatus::Running
        {
            return Err(ApplicationError::new(
                "reset-active-mission",
                "活动 Mission 运行或等待审批时不能 reset；请先完成或取消",
            ));
        }
        let checkpoint = self.create_checkpoint_record(Some("before-context-reset"))?;
        let current = self.active_context_series()?;
        let retained = current
            .items
            .iter()
            .filter(|item| item.context.kind == ContextKind::Goal || item.context.hard_required)
            .cloned()
            .collect::<Vec<_>>();
        let next = ContextSeries {
            id: ContextSeriesId::from(self.ids.next_id("series")),
            session_id: self.session_id.clone(),
            parent_series_id: Some(current.id.clone()),
            restored_from_checkpoint_id: None,
            items: retained,
            created_at_millis: self.clock.now_unix_millis(),
        };
        let view = ContextResetView {
            checkpoint_id: checkpoint.id,
            previous_series_id: current.id.clone(),
            next_series_id: next.id.clone(),
            removed_items: current.items.len().saturating_sub(next.items.len()),
            retained_items: next.items.len(),
        };
        self.store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: Some(current.id),
                next_series: next,
                compaction_record: None,
            })
            .map_err(context_storage_error)?;
        self.active_mission_id = None;
        self.publish_context_changed()?;
        Ok(view)
    }

    pub fn set_goal_lock(&mut self, locked: bool) -> Result<StatusView, ApplicationError> {
        self.apply_session_command(SessionCommand::SetGoalLock { locked })?;
        let state = self.recover_session()?;
        let revision = state
            .goal
            .current_revision_id
            .as_ref()
            .and_then(|id| state.goal.revisions.get(id));
        self.publish(
            HarnessEvent::GoalChanged {
                revision_id: revision.map(|revision| revision.id.clone()),
                text: revision.map(|revision| revision.text.clone()),
                locked,
            },
            self.session_scope(),
            EventPriority::Critical,
        )?;
        self.status()
    }

    pub fn status(&self) -> Result<StatusView, ApplicationError> {
        let state = self.recover_session()?;
        let goal = state
            .goal
            .current_revision_id
            .as_ref()
            .and_then(|id| state.goal.revisions.get(id))
            .map(|revision| revision.text.clone());
        let (model, reasoning) = self.model_runtime.as_ref().map_or_else(
            || Ok(("fake/deterministic".to_owned(), "off".to_owned())),
            |runtime| {
                let view = runtime
                    .view()
                    .map_err(|error| ApplicationError::new(error.code, error.message))?;
                Ok::<_, ApplicationError>((
                    format!("{}/{}", view.provider_id, view.model_id),
                    reasoning_name(view.reasoning_requested).to_owned(),
                ))
            },
        )?;
        Ok(StatusView {
            session_id: state.session_id,
            session_status: state.status,
            session_version: state.version,
            goal,
            goal_locked: state.goal.locked,
            active_mission_id: self.active_mission_id.clone(),
            mode: self.mode_profile().mode.to_string(),
            model,
            reasoning,
        })
    }

    /// 返回所有有效设置、每项来源以及实际生效的模式预算。
    #[must_use]
    pub fn config(&self) -> EffectiveConfigView {
        self.effective_config.clone()
    }

    #[must_use]
    pub fn mode_profile(&self) -> ModeProfile {
        self.mode_profile.clone()
    }

    #[must_use]
    pub fn statusbar_visible(&self) -> bool {
        self.effective_config.settings.ui_statusbar
    }

    /// Session 层写入 Kernel Event Store；Runtime 层仅在当前进程存活。
    pub fn set_setting(
        &mut self,
        key: &str,
        value: &str,
        layer: ConfigLayer,
    ) -> Result<EffectiveConfigView, ApplicationError> {
        let mut candidate = self.config.clone();
        match layer {
            ConfigLayer::Session => candidate.set_session(key, value).map_err(config_error)?,
            ConfigLayer::Runtime => candidate.set_runtime(key, value).map_err(config_error)?,
            _ => {
                return Err(ApplicationError::new(
                    "config-layer-read-only",
                    "终端只允许修改 session 或 runtime；global/project 请编辑 TOML",
                ));
            }
        }
        let effective_config = candidate.effective();
        let previous_policy = self.tool_runtime.as_ref().map_or(Ok(None), |runtime| {
            let previous = runtime.approval_policy().map_err(tool_error)?;
            let target = permission_approval_policy(effective_config.settings.permission_mode);
            if previous != target {
                runtime.set_approval_policy(target).map_err(tool_error)?;
                Ok(Some(previous))
            } else {
                Ok(None)
            }
        })?;
        let previous_route = if let Some(runtime) = self.model_runtime.as_mut() {
            let previous = runtime.failover_policy();
            let target = model_route_policy(&effective_config)?;
            if previous != target {
                if let Err(error) = runtime.set_failover_policy(target) {
                    if let (Some(tool_runtime), Some(previous)) =
                        (self.tool_runtime.as_ref(), previous_policy)
                    {
                        let _ = tool_runtime.set_approval_policy(previous);
                    }
                    return Err(ApplicationError::new(error.code, error.message));
                }
                Some(previous)
            } else {
                None
            }
        } else {
            None
        };
        if layer == ConfigLayer::Session {
            let state = self.recover_session()?;
            if state.settings.get(key).map(String::as_str) != Some(value)
                && let Err(error) = self.apply_session_command(SessionCommand::SetSetting {
                    key: key.to_owned(),
                    value: value.to_owned(),
                })
            {
                if let (Some(runtime), Some(previous)) =
                    (self.tool_runtime.as_ref(), previous_policy)
                {
                    let _ = runtime.set_approval_policy(previous);
                }
                if let (Some(runtime), Some(previous)) =
                    (self.model_runtime.as_mut(), previous_route)
                {
                    let _ = runtime.set_failover_policy(previous);
                }
                return Err(error);
            }
        }
        let mode_profile = ModeProfile::resolve(&effective_config.settings);
        self.agent_budget_policy = mode_agent_budget(&mode_profile);
        self.config = candidate;
        self.effective_config = effective_config;
        self.mode_profile = mode_profile;
        Ok(self.effective_config.clone())
    }

    pub fn clear_setting(
        &mut self,
        key: &str,
        layer: ConfigLayer,
    ) -> Result<EffectiveConfigView, ApplicationError> {
        let mut candidate = self.config.clone();
        match layer {
            ConfigLayer::Session => candidate.clear_session(key).map_err(config_error)?,
            ConfigLayer::Runtime => candidate.clear_runtime(key).map_err(config_error)?,
            _ => {
                return Err(ApplicationError::new(
                    "config-layer-read-only",
                    "终端只允许清除 session 或 runtime 设置",
                ));
            }
        }
        let effective_config = candidate.effective();
        let previous_policy = self.tool_runtime.as_ref().map_or(Ok(None), |runtime| {
            let previous = runtime.approval_policy().map_err(tool_error)?;
            let target = permission_approval_policy(effective_config.settings.permission_mode);
            if previous != target {
                runtime.set_approval_policy(target).map_err(tool_error)?;
                Ok(Some(previous))
            } else {
                Ok(None)
            }
        })?;
        let previous_route = if let Some(runtime) = self.model_runtime.as_mut() {
            let previous = runtime.failover_policy();
            let target = model_route_policy(&effective_config)?;
            if previous != target {
                if let Err(error) = runtime.set_failover_policy(target) {
                    if let (Some(tool_runtime), Some(previous)) =
                        (self.tool_runtime.as_ref(), previous_policy)
                    {
                        let _ = tool_runtime.set_approval_policy(previous);
                    }
                    return Err(ApplicationError::new(error.code, error.message));
                }
                Some(previous)
            } else {
                None
            }
        } else {
            None
        };
        if layer == ConfigLayer::Session {
            let state = self.recover_session()?;
            if state.settings.contains_key(key)
                && let Err(error) = self.apply_session_command(SessionCommand::ClearSetting {
                    key: key.to_owned(),
                })
            {
                if let (Some(runtime), Some(previous)) =
                    (self.tool_runtime.as_ref(), previous_policy)
                {
                    let _ = runtime.set_approval_policy(previous);
                }
                if let (Some(runtime), Some(previous)) =
                    (self.model_runtime.as_mut(), previous_route)
                {
                    let _ = runtime.set_failover_policy(previous);
                }
                return Err(error);
            }
        }
        let mode_profile = ModeProfile::resolve(&effective_config.settings);
        self.agent_budget_policy = mode_agent_budget(&mode_profile);
        self.config = candidate;
        self.effective_config = effective_config;
        self.mode_profile = mode_profile;
        Ok(self.effective_config.clone())
    }

    /// Composition root 记录从进程入口到 Backend 就绪的真实启动耗时。
    pub fn record_startup_millis(&self, elapsed_millis: u64) {
        self.record_profile_sample("startup", elapsed_millis);
    }

    pub fn profile(&self) -> Result<ProfileView, ApplicationError> {
        let mut samples = self
            .profile_samples
            .lock()
            .map_err(|_| ApplicationError::new("profile-poisoned", "profile samples"))?
            .clone();
        if let Some(runtime) = self.tool_runtime.as_ref() {
            let tools = samples.entry("tool-wall".to_owned()).or_default();
            for record in runtime.journal().list().map_err(tool_error)? {
                let elapsed = record
                    .updated_at_millis
                    .saturating_sub(record.created_at_millis);
                tools.push_back(u64::try_from(elapsed).unwrap_or_default());
                while tools.len() > MAX_PROFILE_SAMPLES {
                    tools.pop_front();
                }
            }
        }
        let metrics = samples
            .into_iter()
            .filter_map(|(name, values)| profile_metric(name, values))
            .collect();
        Ok(ProfileView {
            uptime_millis: u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            metrics,
        })
    }

    pub fn why(&self) -> Result<WhyView, ApplicationError> {
        let status = self.status()?;
        let series = self.active_context_series()?;
        let compiled = ContextBroker
            .compile_for_role(
                Role::Supervisor,
                series.items.into_iter().map(|item| item.context).collect(),
                &self.context_budget(),
                self.clock.now_unix_millis(),
            )
            .map_err(|error| ApplicationError::new(error.code, error.to_string()))?;
        let context_sources = compiled
            .selected
            .into_iter()
            .take(16)
            .map(|item| format!("{:?} · {}", item.kind, item.source_identity))
            .collect::<Vec<_>>();
        let mut recent_tools = self
            .tool_runtime
            .as_ref()
            .map_or(Ok(Vec::new()), |runtime| {
                runtime.journal().list().map_err(tool_error)
            })?;
        recent_tools.sort_by(|left, right| {
            right
                .updated_at_millis
                .cmp(&left.updated_at_millis)
                .then_with(|| left.id.cmp(&right.id))
        });
        let recent_tools = recent_tools
            .into_iter()
            .take(8)
            .map(|record| WhyToolEvidence {
                invocation_id: record.id,
                tool_name: record.tool_name,
                status: record.status,
                updated_at_millis: record.updated_at_millis,
            })
            .collect::<Vec<_>>();
        let plan = status
            .active_mission_id
            .as_ref()
            .map(|mission_id| self.plan_for(mission_id))
            .transpose()?;
        let summary = plan.map_or_else(
            || "当前没有活动 Mission；依据是 durable Goal 与筛选后的 Context。".to_owned(),
            |plan| {
                format!(
                    "当前计划：accepted={} running={} pending={} blocked={}；仅展示可审计依据摘要。",
                    plan.accepted, plan.running, plan.pending, plan.blocked
                )
            },
        );
        Ok(WhyView {
            goal: status.goal,
            mission_id: status.active_mission_id,
            summary,
            context_sources,
            recent_tools,
        })
    }

    pub fn model(&self) -> Result<ModelView, ApplicationError> {
        let runtime = self.model_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("model-runtime-missing", "Model Runtime 尚未注入")
        })?;
        runtime
            .view()
            .map(model_view)
            .map_err(|error| ApplicationError::new(error.code, error.message))
    }

    #[must_use]
    pub fn failover(&self) -> FailoverView {
        FailoverView {
            enabled: self.effective_config.settings.failover_enabled,
            cost_confirmed: self.effective_config.settings.failover_cost_confirmed,
            targets: parse_failover_targets(&self.effective_config.settings.failover_targets)
                .into_iter()
                .map(|target| format!("{}/{}", target.provider_id, target.model_id))
                .collect(),
        }
    }

    pub fn configure_failover(
        &mut self,
        enabled: bool,
        cost_confirmed: bool,
        targets: &[String],
    ) -> Result<FailoverView, ApplicationError> {
        let joined = targets.join(",");
        let mut candidate = self.config.clone();
        candidate
            .set_runtime_many(&[
                ("failover.targets", joined.as_str()),
                (
                    "failover.cost-confirmed",
                    if cost_confirmed { "true" } else { "false" },
                ),
                ("failover.enabled", if enabled { "true" } else { "false" }),
            ])
            .map_err(config_error)?;
        let effective_config = candidate.effective();
        if let Some(runtime) = self.model_runtime.as_mut() {
            runtime
                .set_failover_policy(model_route_policy(&effective_config)?)
                .map_err(|error| ApplicationError::new(error.code, error.message))?;
        }
        self.config = candidate;
        self.effective_config = effective_config;
        self.mode_profile = ModeProfile::resolve(&self.effective_config.settings);
        Ok(self.failover())
    }

    #[must_use]
    pub fn models(&self) -> Vec<ModelCapability> {
        self.model_runtime
            .as_ref()
            .map_or_else(Vec::new, ModelRuntime::models)
    }

    pub fn refresh_models(
        &mut self,
        provider_id: Option<ProviderId>,
    ) -> Result<Vec<ModelCapability>, ApplicationError> {
        let runtime = self.model_runtime.as_mut().ok_or_else(|| {
            ApplicationError::new("model-runtime-missing", "Model Runtime 尚未注入")
        })?;
        let provider = provider_id.ok_or_else(|| {
            ApplicationError::new(
                "model-refresh-provider-required",
                "刷新必须显式指定一个 Provider，避免无意访问多个网络目录",
            )
        })?;
        runtime
            .refresh_provider(&provider)
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        Ok(runtime.models())
    }

    pub fn select_model(
        &mut self,
        provider_id: ProviderId,
        model_id: ModelId,
    ) -> Result<ModelView, ApplicationError> {
        let state = self.recover_session()?;
        if state.model.provider_id.as_ref() == Some(&provider_id)
            && state.model.model_id.as_ref() == Some(&model_id)
        {
            return self.model();
        }
        let runtime = self.model_runtime.as_mut().ok_or_else(|| {
            ApplicationError::new("model-runtime-missing", "Model Runtime 尚未注入")
        })?;
        let previous = runtime
            .view()
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        let selected = runtime
            .select(provider_id.clone(), model_id.clone())
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        if let Err(error) = self.apply_session_command(SessionCommand::SelectModel {
            provider_id,
            model_id,
        }) {
            let _ = self
                .model_runtime
                .as_mut()
                .expect("runtime 已检查")
                .select(previous.provider_id, previous.model_id);
            return Err(error);
        }
        self.publish_model_changed(&selected)?;
        Ok(model_view(selected))
    }

    /// 原子注册新 Provider 并把当前 Session 切到其默认模型。
    pub fn register_and_select_provider(
        &mut self,
        provider: Arc<dyn ModelProvider>,
        provider_id: ProviderId,
        model_id: ModelId,
    ) -> Result<ModelView, ApplicationError> {
        let previous = self.model_runtime.clone().ok_or_else(|| {
            ApplicationError::new("model-runtime-missing", "Model Runtime 尚未注入")
        })?;
        self.model_runtime
            .as_mut()
            .expect("runtime 已检查")
            .register_provider(provider)
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        match self.select_model(provider_id, model_id) {
            Ok(view) => Ok(view),
            Err(error) => {
                self.model_runtime = Some(previous);
                Err(error)
            }
        }
    }

    pub fn set_reasoning(
        &mut self,
        reasoning: ReasoningLevel,
    ) -> Result<ModelView, ApplicationError> {
        let state = self.recover_session()?;
        if state.model.reasoning == reasoning {
            return self.model();
        }
        let runtime = self.model_runtime.as_mut().ok_or_else(|| {
            ApplicationError::new("model-runtime-missing", "Model Runtime 尚未注入")
        })?;
        let previous = runtime
            .view()
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        let selected = runtime
            .set_reasoning(reasoning)
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        if let Err(error) = self.apply_session_command(SessionCommand::SetReasoning { reasoning }) {
            let _ = self
                .model_runtime
                .as_mut()
                .expect("runtime 已检查")
                .set_reasoning(previous.reasoning_requested);
            return Err(error);
        }
        self.publish_model_changed(&selected)?;
        Ok(model_view(selected))
    }

    pub fn account(&self) -> Result<AuthView, ApplicationError> {
        let credentials = self.credentials.as_ref().ok_or_else(|| {
            ApplicationError::new("credential-store-missing", "Credential Store 尚未注入")
        })?;
        let configured = credentials
            .get(&CredentialId::new(OPENAI_API_KEY_CREDENTIAL_ID))
            .map_err(|error| ApplicationError::new(error.code, error.message))?
            .is_some();
        Ok(AuthView {
            provider_id: ProviderId::from("openai"),
            auth_method: "api-key".to_owned(),
            configured,
            storage: "os-credential-store".to_owned(),
        })
    }

    pub fn logout(&self, provider: &str) -> Result<bool, ApplicationError> {
        if provider != "openai" {
            return Err(ApplicationError::new(
                "logout-provider-unsupported",
                format!("当前只实现 OpenAI API Key logout：{provider}"),
            ));
        }
        self.credentials
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("credential-store-missing", "Credential Store 尚未注入")
            })?
            .delete(&CredentialId::new(OPENAI_API_KEY_CREDENTIAL_ID))
            .map_err(|error| ApplicationError::new(error.code, error.message))
    }

    pub fn tools(&self) -> Result<ToolRuntimeView, ApplicationError> {
        let runtime = self.tool_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("tool-runtime-missing", "Tool Runtime 尚未注入")
        })?;
        Ok(ToolRuntimeView {
            tools: runtime.tools(),
            pending_approvals: runtime.pending_approvals().map_err(tool_error)?.len(),
            active_grants: runtime.active_grants().map_err(tool_error)?.len(),
            approval_policy: runtime.approval_policy().map_err(tool_error)?,
            permission_rules: runtime.permission_rules().map_err(tool_error)?,
        })
    }

    pub fn add_permission_rule(
        &self,
        effect: PermissionRuleEffect,
        action: PermissionRuleAction,
        pattern: String,
    ) -> Result<PermissionRule, ApplicationError> {
        let runtime = self.tool_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("tool-runtime-missing", "Tool Runtime 尚未注入")
        })?;
        let rule = PermissionRule {
            id: self.ids.next_id("permission-rule"),
            effect,
            action,
            pattern,
        };
        runtime
            .add_permission_rule(rule.clone())
            .map_err(tool_error)?;
        Ok(rule)
    }

    pub fn remove_permission_rule(&self, rule_id: &str) -> Result<bool, ApplicationError> {
        self.tool_runtime
            .as_ref()
            .ok_or_else(|| ApplicationError::new("tool-runtime-missing", "Tool Runtime 尚未注入"))?
            .remove_permission_rule(rule_id)
            .map_err(tool_error)
    }

    pub fn replace_permission_rules(
        &self,
        rules: Vec<PermissionRule>,
    ) -> Result<(), ApplicationError> {
        self.tool_runtime
            .as_ref()
            .ok_or_else(|| ApplicationError::new("tool-runtime-missing", "Tool Runtime 尚未注入"))?
            .replace_permission_rules(rules)
            .map_err(tool_error)
    }

    pub fn browser_view(&self) -> Result<BrowserCapabilityView, ApplicationError> {
        self.browser_runtime.as_ref().map_or(
            Ok(BrowserCapabilityView {
                configured: false,
                runtime: None,
            }),
            |runtime| {
                runtime
                    .view()
                    .map(|view| BrowserCapabilityView {
                        configured: true,
                        runtime: Some(view),
                    })
                    .map_err(browser_error)
            },
        )
    }

    pub fn open_browser(&self) -> Result<BrowserCapabilityView, ApplicationError> {
        let runtime = self.browser_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new(
                "browser-runtime-unconfigured",
                "配置 HARNESS_BROWSER_PYTHON、HARNESS_BROWSER_EXECUTABLE 和 HARNESS_BROWSER_ALLOWED_ORIGINS",
            )
        })?;
        let view = runtime
            .open(self.clock.now_unix_millis())
            .map_err(browser_error)?;
        self.publish(
            HarnessEvent::BrowserStatus {
                session_id: view.session_id.to_string(),
                status: format!("{:?}", view.status).to_lowercase(),
                detail: "isolated profile ready".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(BrowserCapabilityView {
            configured: true,
            runtime: Some(view),
        })
    }

    pub fn navigate_browser(&self, url: &str) -> Result<BrowserResult, ApplicationError> {
        let runtime = self.browser_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("browser-runtime-unconfigured", "Browser Runtime 尚未配置")
        })?;
        let result = runtime
            .execute(
                BrowserActionId::from(self.ids.next_id("browser-action")),
                BrowserCommand::Navigate {
                    url: url.to_owned(),
                },
                self.clock.now_unix_millis(),
            )
            .map_err(browser_error)?;
        self.publish(
            HarnessEvent::BrowserStatus {
                session_id: runtime
                    .view()
                    .map_err(browser_error)?
                    .session_id
                    .to_string(),
                status: "navigated".to_owned(),
                detail: url.to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(result)
    }

    pub fn close_browser(&self) -> Result<BrowserCapabilityView, ApplicationError> {
        let runtime = self.browser_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("browser-runtime-unconfigured", "Browser Runtime 尚未配置")
        })?;
        let view = runtime
            .close(self.clock.now_unix_millis())
            .map_err(browser_error)?;
        self.publish(
            HarnessEvent::BrowserStatus {
                session_id: view.session_id.to_string(),
                status: "closed".to_owned(),
                detail: "process reaped".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(BrowserCapabilityView {
            configured: true,
            runtime: Some(view),
        })
    }

    pub fn handoff_browser(&self) -> Result<BrowserCapabilityView, ApplicationError> {
        let runtime = self.browser_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("browser-runtime-unconfigured", "Browser Runtime 尚未配置")
        })?;
        let view = runtime
            .handoff(
                BrowserActionId::from(self.ids.next_id("browser-action")),
                self.clock.now_unix_millis(),
            )
            .map_err(browser_error)?;
        self.publish(
            HarnessEvent::BrowserStatus {
                session_id: view.session_id.to_string(),
                status: "user-control".to_owned(),
                detail: "visible browser uses the same isolated profile".to_owned(),
            },
            self.session_scope(),
            EventPriority::Critical,
        )?;
        Ok(BrowserCapabilityView {
            configured: true,
            runtime: Some(view),
        })
    }

    pub fn reclaim_browser(&self) -> Result<BrowserCapabilityView, ApplicationError> {
        let runtime = self.browser_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("browser-runtime-unconfigured", "Browser Runtime 尚未配置")
        })?;
        let view = runtime
            .reclaim(
                BrowserActionId::from(self.ids.next_id("browser-action")),
                self.clock.now_unix_millis(),
            )
            .map_err(browser_error)?;
        self.publish(
            HarnessEvent::BrowserStatus {
                session_id: view.session_id.to_string(),
                status: "agent-control".to_owned(),
                detail: "headless browser restored with the same isolated profile".to_owned(),
            },
            self.session_scope(),
            EventPriority::Critical,
        )?;
        Ok(BrowserCapabilityView {
            configured: true,
            runtime: Some(view),
        })
    }

    pub fn browser_actions(&self) -> Result<Vec<BrowserActionRecord>, ApplicationError> {
        self.browser_runtime
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("browser-runtime-unconfigured", "Browser Runtime 尚未配置")
            })?
            .actions()
            .map_err(browser_error)
    }

    pub fn approve_tool(
        &mut self,
        invocation_id: ToolInvocationId,
        scope: GrantScope,
    ) -> Result<ToolInvocationRecord, ApplicationError> {
        let runtime = self.tool_runtime.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new("tool-runtime-missing", "Tool Runtime 尚未注入")
        })?;
        let invocation = runtime
            .journal()
            .get(&invocation_id)
            .map_err(tool_error)?
            .ok_or_else(|| {
                ApplicationError::new("tool-invocation-not-found", invocation_id.to_string())
            })?;
        let has_continuation = self
            .pending_model_continuations
            .lock()
            .map_err(|_| {
                ApplicationError::new("model-continuation-poisoned", "pending continuation lock")
            })?
            .contains_key(&invocation_id);
        let has_parallel_continuation = self
            .pending_parallel_tool_continuations
            .contains_key(&invocation_id);
        if invocation.envelope.origin == InvocationOrigin::Agent
            && !has_continuation
            && !has_parallel_continuation
        {
            return Err(ApplicationError::new(
                "model-continuation-unavailable",
                "Agent Tool 的模型 continuation 已因重启丢失；为避免重复副作用，请 /deny 后重新提交任务",
            ));
        }
        let write_lease = self.acquire_agent_write_lease(
            &invocation.tool_name,
            &invocation.args,
            invocation.envelope.run_id.as_ref(),
        )?;
        let response = runtime.resume_after_approval(
            &invocation_id,
            &invocation.envelope,
            scope,
            PermissionGrantId::from(self.ids.next_id("grant")),
            PermissionRequestId::from(self.ids.next_id("approval")),
            self.clock.now_unix_millis(),
        );
        self.release_agent_write_lease(write_lease)?;
        let response = response.map_err(tool_error)?;
        self.publish(
            HarnessEvent::ToolStatus {
                tool: response.invocation.tool_name.clone(),
                status: format!("{:?}", response.invocation.status).to_lowercase(),
                summary: response.invocation.id.to_string(),
            },
            self.mission_scope(&response.invocation.envelope.mission_id),
            EventPriority::Normal,
        )?;
        let resumed_invocation = response.invocation;
        if has_parallel_continuation && resumed_invocation.status == ToolInvocationStatus::Completed
        {
            let pending = self
                .pending_parallel_tool_continuations
                .remove(&invocation_id)
                .expect("并行 continuation 已检查");
            let request_id = invocation.approval_request_id.clone().ok_or_else(|| {
                ApplicationError::new("tool-approval-id-missing", invocation_id.to_string())
            })?;
            let mission_id = pending.request.contract.mission_id.clone();
            let run_id = pending.request.contract.run_id.clone();
            self.active_mission_id = Some(mission_id.clone());
            self.apply_mission_command(
                &mission_id,
                MissionCommand::ResolveApproval {
                    approval_id: ApprovalId::from(request_id.to_string()),
                    decision: ApprovalDecision::Allow,
                },
            )?;
            let claimed = self.claim_agent_resume_effect(&mission_id, &run_id)?;
            let output = resumed_invocation.result.clone().ok_or_else(|| {
                ApplicationError::new("tool-result-missing", resumed_invocation.tool_name.clone())
            })?;
            let mut continuation = pending.waiting.continuation;
            self.record_lsp_tool_fact(
                &pending.waiting.waiting_call.name,
                &pending.waiting.waiting_call.arguments,
                &output,
                Some(&run_id),
            )?;
            append_parallel_tool_result(&mut continuation, &pending.waiting.waiting_call, output);
            let mut request = pending.request;
            if let Some(next_waiting) = self.execute_parallel_tool_calls(
                &mut request,
                continuation,
                pending.waiting.remaining_calls,
            )? {
                let next_invocation_id = next_waiting.invocation.id.clone();
                let next_request_id = next_waiting
                    .invocation
                    .approval_request_id
                    .clone()
                    .ok_or_else(|| {
                        ApplicationError::new(
                            "tool-approval-id-missing",
                            next_invocation_id.to_string(),
                        )
                    })?;
                let next_approval = runtime
                    .pending_approvals()
                    .map_err(tool_error)?
                    .into_iter()
                    .find(|approval| approval.id == next_request_id)
                    .ok_or_else(|| {
                        ApplicationError::new(
                            "tool-approval-request-not-found",
                            next_request_id.to_string(),
                        )
                    })?;
                self.apply_mission_command(
                    &mission_id,
                    MissionCommand::RequestApproval {
                        node_id: request.contract.task_id.clone(),
                        run_id: run_id.clone(),
                        approval_id: ApprovalId::from(next_request_id.to_string()),
                        action: serde_json::json!({
                            "tool":next_waiting.invocation.tool_name.clone(),
                            "toolInvocationId":next_invocation_id,
                            "permission":next_waiting.invocation.permission_action.clone(),
                            "risk":format!("{:?}",next_approval.risk).to_lowercase()
                        })
                        .to_string(),
                        reason: next_approval.reason,
                    },
                )?;
                self.set_agent_session_status(&run_id, AgentSessionStatus::WaitingApproval)?;
                self.set_agent_session_previous_response(
                    &run_id,
                    next_waiting.continuation.previous_response_id.clone(),
                )?;
                self.store
                    .complete_effect(EffectCompletion {
                        fence: CompletionFence {
                            effect_id: claimed.claim.effect_id,
                            mission_epoch: claimed.claim.mission_epoch,
                            claim_token: claimed.claim.claim_token,
                            run_fence: claimed.claim.run_fence,
                        },
                        outcome: EffectOutcome::Completed,
                        result: Some(serde_json::json!({
                            "waitingApproval":next_request_id,
                            "toolInvocationId":next_invocation_id.clone()
                        })),
                        error: None,
                        recorded_at_millis: self.clock.now_unix_millis(),
                    })
                    .map_err(storage_error)?;
                self.pending_parallel_tool_continuations.insert(
                    next_invocation_id,
                    PendingParallelToolContinuation {
                        request,
                        waiting: next_waiting,
                        prompt: pending.prompt,
                    },
                );
                return Ok(resumed_invocation);
            }
            self.set_agent_session_status(&run_id, AgentSessionStatus::Running)?;
            self.ready_parallel_resume =
                Some(self.build_parallel_resume_job(request, claimed, pending.prompt)?);
            return Ok(resumed_invocation);
        }
        if has_continuation && resumed_invocation.status == ToolInvocationStatus::Completed {
            let request_id = invocation.approval_request_id.ok_or_else(|| {
                ApplicationError::new("tool-approval-id-missing", invocation_id.to_string())
            })?;
            let mission_id = invocation.envelope.mission_id;
            self.active_mission_id = Some(mission_id.clone());
            self.apply_mission_command(
                &mission_id,
                MissionCommand::ResolveApproval {
                    approval_id: ApprovalId::from(request_id.to_string()),
                    decision: ApprovalDecision::Allow,
                },
            )?;
            let prompt = self.recover_mission(&mission_id)?.goal;
            self.drive_effects(&mission_id, &prompt)?;
            let state = self.recover_mission(&mission_id)?;
            if state.status == MissionStatus::Running
                && state
                    .nodes
                    .values()
                    .all(|node| node.status == NodeStatus::Accepted)
            {
                self.apply_mission_command(&mission_id, MissionCommand::CompleteMission {})?;
            }
        }
        Ok(resumed_invocation)
    }

    pub fn take_ready_parallel_resume(&mut self) -> Option<PreparedAgentTeam> {
        self.ready_parallel_resume.take()
    }

    pub fn deny_tool(
        &mut self,
        invocation_id: ToolInvocationId,
    ) -> Result<ToolInvocationRecord, ApplicationError> {
        let runtime = self.tool_runtime.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new("tool-runtime-missing", "Tool Runtime 尚未注入")
        })?;
        let invocation = runtime
            .journal()
            .get(&invocation_id)
            .map_err(tool_error)?
            .ok_or_else(|| {
                ApplicationError::new("tool-invocation-not-found", invocation_id.to_string())
            })?;
        let response = runtime
            .deny_approval(
                &invocation_id,
                PermissionGrantId::from(self.ids.next_id("grant-unused")),
                self.clock.now_unix_millis(),
            )
            .map_err(tool_error)?;
        let continuation = self
            .pending_model_continuations
            .lock()
            .map_err(|_| {
                ApplicationError::new("model-continuation-poisoned", "pending continuation lock")
            })?
            .remove(&invocation_id);
        let parallel_continuation = self
            .pending_parallel_tool_continuations
            .remove(&invocation_id);
        let request_id = invocation.approval_request_id.ok_or_else(|| {
            ApplicationError::new("tool-approval-id-missing", invocation_id.to_string())
        })?;
        let approval_id = ApprovalId::from(request_id.to_string());
        let kernel_waiting = self
            .recover_mission(&invocation.envelope.mission_id)
            .ok()
            .and_then(|state| state.approvals.get(&approval_id).cloned())
            .is_some_and(|approval| approval.status == KernelApprovalStatus::Pending);
        if continuation.is_some() || parallel_continuation.is_some() || kernel_waiting {
            self.apply_mission_command(
                &invocation.envelope.mission_id,
                MissionCommand::ResolveApproval {
                    approval_id,
                    decision: ApprovalDecision::Deny,
                },
            )?;
        }
        if let Some(parallel) = parallel_continuation {
            let run_id = parallel.request.contract.run_id;
            let agent_id = parallel.request.contract.agent_definition_id;
            self.set_agent_session_status(&run_id, AgentSessionStatus::Failed)?;
            self.finish_agent_run(&agent_id, &run_id, BudgetEscrowStatus::Failed)?;
        }
        Ok(response.invocation)
    }

    pub fn retry_tool(
        &self,
        invocation_id: ToolInvocationId,
    ) -> Result<ToolInvocationRecord, ApplicationError> {
        let runtime = self.tool_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("tool-runtime-missing", "Tool Runtime 尚未注入")
        })?;
        let invocation = runtime
            .journal()
            .get(&invocation_id)
            .map_err(tool_error)?
            .ok_or_else(|| {
                ApplicationError::new("tool-invocation-not-found", invocation_id.to_string())
            })?;
        runtime
            .retry(
                &invocation_id,
                &invocation.envelope,
                PermissionRequestId::from(self.ids.next_id("approval")),
                self.clock.now_unix_millis(),
            )
            .map(|response| response.invocation)
            .map_err(tool_error)
    }

    pub fn invoke_tool(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<ToolInvocationRecord, ApplicationError> {
        let runtime = self.tool_runtime.as_ref().ok_or_else(|| {
            ApplicationError::new("tool-runtime-missing", "Tool Runtime 尚未注入")
        })?;
        let mission_id = self
            .active_mission_id
            .clone()
            .unwrap_or_else(|| MissionId::from("mission:terminal"));
        let response = runtime
            .invoke(ToolInvokeRequest {
                invocation_id: ToolInvocationId::from(self.ids.next_id("tool-invocation")),
                approval_request_id: PermissionRequestId::from(self.ids.next_id("approval")),
                idempotency_key: self.ids.next_id("tool-key"),
                envelope: ExecutionEnvelope {
                    project_id: self.project_id.clone(),
                    mission_id: mission_id.clone(),
                    run_id: Some(RunId::from("run:terminal")),
                    actor_id: ActorId::from("user:terminal"),
                    origin: InvocationOrigin::User,
                    information_flow: InformationFlowLabel {
                        integrity: IntegrityLabel::Trusted,
                        confidentiality: ConfidentialityLabel::ProjectPrivate,
                    },
                },
                tool_name: tool_name.to_owned(),
                args,
                now_millis: self.clock.now_unix_millis(),
            })
            .map_err(tool_error)?;
        self.publish(
            HarnessEvent::ToolStatus {
                tool: response.invocation.tool_name.clone(),
                status: format!("{:?}", response.invocation.status).to_lowercase(),
                summary: response.invocation.id.to_string(),
            },
            self.mission_scope(&mission_id),
            EventPriority::Normal,
        )?;
        Ok(response.invocation)
    }

    pub fn undo_patch(&self, patch_id: &str) -> Result<PatchRecord, ApplicationError> {
        self.patch_store
            .as_ref()
            .ok_or_else(|| ApplicationError::new("patch-store-missing", "Patch Store 尚未注入"))?
            .undo(patch_id, self.clock.now_unix_millis())
            .map_err(tool_error)
    }

    pub fn undo_latest_patch(&self) -> Result<PatchRecord, ApplicationError> {
        self.patch_store
            .as_ref()
            .ok_or_else(|| ApplicationError::new("patch-store-missing", "Patch Store 尚未注入"))?
            .undo_latest(self.clock.now_unix_millis())
            .map_err(tool_error)
    }

    pub fn patches(&self) -> Result<Vec<PatchRecord>, ApplicationError> {
        self.patch_store
            .as_ref()
            .ok_or_else(|| ApplicationError::new("patch-store-missing", "Patch Store 尚未注入"))?
            .list()
            .map_err(tool_error)
    }

    pub fn mcp_servers(&self) -> Result<Vec<McpServerView>, ApplicationError> {
        self.mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .list_servers()
            .map_err(mcp_error)
    }

    pub fn mcp_add_server(
        &self,
        config: McpServerConfig,
    ) -> Result<McpServerView, ApplicationError> {
        let manager = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?;
        let server = manager.add_server(config).map_err(mcp_error)?;
        if server.enabled
            && let Some(runtime) = self.tool_runtime.as_ref()
            && let Err(error) = runtime.allow_mcp_server(&server.id).map_err(tool_error)
        {
            let _ = manager.remove_server(&server.id);
            return Err(error);
        }
        self.publish(
            HarnessEvent::McpStatus {
                server_id: server.id.clone(),
                status: "added".to_owned(),
                detail: format!("{} · runtime-only", server.transport),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(server)
    }

    pub fn mcp_remove_server(&self, server_id: &str) -> Result<bool, ApplicationError> {
        let manager = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?;
        manager.disconnect(server_id).map_err(mcp_error)?;
        let removed = manager.remove_server(server_id).map_err(mcp_error)?;
        if let Some(runtime) = self.tool_runtime.as_ref() {
            runtime.remove_mcp_server(server_id).map_err(tool_error)?;
        }
        self.publish(
            HarnessEvent::McpStatus {
                server_id: server_id.to_owned(),
                status: "removed".to_owned(),
                detail: "runtime registry".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(removed)
    }

    pub fn mcp_enable_server(&self, server_id: &str) -> Result<McpServerView, ApplicationError> {
        let manager = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?;
        let server = manager.enable_server(server_id).map_err(mcp_error)?;
        if let Some(runtime) = self.tool_runtime.as_ref()
            && let Err(error) = runtime.allow_mcp_server(server_id).map_err(tool_error)
        {
            let _ = manager.disable_server(server_id);
            return Err(error);
        }
        self.publish(
            HarnessEvent::McpStatus {
                server_id: server.id.clone(),
                status: "enabled".to_owned(),
                detail: "permission allowlist synchronized".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(server)
    }

    pub fn mcp_disable_server(&self, server_id: &str) -> Result<McpServerView, ApplicationError> {
        let manager = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?;
        let server = manager.disable_server(server_id).map_err(mcp_error)?;
        if let Some(runtime) = self.tool_runtime.as_ref() {
            runtime.remove_mcp_server(server_id).map_err(tool_error)?;
        }
        self.publish(
            HarnessEvent::McpStatus {
                server_id: server.id.clone(),
                status: "disabled".to_owned(),
                detail: "disconnected and permission allowlist removed".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(server)
    }

    pub fn mcp_connect(
        &self,
        server_id: &str,
        force: bool,
    ) -> Result<McpServerView, ApplicationError> {
        let server = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .connect(server_id, force)
            .map_err(mcp_error)?;
        self.publish(
            HarnessEvent::McpStatus {
                server_id: server.id.clone(),
                status: format!("{:?}", server.status).to_lowercase(),
                detail: format!(
                    "tools={}/{}, resources={}, prompts={}",
                    server.supported_tool_count,
                    server.tool_count,
                    server.resource_count,
                    server.prompt_count
                ),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(server)
    }

    pub fn mcp_disconnect(&self, server_id: &str) -> Result<McpServerView, ApplicationError> {
        let server = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .disconnect(server_id)
            .map_err(mcp_error)?;
        self.publish(
            HarnessEvent::McpStatus {
                server_id: server.id.clone(),
                status: "disconnected".to_owned(),
                detail: "catalog contributions removed".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(server)
    }

    pub fn mcp_oauth_start(&self, server_id: &str) -> Result<McpOAuthStart, ApplicationError> {
        let started = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .oauth_start(server_id)
            .map_err(mcp_error)?;
        self.publish(
            HarnessEvent::McpStatus {
                server_id: server_id.to_owned(),
                status: "oauth-pending".to_owned(),
                detail: format!("callback={}", started.redirect_uri),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(started)
    }

    pub fn mcp_oauth_finish(&self, server_id: &str) -> Result<McpOAuthStatus, ApplicationError> {
        let status = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .oauth_finish(server_id)
            .map_err(mcp_error)?;
        self.publish_mcp_oauth_status(&status)?;
        Ok(status)
    }

    pub fn mcp_oauth_refresh(&self, server_id: &str) -> Result<McpOAuthStatus, ApplicationError> {
        let status = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .oauth_refresh(server_id)
            .map_err(mcp_error)?;
        self.publish_mcp_oauth_status(&status)?;
        Ok(status)
    }

    pub fn mcp_oauth_status(&self, server_id: &str) -> Result<McpOAuthStatus, ApplicationError> {
        self.mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .oauth_status(server_id)
            .map_err(mcp_error)
    }

    fn publish_mcp_oauth_status(&self, status: &McpOAuthStatus) -> Result<(), ApplicationError> {
        self.publish(
            HarnessEvent::McpStatus {
                server_id: status.server_id.clone(),
                status: if status.authenticated {
                    "authenticated"
                } else if status.pending {
                    "oauth-pending"
                } else {
                    "unauthenticated"
                }
                .to_owned(),
                detail: status.credential_id.as_ref().map_or_else(
                    || "no credential".to_owned(),
                    |id| format!("credential={id}"),
                ),
            },
            self.session_scope(),
            EventPriority::Normal,
        )
    }

    pub fn mcp_tools(&self, server_id: &str) -> Result<Vec<McpToolDescriptor>, ApplicationError> {
        self.mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .list_tools(server_id)
            .map_err(mcp_error)
    }

    pub fn mcp_resources(
        &self,
        server_id: &str,
    ) -> Result<Vec<McpResourceDescriptor>, ApplicationError> {
        self.mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .list_resources(server_id)
            .map_err(mcp_error)
    }

    pub fn mcp_prompts(
        &self,
        server_id: &str,
    ) -> Result<Vec<McpPromptDescriptor>, ApplicationError> {
        self.mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .list_prompts(server_id)
            .map_err(mcp_error)
    }

    pub fn mcp_read_resource(
        &self,
        server_id: &str,
        uri: &str,
    ) -> Result<Vec<serde_json::Value>, ApplicationError> {
        self.mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .read_resource(server_id, uri)
            .map_err(mcp_error)
    }

    pub fn mcp_poll(&self, server_id: &str) -> Result<Vec<serde_json::Value>, ApplicationError> {
        self.mcp_manager
            .as_ref()
            .ok_or_else(|| ApplicationError::new("mcp-runtime-missing", "MCP Runtime 尚未注入"))?
            .poll_notifications(server_id)
            .map_err(mcp_error)
    }

    pub fn plugins(&self) -> Result<Vec<PluginView>, ApplicationError> {
        self.plugin_manager
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("plugin-runtime-missing", "Plugin Runtime 尚未注入")
            })?
            .list()
            .map_err(plugin_error)
    }

    pub fn plugin_review(
        &self,
        plugin_id: &str,
    ) -> Result<PluginPermissionReview, ApplicationError> {
        self.plugin_manager
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("plugin-runtime-missing", "Plugin Runtime 尚未注入")
            })?
            .review(plugin_id)
            .map_err(plugin_error)
    }

    pub fn enable_plugin(
        &mut self,
        plugin_id: &str,
        approved_review_hash: &str,
    ) -> Result<PluginView, ApplicationError> {
        let manager = self.plugin_manager.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new("plugin-runtime-missing", "Plugin Runtime 尚未注入")
        })?;
        let view = manager
            .enable(
                plugin_id,
                &self.project_id.to_string(),
                approved_review_hash,
                serde_json::json!({}),
            )
            .map_err(plugin_error)?;
        if view.status != harness_plugin::PluginLifecycleStatus::Active {
            self.publish(
                HarnessEvent::PluginStatus {
                    plugin_id: view.id.clone(),
                    status: format!("{:?}", view.status).to_lowercase(),
                    detail: view
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "activation did not become active".to_owned()),
                },
                self.session_scope(),
                EventPriority::Normal,
            )?;
            return Ok(view);
        }
        if self.plugin_extensions.contains_key(plugin_id) {
            return Ok(view);
        }
        let (skill_paths, mcp_paths) = manager
            .contribution_paths(plugin_id)
            .map_err(plugin_error)?;
        let mut registration = PluginExtensionRegistration::default();
        let activation = (|| {
            if let Some(skills) = &self.skill_registry {
                for path in skill_paths {
                    let installed = skills
                        .install(
                            &path,
                            SkillSource::Plugin {
                                plugin_id: plugin_id.to_owned(),
                            },
                        )
                        .map_err(skill_error)?;
                    registration.skill_ids.push(installed.id);
                }
            } else if !skill_paths.is_empty() {
                return Err(ApplicationError::new(
                    "skill-runtime-missing",
                    "Plugin 声明了 Skill，但 Skill Registry 未注入",
                ));
            }
            if let Some(mcp) = &self.mcp_manager {
                for path in mcp_paths {
                    let config = harness_mcp::load_config_file(&path).map_err(mcp_error)?;
                    for server in config.servers {
                        let added = mcp.add_server(server).map_err(mcp_error)?;
                        registration.mcp_server_ids.push(added.id);
                    }
                }
            } else if !mcp_paths.is_empty() {
                return Err(ApplicationError::new(
                    "mcp-runtime-missing",
                    "Plugin 声明了 MCP Server，但 MCP Runtime 未注入",
                ));
            }
            Ok(())
        })();
        if let Err(error) = activation {
            self.rollback_plugin_extensions(plugin_id, &registration);
            let _ = manager.disable(plugin_id);
            return Err(error);
        }
        self.plugin_extensions
            .insert(plugin_id.to_owned(), registration);
        self.publish(
            HarnessEvent::PluginStatus {
                plugin_id: view.id.clone(),
                status: format!("{:?}", view.status).to_lowercase(),
                detail: format!("contributions={}", view.contribution_count),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(view)
    }

    pub fn disable_plugin(&mut self, plugin_id: &str) -> Result<PluginView, ApplicationError> {
        if let Some(registration) = self.plugin_extensions.remove(plugin_id) {
            self.rollback_plugin_extensions(plugin_id, &registration);
        }
        let plugin = self
            .plugin_manager
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("plugin-runtime-missing", "Plugin Runtime 尚未注入")
            })?
            .disable(plugin_id)
            .map_err(plugin_error)?;
        self.publish(
            HarnessEvent::PluginStatus {
                plugin_id: plugin.id.clone(),
                status: format!("{:?}", plugin.status).to_lowercase(),
                detail: "contributions removed".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(plugin)
    }

    pub fn skills(&self) -> Result<Vec<SkillView>, ApplicationError> {
        self.skill_registry
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("skill-runtime-missing", "Skill Registry 尚未注入")
            })?
            .list()
            .map_err(skill_error)
    }

    pub fn search_skills(&self, query: &str) -> Result<Vec<SkillView>, ApplicationError> {
        self.skill_registry
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("skill-runtime-missing", "Skill Registry 尚未注入")
            })?
            .search(query, 20)
            .map_err(skill_error)
    }

    pub fn load_skill(&self, skill_id: &str) -> Result<LoadedSkill, ApplicationError> {
        let skill = self
            .skill_registry
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("skill-runtime-missing", "Skill Registry 尚未注入")
            })?
            .load(skill_id, None)
            .map_err(skill_error)?;
        self.publish(
            HarnessEvent::SkillStatus {
                skill_id: skill.view.id.clone(),
                status: "loaded".to_owned(),
                detail: format!(
                    "promptBytes={}, references={}",
                    skill.prompt.len(),
                    skill.references.len()
                ),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(skill)
    }

    pub fn unload_skill(&self, skill_id: &str) -> Result<SkillView, ApplicationError> {
        let skill = self
            .skill_registry
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("skill-runtime-missing", "Skill Registry 尚未注入")
            })?
            .unload(skill_id)
            .map_err(skill_error)?;
        self.publish(
            HarnessEvent::SkillStatus {
                skill_id: skill.id.clone(),
                status: "metadata-only".to_owned(),
                detail: "prompt/reference cache released".to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )?;
        Ok(skill)
    }

    fn rollback_plugin_extensions(
        &self,
        plugin_id: &str,
        registration: &PluginExtensionRegistration,
    ) {
        if let Some(skills) = &self.skill_registry {
            let source = SkillSource::Plugin {
                plugin_id: plugin_id.to_owned(),
            };
            for skill_id in registration.skill_ids.iter().rev() {
                let _ = skills.uninstall(skill_id, &source);
            }
        }
        if let Some(mcp) = &self.mcp_manager {
            for server_id in registration.mcp_server_ids.iter().rev() {
                let _ = mcp.disconnect(server_id);
                let _ = mcp.remove_server(server_id);
            }
        }
    }

    pub fn memory_view(&self) -> Result<ProjectMemoryView, ApplicationError> {
        self.memory
            .as_ref()
            .ok_or_else(|| ApplicationError::new("memory-runtime-missing", "Memory 尚未注入"))?
            .view()
            .map_err(memory_error)
    }

    pub fn search_memory(
        &mut self,
        query: &str,
        mode: RetrievalMode,
        limit: usize,
    ) -> Result<MemorySearchResponse, ApplicationError> {
        let started = Instant::now();
        let response = self
            .memory
            .as_mut()
            .ok_or_else(|| ApplicationError::new("memory-runtime-missing", "Memory 尚未注入"))?
            .search(query, mode, limit)
            .map_err(memory_error)?;
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.record_profile_sample("retrieval", elapsed);
        if matches!(
            response.executed_mode,
            harness_memory::ExecutedRetrievalMode::Semantic
                | harness_memory::ExecutedRetrievalMode::Hybrid
        ) {
            self.record_profile_sample("vector", elapsed);
        }
        Ok(response)
    }

    pub fn add_memory(
        &mut self,
        kind: MemoryKind,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<MemoryRecord, ApplicationError> {
        self.memory
            .as_mut()
            .ok_or_else(|| ApplicationError::new("memory-runtime-missing", "Memory 尚未注入"))?
            .add(
                NewMemoryRecord {
                    id: self.ids.next_id("memory"),
                    kind,
                    title,
                    content,
                    tags,
                    source_ref: Some(self.session_id.to_string()),
                    status: MemoryStatus::Observed,
                },
                self.clock.now_unix_millis(),
            )
            .map_err(memory_error)
    }

    pub fn forget_memory(&mut self, id: &str) -> Result<bool, ApplicationError> {
        self.memory
            .as_mut()
            .ok_or_else(|| ApplicationError::new("memory-runtime-missing", "Memory 尚未注入"))?
            .forget(id)
            .map_err(memory_error)
    }

    pub fn purge_vectors(&mut self) -> Result<ProjectMemoryView, ApplicationError> {
        let memory = self
            .memory
            .as_mut()
            .ok_or_else(|| ApplicationError::new("memory-runtime-missing", "Memory 尚未注入"))?;
        memory.purge_vectors().map_err(memory_error)?;
        memory.view().map_err(memory_error)
    }

    pub fn repository_view(&self) -> Result<RepositoryIndexView, ApplicationError> {
        self.repository
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("repository-runtime-missing", "Repository Index 尚未注入")
            })?
            .view()
            .map_err(memory_error)
    }

    pub fn update_repository(&mut self) -> Result<RepositoryUpdateStats, ApplicationError> {
        self.repository
            .as_mut()
            .ok_or_else(|| {
                ApplicationError::new("repository-runtime-missing", "Repository Index 尚未注入")
            })?
            .update(self.clock.now_unix_millis())
            .map_err(memory_error)
    }

    pub fn search_repository(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RepositorySearchResult>, ApplicationError> {
        self.repository
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("repository-runtime-missing", "Repository Index 尚未注入")
            })?
            .search(query, limit)
            .map_err(memory_error)
    }

    pub fn repository_map(&self) -> Result<String, ApplicationError> {
        self.repository
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("repository-runtime-missing", "Repository Index 尚未注入")
            })?
            .repository_map()
            .map_err(memory_error)
    }

    pub fn clear_repository(&mut self) -> Result<RepositoryIndexView, ApplicationError> {
        let repository = self.repository.as_mut().ok_or_else(|| {
            ApplicationError::new("repository-runtime-missing", "Repository Index 尚未注入")
        })?;
        repository.clear().map_err(memory_error)?;
        repository.view().map_err(memory_error)
    }

    pub fn lsp_servers(&self) -> Result<Vec<LspServerView>, ApplicationError> {
        self.lsp
            .as_ref()
            .ok_or_else(|| ApplicationError::new("lsp-runtime-missing", "LSP 尚未配置"))?
            .list()
            .map_err(lsp_error)
    }

    pub fn lsp_start(&self, server_id: &str) -> Result<LspServerView, ApplicationError> {
        self.lsp
            .as_ref()
            .ok_or_else(|| ApplicationError::new("lsp-runtime-missing", "LSP 尚未配置"))?
            .start(server_id)
            .map_err(lsp_error)
    }

    pub fn lsp_stop(&self, server_id: &str) -> Result<bool, ApplicationError> {
        self.lsp
            .as_ref()
            .ok_or_else(|| ApplicationError::new("lsp-runtime-missing", "LSP 尚未配置"))?
            .stop(server_id)
            .map_err(lsp_error)
    }

    pub fn lsp_symbols(
        &self,
        server_id: &str,
        path: &Path,
    ) -> Result<Vec<LspSymbol>, ApplicationError> {
        self.lsp
            .as_ref()
            .ok_or_else(|| ApplicationError::new("lsp-runtime-missing", "LSP 尚未配置"))?
            .document_symbols(server_id, path)
            .map_err(lsp_error)
    }

    pub fn lsp_definition(
        &self,
        server_id: &str,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, ApplicationError> {
        self.lsp
            .as_ref()
            .ok_or_else(|| ApplicationError::new("lsp-runtime-missing", "LSP 尚未配置"))?
            .definition(server_id, path, line, character)
            .map_err(lsp_error)
    }

    pub fn lsp_references(
        &self,
        server_id: &str,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, ApplicationError> {
        self.lsp
            .as_ref()
            .ok_or_else(|| ApplicationError::new("lsp-runtime-missing", "LSP 尚未配置"))?
            .references(server_id, path, line, character)
            .map_err(lsp_error)
    }

    pub fn lsp_diagnostics(
        &self,
        server_id: &str,
        path: &Path,
    ) -> Result<Vec<LspDiagnostic>, ApplicationError> {
        self.lsp
            .as_ref()
            .ok_or_else(|| ApplicationError::new("lsp-runtime-missing", "LSP 尚未配置"))?
            .diagnostics(server_id, path)
            .map_err(lsp_error)
    }

    pub fn plan(&self) -> Result<PlanView, ApplicationError> {
        let Some(mission_id) = &self.active_mission_id else {
            return Ok(PlanView {
                mission_id: None,
                status: None,
                accepted: 0,
                running: 0,
                pending: 0,
                blocked: 0,
            });
        };
        let state = self.recover_mission(mission_id)?;
        Ok(plan_view(&state))
    }

    pub fn agents(&self) -> Result<AgentTeamView, ApplicationError> {
        let catalog = self.agent_catalog.as_ref().ok_or_else(|| {
            ApplicationError::new("agent-catalog-missing", "Agent Catalog 尚未注入")
        })?;
        let agents = catalog
            .list()
            .into_iter()
            .map(|definition| {
                let lifecycle = catalog
                    .lifecycle(&definition.id)
                    .unwrap_or(AgentLifecycle::Failed);
                AgentView {
                    id: definition.id.clone(),
                    name: definition.name,
                    roles: definition.roles.iter().copied().collect(),
                    capabilities: definition.capabilities.into_iter().collect(),
                    lifecycle,
                    active: catalog.active_count(&definition.id),
                    max_concurrency: definition.max_concurrency,
                    control_plane: definition.roles.contains(&AgentRole::Coordinator)
                        || definition.roles.contains(&AgentRole::StaffingRouter),
                }
            })
            .collect::<Vec<_>>();
        let active_run_controls = self
            .run_controls
            .lock()
            .map_err(|_| ApplicationError::new("run-controls-poisoned", "Run 控制树锁损坏"))?
            .active_run_ids()
            .len();
        let recoverable_sessions = self
            .agent_state
            .as_ref()
            .map_or(Ok(0), |state| {
                state.recoverable_sessions().map(|sessions| sessions.len())
            })
            .map_err(agent_error)?;
        Ok(AgentTeamView {
            total: agents.len(),
            sleeping: agents
                .iter()
                .filter(|agent| agent.lifecycle == AgentLifecycle::Sleeping)
                .count(),
            reserved: agents
                .iter()
                .filter(|agent| agent.lifecycle == AgentLifecycle::Reserved)
                .count(),
            running: agents
                .iter()
                .filter(|agent| agent.lifecycle == AgentLifecycle::Running)
                .count(),
            durable_messages: self.agent_messages.is_some(),
            file_leases: self.file_leases.is_some(),
            active_run_controls,
            recoverable_sessions,
            agents,
        })
    }

    pub fn agent(&self, id: &AgentDefinitionId) -> Result<AgentView, ApplicationError> {
        self.agents()?
            .agents
            .into_iter()
            .find(|agent| &agent.id == id)
            .ok_or_else(|| ApplicationError::new("agent-not-found", id.to_string()))
    }

    pub fn agent_queue(&self) -> Result<AgentQueueView, ApplicationError> {
        let Some(mission_id) = &self.active_mission_id else {
            return Ok(AgentQueueView {
                mission_id: None,
                items: Vec::new(),
            });
        };
        let state = self.recover_mission(mission_id)?;
        let ready = find_ready_node_ids(&state)
            .into_iter()
            .collect::<BTreeSet<_>>();
        Ok(AgentQueueView {
            mission_id: Some(mission_id.clone()),
            items: state
                .nodes
                .values()
                .map(|node| {
                    let priority = self
                        .agent_state
                        .as_ref()
                        .map_or(Ok(0), |store| store.task_priority(mission_id, &node.id))?;
                    Ok(AgentQueueItemView {
                        task_id: node.id.clone(),
                        title: node.title.clone(),
                        agent_definition_id: node.agent_definition_id.clone(),
                        status: node.status,
                        ready: ready.contains(&node.id),
                        priority,
                    })
                })
                .collect::<Result<Vec<_>, harness_agent::AgentError>>()
                .map_err(agent_error)?,
        })
    }

    pub fn set_queue_priority(
        &self,
        task_id: &TaskId,
        priority: i32,
    ) -> Result<AgentQueueView, ApplicationError> {
        let mission_id = self.active_mission_id.as_ref().ok_or_else(|| {
            ApplicationError::new("active-mission-missing", "当前没有活动 Mission")
        })?;
        let mission = self.recover_mission(mission_id)?;
        if !mission.nodes.contains_key(task_id) {
            return Err(ApplicationError::new(
                "agent-node-missing",
                task_id.to_string(),
            ));
        }
        self.agent_state
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("agent-state-missing", "Agent State Store 尚未注入")
            })?
            .set_task_priority(
                mission_id,
                task_id,
                priority.clamp(-100, 100),
                self.clock.now_unix_millis(),
            )
            .map_err(agent_error)?;
        self.agent_queue()
    }

    pub fn cancel_queue_task(
        &mut self,
        task_id: &TaskId,
        reason: &str,
    ) -> Result<AgentQueueView, ApplicationError> {
        let mission_id = self.active_mission_id.clone().ok_or_else(|| {
            ApplicationError::new("active-mission-missing", "当前没有活动 Mission")
        })?;
        self.apply_mission_command(
            &mission_id,
            MissionCommand::CancelNode {
                node_id: task_id.clone(),
                reason: reason.to_owned(),
            },
        )?;
        self.drive_effects(&mission_id, "")?;
        self.agent_queue()
    }

    pub fn cancel_active_mission(&mut self, reason: &str) -> Result<PlanView, ApplicationError> {
        let mission_id = self.active_mission_id.clone().ok_or_else(|| {
            ApplicationError::new("active-mission-missing", "当前没有活动 Mission")
        })?;
        self.apply_mission_command(
            &mission_id,
            MissionCommand::CancelMission {
                reason: reason.to_owned(),
            },
        )?;
        self.drive_effects(&mission_id, "")?;
        self.plan_for(&mission_id)
    }

    pub fn steer(&mut self, instruction: &str) -> Result<SteeringView, ApplicationError> {
        if instruction.trim().is_empty() {
            return Err(ApplicationError::new(
                "steering-empty",
                "Steering instruction 不能为空",
            ));
        }
        let mission_id = self.active_mission_id.clone().ok_or_else(|| {
            ApplicationError::new("active-mission-missing", "当前没有活动 Mission")
        })?;
        let message_id = self.ids.next_id("agent-message");
        let recipient = "kernel:supervisor:steering".to_owned();
        let message = self
            .agent_messages
            .as_mut()
            .ok_or_else(|| {
                ApplicationError::new("agent-message-bus-missing", "Agent MessageBus 尚未注入")
            })?
            .send(AgentMessage {
                id: message_id.clone(),
                idempotency_key: format!("steering:{mission_id}:{message_id}"),
                mission_id: mission_id.clone(),
                from: "user:terminal".to_owned(),
                to: recipient.clone(),
                kind: AgentMessageKind::Steering,
                payload: serde_json::json!({"instruction":instruction.trim()}),
                sequence: 0,
                created_at_millis: self.clock.now_unix_millis(),
                acknowledged_at_millis: None,
            })
            .map_err(agent_error)?;
        Ok(SteeringView {
            message_id: message.id,
            mission_id,
            recipient,
            queued: true,
        })
    }

    #[must_use]
    pub fn agent_budget(&self) -> AgentBudgetView {
        AgentBudgetView {
            scope: "runtime".to_owned(),
            max_agents: self.agent_budget_policy.max_agents,
            max_parallel_agents: self.agent_budget_policy.max_parallel_agents,
            max_total_tokens: self.agent_budget_policy.max_total_tokens,
            max_tool_calls: self.agent_budget_policy.max_tool_calls,
            max_runtime_millis: self.agent_budget_policy.max_runtime_millis,
            max_retries: self.agent_budget_policy.max_retries,
            max_cost_units: self.mode_profile.max_cost_units,
        }
    }

    pub fn set_agent_budget(
        &mut self,
        field: &str,
        value: u64,
    ) -> Result<AgentBudgetView, ApplicationError> {
        let key = match field {
            "agents" => "agents.max",
            "parallel" => "agents.parallel",
            "tokens" => "agents.tokens",
            "tools" => "agents.tools",
            "runtime-ms" => "agents.runtime-ms",
            "retries" => "agents.retries",
            "cost" => "agents.cost",
            _ => {
                return Err(ApplicationError::new(
                    "budget-field-invalid",
                    "支持 agents/parallel/tokens/tools/runtime-ms/retries/cost",
                ));
            }
        };
        self.set_setting(key, &value.to_string(), ConfigLayer::Runtime)?;
        Ok(self.agent_budget())
    }

    pub fn coordinate_results(
        &mut self,
        mission_id: &MissionId,
        results: &[AgentResultEnvelope],
    ) -> Result<CoordinationView, ApplicationError> {
        let meeting_id = self.ids.next_id("meeting");
        let merge_dependencies = results
            .iter()
            .map(|result| result.task_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let outcome = Coordinator::inspect(
            mission_id.clone(),
            results,
            meeting_id.clone(),
            self.clock.now_unix_millis(),
        );
        let Some(meeting) = outcome.meeting else {
            return Ok(CoordinationView {
                conflict_count: 0,
                meeting_id: None,
                memory_id: None,
                merge_required: false,
                merge_task_id: None,
            });
        };
        self.agent_messages
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("agent-message-bus-missing", "Agent MessageBus 尚未注入")
            })?
            .save_meeting(&meeting, self.clock.now_unix_millis())
            .map_err(agent_error)?;
        let memory_id = if let Some(memory) = self.memory.as_mut() {
            let id = self.ids.next_id("memory");
            memory
                .add(
                    NewMemoryRecord {
                        id: id.clone(),
                        kind: MemoryKind::Meeting,
                        title: meeting.topic.clone(),
                        content: serde_json::to_string(&meeting).map_err(|error| {
                            ApplicationError::new("meeting-json", error.to_string())
                        })?,
                        tags: vec!["agent-meeting".to_owned(), "conflict".to_owned()],
                        source_ref: Some(meeting.id.clone()),
                        status: MemoryStatus::Observed,
                    },
                    self.clock.now_unix_millis(),
                )
                .map_err(memory_error)?;
            Some(id)
        } else {
            None
        };
        let merge_task_id = if self.active_mission_id.as_ref() == Some(mission_id) {
            let state = self.recover_mission(mission_id)?;
            if merge_dependencies
                .iter()
                .all(|task_id| state.nodes.contains_key(task_id))
            {
                let task_id = TaskId::from(self.ids.next_id("task:merge"));
                self.apply_mission_command(
                    mission_id,
                    MissionCommand::AppendPlanNodes {
                        nodes: vec![WorkflowNodeDefinition {
                            id: task_id.clone(),
                            title: format!("解决会议 {} 中的方案/补丁冲突", meeting.id),
                            kind: NodeKind::Merge,
                            depends_on: merge_dependencies,
                            agent_definition_id: AgentDefinitionId::from("agent:merge"),
                            requires_approval: None,
                        }],
                    },
                )?;
                Some(task_id)
            } else {
                None
            }
        } else {
            None
        };
        Ok(CoordinationView {
            conflict_count: outcome.conflicts.len(),
            meeting_id: Some(meeting.id),
            memory_id,
            merge_required: true,
            merge_task_id,
        })
    }

    /// 为一个子 Agent 编译最小工作上下文；不会返回完整 Context Series。
    pub fn agent_working_context(
        &self,
        agent_role: AgentRole,
    ) -> Result<AgentWorkingContext, ApplicationError> {
        let _profile_span = self.profile_span("context-build");
        let series = self.active_context_series()?;
        let compiled = ContextBroker
            .compile_for_role(
                context_role(agent_role),
                series.items.into_iter().map(|item| item.context).collect(),
                &self.context_budget(),
                self.clock.now_unix_millis(),
            )
            .map_err(|error| ApplicationError::new(error.code, error.to_string()))?;
        let selected_item_ids = compiled
            .selected
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        let excluded_item_ids = compiled
            .exclusions
            .iter()
            .map(|item| item.item_id.clone())
            .collect::<Vec<_>>();
        let segments = compiled
            .selected
            .into_iter()
            .map(context_prompt_segment)
            .collect::<Vec<_>>();
        let stable_segments = segments
            .iter()
            .filter(|segment| segment.cacheability == PromptCacheability::Static)
            .cloned()
            .collect();
        let dynamic_segments = segments
            .iter()
            .filter(|segment| segment.cacheability != PromptCacheability::Static)
            .cloned()
            .collect();
        let canonicalizer = PromptCanonicalizer;
        let stable = canonicalizer
            .compile(stable_segments, vec![])
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        let dynamic = canonicalizer
            .compile(dynamic_segments, vec![])
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        let full = canonicalizer
            .compile(segments, vec![])
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        Ok(AgentWorkingContext {
            stable_instructions: stable.text,
            dynamic_context: dynamic.text,
            selected_item_ids,
            excluded_item_ids,
            token_cost: compiled.token_cost,
            max_input_tokens: compiled.max_input_tokens,
            fingerprint: full.full_hash.to_string(),
        })
    }

    /// 编译当前 Context 并返回预算/筛选统计。
    pub fn context(&self) -> Result<ContextView, ApplicationError> {
        let _profile_span = self.profile_span("context-build");
        let series = self.active_context_series()?;
        let budget = self.context_budget();
        let compiled = ContextBroker
            .compile_for_role(
                Role::Supervisor,
                series
                    .items
                    .iter()
                    .map(|item| item.context.clone())
                    .collect(),
                &budget,
                self.clock.now_unix_millis(),
            )
            .map_err(|error| ApplicationError::new(error.code, error.to_string()))?;
        let max_tokens = compiled.max_input_tokens;
        let percent = compiled
            .token_cost
            .saturating_mul(100)
            .checked_div(max_tokens)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(100);
        let checkpoint_count = self
            .store
            .list_context_checkpoints(&self.session_id)
            .map_err(context_storage_error)?
            .len();
        Ok(ContextView {
            series_id: series.id,
            item_count: series.items.len(),
            selected_count: compiled.selected.len(),
            excluded_count: compiled.exclusions.len(),
            used_tokens: compiled.token_cost,
            max_tokens,
            percent,
            checkpoint_count,
        })
    }

    /// 创建 durable Context checkpoint，不改动活动 Series。
    pub fn create_checkpoint(
        &self,
        name: Option<&str>,
    ) -> Result<CheckpointView, ApplicationError> {
        let checkpoint = self.create_checkpoint_record(name)?;
        Ok(CheckpointView {
            checkpoint_id: checkpoint.id,
            context_series_id: checkpoint.context_series_id,
            created_at_millis: checkpoint.created_at_millis,
        })
    }

    /// Safe/Aggressive 压缩都先创建恢复锚点，再 CAS 切换活动 Series。
    pub fn compact(&mut self, mode: CompactionMode) -> Result<CompactionView, ApplicationError> {
        let current = self.active_context_series()?;
        let checkpoint = self.create_checkpoint_record(Some(match mode {
            CompactionMode::Safe => "auto-before-safe-compact",
            CompactionMode::Aggressive => "auto-before-aggressive-compact",
        }))?;
        let result = ContextCompactor::new(DeterministicSummaryProvider)
            .compact(
                mode,
                current.items.clone(),
                2,
                512,
                current.id.clone(),
                Some(checkpoint.clone()),
                self.clock.now_unix_millis(),
            )
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        let record = result.record.clone();
        let next = ContextSeries {
            id: record.next_series_id.clone(),
            session_id: self.session_id.clone(),
            parent_series_id: Some(current.id.clone()),
            restored_from_checkpoint_id: None,
            items: result.visible_items,
            created_at_millis: self.clock.now_unix_millis(),
        };
        self.store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: Some(current.id.clone()),
                next_series: next,
                compaction_record: Some(record.clone()),
            })
            .map_err(context_storage_error)?;
        self.cache_current_prompt()?;
        self.publish_context_changed()?;
        Ok(CompactionView {
            mode,
            previous_series_id: record.previous_series_id,
            next_series_id: record.next_series_id,
            checkpoint_id: checkpoint.id,
            token_cost_before: record.token_cost_before,
            token_cost_after: record.token_cost_after,
        })
    }

    /// 默认 Auto 策略在 80% 输入预算处触发 Safe compaction。
    pub fn auto_compact_if_needed(&mut self) -> Result<Option<CompactionView>, ApplicationError> {
        let context = self.context()?;
        if context.percent < 80 {
            return Ok(None);
        }
        self.compact(CompactionMode::Safe).map(Some)
    }

    /// Checkpoint 命令既接受稳定 ID，也接受唯一的人类可读名称。
    pub fn resolve_checkpoint(
        &self,
        reference: &str,
    ) -> Result<ContextCheckpoint, ApplicationError> {
        let checkpoints = self
            .store
            .list_context_checkpoints(&self.session_id)
            .map_err(context_storage_error)?;
        if let Some(checkpoint) = checkpoints
            .iter()
            .find(|checkpoint| checkpoint.id.as_str() == reference)
        {
            return Ok(checkpoint.clone());
        }
        let named = checkpoints
            .into_iter()
            .filter(|checkpoint| checkpoint.name.as_deref() == Some(reference))
            .collect::<Vec<_>>();
        match named.as_slice() {
            [checkpoint] => Ok(checkpoint.clone()),
            [] => Err(ApplicationError::new(
                "checkpoint-not-found",
                format!("未找到 Checkpoint：{reference}"),
            )),
            _ => Err(ApplicationError::new(
                "checkpoint-name-ambiguous",
                format!("Checkpoint 名称不唯一，请改用 ID：{reference}"),
            )),
        }
    }

    pub fn rollback_reference(&mut self, reference: &str) -> Result<ContextView, ApplicationError> {
        let checkpoint = self.resolve_checkpoint(reference)?;
        self.rollback(&checkpoint.id)
    }

    /// Rollback 创建新 Series 指向 checkpoint 快照；旧历史保持不可变。
    pub fn rollback(
        &mut self,
        checkpoint_id: &CheckpointId,
    ) -> Result<ContextView, ApplicationError> {
        let checkpoint = self
            .store
            .load_context_checkpoint(&self.session_id, checkpoint_id)
            .map_err(context_storage_error)?
            .ok_or_else(|| {
                ApplicationError::new(
                    "checkpoint-not-found",
                    format!("未找到 Checkpoint：{checkpoint_id}"),
                )
            })?;
        let source = self
            .store
            .load_context_series(&checkpoint.context_series_id)
            .map_err(context_storage_error)?
            .ok_or_else(|| {
                ApplicationError::new(
                    "checkpoint-series-not-found",
                    checkpoint.context_series_id.to_string(),
                )
            })?;
        let current = self.active_context_series()?;
        let next = ContextSeries {
            id: ContextSeriesId::from(self.ids.next_id("series")),
            session_id: self.session_id.clone(),
            parent_series_id: Some(current.id.clone()),
            restored_from_checkpoint_id: Some(checkpoint.id),
            items: source.items,
            created_at_millis: self.clock.now_unix_millis(),
        };
        self.store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: Some(current.id),
                next_series: next,
                compaction_record: None,
            })
            .map_err(context_storage_error)?;
        self.cache_current_prompt()?;
        self.publish_context_changed()?;
        self.context()
    }

    /// 从 Parent checkpoint 创建独立 Child Session；当前 Application 仍停留在 Parent。
    pub fn fork_session(
        &self,
        checkpoint_id: &CheckpointId,
        requested_child_session_id: Option<SessionId>,
    ) -> Result<ForkView, ApplicationError> {
        let parent_before = self.recover_session()?;
        let checkpoint = self
            .store
            .load_context_checkpoint(&self.session_id, checkpoint_id)
            .map_err(context_storage_error)?
            .ok_or_else(|| {
                ApplicationError::new(
                    "checkpoint-not-found",
                    format!("未找到 Checkpoint：{checkpoint_id}"),
                )
            })?;
        let source = self
            .store
            .load_context_series(&checkpoint.context_series_id)
            .map_err(context_storage_error)?
            .ok_or_else(|| {
                ApplicationError::new(
                    "checkpoint-series-not-found",
                    checkpoint.context_series_id.to_string(),
                )
            })?;
        let child_session_id = requested_child_session_id
            .unwrap_or_else(|| SessionId::from(self.ids.next_id("session")));
        if !self
            .store
            .load_session_events(&child_session_id, 0)
            .map_err(storage_error)?
            .is_empty()
            || self
                .store
                .load_session_snapshot(&child_session_id)
                .map_err(storage_error)?
                .is_some()
            || self
                .store
                .load_active_context_series(&child_session_id)
                .map_err(context_storage_error)?
                .is_some()
        {
            return Err(ApplicationError::new(
                "session-exists",
                format!("Child Session 已存在：{child_session_id}"),
            ));
        }

        let mut child = SessionState::empty(child_session_id.clone());
        let mut events = decide_session(
            &child,
            &SessionCommand::ForkSession {
                child_session_id: child_session_id.clone(),
                project_id: self.project_id.clone(),
                parent_session_id: self.session_id.clone(),
                checkpoint_id: checkpoint.id.clone(),
            },
        )
        .map_err(|error| ApplicationError::new(error.code, error.message))?;
        for event in &events {
            child = reduce_session(&child, event)
                .map_err(|error| ApplicationError::new(error.code, error.message))?;
        }
        if let (Some(provider_id), Some(model_id)) = (
            parent_before.model.provider_id.clone(),
            parent_before.model.model_id.clone(),
        ) {
            let model_events = decide_session(
                &child,
                &SessionCommand::SelectModel {
                    provider_id,
                    model_id,
                },
            )
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
            for event in &model_events {
                child = reduce_session(&child, event)
                    .map_err(|error| ApplicationError::new(error.code, error.message))?;
            }
            events.extend(model_events);
        }
        if parent_before.model.reasoning != ReasoningLevel::Off {
            let reasoning_events = decide_session(
                &child,
                &SessionCommand::SetReasoning {
                    reasoning: parent_before.model.reasoning,
                },
            )
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
            events.extend(reasoning_events);
        }
        if let Some(goal) = checkpoint
            .goal_revision_id
            .as_ref()
            .and_then(|id| parent_before.goal.revisions.get(id))
        {
            let goal_events = decide_session(
                &child,
                &SessionCommand::ReviseGoal {
                    revision: GoalRevision {
                        id: GoalRevisionId::from(self.ids.next_id("goal")),
                        parent_revision_id: None,
                        text: goal.text.clone(),
                        created_by: ActorId::from("system:session-fork"),
                        reason: format!("fork-from:{}", checkpoint.id),
                        created_at_millis: self.clock.now_unix_millis(),
                    },
                },
            )
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
            events.extend(goal_events);
        }
        self.store
            .commit_session(&child_session_id, 0, events, self.clock.now_unix_millis())
            .map_err(storage_error)?;

        let child_series = fork_context_series(
            &source,
            &checkpoint,
            child_session_id.clone(),
            ContextSeriesId::from(self.ids.next_id("series")),
            self.clock.now_unix_millis(),
        )
        .map_err(context_storage_error)?;
        self.store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: None,
                next_series: child_series.clone(),
                compaction_record: None,
            })
            .map_err(context_storage_error)?;

        Ok(ForkView {
            parent_session_id: self.session_id.clone(),
            child_session_id,
            checkpoint_id: checkpoint.id,
            child_context_series_id: child_series.id,
        })
    }

    pub fn fork_session_reference(
        &self,
        checkpoint_reference: &str,
        requested_child_session_id: Option<SessionId>,
    ) -> Result<ForkView, ApplicationError> {
        let checkpoint = self.resolve_checkpoint(checkpoint_reference)?;
        self.fork_session(&checkpoint.id, requested_child_session_id)
    }

    /// 把用户指定内容作为不可压缩 Pinned Context。
    pub fn pin(&mut self, value: &str) -> Result<ContextView, ApplicationError> {
        if value.trim().is_empty() {
            return Err(ApplicationError::new("empty-pin", "Pin 内容不能为空"));
        }
        self.append_context_item(
            ContextKind::Pinned,
            Priority::Critical,
            &format!("pin:{}", self.ids.next_id("source")),
            value,
            true,
        )?;
        self.publish_context_changed()?;
        self.context()
    }

    /// 设置或清除单一检索焦点；焦点本身是 durable Context Item。
    pub fn focus(&mut self, value: Option<&str>) -> Result<ContextView, ApplicationError> {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(ApplicationError::new("empty-focus", "Focus 不能为空"));
        }
        let replacement = value.map(|value| {
            self.new_context_item(
                ContextKind::Task,
                Priority::High,
                "focus:active",
                value,
                true,
            )
        });
        self.replace_context_source("focus:active", replacement)?;
        self.publish_context_changed()?;
        self.context()
    }

    #[must_use]
    pub fn cache(&self) -> CacheView {
        let l1 = self.cache.l1_metrics();
        let l2 = self.cache.l2_metrics();
        let hits = l1.hits.saturating_add(l2.map_or(0, |metrics| metrics.hits));
        let misses = l1
            .misses
            .saturating_add(l2.map_or(0, |metrics| metrics.misses));
        let total = hits.saturating_add(misses);
        CacheView {
            l1,
            l2,
            effective_hit_rate_percent: (total != 0)
                .then(|| u8::try_from(hits.saturating_mul(100) / total).unwrap_or(100)),
        }
    }

    pub fn shutdown(&self, reason: &str) -> Result<(), ApplicationError> {
        if let Some(browser) = &self.browser_runtime
            && browser.view().map_err(browser_error)?.adapter_alive
        {
            browser
                .close(self.clock.now_unix_millis())
                .map_err(browser_error)?;
        }
        self.publish(
            HarnessEvent::SystemShutdown {
                reason: reason.to_owned(),
            },
            self.session_scope(),
            EventPriority::Critical,
        )
    }

    /// 运行 2–8 个只读 Researcher Agent；Kernel/Outbox/Session/Budget 全链真实接线。
    pub fn run_parallel_agent_team(
        &mut self,
        prompt: &str,
        agent_count: usize,
    ) -> Result<PlanView, ApplicationError> {
        let PreparedAgentTeam { job, continuation } =
            self.prepare_parallel_agent_team(prompt, agent_count)?;
        let outcomes = job.execute()?;
        self.finalize_parallel_agent_team(continuation, outcomes, &BTreeSet::new(), false)
    }

    /// 前台完成 Kernel/Outbox/Session 准备，返回可安全移入后台线程的纯 Model Job。
    pub fn prepare_parallel_agent_team(
        &mut self,
        prompt: &str,
        agent_count: usize,
    ) -> Result<PreparedAgentTeam, ApplicationError> {
        if !(2..=8).contains(&agent_count) {
            return Err(ApplicationError::new(
                "agent-team-size-invalid",
                "并行 Team 当前支持 2..=8 个 Agent",
            ));
        }
        let runtime = self.model_runtime.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new("model-runtime-missing", "并行 Team 需要 Model Runtime")
        })?;
        if self.status()?.goal.is_none() {
            self.set_goal(prompt)?;
        }
        self.replace_context_source(
            "task:active",
            Some(self.new_context_item(
                ContextKind::Task,
                Priority::Critical,
                "task:active",
                prompt,
                true,
            )),
        )?;
        let mission_id = MissionId::from(self.ids.next_id("mission"));
        self.active_mission_id = Some(mission_id.clone());
        self.apply_mission_command(
            &mission_id,
            MissionCommand::CreateMission {
                mission_id: mission_id.clone(),
                project_id: self.project_id.clone(),
                goal: prompt.to_owned(),
            },
        )?;
        let staffing_tasks = (0..agent_count)
            .map(|index| StaffingTask {
                task_id: TaskId::from(format!("task:research:{index:02}")),
                required_capabilities: ["research".to_owned()].into_iter().collect(),
                preferred_roles: [AgentRole::Researcher].into_iter().collect(),
                forbidden_agents: BTreeSet::new(),
            })
            .collect::<Vec<_>>();
        let catalog = self.agent_catalog.as_ref().ok_or_else(|| {
            ApplicationError::new("agent-catalog-missing", "Agent Catalog 尚未注入")
        })?;
        let assignments = StaffingRouter::assign(&staffing_tasks, catalog).map_err(agent_error)?;
        self.apply_mission_command(
            &mission_id,
            MissionCommand::InstallPlan {
                nodes: assignments
                    .iter()
                    .enumerate()
                    .map(|(index, assignment)| WorkflowNodeDefinition {
                        id: assignment.task_id.clone(),
                        title: format!("并行研究分片 {}：{prompt}", index + 1),
                        kind: NodeKind::Task,
                        depends_on: vec![],
                        agent_definition_id: assignment.agent_id.clone(),
                        requires_approval: None,
                    })
                    .collect(),
            },
        )?;
        let model_handler = Arc::new(
            ModelAgentHandler::new(Arc::new(runtime), Duration::from_secs(120))
                .map_err(agent_error)?,
        );
        let live_steering = SharedSteeringBuffer::default();
        let handler = Arc::new(SteeringAgentHandler::new(
            model_handler,
            live_steering.clone(),
        ));
        let executor = BoundedAgentExecutor::new(
            handler,
            self.agent_budget_policy
                .max_parallel_agents
                .min(agent_count),
        )
        .map_err(agent_error)?;
        let state = self.recover_mission(&mission_id)?;
        let priorities = state
            .nodes
            .keys()
            .map(|task_id| {
                let priority = self
                    .agent_state
                    .as_ref()
                    .map_or(Ok(0), |store| store.task_priority(&mission_id, task_id))?;
                Ok((task_id.clone(), priority))
            })
            .collect::<Result<BTreeMap<_, _>, harness_agent::AgentError>>()
            .map_err(agent_error)?;
        let estimated_costs = self.estimated_assignment_costs(&assignments);
        let scheduled = AgentScheduler {
            // 所有 Run 进入有界 Executor queue；真正并行度仍由 max_parallel worker 限制。
            concurrency_limit: agent_count,
        }
        .schedule_with_constraints(
            &state,
            &assignments,
            &priorities,
            &BTreeMap::new(),
            &estimated_costs,
            &BTreeSet::new(),
            self.mode_profile.max_cost_units,
        )
        .runs;
        if scheduled.is_empty() {
            return Err(ApplicationError::new(
                "agent-cost-budget-exhausted",
                self.mode_profile.max_cost_units.to_string(),
            ));
        }
        let steering_messages = self.take_steering_messages(&mission_id)?;
        let mut dispatches = Vec::new();
        let mut claimed_by_run = BTreeMap::new();
        let mut requests_by_run = BTreeMap::new();
        for scheduled_run in scheduled {
            let run_id = RunId::from(self.ids.next_id("run"));
            self.apply_mission_command(
                &mission_id,
                MissionCommand::StartNode {
                    node_id: scheduled_run.task_id.clone(),
                    run_id: run_id.clone(),
                },
            )?;
            let now = self.clock.now_unix_millis();
            let effect = self
                .store
                .list_claimable_effects(now, 256)
                .map_err(storage_error)?
                .into_iter()
                .find(|entry| {
                    entry.mission_id == mission_id
                        && matches!(
                            &entry.intent,
                            EffectIntent::StartAgentRun { run_id: candidate, .. }
                                if candidate == &run_id
                        )
                })
                .ok_or_else(|| {
                    ApplicationError::new("agent-start-effect-missing", run_id.to_string())
                })?;
            let claimed = self
                .store
                .try_claim_effect(
                    &effect.effect_id,
                    ClaimToken::from(self.ids.next_id("claim")),
                    now,
                    now.saturating_add(30_000),
                )
                .map_err(storage_error)?
                .ok_or_else(|| {
                    ApplicationError::new("agent-start-effect-claim-lost", run_id.to_string())
                })?;
            let (agent_id, role, cancellation, context) =
                self.begin_agent_run(&mission_id, &scheduled_run.task_id, &run_id, false)?;
            self.publish(
                HarnessEvent::AgentStatus {
                    agent_id: agent_id.clone(),
                    role: format!("{role:?}"),
                    status: "running".to_owned(),
                    detail: scheduled_run.task_id.to_string(),
                },
                self.mission_scope(&mission_id),
                EventPriority::Normal,
            )?;
            let endpoint_id = self
                .recover_mission(&mission_id)?
                .runs
                .get(&run_id)
                .map(|run| run.endpoint_id.clone())
                .ok_or_else(|| ApplicationError::new("agent-run-missing", run_id.to_string()))?;
            let request = AgentExecutionRequest {
                session_id: agent_session_id(&run_id),
                contract: AgentTaskContract {
                    mission_id: mission_id.clone(),
                    task_id: scheduled_run.task_id.clone(),
                    run_id: run_id.clone(),
                    parent_run_id: None,
                    endpoint_id,
                    agent_definition_id: agent_id,
                    role,
                    objective: format!(
                        "{prompt}\n你负责独立研究分片 {}，返回可验证的简明结论。",
                        scheduled_run.task_id
                    ),
                    acceptance_criteria: vec!["返回非空结论".to_owned()],
                    max_turns: 2,
                    deadline_millis: now.saturating_add(120_000),
                    planning_budget: None,
                },
                context,
                steering_messages: steering_messages.clone(),
                model_tools: vec![],
                model_continuation: None,
            };
            dispatches.push(AgentDispatch {
                cancellation,
                request: request.clone(),
            });
            requests_by_run.insert(run_id.clone(), request);
            claimed_by_run.insert(run_id, claimed);
        }
        Ok(PreparedAgentTeam {
            job: AgentTeamModelJob {
                executor,
                dispatches,
                started_at_millis: self.clock.now_unix_millis(),
                steering: live_steering,
            },
            continuation: AgentTeamContinuation {
                mission_id,
                prompt: prompt.to_owned(),
                claimed_by_run,
                requests_by_run,
            },
        })
    }

    /// 创建 Requirements + Explorer → Architect → Planner → Coder workers →
    /// 独立审查门 → Tester → 可选 Release 的能力路由 Evidence DAG。
    pub fn prepare_adaptive_agent_team(
        &mut self,
        prompt: &str,
        worker_count: usize,
    ) -> Result<PreparedAgentTeam, ApplicationError> {
        if !(1..=4).contains(&worker_count) {
            return Err(ApplicationError::new(
                "adaptive-workflow-worker-count-invalid",
                "Adaptive workflow 当前支持 1..=4 个 Coder worker",
            ));
        }
        if self.model_runtime.is_none() {
            return Err(ApplicationError::new(
                "model-runtime-missing",
                "Adaptive workflow 需要 Model Runtime",
            ));
        }
        let profile = AdaptiveTeamProfile::classify(prompt);
        let requirements_id = TaskId::from("task:requirements");
        let explorer_id = TaskId::from("task:explorer");
        let architect_id = TaskId::from("task:architect");
        let planner_id = TaskId::from("task:planner");
        let worker_ids = (0..worker_count)
            .map(|index| TaskId::from(format!("task:coder:{index:02}")))
            .collect::<Vec<_>>();
        let reviewer_id = TaskId::from("task:reviewer");
        let security_id = TaskId::from("task:security");
        let performance_id = TaskId::from("task:performance");
        let tester_id = TaskId::from("task:tester");
        let release_id = TaskId::from("task:release");

        let mut blueprints = vec![
            AdaptiveNodeBlueprint {
                id: requirements_id.clone(),
                title: format!("澄清范围、非目标与可验证验收标准：{prompt}"),
                kind: NodeKind::Task,
                depends_on: vec![],
                required_capability: "requirements-analysis",
                preferred_role: AgentRole::RequirementsAnalyst,
            },
            AdaptiveNodeBlueprint {
                id: explorer_id.clone(),
                title: format!("只读定位入口、依赖、符号与数据流：{prompt}"),
                kind: NodeKind::Task,
                depends_on: vec![],
                required_capability: "codebase-exploration",
                preferred_role: AgentRole::Explorer,
            },
            AdaptiveNodeBlueprint {
                id: architect_id.clone(),
                title: format!("定义边界、契约、失败模式与架构决策：{prompt}"),
                kind: NodeKind::Task,
                depends_on: vec![requirements_id.clone(), explorer_id.clone()],
                required_capability: "system-design",
                preferred_role: AgentRole::Architect,
            },
            AdaptiveNodeBlueprint {
                id: planner_id.clone(),
                title: format!("把需求与架构转成可并行 Evidence DAG：{prompt}"),
                kind: NodeKind::Task,
                depends_on: vec![architect_id.clone()],
                required_capability: "task-decomposition",
                preferred_role: AgentRole::Planner,
            },
        ];
        blueprints.extend(worker_ids.iter().enumerate().map(|(index, task_id)| {
            AdaptiveNodeBlueprint {
                id: task_id.clone(),
                title: format!("实现受控编码分片 {}：{prompt}", index + 1),
                kind: NodeKind::Task,
                depends_on: vec![planner_id.clone()],
                required_capability: "code-edit",
                preferred_role: AgentRole::Coder,
            }
        }));
        blueprints.push(AdaptiveNodeBlueprint {
            id: reviewer_id.clone(),
            title: format!("独立审查正确性、契约与回归风险：{prompt}"),
            kind: NodeKind::Review,
            depends_on: worker_ids.clone(),
            required_capability: "code-review",
            preferred_role: AgentRole::Reviewer,
        });
        let mut verification_dependencies = vec![reviewer_id.clone()];
        if profile.security {
            blueprints.push(AdaptiveNodeBlueprint {
                id: security_id.clone(),
                title: format!("独立安全审计与威胁模型：{prompt}"),
                kind: NodeKind::Review,
                depends_on: worker_ids.clone(),
                required_capability: "security-audit",
                preferred_role: AgentRole::SecurityAuditor,
            });
            verification_dependencies.push(security_id);
        }
        if profile.performance {
            blueprints.push(AdaptiveNodeBlueprint {
                id: performance_id.clone(),
                title: format!("测量基线、瓶颈与性能回归阈值：{prompt}"),
                kind: NodeKind::Review,
                depends_on: worker_ids.clone(),
                required_capability: "performance-analysis",
                preferred_role: AgentRole::PerformanceEngineer,
            });
            verification_dependencies.push(performance_id);
        }
        blueprints.push(AdaptiveNodeBlueprint {
            id: tester_id.clone(),
            title: format!("按验收标准验证所有实现与独立审查结论：{prompt}"),
            kind: NodeKind::Task,
            depends_on: verification_dependencies,
            required_capability: "test-execution",
            preferred_role: AgentRole::Tester,
        });
        if profile.release {
            blueprints.push(AdaptiveNodeBlueprint {
                id: release_id,
                title: format!("验证版本、产物、校验和与回滚方案：{prompt}"),
                kind: NodeKind::Review,
                depends_on: vec![tester_id],
                required_capability: "release-readiness",
                preferred_role: AgentRole::ReleaseManager,
            });
        }

        let catalog = self.agent_catalog.as_ref().ok_or_else(|| {
            ApplicationError::new("agent-catalog-missing", "Agent Catalog 尚未注入")
        })?;
        let staffing_tasks = blueprints
            .iter()
            .map(|node| StaffingTask {
                task_id: node.id.clone(),
                required_capabilities: [node.required_capability.to_owned()].into_iter().collect(),
                preferred_roles: [node.preferred_role].into_iter().collect(),
                forbidden_agents: BTreeSet::new(),
            })
            .collect::<Vec<_>>();
        let assignments = StaffingRouter::assign(&staffing_tasks, catalog).map_err(agent_error)?;
        let assignments = assignments
            .into_iter()
            .map(|assignment| (assignment.task_id, assignment.agent_id))
            .collect::<BTreeMap<_, _>>();
        let nodes = blueprints
            .into_iter()
            .map(|node| {
                let agent_definition_id = assignments.get(&node.id).cloned().ok_or_else(|| {
                    ApplicationError::new(
                        "adaptive-staffing-assignment-missing",
                        node.id.to_string(),
                    )
                })?;
                Ok(WorkflowNodeDefinition {
                    id: node.id,
                    title: node.title,
                    kind: node.kind,
                    depends_on: node.depends_on,
                    agent_definition_id,
                    requires_approval: None,
                })
            })
            .collect::<Result<Vec<_>, ApplicationError>>()?;

        if self.status()?.goal.is_none() {
            self.set_goal(prompt)?;
        }
        self.replace_context_source(
            "task:active",
            Some(self.new_context_item(
                ContextKind::Task,
                Priority::Critical,
                "task:active",
                prompt,
                true,
            )),
        )?;
        let mission_id = MissionId::from(self.ids.next_id("mission"));
        self.active_mission_id = Some(mission_id.clone());
        self.apply_mission_command(
            &mission_id,
            MissionCommand::CreateMission {
                mission_id: mission_id.clone(),
                project_id: self.project_id.clone(),
                goal: prompt.to_owned(),
            },
        )?;
        self.apply_mission_command(&mission_id, MissionCommand::InstallPlan { nodes })?;
        self.prepare_next_agent_wave(&mission_id)
    }

    /// 创建 Planner → Coder workers → Reviewer → Tester 的精简 Evidence DAG。
    pub fn prepare_role_evidence_team(
        &mut self,
        prompt: &str,
        worker_count: usize,
    ) -> Result<PreparedAgentTeam, ApplicationError> {
        if !(1..=4).contains(&worker_count) {
            return Err(ApplicationError::new(
                "agent-workflow-worker-count-invalid",
                "Role workflow 当前支持 1..=4 个 Coder worker",
            ));
        }
        if self.model_runtime.is_none() {
            return Err(ApplicationError::new(
                "model-runtime-missing",
                "Role workflow 需要 Model Runtime",
            ));
        }
        if self.status()?.goal.is_none() {
            self.set_goal(prompt)?;
        }
        self.replace_context_source(
            "task:active",
            Some(self.new_context_item(
                ContextKind::Task,
                Priority::Critical,
                "task:active",
                prompt,
                true,
            )),
        )?;
        let mission_id = MissionId::from(self.ids.next_id("mission"));
        self.active_mission_id = Some(mission_id.clone());
        self.apply_mission_command(
            &mission_id,
            MissionCommand::CreateMission {
                mission_id: mission_id.clone(),
                project_id: self.project_id.clone(),
                goal: prompt.to_owned(),
            },
        )?;
        let planner_id = TaskId::from("task:planner");
        let worker_ids = (0..worker_count)
            .map(|index| TaskId::from(format!("task:coder:{index:02}")))
            .collect::<Vec<_>>();
        let reviewer_id = TaskId::from("task:reviewer");
        let tester_id = TaskId::from("task:tester");
        let mut nodes = vec![WorkflowNodeDefinition {
            id: planner_id.clone(),
            title: format!("规划：{prompt}"),
            kind: NodeKind::Task,
            depends_on: vec![],
            agent_definition_id: AgentDefinitionId::from("agent:planner"),
            requires_approval: None,
        }];
        nodes.extend(worker_ids.iter().enumerate().map(|(index, task_id)| {
            WorkflowNodeDefinition {
                id: task_id.clone(),
                title: format!("编码分片 {}：{prompt}", index + 1),
                kind: NodeKind::Task,
                depends_on: vec![planner_id.clone()],
                agent_definition_id: AgentDefinitionId::from("agent:coder"),
                requires_approval: None,
            }
        }));
        nodes.push(WorkflowNodeDefinition {
            id: reviewer_id.clone(),
            title: format!("审查所有编码分片：{prompt}"),
            kind: NodeKind::Review,
            depends_on: worker_ids,
            agent_definition_id: AgentDefinitionId::from("agent:reviewer"),
            requires_approval: None,
        });
        nodes.push(WorkflowNodeDefinition {
            id: tester_id,
            title: format!("验证审查结论：{prompt}"),
            kind: NodeKind::Task,
            depends_on: vec![reviewer_id],
            agent_definition_id: AgentDefinitionId::from("agent:tester"),
            requires_approval: None,
        });
        self.apply_mission_command(&mission_id, MissionCommand::InstallPlan { nodes })?;
        self.prepare_next_agent_wave(&mission_id)
    }

    /// `/review` 使用单一 Reviewer Node，但仍经过 AgentSession、Evidence Gate 与 Kernel。
    pub fn prepare_review_agent(
        &mut self,
        review_input: &str,
    ) -> Result<PreparedAgentTeam, ApplicationError> {
        if review_input.trim().is_empty() {
            return Err(ApplicationError::new("review-input-empty", "Diff 为空"));
        }
        self.replace_context_source(
            "task:active",
            Some(self.new_context_item(
                ContextKind::Task,
                Priority::Critical,
                "task:active",
                review_input,
                true,
            )),
        )?;
        let mission_id = MissionId::from(self.ids.next_id("mission"));
        self.active_mission_id = Some(mission_id.clone());
        self.apply_mission_command(
            &mission_id,
            MissionCommand::CreateMission {
                mission_id: mission_id.clone(),
                project_id: self.project_id.clone(),
                goal: "审查当前 Git diff 的 Bug/Security/Logic/Regression/Performance".to_owned(),
            },
        )?;
        self.apply_mission_command(
            &mission_id,
            MissionCommand::InstallPlan {
                nodes: vec![WorkflowNodeDefinition {
                    id: TaskId::from("task:reviewer"),
                    title: "自动 Review Git diff".to_owned(),
                    kind: NodeKind::Review,
                    depends_on: vec![],
                    agent_definition_id: AgentDefinitionId::from("agent:reviewer"),
                    requires_approval: None,
                }],
            },
        )?;
        self.prepare_next_agent_wave(&mission_id)
    }

    /// 从 Kernel ready nodes 创建下一波后台 Model Job；依赖结果进入动态上下文尾部。
    pub fn prepare_next_agent_wave(
        &mut self,
        mission_id: &MissionId,
    ) -> Result<PreparedAgentTeam, ApplicationError> {
        let runtime = self.model_runtime.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new("model-runtime-missing", "Agent wave 需要 Model Runtime")
        })?;
        let mission = self.recover_mission(mission_id)?;
        let ready = find_ready_node_ids(&mission);
        if ready.is_empty() {
            return Err(ApplicationError::new(
                "agent-wave-not-ready",
                mission_id.to_string(),
            ));
        }
        let assignments = ready
            .iter()
            .map(|task_id| StaffingAssignment {
                task_id: task_id.clone(),
                agent_id: mission.nodes[task_id].agent_definition_id.clone(),
                score: 0,
                reason_summary: "kernel-ready-node".to_owned(),
                catalog_fingerprint: "kernel-plan".to_owned(),
            })
            .collect::<Vec<_>>();
        let priorities = ready
            .iter()
            .map(|task_id| {
                let priority = self
                    .agent_state
                    .as_ref()
                    .map_or(Ok(0), |store| store.task_priority(mission_id, task_id))?;
                Ok((task_id.clone(), priority))
            })
            .collect::<Result<BTreeMap<_, _>, harness_agent::AgentError>>()
            .map_err(agent_error)?;
        let estimated_costs = self.estimated_assignment_costs(&assignments);
        let scheduled = AgentScheduler {
            concurrency_limit: self.agent_budget_policy.max_parallel_agents,
        }
        .schedule_with_constraints(
            &mission,
            &assignments,
            &priorities,
            &BTreeMap::new(),
            &estimated_costs,
            &BTreeSet::new(),
            self.mode_profile.max_cost_units,
        )
        .runs;
        if scheduled.is_empty() {
            return Err(ApplicationError::new(
                "agent-cost-budget-exhausted",
                self.mode_profile.max_cost_units.to_string(),
            ));
        }
        let steering_messages = self.take_steering_messages(mission_id)?;
        let mut dispatches = Vec::new();
        let mut claimed_by_run = BTreeMap::new();
        let mut requests_by_run = BTreeMap::new();
        for scheduled_run in scheduled {
            let node = mission.nodes.get(&scheduled_run.task_id).ok_or_else(|| {
                ApplicationError::new("agent-node-missing", scheduled_run.task_id.to_string())
            })?;
            let run_id = RunId::from(self.ids.next_id("run"));
            self.apply_mission_command(
                mission_id,
                MissionCommand::StartNode {
                    node_id: scheduled_run.task_id.clone(),
                    run_id: run_id.clone(),
                },
            )?;
            let now = self.clock.now_unix_millis();
            let effect = self
                .store
                .list_claimable_effects(now, 256)
                .map_err(storage_error)?
                .into_iter()
                .find(|entry| {
                    entry.mission_id == *mission_id
                        && matches!(
                            &entry.intent,
                            EffectIntent::StartAgentRun { run_id: candidate, .. }
                                if candidate == &run_id
                        )
                })
                .ok_or_else(|| {
                    ApplicationError::new("agent-start-effect-missing", run_id.to_string())
                })?;
            let claimed = self
                .store
                .try_claim_effect(
                    &effect.effect_id,
                    ClaimToken::from(self.ids.next_id("claim")),
                    now,
                    now.saturating_add(30_000),
                )
                .map_err(storage_error)?
                .ok_or_else(|| {
                    ApplicationError::new("agent-start-effect-claim-lost", run_id.to_string())
                })?;
            let (agent_id, role, cancellation, mut context) =
                self.begin_agent_run(mission_id, &scheduled_run.task_id, &run_id, false)?;
            let dependency_summary = node
                .depends_on
                .iter()
                .filter_map(|dependency| {
                    mission.nodes.get(dependency).and_then(|dependency_node| {
                        dependency_node
                            .output_summary
                            .as_ref()
                            .map(|summary| format!("- {dependency}: {summary}"))
                    })
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !dependency_summary.is_empty() {
                context.dynamic_context.push_str(&format!(
                    "\n<dependency-results>\n{dependency_summary}\n</dependency-results>"
                ));
                context.token_cost = context
                    .token_cost
                    .saturating_add(HeuristicTokenizer.count_tokens(&dependency_summary));
                context.fingerprint = hash_text(&format!(
                    "{}\n{}",
                    context.stable_instructions, context.dynamic_context
                ))
                .to_string();
                self.set_agent_session_context_fingerprint(&run_id, &context.fingerprint)?;
            }
            let endpoint_id = self
                .recover_mission(mission_id)?
                .runs
                .get(&run_id)
                .map(|run| run.endpoint_id.clone())
                .ok_or_else(|| ApplicationError::new("agent-run-missing", run_id.to_string()))?;
            let model_tools = self.agent_model_tools(&agent_id, role, &node.title)?;
            let request = AgentExecutionRequest {
                session_id: agent_session_id(&run_id),
                contract: AgentTaskContract {
                    mission_id: mission_id.clone(),
                    task_id: scheduled_run.task_id.clone(),
                    run_id: run_id.clone(),
                    parent_run_id: None,
                    endpoint_id,
                    agent_definition_id: agent_id,
                    role,
                    objective: format!("{}\n当前节点：{}", mission.goal, node.title),
                    acceptance_criteria: role_acceptance_criteria(role),
                    max_turns: role_max_turns(role),
                    deadline_millis: now.saturating_add(120_000),
                    planning_budget: (role == AgentRole::Planner)
                        .then(PlanningBudget::bounded_default),
                },
                context,
                steering_messages: steering_messages.clone(),
                model_tools,
                model_continuation: None,
            };
            dispatches.push(AgentDispatch {
                cancellation,
                request: request.clone(),
            });
            requests_by_run.insert(run_id.clone(), request);
            claimed_by_run.insert(run_id, claimed);
        }
        let model_handler = Arc::new(
            ModelAgentHandler::new(Arc::new(runtime), Duration::from_secs(120))
                .map_err(agent_error)?,
        );
        let live_steering = SharedSteeringBuffer::default();
        let handler = Arc::new(SteeringAgentHandler::new(
            model_handler,
            live_steering.clone(),
        ));
        let executor = BoundedAgentExecutor::new(
            handler,
            self.agent_budget_policy
                .max_parallel_agents
                .min(dispatches.len()),
        )
        .map_err(agent_error)?;
        Ok(PreparedAgentTeam {
            job: AgentTeamModelJob {
                executor,
                dispatches,
                started_at_millis: self.clock.now_unix_millis(),
                steering: live_steering,
            },
            continuation: AgentTeamContinuation {
                mission_id: mission_id.clone(),
                prompt: mission.goal,
                claimed_by_run,
                requests_by_run,
            },
        })
    }

    /// 显式恢复崩溃前已进入 Running、且 Start Effect 租约已经可重新领取的 Team。
    pub fn prepare_recovered_agent_team(&mut self) -> Result<PreparedAgentTeam, ApplicationError> {
        let mission_id = self.active_mission_id.clone().ok_or_else(|| {
            ApplicationError::new("agent-recovery-mission-missing", "没有可恢复的活动 Mission")
        })?;
        let runtime = self.model_runtime.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new("model-runtime-missing", "恢复 Team 需要 Model Runtime")
        })?;
        let sessions = self
            .agent_state
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("agent-state-missing", "Agent State Store 尚未注入")
            })?
            .recoverable_sessions()
            .map_err(agent_error)?
            .into_iter()
            .filter(|session| {
                session.mission_id == mission_id
                    && matches!(
                        session.status,
                        AgentSessionStatus::Prepared | AgentSessionStatus::Running
                    )
            })
            .collect::<Vec<_>>();
        if sessions.is_empty() {
            return Err(ApplicationError::new(
                "agent-recovery-empty",
                mission_id.to_string(),
            ));
        }
        let mission = self.recover_mission(&mission_id)?;
        let prompt = mission.goal.clone();
        let steering_messages = self.take_steering_messages(&mission_id)?;
        let mut dispatches = Vec::new();
        let mut claimed_by_run = BTreeMap::new();
        let mut requests_by_run = BTreeMap::new();
        for session in sessions {
            let node = mission.nodes.get(&session.task_id).ok_or_else(|| {
                ApplicationError::new("agent-recovery-node-missing", session.task_id.to_string())
            })?;
            if node.status != NodeStatus::Running || node.run_id.as_ref() != Some(&session.run_id) {
                return Err(ApplicationError::new(
                    "agent-recovery-kernel-mismatch",
                    session.run_id.to_string(),
                ));
            }
            let now = self.clock.now_unix_millis();
            let effect = self
                .store
                .list_claimable_effects(now, 256)
                .map_err(storage_error)?
                .into_iter()
                .find(|entry| {
                    entry.mission_id == mission_id
                        && matches!(
                            &entry.intent,
                            EffectIntent::StartAgentRun { run_id, .. }
                                | EffectIntent::ResumeAgentRun { run_id, .. }
                                if run_id == &session.run_id
                        )
                })
                .ok_or_else(|| {
                    ApplicationError::new(
                        "agent-recovery-effect-not-claimable",
                        format!(
                            "run={}；原 worker 租约可能仍有效，请稍后重试 /resume",
                            session.run_id
                        ),
                    )
                })?;
            let claimed = self
                .store
                .try_claim_effect(
                    &effect.effect_id,
                    ClaimToken::from(self.ids.next_id("claim")),
                    now,
                    now.saturating_add(30_000),
                )
                .map_err(storage_error)?
                .ok_or_else(|| {
                    ApplicationError::new(
                        "agent-recovery-effect-claim-lost",
                        session.run_id.to_string(),
                    )
                })?;
            let (agent_id, role, cancellation, context) =
                self.begin_agent_run(&mission_id, &session.task_id, &session.run_id, true)?;
            self.publish(
                HarnessEvent::AgentStatus {
                    agent_id: agent_id.clone(),
                    role: format!("{role:?}"),
                    status: "resumed".to_owned(),
                    detail: session.task_id.to_string(),
                },
                self.mission_scope(&mission_id),
                EventPriority::Normal,
            )?;
            let model_tools = self.agent_model_tools(&agent_id, role, &node.title)?;
            let request = AgentExecutionRequest {
                session_id: session.id,
                contract: AgentTaskContract {
                    mission_id: mission_id.clone(),
                    task_id: session.task_id.clone(),
                    run_id: session.run_id.clone(),
                    parent_run_id: session.parent_run_id,
                    endpoint_id: session.endpoint_id,
                    agent_definition_id: agent_id,
                    role,
                    objective: format!("{}\n恢复任务 {}：{}", prompt, session.task_id, node.title),
                    acceptance_criteria: role_acceptance_criteria(role),
                    max_turns: role_max_turns(role),
                    deadline_millis: now.saturating_add(120_000),
                    planning_budget: (role == AgentRole::Planner)
                        .then(PlanningBudget::bounded_default),
                },
                context,
                steering_messages: steering_messages.clone(),
                model_tools,
                model_continuation: None,
            };
            dispatches.push(AgentDispatch {
                cancellation,
                request: request.clone(),
            });
            requests_by_run.insert(session.run_id.clone(), request);
            claimed_by_run.insert(session.run_id, claimed);
        }
        let model_handler = Arc::new(
            ModelAgentHandler::new(Arc::new(runtime), Duration::from_secs(120))
                .map_err(agent_error)?,
        );
        let live_steering = SharedSteeringBuffer::default();
        let handler = Arc::new(SteeringAgentHandler::new(
            model_handler,
            live_steering.clone(),
        ));
        let executor = BoundedAgentExecutor::new(
            handler,
            self.agent_budget_policy
                .max_parallel_agents
                .min(dispatches.len()),
        )
        .map_err(agent_error)?;
        Ok(PreparedAgentTeam {
            job: AgentTeamModelJob {
                executor,
                dispatches,
                started_at_millis: self.clock.now_unix_millis(),
                steering: live_steering,
            },
            continuation: AgentTeamContinuation {
                mission_id,
                prompt,
                claimed_by_run,
                requests_by_run,
            },
        })
    }

    /// 后台结果回到前台后，唯一地提交 Kernel/SQLite 终态。
    pub fn finalize_parallel_agent_team(
        &mut self,
        continuation: AgentTeamContinuation,
        outcomes: Vec<AgentExecutionOutcome>,
        cancelled_tasks: &BTreeSet<TaskId>,
        cancel_mission: bool,
    ) -> Result<PlanView, ApplicationError> {
        let mut continuation = continuation;
        let mut outcomes = outcomes;
        let mut cancelled_tasks = cancelled_tasks.clone();
        let mut cancel_mission = cancel_mission;
        loop {
            let step = self.finalize_parallel_agent_team_step(
                continuation,
                outcomes,
                &cancelled_tasks,
                cancel_mission,
            )?;
            let Some(next) = step.next else {
                return Ok(step.plan);
            };
            let PreparedAgentTeam {
                job,
                continuation: next_continuation,
            } = next;
            outcomes = job.execute()?;
            continuation = next_continuation;
            cancelled_tasks.clear();
            cancel_mission = false;
        }
    }

    /// 完成一个 Model wave；ToolYield 会生成下一次后台 Model Job，而不阻塞 Terminal。
    pub fn finalize_parallel_agent_team_step(
        &mut self,
        mut continuation: AgentTeamContinuation,
        outcomes: Vec<AgentExecutionOutcome>,
        cancelled_tasks: &BTreeSet<TaskId>,
        cancel_mission: bool,
    ) -> Result<AgentTeamFinalizeStep, ApplicationError> {
        if cancel_mission {
            for (_, claimed) in continuation.claimed_by_run {
                self.store
                    .complete_effect(EffectCompletion {
                        fence: CompletionFence {
                            effect_id: claimed.claim.effect_id,
                            mission_epoch: claimed.claim.mission_epoch,
                            claim_token: claimed.claim.claim_token,
                            run_fence: claimed.claim.run_fence,
                        },
                        outcome: EffectOutcome::Completed,
                        result: Some(serde_json::json!({"cancelled":true})),
                        error: None,
                        recorded_at_millis: self.clock.now_unix_millis(),
                    })
                    .map_err(storage_error)?;
            }
            self.apply_mission_command(
                &continuation.mission_id,
                MissionCommand::CancelMission {
                    reason: "user-cancelled-background-team".to_owned(),
                },
            )?;
            self.drive_effects(&continuation.mission_id, &continuation.prompt)?;
            return Ok(AgentTeamFinalizeStep {
                plan: self.plan_for(&continuation.mission_id)?,
                next: None,
            });
        }
        let mut accepted_results = Vec::new();
        let mut next_dispatches = Vec::new();
        let mut next_claimed_by_run = BTreeMap::new();
        let mut next_requests_by_run = BTreeMap::new();
        for outcome in outcomes {
            let claimed = continuation
                .claimed_by_run
                .remove(&outcome.run_id)
                .ok_or_else(|| {
                    ApplicationError::new(
                        "agent-claimed-effect-missing",
                        outcome.run_id.to_string(),
                    )
                })?;
            let request = continuation.requests_by_run.remove(&outcome.run_id);
            let completion_fence = CompletionFence {
                effect_id: claimed.claim.effect_id.clone(),
                mission_epoch: claimed.claim.mission_epoch,
                claim_token: claimed.claim.claim_token.clone(),
                run_fence: claimed.claim.run_fence,
            };
            if cancelled_tasks.contains(&outcome.task_id) {
                self.store
                    .complete_effect(EffectCompletion {
                        fence: completion_fence,
                        outcome: EffectOutcome::Completed,
                        result: Some(serde_json::json!({"cancelled":true})),
                        error: None,
                        recorded_at_millis: self.clock.now_unix_millis(),
                    })
                    .map_err(storage_error)?;
                self.apply_mission_command(
                    &continuation.mission_id,
                    MissionCommand::CancelNode {
                        node_id: outcome.task_id,
                        reason: "user-queue-cancel".to_owned(),
                    },
                )?;
                continue;
            }
            if let Some(mut result) = outcome.result {
                if let Some(tool_yield) = result.model_tool_yield.take() {
                    let mut request = request.ok_or_else(|| {
                        ApplicationError::new(
                            "agent-tool-request-missing",
                            outcome.run_id.to_string(),
                        )
                    })?;
                    self.set_agent_session_status(
                        &outcome.run_id,
                        AgentSessionStatus::WaitingTool,
                    )?;
                    if let Some(waiting) = self.apply_agent_tool_yield(&mut request, tool_yield)? {
                        let invocation_id = waiting.invocation.id.clone();
                        let request_id = waiting
                            .invocation
                            .approval_request_id
                            .clone()
                            .ok_or_else(|| {
                                ApplicationError::new(
                                    "tool-approval-id-missing",
                                    invocation_id.to_string(),
                                )
                            })?;
                        let approval = self
                            .tool_runtime
                            .as_ref()
                            .ok_or_else(|| {
                                ApplicationError::new(
                                    "tool-runtime-missing",
                                    "Tool Runtime 尚未注入",
                                )
                            })?
                            .pending_approvals()
                            .map_err(tool_error)?
                            .into_iter()
                            .find(|approval| approval.id == request_id)
                            .ok_or_else(|| {
                                ApplicationError::new(
                                    "tool-approval-request-not-found",
                                    request_id.to_string(),
                                )
                            })?;
                        self.apply_mission_command(
                            &continuation.mission_id,
                            MissionCommand::RequestApproval {
                                node_id: outcome.task_id.clone(),
                                run_id: outcome.run_id.clone(),
                                approval_id: ApprovalId::from(request_id.to_string()),
                                action: serde_json::json!({
                                    "tool":waiting.invocation.tool_name.clone(),
                                    "toolInvocationId":invocation_id,
                                    "permission":waiting.invocation.permission_action.clone(),
                                    "risk":format!("{:?}",approval.risk).to_lowercase()
                                })
                                .to_string(),
                                reason: approval.reason,
                            },
                        )?;
                        self.set_agent_session_status(
                            &outcome.run_id,
                            AgentSessionStatus::WaitingApproval,
                        )?;
                        self.set_agent_session_previous_response(
                            &outcome.run_id,
                            waiting.continuation.previous_response_id.clone(),
                        )?;
                        self.store
                            .complete_effect(EffectCompletion {
                                fence: completion_fence,
                                outcome: EffectOutcome::Completed,
                                result: Some(serde_json::json!({
                                    "waitingApproval":request_id,
                                    "toolInvocationId":invocation_id
                                })),
                                error: None,
                                recorded_at_millis: self.clock.now_unix_millis(),
                            })
                            .map_err(storage_error)?;
                        self.pending_parallel_tool_continuations.insert(
                            invocation_id,
                            PendingParallelToolContinuation {
                                request,
                                waiting,
                                prompt: continuation.prompt.clone(),
                            },
                        );
                        continue;
                    }
                    self.set_agent_session_status(&outcome.run_id, AgentSessionStatus::Running)?;
                    let cancellation = self
                        .run_controls
                        .lock()
                        .map_err(|_| {
                            ApplicationError::new(
                                "run-controls-poisoned",
                                outcome.run_id.to_string(),
                            )
                        })?
                        .token(&outcome.run_id)
                        .ok_or_else(|| {
                            ApplicationError::new("run-control-missing", outcome.run_id.to_string())
                        })?;
                    next_dispatches.push(AgentDispatch {
                        request: request.clone(),
                        cancellation,
                    });
                    next_claimed_by_run.insert(outcome.run_id.clone(), claimed);
                    next_requests_by_run.insert(outcome.run_id, request);
                    continue;
                }
                let role = self
                    .agent_catalog
                    .as_ref()
                    .and_then(|catalog| catalog.definition(&outcome.agent_definition_id))
                    .and_then(|definition| definition.roles.iter().next().copied())
                    .unwrap_or(AgentRole::Coder);
                if let Some(kind) = role_evidence_kind(role) {
                    result.evidence.push(Evidence {
                        kind: kind.to_owned(),
                        reference: outcome.run_id.to_string(),
                        summary: result.summary.clone(),
                    });
                }
                if matches!(
                    role,
                    AgentRole::Reviewer
                        | AgentRole::SecurityAuditor
                        | AgentRole::PerformanceEngineer
                        | AgentRole::Tester
                        | AgentRole::ReleaseManager
                ) && let Some(repository) = self.repository.as_ref()
                {
                    result
                        .evidence
                        .extend(lsp_run_evidence(repository, outcome.run_id.as_str())?);
                }
                let required_evidence = role_evidence_kind(role).into_iter().collect::<Vec<_>>();
                validate_required_evidence(&result, &required_evidence).map_err(agent_error)?;
                self.charge_agent_budget(
                    Some(&outcome.run_id),
                    result
                        .metrics
                        .input_tokens
                        .saturating_add(result.metrics.output_tokens),
                    result.metrics.tool_calls,
                )?;
                if let Some(store) = self.agent_state.as_ref() {
                    store
                        .save_result(&outcome.run_id, &result, self.clock.now_unix_millis())
                        .map_err(agent_error)?;
                }
                self.set_agent_session_status(&outcome.run_id, AgentSessionStatus::Submitted)?;
                self.apply_mission_command(
                    &continuation.mission_id,
                    MissionCommand::SubmitNode {
                        node_id: outcome.task_id.clone(),
                        run_id: outcome.run_id.clone(),
                        output_summary: result.summary.clone(),
                    },
                )?;
                self.store
                    .complete_effect(EffectCompletion {
                        fence: completion_fence,
                        outcome: EffectOutcome::Completed,
                        result: None,
                        error: None,
                        recorded_at_millis: self.clock.now_unix_millis(),
                    })
                    .map_err(storage_error)?;
                self.publish(
                    HarnessEvent::AgentStatus {
                        agent_id: outcome.agent_definition_id.clone(),
                        role: format!("{role:?}"),
                        status: "submitted".to_owned(),
                        detail: outcome.task_id.to_string(),
                    },
                    self.mission_scope(&continuation.mission_id),
                    EventPriority::Normal,
                )?;
                accepted_results.push(AgentResultEnvelope {
                    agent_id: outcome.agent_definition_id,
                    task_id: outcome.task_id,
                    run_id: outcome.run_id,
                    result,
                    decision_claims: BTreeMap::new(),
                });
            } else {
                let error = outcome.error.unwrap_or_else(|| {
                    harness_agent::AgentError::new(
                        "agent-outcome-empty",
                        outcome.run_id.to_string(),
                    )
                });
                self.set_agent_session_status(&outcome.run_id, AgentSessionStatus::Failed)?;
                self.finish_agent_run(
                    &outcome.agent_definition_id,
                    &outcome.run_id,
                    BudgetEscrowStatus::Failed,
                )?;
                self.apply_mission_command(
                    &continuation.mission_id,
                    MissionCommand::FailNode {
                        node_id: outcome.task_id,
                        run_id: outcome.run_id,
                        error: error.to_string(),
                    },
                )?;
                self.store
                    .complete_effect(EffectCompletion {
                        fence: completion_fence,
                        outcome: EffectOutcome::Failed,
                        result: None,
                        error: Some(error.to_string()),
                        recorded_at_millis: self.clock.now_unix_millis(),
                    })
                    .map_err(storage_error)?;
            }
        }
        if !continuation.claimed_by_run.is_empty() || !continuation.requests_by_run.is_empty() {
            return Err(ApplicationError::new(
                "agent-outcome-count-mismatch",
                format!(
                    "claims={}, requests={}",
                    continuation.claimed_by_run.len(),
                    continuation.requests_by_run.len()
                ),
            ));
        }
        self.drive_effects(&continuation.mission_id, &continuation.prompt)?;
        if accepted_results.len() > 1 {
            let _ = self.coordinate_results(&continuation.mission_id, &accepted_results)?;
        }
        if !next_dispatches.is_empty() {
            let runtime = self.model_runtime.as_ref().cloned().ok_or_else(|| {
                ApplicationError::new(
                    "model-runtime-missing",
                    "Tool continuation 需要 Model Runtime",
                )
            })?;
            let model_handler = Arc::new(
                ModelAgentHandler::new(Arc::new(runtime), Duration::from_secs(120))
                    .map_err(agent_error)?,
            );
            let live_steering = SharedSteeringBuffer::default();
            let handler = Arc::new(SteeringAgentHandler::new(
                model_handler,
                live_steering.clone(),
            ));
            let executor = BoundedAgentExecutor::new(
                handler,
                self.agent_budget_policy
                    .max_parallel_agents
                    .min(next_dispatches.len()),
            )
            .map_err(agent_error)?;
            return Ok(AgentTeamFinalizeStep {
                plan: self.plan_for(&continuation.mission_id)?,
                next: Some(PreparedAgentTeam {
                    job: AgentTeamModelJob {
                        executor,
                        dispatches: next_dispatches,
                        started_at_millis: self.clock.now_unix_millis(),
                        steering: live_steering,
                    },
                    continuation: AgentTeamContinuation {
                        mission_id: continuation.mission_id,
                        prompt: continuation.prompt,
                        claimed_by_run: next_claimed_by_run,
                        requests_by_run: next_requests_by_run,
                    },
                }),
            });
        }
        let state = self.recover_mission(&continuation.mission_id)?;
        if state.status == MissionStatus::Running
            && state
                .nodes
                .values()
                .all(|node| node.status == NodeStatus::Accepted)
        {
            self.apply_mission_command(
                &continuation.mission_id,
                MissionCommand::CompleteMission {},
            )?;
        }
        Ok(AgentTeamFinalizeStep {
            plan: self.plan_for(&continuation.mission_id)?,
            next: None,
        })
    }

    /// 用真实 Kernel/Storage/Outbox 驱动当前 Model Provider。
    pub fn run_fake_task(&mut self, prompt: &str) -> Result<PlanView, ApplicationError> {
        if self.status()?.goal.is_none() {
            self.set_goal(prompt)?;
        }
        self.append_context_item(
            ContextKind::Conversation,
            Priority::High,
            "user:terminal",
            prompt,
            false,
        )?;
        self.replace_context_source(
            "task:active",
            Some(self.new_context_item(
                ContextKind::Task,
                Priority::Critical,
                "task:active",
                prompt,
                true,
            )),
        )?;
        self.auto_compact_if_needed()?;
        self.cache_current_prompt()?;
        self.publish_context_changed()?;
        let mission_id = MissionId::from(self.ids.next_id("mission"));
        self.active_mission_id = Some(mission_id.clone());
        self.apply_mission_command(
            &mission_id,
            MissionCommand::CreateMission {
                mission_id: mission_id.clone(),
                project_id: self.project_id.clone(),
                goal: prompt.to_owned(),
            },
        )?;
        self.apply_mission_command(
            &mission_id,
            MissionCommand::InstallPlan {
                nodes: vec![WorkflowNodeDefinition {
                    id: TaskId::from("task:main"),
                    title: prompt.to_owned(),
                    kind: NodeKind::Task,
                    depends_on: vec![],
                    agent_definition_id: AgentDefinitionId::from("agent:coder"),
                    requires_approval: None,
                }],
            },
        )?;
        let run_id = RunId::from(self.ids.next_id("run"));
        self.apply_mission_command(
            &mission_id,
            MissionCommand::StartNode {
                node_id: TaskId::from("task:main"),
                run_id,
            },
        )?;
        self.drive_effects(&mission_id, prompt)?;

        let state = self.recover_mission(&mission_id)?;
        if state.status == MissionStatus::Running
            && state
                .nodes
                .values()
                .all(|node| node.status == NodeStatus::Accepted)
        {
            self.apply_mission_command(&mission_id, MissionCommand::CompleteMission {})?;
        }
        let state = self.recover_mission(&mission_id)?;
        self.publish(
            HarnessEvent::TextOutput {
                text: if state.status == MissionStatus::Completed {
                    format!("Agent 已完成：{prompt}")
                } else if state
                    .nodes
                    .values()
                    .any(|node| node.status == NodeStatus::WaitingApproval)
                {
                    format!("Agent 等待审批：{prompt}")
                } else {
                    format!("Agent 暂停：{prompt}")
                },
            },
            self.mission_scope(&mission_id),
            EventPriority::Normal,
        )?;
        Ok(plan_view(&state))
    }

    fn drive_effects(
        &mut self,
        mission_id: &MissionId,
        prompt: &str,
    ) -> Result<(), ApplicationError> {
        loop {
            let now = self.clock.now_unix_millis();
            let claimable = self
                .store
                .list_claimable_effects(now, 1)
                .map_err(storage_error)?;
            let Some(entry) = claimable.first() else {
                break;
            };
            let claimed = self
                .store
                .try_claim_effect(
                    &entry.effect_id,
                    ClaimToken::from(self.ids.next_id("claim")),
                    now,
                    now.saturating_add(30_000),
                )
                .map_err(storage_error)?
                .ok_or_else(|| {
                    ApplicationError::new("effect-claim-lost", "Effect 被其他 Runner 领取")
                })?;
            let command = match &claimed.intent {
                EffectIntent::StartAgentRun {
                    node_id, run_id, ..
                }
                | EffectIntent::ResumeAgentRun {
                    node_id, run_id, ..
                } => {
                    let is_resume = matches!(&claimed.intent, EffectIntent::ResumeAgentRun { .. });
                    let (agent_id, agent_role, cancellation, working_context) =
                        self.begin_agent_run(mission_id, node_id, run_id, is_resume)?;
                    self.publish(
                        HarnessEvent::AgentStatus {
                            agent_id: agent_id.clone(),
                            role: format!("{agent_role:?}"),
                            status: "running".to_owned(),
                            detail: node_id.to_string(),
                        },
                        self.mission_scope(mission_id),
                        EventPriority::Normal,
                    )?;
                    let output_summary = match self.execute_model(
                        prompt,
                        mission_id,
                        Some(run_id),
                        &agent_id,
                        &working_context,
                        cancellation,
                    ) {
                        Ok(output) => output,
                        Err(error) => {
                            if error.code == "tool-approval-required" {
                                let invocation_id = error
                                    .message
                                    .strip_prefix("invocation=")
                                    .map(ToolInvocationId::from)
                                    .ok_or_else(|| {
                                        ApplicationError::new(
                                            "tool-approval-continuation-invalid",
                                            error.message.clone(),
                                        )
                                    })?;
                                let runtime = self.tool_runtime.as_ref().ok_or_else(|| {
                                    ApplicationError::new(
                                        "tool-runtime-missing",
                                        "Tool Runtime 尚未注入",
                                    )
                                })?;
                                let invocation = runtime
                                    .journal()
                                    .get(&invocation_id)
                                    .map_err(tool_error)?
                                    .ok_or_else(|| {
                                        ApplicationError::new(
                                            "tool-invocation-not-found",
                                            invocation_id.to_string(),
                                        )
                                    })?;
                                let request_id =
                                    invocation.approval_request_id.clone().ok_or_else(|| {
                                        ApplicationError::new(
                                            "tool-approval-id-missing",
                                            invocation_id.to_string(),
                                        )
                                    })?;
                                let request = runtime
                                    .pending_approvals()
                                    .map_err(tool_error)?
                                    .into_iter()
                                    .find(|request| request.id == request_id)
                                    .ok_or_else(|| {
                                        ApplicationError::new(
                                            "tool-approval-request-not-found",
                                            request_id.to_string(),
                                        )
                                    })?;
                                self.apply_mission_command(
                                    mission_id,
                                    MissionCommand::RequestApproval {
                                        node_id: node_id.clone(),
                                        run_id: run_id.clone(),
                                        approval_id: ApprovalId::from(request_id.to_string()),
                                        action: serde_json::json!({
                                            "tool":invocation.tool_name,
                                            "toolInvocationId":invocation_id,
                                            "permission":invocation.permission_action,
                                            "risk":format!("{:?}", request.risk).to_lowercase()
                                        })
                                        .to_string(),
                                        reason: request.reason,
                                    },
                                )?;
                                self.set_agent_session_status(
                                    run_id,
                                    AgentSessionStatus::WaitingApproval,
                                )?;
                                self.store
                                    .complete_effect(EffectCompletion {
                                        fence: CompletionFence {
                                            effect_id: claimed.claim.effect_id,
                                            mission_epoch: claimed.claim.mission_epoch,
                                            claim_token: claimed.claim.claim_token,
                                            run_fence: claimed.claim.run_fence,
                                        },
                                        outcome: EffectOutcome::Completed,
                                        result: Some(serde_json::json!({
                                            "waitingApproval":request_id,
                                            "toolInvocationId":invocation_id
                                        })),
                                        error: None,
                                        recorded_at_millis: self.clock.now_unix_millis(),
                                    })
                                    .map_err(storage_error)?;
                                return Ok(());
                            }
                            self.store
                                .complete_effect(EffectCompletion {
                                    fence: CompletionFence {
                                        effect_id: claimed.claim.effect_id,
                                        mission_epoch: claimed.claim.mission_epoch,
                                        claim_token: claimed.claim.claim_token,
                                        run_fence: claimed.claim.run_fence,
                                    },
                                    outcome: EffectOutcome::Failed,
                                    result: None,
                                    error: Some(error.to_string()),
                                    recorded_at_millis: self.clock.now_unix_millis(),
                                })
                                .map_err(storage_error)?;
                            self.set_agent_session_status(run_id, AgentSessionStatus::Failed)?;
                            self.finish_agent_run(&agent_id, run_id, BudgetEscrowStatus::Failed)?;
                            self.apply_mission_command(
                                mission_id,
                                MissionCommand::FailNode {
                                    node_id: node_id.clone(),
                                    run_id: run_id.clone(),
                                    error: error.to_string(),
                                },
                            )?;
                            return Err(error);
                        }
                    };
                    let compressed_result = AgentResult {
                        status: "completed".to_owned(),
                        summary: output_summary.clone(),
                        artifacts: vec![],
                        changed_files: vec![],
                        evidence: vec![],
                        warnings: vec![],
                        errors: vec![],
                        metrics: AgentExecutionMetrics::default(),
                        confidence: 0.5,
                        follow_up: vec![],
                        model_tool_yield: None,
                    };
                    if let Some(store) = self.agent_state.as_ref() {
                        store
                            .save_result(run_id, &compressed_result, self.clock.now_unix_millis())
                            .map_err(agent_error)?;
                    }
                    self.set_agent_session_status(run_id, AgentSessionStatus::Submitted)?;
                    MissionCommand::SubmitNode {
                        node_id: node_id.clone(),
                        run_id: run_id.clone(),
                        output_summary,
                    }
                }
                EffectIntent::VerifyAgentRun {
                    node_id, run_id, ..
                } => {
                    let state = self.recover_mission(mission_id)?;
                    let agent_id = state
                        .nodes
                        .get(node_id)
                        .map(|node| node.agent_definition_id.clone())
                        .ok_or_else(|| {
                            ApplicationError::new("agent-node-missing", node_id.to_string())
                        })?;
                    self.set_agent_session_status(run_id, AgentSessionStatus::Completed)?;
                    self.finish_agent_run(&agent_id, run_id, BudgetEscrowStatus::Completed)?;
                    MissionCommand::AcceptNode {
                        node_id: node_id.clone(),
                        run_id: run_id.clone(),
                    }
                }
                EffectIntent::CancelAgentRun {
                    node_id,
                    run_id,
                    reason,
                    ..
                } => {
                    let state = self.recover_mission(mission_id)?;
                    let agent_id = state
                        .nodes
                        .get(node_id)
                        .map(|node| node.agent_definition_id.clone())
                        .ok_or_else(|| {
                            ApplicationError::new("agent-node-missing", node_id.to_string())
                        })?;
                    let has_control = self
                        .run_controls
                        .lock()
                        .map_err(|_| {
                            ApplicationError::new("run-controls-poisoned", run_id.to_string())
                        })?
                        .token(run_id)
                        .is_some();
                    if has_control {
                        self.run_controls
                            .lock()
                            .map_err(|_| {
                                ApplicationError::new("run-controls-poisoned", run_id.to_string())
                            })?
                            .cancel_subtree(run_id)
                            .map_err(agent_error)?;
                    }
                    self.set_agent_session_status(run_id, AgentSessionStatus::Cancelled)?;
                    self.finish_agent_run(&agent_id, run_id, BudgetEscrowStatus::Cancelled)?;
                    self.publish(
                        HarnessEvent::AgentStatus {
                            agent_id,
                            role: "Worker".to_owned(),
                            status: "cancelled".to_owned(),
                            detail: format!("{node_id}: {reason}"),
                        },
                        self.mission_scope(mission_id),
                        EventPriority::Critical,
                    )?;
                    self.store
                        .complete_effect(EffectCompletion {
                            fence: CompletionFence {
                                effect_id: claimed.claim.effect_id,
                                mission_epoch: claimed.claim.mission_epoch,
                                claim_token: claimed.claim.claim_token,
                                run_fence: claimed.claim.run_fence,
                            },
                            outcome: EffectOutcome::Completed,
                            result: Some(serde_json::json!({
                                "cancelledRunId": run_id,
                                "reason": reason
                            })),
                            error: None,
                            recorded_at_millis: self.clock.now_unix_millis(),
                        })
                        .map_err(storage_error)?;
                    continue;
                }
            };
            self.apply_mission_command(mission_id, command)?;
            self.store
                .complete_effect(EffectCompletion {
                    fence: CompletionFence {
                        effect_id: claimed.claim.effect_id,
                        mission_epoch: claimed.claim.mission_epoch,
                        claim_token: claimed.claim.claim_token,
                        run_fence: claimed.claim.run_fence,
                    },
                    outcome: EffectOutcome::Completed,
                    result: None,
                    error: None,
                    recorded_at_millis: self.clock.now_unix_millis(),
                })
                .map_err(storage_error)?;
        }
        Ok(())
    }

    fn begin_agent_run(
        &mut self,
        mission_id: &MissionId,
        node_id: &TaskId,
        run_id: &RunId,
        _is_resume: bool,
    ) -> Result<
        (
            AgentDefinitionId,
            AgentRole,
            CancellationToken,
            AgentWorkingContext,
        ),
        ApplicationError,
    > {
        let mission = self.recover_mission(mission_id)?;
        let node = mission
            .nodes
            .get(node_id)
            .ok_or_else(|| ApplicationError::new("agent-node-missing", node_id.to_string()))?;
        let agent_id = node.agent_definition_id.clone();
        let role = self
            .agent_catalog
            .as_ref()
            .and_then(|catalog| catalog.definition(&agent_id))
            .and_then(|definition| definition.roles.iter().next().copied())
            .unwrap_or(AgentRole::Coder);
        let working_context = self.agent_working_context(role)?;
        if let Some(budgets) = self.agent_budgets.as_mut() {
            budgets
                .reserve(
                    mission_id.clone(),
                    run_id.clone(),
                    &AgentBudgetRequest {
                        reserved_tokens: u64::from(working_context.token_cost)
                            .saturating_add(4_096)
                            .max(8_192),
                        reserved_tool_calls: 16,
                        reserved_runtime_millis: 2 * 60 * 1_000,
                        reserved_retries: 1,
                    },
                    &self.agent_budget_policy,
                    self.clock.now_unix_millis(),
                )
                .map_err(agent_error)?;
        }
        let existing_token = self
            .run_controls
            .lock()
            .map_err(|_| ApplicationError::new("run-controls-poisoned", run_id.to_string()))?
            .token(run_id);
        let (token, newly_registered) = if let Some(token) = existing_token {
            (token, false)
        } else {
            let token = self
                .run_controls
                .lock()
                .map_err(|_| ApplicationError::new("run-controls-poisoned", run_id.to_string()))?
                .register(run_id.clone(), None)
                .map_err(agent_error)?;
            (token, true)
        };
        if newly_registered
            && let Some(catalog) = self.agent_catalog.as_mut()
            && catalog.definition(&agent_id).is_some()
            && let Err(error) = catalog
                .reserve(&agent_id)
                .and_then(|()| catalog.start(&agent_id))
        {
            let _ = self
                .run_controls
                .lock()
                .map_err(|_| ApplicationError::new("run-controls-poisoned", run_id.to_string()))?
                .finish(run_id);
            return Err(agent_error(error));
        }
        if let Some(store) = self.agent_state.as_mut() {
            let session_id = agent_session_id(run_id);
            let now = self.clock.now_unix_millis();
            if let Some(mut session) = store.session(&session_id).map_err(agent_error)? {
                let expected_version = session.version;
                session.status = AgentSessionStatus::Running;
                session.context_fingerprint = working_context.fingerprint.clone();
                session.updated_at_millis = now;
                store
                    .update_session(expected_version, &mut session)
                    .map_err(agent_error)?;
            } else {
                let endpoint_id = mission
                    .runs
                    .get(run_id)
                    .map(|run| run.endpoint_id.clone())
                    .ok_or_else(|| {
                        ApplicationError::new("agent-run-missing", run_id.to_string())
                    })?;
                store
                    .create_session(&AgentSession {
                        id: session_id,
                        mission_id: mission_id.clone(),
                        task_id: node_id.clone(),
                        run_id: run_id.clone(),
                        parent_run_id: None,
                        endpoint_id,
                        agent_definition_id: agent_id.clone(),
                        role,
                        status: AgentSessionStatus::Running,
                        context_fingerprint: working_context.fingerprint.clone(),
                        previous_response_id: None,
                        created_at_millis: now,
                        updated_at_millis: now,
                        version: 1,
                    })
                    .map_err(agent_error)?;
            }
        }
        if newly_registered {
            self.adjust_agent_endpoint(&agent_id, true)?;
        }
        Ok((agent_id, role, token, working_context))
    }

    fn set_agent_session_status(
        &mut self,
        run_id: &RunId,
        status: AgentSessionStatus,
    ) -> Result<(), ApplicationError> {
        let Some(store) = self.agent_state.as_mut() else {
            return Ok(());
        };
        let session_id = agent_session_id(run_id);
        let Some(mut session) = store.session(&session_id).map_err(agent_error)? else {
            return Ok(());
        };
        let expected_version = session.version;
        session.status = status;
        session.updated_at_millis = self.clock.now_unix_millis();
        store
            .update_session(expected_version, &mut session)
            .map_err(agent_error)
    }

    fn set_agent_session_context_fingerprint(
        &mut self,
        run_id: &RunId,
        fingerprint: &str,
    ) -> Result<(), ApplicationError> {
        let Some(store) = self.agent_state.as_mut() else {
            return Ok(());
        };
        let session_id = agent_session_id(run_id);
        let Some(mut session) = store.session(&session_id).map_err(agent_error)? else {
            return Ok(());
        };
        let expected_version = session.version;
        session.context_fingerprint = fingerprint.to_owned();
        session.updated_at_millis = self.clock.now_unix_millis();
        store
            .update_session(expected_version, &mut session)
            .map_err(agent_error)
    }

    fn set_agent_session_previous_response(
        &mut self,
        run_id: &RunId,
        response_id: Option<ResponseId>,
    ) -> Result<(), ApplicationError> {
        let Some(store) = self.agent_state.as_mut() else {
            return Ok(());
        };
        let session_id = agent_session_id(run_id);
        let Some(mut session) = store.session(&session_id).map_err(agent_error)? else {
            return Ok(());
        };
        let expected_version = session.version;
        session.previous_response_id = response_id;
        session.updated_at_millis = self.clock.now_unix_millis();
        store
            .update_session(expected_version, &mut session)
            .map_err(agent_error)
    }

    fn finish_agent_run(
        &mut self,
        agent_id: &AgentDefinitionId,
        run_id: &RunId,
        budget_status: BudgetEscrowStatus,
    ) -> Result<(), ApplicationError> {
        let has_control = self
            .run_controls
            .lock()
            .map_err(|_| ApplicationError::new("run-controls-poisoned", run_id.to_string()))?
            .token(run_id)
            .is_some();
        if has_control {
            self.run_controls
                .lock()
                .map_err(|_| ApplicationError::new("run-controls-poisoned", run_id.to_string()))?
                .finish(run_id)
                .map_err(agent_error)?;
        }
        if let Some(catalog) = self.agent_catalog.as_mut()
            && catalog.active_count(agent_id) > 0
        {
            catalog.release(agent_id).map_err(agent_error)?;
        }
        self.adjust_agent_endpoint(agent_id, false)?;
        if let Some(budgets) = self.agent_budgets.as_ref()
            && budgets.get(run_id).map_err(agent_error)?.is_some()
        {
            budgets
                .release(run_id, budget_status)
                .map_err(agent_error)?;
        }
        Ok(())
    }

    fn charge_agent_budget(
        &mut self,
        run_id: Option<&RunId>,
        tokens: u64,
        tool_calls: u64,
    ) -> Result<(), ApplicationError> {
        let (Some(run_id), Some(budgets)) = (run_id, self.agent_budgets.as_mut()) else {
            return Ok(());
        };
        budgets
            .charge(run_id, tokens, tool_calls, self.clock.now_unix_millis())
            .map_err(agent_error)?;
        Ok(())
    }

    fn agent_model_tools(
        &self,
        agent_id: &AgentDefinitionId,
        role: AgentRole,
        query: &str,
    ) -> Result<Vec<ToolDefinition>, ApplicationError> {
        let allowed_tools = self
            .agent_catalog
            .as_ref()
            .and_then(|catalog| catalog.definition(agent_id))
            .map(|definition| &definition.allowed_tools)
            .ok_or_else(|| ApplicationError::new("agent-not-found", agent_id.to_string()))?;
        if allowed_tools.is_empty() {
            return Ok(Vec::new());
        }
        let Some(runtime) = self.tool_runtime.as_ref() else {
            return Ok(Vec::new());
        };
        let role_terms = match role {
            AgentRole::Explorer => "files read search symbols references",
            AgentRole::RequirementsAnalyst => "files read search requirements",
            AgentRole::Architect => "files read search architecture dependencies",
            AgentRole::Reviewer => "files read diff diagnostics",
            AgentRole::SecurityAuditor => "files read search diff security secrets dependencies",
            AgentRole::PerformanceEngineer => "files read process benchmark profile performance",
            AgentRole::Tester => "files read process test diagnostics",
            AgentRole::ReleaseManager => "files read process git diff package version checksum",
            AgentRole::Debugger => "files read process diagnostics reproduce",
            AgentRole::Researcher => "files read search browser documentation",
            _ => "code file read write test",
        };
        runtime
            .model_tools(
                &format!("{query} {role_terms}"),
                self.mode_profile().max_on_demand_tools,
            )
            .map_err(tool_error)
            .map(|tools| {
                tools
                    .into_iter()
                    .filter(|tool| {
                        let name = tool.canonical_name.as_str();
                        if name == "process.run" {
                            return allowed_tools.contains("process.run");
                        }
                        if name.contains("write")
                            || name.contains("apply")
                            || name.contains("patch")
                        {
                            return allowed_tools.contains("file.write");
                        }
                        if name.contains("diff") {
                            return allowed_tools.contains("diff.read")
                                || allowed_tools.contains("repository.read");
                        }
                        if name.contains("browser")
                            || name.contains("network")
                            || name.contains("http")
                        {
                            return allowed_tools.contains("network.read");
                        }
                        if name.contains("lsp") {
                            return allowed_tools.contains("lsp.read")
                                || allowed_tools.contains("repository.read");
                        }
                        let repository_read = name.contains("read")
                            || name.contains("search")
                            || name.contains("status")
                            || name.contains("symbol")
                            || name.contains("definition")
                            || name.contains("reference")
                            || name.contains("diagnostic")
                            || name.contains("snapshot");
                        repository_read && allowed_tools.contains("repository.read")
                    })
                    .map(|tool| ToolDefinition {
                        name: tool.canonical_name,
                        description: tool.description,
                        input_schema: tool.input_schema,
                        strict: true,
                    })
                    .collect()
            })
    }

    fn adjust_agent_endpoint(
        &mut self,
        agent_id: &AgentDefinitionId,
        starting: bool,
    ) -> Result<(), ApplicationError> {
        let Some(store) = self.agent_state.as_mut() else {
            return Ok(());
        };
        let endpoint_id = AgentEndpointId::from(format!("endpoint:{agent_id}"));
        let Some(mut endpoint) = store.endpoint(&endpoint_id).map_err(agent_error)? else {
            return Ok(());
        };
        let expected_version = endpoint.version;
        endpoint.active_runs = if starting {
            endpoint.active_runs.saturating_add(1)
        } else {
            endpoint.active_runs.saturating_sub(1)
        };
        endpoint.status = if endpoint.active_runs == 0 {
            AgentEndpointStatus::Idle
        } else {
            AgentEndpointStatus::Busy
        };
        endpoint.last_heartbeat_millis = self.clock.now_unix_millis();
        store
            .update_endpoint(expected_version, &mut endpoint)
            .map_err(agent_error)
    }

    fn execute_model(
        &mut self,
        prompt: &str,
        mission_id: &MissionId,
        run_id: Option<&RunId>,
        agent_id: &AgentDefinitionId,
        working_context: &AgentWorkingContext,
        cancellation: CancellationToken,
    ) -> Result<String, ApplicationError> {
        let retrieved_project_context = self.retrieve_project_context(prompt);
        let current_view = match self.model_runtime.as_ref() {
            Some(runtime) => runtime
                .view()
                .map_err(|error| ApplicationError::new(error.code, error.message))?,
            None => return Ok("deterministic fake result".to_owned()),
        };
        let pending = run_id.map_or(Ok(None), |run_id| {
            self.take_pending_model_continuation(mission_id, run_id)
        })?;
        let mut state = if let Some(pending) = pending {
            if pending.state.view.provider_id != current_view.provider_id
                || pending.state.view.model_id != current_view.model_id
            {
                self.store_pending_model_continuation(pending)?;
                return Err(ApplicationError::new(
                    "model-continuation-selection-changed",
                    "审批等待期间 Provider/Model 已变化；请切回原模型后再继续",
                ));
            }
            let mut state = pending.state;
            self.process_tool_batch(mission_id, run_id, agent_id, &mut state, pending.batch)?;
            state
        } else {
            let mut canonical_context = [
                working_context.stable_instructions.as_str(),
                working_context.dynamic_context.as_str(),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            canonical_context.push_str(&retrieved_project_context);
            let tools = self
                .tool_runtime
                .as_ref()
                .map_or(Ok(Vec::new()), |runtime| {
                    runtime.model_tools(prompt, self.mode_profile().max_on_demand_tools)
                })
                .map_err(tool_error)?
                .into_iter()
                .map(|tool| ToolDefinition {
                    name: tool.canonical_name,
                    description: tool.description,
                    input_schema: tool.input_schema,
                    strict: true,
                })
                .collect();
            let transcript = vec![ModelInputItem::Message {
                role: ModelMessageRole::User,
                content: prompt.to_owned(),
            }];
            ModelLoopState {
                view: current_view,
                canonical_context,
                tools,
                next_input: transcript.clone(),
                transcript,
                previous_response_id: None,
                output: String::new(),
                next_turn: 0,
            }
        };
        while state.next_turn < 8 {
            let _profile_span = self.profile_span("model");
            if cancellation.is_cancelled() {
                return Err(ApplicationError::new(
                    "agent-run-cancelled",
                    run_id.map_or_else(|| "unknown".to_owned(), ToString::to_string),
                ));
            }
            for instruction in self.take_steering_messages(mission_id)? {
                let input = ModelInputItem::Message {
                    role: ModelMessageRole::User,
                    content: format!("[Steering]\n{instruction}"),
                };
                state.next_input.push(input.clone());
                if !state.view.capability.conversation_continuation {
                    state.transcript.push(input);
                }
                self.publish(
                    HarnessEvent::ReasoningSummary {
                        agent_id: agent_id.clone(),
                        summary: format!("已接收 Steering：{instruction}"),
                    },
                    self.mission_scope(mission_id),
                    EventPriority::Normal,
                )?;
            }
            let turn = state.next_turn;
            let request = ModelRequest {
                model_id: state.view.model_id.clone(),
                instructions: state.canonical_context.clone(),
                input: state.next_input.clone(),
                tools: state.tools.clone(),
                reasoning: state.view.reasoning_requested,
                response_format: ResponseFormat::Text,
                max_output_tokens: state.view.capability.max_output_tokens.min(1_024),
                previous_response_id: state.previous_response_id.clone(),
                store: false,
                timeout: Duration::from_secs(120),
            };
            let mut turn_output = String::new();
            let mut response_id = None;
            let mut tool_calls = Vec::<(ToolCallId, String, serde_json::Value)>::new();
            let mut completed = false;
            let stream = self
                .model_runtime
                .as_ref()
                .expect("Model Runtime 已检查")
                .stream(request, cancellation.clone())
                .map_err(|error| ApplicationError::new(error.code, error.message))?;
            for event in stream {
                let event =
                    event.map_err(|error| ApplicationError::new(error.code, error.message))?;
                match event {
                    ModelEvent::Started {
                        response_id: started_id,
                        ..
                    } => response_id = Some(started_id),
                    ModelEvent::TextDelta { delta } => {
                        state.output.push_str(&delta);
                        turn_output.push_str(&delta);
                        self.publish(
                            HarnessEvent::TextOutput { text: delta },
                            self.mission_scope(mission_id),
                            EventPriority::Delta,
                        )?;
                    }
                    ModelEvent::ReasoningSummaryDelta { delta } => {
                        self.publish(
                            HarnessEvent::ReasoningSummary {
                                agent_id: agent_id.clone(),
                                summary: delta,
                            },
                            self.mission_scope(mission_id),
                            EventPriority::Delta,
                        )?;
                    }
                    ModelEvent::ToolCall {
                        call_id,
                        name,
                        arguments,
                    } => {
                        self.charge_agent_budget(run_id, 0, 1)?;
                        tool_calls.push((call_id, name, arguments));
                    }
                    ModelEvent::Usage { usage } => {
                        self.charge_agent_budget(run_id, usage.total_tokens, 0)?;
                        self.publish(
                            HarnessEvent::ModelUsage {
                                input_tokens: usage.input_tokens,
                                cached_input_tokens: usage.cached_input_tokens,
                                cache_write_tokens: usage.cache_write_tokens,
                                output_tokens: usage.output_tokens,
                                reasoning_tokens: usage.reasoning_tokens,
                                total_tokens: usage.total_tokens,
                            },
                            self.mission_scope(mission_id),
                            EventPriority::Normal,
                        )?;
                    }
                    ModelEvent::Completed {
                        status: CompletionStatus::Completed,
                        ..
                    } => completed = true,
                    ModelEvent::Completed {
                        status: CompletionStatus::Incomplete,
                        incomplete_reason,
                    } => {
                        return Err(ApplicationError::new(
                            "model-response-incomplete",
                            incomplete_reason.unwrap_or_else(|| "unknown".to_owned()),
                        ));
                    }
                }
            }
            if !completed {
                return Err(ApplicationError::new(
                    "model-stream-no-completion",
                    "Model stream 没有 terminal completion",
                ));
            }
            state.next_turn = turn.saturating_add(1);
            if tool_calls.is_empty() {
                if state.output.trim().is_empty() {
                    return Err(ApplicationError::new(
                        "model-output-empty",
                        "Model 没有产生文本输出",
                    ));
                }
                return Ok(state.output);
            }
            if !state.view.capability.conversation_continuation && !turn_output.is_empty() {
                state.transcript.push(ModelInputItem::Message {
                    role: ModelMessageRole::Assistant,
                    content: turn_output,
                });
            }
            let batch = PendingToolBatch {
                response_id,
                results: vec![],
                calls: tool_calls
                    .into_iter()
                    .map(|(call_id, name, arguments)| (call_id, name, arguments, None))
                    .collect(),
            };
            self.process_tool_batch(mission_id, run_id, agent_id, &mut state, batch)?;
            if turn == 7 {
                return Err(ApplicationError::new(
                    "model-tool-loop-limit",
                    "Model Tool loop 超过 8 轮",
                ));
            }
        }
        Err(ApplicationError::new(
            "model-tool-loop-exhausted",
            "Model Tool loop exhausted",
        ))
    }

    fn take_steering_messages(
        &mut self,
        mission_id: &MissionId,
    ) -> Result<Vec<String>, ApplicationError> {
        let delivery_token = self.ids.next_id("steering-delivery");
        let now = self.clock.now_unix_millis();
        let Some(bus) = self.agent_messages.as_mut() else {
            return Ok(Vec::new());
        };
        let claimed = bus
            .claim_for_mission(
                "kernel:supervisor:steering",
                mission_id,
                0,
                100,
                &delivery_token,
                now,
                30_000,
            )
            .map_err(agent_error)?;
        let mut instructions = Vec::new();
        for claimed_message in claimed {
            if claimed_message.message.kind == AgentMessageKind::Steering
                && let Some(instruction) = claimed_message
                    .message
                    .payload
                    .get("instruction")
                    .and_then(serde_json::Value::as_str)
                && !instruction.trim().is_empty()
            {
                instructions.push(instruction.trim().to_owned());
            }
            if !bus
                .acknowledge_claim(&claimed_message.message.id, &delivery_token, now)
                .map_err(agent_error)?
            {
                return Err(ApplicationError::new(
                    "steering-ack-lost",
                    claimed_message.message.id,
                ));
            }
        }
        Ok(instructions)
    }

    fn retrieve_project_context(&mut self, query: &str) -> String {
        let started = Instant::now();
        let mut output = String::new();
        let settings = &self.effective_config.settings;
        let profile = &self.mode_profile;
        let retrieval_mode = match settings.vector_mode {
            VectorMode::Off => RetrievalMode::Lexical,
            VectorMode::On => RetrievalMode::Semantic,
            VectorMode::Auto if profile.proactive_semantic_retrieval => RetrievalMode::Auto,
            VectorMode::Auto => RetrievalMode::Lexical,
        };
        let mut vector_executed = false;
        if let Some(memory) = self.memory.as_mut()
            && let Ok(response) = memory.search(query, retrieval_mode, 6)
        {
            vector_executed = matches!(
                response.executed_mode,
                harness_memory::ExecutedRetrievalMode::Semantic
                    | harness_memory::ExecutedRetrievalMode::Hybrid
            );
            if !response.results.is_empty() {
                output.push_str("\n\n[Retrieved Project Memory]\n");
                for result in response.results {
                    output.push_str(&format!(
                        "- {}: {}\n",
                        result.record.title,
                        result.record.content.chars().take(1200).collect::<String>()
                    ));
                }
            }
        }
        if let Some(repository) = self.repository.as_ref()
            && let Ok(results) = repository.search(query, 8)
            && !results.is_empty()
        {
            output.push_str("\n[Retrieved Repository Context]\n");
            for result in results {
                output.push_str(&format!(
                    "- {} [{}] {} symbols={} diagnostics={} matched={}\n",
                    result.path,
                    result.language,
                    result.summary.chars().take(800).collect::<String>(),
                    result.symbols.join(","),
                    result.diagnostics.join(" | "),
                    result.matched_by
                ));
            }
        }
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.record_profile_sample("retrieval", elapsed);
        if vector_executed {
            self.record_profile_sample("vector", elapsed);
        }
        output
    }

    /// LSP Tool 的完整 JSON 只回到当前 continuation；Context 保存稳定、限长的事实摘要。
    fn record_lsp_tool_fact(
        &mut self,
        tool_name: &str,
        arguments: &serde_json::Value,
        result: &serde_json::Value,
        run_id: Option<&RunId>,
    ) -> Result<(), ApplicationError> {
        if !tool_name.starts_with("lsp.") {
            return Ok(());
        }
        let server_id = arguments
            .get("serverId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let path = result
            .get("path")
            .and_then(serde_json::Value::as_str)
            .or_else(|| arguments.get("path").and_then(serde_json::Value::as_str))
            .unwrap_or("unknown");
        let count = result
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let returned = result
            .get("returned")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let truncated = result
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let projection_note = if let (Some(repository), Some(facts)) = (
            self.repository.as_mut(),
            result.get("facts").and_then(serde_json::Value::as_array),
        ) {
            match repository.ingest_lsp_facts(LspFactBatch {
                tool_name,
                server_id,
                path: Path::new(path),
                facts,
                expected_file_hash: result.get("fileHash").and_then(serde_json::Value::as_str),
                run_id: run_id.map(RunId::as_str),
                observed_at_millis: self.clock.now_unix_millis(),
            }) {
                Ok(Some(report)) => format!(
                    " · repositoryProjection={} added={} removed={}",
                    report.kind, report.added, report.removed
                ),
                Ok(None) => String::new(),
                Err(error) => format!(" · repositoryProjection=isolated({})", error.code),
            }
        } else {
            String::new()
        };
        let mut content = format!(
            "LSP fact [UNTRUSTED EXTERNAL PROCESS OUTPUT] · tool={tool_name} · server={server_id} · path={path} · count={count} · returned={returned} · truncated={truncated}{projection_note}\n"
        );
        if let Some(facts) = result.get("facts").and_then(serde_json::Value::as_array) {
            for fact in facts.iter().take(32) {
                let line = compact_lsp_fact(tool_name, fact);
                if content.len().saturating_add(line.len()).saturating_add(3) > 8 * 1024 {
                    content.push_str("- [context-summary-truncated]\n");
                    break;
                }
                content.push_str("- ");
                content.push_str(&line);
                content.push('\n');
            }
        }
        let identity_hash = format!(
            "{:x}",
            Sha256::digest(format!("{tool_name}\n{server_id}\n{path}").as_bytes())
        );
        let identity = format!("lsp-fact:{}", &identity_hash[..24]);
        let mut item = self.new_context_item(
            ContextKind::Repository,
            Priority::High,
            &identity,
            &content,
            false,
        );
        item.information_flow.integrity = IntegrityLabel::Untrusted;
        self.replace_context_source(&identity, Some(item))
    }

    fn process_tool_batch(
        &mut self,
        mission_id: &MissionId,
        run_id: Option<&RunId>,
        agent_id: &AgentDefinitionId,
        state: &mut ModelLoopState,
        mut batch: PendingToolBatch,
    ) -> Result<(), ApplicationError> {
        let tool_runtime = self.tool_runtime.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new(
                "tool-runtime-missing",
                "Model 请求了 Tool，但 Runtime 未注入",
            )
        })?;
        while let Some((call_id, name, arguments, existing_id)) = batch.calls.pop_front() {
            let write_lease = self.acquire_agent_write_lease(&name, &arguments, run_id)?;
            let invocation_result = (|| -> Result<ToolInvocationRecord, ApplicationError> {
                if let Some(existing_id) = existing_id {
                    tool_runtime
                        .journal()
                        .get(&existing_id)
                        .map_err(tool_error)?
                        .ok_or_else(|| {
                            ApplicationError::new(
                                "tool-invocation-not-found",
                                existing_id.to_string(),
                            )
                        })
                } else {
                    tool_runtime
                        .invoke(ToolInvokeRequest {
                            invocation_id: ToolInvocationId::from(
                                self.ids.next_id("tool-invocation"),
                            ),
                            approval_request_id: PermissionRequestId::from(
                                self.ids.next_id("approval"),
                            ),
                            idempotency_key: format!("{mission_id}:{call_id}"),
                            envelope: ExecutionEnvelope {
                                project_id: self.project_id.clone(),
                                mission_id: mission_id.clone(),
                                run_id: run_id.cloned(),
                                actor_id: ActorId::from(agent_id.to_string()),
                                origin: InvocationOrigin::Agent,
                                information_flow: InformationFlowLabel {
                                    integrity: IntegrityLabel::Trusted,
                                    confidentiality: ConfidentialityLabel::ProjectPrivate,
                                },
                            },
                            tool_name: name.clone(),
                            args: arguments.clone(),
                            now_millis: self.clock.now_unix_millis(),
                        })
                        .map(|response| response.invocation)
                        .map_err(tool_error)
                }
            })();
            let release_result = self.release_agent_write_lease(write_lease);
            let invocation = invocation_result?;
            release_result?;
            self.publish(
                HarnessEvent::ToolStatus {
                    tool: name.clone(),
                    status: format!("{:?}", invocation.status).to_lowercase(),
                    summary: invocation.id.to_string(),
                },
                self.mission_scope(mission_id),
                EventPriority::Normal,
            )?;
            match invocation.status {
                ToolInvocationStatus::Completed => {
                    let result = invocation.result.ok_or_else(|| {
                        ApplicationError::new("tool-result-missing", name.clone())
                    })?;
                    self.record_lsp_tool_fact(&name, &arguments, &result, run_id)?;
                    let tool_call = ModelInputItem::ToolCall {
                        call_id: call_id.clone(),
                        name,
                        arguments,
                    };
                    let tool_result = ModelInputItem::ToolResult {
                        call_id,
                        output: result,
                    };
                    if state.view.capability.conversation_continuation {
                        batch.results.push(tool_result);
                    } else {
                        state.transcript.push(tool_call);
                        state.transcript.push(tool_result);
                    }
                }
                ToolInvocationStatus::WaitingApproval => {
                    let run_id = run_id.cloned().ok_or_else(|| {
                        ApplicationError::new(
                            "tool-approval-run-missing",
                            invocation.id.to_string(),
                        )
                    })?;
                    invocation.approval_request_id.clone().ok_or_else(|| {
                        ApplicationError::new("tool-approval-id-missing", invocation.id.to_string())
                    })?;
                    batch
                        .calls
                        .push_front((call_id, name, arguments, Some(invocation.id.clone())));
                    self.pending_model_continuations
                        .lock()
                        .map_err(|_| {
                            ApplicationError::new(
                                "model-continuation-poisoned",
                                "pending continuation lock",
                            )
                        })?
                        .insert(
                            invocation.id.clone(),
                            PendingModelContinuation {
                                mission_id: mission_id.clone(),
                                run_id,
                                state: state.clone(),
                                batch,
                            },
                        );
                    return Err(ApplicationError::new(
                        "tool-approval-required",
                        format!("invocation={}", invocation.id),
                    ));
                }
                status => {
                    return Err(ApplicationError::new(
                        "tool-invocation-not-completed",
                        format!("tool={name}, status={status:?}"),
                    ));
                }
            }
        }
        if state.view.capability.conversation_continuation {
            state.previous_response_id = batch.response_id;
            state.next_input = batch.results;
        } else {
            state.previous_response_id = None;
            state.next_input.clone_from(&state.transcript);
        }
        Ok(())
    }

    fn apply_agent_tool_yield(
        &mut self,
        request: &mut AgentExecutionRequest,
        tool_yield: AgentModelToolYield,
    ) -> Result<Option<WaitingParallelTool>, ApplicationError> {
        let mut continuation = tool_yield.continuation;
        if continuation.conversation_continuation {
            continuation.previous_response_id = tool_yield.response_id;
            continuation.next_input.clear();
        }
        self.execute_parallel_tool_calls(request, continuation, tool_yield.calls)
    }

    fn execute_parallel_tool_calls(
        &mut self,
        request: &mut AgentExecutionRequest,
        mut continuation: AgentModelContinuation,
        calls: Vec<AgentToolCall>,
    ) -> Result<Option<WaitingParallelTool>, ApplicationError> {
        let tool_runtime = self.tool_runtime.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new("tool-runtime-missing", "后台 Agent 请求了 Tool")
        })?;
        for (index, call) in calls.iter().cloned().enumerate() {
            let write_lease = self.acquire_agent_write_lease(
                &call.name,
                &call.arguments,
                Some(&request.contract.run_id),
            )?;
            let invocation = tool_runtime.invoke(ToolInvokeRequest {
                invocation_id: ToolInvocationId::from(self.ids.next_id("tool-invocation")),
                approval_request_id: PermissionRequestId::from(self.ids.next_id("approval")),
                idempotency_key: format!(
                    "{}:{}:{}",
                    request.contract.mission_id, request.contract.run_id, call.call_id
                ),
                envelope: ExecutionEnvelope {
                    project_id: self.project_id.clone(),
                    mission_id: request.contract.mission_id.clone(),
                    run_id: Some(request.contract.run_id.clone()),
                    actor_id: ActorId::from(request.contract.agent_definition_id.to_string()),
                    origin: InvocationOrigin::Agent,
                    information_flow: InformationFlowLabel {
                        integrity: IntegrityLabel::Trusted,
                        confidentiality: ConfidentialityLabel::ProjectPrivate,
                    },
                },
                tool_name: call.name.clone(),
                args: call.arguments.clone(),
                now_millis: self.clock.now_unix_millis(),
            });
            self.release_agent_write_lease(write_lease)?;
            let invocation = invocation.map_err(tool_error)?.invocation;
            self.publish(
                HarnessEvent::ToolStatus {
                    tool: call.name.clone(),
                    status: format!("{:?}", invocation.status).to_lowercase(),
                    summary: invocation.id.to_string(),
                },
                self.mission_scope(&request.contract.mission_id),
                EventPriority::Normal,
            )?;
            match invocation.status {
                ToolInvocationStatus::Completed => {
                    let output = invocation.result.ok_or_else(|| {
                        ApplicationError::new("tool-result-missing", call.name.clone())
                    })?;
                    self.record_lsp_tool_fact(
                        &call.name,
                        &call.arguments,
                        &output,
                        Some(&request.contract.run_id),
                    )?;
                    append_parallel_tool_result(&mut continuation, &call, output);
                }
                ToolInvocationStatus::WaitingApproval => {
                    return Ok(Some(WaitingParallelTool {
                        invocation,
                        continuation,
                        waiting_call: call,
                        remaining_calls: calls[index + 1..].to_vec(),
                    }));
                }
                status => {
                    return Err(ApplicationError::new(
                        "parallel-tool-failed",
                        format!("tool={}, status={status:?}", call.name),
                    ));
                }
            }
        }
        if !continuation.conversation_continuation {
            continuation.previous_response_id = None;
            continuation.next_input.clone_from(&continuation.transcript);
        }
        request.model_continuation = Some(continuation.clone());
        request.steering_messages.clear();
        self.set_agent_session_previous_response(
            &request.contract.run_id,
            continuation.previous_response_id,
        )?;
        Ok(None)
    }

    fn claim_agent_resume_effect(
        &self,
        mission_id: &MissionId,
        run_id: &RunId,
    ) -> Result<ClaimedEffect, ApplicationError> {
        let now = self.clock.now_unix_millis();
        let effect = self
            .store
            .list_claimable_effects(now, 256)
            .map_err(storage_error)?
            .into_iter()
            .find(|entry| {
                entry.mission_id == *mission_id
                    && matches!(
                        &entry.intent,
                        EffectIntent::ResumeAgentRun { run_id: candidate, .. }
                            if candidate == run_id
                    )
            })
            .ok_or_else(|| {
                ApplicationError::new("agent-resume-effect-missing", run_id.to_string())
            })?;
        self.store
            .try_claim_effect(
                &effect.effect_id,
                ClaimToken::from(self.ids.next_id("claim")),
                now,
                now.saturating_add(30_000),
            )
            .map_err(storage_error)?
            .ok_or_else(|| {
                ApplicationError::new("agent-resume-effect-claim-lost", run_id.to_string())
            })
    }

    fn build_parallel_resume_job(
        &mut self,
        mut request: AgentExecutionRequest,
        claimed: ClaimedEffect,
        prompt: String,
    ) -> Result<PreparedAgentTeam, ApplicationError> {
        let steering_mission_id = request.contract.mission_id.clone();
        let steering = self.take_steering_messages(&steering_mission_id)?;
        request.steering_messages.extend(steering);
        let runtime = self.model_runtime.as_ref().cloned().ok_or_else(|| {
            ApplicationError::new(
                "model-runtime-missing",
                "Tool continuation 需要 Model Runtime",
            )
        })?;
        let model_handler = Arc::new(
            ModelAgentHandler::new(Arc::new(runtime), Duration::from_secs(120))
                .map_err(agent_error)?,
        );
        let live_steering = SharedSteeringBuffer::default();
        let handler = Arc::new(SteeringAgentHandler::new(
            model_handler,
            live_steering.clone(),
        ));
        let executor = BoundedAgentExecutor::new(handler, 1).map_err(agent_error)?;
        let cancellation = self
            .run_controls
            .lock()
            .map_err(|_| {
                ApplicationError::new("run-controls-poisoned", request.contract.run_id.to_string())
            })?
            .token(&request.contract.run_id)
            .ok_or_else(|| {
                ApplicationError::new("run-control-missing", request.contract.run_id.to_string())
            })?;
        let run_id = request.contract.run_id.clone();
        let mission_id = request.contract.mission_id.clone();
        Ok(PreparedAgentTeam {
            job: AgentTeamModelJob {
                executor,
                dispatches: vec![AgentDispatch {
                    request: request.clone(),
                    cancellation,
                }],
                started_at_millis: self.clock.now_unix_millis(),
                steering: live_steering,
            },
            continuation: AgentTeamContinuation {
                mission_id,
                prompt,
                claimed_by_run: [(run_id.clone(), claimed)].into_iter().collect(),
                requests_by_run: [(run_id, request)].into_iter().collect(),
            },
        })
    }

    fn acquire_agent_write_lease(
        &mut self,
        tool_name: &str,
        arguments: &serde_json::Value,
        run_id: Option<&RunId>,
    ) -> Result<Option<FileLease>, ApplicationError> {
        if tool_name != "files.write" {
            return Ok(None);
        }
        let run_id = run_id.ok_or_else(|| {
            ApplicationError::new("file-lease-run-missing", "Agent 写入必须绑定 Run")
        })?;
        let path = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(std::path::Path::new)
            .ok_or_else(|| ApplicationError::new("file-lease-path-missing", tool_name))?;
        self.file_leases
            .as_mut()
            .ok_or_else(|| {
                ApplicationError::new("file-lease-runtime-missing", "Agent 写入禁止绕过 FileLease")
            })?
            .acquire(path, run_id.clone(), self.clock.now_unix_millis(), 120_000)
            .map(Some)
            .map_err(agent_error)
    }

    fn release_agent_write_lease(&self, lease: Option<FileLease>) -> Result<(), ApplicationError> {
        let Some(lease) = lease else {
            return Ok(());
        };
        let released = self
            .file_leases
            .as_ref()
            .ok_or_else(|| {
                ApplicationError::new("file-lease-runtime-missing", "FileLease Runtime 丢失")
            })?
            .release(&lease)
            .map_err(agent_error)?;
        if !released {
            return Err(ApplicationError::new(
                "file-lease-stale-release",
                lease.path.display().to_string(),
            ));
        }
        Ok(())
    }

    fn take_pending_model_continuation(
        &self,
        mission_id: &MissionId,
        run_id: &RunId,
    ) -> Result<Option<PendingModelContinuation>, ApplicationError> {
        let mut pending = self.pending_model_continuations.lock().map_err(|_| {
            ApplicationError::new("model-continuation-poisoned", "pending continuation lock")
        })?;
        let key = pending
            .iter()
            .find(|(_, continuation)| {
                &continuation.mission_id == mission_id && &continuation.run_id == run_id
            })
            .map(|(id, _)| id.clone());
        Ok(key.and_then(|id| pending.remove(&id)))
    }

    fn store_pending_model_continuation(
        &self,
        continuation: PendingModelContinuation,
    ) -> Result<(), ApplicationError> {
        let invocation_id = continuation
            .batch
            .calls
            .front()
            .and_then(|(_, _, _, invocation_id)| invocation_id.clone())
            .ok_or_else(|| {
                ApplicationError::new(
                    "model-continuation-invalid",
                    "等待审批的 Tool Invocation 缺失",
                )
            })?;
        self.pending_model_continuations
            .lock()
            .map_err(|_| {
                ApplicationError::new("model-continuation-poisoned", "pending continuation lock")
            })?
            .insert(invocation_id, continuation);
        Ok(())
    }

    fn reconcile_model_session(&mut self) -> Result<(), ApplicationError> {
        if self.model_runtime.is_none() {
            return Ok(());
        }
        let state = self.recover_session()?;
        let legacy_fake_selection = state.model.provider_id.as_ref().is_some_and(|provider| {
            provider.as_str() == "fake"
                && state
                    .model
                    .model_id
                    .as_ref()
                    .is_some_and(|model| model.as_str() == "deterministic")
        });
        let legacy_fake_is_registered = self
            .model_runtime
            .as_ref()
            .expect("runtime 已检查")
            .models()
            .iter()
            .any(|model| {
                model.provider_id.as_str() == "fake" && model.model_id.as_str() == "deterministic"
            });
        if legacy_fake_selection && !legacy_fake_is_registered {
            // 0.1 早期发布错误地把 deterministic 测试模型持久化为用户选择。
            // 新发布版将它迁移为 Runtime 当前的显式“未配置”占位；只追加
            // Session Event，不删除 Goal、Context 或其他历史。
            let view = self
                .model_runtime
                .as_ref()
                .expect("runtime 已检查")
                .view()
                .map_err(|error| ApplicationError::new(error.code, error.message))?;
            self.apply_session_command(SessionCommand::SelectModel {
                provider_id: view.provider_id.clone(),
                model_id: view.model_id.clone(),
            })?;
            if state.model.reasoning != view.reasoning_requested {
                self.apply_session_command(SessionCommand::SetReasoning {
                    reasoning: view.reasoning_requested,
                })?;
            }
            return self.publish_model_changed(&view);
        }
        let runtime = self.model_runtime.as_mut().expect("runtime 已检查");
        match (
            state.model.provider_id.clone(),
            state.model.model_id.clone(),
        ) {
            (Some(provider_id), Some(model_id)) => {
                runtime
                    .select(provider_id, model_id)
                    .map_err(|error| ApplicationError::new(error.code, error.message))?;
                runtime
                    .set_reasoning(state.model.reasoning)
                    .map_err(|error| ApplicationError::new(error.code, error.message))?;
            }
            (None, None) => {
                let view = runtime
                    .view()
                    .map_err(|error| ApplicationError::new(error.code, error.message))?;
                self.apply_session_command(SessionCommand::SelectModel {
                    provider_id: view.provider_id.clone(),
                    model_id: view.model_id.clone(),
                })?;
                if view.reasoning_requested != ReasoningLevel::Off {
                    self.apply_session_command(SessionCommand::SetReasoning {
                        reasoning: view.reasoning_requested,
                    })?;
                }
            }
            _ => {
                return Err(ApplicationError::new(
                    "session-model-lineage-incomplete",
                    "Session 的 provider/model 选择不完整",
                ));
            }
        }
        let view = self
            .model_runtime
            .as_ref()
            .expect("runtime 已检查")
            .view()
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        self.publish_model_changed(&view)
    }

    fn reconcile_agent_runtime(&mut self) -> Result<(), ApplicationError> {
        let Some(catalog) = self.agent_catalog.as_ref() else {
            return Ok(());
        };
        let Some(store) = self.agent_state.as_mut() else {
            return Ok(());
        };
        let definitions = catalog.list();
        let recoverable_sessions = store.recoverable_sessions().map_err(agent_error)?;
        if self.active_mission_id.is_none()
            && let Some(session) = recoverable_sessions.last()
        {
            self.active_mission_id = Some(session.mission_id.clone());
        }
        let recoverable_agents = recoverable_sessions
            .into_iter()
            .map(|session| session.agent_definition_id)
            .collect::<BTreeSet<_>>();
        let now = self.clock.now_unix_millis();
        for definition in definitions {
            let endpoint_id = AgentEndpointId::from(format!("endpoint:{}", definition.id));
            let status = if recoverable_agents.contains(&definition.id) {
                AgentEndpointStatus::Offline
            } else {
                AgentEndpointStatus::Idle
            };
            if let Some(mut endpoint) = store.endpoint(&endpoint_id).map_err(agent_error)? {
                let expected_version = endpoint.version;
                endpoint.instance_id = AgentInstanceId::from(self.ids.next_id("agent-instance"));
                endpoint.status = status;
                endpoint.generation = endpoint.generation.saturating_add(1);
                endpoint.active_runs = 0;
                endpoint.max_concurrency = definition.max_concurrency;
                endpoint.last_heartbeat_millis = now;
                store
                    .update_endpoint(expected_version, &mut endpoint)
                    .map_err(agent_error)?;
            } else {
                store
                    .create_endpoint(&AgentEndpoint {
                        id: endpoint_id,
                        definition_id: definition.id,
                        instance_id: AgentInstanceId::from(self.ids.next_id("agent-instance")),
                        status,
                        generation: 1,
                        active_runs: 0,
                        max_concurrency: definition.max_concurrency,
                        last_heartbeat_millis: now,
                        version: 1,
                    })
                    .map_err(agent_error)?;
            }
        }
        Ok(())
    }

    fn publish_model_changed(&self, view: &ModelRuntimeView) -> Result<(), ApplicationError> {
        self.publish(
            HarnessEvent::ModelChanged {
                provider: view.provider_id.to_string(),
                model: view.model_id.to_string(),
                reasoning_requested: reasoning_name(view.reasoning_requested).to_owned(),
                reasoning_effective: view
                    .reasoning_effective
                    .map(reasoning_name)
                    .map(str::to_owned),
                reasoning_mapping: reasoning_mapping_name(view.reasoning_mapping).to_owned(),
            },
            self.session_scope(),
            EventPriority::Normal,
        )
    }

    fn ensure_context_series(&self) -> Result<(), ApplicationError> {
        if self
            .store
            .load_active_context_series(&self.session_id)
            .map_err(context_storage_error)?
            .is_some()
        {
            return Ok(());
        }
        let session = self.recover_session()?;
        let initial = match (
            session.parent_session_id.as_ref(),
            session.forked_from_checkpoint_id.as_ref(),
        ) {
            (Some(parent_session_id), Some(checkpoint_id)) => {
                let checkpoint = self
                    .store
                    .load_context_checkpoint(parent_session_id, checkpoint_id)
                    .map_err(context_storage_error)?
                    .ok_or_else(|| {
                        ApplicationError::new(
                            "fork-checkpoint-not-found",
                            format!("无法修复 Child Context：{checkpoint_id}"),
                        )
                    })?;
                let source = self
                    .store
                    .load_context_series(&checkpoint.context_series_id)
                    .map_err(context_storage_error)?
                    .ok_or_else(|| {
                        ApplicationError::new(
                            "fork-series-not-found",
                            checkpoint.context_series_id.to_string(),
                        )
                    })?;
                fork_context_series(
                    &source,
                    &checkpoint,
                    self.session_id.clone(),
                    ContextSeriesId::from(self.ids.next_id("series")),
                    self.clock.now_unix_millis(),
                )
                .map_err(context_storage_error)?
            }
            (None, None) => ContextSeries::initial(
                ContextSeriesId::from(self.ids.next_id("series")),
                self.session_id.clone(),
                self.clock.now_unix_millis(),
            ),
            _ => {
                return Err(ApplicationError::new(
                    "incomplete-session-lineage",
                    "Session fork lineage 不完整，拒绝创建错误的空 Context",
                ));
            }
        };
        self.store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: None,
                next_series: initial,
                compaction_record: None,
            })
            .map_err(context_storage_error)
    }

    fn active_context_series(&self) -> Result<ContextSeries, ApplicationError> {
        self.store
            .load_active_context_series(&self.session_id)
            .map_err(context_storage_error)?
            .ok_or_else(|| {
                ApplicationError::new(
                    "context-series-missing",
                    "Session 尚未建立活动 Context Series",
                )
            })
    }

    fn reconcile_goal_context(&mut self) -> Result<(), ApplicationError> {
        let state = self.recover_session()?;
        let Some(revision) = state
            .goal
            .current_revision_id
            .as_ref()
            .and_then(|id| state.goal.revisions.get(id))
        else {
            return Ok(());
        };
        let active = self.active_context_series()?;
        if active.items.iter().any(|item| {
            item.context.source_identity == "goal:active" && item.context.content == revision.text
        }) {
            return Ok(());
        }
        self.replace_goal_context(revision)
    }

    fn replace_goal_context(&mut self, revision: &GoalRevision) -> Result<(), ApplicationError> {
        let item = self.new_context_item(
            ContextKind::Goal,
            Priority::Critical,
            "goal:active",
            &revision.text,
            true,
        );
        self.replace_context_source("goal:active", Some(item))
    }

    fn append_context_item(
        &mut self,
        kind: ContextKind,
        priority: Priority,
        source_identity: &str,
        content: &str,
        hard_required: bool,
    ) -> Result<(), ApplicationError> {
        let current = self.active_context_series()?;
        let mut items = current.items.clone();
        let mut context =
            self.new_context_item(kind, priority, source_identity, content, hard_required);
        context.order = next_context_order(&items);
        items.push(CompactionItem {
            context,
            pair_id: None,
            tool_phase: None,
            in_flight: false,
        });
        self.commit_context_items(current, items)
    }

    fn replace_context_source(
        &mut self,
        source_identity: &str,
        replacement: Option<ContextItem>,
    ) -> Result<(), ApplicationError> {
        let current = self.active_context_series()?;
        let mut items = current
            .items
            .iter()
            .filter(|item| item.context.source_identity != source_identity)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(mut context) = replacement {
            context.order = next_context_order(&items);
            items.push(CompactionItem {
                context,
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            });
        }
        if items == current.items {
            return Ok(());
        }
        self.commit_context_items(current, items)
    }

    fn commit_context_items(
        &self,
        current: ContextSeries,
        items: Vec<CompactionItem>,
    ) -> Result<(), ApplicationError> {
        let next = ContextSeries {
            id: ContextSeriesId::from(self.ids.next_id("series")),
            session_id: self.session_id.clone(),
            parent_series_id: Some(current.id.clone()),
            restored_from_checkpoint_id: None,
            items,
            created_at_millis: self.clock.now_unix_millis(),
        };
        self.store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: Some(current.id),
                next_series: next,
                compaction_record: None,
            })
            .map_err(context_storage_error)
    }

    fn new_context_item(
        &self,
        kind: ContextKind,
        priority: Priority,
        source_identity: &str,
        content: &str,
        hard_required: bool,
    ) -> ContextItem {
        let now = self.clock.now_unix_millis();
        ContextItem {
            id: ContextItemId::from(self.ids.next_id("context")),
            kind,
            priority,
            token_cost: HeuristicTokenizer.count_tokens(content).max(1),
            source: source_identity.to_owned(),
            timestamp_millis: now,
            importance: match priority {
                Priority::Critical => 1_000,
                Priority::High => 800,
                Priority::Medium => 500,
                Priority::Low => 200,
            },
            compressibility: if hard_required {
                harness_context::Compressibility::Exact
            } else {
                harness_context::Compressibility::Structured
            },
            ttl_millis: None,
            content_hash: hash_text(content),
            source_identity: source_identity.to_owned(),
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality: ConfidentialityLabel::ProjectPrivate,
            },
            cache_class: match kind {
                ContextKind::System | ContextKind::Constraint => CacheClass::Static,
                ContextKind::Repository | ContextKind::Memory | ContextKind::Pinned => {
                    CacheClass::SemiStable
                }
                _ => CacheClass::DynamicTail,
            },
            order: 0,
            hard_required,
            content: content.to_owned(),
        }
    }

    fn create_checkpoint_record(
        &self,
        name: Option<&str>,
    ) -> Result<ContextCheckpoint, ApplicationError> {
        let state = self.recover_session()?;
        let series = self.active_context_series()?;
        let (prompt_fingerprint, _) = self.compile_current_prompt()?;
        let checkpoint = ContextCheckpoint {
            id: CheckpointId::from(self.ids.next_id("checkpoint")),
            name: name.map(str::to_owned),
            session_id: self.session_id.clone(),
            context_series_id: series.id.clone(),
            goal_revision_id: state.goal.current_revision_id,
            plan_revision: self.active_mission_id.as_ref().map(MissionId::to_string),
            completed_tasks: vec![],
            pending_tasks: vec![],
            decision_refs: series
                .items
                .iter()
                .filter(|item| item.context.kind == ContextKind::Decision)
                .map(|item| item.context.id.to_string())
                .collect(),
            constraint_refs: series
                .items
                .iter()
                .filter(|item| item.context.kind == ContextKind::Constraint)
                .map(|item| item.context.id.to_string())
                .collect(),
            modified_file_refs: Vec::<ArtifactId>::new(),
            error_refs: series
                .items
                .iter()
                .filter(|item| item.context.kind == ContextKind::Error)
                .map(|item| item.context.id.to_string())
                .collect(),
            memory_refs: series
                .items
                .iter()
                .filter(|item| item.context.kind == ContextKind::Memory)
                .map(|item| item.context.id.to_string())
                .collect(),
            prompt_fingerprint,
            created_at_millis: self.clock.now_unix_millis(),
        };
        self.store
            .save_context_checkpoint(&series.id, checkpoint.clone())
            .map_err(context_storage_error)?;
        Ok(checkpoint)
    }

    fn compile_current_prompt(&self) -> Result<(ContentHash, String), ApplicationError> {
        let _profile_span = self.profile_span("context-build");
        let series = self.active_context_series()?;
        let compiled = ContextBroker
            .compile_for_role(
                Role::Supervisor,
                series.items.into_iter().map(|item| item.context).collect(),
                &self.context_budget(),
                self.clock.now_unix_millis(),
            )
            .map_err(|error| ApplicationError::new(error.code, error.to_string()))?;
        let segments = compiled
            .selected
            .into_iter()
            .map(context_prompt_segment)
            .collect();
        let prompt = PromptCanonicalizer
            .compile(segments, vec![])
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        Ok((prompt.full_hash, prompt.text))
    }

    fn cache_current_prompt(&mut self) -> Result<(), ApplicationError> {
        let (prompt_hash, prompt_text) = self.compile_current_prompt()?;
        let key = CacheKey {
            namespace: CacheNamespace::PromptSegment,
            scope: CacheScope {
                project_id: Some(self.project_id.clone()),
                session_id: Some(self.session_id.clone()),
                provider: Some("fake".to_owned()),
                model: Some("deterministic".to_owned()),
            },
            input_hash: prompt_hash,
            schema_version: "canonical-prompt-v1".to_owned(),
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality: ConfidentialityLabel::ProjectPrivate,
            },
        };
        if self
            .cache
            .get(&key, self.clock.now_unix_millis())
            .map_err(|error| ApplicationError::new(error.code, error.message))?
            .is_none()
        {
            self.cache
                .put(CacheEntry {
                    key,
                    value: serde_json::json!({"prompt": prompt_text}),
                    effect_class: CacheEffectClass::Pure,
                    created_at_millis: self.clock.now_unix_millis(),
                    expires_at_millis: None,
                })
                .map_err(|error| ApplicationError::new(error.code, error.message))?;
        }
        Ok(())
    }

    fn publish_context_changed(&self) -> Result<(), ApplicationError> {
        let view = self.context()?;
        self.publish(
            HarnessEvent::ContextChanged {
                used_tokens: u64::from(view.used_tokens),
                max_tokens: u64::from(view.max_tokens),
                cache_percent: self.cache().effective_hit_rate_percent,
            },
            self.session_scope(),
            EventPriority::Normal,
        )
    }

    fn apply_session_command(&self, command: SessionCommand) -> Result<(), ApplicationError> {
        let state = self.recover_session()?;
        let events = decide_session(&state, &command)
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        self.store
            .commit_session(
                &self.session_id,
                state.version,
                events,
                self.clock.now_unix_millis(),
            )
            .map_err(storage_error)?;
        Ok(())
    }

    fn apply_mission_command(
        &self,
        mission_id: &MissionId,
        command: MissionCommand,
    ) -> Result<(), ApplicationError> {
        let state = self.recover_mission(mission_id)?;
        let decision = decide_mission(&state, &command)
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        let effects = decision
            .effects
            .into_iter()
            .map(|intent| {
                let (run_id, run_fence) = effect_run(&intent);
                NewEffect {
                    effect_id: EffectId::from(self.ids.next_id("effect")),
                    intent,
                    mission_epoch: MissionEpoch(1),
                    run_id,
                    run_fence,
                }
            })
            .collect();
        self.store
            .commit_mission(
                mission_id,
                state.version,
                decision.events.clone(),
                effects,
                self.clock.now_unix_millis(),
            )
            .map_err(storage_error)?;
        for event in decision.events {
            self.publish_mission_event(mission_id, event)?;
        }
        Ok(())
    }

    fn recover_session(&self) -> Result<SessionState, ApplicationError> {
        self.recover_session_id(&self.session_id)
    }

    fn recover_session_id(&self, session_id: &SessionId) -> Result<SessionState, ApplicationError> {
        let snapshot = self
            .store
            .load_session_snapshot(session_id)
            .map_err(storage_error)?;
        let mut state = snapshot
            .map(|snapshot| snapshot.state)
            .unwrap_or_else(|| SessionState::empty(session_id.clone()));
        for stored in self
            .store
            .load_session_events(session_id, state.version)
            .map_err(storage_error)?
        {
            state = reduce_session(&state, &stored.event)
                .map_err(|error| ApplicationError::new(error.code, error.message))?;
        }
        Ok(state)
    }

    fn recover_mission(&self, mission_id: &MissionId) -> Result<MissionState, ApplicationError> {
        let snapshot = self
            .store
            .load_mission_snapshot(mission_id)
            .map_err(storage_error)?;
        let mut state = snapshot
            .map(|snapshot| snapshot.state)
            .unwrap_or_else(|| MissionState::empty(mission_id.clone()));
        for stored in self
            .store
            .load_mission_events(mission_id, state.version)
            .map_err(storage_error)?
        {
            state = harness_kernel::reduce_mission(&state, &stored.event)
                .map_err(|error| ApplicationError::new("mission-replay", error.to_string()))?;
        }
        Ok(state)
    }

    fn publish_mission_event(
        &self,
        mission_id: &MissionId,
        event: DomainEvent,
    ) -> Result<(), ApplicationError> {
        let priority = if matches!(&event, DomainEvent::ApprovalRequested { .. }) {
            EventPriority::Critical
        } else {
            EventPriority::Normal
        };
        let view_event = match event {
            DomainEvent::MissionPlanInstalled { .. }
            | DomainEvent::MissionNodesAppended { .. }
            | DomainEvent::NodeStarted { .. }
            | DomainEvent::NodeSubmitted { .. }
            | DomainEvent::NodeAccepted { .. }
            | DomainEvent::NodeFailed { .. }
            | DomainEvent::NodeCancelled { .. }
            | DomainEvent::MissionCompleted {}
            | DomainEvent::MissionCancelled { .. } => {
                let plan = self.plan_for(mission_id)?;
                HarnessEvent::PlanChanged {
                    accepted: plan.accepted,
                    running: plan.running,
                    pending: plan.pending,
                    blocked: plan.blocked,
                }
            }
            DomainEvent::ApprovalRequested {
                approval_id,
                action,
                reason,
                ..
            } => {
                let metadata = serde_json::from_str::<serde_json::Value>(&action).ok();
                let invocation_id = metadata
                    .as_ref()
                    .and_then(|value| value.get("toolInvocationId"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToolInvocationId::from);
                let risk = metadata
                    .as_ref()
                    .and_then(|value| value.get("risk"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("medium")
                    .to_owned();
                let action = metadata
                    .as_ref()
                    .and_then(|value| value.get("permission"))
                    .map_or(action, serde_json::Value::to_string);
                HarnessEvent::PermissionRequested {
                    approval_id,
                    invocation_id,
                    action,
                    risk,
                    reason,
                }
            }
            DomainEvent::ApprovalResolved {
                approval_id,
                decision,
            } => HarnessEvent::ToolStatus {
                tool: approval_id.to_string(),
                status: "approval-resolved".to_owned(),
                summary: format!("{decision:?}"),
            },
            DomainEvent::MissionCreated { goal, .. } => HarnessEvent::ReasoningSummary {
                agent_id: harness_types::AgentDefinitionId::from("agent:planner"),
                summary: format!("创建 Mission：{goal}"),
            },
        };
        self.publish(view_event, self.mission_scope(mission_id), priority)
    }

    fn plan_for(&self, mission_id: &MissionId) -> Result<PlanView, ApplicationError> {
        self.recover_mission(mission_id)
            .map(|state| plan_view(&state))
    }

    fn publish(
        &self,
        event: HarnessEvent,
        mut scope: EventScope,
        priority: EventPriority,
    ) -> Result<(), ApplicationError> {
        if self.effective_config.settings.trace_enabled && scope.trace_id.is_none() {
            scope.trace_id = Some(TraceId::from(self.ids.next_id("trace")));
        }
        self.events
            .publish(event, scope, priority, self.clock.now_unix_millis())
            .map_err(|error| ApplicationError::new(error.code, error.message))?;
        Ok(())
    }

    fn session_scope(&self) -> EventScope {
        EventScope {
            session_id: Some(self.session_id.clone()),
            ..EventScope::default()
        }
    }

    fn mission_scope(&self, mission_id: &MissionId) -> EventScope {
        EventScope {
            session_id: Some(self.session_id.clone()),
            mission_id: Some(mission_id.clone()),
            ..EventScope::default()
        }
    }

    fn context_budget(&self) -> ContextBudget {
        let profile = self.mode_profile();
        ContextBudget {
            model_context_window: profile.context_window_tokens,
            reserved_output_tokens: 1_024,
            reserved_tool_tokens: profile.reserved_tool_tokens,
            reserved_recovery_tokens: 512,
            lane_caps: Default::default(),
        }
    }

    fn estimated_assignment_costs(
        &self,
        assignments: &[StaffingAssignment],
    ) -> BTreeMap<TaskId, u64> {
        assignments
            .iter()
            .map(|assignment| {
                let cost = self
                    .agent_catalog
                    .as_ref()
                    .and_then(|catalog| catalog.definition(&assignment.agent_id))
                    .map_or(1, |definition| u64::from(definition.cost_weight));
                (assignment.task_id.clone(), cost)
            })
            .collect()
    }

    fn profile_span(&self, name: &'static str) -> ProfileSpan {
        ProfileSpan {
            name,
            started: Instant::now(),
            samples: self.profile_samples.clone(),
        }
    }

    fn record_profile_sample(&self, name: &'static str, elapsed_millis: u64) {
        if let Ok(mut samples) = self.profile_samples.lock() {
            let values = samples.entry(name.to_owned()).or_default();
            values.push_back(elapsed_millis);
            while values.len() > MAX_PROFILE_SAMPLES {
                values.pop_front();
            }
        }
    }

    /// 测试和 composition root 读取 EventBus；业务状态仍不在 Renderer。
    #[must_use]
    pub fn event_bus(&self) -> EventBus {
        self.events.clone()
    }

    /// 只读访问 Store，便于下一层 composition/recovery。
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }
}

fn mode_agent_budget(profile: &ModeProfile) -> AgentBudgetPolicy {
    AgentBudgetPolicy {
        max_agents: profile.max_agents,
        max_parallel_agents: profile.max_parallel_agents,
        max_total_tokens: profile.max_total_tokens,
        max_tool_calls: profile.max_tool_calls,
        max_runtime_millis: profile.max_runtime_millis,
        max_retries: profile.max_retries,
    }
}

const fn permission_approval_policy(mode: PermissionMode) -> ApprovalPolicy {
    match mode {
        PermissionMode::Safe => ApprovalPolicy::Always,
        PermissionMode::Ask | PermissionMode::Custom => ApprovalPolicy::OnRequest,
        PermissionMode::Auto => ApprovalPolicy::UntrustedOnly,
        PermissionMode::Full => ApprovalPolicy::NeverWithinSandbox,
    }
}

fn parse_failover_targets(value: &str) -> Vec<FailoverTarget> {
    value
        .split(',')
        .map(str::trim)
        .filter_map(|target| target.split_once('/'))
        .map(|(provider, model)| FailoverTarget {
            provider_id: ProviderId::from(provider),
            model_id: ModelId::from(model),
        })
        .collect()
}

fn model_route_policy(config: &EffectiveConfigView) -> Result<ModelRoutePolicy, ApplicationError> {
    let targets = parse_failover_targets(&config.settings.failover_targets);
    if config.settings.failover_enabled && targets.is_empty() {
        return Err(ApplicationError::new(
            "failover-targets-empty",
            "启用 Failover 必须提供精确 provider/model allowlist",
        ));
    }
    Ok(ModelRoutePolicy {
        enabled: config.settings.failover_enabled,
        user_confirmed_cost_scope: config.settings.failover_cost_confirmed,
        allowlist: targets,
    })
}

fn profile_metric(name: String, values: VecDeque<u64>) -> Option<ProfileMetricView> {
    let last_millis = *values.back()?;
    let mut sorted = values.into_iter().collect::<Vec<_>>();
    sorted.sort_unstable();
    let count = sorted.len();
    let percentile = |percent: usize| {
        let index = count.saturating_sub(1).saturating_mul(percent) / 100;
        sorted[index]
    };
    Some(ProfileMetricView {
        name,
        count,
        total_millis: sorted.iter().copied().sum(),
        p50_millis: percentile(50),
        p95_millis: percentile(95),
        max_millis: *sorted.last().unwrap_or(&last_millis),
        last_millis,
    })
}

fn model_view(view: ModelRuntimeView) -> ModelView {
    ModelView {
        provider_id: view.provider_id,
        model_id: view.model_id,
        reasoning_requested: view.reasoning_requested,
        reasoning_effective: view.reasoning_effective,
        reasoning_mapping: view.reasoning_mapping,
        context_window_tokens: view.capability.context_window_tokens,
        max_output_tokens: view.capability.max_output_tokens,
        tool_calling: view.capability.tool_calling,
        structured_output: view.capability.structured_output,
    }
}

const fn reasoning_name(reasoning: ReasoningLevel) -> &'static str {
    match reasoning {
        ReasoningLevel::Off => "off",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::Xhigh => "xhigh",
        ReasoningLevel::Max => "max",
    }
}

const fn reasoning_mapping_name(mapping: ReasoningMapping) -> &'static str {
    match mapping {
        ReasoningMapping::Exact => "exact",
        ReasoningMapping::ClampedDown => "clamped-down",
        ReasoningMapping::ClampedUp => "clamped-up",
        ReasoningMapping::UnsupportedIgnored => "unsupported-ignored",
    }
}

fn next_context_order(items: &[CompactionItem]) -> i32 {
    items
        .iter()
        .map(|item| item.context.order)
        .max()
        .map_or(0, |order| order.saturating_add(1))
}

fn hash_text(value: &str) -> ContentHash {
    ContentHash::from(format!("{:x}", Sha256::digest(value.as_bytes())))
}

fn agent_session_id(run_id: &RunId) -> AgentSessionId {
    AgentSessionId::from(format!("agent-session:{run_id}"))
}

fn compact_lsp_fact(tool_name: &str, fact: &serde_json::Value) -> String {
    let location = fact.get("location").unwrap_or(fact);
    let path = location
        .get("path")
        .and_then(serde_json::Value::as_str)
        .or_else(|| fact.get("path").and_then(serde_json::Value::as_str))
        .unwrap_or("<external>");
    let human = location.get("humanRange");
    let line = human
        .and_then(|range| range.pointer("/start/line"))
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.to_string())
        .or_else(|| {
            location
                .pointer("/range/start/line")
                .and_then(serde_json::Value::as_u64)
                .map(|line| (line + 1).to_string())
        })
        .unwrap_or_else(|| "?".to_owned());
    let character = human
        .and_then(|range| range.pointer("/start/character"))
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.to_string())
        .or_else(|| {
            location
                .pointer("/range/start/character")
                .and_then(serde_json::Value::as_u64)
                .map(|character| (character + 1).to_string())
        })
        .unwrap_or_else(|| "?".to_owned());
    match tool_name {
        "lsp.symbols" => format!(
            "{} · kind={} · {path}:{line}:{character}",
            fact.get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<symbol>"),
            fact.get("kind")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        ),
        "lsp.diagnostics" => {
            let message = fact
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<diagnostic>")
                .replace(['\r', '\n'], " ")
                .chars()
                .take(512)
                .collect::<String>();
            format!(
                "severity={} · code={} · {path}:{line}:{character} · {message}",
                fact.get("severity")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                fact.get("code")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("none")
            )
        }
        _ => format!("{path}:{line}:{character}"),
    }
}

fn append_parallel_tool_result(
    continuation: &mut AgentModelContinuation,
    call: &AgentToolCall,
    output: serde_json::Value,
) {
    if call.name == "files.write"
        && let Some(path) = output.get("path").and_then(serde_json::Value::as_str)
    {
        let path = std::path::PathBuf::from(path);
        if !continuation.changed_files.contains(&path) {
            continuation.changed_files.push(path);
        }
    }
    let tool_call = ModelInputItem::ToolCall {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
    };
    let tool_result = ModelInputItem::ToolResult {
        call_id: call.call_id.clone(),
        output,
    };
    if continuation.conversation_continuation {
        continuation.next_input.push(tool_result);
    } else {
        continuation.transcript.push(tool_call);
        continuation.transcript.push(tool_result);
    }
}

const fn context_role(role: AgentRole) -> Role {
    match role {
        AgentRole::RequirementsAnalyst => Role::Requirements,
        AgentRole::Explorer => Role::Explorer,
        AgentRole::Architect => Role::Architect,
        AgentRole::Planner | AgentRole::Researcher => Role::Planner,
        AgentRole::StaffingRouter => Role::Staffing,
        AgentRole::Coder | AgentRole::Debugger | AgentRole::MergeAgent => Role::Coder,
        AgentRole::Reviewer => Role::Reviewer,
        AgentRole::SecurityAuditor => Role::Security,
        AgentRole::PerformanceEngineer => Role::Performance,
        AgentRole::Tester => Role::Tester,
        AgentRole::ReleaseManager => Role::Release,
        AgentRole::Coordinator => Role::Coordinator,
        AgentRole::Supervisor => Role::Supervisor,
    }
}

const fn role_evidence_kind(role: AgentRole) -> Option<&'static str> {
    match role {
        AgentRole::RequirementsAnalyst => Some("requirements"),
        AgentRole::Explorer => Some("exploration"),
        AgentRole::Architect => Some("architecture"),
        AgentRole::Reviewer => Some("review"),
        AgentRole::SecurityAuditor => Some("security"),
        AgentRole::PerformanceEngineer => Some("performance"),
        AgentRole::Tester => Some("test"),
        AgentRole::ReleaseManager => Some("release"),
        _ => None,
    }
}

fn role_acceptance_criteria(role: AgentRole) -> Vec<String> {
    let criterion = match role {
        AgentRole::RequirementsAnalyst => "提供范围、非目标、歧义和可确定验证的验收标准",
        AgentRole::Explorer => "提供带文件/符号引用的入口、依赖和数据流地图",
        AgentRole::Architect => "提供边界、契约、失败模式、权衡和 ADR",
        AgentRole::Planner => "提供无环依赖、文件所有权、验收证据和回滚点",
        AgentRole::Coder => "提交最小实现并报告真实工具/验证结果",
        AgentRole::Reviewer => "提供带证据位置、影响和复现条件的 review evidence",
        AgentRole::SecurityAuditor => {
            "提供严重度、证据位置、攻击前提和修复验收条件的 security evidence"
        }
        AgentRole::PerformanceEngineer => {
            "提供基线、负载、指标、瓶颈和回归阈值的 performance evidence"
        }
        AgentRole::Tester => "提供命令、环境、实际结果和覆盖映射的 test evidence",
        AgentRole::ReleaseManager => "提供版本、测试、产物、校验和与回滚核对的 release evidence",
        AgentRole::Debugger => "提供稳定复现、互斥假设、最小实验和根因链",
        AgentRole::Researcher => "提供带来源和版本日期的事实/推断分离结论",
        AgentRole::MergeAgent => "提供不丢失契约和会议决定的冲突解决结果",
        AgentRole::Coordinator => "提供冲突、会议记录和可追踪决定",
        AgentRole::StaffingRouter => "提供结构化能力、容量和成本分配理由",
        AgentRole::Supervisor => "提供目标、预算、权限、依赖和证据门状态",
    };
    vec![criterion.to_owned()]
}

const fn role_max_turns(role: AgentRole) -> u8 {
    match role {
        AgentRole::RequirementsAnalyst | AgentRole::Explorer | AgentRole::Planner => 2,
        AgentRole::Architect | AgentRole::Reviewer | AgentRole::SecurityAuditor => 3,
        AgentRole::PerformanceEngineer | AgentRole::Tester | AgentRole::ReleaseManager => 4,
        _ => 4,
    }
}

fn context_prompt_segment(item: ContextItem) -> PromptSegment {
    PromptSegment {
        id: PromptSegmentId::from(item.id.to_string()),
        version: "1".to_owned(),
        role: prompt_role(item.kind),
        priority: priority_value(item.priority),
        cacheability: match item.cache_class {
            CacheClass::Static => PromptCacheability::Static,
            CacheClass::SemiStable => PromptCacheability::SemiStable,
            CacheClass::DynamicTail => PromptCacheability::DynamicTail,
        },
        order: item.order,
        source: PromptSource {
            kind: format!("{:?}", item.kind).to_lowercase(),
            reference: item.source,
        },
        content: item.content,
    }
}

const fn prompt_role(kind: ContextKind) -> PromptRole {
    match kind {
        ContextKind::System => PromptRole::System,
        ContextKind::Constraint => PromptRole::Developer,
        ContextKind::Tool => PromptRole::Tool,
        _ => PromptRole::User,
    }
}

const fn priority_value(priority: Priority) -> i32 {
    match priority {
        Priority::Low => 10,
        Priority::Medium => 20,
        Priority::High => 30,
        Priority::Critical => 40,
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct DeterministicSummaryProvider;

impl SummaryProvider for DeterministicSummaryProvider {
    fn summarize(
        &self,
        items: &[CompactionItem],
        max_summary_tokens: u32,
    ) -> Result<StructuredSummary, harness_context::CompactionError> {
        if items.is_empty() || max_summary_tokens == 0 {
            return Err(harness_context::CompactionError::new(
                "invalid-summary-input",
                "Summary 输入或预算为空",
            ));
        }
        let sources = items
            .iter()
            .map(|item| item.context.source_identity.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let summary = format!(
            "结构化历史摘要：已压缩 {} 项；来源 [{}]。原始项仍保存在前一 Context Series。",
            items.len(),
            sources
        );
        Ok(StructuredSummary {
            token_cost: HeuristicTokenizer
                .count_tokens(&summary)
                .min(max_summary_tokens)
                .max(1),
            summary,
            active_assumptions: vec![],
            unresolved_blockers: vec![],
            completed_actions: items
                .iter()
                .map(|item| format!("preserved-ref:{}", item.context.id))
                .collect(),
            next_goal: "继续当前 durable Goal".to_owned(),
        })
    }
}

fn context_storage_error(error: harness_context::ContextStoreError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn config_error(error: harness_config::ConfigError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn tool_error(error: harness_tool::ToolError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn mcp_error(error: harness_mcp::McpError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn plugin_error(error: harness_plugin::PluginError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn skill_error(error: harness_skill::SkillError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn memory_error(error: harness_memory::MemoryError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn lsp_run_evidence(
    repository: &RepositoryIndex,
    run_id: &str,
) -> Result<Vec<Evidence>, ApplicationError> {
    repository
        .lsp_diagnostic_evidence(run_id)
        .map_err(memory_error)?
        .into_iter()
        .map(|diagnostic| {
            Ok(Evidence {
                kind: "lsp-diagnostic-delta".to_owned(),
                reference: format!(
                    "lsp:{}:{}:{}",
                    diagnostic.server_id,
                    diagnostic.path,
                    diagnostic.file_hash.get(..16).ok_or_else(|| {
                        ApplicationError::new(
                            "lsp-evidence-file-hash-invalid",
                            diagnostic.file_hash.clone(),
                        )
                    })?
                ),
                summary: format!(
                    "diagnostics {}→{} · added={} removed={} errors={} warnings={}",
                    diagnostic.before_count,
                    diagnostic.after_count,
                    diagnostic.added,
                    diagnostic.removed,
                    diagnostic.error_count,
                    diagnostic.warning_count
                ),
            })
        })
        .collect()
}

fn lsp_error(error: harness_lsp::LspError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn browser_error(error: harness_browser::BrowserError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn agent_error(error: harness_agent::AgentError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn storage_error(error: harness_kernel::StoragePortError) -> ApplicationError {
    ApplicationError::new(error.code, error.message)
}

fn effect_run(intent: &EffectIntent) -> (Option<RunId>, Option<RunFence>) {
    let run_id = match intent {
        EffectIntent::StartAgentRun { run_id, .. }
        | EffectIntent::ResumeAgentRun { run_id, .. }
        | EffectIntent::VerifyAgentRun { run_id, .. }
        | EffectIntent::CancelAgentRun { run_id, .. } => run_id.clone(),
    };
    (Some(run_id), Some(RunFence(1)))
}

fn plan_view(state: &MissionState) -> PlanView {
    let mut accepted = 0;
    let mut running = 0;
    let mut pending = 0;
    let mut blocked = 0;
    for node in state.nodes.values() {
        match node.status {
            NodeStatus::Accepted => accepted += 1,
            NodeStatus::Running | NodeStatus::Submitted => running += 1,
            NodeStatus::Queued => pending += 1,
            NodeStatus::WaitingApproval | NodeStatus::Failed | NodeStatus::Cancelled => {
                blocked += 1
            }
        }
    }
    PlanView {
        mission_id: Some(state.mission_id.clone()),
        status: Some(state.status),
        accepted,
        running,
        pending,
        blocked,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use harness_agent::builtin_agent_catalog;
    use harness_auth::{MemoryCredentialStore, SecretString};
    use harness_builtin_tools::{
        PatchStatus, WorkspacePathGuard, WorkspaceSandbox, register_file_tools,
        register_file_tools_with_patch_store,
    };
    use harness_event::EventSubscription;
    use harness_model::{FakeModelProvider, FakeScenario, ModelRegistry, ModelUsage};
    use harness_permission::{ApprovalPolicy, PermissionEngine, workspace_write_profile};
    use harness_storage::SqliteKernelStore;
    use harness_testkit::{FixedClock, SequenceIdGenerator};
    use harness_tool::{MemoryToolJournal, ToolInvocationJournal, ToolRegistry, ToolRuntime};
    use tempfile::tempdir;

    use super::*;

    fn application(
        path: &std::path::Path,
    ) -> (
        HarnessApplication<SqliteKernelStore, FixedClock, SequenceIdGenerator>,
        EventSubscription,
    ) {
        let bus = EventBus::new();
        let subscription = bus.subscribe(128).expect("event subscription");
        let store = SqliteKernelStore::open(path).expect("Store 应打开");
        let application = HarnessApplication::new(
            store,
            bus,
            FixedClock::new(1_000),
            SequenceIdGenerator::starting_at(1),
            ProjectId::from("project:test"),
            "C:/project".to_owned(),
            SessionId::from("session:test"),
        );
        (application, subscription)
    }

    fn fake_model_runtime() -> ModelRuntime {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(FakeModelProvider::echo()))
            .expect("register fake provider");
        ModelRuntime::new(
            registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("model runtime")
    }

    #[test]
    fn mode_settings_change_real_budgets_and_invalid_runtime_update_is_atomic() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, _) = application(&temporary.path().join("config.sqlite"));
        application.boot().expect("boot");
        assert_eq!(application.status().expect("status").mode, "balanced");
        assert_eq!(application.agent_budget().max_agents, 8);

        application
            .set_setting("mode", "full", ConfigLayer::Session)
            .expect("full mode");
        assert_eq!(application.status().expect("status").mode, "full");
        assert_eq!(application.agent_budget().max_agents, 12);
        assert_eq!(application.agent_budget().max_parallel_agents, 6);
        let state = application.recover_session().expect("session");
        assert_eq!(state.settings.get("mode").map(String::as_str), Some("full"));

        let error = application
            .set_setting("agents.parallel", "0", ConfigLayer::Runtime)
            .expect_err("invalid parallel");
        assert_eq!(error.code, "config-effective-invalid");
        assert_eq!(application.agent_budget().max_parallel_agents, 6);
        assert_eq!(
            application.config().provenance["agents.parallel"],
            ConfigLayer::Session
        );
    }

    #[test]
    fn cost_unit_budget_is_visible_and_blocks_agent_dispatch_before_wakeup() {
        let temporary = tempdir().expect("tempdir");
        let (application, _) = application(&temporary.path().join("cost-budget.sqlite"));
        let mut application = application
            .with_model_runtime(fake_model_runtime())
            .with_agent_catalog(builtin_agent_catalog().expect("catalog"));
        application.boot().expect("boot");
        let failover = application
            .configure_failover(true, true, &["fake/deterministic".to_owned()])
            .expect("explicit failover");
        assert!(failover.enabled);
        assert!(failover.cost_confirmed);
        assert_eq!(failover.targets, vec!["fake/deterministic"]);
        assert!(
            !application
                .configure_failover(true, false, &["fake/deterministic".to_owned()])
                .is_ok()
        );
        application
            .configure_failover(false, false, &[])
            .expect("disable failover");
        let budget = application
            .set_agent_budget("cost", 1)
            .expect("cost budget");
        assert_eq!(budget.max_cost_units, 1);
        let error = match application.prepare_parallel_agent_team("cost constrained", 2) {
            Err(error) => error,
            Ok(_) => panic!("researchers must exceed one cost unit"),
        };
        assert_eq!(error.code, "agent-cost-budget-exhausted");
        assert!(
            application
                .agents()
                .expect("agents")
                .agents
                .iter()
                .all(|agent| { matches!(agent.lifecycle, AgentLifecycle::Sleeping) })
        );
    }

    #[test]
    fn profile_why_and_trace_expose_bounded_evidence_without_private_reasoning() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, subscription) =
            application(&temporary.path().join("observability.sqlite"));
        application.boot().expect("boot");
        application
            .set_setting("trace.enabled", "true", ConfigLayer::Runtime)
            .expect("trace");
        application.set_goal("explain evidence").expect("goal");
        application.context().expect("context");

        let why = application.why().expect("why");
        assert_eq!(why.goal.as_deref(), Some("explain evidence"));
        assert!(why.summary.contains("durable Goal"));
        assert!(!why.context_sources.is_empty());
        assert!(
            !format!("{why:?}")
                .to_lowercase()
                .contains("chain-of-thought")
        );

        application.record_startup_millis(17);
        let profile = application.profile().expect("profile");
        assert!(profile.metrics.iter().any(|metric| {
            metric.name == "startup" && metric.count == 1 && metric.last_millis == 17
        }));
        assert!(
            profile
                .metrics
                .iter()
                .any(|metric| metric.name == "context-build" && metric.count > 0)
        );

        let mut traced_goal = false;
        while let Ok(event) = subscription.try_recv() {
            if matches!(event.event, HarnessEvent::GoalChanged { .. }) {
                traced_goal = event.scope.trace_id.is_some();
            }
        }
        assert!(traced_goal);
    }

    #[test]
    fn goal_history_clear_context_reset_and_session_listing_are_durable_and_recoverable() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, _) = application(&temporary.path().join("session-controls.sqlite"));
        application.boot().expect("boot");
        application.set_goal("goal one").expect("goal one");
        application.set_goal("goal two").expect("goal two");
        let history = application.goal_history(20).expect("history");
        assert_eq!(history.revisions.len(), 2);
        assert_eq!(history.revisions[0].text, "goal two");

        application.clear_goal().expect("clear");
        assert!(application.status().expect("status").goal.is_none());
        assert!(
            application
                .active_context_series()
                .expect("series")
                .items
                .iter()
                .all(|item| item.context.source_identity != "goal:active")
        );
        assert_eq!(
            application
                .goal_history(20)
                .expect("history")
                .revisions
                .len(),
            2
        );

        application.set_goal("goal after clear").expect("new goal");
        application.pin("preserve this constraint").expect("pin");
        application
            .append_context_item(
                ContextKind::Conversation,
                Priority::Medium,
                "conversation:reset-test",
                "discard this conversation",
                false,
            )
            .expect("conversation");
        let before = application.active_context_series().expect("before");
        let reset = application.reset_context().expect("reset");
        assert_eq!(reset.removed_items, 1);
        assert_eq!(reset.retained_items, 2);
        assert_ne!(reset.previous_series_id, reset.next_series_id);
        assert_eq!(reset.previous_series_id, before.id);

        let checkpoint = application
            .create_checkpoint(Some("fork-list"))
            .expect("checkpoint");
        let child = application
            .fork_session_reference(
                checkpoint.checkpoint_id.as_str(),
                Some(SessionId::from("session:listed-child")),
            )
            .expect("fork");
        let sessions = application.sessions().expect("sessions");
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| session.current));
        let child_view = sessions
            .iter()
            .find(|session| session.session_id == child.child_session_id)
            .expect("child session");
        assert_eq!(
            child_view.parent_session_id.as_ref(),
            Some(&SessionId::from("session:test"))
        );
    }

    #[test]
    fn lsp_tool_result_becomes_bounded_replaceable_repository_context_fact() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().join("project");
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("source");
        let repository =
            RepositoryIndex::open(&root, temporary.path().join("lsp-repository.sqlite"))
                .expect("repository");
        let (application, _) = application(&temporary.path().join("lsp-context.sqlite"));
        let mut application = application.with_repository(repository);
        application.boot().expect("boot");
        let arguments = serde_json::json!({
            "serverId":"fixture",
            "path":"src/main.rs"
        });
        let result = serde_json::json!({
            "serverId":"fixture",
            "path":"src/main.rs",
            "source":"lsp",
            "integrity":"untrusted",
            "fileHash":format!("{:x}", Sha256::digest(b"fn main() {}\n")),
            "documentVersion":1,
            "positionEncoding":"utf-16",
            "count":1,
            "returned":1,
            "truncated":false,
            "facts":[{
                "path":"src/main.rs",
                "range":{"start":{"line":2,"character":4},"end":{"line":2,"character":8}},
                "severity":1,
                "code":"E100",
                "message":"fixture diagnostic"
            }]
        });
        application
            .record_lsp_tool_fact(
                "lsp.diagnostics",
                &arguments,
                &result,
                Some(&RunId::from("run:reviewer")),
            )
            .expect("record");
        application
            .record_lsp_tool_fact("lsp.diagnostics", &arguments, &result, None)
            .expect("replace");
        let series = application.active_context_series().expect("series");
        let facts = series
            .items
            .iter()
            .filter(|item| item.context.source_identity.starts_with("lsp-fact:"))
            .collect::<Vec<_>>();
        assert_eq!(facts.len(), 1, "同一查询只保留最新事实摘要");
        assert_eq!(facts[0].context.kind, ContextKind::Repository);
        assert_eq!(
            facts[0].context.information_flow.integrity,
            IntegrityLabel::Untrusted
        );
        assert!(facts[0].context.content.contains("fixture diagnostic"));
        assert!(
            facts[0]
                .context
                .content
                .contains("repositoryProjection=diagnostics")
        );
        assert!(facts[0].context.content.len() <= 8 * 1024 + 128);
        let working = application
            .agent_working_context(AgentRole::Researcher)
            .expect("context broker");
        assert!(working.dynamic_context.contains("fixture diagnostic"));
        let evidence = application
            .repository
            .as_ref()
            .expect("repository")
            .lsp_diagnostic_evidence("run:reviewer")
            .expect("evidence");
        assert_eq!(evidence[0].added, 1);
        let role_evidence = lsp_run_evidence(
            application.repository.as_ref().expect("repository"),
            "run:reviewer",
        )
        .expect("role evidence");
        assert_eq!(role_evidence[0].kind, "lsp-diagnostic-delta");
        assert!(role_evidence[0].summary.contains("added=1"));
    }

    #[test]
    fn boot_goal_and_fake_task_flow_through_store_and_events() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, subscription) = application(&temporary.path().join("app.sqlite"));
        let boot = application.boot().expect("boot");
        assert_eq!(boot.session_version, 1);
        let goal = application
            .set_goal("完成终端 vertical slice")
            .expect("goal");
        assert_eq!(goal.goal.as_deref(), Some("完成终端 vertical slice"));
        let plan = application
            .run_fake_task("完成终端 vertical slice")
            .expect("fake task");
        assert_eq!(plan.accepted, 1);
        assert_eq!(plan.status, Some(MissionStatus::Completed));
        assert!(
            application
                .store()
                .list_outbox()
                .expect("outbox")
                .iter()
                .all(|entry| entry.status == harness_kernel::OutboxStatus::Completed)
        );

        let mut event_types = Vec::new();
        while let Ok(envelope) = subscription.recv_timeout(Duration::from_millis(1)) {
            event_types.push(envelope.event);
        }
        assert!(
            event_types
                .iter()
                .any(|event| matches!(event, HarnessEvent::SystemReady { .. }))
        );
        assert!(
            event_types
                .iter()
                .any(|event| matches!(event, HarnessEvent::AgentStatus { .. }))
        );
        assert!(
            event_types
                .iter()
                .any(|event| matches!(event, HarnessEvent::TextOutput { .. }))
        );
    }

    #[test]
    fn builtin_agents_stay_sleeping_and_queue_reuses_mission_kernel() {
        let temporary = tempdir().expect("tempdir");
        let (application, _subscription) = application(&temporary.path().join("agents.sqlite"));
        let mut application =
            application.with_agent_catalog(builtin_agent_catalog().expect("catalog"));
        let team = application.agents().expect("team");
        assert_eq!(team.total, 15);
        assert_eq!(team.sleeping, 15);
        assert_eq!(team.running, 0);
        assert!(
            application
                .agent(&AgentDefinitionId::from("agent:coordinator"))
                .expect("coordinator")
                .control_plane
        );
        application.boot().expect("boot");
        application.run_fake_task("inspect queue").expect("task");
        let queue = application.agent_queue().expect("queue");
        assert!(queue.mission_id.is_some());
        assert_eq!(queue.items.len(), 1);
    }

    #[test]
    fn adaptive_team_routes_elite_specialists_and_builds_conditional_evidence_gates() {
        let temporary = tempdir().expect("tempdir");
        let agent_path = temporary.path().join("adaptive-agents.sqlite");
        let (application, _subscription) =
            application(&temporary.path().join("adaptive-kernel.sqlite"));
        let mut application = application
            .with_model_runtime(fake_model_runtime())
            .with_agent_catalog(builtin_agent_catalog().expect("catalog"))
            .with_agent_control_plane(
                AgentMessageBus::open(&agent_path).expect("messages"),
                FileLeaseManager::open(
                    temporary.path(),
                    temporary.path().join("adaptive-leases.sqlite"),
                )
                .expect("leases"),
                AgentStateStore::open(&agent_path).expect("state"),
                AgentBudgetManager::open(&agent_path).expect("budgets"),
            );
        application.boot().expect("boot");
        let prepared = application
            .prepare_adaptive_agent_team(
                "release secure auth service with performance benchmark",
                2,
            )
            .expect("adaptive team");
        assert_eq!(prepared.job.dispatches.len(), 2);
        let first_wave = prepared
            .job
            .dispatches
            .iter()
            .map(|dispatch| dispatch.request.contract.agent_definition_id.to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            first_wave,
            ["agent:explorer".to_owned(), "agent:requirements".to_owned()]
                .into_iter()
                .collect()
        );
        let mission = application
            .recover_mission(prepared.continuation.mission_id())
            .expect("mission");
        assert_eq!(mission.nodes.len(), 11);
        for (task, agent) in [
            ("task:requirements", "agent:requirements"),
            ("task:explorer", "agent:explorer"),
            ("task:architect", "agent:architect"),
            ("task:security", "agent:security"),
            ("task:performance", "agent:performance"),
            ("task:release", "agent:release"),
        ] {
            assert_eq!(
                mission.nodes[&TaskId::from(task)].agent_definition_id,
                AgentDefinitionId::from(agent)
            );
        }
        let tester = &mission.nodes[&TaskId::from("task:tester")];
        assert!(tester.depends_on.contains(&TaskId::from("task:reviewer")));
        assert!(tester.depends_on.contains(&TaskId::from("task:security")));
        assert!(
            tester
                .depends_on
                .contains(&TaskId::from("task:performance"))
        );
        assert_eq!(
            mission.nodes[&TaskId::from("task:release")].depends_on,
            vec![TaskId::from("task:tester")]
        );

        let lean = AdaptiveTeamProfile::classify("fix a typo in one message");
        assert_eq!(lean, AdaptiveTeamProfile::default());
    }

    #[test]
    fn specialist_tool_views_enforce_read_execute_and_write_boundaries() {
        let temporary = tempdir().expect("tempdir");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let mut tools = ToolRegistry::new();
        register_file_tools(&mut tools, guard.clone(), 1024).expect("file tools");
        let tool_runtime = Arc::new(ToolRuntime::new(
            tools,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::NeverWithinSandbox,
            ),
            Arc::new(MemoryToolJournal::new()),
            Arc::new(WorkspaceSandbox::new(guard)),
        ));
        let (application, _) = application(&temporary.path().join("tool-boundaries.sqlite"));
        let application = application
            .with_tool_runtime(tool_runtime)
            .with_agent_catalog(builtin_agent_catalog().expect("catalog"));
        let security = application
            .agent_model_tools(
                &AgentDefinitionId::from("agent:security"),
                AgentRole::SecurityAuditor,
                "audit files",
            )
            .expect("security tools");
        assert!(security.iter().any(|tool| tool.name == "files.read"));
        assert!(security.iter().all(|tool| tool.name != "files.write"));
        let coder = application
            .agent_model_tools(
                &AgentDefinitionId::from("agent:coder"),
                AgentRole::Coder,
                "edit files",
            )
            .expect("coder tools");
        assert!(coder.iter().any(|tool| tool.name == "files.write"));
    }

    #[test]
    fn child_agent_context_excludes_irrelevant_main_history_by_role() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, _subscription) =
            application(&temporary.path().join("context.sqlite"));
        application.boot().expect("boot");
        application.set_goal("审查认证补丁").expect("goal");
        application
            .append_context_item(
                ContextKind::Conversation,
                Priority::Low,
                "conversation:unrelated",
                "无关的主会话私有历史",
                false,
            )
            .expect("conversation");
        application
            .append_context_item(
                ContextKind::Agent,
                Priority::Medium,
                "agent:catalog-body",
                "另一个 Agent 的完整能力正文",
                false,
            )
            .expect("agent context");
        application
            .append_context_item(
                ContextKind::Task,
                Priority::Critical,
                "task:review-auth",
                "只审查 auth.rs 的公开 diff",
                true,
            )
            .expect("task context");

        let reviewer = application
            .agent_working_context(AgentRole::Reviewer)
            .expect("reviewer context");
        assert!(reviewer.dynamic_context.contains("只审查 auth.rs"));
        assert!(!reviewer.dynamic_context.contains("无关的主会话私有历史"));
        assert!(!reviewer.dynamic_context.contains("完整能力正文"));
        assert_eq!(reviewer.excluded_item_ids.len(), 2);

        let coder = application
            .agent_working_context(AgentRole::Coder)
            .expect("coder context");
        assert!(coder.dynamic_context.contains("无关的主会话私有历史"));
        assert!(!coder.dynamic_context.contains("完整能力正文"));
        assert!(!coder.fingerprint.is_empty());

        let explorer = application
            .agent_working_context(AgentRole::Explorer)
            .expect("explorer context");
        assert!(explorer.dynamic_context.contains("只审查 auth.rs"));
        assert!(!explorer.dynamic_context.contains("无关的主会话私有历史"));
        assert!(!explorer.dynamic_context.contains("完整能力正文"));

        let requirements = application
            .agent_working_context(AgentRole::RequirementsAnalyst)
            .expect("requirements context");
        assert!(
            requirements
                .dynamic_context
                .contains("无关的主会话私有历史")
        );
        assert!(!requirements.dynamic_context.contains("完整能力正文"));
    }

    #[test]
    fn queue_cancel_uses_kernel_cascade_without_orphan_descendants() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, _subscription) = application(&temporary.path().join("cancel.sqlite"));
        application.boot().expect("boot");
        let mission_id = MissionId::from("mission:queue-cancel");
        application.active_mission_id = Some(mission_id.clone());
        application
            .apply_mission_command(
                &mission_id,
                MissionCommand::CreateMission {
                    mission_id: mission_id.clone(),
                    project_id: ProjectId::from("project:test"),
                    goal: "cancel branch".to_owned(),
                },
            )
            .expect("create");
        application
            .apply_mission_command(
                &mission_id,
                MissionCommand::InstallPlan {
                    nodes: vec![
                        WorkflowNodeDefinition {
                            id: TaskId::from("task:parent"),
                            title: "parent".to_owned(),
                            kind: NodeKind::Task,
                            depends_on: vec![],
                            agent_definition_id: AgentDefinitionId::from("agent:coder"),
                            requires_approval: None,
                        },
                        WorkflowNodeDefinition {
                            id: TaskId::from("task:child"),
                            title: "child".to_owned(),
                            kind: NodeKind::Task,
                            depends_on: vec![TaskId::from("task:parent")],
                            agent_definition_id: AgentDefinitionId::from("agent:reviewer"),
                            requires_approval: None,
                        },
                    ],
                },
            )
            .expect("plan");
        let queue = application
            .cancel_queue_task(&TaskId::from("task:parent"), "user changed scope")
            .expect("cancel");
        assert_eq!(queue.items.len(), 2);
        assert!(
            queue
                .items
                .iter()
                .all(|item| item.status == NodeStatus::Cancelled)
        );
        assert!(
            application
                .recover_mission(&mission_id)
                .expect("recover")
                .runs
                .is_empty()
        );
    }

    #[test]
    fn expired_start_effect_and_recoverable_sessions_resume_after_process_restart() {
        let temporary = tempdir().expect("tempdir");
        let kernel_path = temporary.path().join("kernel.sqlite");
        let agent_path = temporary.path().join("agents.sqlite");
        let lease_path = temporary.path().join("leases.sqlite");
        let (first, _subscription) = application(&kernel_path);
        let mut first = first
            .with_model_runtime(fake_model_runtime())
            .with_agent_catalog(harness_agent::builtin_agent_catalog().expect("catalog"))
            .with_agent_control_plane(
                AgentMessageBus::open(&agent_path).expect("messages"),
                FileLeaseManager::open(temporary.path(), &lease_path).expect("leases"),
                AgentStateStore::open(&agent_path).expect("state"),
                AgentBudgetManager::open(&agent_path).expect("budgets"),
            );
        first.boot().expect("first boot");
        let prepared = first
            .prepare_parallel_agent_team("recover this team", 2)
            .expect("prepare before crash");
        let mission_id = prepared.continuation.mission_id().clone();
        // 不执行也不 finalize，模拟进程在 Start Effect claim 后立即崩溃。
        drop(prepared);
        drop(first);

        let mut early = HarnessApplication::new(
            SqliteKernelStore::open(&kernel_path).expect("early kernel"),
            EventBus::new(),
            FixedClock::new(10_000),
            SequenceIdGenerator::starting_at(50),
            ProjectId::from("project:test"),
            temporary.path().display().to_string(),
            SessionId::from("session:test"),
        )
        .with_model_runtime(fake_model_runtime())
        .with_agent_catalog(harness_agent::builtin_agent_catalog().expect("catalog"))
        .with_agent_control_plane(
            AgentMessageBus::open(&agent_path).expect("messages"),
            FileLeaseManager::open(temporary.path(), &lease_path).expect("leases"),
            AgentStateStore::open(&agent_path).expect("state"),
            AgentBudgetManager::open(&agent_path).expect("budgets"),
        );
        early.boot().expect("early boot");
        assert_eq!(
            early
                .prepare_recovered_agent_team()
                .err()
                .expect("live claim lease must block duplicate resume")
                .code,
            "agent-recovery-effect-not-claimable"
        );
        drop(early);

        let store = SqliteKernelStore::open(&kernel_path).expect("reopen kernel");
        let mut recovered = HarnessApplication::new(
            store,
            EventBus::new(),
            FixedClock::new(40_000),
            SequenceIdGenerator::starting_at(100),
            ProjectId::from("project:test"),
            temporary.path().display().to_string(),
            SessionId::from("session:test"),
        )
        .with_model_runtime(fake_model_runtime())
        .with_agent_catalog(harness_agent::builtin_agent_catalog().expect("catalog"))
        .with_agent_control_plane(
            AgentMessageBus::open(&agent_path).expect("messages"),
            FileLeaseManager::open(temporary.path(), &lease_path).expect("leases"),
            AgentStateStore::open(&agent_path).expect("state"),
            AgentBudgetManager::open(&agent_path).expect("budgets"),
        );
        let status = recovered.boot().expect("recovery boot");
        assert_eq!(status.active_mission_id, Some(mission_id.clone()));
        let resumed = recovered
            .prepare_recovered_agent_team()
            .expect("lease expired and resumable");
        let outcomes = resumed.job.execute().expect("execute recovered batch");
        let plan = recovered
            .finalize_parallel_agent_team(resumed.continuation, outcomes, &BTreeSet::new(), false)
            .expect("finalize recovered");
        assert_eq!(plan.mission_id, Some(mission_id));
        assert_eq!(plan.status, Some(MissionStatus::Completed));
        assert_eq!(plan.accepted, 2);
        assert!(
            AgentStateStore::open(agent_path)
                .expect("reopen state")
                .recoverable_sessions()
                .expect("recoverable")
                .is_empty()
        );
    }

    #[test]
    fn reviewer_and_tester_requests_receive_only_dependency_result_tail() {
        let temporary = tempdir().expect("tempdir");
        let agent_path = temporary.path().join("agents.sqlite");
        let (application, _subscription) = application(&temporary.path().join("kernel.sqlite"));
        let provider = Arc::new(FakeModelProvider::echo());
        let mut registry = ModelRegistry::new();
        registry.register(provider.clone()).expect("provider");
        let runtime = ModelRuntime::new(
            registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("runtime");
        let mut application = application
            .with_model_runtime(runtime)
            .with_agent_catalog(harness_agent::builtin_agent_catalog().expect("catalog"))
            .with_agent_control_plane(
                AgentMessageBus::open(&agent_path).expect("messages"),
                FileLeaseManager::open(temporary.path(), temporary.path().join("leases.sqlite"))
                    .expect("leases"),
                AgentStateStore::open(&agent_path).expect("state"),
                AgentBudgetManager::open(&agent_path).expect("budgets"),
            );
        application.boot().expect("boot");
        let mut prepared = application
            .prepare_role_evidence_team("implement auth safely", 2)
            .expect("prepare planner");
        loop {
            let mission_id = prepared.continuation.mission_id().clone();
            let outcomes = prepared.job.execute().expect("execute wave");
            let plan = application
                .finalize_parallel_agent_team(
                    prepared.continuation,
                    outcomes,
                    &BTreeSet::new(),
                    false,
                )
                .expect("finalize wave");
            if plan.status == Some(MissionStatus::Completed) {
                break;
            }
            prepared = application
                .prepare_next_agent_wave(&mission_id)
                .expect("next wave");
        }
        let requests = provider.requests().expect("requests");
        assert_eq!(requests.len(), 5);
        let request_for = |needle: &str| {
            requests
                .iter()
                .find(|request| {
                    request.input.iter().any(|item| {
                        matches!(
                            item,
                            ModelInputItem::Message { content, .. } if content.contains(needle)
                        )
                    })
                })
                .unwrap_or_else(|| panic!("request missing: {needle}"))
        };
        let reviewer = request_for("审查所有编码分片");
        assert!(reviewer.instructions.contains("<dependency-results>"));
        assert!(reviewer.instructions.contains("task:coder:00"));
        assert!(reviewer.instructions.contains("task:coder:01"));
        let tester = request_for("验证审查结论");
        assert!(tester.instructions.contains("task:reviewer"));
        assert!(!tester.instructions.contains("无关的主会话私有历史"));
    }

    #[test]
    fn locked_goal_is_enforced_by_session_kernel() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, _subscription) = application(&temporary.path().join("goal.sqlite"));
        application.boot().expect("boot");
        application.set_goal("first").expect("goal");
        application.set_goal_lock(true).expect("lock");
        let error = application
            .set_goal("second")
            .expect_err("locked goal must reject");
        assert_eq!(error.code, "goal-locked");
    }

    #[test]
    fn checkpoint_rollback_compaction_and_cache_share_durable_context() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, _subscription) =
            application(&temporary.path().join("context-app.sqlite"));
        application.boot().expect("boot");
        application.set_goal("保持 Goal 不丢失").expect("goal");
        for index in 0..5 {
            application
                .append_context_item(
                    ContextKind::Conversation,
                    Priority::Medium,
                    &format!("turn:{index}"),
                    &format!("第 {index} 轮的长工具观察：{}", "evidence ".repeat(120)),
                    false,
                )
                .expect("append context");
        }
        application.cache_current_prompt().expect("prompt cache");
        let before = application.context().expect("context before");
        let checkpoint = application
            .create_checkpoint(Some("before-pin"))
            .expect("checkpoint");
        let parent_before = application
            .store()
            .recover_session(&SessionId::from("session:test"))
            .expect("parent before");
        let fork = application
            .fork_session(&checkpoint.checkpoint_id, None)
            .expect("fork");
        assert_eq!(
            application
                .store()
                .recover_session(&SessionId::from("session:test"))
                .expect("parent after"),
            parent_before,
            "Fork 不得向 Parent aggregate 追加事件"
        );
        let child = application
            .store()
            .recover_session(&fork.child_session_id)
            .expect("child session");
        assert_eq!(
            child.parent_session_id,
            Some(fork.parent_session_id.clone())
        );
        assert_eq!(
            child
                .goal
                .current_revision_id
                .as_ref()
                .and_then(|id| child.goal.revisions.get(id))
                .map(|goal| goal.text.as_str()),
            Some("保持 Goal 不丢失")
        );
        assert_eq!(
            application
                .store()
                .load_active_context_series(&fork.child_session_id)
                .expect("child context")
                .expect("child context exists")
                .id,
            fork.child_context_series_id
        );
        application.pin("这个约束必须保留").expect("pin");
        let after_pin = application.context().expect("context after pin");
        assert!(after_pin.item_count > before.item_count);

        let rolled_back = application
            .rollback(&checkpoint.checkpoint_id)
            .expect("rollback");
        assert_eq!(rolled_back.item_count, before.item_count);
        assert_ne!(rolled_back.series_id, checkpoint.context_series_id);
        assert!(
            application
                .store()
                .load_context_series(&checkpoint.context_series_id)
                .expect("old series")
                .is_some(),
            "Rollback 不得删除 Parent Series"
        );

        let compacted = application
            .compact(CompactionMode::Safe)
            .expect("safe compact");
        assert!(compacted.token_cost_after < compacted.token_cost_before);
        assert_eq!(
            application.status().expect("status").goal.as_deref(),
            Some("保持 Goal 不丢失")
        );
        assert!(application.cache().l1.writes >= 1);
    }

    #[test]
    fn auto_compaction_triggers_at_eighty_percent_budget() {
        let temporary = tempdir().expect("tempdir");
        let (mut application, _subscription) =
            application(&temporary.path().join("auto-compact.sqlite"));
        application.boot().expect("boot");
        application.set_goal("auto compact goal").expect("goal");
        for (index, bytes) in [8_000, 6_000, 6_000].into_iter().enumerate() {
            application
                .append_context_item(
                    ContextKind::Conversation,
                    Priority::Medium,
                    &format!("large-turn:{index}"),
                    &"x".repeat(bytes),
                    false,
                )
                .expect("append large context");
        }
        assert!(application.context().expect("before").percent >= 80);
        let compacted = application
            .auto_compact_if_needed()
            .expect("auto compact")
            .expect("threshold should trigger");
        assert!(compacted.token_cost_after < compacted.token_cost_before);
        assert!(application.context().expect("after").percent < 80);
    }

    #[test]
    fn model_runtime_drives_mission_and_persists_selection() {
        let temporary = tempdir().expect("tempdir");
        let (application, subscription) = application(&temporary.path().join("model.sqlite"));
        let mut application = application.with_model_runtime(fake_model_runtime());
        let boot = application.boot().expect("boot");
        assert_eq!(boot.model, "fake/deterministic");
        assert_eq!(boot.session_version, 2, "create + model selection");
        let reasoning = application
            .set_reasoning(ReasoningLevel::Max)
            .expect("reasoning");
        assert_eq!(reasoning.reasoning_effective, Some(ReasoningLevel::High));
        assert_eq!(reasoning.reasoning_mapping, ReasoningMapping::ClampedDown);
        let plan = application
            .run_fake_task("model-backed task")
            .expect("model task");
        assert_eq!(plan.status, Some(MissionStatus::Completed));
        let session = application
            .store()
            .recover_session(&SessionId::from("session:test"))
            .expect("session");
        assert_eq!(session.model.provider_id, Some(ProviderId::from("fake")));
        assert_eq!(session.model.model_id, Some(ModelId::from("deterministic")));
        assert_eq!(session.model.reasoning, ReasoningLevel::Max);

        let mut saw_text = false;
        let mut saw_usage = false;
        while let Ok(envelope) = subscription.recv_timeout(Duration::from_millis(1)) {
            match envelope.event {
                HarnessEvent::TextOutput { text }
                    if text.contains("deterministic:model-backed") =>
                {
                    saw_text = true;
                }
                HarnessEvent::ModelUsage { .. } => saw_usage = true,
                _ => {}
            }
        }
        assert!(saw_text);
        assert!(saw_usage);
    }

    #[test]
    fn model_tool_call_executes_and_continues_through_unified_runtime() {
        let temporary = tempdir().expect("tempdir");
        std::fs::write(temporary.path().join("note.txt"), "tool evidence").expect("fixture");
        let (application, subscription) = application(&temporary.path().join("tool-model.sqlite"));
        let mut model_registry = ModelRegistry::new();
        let usage = ModelUsage {
            input_tokens: 2,
            output_tokens: 1,
            total_tokens: 3,
            ..ModelUsage::default()
        };
        model_registry
            .register(Arc::new(FakeModelProvider::standard(vec![
                FakeScenario::tool(
                    "files.read",
                    serde_json::json!({"path":temporary.path().join("note.txt")}),
                    usage,
                ),
                FakeScenario::text(&["finished-with-tool"], usage),
            ])))
            .expect("model provider");
        let model_runtime = ModelRuntime::new(
            model_registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("model runtime");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let mut tool_registry = ToolRegistry::new();
        register_file_tools(&mut tool_registry, guard.clone(), 1024).expect("file tools");
        let journal = Arc::new(MemoryToolJournal::new());
        let tool_runtime = Arc::new(ToolRuntime::new(
            tool_registry,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::NeverWithinSandbox,
            ),
            journal.clone(),
            Arc::new(WorkspaceSandbox::new(guard)),
        ));
        let mut application = application
            .with_model_runtime(model_runtime)
            .with_tool_runtime(tool_runtime);
        application.boot().expect("boot");
        let plan = application
            .run_fake_task("read note then finish")
            .expect("tool-backed task");
        assert_eq!(plan.status, Some(MissionStatus::Completed));
        assert_eq!(
            journal.list().expect("journal")[0].status,
            ToolInvocationStatus::Completed
        );
        let mut saw_final = false;
        while let Ok(event) = subscription.recv_timeout(Duration::from_millis(1)) {
            if matches!(event.event, HarnessEvent::TextOutput { ref text } if text == "finished-with-tool")
            {
                saw_final = true;
            }
        }
        assert!(saw_final);
    }

    #[test]
    fn agent_file_write_holds_lease_through_patch_application_and_releases_afterward() {
        let temporary = tempdir().expect("tempdir");
        let target = temporary.path().join("leased.txt");
        let kernel_path = temporary.path().join("kernel.sqlite");
        let agent_path = temporary.path().join("agents.sqlite");
        let lease_path = temporary.path().join("leases.sqlite");
        let (application, _subscription) = application(&kernel_path);
        let usage = ModelUsage {
            input_tokens: 2,
            output_tokens: 1,
            total_tokens: 3,
            ..ModelUsage::default()
        };
        let mut model_registry = ModelRegistry::new();
        model_registry
            .register(Arc::new(FakeModelProvider::standard(vec![
                FakeScenario::tool(
                    "files.write",
                    serde_json::json!({"path":target.clone(),"content":"leased content"}),
                    usage,
                ),
                FakeScenario::text(&["write completed"], usage),
            ])))
            .expect("model provider");
        let model_runtime = ModelRuntime::new(
            model_registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("model runtime");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let patch_store = Arc::new(
            PatchStore::open(temporary.path().join("patches"), guard.clone()).expect("patches"),
        );
        let mut tool_registry = ToolRegistry::new();
        register_file_tools_with_patch_store(
            &mut tool_registry,
            guard.clone(),
            1024,
            Some(patch_store.clone()),
        )
        .expect("file tools");
        let tool_runtime = Arc::new(ToolRuntime::new(
            tool_registry,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::NeverWithinSandbox,
            ),
            Arc::new(MemoryToolJournal::new()),
            Arc::new(WorkspaceSandbox::new(guard)),
        ));
        let mut application = application
            .with_model_runtime(model_runtime)
            .with_tool_runtime(tool_runtime)
            .with_patch_store(patch_store.clone())
            .with_agent_catalog(harness_agent::builtin_agent_catalog().expect("catalog"))
            .with_agent_control_plane(
                AgentMessageBus::open(&agent_path).expect("messages"),
                FileLeaseManager::open(temporary.path(), &lease_path).expect("leases"),
                AgentStateStore::open(&agent_path).expect("state"),
                AgentBudgetManager::open(&agent_path).expect("budgets"),
            );
        application.boot().expect("boot");
        let plan = application
            .run_fake_task("write through lease and patch")
            .expect("task");
        assert_eq!(plan.status, Some(MissionStatus::Completed));
        assert_eq!(
            std::fs::read_to_string(&target).expect("target"),
            "leased content"
        );
        let patches = patch_store.list().expect("patch list");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].status, PatchStatus::Applied);
        let mut reopened =
            FileLeaseManager::open(temporary.path(), &lease_path).expect("reopen leases");
        let lease = reopened
            .acquire(&target, RunId::from("run:after"), 10_000, 100)
            .expect("lease was released");
        assert!(reopened.release(&lease).expect("release probe"));
    }

    #[test]
    fn role_workflow_coder_yields_file_tool_and_resumes_in_next_model_job() {
        let temporary = tempdir().expect("tempdir");
        let target = temporary.path().join("parallel-tool.txt");
        let agent_path = temporary.path().join("agents.sqlite");
        let lease_path = temporary.path().join("leases.sqlite");
        let (application, _subscription) = application(&temporary.path().join("kernel.sqlite"));
        let usage = ModelUsage {
            input_tokens: 2,
            output_tokens: 1,
            total_tokens: 3,
            ..ModelUsage::default()
        };
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(FakeModelProvider::standard(vec![
                FakeScenario::text(&["planner result"], usage),
                FakeScenario::tool(
                    "files.write",
                    serde_json::json!({"path":target.clone(),"content":"parallel tool write"}),
                    usage,
                ),
                FakeScenario::text(&["coder completed after tool"], usage),
                FakeScenario::text(&["review passed"], usage),
                FakeScenario::text(&["tests passed"], usage),
            ])))
            .expect("provider");
        let runtime = ModelRuntime::new(
            registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("runtime");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let patch_store = Arc::new(
            PatchStore::open(temporary.path().join("patches"), guard.clone()).expect("patches"),
        );
        let mut tools = ToolRegistry::new();
        register_file_tools_with_patch_store(
            &mut tools,
            guard.clone(),
            1024,
            Some(patch_store.clone()),
        )
        .expect("tools");
        let tool_runtime = Arc::new(ToolRuntime::new(
            tools,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::NeverWithinSandbox,
            ),
            Arc::new(MemoryToolJournal::new()),
            Arc::new(WorkspaceSandbox::new(guard)),
        ));
        let mut application = application
            .with_model_runtime(runtime)
            .with_tool_runtime(tool_runtime)
            .with_patch_store(patch_store.clone())
            .with_agent_catalog(harness_agent::builtin_agent_catalog().expect("catalog"))
            .with_agent_control_plane(
                AgentMessageBus::open(&agent_path).expect("messages"),
                FileLeaseManager::open(temporary.path(), &lease_path).expect("leases"),
                AgentStateStore::open(&agent_path).expect("state"),
                AgentBudgetManager::open(&agent_path).expect("budgets"),
            );
        application.boot().expect("boot");
        let mut prepared = application
            .prepare_role_evidence_team("write then review", 1)
            .expect("planner");
        let mut tool_continuations = 0;
        loop {
            let mission_id = prepared.continuation.mission_id().clone();
            let outcomes = prepared.job.execute().expect("execute wave");
            let step = application
                .finalize_parallel_agent_team_step(
                    prepared.continuation,
                    outcomes,
                    &BTreeSet::new(),
                    false,
                )
                .expect("finalize step");
            if let Some(next) = step.next {
                tool_continuations += 1;
                prepared = next;
                continue;
            }
            if step.plan.status == Some(MissionStatus::Completed) {
                assert_eq!(step.plan.accepted, 4);
                break;
            }
            prepared = application
                .prepare_next_agent_wave(&mission_id)
                .expect("next role wave");
        }
        assert_eq!(tool_continuations, 1);
        assert_eq!(
            std::fs::read_to_string(&target).expect("target"),
            "parallel tool write"
        );
        assert_eq!(patch_store.list().expect("patches").len(), 1);
        let state = AgentStateStore::open(agent_path).expect("state");
        let coder_session = state
            .recoverable_sessions()
            .expect("recoverable")
            .into_iter()
            .find(|session| session.role == AgentRole::Coder);
        assert!(
            coder_session.is_none(),
            "completed Coder must not be recoverable"
        );
        let coder_run = application
            .recover_mission(application.active_mission_id.as_ref().expect("mission"))
            .expect("mission")
            .runs
            .values()
            .find(|run| run.agent_definition_id == AgentDefinitionId::from("agent:coder"))
            .expect("coder run")
            .id
            .clone();
        let result = state
            .result(&coder_run)
            .expect("result")
            .expect("coder result");
        assert_eq!(result.metrics.turns, 2);
        assert_eq!(result.metrics.tool_calls, 1);
        assert_eq!(
            result.changed_files,
            vec![std::fs::canonicalize(target).expect("canonical target")]
        );
    }

    #[test]
    fn parallel_coder_waits_for_approval_then_resumes_same_model_continuation() {
        let temporary = tempdir().expect("tempdir");
        let target = temporary.path().join("approved-parallel.txt");
        let agent_path = temporary.path().join("agents.sqlite");
        let lease_path = temporary.path().join("leases.sqlite");
        let (application, subscription) = application(&temporary.path().join("kernel.sqlite"));
        let usage = ModelUsage {
            input_tokens: 2,
            output_tokens: 1,
            total_tokens: 3,
            ..ModelUsage::default()
        };
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(FakeModelProvider::standard(vec![
                FakeScenario::text(&["planner result"], usage),
                FakeScenario::tool(
                    "files.write",
                    serde_json::json!({"path":target.clone(),"content":"approved parallel write"}),
                    usage,
                ),
                FakeScenario::text(&["coder resumed after approval"], usage),
                FakeScenario::text(&["review passed"], usage),
                FakeScenario::text(&["tests passed"], usage),
            ])))
            .expect("provider");
        let runtime = ModelRuntime::new(
            registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("runtime");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let patch_store = Arc::new(
            PatchStore::open(temporary.path().join("patches"), guard.clone()).expect("patches"),
        );
        let mut tools = ToolRegistry::new();
        register_file_tools_with_patch_store(
            &mut tools,
            guard.clone(),
            1024,
            Some(patch_store.clone()),
        )
        .expect("tools");
        let journal = Arc::new(MemoryToolJournal::new());
        let tool_runtime = Arc::new(ToolRuntime::new(
            tools,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::Always,
            ),
            journal.clone(),
            Arc::new(WorkspaceSandbox::new(guard)),
        ));
        let mut application = application
            .with_model_runtime(runtime)
            .with_tool_runtime(tool_runtime)
            .with_patch_store(patch_store)
            .with_agent_catalog(harness_agent::builtin_agent_catalog().expect("catalog"))
            .with_agent_control_plane(
                AgentMessageBus::open(&agent_path).expect("messages"),
                FileLeaseManager::open(temporary.path(), &lease_path).expect("leases"),
                AgentStateStore::open(&agent_path).expect("state"),
                AgentBudgetManager::open(&agent_path).expect("budgets"),
            );
        application.boot().expect("boot");
        let planner = application
            .prepare_role_evidence_team("write with approval", 1)
            .expect("planner");
        let mission_id = planner.continuation.mission_id().clone();
        let planner_outcomes = planner.job.execute().expect("planner execute");
        let planner_step = application
            .finalize_parallel_agent_team_step(
                planner.continuation,
                planner_outcomes,
                &BTreeSet::new(),
                false,
            )
            .expect("planner finalize");
        assert!(planner_step.next.is_none());
        let coder = application
            .prepare_next_agent_wave(&mission_id)
            .expect("coder wave");
        let coder_outcomes = coder.job.execute().expect("coder first turn");
        let waiting = application
            .finalize_parallel_agent_team_step(
                coder.continuation,
                coder_outcomes,
                &BTreeSet::new(),
                false,
            )
            .expect("waiting approval");
        assert_eq!(waiting.plan.blocked, 1);
        assert!(waiting.next.is_none());
        let invocation = journal
            .list()
            .expect("journal")
            .into_iter()
            .find(|invocation| invocation.status == ToolInvocationStatus::WaitingApproval)
            .expect("waiting invocation");
        let approved = application
            .approve_tool(invocation.id.clone(), GrantScope::Once)
            .expect("approve parallel tool");
        assert_eq!(approved.status, ToolInvocationStatus::Completed);
        let resumed = application
            .take_ready_parallel_resume()
            .expect("prepared model resume");
        let outcomes = resumed.job.execute().expect("resumed model");
        let step = application
            .finalize_parallel_agent_team_step(
                resumed.continuation,
                outcomes,
                &BTreeSet::new(),
                false,
            )
            .expect("resume finalize");
        assert!(step.next.is_none());
        let mut plan = step.plan;
        while plan.status != Some(MissionStatus::Completed) {
            let wave = application
                .prepare_next_agent_wave(&mission_id)
                .expect("next role wave");
            let outcomes = wave.job.execute().expect("execute role wave");
            let step = application
                .finalize_parallel_agent_team_step(
                    wave.continuation,
                    outcomes,
                    &BTreeSet::new(),
                    false,
                )
                .expect("finalize role wave");
            assert!(step.next.is_none());
            plan = step.plan;
        }
        assert_eq!(plan.accepted, 4);
        assert_eq!(
            std::fs::read_to_string(&target).expect("target"),
            "approved parallel write"
        );
        let mut saw_permission = false;
        while let Ok(event) = subscription.recv_timeout(Duration::from_millis(1)) {
            if matches!(event.event, HarnessEvent::PermissionRequested { .. }) {
                saw_permission = true;
            }
        }
        assert!(saw_permission);
    }

    #[test]
    fn coordinator_persists_conflict_meeting_into_non_vector_project_memory() {
        let temporary = tempdir().expect("tempdir");
        let agent_path = temporary.path().join("agents.sqlite");
        let (application, _subscription) = application(&temporary.path().join("kernel.sqlite"));
        let memory = ProjectMemory::open(
            "project:test",
            temporary.path().join("memory.sqlite"),
            harness_memory::EmbeddingConfig {
                model: None,
                provider: None,
                dimensions: None,
            },
        )
        .expect("memory");
        let mut application = application
            .with_memory(memory)
            .with_agent_catalog(harness_agent::builtin_agent_catalog().expect("catalog"))
            .with_agent_control_plane(
                AgentMessageBus::open(&agent_path).expect("messages"),
                FileLeaseManager::open(temporary.path(), temporary.path().join("leases.sqlite"))
                    .expect("leases"),
                AgentStateStore::open(&agent_path).expect("state"),
                AgentBudgetManager::open(&agent_path).expect("budgets"),
            );
        application.boot().expect("boot");
        let mission_id = MissionId::from("mission:coordination");
        application.active_mission_id = Some(mission_id.clone());
        application
            .apply_mission_command(
                &mission_id,
                MissionCommand::CreateMission {
                    mission_id: mission_id.clone(),
                    project_id: ProjectId::from("project:test"),
                    goal: "coordinate two proposals".to_owned(),
                },
            )
            .expect("create mission");
        application
            .apply_mission_command(
                &mission_id,
                MissionCommand::InstallPlan {
                    nodes: vec![
                        WorkflowNodeDefinition {
                            id: TaskId::from("task:run:a"),
                            title: "proposal a".to_owned(),
                            kind: NodeKind::Task,
                            depends_on: vec![],
                            agent_definition_id: AgentDefinitionId::from("agent:coder"),
                            requires_approval: None,
                        },
                        WorkflowNodeDefinition {
                            id: TaskId::from("task:run:b"),
                            title: "proposal b".to_owned(),
                            kind: NodeKind::Review,
                            depends_on: vec![],
                            agent_definition_id: AgentDefinitionId::from("agent:reviewer"),
                            requires_approval: None,
                        },
                    ],
                },
            )
            .expect("install plan");
        let result = |agent: &str, run: &str, strategy: &str| AgentResultEnvelope {
            agent_id: AgentDefinitionId::from(agent),
            task_id: TaskId::from(format!("task:{run}")),
            run_id: RunId::from(run),
            result: AgentResult {
                status: "completed".to_owned(),
                summary: strategy.to_owned(),
                artifacts: vec![],
                changed_files: vec![std::path::PathBuf::from("src/auth.rs")],
                evidence: vec![],
                warnings: vec![],
                errors: vec![],
                metrics: AgentExecutionMetrics::default(),
                confidence: 0.8,
                follow_up: vec![],
                model_tool_yield: None,
            },
            decision_claims: [("auth.strategy".to_owned(), strategy.to_owned())]
                .into_iter()
                .collect(),
        };
        let view = application
            .coordinate_results(
                &mission_id,
                &[
                    result("agent:coder", "run:a", "token"),
                    result("agent:reviewer", "run:b", "session"),
                ],
            )
            .expect("coordinate");
        assert!(view.conflict_count >= 2);
        assert!(view.merge_required);
        let merge_task_id = view.merge_task_id.clone().expect("merge task");
        assert!(view.memory_id.is_some());
        let mission = application.recover_mission(&mission_id).expect("mission");
        assert_eq!(mission.nodes[&merge_task_id].kind, NodeKind::Merge);
        assert_eq!(
            mission.nodes[&merge_task_id].agent_definition_id,
            AgentDefinitionId::from("agent:merge")
        );
        assert!(matches!(
            application.memory_view().expect("memory view").semantic,
            harness_memory::SemanticCapability::Absent { .. }
        ));
        let search = application
            .search_memory("方案冲突", RetrievalMode::Lexical, 10)
            .expect("meeting search");
        assert_eq!(search.results.len(), 1);
        let meeting_id = view.meeting_id.expect("meeting id");
        drop(application);
        assert!(
            AgentMessageBus::open(agent_path)
                .expect("reopen messages")
                .meeting(&meeting_id)
                .expect("meeting")
                .is_some()
        );
    }

    #[test]
    fn model_tool_approval_pauses_kernel_and_resumes_exact_continuation() {
        let temporary = tempdir().expect("tempdir");
        std::fs::write(
            temporary.path().join("approval-note.txt"),
            "approved evidence",
        )
        .expect("fixture");
        let (application, subscription) =
            application(&temporary.path().join("tool-approval.sqlite"));
        let usage = ModelUsage {
            input_tokens: 2,
            output_tokens: 1,
            total_tokens: 3,
            ..ModelUsage::default()
        };
        let mut model_registry = ModelRegistry::new();
        model_registry
            .register(Arc::new(FakeModelProvider::standard(vec![
                FakeScenario::tool(
                    "files.read",
                    serde_json::json!({"path":temporary.path().join("approval-note.txt")}),
                    usage,
                ),
                FakeScenario::text(&["continued-after-approval"], usage),
            ])))
            .expect("model provider");
        let model_runtime = ModelRuntime::new(
            model_registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("model runtime");
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let mut tool_registry = ToolRegistry::new();
        register_file_tools(&mut tool_registry, guard.clone(), 1024).expect("file tools");
        let journal = Arc::new(MemoryToolJournal::new());
        let tool_runtime = Arc::new(ToolRuntime::new(
            tool_registry,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::Always,
            ),
            journal.clone(),
            Arc::new(WorkspaceSandbox::new(guard)),
        ));
        let mut application = application
            .with_model_runtime(model_runtime)
            .with_tool_runtime(tool_runtime);
        application.boot().expect("boot");
        let waiting = application
            .run_fake_task("read only after approval")
            .expect("waiting plan");
        assert_eq!(waiting.blocked, 1);
        assert_eq!(waiting.accepted, 0);
        let invocation = journal.list().expect("journal").remove(0);
        assert_eq!(invocation.status, ToolInvocationStatus::WaitingApproval);
        let mut permission_events = 0;
        while let Ok(event) = subscription.recv_timeout(Duration::from_millis(1)) {
            if let HarnessEvent::PermissionRequested {
                invocation_id: Some(ref event_invocation_id),
                ..
            } = event.event
            {
                assert_eq!(event_invocation_id, &invocation.id);
                permission_events += 1;
            }
        }
        assert_eq!(permission_events, 1);

        let completed = application
            .approve_tool(invocation.id, GrantScope::Once)
            .expect("approve and resume");
        assert_eq!(completed.status, ToolInvocationStatus::Completed);
        let plan = application.plan().expect("resumed plan");
        assert_eq!(plan.status, Some(MissionStatus::Completed));
        assert_eq!(plan.accepted, 1);
        assert_eq!(journal.list().expect("journal").len(), 1);
    }

    #[test]
    fn recovered_agent_approval_without_model_continuation_fails_closed() {
        let temporary = tempdir().expect("tempdir");
        let note = temporary.path().join("recovered.txt");
        std::fs::write(&note, "do not execute yet").expect("fixture");
        let (application, _subscription) = application(&temporary.path().join("recovered.sqlite"));
        let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
        let mut tool_registry = ToolRegistry::new();
        register_file_tools(&mut tool_registry, guard.clone(), 1024).expect("file tools");
        let journal = Arc::new(MemoryToolJournal::new());
        let tool_runtime = Arc::new(ToolRuntime::new(
            tool_registry,
            PermissionEngine::new(
                workspace_write_profile(guard.root().to_path_buf()),
                ApprovalPolicy::Always,
            ),
            journal.clone(),
            Arc::new(WorkspaceSandbox::new(guard)),
        ));
        let invocation_id = ToolInvocationId::from("invocation:recovered-agent");
        let waiting = tool_runtime
            .invoke(ToolInvokeRequest {
                invocation_id: invocation_id.clone(),
                approval_request_id: PermissionRequestId::from("approval:recovered-agent"),
                idempotency_key: "recovered-agent".to_owned(),
                envelope: ExecutionEnvelope {
                    project_id: ProjectId::from("project:test"),
                    mission_id: MissionId::from("mission:recovered"),
                    run_id: Some(RunId::from("run:recovered")),
                    actor_id: ActorId::from("agent:model"),
                    origin: InvocationOrigin::Agent,
                    information_flow: InformationFlowLabel {
                        integrity: IntegrityLabel::Trusted,
                        confidentiality: ConfidentialityLabel::ProjectPrivate,
                    },
                },
                tool_name: "files.read".to_owned(),
                args: serde_json::json!({"path":note}),
                now_millis: 1,
            })
            .expect("waiting");
        assert_eq!(
            waiting.invocation.status,
            ToolInvocationStatus::WaitingApproval
        );
        let mut application = application.with_tool_runtime(tool_runtime);
        let error = application
            .approve_tool(invocation_id.clone(), GrantScope::Once)
            .expect_err("missing model continuation must fail closed");
        assert_eq!(error.code, "model-continuation-unavailable");
        assert_eq!(
            journal
                .get(&invocation_id)
                .expect("journal")
                .expect("record")
                .status,
            ToolInvocationStatus::WaitingApproval
        );
    }

    #[test]
    fn account_and_logout_use_credential_port_without_exposing_secret() {
        let temporary = tempdir().expect("tempdir");
        let (application, _subscription) = application(&temporary.path().join("auth.sqlite"));
        let credentials = Arc::new(MemoryCredentialStore::new());
        credentials
            .put(
                &CredentialId::new(OPENAI_API_KEY_CREDENTIAL_ID),
                SecretString::new("sk-test-hidden"),
            )
            .expect("put");
        let application = application.with_credentials(credentials);
        let view = application.account().expect("account");
        assert!(view.configured);
        assert!(
            !serde_json::to_string(&view)
                .expect("json")
                .contains("sk-test-hidden")
        );
        assert!(application.logout("openai").expect("logout"));
        assert!(!application.account().expect("after").configured);
    }
}
