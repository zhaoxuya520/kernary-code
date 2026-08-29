#![forbid(unsafe_code)]

//! Harness 的确定性 Kernel。
//!
//! Stage 1 只迁移纯 Mission Event/Reducer；没有 I/O、SQLite、Tokio、Model 或 TUI。

mod fencing;
mod mission;
mod mission_decision;
mod session;
mod storage_port;

pub use fencing::{
    CompletionFence, EffectClaim, FencingError, MissionEpoch, RunFence, validate_completion_fence,
};
pub use mission::{
    AgentRunState, AgentRunStatus, ApprovalDecision, ApprovalState, ApprovalStatus, DomainEvent,
    KernelError, MissionState, MissionStatus, NodeKind, NodeStatus, VersionedEvent,
    WorkflowNodeDefinition, WorkflowNodeState, find_ready_node_ids, reduce_mission, replay_mission,
};
pub use mission_decision::{
    CommandError, DecideResult, EffectIntent, MissionCommand, decide_mission,
};
pub use session::{
    GoalRevision, GoalState, SessionCommand, SessionCommandError, SessionEvent, SessionModelState,
    SessionState, SessionStatus, SessionVersionedEvent, decide_session, reduce_session,
    replay_session,
};
pub use storage_port::{
    ClaimedEffect, CommitReceipt, EffectCompletion, EffectOutcome, EffectResultRecord, KernelStore,
    MissionSnapshot, NewEffect, OutboxEntry, OutboxStatus, SessionSnapshot, StoragePortError,
    StoredMissionEvent, StoredSessionEvent,
};
