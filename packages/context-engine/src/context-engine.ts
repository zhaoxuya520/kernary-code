import { createHash, randomUUID } from "node:crypto";

export type CacheClass = "static" | "append-only" | "dynamic-tail";

export interface ContextFragment {
  id: string;
  kind: string;
  role: "system" | "developer" | "user" | "assistant" | "tool";
  content: string;
  priority: number;
  order: number;
  cacheClass: CacheClass;
  hardRequired?: boolean;
  tokenEstimate?: number;
  sourceRefs: string[];
}

export interface ContextBudget {
  modelContextWindow: number;
  reservedOutputTokens: number;
  reservedToolTokens: number;
  reservedRecoveryTokens: number;
}

export interface CompiledContext {
  fragments: ContextFragment[];
  excluded: Array<{ fragment: ContextFragment; reason: "budget" }>;
  inputTokenEstimate: number;
  maxInputTokens: number;
  cache: CacheFingerprint;
}

export interface CacheFingerprint {
  stablePrefixHash: string;
  appendOnlyHash: string;
  contextSeriesId: string;
  fullFingerprint: string;
  stableTokenEstimate: number;
  appendOnlyTokenEstimate: number;
  dynamicTokenEstimate: number;
}

export class ContextBudgetExceeded extends Error {
  readonly requiredTokens: number;
  readonly maxInputTokens: number;

  constructor(requiredTokens: number, maxInputTokens: number) {
    super(`required-context-exceeds-budget: required=${requiredTokens}, max=${maxInputTokens}`);
    this.requiredTokens = requiredTokens;
    this.maxInputTokens = maxInputTokens;
  }
}

/** 粗略估算仅用于本地预算；正式 Provider 可以替换成自己的 tokenizer。 */
export function estimateTokens(content: string): number {
  let units = 0;
  for (const character of content) {
    units += /[\u3400-\u9fff\uf900-\ufaff]/u.test(character) ? 0.75 : 0.25;
  }
  return Math.max(1, Math.ceil(units));
}

function fragmentTokens(fragment: ContextFragment): number {
  return fragment.tokenEstimate ?? estimateTokens(fragment.content);
}

function digest(parts: string[]): string {
  const hash = createHash("sha256");
  for (const part of parts) {
    hash.update(String(part.length));
    hash.update(":");
    hash.update(part);
    hash.update("\n");
  }
  return hash.digest("hex");
}

function canonicalFragment(fragment: ContextFragment): string {
  return JSON.stringify({
    id: fragment.id,
    kind: fragment.kind,
    role: fragment.role,
    content: fragment.content,
    priority: fragment.priority,
    order: fragment.order,
    cacheClass: fragment.cacheClass,
    sourceRefs: fragment.sourceRefs,
  });
}

/**
 * ContextCompiler 先固定 ABI 顺序，再从低优先级可选片段开始裁剪。
 * 不能通过重排旧内容来“凑预算”，否则会无谓破坏 Prompt Cache。
 */
