use harness_kernel::{
    DomainEvent, EffectIntent, MissionCommand, MissionState, decide_mission, reduce_mission,
};
use harness_testkit::{mission_command_fixture, verify_ts_oracle_manifest};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandOracle {
    schema_version: u32,
    fixture: String,
    effect_vocabulary: String,
    steps: Vec<DecisionStep>,
    final_state: MissionState,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DecisionStep {
    command: MissionCommand,
    state_version_before: u64,
    events: Vec<DomainEvent>,
    effects: Vec<EffectIntent>,
    state_version_after: u64,
}

#[test]
fn rust_decisions_match_typescript_command_oracle() {
    verify_ts_oracle_manifest().expect("新增 oracle 也必须通过 manifest 哈希");
    let fixture: CommandOracle =
        serde_json::from_str(mission_command_fixture()).expect("Command oracle schema 必须可解析");
    assert_eq!(fixture.schema_version, 1);
    assert_eq!(fixture.fixture, "mission-command-decisions");
    assert_eq!(fixture.effect_vocabulary, "production-normalized-v1");

    let mut state = MissionState::empty(harness_types::MissionId::from("mission:command-oracle"));
    for step in fixture.steps {
        assert_eq!(state.version, step.state_version_before);
        let decision =
            decide_mission(&state, &step.command).expect("TS 有效 Command 应被 Rust 接受");
        assert_eq!(decision.events, step.events);
        assert_eq!(decision.effects, step.effects);
        for event in decision.events {
            state = reduce_mission(&state, &event).expect("Rust 自己产生的 Event 应可 reduce");
        }
        assert_eq!(state.version, step.state_version_after);
    }
    assert_eq!(state, fixture.final_state);
}
