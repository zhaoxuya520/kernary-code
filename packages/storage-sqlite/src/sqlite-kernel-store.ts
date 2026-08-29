import { randomUUID } from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import type { DomainEvent, EffectIntent, OutboxRecord, StoredEvent } from "../../domain/src/model.ts";
import type { KernelStore } from "../../kernel/src/kernel-store.ts";
import { parseDomainEvent, parseEffectIntent, parseJson } from "../../protocol/src/runtime-schema.ts";

type SqlValue = string | number | bigint | null;
type SqlRow = Record<string, SqlValue>;

function text(value: SqlValue | undefined, column: string): string {
  if (typeof value !== "string") throw new Error(`sqlite-invalid-text: ${column}`);
  return value;
}

function number(value: SqlValue | undefined, column: string): number {
  if (typeof value !== "number" && typeof value !== "bigint") {
    throw new Error(`sqlite-invalid-number: ${column}`);
  }
  return Number(value);
}

/**
 * 使用 Node 24 内置 node:sqlite，不引入第三方 ORM。
 * 这一版实现 Event Store、CAS、Transactional Outbox 和 Claim Token。
 */
export class SQLiteKernelStore implements KernelStore {
  readonly #database: DatabaseSync;

  constructor(path: string) {
    this.#database = new DatabaseSync(path);
    this.#database.exec("PRAGMA foreign_keys = ON");
    this.#database.exec("PRAGMA busy_timeout = 5000");
    this.#database.exec("PRAGMA journal_mode = WAL");
    this.migrate();
  }

  close(): void {
    this.#database.close();
  }

  private migrate(): void {
    this.#database.exec(`
      CREATE TABLE IF NOT EXISTS kernel_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );

      CREATE TABLE IF NOT EXISTS aggregate_events (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        mission_id TEXT NOT NULL,
        aggregate_version INTEGER NOT NULL,
        event_type TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        recorded_at TEXT NOT NULL,
        UNIQUE (mission_id, aggregate_version)
      );

      CREATE INDEX IF NOT EXISTS idx_events_mission_sequence
        ON aggregate_events (mission_id, sequence);

      CREATE TABLE IF NOT EXISTS outbox (
        id TEXT PRIMARY KEY,
        mission_id TEXT NOT NULL,
        aggregate_version INTEGER NOT NULL,
        effect_kind TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('pending', 'claimed', 'completed', 'failed')),
        claim_token TEXT,
        attempts INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_outbox_status_created
        ON outbox (status, created_at, id);

