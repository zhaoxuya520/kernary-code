use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{
    ArtifactId, CheckpointId, ConfidentialityLabel, ContentHash, ContextItemId, ContextSeriesId,
    GoalRevisionId, InformationFlowLabel, IntegrityLabel, SessionId, TaskId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CacheClass, Compressibility, ContextItem, ContextKind, Priority};

/// Tool Context 在调用对中的位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolPhase {
    Call,
    Result,
}

/// 带 Tool continuation 信息的 Compaction 输入项。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompactionItem {
    pub context: ContextItem,
    pub pair_id: Option<String>,
    pub tool_phase: Option<ToolPhase>,
    pub in_flight: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionMode {
    Safe,
    Aggressive,
}

/// Summary Provider 必须返回结构化状态，而不是只有一段自然语言。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StructuredSummary {
    pub method: String,
    pub summary: String,
    pub token_cost: u32,
    #[serde(default)]
    pub confirmed_requirements: Vec<String>,
    #[serde(default)]
    pub non_goals: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub history_digest: Vec<String>,
    pub active_assumptions: Vec<String>,
    pub unresolved_blockers: Vec<String>,
    pub completed_actions: Vec<String>,
    #[serde(default)]
    pub modified_files: Vec<String>,
    #[serde(default)]
    pub failed_approaches: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub next_goal: String,
}

pub trait SummaryProvider: Send + Sync {
    fn summarize(
        &self,
        items: &[CompactionItem],
        max_summary_tokens: u32,
    ) -> Result<StructuredSummary, CompactionError>;
}

/// Context + Session 恢复锚点。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextCheckpoint {
    pub id: CheckpointId,
    pub name: Option<String>,
    pub session_id: SessionId,
    pub context_series_id: ContextSeriesId,
    pub goal_revision_id: Option<GoalRevisionId>,
    pub plan_revision: Option<String>,
    pub completed_tasks: Vec<TaskId>,
    pub pending_tasks: Vec<TaskId>,
    pub decision_refs: Vec<String>,
    pub constraint_refs: Vec<String>,
    pub modified_file_refs: Vec<ArtifactId>,
    pub error_refs: Vec<String>,
    pub memory_refs: Vec<String>,
    pub prompt_fingerprint: ContentHash,
    pub created_at_millis: i64,
}

