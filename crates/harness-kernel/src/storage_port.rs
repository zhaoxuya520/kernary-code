use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{ClaimToken, EffectId, JsonValue, MissionId, RunId, SessionId};
use serde::{Deserialize, Serialize};

use crate::{
    CompletionFence, DomainEvent, EffectClaim, EffectIntent, MissionEpoch, MissionState, RunFence,
    SessionEvent, SessionState,
};

/// Outbox 中一个 Effect 的持久化状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutboxStatus {
    Pending,
    Claimed,
    Completed,
    Failed,
    Uncertain,
}

/// 与 Domain Event 同事务写入的新 Effect。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewEffect {
    pub effect_id: EffectId,
    pub intent: EffectIntent,
    pub mission_epoch: MissionEpoch,
    pub run_id: Option<RunId>,
    pub run_fence: Option<RunFence>,
}

/// 可供 Runner 观察的完整 Outbox 项。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutboxEntry {
    pub effect_id: EffectId,
    pub mission_id: MissionId,
    pub aggregate_version: u64,
    pub intent: EffectIntent,
    pub status: OutboxStatus,
    pub mission_epoch: MissionEpoch,
    pub run_id: Option<RunId>,
    pub run_fence: Option<RunFence>,
    pub claim_token: Option<ClaimToken>,
    pub attempt: u32,
    pub lease_expires_at_millis: Option<i64>,
    pub created_at_millis: i64,
    pub updated_at_millis: i64,
}

/// Runner 成功领取的 Effect 和执行权证明。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimedEffect {
    pub claim: EffectClaim,
    pub intent: EffectIntent,
}

/// 外部副作用的最终/不确定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectOutcome {
    Completed,
    Failed,
    Uncertain,
}

/// Runtime 提交的 Effect completion。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectCompletion {
    pub fence: CompletionFence,
    pub outcome: EffectOutcome,
    pub result: Option<JsonValue>,
    pub error: Option<String>,
    pub recorded_at_millis: i64,
}

/// Effect result journal 中的记录。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectResultRecord {
    pub effect_id: EffectId,
    pub outcome: EffectOutcome,
    pub result: Option<JsonValue>,
    pub error: Option<String>,
    pub recorded_at_millis: i64,
}

/// 已持久化的 Mission Event。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredMissionEvent {
    pub sequence: u64,
    pub aggregate_version: u64,
    pub event: DomainEvent,
    pub recorded_at_millis: i64,
}

/// 已持久化的 Session Event。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredSessionEvent {
    pub sequence: u64,
    pub aggregate_version: u64,
    pub event: SessionEvent,
    pub recorded_at_millis: i64,
}

/// Mission snapshot 只是 replay 加速器，Event Store 仍是事实源。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MissionSnapshot {
    pub state: MissionState,
    pub created_at_millis: i64,
}

/// Session snapshot。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSnapshot {
    pub state: SessionState,
    pub created_at_millis: i64,
}

/// 原子提交后的版本和新序号。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommitReceipt {
    pub aggregate_version: u64,
    pub event_sequences: Vec<u64>,
    pub effect_ids: Vec<EffectId>,
}

/// Storage Adapter 在 Port 边界返回的稳定错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoragePortError {
    pub code: String,
    pub message: String,
}

impl StoragePortError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for StoragePortError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for StoragePortError {}

/// Kernel/Application 消费的持久化 Port。
pub trait KernelStore: Send + Sync {
    fn load_mission_events(
        &self,
        mission_id: &MissionId,
        after_version: u64,
    ) -> Result<Vec<StoredMissionEvent>, StoragePortError>;

    fn commit_mission(
        &self,
        mission_id: &MissionId,
        expected_version: u64,
        events: Vec<DomainEvent>,
        effects: Vec<NewEffect>,
        recorded_at_millis: i64,
    ) -> Result<CommitReceipt, StoragePortError>;

    fn load_session_events(
        &self,
        session_id: &SessionId,
        after_version: u64,
    ) -> Result<Vec<StoredSessionEvent>, StoragePortError>;

    fn list_session_ids(&self) -> Result<Vec<SessionId>, StoragePortError>;

    fn commit_session(
        &self,
        session_id: &SessionId,
        expected_version: u64,
        events: Vec<SessionEvent>,
        recorded_at_millis: i64,
    ) -> Result<CommitReceipt, StoragePortError>;

    fn save_mission_snapshot(&self, snapshot: MissionSnapshot) -> Result<(), StoragePortError>;

    fn load_mission_snapshot(
        &self,
        mission_id: &MissionId,
    ) -> Result<Option<MissionSnapshot>, StoragePortError>;

    fn save_session_snapshot(&self, snapshot: SessionSnapshot) -> Result<(), StoragePortError>;

    fn load_session_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSnapshot>, StoragePortError>;

    fn list_claimable_effects(
        &self,
        now_millis: i64,
        limit: usize,
    ) -> Result<Vec<OutboxEntry>, StoragePortError>;

    fn try_claim_effect(
        &self,
        effect_id: &EffectId,
        claim_token: ClaimToken,
        now_millis: i64,
        lease_expires_at_millis: i64,
    ) -> Result<Option<ClaimedEffect>, StoragePortError>;

    fn complete_effect(&self, completion: EffectCompletion) -> Result<(), StoragePortError>;

    fn load_effect_result(
        &self,
        effect_id: &EffectId,
    ) -> Result<Option<EffectResultRecord>, StoragePortError>;

    fn list_outbox(&self) -> Result<Vec<OutboxEntry>, StoragePortError>;
}
