use harness_agent::*;
use harness_kernel::{DomainEvent, MissionState, NodeKind, WorkflowNodeDefinition, reduce_mission};
use harness_types::{
    AgentDefinitionId, AgentEndpointId, AgentInstanceId, AgentSessionId, MissionId, ModelId,
    ProjectId, ProviderId, RunId, TaskId,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

use harness_model::{
    CancellationToken, FakeModelProvider, FakeScenario, ModelRegistry, ModelRuntime, ModelUsage,
    ReasoningLevel,
};

fn agent(id: &str, role: AgentRole, capability: &str, cost: u32) -> AgentDefinition {
    AgentDefinition {
        id: AgentDefinitionId::from(id),
        name: id.to_owned(),
        roles: [role].into_iter().collect(),
        capabilities: [capability.to_owned()].into_iter().collect(),
        allowed_tools: BTreeSet::new(),
        max_concurrency: 8,
        cost_weight: cost,
        integrity_floor: "trusted".to_owned(),
    }
}

#[test]
fn builtin_catalog_has_fifteen_nonduplicative_roles_and_routes_specialists() {
    let catalog = builtin_agent_catalog().expect("builtin catalog");
    let definitions = catalog.list();
    assert_eq!(definitions.len(), 15);
    let ids = definitions
        .iter()
        .map(|definition| definition.id.to_string())
        .collect::<BTreeSet<_>>();
    for expected in [
        "agent:requirements",
        "agent:explorer",
        "agent:architect",
        "agent:security",
        "agent:performance",
        "agent:release",
    ] {
        assert!(ids.contains(expected), "missing {expected}");
    }
    for definition in &definitions {
        let control_plane = definition.roles.contains(&AgentRole::Coordinator)
            || definition.roles.contains(&AgentRole::StaffingRouter);
        assert!(!control_plane || !definition.roles.contains(&AgentRole::Coder));
        if definition.roles.iter().any(|role| {
            matches!(
                role,
                AgentRole::RequirementsAnalyst
                    | AgentRole::Explorer
                    | AgentRole::Architect
                    | AgentRole::Reviewer
                    | AgentRole::SecurityAuditor
                    | AgentRole::PerformanceEngineer
                    | AgentRole::ReleaseManager
            )
        }) {
            assert!(!definition.allowed_tools.contains("file.write"));
        }
    }

    let tasks = [
        ("requirements-analysis", AgentRole::RequirementsAnalyst),
        ("codebase-exploration", AgentRole::Explorer),
        ("system-design", AgentRole::Architect),
        ("security-audit", AgentRole::SecurityAuditor),
        ("performance-analysis", AgentRole::PerformanceEngineer),
        ("release-readiness", AgentRole::ReleaseManager),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (capability, role))| StaffingTask {
        task_id: TaskId::from(format!("task:specialist:{index}")),
        required_capabilities: [capability.to_owned()].into_iter().collect(),
        preferred_roles: [role].into_iter().collect(),
        forbidden_agents: BTreeSet::new(),
    })
    .collect::<Vec<_>>();
    let assignments = StaffingRouter::assign(&tasks, &catalog).expect("specialist routing");
    assert_eq!(assignments.len(), tasks.len());
    for (assignment, task) in assignments.iter().zip(tasks) {
        let expected = task.preferred_roles.iter().next().expect("role");
        let actual = catalog
            .definition(&assignment.agent_id)
            .expect("assigned definition");
        assert!(actual.roles.contains(expected));
    }
}

#[test]
fn staffing_is_deterministic_for_two_four_and_eight_tasks_without_kernel_catalog_pollution() {
    let mut catalog = AgentCatalog::default();
    catalog
        .register(agent("agent:coder", AgentRole::Coder, "rust", 10))
        .expect("coder");
    catalog
        .register(agent("agent:reviewer", AgentRole::Reviewer, "review", 5))
        .expect("reviewer");
    for count in [2, 4, 8] {
        let tasks = (0..count)
            .map(|i| StaffingTask {
                task_id: TaskId::from(format!("task:{i}")),
                required_capabilities: ["rust".to_owned()].into_iter().collect(),
                preferred_roles: [AgentRole::Coder].into_iter().collect(),
                forbidden_agents: BTreeSet::new(),
            })
            .collect::<Vec<_>>();
        let first = StaffingRouter::assign(&tasks, &catalog).expect("assign");
        let second = StaffingRouter::assign(&tasks, &catalog).expect("assign again");
        assert_eq!(first, second);
        assert!(
            first
                .iter()
                .all(|a| a.agent_id == AgentDefinitionId::from("agent:coder"))
        );
        assert!(first.iter().all(|a| !a.catalog_fingerprint.is_empty()));
    }
    assert_eq!(
        catalog.lifecycle(&AgentDefinitionId::from("agent:coder")),
        Some(AgentLifecycle::Sleeping)
    );
}

