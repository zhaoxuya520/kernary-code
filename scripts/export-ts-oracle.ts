import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { CompactionEngine, ContextCompiler } from "../packages/context-engine/src/context-engine.ts";
import type { ContextFragment, SessionItem } from "../packages/context-engine/src/context-engine.ts";
import { findReadyNodeIds } from "../packages/domain/src/mission.ts";
import { decideMission, reduceMission } from "../packages/domain/src/mission.ts";
import type { EffectIntent, MissionCommand, MissionState } from "../packages/domain/src/model.ts";
import { createEmptyMissionState } from "../packages/domain/src/model.ts";
import { FakeAgentRuntime } from "../packages/fake-runtime/src/fake-agent-runtime.ts";
import { InMemoryKernelStore } from "../packages/kernel/src/in-memory-kernel-store.ts";
import { MissionActor } from "../packages/kernel/src/mission-actor.ts";
import { OutboxRunner } from "../packages/kernel/src/outbox-runner.ts";
import { PermissionEngine, createWorkspaceWriteProfile } from "../packages/permissions/src/permission-engine.ts";
import { ProjectMemory } from "../packages/project-memory/src/project-memory.ts";
import { projectMission } from "../packages/presentation-model/src/mission-projection.ts";

const workspaceRoot = resolve(process.cwd());
const fixtureDirectory = join(workspaceRoot, "fixtures", "ts-oracle");
const checkOnly = process.argv.includes("--check");

function stableJson(value: unknown): string {
  return `${JSON.stringify(value, null, 2)}\n`;
}

function sha256(content: string): string {
  return createHash("sha256").update(content).digest("hex");
}

async function buildMissionFixture(): Promise<unknown> {
  const store = new InMemoryKernelStore();
  const actor = new MissionActor("mission:oracle", store);
  const runner = new OutboxRunner(store, new FakeAgentRuntime(actor), 4);

  await actor.dispatch({
    type: "CreateMission",
    missionId: actor.missionId,
    projectId: "project:oracle",
    goal: "验证并行任务、审批、验收与 Join",
  });
  await actor.dispatch({
    type: "InstallPlan",
    nodes: [
      { id: "a", title: "任务 A", kind: "task", dependsOn: [], agentDefinitionId: "agent:a" },
      {
        id: "b",
        title: "任务 B",
        kind: "task",
        dependsOn: [],
        agentDefinitionId: "agent:b",
        requiresApproval: true,
      },
      {
        id: "join",
        title: "汇合验证",
        kind: "join",
        dependsOn: ["a", "b"],
        agentDefinitionId: "agent:verifier",
      },
    ],
  });

  const startReady = async (): Promise<void> => {
    for (const nodeId of findReadyNodeIds(actor.state)) {
      await actor.dispatch({ type: "StartNode", nodeId, runId: `run:${nodeId}` });
    }
  };

  await startReady();
  await runner.drain();
  await actor.dispatch({ type: "ResolveApproval", approvalId: "approval:run:b", decision: "allow" });
  await runner.drain();
  await startReady();
  await runner.drain();
  await actor.dispatch({ type: "CompleteMission" });

  const events = store.readEvents(actor.missionId);
  const view = projectMission(actor.missionId, events);
  return {
    schemaVersion: 1,
    fixture: "mission-parallel-approval-join",
    events: events.map((stored) => ({
      aggregateVersion: stored.aggregateVersion,
      event: stored.event,
    })),
    finalState: actor.state,
    projection: view,
    outbox: {
      count: store.listOutbox().length,
      statuses: store.listOutbox().map((record) => record.status),
      effectKinds: store.listOutbox().map((record) => record.effect.kind),
    },
    invariants: {
      allNodesAccepted: Object.values(actor.state.nodes).every((node) => node.status === "accepted"),
      missionCompleted: actor.state.status === "completed",
      pendingApprovals: view.pendingApprovalIds.length,
    },
  };
}

function normalizeEffect(effect: EffectIntent): unknown {
  const kind = {
    "start-fake-run": "agent-run.start",
    "resume-fake-run": "agent-run.resume",
    "verify-fake-run": "agent-run.verify",
  }[effect.kind];
  return {
    kind,
    missionId: effect.missionId,
    nodeId: effect.nodeId,
    runId: effect.runId,
  };
}

