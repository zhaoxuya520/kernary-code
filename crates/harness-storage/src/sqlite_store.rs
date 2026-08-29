use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};

use harness_context::{
    ContextCheckpoint, ContextSeries, ContextStore, ContextStoreError, ContextTransition,
};
use harness_kernel::{
    ClaimedEffect, CommitReceipt, DomainEvent, EffectClaim, EffectCompletion, EffectOutcome,
    EffectResultRecord, KernelStore, MissionEpoch, MissionSnapshot, MissionState, NewEffect,
    OutboxEntry, OutboxStatus, RunFence, SessionEvent, SessionSnapshot, SessionState,
    StoragePortError, StoredMissionEvent, StoredSessionEvent, reduce_mission, reduce_session,
    validate_completion_fence,
};
use harness_tool::{
    ToolError, ToolInvocationJournal, ToolInvocationPatch, ToolInvocationRecord,
    ToolInvocationStatus,
};
use harness_types::{
    CheckpointId, ClaimToken, ContextSeriesId, EffectId, MissionId, RunId, SessionId,
    ToolInvocationId,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

const SCHEMA_VERSION: i64 = 3;

type StoreResult<T> = Result<T, StoragePortError>;
type Reply<T> = SyncSender<StoreResult<T>>;

enum StoreCommand {
    SchemaVersion {
        reply: Reply<u32>,
    },
    LoadMissionEvents {
        mission_id: MissionId,
        after_version: u64,
        reply: Reply<Vec<StoredMissionEvent>>,
    },
    CommitMission {
        mission_id: MissionId,
        expected_version: u64,
        events: Vec<DomainEvent>,
        effects: Vec<NewEffect>,
        recorded_at_millis: i64,
        reply: Reply<CommitReceipt>,
    },
    LoadSessionEvents {
        session_id: SessionId,
        after_version: u64,
        reply: Reply<Vec<StoredSessionEvent>>,
    },
    ListSessionIds {
        reply: Reply<Vec<SessionId>>,
    },
    CommitSession {
        session_id: SessionId,
        expected_version: u64,
        events: Vec<SessionEvent>,
        recorded_at_millis: i64,
        reply: Reply<CommitReceipt>,
    },
    SaveMissionSnapshot {
        snapshot: MissionSnapshot,
        reply: Reply<()>,
    },
    LoadMissionSnapshot {
        mission_id: MissionId,
        reply: Reply<Option<MissionSnapshot>>,
    },
    SaveSessionSnapshot {
        snapshot: SessionSnapshot,
        reply: Reply<()>,
    },
    LoadSessionSnapshot {
        session_id: SessionId,
        reply: Reply<Option<SessionSnapshot>>,
    },
    LoadActiveContextSeries {
        session_id: SessionId,
        reply: Reply<Option<ContextSeries>>,
    },
    LoadContextSeries {
        series_id: ContextSeriesId,
        reply: Reply<Option<ContextSeries>>,
    },
    CommitContextTransition {
        transition: ContextTransition,
        reply: Reply<()>,
    },
    SaveContextCheckpoint {
        expected_active_series_id: ContextSeriesId,
        checkpoint: ContextCheckpoint,
        reply: Reply<()>,
    },
    LoadContextCheckpoint {
        session_id: SessionId,
        checkpoint_id: CheckpointId,
        reply: Reply<Option<ContextCheckpoint>>,
    },
    ListContextCheckpoints {
        session_id: SessionId,
        reply: Reply<Vec<ContextCheckpoint>>,
    },
    CreateToolInvocation {
        record: ToolInvocationRecord,
        reply: Reply<()>,
    },
    UpdateToolInvocation {
        id: ToolInvocationId,
        patch: ToolInvocationPatch,
        reply: Reply<ToolInvocationRecord>,
    },
    GetToolInvocation {
        id: ToolInvocationId,
        reply: Reply<Option<ToolInvocationRecord>>,
    },
    FindToolInvocationByKey {
        key: String,
        reply: Reply<Option<ToolInvocationRecord>>,
    },
    ListToolInvocations {
        reply: Reply<Vec<ToolInvocationRecord>>,
    },
    ListClaimableEffects {
        now_millis: i64,
        limit: usize,
        reply: Reply<Vec<OutboxEntry>>,
    },
    TryClaimEffect {
        effect_id: EffectId,
        claim_token: ClaimToken,
        now_millis: i64,
        lease_expires_at_millis: i64,
        reply: Reply<Option<ClaimedEffect>>,
    },
    CompleteEffect {
        completion: EffectCompletion,
        reply: Reply<()>,
    },
    LoadEffectResult {
        effect_id: EffectId,
        reply: Reply<Option<EffectResultRecord>>,
    },
    ListOutbox {
        reply: Reply<Vec<OutboxEntry>>,
    },
    Shutdown,
}

/// 所有 SQLite I/O 都在专用线程中串行执行。
pub struct SqliteKernelStore {
    sender: Sender<StoreCommand>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SqliteKernelStore {
    /// 打开数据库、执行 migration，并等待 DB actor 就绪。
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("harness-sqlite".to_owned())
            .spawn(move || run_worker(path, receiver, ready_sender))
            .map_err(|error| StoragePortError::new("storage-thread-spawn", error.to_string()))?;

        ready_receiver
            .recv()
            .map_err(|_| StoragePortError::new("storage-thread-closed", "DB actor 启动前退出"))??;
        Ok(Self {
            sender,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// 为只读诊断和契约检查创建进程内数据库，不触碰项目目录。
    pub fn open_in_memory() -> StoreResult<Self> {
        Self::open(":memory:")
    }

    /// 当前数据库 schema version。
    pub fn schema_version(&self) -> StoreResult<u32> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::SchemaVersion { reply })?;
        receive(receiver)
    }

    /// 使用最新 snapshot + event tail 恢复 Mission。
    pub fn recover_mission(&self, mission_id: &MissionId) -> StoreResult<MissionState> {
        let snapshot = self.load_mission_snapshot(mission_id)?;
        let mut state = snapshot
            .map(|snapshot| snapshot.state)
            .unwrap_or_else(|| MissionState::empty(mission_id.clone()));
        for stored in self.load_mission_events(mission_id, state.version)? {
            let expected = state.version + 1;
            if stored.aggregate_version != expected {
                return Err(StoragePortError::new(
                    "event-version-mismatch",
                    format!(
                        "Mission tail 不连续：expected={expected}, actual={}",
                        stored.aggregate_version
                    ),
                ));
            }
            state = reduce_mission(&state, &stored.event)
                .map_err(|error| StoragePortError::new("mission-replay", error.to_string()))?;
        }
        Ok(state)
    }

    /// 使用最新 snapshot + event tail 恢复 Session。
    pub fn recover_session(&self, session_id: &SessionId) -> StoreResult<SessionState> {
        let snapshot = self.load_session_snapshot(session_id)?;
        let mut state = snapshot
            .map(|snapshot| snapshot.state)
            .unwrap_or_else(|| SessionState::empty(session_id.clone()));
        for stored in self.load_session_events(session_id, state.version)? {
            let expected = state.version + 1;
            if stored.aggregate_version != expected {
                return Err(StoragePortError::new(
                    "event-version-mismatch",
                    format!(
                        "Session tail 不连续：expected={expected}, actual={}",
                        stored.aggregate_version
                    ),
                ));
            }
            state = reduce_session(&state, &stored.event)
                .map_err(|error| StoragePortError::new("session-replay", error.to_string()))?;
        }
        Ok(state)
    }

    fn send(&self, command: StoreCommand) -> StoreResult<()> {
        self.sender
            .send(command)
            .map_err(|_| StoragePortError::new("storage-thread-closed", "DB actor 不再接收命令"))
    }
}

impl KernelStore for SqliteKernelStore {
    fn load_mission_events(
        &self,
        mission_id: &MissionId,
        after_version: u64,
    ) -> StoreResult<Vec<StoredMissionEvent>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::LoadMissionEvents {
            mission_id: mission_id.clone(),
            after_version,
            reply,
        })?;
        receive(receiver)
    }

    fn commit_mission(
        &self,
        mission_id: &MissionId,
        expected_version: u64,
        events: Vec<DomainEvent>,
        effects: Vec<NewEffect>,
        recorded_at_millis: i64,
    ) -> StoreResult<CommitReceipt> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::CommitMission {
            mission_id: mission_id.clone(),
            expected_version,
            events,
            effects,
            recorded_at_millis,
            reply,
        })?;
        receive(receiver)
    }

    fn load_session_events(
        &self,
        session_id: &SessionId,
        after_version: u64,
    ) -> StoreResult<Vec<StoredSessionEvent>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::LoadSessionEvents {
            session_id: session_id.clone(),
            after_version,
            reply,
        })?;
        receive(receiver)
    }

    fn list_session_ids(&self) -> StoreResult<Vec<SessionId>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::ListSessionIds { reply })?;
        receive(receiver)
    }

    fn commit_session(
        &self,
        session_id: &SessionId,
        expected_version: u64,
        events: Vec<SessionEvent>,
        recorded_at_millis: i64,
    ) -> StoreResult<CommitReceipt> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::CommitSession {
            session_id: session_id.clone(),
            expected_version,
            events,
            recorded_at_millis,
            reply,
        })?;
        receive(receiver)
    }

    fn save_mission_snapshot(&self, snapshot: MissionSnapshot) -> StoreResult<()> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::SaveMissionSnapshot { snapshot, reply })?;
        receive(receiver)
    }

    fn load_mission_snapshot(
        &self,
        mission_id: &MissionId,
    ) -> StoreResult<Option<MissionSnapshot>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::LoadMissionSnapshot {
            mission_id: mission_id.clone(),
            reply,
        })?;
        receive(receiver)
    }

    fn save_session_snapshot(&self, snapshot: SessionSnapshot) -> StoreResult<()> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::SaveSessionSnapshot { snapshot, reply })?;
        receive(receiver)
    }

    fn load_session_snapshot(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Option<SessionSnapshot>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::LoadSessionSnapshot {
            session_id: session_id.clone(),
            reply,
        })?;
        receive(receiver)
    }

    fn list_claimable_effects(
        &self,
        now_millis: i64,
        limit: usize,
    ) -> StoreResult<Vec<OutboxEntry>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::ListClaimableEffects {
            now_millis,
            limit,
            reply,
        })?;
        receive(receiver)
    }

    fn try_claim_effect(
        &self,
        effect_id: &EffectId,
        claim_token: ClaimToken,
        now_millis: i64,
        lease_expires_at_millis: i64,
    ) -> StoreResult<Option<ClaimedEffect>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::TryClaimEffect {
            effect_id: effect_id.clone(),
            claim_token,
            now_millis,
            lease_expires_at_millis,
            reply,
        })?;
        receive(receiver)
    }

    fn complete_effect(&self, completion: EffectCompletion) -> StoreResult<()> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::CompleteEffect { completion, reply })?;
        receive(receiver)
    }

    fn load_effect_result(&self, effect_id: &EffectId) -> StoreResult<Option<EffectResultRecord>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::LoadEffectResult {
            effect_id: effect_id.clone(),
            reply,
        })?;
        receive(receiver)
    }

    fn list_outbox(&self) -> StoreResult<Vec<OutboxEntry>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::ListOutbox { reply })?;
        receive(receiver)
    }
}

