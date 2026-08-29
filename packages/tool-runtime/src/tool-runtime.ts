import { randomUUID } from "node:crypto";
import type {
  ExecutionEnvelope,
  GrantScope,
  PermissionAction,
  PermissionEngine,
} from "../../permissions/src/permission-engine.ts";

export type ToolEffectClass =
  | "read-only-retryable"
  | "idempotent-effect"
  | "verifiable-effect"
  | "non-repeatable-effect";

export type ToolInvocationStatus =
  | "requested"
  | "waiting-approval"
  | "running"
  | "completed"
  | "denied"
  | "failed"
  | "uncertain";

export interface ToolDescriptor<Args = unknown, Result = unknown> {
  canonicalName: string;
  version: string;
  description: string;
  effectClass: ToolEffectClass;
  validateArgs(value: unknown): Args;
  validateResult(value: unknown): Result;
  permissionAction(args: Args): PermissionAction;
}

export interface ToolProvider<Args = unknown, Result = unknown> {
  execute(input: { invocationId: string; envelope: ExecutionEnvelope; args: Args }): Promise<Result>;
}

interface RegisteredTool {
  descriptor: ToolDescriptor<unknown, unknown>;
  provider: ToolProvider<unknown, unknown>;
}

export class ToolRegistry {
  readonly #tools = new Map<string, RegisteredTool>();

  register<Args, Result>(descriptor: ToolDescriptor<Args, Result>, provider: ToolProvider<Args, Result>): () => void {
    const key = descriptor.canonicalName;
    if (this.#tools.has(key)) throw new Error(`tool-already-registered: ${key}`);
    this.#tools.set(key, {
      descriptor: descriptor as ToolDescriptor<unknown, unknown>,
      provider: provider as ToolProvider<unknown, unknown>,
    });
    return () => {
      if (this.#tools.get(key)?.descriptor === descriptor) this.#tools.delete(key);
    };
  }

  resolve(canonicalName: string): RegisteredTool {
    const tool = this.#tools.get(canonicalName);
    if (!tool) throw new Error(`tool-not-found: ${canonicalName}`);
    return tool;
  }

  list(): ToolDescriptor[] {
    return [...this.#tools.values()].map((entry) => entry.descriptor).sort((a, b) => a.canonicalName.localeCompare(b.canonicalName));
  }
}

export interface ToolInvocationRecord {
  id: string;
  idempotencyKey: string;
  projectId: string;
  missionId: string;
  runId?: string;
  actorId: string;
  toolName: string;
  toolVersion: string;
  effectClass: ToolEffectClass;
  status: ToolInvocationStatus;
  args: unknown;
  permissionAction: PermissionAction;
  approvalRequestId?: string;
  result?: unknown;
  error?: string;
  createdAt: string;
  updatedAt: string;
}

export interface ToolInvocationJournal {
  create(record: ToolInvocationRecord): void;
  update(id: string, patch: Partial<ToolInvocationRecord>): ToolInvocationRecord;
  get(id: string): ToolInvocationRecord | undefined;
  findByIdempotencyKey(idempotencyKey: string): ToolInvocationRecord | undefined;
  list(): ToolInvocationRecord[];
}

export interface ToolInvokeRequest {
  idempotencyKey: string;
  envelope: ExecutionEnvelope;
  toolName: string;
  args: unknown;
}

export interface ToolInvokeResponse {
  invocation: ToolInvocationRecord;
  needsApproval: boolean;
  retryable: boolean;
}

function retryable(effectClass: ToolEffectClass, status: ToolInvocationStatus): boolean {
  return (
    status === "failed" &&
    (effectClass === "read-only-retryable" || effectClass === "idempotent-effect")
  );
}

/**
 * 统一工具流水线。PermissionEngine 只是策略门；Provider 仍需在真实 SandboxPort 中执行。
 */
export class ToolRuntime {
  readonly registry: ToolRegistry;
  readonly permissions: PermissionEngine;
  readonly journal: ToolInvocationJournal;

  constructor(input: {
    registry: ToolRegistry;
    permissions: PermissionEngine;
    journal: ToolInvocationJournal;
  }) {
    this.registry = input.registry;
    this.permissions = input.permissions;
    this.journal = input.journal;
  }