function buildCommandDecisionFixture(): unknown {
  const nodes = [
    { id: "a", title: "任务 A", kind: "task" as const, dependsOn: [], agentDefinitionId: "agent:a" },
    {
      id: "b",
      title: "任务 B",
      kind: "task" as const,
      dependsOn: [],
      agentDefinitionId: "agent:b",
      requiresApproval: true,
    },
    {
      id: "join",
      title: "汇合验证",
      kind: "join" as const,
      dependsOn: ["a", "b"],
      agentDefinitionId: "agent:verifier",
    },
  ];
  const commands: MissionCommand[] = [
    {
      type: "CreateMission",
      missionId: "mission:command-oracle",
      projectId: "project:command-oracle",
      goal: "验证 Command/Decision/Effect 语义",
    },
    { type: "InstallPlan", nodes },
    { type: "StartNode", nodeId: "a", runId: "run:a" },
    { type: "StartNode", nodeId: "b", runId: "run:b" },
    { type: "SubmitNode", nodeId: "a", runId: "run:a", outputSummary: "任务 A 完成" },
    {
      type: "RequestApproval",
      nodeId: "b",
      runId: "run:b",
      approvalId: "approval:run:b",
      action: "filesystem.write",
      reason: "任务 B 需要写入",
    },
    { type: "AcceptNode", nodeId: "a", runId: "run:a" },
    { type: "ResolveApproval", approvalId: "approval:run:b", decision: "allow" },
    { type: "SubmitNode", nodeId: "b", runId: "run:b", outputSummary: "任务 B 完成" },
    { type: "AcceptNode", nodeId: "b", runId: "run:b" },
    { type: "StartNode", nodeId: "join", runId: "run:join" },
    { type: "SubmitNode", nodeId: "join", runId: "run:join", outputSummary: "Join 完成" },
    { type: "AcceptNode", nodeId: "join", runId: "run:join" },
    { type: "CompleteMission" },
  ];
  let state: MissionState = createEmptyMissionState("mission:command-oracle");
  const steps = commands.map((command) => {
    const stateVersionBefore = state.version;
    const decision = decideMission(state, command);
    for (const event of decision.events) state = reduceMission(state, event);
    return {
      command,
      stateVersionBefore,
      events: decision.events,
      effects: decision.effects.map(normalizeEffect),
      stateVersionAfter: state.version,
    };
  });
  return {
    schemaVersion: 1,
    fixture: "mission-command-decisions",
    effectVocabulary: "production-normalized-v1",
    steps,
    finalState: state,
  };
}

async function buildContextFixture(): Promise<unknown> {
  const compiler = new ContextCompiler();
  const fragments: ContextFragment[] = [
    {
      id: "core",
      kind: "core",
      role: "system",
      content: "稳定核心规则",
      priority: 100,
      order: 0,
      cacheClass: "static",
      hardRequired: true,
      tokenEstimate: 20,
      sourceRefs: ["prompt:core@1"],
    },
    {
      id: "goal",
      kind: "goal",
      role: "developer",
      content: "完成认证模块",
      priority: 100,
      order: 0,
      cacheClass: "append-only",
      hardRequired: true,
      tokenEstimate: 15,
      sourceRefs: ["goal:1"],
    },
    {
      id: "history",
      kind: "history",
      role: "user",
      content: "最近对话",
      priority: 70,
      order: 1,
      cacheClass: "append-only",
      tokenEstimate: 20,
      sourceRefs: ["turn:1"],
    },
    {
      id: "low-memory",
      kind: "memory",
      role: "developer",
      content: "低优先级旧记忆",
      priority: 10,
      order: 0,
      cacheClass: "dynamic-tail",
      tokenEstimate: 30,
      sourceRefs: ["memory:old"],
    },
    {
      id: "runtime",
      kind: "runtime",
      role: "developer",
      content: "budget=10",
      priority: 90,
      order: 1,
      cacheClass: "dynamic-tail",
      tokenEstimate: 10,
      sourceRefs: ["runtime:1"],
    },
  ];
  const budget = {
    modelContextWindow: 100,
    reservedOutputTokens: 20,
    reservedToolTokens: 10,
    reservedRecoveryTokens: 10,
  };
  const first = compiler.compile({ fragments, budget, contextSeriesId: "series:1" });
  const second = compiler.compile({
    fragments: fragments.map((fragment) =>
      fragment.id === "runtime" ? { ...fragment, content: "budget=9" } : fragment,
    ),
    budget,
    contextSeriesId: "series:1",
  });

  const sessionItems: SessionItem[] = [
    { id: "old-user", sequence: 1, kind: "user-message", content: "旧用户消息", tokenEstimate: 30, sourceRefs: [] },
    { id: "old-agent", sequence: 2, kind: "agent-message", content: "旧 Agent 消息", tokenEstimate: 30, sourceRefs: [] },
    { id: "tool-call", sequence: 3, kind: "tool-call", content: "read", pairId: "tool:1", tokenEstimate: 10, sourceRefs: [] },
    { id: "tool-result", sequence: 4, kind: "tool-result", content: "data", pairId: "tool:1", tokenEstimate: 20, sourceRefs: [] },
    { id: "approval", sequence: 5, kind: "approval", content: "allow once", tokenEstimate: 10, sourceRefs: [] },
    { id: "contract", sequence: 6, kind: "contract", content: "workspace only", tokenEstimate: 10, sourceRefs: [] },
    { id: "recent-user", sequence: 7, kind: "user-message", content: "继续", tokenEstimate: 5, sourceRefs: [] },
    { id: "recent-agent", sequence: 8, kind: "agent-message", content: "继续中", tokenEstimate: 5, sourceRefs: [] },
  ];
  const compaction = await new CompactionEngine({
    async summarize({ items }) {
      return {
        summary: `历史摘要：${items.map((item) => item.id).join(",")}`,
        activeAssumptions: [],
        unresolvedBlockers: [],
        completedActions: ["旧分析完成"],
        nextGoal: "继续当前任务",
      };
    },
  }).compact({
    items: sessionItems,
    recentItemCount: 2,
    maxSummaryTokens: 20,
    contextSeriesId: "series:before",
  });

  return {
    schemaVersion: 1,
    fixture: "context-budget-cache-compaction",
    compile: {
      selectedIds: first.fragments.map((fragment) => fragment.id),
      excludedIds: first.excluded.map((entry) => entry.fragment.id),
      inputTokenEstimate: first.inputTokenEstimate,
      maxInputTokens: first.maxInputTokens,
      firstCache: first.cache,
      secondCache: second.cache,
      stablePrefixUnchanged: first.cache.stablePrefixHash === second.cache.stablePrefixHash,
      fullFingerprintChanged: first.cache.fullFingerprint !== second.cache.fullFingerprint,
    },
    compaction: {
      retainedItemIds: compaction.record.retainedItemIds,
      summarizedItemIds: compaction.record.summarizedItemIds,
      tokenUsageBefore: compaction.record.tokenUsageBefore,
      tokenEstimateAfter: compaction.record.tokenEstimateAfter,
      previousSeriesId: compaction.record.previousSeriesId,
      nextSeriesId: compaction.record.nextSeriesId,
      summary: compaction.record.summaryItem.content,
      visibleKinds: compaction.visibleItems.map((item) => item.kind),
    },
  };
}

