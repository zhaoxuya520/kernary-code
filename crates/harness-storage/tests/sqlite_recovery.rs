use harness_context::{ContextCheckpoint, ContextSeries, ContextStore, ContextTransition};
use harness_kernel::{
    ApprovalDecision, CompletionFence, DomainEvent, EffectCompletion, EffectIntent, EffectOutcome,
    KernelStore, MissionEpoch, MissionSnapshot, MissionState, NewEffect, OutboxStatus, RunFence,
    SessionEvent, SessionSnapshot, VersionedEvent, replay_mission, replay_session,
};
use harness_permission::{ExecutionEnvelope, PermissionAction};
use harness_storage::SqliteKernelStore;
use harness_testkit::{mission_fixture, verify_ts_oracle_manifest};
use harness_tool::{
    ToolEffectClass, ToolInvocationJournal, ToolInvocationPatch, ToolInvocationRecord,
    ToolInvocationStatus,
};
use harness_types::{
    ActorId, CheckpointId, ClaimToken, ConfidentialityLabel, ContentHash, ContextSeriesId,
    EffectId, GoalRevisionId, InformationFlowLabel, IntegrityLabel, MissionId, PermissionRequestId,
    ProjectId, RunId, SessionId, TaskId, ToolInvocationId,
};
use serde::Deserialize;
use std::process::Command;
use tempfile::tempdir;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MissionFixture {
    schema_version: u32,
    fixture: String,
    events: Vec<VersionedEvent>,
    final_state: MissionState,
    projection: serde_json::Value,
    outbox: serde_json::Value,
    invariants: serde_json::Value,
}

fn load_fixture() -> MissionFixture {
    verify_ts_oracle_manifest().expect("TS fixture manifest 必须有效");
    serde_json::from_str(mission_fixture()).expect("Mission fixture 应可解析")
}

#[test]
fn mission_snapshot_tail_and_reopen_recover_typescript_state() {
    let temporary = tempdir().expect("应创建测试目录");
    let database_path = temporary.path().join("kernel.sqlite");
    let fixture = load_fixture();
    assert_eq!(fixture.schema_version, 1);
    assert_eq!(fixture.fixture, "mission-parallel-approval-join");
    assert!(fixture.projection.is_object());
    assert!(fixture.outbox.is_object());
    assert!(fixture.invariants.is_object());
    let mission_id = MissionId::from("mission:oracle");

    {
        let store = SqliteKernelStore::open(&database_path).expect("SQLite Store 应打开");
        assert_eq!(store.schema_version().expect("schema version"), 3);
        let first_ten = fixture.events[..10]
            .iter()
            .map(|event| event.event.clone())
            .collect();
        let first_receipt = store
            .commit_mission(&mission_id, 0, first_ten, vec![], 100)
            .expect("前十个事件应原子提交");
        assert_eq!(first_receipt.aggregate_version, 10);

        let state_at_ten =
            replay_mission(mission_id.clone(), &fixture.events[..10]).expect("前十个事件应可重放");
        store
            .save_mission_snapshot(MissionSnapshot {
                state: state_at_ten,
                created_at_millis: 110,
            })
            .expect("snapshot 应保存");

        let tail = fixture.events[10..]
            .iter()
            .map(|event| event.event.clone())
            .collect();
        let tail_receipt = store
            .commit_mission(&mission_id, 10, tail, vec![], 120)
            .expect("tail 应提交");
        assert_eq!(tail_receipt.aggregate_version, 14);
        assert_eq!(
            store
                .recover_mission(&mission_id)
                .expect("应由 snapshot+tail 恢复"),
            fixture.final_state
        );

        let conflict = store
            .commit_mission(
                &mission_id,
                0,
                vec![DomainEvent::MissionCompleted {}],
                vec![],
                130,
            )
            .expect_err("旧 expected version 必须冲突");
        assert_eq!(conflict.code, "version-conflict");
        assert_eq!(
            store
                .load_mission_events(&mission_id, 0)
                .expect("事件应可读")
                .len(),
            14
        );
    }

    let reopened = SqliteKernelStore::open(&database_path).expect("数据库应可重开");
    assert_eq!(reopened.schema_version().expect("schema version"), 3);
    assert_eq!(
        reopened.recover_mission(&mission_id).expect("重开后应恢复"),
        fixture.final_state
    );
}

