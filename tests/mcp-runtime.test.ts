import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import test from "node:test";
import { McpManager } from "../packages/mcp-runtime/src/mcp-runtime.ts";
import { PermissionEngine, createWorkspaceWriteProfile } from "../packages/permissions/src/permission-engine.ts";
import { SQLiteToolInvocationJournal } from "../packages/storage-sqlite/src/sqlite-tool-journal.ts";
import { ToolRegistry, ToolRuntime } from "../packages/tool-runtime/src/tool-runtime.ts";

const workspace = resolve(process.cwd());

test("MCP STDIO 完成 initialize、tools/resources 和统一 Tool Runtime 调用", async () => {
  const manager = new McpManager();
  manager.addServer({
    id: "fake",
    name: "Fake MCP",
    transport: "stdio",
    command: process.execPath,
    args: [join(workspace, "tests", "fixtures", "demo-mcp-server.ts")],
    cwd: workspace,
    requestTimeoutMs: 5_000,
  });

  assert.equal(manager.listServers()[0]?.status, "disconnected");
  const connected = await manager.connect("fake");
  assert.equal(connected.status, "ready");
  assert.equal(connected.protocolVersion, "2025-06-18");
  assert.equal(connected.toolCount, 2);
  assert.equal(connected.resourceCount, 1);
  assert.equal(manager.listTools("fake")[0]?.name, "echo.read");
  assert.equal(manager.listResources("fake")[0]?.uri, "memory://guide");
  assert.equal((await manager.readResource("fake", "memory://guide"))[0]?.text, "fake resource");

  const registry = new ToolRegistry();
  const dispose = manager.contributeTools("fake", registry);
  const profile = createWorkspaceWriteProfile(workspace);
  profile.mcp.allowedServerIds = ["fake"];
  profile.mcp.allowedToolPatterns = ["echo.*"];
  const journal = new SQLiteToolInvocationJournal(":memory:");
  const runtime = new ToolRuntime({
    registry,
    permissions: new PermissionEngine(profile, "never-within-sandbox"),
    journal,
  });
  try {
    const response = await runtime.invoke({
      idempotencyKey: "mcp:echo:1",
      envelope: {
        projectId: "project:mcp",
        missionId: "mission:mcp",
        runId: "run:mcp",
        actorId: "agent:mcp",
        integrity: "trusted",
      },
      toolName: "mcp.fake.echo.read",
      args: { text: "你好 MCP" },
    });
    assert.equal(response.invocation.status, "completed");
    assert.equal(response.invocation.effectClass, "read-only-retryable");
    assert.deepEqual(response.invocation.result?.structuredContent, {
      tool: "echo.read",
      args: { text: "你好 MCP" },
    });

    const sideEffect = await runtime.invoke({
      idempotencyKey: "mcp:send:1",
      envelope: {
        projectId: "project:mcp",
        missionId: "mission:mcp",
        runId: "run:mcp",
        actorId: "agent:mcp",
        integrity: "trusted",
      },
      toolName: "mcp.fake.message.send",
      args: { text: "不能直接发送" },
    });
    assert.equal(sideEffect.invocation.status, "waiting-approval");
  } finally {
    dispose();
    journal.close();
    await manager.disconnect("fake");
  }
  assert.equal(registry.list().length, 0);
});