async function buildMemoryFixture(): Promise<unknown> {
  const lexicalMemory = new ProjectMemory({ projectId: "project:lexical-oracle", databasePath: ":memory:" });
  await lexicalMemory.addRecord({
    id: "memory:approval",
    kind: "decision",
    title: "权限审批必须经过 Kernel",
    content: "Agent 不可以直接扩大 sandbox 权限。",
    tags: ["权限", "sandbox"],
    status: "verified",
  });
  const lexical = await lexicalMemory.search({ query: "权限审批", mode: "lexical" });
  const fallback = await lexicalMemory.search({ query: "如何控制危险操作", mode: "semantic" });
  const lexicalStatus = lexicalMemory.status();
  lexicalMemory.close();

  let embeddingCalls = 0;
  const semanticMemory = new ProjectMemory({
    projectId: "project:semantic-oracle",
    databasePath: ":memory:",
    embeddingProvider: {
      profile: { id: "fake@1", model: "fake", dimensions: 3 },
      async embed(text: string) {
        embeddingCalls += 1;
        return [
          text.includes("权限") || text.includes("危险") ? 1 : 0.05,
          text.includes("缓存") ? 1 : 0.05,
          text.includes("向量") ? 1 : 0.05,
        ];
      },
    },
  });
  await semanticMemory.addRecord({ id: "m:safety", kind: "decision", title: "权限安全", content: "危险操作需要审批。" });
  await semanticMemory.addRecord({ id: "m:cache", kind: "lesson", title: "缓存", content: "稳定前缀提高命中。" });
  const callsBeforeInitialize = embeddingCalls;
  await semanticMemory.initialize();
  const callsAfterInitialize = embeddingCalls;
  const semantic = await semanticMemory.search({ query: "危险权限操作", mode: "semantic" });
  const callsBeforeExact = embeddingCalls;
  const exact = await semanticMemory.search({ query: "TaskContract", mode: "auto" });
  const semanticStatus = semanticMemory.status();
  semanticMemory.close();

  return {
    schemaVersion: 1,
    fixture: "memory-vector-dual-path",
    lexicalOnly: {
      status: {
        lexicalReady: lexicalStatus.lexicalReady,
        recordCount: lexicalStatus.recordCount,
        ftsIndexedCount: lexicalStatus.ftsIndexedCount,
        semanticConfigured: lexicalStatus.semanticConfigured,
        semanticStatus: lexicalStatus.semanticStatus,
        vectorIndexedCount: lexicalStatus.vectorIndexedCount,
      },
      lexicalResultIds: lexical.results.map((result) => result.record.id),
      semanticRequest: {
        requestedMode: fallback.requestedMode,
        executedMode: fallback.executedMode,
        degraded: fallback.degraded,
        degradationReason: fallback.degradationReason,
      },
    },
    semantic: {
      callsBeforeInitialize,
      callsAfterInitialize,
      resultIds: semantic.results.map((result) => result.record.id),
      executedMode: semantic.executedMode,
      callsBeforeExact,
      callsAfterExact: embeddingCalls,
      exactExecutedMode: exact.executedMode,
      status: {
        semanticConfigured: semanticStatus.semanticConfigured,
        semanticStatus: semanticStatus.semanticStatus,
        vectorIndexedCount: semanticStatus.vectorIndexedCount,
      },
    },
  };
}

