import type { StoredEvent } from "../../domain/src/model.ts";
import { replayMission } from "../../kernel/src/mission-actor.ts";
import type { MissionPresentationView, PresentationItem } from "./types.ts";

function projectItem(stored: StoredEvent): PresentationItem {
  const event = stored.event;
  const base = {
    id: `${stored.missionId}:${stored.sequence}`,
    sequence: stored.sequence,
  };

  switch (event.type) {
    case "mission.created":
      return { ...base, type: event.type, actor: "user", title: "创建任务", summary: event.goal, status: "completed" };
    case "mission.plan-installed":
      return { ...base, type: event.type, actor: "kernel", title: "安装任务图", summary: `${event.nodes.length} 个节点`, status: "completed" };
    case "node.started":
      return { ...base, type: event.type, actor: "worker", title: `启动 ${event.nodeId}`, summary: event.runId, status: "started" };
    case "node.submitted":
      return { ...base, type: event.type, actor: "worker", title: `提交 ${event.nodeId}`, summary: event.outputSummary, status: "completed" };
    case "node.accepted":
      return { ...base, type: event.type, actor: "verifier", title: `验收 ${event.nodeId}`, summary: event.runId, status: "completed" };
    case "approval.requested":
      return { ...base, type: event.type, actor: "worker", title: "等待权限", summary: `${event.action}：${event.reason}`, status: "waiting" };
    case "approval.resolved":
      return { ...base, type: event.type, actor: "user", title: "审批已处理", summary: event.decision, status: "completed" };
    case "mission.completed":
      return { ...base, type: event.type, actor: "kernel", title: "Mission 完成", summary: "所有节点均已验收", status: "completed" };
  }
}

/** 把 Event Stream 投影成 Terminal/JSON/API 共用的只读模型。 */
export function projectMission(missionId: string, events: StoredEvent[]): MissionPresentationView {
  const state = replayMission(missionId, events);
  return {
    missionId: state.missionId,
    projectId: state.projectId,
    goal: state.goal,
    status: state.status,
    version: state.version,
    lanes: Object.values(state.nodes)
      .map((node) => ({
        nodeId: node.id,
        title: node.title,
        kind: node.kind,
        agentDefinitionId: node.agentDefinitionId,
        runId: node.runId,
        status: node.status,
        dependsOn: [...node.dependsOn],
      }))
      .sort((a, b) => a.nodeId.localeCompare(b.nodeId)),
    items: events.map(projectItem),
    pendingApprovalIds: Object.values(state.approvals)
      .filter((approval) => approval.status === "pending")
      .map((approval) => approval.id),
  };
}
