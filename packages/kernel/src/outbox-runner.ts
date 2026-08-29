import type { OutboxRecord } from "../../domain/src/model.ts";
import type { KernelStore } from "./kernel-store.ts";

export interface EffectHandler {
  execute(record: OutboxRecord): Promise<void>;
}

/**
 * OutboxRunner 批量领取 Effect，并行执行不同 Agent 的工作。
 * Mission 的回报仍会经过 MissionActor 串行化，所以“副作用并行、状态写入串行”。
 */
export class OutboxRunner {
  readonly store: KernelStore;
  readonly handler: EffectHandler;
  readonly concurrency: number;

  constructor(store: KernelStore, handler: EffectHandler, concurrency = 4) {
    this.store = store;
    this.handler = handler;
    this.concurrency = concurrency;
  }

  async drain(): Promise<void> {
    while (true) {
      const claimed = this.store.claimPending(this.concurrency);
      if (claimed.length === 0) return;

      await Promise.all(
        claimed.map(async (record) => {
          const claimToken = record.claimToken;
          if (!claimToken) throw new Error(`missing-claim-token: ${record.id}`);
          try {
            await this.handler.execute(record);
            this.store.completeEffect(record.id, claimToken);
          } catch (error) {
            this.store.failEffect(record.id, claimToken);
            throw error;
          }
        }),
      );
    }
  }
}
