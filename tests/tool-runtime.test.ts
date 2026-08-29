import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import test from "node:test";
import { PermissionEngine, createWorkspaceWriteProfile } from "../packages/permissions/src/permission-engine.ts";
import { SQLiteToolInvocationJournal } from "../packages/storage-sqlite/src/sqlite-tool-journal.ts";
import { ToolRegistry, ToolRuntime, type ToolEffectClass } from "../packages/tool-runtime/src/tool-runtime.ts";

const workspace = resolve(process.cwd());
const envelope = {
  projectId: "project:tools",
  missionId: "mission:tools",
  runId: "run:tools",
  actorId: "agent:tools",
  integrity: "trusted" as const,
};

function createRuntime(input: {
  approvalPolicy?: "always" | "never-within-sandbox";
  effectClass?: ToolEffectClass;
  execute?: () => Promise<unknown>;
}) {
  const registry = new ToolRegistry();
  let executions = 0;
  registry.register(
    {
      canonicalName: "files.write",
      version: "1",
      description: "测试写入工具",
      effectClass: input.effectClass ?? "idempotent-effect",
      validateArgs(value) {
        if (typeof value !== "object" || value === null || typeof value.path !== "string") {
          throw new Error("invalid-files-write-args");
        }
        return value as { path: string; content: string };
      },
      validateResult(value) {
        if (typeof value !== "object" || value === null || value.ok !== true) throw new Error("invalid-result");
        return value as { ok: true };
      },
      permissionAction(args) {
        return { kind: "filesystem.write", path: args.path };
      },
    },
    {
      async execute() {
        executions += 1;
        return input.execute ? input.execute() : { ok: true };
      },
    },
  );
  const journal = new SQLiteToolInvocationJournal(":memory:");
  const permissions = new PermissionEngine(
    createWorkspaceWriteProfile(workspace),
    input.approvalPolicy ?? "never-within-sandbox",
  );
  return {
    runtime: new ToolRuntime({ registry, permissions, journal }),
    journal,
    executions: () => executions,
  };
}

test("统一 Tool Runtime 在 sandbox 内执行并持久化 completed journal", async () => {
  const setup = createRuntime({});
  try {
    const response = await setup.runtime.invoke({
      idempotencyKey: "tool:safe:1",
      envelope,
      toolName: "files.write",
      args: { path: join(workspace, "safe.ts"), content: "ok" },
    });
    assert.equal(response.invocation.status, "completed");
    assert.equal(response.needsApproval, false);
    assert.equal(setup.executions(), 1);
    assert.equal(setup.journal.list()[0]?.status, "completed");
  } finally {
    setup.journal.close();
  }
});

test("等待审批的 ToolInvocation 可用 allow-once 精确恢复且不重复执行", async () => {
  const setup = createRuntime({ approvalPolicy: "always" });
  try {
    const request = {
      idempotencyKey: "tool:approval:1",
      envelope,
      toolName: "files.write",
      args: { path: join(workspace, "approved.ts"), content: "ok" },
    };
    const waiting = await setup.runtime.invoke(request);
    assert.equal(waiting.invocation.status, "waiting-approval");
    assert.equal(setup.executions(), 0);

    const completed = await setup.runtime.resumeAfterApproval(waiting.invocation.id, "once", envelope);
    assert.equal(completed.invocation.status, "completed");
    assert.equal(setup.executions(), 1);

    const duplicate = await setup.runtime.invoke(request);
    assert.equal(duplicate.invocation.id, completed.invocation.id);
    assert.equal(setup.executions(), 1);
  } finally {
    setup.journal.close();
  }
});

test("硬拒绝不会触发 Provider", async () => {
  const setup = createRuntime({ approvalPolicy: "always" });
  setup.runtime.permissions.profile.filesystem.deniedRoots = [join(workspace, ".secret")];
  try {
    const response = await setup.runtime.invoke({
      idempotencyKey: "tool:deny:1",
      envelope,
      toolName: "files.write",
      args: { path: join(workspace, ".secret", "token"), content: "x" },
    });
    assert.equal(response.invocation.status, "denied");
    assert.equal(setup.executions(), 0);
  } finally {
    setup.journal.close();
  }
});

test("幂等工具失败可重试，非重复副作用失败进入 uncertain", async () => {
  const idempotent = createRuntime({
    effectClass: "idempotent-effect",
    async execute() {
      throw new Error("disk-busy");
    },
  });
  const nonRepeatable = createRuntime({
    effectClass: "non-repeatable-effect",
    async execute() {
      throw new Error("connection-lost-after-send");
    },
  });
  try {
    const failed = await idempotent.runtime.invoke({
      idempotencyKey: "tool:retryable",
      envelope,
      toolName: "files.write",
      args: { path: join(workspace, "retry.ts"), content: "x" },
    });
    const uncertain = await nonRepeatable.runtime.invoke({
      idempotencyKey: "tool:uncertain",
      envelope,
      toolName: "files.write",
      args: { path: join(workspace, "send.ts"), content: "x" },
    });
    assert.equal(failed.invocation.status, "failed");
    assert.equal(failed.retryable, true);
    assert.equal(uncertain.invocation.status, "uncertain");
    assert.equal(uncertain.retryable, false);
  } finally {
    idempotent.journal.close();
    nonRepeatable.journal.close();
  }
});