#[test]
fn staffing_obeys_per_agent_capacity_and_control_plane_cannot_code() {
    let mut catalog = AgentCatalog::default();
    let mut limited = agent("agent:limited", AgentRole::Coder, "rust", 1);
    limited.max_concurrency = 1;
    catalog.register(limited).expect("limited coder");
    let tasks = (0..2)
        .map(|i| StaffingTask {
            task_id: TaskId::from(format!("task:{i}")),
            required_capabilities: ["rust".to_owned()].into_iter().collect(),
            preferred_roles: [AgentRole::Coder].into_iter().collect(),
            forbidden_agents: BTreeSet::new(),
        })
        .collect::<Vec<_>>();
    assert!(StaffingRouter::assign(&tasks, &catalog).is_err());

    let mut invalid = agent(
        "agent:coordinator-coder",
        AgentRole::Coordinator,
        "coordination",
        1,
    );
    invalid.roles.insert(AgentRole::Coder);
    assert_eq!(
        catalog.register(invalid).expect_err("must reject").code,
        "agent-control-plane-cannot-code"
    );
}

#[test]
fn catalog_tracks_multiple_reserved_and_running_instances_without_overwrite() {
    let mut catalog = AgentCatalog::default();
    let mut definition = agent("agent:pool", AgentRole::Coder, "rust", 1);
    definition.max_concurrency = 2;
    catalog.register(definition.clone()).expect("register");
    assert!(catalog.register(definition).is_err());
    assert_eq!(catalog.list()[0].name, "agent:pool");

    let id = AgentDefinitionId::from("agent:pool");
    catalog.reserve(&id).expect("reserve first");
    catalog.start(&id).expect("start first");
    catalog
        .reserve(&id)
        .expect("reserve second while first runs");
    assert_eq!(catalog.active_count(&id), 2);
    assert_eq!(catalog.lifecycle(&id), Some(AgentLifecycle::Running));
    assert!(catalog.reserve(&id).is_err());
    catalog.start(&id).expect("start second");
    catalog.release(&id).expect("release first");
    assert_eq!(catalog.lifecycle(&id), Some(AgentLifecycle::Running));
    catalog.release(&id).expect("release second");
    assert_eq!(catalog.lifecycle(&id), Some(AgentLifecycle::Sleeping));
    assert_eq!(
        catalog.release(&id).expect_err("inactive release").code,
        "agent-not-active"
    );
}

#[test]
fn scheduler_uses_kernel_ready_nodes_priority_and_concurrency() {
    let mission_id = MissionId::from("mission:test");
    let mut state = MissionState::empty(mission_id.clone());
    state = reduce_mission(
        &state,
        &DomainEvent::MissionCreated {
            mission_id,
            project_id: ProjectId::from("project:test"),
            goal: "goal".to_owned(),
        },
    )
    .expect("create");
    let nodes = (0..4)
        .map(|i| WorkflowNodeDefinition {
            id: TaskId::from(format!("task:{i}")),
            title: format!("task {i}"),
            kind: NodeKind::Task,
            depends_on: vec![],
            agent_definition_id: AgentDefinitionId::from("agent:coder"),
            requires_approval: None,
        })
        .collect::<Vec<_>>();
    state = reduce_mission(&state, &DomainEvent::MissionPlanInstalled { nodes }).expect("plan");
    let assignments = (0..4)
        .map(|i| StaffingAssignment {
            task_id: TaskId::from(format!("task:{i}")),
            agent_id: AgentDefinitionId::from("agent:coder"),
            score: 1,
            reason_summary: "rust".to_owned(),
            catalog_fingerprint: "hash".to_owned(),
        })
        .collect::<Vec<_>>();
    let priorities = [(TaskId::from("task:3"), 10)].into_iter().collect();
    let scheduled = AgentScheduler {
        concurrency_limit: 2,
    }
    .schedule(&state, &assignments, &priorities);
    assert_eq!(scheduled.len(), 2);
    assert_eq!(scheduled[0].task_id, TaskId::from("task:3"));
}

