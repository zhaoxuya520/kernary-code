use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{
    ActorId, CheckpointId, GoalRevisionId, ModelId, ProjectId, ProviderId, ReasoningLevel,
    SessionId,
};
use serde::{Deserialize, Serialize};

/// Session 生命周期的 Stage 2 子集。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionStatus {
    #[default]
    New,
    Active,
    Suspended,
    Completed,
    Abandoned,
}

/// Goal 的不可变修订版。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalRevision {
    pub id: GoalRevisionId,
    pub parent_revision_id: Option<GoalRevisionId>,
    pub text: String,
    pub created_by: ActorId,
    pub reason: String,
    pub created_at_millis: i64,
}

/// 当前 Goal 指针、完整历史和锁定状态。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalState {
    pub current_revision_id: Option<GoalRevisionId>,
    pub revisions: BTreeMap<GoalRevisionId, GoalRevision>,
    pub locked: bool,
}

/// Session 持久化的显式 Model 选择；effective reasoning 由 capability 动态推导。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionModelState {
    pub provider_id: Option<ProviderId>,
    pub model_id: Option<ModelId>,
    pub reasoning: ReasoningLevel,
}

/// Session 聚合的最小状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionState {
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub status: SessionStatus,
    pub version: u64,
    pub goal: GoalState,
    #[serde(default)]
    pub model: SessionModelState,
    #[serde(default)]
    pub parent_session_id: Option<SessionId>,
    #[serde(default)]
    pub forked_from_checkpoint_id: Option<CheckpointId>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
}

impl SessionState {
    #[must_use]
    pub fn empty(session_id: SessionId) -> Self {
        Self {
            session_id,
            project_id: ProjectId::default(),
            status: SessionStatus::New,
            version: 0,
            goal: GoalState::default(),
            model: SessionModelState::default(),
            parent_session_id: None,
            forked_from_checkpoint_id: None,
            settings: BTreeMap::new(),
        }
    }
}

/// Session/Goal 的用户意图。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum SessionCommand {
    #[serde(rename = "session.create")]
    CreateSession {
        #[serde(rename = "sessionId")]
        session_id: SessionId,
        #[serde(rename = "projectId")]
        project_id: ProjectId,
    },
    #[serde(rename = "session.fork")]
    ForkSession {
        #[serde(rename = "childSessionId")]
        child_session_id: SessionId,
        #[serde(rename = "projectId")]
        project_id: ProjectId,
        #[serde(rename = "parentSessionId")]
        parent_session_id: SessionId,
        #[serde(rename = "checkpointId")]
        checkpoint_id: CheckpointId,
    },
    #[serde(rename = "goal.revise")]
    ReviseGoal { revision: GoalRevision },
    #[serde(rename = "goal.clear")]
    ClearGoal { reason: String },
    #[serde(rename = "goal.lock")]
    SetGoalLock { locked: bool },
    #[serde(rename = "model.selected")]
    SelectModel {
        #[serde(rename = "providerId")]
        provider_id: ProviderId,
        #[serde(rename = "modelId")]
        model_id: ModelId,
    },
    #[serde(rename = "reasoning.selected")]
    SetReasoning { reasoning: ReasoningLevel },
    #[serde(rename = "session.setting-set")]
    SetSetting { key: String, value: String },
    #[serde(rename = "session.setting-clear")]
    ClearSetting { key: String },
}

/// Session 已发生的事实。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum SessionEvent {
    #[serde(rename = "session.created")]
    SessionCreated {
        #[serde(rename = "sessionId")]
        session_id: SessionId,
        #[serde(rename = "projectId")]
        project_id: ProjectId,
    },
    #[serde(rename = "session.forked")]
    SessionForked {
        #[serde(rename = "childSessionId")]
        child_session_id: SessionId,
        #[serde(rename = "projectId")]
        project_id: ProjectId,
        #[serde(rename = "parentSessionId")]
        parent_session_id: SessionId,
        #[serde(rename = "checkpointId")]
        checkpoint_id: CheckpointId,
    },
    #[serde(rename = "goal.revised")]
    GoalRevised { revision: GoalRevision },
    #[serde(rename = "goal.cleared")]
    GoalCleared { reason: String },
    #[serde(rename = "goal.lock-changed")]
    GoalLockChanged { locked: bool },
    #[serde(rename = "model.selection-changed")]
    ModelSelectionChanged {
        #[serde(rename = "providerId")]
        provider_id: ProviderId,
        #[serde(rename = "modelId")]
        model_id: ModelId,
    },
    #[serde(rename = "reasoning.selection-changed")]
    ReasoningSelectionChanged { reasoning: ReasoningLevel },
    #[serde(rename = "session.setting-changed")]
    SessionSettingChanged { key: String, value: String },
    #[serde(rename = "session.setting-cleared")]
    SessionSettingCleared { key: String },
}

