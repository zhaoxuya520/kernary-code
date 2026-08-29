import { DatabaseSync } from "node:sqlite";
import type {
  ToolInvocationJournal,
  ToolInvocationRecord,
} from "../../tool-runtime/src/tool-runtime.ts";

type SqlValue = string | number | bigint | null;
type SqlRow = Record<string, SqlValue>;

function text(value: SqlValue | undefined, column: string): string {
  if (typeof value !== "string") throw new Error(`tool-journal-invalid-text: ${column}`);
  return value;
}

function parseRecord(row: SqlRow): ToolInvocationRecord {
  const runId = row.run_id === null ? undefined : text(row.run_id, "run_id");
  const approvalRequestId = row.approval_request_id === null ? undefined : text(row.approval_request_id, "approval_request_id");
  const result = row.result_json === null ? undefined : JSON.parse(text(row.result_json, "result_json"));
  const error = row.error === null ? undefined : text(row.error, "error");
  return {
    id: text(row.id, "id"),
    idempotencyKey: text(row.idempotency_key, "idempotency_key"),
    projectId: text(row.project_id, "project_id"),
    missionId: text(row.mission_id, "mission_id"),
    ...(runId ? { runId } : {}),
    actorId: text(row.actor_id, "actor_id"),
    toolName: text(row.tool_name, "tool_name"),
    toolVersion: text(row.tool_version, "tool_version"),
    effectClass: text(row.effect_class, "effect_class") as ToolInvocationRecord["effectClass"],
    status: text(row.status, "status") as ToolInvocationRecord["status"],
    args: JSON.parse(text(row.args_json, "args_json")),
    permissionAction: JSON.parse(text(row.permission_action_json, "permission_action_json")),
    ...(approvalRequestId ? { approvalRequestId } : {}),
    ...(result === undefined ? {} : { result }),
    ...(error === undefined ? {} : { error }),
    createdAt: text(row.created_at, "created_at"),
    updatedAt: text(row.updated_at, "updated_at"),
  };
}

export class SQLiteToolInvocationJournal implements ToolInvocationJournal {
  readonly #database: DatabaseSync;
  readonly #ownsDatabase: boolean;

  constructor(pathOrDatabase: string | DatabaseSync) {
    this.#ownsDatabase = typeof pathOrDatabase === "string";
    this.#database = typeof pathOrDatabase === "string" ? new DatabaseSync(pathOrDatabase) : pathOrDatabase;
    this.#database.exec("PRAGMA busy_timeout = 5000");
    this.#database.exec(`
      CREATE TABLE IF NOT EXISTS tool_invocations (
        id TEXT PRIMARY KEY,
        idempotency_key TEXT NOT NULL UNIQUE,
        project_id TEXT NOT NULL,
        mission_id TEXT NOT NULL,
        run_id TEXT,
        actor_id TEXT NOT NULL,
        tool_name TEXT NOT NULL,
        tool_version TEXT NOT NULL,
        effect_class TEXT NOT NULL,
        status TEXT NOT NULL,
        args_json TEXT NOT NULL,
        permission_action_json TEXT NOT NULL,
        approval_request_id TEXT,
        result_json TEXT,
        error TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );
      CREATE INDEX IF NOT EXISTS idx_tool_invocations_run_status
        ON tool_invocations (run_id, status, updated_at);
    `);
  }

  close(): void {
    if (this.#ownsDatabase) this.#database.close();
  }

  create(record: ToolInvocationRecord): void {
    this.#database
      .prepare(`
        INSERT INTO tool_invocations (
          id, idempotency_key, project_id, mission_id, run_id, actor_id,
          tool_name, tool_version, effect_class, status, args_json,
          permission_action_json, approval_request_id, result_json, error,
          created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      `)
      .run(
        record.id,
        record.idempotencyKey,
        record.projectId,
        record.missionId,
        record.runId ?? null,
        record.actorId,
        record.toolName,
        record.toolVersion,
        record.effectClass,
        record.status,
        JSON.stringify(record.args),
        JSON.stringify(record.permissionAction),
        record.approvalRequestId ?? null,
        record.result === undefined ? null : JSON.stringify(record.result),
        record.error ?? null,
        record.createdAt,
        record.updatedAt,
      );
  }

  update(id: string, patch: Partial<ToolInvocationRecord>): ToolInvocationRecord {
    const current = this.get(id);
    if (!current) throw new Error(`tool-invocation-not-found: ${id}`);
    const next = { ...current, ...patch, id: current.id, idempotencyKey: current.idempotencyKey };
    const result = this.#database
      .prepare(`
        UPDATE tool_invocations SET
          status = ?, approval_request_id = ?, result_json = ?, error = ?, updated_at = ?
        WHERE id = ?
      `)
      .run(
        next.status,
        next.approvalRequestId ?? null,
        next.result === undefined ? null : JSON.stringify(next.result),
        next.error ?? null,
        next.updatedAt,
        id,
      );
    if (Number(result.changes) !== 1) throw new Error(`tool-invocation-update-conflict: ${id}`);
    const updated = this.get(id);
    if (!updated) throw new Error(`tool-invocation-lost-after-update: ${id}`);
    return updated;
  }

  get(id: string): ToolInvocationRecord | undefined {
    const row = this.#database.prepare("SELECT * FROM tool_invocations WHERE id = ?").get(id) as SqlRow | undefined;
    return row ? parseRecord(row) : undefined;
  }

  findByIdempotencyKey(idempotencyKey: string): ToolInvocationRecord | undefined {
    const row = this.#database
      .prepare("SELECT * FROM tool_invocations WHERE idempotency_key = ?")
      .get(idempotencyKey) as SqlRow | undefined;
    return row ? parseRecord(row) : undefined;
  }

  list(): ToolInvocationRecord[] {
    return (this.#database.prepare("SELECT * FROM tool_invocations ORDER BY created_at, id").all() as SqlRow[]).map(parseRecord);
  }
}
