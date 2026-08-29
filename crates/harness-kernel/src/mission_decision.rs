use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{ApprovalId, MissionId, ProjectId, RunId, TaskId};
use serde::{Deserialize, Serialize};

use crate::{
    AgentRunStatus, ApprovalDecision, ApprovalStatus, DomainEvent, MissionState, MissionStatus,
    NodeStatus, WorkflowNodeDefinition,
};

/// 修改 Mission 聚合的意图。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum MissionCommand {
    #[serde(rename = "CreateMission")]
    CreateMission {
        #[serde(rename = "missionId")]
        mission_id: MissionId,
        #[serde(rename = "projectId")]
        project_id: ProjectId,
        goal: String,
    },
    #[serde(rename = "InstallPlan")]
    InstallPlan { nodes: Vec<WorkflowNodeDefinition> },
    #[serde(rename = "AppendPlanNodes")]
    AppendPlanNodes { nodes: Vec<WorkflowNodeDefinition> },
    #[serde(rename = "StartNode")]
    StartNode {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
    },
    #[serde(rename = "SubmitNode")]
    SubmitNode {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
        #[serde(rename = "outputSummary")]
        output_summary: String,
    },
    #[serde(rename = "AcceptNode")]
    AcceptNode {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
    },
    #[serde(rename = "FailNode")]
    FailNode {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
        error: String,
    },
    #[serde(rename = "CancelNode")]
    CancelNode {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        reason: String,
    },
    #[serde(rename = "RequestApproval")]
    RequestApproval {
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
        #[serde(rename = "approvalId")]
        approval_id: ApprovalId,
        action: String,
        reason: String,
    },
    #[serde(rename = "ResolveApproval")]
    ResolveApproval {
        #[serde(rename = "approvalId")]
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    #[serde(rename = "CompleteMission")]
    CompleteMission {},
    #[serde(rename = "CancelMission")]
    CancelMission { reason: String },
}

/// Kernel 交给 Runtime 的生产语义 Effect。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum EffectIntent {
    #[serde(rename = "agent-run.start")]
    StartAgentRun {
        #[serde(rename = "missionId")]
        mission_id: MissionId,
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
    },
    #[serde(rename = "agent-run.resume")]
    ResumeAgentRun {
        #[serde(rename = "missionId")]
        mission_id: MissionId,
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
    },
    #[serde(rename = "agent-run.verify")]
    VerifyAgentRun {
        #[serde(rename = "missionId")]
        mission_id: MissionId,
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
    },
    #[serde(rename = "agent-run.cancel")]
    CancelAgentRun {
        #[serde(rename = "missionId")]
        mission_id: MissionId,
        #[serde(rename = "nodeId")]
        node_id: TaskId,
        #[serde(rename = "runId")]
        run_id: RunId,
        reason: String,
    },
}

/// 一次纯 Command 决策的输出。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecideResult {
    pub events: Vec<DomainEvent>,
    pub effects: Vec<EffectIntent>,
}

/// Command 验证错误，code 是稳定协议字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandError {
    pub code: &'static str,
    pub message: String,
}

impl CommandError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Display for CommandError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for CommandError {}

fn node<'a>(
    state: &'a MissionState,
    node_id: &TaskId,
) -> Result<&'a crate::WorkflowNodeState, CommandError> {
    state
        .nodes
        .get(node_id)
        .ok_or_else(|| CommandError::new("node-not-found", format!("节点不存在：{node_id}")))
}

