use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{
    AgentDefinitionId, AgentEndpointId, ApprovalId, MissionId, ProjectId, RunId, TaskId,
};
use serde::{Deserialize, Serialize};

/// Mission 生命周期。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissionStatus {
    #[default]
    New,
    Running,
    Completed,
    Cancelled,
}

/// 当前 Stage 1 支持的节点类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    Task,
    Join,
    Review,
    Meeting,
    Merge,
}

/// 节点执行状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeStatus {
    Queued,
    Running,
    WaitingApproval,
    Submitted,
    Accepted,
    Failed,
    Cancelled,
}

/// Agent Run 状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentRunStatus {
    Running,
    WaitingApproval,
    Submitted,
    Accepted,
    Failed,
    Cancelled,
}

/// 审批状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalStatus {
    Pending,
    Allowed,
    Denied,
    Cancelled,
}

/// 用户的审批决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// 任务图中的不可变节点定义。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowNodeDefinition {
    pub id: TaskId,
    pub title: String,
    pub kind: NodeKind,
    pub depends_on: Vec<TaskId>,
    pub agent_definition_id: AgentDefinitionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_approval: Option<bool>,
}

/// 带运行状态的节点。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowNodeState {
    pub id: TaskId,
    pub title: String,
    pub kind: NodeKind,
    pub depends_on: Vec<TaskId>,
    pub agent_definition_id: AgentDefinitionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_approval: Option<bool>,
    pub status: NodeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<ApprovalId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
}

impl WorkflowNodeState {
    fn queued(definition: &WorkflowNodeDefinition) -> Self {
        Self {
            id: definition.id.clone(),
            title: definition.title.clone(),
            kind: definition.kind,
            depends_on: definition.depends_on.clone(),
            agent_definition_id: definition.agent_definition_id.clone(),
            requires_approval: definition.requires_approval,
            status: NodeStatus::Queued,
            run_id: None,
            approval_id: None,
            output_summary: None,
        }
    }
}

/// 一次 Agent Run。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRunState {
    pub id: RunId,
    pub node_id: TaskId,
    pub agent_definition_id: AgentDefinitionId,
    pub endpoint_id: AgentEndpointId,
    pub attempt: u32,
    pub status: AgentRunStatus,
}

/// 一个 durable Approval。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalState {
    pub id: ApprovalId,
    pub node_id: TaskId,
    pub run_id: RunId,
    pub action: String,
    pub reason: String,
    pub status: ApprovalStatus,
}

/// Mission 聚合状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MissionState {
    pub mission_id: MissionId,
    pub project_id: ProjectId,
    pub goal: String,
    pub status: MissionStatus,
    pub version: u64,
    pub nodes: BTreeMap<TaskId, WorkflowNodeState>,
    pub runs: BTreeMap<RunId, AgentRunState>,
    pub approvals: BTreeMap<ApprovalId, ApprovalState>,
}

impl MissionState {
    /// 创建尚未收到 `mission.created` 的空状态。
    #[must_use]
    pub fn empty(mission_id: MissionId) -> Self {
        Self {
            mission_id,
            project_id: ProjectId::default(),
            goal: String::new(),
            status: MissionStatus::New,
            version: 0,
            nodes: BTreeMap::new(),
            runs: BTreeMap::new(),
            approvals: BTreeMap::new(),
        }
    }
}

/// TypeScript oracle 使用的版本化事件包装。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VersionedEvent {
    pub aggregate_version: u64,
    pub event: DomainEvent,
}

/// 已发生的 Mission 事实。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum DomainEvent {
    #[serde(rename = "mission.created")]
    MissionCreated {
        #[serde(rename = "missionId")]
        mission_id: MissionId,
        #[serde(rename = "projectId")]
        project_id: ProjectId,
        goal: String,
    },
    #[serde(rename = "mission.plan-installed")]
    MissionPlanInstalled { nodes: Vec<WorkflowNodeDefinition> },
    #[serde(rename = "mission.nodes-appended")]
    MissionNodesAppended { nodes: Vec<WorkflowNodeDefinition> },
    #[serde(rename = "node.started")]
    NodeStarted {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
    },
    #[serde(rename = "node.submitted")]
    NodeSubmitted {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
        #[serde(rename = "outputSummary")]
        output_summary: String,
    },
    #[serde(rename = "node.accepted")]
    NodeAccepted {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
    },
    #[serde(rename = "node.failed")]
    NodeFailed {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
        error: String,
    },
    #[serde(rename = "node.cancelled")]
    NodeCancelled {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: Option<RunId>,
        reason: String,
    },
    #[serde(rename = "approval.requested")]
    ApprovalRequested {
        #[serde(rename = "approvalId")]
        approval_id: ApprovalId,
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
        action: String,
        reason: String,
    },
    #[serde(rename = "approval.resolved")]
    ApprovalResolved {
        #[serde(rename = "approvalId")]
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    #[serde(rename = "mission.completed")]
    MissionCompleted {},
    #[serde(rename = "mission.cancelled")]
    MissionCancelled { reason: String },
}

