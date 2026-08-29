use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{ContentHash, ContextItemId, InformationFlowLabel};
use serde::{Deserialize, Serialize};

/// Context 的业务分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextKind {
    System,
    Goal,
    Task,
    Conversation,
    Repository,
    Memory,
    Tool,
    Agent,
    Temporary,
    Pinned,
    Constraint,
    Decision,
    Error,
}

/// Context 优先级。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

/// Context 被压缩/淘汰的允许程度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Compressibility {
    Exact,
    Structured,
    Semantic,
    Disposable,
}

/// Prompt Cache 中的稳定性类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheClass {
    Static,
    SemiStable,
    DynamicTail,
}

/// 单段 Context；所有 Provider 输入都由这些带来源的片段编译而来。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextItem {
    pub id: ContextItemId,
    pub kind: ContextKind,
    pub priority: Priority,
    pub token_cost: u32,
    pub source: String,
    pub timestamp_millis: i64,
    /// 0–1000，避免浮点比较破坏确定性。
    pub importance: u16,
    pub compressibility: Compressibility,
    pub ttl_millis: Option<u64>,
    pub content_hash: ContentHash,
    pub source_identity: String,
    pub information_flow: InformationFlowLabel,
    pub cache_class: CacheClass,
    pub order: i32,
    pub hard_required: bool,
    pub content: String,
}

impl ContextItem {
    #[must_use]
    pub fn expired_at(&self, now_millis: i64) -> bool {
        let Some(ttl) = self.ttl_millis else {
            return false;
        };
        let ttl = i64::try_from(ttl).unwrap_or(i64::MAX);
        now_millis > self.timestamp_millis.saturating_add(ttl)
    }
}

/// Registry 插入结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    Replaced,
    Unchanged,
}

/// 当前作用域的 Context Registry。
#[derive(Clone, Debug, Default)]
pub struct ContextRegistry {
    items: BTreeMap<ContextItemId, ContextItem>,
}

impl ContextRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, item: ContextItem) -> InsertOutcome {
        match self.items.get(&item.id) {
            Some(existing) if existing == &item => InsertOutcome::Unchanged,
            Some(_) => {
                self.items.insert(item.id.clone(), item);
                InsertOutcome::Replaced
            }
            None => {
                self.items.insert(item.id.clone(), item);
                InsertOutcome::Inserted
            }
        }
    }

    pub fn remove(&mut self, id: &ContextItemId) -> Option<ContextItem> {
        self.items.remove(id)
    }

    #[must_use]
    pub fn items(&self) -> Vec<ContextItem> {
        self.items.values().cloned().collect()
    }
}

/// 精确 source/hash 去重结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupResult {
    pub selected: Vec<ContextItem>,
    pub duplicate_ids: Vec<ContextItemId>,
}

fn preference(item: &ContextItem) -> (Priority, u16, bool, i64, Reverse<ContextItemId>) {
    (
        item.priority,
        item.importance,
        item.hard_required,
        item.timestamp_millis,
        Reverse(item.id.clone()),
    )
}

/// 按 source identity + content hash 去重，并保留更权威的片段。
#[must_use]
pub fn deduplicate(items: Vec<ContextItem>) -> DedupResult {
    let mut by_source = BTreeMap::<(String, ContentHash), ContextItem>::new();
    let mut duplicates = Vec::new();
    for item in items {
        let key = (item.source_identity.clone(), item.content_hash.clone());
        match by_source.get(&key) {
            Some(existing) if preference(existing) >= preference(&item) => {
                duplicates.push(item.id);
            }
            Some(existing) => {
                duplicates.push(existing.id.clone());
                by_source.insert(key, item);
            }
            None => {
                by_source.insert(key, item);
            }
        }
    }
    duplicates.sort();
    DedupResult {
        selected: by_source.into_values().collect(),
        duplicate_ids: duplicates,
    }
}