impl ContextStore for SqliteKernelStore {
    fn load_active_context_series(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<ContextSeries>, ContextStoreError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::LoadActiveContextSeries {
            session_id: session_id.clone(),
            reply,
        })
        .and_then(|()| receive(receiver))
        .map_err(context_store_error)
    }

    fn load_context_series(
        &self,
        series_id: &ContextSeriesId,
    ) -> Result<Option<ContextSeries>, ContextStoreError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::LoadContextSeries {
            series_id: series_id.clone(),
            reply,
        })
        .and_then(|()| receive(receiver))
        .map_err(context_store_error)
    }

    fn commit_context_transition(
        &self,
        transition: ContextTransition,
    ) -> Result<(), ContextStoreError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::CommitContextTransition { transition, reply })
            .and_then(|()| receive(receiver))
            .map_err(context_store_error)
    }

    fn save_context_checkpoint(
        &self,
        expected_active_series_id: &ContextSeriesId,
        checkpoint: ContextCheckpoint,
    ) -> Result<(), ContextStoreError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::SaveContextCheckpoint {
            expected_active_series_id: expected_active_series_id.clone(),
            checkpoint,
            reply,
        })
        .and_then(|()| receive(receiver))
        .map_err(context_store_error)
    }

    fn load_context_checkpoint(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> Result<Option<ContextCheckpoint>, ContextStoreError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::LoadContextCheckpoint {
            session_id: session_id.clone(),
            checkpoint_id: checkpoint_id.clone(),
            reply,
        })
        .and_then(|()| receive(receiver))
        .map_err(context_store_error)
    }

    fn list_context_checkpoints(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ContextCheckpoint>, ContextStoreError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::ListContextCheckpoints {
            session_id: session_id.clone(),
            reply,
        })
        .and_then(|()| receive(receiver))
        .map_err(context_store_error)
    }
}

impl ToolInvocationJournal for SqliteKernelStore {
    fn create(&self, record: ToolInvocationRecord) -> Result<(), ToolError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::CreateToolInvocation { record, reply })
            .and_then(|()| receive(receiver))
            .map_err(tool_store_error)
    }

    fn update(
        &self,
        id: &ToolInvocationId,
        patch: ToolInvocationPatch,
    ) -> Result<ToolInvocationRecord, ToolError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::UpdateToolInvocation {
            id: id.clone(),
            patch,
            reply,
        })
        .and_then(|()| receive(receiver))
        .map_err(tool_store_error)
    }

    fn get(&self, id: &ToolInvocationId) -> Result<Option<ToolInvocationRecord>, ToolError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::GetToolInvocation {
            id: id.clone(),
            reply,
        })
        .and_then(|()| receive(receiver))
        .map_err(tool_store_error)
    }

    fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<ToolInvocationRecord>, ToolError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::FindToolInvocationByKey {
            key: key.to_owned(),
            reply,
        })
        .and_then(|()| receive(receiver))
        .map_err(tool_store_error)
    }

    fn list(&self) -> Result<Vec<ToolInvocationRecord>, ToolError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(StoreCommand::ListToolInvocations { reply })
            .and_then(|()| receive(receiver))
            .map_err(tool_store_error)
    }
}