/// Reducer 在遇到损坏事件流时返回的确定性错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelError {
    NodeNotFound(TaskId),
    RunNotFound(RunId),
    ApprovalNotFound(ApprovalId),
    DuplicateNode(TaskId),
    EventVersionMismatch { expected: u64, actual: u64 },
}

impl Display for KernelError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodeNotFound(id) => write!(formatter, "node-not-found: {id}"),
            Self::RunNotFound(id) => write!(formatter, "run-not-found: {id}"),
            Self::ApprovalNotFound(id) => write!(formatter, "approval-not-found: {id}"),
            Self::DuplicateNode(id) => write!(formatter, "duplicate-node: {id}"),
            Self::EventVersionMismatch { expected, actual } => {
                write!(
                    formatter,
                    "event-version-mismatch: expected={expected}, actual={actual}"
                )
            }
        }
    }
}

impl Error for KernelError {}

/// 将一个事件纯函数地应用到 MissionState。
pub fn reduce_mission(
    state: &MissionState,
    event: &DomainEvent,
) -> Result<MissionState, KernelError> {
    let mut next = state.clone();
    next.version += 1;

    match event {
        DomainEvent::MissionCreated {
            mission_id,
            project_id,
            goal,
        } => {
            next.mission_id = mission_id.clone();
            next.project_id = project_id.clone();
            next.goal = goal.clone();
        }
        DomainEvent::MissionPlanInstalled { nodes } => {
            let mut next_nodes = BTreeMap::new();
            for definition in nodes {
                let previous =
                    next_nodes.insert(definition.id.clone(), WorkflowNodeState::queued(definition));
                if previous.is_some() {
                    return Err(KernelError::DuplicateNode(definition.id.clone()));
                }
            }
            next.nodes = next_nodes;
            next.status = MissionStatus::Running;
        }
        DomainEvent::MissionNodesAppended { nodes } => {
            for definition in nodes {
                if next
                    .nodes
                    .insert(definition.id.clone(), WorkflowNodeState::queued(definition))
                    .is_some()
                {
                    return Err(KernelError::DuplicateNode(definition.id.clone()));
                }
            }
        }
        DomainEvent::NodeStarted { node_id, run_id } => {
            let node = next
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| KernelError::NodeNotFound(node_id.clone()))?;
            node.status = NodeStatus::Running;
            node.run_id = Some(run_id.clone());
            next.runs.insert(
                run_id.clone(),
                AgentRunState {
                    id: run_id.clone(),
                    node_id: node.id.clone(),
                    agent_definition_id: node.agent_definition_id.clone(),
                    endpoint_id: AgentEndpointId::from(format!(
                        "endpoint:{}",
                        node.agent_definition_id
                    )),
                    attempt: 1,
                    status: AgentRunStatus::Running,
                },
            );
        }
        DomainEvent::NodeSubmitted {
            node_id,
            run_id,
            output_summary,
        } => {
            let node = next
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| KernelError::NodeNotFound(node_id.clone()))?;
            node.status = NodeStatus::Submitted;
            node.output_summary = Some(output_summary.clone());
            let run = next
                .runs
                .get_mut(run_id)
                .ok_or_else(|| KernelError::RunNotFound(run_id.clone()))?;
            run.status = AgentRunStatus::Submitted;
        }
        DomainEvent::NodeAccepted { node_id, run_id } => {
            let node = next
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| KernelError::NodeNotFound(node_id.clone()))?;
            node.status = NodeStatus::Accepted;
            let run = next
                .runs
                .get_mut(run_id)
                .ok_or_else(|| KernelError::RunNotFound(run_id.clone()))?;
            run.status = AgentRunStatus::Accepted;
        }
        DomainEvent::NodeFailed {
            node_id,
            run_id,
            error,
        } => {
            let node = next
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| KernelError::NodeNotFound(node_id.clone()))?;
            node.status = NodeStatus::Failed;
            node.output_summary = Some(error.clone());
            let run = next
                .runs
                .get_mut(run_id)
                .ok_or_else(|| KernelError::RunNotFound(run_id.clone()))?;
            run.status = AgentRunStatus::Failed;
        }
        DomainEvent::NodeCancelled {
            node_id, run_id, ..
        } => {
            let node = next
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| KernelError::NodeNotFound(node_id.clone()))?;
            node.status = NodeStatus::Cancelled;
            if let Some(run_id) = run_id {
                let run = next
                    .runs
                    .get_mut(run_id)
                    .ok_or_else(|| KernelError::RunNotFound(run_id.clone()))?;
                run.status = AgentRunStatus::Cancelled;
            }
            for approval in next.approvals.values_mut() {
                if approval.node_id == *node_id && approval.status == ApprovalStatus::Pending {
                    approval.status = ApprovalStatus::Cancelled;
                }
            }
        }
        DomainEvent::ApprovalRequested {
            approval_id,
            node_id,
            run_id,
            action,
            reason,
        } => {
            let node = next
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| KernelError::NodeNotFound(node_id.clone()))?;
            node.status = NodeStatus::WaitingApproval;
            node.approval_id = Some(approval_id.clone());
            let run = next
                .runs
                .get_mut(run_id)
                .ok_or_else(|| KernelError::RunNotFound(run_id.clone()))?;
            run.status = AgentRunStatus::WaitingApproval;
            next.approvals.insert(
                approval_id.clone(),
                ApprovalState {
                    id: approval_id.clone(),
                    node_id: node_id.clone(),
                    run_id: run_id.clone(),
                    action: action.clone(),
                    reason: reason.clone(),
                    status: ApprovalStatus::Pending,
                },
            );
        }
        DomainEvent::ApprovalResolved {
            approval_id,
            decision,
        } => {
            let approval = next
                .approvals
                .get_mut(approval_id)
                .ok_or_else(|| KernelError::ApprovalNotFound(approval_id.clone()))?;
            approval.status = match decision {
                ApprovalDecision::Allow => ApprovalStatus::Allowed,
                ApprovalDecision::Deny => ApprovalStatus::Denied,
            };
            let node = next
                .nodes
                .get_mut(&approval.node_id)
                .ok_or_else(|| KernelError::NodeNotFound(approval.node_id.clone()))?;
            let run = next
                .runs
                .get_mut(&approval.run_id)
                .ok_or_else(|| KernelError::RunNotFound(approval.run_id.clone()))?;
            match decision {
                ApprovalDecision::Allow => {
                    node.status = NodeStatus::Running;
                    run.status = AgentRunStatus::Running;
                }
                ApprovalDecision::Deny => {
                    node.status = NodeStatus::Failed;
                    run.status = AgentRunStatus::Failed;
                }
            }
        }
        DomainEvent::MissionCompleted {} => {
            next.status = MissionStatus::Completed;
        }
        DomainEvent::MissionCancelled { .. } => {
            next.status = MissionStatus::Cancelled;
            for node in next.nodes.values_mut() {
                if !matches!(node.status, NodeStatus::Accepted | NodeStatus::Failed) {
                    node.status = NodeStatus::Cancelled;
                }
            }
            for run in next.runs.values_mut() {
                if !matches!(
                    run.status,
                    AgentRunStatus::Accepted | AgentRunStatus::Failed
                ) {
                    run.status = AgentRunStatus::Cancelled;
                }
            }
            for approval in next.approvals.values_mut() {
                if approval.status == ApprovalStatus::Pending {
                    approval.status = ApprovalStatus::Cancelled;
                }
            }
        }
    }

    Ok(next)
}

