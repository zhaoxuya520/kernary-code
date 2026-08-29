import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync } from "node:fs";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";
import { HarnessAppServer } from "../packages/app-server/src/harness-app-server.ts";
import { findReadyNodeIds } from "../packages/domain/src/mission.ts";
import { FakeAgentRuntime } from "../packages/fake-runtime/src/fake-agent-runtime.ts";
import { InMemoryKernelStore } from "../packages/kernel/src/in-memory-kernel-store.ts";
import { MissionActor, replayMission } from "../packages/kernel/src/mission-actor.ts";
import { OutboxRunner } from "../packages/kernel/src/outbox-runner.ts";
import { parseDomainEvent, SchemaError } from "../packages/protocol/src/runtime-schema.ts";
import { ProjectMemory } from "../packages/project-memory/src/project-memory.ts";
import { SQLiteKernelStore } from "../packages/storage-sqlite/src/sqlite-kernel-store.ts";
import { projectMission } from "../packages/presentation-model/src/mission-projection.ts";

async function startReady(actor: MissionActor): Promise<void> {
  for (const nodeId of findReadyNodeIds(actor.state)) {
    await actor.dispatch({ type: "StartNode", nodeId, runId: `run:${nodeId}` });
  }
}

test("双 Agent、审批、Verifier、Join 和 replay 保持一致", async () => {
  const store = new InMemoryKernelStore();
  const actor = new MissionActor("mission:test", store);
  const runner = new OutboxRunner(store, new FakeAgentRuntime(actor), 4);

  await actor.dispatch({
    type: "CreateMission",
    missionId: "mission:test",
    projectId: "project:test",
    goal: "验证 D0 垂直切片",
  });
  await actor.dispatch({
    type: "InstallPlan",
    nodes: [
      { id: "a", title: "任务 A", kind: "task", dependsOn: [], agentDefinitionId: "agent:a" },
      { id: "b", title: "任务 B", kind: "task", dependsOn: [], agentDefinitionId: "agent:b", requiresApproval: true },
      { id: "join", title: "汇合", kind: "join", dependsOn: ["a", "b"], agentDefinitionId: "agent:verifier" },
    ],
  });

  assert.deepEqual(findReadyNodeIds(actor.state), ["a", "b"]);
  await startReady(actor);
  await runner.drain();

  assert.equal(actor.state.nodes.a.status, "accepted");
  assert.equal(actor.state.runs["run:a"]?.status, "accepted");
  assert.equal(actor.state.nodes.b.status, "waiting-approval");
  assert.equal(actor.state.runs["run:b"]?.status, "waiting-approval");
  assert.deepEqual(findReadyNodeIds(actor.state), []);

  await actor.dispatch({
    type: "ResolveApproval",
    approvalId: "approval:run:b",
    decision: "allow",
  });
  await runner.drain();
  assert.equal(actor.state.nodes.b.status, "accepted");
  assert.deepEqual(findReadyNodeIds(actor.state), ["join"]);

  await startReady(actor);
  await runner.drain();
  await actor.dispatch({ type: "CompleteMission" });

  const events = store.readEvents(actor.missionId);
  const replayed = replayMission(actor.missionId, events);
  assert.deepEqual(replayed, actor.state);
  assert.equal(replayed.status, "completed");
  assert.ok(events.some((entry) => entry.event.type === "approval.requested"));
  assert.ok(store.listOutbox().every((record) => record.status === "completed"));

  const view = projectMission(actor.missionId, events);
  assert.equal(view.status, "completed");
  assert.equal(view.lanes.length, 3);
  assert.equal(view.pendingApprovalIds.length, 0);
});

