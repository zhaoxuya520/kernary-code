import assert from "node:assert/strict";
import { resolve } from "node:path";
import test from "node:test";
import { PermissionEngine, createWorkspaceWriteProfile } from "../packages/permissions/src/permission-engine.ts";
import { ServiceRegistry, PluginHost, composePluginSettings } from "../packages/plugin-runtime/src/plugin-runtime.ts";
import { SQLiteToolInvocationJournal } from "../packages/storage-sqlite/src/sqlite-tool-journal.ts";
import { ToolRegistry, ToolRuntime } from "../packages/tool-runtime/src/tool-runtime.ts";

const manifest = {
  id: "demo.plugin",
  name: "Demo Plugin",
  version: "1.0.0",
  description: "测试插件",
  engineRange: ">=0.0.1",
  permissions: ["internal.compute"],
  contributions: { tools: ["plugin.demo.uppercase"], contextProviders: ["demo-context"] },
};

test("插件启用时注册 Service/Tool，禁用时按 disposer 撤销", async () => {
  const services = new ServiceRegistry();
  const contextService = services.define({
    id: "context-provider",
    version: "1",
    cardinality: "many" as const,
    validate(value: unknown) {
      if (typeof value !== "string") throw new Error("invalid-context-provider");
      return value;
    },
  });
  const tools = new ToolRegistry();
  const host = new PluginHost({ services, tools });
  host.install(manifest, {
    activate(context) {
      context.provideService(contextService, "demo context");
      context.registerTool(
        {
          canonicalName: "plugin.demo.uppercase",
          version: "1",
          description: "uppercase",
          effectClass: "read-only-retryable",
          validateArgs(value) {
            if (typeof value !== "object" || value === null || typeof value.text !== "string") throw new Error("invalid-args");
            return value as { text: string };
          },
          validateResult(value) {
            if (typeof value !== "string") throw new Error("invalid-result");
            return value;
          },
          permissionAction() {
            return { kind: "internal.compute", capability: "text.uppercase" };
          },
        },
        { async execute({ args }) { return args.text.toUpperCase(); } },
      );
    },
  });

  const active = await host.enable("demo.plugin", "project:test");
  assert.equal(active.status, "active");
  assert.equal(active.activeContributionCount, 2);
  assert.deepEqual(services.consumeMany(contextService, "project:test"), ["demo context"]);
  assert.equal(tools.list()[0]?.canonicalName, "plugin.demo.uppercase");

  // Plugin Tool 仍然经过统一 Permission/Journal，不允许从 PluginHost 旁路执行。
  const journal = new SQLiteToolInvocationJournal(":memory:");
  try {
    const runtime = new ToolRuntime({
      registry: tools,
      permissions: new PermissionEngine(createWorkspaceWriteProfile(resolve(process.cwd())), "never-within-sandbox"),
      journal,
    });
    const result = await runtime.invoke({
      idempotencyKey: "plugin:uppercase:1",
      envelope: {
        projectId: "project:test",
        missionId: "mission:test",
        runId: "run:test",
        actorId: "agent:test",
        integrity: "trusted",
      },
      toolName: "plugin.demo.uppercase",
      args: { text: "harness" },
    });
    assert.equal(result.invocation.status, "completed");
    assert.equal(result.invocation.result, "HARNESS");
  } finally {
    journal.close();
  }

  const disabled = await host.disable("demo.plugin");
  assert.equal(disabled.status, "disabled");
  assert.equal(services.countProviders("demo.plugin"), 0);
  assert.equal(tools.list().length, 0);
});

test("插件激活中途失败会逆序回滚已有贡献", async () => {
  const services = new ServiceRegistry();
  const service = services.define({ id: "many", version: "1", cardinality: "many" as const, validate: (value: unknown) => value });
  const tools = new ToolRegistry();
  const host = new PluginHost({ services, tools });
  const disposed: string[] = [];
  host.install({ ...manifest, id: "broken.plugin", name: "Broken" }, {
    activate(context) {
      context.provideService(service, "value");
      context.onDispose(() => disposed.push("first"));
      context.onDispose(() => disposed.push("second"));
      throw new Error("activation-boom");
    },
  });
  const view = await host.enable("broken.plugin", "project:test");
  assert.equal(view.status, "failed");
  assert.match(view.lastError ?? "", /activation-boom/);
  assert.deepEqual(disposed, ["second", "first"]);
  assert.equal(services.countProviders("broken.plugin"), 0);
});

test("插件不能提供 core-owned service，single provider 不能冲突", () => {
  const services = new ServiceRegistry();
  const core = services.define({ id: "kernel-store", version: "1", cardinality: "one" as const, coreOwned: true, validate: (value: unknown) => value });
  assert.throws(
    () => services.provide({ pluginId: "evil", scope: "project", definition: core, value: {} }),
    /core-service-cannot-be-provided/,
  );

  const single = services.define({ id: "single", version: "1", cardinality: "one" as const, validate: (value: unknown) => value });
  services.provide({ pluginId: "a", scope: "project", definition: single, value: "a" });
  assert.throws(
    () => services.provide({ pluginId: "b", scope: "project", definition: single, value: "b" }),
    /single-service-provider-conflict/,
  );
});

test("插件 Profile/Bundle/Override 递归合并，数组由后层替换", () => {
  assert.deepEqual(
    composePluginSettings(
      { enabled: true, nested: { a: 1, list: [1, 2] } },
      { nested: { b: 2 } },
      { nested: { list: [3] } },
    ),
    { enabled: true, nested: { a: 1, b: 2, list: [3] } },
  );
});
