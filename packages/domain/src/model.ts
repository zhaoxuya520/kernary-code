/**
 * 领域层只描述“系统是什么”，不依赖数据库、模型 SDK、具体界面或浏览器。
 * 这里的类型同时是后续 Runtime Schema 的设计底稿。
 */

export type MissionStatus = "new" | "running" | "completed" | "cancelled";

export type NodeKind = "task" | "join";

export type NodeStatus =
  | "queued"
  | "running"
  | "waiting-approval"
  | "submitted"
  | "accepted"
  | "failed";

/** AgentRun 与 Node 分开：一个 Node 后续可以经历多个 attempt/run。 */
export type AgentRunStatus =
  | "running"
  | "waiting-approval"
  | "submitted"
  | "accepted"
  | "failed"
  | "cancelled";

export interface AgentRunState {
  id: string;
  nodeId: string;
  agentDefinitionId: string;
  endpointId: string;
  attempt: number;
  status: AgentRunStatus;
}

export interface WorkflowNodeDefinition {
  id: string;
  title: string;
  kind: NodeKind;
  dependsOn: string[];
  agentDefinitionId: string;
  requiresApproval?: boolean;
}

export interface WorkflowNodeState extends WorkflowNodeDefinition {
  status: NodeStatus;
  runId?: string;
  outputSummary?: string;
  approvalId?: string;
}

export interface ApprovalState {
  id: string;
  nodeId: string;
  runId: string;
  action: string;
  reason: string;
  status: "pending" | "allowed" | "denied";
}

export interface MissionState {
  missionId: string;
  projectId: string;
  goal: string;
  status: MissionStatus;
  version: number;
  nodes: Record<string, WorkflowNodeState>;
  runs: Record<string, AgentRunState>;
  approvals: Record<string, ApprovalState>;
}

/** 所有 Mission 修改都必须先表达成命令。 */
export type MissionCommand =
  | {
      type: "CreateMission";
      missionId: string;
      projectId: string;
      goal: string;
    }
  | {
      type: "InstallPlan";
      nodes: WorkflowNodeDefinition[];
    }
  | {
      type: "StartNode";
      nodeId: string;
      runId: string;
    }
  | {
      type: "SubmitNode";
      nodeId: string;
      runId: string;
      outputSummary: string;
    }
  | {
      type: "AcceptNode";
      nodeId: string;
      runId: string;
    }
  | {
      type: "RequestApproval";
      nodeId: string;
      runId: string;
      approvalId: string;
      action: string;
      reason: string;
    }
  | {
      type: "ResolveApproval";
      approvalId: string;
      decision: "allow" | "deny";
    }
  | {
      type: "CompleteMission";
    };

/**
 * 领域事件是已经发生的事实；事件一旦落盘就不能就地修改。
 * payload 暂时直接使用 TypeScript 类型，下一阶段会为每种事件增加运行时 Schema。
 */
export type DomainEvent =
  | {
      type: "mission.created";
      missionId: string;
      projectId: string;
      goal: string;
    }
  | {
      type: "mission.plan-installed";
      nodes: WorkflowNodeDefinition[];
    }
  | {
      type: "node.started";
      nodeId: string;
      runId: string;
    }
  | {
      type: "node.submitted";
      nodeId: string;
      runId: string;
      outputSummary: string;
    }
  | {
      type: "node.accepted";
      nodeId: string;
      runId: string;
    }
  | {
      type: "approval.requested";
      approvalId: string;
      nodeId: string;
      runId: string;
      action: string;
      reason: string;
    }
  | {
      type: "approval.resolved";
      approvalId: string;
      decision: "allow" | "deny";
    }
  | {
      type: "mission.completed";
    };

/**
 * EffectIntent 只描述“需要做什么”，不在 Command Handler 内执行副作用。
 * OutboxRunner 稍后领取并执行它。
 */
export type EffectIntent =
  | {
      kind: "start-fake-run";
      missionId: string;
      nodeId: string;
      runId: string;
    }
  | {
      kind: "resume-fake-run";
      missionId: string;
      nodeId: string;
      runId: string;
    }
  | {
      kind: "verify-fake-run";
      missionId: string;
      nodeId: string;
      runId: string;
    };

export interface DecideResult {
  events: DomainEvent[];
  effects: EffectIntent[];
}

export interface StoredEvent {
  sequence: number;
  missionId: string;
  aggregateVersion: number;
  event: DomainEvent;
  recordedAt: string;
}

export interface OutboxRecord {
  id: string;
  missionId: string;
  aggregateVersion: number;
  effect: EffectIntent;
  status: "pending" | "claimed" | "completed" | "failed";
  claimToken?: string;
  attempts: number;
}

export function createEmptyMissionState(missionId: string): MissionState {
  return {
    missionId,
    projectId: "",
    goal: "",
    status: "new",
    version: 0,
    nodes: {},
    runs: {},
    approvals: {},
  };
}
