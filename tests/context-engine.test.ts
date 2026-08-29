import assert from "node:assert/strict";
import test from "node:test";
import {
  CompactionEngine,
  ContextBudgetExceeded,
  ContextCompiler,
  type ContextFragment,
  type SessionItem,
} from "../packages/context-engine/src/context-engine.ts";

const budget = {
  modelContextWindow: 100,
  reservedOutputTokens: 20,
  reservedToolTokens: 10,
  reservedRecoveryTokens: 10,
};

test("Context Compiler 保持 static → append-only → dynamic-tail，并优先裁掉低价值记忆", () => {
  const compiler = new ContextCompiler();
  const fragments: ContextFragment[] = [
    { id: "dynamic", kind: "runtime", role: "developer", content: "当前预算", priority: 90, order: 0, cacheClass: "dynamic-tail", tokenEstimate: 10, sourceRefs: [] },
    { id: "memory", kind: "memory", role: "developer", content: "低价值历史记忆", priority: 10, order: 0, cacheClass: "dynamic-tail", tokenEstimate: 30, sourceRefs: ["m1"] },
    { id: "history", kind: "history", role: "user", content: "最近对话", priority: 70, order: 0, cacheClass: "append-only", tokenEstimate: 20, sourceRefs: [] },
    { id: "safety", kind: "safety", role: "system", content: "安全规则", priority: 100, order: 0, cacheClass: "static", hardRequired: true, tokenEstimate: 20, sourceRefs: [] },
  ];
  const result = compiler.compile({ fragments, budget, contextSeriesId: "series:1" });
  assert.deepEqual(result.fragments.map((fragment) => fragment.id), ["safety", "history", "dynamic"]);
  assert.deepEqual(result.excluded.map((item) => item.fragment.id), ["memory"]);
  assert.equal(result.inputTokenEstimate, 50);
});

test("动态尾部变化不改变 stable prefix hash，但会改变完整指纹", () => {
  const compiler = new ContextCompiler();
  const base: ContextFragment[] = [
    { id: "abi", kind: "role-abi", role: "system", content: "稳定 ABI", priority: 100, order: 0, cacheClass: "static", hardRequired: true, sourceRefs: [] },
    { id: "history", kind: "history", role: "user", content: "append only", priority: 80, order: 0, cacheClass: "append-only", sourceRefs: [] },
  ];
  const first = compiler.compile({
    fragments: [...base, { id: "tail", kind: "runtime", role: "developer", content: "budget=10", priority: 50, order: 0, cacheClass: "dynamic-tail", sourceRefs: [] }],
    budget: { ...budget, modelContextWindow: 1000 },
    contextSeriesId: "series:cache",
  });
  const second = compiler.compile({
    fragments: [...base, { id: "tail", kind: "runtime", role: "developer", content: "budget=9", priority: 50, order: 0, cacheClass: "dynamic-tail", sourceRefs: [] }],
    budget: { ...budget, modelContextWindow: 1000 },
    contextSeriesId: "series:cache",
  });
  assert.equal(first.cache.stablePrefixHash, second.cache.stablePrefixHash);
  assert.equal(first.cache.appendOnlyHash, second.cache.appendOnlyHash);
  assert.notEqual(first.cache.fullFingerprint, second.cache.fullFingerprint);
});

test("必需上下文本身超过预算时明确失败", () => {
  const compiler = new ContextCompiler();
  assert.throws(
    () =>
      compiler.compile({
        fragments: [{ id: "contract", kind: "contract", role: "developer", content: "x", priority: 100, order: 0, cacheClass: "static", hardRequired: true, tokenEstimate: 61, sourceRefs: [] }],
        budget,
        contextSeriesId: "series:overflow",
      }),
    ContextBudgetExceeded,
  );
});

test("Compaction 精确保留工具对、审批、契约和最近消息", async () => {
  const items: SessionItem[] = [
    { id: "u1", sequence: 1, kind: "user-message", content: "请实现功能", tokenEstimate: 30, sourceRefs: [] },
    { id: "a1", sequence: 2, kind: "agent-message", content: "开始分析", tokenEstimate: 30, sourceRefs: [] },
    { id: "tc", sequence: 3, kind: "tool-call", content: "read file", pairId: "tool:1", tokenEstimate: 10, sourceRefs: [] },
    { id: "tr", sequence: 4, kind: "tool-result", content: "file data", pairId: "tool:1", tokenEstimate: 20, sourceRefs: [] },
    { id: "approval", sequence: 5, kind: "approval", content: "allow once", tokenEstimate: 10, sourceRefs: ["approval:1"] },
    { id: "contract", sequence: 6, kind: "contract", content: "workspace only", tokenEstimate: 10, sourceRefs: ["contract:1"] },
    { id: "recent-user", sequence: 7, kind: "user-message", content: "继续", tokenEstimate: 5, sourceRefs: [] },
    { id: "recent-agent", sequence: 8, kind: "agent-message", content: "正在继续", tokenEstimate: 5, sourceRefs: [] },
  ];
  const engine = new CompactionEngine({
    async summarize({ items: summarizedItems }) {
      return {
        summary: `已总结：${summarizedItems.map((item) => item.id).join(",")}`,
        activeAssumptions: [],
        unresolvedBlockers: [],
        completedActions: ["分析完成"],
        nextGoal: "继续实现",
      };
    },
  });
  const result = await engine.compact({
    items,
    recentItemCount: 2,
    maxSummaryTokens: 20,
    contextSeriesId: "series:before",
  });
  const visibleIds = result.visibleItems.map((item) => item.id);
  for (const retainedId of ["tc", "tr", "approval", "contract", "recent-user", "recent-agent"]) {
    assert.ok(visibleIds.includes(retainedId), `${retainedId} 应被精确保留`);
  }
  assert.deepEqual(result.record.summarizedItemIds, ["u1", "a1"]);
  assert.ok(result.record.tokenEstimateAfter < result.record.tokenUsageBefore);
  assert.notEqual(result.record.previousSeriesId, result.record.nextSeriesId);
});

test("缺少 Tool Result 时拒绝压缩", async () => {
  const engine = new CompactionEngine({
    async summarize() {
      return { summary: "x", activeAssumptions: [], unresolvedBlockers: [], completedActions: [], nextGoal: "x" };
    },
  });
  await assert.rejects(
    engine.compact({
      items: [
        { id: "old", sequence: 1, kind: "agent-message", content: "old", sourceRefs: [] },
        { id: "call", sequence: 2, kind: "tool-call", content: "call", pairId: "missing", sourceRefs: [] },
        { id: "recent", sequence: 3, kind: "user-message", content: "recent", sourceRefs: [] },
      ],
      recentItemCount: 1,
      maxSummaryTokens: 10,
      contextSeriesId: "series:broken",
    }),
    /incomplete-tool-pair/,
  );
});
