import { createHash, randomUUID } from "node:crypto";
import { mkdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

export type MemoryKind =
  | "architecture"
  | "decision"
  | "contract"
  | "lesson"
  | "failure"
  | "verification"
  | "meeting";

export type RetrievalMode = "metadata" | "lexical" | "semantic" | "hybrid" | "auto";
export type ExecutedRetrievalMode = Exclude<RetrievalMode, "auto">;

export interface MemoryRecordInput {
  id?: string;
  kind: MemoryKind;
  title: string;
  content: string;
  tags?: string[];
  sourceRef?: string;
  status?: "observed" | "verified";
}

export interface MemoryRecord {
  id: string;
  projectId: string;
  kind: MemoryKind;
  title: string;
  content: string;
  tags: string[];
  sourceRef?: string;
  status: "observed" | "verified";
  createdAt: string;
  updatedAt: string;
}

export interface MemorySearchResult {
  record: MemoryRecord;
  score: number;
  matchedBy: "metadata" | "fts" | "vector" | "hybrid";
}

export interface MemorySearchResponse {
  requestedMode: RetrievalMode;
  executedMode: ExecutedRetrievalMode;
  degraded: boolean;
  degradationReason?: "semantic-not-configured" | "semantic-not-ready" | "semantic-failed";
  results: MemorySearchResult[];
}

export interface EmbeddingProfile {
  id: string;
  model: string;
  dimensions: number;
}

export interface EmbeddingProviderPort {
  readonly profile: EmbeddingProfile;
  embed(text: string): Promise<number[]>;
}

export type SemanticStatus = "not-configured" | "initializing" | "indexing" | "ready" | "degraded";

export interface ProjectMemoryStatus {
  projectId: string;
  lexicalReady: true;
  recordCount: number;
  ftsIndexedCount: number;
  semanticConfigured: boolean;
  semanticStatus: SemanticStatus;
  embeddingProfile?: EmbeddingProfile;
  vectorIndexedCount: number;
  vectorPendingCount: number;
  databasePath: string;
  databaseBytes: number;
}

type SqlValue = string | number | bigint | null;
type SqlRow = Record<string, SqlValue>;

function sqlText(value: SqlValue | undefined, column: string): string {
  if (typeof value !== "string") throw new Error(`memory-invalid-text: ${column}`);
  return value;
}

function sqlNumber(value: SqlValue | undefined, column: string): number {
  if (typeof value !== "number" && typeof value !== "bigint") {
    throw new Error(`memory-invalid-number: ${column}`);
  }
  return Number(value);
}

function parseTags(value: SqlValue | undefined): string[] {
  const parsed = JSON.parse(sqlText(value, "memory_records.tags_json"));
  if (!Array.isArray(parsed) || parsed.some((item) => typeof item !== "string")) {
    throw new Error("memory-invalid-tags");
  }
  return parsed;
}

function rowToRecord(row: SqlRow): MemoryRecord {
  const sourceRef = row.source_ref === null ? undefined : sqlText(row.source_ref, "memory_records.source_ref");
  return {
    id: sqlText(row.id, "memory_records.id"),
    projectId: sqlText(row.project_id, "memory_records.project_id"),
    kind: sqlText(row.kind, "memory_records.kind") as MemoryKind,
    title: sqlText(row.title, "memory_records.title"),
    content: sqlText(row.content, "memory_records.content"),
    tags: parseTags(row.tags_json),
    ...(sourceRef === undefined ? {} : { sourceRef }),
    status: sqlText(row.status, "memory_records.status") as MemoryRecord["status"],
    createdAt: sqlText(row.created_at, "memory_records.created_at"),
    updatedAt: sqlText(row.updated_at, "memory_records.updated_at"),
  };
}

function cosineSimilarity(left: number[], right: number[]): number {
  if (left.length !== right.length || left.length === 0) return -1;
  let dot = 0;
  let leftNorm = 0;
  let rightNorm = 0;
  for (let index = 0; index < left.length; index += 1) {
    const a = left[index] ?? 0;
    const b = right[index] ?? 0;
    dot += a * b;
    leftNorm += a * a;
    rightNorm += b * b;
  }
  if (leftNorm === 0 || rightNorm === 0) return -1;
  return dot / (Math.sqrt(leftNorm) * Math.sqrt(rightNorm));
}

function validateVector(vector: number[], expectedDimensions: number): void {
  if (vector.length !== expectedDimensions || vector.some((value) => !Number.isFinite(value))) {
    throw new Error(`invalid-embedding-vector: expected=${expectedDimensions}, actual=${vector.length}`);
  }
}

function embeddingInput(record: MemoryRecord): string {
  return `${record.kind}\n${record.title}\n${record.content}\n${record.tags.join(" ")}`;
}

/**
 * 每个项目一个 SQLite 数据库。Lexical 永远存在，Semantic 只有传入 provider 才存在。
 * 没有 provider 时，本类不会构造替身 provider，也不会执行任何向量初始化 SQL。
 */
export class ProjectMemory {
  readonly projectId: string;
  readonly databasePath: string;
  readonly #database: DatabaseSync;
  readonly #embeddingProvider?: EmbeddingProviderPort;
  #semanticStatus: SemanticStatus;

  constructor(input: {
    projectId: string;
    databasePath: string;
    embeddingProvider?: EmbeddingProviderPort;
  }) {
    this.projectId = input.projectId;
    this.databasePath = input.databasePath;
    this.#embeddingProvider = input.embeddingProvider;
    this.#semanticStatus = input.embeddingProvider ? "initializing" : "not-configured";
    this.#database = new DatabaseSync(input.databasePath);
    this.#database.exec("PRAGMA foreign_keys = ON");
    this.#database.exec("PRAGMA busy_timeout = 5000");
    this.#database.exec("PRAGMA journal_mode = WAL");
    this.migrateLexical();

    // 只有真的配置了 Embedding Provider，才创建向量投影表。
    if (this.#embeddingProvider) this.migrateSemantic();
  }

  close(): void {
    this.#database.close();
  }

  private migrateLexical(): void {
    this.#database.exec(`
      CREATE TABLE IF NOT EXISTS memory_records (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        title TEXT NOT NULL,
        content TEXT NOT NULL,
        tags_json TEXT NOT NULL,
        source_ref TEXT,
        status TEXT NOT NULL CHECK (status IN ('observed', 'verified')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_memory_project_kind_status
        ON memory_records (project_id, kind, status, created_at);

      CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
        record_id UNINDEXED,
        project_id UNINDEXED,
        title,
        content,
        tags,
        tokenize='trigram'
      );
    `);
  }

  private migrateSemantic(): void {
    this.#database.exec(`
      CREATE TABLE IF NOT EXISTS embedding_profiles (
        id TEXT PRIMARY KEY,
        model TEXT NOT NULL,
        dimensions INTEGER NOT NULL,
        activated_at TEXT NOT NULL
      );

      CREATE TABLE IF NOT EXISTS memory_embeddings (
        record_id TEXT NOT NULL,
        profile_id TEXT NOT NULL,
        dimensions INTEGER NOT NULL,
        vector_json TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (record_id, profile_id),
        FOREIGN KEY (record_id) REFERENCES memory_records(id) ON DELETE CASCADE
      );
    `);
  }

  async initialize(): Promise<void> {
    const provider = this.#embeddingProvider;
    if (!provider) return;

    this.#semanticStatus = "indexing";
    try {
      this.#database
        .prepare(`
          INSERT INTO embedding_profiles (id, model, dimensions, activated_at)
          VALUES (?, ?, ?, ?)
          ON CONFLICT(id) DO UPDATE SET
            model = excluded.model,
            dimensions = excluded.dimensions,
            activated_at = excluded.activated_at
        `)
        .run(provider.profile.id, provider.profile.model, provider.profile.dimensions, new Date().toISOString());

      const missingRows = this.#database
        .prepare(`
          SELECT r.*
          FROM memory_records r
          LEFT JOIN memory_embeddings e
            ON e.record_id = r.id AND e.profile_id = ?
          WHERE r.project_id = ? AND e.record_id IS NULL
          ORDER BY r.created_at ASC
        `)
        .all(provider.profile.id, this.projectId) as SqlRow[];

      for (const row of missingRows) await this.indexRecord(rowToRecord(row));
      this.#semanticStatus = "ready";
    } catch (error) {
      this.#semanticStatus = "degraded";
      throw error;
    }
  }

  async addRecord(input: MemoryRecordInput): Promise<MemoryRecord> {
    const now = new Date().toISOString();
    const record: MemoryRecord = {
      id: input.id ?? randomUUID(),
      projectId: this.projectId,
      kind: input.kind,
      title: input.title.trim(),
      content: input.content.trim(),
      tags: [...new Set(input.tags ?? [])].sort(),
      ...(input.sourceRef === undefined ? {} : { sourceRef: input.sourceRef }),
      status: input.status ?? "observed",
      createdAt: now,
      updatedAt: now,
    };
    if (!record.title || !record.content) throw new Error("memory-title-and-content-required");

    this.#database.exec("BEGIN IMMEDIATE");
    try {
      this.#database
        .prepare(`
          INSERT INTO memory_records (
            id, project_id, kind, title, content, tags_json,
            source_ref, status, created_at, updated_at
          ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        `)
        .run(
          record.id,
          record.projectId,
          record.kind,
          record.title,
          record.content,
          JSON.stringify(record.tags),
          record.sourceRef ?? null,
          record.status,
          record.createdAt,
          record.updatedAt,
        );
      this.#database
        .prepare(`
          INSERT INTO memory_fts (record_id, project_id, title, content, tags)
          VALUES (?, ?, ?, ?, ?)
        `)
        .run(record.id, record.projectId, record.title, record.content, record.tags.join(" "));
      this.#database.exec("COMMIT");
    } catch (error) {
      this.#database.exec("ROLLBACK");
      throw error;
    }

    if (this.#embeddingProvider && this.#semanticStatus === "ready") {
      try {
        await this.indexRecord(record);
      } catch {
        this.#semanticStatus = "degraded";
      }
    }
    return record;
  }

  private async indexRecord(record: MemoryRecord): Promise<void> {
    const provider = this.#embeddingProvider;
    if (!provider) throw new Error("semantic-not-configured");
    const vector = await provider.embed(embeddingInput(record));
    validateVector(vector, provider.profile.dimensions);
    this.#database
      .prepare(`
        INSERT INTO memory_embeddings (
          record_id, profile_id, dimensions, vector_json, updated_at
        ) VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(record_id, profile_id) DO UPDATE SET
          dimensions = excluded.dimensions,
          vector_json = excluded.vector_json,
          updated_at = excluded.updated_at
      `)
      .run(
        record.id,
        provider.profile.id,
        provider.profile.dimensions,
        JSON.stringify(vector),
        new Date().toISOString(),
      );
  }

  async search(input: {
    query: string;
    mode?: RetrievalMode;
    limit?: number;
    kinds?: MemoryKind[];
  }): Promise<MemorySearchResponse> {
    const requestedMode = input.mode ?? "auto";
    const query = input.query.trim();
    const limit = Math.max(1, Math.min(input.limit ?? 8, 50));
    if (!query) return { requestedMode, executedMode: "metadata", degraded: false, results: [] };

    const autoMode = this.resolveAutoMode(query);
    const desiredMode = requestedMode === "auto" ? autoMode : requestedMode;
    if (desiredMode === "metadata") {
      return { requestedMode, executedMode: "metadata", degraded: false, results: this.metadataSearch(query, limit, input.kinds) };
    }
    if (desiredMode === "lexical") {
      return { requestedMode, executedMode: "lexical", degraded: false, results: this.lexicalSearch(query, limit, input.kinds) };
    }

    if (!this.#embeddingProvider) {
      return {
        requestedMode,
        executedMode: "lexical",
        degraded: true,
        degradationReason: "semantic-not-configured",
        results: this.lexicalSearch(query, limit, input.kinds),
      };
    }
    if (this.#semanticStatus !== "ready") {
      return {
        requestedMode,
        executedMode: "lexical",
        degraded: true,
        degradationReason: this.#semanticStatus === "degraded" ? "semantic-failed" : "semantic-not-ready",
        results: this.lexicalSearch(query, limit, input.kinds),
      };
    }

    try {
      if (desiredMode === "semantic") {
        return { requestedMode, executedMode: "semantic", degraded: false, results: await this.semanticSearch(query, limit, input.kinds) };
      }
      return { requestedMode, executedMode: "hybrid", degraded: false, results: await this.hybridSearch(query, limit, input.kinds) };
    } catch {
      this.#semanticStatus = "degraded";
      return {
        requestedMode,
        executedMode: "lexical",
        degraded: true,
        degradationReason: "semantic-failed",
        results: this.lexicalSearch(query, limit, input.kinds),
      };
    }
  }

  private resolveAutoMode(query: string): ExecutedRetrievalMode {
    const looksExact =
      /^[A-Za-z_$][\w$./:@-]*$/.test(query) ||
      /\b(error|errno|exception|api|config|\.ts|\.js|\.json)\b/i.test(query);
    if (looksExact || !this.#embeddingProvider || this.#semanticStatus !== "ready") return "lexical";
    return "hybrid";
  }

  private kindClause(kinds?: MemoryKind[]): { sql: string; parameters: string[] } {
    if (!kinds?.length) return { sql: "", parameters: [] };
    return { sql: ` AND r.kind IN (${kinds.map(() => "?").join(", ")})`, parameters: kinds };
  }

  private metadataSearch(query: string, limit: number, kinds?: MemoryKind[]): MemorySearchResult[] {
    const kind = this.kindClause(kinds);
    const pattern = `%${query}%`;
    const rows = this.#database
      .prepare(`
        SELECT r.*
        FROM memory_records r
        WHERE r.project_id = ?
          AND (r.title LIKE ? OR r.content LIKE ? OR r.tags_json LIKE ?)
          ${kind.sql}
        ORDER BY r.updated_at DESC
        LIMIT ?
      `)
      .all(this.projectId, pattern, pattern, pattern, ...kind.parameters, limit) as SqlRow[];
    return rows.map((row, index) => ({ record: rowToRecord(row), score: 1 / (index + 1), matchedBy: "metadata" }));
  }

  private lexicalSearch(query: string, limit: number, kinds?: MemoryKind[]): MemorySearchResult[] {
    // trigram 至少需要三个 Unicode 字符，短查询改用 metadata LIKE。
    if ([...query].length < 3) return this.metadataSearch(query, limit, kinds);
    const kind = this.kindClause(kinds);
    const escaped = query.replaceAll('"', '""');
    const rows = this.#database
      .prepare(`
        SELECT r.*, -bm25(memory_fts, 0.0, 5.0, 1.0, 2.0) AS lexical_score
        FROM memory_fts
        JOIN memory_records r ON r.id = memory_fts.record_id
        WHERE memory_fts MATCH ? AND r.project_id = ?
          ${kind.sql}
        ORDER BY bm25(memory_fts, 0.0, 5.0, 1.0, 2.0) ASC
        LIMIT ?
      `)
      .all(`"${escaped}"`, this.projectId, ...kind.parameters, limit) as SqlRow[];

    if (rows.length === 0) return this.metadataSearch(query, limit, kinds);
    return rows.map((row) => ({
      record: rowToRecord(row),
      score: sqlNumber(row.lexical_score, "memory_fts.lexical_score"),
      matchedBy: "fts",
    }));
  }

  private async semanticSearch(query: string, limit: number, kinds?: MemoryKind[]): Promise<MemorySearchResult[]> {
    const provider = this.#embeddingProvider;
    if (!provider) throw new Error("semantic-not-configured");
    const queryVector = await provider.embed(query);
    validateVector(queryVector, provider.profile.dimensions);
    const kind = this.kindClause(kinds);
    const rows = this.#database
      .prepare(`
        SELECT r.*, e.vector_json
        FROM memory_embeddings e
        JOIN memory_records r ON r.id = e.record_id
        WHERE e.profile_id = ? AND r.project_id = ?
          ${kind.sql}
      `)
      .all(provider.profile.id, this.projectId, ...kind.parameters) as SqlRow[];

    return rows
      .map<MemorySearchResult>((row) => ({
        record: rowToRecord(row),
        score: cosineSimilarity(queryVector, JSON.parse(sqlText(row.vector_json, "memory_embeddings.vector_json")) as number[]),
        matchedBy: "vector",
      }))
      .sort((a, b) => b.score - a.score)
      .slice(0, limit);
  }

  private async hybridSearch(query: string, limit: number, kinds?: MemoryKind[]): Promise<MemorySearchResult[]> {
    const lexical = this.lexicalSearch(query, limit * 2, kinds);
    const semantic = await this.semanticSearch(query, limit * 2, kinds);
    const scores = new Map<string, { record: MemoryRecord; score: number }>();
    const addRank = (results: MemorySearchResult[], weight: number): void => {
      results.forEach((result, index) => {
        const previous = scores.get(result.record.id) ?? { record: result.record, score: 0 };
        previous.score += weight / (60 + index + 1);
        scores.set(result.record.id, previous);
      });
    };
    addRank(lexical, 1);
    addRank(semantic, 1);
    return [...scores.values()]
      .sort((a, b) => b.score - a.score)
      .slice(0, limit)
      .map((result) => ({ ...result, matchedBy: "hybrid" }));
  }

  status(): ProjectMemoryStatus {
    const recordCount = sqlNumber(
      (this.#database.prepare("SELECT COUNT(*) AS count FROM memory_records WHERE project_id = ?").get(this.projectId) as SqlRow).count,
      "memory_records.count",
    );
    const ftsIndexedCount = sqlNumber(
      (this.#database.prepare("SELECT COUNT(*) AS count FROM memory_fts WHERE project_id = ?").get(this.projectId) as SqlRow).count,
      "memory_fts.count",
    );
    const vectorIndexedCount = this.#embeddingProvider
      ? sqlNumber(
          (this.#database
            .prepare(`
              SELECT COUNT(*) AS count
              FROM memory_embeddings e
              JOIN memory_records r ON r.id = e.record_id
              WHERE r.project_id = ? AND e.profile_id = ?
            `)
            .get(this.projectId, this.#embeddingProvider.profile.id) as SqlRow).count,
          "memory_embeddings.count",
        )
      : 0;
    return {
      projectId: this.projectId,
      lexicalReady: true,
      recordCount,
      ftsIndexedCount,
      semanticConfigured: Boolean(this.#embeddingProvider),
      semanticStatus: this.#semanticStatus,
      ...(this.#embeddingProvider ? { embeddingProfile: this.#embeddingProvider.profile } : {}),
      vectorIndexedCount,
      vectorPendingCount: this.#embeddingProvider ? Math.max(0, recordCount - vectorIndexedCount) : 0,
      databasePath: this.databasePath,
      databaseBytes: this.databasePath === ":memory:" ? 0 : statSync(this.databasePath).size,
    };
  }
}

/** 为每个项目生成稳定且 Windows 安全的数据库目录。 */
export class ProjectMemoryManager {
  readonly #rootDirectory: string;
  readonly #handles = new Map<string, ProjectMemory>();

  constructor(rootDirectory: string) {
    this.#rootDirectory = rootDirectory;
    mkdirSync(rootDirectory, { recursive: true });
  }

  open(input: { projectId: string; embeddingProvider?: EmbeddingProviderPort }): ProjectMemory {
    const existing = this.#handles.get(input.projectId);
    if (existing) return existing;
    const readable = input.projectId.replace(/[^A-Za-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 32) || "project";
    const digest = createHash("sha256").update(input.projectId).digest("hex").slice(0, 12);
    const directory = join(this.#rootDirectory, `${readable}-${digest}`);
    mkdirSync(directory, { recursive: true });
    const handle = new ProjectMemory({
      projectId: input.projectId,
      databasePath: join(directory, "memory.sqlite"),
      ...(input.embeddingProvider ? { embeddingProvider: input.embeddingProvider } : {}),
    });
    this.#handles.set(input.projectId, handle);
    return handle;
  }

  closeAll(): void {
    for (const handle of this.#handles.values()) handle.close();
    this.#handles.clear();
  }
}