export class ContextCompiler {
  compile(input: {
    fragments: ContextFragment[];
    budget: ContextBudget;
    contextSeriesId: string;
  }): CompiledContext {
    const maxInputTokens =
      input.budget.modelContextWindow -
      input.budget.reservedOutputTokens -
      input.budget.reservedToolTokens -
      input.budget.reservedRecoveryTokens;
    if (maxInputTokens <= 0) throw new ContextBudgetExceeded(0, maxInputTokens);

    const classOrder: Record<CacheClass, number> = {
      static: 0,
      "append-only": 1,
      "dynamic-tail": 2,
    };
    const ordered = [...input.fragments].sort(
      (left, right) =>
        classOrder[left.cacheClass] - classOrder[right.cacheClass] ||
        left.order - right.order ||
        left.id.localeCompare(right.id),
    );

    const requiredTokens = ordered
      .filter((fragment) => fragment.hardRequired)
      .reduce((total, fragment) => total + fragmentTokens(fragment), 0);
    if (requiredTokens > maxInputTokens) throw new ContextBudgetExceeded(requiredTokens, maxInputTokens);

    let selected = [...ordered];
    let totalTokens = selected.reduce((total, fragment) => total + fragmentTokens(fragment), 0);
    const excluded: CompiledContext["excluded"] = [];

    if (totalTokens > maxInputTokens) {
      const removable = selected
        .filter((fragment) => !fragment.hardRequired)
        .sort(
          (left, right) =>
            left.priority - right.priority ||
            classOrder[right.cacheClass] - classOrder[left.cacheClass] ||
            right.order - left.order,
        );

      const removedIds = new Set<string>();
      for (const fragment of removable) {
        if (totalTokens <= maxInputTokens) break;
        removedIds.add(fragment.id);
        totalTokens -= fragmentTokens(fragment);
        excluded.push({ fragment, reason: "budget" });
      }
      selected = selected.filter((fragment) => !removedIds.has(fragment.id));
    }

    if (totalTokens > maxInputTokens) throw new ContextBudgetExceeded(totalTokens, maxInputTokens);

    const stable = selected.filter((fragment) => fragment.cacheClass === "static");
    const appendOnly = selected.filter((fragment) => fragment.cacheClass === "append-only");
    const dynamic = selected.filter((fragment) => fragment.cacheClass === "dynamic-tail");
    const stablePrefixHash = digest(stable.map(canonicalFragment));
    const appendOnlyHash = digest(appendOnly.map(canonicalFragment));
    const fullFingerprint = digest([
      stablePrefixHash,
      appendOnlyHash,
      input.contextSeriesId,
      ...dynamic.map(canonicalFragment),
    ]);

    return {
      fragments: selected,
      excluded,
      inputTokenEstimate: totalTokens,
      maxInputTokens,
      cache: {
        stablePrefixHash,
        appendOnlyHash,
        contextSeriesId: input.contextSeriesId,
        fullFingerprint,
        stableTokenEstimate: stable.reduce((total, fragment) => total + fragmentTokens(fragment), 0),
        appendOnlyTokenEstimate: appendOnly.reduce((total, fragment) => total + fragmentTokens(fragment), 0),
        dynamicTokenEstimate: dynamic.reduce((total, fragment) => total + fragmentTokens(fragment), 0),
      },
    };
  }
}

export type SessionItemKind =
  | "user-message"
  | "agent-message"
  | "reasoning-summary"
  | "tool-call"
  | "tool-result"
  | "approval"
  | "contract"
  | "decision"
  | "compaction-summary";

export interface SessionItem {
  id: string;
  sequence: number;
  kind: SessionItemKind;
  content: string;
  pairId?: string;
  inFlight?: boolean;
  tokenEstimate?: number;
  sourceRefs: string[];
}

export interface StructuredCompactionSummary {
  summary: string;
  activeAssumptions: string[];
  unresolvedBlockers: string[];
  completedActions: string[];
  nextGoal: string;
}

export interface CompactionSummarizerPort {
  summarize(input: {
    items: SessionItem[];
    maxSummaryTokens: number;
  }): Promise<StructuredCompactionSummary>;
}

export interface CompactionRecord {
  id: string;
  sourceRange: { firstSequence: number; lastSequence: number };
  summaryItem: SessionItem;
  retainedItemIds: string[];
  summarizedItemIds: string[];
  tokenUsageBefore: number;
  tokenEstimateAfter: number;
  activeAssumptions: string[];
  unresolvedBlockers: string[];
  completedActions: string[];
  nextGoal: string;
  sourceHash: string;
  previousSeriesId: string;
  nextSeriesId: string;
}

export interface CompactionResult {
  visibleItems: SessionItem[];
  record: CompactionRecord;
}

function sessionItemTokens(item: SessionItem): number {
  return item.tokenEstimate ?? estimateTokens(item.content);
}

