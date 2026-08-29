import type {
  DecideResult,
  DomainEvent,
  MissionCommand,
  MissionState,
  WorkflowNodeDefinition,
} from "./model.ts";

/** 领域错误携带稳定错误码，UI 和协议层不需要解析自然语言。 */
export class DomainError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }
}

function assert(condition: unknown, code: string, message: string): asserts condition {
  if (!condition) {
    throw new DomainError(code, message);
  }
}

/** 检查任务图无重复节点、无悬空依赖、无自环和无循环。 */
function validatePlan(nodes: WorkflowNodeDefinition[]): void {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  assert(byId.size === nodes.length, "duplicate-node", "任务图包含重复节点 ID");

  for (const node of nodes) {
    for (const dependencyId of node.dependsOn) {
      assert(byId.has(dependencyId), "missing-dependency", `依赖节点不存在：${dependencyId}`);
      assert(dependencyId !== node.id, "self-cycle", `节点不能依赖自己：${node.id}`);
    }
  }

  const visiting = new Set<string>();
  const visited = new Set<string>();

  const visit = (nodeId: string): void => {
    if (visited.has(nodeId)) return;
    assert(!visiting.has(nodeId), "cyclic-plan", `任务图出现循环：${nodeId}`);
    visiting.add(nodeId);
    for (const dependencyId of byId.get(nodeId)?.dependsOn ?? []) visit(dependencyId);
    visiting.delete(nodeId);
    visited.add(nodeId);
  };

  for (const node of nodes) visit(node.id);
}

/**
 * Command Handler 是纯决策函数：只查看状态并返回事件和 EffectIntent。
 * 它不能访问网络、数据库、Shell、浏览器或模型。
 */
export function decideMission(state: Readonly<MissionState>, command: MissionCommand): DecideResult {
  switch (command.type) {
    case "CreateMission": {
      assert(state.version === 0 && state.projectId === "", "mission-exists", "Mission 已经创建");
      assert(command.goal.trim().length > 0, "empty-goal", "Mission 目标不能为空");
      return {
        events: [
          {
            type: "mission.created",
            missionId: command.missionId,
            projectId: command.projectId,
            goal: command.goal,
          },
        ],
        effects: [],
      };
    }

    case "InstallPlan": {
      assert(state.projectId !== "", "mission-not-created", "请先创建 Mission");
      assert(Object.keys(state.nodes).length === 0, "plan-exists", "当前 D0 切片不允许覆盖已有 Plan");
      assert(command.nodes.length > 0, "empty-plan", "任务图不能为空");
      validatePlan(command.nodes);
      return {
        events: [{ type: "mission.plan-installed", nodes: command.nodes }],
        effects: [],
      };
    }

    case "StartNode": {
      const node = state.nodes[command.nodeId];
      assert(node, "node-not-found", `节点不存在：${command.nodeId}`);
      assert(node.status === "queued", "node-not-queued", `节点不是 queued：${command.nodeId}`);
      const dependenciesAccepted = node.dependsOn.every(
        (dependencyId) => state.nodes[dependencyId]?.status === "accepted",
      );
      assert(dependenciesAccepted, "dependency-not-ready", `节点依赖尚未全部 accepted：${command.nodeId}`);
      return {
        events: [{ type: "node.started", nodeId: node.id, runId: command.runId }],
        effects: [
          {
            kind: "start-fake-run",
            missionId: state.missionId,
            nodeId: node.id,
            runId: command.runId,
          },
        ],
      };
    }

    case "SubmitNode": {
      const node = state.nodes[command.nodeId];
      assert(node, "node-not-found", `节点不存在：${command.nodeId}`);
      assert(node.runId === command.runId, "stale-run", "迟到或错误 Run 不能提交结果");
      assert(node.status === "running", "node-not-running", "只有 running 节点可以提交");
      return {
        events: [
          {
            type: "node.submitted",
            nodeId: node.id,
            runId: command.runId,
            outputSummary: command.outputSummary,
          },
        ],
        effects: [
          {
            kind: "verify-fake-run",
            missionId: state.missionId,
            nodeId: node.id,
            runId: command.runId,
          },
        ],
      };
    }

    case "AcceptNode": {
      const node = state.nodes[command.nodeId];
      assert(node, "node-not-found", `节点不存在：${command.nodeId}`);
      assert(node.runId === command.runId, "stale-run", "Verifier 收到了过期 Run");
      assert(node.status === "submitted", "node-not-submitted", "只有 submitted 节点可以验收");
      return {
        events: [{ type: "node.accepted", nodeId: node.id, runId: command.runId }],
        effects: [],
      };
    }

    case "RequestApproval": {
      const node = state.nodes[command.nodeId];
      assert(node, "node-not-found", `节点不存在：${command.nodeId}`);
      assert(node.runId === command.runId, "stale-run", "过期 Run 不能请求权限");
      assert(node.status === "running", "node-not-running", "只有 running 节点可以请求权限");
      assert(!state.approvals[command.approvalId], "approval-exists", "Approval ID 已存在");
      return {
        events: [
          {
            type: "approval.requested",
            approvalId: command.approvalId,
            nodeId: command.nodeId,
            runId: command.runId,
            action: command.action,
            reason: command.reason,
          },
        ],
        effects: [],
      };
    }

    case "ResolveApproval": {
      const approval = state.approvals[command.approvalId];
      assert(approval, "approval-not-found", `审批不存在：${command.approvalId}`);
      assert(approval.status === "pending", "approval-resolved", "审批已经处理");
      return {
        events: [
          {
            type: "approval.resolved",
            approvalId: approval.id,
            decision: command.decision,
          },
        ],
        effects:
          command.decision === "allow"
            ? [
                {
                  kind: "resume-fake-run",
                  missionId: state.missionId,
                  nodeId: approval.nodeId,
                  runId: approval.runId,
                },
              ]
            : [],
      };
    }

    case "CompleteMission": {
      const nodes = Object.values(state.nodes);
      assert(nodes.length > 0, "plan-missing", "没有 Plan 的 Mission 不能完成");
      assert(nodes.every((node) => node.status === "accepted"), "nodes-incomplete", "仍有节点未验收");
      return {
        events: [{ type: "mission.completed" }],
        effects: [],
      };
    }
  }
}

