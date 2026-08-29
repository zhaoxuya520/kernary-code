import type { MissionCommand, MissionState, StoredEvent } from "../../domain/src/model.ts";
import { createEmptyMissionState } from "../../domain/src/model.ts";
import { decideMission, reduceMission } from "../../domain/src/mission.ts";
import type { KernelStore } from "./kernel-store.ts";

/**
 * MissionActor 保证同一个 Mission 的命令串行执行。
 * 多个不同 Mission 仍可由不同 Actor 并行运行。
 */
export class MissionActor {
  readonly missionId: string;
  readonly store: KernelStore;
  #state: MissionState;
  #queue: Promise<unknown> = Promise.resolve();
  readonly #listeners = new Set<(events: StoredEvent[]) => void>();

  constructor(missionId: string, store: KernelStore) {
    this.missionId = missionId;
    this.store = store;
    this.#state = replayMission(missionId, store.readEvents(missionId));
  }

  get state(): Readonly<MissionState> {
    return this.#state;
  }

  /** UI/App Server 只能订阅已经提交的事件，不能订阅事务中间状态。 */
  subscribe(listener: (events: StoredEvent[]) => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  dispatch(command: MissionCommand): Promise<StoredEvent[]> {
    const execute = (): StoredEvent[] => {
      const decision = decideMission(this.#state, command);
      const recorded = this.store.commit({
        missionId: this.missionId,
        expectedVersion: this.#state.version,
        events: decision.events,
        effects: decision.effects,
      });

      for (const storedEvent of recorded) {
        this.#state = reduceMission(this.#state, storedEvent.event);
      }

      // Listener 属于投影层；它失败不能回滚已经成功的领域事务。
      for (const listener of this.#listeners) {
        try {
          listener(recorded);
        } catch {
          // 后续会把投影错误写入 diagnostics；D0 先隔离错误。
        }
      }
      return recorded;
    };

    // 无论前一个命令成功还是失败，后续命令都能继续排队。
    const result = this.#queue.then(execute, execute);
    this.#queue = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }
}

/** 从不可变事件重新构造 Mission；这是崩溃恢复和一致性测试的基础。 */
export function replayMission(missionId: string, events: StoredEvent[]): MissionState {
  return events.reduce(
    (state, storedEvent) => reduceMission(state, storedEvent.event),
    createEmptyMissionState(missionId),
  );
}
