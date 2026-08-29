use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use harness_model::{
    CancellationToken, CompletionStatus, ModelEvent, ModelInputItem, ModelMessageRole,
    ModelRequest, ModelRuntime, ResponseFormat, ToolDefinition,
};
use harness_types::{
    AgentDefinitionId, AgentEndpointId, AgentInstanceId, AgentSessionId, ContextItemId, MissionId,
    ResponseId, RunId, TaskId, ToolCallId,
};
use serde::{Deserialize, Serialize};

use crate::{AgentError, AgentResult, AgentRole};

/// 可被 Scheduler 寻址的稳定执行端点；具体线程或远程进程可以重绑到同一 ID。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentEndpointStatus {
    Offline,
    Idle,
    Busy,
    Draining,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentEndpoint {
    pub id: AgentEndpointId,
    pub definition_id: AgentDefinitionId,
    pub instance_id: AgentInstanceId,
    pub status: AgentEndpointStatus,
    pub generation: u64,
    pub active_runs: usize,
    pub max_concurrency: usize,
    pub last_heartbeat_millis: i64,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentSessionStatus {
    Prepared,
    Running,
    WaitingTool,
    WaitingApproval,
    Submitted,
    Completed,
    CancelRequested,
    Cancelled,
    Failed,
}

impl AgentSessionStatus {
    #[must_use]
    pub fn recoverable(self) -> bool {
        matches!(
            self,
            Self::Prepared
                | Self::Running
                | Self::WaitingTool
                | Self::WaitingApproval
                | Self::CancelRequested
        )
    }
}

/// 一段独立、可恢复的子 Agent 模型会话；不复制 Mission 的任务状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSession {
    pub id: AgentSessionId,
    pub mission_id: MissionId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub parent_run_id: Option<RunId>,
    pub endpoint_id: AgentEndpointId,
    pub agent_definition_id: AgentDefinitionId,
    pub role: AgentRole,
    pub status: AgentSessionStatus,
    pub context_fingerprint: String,
    pub previous_response_id: Option<ResponseId>,
    pub created_at_millis: i64,
    pub updated_at_millis: i64,
    pub version: u64,
}

/// Kernel 节点派发给 Agent Executor 的有界合同。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentTaskContract {
    pub mission_id: MissionId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub parent_run_id: Option<RunId>,
    pub endpoint_id: AgentEndpointId,
    pub agent_definition_id: AgentDefinitionId,
    pub role: AgentRole,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub max_turns: u8,
    pub deadline_millis: i64,
    pub planning_budget: Option<PlanningBudget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanningBudget {
    pub max_planning_iterations: u8,
    pub max_planner_tokens: u32,
    pub max_plan_depth: u8,
    pub max_wall_time_millis: u64,
    pub max_discovery_actions: u32,
}

impl PlanningBudget {
    #[must_use]
    pub const fn bounded_default() -> Self {
        Self {
            max_planning_iterations: 2,
            max_planner_tokens: 8_192,
            max_plan_depth: 4,
            max_wall_time_millis: 120_000,
            max_discovery_actions: 16,
        }
    }
}

