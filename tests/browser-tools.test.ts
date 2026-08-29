import assert from "node:assert/strict";
import { resolve } from "node:path";
import test from "node:test";
import { contributeBrowserTools } from "../packages/browser-runtime/src/cdp-browser.ts";
import { PermissionEngine, createWorkspaceWriteProfile } from "../packages/permissions/src/permission-engine.ts";
import { SQLiteToolInvocationJournal } from "../packages/storage-sqlite/src/sqlite-tool-journal.ts";
import { ToolRegistry, ToolRuntime } from "../packages/tool-runtime/src/tool-runtime.ts";

test("Agent Browser Catalog 只暴露五个结构化工具并经过 Permission/Journal", async () => {
  const calls: string[] = [];
  const controller = {
    async navigate(url: string) { calls.push(`navigate:${url}`); },
    async snapshot() { calls.push("snapshot"); return { url: "http://127.0.0.1:4173/", title: "Harness", nodes: [] }; },
    async click(ref: string) { calls.push(`click:${ref}`); },
    async type(ref: string, text: string) { calls.push(`type:${ref}:${text}`); },
    async screenshot() { calls.push("screenshot"); return { id: "shot", path: "shot.png", mimeType: "image/png" as const, bytes: 10 }; },
  };
  const registry = new ToolRegistry();
  const dispose = contributeBrowserTools(registry, controller, () => "http://127.0.0.1:4173");
  assert.deepEqual(
    registry.list().map((tool) => tool.canonicalName),
    ["browser.click", "browser.navigate", "browser.screenshot", "browser.snapshot", "browser.type"],
  );
  assert.ok(registry.list().every((tool) => !tool.canonicalName.includes("cdp")));

  const journal = new SQLiteToolInvocationJournal(":memory:");
  const runtime = new ToolRuntime({
    registry,
    permissions: new PermissionEngine(createWorkspaceWriteProfile(resolve(process.cwd())), "never-within-sandbox"),
    journal,
  });
  try {
    const result = await runtime.invoke({
      idempotencyKey: "browser:snapshot:1",
      envelope: {
        projectId: "project:browser",
        missionId: "mission:browser",
        runId: "run:browser",
        actorId: "agent:browser",
        integrity: "trusted",
      },
      toolName: "browser.snapshot",
      args: {},
    });
    assert.equal(result.invocation.status, "completed");
    assert.deepEqual(calls, ["snapshot"]);
    assert.equal(journal.list()[0]?.toolName, "browser.snapshot");
  } finally {
    dispose();
    journal.close();
  }
  assert.equal(registry.list().length, 0);
});
