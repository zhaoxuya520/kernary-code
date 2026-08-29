import type { NodeKind, NodeStatus } from "../../domain/src/model.ts";

/** Terminal/JSON/API 都可消费的稳定事件投影项。 */
export interface PresentationItem {
  id: string;
  sequence: number;
  type: string;
  actor: "kernel" | "worker" | "verifier" | "user";
  title: string;
  summary: string;
  status: "started" | "completed" | "waiting";
}

export interface AgentLaneView {
  nodeId: string;
  title: string;
  kind: NodeKind;
  agentDefinitionId: string;
  runId?: string;
  status: NodeStatus;
  dependsOn: string[];
}

/** 不绑定具体 Interface 的 Mission 只读 ViewModel。 */
export interface MissionPresentationView {
  missionId: string;
  projectId: string;
  goal: string;
  status: string;
  version: number;
  lanes: AgentLaneView[];
  items: PresentationItem[];
  pendingApprovalIds: string[];
}