test("SQLite 关库重开后可以重放 Mission，Event 与 Outbox 保持一致", async () => {
  const testTempRoot = join(process.cwd(), "output", "test-temp");
  mkdirSync(testTempRoot, { recursive: true });
  const temporaryDirectory = mkdtempSync(join(testTempRoot, "harness-kernel-test-"));
  const databasePath = join(temporaryDirectory, "kernel.sqlite");
  let store: SQLiteKernelStore | undefined;
  let reopened: SQLiteKernelStore | undefined;

  try {
    store = new SQLiteKernelStore(databasePath);
    const actor = new MissionActor("mission:sqlite", store);
    await actor.dispatch({
      type: "CreateMission",
      missionId: actor.missionId,
      projectId: "project:sqlite",
      goal: "验证 SQLite replay",
    });
    await actor.dispatch({
      type: "InstallPlan",
      nodes: [
        {
          id: "task",
          title: "持久化任务",
          kind: "task",
          dependsOn: [],
          agentDefinitionId: "agent:sqlite",
        },
      ],
    });
    await actor.dispatch({ type: "StartNode", nodeId: "task", runId: "run:sqlite" });

    // node.started 与 start-fake-run 必须一起出现，不能只写入其中一个。
    assert.equal(store.readEvents(actor.missionId).at(-1)?.event.type, "node.started");
    assert.equal(store.listOutbox().filter((record) => record.status === "pending").length, 1);

    await new OutboxRunner(store, new FakeAgentRuntime(actor)).drain();
    await actor.dispatch({ type: "CompleteMission" });
    const beforeClose = actor.state;
    store.close();
    store = undefined;

    reopened = new SQLiteKernelStore(databasePath);
    const recoveredActor = new MissionActor(actor.missionId, reopened);
    assert.deepEqual(recoveredActor.state, beforeClose);
    assert.equal(recoveredActor.state.status, "completed");
    assert.equal(recoveredActor.state.runs["run:sqlite"]?.status, "accepted");
    assert.ok(reopened.listOutbox().every((record) => record.status === "completed"));
    assert.deepEqual(reopened.listMissionIds(), [actor.missionId]);

    const recoveredServer = new HarnessAppServer(reopened);
    const recoveredView = recoveredServer.openMission(actor.missionId);
    assert.equal(recoveredView.status, "completed");
    assert.equal(recoveredView.version, beforeClose.version);
  } finally {
    reopened?.close();
    store?.close();
    // 精确临时目录在子测试进程退出后由 scripts/run-tests.ts 清理。
  }
});

test("SQLite CAS 拒绝持有旧版本的第二个 MissionActor", async () => {
  const store = new SQLiteKernelStore(":memory:");
  try {
    const first = new MissionActor("mission:cas", store);
    const stale = new MissionActor("mission:cas", store);
    await first.dispatch({ type: "CreateMission", missionId: first.missionId, projectId: "p", goal: "first" });
    await assert.rejects(
      stale.dispatch({ type: "CreateMission", missionId: stale.missionId, projectId: "p", goal: "stale" }),
      /version-conflict/,
    );
    assert.equal(store.currentVersion(first.missionId), 1);
  } finally {
    store.close();
  }
});

test("运行时 Schema 拒绝损坏或未知的持久化事件", () => {
  assert.throws(
    () => parseDomainEvent({ type: "mission.created", missionId: 42, projectId: "p", goal: "g" }),
    SchemaError,
  );
  assert.throws(() => parseDomainEvent({ type: "mission.teleported" }), /未知事件类型/);
});

test("未配置向量模型时只运行 Metadata + FTS，语义查询类型化降级", async () => {
  const root = join(process.cwd(), "output", "test-temp");
  mkdirSync(root, { recursive: true });
  const directory = mkdtempSync(join(root, "memory-lexical-"));
  const databasePath = join(directory, "memory.sqlite");
  const memory = new ProjectMemory({ projectId: "project:lexical", databasePath });

  await memory.addRecord({
    id: "memory:approval",
    kind: "decision",
    title: "权限审批必须经过 Kernel",
    content: "Agent 不可以直接扩大 sandbox 权限，必须创建 ApprovalRequest。",
    tags: ["权限", "sandbox"],
    status: "verified",
  });

  const lexical = await memory.search({ query: "权限审批", mode: "lexical" });
  assert.equal(lexical.executedMode, "lexical");
  assert.equal(lexical.results[0]?.record.id, "memory:approval");

  const semanticFallback = await memory.search({ query: "怎样控制高风险操作", mode: "semantic" });
  assert.equal(semanticFallback.executedMode, "lexical");
  assert.equal(semanticFallback.degraded, true);
  assert.equal(semanticFallback.degradationReason, "semantic-not-configured");

  const status = memory.status();
  assert.equal(status.lexicalReady, true);
  assert.equal(status.semanticConfigured, false);
  assert.equal(status.semanticStatus, "not-configured");
  assert.equal(status.vectorIndexedCount, 0);
  memory.close();

  // 没有 provider 时连语义表都不创建，证明不是“创建后禁用”的假关闭。
  const inspection = new DatabaseSync(databasePath, { readOnly: true });
  const semanticTable = inspection
    .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'memory_embeddings'")
    .get();
  assert.equal(semanticTable, undefined);
  inspection.close();
});