/// 模型窗口与各 lane 的硬预算。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextBudget {
    pub model_context_window: u32,
    pub reserved_output_tokens: u32,
    pub reserved_tool_tokens: u32,
    pub reserved_recovery_tokens: u32,
    pub lane_caps: BTreeMap<ContextKind, u32>,
}

impl ContextBudget {
    #[must_use]
    pub fn max_input_tokens(&self) -> u32 {
        self.model_context_window
            .saturating_sub(self.reserved_output_tokens)
            .saturating_sub(self.reserved_tool_tokens)
            .saturating_sub(self.reserved_recovery_tokens)
    }
}

/// Context 未进入 Prompt 的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompileExclusionReason {
    Expired,
    Duplicate,
    RoleIsolation,
    LaneBudget,
    TotalBudget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompileExclusion {
    pub item_id: ContextItemId,
    pub reason: CompileExclusionReason,
}

/// 预算编译后的 Context。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompiledContext {
    pub selected: Vec<ContextItem>,
    pub exclusions: Vec<CompileExclusion>,
    pub token_cost: u32,
    pub max_input_tokens: u32,
}

/// 必需 Context 超过硬预算。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetError {
    pub code: &'static str,
    pub required_tokens: u32,
    pub available_tokens: u32,
    pub kind: Option<ContextKind>,
}

impl Display for BudgetError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}: required={}, available={}, kind={:?}",
            self.code, self.required_tokens, self.available_tokens, self.kind
        )
    }
}

impl Error for BudgetError {}

/// 子 Agent/Decision 调用的角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Supervisor,
    Planner,
    Staffing,
    Coder,
    Reviewer,
    Coordinator,
    Tester,
}

/// Provider-specific tokenizer 的 Port。
pub trait Tokenizer: Send + Sync {
    fn count_tokens(&self, content: &str) -> u32;
}

/// 没有 Provider tokenizer 时的确定性保守估算。
#[derive(Clone, Copy, Debug, Default)]
pub struct HeuristicTokenizer;

impl Tokenizer for HeuristicTokenizer {
    fn count_tokens(&self, content: &str) -> u32 {
        let units = content.chars().fold(0_u32, |total, character| {
            total.saturating_add(if ('\u{3400}'..='\u{9fff}').contains(&character) {
                3
            } else {
                1
            })
        });
        units.saturating_add(3) / 4
    }
}

/// 按角色生成 Minimal Working Context。
#[derive(Clone, Copy, Debug, Default)]
pub struct ContextBroker;

impl ContextBroker {
    pub fn compile_for_role(
        &self,
        role: Role,
        items: Vec<ContextItem>,
        budget: &ContextBudget,
        now_millis: i64,
    ) -> Result<CompiledContext, BudgetError> {
        let mut exclusions = Vec::new();
        let filtered = items
            .into_iter()
            .filter(|item| {
                let allowed = role_allows(role, item.kind);
                if !allowed {
                    exclusions.push(CompileExclusion {
                        item_id: item.id.clone(),
                        reason: CompileExclusionReason::RoleIsolation,
                    });
                }
                allowed
            })
            .collect();
        let mut compiled = compile_context(filtered, budget, now_millis)?;
        exclusions.append(&mut compiled.exclusions);
        compiled.exclusions = exclusions;
        Ok(compiled)
    }
}

