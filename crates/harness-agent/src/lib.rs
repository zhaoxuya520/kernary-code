#![forbid(unsafe_code)]

//! Agent Runtime policy。Mission Kernel 仍是唯一任务状态机，本 crate 不复制 DAG/Event/Run 状态。

mod budget;
mod execution;
mod state_store;

pub use budget::{
    AgentBudgetEscrow, AgentBudgetManager, AgentBudgetPolicy, AgentBudgetRequest,
    BudgetEscrowStatus,
};
pub use execution::{
    AgentDispatch, AgentEndpoint, AgentEndpointStatus, AgentExecutionMetrics,
    AgentExecutionOutcome, AgentExecutionRequest, AgentModelContinuation, AgentModelToolYield,
    AgentSession, AgentSessionStatus, AgentTaskContract, AgentTaskHandler, AgentToolCall,
    AgentWorkingContext, BoundedAgentExecutor, ModelAgentHandler, PlanningBudget,
    RunCancellationTree, SharedSteeringBuffer, SteeringAgentHandler,
};
pub use state_store::AgentStateStore;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use harness_kernel::{MissionState, find_ready_node_ids};
use harness_types::{AgentDefinitionId, ArtifactId, MissionId, RunId, TaskId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentError {
    pub code: String,
    pub message: String,
}
impl AgentError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl Display for AgentError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl Error for AgentError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentRole {
    RequirementsAnalyst,
    Explorer,
    Architect,
    Planner,
    Coder,
    Reviewer,
    SecurityAuditor,
    PerformanceEngineer,
    Tester,
    ReleaseManager,
    Debugger,
    Researcher,
    MergeAgent,
    Coordinator,
    StaffingRouter,
    Supervisor,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentLifecycle {
    Sleeping,
    Reserved,
    Running,
    Draining,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDefinition {
    pub id: AgentDefinitionId,
    pub name: String,
    pub roles: BTreeSet<AgentRole>,
    pub capabilities: BTreeSet<String>,
    pub allowed_tools: BTreeSet<String>,
    pub max_concurrency: usize,
    pub cost_weight: u32,
    pub integrity_floor: String,
}

#[derive(Default)]
pub struct AgentCatalog {
    definitions: BTreeMap<AgentDefinitionId, AgentDefinition>,
    reserved: BTreeMap<AgentDefinitionId, usize>,
    running: BTreeMap<AgentDefinitionId, usize>,
}
impl AgentCatalog {
    pub fn register(&mut self, definition: AgentDefinition) -> Result<(), AgentError> {
        if definition.name.trim().is_empty()
            || definition.capabilities.is_empty()
            || definition.max_concurrency == 0
        {
            return Err(AgentError::new(
                "agent-definition-invalid",
                definition.id.to_string(),
            ));
        }
        // 协调员和分配员只能观察、路由与记录，不能同时承担写代码职责。
        let is_control_plane = definition.roles.contains(&AgentRole::Coordinator)
            || definition.roles.contains(&AgentRole::StaffingRouter);
        if is_control_plane && definition.roles.contains(&AgentRole::Coder) {
            return Err(AgentError::new(
                "agent-control-plane-cannot-code",
                definition.id.to_string(),
            ));
        }
        let id = definition.id.clone();
        if self.definitions.contains_key(&id) {
            return Err(AgentError::new("agent-definition-exists", "duplicate"));
        }
        self.definitions.insert(id.clone(), definition);
        self.reserved.insert(id.clone(), 0);
        self.running.insert(id, 0);
        Ok(())
    }
    pub fn list(&self) -> Vec<AgentDefinition> {
        self.definitions.values().cloned().collect()
    }
    #[must_use]
    pub fn definition(&self, id: &AgentDefinitionId) -> Option<&AgentDefinition> {
        self.definitions.get(id)
    }
    pub fn lifecycle(&self, id: &AgentDefinitionId) -> Option<AgentLifecycle> {
        self.definitions.get(id).map(|_| {
            if self.running.get(id).copied().unwrap_or(0) > 0 {
                AgentLifecycle::Running
            } else if self.reserved.get(id).copied().unwrap_or(0) > 0 {
                AgentLifecycle::Reserved
            } else {
                AgentLifecycle::Sleeping
            }
        })
    }
    #[must_use]
    pub fn active_count(&self, id: &AgentDefinitionId) -> usize {
        self.reserved
            .get(id)
            .copied()
            .unwrap_or(0)
            .saturating_add(self.running.get(id).copied().unwrap_or(0))
    }
    fn available(&self, id: &AgentDefinitionId) -> bool {
        self.definitions
            .get(id)
            .is_some_and(|definition| self.active_count(id) < definition.max_concurrency)
    }
    pub fn reserve(&mut self, id: &AgentDefinitionId) -> Result<(), AgentError> {
        if !self.available(id) {
            return Err(AgentError::new("agent-not-available", id.to_string()));
        }
        *self.reserved.entry(id.clone()).or_default() += 1;
        Ok(())
    }
    pub fn start(&mut self, id: &AgentDefinitionId) -> Result<(), AgentError> {
        let reserved = self
            .reserved
            .get_mut(id)
            .ok_or_else(|| AgentError::new("agent-not-found", id.to_string()))?;
        if *reserved == 0 {
            return Err(AgentError::new("agent-not-reserved", id.to_string()));
        }
        *reserved -= 1;
        *self.running.entry(id.clone()).or_default() += 1;
        Ok(())
    }
    pub fn release(&mut self, id: &AgentDefinitionId) -> Result<(), AgentError> {
        let running = self
            .running
            .get_mut(id)
            .ok_or_else(|| AgentError::new("agent-not-found", id.to_string()))?;
        if *running > 0 {
            *running -= 1;
            return Ok(());
        }
        let reserved = self.reserved.get_mut(id).expect("已检查 Agent 存在");
        if *reserved > 0 {
            *reserved -= 1;
            return Ok(());
        }
        Err(AgentError::new("agent-not-active", id.to_string()))
    }
}

/// 内置 Agent 只注册能力元数据，启动时全部保持 Sleeping。
pub fn builtin_agent_catalog() -> Result<AgentCatalog, AgentError> {
    let definitions = [
        builtin_definition(
            "agent:staffing-router",
            "专职分配员",
            AgentRole::StaffingRouter,
            &["capability-routing", "capacity-planning"],
            &[],
            1,
            1,
        ),
        builtin_definition(
            "agent:coordinator",
            "协调记录员",
            AgentRole::Coordinator,
            &["coordination", "conflict-detection", "meeting-record"],
            &["message.read", "message.write", "memory.write"],
            1,
            2,
        ),
        builtin_definition(
            "agent:requirements",
            "需求分析师",
            AgentRole::RequirementsAnalyst,
            &[
                "requirements-analysis",
                "acceptance-criteria",
                "ambiguity-detection",
                "scope-control",
            ],
            &["memory.read", "repository.read"],
            2,
            2,
        ),
        builtin_definition(
            "agent:explorer",
            "代码库探索员",
            AgentRole::Explorer,
            &[
                "codebase-exploration",
                "dependency-tracing",
                "repository-map",
                "symbol-discovery",
            ],
            &["repository.read", "lsp.read"],
            8,
            1,
        ),
        builtin_definition(
            "agent:architect",
            "架构师",
            AgentRole::Architect,
            &[
                "system-design",
                "boundary-analysis",
                "architecture-decision",
                "risk-analysis",
            ],
            &["repository.read", "memory.read", "memory.write"],
            2,
            4,
        ),
        builtin_definition(
            "agent:planner",
            "规划员",
            AgentRole::Planner,
            &["task-decomposition", "dependency-analysis"],
            &["repository.read", "memory.read"],
            2,
            3,
        ),
        builtin_definition(
            "agent:coder",
            "编码员",
            AgentRole::Coder,
            &["code-edit", "rust", "typescript"],
            &["repository.read", "file.write", "process.run"],
            4,
            5,
        ),
        builtin_definition(
            "agent:reviewer",
            "审查员",
            AgentRole::Reviewer,
            &["code-review", "conflict-detection"],
            &["repository.read", "diff.read"],
            2,
            3,
        ),
        builtin_definition(
            "agent:security",
            "安全审计员",
            AgentRole::SecurityAuditor,
            &[
                "security-audit",
                "threat-model",
                "vulnerability-analysis",
                "supply-chain-review",
            ],
            &["repository.read", "diff.read", "network.read"],
            2,
            4,
        ),
        builtin_definition(
            "agent:performance",
            "性能工程师",
            AgentRole::PerformanceEngineer,
            &[
                "performance-analysis",
                "benchmark",
                "profiling",
                "regression-analysis",
            ],
            &["repository.read", "process.run"],
            2,
            4,
        ),
        builtin_definition(
            "agent:tester",
            "测试员",
            AgentRole::Tester,
            &["test-execution", "evidence"],
            &["repository.read", "process.run"],
            2,
            3,
        ),
        builtin_definition(
            "agent:release",
            "发布经理",
            AgentRole::ReleaseManager,
            &[
                "release-readiness",
                "artifact-verification",
                "version-policy",
                "rollback-planning",
            ],
            &["repository.read", "diff.read", "process.run"],
            1,
            3,
        ),
        builtin_definition(
            "agent:debugger",
            "调试员",
            AgentRole::Debugger,
            &["diagnosis", "failure-localization"],
            &["repository.read", "process.run"],
            2,
            4,
        ),
        builtin_definition(
            "agent:researcher",
            "研究员",
            AgentRole::Researcher,
            &["documentation", "research"],
            &["repository.read", "network.read"],
            8,
            4,
        ),
        builtin_definition(
            "agent:merge",
            "合并员",
            AgentRole::MergeAgent,
            &["code-merge", "conflict-resolution", "code-edit"],
            &["repository.read", "diff.read", "file.write", "process.run"],
            1,
            5,
        ),
    ];
    let mut catalog = AgentCatalog::default();
    for definition in definitions {
        catalog.register(definition)?;
    }
    Ok(catalog)
}

fn builtin_definition(
    id: &str,
    name: &str,
    role: AgentRole,
    capabilities: &[&str],
    allowed_tools: &[&str],
    max_concurrency: usize,
    cost_weight: u32,
) -> AgentDefinition {
    AgentDefinition {
        id: AgentDefinitionId::from(id),
        name: name.to_owned(),
        roles: [role].into_iter().collect(),
        capabilities: capabilities
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        allowed_tools: allowed_tools
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        max_concurrency,
        cost_weight,
        integrity_floor: "trusted".to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaffingTask {
    pub task_id: TaskId,
    pub required_capabilities: BTreeSet<String>,
    pub preferred_roles: BTreeSet<AgentRole>,
    pub forbidden_agents: BTreeSet<AgentDefinitionId>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StaffingAssignment {
    pub task_id: TaskId,
    pub agent_id: AgentDefinitionId,
    pub score: i64,
    pub reason_summary: String,
    pub catalog_fingerprint: String,
}
pub struct StaffingRouter;
impl StaffingRouter {
    pub fn assign(
        tasks: &[StaffingTask],
        catalog: &AgentCatalog,
    ) -> Result<Vec<StaffingAssignment>, AgentError> {
        let fingerprint = catalog_fingerprint(catalog);
        let mut load = BTreeMap::<AgentDefinitionId, usize>::new();
        let mut output = Vec::new();
        for task in tasks {
            let mut candidates = catalog
                .definitions
                .values()
                .filter(|agent| {
                    let already_assigned = load.get(&agent.id).copied().unwrap_or(0);
                    let already_active = catalog.active_count(&agent.id);
                    !task.forbidden_agents.contains(&agent.id)
                        && task.required_capabilities.is_subset(&agent.capabilities)
                        && catalog.available(&agent.id)
                        && already_active.saturating_add(already_assigned) < agent.max_concurrency
                })
                .map(|agent| {
                    let role_matches =
                        task.preferred_roles.intersection(&agent.roles).count() as i64;
                    let assigned = load.get(&agent.id).copied().unwrap_or(0) as i64;
                    let score = role_matches * 100 - (agent.cost_weight as i64) - assigned * 25;
                    (score, agent.id.clone())
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let (score, agent_id) = candidates.into_iter().next().ok_or_else(|| {
                AgentError::new("staffing-no-capable-agent", task.task_id.to_string())
            })?;
            *load.entry(agent_id.clone()).or_default() += 1;
            output.push(StaffingAssignment {
                task_id: task.task_id.clone(),
                agent_id,
                score,
                reason_summary: format!(
                    "required={} preferredRoles={}",
                    task.required_capabilities.len(),
                    task.preferred_roles.len()
                ),
                catalog_fingerprint: fingerprint.clone(),
            });
        }
        Ok(output)
    }
}

fn catalog_fingerprint(catalog: &AgentCatalog) -> String {
    let bytes =
        serde_json::to_vec(&catalog.definitions.values().collect::<Vec<_>>()).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledRun {
    pub task_id: TaskId,
    pub agent_id: AgentDefinitionId,
    pub priority: i32,
    pub estimated_cost_units: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulingDeferral {
    pub task_id: TaskId,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleBatch {
    pub runs: Vec<ScheduledRun>,
    pub deferred: Vec<SchedulingDeferral>,
    pub total_cost_units: u64,
}
pub struct AgentScheduler {
    pub concurrency_limit: usize,
}
impl AgentScheduler {
    pub fn schedule(
        &self,
        mission: &MissionState,
        assignments: &[StaffingAssignment],
        priorities: &BTreeMap<TaskId, i32>,
    ) -> Vec<ScheduledRun> {
        self.schedule_with_constraints(
            mission,
            assignments,
            priorities,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            u64::MAX,
        )
        .runs
    }

    /// 等待轮次提供抗饥饿提升；取消集合和批次预算在唤醒 Agent 前生效。
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_with_constraints(
        &self,
        mission: &MissionState,
        assignments: &[StaffingAssignment],
        priorities: &BTreeMap<TaskId, i32>,
        wait_cycles: &BTreeMap<TaskId, u32>,
        estimated_costs: &BTreeMap<TaskId, u64>,
        cancelled_tasks: &BTreeSet<TaskId>,
        max_total_cost_units: u64,
    ) -> ScheduleBatch {
        let by_task = assignments
            .iter()
            .map(|a| (a.task_id.clone(), a.agent_id.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut ready = find_ready_node_ids(mission)
            .into_iter()
            .filter_map(|task_id| {
                by_task.get(&task_id).cloned().map(|agent_id| ScheduledRun {
                    priority: priorities
                        .get(&task_id)
                        .copied()
                        .unwrap_or(0)
                        .saturating_add(
                            i32::try_from(wait_cycles.get(&task_id).copied().unwrap_or(0))
                                .unwrap_or(i32::MAX)
                                .saturating_mul(10),
                        ),
                    estimated_cost_units: estimated_costs.get(&task_id).copied().unwrap_or(1),
                    task_id,
                    agent_id,
                })
            })
            .collect::<Vec<_>>();
        ready.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.task_id.cmp(&b.task_id))
        });
        let mut runs = Vec::new();
        let mut deferred = Vec::new();
        let mut total_cost_units = 0_u64;
        for candidate in ready {
            if cancelled_tasks.contains(&candidate.task_id) {
                deferred.push(SchedulingDeferral {
                    task_id: candidate.task_id,
                    reason: "cancelled-before-dispatch".to_owned(),
                });
                continue;
            }
            if runs.len() >= self.concurrency_limit {
                deferred.push(SchedulingDeferral {
                    task_id: candidate.task_id,
                    reason: "global-concurrency-limit".to_owned(),
                });
                continue;
            }
            if total_cost_units.saturating_add(candidate.estimated_cost_units)
                > max_total_cost_units
            {
                deferred.push(SchedulingDeferral {
                    task_id: candidate.task_id,
                    reason: "batch-cost-budget".to_owned(),
                });
                continue;
            }
            total_cost_units = total_cost_units.saturating_add(candidate.estimated_cost_units);
            runs.push(candidate);
        }
        ScheduleBatch {
            runs,
            deferred,
            total_cost_units,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMessageKind {
    Task,
    Result,
    Question,
    ContextRequest,
    ContextResponse,
    Review,
    Error,
    Event,
    Cancel,
    Steering,
    Meeting,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentMessage {
    pub id: String,
    pub idempotency_key: String,
    pub mission_id: MissionId,
    pub from: String,
    pub to: String,
    pub kind: AgentMessageKind,
    pub payload: serde_json::Value,
    pub sequence: u64,
    pub created_at_millis: i64,
    pub acknowledged_at_millis: Option<i64>,
}

/// 一次带租约的消息领取。只有持有当前令牌的消费者才能确认消息。
#[derive(Clone, Debug, PartialEq)]
pub struct ClaimedMessage {
    pub message: AgentMessage,
    pub delivery_token: String,
    pub lease_expires_at_millis: i64,
}

pub struct AgentMessageBus {
    connection: Connection,
}
impl AgentMessageBus {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AgentError> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let connection = Connection::open(path).map_err(sql_error)?;
        connection.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE IF NOT EXISTS agent_messages(id TEXT PRIMARY KEY,idempotency_key TEXT NOT NULL UNIQUE,mission_id TEXT NOT NULL,sender TEXT NOT NULL,recipient TEXT NOT NULL,kind_json TEXT NOT NULL,payload_json TEXT NOT NULL,sequence INTEGER NOT NULL,created_at_millis INTEGER NOT NULL,acknowledged_at_millis INTEGER,delivery_token TEXT,lease_expires_at_millis INTEGER);CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_recipient_sequence ON agent_messages(recipient,sequence);CREATE TABLE IF NOT EXISTS agent_meetings(id TEXT PRIMARY KEY,mission_id TEXT NOT NULL,record_json TEXT NOT NULL,updated_at_millis INTEGER NOT NULL);").map_err(sql_error)?;
        ensure_column(&connection, "agent_messages", "delivery_token", "TEXT")?;
        ensure_column(
            &connection,
            "agent_messages",
            "lease_expires_at_millis",
            "INTEGER",
        )?;
        Ok(Self { connection })
    }
    pub fn send(&mut self, message: AgentMessage) -> Result<AgentMessage, AgentError> {
        if let Some(existing) = self.by_key(&message.idempotency_key)? {
            return Ok(existing);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let sequence: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(sequence),0)+1 FROM agent_messages WHERE recipient=?1",
                [&message.to],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        tx.execute(
            "INSERT INTO agent_messages(id,idempotency_key,mission_id,sender,recipient,kind_json,payload_json,sequence,created_at_millis,acknowledged_at_millis,delivery_token,lease_expires_at_millis) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL,NULL,NULL)",
            params![
                message.id,
                message.idempotency_key,
                message.mission_id.to_string(),
                message.from,
                message.to,
                serde_json::to_string(&message.kind).map_err(json_error)?,
                serde_json::to_string(&message.payload).map_err(json_error)?,
                sequence,
                message.created_at_millis
            ],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        self.by_key(&message.idempotency_key)?
            .ok_or_else(|| AgentError::new("agent-message-missing-after-send", message.id))
    }
    pub fn pending(
        &self,
        recipient: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<AgentMessage>, AgentError> {
        // 这是管理/调试用只读投影；执行器必须使用 claim，不能用本方法消费。
        let mut stmt=self.connection.prepare("SELECT id,idempotency_key,mission_id,sender,recipient,kind_json,payload_json,sequence,created_at_millis,acknowledged_at_millis FROM agent_messages WHERE recipient=?1 AND sequence>?2 AND acknowledged_at_millis IS NULL ORDER BY sequence LIMIT ?3").map_err(sql_error)?;
        let rows = stmt
            .query_map(params![recipient, after, limit.clamp(1, 1000)], row_message)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    /// 原子领取消息并设置可恢复租约，避免多个执行器同时看见同一条消息。
    pub fn claim(
        &mut self,
        recipient: &str,
        after: u64,
        limit: usize,
        delivery_token: &str,
        now: i64,
        lease_millis: i64,
    ) -> Result<Vec<ClaimedMessage>, AgentError> {
        self.claim_scoped(
            recipient,
            None,
            after,
            limit,
            delivery_token,
            now,
            lease_millis,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claim_for_mission(
        &mut self,
        recipient: &str,
        mission_id: &MissionId,
        after: u64,
        limit: usize,
        delivery_token: &str,
        now: i64,
        lease_millis: i64,
    ) -> Result<Vec<ClaimedMessage>, AgentError> {
        self.claim_scoped(
            recipient,
            Some(mission_id),
            after,
            limit,
            delivery_token,
            now,
            lease_millis,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn claim_scoped(
        &mut self,
        recipient: &str,
        mission_id: Option<&MissionId>,
        after: u64,
        limit: usize,
        delivery_token: &str,
        now: i64,
        lease_millis: i64,
    ) -> Result<Vec<ClaimedMessage>, AgentError> {
        if delivery_token.trim().is_empty() {
            return Err(AgentError::new("delivery-token-empty", recipient));
        }
        let mission_id = mission_id.map(ToString::to_string);
        let lease_expires_at_millis = now.saturating_add(lease_millis.max(1));
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let ids = {
            let mut stmt = tx
                .prepare("SELECT id FROM agent_messages WHERE recipient=?1 AND (?2 IS NULL OR mission_id=?2) AND sequence>?3 AND acknowledged_at_millis IS NULL AND (delivery_token IS NULL OR lease_expires_at_millis<=?4) ORDER BY sequence LIMIT ?5")
                .map_err(sql_error)?;
            let rows = stmt
                .query_map(
                    params![recipient, mission_id, after, now, limit.clamp(1, 1000)],
                    |row| row.get::<_, String>(0),
                )
                .map_err(sql_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?
        };
        for id in &ids {
            tx.execute(
                "UPDATE agent_messages SET delivery_token=?2,lease_expires_at_millis=?3 WHERE id=?1 AND acknowledged_at_millis IS NULL AND (delivery_token IS NULL OR lease_expires_at_millis<=?4)",
                params![id, delivery_token, lease_expires_at_millis, now],
            )
            .map_err(sql_error)?;
        }
        let claimed = {
            let mut stmt = tx
                .prepare("SELECT id,idempotency_key,mission_id,sender,recipient,kind_json,payload_json,sequence,created_at_millis,acknowledged_at_millis FROM agent_messages WHERE delivery_token=?1 AND acknowledged_at_millis IS NULL ORDER BY sequence")
                .map_err(sql_error)?;
            let rows = stmt
                .query_map([delivery_token], row_message)
                .map_err(sql_error)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .filter(|message| ids.contains(&message.id))
                .map(|message| ClaimedMessage {
                    message,
                    delivery_token: delivery_token.to_owned(),
                    lease_expires_at_millis,
                })
                .collect::<Vec<_>>()
        };
        tx.commit().map_err(sql_error)?;
        Ok(claimed)
    }

    /// 仅当前租约持有者可以确认；旧消费者迟到时不会误确认新领取者的工作。
    pub fn acknowledge_claim(
        &self,
        id: &str,
        delivery_token: &str,
        now: i64,
    ) -> Result<bool, AgentError> {
        Ok(self
            .connection
            .execute(
                "UPDATE agent_messages SET acknowledged_at_millis=?3,delivery_token=NULL,lease_expires_at_millis=NULL WHERE id=?1 AND delivery_token=?2 AND acknowledged_at_millis IS NULL AND lease_expires_at_millis>?3",
                params![id, delivery_token, now],
            )
            .map_err(sql_error)?
            > 0)
    }
    /// 会议记录与消息使用同一 durable SQLite 边界，重启后仍可追溯。
    pub fn save_meeting(&self, meeting: &MeetingRecord, now: i64) -> Result<(), AgentError> {
        self.connection
            .execute(
                "INSERT INTO agent_meetings(id,mission_id,record_json,updated_at_millis) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET record_json=excluded.record_json,updated_at_millis=excluded.updated_at_millis",
                params![
                    meeting.id,
                    meeting.mission_id.to_string(),
                    serde_json::to_string(meeting).map_err(json_error)?,
                    now
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }
    pub fn meeting(&self, id: &str) -> Result<Option<MeetingRecord>, AgentError> {
        self.connection
            .query_row(
                "SELECT record_json FROM agent_meetings WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?
            .map(|json| serde_json::from_str(&json).map_err(json_error))
            .transpose()
    }
    fn by_key(&self, key: &str) -> Result<Option<AgentMessage>, AgentError> {
        self.connection.query_row("SELECT id,idempotency_key,mission_id,sender,recipient,kind_json,payload_json,sequence,created_at_millis,acknowledged_at_millis FROM agent_messages WHERE idempotency_key=?1",[key],row_message).optional().map_err(sql_error)
    }
}

fn row_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentMessage> {
    let kind: String = row.get(5)?;
    let payload: String = row.get(6)?;
    Ok(AgentMessage {
        id: row.get(0)?,
        idempotency_key: row.get(1)?,
        mission_id: MissionId::from(row.get::<_, String>(2)?),
        from: row.get(3)?,
        to: row.get(4)?,
        kind: serde_json::from_str(&kind).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?,
        payload: serde_json::from_str(&payload).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?,
        sequence: row.get(7)?,
        created_at_millis: row.get(8)?,
        acknowledged_at_millis: row.get(9)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLease {
    pub path: PathBuf,
    pub owner_run: RunId,
    pub fence: u64,
    pub expires_at_millis: i64,
}
pub struct FileLeaseManager {
    root: PathBuf,
    connection: Connection,
}
impl FileLeaseManager {
    pub fn open(root: impl AsRef<Path>, database: impl AsRef<Path>) -> Result<Self, AgentError> {
        let root = fs::canonicalize(root).map_err(io_error)?;
        if let Some(parent) = database.as_ref().parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let connection = Connection::open(database).map_err(sql_error)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS file_leases(path TEXT PRIMARY KEY,owner_run TEXT NOT NULL,fence INTEGER NOT NULL,expires_at_millis INTEGER NOT NULL);").map_err(sql_error)?;
        Ok(Self { root, connection })
    }
    pub fn acquire(
        &mut self,
        path: &Path,
        owner_run: RunId,
        now: i64,
        ttl: i64,
    ) -> Result<FileLease, AgentError> {
        let path = resolve_write(&self.root, path)?;
        let key = normalized(&path);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        tx.execute("DELETE FROM file_leases WHERE expires_at_millis<=?1", [now])
            .map_err(sql_error)?;
        let rows = tx
            .prepare("SELECT path,owner_run FROM file_leases")
            .map_err(sql_error)?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        if rows
            .iter()
            .any(|(leased, owner)| owner != owner_run.as_str() && paths_overlap(leased, &key))
        {
            return Err(AgentError::new("file-lease-conflict", key));
        }
        let fence: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(fence),0)+1 FROM file_leases",
                [],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        let expires = now.saturating_add(ttl.max(1));
        tx.execute("INSERT INTO file_leases VALUES(?1,?2,?3,?4) ON CONFLICT(path) DO UPDATE SET owner_run=excluded.owner_run,fence=excluded.fence,expires_at_millis=excluded.expires_at_millis",params![key,owner_run.to_string(),fence,expires]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(FileLease {
            path,
            owner_run,
            fence,
            expires_at_millis: expires,
        })
    }
    pub fn release(&self, lease: &FileLease) -> Result<bool, AgentError> {
        Ok(self
            .connection
            .execute(
                "DELETE FROM file_leases WHERE path=?1 AND owner_run=?2 AND fence=?3",
                params![
                    normalized(&lease.path),
                    lease.owner_run.to_string(),
                    lease.fence
                ],
            )
            .map_err(sql_error)?
            > 0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Evidence {
    pub kind: String,
    pub reference: String,
    pub summary: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentResult {
    pub status: String,
    pub summary: String,
    pub artifacts: Vec<ArtifactId>,
    pub changed_files: Vec<PathBuf>,
    pub evidence: Vec<Evidence>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub metrics: AgentExecutionMetrics,
    pub confidence: f32,
    pub follow_up: Vec<String>,
    pub model_tool_yield: Option<AgentModelToolYield>,
}

/// 子 Agent 只返回压缩结果、证据和显式决策，不把整段私有上下文灌回主会话。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentResultEnvelope {
    pub agent_id: AgentDefinitionId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub result: AgentResult,
    pub decision_claims: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictKind {
    FileOverlap,
    DecisionMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DetectedConflict {
    pub kind: ConflictKind,
    pub left_agent: AgentDefinitionId,
    pub right_agent: AgentDefinitionId,
    pub subject: String,
    pub left_value: String,
    pub right_value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinationOutcome {
    pub conflicts: Vec<DetectedConflict>,
    pub meeting: Option<MeetingRecord>,
}

pub struct Coordinator;
impl Coordinator {
    /// 旁观检查所有子 Agent 结果；发现冲突时自动发起会议，但不修改代码。
    pub fn inspect(
        mission_id: MissionId,
        results: &[AgentResultEnvelope],
        meeting_id: impl Into<String>,
        now: i64,
    ) -> CoordinationOutcome {
        let conflicts = detect_conflicts(results);
        let meeting = (!conflicts.is_empty()).then(|| {
            let mut participants = results
                .iter()
                .map(|result| result.agent_id.to_string())
                .collect::<BTreeSet<_>>();
            participants.insert("agent:coordinator".to_owned());
            MeetingRecord {
                id: meeting_id.into(),
                mission_id,
                topic: "自动检测到子 Agent 方案冲突".to_owned(),
                participants,
                transcript: Vec::new(),
                decisions: Vec::new(),
                conflicts: conflicts.iter().map(format_conflict).collect(),
                created_at_millis: now,
                closed_at_millis: None,
            }
        });
        CoordinationOutcome { conflicts, meeting }
    }
}

#[must_use]
pub fn detect_conflicts(results: &[AgentResultEnvelope]) -> Vec<DetectedConflict> {
    let mut conflicts = Vec::new();
    for left_index in 0..results.len() {
        for right in &results[left_index + 1..] {
            let left = &results[left_index];
            for left_path in &left.result.changed_files {
                for right_path in &right.result.changed_files {
                    if paths_overlap(&normalized(left_path), &normalized(right_path)) {
                        conflicts.push(DetectedConflict {
                            kind: ConflictKind::FileOverlap,
                            left_agent: left.agent_id.clone(),
                            right_agent: right.agent_id.clone(),
                            subject: "changed_files".to_owned(),
                            left_value: normalized(left_path),
                            right_value: normalized(right_path),
                        });
                    }
                }
            }
            for (key, left_value) in &left.decision_claims {
                if let Some(right_value) = right.decision_claims.get(key)
                    && left_value.trim() != right_value.trim()
                {
                    conflicts.push(DetectedConflict {
                        kind: ConflictKind::DecisionMismatch,
                        left_agent: left.agent_id.clone(),
                        right_agent: right.agent_id.clone(),
                        subject: key.clone(),
                        left_value: left_value.clone(),
                        right_value: right_value.clone(),
                    });
                }
            }
        }
    }
    conflicts
}

fn format_conflict(conflict: &DetectedConflict) -> String {
    format!(
        "{:?}: {} <> {} · {} · {} <> {}",
        conflict.kind,
        conflict.left_agent,
        conflict.right_agent,
        conflict.subject,
        conflict.left_value,
        conflict.right_value
    )
}
pub fn validate_acceptance(
    result: &AgentResult,
    requires_test: bool,
    requires_review: bool,
) -> Result<(), AgentError> {
    let mut required = Vec::new();
    if requires_test {
        required.push("test");
    }
    if requires_review {
        required.push("review");
    }
    validate_required_evidence(result, &required)
}

pub fn validate_required_evidence(
    result: &AgentResult,
    required_evidence: &[&str],
) -> Result<(), AgentError> {
    if result.status != "completed" || result.summary.trim().is_empty() {
        return Err(AgentError::new("agent-result-incomplete", "status/summary"));
    }
    if !(0.0..=1.0).contains(&result.confidence) || !result.confidence.is_finite() {
        return Err(AgentError::new("agent-result-confidence", "range"));
    }
    for required in required_evidence {
        if !result
            .evidence
            .iter()
            .any(|evidence| evidence.kind == *required)
        {
            return Err(AgentError::new(
                "agent-evidence-required",
                (*required).to_owned(),
            ));
        }
    }
    if result.evidence.iter().any(|evidence| {
        evidence.kind.trim().is_empty()
            || evidence.reference.trim().is_empty()
            || evidence.summary.trim().is_empty()
    }) {
        return Err(AgentError::new(
            "agent-evidence-invalid",
            "kind/reference/summary",
        ));
    }
    if !required_evidence.is_empty() && result.metrics.turns == 0 {
        return Err(AgentError::new("agent-evidence-metrics-missing", "turns=0"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MeetingRecord {
    pub id: String,
    pub mission_id: MissionId,
    pub topic: String,
    pub participants: BTreeSet<String>,
    pub transcript: Vec<String>,
    pub decisions: Vec<String>,
    pub conflicts: Vec<String>,
    pub created_at_millis: i64,
    pub closed_at_millis: Option<i64>,
}
impl MeetingRecord {
    pub fn append(&mut self, speaker: &str, message: &str) -> Result<(), AgentError> {
        if self.closed_at_millis.is_some() {
            return Err(AgentError::new("meeting-closed", &self.id));
        }
        if !self.participants.contains(speaker) {
            return Err(AgentError::new("meeting-speaker-not-participant", speaker));
        }
        self.transcript
            .push(format!("{speaker}: {}", message.trim()));
        Ok(())
    }
    pub fn decide(&mut self, decision: &str) {
        self.decisions.push(decision.trim().to_owned());
    }
    pub fn conflict(&mut self, conflict: &str) {
        self.conflicts.push(conflict.trim().to_owned());
    }
    pub fn close(&mut self, now: i64) {
        self.closed_at_millis = Some(now);
    }
}

fn paths_overlap(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left == right || left.starts_with(right) || right.starts_with(left)
}
fn resolve_write(root: &Path, path: &Path) -> Result<PathBuf, AgentError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let mut existing = joined.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| AgentError::new("lease-parent-missing", joined.display().to_string()))?;
    }
    let canonical = fs::canonicalize(existing).map_err(io_error)?;
    if !canonical.starts_with(root) {
        return Err(AgentError::new(
            "lease-path-outside-root",
            joined.display().to_string(),
        ));
    }
    let suffix = joined
        .strip_prefix(existing)
        .map_err(|_| AgentError::new("lease-path-invalid", joined.display().to_string()))?;
    Ok(if suffix.as_os_str().is_empty() {
        canonical
    } else {
        canonical.join(suffix)
    })
}
fn normalized(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}
fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    sql_type: &str,
) -> Result<(), AgentError> {
    let exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
            params![table, column],
            |row| row.get::<_, bool>(0),
        )
        .map_err(sql_error)?;
    if !exists {
        // 参数不能绑定到 DDL 标识符；调用点只传入本 crate 内的固定标识符。
        connection
            .execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {sql_type}"),
                [],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}
fn sql_error(e: rusqlite::Error) -> AgentError {
    AgentError::new("agent-sqlite", e.to_string())
}
fn io_error(e: std::io::Error) -> AgentError {
    AgentError::new("agent-io", e.to_string())
}
fn json_error(e: serde_json::Error) -> AgentError {
    AgentError::new("agent-json", e.to_string())
}
