import { randomUUID } from "node:crypto";
import type { MissionCommand, StoredEvent, WorkflowNodeDefinition } from "../../domain/src/model.ts";
import { findReadyNodeIds } from "../../domain/src/mission.ts";
import { FakeAgentRuntime } from "../../fake-runtime/src/fake-agent-runtime.ts";
import { InMemoryKernelStore } from "../../kernel/src/in-memory-kernel-store.ts";
import type { KernelStore } from "../../kernel/src/kernel-store.ts";
import { MissionActor } from "../../kernel/src/mission-actor.ts";
import { OutboxRunner } from "../../kernel/src/outbox-runner.ts";
import type { MissionPresentationView } from "../../presentation-model/src/types.ts";
import { projectMission } from "../../presentation-model/src/mission-projection.ts";

interface MissionRuntime {
  actor: MissionActor;
  runner: OutboxRunner;
}

export interface CreateMissionRequest {
  missionId?: string;
  projectId: string;
  goal: string;
}

/**
 * D0 App Server 是进程内实现；以后只需在外面增加 IPC/STDIO/WebSocket Transport。
 * Interface 不会直接拿到 Actor 或 Store，只能调用这里的命令和查询。
 */
export class HarnessAppServer {
  readonly #store: KernelStore;
  readonly #missions = new Map<string, MissionRuntime>();

  constructor(store: KernelStore = new InMemoryKernelStore()) {
    this.#store = store;
  }

  async createMission(request: CreateMissionRequest): Promise<MissionPresentationView> {
    const missionId = request.missionId ?? `mission:${randomUUID()}`;
    if (this.#missions.has(missionId) || this.#store.currentVersion(missionId) > 0) {
      throw new Error(`mission-exists: ${missionId}`);
    }

    const actor = this.registerRuntime(missionId).actor;

    await actor.dispatch({
      type: "CreateMission",
      missionId,
      projectId: request.projectId,
      goal: request.goal,
    });
    return this.readMission(missionId);
  }

  /** Core 重启后从持久化事件恢复一个 Mission Runtime。 */
  openMission(missionId: string): MissionPresentationView {
    if (!this.#missions.has(missionId)) {
      if (this.#store.currentVersion(missionId) === 0) throw new Error(`mission-not-found: ${missionId}`);
      this.registerRuntime(missionId);
    }
    return this.readMission(missionId);
  }

  listMissionIds(): string[] {
    return this.#store.listMissionIds();
  }

  async installPlan(missionId: string, nodes: WorkflowNodeDefinition[]): Promise<MissionPresentationView> {
    await this.dispatch(missionId, { type: "InstallPlan", nodes });
    return this.readMission(missionId);
  }

  async respondApproval(
    missionId: string,
    approvalId: string,
    decision: "allow" | "deny",
  ): Promise<MissionPresentationView> {
    await this.dispatch(missionId, { type: "ResolveApproval", approvalId, decision });
    return this.readMission(missionId);
  }

  /**
   * 驱动任务直到完成、等待审批或没有可推进工作。
   * 这段循环以后会搬到持久化 Scheduler，而 RPC 接口保持不变。
   */
  async runUntilBlocked(missionId: string): Promise<MissionPresentationView> {
    const runtime = this.runtime(missionId);

    while (runtime.actor.state.status === "running") {
      const readyNodeIds = findReadyNodeIds(runtime.actor.state);
      for (const nodeId of readyNodeIds) {
        await runtime.actor.dispatch({
          type: "StartNode",
          nodeId,
          runId: `run:${nodeId}:${randomUUID()}`,
        });
      }

      await runtime.runner.drain();

      const hasPendingApproval = Object.values(runtime.actor.state.approvals).some(
        (approval) => approval.status === "pending",
      );
      if (hasPendingApproval) break;

      const allAccepted =
        Object.keys(runtime.actor.state.nodes).length > 0 &&
        Object.values(runtime.actor.state.nodes).every((node) => node.status === "accepted");
      if (allAccepted) {
        await runtime.actor.dispatch({ type: "CompleteMission" });
        break;
      }

      if (findReadyNodeIds(runtime.actor.state).length === 0) break;
    }

    return this.readMission(missionId);
  }

  readMission(missionId: string): MissionPresentationView {
    this.runtime(missionId);
    return projectMission(missionId, this.#store.readEvents(missionId));
  }

  /** afterSequence 用于 Interface 断线重连后的增量订阅。 */
  subscribeMission(
    missionId: string,
    afterSequence: number,
    listener: (events: StoredEvent[]) => void,
  ): () => void {
    const runtime = this.runtime(missionId);
    const missed = this.#store
      .readEvents(missionId)
      .filter((event) => event.sequence > afterSequence);
    if (missed.length > 0) listener(missed);
    return runtime.actor.subscribe(listener);
  }

  async dispatch(missionId: string, command: MissionCommand): Promise<StoredEvent[]> {
    return this.runtime(missionId).actor.dispatch(command);
  }

  private runtime(missionId: string): MissionRuntime {
    const runtime = this.#missions.get(missionId);
    if (!runtime) throw new Error(`mission-runtime-not-found: ${missionId}`);
    return runtime;
  }

  private registerRuntime(missionId: string): MissionRuntime {
    const actor = new MissionActor(missionId, this.#store);
    const runtime = {
      actor,
      runner: new OutboxRunner(this.#store, new FakeAgentRuntime(actor), 4),
    };
    this.#missions.set(missionId, runtime);
    return runtime;
  }
}