/// ContextBroker 已筛选后的最小工作集；Executor 不接触完整主会话。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentWorkingContext {
    pub stable_instructions: String,
    pub dynamic_context: String,
    pub selected_item_ids: Vec<ContextItemId>,
    pub excluded_item_ids: Vec<ContextItemId>,
    pub token_cost: u32,
    pub max_input_tokens: u32,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentExecutionRequest {
    pub session_id: AgentSessionId,
    pub contract: AgentTaskContract,
    pub context: AgentWorkingContext,
    pub steering_messages: Vec<String>,
    pub model_tools: Vec<ToolDefinition>,
    pub model_continuation: Option<AgentModelContinuation>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentExecutionMetrics {
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cached_input_tokens: u64,
    pub tool_calls: u64,
    pub elapsed_millis: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelContinuation {
    pub instructions: String,
    pub transcript: Vec<ModelInputItem>,
    pub next_input: Vec<ModelInputItem>,
    pub tools: Vec<ToolDefinition>,
    pub previous_response_id: Option<ResponseId>,
    pub output: String,
    pub next_turn: u8,
    pub max_turns: u8,
    pub conversation_continuation: bool,
    pub metrics: AgentExecutionMetrics,
    pub changed_files: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentToolCall {
    pub call_id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelToolYield {
    pub continuation: AgentModelContinuation,
    pub response_id: Option<ResponseId>,
    pub calls: Vec<AgentToolCall>,
}

#[derive(Clone, Debug)]
pub struct AgentDispatch {
    pub request: AgentExecutionRequest,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentExecutionOutcome {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub agent_definition_id: AgentDefinitionId,
    pub result: Option<AgentResult>,
    pub error: Option<AgentError>,
}

/// Model/Tool 组合适配器实现此 Port；Executor 本身不依赖某家 Provider。
pub trait AgentTaskHandler: Send + Sync + 'static {
    fn execute(
        &self,
        request: AgentExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<AgentResult, AgentError>;
}

/// 前台可写、后台 worker 可读的 Steering 安全边界；只在下一次 Agent 调用开始前快照。
#[derive(Clone, Default)]
pub struct SharedSteeringBuffer {
    messages: Arc<Mutex<Vec<String>>>,
}

impl SharedSteeringBuffer {
    pub fn push(&self, instruction: &str) -> Result<(), AgentError> {
        if instruction.trim().is_empty() {
            return Err(AgentError::new("steering-empty", "instruction"));
        }
        self.messages
            .lock()
            .map_err(|_| AgentError::new("steering-buffer-poisoned", "write"))?
            .push(instruction.trim().to_owned());
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Vec<String>, AgentError> {
        self.messages
            .lock()
            .map_err(|_| AgentError::new("steering-buffer-poisoned", "read"))
            .map(|messages| messages.clone())
    }
}

pub struct SteeringAgentHandler {
    inner: Arc<dyn AgentTaskHandler>,
    steering: SharedSteeringBuffer,
}

impl SteeringAgentHandler {
    #[must_use]
    pub fn new(inner: Arc<dyn AgentTaskHandler>, steering: SharedSteeringBuffer) -> Self {
        Self { inner, steering }
    }
}

impl AgentTaskHandler for SteeringAgentHandler {
    fn execute(
        &self,
        mut request: AgentExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<AgentResult, AgentError> {
        request.steering_messages.extend(self.steering.snapshot()?);
        self.inner.execute(request, cancellation)
    }
}

/// 有界线程池式执行器；一次 batch 只创建 `max_parallel` 个短生命周期 worker。
pub struct BoundedAgentExecutor {
    handler: Arc<dyn AgentTaskHandler>,
    max_parallel: usize,
}

/// 不带 Tool 的真实 ModelRuntime Adapter，适合 Planner/Reviewer/Researcher 等只读任务。
pub struct ModelAgentHandler {
    runtime: Arc<ModelRuntime>,
    timeout: Duration,
}

impl ModelAgentHandler {
    pub fn new(runtime: Arc<ModelRuntime>, timeout: Duration) -> Result<Self, AgentError> {
        runtime
            .view()
            .map_err(|error| AgentError::new(error.code, error.message))?;
        if timeout.is_zero() {
            return Err(AgentError::new("agent-model-timeout-zero", "0"));
        }
        Ok(Self { runtime, timeout })
    }
}

impl AgentTaskHandler for ModelAgentHandler {
    fn execute(
        &self,
        mut request: AgentExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<AgentResult, AgentError> {
        let view = self
            .runtime
            .view()
            .map_err(|error| AgentError::new(error.code, error.message))?;
        let existing = request.model_continuation.take();
        let mut state = if let Some(mut continuation) = existing {
            for instruction in &request.steering_messages {
                let input = ModelInputItem::Message {
                    role: ModelMessageRole::User,
                    content: format!("[Steering]\n{instruction}"),
                };
                continuation.next_input.push(input.clone());
                if !continuation.conversation_continuation {
                    continuation.transcript.push(input);
                }
            }
            continuation
        } else {
            let instructions = [
                request.context.stable_instructions.as_str(),
                request.context.dynamic_context.as_str(),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            let mut input = vec![ModelInputItem::Message {
                role: ModelMessageRole::User,
                content: request.contract.objective.clone(),
            }];
            input.extend(request.steering_messages.iter().map(|instruction| {
                ModelInputItem::Message {
                    role: ModelMessageRole::User,
                    content: format!("[Steering]\n{instruction}"),
                }
            }));
            AgentModelContinuation {
                instructions,
                transcript: input.clone(),
                next_input: input,
                tools: request.model_tools,
                previous_response_id: None,
                output: String::new(),
                next_turn: 0,
                max_turns: request.contract.max_turns,
                conversation_continuation: view.capability.conversation_continuation,
                metrics: AgentExecutionMetrics::default(),
                changed_files: Vec::new(),
            }
        };
        if state.next_turn >= state.max_turns {
            return Err(AgentError::new(
                "agent-model-turn-limit",
                request.contract.run_id.to_string(),
            ));
        }
        let model_request = ModelRequest {
            model_id: view.model_id,
            instructions: state.instructions.clone(),
            input: state.next_input.clone(),
            tools: state.tools.clone(),
            reasoning: view.reasoning_requested,
            response_format: ResponseFormat::Text,
            max_output_tokens: view.capability.max_output_tokens.min(2_048),
            previous_response_id: state.previous_response_id.clone(),
            store: false,
            timeout: self.timeout,
        };
        let stream = self
            .runtime
            .stream(model_request, cancellation)
            .map_err(|error| AgentError::new(error.code, error.message))?;
        let mut response_id = None;
        let mut turn_output = String::new();
        let mut tool_calls = Vec::new();
        let mut completed = false;
        for event in stream {
            match event.map_err(|error| AgentError::new(error.code, error.message))? {
                ModelEvent::Started {
                    response_id: started_id,
                    ..
                } => response_id = Some(started_id),
                ModelEvent::TextDelta { delta } => {
                    state.output.push_str(&delta);
                    turn_output.push_str(&delta);
                }
                ModelEvent::Usage { usage } => {
                    state.metrics.input_tokens = state
                        .metrics
                        .input_tokens
                        .saturating_add(usage.input_tokens);
                    state.metrics.output_tokens = state
                        .metrics
                        .output_tokens
                        .saturating_add(usage.output_tokens);
                    state.metrics.reasoning_tokens = state
                        .metrics
                        .reasoning_tokens
                        .saturating_add(usage.reasoning_tokens);
                    state.metrics.cached_input_tokens = state
                        .metrics
                        .cached_input_tokens
                        .saturating_add(usage.cached_input_tokens);
                }
                ModelEvent::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => tool_calls.push(AgentToolCall {
                    call_id,
                    name,
                    arguments,
                }),
                ModelEvent::Completed {
                    status: CompletionStatus::Completed,
                    ..
                } => completed = true,
                ModelEvent::Completed {
                    status: CompletionStatus::Incomplete,
                    incomplete_reason,
                } => {
                    return Err(AgentError::new(
                        "agent-model-incomplete",
                        incomplete_reason.unwrap_or_else(|| "unknown".to_owned()),
                    ));
                }
                ModelEvent::ReasoningSummaryDelta { .. } => {}
            }
        }
        if !completed {
            return Err(AgentError::new(
                "agent-model-no-completion",
                request.contract.run_id.to_string(),
            ));
        }
        state.next_turn = state.next_turn.saturating_add(1);
        state.metrics.turns = state.metrics.turns.saturating_add(1);
        if !tool_calls.is_empty() {
            if !state.conversation_continuation && !turn_output.is_empty() {
                state.transcript.push(ModelInputItem::Message {
                    role: ModelMessageRole::Assistant,
                    content: turn_output,
                });
            }
            state.metrics.tool_calls = state
                .metrics
                .tool_calls
                .saturating_add(u64::try_from(tool_calls.len()).unwrap_or(u64::MAX));
            return Ok(AgentResult {
                status: "waiting-tool".to_owned(),
                summary: state.output.clone(),
                artifacts: vec![],
                changed_files: vec![],
                evidence: vec![],
                warnings: vec![],
                errors: vec![],
                metrics: state.metrics.clone(),
                confidence: 0.5,
                follow_up: vec![],
                model_tool_yield: Some(AgentModelToolYield {
                    continuation: state,
                    response_id,
                    calls: tool_calls,
                }),
            });
        }
        if state.output.trim().is_empty() {
            return Err(AgentError::new(
                "agent-model-empty-result",
                request.contract.run_id.to_string(),
            ));
        }
        Ok(AgentResult {
            status: "completed".to_owned(),
            summary: state.output,
            artifacts: vec![],
            changed_files: state.changed_files,
            evidence: vec![],
            warnings: vec![],
            errors: vec![],
            metrics: state.metrics,
            confidence: 0.8,
            follow_up: vec![],
            model_tool_yield: None,
        })
    }
}

impl BoundedAgentExecutor {
    pub fn new(
        handler: Arc<dyn AgentTaskHandler>,
        max_parallel: usize,
    ) -> Result<Self, AgentError> {
        if max_parallel == 0 {
            return Err(AgentError::new("agent-executor-zero-parallelism", "0"));
        }
        Ok(Self {
            handler,
            max_parallel,
        })
    }

    pub fn execute_batch(
        &self,
        dispatches: Vec<AgentDispatch>,
        now_millis: i64,
    ) -> Result<Vec<AgentExecutionOutcome>, AgentError> {
        let mut run_ids = BTreeSet::new();
        for dispatch in &dispatches {
            let contract = &dispatch.request.contract;
            if !run_ids.insert(contract.run_id.clone()) {
                return Err(AgentError::new(
                    "agent-dispatch-duplicate-run",
                    contract.run_id.to_string(),
                ));
            }
            if contract.objective.trim().is_empty()
                || contract.max_turns == 0
                || dispatch.request.context.fingerprint.trim().is_empty()
            {
                return Err(AgentError::new(
                    "agent-dispatch-invalid",
                    contract.run_id.to_string(),
                ));
            }
            if contract.role == AgentRole::Planner {
                let budget = contract.planning_budget.as_ref().ok_or_else(|| {
                    AgentError::new("planner-budget-missing", contract.run_id.to_string())
                })?;
                if budget.max_planning_iterations == 0
                    || contract.max_turns > budget.max_planning_iterations
                    || budget.max_plan_depth == 0
                    || budget.max_wall_time_millis == 0
                    || budget.max_discovery_actions == 0
                    || dispatch.request.context.token_cost > budget.max_planner_tokens
                {
                    return Err(AgentError::new(
                        "planner-budget-invalid",
                        contract.run_id.to_string(),
                    ));
                }
            }
        }
        if dispatches.is_empty() {
            return Ok(Vec::new());
        }
        let worker_count = self.max_parallel.min(dispatches.len());
        let queue = Arc::new(Mutex::new(VecDeque::from(dispatches)));
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        thread::scope(|scope| {
            for _ in 0..worker_count {
                let queue = queue.clone();
                let outcomes = outcomes.clone();
                let handler = self.handler.clone();
                scope.spawn(move || {
                    loop {
                        let dispatch = queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .pop_front();
                        let Some(dispatch) = dispatch else {
                            break;
                        };
                        let contract = &dispatch.request.contract;
                        let run_id = contract.run_id.clone();
                        let task_id = contract.task_id.clone();
                        let agent_definition_id = contract.agent_definition_id.clone();
                        let execution = if dispatch.cancellation.is_cancelled() {
                            Err(AgentError::new(
                                "agent-run-cancelled-before-start",
                                run_id.to_string(),
                            ))
                        } else if now_millis >= contract.deadline_millis {
                            dispatch.cancellation.cancel();
                            Err(AgentError::new(
                                "agent-run-deadline-exceeded",
                                run_id.to_string(),
                            ))
                        } else {
                            catch_unwind(AssertUnwindSafe(|| {
                                handler.execute(dispatch.request, dispatch.cancellation)
                            }))
                            .unwrap_or_else(|_| {
                                Err(AgentError::new(
                                    "agent-handler-panicked",
                                    run_id.to_string(),
                                ))
                            })
                        };
                        let (result, error) = match execution {
                            Ok(result) => (Some(result), None),
                            Err(error) => (None, Some(error)),
                        };
                        outcomes
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(AgentExecutionOutcome {
                                run_id,
                                task_id,
                                agent_definition_id,
                                result,
                                error,
                            });
                    }
                });
            }
        });
        let mut outcomes = Arc::try_unwrap(outcomes)
            .map_err(|_| AgentError::new("agent-executor-outcomes-shared", "worker leak"))?
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        outcomes.sort_by(|left, right| {
            left.task_id
                .cmp(&right.task_id)
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        Ok(outcomes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunActivity {
    Running,
    CancelRequested,
}

#[derive(Clone, Debug)]
struct RunControl {
    parent: Option<RunId>,
    children: BTreeSet<RunId>,
    token: CancellationToken,
    activity: RunActivity,
}

/// Structured concurrency：父 Run 的生命周期严格覆盖所有子 Run。
#[derive(Default)]
pub struct RunCancellationTree {
    runs: BTreeMap<RunId, RunControl>,
}

impl RunCancellationTree {
    pub fn register(
        &mut self,
        run_id: RunId,
        parent: Option<RunId>,
    ) -> Result<CancellationToken, AgentError> {
        if self.runs.contains_key(&run_id) {
            return Err(AgentError::new("run-control-exists", run_id.to_string()));
        }
        if let Some(parent_id) = &parent {
            let parent_control = self.runs.get_mut(parent_id).ok_or_else(|| {
                AgentError::new("parent-run-control-missing", parent_id.to_string())
            })?;
            if parent_control.activity == RunActivity::CancelRequested {
                return Err(AgentError::new(
                    "parent-run-cancelling",
                    parent_id.to_string(),
                ));
            }
            parent_control.children.insert(run_id.clone());
        }
        let token = CancellationToken::new();
        self.runs.insert(
            run_id,
            RunControl {
                parent,
                children: BTreeSet::new(),
                token: token.clone(),
                activity: RunActivity::Running,
            },
        );
        Ok(token)
    }

    #[must_use]
    pub fn token(&self, run_id: &RunId) -> Option<CancellationToken> {
        self.runs.get(run_id).map(|control| control.token.clone())
    }

    pub fn cancel_subtree(&mut self, run_id: &RunId) -> Result<Vec<RunId>, AgentError> {
        if !self.runs.contains_key(run_id) {
            return Err(AgentError::new("run-control-missing", run_id.to_string()));
        }
        let mut stack = vec![run_id.clone()];
        let mut cancelled = Vec::new();
        while let Some(current) = stack.pop() {
            let control = self
                .runs
                .get_mut(&current)
                .expect("子 Run 来自已登记控制树");
            stack.extend(control.children.iter().cloned());
            control.activity = RunActivity::CancelRequested;
            control.token.cancel();
            cancelled.push(current);
        }
        cancelled.sort();
        Ok(cancelled)
    }

    pub fn finish(&mut self, run_id: &RunId) -> Result<(), AgentError> {
        let control = self
            .runs
            .get(run_id)
            .ok_or_else(|| AgentError::new("run-control-missing", run_id.to_string()))?;
        if !control.children.is_empty() {
            return Err(AgentError::new(
                "run-has-active-children",
                format!("{} children={}", run_id, control.children.len()),
            ));
        }
        let parent = control.parent.clone();
        self.runs.remove(run_id);
        if let Some(parent) = parent
            && let Some(parent_control) = self.runs.get_mut(&parent)
        {
            parent_control.children.remove(run_id);
        }
        Ok(())
    }

    #[must_use]
    pub fn active_run_ids(&self) -> Vec<RunId> {
        self.runs.keys().cloned().collect()
    }
}