fn role_allows(role: Role, kind: ContextKind) -> bool {
    if role == Role::Supervisor {
        return true;
    }
    match role {
        Role::Planner => matches!(
            kind,
            ContextKind::System
                | ContextKind::Goal
                | ContextKind::Task
                | ContextKind::Repository
                | ContextKind::Memory
                | ContextKind::Pinned
                | ContextKind::Constraint
                | ContextKind::Decision
                | ContextKind::Error
        ),
        Role::Staffing => matches!(
            kind,
            ContextKind::System
                | ContextKind::Goal
                | ContextKind::Task
                | ContextKind::Agent
                | ContextKind::Pinned
                | ContextKind::Constraint
                | ContextKind::Decision
        ),
        Role::Coder => !matches!(kind, ContextKind::Agent),
        Role::Reviewer | Role::Tester => !matches!(
            kind,
            ContextKind::Conversation | ContextKind::Agent | ContextKind::Temporary
        ),
        Role::Coordinator => matches!(
            kind,
            ContextKind::System
                | ContextKind::Goal
                | ContextKind::Task
                | ContextKind::Agent
                | ContextKind::Pinned
                | ContextKind::Constraint
                | ContextKind::Decision
                | ContextKind::Error
        ),
        Role::Supervisor => true,
    }
}

fn removal_order(item: &ContextItem) -> (Priority, bool, Compressibility, u16, i64, ContextItemId) {
    (
        item.priority,
        item.hard_required,
        item.compressibility,
        item.importance,
        item.timestamp_millis,
        item.id.clone(),
    )
}

fn presentation_order(item: &ContextItem) -> (CacheClass, i32, ContextItemId) {
    (item.cache_class, item.order, item.id.clone())
}

/// 去重、TTL、lane 和总预算编译。
pub fn compile_context(
    items: Vec<ContextItem>,
    budget: &ContextBudget,
    now_millis: i64,
) -> Result<CompiledContext, BudgetError> {
    let mut exclusions = Vec::new();
    let active = items
        .into_iter()
        .filter(|item| {
            let keep = item.hard_required || !item.expired_at(now_millis);
            if !keep {
                exclusions.push(CompileExclusion {
                    item_id: item.id.clone(),
                    reason: CompileExclusionReason::Expired,
                });
            }
            keep
        })
        .collect();
    let dedup = deduplicate(active);
    exclusions.extend(
        dedup
            .duplicate_ids
            .into_iter()
            .map(|item_id| CompileExclusion {
                item_id,
                reason: CompileExclusionReason::Duplicate,
            }),
    );
    let mut selected = dedup.selected;

    for (kind, cap) in &budget.lane_caps {
        let required = selected
            .iter()
            .filter(|item| item.kind == *kind && item.hard_required)
            .map(|item| item.token_cost)
            .sum::<u32>();
        if required > *cap {
            return Err(BudgetError {
                code: "required-lane-context-exceeds-budget",
                required_tokens: required,
                available_tokens: *cap,
                kind: Some(*kind),
            });
        }
        let mut lane_total = selected
            .iter()
            .filter(|item| item.kind == *kind)
            .map(|item| item.token_cost)
            .sum::<u32>();
        let mut candidates = selected
            .iter()
            .filter(|item| item.kind == *kind && !item.hard_required)
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by_key(removal_order);
        let mut removed = BTreeSet::new();
        for item in candidates {
            if lane_total <= *cap {
                break;
            }
            lane_total = lane_total.saturating_sub(item.token_cost);
            removed.insert(item.id.clone());
            exclusions.push(CompileExclusion {
                item_id: item.id,
                reason: CompileExclusionReason::LaneBudget,
            });
        }
        selected.retain(|item| !removed.contains(&item.id));
    }

    let max_input = budget.max_input_tokens();
    let required = selected
        .iter()
        .filter(|item| item.hard_required)
        .map(|item| item.token_cost)
        .sum::<u32>();
    if required > max_input {
        return Err(BudgetError {
            code: "required-context-exceeds-budget",
            required_tokens: required,
            available_tokens: max_input,
            kind: None,
        });
    }
    let mut total = selected.iter().map(|item| item.token_cost).sum::<u32>();
    let mut candidates = selected
        .iter()
        .filter(|item| !item.hard_required)
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by_key(removal_order);
    let mut removed = BTreeSet::new();
    for item in candidates {
        if total <= max_input {
            break;
        }
        total = total.saturating_sub(item.token_cost);
        removed.insert(item.id.clone());
        exclusions.push(CompileExclusion {
            item_id: item.id,
            reason: CompileExclusionReason::TotalBudget,
        });
    }
    selected.retain(|item| !removed.contains(&item.id));
    selected.sort_by_key(presentation_order);
    exclusions.sort_by(|left, right| left.item_id.cmp(&right.item_id));
    Ok(CompiledContext {
        selected,
        exclusions,
        token_cost: total,
        max_input_tokens: max_input,
    })
}

