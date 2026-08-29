import { randomUUID } from "node:crypto";
import type { DomainEvent, EffectIntent, OutboxRecord, StoredEvent } from "../../domain/src/model.ts";
import type { KernelStore } from "./kernel-store.ts";

/** 内存 Store 只用于 D0 测试；接口语义之后会原样迁移到 SQLite。 */
export class InMemoryKernelStore implements KernelStore {
  readonly #eventsByMission = new Map<string, StoredEvent[]>();
  readonly #outbox = new Map<string, OutboxRecord>();
  #globalSequence = 0;

  listMissionIds(): string[] {
    return [...this.#eventsByMission.entries()]
      .filter(([, events]) => events.length > 0)
      .sort((a, b) => (a[1].at(-1)?.sequence ?? 0) - (b[1].at(-1)?.sequence ?? 0))
      .map(([missionId]) => missionId);
  }

  readEvents(missionId: string): StoredEvent[] {
    return [...(this.#eventsByMission.get(missionId) ?? [])];
  }

  currentVersion(missionId: string): number {
    return this.#eventsByMission.get(missionId)?.length ?? 0;
  }

  /**
   * 事件与 Outbox 在同一个同步临界区提交。
   * SQLite 版本会把这里替换成 BEGIN IMMEDIATE / COMMIT 事务。
   */
  commit(input: {
    missionId: string;
    expectedVersion: number;
    events: DomainEvent[];
    effects: EffectIntent[];
  }): StoredEvent[] {
    const currentVersion = this.currentVersion(input.missionId);
    if (currentVersion !== input.expectedVersion) {
      throw new Error(`version-conflict: expected=${input.expectedVersion}, actual=${currentVersion}`);
    }

    const existing = this.#eventsByMission.get(input.missionId) ?? [];
    const recorded: StoredEvent[] = input.events.map((event, index) => ({
      sequence: ++this.#globalSequence,
      missionId: input.missionId,
      aggregateVersion: currentVersion + index + 1,
      event,
      recordedAt: new Date().toISOString(),
    }));

    // 先构造全部记录，再一次性替换集合，模拟事务的“全有或全无”。
    const nextEvents = [...existing, ...recorded];
    const nextOutbox = input.effects.map<OutboxRecord>((effect) => ({
      id: randomUUID(),
      missionId: input.missionId,
      aggregateVersion: currentVersion + input.events.length,
      effect,
      status: "pending",
      attempts: 0,
    }));

    this.#eventsByMission.set(input.missionId, nextEvents);
    for (const record of nextOutbox) this.#outbox.set(record.id, record);
    return recorded;
  }

  claimPending(limit: number): OutboxRecord[] {
    const records = [...this.#outbox.values()]
      .filter((record) => record.status === "pending")
      .slice(0, limit);

    return records.map((record) => {
      const claimed: OutboxRecord = {
        ...record,
        status: "claimed",
        claimToken: randomUUID(),
        attempts: record.attempts + 1,
      };
      this.#outbox.set(record.id, claimed);
      return claimed;
    });
  }

  completeEffect(id: string, claimToken: string): void {
    const record = this.#outbox.get(id);
    if (!record || record.status !== "claimed" || record.claimToken !== claimToken) {
      throw new Error(`stale-effect-claim: ${id}`);
    }
    this.#outbox.set(id, { ...record, status: "completed" });
  }

  failEffect(id: string, claimToken: string): void {
    const record = this.#outbox.get(id);
    if (!record || record.status !== "claimed" || record.claimToken !== claimToken) {
      throw new Error(`stale-effect-claim: ${id}`);
    }
    this.#outbox.set(id, { ...record, status: "failed" });
  }

  listOutbox(): OutboxRecord[] {
    return [...this.#outbox.values()];
  }
}