  async invoke(request: ToolInvokeRequest): Promise<ToolInvokeResponse> {
    const existing = this.journal.findByIdempotencyKey(request.idempotencyKey);
    if (existing) return this.response(existing);

    const registered = this.registry.resolve(request.toolName);
    const args = registered.descriptor.validateArgs(request.args);
    const permissionAction = registered.descriptor.permissionAction(args);
    const now = new Date().toISOString();
    const invocation: ToolInvocationRecord = {
      id: `tool-invocation:${randomUUID()}`,
      idempotencyKey: request.idempotencyKey,
      projectId: request.envelope.projectId,
      missionId: request.envelope.missionId,
      ...(request.envelope.runId ? { runId: request.envelope.runId } : {}),
      actorId: request.envelope.actorId,
      toolName: registered.descriptor.canonicalName,
      toolVersion: registered.descriptor.version,
      effectClass: registered.descriptor.effectClass,
      status: "requested",
      args,
      permissionAction,
      createdAt: now,
      updatedAt: now,
    };
    this.journal.create(invocation);
    return this.authorizeAndMaybeExecute(invocation, registered, request.envelope);
  }

  async resumeAfterApproval(
    invocationId: string,
    scope: GrantScope,
    envelope: ExecutionEnvelope,
  ): Promise<ToolInvokeResponse> {
    const invocation = this.journal.get(invocationId);
    if (!invocation || invocation.status !== "waiting-approval" || !invocation.approvalRequestId) {
      throw new Error(`tool-invocation-not-waiting-approval: ${invocationId}`);
    }
    if (
      invocation.projectId !== envelope.projectId ||
      invocation.missionId !== envelope.missionId ||
      invocation.runId !== envelope.runId
    ) {
      throw new Error("tool-invocation-envelope-mismatch");
    }
    this.permissions.respond(invocation.approvalRequestId, "allow", scope);
    const registered = this.registry.resolve(invocation.toolName);
    return this.authorizeAndMaybeExecute(invocation, registered, envelope);
  }

  denyApproval(invocationId: string): ToolInvokeResponse {
    const invocation = this.journal.get(invocationId);
    if (!invocation || invocation.status !== "waiting-approval" || !invocation.approvalRequestId) {
      throw new Error(`tool-invocation-not-waiting-approval: ${invocationId}`);
    }
    this.permissions.respond(invocation.approvalRequestId, "deny");
    return this.response(
      this.journal.update(invocation.id, {
        status: "denied",
        error: "user-denied-approval",
        updatedAt: new Date().toISOString(),
      }),
    );
  }

  private async authorizeAndMaybeExecute(
    invocation: ToolInvocationRecord,
    registered: RegisteredTool,
    envelope: ExecutionEnvelope,
  ): Promise<ToolInvokeResponse> {
    const decision = this.permissions.evaluate(invocation.permissionAction, envelope);
    if (decision.kind === "deny") {
      return this.response(
        this.journal.update(invocation.id, {
          status: "denied",
          error: decision.reason,
          updatedAt: new Date().toISOString(),
        }),
      );
    }
    if (decision.kind === "request-approval") {
      return this.response(
        this.journal.update(invocation.id, {
          status: "waiting-approval",
          approvalRequestId: decision.request.id,
          updatedAt: new Date().toISOString(),
        }),
      );
    }
    return this.execute(invocation, registered, envelope);
  }

  private async execute(
    invocation: ToolInvocationRecord,
    registered: RegisteredTool,
    envelope: ExecutionEnvelope,
  ): Promise<ToolInvokeResponse> {
    this.journal.update(invocation.id, { status: "running", updatedAt: new Date().toISOString() });
    try {
      const rawResult = await registered.provider.execute({
        invocationId: invocation.id,
        envelope,
        args: invocation.args,
      });
      const result = registered.descriptor.validateResult(rawResult);
      return this.response(
        this.journal.update(invocation.id, {
          status: "completed",
          result,
          error: undefined,
          updatedAt: new Date().toISOString(),
        }),
      );
    } catch (error) {
      const uncertain =
        invocation.effectClass === "verifiable-effect" ||
        invocation.effectClass === "non-repeatable-effect";
      return this.response(
        this.journal.update(invocation.id, {
          status: uncertain ? "uncertain" : "failed",
          error: error instanceof Error ? error.message : String(error),
          updatedAt: new Date().toISOString(),
        }),
      );
    }
  }

  private response(invocation: ToolInvocationRecord): ToolInvokeResponse {
    return {
      invocation,
      needsApproval: invocation.status === "waiting-approval",
      retryable: retryable(invocation.effectClass, invocation.status),
    };
  }
}