      INSERT OR IGNORE INTO kernel_meta (key, value) VALUES ('schema_version', '1');
    `);

    const row = this.#database.prepare("SELECT value FROM kernel_meta WHERE key = 'schema_version'").get() as
      | SqlRow
      | undefined;
    if (!row || text(row.value, "kernel_meta.value") !== "1") {
      throw new Error("unsupported-kernel-schema-version");
    }
  }

  listMissionIds(): string[] {
    const rows = this.#database
      .prepare(`
        SELECT mission_id, MAX(sequence) AS last_sequence
        FROM aggregate_events
        GROUP BY mission_id
        ORDER BY last_sequence ASC
      `)
      .all() as SqlRow[];
    return rows.map((row) => text(row.mission_id, "aggregate_events.mission_id"));
  }

  readEvents(missionId: string): StoredEvent[] {
    const rows = this.#database
      .prepare(`
        SELECT sequence, mission_id, aggregate_version, payload_json, recorded_at
        FROM aggregate_events
        WHERE mission_id = ?
        ORDER BY aggregate_version ASC
      `)
      .all(missionId) as SqlRow[];

    return rows.map((row) => ({
      sequence: number(row.sequence, "aggregate_events.sequence"),
      missionId: text(row.mission_id, "aggregate_events.mission_id"),
      aggregateVersion: number(row.aggregate_version, "aggregate_events.aggregate_version"),
      event: parseDomainEvent(
        parseJson(text(row.payload_json, "aggregate_events.payload_json"), "aggregate_events.payload_json"),
      ),
      recordedAt: text(row.recorded_at, "aggregate_events.recorded_at"),
    }));
  }

  currentVersion(missionId: string): number {
    const row = this.#database
      .prepare(`
        SELECT COALESCE(MAX(aggregate_version), 0) AS version
        FROM aggregate_events
        WHERE mission_id = ?
      `)
      .get(missionId) as SqlRow;
    return number(row.version, "aggregate_events.version");
  }

  commit(input: {
    missionId: string;
    expectedVersion: number;
    events: DomainEvent[];
    effects: EffectIntent[];
  }): StoredEvent[] {
    this.#database.exec("BEGIN IMMEDIATE");
    try {
      const actualVersion = this.currentVersion(input.missionId);
      if (actualVersion !== input.expectedVersion) {
        throw new Error(`version-conflict: expected=${input.expectedVersion}, actual=${actualVersion}`);
      }

      const insertEvent = this.#database.prepare(`
        INSERT INTO aggregate_events (
          mission_id, aggregate_version, event_type, payload_json, recorded_at
        ) VALUES (?, ?, ?, ?, ?)
      `);
      const recorded: StoredEvent[] = [];

      for (const [index, event] of input.events.entries()) {
        const aggregateVersion = actualVersion + index + 1;
        const recordedAt = new Date().toISOString();
        const result = insertEvent.run(
          input.missionId,
          aggregateVersion,
          event.type,
          JSON.stringify(event),
          recordedAt,
        );
        recorded.push({
          sequence: Number(result.lastInsertRowid),
          missionId: input.missionId,
          aggregateVersion,
          event,
          recordedAt,
        });
      }

      const effectVersion = actualVersion + input.events.length;
      const now = new Date().toISOString();
      const insertEffect = this.#database.prepare(`
        INSERT INTO outbox (
          id, mission_id, aggregate_version, effect_kind, payload_json,
          status, attempts, created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, 'pending', 0, ?, ?)
      `);
      for (const effect of input.effects) {
        insertEffect.run(
          randomUUID(),
          input.missionId,
          effectVersion,
          effect.kind,
          JSON.stringify(effect),
          now,
          now,
        );
      }

      this.#database.exec("COMMIT");
      return recorded;
    } catch (error) {
      this.#database.exec("ROLLBACK");
      throw error;
    }
  }

  claimPending(limit: number): OutboxRecord[] {
    this.#database.exec("BEGIN IMMEDIATE");
    try {
      const rows = this.#database
        .prepare(`
          SELECT id, mission_id, aggregate_version, payload_json, attempts
          FROM outbox
          WHERE status = 'pending'
          ORDER BY created_at ASC, id ASC
          LIMIT ?
        `)
        .all(limit) as SqlRow[];
      const update = this.#database.prepare(`
        UPDATE outbox
        SET status = 'claimed', claim_token = ?, attempts = attempts + 1, updated_at = ?
        WHERE id = ? AND status = 'pending'
      `);

      const claimed = rows.map<OutboxRecord>((row) => {
        const id = text(row.id, "outbox.id");
        const claimToken = randomUUID();
        const result = update.run(claimToken, new Date().toISOString(), id);
        if (Number(result.changes) !== 1) throw new Error(`outbox-claim-conflict: ${id}`);
        return {
          id,
          missionId: text(row.mission_id, "outbox.mission_id"),
          aggregateVersion: number(row.aggregate_version, "outbox.aggregate_version"),
          effect: parseEffectIntent(
            parseJson(text(row.payload_json, "outbox.payload_json"), "outbox.payload_json"),
          ),
          status: "claimed",
          claimToken,
          attempts: number(row.attempts, "outbox.attempts") + 1,
        };
      });

      this.#database.exec("COMMIT");
      return claimed;
    } catch (error) {
      this.#database.exec("ROLLBACK");
      throw error;
    }
  }

  completeEffect(id: string, claimToken: string): void {
    this.finishEffect(id, claimToken, "completed");
  }

  failEffect(id: string, claimToken: string): void {
    this.finishEffect(id, claimToken, "failed");
  }

  private finishEffect(id: string, claimToken: string, status: "completed" | "failed"): void {
    const result = this.#database
      .prepare(`
        UPDATE outbox
        SET status = ?, updated_at = ?
        WHERE id = ? AND status = 'claimed' AND claim_token = ?
      `)
      .run(status, new Date().toISOString(), id, claimToken);
    if (Number(result.changes) !== 1) throw new Error(`stale-effect-claim: ${id}`);
  }

  listOutbox(): OutboxRecord[] {
    const rows = this.#database
      .prepare(`
        SELECT id, mission_id, aggregate_version, payload_json, status, claim_token, attempts
        FROM outbox
        ORDER BY created_at ASC, id ASC
      `)
      .all() as SqlRow[];

    return rows.map((row) => ({
      id: text(row.id, "outbox.id"),
      missionId: text(row.mission_id, "outbox.mission_id"),
      aggregateVersion: number(row.aggregate_version, "outbox.aggregate_version"),
      effect: parseEffectIntent(
        parseJson(text(row.payload_json, "outbox.payload_json"), "outbox.payload_json"),
      ),
      status: text(row.status, "outbox.status") as OutboxRecord["status"],
      claimToken: row.claim_token === null ? undefined : text(row.claim_token, "outbox.claim_token"),
      attempts: number(row.attempts, "outbox.attempts"),
    }));
  }
}