fn validate_plan(nodes: &[WorkflowNodeDefinition]) -> Result<(), CommandError> {
    let mut by_id = BTreeMap::new();
    for definition in nodes {
        if by_id.insert(definition.id.clone(), definition).is_some() {
            return Err(CommandError::new("duplicate-node", "任务图包含重复节点 ID"));
        }
    }

    for definition in nodes {
        for dependency_id in &definition.depends_on {
            if !by_id.contains_key(dependency_id) {
                return Err(CommandError::new(
                    "missing-dependency",
                    format!("依赖节点不存在：{dependency_id}"),
                ));
            }
            if dependency_id == &definition.id {
                return Err(CommandError::new(
                    "self-cycle",
                    format!("节点不能依赖自己：{}", definition.id),
                ));
            }
        }
    }

    fn visit(
        node_id: &TaskId,
        by_id: &BTreeMap<TaskId, &WorkflowNodeDefinition>,
        visiting: &mut BTreeSet<TaskId>,
        visited: &mut BTreeSet<TaskId>,
    ) -> Result<(), CommandError> {
        if visited.contains(node_id) {
            return Ok(());
        }
        if !visiting.insert(node_id.clone()) {
            return Err(CommandError::new(
                "cyclic-plan",
                format!("任务图出现循环：{node_id}"),
            ));
        }
        if let Some(definition) = by_id.get(node_id) {
            for dependency_id in &definition.depends_on {
                visit(dependency_id, by_id, visiting, visited)?;
            }
        }
        visiting.remove(node_id);
        visited.insert(node_id.clone());
        Ok(())
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for node_id in by_id.keys() {
        visit(node_id, &by_id, &mut visiting, &mut visited)?;
    }
    Ok(())
}

/// 纯函数地把 Command 决策为 Event/Effect。
pub fn decide_mission(
    state: &MissionState,
    command: &MissionCommand,
) -> Result<DecideResult, CommandError> {
    match command {
        MissionCommand::CreateMission {
            mission_id,
            project_id,
            goal,
        } => {
            if state.version != 0 || !state.project_id.as_str().is_empty() {
                return Err(CommandError::new("mission-exists", "Mission 已经创建"));
            }
            if goal.trim().is_empty() {
                return Err(CommandError::new("empty-goal", "Mission 目标不能为空"));
            }
            Ok(DecideResult {
                events: vec![DomainEvent::MissionCreated {
                    mission_id: mission_id.clone(),
                    project_id: project_id.clone(),
                    goal: goal.clone(),
                }],
                effects: vec![],
            })
        }
        MissionCommand::InstallPlan { nodes } => {
            if state.project_id.as_str().is_empty() {
                return Err(CommandError::new("mission-not-created", "请先创建 Mission"));
            }
            if !state.nodes.is_empty() {
                return Err(CommandError::new("plan-exists", "Plan 已存在"));
            }
            if nodes.is_empty() {
                return Err(CommandError::new("empty-plan", "任务图不能为空"));
            }
            validate_plan(nodes)?;
            Ok(DecideResult {
                events: vec![DomainEvent::MissionPlanInstalled {
                    nodes: nodes.clone(),
                }],
                effects: vec![],
            })
        }
        MissionCommand::AppendPlanNodes { nodes } => {
            if state.status != MissionStatus::Running {
                return Err(CommandError::new(
                    "mission-not-running",
                    "只有 running Mission 可以追加节点",
                ));
            }
            if nodes.is_empty() {
                return Err(CommandError::new("empty-plan-append", "追加节点不能为空"));
            }
            if nodes.iter().any(|node| state.nodes.contains_key(&node.id)) {
                return Err(CommandError::new(
                    "append-node-exists",
                    "追加节点与现有 Plan 重名",
                ));
            }
            let mut combined = state
                .nodes
                .values()
                .map(|node| WorkflowNodeDefinition {
                    id: node.id.clone(),
                    title: node.title.clone(),
                    kind: node.kind,
                    depends_on: node.depends_on.clone(),
                    agent_definition_id: node.agent_definition_id.clone(),
                    requires_approval: node.requires_approval,
                })
                .collect::<Vec<_>>();
            combined.extend(nodes.iter().cloned());
            validate_plan(&combined)?;
            Ok(DecideResult {
                events: vec![DomainEvent::MissionNodesAppended {
                    nodes: nodes.clone(),
                }],
                effects: vec![],
            })
        }
        MissionCommand::StartNode { node_id, run_id } => {
            let node = node(state, node_id)?;
            if node.status != NodeStatus::Queued {
                return Err(CommandError::new(
                    "node-not-queued",
                    format!("节点不是 queued：{node_id}"),
                ));
            }
            let dependencies_accepted = node.depends_on.iter().all(|dependency_id| {
                state
                    .nodes
                    .get(dependency_id)
                    .is_some_and(|dependency| dependency.status == NodeStatus::Accepted)
            });
            if !dependencies_accepted {
                return Err(CommandError::new(
                    "dependency-not-ready",
                    format!("节点依赖尚未全部 accepted：{node_id}"),
                ));
            }
            Ok(DecideResult {
                events: vec![DomainEvent::NodeStarted {
                    node_id: node_id.clone(),
                    run_id: run_id.clone(),
                }],
                effects: vec![EffectIntent::StartAgentRun {
                    mission_id: state.mission_id.clone(),
                    node_id: node_id.clone(),
                    run_id: run_id.clone(),
                }],
            })
        }
        MissionCommand::SubmitNode {
            node_id,
            run_id,
            output_summary,
        } => {
            let node = node(state, node_id)?;
            if node.run_id.as_ref() != Some(run_id) {
                return Err(CommandError::new(
                    "stale-run",
                    "迟到或错误 Run 不能提交结果",
                ));
            }
            if node.status != NodeStatus::Running {
                return Err(CommandError::new(
                    "node-not-running",
                    "只有 running 节点可以提交",
                ));
            }
            Ok(DecideResult {
                events: vec![DomainEvent::NodeSubmitted {
                    node_id: node_id.clone(),
                    run_id: run_id.clone(),
                    output_summary: output_summary.clone(),
                }],
                effects: vec![EffectIntent::VerifyAgentRun {
                    mission_id: state.mission_id.clone(),
                    node_id: node_id.clone(),
                    run_id: run_id.clone(),
                }],
            })
        }
        MissionCommand::AcceptNode { node_id, run_id } => {
            let node = node(state, node_id)?;
            if node.run_id.as_ref() != Some(run_id) {
                return Err(CommandError::new("stale-run", "Verifier 收到了过期 Run"));
            }
            if node.status != NodeStatus::Submitted {
                return Err(CommandError::new(
                    "node-not-submitted",
                    "只有 submitted 节点可以验收",
                ));
            }
            Ok(DecideResult {
                events: vec![DomainEvent::NodeAccepted {
                    node_id: node_id.clone(),
                    run_id: run_id.clone(),
                }],
                effects: vec![],
            })
        }
        MissionCommand::FailNode {
            node_id,
            run_id,
            error,
        } => {
            let node = node(state, node_id)?;
            if node.run_id.as_ref() != Some(run_id) {
                return Err(CommandError::new("stale-run", "过期 Run 不能标记失败"));
            }
            if !matches!(node.status, NodeStatus::Running | NodeStatus::Submitted) {
                return Err(CommandError::new(
                    "node-not-failable",
                    "只有 running/submitted 节点可以标记失败",
                ));
            }
            if error.trim().is_empty() {
                return Err(CommandError::new("node-error-empty", "失败原因不能为空"));
            }
            Ok(DecideResult {
                events: vec![DomainEvent::NodeFailed {
                    node_id: node_id.clone(),
                    run_id: run_id.clone(),
                    error: error.clone(),
                }],
                effects: vec![],
            })
        }
        MissionCommand::CancelNode { node_id, reason } => {
            let target = node(state, node_id)?;
            if matches!(
                target.status,
                NodeStatus::Accepted | NodeStatus::Failed | NodeStatus::Cancelled
            ) {
                return Err(CommandError::new("node-terminal", "终态节点不能再次取消"));
            }
            if reason.trim().is_empty() {
                return Err(CommandError::new("cancel-reason-empty", "取消原因不能为空"));
            }
            let mut affected = BTreeSet::from([node_id.clone()]);
            loop {
                let before = affected.len();
                for candidate in state.nodes.values() {
                    if candidate
                        .depends_on
                        .iter()
                        .any(|dependency| affected.contains(dependency))
                    {
                        affected.insert(candidate.id.clone());
                    }
                }
                if affected.len() == before {
                    break;
                }
            }
            let mut events = Vec::new();
            let mut effects = Vec::new();
            for affected_id in affected {
                let affected_node = &state.nodes[&affected_id];
                if matches!(
                    affected_node.status,
                    NodeStatus::Accepted | NodeStatus::Failed | NodeStatus::Cancelled
                ) {
                    continue;
                }
                events.push(DomainEvent::NodeCancelled {
                    node_id: affected_id.clone(),
                    run_id: affected_node.run_id.clone(),
                    reason: reason.clone(),
                });
                if let Some(run_id) = &affected_node.run_id
                    && state.runs.get(run_id).is_some_and(|run| {
                        matches!(
                            run.status,
                            AgentRunStatus::Running
                                | AgentRunStatus::WaitingApproval
                                | AgentRunStatus::Submitted
                        )
                    })
                {
                    effects.push(EffectIntent::CancelAgentRun {
                        mission_id: state.mission_id.clone(),
                        node_id: affected_id,
                        run_id: run_id.clone(),
                        reason: reason.clone(),
                    });
                }
            }
            Ok(DecideResult { events, effects })
        }
        MissionCommand::RequestApproval {
            node_id,
            run_id,
            approval_id,
            action,
            reason,
        } => {
            let node = node(state, node_id)?;
            if node.run_id.as_ref() != Some(run_id) {
                return Err(CommandError::new("stale-run", "过期 Run 不能请求权限"));
            }
            if node.status != NodeStatus::Running {
                return Err(CommandError::new(
                    "node-not-running",
                    "只有 running 节点可以请求权限",
                ));
            }
            if state.approvals.contains_key(approval_id) {
                return Err(CommandError::new("approval-exists", "Approval ID 已存在"));
            }
            Ok(DecideResult {
                events: vec![DomainEvent::ApprovalRequested {
                    approval_id: approval_id.clone(),
                    node_id: node_id.clone(),
                    run_id: run_id.clone(),
                    action: action.clone(),
                    reason: reason.clone(),
                }],
                effects: vec![],
            })
        }
        MissionCommand::ResolveApproval {
            approval_id,
            decision,
        } => {
            let approval = state.approvals.get(approval_id).ok_or_else(|| {
                CommandError::new("approval-not-found", format!("审批不存在：{approval_id}"))
            })?;
            if approval.status != ApprovalStatus::Pending {
                return Err(CommandError::new("approval-resolved", "审批已经处理"));
            }
            Ok(DecideResult {
                events: vec![DomainEvent::ApprovalResolved {
                    approval_id: approval_id.clone(),
                    decision: *decision,
                }],
                effects: if *decision == ApprovalDecision::Allow {
                    vec![EffectIntent::ResumeAgentRun {
                        mission_id: state.mission_id.clone(),
                        node_id: approval.node_id.clone(),
                        run_id: approval.run_id.clone(),
                    }]
                } else {
                    vec![]
                },
            })
        }
        MissionCommand::CompleteMission {} => {
            if state.nodes.is_empty() {
                return Err(CommandError::new(
                    "plan-missing",
                    "没有 Plan 的 Mission 不能完成",
                ));
            }
            if !state
                .nodes
                .values()
                .all(|node| node.status == NodeStatus::Accepted)
            {
                return Err(CommandError::new("nodes-incomplete", "仍有节点未验收"));
            }
            Ok(DecideResult {
                events: vec![DomainEvent::MissionCompleted {}],
                effects: vec![],
            })
        }
        MissionCommand::CancelMission { reason } => {
            if matches!(
                state.status,
                MissionStatus::Completed | MissionStatus::Cancelled
            ) {
                return Err(CommandError::new(
                    "mission-terminal",
                    "终态 Mission 不能再次取消",
                ));
            }
            if reason.trim().is_empty() {
                return Err(CommandError::new("cancel-reason-empty", "取消原因不能为空"));
            }
            let effects = state
                .runs
                .values()
                .filter(|run| {
                    matches!(
                        run.status,
                        AgentRunStatus::Running
                            | AgentRunStatus::WaitingApproval
                            | AgentRunStatus::Submitted
                    )
                })
                .map(|run| EffectIntent::CancelAgentRun {
                    mission_id: state.mission_id.clone(),
                    node_id: run.node_id.clone(),
                    run_id: run.id.clone(),
                    reason: reason.clone(),
                })
                .collect();
            Ok(DecideResult {
                events: vec![DomainEvent::MissionCancelled {
                    reason: reason.clone(),
                }],
                effects,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use harness_types::AgentDefinitionId;

    use super::*;
    use crate::{MissionStatus, NodeKind};

    fn node_definition(id: &str, dependencies: &[&str]) -> WorkflowNodeDefinition {
        WorkflowNodeDefinition {
            id: TaskId::from(id),
            title: id.to_owned(),
            kind: NodeKind::Task,
            depends_on: dependencies.iter().copied().map(TaskId::from).collect(),
            agent_definition_id: AgentDefinitionId::from("agent:test"),
            requires_approval: None,
        }
    }

    #[test]
    fn cyclic_plan_is_rejected() {
        let error = validate_plan(&[node_definition("a", &["b"]), node_definition("b", &["a"])])
            .expect_err("循环 Plan 必须拒绝");
        assert_eq!(error.code, "cyclic-plan");
    }

    #[test]
    fn incomplete_mission_cannot_complete() {
        let mut state = MissionState::empty(MissionId::from("mission:test"));
        state.project_id = ProjectId::from("project:test");
        state.status = MissionStatus::Running;
        state.nodes.insert(
            TaskId::from("a"),
            crate::WorkflowNodeState {
                id: TaskId::from("a"),
                title: "a".to_owned(),
                kind: NodeKind::Task,
                depends_on: vec![],
                agent_definition_id: AgentDefinitionId::from("agent:test"),
                requires_approval: None,
                status: NodeStatus::Queued,
                run_id: None,
                approval_id: None,
                output_summary: None,
            },
        );
        let error = decide_mission(&state, &MissionCommand::CompleteMission {})
            .expect_err("未完成 Mission 必须拒绝");
        assert_eq!(error.code, "nodes-incomplete");
    }

    #[test]
    fn cancelling_mission_emits_one_cancel_effect_per_active_run() {
        let mission_id = MissionId::from("mission:cancel");
        let mut state = MissionState::empty(mission_id.clone());
        state = crate::reduce_mission(
            &state,
            &DomainEvent::MissionCreated {
                mission_id: mission_id.clone(),
                project_id: ProjectId::from("project:test"),
                goal: "cancel safely".to_owned(),
            },
        )
        .expect("create");
        state = crate::reduce_mission(
            &state,
            &DomainEvent::MissionPlanInstalled {
                nodes: vec![node_definition("task:a", &[])],
            },
        )
        .expect("plan");
        state = crate::reduce_mission(
            &state,
            &DomainEvent::NodeStarted {
                node_id: TaskId::from("task:a"),
                run_id: RunId::from("run:a"),
            },
        )
        .expect("start");

        let decision = decide_mission(
            &state,
            &MissionCommand::CancelMission {
                reason: "user-request".to_owned(),
            },
        )
        .expect("cancel");
        assert_eq!(
            decision.events,
            vec![DomainEvent::MissionCancelled {
                reason: "user-request".to_owned()
            }]
        );
        assert_eq!(decision.effects.len(), 1);
        assert!(matches!(
            &decision.effects[0],
            EffectIntent::CancelAgentRun { run_id, .. } if run_id == &RunId::from("run:a")
        ));

        let cancelled = crate::reduce_mission(&state, &decision.events[0]).expect("reduce cancel");
        assert_eq!(cancelled.status, MissionStatus::Cancelled);
        assert_eq!(
            cancelled.nodes[&TaskId::from("task:a")].status,
            NodeStatus::Cancelled
        );
        assert_eq!(
            cancelled.runs[&RunId::from("run:a")].status,
            AgentRunStatus::Cancelled
        );
        assert!(crate::find_ready_node_ids(&cancelled).is_empty());
        assert_eq!(
            decide_mission(
                &cancelled,
                &MissionCommand::CancelMission {
                    reason: "again".to_owned()
                }
            )
            .expect_err("terminal cancel")
            .code,
            "mission-terminal"
        );
    }

    #[test]
    fn active_run_failure_is_durable_and_stale_failure_is_rejected() {
        let mission_id = MissionId::from("mission:failure");
        let mut state = MissionState::empty(mission_id.clone());
        for event in [
            DomainEvent::MissionCreated {
                mission_id,
                project_id: ProjectId::from("project:test"),
                goal: "fail safely".to_owned(),
            },
            DomainEvent::MissionPlanInstalled {
                nodes: vec![node_definition("task:a", &[])],
            },
            DomainEvent::NodeStarted {
                node_id: TaskId::from("task:a"),
                run_id: RunId::from("run:a"),
            },
        ] {
            state = crate::reduce_mission(&state, &event).expect("reduce");
        }
        let decision = decide_mission(
            &state,
            &MissionCommand::FailNode {
                node_id: TaskId::from("task:a"),
                run_id: RunId::from("run:a"),
                error: "provider timeout".to_owned(),
            },
        )
        .expect("fail");
        let failed = crate::reduce_mission(&state, &decision.events[0]).expect("reduce failure");
        assert_eq!(
            failed.nodes[&TaskId::from("task:a")].status,
            NodeStatus::Failed
        );
        assert_eq!(
            failed.runs[&RunId::from("run:a")].status,
            AgentRunStatus::Failed
        );
        assert_eq!(
            decide_mission(
                &state,
                &MissionCommand::FailNode {
                    node_id: TaskId::from("task:a"),
                    run_id: RunId::from("run:stale"),
                    error: "late".to_owned(),
                }
            )
            .expect_err("stale")
            .code,
            "stale-run"
        );
    }

    #[test]
    fn cancelling_node_cascades_to_all_dependent_descendants() {
        let mission_id = MissionId::from("mission:node-cancel");
        let mut state = MissionState::empty(mission_id.clone());
        for event in [
            DomainEvent::MissionCreated {
                mission_id,
                project_id: ProjectId::from("project:test"),
                goal: "cancel branch".to_owned(),
            },
            DomainEvent::MissionPlanInstalled {
                nodes: vec![
                    node_definition("task:a", &[]),
                    node_definition("task:b", &["task:a"]),
                    node_definition("task:c", &["task:b"]),
                    node_definition("task:independent", &[]),
                ],
            },
        ] {
            state = crate::reduce_mission(&state, &event).expect("reduce");
        }
        let decision = decide_mission(
            &state,
            &MissionCommand::CancelNode {
                node_id: TaskId::from("task:a"),
                reason: "no longer needed".to_owned(),
            },
        )
        .expect("cancel branch");
        assert_eq!(decision.events.len(), 3);
        assert!(decision.effects.is_empty());
        for event in decision.events {
            state = crate::reduce_mission(&state, &event).expect("reduce cancellation");
        }
        for task_id in ["task:a", "task:b", "task:c"] {
            assert_eq!(
                state.nodes[&TaskId::from(task_id)].status,
                NodeStatus::Cancelled
            );
        }
        assert_eq!(
            state.nodes[&TaskId::from("task:independent")].status,
            NodeStatus::Queued
        );
        assert_eq!(
            crate::find_ready_node_ids(&state),
            vec![TaskId::from("task:independent")]
        );
    }

    #[test]
    fn running_mission_can_append_valid_merge_node_but_not_missing_dependency() {
        let mission_id = MissionId::from("mission:append");
        let mut state = MissionState::empty(mission_id.clone());
        for event in [
            DomainEvent::MissionCreated {
                mission_id,
                project_id: ProjectId::from("project:test"),
                goal: "merge".to_owned(),
            },
            DomainEvent::MissionPlanInstalled {
                nodes: vec![node_definition("task:a", &[])],
            },
        ] {
            state = crate::reduce_mission(&state, &event).expect("reduce");
        }
        let merge = WorkflowNodeDefinition {
            id: TaskId::from("task:merge"),
            title: "merge conflict".to_owned(),
            kind: NodeKind::Merge,
            depends_on: vec![TaskId::from("task:a")],
            agent_definition_id: AgentDefinitionId::from("agent:merge"),
            requires_approval: None,
        };
        let decision = decide_mission(
            &state,
            &MissionCommand::AppendPlanNodes {
                nodes: vec![merge.clone()],
            },
        )
        .expect("append");
        state = crate::reduce_mission(&state, &decision.events[0]).expect("reduce append");
        assert_eq!(state.nodes[&merge.id].kind, NodeKind::Merge);
        assert_eq!(
            decide_mission(
                &state,
                &MissionCommand::AppendPlanNodes {
                    nodes: vec![WorkflowNodeDefinition {
                        id: TaskId::from("task:bad"),
                        title: "bad".to_owned(),
                        kind: NodeKind::Merge,
                        depends_on: vec![TaskId::from("task:missing")],
                        agent_definition_id: AgentDefinitionId::from("agent:merge"),
                        requires_approval: None,
                    }]
                }
            )
            .expect_err("missing dependency")
            .code,
            "missing-dependency"
        );
    }
}
