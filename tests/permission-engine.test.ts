import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import test from "node:test";
import {
  PermissionEngine,
  createWorkspaceWriteProfile,
  type ExecutionEnvelope,
} from "../packages/permissions/src/permission-engine.ts";

const workspace = resolve(process.cwd());
const envelope: ExecutionEnvelope = {
  projectId: "project:test",
  missionId: "mission:test",
  runId: "run:test",
  actorId: "agent:test",
  integrity: "trusted",
};

test("workspace-write 允许项目内写入，但相似前缀目录不能越界", () => {
  const engine = new PermissionEngine(createWorkspaceWriteProfile(workspace), "never-within-sandbox");
  assert.deepEqual(
    engine.evaluate({ kind: "filesystem.write", path: join(workspace, "src", "index.ts") }, envelope),
    { kind: "allow", source: "sandbox" },
  );
  const escaped = engine.evaluate({ kind: "filesystem.write", path: `${workspace}-other/file.ts` }, envelope);
  assert.equal(escaped.kind, "request-approval");
});

test("denied root 是硬拒绝，不能通过普通审批绕过", () => {
  const profile = createWorkspaceWriteProfile(workspace);
  profile.filesystem.deniedRoots = [join(workspace, ".secrets")];
  const engine = new PermissionEngine(profile, "always");
  const decision = engine.evaluate({ kind: "filesystem.read", path: join(workspace, ".secrets", "token") }, envelope);
  assert.equal(decision.kind, "deny");
  if (decision.kind === "deny") assert.equal(decision.hard, true);
  assert.equal(engine.listPendingRequests().length, 0);
});

test("allow-once 只消费一次，第二次相同动作重新请求审批", () => {
  const engine = new PermissionEngine(createWorkspaceWriteProfile(workspace), "always");
  const action = { kind: "filesystem.write" as const, path: join(workspace, "file.ts") };
  const first = engine.evaluate(action, envelope);
  assert.equal(first.kind, "request-approval");
  if (first.kind !== "request-approval") throw new Error("expected approval");
  const grant = engine.respond(first.request.id, "allow", "once");
  assert.equal(grant?.remainingUses, 1);

  const allowed = engine.evaluate(action, envelope);
  assert.equal(allowed.kind, "allow");
  if (allowed.kind === "allow") assert.equal(allowed.source, "grant");
  assert.equal(engine.evaluate(action, envelope).kind, "request-approval");
});

test("run grant 不会泄漏给另一个 Run，project grant 可以复用", () => {
  const action = { kind: "network.connect" as const, host: "api.example.com" };
  const engine = new PermissionEngine(createWorkspaceWriteProfile(workspace), "on-request");
  const request = engine.evaluate(action, envelope);
  if (request.kind !== "request-approval") throw new Error("expected approval");
  engine.respond(request.request.id, "allow", "run");
  assert.equal(engine.evaluate(action, envelope).kind, "allow");
  assert.equal(engine.evaluate(action, { ...envelope, runId: "run:other" }).kind, "request-approval");

  const otherRequest = engine.evaluate(action, { ...envelope, runId: "run:other" });
  if (otherRequest.kind !== "request-approval") throw new Error("expected project approval");
  engine.respond(otherRequest.request.id, "allow", "project");
  assert.equal(engine.evaluate(action, { ...envelope, runId: "run:third" }).kind, "allow");
});

test("MCP server 和 tool pattern 必须同时命中", () => {
  const profile = createWorkspaceWriteProfile(workspace);
  profile.mcp.allowedServerIds = ["github"];
  profile.mcp.allowedToolPatterns = ["issues.read", "pulls.*"];
  const engine = new PermissionEngine(profile, "never-within-sandbox");
  assert.equal(
    engine.evaluate({ kind: "mcp.call", serverId: "github", toolName: "pulls.list", sideEffect: false }, envelope).kind,
    "allow",
  );
  assert.equal(
    engine.evaluate({ kind: "mcp.call", serverId: "github", toolName: "repos.delete", sideEffect: true }, envelope).kind,
    "request-approval",
  );
});