/// 带聚合版本的 Session Event。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionVersionedEvent {
    pub aggregate_version: u64,
    pub event: SessionEvent,
}

/// Session Command 验证错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCommandError {
    pub code: &'static str,
    pub message: String,
}

impl SessionCommandError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Display for SessionCommandError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for SessionCommandError {}

/// 纯函数地验证 Session Command 并产生 Event。
pub fn decide_session(
    state: &SessionState,
    command: &SessionCommand,
) -> Result<Vec<SessionEvent>, SessionCommandError> {
    match command {
        SessionCommand::CreateSession {
            session_id,
            project_id,
        } => {
            if state.version != 0 || !state.project_id.as_str().is_empty() {
                return Err(SessionCommandError::new(
                    "session-exists",
                    "Session 已经创建",
                ));
            }
            Ok(vec![SessionEvent::SessionCreated {
                session_id: session_id.clone(),
                project_id: project_id.clone(),
            }])
        }
        SessionCommand::ForkSession {
            child_session_id,
            project_id,
            parent_session_id,
            checkpoint_id,
        } => {
            if state.version != 0 || !state.project_id.as_str().is_empty() {
                return Err(SessionCommandError::new(
                    "session-exists",
                    "Child Session 已经创建",
                ));
            }
            if child_session_id == parent_session_id {
                return Err(SessionCommandError::new(
                    "session-self-fork",
                    "Child Session 不能与 Parent 相同",
                ));
            }
            Ok(vec![SessionEvent::SessionForked {
                child_session_id: child_session_id.clone(),
                project_id: project_id.clone(),
                parent_session_id: parent_session_id.clone(),
                checkpoint_id: checkpoint_id.clone(),
            }])
        }
        SessionCommand::ReviseGoal { revision } => {
            if state.status == SessionStatus::New {
                return Err(SessionCommandError::new(
                    "session-not-created",
                    "请先创建 Session",
                ));
            }
            if state.goal.locked {
                return Err(SessionCommandError::new("goal-locked", "Goal 已锁定"));
            }
            if revision.text.trim().is_empty() {
                return Err(SessionCommandError::new("empty-goal", "Goal 不能为空"));
            }
            if state.goal.revisions.contains_key(&revision.id) {
                return Err(SessionCommandError::new(
                    "goal-revision-exists",
                    "Goal revision ID 已存在",
                ));
            }
            if revision.parent_revision_id != state.goal.current_revision_id {
                return Err(SessionCommandError::new(
                    "goal-parent-mismatch",
                    "Goal revision parent 不是当前 revision",
                ));
            }
            Ok(vec![SessionEvent::GoalRevised {
                revision: revision.clone(),
            }])
        }
        SessionCommand::ClearGoal { reason } => {
            if state.goal.locked {
                return Err(SessionCommandError::new("goal-locked", "Goal 已锁定"));
            }
            if state.goal.current_revision_id.is_none() {
                return Err(SessionCommandError::new("goal-missing", "当前没有 Goal"));
            }
            Ok(vec![SessionEvent::GoalCleared {
                reason: reason.clone(),
            }])
        }
        SessionCommand::SetGoalLock { locked } => {
            if *locked && state.goal.current_revision_id.is_none() {
                return Err(SessionCommandError::new(
                    "goal-missing",
                    "没有 Goal 时不能锁定",
                ));
            }
            if state.goal.locked == *locked {
                return Err(SessionCommandError::new(
                    "goal-lock-unchanged",
                    "Goal lock 状态没有变化",
                ));
            }
            Ok(vec![SessionEvent::GoalLockChanged { locked: *locked }])
        }
        SessionCommand::SelectModel {
            provider_id,
            model_id,
        } => {
            if state.status == SessionStatus::New {
                return Err(SessionCommandError::new(
                    "session-not-created",
                    "请先创建 Session",
                ));
            }
            if provider_id.as_str().trim().is_empty() || model_id.as_str().trim().is_empty() {
                return Err(SessionCommandError::new(
                    "model-selection-empty",
                    "Provider/Model 不能为空",
                ));
            }
            if state.model.provider_id.as_ref() == Some(provider_id)
                && state.model.model_id.as_ref() == Some(model_id)
            {
                return Err(SessionCommandError::new(
                    "model-selection-unchanged",
                    "Model selection 没有变化",
                ));
            }
            Ok(vec![SessionEvent::ModelSelectionChanged {
                provider_id: provider_id.clone(),
                model_id: model_id.clone(),
            }])
        }
        SessionCommand::SetReasoning { reasoning } => {
            if state.status == SessionStatus::New {
                return Err(SessionCommandError::new(
                    "session-not-created",
                    "请先创建 Session",
                ));
            }
            if state.model.reasoning == *reasoning {
                return Err(SessionCommandError::new(
                    "reasoning-unchanged",
                    "Reasoning selection 没有变化",
                ));
            }
            Ok(vec![SessionEvent::ReasoningSelectionChanged {
                reasoning: *reasoning,
            }])
        }
        SessionCommand::SetSetting { key, value } => {
            if state.status == SessionStatus::New {
                return Err(SessionCommandError::new(
                    "session-not-created",
                    "请先创建 Session",
                ));
            }
            if !valid_setting_key(key) || value.len() > 1024 {
                return Err(SessionCommandError::new("session-setting-invalid", key));
            }
            if state.settings.get(key) == Some(value) {
                return Err(SessionCommandError::new("session-setting-unchanged", key));
            }
            Ok(vec![SessionEvent::SessionSettingChanged {
                key: key.clone(),
                value: value.clone(),
            }])
        }
        SessionCommand::ClearSetting { key } => {
            if !state.settings.contains_key(key) {
                return Err(SessionCommandError::new("session-setting-missing", key));
            }
            Ok(vec![SessionEvent::SessionSettingCleared {
                key: key.clone(),
            }])
        }
    }
}