/// 从版本化事件流重放 Mission，并验证版本连续性。
pub fn replay_mission(
    mission_id: MissionId,
    events: &[VersionedEvent],
) -> Result<MissionState, KernelError> {
    let mut state = MissionState::empty(mission_id);
    for versioned in events {
        let expected = state.version + 1;
        if versioned.aggregate_version != expected {
            return Err(KernelError::EventVersionMismatch {
                expected,
                actual: versioned.aggregate_version,
            });
        }
        state = reduce_mission(&state, &versioned.event)?;
    }
    Ok(state)
}

/// 返回依赖全部 accepted 的 queued 节点，并按 ID 稳定排序。
#[must_use]
pub fn find_ready_node_ids(state: &MissionState) -> Vec<TaskId> {
    if state.status != MissionStatus::Running {
        return Vec::new();
    }
    state
        .nodes
        .values()
        .filter(|node| {
            node.status == NodeStatus::Queued
                && node.depends_on.iter().all(|dependency_id| {
                    state
                        .nodes
                        .get(dependency_id)
                        .is_some_and(|dependency| dependency.status == NodeStatus::Accepted)
                })
        })
        .map(|node| node.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_node_event_is_rejected() {
        let state = MissionState::empty(MissionId::from("mission:test"));
        let error = reduce_mission(
            &state,
            &DomainEvent::NodeStarted {
                node_id: TaskId::from("missing"),
                run_id: RunId::from("run:missing"),
            },
        )
        .expect_err("损坏事件必须被拒绝");
        assert_eq!(error, KernelError::NodeNotFound(TaskId::from("missing")));
    }

    #[test]
    fn event_versions_must_be_contiguous() {
        let events = vec![VersionedEvent {
            aggregate_version: 2,
            event: DomainEvent::MissionCompleted {},
        }];
        let error = replay_mission(MissionId::from("mission:test"), &events)
            .expect_err("跳号事件必须被拒绝");
        assert_eq!(
            error,
            KernelError::EventVersionMismatch {
                expected: 1,
                actual: 2,
            }
        );
    }
}
