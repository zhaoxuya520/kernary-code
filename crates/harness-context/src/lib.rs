#![forbid(unsafe_code)]

//! Context、Prompt 和 Compaction 的确定性算法核心。

mod compaction;
mod context;
mod prompt;
mod store;

pub use compaction::{
    CompactionError, CompactionItem, CompactionMode, CompactionRecord, CompactionResult,
    ContextCheckpoint, ContextCompactor, StructuredSummary, SummaryProvider, ToolPhase,
};
pub use context::{
    BudgetError, CacheClass, CompileExclusion, CompileExclusionReason, CompiledContext,
    Compressibility, ContextBroker, ContextBudget, ContextItem, ContextKind, ContextRegistry,
    DedupResult, HeuristicTokenizer, InsertOutcome, Priority, Role, Tokenizer,
};
pub use prompt::{
    CanonicalPrompt, PromptCacheability, PromptCanonicalizer, PromptError, PromptRegistry,
    PromptRole, PromptSegment, PromptSource, ToolPromptSchema, canonical_json,
};
pub use store::{
    ContextSeries, ContextStore, ContextStoreError, ContextTransition, fork_context_series,
};
