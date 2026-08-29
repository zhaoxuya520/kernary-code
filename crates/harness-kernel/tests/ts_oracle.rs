use harness_kernel::{MissionState, VersionedEvent, find_ready_node_ids, replay_mission};
use harness_testkit::{mission_fixture, verify_ts_oracle_manifest};
use harness_types::MissionId;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MissionOracle {
    schema_version: u32,
    fixture: String,
    events: Vec<VersionedEvent>,
    final_state: MissionState,
    invariants: MissionInvariants,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MissionInvariants {
    all_nodes_accepted: bool,
    mission_completed: bool,
    pending_approvals: usize,
}

#[test]
fn rust_replay_matches_typescript_oracle() {
    verify_ts_oracle_manifest().expect("TS oracle manifest 哈希必须匹配");
    let raw: serde_json::Value =
        serde_json::from_str(mission_fixture()).expect("Mission oracle 必须是合法 JSON");
    let fixture: MissionOracle =
        serde_json::from_str(mission_fixture()).expect("Mission oracle schema 必须可解析");
    assert_eq!(fixture.schema_version, 1);
    assert_eq!(fixture.fixture, "mission-parallel-approval-join");

    let replayed = replay_mission(MissionId::from("mission:oracle"), &fixture.events)
        .expect("Rust reducer 应能重放 TS 事件");
    assert_eq!(replayed, fixture.final_state);
    assert_eq!(
        serde_json::to_value(&replayed).expect("state JSON"),
        raw["finalState"]
    );
    assert!(fixture.invariants.all_nodes_accepted);
    assert!(fixture.invariants.mission_completed);
    assert_eq!(fixture.invariants.pending_approvals, 0);
    assert!(find_ready_node_ids(&replayed).is_empty());

    let after_plan = replay_mission(MissionId::from("mission:oracle"), &fixture.events[..2])
        .expect("Plan revision 应可重放");
    assert_eq!(
        find_ready_node_ids(&after_plan),
        ["a", "b"].map(harness_types::TaskId::from)
    );
    let before_join = replay_mission(MissionId::from("mission:oracle"), &fixture.events[..10])
        .expect("Join 前状态应可重放");
    assert_eq!(
        find_ready_node_ids(&before_join),
        [harness_types::TaskId::from("join")]
    );
}

#[test]
fn unknown_event_type_is_rejected_by_runtime_schema() {
    let result =
        serde_json::from_str::<harness_kernel::DomainEvent>(r#"{"type":"mission.teleported"}"#);
    assert!(result.is_err());

    let unknown_field = serde_json::from_str::<harness_kernel::DomainEvent>(
        r#"{"type":"mission.completed","unexpected":true}"#,
    );
    assert!(unknown_field.is_err());
}