#[test]
fn scheduler_applies_aging_cancel_and_cost_budget_before_dispatch() {
    let mission_id = MissionId::from("mission:scheduling");
    let mut state = MissionState::empty(mission_id.clone());
    state = reduce_mission(
        &state,
        &DomainEvent::MissionCreated {
            mission_id,
            project_id: ProjectId::from("project:test"),
            goal: "goal".to_owned(),
        },
    )
    .expect("create");
    state = reduce_mission(
        &state,
        &DomainEvent::MissionPlanInstalled {
            nodes: (0..3)
                .map(|i| WorkflowNodeDefinition {
                    id: TaskId::from(format!("task:{i}")),
                    title: format!("task {i}"),
                    kind: NodeKind::Task,
                    depends_on: vec![],
                    agent_definition_id: AgentDefinitionId::from("agent:coder"),
                    requires_approval: None,
                })
                .collect(),
        },
    )
    .expect("plan");
    let assignments = (0..3)
        .map(|i| StaffingAssignment {
            task_id: TaskId::from(format!("task:{i}")),
            agent_id: AgentDefinitionId::from("agent:coder"),
            score: 1,
            reason_summary: "rust".to_owned(),
            catalog_fingerprint: "hash".to_owned(),
        })
        .collect::<Vec<_>>();
    let priorities = [(TaskId::from("task:1"), 50)].into_iter().collect();
    let wait_cycles = [(TaskId::from("task:0"), 10)].into_iter().collect();
    let costs = [
        (TaskId::from("task:0"), 2),
        (TaskId::from("task:1"), 2),
        (TaskId::from("task:2"), 1),
    ]
    .into_iter()
    .collect();
    let cancelled = [TaskId::from("task:1")].into_iter().collect();
    let batch = AgentScheduler {
        concurrency_limit: 3,
    }
    .schedule_with_constraints(
        &state,
        &assignments,
        &priorities,
        &wait_cycles,
        &costs,
        &cancelled,
        2,
    );
    assert_eq!(batch.runs.len(), 1);
    assert_eq!(batch.runs[0].task_id, TaskId::from("task:0"));
    assert_eq!(batch.total_cost_units, 2);
    assert!(batch.deferred.iter().any(|item| {
        item.task_id == TaskId::from("task:1") && item.reason == "cancelled-before-dispatch"
    }));
    assert!(batch.deferred.iter().any(|item| {
        item.task_id == TaskId::from("task:2") && item.reason == "batch-cost-budget"
    }));
}

#[test]
fn message_bus_is_idempotent_ordered_and_acknowledged_per_recipient() {
    let temporary = tempdir().expect("tempdir");
    let mut bus = AgentMessageBus::open(temporary.path().join("messages.sqlite")).expect("bus");
    let base = AgentMessage {
        id: "message:1".to_owned(),
        idempotency_key: "key:1".to_owned(),
        mission_id: MissionId::from("mission:test"),
        from: "agent:a".to_owned(),
        to: "agent:b".to_owned(),
        kind: AgentMessageKind::Question,
        payload: serde_json::json!({"question":"conflict?"}),
        sequence: 0,
        created_at_millis: 1,
        acknowledged_at_millis: None,
    };
    let first = bus.send(base.clone()).expect("send");
    let duplicate = bus
        .send(AgentMessage {
            id: "different".to_owned(),
            ..base
        })
        .expect("dedup");
    assert_eq!(first.id, duplicate.id);
    assert_eq!(first.sequence, 1);
    assert_eq!(bus.pending("agent:b", 0, 10).expect("pending").len(), 1);
    assert_eq!(
        bus.claim("agent:b", 0, 10, "delivery:ordered", 2, 100)
            .expect("claim")
            .len(),
        1
    );
    assert!(
        bus.acknowledge_claim(&first.id, "delivery:ordered", 3)
            .expect("ack")
    );
    assert!(
        bus.pending("agent:b", 0, 10)
            .expect("pending after")
            .is_empty()
    );
}

#[test]
fn message_claim_is_exactly_once_visible_until_lease_expires() {
    let temporary = tempdir().expect("tempdir");
    let mut bus = AgentMessageBus::open(temporary.path().join("messages.sqlite")).expect("bus");
    let sent = bus
        .send(AgentMessage {
            id: "message:leased".to_owned(),
            idempotency_key: "key:leased".to_owned(),
            mission_id: MissionId::from("mission:test"),
            from: "agent:a".to_owned(),
            to: "agent:b".to_owned(),
            kind: AgentMessageKind::Task,
            payload: serde_json::json!({"task":"review"}),
            sequence: 0,
            created_at_millis: 1,
            acknowledged_at_millis: None,
        })
        .expect("send");
    let first = bus
        .claim("agent:b", 0, 10, "delivery:first", 10, 100)
        .expect("first claim");
    assert_eq!(first.len(), 1);
    assert!(
        bus.claim("agent:b", 0, 10, "delivery:second", 20, 100)
            .expect("hidden while leased")
            .is_empty()
    );
    assert!(
        !bus.acknowledge_claim(&sent.id, "delivery:first", 110)
            .expect("expired ack")
    );
    let reclaimed = bus
        .claim("agent:b", 0, 10, "delivery:second", 111, 100)
        .expect("reclaim after expiry");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].message.id, sent.id);
    assert!(
        !bus.acknowledge_claim(&sent.id, "delivery:first", 112)
            .expect("stale ack")
    );
    assert!(
        bus.acknowledge_claim(&sent.id, "delivery:second", 113)
            .expect("current ack")
    );
    assert!(bus.pending("agent:b", 0, 10).expect("pending").is_empty());
}