impl Drop for SqliteKernelStore {
    fn drop(&mut self) {
        let _ = self.sender.send(StoreCommand::Shutdown);
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn receive<T>(receiver: Receiver<StoreResult<T>>) -> StoreResult<T> {
    receiver
        .recv()
        .map_err(|_| StoragePortError::new("storage-thread-closed", "DB actor 未返回结果"))?
}

fn context_store_error(error: StoragePortError) -> ContextStoreError {
    ContextStoreError::new(error.code, error.message)
}

const fn tool_status_name(status: ToolInvocationStatus) -> &'static str {
    match status {
        ToolInvocationStatus::Requested => "requested",
        ToolInvocationStatus::WaitingApproval => "waiting-approval",
        ToolInvocationStatus::Running => "running",
        ToolInvocationStatus::Completed => "completed",
        ToolInvocationStatus::Denied => "denied",
        ToolInvocationStatus::Failed => "failed",
        ToolInvocationStatus::Uncertain => "uncertain",
    }
}

fn tool_store_error(error: StoragePortError) -> ToolError {
    ToolError::new(error.code, error.message)
}

fn run_worker(path: PathBuf, receiver: Receiver<StoreCommand>, ready: SyncSender<StoreResult<()>>) {
    let mut worker = match SqliteWorker::open(&path) {
        Ok(worker) => {
            let _ = ready.send(Ok(()));
            worker
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    while let Ok(command) = receiver.recv() {
        match command {
            StoreCommand::SchemaVersion { reply } => reply_result(reply, worker.schema_version()),
            StoreCommand::LoadMissionEvents {
                mission_id,
                after_version,
                reply,
            } => reply_result(
                reply,
                worker.load_mission_events(&mission_id, after_version),
            ),
            StoreCommand::CommitMission {
                mission_id,
                expected_version,
                events,
                effects,
                recorded_at_millis,
                reply,
            } => reply_result(
                reply,
                worker.commit_mission(
                    &mission_id,
                    expected_version,
                    events,
                    effects,
                    recorded_at_millis,
                ),
            ),
            StoreCommand::LoadSessionEvents {
                session_id,
                after_version,
                reply,
            } => reply_result(
                reply,
                worker.load_session_events(&session_id, after_version),
            ),
            StoreCommand::ListSessionIds { reply } => {
                reply_result(reply, worker.list_session_ids());
            }
            StoreCommand::CommitSession {
                session_id,
                expected_version,
                events,
                recorded_at_millis,
                reply,
            } => reply_result(
                reply,
                worker.commit_session(&session_id, expected_version, events, recorded_at_millis),
            ),
            StoreCommand::SaveMissionSnapshot { snapshot, reply } => {
                reply_result(reply, worker.save_mission_snapshot(snapshot));
            }
            StoreCommand::LoadMissionSnapshot { mission_id, reply } => {
                reply_result(reply, worker.load_mission_snapshot(&mission_id));
            }
            StoreCommand::SaveSessionSnapshot { snapshot, reply } => {
                reply_result(reply, worker.save_session_snapshot(snapshot));
            }
            StoreCommand::LoadSessionSnapshot { session_id, reply } => {
                reply_result(reply, worker.load_session_snapshot(&session_id));
            }
            StoreCommand::LoadActiveContextSeries { session_id, reply } => {
                reply_result(reply, worker.load_active_context_series(&session_id));
            }
            StoreCommand::LoadContextSeries { series_id, reply } => {
                reply_result(reply, worker.load_context_series(&series_id));
            }
            StoreCommand::CommitContextTransition { transition, reply } => {
                reply_result(reply, worker.commit_context_transition(transition));
            }
            StoreCommand::SaveContextCheckpoint {
                expected_active_series_id,
                checkpoint,
                reply,
            } => reply_result(
                reply,
                worker.save_context_checkpoint(&expected_active_series_id, checkpoint),
            ),
            StoreCommand::LoadContextCheckpoint {
                session_id,
                checkpoint_id,
                reply,
            } => reply_result(
                reply,
                worker.load_context_checkpoint(&session_id, &checkpoint_id),
            ),
            StoreCommand::ListContextCheckpoints { session_id, reply } => {
                reply_result(reply, worker.list_context_checkpoints(&session_id));
            }
            StoreCommand::CreateToolInvocation { record, reply } => {
                reply_result(reply, worker.create_tool_invocation(record));
            }
            StoreCommand::UpdateToolInvocation { id, patch, reply } => {
                reply_result(reply, worker.update_tool_invocation(&id, patch));
            }
            StoreCommand::GetToolInvocation { id, reply } => {
                reply_result(reply, worker.get_tool_invocation(&id));
            }
            StoreCommand::FindToolInvocationByKey { key, reply } => {
                reply_result(reply, worker.find_tool_invocation_by_key(&key));
            }
            StoreCommand::ListToolInvocations { reply } => {
                reply_result(reply, worker.list_tool_invocations());
            }
            StoreCommand::ListClaimableEffects {
                now_millis,
                limit,
                reply,
            } => reply_result(reply, worker.list_claimable_effects(now_millis, limit)),
            StoreCommand::TryClaimEffect {
                effect_id,
                claim_token,
                now_millis,
                lease_expires_at_millis,
                reply,
            } => reply_result(
                reply,
                worker.try_claim_effect(
                    &effect_id,
                    claim_token,
                    now_millis,
                    lease_expires_at_millis,
                ),
            ),
            StoreCommand::CompleteEffect { completion, reply } => {
                reply_result(reply, worker.complete_effect(completion));
            }
            StoreCommand::LoadEffectResult { effect_id, reply } => {
                reply_result(reply, worker.load_effect_result(&effect_id));
            }
            StoreCommand::ListOutbox { reply } => {
                reply_result(reply, worker.list_outbox());
            }
            StoreCommand::Shutdown => break,
        }
    }
}

fn reply_result<T>(reply: Reply<T>, result: StoreResult<T>) {
    let _ = reply.send(result);
}

struct SqliteWorker {
    connection: Connection,
}

impl SqliteWorker {
    fn open(path: &Path) -> StoreResult<Self> {
        let connection = Connection::open(path).map_err(|error| sqlite_error("open", error))?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;",
            )
            .map_err(|error| sqlite_error("pragmas", error))?;
        let mut worker = Self { connection };
        worker.migrate()?;
        Ok(worker)
    }

    fn migrate(&mut self) -> StoreResult<()> {
        let mut version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| sqlite_error("read-user-version", error))?;
        if version == 0 {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| sqlite_error("begin-migration", error))?;
            transaction
                .execute_batch(
                    "CREATE TABLE migration_history (
                             version INTEGER PRIMARY KEY,
                             name TEXT NOT NULL,
                             applied_at_millis INTEGER NOT NULL
                         );
                         CREATE TABLE aggregate_events (
                             sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                             aggregate_kind TEXT NOT NULL,
                             aggregate_id TEXT NOT NULL,
                             aggregate_version INTEGER NOT NULL,
                             event_type TEXT NOT NULL,
                             payload_json TEXT NOT NULL,
                             recorded_at_millis INTEGER NOT NULL,
                             UNIQUE (aggregate_kind, aggregate_id, aggregate_version)
                         );
                         CREATE INDEX idx_events_aggregate
                             ON aggregate_events (aggregate_kind, aggregate_id, aggregate_version);
                         CREATE TABLE aggregate_snapshots (
                             aggregate_kind TEXT NOT NULL,
                             aggregate_id TEXT NOT NULL,
                             aggregate_version INTEGER NOT NULL,
                             payload_json TEXT NOT NULL,
                             created_at_millis INTEGER NOT NULL,
                             PRIMARY KEY (aggregate_kind, aggregate_id)
                         );
                         CREATE TABLE outbox (
                             effect_id TEXT PRIMARY KEY,
                             mission_id TEXT NOT NULL,
                             aggregate_version INTEGER NOT NULL,
                             effect_kind TEXT NOT NULL,
                             payload_json TEXT NOT NULL,
                             status TEXT NOT NULL,
                             mission_epoch INTEGER NOT NULL,
                             run_id TEXT,
                             run_fence INTEGER,
                             claim_token TEXT,
                             attempt INTEGER NOT NULL DEFAULT 0,
                             lease_expires_at_millis INTEGER,
                             created_at_millis INTEGER NOT NULL,
                             updated_at_millis INTEGER NOT NULL
                         );
                         CREATE INDEX idx_outbox_claimable
                             ON outbox (status, lease_expires_at_millis, created_at_millis);
                         CREATE TABLE effect_results (
                             effect_id TEXT PRIMARY KEY,
                             outcome TEXT NOT NULL,
                             result_json TEXT,
                             error TEXT,
                             recorded_at_millis INTEGER NOT NULL,
                             FOREIGN KEY (effect_id) REFERENCES outbox(effect_id)
                         );",
                )
                .map_err(|error| sqlite_error("schema-v1", error))?;
            transaction
                .execute(
                    "INSERT INTO migration_history (version, name, applied_at_millis)
                         VALUES (1, 'initial-kernel-session-outbox', unixepoch('subsec') * 1000)",
                    [],
                )
                .map_err(|error| sqlite_error("record-migration-v1", error))?;
            transaction
                .execute_batch("PRAGMA user_version = 1;")
                .map_err(|error| sqlite_error("set-user-version", error))?;
            transaction
                .commit()
                .map_err(|error| sqlite_error("commit-migration", error))?;
            version = 1;
        }