test("配置向量模型后按需建立 exact-scan 投影，Auto 同时保留 lexical fast path", async () => {
  const root = join(process.cwd(), "output", "test-temp");
  mkdirSync(root, { recursive: true });
  const directory = mkdtempSync(join(root, "memory-semantic-"));
  let embeddingCalls = 0;
  const provider = {
    profile: { id: "fake-embedding@1", model: "fake-embedding", dimensions: 3 },
    async embed(text: string): Promise<number[]> {
      embeddingCalls += 1;
      const normalized = text.toLowerCase();
      return [
        normalized.includes("权限") || normalized.includes("安全") ? 1 : 0.05,
        normalized.includes("缓存") || normalized.includes("context") ? 1 : 0.05,
        normalized.includes("向量") || normalized.includes("semantic") ? 1 : 0.05,
      ];
    },
  };
  const memory = new ProjectMemory({
    projectId: "project:semantic",
    databasePath: join(directory, "memory.sqlite"),
    embeddingProvider: provider,
  });

  // 初始化前写入只落 Metadata/FTS，不偷偷调用 Embedding。
  await memory.addRecord({ id: "m:safety", kind: "decision", title: "权限安全", content: "危险操作需要审批。" });
  await memory.addRecord({ id: "m:cache", kind: "lesson", title: "上下文缓存", content: "稳定前缀提高缓存命中率。" });
  assert.equal(embeddingCalls, 0);

  await memory.initialize();
  assert.equal(embeddingCalls, 2);
  assert.equal(memory.status().semanticStatus, "ready");
  assert.equal(memory.status().vectorIndexedCount, 2);

  const semantic = await memory.search({ query: "如何限制危险的 Agent 操作", mode: "semantic" });
  assert.equal(semantic.executedMode, "semantic");
  assert.equal(semantic.degraded, false);
  assert.equal(semantic.results[0]?.record.id, "m:safety");

  const callsBeforeExactQuery = embeddingCalls;
  const lexical = await memory.search({ query: "TaskContract", mode: "auto" });
  assert.equal(lexical.executedMode, "lexical");
  assert.equal(embeddingCalls, callsBeforeExactQuery);

  await memory.addRecord({ id: "m:vector", kind: "architecture", title: "可选向量", content: "Semantic 只在配置模型后启动。" });
  assert.equal(memory.status().vectorIndexedCount, 3);
  memory.close();
});

test("过期 Run 不能提交结果", async () => {
  const store = new InMemoryKernelStore();
  const actor = new MissionActor("mission:stale", store);
  await actor.dispatch({ type: "CreateMission", missionId: actor.missionId, projectId: "p", goal: "stale" });
  await actor.dispatch({
    type: "InstallPlan",
    nodes: [{ id: "a", title: "A", kind: "task", dependsOn: [], agentDefinitionId: "agent:a" }],
  });
  await actor.dispatch({ type: "StartNode", nodeId: "a", runId: "run:current" });

  await assert.rejects(
    actor.dispatch({ type: "SubmitNode", nodeId: "a", runId: "run:old", outputSummary: "迟到结果" }),
    /迟到或错误 Run/,
  );
});

test("App Server 提供 snapshot、增量订阅和阻塞式审批边界", async () => {
  const server = new HarnessAppServer();
  const created = await server.createMission({
    missionId: "mission:server",
    projectId: "project:server",
    goal: "测试 App Server",
  });

  const receivedSequences: number[] = [];
  const unsubscribe = server.subscribeMission(
    created.missionId,
    created.items.at(-1)?.sequence ?? 0,
    (events) => receivedSequences.push(...events.map((event) => event.sequence)),
  );

  await server.installPlan(created.missionId, [
    {
      id: "write",
      title: "写入文件",
      kind: "task",
      dependsOn: [],
      agentDefinitionId: "agent:writer",
      requiresApproval: true,
    },
  ]);
  const waiting = await server.runUntilBlocked(created.missionId);
  assert.equal(waiting.pendingApprovalIds.length, 1);
  assert.equal(waiting.lanes[0]?.status, "waiting-approval");

  await server.respondApproval(
    created.missionId,
    waiting.pendingApprovalIds[0] ?? "missing",
    "allow",
  );
  const completed = await server.runUntilBlocked(created.missionId);
  unsubscribe();

  assert.equal(completed.status, "completed");
  assert.ok(receivedSequences.length > 0);
  assert.deepEqual(receivedSequences, [...receivedSequences].sort((a, b) => a - b));
});