/** 审批、契约、决策和完整工具调用对在 D0 中精确保留，不交给摘要模型改写。 */
function mustRetainExactly(item: SessionItem): boolean {
  return (
    item.kind === "approval" ||
    item.kind === "contract" ||
    item.kind === "decision" ||
    item.kind === "tool-call" ||
    item.kind === "tool-result" ||
    item.inFlight === true
  );
}

export class CompactionEngine {
  readonly summarizer: CompactionSummarizerPort;

  constructor(summarizer: CompactionSummarizerPort) {
    this.summarizer = summarizer;
  }

  async compact(input: {
    items: SessionItem[];
    recentItemCount: number;
    maxSummaryTokens: number;
    contextSeriesId: string;
  }): Promise<CompactionResult> {
    if (input.items.length === 0) throw new Error("cannot-compact-empty-session");
    const ordered = [...input.items].sort((left, right) => left.sequence - right.sequence);
    const recentStart = Math.max(0, ordered.length - Math.max(0, input.recentItemCount));
    const older = ordered.slice(0, recentStart);
    const recent = ordered.slice(recentStart);
    const retainedOlder = older.filter(mustRetainExactly);
    const summarized = older.filter((item) => !mustRetainExactly(item));
    if (summarized.length === 0) throw new Error("no-safe-items-to-compact");

    // 工具必须成对存在；发现缺失时拒绝压缩，而不是产生不可继续的上下文。
    const toolPairs = new Map<string, Set<SessionItemKind>>();
    for (const item of ordered.filter((candidate) => candidate.kind === "tool-call" || candidate.kind === "tool-result")) {
      if (!item.pairId) throw new Error(`tool-item-missing-pair-id: ${item.id}`);
      const pair = toolPairs.get(item.pairId) ?? new Set<SessionItemKind>();
      pair.add(item.kind);
      toolPairs.set(item.pairId, pair);
    }
    for (const [pairId, kinds] of toolPairs) {
      if (!kinds.has("tool-call") || !kinds.has("tool-result")) {
        throw new Error(`incomplete-tool-pair: ${pairId}`);
      }
    }

    const summary = await this.summarizer.summarize({
      items: summarized,
      maxSummaryTokens: input.maxSummaryTokens,
    });
    const firstSequence = summarized[0]?.sequence ?? older[0]?.sequence ?? 0;
    const lastSequence = summarized.at(-1)?.sequence ?? older.at(-1)?.sequence ?? firstSequence;
    const summaryItem: SessionItem = {
      id: `compaction:${randomUUID()}`,
      sequence: firstSequence,
      kind: "compaction-summary",
      content: summary.summary,
      tokenEstimate: Math.min(input.maxSummaryTokens, estimateTokens(summary.summary)),
      sourceRefs: summarized.flatMap((item) => item.sourceRefs),
    };
    const retained = [...retainedOlder, ...recent].sort((left, right) => left.sequence - right.sequence);
    const visibleItems = [summaryItem, ...retained].sort((left, right) => left.sequence - right.sequence || left.id.localeCompare(right.id));
    const previousSeriesId = input.contextSeriesId;
    const sourceHash = digest(summarized.map((item) => JSON.stringify(item)));
    const nextSeriesId = `series:${digest([previousSeriesId, sourceHash, summary.summary]).slice(0, 24)}`;

    return {
      visibleItems,
      record: {
        id: `compaction-record:${randomUUID()}`,
        sourceRange: { firstSequence, lastSequence },
        summaryItem,
        retainedItemIds: retained.map((item) => item.id),
        summarizedItemIds: summarized.map((item) => item.id),
        tokenUsageBefore: ordered.reduce((total, item) => total + sessionItemTokens(item), 0),
        tokenEstimateAfter: visibleItems.reduce((total, item) => total + sessionItemTokens(item), 0),
        activeAssumptions: summary.activeAssumptions,
        unresolvedBlockers: summary.unresolvedBlockers,
        completedActions: summary.completedActions,
        nextGoal: summary.nextGoal,
        sourceHash,
        previousSeriesId,
        nextSeriesId,
      },
    };
  }
}
