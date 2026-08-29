import { randomUUID } from "node:crypto";
import { isAbsolute, relative, resolve, sep } from "node:path";

export type ApprovalPolicy = "untrusted-only" | "on-request" | "always" | "never-within-sandbox";

export interface PermissionProfile {
  id: string;
  name: string;
  filesystem: {
    readRoots: string[];
    writeRoots: string[];
    deniedRoots: string[];
  };
  subprocess: {
    enabled: boolean;
    allowedExecutables?: string[];
  };
  network: {
    enabled: boolean;
    allowedHosts: string[];
  };
  browser: {
    enabled: boolean;
    allowUploads: boolean;
    allowDownloads: boolean;
  };
  mcp: {
    allowedServerIds: string[];
    allowedToolPatterns: string[];
  };
}

export interface ExecutionEnvelope {
  projectId: string;
  missionId: string;
  runId?: string;
  actorId: string;
  integrity: "trusted" | "untrusted";
}

export type PermissionAction =
  | { kind: "internal.compute"; capability: string }
  | { kind: "filesystem.read"; path: string }
  | { kind: "filesystem.write"; path: string }
  | { kind: "process.spawn"; executable: string }
  | { kind: "network.connect"; host: string }
  | { kind: "browser.open"; origin: string }
  | { kind: "browser.snapshot"; origin: string }
  | { kind: "browser.act"; origin: string; action: "click" | "type" }
  | { kind: "browser.upload"; origin: string; path: string }
  | { kind: "browser.download"; origin: string }
  | { kind: "mcp.call"; serverId: string; toolName: string; sideEffect: boolean };

export type PermissionDecision =
  | { kind: "allow"; source: "sandbox" | "grant"; grantId?: string }
  | { kind: "deny"; reason: string; hard: boolean }
  | { kind: "request-approval"; request: ApprovalRequest };

export interface ApprovalRequest {
  id: string;
  envelope: ExecutionEnvelope;
  action: PermissionAction;
  actionKey: string;
  reason: string;
  risk: "low" | "medium" | "high" | "critical";
  sandboxAllowed: boolean;
  availableScopes: GrantScope[];
  status: "pending" | "allowed" | "denied";
}

export type GrantScope = "once" | "run" | "project";

export interface PermissionGrant {
  id: string;
  requestId: string;
  actionKey: string;
  projectId: string;
  runId?: string;
  scope: GrantScope;
  remainingUses?: number;
  createdAt: string;
  revokedAt?: string;
}

function normalizePath(value: string): string {
  const normalized = resolve(value);
  return process.platform === "win32" ? normalized.toLowerCase() : normalized;
}

/** 不能使用 startsWith 判断目录，否则 C:\project-other 会被误认为在 C:\project 内。 */
function isInside(root: string, target: string): boolean {
  const normalizedRoot = normalizePath(root);
  const normalizedTarget = normalizePath(target);
  const pathFromRoot = relative(normalizedRoot, normalizedTarget);
  return pathFromRoot === "" || (!pathFromRoot.startsWith(`..${sep}`) && pathFromRoot !== ".." && !isAbsolute(pathFromRoot));
}

function matchesPattern(value: string, pattern: string): boolean {
  const escaped = pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replaceAll("*", ".*");
  return new RegExp(`^${escaped}$`, "i").test(value);
}

function actionKey(action: PermissionAction): string {
  switch (action.kind) {
    case "internal.compute":
      return `${action.kind}:${action.capability}`;
    case "filesystem.read":
    case "filesystem.write":
      return `${action.kind}:${normalizePath(action.path)}`;
    case "process.spawn":
      return `${action.kind}:${normalizePath(action.executable)}`;
    case "network.connect":
      return `${action.kind}:${action.host.toLowerCase()}`;
    case "browser.open":
    case "browser.snapshot":
    case "browser.act":
    case "browser.download":
      return action.kind === "browser.act"
        ? `${action.kind}:${action.origin.toLowerCase()}:${action.action}`
        : `${action.kind}:${action.origin.toLowerCase()}`;
    case "browser.upload":
      return `${action.kind}:${action.origin.toLowerCase()}:${normalizePath(action.path)}`;
    case "mcp.call":
      return `${action.kind}:${action.serverId}:${action.toolName}`;
  }
}

