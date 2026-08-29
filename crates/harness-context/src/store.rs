use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{CheckpointId, ContextSeriesId, SessionId};
use serde::{Deserialize, Serialize};

use crate::{CompactionItem, CompactionRecord, ContextCheckpoint};

/// 一条不可变的 Context 可见快照。
///
/// Compact、Rollback、Fork 都创建新 Series；已经持久化的 Series 永不原地修改。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextSeries {
    pub id: ContextSeriesId,
    pub session_id: SessionId,
    pub parent_series_id: Option<ContextSeriesId>,
    pub restored_from_checkpoint_id: Option<CheckpointId>,
    pub items: Vec<CompactionItem>,
    pub created_at_millis: i64,
}

impl ContextSeries {
    #[must_use]
    pub fn initial(id: ContextSeriesId, session_id: SessionId, created_at_millis: i64) -> Self {
        Self {
            id,
            session_id,
            parent_series_id: None,
            restored_from_checkpoint_id: None,
            items: Vec::new(),
            created_at_millis,
        }
    }
}

/// 一次活动 Context 指针的原子 CAS 迁移。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextTransition {
    pub expected_active_series_id: Option<ContextSeriesId>,
    pub next_series: ContextSeries,
    pub compaction_record: Option<CompactionRecord>,
}

/// Context 持久化 Port 的稳定错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextStoreError {
    pub code: String,
    pub message: String,
}

impl ContextStoreError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for ContextStoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ContextStoreError {}

/// Context/Application 消费的持久化 Port。
pub trait ContextStore: Send + Sync {
    fn load_active_context_series(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<ContextSeries>, ContextStoreError>;

    fn load_context_series(
        &self,
        series_id: &ContextSeriesId,
    ) -> Result<Option<ContextSeries>, ContextStoreError>;

    fn commit_context_transition(
        &self,
        transition: ContextTransition,
    ) -> Result<(), ContextStoreError>;

    fn save_context_checkpoint(
        &self,
        expected_active_series_id: &ContextSeriesId,
        checkpoint: ContextCheckpoint,
    ) -> Result<(), ContextStoreError>;

    fn load_context_checkpoint(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> Result<Option<ContextCheckpoint>, ContextStoreError>;

    fn list_context_checkpoints(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ContextCheckpoint>, ContextStoreError>;
}

/// 从 checkpoint 对应的不可变快照创建子 Session 的第一条 Context Series。
///
/// 输入按值借用且返回新对象，因此 Parent Series 不可能被此纯函数修改。
pub fn fork_context_series(
    parent: &ContextSeries,
    checkpoint: &ContextCheckpoint,
    child_session_id: SessionId,
    child_series_id: ContextSeriesId,
    created_at_millis: i64,
) -> Result<ContextSeries, ContextStoreError> {
    if parent.id != checkpoint.context_series_id {
        return Err(ContextStoreError::new(
            "checkpoint-series-mismatch",
            "Checkpoint 没有引用给定的 Parent Context Series",
        ));
    }
    if parent.session_id != checkpoint.session_id {
        return Err(ContextStoreError::new(
            "checkpoint-session-mismatch",
            "Checkpoint 与 Parent Session 不一致",
        ));
    }
    Ok(ContextSeries {
        id: child_series_id,
        session_id: child_session_id,
        parent_series_id: Some(parent.id.clone()),
        restored_from_checkpoint_id: Some(checkpoint.id.clone()),
        items: parent.items.clone(),
        created_at_millis,
    })
}

#[cfg(test)]
mod tests {
    use harness_types::{ContentHash, GoalRevisionId};

    use super::*;

    fn checkpoint() -> ContextCheckpoint {
        ContextCheckpoint {
            id: CheckpointId::from("checkpoint:1"),
            name: Some("fork-point".to_owned()),
            session_id: SessionId::from("session:parent"),
            context_series_id: ContextSeriesId::from("series:parent"),
            goal_revision_id: Some(GoalRevisionId::from("goal:1")),
            plan_revision: None,
            completed_tasks: vec![],
            pending_tasks: vec![],
            decision_refs: vec![],
            constraint_refs: vec![],
            modified_file_refs: vec![],
            error_refs: vec![],
            memory_refs: vec![],
            prompt_fingerprint: ContentHash::from("prompt:1"),
            created_at_millis: 10,
        }
    }

    #[test]
    fn fork_creates_child_without_mutating_parent() {
        let parent = ContextSeries::initial(
            ContextSeriesId::from("series:parent"),
            SessionId::from("session:parent"),
            1,
        );
        let before = parent.clone();
        let child = fork_context_series(
            &parent,
            &checkpoint(),
            SessionId::from("session:child"),
            ContextSeriesId::from("series:child"),
            20,
        )
        .expect("fork");

        assert_eq!(parent, before);
        assert_eq!(child.parent_series_id, Some(parent.id.clone()));
        assert_eq!(
            child.restored_from_checkpoint_id,
            Some(CheckpointId::from("checkpoint:1"))
        );
        assert_eq!(child.session_id, SessionId::from("session:child"));
    }
}
