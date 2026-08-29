import type { DomainEvent, EffectIntent, OutboxRecord, StoredEvent } from "../../domain/src/model.ts";

/**
 * KernelStore 是确定性内核唯一依赖的持久化端口。
 * 内存和 SQLite 实现必须遵守完全相同的 CAS、Outbox 与 Claim 语义。
 */
export interface KernelStore {
  listMissionIds(): string[];
  readEvents(missionId: string): StoredEvent[];
  currentVersion(missionId: string): number;
  commit(input: {
    missionId: string;
    expectedVersion: number;
    events: DomainEvent[];
    effects: EffectIntent[];
  }): StoredEvent[];
  claimPending(limit: number): OutboxRecord[];
  completeEffect(id: string, claimToken: string): void;
  failEffect(id: string, claimToken: string): void;
  listOutbox(): OutboxRecord[];
}