/// 纯函数地应用 Session Event。
pub fn reduce_session(
    state: &SessionState,
    event: &SessionEvent,
) -> Result<SessionState, SessionCommandError> {
    let mut next = state.clone();
    next.version += 1;
    match event {
        SessionEvent::SessionCreated {
            session_id,
            project_id,
        } => {
            next.session_id = session_id.clone();
            next.project_id = project_id.clone();
            next.status = SessionStatus::Active;
        }
        SessionEvent::SessionForked {
            child_session_id,
            project_id,
            parent_session_id,
            checkpoint_id,
        } => {
            next.session_id = child_session_id.clone();
            next.project_id = project_id.clone();
            next.status = SessionStatus::Active;
            next.parent_session_id = Some(parent_session_id.clone());
            next.forked_from_checkpoint_id = Some(checkpoint_id.clone());
        }
        SessionEvent::GoalRevised { revision } => {
            if next.goal.revisions.contains_key(&revision.id) {
                return Err(SessionCommandError::new(
                    "goal-revision-exists",
                    "重复 Goal revision event",
                ));
            }
            next.goal
                .revisions
                .insert(revision.id.clone(), revision.clone());
            next.goal.current_revision_id = Some(revision.id.clone());
        }
        SessionEvent::GoalCleared { .. } => {
            next.goal.current_revision_id = None;
        }
        SessionEvent::GoalLockChanged { locked } => {
            next.goal.locked = *locked;
        }
        SessionEvent::ModelSelectionChanged {
            provider_id,
            model_id,
        } => {
            next.model.provider_id = Some(provider_id.clone());
            next.model.model_id = Some(model_id.clone());
        }
        SessionEvent::ReasoningSelectionChanged { reasoning } => {
            next.model.reasoning = *reasoning;
        }
        SessionEvent::SessionSettingChanged { key, value } => {
            next.settings.insert(key.clone(), value.clone());
        }
        SessionEvent::SessionSettingCleared { key } => {
            next.settings.remove(key);
        }
    }
    Ok(next)
}

fn valid_setting_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'-' | b'_'))
        })
}

