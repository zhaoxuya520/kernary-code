import { findReadyNodeIds } from "../../../packages/domain/src/mission.ts";
import { FakeAgentRuntime } from "../../../packages/fake-runtime/src/fake-agent-runtime.ts";
import { InMemoryKernelStore } from "../../../packages/kernel/src/in-memory-kernel-store.ts";
import { MissionActor } from "../../../packages/kernel/src/mission-actor.ts";
import { OutboxRunner } from "../../../packages/kernel/src/outbox-runner.ts";
import { projectMission } from "../../../packages/presentation-model/src/mission-projection.ts";

/** 启动当前所有 ready 节点；没有依赖的两个 Worker 会一起进入 Outbox。 */
async function startReadyNodes(actor: MissionActor): Promise<void> {
  for (const nodeId of findReadyNodeIds(actor.state)) {
    await actor.dispatch({ type: "StartNode", nodeId, runId: `run:${nodeId}` });
  }
}

async function main(): Promise<void> {
  const store = new InMemoryKernelStore();
  const actor = new MissionActor("mission:demo", store);
  const runtime = new FakeAgentRuntime(actor);
  const outbox = new OutboxRunner(store, runtime, 4);

  await actor.dispatch({
    type: "CreateMission",
    missionId: "mission:demo",
    projectId: "project:harness",
    goal: "演示两个 Agent 并行、审批、验证和 Join",
  });

  await actor.dispatch({
    type: "InstallPlan",
    nodes: [
      {
        id: "worker-a",
        title: "设计协议",
        kind: "task",
        dependsOn: [],
        agentDefinitionId: "agent:protocol-specialist",
      },
      {
        id: "worker-b",
        title: "实现 UI 投影",
        kind: "task",
        dependsOn: [],
        agentDefinitionId: "agent:ui-specialist",
        requiresApproval: true,
      },
      {
        id: "join",
        title: "合并并验证结果",
        kind: "join",
        dependsOn: ["worker-a", "worker-b"],
        agentDefinitionId: "agent:verifier",
      },
    ],
  });

  // 第一轮同时启动 A/B；A 完成，B 等待用户审批。
  await startReadyNodes(actor);
  await outbox.drain();

  const approvalId = Object.values(actor.state.approvals).find(
    (approval) => approval.status === "pending",
  )?.id;
  if (!approvalId) throw new Error("demo-approval-not-created");

  await actor.dispatch({ type: "ResolveApproval", approvalId, decision: "allow" });
  await outbox.drain();

  // 两个 Worker 都 accepted 后，Join 才能启动。
  await startReadyNodes(actor);
  await outbox.drain();
  await actor.dispatch({ type: "CompleteMission" });

  const view = projectMission(actor.missionId, store.readEvents(actor.missionId));
  console.log(JSON.stringify(view, null, 2));
}

await main();