        if version == 1 {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| sqlite_error("begin-context-migration", error))?;
            transaction
                .execute_batch(
                    "CREATE TABLE context_series (
                         series_id TEXT PRIMARY KEY,
                         session_id TEXT NOT NULL,
                         parent_series_id TEXT,
                         restored_from_checkpoint_id TEXT,
                         payload_json TEXT NOT NULL,
                         created_at_millis INTEGER NOT NULL,
                         FOREIGN KEY (parent_series_id) REFERENCES context_series(series_id)
                     );
                     CREATE INDEX idx_context_series_session
                         ON context_series (session_id, created_at_millis, series_id);
                     CREATE TABLE context_heads (
                         session_id TEXT PRIMARY KEY,
                         series_id TEXT NOT NULL,
                         updated_at_millis INTEGER NOT NULL,
                         FOREIGN KEY (series_id) REFERENCES context_series(series_id)
                     );
                     CREATE TABLE context_checkpoints (
                         checkpoint_id TEXT PRIMARY KEY,
                         session_id TEXT NOT NULL,
                         series_id TEXT NOT NULL,
                         payload_json TEXT NOT NULL,
                         created_at_millis INTEGER NOT NULL,
                         FOREIGN KEY (series_id) REFERENCES context_series(series_id)
                     );
                     CREATE INDEX idx_context_checkpoints_session
                         ON context_checkpoints (session_id, created_at_millis, checkpoint_id);
                     CREATE TABLE context_compactions (
                         next_series_id TEXT PRIMARY KEY,
                         previous_series_id TEXT NOT NULL,
                         checkpoint_id TEXT,
                         payload_json TEXT NOT NULL,
                         recorded_at_millis INTEGER NOT NULL,
                         FOREIGN KEY (next_series_id) REFERENCES context_series(series_id),
                         FOREIGN KEY (previous_series_id) REFERENCES context_series(series_id)
                     );",
                )
                .map_err(|error| sqlite_error("schema-v2", error))?;
            transaction
                .execute(
                    "INSERT INTO migration_history (version, name, applied_at_millis)
                     VALUES (2, 'context-series-checkpoint-compaction', unixepoch('subsec') * 1000)",
                    [],
                )
                .map_err(|error| sqlite_error("record-migration-v2", error))?;
            transaction
                .execute_batch("PRAGMA user_version = 2;")
                .map_err(|error| sqlite_error("set-user-version-v2", error))?;
            transaction
                .commit()
                .map_err(|error| sqlite_error("commit-context-migration", error))?;
            version = 2;
        }

        if version == 2 {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| sqlite_error("begin-tool-migration", error))?;
            transaction
                .execute_batch(
                    "CREATE TABLE tool_invocations (
                         id TEXT PRIMARY KEY,
                         idempotency_key TEXT NOT NULL UNIQUE,
                         project_id TEXT NOT NULL,
                         mission_id TEXT NOT NULL,
                         run_id TEXT,
                         tool_name TEXT NOT NULL,
                         status TEXT NOT NULL,
                         payload_json TEXT NOT NULL,
                         created_at_millis INTEGER NOT NULL,
                         updated_at_millis INTEGER NOT NULL
                     );
                     CREATE INDEX idx_tool_invocations_run_status
                         ON tool_invocations (run_id, status, updated_at_millis);",
                )
                .map_err(|error| sqlite_error("schema-v3", error))?;
            transaction
                .execute(
                    "INSERT INTO migration_history (version, name, applied_at_millis)
                     VALUES (3, 'tool-invocation-journal', unixepoch('subsec') * 1000)",
                    [],
                )
                .map_err(|error| sqlite_error("record-migration-v3", error))?;
            transaction
                .execute_batch("PRAGMA user_version = 3;")
                .map_err(|error| sqlite_error("set-user-version-v3", error))?;
            transaction
                .commit()
                .map_err(|error| sqlite_error("commit-tool-migration", error))?;
            version = 3;
        }

        if version != SCHEMA_VERSION {
            return Err(StoragePortError::new(
                "unsupported-schema-version",
                format!("数据库 schema={version}，当前支持={SCHEMA_VERSION}"),
            ));
        }
        Ok(())
    }

    fn schema_version(&self) -> StoreResult<u32> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| sqlite_error("read-user-version", error))?;
        u32::try_from(version)
            .map_err(|_| StoragePortError::new("invalid-schema-version", version.to_string()))
    }

    fn load_mission_events(
        &self,
        mission_id: &MissionId,
        after_version: u64,
    ) -> StoreResult<Vec<StoredMissionEvent>> {
        let raw = self.load_events("mission", mission_id.as_str(), after_version)?;
        raw.into_iter()
            .map(|raw| {
                Ok(StoredMissionEvent {
                    sequence: raw.sequence,
                    aggregate_version: raw.aggregate_version,
                    event: deserialize(&raw.payload_json, "mission-event")?,
                    recorded_at_millis: raw.recorded_at_millis,
                })
            })
            .collect()
    }

    fn list_session_ids(&self) -> StoreResult<Vec<SessionId>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT aggregate_id
                 FROM aggregate_events
                 WHERE aggregate_kind = 'session'
                 GROUP BY aggregate_id
                 ORDER BY MAX(recorded_at_millis) DESC, aggregate_id ASC",
            )
            .map_err(|error| sqlite_error("prepare-list-sessions", error))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| sqlite_error("query-list-sessions", error))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("read-list-sessions", error))
            .map(|ids| ids.into_iter().map(SessionId::from).collect())
    }

    fn load_session_events(
        &self,
        session_id: &SessionId,
        after_version: u64,
    ) -> StoreResult<Vec<StoredSessionEvent>> {
        let raw = self.load_events("session", session_id.as_str(), after_version)?;
        raw.into_iter()
            .map(|raw| {
                Ok(StoredSessionEvent {
                    sequence: raw.sequence,
                    aggregate_version: raw.aggregate_version,
                    event: deserialize(&raw.payload_json, "session-event")?,
                    recorded_at_millis: raw.recorded_at_millis,
                })
            })
            .collect()
    }

    fn load_events(
        &self,
        kind: &str,
        aggregate_id: &str,
        after_version: u64,
    ) -> StoreResult<Vec<NormalizedRawEvent>> {
        let after_version = to_i64(after_version, "aggregate-version")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence, aggregate_version, payload_json, recorded_at_millis
                 FROM aggregate_events
                 WHERE aggregate_kind = ?1 AND aggregate_id = ?2 AND aggregate_version > ?3
                 ORDER BY aggregate_version ASC",
            )
            .map_err(|error| sqlite_error("prepare-load-events", error))?;
        let rows = statement
            .query_map(params![kind, aggregate_id, after_version], |row| {
                Ok(RawEvent {
                    sequence: row.get(0)?,
                    aggregate_version: row.get(1)?,
                    payload_json: row.get(2)?,
                    recorded_at_millis: row.get(3)?,
                })
            })
            .map_err(|error| sqlite_error("query-load-events", error))?;
        let raw = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("read-load-events", error))?;
        raw.into_iter().map(RawEvent::try_normalize).collect()
    }

    fn commit_mission(
        &mut self,
        mission_id: &MissionId,
        expected_version: u64,
        events: Vec<DomainEvent>,
        effects: Vec<NewEffect>,
        recorded_at_millis: i64,
    ) -> StoreResult<CommitReceipt> {
        let serialized = events
            .iter()
            .map(|event| serialize_tagged(event, "mission-event"))
            .collect::<StoreResult<Vec<_>>>()?;
        self.commit_aggregate(
            "mission",
            mission_id.as_str(),
            expected_version,
            serialized,
            Some((mission_id, effects)),
            recorded_at_millis,
        )
    }

    fn commit_session(
        &mut self,
        session_id: &SessionId,
        expected_version: u64,
        events: Vec<SessionEvent>,
        recorded_at_millis: i64,
    ) -> StoreResult<CommitReceipt> {
        let serialized = events
            .iter()
            .map(|event| serialize_tagged(event, "session-event"))
            .collect::<StoreResult<Vec<_>>>()?;
        self.commit_aggregate(
            "session",
            session_id.as_str(),
            expected_version,
            serialized,
            None,
            recorded_at_millis,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_aggregate(
        &mut self,
        kind: &str,
        aggregate_id: &str,
        expected_version: u64,
        events: Vec<(String, String)>,
        mission_effects: Option<(&MissionId, Vec<NewEffect>)>,
        recorded_at_millis: i64,
    ) -> StoreResult<CommitReceipt> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin-commit", error))?;
        let actual_version = current_version(&transaction, kind, aggregate_id)?;
        if actual_version != expected_version {
            return Err(StoragePortError::new(
                "version-conflict",
                format!("expected={expected_version}, actual={actual_version}"),
            ));
        }

        let mut sequences = Vec::with_capacity(events.len());
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO aggregate_events (
                         aggregate_kind, aggregate_id, aggregate_version, event_type,
                         payload_json, recorded_at_millis
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(|error| sqlite_error("prepare-insert-event", error))?;
            for (index, (event_type, payload_json)) in events.iter().enumerate() {
                let version = expected_version
                    + u64::try_from(index)
                        .map_err(|_| StoragePortError::new("version-overflow", "事件索引溢出"))?
                    + 1;
                insert
                    .execute(params![
                        kind,
                        aggregate_id,
                        to_i64(version, "aggregate-version")?,
                        event_type,
                        payload_json,
                        recorded_at_millis
                    ])
                    .map_err(|error| sqlite_error("insert-event", error))?;
                sequences.push(to_u64(transaction.last_insert_rowid(), "event-sequence")?);
            }
        }

        let aggregate_version = expected_version
            + u64::try_from(events.len())
                .map_err(|_| StoragePortError::new("version-overflow", "事件数量溢出"))?;
        let mut effect_ids = Vec::new();
        if let Some((mission_id, effects)) = mission_effects {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO outbox (
                         effect_id, mission_id, aggregate_version, effect_kind, payload_json,
                         status, mission_epoch, run_id, run_fence, claim_token, attempt,
                         lease_expires_at_millis, created_at_millis, updated_at_millis
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, ?8, NULL, 0, NULL, ?9, ?9)",
                )
                .map_err(|error| sqlite_error("prepare-insert-effect", error))?;
            for effect in effects {
                let (effect_kind, payload_json) =
                    serialize_tagged(&effect.intent, "effect-intent")?;
                insert
                    .execute(params![
                        effect.effect_id.as_str(),
                        mission_id.as_str(),
                        to_i64(aggregate_version, "aggregate-version")?,
                        effect_kind,
                        payload_json,
                        to_i64(effect.mission_epoch.0, "mission-epoch")?,
                        effect.run_id.as_ref().map(RunId::as_str),
                        effect
                            .run_fence
                            .map(|fence| to_i64(fence.0, "run-fence"))
                            .transpose()?,
                        recorded_at_millis,
                    ])
                    .map_err(|error| sqlite_error("insert-effect", error))?;
                effect_ids.push(effect.effect_id);
            }
        }

        transaction
            .commit()
            .map_err(|error| sqlite_error("commit-events-outbox", error))?;
        Ok(CommitReceipt {
            aggregate_version,
            event_sequences: sequences,
            effect_ids,
        })
    }

    fn save_mission_snapshot(&mut self, snapshot: MissionSnapshot) -> StoreResult<()> {
        let id = snapshot.state.mission_id.to_string();
        let version = snapshot.state.version;
        self.save_snapshot(
            "mission",
            &id,
            version,
            &snapshot,
            snapshot.created_at_millis,
        )
    }

    fn save_session_snapshot(&mut self, snapshot: SessionSnapshot) -> StoreResult<()> {
        let id = snapshot.state.session_id.to_string();
        let version = snapshot.state.version;
        self.save_snapshot(
            "session",
            &id,
            version,
            &snapshot,
            snapshot.created_at_millis,
        )
    }

    fn save_snapshot<T: serde::Serialize>(
        &mut self,
        kind: &str,
        id: &str,
        version: u64,
        snapshot: &T,
        created_at_millis: i64,
    ) -> StoreResult<()> {
        let payload = serde_json::to_string(snapshot)
            .map_err(|error| schema_error("serialize-snapshot", error))?;
        self.connection
            .execute(
                "INSERT INTO aggregate_snapshots (
                     aggregate_kind, aggregate_id, aggregate_version, payload_json, created_at_millis
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(aggregate_kind, aggregate_id) DO UPDATE SET
                     aggregate_version = excluded.aggregate_version,
                     payload_json = excluded.payload_json,
                     created_at_millis = excluded.created_at_millis
                 WHERE excluded.aggregate_version >= aggregate_snapshots.aggregate_version",
                params![kind, id, to_i64(version, "snapshot-version")?, payload, created_at_millis],
            )
            .map_err(|error| sqlite_error("save-snapshot", error))?;
        Ok(())
    }

    fn load_mission_snapshot(
        &self,
        mission_id: &MissionId,
    ) -> StoreResult<Option<MissionSnapshot>> {
        self.load_snapshot("mission", mission_id.as_str())
    }

    fn load_session_snapshot(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Option<SessionSnapshot>> {
        self.load_snapshot("session", session_id.as_str())
    }

    fn load_snapshot<T: serde::de::DeserializeOwned>(
        &self,
        kind: &str,
        id: &str,
    ) -> StoreResult<Option<T>> {
        let payload: Option<String> = self
            .connection
            .query_row(
                "SELECT payload_json FROM aggregate_snapshots
                 WHERE aggregate_kind = ?1 AND aggregate_id = ?2",
                params![kind, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("load-snapshot", error))?;
        payload
            .map(|payload| deserialize(&payload, "snapshot"))
            .transpose()
    }

    fn load_active_context_series(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Option<ContextSeries>> {
        let payload: Option<String> = self
            .connection
            .query_row(
                "SELECT s.payload_json
                 FROM context_heads h
                 JOIN context_series s ON s.series_id = h.series_id
                 WHERE h.session_id = ?1",
                [session_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("load-active-context-series", error))?;
        payload
            .map(|payload| deserialize(&payload, "context-series"))
            .transpose()
    }

    fn load_context_series(
        &self,
        series_id: &ContextSeriesId,
    ) -> StoreResult<Option<ContextSeries>> {
        let payload: Option<String> = self
            .connection
            .query_row(
                "SELECT payload_json FROM context_series WHERE series_id = ?1",
                [series_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("load-context-series", error))?;
        payload
            .map(|payload| deserialize(&payload, "context-series"))
            .transpose()
    }

    fn commit_context_transition(&mut self, transition: ContextTransition) -> StoreResult<()> {
        let next = &transition.next_series;
        if let Some(expected) = &transition.expected_active_series_id
            && next.parent_series_id.as_ref() != Some(expected)
        {
            return Err(StoragePortError::new(
                "context-parent-mismatch",
                "新 Context Series 的 parent 必须是预期活动 Series",
            ));
        }
        if let Some(record) = &transition.compaction_record
            && (record.next_series_id != next.id
                || Some(&record.previous_series_id)
                    != transition.expected_active_series_id.as_ref())
        {
            return Err(StoragePortError::new(
                "compaction-lineage-mismatch",
                "Compaction record 与 Context transition lineage 不一致",
            ));
        }

        let series_json = serde_json::to_string(next)
            .map_err(|error| schema_error("serialize-context-series", error))?;
        let compaction_json = transition
            .compaction_record
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| schema_error("serialize-compaction-record", error))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin-context-transition", error))?;
        let actual: Option<String> = transaction
            .query_row(
                "SELECT series_id FROM context_heads WHERE session_id = ?1",
                [next.session_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("read-context-head", error))?;
        let expected = transition
            .expected_active_series_id
            .as_ref()
            .map(ContextSeriesId::as_str);
        if actual.as_deref() != expected {
            return Err(StoragePortError::new(
                "context-series-conflict",
                format!("expected={expected:?}, actual={:?}", actual.as_deref()),
            ));
        }
        transaction
            .execute(
                "INSERT INTO context_series (
                     series_id, session_id, parent_series_id, restored_from_checkpoint_id,
                     payload_json, created_at_millis
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    next.id.as_str(),
                    next.session_id.as_str(),
                    next.parent_series_id.as_ref().map(ContextSeriesId::as_str),
                    next.restored_from_checkpoint_id
                        .as_ref()
                        .map(CheckpointId::as_str),
                    series_json,
                    next.created_at_millis,
                ],
            )
            .map_err(|error| sqlite_error("insert-context-series", error))?;
        if let (Some(record), Some(payload)) =
            (transition.compaction_record.as_ref(), compaction_json)
        {
            transaction
                .execute(
                    "INSERT INTO context_compactions (
                         next_series_id, previous_series_id, checkpoint_id,
                         payload_json, recorded_at_millis
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        record.next_series_id.as_str(),
                        record.previous_series_id.as_str(),
                        record.checkpoint_id.as_ref().map(CheckpointId::as_str),
                        payload,
                        next.created_at_millis,
                    ],
                )
                .map_err(|error| sqlite_error("insert-compaction-record", error))?;
        }
        if actual.is_some() {
            let changed = transaction
                .execute(
                    "UPDATE context_heads
                     SET series_id = ?1, updated_at_millis = ?2
                     WHERE session_id = ?3 AND series_id = ?4",
                    params![
                        next.id.as_str(),
                        next.created_at_millis,
                        next.session_id.as_str(),
                        expected,
                    ],
                )
                .map_err(|error| sqlite_error("update-context-head", error))?;
            if changed != 1 {
                return Err(StoragePortError::new(
                    "context-series-conflict",
                    "Context head CAS 更新失败",
                ));
            }
        } else {
            transaction
                .execute(
                    "INSERT INTO context_heads (session_id, series_id, updated_at_millis)
                     VALUES (?1, ?2, ?3)",
                    params![
                        next.session_id.as_str(),
                        next.id.as_str(),
                        next.created_at_millis,
                    ],
                )
                .map_err(|error| sqlite_error("insert-context-head", error))?;
        }
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit-context-transition", error))?;
        Ok(())
    }

    fn save_context_checkpoint(
        &mut self,
        expected_active_series_id: &ContextSeriesId,
        checkpoint: ContextCheckpoint,
    ) -> StoreResult<()> {
        if checkpoint.context_series_id != *expected_active_series_id {
            return Err(StoragePortError::new(
                "checkpoint-series-mismatch",
                "Checkpoint 必须引用预期活动 Context Series",
            ));
        }
        let payload = serde_json::to_string(&checkpoint)
            .map_err(|error| schema_error("serialize-context-checkpoint", error))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin-save-checkpoint", error))?;
        let actual: Option<String> = transaction
            .query_row(
                "SELECT series_id FROM context_heads WHERE session_id = ?1",
                [checkpoint.session_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("read-checkpoint-context-head", error))?;
        if actual.as_deref() != Some(expected_active_series_id.as_str()) {
            return Err(StoragePortError::new(
                "context-series-conflict",
                format!(
                    "expected={}, actual={:?}",
                    expected_active_series_id,
                    actual.as_deref()
                ),
            ));
        }
        transaction
            .execute(
                "INSERT INTO context_checkpoints (
                     checkpoint_id, session_id, series_id, payload_json, created_at_millis
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    checkpoint.id.as_str(),
                    checkpoint.session_id.as_str(),
                    checkpoint.context_series_id.as_str(),
                    payload,
                    checkpoint.created_at_millis,
                ],
            )
            .map_err(|error| sqlite_error("insert-context-checkpoint", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit-context-checkpoint", error))?;
        Ok(())
    }

    fn load_context_checkpoint(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> StoreResult<Option<ContextCheckpoint>> {
        let payload: Option<String> = self
            .connection
            .query_row(
                "SELECT payload_json FROM context_checkpoints
                 WHERE session_id = ?1 AND checkpoint_id = ?2",
                params![session_id.as_str(), checkpoint_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("load-context-checkpoint", error))?;
        payload
            .map(|payload| deserialize(&payload, "context-checkpoint"))
            .transpose()
    }

    fn list_context_checkpoints(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Vec<ContextCheckpoint>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload_json FROM context_checkpoints
                 WHERE session_id = ?1
                 ORDER BY created_at_millis DESC, checkpoint_id DESC",
            )
            .map_err(|error| sqlite_error("prepare-list-context-checkpoints", error))?;
        let rows = statement
            .query_map([session_id.as_str()], |row| row.get::<_, String>(0))
            .map_err(|error| sqlite_error("query-list-context-checkpoints", error))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("read-list-context-checkpoints", error))?
            .into_iter()
            .map(|payload| deserialize(&payload, "context-checkpoint"))
            .collect()
    }

    fn create_tool_invocation(&mut self, record: ToolInvocationRecord) -> StoreResult<()> {
        let payload = serde_json::to_string(&record)
            .map_err(|error| schema_error("serialize-tool-invocation", error))?;
        self.connection
            .execute(
                "INSERT INTO tool_invocations (
                     id, idempotency_key, project_id, mission_id, run_id, tool_name,
                     status, payload_json, created_at_millis, updated_at_millis
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    record.id.as_str(),
                    record.idempotency_key,
                    record.envelope.project_id.as_str(),
                    record.envelope.mission_id.as_str(),
                    record.envelope.run_id.as_ref().map(RunId::as_str),
                    record.tool_name,
                    tool_status_name(record.status),
                    payload,
                    record.created_at_millis,
                    record.updated_at_millis,
                ],
            )
            .map_err(|error| sqlite_error("insert-tool-invocation", error))?;
        Ok(())
    }

    fn update_tool_invocation(
        &mut self,
        id: &ToolInvocationId,
        patch: ToolInvocationPatch,
    ) -> StoreResult<ToolInvocationRecord> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin-tool-update", error))?;
        let current: Option<(String, String)> = transaction
            .query_row(
                "SELECT status, payload_json FROM tool_invocations WHERE id = ?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| sqlite_error("load-tool-for-update", error))?;
        let Some((status, payload)) = current else {
            return Err(StoragePortError::new(
                "tool-invocation-not-found",
                id.to_string(),
            ));
        };
        if status != tool_status_name(patch.expected_status) {
            return Err(StoragePortError::new(
                "tool-invocation-update-conflict",
                format!("expected={:?}, actual={status}", patch.expected_status),
            ));
        }
        let mut record: ToolInvocationRecord = deserialize(&payload, "tool-invocation")?;
        record.status = patch.status;
        record.approval_request_id = patch.approval_request_id;
        record.result = patch.result;
        record.error = patch.error;
        record.updated_at_millis = patch.updated_at_millis;
        let payload = serde_json::to_string(&record)
            .map_err(|error| schema_error("serialize-tool-invocation", error))?;
        let changed = transaction
            .execute(
                "UPDATE tool_invocations
                 SET status = ?1, payload_json = ?2, updated_at_millis = ?3
                 WHERE id = ?4 AND status = ?5",
                params![
                    tool_status_name(record.status),
                    payload,
                    record.updated_at_millis,
                    id.as_str(),
                    status,
                ],
            )
            .map_err(|error| sqlite_error("update-tool-invocation", error))?;
        if changed != 1 {
            return Err(StoragePortError::new(
                "tool-invocation-update-conflict",
                id.to_string(),
            ));
        }
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit-tool-update", error))?;
        Ok(record)
    }

    fn get_tool_invocation(
        &self,
        id: &ToolInvocationId,
    ) -> StoreResult<Option<ToolInvocationRecord>> {
        let payload: Option<String> = self
            .connection
            .query_row(
                "SELECT payload_json FROM tool_invocations WHERE id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("get-tool-invocation", error))?;
        payload
            .map(|payload| deserialize(&payload, "tool-invocation"))
            .transpose()
    }

    fn find_tool_invocation_by_key(&self, key: &str) -> StoreResult<Option<ToolInvocationRecord>> {
        let payload: Option<String> = self
            .connection
            .query_row(
                "SELECT payload_json FROM tool_invocations WHERE idempotency_key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| sqlite_error("find-tool-invocation", error))?;
        payload
            .map(|payload| deserialize(&payload, "tool-invocation"))
            .transpose()
    }

    fn list_tool_invocations(&self) -> StoreResult<Vec<ToolInvocationRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload_json FROM tool_invocations
                 ORDER BY created_at_millis, id",
            )
            .map_err(|error| sqlite_error("prepare-list-tool-invocations", error))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| sqlite_error("query-list-tool-invocations", error))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("read-list-tool-invocations", error))?
            .into_iter()
            .map(|payload| deserialize(&payload, "tool-invocation"))
            .collect()
    }

    fn list_claimable_effects(
        &self,
        now_millis: i64,
        limit: usize,
    ) -> StoreResult<Vec<OutboxEntry>> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoragePortError::new("invalid-limit", "limit 超出 i64"))?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT effect_id, mission_id, aggregate_version, payload_json, status,
                        mission_epoch, run_id, run_fence, claim_token, attempt,
                        lease_expires_at_millis, created_at_millis, updated_at_millis
                 FROM outbox
                 WHERE status = 'pending'
                    OR (status = 'claimed' AND lease_expires_at_millis <= ?1)
                 ORDER BY created_at_millis ASC, effect_id ASC
                 LIMIT ?2",
            )
            .map_err(|error| sqlite_error("prepare-list-claimable", error))?;
        let rows = statement
            .query_map(params![now_millis, limit], read_raw_outbox)
            .map_err(|error| sqlite_error("query-list-claimable", error))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("read-list-claimable", error))?
            .into_iter()
            .map(RawOutbox::try_into_entry)
            .collect()
    }

    fn try_claim_effect(
        &mut self,
        effect_id: &EffectId,
        claim_token: ClaimToken,
        now_millis: i64,
        lease_expires_at_millis: i64,
    ) -> StoreResult<Option<ClaimedEffect>> {
        if lease_expires_at_millis <= now_millis {
            return Err(StoragePortError::new(
                "invalid-lease",
                "lease expiry 必须晚于 now",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin-claim", error))?;
        let raw = transaction
            .query_row(
                "SELECT effect_id, mission_id, aggregate_version, payload_json, status,
                        mission_epoch, run_id, run_fence, claim_token, attempt,
                        lease_expires_at_millis, created_at_millis, updated_at_millis
                 FROM outbox WHERE effect_id = ?1",
                [effect_id.as_str()],
                read_raw_outbox,
            )
            .optional()
            .map_err(|error| sqlite_error("select-claim", error))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let entry = raw.try_into_entry()?;
        let claimable = entry.status == OutboxStatus::Pending
            || (entry.status == OutboxStatus::Claimed
                && entry
                    .lease_expires_at_millis
                    .is_some_and(|expiry| expiry <= now_millis));
        if !claimable {
            return Ok(None);
        }
        let changed = transaction
            .execute(
                "UPDATE outbox SET
                     status = 'claimed', claim_token = ?1, attempt = attempt + 1,
                     lease_expires_at_millis = ?2, updated_at_millis = ?3
                 WHERE effect_id = ?4
                   AND (status = 'pending'
                        OR (status = 'claimed' AND lease_expires_at_millis <= ?3))",
                params![
                    claim_token.as_str(),
                    lease_expires_at_millis,
                    now_millis,
                    effect_id.as_str()
                ],
            )
            .map_err(|error| sqlite_error("update-claim", error))?;
        if changed != 1 {
            return Ok(None);
        }
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit-claim", error))?;
        Ok(Some(ClaimedEffect {
            claim: EffectClaim {
                effect_id: entry.effect_id,
                mission_id: entry.mission_id,
                mission_epoch: entry.mission_epoch,
                claim_token,
                run_id: entry.run_id,
                run_fence: entry.run_fence,
                attempt: entry.attempt + 1,
                lease_expires_at_millis,
            },
            intent: entry.intent,
        }))
    }

    fn complete_effect(&mut self, completion: EffectCompletion) -> StoreResult<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin-completion", error))?;
        let raw = transaction
            .query_row(
                "SELECT effect_id, mission_id, aggregate_version, payload_json, status,
                        mission_epoch, run_id, run_fence, claim_token, attempt,
                        lease_expires_at_millis, created_at_millis, updated_at_millis
                 FROM outbox WHERE effect_id = ?1",
                [completion.fence.effect_id.as_str()],
                read_raw_outbox,
            )
            .optional()
            .map_err(|error| sqlite_error("select-completion", error))?
            .ok_or_else(|| StoragePortError::new("effect-not-found", "Effect 不存在"))?;
        let entry = raw.try_into_entry()?;

        if entry.status != OutboxStatus::Claimed {
            let existing = load_effect_result_tx(&transaction, &entry.effect_id)?;
            let desired = EffectResultRecord {
                effect_id: entry.effect_id,
                outcome: completion.outcome,
                result: completion.result,
                error: completion.error,
                recorded_at_millis: completion.recorded_at_millis,
            };
            if existing.as_ref().is_some_and(|existing| {
                existing.effect_id == desired.effect_id
                    && existing.outcome == desired.outcome
                    && existing.result == desired.result
                    && existing.error == desired.error
            }) {
                return Ok(());
            }
            return Err(StoragePortError::new(
                "effect-not-claimed",
                format!("Effect 状态为 {:?}", entry.status),
            ));
        }

        let claim = EffectClaim {
            effect_id: entry.effect_id.clone(),
            mission_id: entry.mission_id.clone(),
            mission_epoch: entry.mission_epoch,
            claim_token: entry.claim_token.clone().ok_or_else(|| {
                StoragePortError::new("claim-token-missing", "claimed Effect 缺少 token")
            })?,
            run_id: entry.run_id.clone(),
            run_fence: entry.run_fence,
            attempt: entry.attempt,
            lease_expires_at_millis: entry.lease_expires_at_millis.ok_or_else(|| {
                StoragePortError::new("claim-lease-missing", "claimed Effect 缺少 lease")
            })?,
        };
        validate_completion_fence(&claim, &completion.fence)
            .map_err(|error| StoragePortError::new("stale-effect-completion", error.to_string()))?;

        let status = outbox_status_for_outcome(completion.outcome);
        transaction
            .execute(
                "UPDATE outbox SET status = ?1, updated_at_millis = ?2
                 WHERE effect_id = ?3 AND status = 'claimed'",
                params![
                    outbox_status_str(status),
                    completion.recorded_at_millis,
                    entry.effect_id.as_str()
                ],
            )
            .map_err(|error| sqlite_error("update-effect-completion", error))?;
        let result_json = completion
            .result
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| schema_error("serialize-effect-result", error))?;
        transaction
            .execute(
                "INSERT INTO effect_results (
                     effect_id, outcome, result_json, error, recorded_at_millis
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    entry.effect_id.as_str(),
                    outcome_str(completion.outcome),
                    result_json,
                    completion.error,
                    completion.recorded_at_millis
                ],
            )
            .map_err(|error| sqlite_error("insert-effect-result", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit-effect-completion", error))?;
        Ok(())
    }

    fn load_effect_result(&self, effect_id: &EffectId) -> StoreResult<Option<EffectResultRecord>> {
        load_effect_result_connection(&self.connection, effect_id)
    }

    fn list_outbox(&self) -> StoreResult<Vec<OutboxEntry>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT effect_id, mission_id, aggregate_version, payload_json, status,
                        mission_epoch, run_id, run_fence, claim_token, attempt,
                        lease_expires_at_millis, created_at_millis, updated_at_millis
                 FROM outbox ORDER BY created_at_millis ASC, effect_id ASC",
            )
            .map_err(|error| sqlite_error("prepare-list-outbox", error))?;
        let rows = statement
            .query_map([], read_raw_outbox)
            .map_err(|error| sqlite_error("query-list-outbox", error))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("read-list-outbox", error))?
            .into_iter()
            .map(RawOutbox::try_into_entry)
            .collect()
    }
}