#[test]
fn outbox_reclaims_expired_lease_and_rejects_stale_completion() {
    let temporary = tempdir().expect("应创建测试目录");
    let database_path = temporary.path().join("outbox.sqlite");
    let mission_id = MissionId::from("mission:outbox");
    let effect_id = EffectId::from("effect:1");

    {
        let store = SqliteKernelStore::open(&database_path).expect("Store 应打开");
        let effect = NewEffect {
            effect_id: effect_id.clone(),
            intent: EffectIntent::StartAgentRun {
                mission_id: mission_id.clone(),
                node_id: TaskId::from("task:1"),
                run_id: RunId::from("run:1"),
            },
            mission_epoch: MissionEpoch(2),
            run_id: Some(RunId::from("run:1")),
            run_fence: Some(RunFence(3)),
        };
        store
            .commit_mission(
                &mission_id,
                0,
                vec![DomainEvent::MissionCreated {
                    mission_id: mission_id.clone(),
                    project_id: ProjectId::from("project:outbox"),
                    goal: "outbox".to_owned(),
                }],
                vec![effect.clone()],
                0,
            )
            .expect("Event+Outbox 应同事务提交");
        assert_eq!(
            store
                .list_claimable_effects(0, 10)
                .expect("应列出 pending")
                .len(),
            1
        );

        let first = store
            .try_claim_effect(&effect_id, ClaimToken::from("claim:first"), 0, 100)
            .expect("第一次 claim 不应失败")
            .expect("第一次 claim 应成功");
        assert_eq!(first.claim.attempt, 1);
        assert!(
            store
                .list_claimable_effects(99, 10)
                .expect("未过期时可查询")
                .is_empty()
        );
        assert_eq!(
            store
                .list_claimable_effects(101, 10)
                .expect("过期后应重新可领取")
                .len(),
            1
        );

        let second = store
            .try_claim_effect(&effect_id, ClaimToken::from("claim:second"), 101, 200)
            .expect("第二次 claim 不应失败")
            .expect("过期 lease 应可 reclaim");
        assert_eq!(second.claim.attempt, 2);

        let stale = store
            .complete_effect(EffectCompletion {
                fence: CompletionFence {
                    effect_id: effect_id.clone(),
                    mission_epoch: first.claim.mission_epoch,
                    claim_token: first.claim.claim_token,
                    run_fence: first.claim.run_fence,
                },
                outcome: EffectOutcome::Completed,
                result: Some(serde_json::json!({"worker":"old"})),
                error: None,
                recorded_at_millis: 150,
            })
            .expect_err("旧 claim completion 必须拒绝");
        assert_eq!(stale.code, "stale-effect-completion");

        let completion = EffectCompletion {
            fence: CompletionFence {
                effect_id: effect_id.clone(),
                mission_epoch: second.claim.mission_epoch,
                claim_token: second.claim.claim_token,
                run_fence: second.claim.run_fence,
            },
            outcome: EffectOutcome::Uncertain,
            result: Some(serde_json::json!({"sent":true})),
            error: Some("connection-lost-after-send".to_owned()),
            recorded_at_millis: 160,
        };
        store
            .complete_effect(completion.clone())
            .expect("当前 claim completion 应成功");
        store
            .complete_effect(EffectCompletion {
                recorded_at_millis: 170,
                ..completion
            })
            .expect("相同 completion 重放应幂等");
        let result = store
            .load_effect_result(&effect_id)
            .expect("result 应可读取")
            .expect("result 应存在");
        assert_eq!(result.outcome, EffectOutcome::Uncertain);
        assert_eq!(
            store.list_outbox().expect("outbox")[0].status,
            OutboxStatus::Uncertain
        );

        let atomic_failure = store
            .commit_mission(
                &mission_id,
                1,
                vec![DomainEvent::MissionCompleted {}],
                vec![effect],
                180,
            )
            .expect_err("重复 effect ID 应使整个事务失败");
        assert_eq!(atomic_failure.code, "sqlite-error");
        assert_eq!(
            store
                .load_mission_events(&mission_id, 0)
                .expect("事件应可读")
                .len(),
            1,
            "Event insert 必须随 Outbox 冲突回滚"
        );
    }

    let reopened = SqliteKernelStore::open(&database_path).expect("数据库应可重开");
    assert_eq!(
        reopened
            .load_effect_result(&effect_id)
            .expect("result 应可读")
            .expect("result 应持久化")
            .outcome,
        EffectOutcome::Uncertain
    );
}

