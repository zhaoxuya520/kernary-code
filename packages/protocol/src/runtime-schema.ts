import type { DomainEvent, EffectIntent, WorkflowNodeDefinition } from "../../domain/src/model.ts";

/**
 * TypeScript 类型在运行时会被擦除，所以数据库、模型、MCP 和 RPC 边界必须重新验证。
 * D0 先用零依赖解析器；以后可以在这个边界替换成 TypeBox/Zod，而不改变领域层。
 */
export class SchemaError extends Error {
  readonly path: string;

  constructor(path: string, message: string) {
    super(`${path}: ${message}`);
    this.path = path;
  }
}

type JsonObject = Record<string, unknown>;

function object(value: unknown, path: string): JsonObject {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new SchemaError(path, "应为对象");
  }
  return value as JsonObject;
}

function string(value: unknown, path: string): string {
  if (typeof value !== "string") throw new SchemaError(path, "应为字符串");
  return value;
}

function stringArray(value: unknown, path: string): string[] {
  if (!Array.isArray(value)) throw new SchemaError(path, "应为字符串数组");
  return value.map((item, index) => string(item, `${path}[${index}]`));
}

function optionalBoolean(value: unknown, path: string): boolean | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "boolean") throw new SchemaError(path, "应为布尔值");
  return value;
}

function literal<T extends string>(value: unknown, values: readonly T[], path: string): T {
  if (typeof value !== "string" || !values.includes(value as T)) {
    throw new SchemaError(path, `应为 ${values.join(" | ")}`);
  }
  return value as T;
}

function parseNode(value: unknown, path: string): WorkflowNodeDefinition {
  const input = object(value, path);
  const requiresApproval = optionalBoolean(input.requiresApproval, `${path}.requiresApproval`);
  return {
    id: string(input.id, `${path}.id`),
    title: string(input.title, `${path}.title`),
    kind: literal(input.kind, ["task", "join"] as const, `${path}.kind`),
    dependsOn: stringArray(input.dependsOn, `${path}.dependsOn`),
    agentDefinitionId: string(input.agentDefinitionId, `${path}.agentDefinitionId`),
    // JSON 会省略 undefined；重放时也必须省略该属性，才能保持结构确定性。
    ...(requiresApproval === undefined ? {} : { requiresApproval }),
  };
}

export function parseDomainEvent(value: unknown): DomainEvent {
  const input = object(value, "event");
  const type = string(input.type, "event.type");

  switch (type) {
    case "mission.created":
      return {
        type,
        missionId: string(input.missionId, "event.missionId"),
        projectId: string(input.projectId, "event.projectId"),
        goal: string(input.goal, "event.goal"),
      };
    case "mission.plan-installed":
      if (!Array.isArray(input.nodes)) throw new SchemaError("event.nodes", "应为数组");
      return { type, nodes: input.nodes.map((node, index) => parseNode(node, `event.nodes[${index}]`)) };
    case "node.started":
    case "node.accepted":
      return {
        type,
        nodeId: string(input.nodeId, "event.nodeId"),
        runId: string(input.runId, "event.runId"),
      };
    case "node.submitted":
      return {
        type,
        nodeId: string(input.nodeId, "event.nodeId"),
        runId: string(input.runId, "event.runId"),
        outputSummary: string(input.outputSummary, "event.outputSummary"),
      };
    case "approval.requested":
      return {
        type,
        approvalId: string(input.approvalId, "event.approvalId"),
        nodeId: string(input.nodeId, "event.nodeId"),
        runId: string(input.runId, "event.runId"),
        action: string(input.action, "event.action"),
        reason: string(input.reason, "event.reason"),
      };
    case "approval.resolved":
      return {
        type,
        approvalId: string(input.approvalId, "event.approvalId"),
        decision: literal(input.decision, ["allow", "deny"] as const, "event.decision"),
      };
    case "mission.completed":
      return { type };
    default:
      throw new SchemaError("event.type", `未知事件类型：${type}`);
  }
}

export function parseEffectIntent(value: unknown): EffectIntent {
  const input = object(value, "effect");
  const kind = literal(
    input.kind,
    ["start-fake-run", "resume-fake-run", "verify-fake-run"] as const,
    "effect.kind",
  );
  return {
    kind,
    missionId: string(input.missionId, "effect.missionId"),
    nodeId: string(input.nodeId, "effect.nodeId"),
    runId: string(input.runId, "effect.runId"),
  };
}

export function parseJson(text: string, path: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    throw new SchemaError(path, "不是合法 JSON");
  }
}