struct RawEvent {
    sequence: i64,
    aggregate_version: i64,
    payload_json: String,
    recorded_at_millis: i64,
}

impl RawEvent {
    fn try_normalize(self) -> StoreResult<NormalizedRawEvent> {
        Ok(NormalizedRawEvent {
            sequence: to_u64(self.sequence, "event-sequence")?,
            aggregate_version: to_u64(self.aggregate_version, "aggregate-version")?,
            payload_json: self.payload_json,
            recorded_at_millis: self.recorded_at_millis,
        })
    }
}

struct NormalizedRawEvent {
    sequence: u64,
    aggregate_version: u64,
    payload_json: String,
    recorded_at_millis: i64,
}

struct RawOutbox {
    effect_id: String,
    mission_id: String,
    aggregate_version: i64,
    payload_json: String,
    status: String,
    mission_epoch: i64,
    run_id: Option<String>,
    run_fence: Option<i64>,
    claim_token: Option<String>,
    attempt: i64,
    lease_expires_at_millis: Option<i64>,
    created_at_millis: i64,
    updated_at_millis: i64,
}

impl RawOutbox {
    fn try_into_entry(self) -> StoreResult<OutboxEntry> {
        Ok(OutboxEntry {
            effect_id: EffectId::from(self.effect_id),
            mission_id: MissionId::from(self.mission_id),
            aggregate_version: to_u64(self.aggregate_version, "aggregate-version")?,
            intent: deserialize(&self.payload_json, "effect-intent")?,
            status: parse_outbox_status(&self.status)?,
            mission_epoch: MissionEpoch(to_u64(self.mission_epoch, "mission-epoch")?),
            run_id: self.run_id.map(RunId::from),
            run_fence: self
                .run_fence
                .map(|fence| to_u64(fence, "run-fence").map(RunFence))
                .transpose()?,
            claim_token: self.claim_token.map(ClaimToken::from),
            attempt: u32::try_from(self.attempt)
                .map_err(|_| StoragePortError::new("invalid-attempt", self.attempt.to_string()))?,
            lease_expires_at_millis: self.lease_expires_at_millis,
            created_at_millis: self.created_at_millis,
            updated_at_millis: self.updated_at_millis,
        })
    }
}