#[test]
fn session_snapshot_and_tail_preserve_goal_lock() {
    let temporary = tempdir().expect("应创建测试目录");
    let database_path = temporary.path().join("session.sqlite");
    let store = SqliteKernelStore::open(&database_path).expect("Store 应打开");
    let session_id = SessionId::from("session:1");
    let revision = harness_kernel::GoalRevision {
        id: GoalRevisionId::from("goal:1"),
        parent_revision_id: None,
        text: "完成 Storage".to_owned(),
        created_by: ActorId::from("user:1"),
        reason: "initial".to_owned(),
        created_at_millis: 10,
    };
    let first_events = vec![
        SessionEvent::SessionCreated {
            session_id: session_id.clone(),
            project_id: ProjectId::from("project:1"),
        },
        SessionEvent::GoalRevised { revision },
    ];
    store
        .commit_session(&session_id, 0, first_events.clone(), 10)
        .expect("Session events 应提交");
    let versioned = first_events
        .into_iter()
        .enumerate()
        .map(|(index, event)| harness_kernel::SessionVersionedEvent {
            aggregate_version: u64::try_from(index).expect("index") + 1,
            event,
        })
        .collect::<Vec<_>>();
    let state_at_two = replay_session(session_id.clone(), &versioned).expect("Session 应重放");
    store
        .save_session_snapshot(SessionSnapshot {
            state: state_at_two,
            created_at_millis: 20,
        })
        .expect("Session snapshot 应保存");
    store
        .commit_session(
            &session_id,
            2,
            vec![SessionEvent::GoalLockChanged { locked: true }],
            30,
        )
        .expect("Session tail 应提交");
    let recovered = store.recover_session(&session_id).expect("Session 应恢复");
    assert_eq!(recovered.version, 3);
    assert!(recovered.goal.locked);
    assert_eq!(
        recovered.goal.current_revision_id,
        Some(GoalRevisionId::from("goal:1"))
    );

    let conflict = store
        .commit_session(
            &session_id,
            1,
            vec![SessionEvent::GoalLockChanged { locked: false }],
            40,
        )
        .expect_err("旧 Session version 必须冲突");
    assert_eq!(conflict.code, "version-conflict");
}