#[test]
fn overlapping_file_leases_conflict_and_stale_release_is_rejected() {
    let temporary = tempdir().expect("tempdir");
    let root = temporary.path().join("project");
    std::fs::create_dir_all(root.join("src")).expect("src");
    let sibling = temporary.path().join("project-lookalike");
    std::fs::create_dir_all(&sibling).expect("sibling");
    let mut leases =
        FileLeaseManager::open(&root, temporary.path().join("leases.sqlite")).expect("leases");
    let first = leases
        .acquire(Path::new("src"), RunId::from("run:a"), 0, 100)
        .expect("first");
    assert!(
        leases
            .acquire(&sibling, RunId::from("run:outside"), 0, 100)
            .is_err()
    );
    assert!(
        leases
            .acquire(Path::new("src/auth.rs"), RunId::from("run:b"), 1, 100)
            .is_err()
    );
    let replacement = leases
        .acquire(Path::new("src"), RunId::from("run:b"), 101, 100)
        .expect("expired replacement");
    assert!(!leases.release(&first).expect("stale release"));
    assert!(leases.release(&replacement).expect("release"));
}

#[test]
fn coordinator_records_meeting_and_acceptance_requires_review_and_test_evidence() {
    let mut meeting = MeetingRecord {
        id: "meeting:1".to_owned(),
        mission_id: MissionId::from("mission:test"),
        topic: "conflicting auth plans".to_owned(),
        participants: [
            "agent:a".to_owned(),
            "agent:b".to_owned(),
            "agent:coordinator".to_owned(),
        ]
        .into_iter()
        .collect(),
        transcript: vec![],
        decisions: vec![],
        conflicts: vec![],
        created_at_millis: 1,
        closed_at_millis: None,
    };
    meeting.append("agent:a", "use tokens").expect("append");
    meeting.append("agent:b", "use sessions").expect("append");
    meeting.conflict("token/session strategy conflict");
    meeting.decide("coordinator requests evidence");
    meeting.close(2);
    assert!(meeting.append("agent:a", "late").is_err());
    let mut result = AgentResult {
        status: "completed".to_owned(),
        summary: "implemented".to_owned(),
        artifacts: vec![],
        changed_files: vec![PathBuf::from("src/auth.rs")],
        evidence: vec![Evidence {
            kind: "review".to_owned(),
            reference: "review:1".to_owned(),
            summary: "approved".to_owned(),
        }],
        warnings: vec![],
        errors: vec![],
        metrics: AgentExecutionMetrics {
            turns: 1,
            ..AgentExecutionMetrics::default()
        },
        confidence: 0.9,
        follow_up: vec![],
        model_tool_yield: None,
    };
    assert!(validate_acceptance(&result, true, true).is_err());
    result.evidence.push(Evidence {
        kind: "test".to_owned(),
        reference: "test:1".to_owned(),
        summary: "passed".to_owned(),
    });
    validate_acceptance(&result, true, true).expect("accepted");
    assert!(validate_required_evidence(&result, &["security"]).is_err());
    result.evidence.push(Evidence {
        kind: "security".to_owned(),
        reference: "security:1".to_owned(),
        summary: "threat model passed".to_owned(),
    });
    validate_required_evidence(&result, &["review", "test", "security"])
        .expect("all specialist evidence accepted");
}