fn read_raw_outbox(row: &Row<'_>) -> rusqlite::Result<RawOutbox> {
    Ok(RawOutbox {
        effect_id: row.get(0)?,
        mission_id: row.get(1)?,
        aggregate_version: row.get(2)?,
        payload_json: row.get(3)?,
        status: row.get(4)?,
        mission_epoch: row.get(5)?,
        run_id: row.get(6)?,
        run_fence: row.get(7)?,
        claim_token: row.get(8)?,
        attempt: row.get(9)?,
        lease_expires_at_millis: row.get(10)?,
        created_at_millis: row.get(11)?,
        updated_at_millis: row.get(12)?,
    })
}

fn current_version(transaction: &Transaction<'_>, kind: &str, id: &str) -> StoreResult<u64> {
    let version: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(aggregate_version), 0)
             FROM aggregate_events WHERE aggregate_kind = ?1 AND aggregate_id = ?2",
            params![kind, id],
            |row| row.get(0),
        )
        .map_err(|error| sqlite_error("current-version", error))?;
    to_u64(version, "aggregate-version")
}

fn load_effect_result_connection(
    connection: &Connection,
    effect_id: &EffectId,
) -> StoreResult<Option<EffectResultRecord>> {
    let raw = connection
        .query_row(
            "SELECT effect_id, outcome, result_json, error, recorded_at_millis
             FROM effect_results WHERE effect_id = ?1",
            [effect_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sqlite_error("load-effect-result", error))?;
    raw.map(parse_effect_result).transpose()
}

fn load_effect_result_tx(
    transaction: &Transaction<'_>,
    effect_id: &EffectId,
) -> StoreResult<Option<EffectResultRecord>> {
    let raw = transaction
        .query_row(
            "SELECT effect_id, outcome, result_json, error, recorded_at_millis
             FROM effect_results WHERE effect_id = ?1",
            [effect_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sqlite_error("load-effect-result", error))?;
    raw.map(parse_effect_result).transpose()
}

fn parse_effect_result(
    raw: (String, String, Option<String>, Option<String>, i64),
) -> StoreResult<EffectResultRecord> {
    Ok(EffectResultRecord {
        effect_id: EffectId::from(raw.0),
        outcome: parse_outcome(&raw.1)?,
        result: raw
            .2
            .map(|json| deserialize(&json, "effect-result"))
            .transpose()?,
        error: raw.3,
        recorded_at_millis: raw.4,
    })
}

fn serialize_tagged<T: serde::Serialize>(
    value: &T,
    context: &str,
) -> StoreResult<(String, String)> {
    let json = serde_json::to_value(value).map_err(|error| schema_error(context, error))?;
    let tag = json
        .as_object()
        .and_then(|object| object.get("type").or_else(|| object.get("kind")))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| StoragePortError::new("schema-tag-missing", context))?
        .to_owned();
    let payload = serde_json::to_string(&json).map_err(|error| schema_error(context, error))?;
    Ok((tag, payload))
}

fn deserialize<T: serde::de::DeserializeOwned>(json: &str, context: &str) -> StoreResult<T> {
    serde_json::from_str(json).map_err(|error| schema_error(context, error))
}

fn parse_outbox_status(value: &str) -> StoreResult<OutboxStatus> {
    match value {
        "pending" => Ok(OutboxStatus::Pending),
        "claimed" => Ok(OutboxStatus::Claimed),
        "completed" => Ok(OutboxStatus::Completed),
        "failed" => Ok(OutboxStatus::Failed),
        "uncertain" => Ok(OutboxStatus::Uncertain),
        _ => Err(StoragePortError::new("invalid-outbox-status", value)),
    }
}

const fn outbox_status_str(status: OutboxStatus) -> &'static str {
    match status {
        OutboxStatus::Pending => "pending",
        OutboxStatus::Claimed => "claimed",
        OutboxStatus::Completed => "completed",
        OutboxStatus::Failed => "failed",
        OutboxStatus::Uncertain => "uncertain",
    }
}