function normalizeDecision(decision: ReturnType<PermissionEngine["evaluate"]>): unknown {
  if (decision.kind === "allow") {
    return {
      kind: decision.kind,
      source: decision.source,
      grantApplied: decision.grantId !== undefined,
    };
  }
  if (decision.kind === "deny") return decision;
  const action =
    decision.request.action.kind === "filesystem.read" ||
    decision.request.action.kind === "filesystem.write"
      ? { kind: decision.request.action.kind, path: "{canonical_path}" }
      : decision.request.action;
  return {
    kind: decision.kind,
    request: {
      action,
      reason: decision.request.reason,
      risk: decision.request.risk,
      sandboxAllowed: decision.request.sandboxAllowed,
      availableScopes: decision.request.availableScopes,
      status: decision.request.status,
    },
  };
}

function buildPermissionFixture(): unknown {
  const projectRoot = resolve("oracle-workspace");
  const envelope = {
    projectId: "project:permission-oracle",
    missionId: "mission:permission-oracle",
    runId: "run:permission-oracle",
    actorId: "agent:oracle",
    integrity: "trusted" as const,
  };
  const workspace = new PermissionEngine(createWorkspaceWriteProfile(projectRoot), "never-within-sandbox");
  const inside = workspace.evaluate({ kind: "filesystem.write", path: join(projectRoot, "src", "main.rs") }, envelope);
  const outside = workspace.evaluate({ kind: "filesystem.write", path: `${projectRoot}-other/main.rs` }, envelope);

  const always = new PermissionEngine(createWorkspaceWriteProfile(projectRoot), "always");
  const action = { kind: "filesystem.write" as const, path: join(projectRoot, "src", "lib.rs") };
  const first = always.evaluate(action, envelope);
  if (first.kind !== "request-approval") throw new Error("oracle-approval-not-requested");
  always.respond(first.request.id, "allow", "once");
  const afterGrant = always.evaluate(action, envelope);
  const afterConsume = always.evaluate(action, envelope);

  return {
    schemaVersion: 1,
    fixture: "permission-path-and-grants",
    inside: normalizeDecision(inside),
    outside: normalizeDecision(outside),
    allowOnceSequence: [normalizeDecision(first), normalizeDecision(afterGrant), normalizeDecision(afterConsume)],
  };
}

async function main(): Promise<void> {
  const fixtures: Record<string, unknown> = {
    "mission-parallel-approval-join.v1.json": await buildMissionFixture(),
    "mission-command-decisions.v1.json": buildCommandDecisionFixture(),
    "context-budget-cache-compaction.v1.json": await buildContextFixture(),
    "memory-vector-dual-path.v1.json": await buildMemoryFixture(),
    "permission-path-and-grants.v1.json": buildPermissionFixture(),
  };
  const serialized = Object.fromEntries(
    Object.entries(fixtures).map(([name, value]) => [name, stableJson(value)]),
  );
  const manifest = stableJson({
    schemaVersion: 1,
    source: "TypeScript D0 oracle",
    generatedBy: "scripts/export-ts-oracle.ts",
    fixtures: Object.entries(serialized)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([name, content]) => ({ name, sha256: sha256(content) })),
  });
  const allFiles = { ...serialized, "fixture-manifest.v1.json": manifest };

  if (checkOnly) {
    const mismatches: string[] = [];
    for (const [name, expected] of Object.entries(allFiles)) {
      let actual = "";
      try {
        actual = readFileSync(join(fixtureDirectory, name), "utf8").replaceAll("\r\n", "\n");
      } catch {
        mismatches.push(`${name}: missing`);
        continue;
      }
      if (actual !== expected) mismatches.push(`${name}: content mismatch`);
    }
    if (mismatches.length > 0) throw new Error(`fixture-check-failed\n${mismatches.join("\n")}`);
    console.log(`TS oracle fixtures verified: ${Object.keys(allFiles).length}`);
    return;
  }

  mkdirSync(fixtureDirectory, { recursive: true });
  for (const [name, content] of Object.entries(allFiles)) writeFileSync(join(fixtureDirectory, name), content);
  console.log(`TS oracle fixtures written: ${Object.keys(allFiles).length}`);
}

await main();