function riskOf(action: PermissionAction): ApprovalRequest["risk"] {
  switch (action.kind) {
    case "internal.compute":
      return "low";
    case "filesystem.read":
    case "browser.open":
    case "browser.snapshot":
      return "low";
    case "filesystem.write":
    case "browser.download":
    case "browser.act":
      return "medium";
    case "process.spawn":
    case "network.connect":
    case "mcp.call":
      return action.kind === "mcp.call" && !action.sideEffect ? "low" : "high";
    case "browser.upload":
      return "critical";
  }
}

function hardDenied(profile: PermissionProfile, action: PermissionAction): string | undefined {
  if (action.kind === "filesystem.read" || action.kind === "filesystem.write" || action.kind === "browser.upload") {
    const path = action.path;
    const deniedRoot = profile.filesystem.deniedRoots.find((root) => isInside(root, path));
    if (deniedRoot) return `目标位于 denied root：${deniedRoot}`;
  }
  return undefined;
}

function allowedBySandbox(profile: PermissionProfile, action: PermissionAction): boolean {
  switch (action.kind) {
    case "internal.compute":
      return true;
    case "filesystem.read":
      return profile.filesystem.readRoots.some((root) => isInside(root, action.path));
    case "filesystem.write":
      return profile.filesystem.writeRoots.some((root) => isInside(root, action.path));
    case "process.spawn":
      return (
        profile.subprocess.enabled &&
        (!profile.subprocess.allowedExecutables?.length ||
          profile.subprocess.allowedExecutables.some((item) => normalizePath(item) === normalizePath(action.executable)))
      );
    case "network.connect":
      return profile.network.enabled && profile.network.allowedHosts.some((pattern) => matchesPattern(action.host, pattern));
    case "browser.open":
    case "browser.snapshot":
    case "browser.act":
      return profile.browser.enabled;
    case "browser.upload":
      return profile.browser.enabled && profile.browser.allowUploads;
    case "browser.download":
      return profile.browser.enabled && profile.browser.allowDownloads;
    case "mcp.call":
      return (
        profile.mcp.allowedServerIds.includes(action.serverId) &&
        profile.mcp.allowedToolPatterns.some((pattern) => matchesPattern(action.toolName, pattern))
      );
  }
}

function reasonFor(action: PermissionAction, sandboxAllowed: boolean): string {
  if (!sandboxAllowed) return `${action.kind} 不在当前 sandbox profile 允许范围内`;
  return `${action.kind} 命中当前 approval policy`;
}

/**
 * PermissionEngine 是策略层；它不会假装自己已经提供 OS 级 sandbox。
 * Effect Runner 必须在 allow 后继续调用真正的 SandboxPort/平台隔离执行。
 */
export class PermissionEngine {
  readonly profile: PermissionProfile;
  readonly approvalPolicy: ApprovalPolicy;
  readonly #requests = new Map<string, ApprovalRequest>();
  readonly #grants = new Map<string, PermissionGrant>();

  constructor(profile: PermissionProfile, approvalPolicy: ApprovalPolicy) {
    this.profile = profile;
    this.approvalPolicy = approvalPolicy;
  }