#[test]
fn context_series_checkpoint_rollback_and_reopen_are_durable() {
    let temporary = tempdir().expect("应创建测试目录");
    let database_path = temporary.path().join("context.sqlite");
    let session_id = SessionId::from("session:context");
    let first = ContextSeries::initial(ContextSeriesId::from("series:1"), session_id.clone(), 10);
    let checkpoint = ContextCheckpoint {
        id: CheckpointId::from("checkpoint:1"),
        name: Some("before-change".to_owned()),
        session_id: session_id.clone(),
        context_series_id: first.id.clone(),
        goal_revision_id: Some(GoalRevisionId::from("goal:1")),
        plan_revision: Some("plan:1".to_owned()),
        completed_tasks: vec![],
        pending_tasks: vec![TaskId::from("task:1")],
        decision_refs: vec!["decision:1".to_owned()],
        constraint_refs: vec![],
        modified_file_refs: vec![],
        error_refs: vec![],
        memory_refs: vec![],
        prompt_fingerprint: ContentHash::from("prompt:1"),
        created_at_millis: 20,
    };

    {
        let store = SqliteKernelStore::open(&database_path).expect("Store 应打开");
        store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: None,
                next_series: first.clone(),
                compaction_record: None,
            })
            .expect("初始 Context Series 应提交");
        store
            .save_context_checkpoint(&first.id, checkpoint.clone())
            .expect("Checkpoint 应保存");

        let second = ContextSeries {
            id: ContextSeriesId::from("series:2"),
            session_id: session_id.clone(),
            parent_series_id: Some(first.id.clone()),
            restored_from_checkpoint_id: None,
            items: vec![],
            created_at_millis: 30,
        };
        store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: Some(first.id.clone()),
                next_series: second.clone(),
                compaction_record: None,
            })
            .expect("第二条 Series 应 CAS 提交");

        let rollback = ContextSeries {
            id: ContextSeriesId::from("series:rollback"),
            session_id: session_id.clone(),
            parent_series_id: Some(second.id.clone()),
            restored_from_checkpoint_id: Some(checkpoint.id.clone()),
            items: first.items.clone(),
            created_at_millis: 40,
        };
        store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: Some(second.id.clone()),
                next_series: rollback.clone(),
                compaction_record: None,
            })
            .expect("Rollback 应创建新 Series");
        assert_eq!(
            store
                .load_active_context_series(&session_id)
                .expect("活动 Series")
                .expect("存在"),
            rollback
        );
        assert_eq!(
            store
                .load_context_series(&first.id)
                .expect("旧 Series")
                .expect("旧 Series 仍存在"),
            first
        );
        assert_eq!(
            store
                .list_context_checkpoints(&session_id)
                .expect("Checkpoint 列表"),
            vec![checkpoint.clone()]
        );

        let stale = ContextSeries {
            id: ContextSeriesId::from("series:stale"),
            session_id: session_id.clone(),
            parent_series_id: Some(ContextSeriesId::from("series:2")),
            restored_from_checkpoint_id: None,
            items: vec![],
            created_at_millis: 50,
        };
        let error = store
            .commit_context_transition(ContextTransition {
                expected_active_series_id: Some(ContextSeriesId::from("series:2")),
                next_series: stale,
                compaction_record: None,
            })
            .expect_err("过期 Context head 必须被 CAS 拒绝");
        assert_eq!(error.code, "context-series-conflict");
    }

    let reopened = SqliteKernelStore::open(&database_path).expect("Store 应重开");
    assert_eq!(
        reopened
            .load_context_checkpoint(&session_id, &checkpoint.id)
            .expect("Checkpoint 应读取"),
        Some(checkpoint)
    );
    assert_eq!(
        reopened
            .load_active_context_series(&session_id)
            .expect("活动 Series 应读取")
            .expect("活动 Series 存在")
            .id,
        ContextSeriesId::from("series:rollback")
    );
}

#[test]
fn schema_v1_is_migrated_through_context_and_tool_schema_v3() {
    let temporary = tempdir().expect("应创建测试目录");
    let database_path = temporary.path().join("v1.sqlite");
    {
        let connection = rusqlite::Connection::open(&database_path).expect("应创建 SQLite");
        connection
            .execute_batch(
                "CREATE TABLE migration_history (
                     version INTEGER PRIMARY KEY,
                     name TEXT NOT NULL,
                     applied_at_millis INTEGER NOT NULL
                 );
                 INSERT INTO migration_history VALUES (1, 'initial-kernel-session-outbox', 1);
                 PRAGMA user_version = 1;",
            )
            .expect("应建立 v1 fixture");
    }
    let store = SqliteKernelStore::open(&database_path).expect("v1 应迁移");
    assert_eq!(store.schema_version().expect("schema version"), 3);
    let initial = ContextSeries::initial(
        ContextSeriesId::from("series:migrated"),
        SessionId::from("session:migrated"),
        2,
    );
    store
        .commit_context_transition(ContextTransition {
            expected_active_series_id: None,
            next_series: initial,
            compaction_record: None,
        })
        .expect("迁移后的 Context 表应可用");
}