/** Reducer 是纯函数；Event Replay 完全依赖它。 */
export function reduceMission(state: Readonly<MissionState>, event: Readonly<DomainEvent>): MissionState {
  const nextVersion = state.version + 1;

  switch (event.type) {
    case "mission.created":
      return {
        ...state,
        missionId: event.missionId,
        projectId: event.projectId,
        goal: event.goal,
        version: nextVersion,
      };

    case "mission.plan-installed":
      return {
        ...state,
        status: "running",
        version: nextVersion,
        nodes: Object.fromEntries(
          event.nodes.map((node) => [node.id, { ...node, status: "queued" as const }]),
        ),
      };

    case "node.started": {
      const node = state.nodes[event.nodeId];
      return {
        ...state,
        version: nextVersion,
        nodes: {
          ...state.nodes,
          [event.nodeId]: { ...node, status: "running", runId: event.runId },
        },
        runs: {
          ...state.runs,
          [event.runId]: {
            id: event.runId,
            nodeId: node.id,
            agentDefinitionId: node.agentDefinitionId,
            endpointId: `endpoint:${node.agentDefinitionId}`,
            attempt: 1,
            status: "running",
          },
        },
      };
    }

    case "node.submitted": {
      const node = state.nodes[event.nodeId];
      return {
        ...state,
        version: nextVersion,
        nodes: {
          ...state.nodes,
          [event.nodeId]: {
            ...node,
            status: "submitted",
            outputSummary: event.outputSummary,
          },
        },
        runs: {
          ...state.runs,
          [event.runId]: { ...state.runs[event.runId], status: "submitted" },
        },
      };
    }

    case "node.accepted": {
      const node = state.nodes[event.nodeId];
      return {
        ...state,
        version: nextVersion,
        nodes: {
          ...state.nodes,
          [event.nodeId]: { ...node, status: "accepted" },
        },
        runs: {
          ...state.runs,
          [event.runId]: { ...state.runs[event.runId], status: "accepted" },
        },
      };
    }

    case "approval.requested": {
      const node = state.nodes[event.nodeId];
      return {
        ...state,
        version: nextVersion,
        nodes: {
          ...state.nodes,
          [event.nodeId]: {
            ...node,
            status: "waiting-approval",
            approvalId: event.approvalId,
          },
        },
        approvals: {
          ...state.approvals,
          [event.approvalId]: {
            id: event.approvalId,
            nodeId: event.nodeId,
            runId: event.runId,
            action: event.action,
            reason: event.reason,
            status: "pending",
          },
        },
        runs: {
          ...state.runs,
          [event.runId]: { ...state.runs[event.runId], status: "waiting-approval" },
        },
      };
    }

    case "approval.resolved": {
      const approval = state.approvals[event.approvalId];
      const node = state.nodes[approval.nodeId];
      return {
        ...state,
        version: nextVersion,
        approvals: {
          ...state.approvals,
          [approval.id]: {
            ...approval,
            status: event.decision === "allow" ? "allowed" : "denied",
          },
        },
        nodes: {
          ...state.nodes,
          [node.id]: {
            ...node,
            status: event.decision === "allow" ? "running" : "failed",
          },
        },
        runs: {
          ...state.runs,
          [approval.runId]: {
            ...state.runs[approval.runId],
            status: event.decision === "allow" ? "running" : "failed",
          },
        },
      };
    }

    case "mission.completed":
      return { ...state, status: "completed", version: nextVersion };
  }
}

/** 计算当前可启动节点；Join 也遵循相同依赖规则。 */
export function findReadyNodeIds(state: Readonly<MissionState>): string[] {
  return Object.values(state.nodes)
    .filter(
      (node) =>
        node.status === "queued" &&
        node.dependsOn.every((dependencyId) => state.nodes[dependencyId]?.status === "accepted"),
    )
    .map((node) => node.id)
    .sort();
}
