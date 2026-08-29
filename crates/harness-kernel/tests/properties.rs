use harness_kernel::{
    GoalRevision, MissionCommand, MissionState, SessionCommand, SessionState, decide_mission,
    decide_session, reduce_mission, reduce_session,
};
use harness_types::{ActorId, GoalRevisionId, MissionId, ProjectId, SessionId};
use proptest::prelude::*;

proptest! {
    #[test]
    fn arbitrary_non_empty_goal_round_trips_through_decision(goal in "[^\\s].{0,63}") {
        let state = MissionState::empty(MissionId::from("mission:property"));
        let command = MissionCommand::CreateMission {
            mission_id: MissionId::from("mission:property"),
            project_id: ProjectId::from("project:property"),
            goal: goal.clone(),
        };
        let decision = decide_mission(&state, &command).expect("非空 Goal 应接受");
        prop_assert_eq!(decision.events.len(), 1);
        let next = reduce_mission(&state, &decision.events[0]).expect("产生的 Event 应可 reduce");
        prop_assert_eq!(next.goal, goal);
        prop_assert_eq!(next.version, 1);
    }

    #[test]
    fn goal_revision_chain_preserves_every_revision(
        goals in prop::collection::vec("[^\\s].{0,31}", 1..32)
    ) {
        let mut state = SessionState::empty(SessionId::from("session:property"));
        let create = SessionCommand::CreateSession {
            session_id: SessionId::from("session:property"),
            project_id: ProjectId::from("project:property"),
        };
        for event in decide_session(&state, &create).expect("Session 应创建") {
            state = reduce_session(&state, &event).expect("Session Event 应可 reduce");
        }

        let mut parent = None;
        for (index, text) in goals.iter().enumerate() {
            let id = GoalRevisionId::from(format!("goal:{}", index + 1));
            let revision = GoalRevision {
                id: id.clone(),
                parent_revision_id: parent.clone(),
                text: text.clone(),
                created_by: ActorId::from("user:property"),
                reason: "property".to_owned(),
                created_at_millis: index as i64,
            };
            let command = SessionCommand::ReviseGoal { revision };
            for event in decide_session(&state, &command).expect("线性 revision 应接受") {
                state = reduce_session(&state, &event).expect("Goal Event 应可 reduce");
            }
            parent = Some(id);
        }

        prop_assert_eq!(state.goal.revisions.len(), goals.len());
        prop_assert_eq!(state.goal.current_revision_id, parent);
        for (index, text) in goals.iter().enumerate() {
            let id = GoalRevisionId::from(format!("goal:{}", index + 1));
            prop_assert_eq!(&state.goal.revisions[&id].text, text);
        }
    }
}