/// 重放 Session Event，并检查版本连续性。
pub fn replay_session(
    session_id: SessionId,
    events: &[SessionVersionedEvent],
) -> Result<SessionState, SessionCommandError> {
    let mut state = SessionState::empty(session_id);
    for versioned in events {
        let expected = state.version + 1;
        if versioned.aggregate_version != expected {
            return Err(SessionCommandError::new(
                "event-version-mismatch",
                format!(
                    "Session event 版本不连续：expected={expected}, actual={}",
                    versioned.aggregate_version
                ),
            ));
        }
        state = reduce_session(&state, &versioned.event)?;
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use harness_types::{ActorId, GoalRevisionId};

    use super::*;

    fn revision(id: &str, parent: Option<&str>, text: &str) -> GoalRevision {
        GoalRevision {
            id: GoalRevisionId::from(id),
            parent_revision_id: parent.map(GoalRevisionId::from),
            text: text.to_owned(),
            created_by: ActorId::from("user:test"),
            reason: "test".to_owned(),
            created_at_millis: 42,
        }
    }

    fn apply(state: &mut SessionState, command: SessionCommand) -> Result<(), SessionCommandError> {
        for event in decide_session(state, &command)? {
            *state = reduce_session(state, &event)?;
        }
        Ok(())
    }

    #[test]
    fn locked_goal_rejects_revision_and_history_is_preserved() {
        let mut state = SessionState::empty(SessionId::from("session:test"));
        apply(
            &mut state,
            SessionCommand::CreateSession {
                session_id: SessionId::from("session:test"),
                project_id: ProjectId::from("project:test"),
            },
        )
        .expect("Session 应创建");
        apply(
            &mut state,
            SessionCommand::ReviseGoal {
                revision: revision("goal:1", None, "第一版"),
            },
        )
        .expect("第一版 Goal 应接受");
        apply(&mut state, SessionCommand::SetGoalLock { locked: true }).expect("Goal 应锁定");
        let error = apply(
            &mut state,
            SessionCommand::ReviseGoal {
                revision: revision("goal:2", Some("goal:1"), "第二版"),
            },
        )
        .expect_err("锁定 Goal 必须拒绝修订");
        assert_eq!(error.code, "goal-locked");
        apply(&mut state, SessionCommand::SetGoalLock { locked: false }).expect("用户应可解锁");
        apply(
            &mut state,
            SessionCommand::ReviseGoal {
                revision: revision("goal:2", Some("goal:1"), "第二版"),
            },
        )
        .expect("解锁后应可修订");
        assert_eq!(state.goal.revisions.len(), 2);
        assert_eq!(
            state.goal.current_revision_id,
            Some(GoalRevisionId::from("goal:2"))
        );
    }

    #[test]
    fn session_settings_are_versioned_durable_and_strict() {
        let mut state = SessionState::empty(SessionId::from("session:settings"));
        apply(
            &mut state,
            SessionCommand::CreateSession {
                session_id: SessionId::from("session:settings"),
                project_id: ProjectId::from("project:test"),
            },
        )
        .expect("create");
        apply(
            &mut state,
            SessionCommand::SetSetting {
                key: "mode".to_owned(),
                value: "full".to_owned(),
            },
        )
        .expect("set");
        assert_eq!(state.settings.get("mode").map(String::as_str), Some("full"));
        assert_eq!(
            apply(
                &mut state,
                SessionCommand::SetSetting {
                    key: "INVALID KEY".to_owned(),
                    value: "x".to_owned(),
                },
            )
            .expect_err("invalid key")
            .code,
            "session-setting-invalid"
        );
        apply(
            &mut state,
            SessionCommand::ClearSetting {
                key: "mode".to_owned(),
            },
        )
        .expect("clear");
        assert!(!state.settings.contains_key("mode"));
    }

    #[test]
    fn fork_builds_child_without_mutating_parent() {
        let mut parent = SessionState::empty(SessionId::from("session:parent"));
        apply(
            &mut parent,
            SessionCommand::CreateSession {
                session_id: SessionId::from("session:parent"),
                project_id: ProjectId::from("project:test"),
            },
        )
        .expect("parent");
        apply(
            &mut parent,
            SessionCommand::ReviseGoal {
                revision: revision("goal:1", None, "parent goal"),
            },
        )
        .expect("parent goal");
        let parent_before = parent.clone();

        let mut child = SessionState::empty(SessionId::from("session:child"));
        apply(
            &mut child,
            SessionCommand::ForkSession {
                child_session_id: SessionId::from("session:child"),
                project_id: ProjectId::from("project:test"),
                parent_session_id: parent.session_id.clone(),
                checkpoint_id: CheckpointId::from("checkpoint:1"),
            },
        )
        .expect("child fork");
        assert_eq!(parent, parent_before);
        assert_eq!(child.parent_session_id, Some(parent.session_id));
        assert_eq!(
            child.forked_from_checkpoint_id,
            Some(CheckpointId::from("checkpoint:1"))
        );
    }

    #[test]
    fn model_and_reasoning_are_durable_session_state() {
        let mut state = SessionState::empty(SessionId::from("session:model"));
        apply(
            &mut state,
            SessionCommand::CreateSession {
                session_id: SessionId::from("session:model"),
                project_id: ProjectId::from("project:test"),
            },
        )
        .expect("session");
        apply(
            &mut state,
            SessionCommand::SelectModel {
                provider_id: ProviderId::from("openai"),
                model_id: ModelId::from("gpt-test"),
            },
        )
        .expect("model");
        apply(
            &mut state,
            SessionCommand::SetReasoning {
                reasoning: ReasoningLevel::High,
            },
        )
        .expect("reasoning");
        assert_eq!(state.model.provider_id, Some(ProviderId::from("openai")));
        assert_eq!(state.model.model_id, Some(ModelId::from("gpt-test")));
        assert_eq!(state.model.reasoning, ReasoningLevel::High);
    }
}