const fn outbox_status_for_outcome(outcome: EffectOutcome) -> OutboxStatus {
    match outcome {
        EffectOutcome::Completed => OutboxStatus::Completed,
        EffectOutcome::Failed => OutboxStatus::Failed,
        EffectOutcome::Uncertain => OutboxStatus::Uncertain,
    }
}

const fn outcome_str(outcome: EffectOutcome) -> &'static str {
    match outcome {
        EffectOutcome::Completed => "completed",
        EffectOutcome::Failed => "failed",
        EffectOutcome::Uncertain => "uncertain",
    }
}

fn parse_outcome(value: &str) -> StoreResult<EffectOutcome> {
    match value {
        "completed" => Ok(EffectOutcome::Completed),
        "failed" => Ok(EffectOutcome::Failed),
        "uncertain" => Ok(EffectOutcome::Uncertain),
        _ => Err(StoragePortError::new("invalid-effect-outcome", value)),
    }
}

fn to_i64(value: u64, field: &str) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| StoragePortError::new("integer-overflow", field.to_owned()))
}

fn to_u64(value: i64, field: &str) -> StoreResult<u64> {
    u64::try_from(value)
        .map_err(|_| StoragePortError::new("invalid-negative-integer", field.to_owned()))
}

fn sqlite_error(context: &str, error: rusqlite::Error) -> StoragePortError {
    StoragePortError::new("sqlite-error", format!("{context}: {error}"))
}

fn schema_error(context: &str, error: impl Display) -> StoragePortError {
    StoragePortError::new("schema-error", format!("{context}: {error}"))
}