#[test]
fn coordinator_detects_file_and_decision_conflicts_and_persists_meeting() {
    let result =
        |agent_id: &str, run_id: &str, path: &str, auth_strategy: &str| -> AgentResultEnvelope {
            AgentResultEnvelope {
                agent_id: AgentDefinitionId::from(agent_id),
                task_id: TaskId::from(format!("task:{agent_id}")),
                run_id: RunId::from(run_id),
                result: AgentResult {
                    status: "completed".to_owned(),
                    summary: "proposal".to_owned(),
                    artifacts: vec![],
                    changed_files: vec![PathBuf::from(path)],
                    evidence: vec![],
                    warnings: vec![],
                    errors: vec![],
                    metrics: AgentExecutionMetrics::default(),
                    confidence: 0.8,
                    follow_up: vec![],
                    model_tool_yield: None,
                },
                decision_claims: [("auth.strategy".to_owned(), auth_strategy.to_owned())]
                    .into_iter()
                    .collect(),
            }
        };
    let outcome = Coordinator::inspect(
        MissionId::from("mission:test"),
        &[
            result("agent:a", "run:a", "src/auth", "token"),
            result("agent:b", "run:b", "src/auth/session.rs", "session"),
        ],
        "meeting:auto",
        10,
    );
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::FileOverlap)
    );
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::DecisionMismatch)
    );
    let meeting = outcome.meeting.expect("automatic meeting");
    assert!(meeting.participants.contains("agent:coordinator"));

    let temporary = tempdir().expect("tempdir");
    let bus = AgentMessageBus::open(temporary.path().join("messages.sqlite")).expect("bus");
    bus.save_meeting(&meeting, 11).expect("save meeting");
    assert_eq!(
        bus.meeting("meeting:auto").expect("load meeting"),
        Some(meeting)
    );
}

#[test]
fn parent_cancellation_reaches_every_child_and_parent_cannot_finish_early() {
    let mut tree = RunCancellationTree::default();
    let parent = RunId::from("run:parent");
    let child_a = RunId::from("run:child-a");
    let child_b = RunId::from("run:child-b");
    let grandchild = RunId::from("run:grandchild");
    let parent_token = tree.register(parent.clone(), None).expect("parent");
    let child_a_token = tree
        .register(child_a.clone(), Some(parent.clone()))
        .expect("child a");
    let child_b_token = tree
        .register(child_b.clone(), Some(parent.clone()))
        .expect("child b");
    let grandchild_token = tree
        .register(grandchild.clone(), Some(child_a.clone()))
        .expect("grandchild");
    assert_eq!(
        tree.finish(&parent)
            .expect_err("children still active")
            .code,
        "run-has-active-children"
    );
    let cancelled = tree.cancel_subtree(&parent).expect("cancel tree");
    assert_eq!(
        cancelled,
        vec![
            child_a.clone(),
            child_b.clone(),
            grandchild.clone(),
            parent.clone()
        ]
    );
    assert!(parent_token.is_cancelled());
    assert!(child_a_token.is_cancelled());
    assert!(child_b_token.is_cancelled());
    assert!(grandchild_token.is_cancelled());
    tree.finish(&grandchild).expect("finish grandchild");
    tree.finish(&child_a).expect("finish child a");
    tree.finish(&child_b).expect("finish child b");
    tree.finish(&parent).expect("finish parent last");
    assert!(tree.active_run_ids().is_empty());
}

#[test]
fn endpoint_and_agent_session_recover_with_cas_versions() {
    let temporary = tempdir().expect("tempdir");
    let path = temporary.path().join("agent-state.sqlite");
    let mut store = AgentStateStore::open(&path).expect("store");
    let mut endpoint = AgentEndpoint {
        id: AgentEndpointId::from("endpoint:coder"),
        definition_id: AgentDefinitionId::from("agent:coder"),
        instance_id: AgentInstanceId::from("instance:coder:1"),
        status: AgentEndpointStatus::Idle,
        generation: 1,
        active_runs: 0,
        max_concurrency: 2,
        last_heartbeat_millis: 10,
        version: 1,
    };
    store.create_endpoint(&endpoint).expect("create endpoint");
    endpoint.status = AgentEndpointStatus::Busy;
    endpoint.active_runs = 1;
    store
        .update_endpoint(1, &mut endpoint)
        .expect("update endpoint");
    assert_eq!(endpoint.version, 2);
    assert_eq!(
        store
            .endpoint(&endpoint.id)
            .expect("read endpoint")
            .expect("endpoint"),
        endpoint
    );
    let mut stale_endpoint = endpoint.clone();
    assert_eq!(
        store
            .update_endpoint(1, &mut stale_endpoint)
            .expect_err("stale endpoint")
            .code,
        "agent-endpoint-cas-conflict"
    );

    let mut session = AgentSession {
        id: AgentSessionId::from("agent-session:1"),
        mission_id: MissionId::from("mission:test"),
        task_id: TaskId::from("task:test"),
        run_id: RunId::from("run:test"),
        parent_run_id: None,
        endpoint_id: endpoint.id,
        agent_definition_id: AgentDefinitionId::from("agent:coder"),
        role: AgentRole::Coder,
        status: AgentSessionStatus::Prepared,
        context_fingerprint: "context:hash".to_owned(),
        previous_response_id: None,
        created_at_millis: 10,
        updated_at_millis: 10,
        version: 1,
    };
    store.create_session(&session).expect("create session");
    session.status = AgentSessionStatus::Running;
    session.updated_at_millis = 11;
    store
        .update_session(1, &mut session)
        .expect("update session");
    let result = AgentResult {
        status: "completed".to_owned(),
        summary: "compressed result".to_owned(),
        artifacts: vec![],
        changed_files: vec![],
        evidence: vec![],
        warnings: vec![],
        errors: vec![],
        metrics: AgentExecutionMetrics::default(),
        confidence: 0.75,
        follow_up: vec!["review".to_owned()],
        model_tool_yield: None,
    };
    store
        .save_result(&session.run_id, &result, 12)
        .expect("save result");
    store
        .set_task_priority(&session.mission_id, &session.task_id, 50, 12)
        .expect("priority");
    assert_eq!(
        store
            .task_priority(&session.mission_id, &session.task_id)
            .expect("read priority"),
        50
    );
    assert_eq!(
        store.recoverable_sessions().expect("recoverable"),
        vec![session.clone()]
    );
    drop(store);
    let reopened = AgentStateStore::open(path).expect("reopen");
    assert_eq!(
        reopened
            .session(&session.id)
            .expect("read session")
            .expect("session"),
        session
    );
    assert_eq!(
        reopened.result(&session.run_id).expect("result"),
        Some(result)
    );
}

