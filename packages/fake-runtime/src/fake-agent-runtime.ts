import type { OutboxRecord } from "../../domain/src/model.ts";
import type { EffectHandler } from "../../kernel/src/outbox-runner.ts";
import { MissionActor } from "../../kernel/src/mission-actor.ts";

/**
 * Fake Runtime 用固定规则代替模型，便于先验证内核。
 * worker-b 会申请一次权限，模拟真实 Agent 被 sandbox/approval 阻塞。
 */
export class FakeAgentRuntime implements EffectHandler {
  readonly actor: MissionActor;

  constructor(actor: MissionActor) {
    this.actor = actor;
  }

  async execute(record: OutboxRecord): Promise<void> {
    const effect = record.effect;
    const node = this.actor.state.nodes[effect.nodeId];
    if (!node) throw new Error(`fake-runtime-node-not-found: ${effect.nodeId}`);

    switch (effect.kind) {
      case "start-fake-run":
        if (node.requiresApproval) {
          await this.actor.dispatch({
            type: "RequestApproval",
            nodeId: node.id,
            runId: effect.runId,
            approvalId: `approval:${effect.runId}`,
            action: "filesystem.write",
            reason: `${node.title} 需要写入项目文件`,
          });
          return;
        }

        await this.actor.dispatch({
          type: "SubmitNode",
          nodeId: node.id,
          runId: effect.runId,
          outputSummary: `${node.title} 已由 ${node.agentDefinitionId} 完成`,
        });
        return;

      case "resume-fake-run":
        await this.actor.dispatch({
          type: "SubmitNode",
          nodeId: node.id,
          runId: effect.runId,
          outputSummary: `${node.title} 在获得一次性权限后完成`,
        });
        return;

      case "verify-fake-run":
        // D0 Verifier 固定接受；下一阶段会加入 acceptance criteria 和 Evidence。
        await this.actor.dispatch({
          type: "AcceptNode",
          nodeId: node.id,
          runId: effect.runId,
        });
        return;
    }
  }
}