/// 可审计的压缩记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompactionRecord {
    pub mode: CompactionMode,
    #[serde(default)]
    pub summary_method: String,
    pub source_item_ids: Vec<ContextItemId>,
    pub retained_item_ids: Vec<ContextItemId>,
    pub summary_item_id: ContextItemId,
    pub token_cost_before: u32,
    pub token_cost_after: u32,
    pub previous_series_id: ContextSeriesId,
    pub next_series_id: ContextSeriesId,
    pub checkpoint_id: Option<CheckpointId>,
    #[serde(default)]
    pub semantic_anchor_ids: Vec<ContextItemId>,
    #[serde(default)]
    pub anchor_method: String,
    #[serde(default)]
    pub confirmed_requirements: Vec<String>,
    #[serde(default)]
    pub non_goals: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub history_digest: Vec<String>,
    pub active_assumptions: Vec<String>,
    pub unresolved_blockers: Vec<String>,
    pub completed_actions: Vec<String>,
    #[serde(default)]
    pub modified_files: Vec<String>,
    #[serde(default)]
    pub failed_approaches: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub next_goal: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompactionResult {
    pub visible_items: Vec<CompactionItem>,
    pub record: CompactionRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionError {
    pub code: &'static str,
    pub message: String,
}

impl CompactionError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Display for CompactionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for CompactionError {}

/// Safe/Aggressive Context Compactor。
pub struct ContextCompactor<P> {
    provider: P,
}

impl<P: SummaryProvider> ContextCompactor<P> {
    #[must_use]
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn compact(
        &self,
        mode: CompactionMode,
        items: Vec<CompactionItem>,
        recent_item_count: usize,
        max_summary_tokens: u32,
        previous_series_id: ContextSeriesId,
        checkpoint: Option<ContextCheckpoint>,
        now_millis: i64,
    ) -> Result<CompactionResult, CompactionError> {
        self.compact_with_protection(
            mode,
            items,
            recent_item_count,
            max_summary_tokens,
            previous_series_id,
            checkpoint,
            now_millis,
            &BTreeSet::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn compact_with_protection(
        &self,
        mode: CompactionMode,
        mut items: Vec<CompactionItem>,
        recent_item_count: usize,
        max_summary_tokens: u32,
        previous_series_id: ContextSeriesId,
        checkpoint: Option<ContextCheckpoint>,
        now_millis: i64,
        protected_item_ids: &BTreeSet<ContextItemId>,
    ) -> Result<CompactionResult, CompactionError> {
        if items.is_empty() {
            return Err(CompactionError::new("empty-context", "空 Context 不能压缩"));
        }
        if mode == CompactionMode::Aggressive && checkpoint.is_none() {
            return Err(CompactionError::new(
                "checkpoint-required",
                "Aggressive compaction 必须先创建 checkpoint",
            ));
        }
        if let Some(checkpoint) = &checkpoint
            && checkpoint.context_series_id != previous_series_id
        {
            return Err(CompactionError::new(
                "checkpoint-series-mismatch",
                "Checkpoint 必须引用正在压缩的 Context Series",
            ));
        }
        items.sort_by_key(|item| {
            (
                item.context.timestamp_millis,
                item.context.order,
                item.context.id.clone(),
            )
        });
        validate_tool_pairs(&items)?;

        let recent_start = items.len().saturating_sub(recent_item_count);
        let mut retained = Vec::new();
        let mut summarized = Vec::new();
        for (index, item) in items.into_iter().enumerate() {
            if index >= recent_start
                || retain_exact(&item, mode)
                || protected_item_ids.contains(&item.context.id)
            {
                retained.push(item);
            } else {
                summarized.push(item);
            }
        }
        if summarized.is_empty() {
            return Err(CompactionError::new(
                "no-safe-items-to-compact",
                "没有可安全压缩的旧 Context",
            ));
        }
        let summary = self.provider.summarize(&summarized, max_summary_tokens)?;
        if summary.summary.trim().is_empty() || summary.token_cost > max_summary_tokens {
            return Err(CompactionError::new(
                "invalid-summary",
                "Summary 为空或超过预算",
            ));
        }

        let source_item_ids = summarized
            .iter()
            .map(|item| item.context.id.clone())
            .collect::<Vec<_>>();
        let summary_hash = hash_parts(
            source_item_ids
                .iter()
                .map(ContextItemId::as_str)
                .chain(std::iter::once(summary.summary.as_str())),
        );
        let mode_name = format!("{mode:?}");
        let next_series_hash = hash_parts([
            previous_series_id.as_str(),
            summary_hash.as_str(),
            mode_name.as_str(),
        ]);
        let information_flow = strictest_flow(&summarized);
        let summary_item = CompactionItem {
            context: ContextItem {
                id: ContextItemId::from(format!("compaction:{}", &summary_hash.as_str()[..16])),
                kind: ContextKind::Conversation,
                priority: Priority::High,
                token_cost: summary.token_cost,
                source: "context-compactor".to_owned(),
                timestamp_millis: now_millis,
                importance: 900,
                compressibility: Compressibility::Structured,
                ttl_millis: None,
                content_hash: summary_hash,
                source_identity: format!("series:{previous_series_id}"),
                information_flow,
                cache_class: CacheClass::DynamicTail,
                order: i32::MIN,
                hard_required: false,
                content: summary.summary.clone(),
            },
            pair_id: None,
            tool_phase: None,
            in_flight: false,
        };
        let token_cost_before = retained
            .iter()
            .chain(summarized.iter())
            .map(|item| item.context.token_cost)
            .sum();
        retained.push(summary_item.clone());
        retained.sort_by_key(|item| {
            (
                item.context.timestamp_millis,
                item.context.order,
                item.context.id.clone(),
            )
        });
        let token_cost_after = retained.iter().map(|item| item.context.token_cost).sum();
        if token_cost_after >= token_cost_before {
            return Err(CompactionError::new(
                "compaction-not-beneficial",
                format!("压缩没有减少 Token：before={token_cost_before}, after={token_cost_after}"),
            ));
        }
        let retained_item_ids = retained
            .iter()
            .filter(|item| item.context.id != summary_item.context.id)
            .map(|item| item.context.id.clone())
            .collect();
        let next_series_id =
            ContextSeriesId::from(format!("series:{}", &next_series_hash.as_str()[..24]));
        Ok(CompactionResult {
            visible_items: retained,
            record: CompactionRecord {
                mode,
                summary_method: summary.method,
                source_item_ids,
                retained_item_ids,
                summary_item_id: summary_item.context.id,
                token_cost_before,
                token_cost_after,
                previous_series_id,
                next_series_id,
                checkpoint_id: checkpoint.map(|checkpoint| checkpoint.id),
                semantic_anchor_ids: protected_item_ids.iter().cloned().collect(),
                anchor_method: if protected_item_ids.is_empty() {
                    "none".to_owned()
                } else {
                    "explicit".to_owned()
                },
                confirmed_requirements: summary.confirmed_requirements,
                non_goals: summary.non_goals,
                decisions: summary.decisions,
                history_digest: summary.history_digest,
                active_assumptions: summary.active_assumptions,
                unresolved_blockers: summary.unresolved_blockers,
                completed_actions: summary.completed_actions,
                modified_files: summary.modified_files,
                failed_approaches: summary.failed_approaches,
                evidence_refs: summary.evidence_refs,
                next_goal: summary.next_goal,
            },
        })
    }
}

fn retain_exact(item: &CompactionItem, mode: CompactionMode) -> bool {
    item.context.hard_required
        || item.in_flight
        || item.context.compressibility == Compressibility::Exact
        || (mode == CompactionMode::Safe
            && matches!(
                item.context.kind,
                ContextKind::Task | ContextKind::Repository | ContextKind::Memory
            ))
        || matches!(
            item.context.kind,
            ContextKind::Goal
                | ContextKind::Pinned
                | ContextKind::Constraint
                | ContextKind::Decision
                | ContextKind::Error
                | ContextKind::Tool
        )
}

fn validate_tool_pairs(items: &[CompactionItem]) -> Result<(), CompactionError> {
    let mut pairs = BTreeMap::<String, BTreeSet<ToolPhase>>::new();
    let mut in_flight = BTreeSet::new();
    for item in items
        .iter()
        .filter(|item| item.context.kind == ContextKind::Tool)
    {
        let pair_id = item.pair_id.as_ref().ok_or_else(|| {
            CompactionError::new(
                "tool-pair-id-missing",
                format!("Tool Context {} 缺少 pair ID", item.context.id),
            )
        })?;
        let phase = item.tool_phase.ok_or_else(|| {
            CompactionError::new(
                "tool-phase-missing",
                format!("Tool Context {} 缺少 phase", item.context.id),
            )
        })?;
        pairs.entry(pair_id.clone()).or_default().insert(phase);
        if item.in_flight {
            in_flight.insert(pair_id.clone());
        }
    }
    for (pair_id, phases) in pairs {
        if !in_flight.contains(&pair_id)
            && (!phases.contains(&ToolPhase::Call) || !phases.contains(&ToolPhase::Result))
        {
            return Err(CompactionError::new(
                "incomplete-tool-pair",
                format!("Tool pair {pair_id} 不完整"),
            ));
        }
    }
    Ok(())
}

fn strictest_flow(items: &[CompactionItem]) -> InformationFlowLabel {
    let integrity = if items
        .iter()
        .any(|item| item.context.information_flow.integrity == IntegrityLabel::Untrusted)
    {
        IntegrityLabel::Untrusted
    } else {
        IntegrityLabel::Trusted
    };
    let confidentiality = if items.iter().any(|item| {
        item.context.information_flow.confidentiality == ConfidentialityLabel::UserSecret
    }) {
        ConfidentialityLabel::UserSecret
    } else if items.iter().any(|item| {
        item.context.information_flow.confidentiality == ConfidentialityLabel::ProjectPrivate
    }) {
        ConfidentialityLabel::ProjectPrivate
    } else {
        ConfidentialityLabel::Public
    };
    InformationFlowLabel {
        integrity,
        confidentiality,
    }
}

fn hash_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> ContentHash {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.len().to_string());
        hash.update(b":");
        hash.update(part.as_bytes());
        hash.update(b"\n");
    }
    ContentHash::from(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use harness_types::{ConfidentialityLabel, IntegrityLabel};

    use super::*;

    struct FakeSummary;

    impl SummaryProvider for FakeSummary {
        fn summarize(
            &self,
            items: &[CompactionItem],
            max_summary_tokens: u32,
        ) -> Result<StructuredSummary, CompactionError> {
            Ok(StructuredSummary {
                method: "fake".to_owned(),
                summary: format!(
                    "summarized:{}",
                    items
                        .iter()
                        .map(|item| item.context.id.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                token_cost: max_summary_tokens.min(10),
                confirmed_requirements: vec![],
                non_goals: vec![],
                decisions: vec![],
                history_digest: vec![],
                active_assumptions: vec![],
                unresolved_blockers: vec![],
                completed_actions: vec!["old work".to_owned()],
                modified_files: vec![],
                failed_approaches: vec![],
                evidence_refs: vec![],
                next_goal: "continue".to_owned(),
            })
        }
    }

    fn context(id: &str, kind: ContextKind, tokens: u32) -> ContextItem {
        ContextItem {
            id: ContextItemId::from(id),
            kind,
            priority: if kind == ContextKind::Goal {
                Priority::Critical
            } else {
                Priority::Medium
            },
            token_cost: tokens,
            source: id.to_owned(),
            timestamp_millis: i64::from(id.as_bytes()[0]),
            importance: 500,
            compressibility: if kind == ContextKind::Goal {
                Compressibility::Exact
            } else {
                Compressibility::Structured
            },
            ttl_millis: None,
            content_hash: ContentHash::from(format!("hash:{id}")),
            source_identity: id.to_owned(),
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality: ConfidentialityLabel::ProjectPrivate,
            },
            cache_class: CacheClass::DynamicTail,
            order: 0,
            hard_required: kind == ContextKind::Goal,
            content: id.to_owned(),
        }
    }

    fn checkpoint(series_id: &str) -> ContextCheckpoint {
        ContextCheckpoint {
            id: CheckpointId::from("checkpoint:1"),
            name: Some("test".to_owned()),
            session_id: SessionId::from("session:1"),
            context_series_id: ContextSeriesId::from(series_id),
            goal_revision_id: Some(GoalRevisionId::from("goal:1")),
            plan_revision: Some("plan:1".to_owned()),
            completed_tasks: vec![],
            pending_tasks: vec![TaskId::from("task:1")],
            decision_refs: vec![],
            constraint_refs: vec![],
            modified_file_refs: vec![],
            error_refs: vec![],
            memory_refs: vec![],
            prompt_fingerprint: ContentHash::from("prompt:1"),
            created_at_millis: 100,
        }
    }

    #[test]
    fn safe_compaction_retains_goal_tool_pair_error_and_recent() {
        let items = vec![
            CompactionItem {
                context: context("a-old", ContextKind::Conversation, 30),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
            CompactionItem {
                context: context("b-goal", ContextKind::Goal, 20),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
            CompactionItem {
                context: context("c-call", ContextKind::Tool, 10),
                pair_id: Some("tool:1".to_owned()),
                tool_phase: Some(ToolPhase::Call),
                in_flight: false,
            },
            CompactionItem {
                context: context("d-result", ContextKind::Tool, 20),
                pair_id: Some("tool:1".to_owned()),
                tool_phase: Some(ToolPhase::Result),
                in_flight: false,
            },
            CompactionItem {
                context: context("e-error", ContextKind::Error, 10),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
            CompactionItem {
                context: context("z-recent", ContextKind::Conversation, 10),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
        ];
        let result = ContextCompactor::new(FakeSummary)
            .compact(
                CompactionMode::Safe,
                items,
                1,
                15,
                ContextSeriesId::from("series:before"),
                Some(checkpoint("series:before")),
                200,
            )
            .expect("safe compaction");
        assert_eq!(
            result
                .record
                .source_item_ids
                .iter()
                .map(ContextItemId::as_str)
                .collect::<Vec<_>>(),
            vec!["a-old"]
        );
        for retained in ["b-goal", "c-call", "d-result", "e-error", "z-recent"] {
            assert!(
                result
                    .record
                    .retained_item_ids
                    .iter()
                    .any(|id| id.as_str() == retained)
            );
        }
        assert!(result.record.token_cost_after < result.record.token_cost_before);
        assert_eq!(
            result.record.checkpoint_id,
            Some(CheckpointId::from("checkpoint:1"))
        );
    }

    #[test]
    fn semantic_protection_retains_relevant_old_item_verbatim() {
        let protected_id = ContextItemId::from("a-protected");
        let items = vec![
            CompactionItem {
                context: context("a-protected", ContextKind::Conversation, 30),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
            CompactionItem {
                context: context("b-summary", ContextKind::Conversation, 30),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
            CompactionItem {
                context: context("z-recent", ContextKind::Conversation, 10),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
        ];
        let result = ContextCompactor::new(FakeSummary)
            .compact_with_protection(
                CompactionMode::Safe,
                items,
                1,
                15,
                ContextSeriesId::from("series:semantic"),
                Some(checkpoint("series:semantic")),
                200,
                &[protected_id.clone()].into_iter().collect(),
            )
            .expect("semantic compaction");
        assert!(
            result
                .visible_items
                .iter()
                .any(|item| item.context.id == protected_id)
        );
        assert_eq!(result.record.semantic_anchor_ids, vec![protected_id]);
    }

    #[test]
    fn incomplete_finished_tool_pair_is_rejected() {
        let result = ContextCompactor::new(FakeSummary).compact(
            CompactionMode::Safe,
            vec![CompactionItem {
                context: context("a-call", ContextKind::Tool, 10),
                pair_id: Some("tool:1".to_owned()),
                tool_phase: Some(ToolPhase::Call),
                in_flight: false,
            }],
            0,
            10,
            ContextSeriesId::from("series:1"),
            None,
            10,
        );
        assert_eq!(
            result.expect_err("incomplete pair").code,
            "incomplete-tool-pair"
        );
    }

    #[test]
    fn summary_that_increases_tokens_is_rejected() {
        let result = ContextCompactor::new(FakeSummary).compact(
            CompactionMode::Safe,
            vec![
                CompactionItem {
                    context: context("a-old", ContextKind::Conversation, 1),
                    pair_id: None,
                    tool_phase: None,
                    in_flight: false,
                },
                CompactionItem {
                    context: context("z-recent", ContextKind::Conversation, 1),
                    pair_id: None,
                    tool_phase: None,
                    in_flight: false,
                },
            ],
            1,
            10,
            ContextSeriesId::from("series:small"),
            Some(checkpoint("series:small")),
            10,
        );
        assert_eq!(
            result.expect_err("larger summary must fail").code,
            "compaction-not-beneficial"
        );
    }

    #[test]
    fn aggressive_mode_requires_checkpoint() {
        let result = ContextCompactor::new(FakeSummary).compact(
            CompactionMode::Aggressive,
            vec![CompactionItem {
                context: context("a-old", ContextKind::Conversation, 10),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            }],
            0,
            10,
            ContextSeriesId::from("series:1"),
            None,
            10,
        );
        assert_eq!(
            result.expect_err("checkpoint required").code,
            "checkpoint-required"
        );
    }

    #[test]
    fn safe_keeps_repository_and_memory_but_aggressive_can_summarize_them() {
        let items = vec![
            CompactionItem {
                context: context("a-repository", ContextKind::Repository, 30),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
            CompactionItem {
                context: context("b-memory", ContextKind::Memory, 30),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
            CompactionItem {
                context: context("z-recent", ContextKind::Conversation, 10),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            },
        ];
        let safe = ContextCompactor::new(FakeSummary).compact(
            CompactionMode::Safe,
            items.clone(),
            1,
            10,
            ContextSeriesId::from("series:safe"),
            Some(checkpoint("series:safe")),
            20,
        );
        assert_eq!(
            safe.expect_err("safe 无可压缩项").code,
            "no-safe-items-to-compact"
        );

        let aggressive = ContextCompactor::new(FakeSummary)
            .compact(
                CompactionMode::Aggressive,
                items,
                1,
                10,
                ContextSeriesId::from("series:aggressive"),
                Some(checkpoint("series:aggressive")),
                20,
            )
            .expect("aggressive");
        assert_eq!(
            aggressive
                .record
                .source_item_ids
                .iter()
                .map(ContextItemId::as_str)
                .collect::<Vec<_>>(),
            vec!["a-repository", "b-memory"]
        );
    }

    #[test]
    fn one_hundred_compaction_serialization_recoveries_preserve_goal() {
        for iteration in 0..100 {
            let previous = ContextSeriesId::from(format!("series:stress:{iteration}"));
            let mut items = vec![CompactionItem {
                context: context(&format!("goal-{iteration}"), ContextKind::Goal, 20),
                pair_id: None,
                tool_phase: None,
                in_flight: false,
            }];
            for turn in 0..6 {
                items.push(CompactionItem {
                    context: context(
                        &format!("turn-{iteration}-{turn}"),
                        ContextKind::Conversation,
                        100,
                    ),
                    pair_id: None,
                    tool_phase: None,
                    in_flight: false,
                });
            }
            let result = ContextCompactor::new(FakeSummary)
                .compact(
                    CompactionMode::Safe,
                    items,
                    2,
                    20,
                    previous.clone(),
                    Some(checkpoint(previous.as_str())),
                    1_000 + iteration,
                )
                .expect("stress compact");
            let encoded = serde_json::to_vec(&result).expect("serialize result");
            let recovered: CompactionResult =
                serde_json::from_slice(&encoded).expect("recover result");
            assert_eq!(recovered.record.previous_series_id, previous);
            assert!(
                recovered
                    .visible_items
                    .iter()
                    .any(|item| item.context.kind == ContextKind::Goal)
            );
            assert!(recovered.record.token_cost_after < recovered.record.token_cost_before);
        }
    }
}