#[test]
fn durable_budget_escrow_enforces_parallel_tokens_tools_and_expiry() {
    let temporary = tempdir().expect("tempdir");
    let mut budgets =
        AgentBudgetManager::open(temporary.path().join("budgets.sqlite")).expect("budgets");
    let policy = AgentBudgetPolicy {
        max_agents: 2,
        max_parallel_agents: 2,
        max_total_tokens: 100,
        max_tool_calls: 3,
        max_runtime_millis: 1_000,
        max_retries: 2,
    };
    let first_request = AgentBudgetRequest {
        reserved_tokens: 60,
        reserved_tool_calls: 2,
        reserved_runtime_millis: 400,
        reserved_retries: 1,
    };
    let mission = MissionId::from("mission:budget");
    let first = budgets
        .reserve(
            mission.clone(),
            RunId::from("run:one"),
            &first_request,
            &policy,
            0,
        )
        .expect("first reserve");
    assert_eq!(
        budgets
            .reserve(
                mission.clone(),
                RunId::from("run:one"),
                &first_request,
                &policy,
                1,
            )
            .expect("idempotent reserve"),
        first
    );
    assert_eq!(
        budgets
            .reserve(
                mission.clone(),
                RunId::from("run:too-large"),
                &AgentBudgetRequest {
                    reserved_tokens: 50,
                    reserved_tool_calls: 1,
                    reserved_runtime_millis: 100,
                    reserved_retries: 0,
                },
                &policy,
                1,
            )
            .expect_err("mission token budget")
            .code,
        "agent-budget-token-limit"
    );
    budgets
        .reserve(
            mission.clone(),
            RunId::from("run:two"),
            &AgentBudgetRequest {
                reserved_tokens: 40,
                reserved_tool_calls: 1,
                reserved_runtime_millis: 400,
                reserved_retries: 0,
            },
            &policy,
            1,
        )
        .expect("second reserve");
    assert_eq!(
        budgets
            .reserve(
                mission,
                RunId::from("run:three"),
                &AgentBudgetRequest {
                    reserved_tokens: 1,
                    reserved_tool_calls: 0,
                    reserved_runtime_millis: 1,
                    reserved_retries: 0,
                },
                &policy,
                1,
            )
            .expect_err("agent limit")
            .code,
        "agent-budget-agent-limit"
    );
    let charged = budgets
        .charge(&RunId::from("run:one"), 50, 1, 10)
        .expect("charge");
    assert_eq!(charged.consumed_tokens, 50);
    assert_eq!(
        budgets
            .charge(&RunId::from("run:one"), 11, 0, 11)
            .expect_err("token escrow")
            .code,
        "agent-budget-token-escrow"
    );
    let released = budgets
        .release(&RunId::from("run:one"), BudgetEscrowStatus::Completed)
        .expect("release");
    assert_eq!(released.status, BudgetEscrowStatus::Completed);

    let mut expiring =
        AgentBudgetManager::open(temporary.path().join("expiring.sqlite")).expect("expiring");
    expiring
        .reserve(
            MissionId::from("mission:expiry"),
            RunId::from("run:expiry"),
            &AgentBudgetRequest {
                reserved_tokens: 1,
                reserved_tool_calls: 0,
                reserved_runtime_millis: 10,
                reserved_retries: 0,
            },
            &policy,
            0,
        )
        .expect("expiring reserve");
    assert_eq!(
        expiring
            .charge(&RunId::from("run:expiry"), 1, 0, 10)
            .expect_err("expired")
            .code,
        "agent-budget-expired"
    );
    assert_eq!(
        expiring
            .get(&RunId::from("run:expiry"))
            .expect("expired escrow")
            .expect("escrow")
            .status,
        BudgetEscrowStatus::Expired
    );
}