#[test]
fn tool_journal_is_durable_idempotent_and_status_cas_protected() {
    let temporary = tempdir().expect("应创建测试目录");
    let database_path = temporary.path().join("tool-journal.sqlite");
    let invocation_id = ToolInvocationId::from("invocation:1");
    let record = ToolInvocationRecord {
        id: invocation_id.clone(),
        idempotency_key: "tool:key:1".to_owned(),
        envelope: ExecutionEnvelope {
            project_id: ProjectId::from("project:tool"),
            mission_id: MissionId::from("mission:tool"),
            run_id: Some(RunId::from("run:tool")),
            actor_id: ActorId::from("agent:tool"),
            origin: harness_permission::InvocationOrigin::Agent,
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality: ConfidentialityLabel::ProjectPrivate,
            },
        },
        tool_name: "files.read".to_owned(),
        tool_version: "1".to_owned(),
        effect_class: ToolEffectClass::ReadOnlyRetryable,
        status: ToolInvocationStatus::Requested,
        args: serde_json::json!({"path":"README.md"}),
        permission_action: PermissionAction::FilesystemRead {
            path: temporary.path().join("README.md"),
        },
        approval_request_id: None,
        result: None,
        error: None,
        created_at_millis: 1,
        updated_at_millis: 1,
    };
    {
        let store = SqliteKernelStore::open(&database_path).expect("store");
        store.create(record.clone()).expect("create");
        let running = store
            .update(
                &invocation_id,
                ToolInvocationPatch {
                    expected_status: ToolInvocationStatus::Requested,
                    status: ToolInvocationStatus::Running,
                    approval_request_id: Some(PermissionRequestId::from("approval:1")),
                    result: None,
                    error: None,
                    updated_at_millis: 2,
                },
            )
            .expect("running");
        assert_eq!(running.status, ToolInvocationStatus::Running);
        let stale = store
            .update(
                &invocation_id,
                ToolInvocationPatch {
                    expected_status: ToolInvocationStatus::Requested,
                    status: ToolInvocationStatus::Completed,
                    approval_request_id: None,
                    result: Some(serde_json::json!({"ok":true})),
                    error: None,
                    updated_at_millis: 3,
                },
            )
            .expect_err("stale status");
        assert_eq!(stale.code, "tool-invocation-update-conflict");
    }
    let reopened = SqliteKernelStore::open(&database_path).expect("reopen");
    assert_eq!(
        reopened
            .get(&invocation_id)
            .expect("get")
            .expect("exists")
            .status,
        ToolInvocationStatus::Running
    );
    assert_eq!(
        reopened
            .find_by_idempotency_key("tool:key:1")
            .expect("find")
            .expect("exists")
            .id,
        invocation_id
    );
}

#[test]
fn approval_event_schema_remains_compatible() {
    let event = DomainEvent::ApprovalResolved {
        approval_id: harness_types::ApprovalId::from("approval:1"),
        decision: ApprovalDecision::Deny,
    };
    let json = serde_json::to_value(event).expect("event JSON");
    assert_eq!(json["type"], "approval.resolved");
    assert_eq!(json["decision"], "deny");
}

#[test]
fn abrupt_process_crash_preserves_committed_event() {
    let temporary = tempdir().expect("应创建测试目录");
    let database_path = temporary.path().join("crash.sqlite");
    let test_binary = std::env::current_exe().expect("应定位当前 test binary");
    let status = Command::new(test_binary)
        .args([
            "--exact",
            "crash_child_writes_then_aborts",
            "--ignored",
            "--nocapture",
        ])
        .env("HARNESS_CRASH_DB", &database_path)
        .status()
        .expect("应启动 crash child");
    assert!(!status.success(), "child 应由 abort 非正常退出");

    let reopened = SqliteKernelStore::open(&database_path).expect("crash 后数据库应可打开");
    let recovered = reopened
        .recover_mission(&MissionId::from("mission:crash"))
        .expect("crash 后已提交 Event 应恢复");
    assert_eq!(recovered.version, 1);
    assert_eq!(recovered.goal, "crash-safe");
}

#[test]
#[ignore = "只由 abrupt_process_crash_preserves_committed_event 子进程调用"]
fn crash_child_writes_then_aborts() {
    let database_path = std::env::var_os("HARNESS_CRASH_DB").expect("child 需要数据库路径");
    let store = SqliteKernelStore::open(database_path).expect("child Store 应打开");
    let mission_id = MissionId::from("mission:crash");
    store
        .commit_mission(
            &mission_id,
            0,
            vec![DomainEvent::MissionCreated {
                mission_id: mission_id.clone(),
                project_id: ProjectId::from("project:crash"),
                goal: "crash-safe".to_owned(),
            }],
            vec![],
            1,
        )
        .expect("child Event 应在 abort 前提交");
    std::process::abort();
}

#[test]
fn future_schema_version_is_rejected() {
    let temporary = tempdir().expect("应创建测试目录");
    let database_path = temporary.path().join("future.sqlite");
    {
        let connection = rusqlite::Connection::open(&database_path).expect("应创建 SQLite");
        connection
            .execute_batch("PRAGMA user_version = 99;")
            .expect("应设置 future version");
    }
    let error = SqliteKernelStore::open(&database_path)
        .err()
        .expect("未来 schema 必须拒绝");
    assert_eq!(error.code, "unsupported-schema-version");
}