#[cfg(test)]
mod tests {
    use harness_types::{ConfidentialityLabel, IntegrityLabel};

    use super::*;

    fn item(id: &str, kind: ContextKind, priority: Priority, tokens: u32) -> ContextItem {
        ContextItem {
            id: ContextItemId::from(id),
            kind,
            priority,
            token_cost: tokens,
            source: id.to_owned(),
            timestamp_millis: 0,
            importance: 500,
            compressibility: Compressibility::Structured,
            ttl_millis: None,
            content_hash: ContentHash::from(format!("hash:{id}")),
            source_identity: id.to_owned(),
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality: ConfidentialityLabel::ProjectPrivate,
            },
            cache_class: CacheClass::DynamicTail,
            order: 0,
            hard_required: priority == Priority::Critical,
            content: id.to_owned(),
        }
    }

    fn budget(max: u32) -> ContextBudget {
        ContextBudget {
            model_context_window: max,
            reserved_output_tokens: 0,
            reserved_tool_tokens: 0,
            reserved_recovery_tokens: 0,
            lane_caps: BTreeMap::new(),
        }
    }

    #[test]
    fn critical_goal_survives_total_budget() {
        let compiled = compile_context(
            vec![
                item("goal", ContextKind::Goal, Priority::Critical, 30),
                item("memory", ContextKind::Memory, Priority::Low, 40),
                item("recent", ContextKind::Conversation, Priority::High, 20),
            ],
            &budget(50),
            0,
        )
        .expect("budget compile");
        assert_eq!(
            compiled
                .selected
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["goal", "recent"]
        );
        assert!(compiled.exclusions.iter().any(|entry| {
            entry.item_id.as_str() == "memory"
                && entry.reason == CompileExclusionReason::TotalBudget
        }));
    }

    #[test]
    fn exact_source_hash_duplicate_keeps_higher_priority() {
        let mut low = item("low", ContextKind::Memory, Priority::Low, 10);
        low.source_identity = "same".to_owned();
        low.content_hash = ContentHash::from("same-hash");
        let mut high = item("high", ContextKind::Memory, Priority::High, 10);
        high.source_identity = "same".to_owned();
        high.content_hash = ContentHash::from("same-hash");
        let result = deduplicate(vec![low, high]);
        assert_eq!(result.selected[0].id.as_str(), "high");
        assert_eq!(result.duplicate_ids[0].as_str(), "low");
    }

    #[test]
    fn reviewer_context_does_not_inherit_conversation_or_agent_catalog() {
        let compiled = ContextBroker
            .compile_for_role(
                Role::Reviewer,
                vec![
                    item("goal", ContextKind::Goal, Priority::Critical, 10),
                    item("chat", ContextKind::Conversation, Priority::High, 10),
                    item("agents", ContextKind::Agent, Priority::Medium, 10),
                    item("repo", ContextKind::Repository, Priority::High, 10),
                ],
                &budget(100),
                0,
            )
            .expect("reviewer context");
        assert_eq!(
            compiled
                .selected
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["goal", "repo"]
        );
    }

    #[test]
    fn expired_optional_item_is_removed() {
        let mut temporary = item("temp", ContextKind::Temporary, Priority::Low, 10);
        temporary.ttl_millis = Some(5);
        let compiled = compile_context(vec![temporary], &budget(100), 6).expect("TTL compile");
        assert!(compiled.selected.is_empty());
        assert_eq!(
            compiled.exclusions[0].reason,
            CompileExclusionReason::Expired
        );
    }
}