  evaluate(action: PermissionAction, envelope: ExecutionEnvelope): PermissionDecision {
    const hardDenyReason = hardDenied(this.profile, action);
    if (hardDenyReason) return { kind: "deny", reason: hardDenyReason, hard: true };

    const key = actionKey(action);
    const grant = this.findGrant(key, envelope);
    if (grant) {
      if (grant.scope === "once") grant.remainingUses = Math.max(0, (grant.remainingUses ?? 1) - 1);
      return { kind: "allow", source: "grant", grantId: grant.id };
    }

    const sandboxAllowed = allowedBySandbox(this.profile, action);
    const policyRequestsApproval =
      this.approvalPolicy === "always" ||
      (this.approvalPolicy === "untrusted-only" && envelope.integrity === "untrusted") ||
      (this.approvalPolicy === "on-request" && !sandboxAllowed);

    if (sandboxAllowed && !policyRequestsApproval) return { kind: "allow", source: "sandbox" };

    // never-within-sandbox 只取消 sandbox 内审批；越界仍必须请求授权，不能直接放行。
    const request = this.createRequest(action, envelope, sandboxAllowed);
    return { kind: "request-approval", request };
  }

  respond(requestId: string, decision: "allow" | "deny", scope?: GrantScope): PermissionGrant | undefined {
    const request = this.#requests.get(requestId);
    if (!request || request.status !== "pending") throw new Error(`approval-request-not-pending: ${requestId}`);
    if (decision === "deny") {
      request.status = "denied";
      return undefined;
    }

    const selectedScope = scope ?? "once";
    if (!request.availableScopes.includes(selectedScope)) throw new Error(`approval-scope-not-available: ${selectedScope}`);
    request.status = "allowed";
    const grant: PermissionGrant = {
      id: `grant:${randomUUID()}`,
      requestId,
      actionKey: request.actionKey,
      projectId: request.envelope.projectId,
      ...(selectedScope === "run" && request.envelope.runId ? { runId: request.envelope.runId } : {}),
      scope: selectedScope,
      ...(selectedScope === "once" ? { remainingUses: 1 } : {}),
      createdAt: new Date().toISOString(),
    };
    this.#grants.set(grant.id, grant);
    return grant;
  }

  revokeGrant(grantId: string): void {
    const grant = this.#grants.get(grantId);
    if (!grant || grant.revokedAt) return;
    grant.revokedAt = new Date().toISOString();
  }

  listPendingRequests(): ApprovalRequest[] {
    return [...this.#requests.values()].filter((request) => request.status === "pending");
  }

  listActiveGrants(): PermissionGrant[] {
    return [...this.#grants.values()].filter(
      (grant) => !grant.revokedAt && (grant.scope !== "once" || (grant.remainingUses ?? 0) > 0),
    );
  }

  private findGrant(key: string, envelope: ExecutionEnvelope): PermissionGrant | undefined {
    return this.listActiveGrants().find(
      (grant) =>
        grant.actionKey === key &&
        grant.projectId === envelope.projectId &&
        (grant.scope === "project" || grant.scope === "once" || grant.runId === envelope.runId),
    );
  }

  private createRequest(
    action: PermissionAction,
    envelope: ExecutionEnvelope,
    sandboxAllowed: boolean,
  ): ApprovalRequest {
    const request: ApprovalRequest = {
      id: `approval:${randomUUID()}`,
      envelope,
      action,
      actionKey: actionKey(action),
      reason: reasonFor(action, sandboxAllowed),
      risk: riskOf(action),
      sandboxAllowed,
      availableScopes: envelope.runId ? ["once", "run", "project"] : ["once", "project"],
      status: "pending",
    };
    this.#requests.set(request.id, request);
    return request;
  }
}

export function createWorkspaceWriteProfile(projectRoot: string): PermissionProfile {
  return {
    id: "workspace-write",
    name: "Workspace write",
    filesystem: {
      readRoots: [projectRoot],
      writeRoots: [projectRoot],
      deniedRoots: [],
    },
    subprocess: { enabled: true },
    network: { enabled: false, allowedHosts: [] },
    browser: { enabled: true, allowUploads: false, allowDownloads: true },
    mcp: { allowedServerIds: [], allowedToolPatterns: [] },
  };
}