struct ParallelHandler {
    barrier: Barrier,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl AgentTaskHandler for ParallelHandler {
    fn execute(
        &self,
        request: AgentExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<AgentResult, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::new(
                "cancelled",
                request.contract.run_id.to_string(),
            ));
        }
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.barrier.wait();
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(AgentResult {
            status: "completed".to_owned(),
            summary: request.contract.task_id.to_string(),
            artifacts: vec![],
            changed_files: vec![],
            evidence: vec![],
            warnings: vec![],
            errors: vec![],
            metrics: AgentExecutionMetrics {
                turns: 1,
                ..AgentExecutionMetrics::default()
            },
            confidence: 1.0,
            follow_up: vec![],
            model_tool_yield: None,
        })
    }
}

#[test]
fn bounded_executor_runs_two_four_and_eight_agents_in_parallel_with_stable_merge_order() {
    for count in [2_usize, 4, 8] {
        let parallel = count.min(4);
        let handler = Arc::new(ParallelHandler {
            barrier: Barrier::new(parallel),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let executor = BoundedAgentExecutor::new(handler.clone(), 4).expect("executor");
        let dispatches = (0..count)
            .rev()
            .map(|index| {
                let run_id = RunId::from(format!("run:{index}"));
                AgentDispatch {
                    cancellation: CancellationToken::new(),
                    request: AgentExecutionRequest {
                        session_id: AgentSessionId::from(format!("agent-session:{index}")),
                        contract: AgentTaskContract {
                            mission_id: MissionId::from("mission:parallel"),
                            task_id: TaskId::from(format!("task:{index}")),
                            run_id,
                            parent_run_id: None,
                            endpoint_id: AgentEndpointId::from("endpoint:coder"),
                            agent_definition_id: AgentDefinitionId::from("agent:coder"),
                            role: AgentRole::Coder,
                            objective: format!("task {index}"),
                            acceptance_criteria: vec!["returns result".to_owned()],
                            max_turns: 4,
                            deadline_millis: 1_000,
                            planning_budget: None,
                        },
                        context: AgentWorkingContext {
                            stable_instructions: "core".to_owned(),
                            dynamic_context: format!("task {index}"),
                            selected_item_ids: vec![],
                            excluded_item_ids: vec![],
                            token_cost: 2,
                            max_input_tokens: 100,
                            fingerprint: format!("context:{index}"),
                        },
                        steering_messages: vec![],
                        model_tools: vec![],
                        model_continuation: None,
                    },
                }
            })
            .collect::<Vec<_>>();
        let outcomes = executor.execute_batch(dispatches, 0).expect("execute");
        assert_eq!(outcomes.len(), count);
        assert_eq!(handler.peak.load(Ordering::SeqCst), parallel);
        assert!(outcomes.iter().all(|outcome| outcome.error.is_none()));
        let task_ids = outcomes
            .iter()
            .map(|outcome| outcome.task_id.to_string())
            .collect::<Vec<_>>();
        let mut sorted = task_ids.clone();
        sorted.sort();
        assert_eq!(task_ids, sorted);
    }
}

#[test]
fn model_agent_handler_executes_isolated_request_through_real_model_runtime() {
    let usage = ModelUsage {
        input_tokens: 3,
        cached_input_tokens: 1,
        output_tokens: 2,
        total_tokens: 5,
        ..ModelUsage::default()
    };
    let provider = Arc::new(FakeModelProvider::standard(vec![FakeScenario::text(
        &["review ", "complete"],
        usage,
    )]));
    let mut registry = ModelRegistry::new();
    registry.register(provider.clone()).expect("provider");
    let runtime = Arc::new(
        ModelRuntime::new(
            registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Low,
        )
        .expect("runtime"),
    );
    let handler = Arc::new(
        ModelAgentHandler::new(runtime, std::time::Duration::from_secs(1)).expect("handler"),
    );
    let executor = BoundedAgentExecutor::new(handler, 1).expect("executor");
    let outcomes = executor
        .execute_batch(
            vec![AgentDispatch {
                cancellation: CancellationToken::new(),
                request: AgentExecutionRequest {
                    session_id: AgentSessionId::from("agent-session:model"),
                    contract: AgentTaskContract {
                        mission_id: MissionId::from("mission:model"),
                        task_id: TaskId::from("task:review"),
                        run_id: RunId::from("run:model"),
                        parent_run_id: None,
                        endpoint_id: AgentEndpointId::from("endpoint:reviewer"),
                        agent_definition_id: AgentDefinitionId::from("agent:reviewer"),
                        role: AgentRole::Reviewer,
                        objective: "review the patch".to_owned(),
                        acceptance_criteria: vec!["report findings".to_owned()],
                        max_turns: 1,
                        deadline_millis: 1_000,
                        planning_budget: None,
                    },
                    context: AgentWorkingContext {
                        stable_instructions: "review only".to_owned(),
                        dynamic_context: "diff: auth.rs".to_owned(),
                        selected_item_ids: vec![],
                        excluded_item_ids: vec![],
                        token_cost: 4,
                        max_input_tokens: 100,
                        fingerprint: "context:model".to_owned(),
                    },
                    steering_messages: vec!["focus on regressions".to_owned()],
                    model_tools: vec![],
                    model_continuation: None,
                },
            }],
            0,
        )
        .expect("batch");
    let result = outcomes[0].result.as_ref().expect("result");
    assert_eq!(result.summary, "review complete");
    assert_eq!(result.metrics.input_tokens, 3);
    assert_eq!(result.metrics.cached_input_tokens, 1);
    assert_eq!(result.metrics.output_tokens, 2);
    let requests = provider.requests().expect("requests");
    assert!(requests[0].instructions.contains("<role-contract>"));
    assert!(requests[0].instructions.contains("独立代码审查员"));
    assert!(requests[0].instructions.contains("review only"));
}

#[test]
fn model_agent_handler_yields_typed_tool_calls_and_resumes_exact_continuation() {
    let usage = ModelUsage {
        input_tokens: 2,
        output_tokens: 1,
        total_tokens: 3,
        ..ModelUsage::default()
    };
    let mut registry = ModelRegistry::new();
    registry
        .register(Arc::new(FakeModelProvider::standard(vec![
            FakeScenario::tool("files.read", serde_json::json!({"path":"note.txt"}), usage),
            FakeScenario::text(&["tool continuation complete"], usage),
        ])))
        .expect("provider");
    let runtime = Arc::new(
        ModelRuntime::new(
            registry,
            ProviderId::from("fake"),
            ModelId::from("deterministic"),
            ReasoningLevel::Off,
        )
        .expect("runtime"),
    );
    let handler =
        ModelAgentHandler::new(runtime, std::time::Duration::from_secs(1)).expect("handler");
    let mut request = AgentExecutionRequest {
        session_id: AgentSessionId::from("agent-session:tool"),
        contract: AgentTaskContract {
            mission_id: MissionId::from("mission:tool"),
            task_id: TaskId::from("task:coder"),
            run_id: RunId::from("run:tool"),
            parent_run_id: None,
            endpoint_id: AgentEndpointId::from("endpoint:coder"),
            agent_definition_id: AgentDefinitionId::from("agent:coder"),
            role: AgentRole::Coder,
            objective: "read note".to_owned(),
            acceptance_criteria: vec!["use tool result".to_owned()],
            max_turns: 4,
            deadline_millis: 1_000,
            planning_budget: None,
        },
        context: AgentWorkingContext {
            stable_instructions: "core".to_owned(),
            dynamic_context: "task".to_owned(),
            selected_item_ids: vec![],
            excluded_item_ids: vec![],
            token_cost: 2,
            max_input_tokens: 100,
            fingerprint: "context:tool".to_owned(),
        },
        steering_messages: vec![],
        model_tools: vec![harness_model::ToolDefinition {
            name: "files.read".to_owned(),
            description: "read".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
            strict: true,
        }],
        model_continuation: None,
    };
    let first = handler
        .execute(request.clone(), CancellationToken::new())
        .expect("first turn");
    assert_eq!(first.status, "waiting-tool");
    let mut yielded = first.model_tool_yield.expect("tool yield");
    assert_eq!(yielded.calls.len(), 1);
    yielded.continuation.previous_response_id = yielded.response_id;
    yielded.continuation.next_input = vec![harness_model::ModelInputItem::ToolResult {
        call_id: yielded.calls[0].call_id.clone(),
        output: serde_json::json!({"content":"evidence"}),
    }];
    request.model_continuation = Some(yielded.continuation);
    let completed = handler
        .execute(request, CancellationToken::new())
        .expect("continuation");
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.summary, "tool continuation complete");
    assert_eq!(completed.metrics.turns, 2);
    assert_eq!(completed.metrics.tool_calls, 1);
}
