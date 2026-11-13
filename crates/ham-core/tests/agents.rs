//! The dashboard's agent table: the agents of the run's view, grouped Working until the run is
//! complete and Completed after, ordered by group then id.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::agents::{AgentGroup, group_counts, rows};
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";

fn env(n: u64, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ{n:016}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: 1,
        ts: "2026-09-25T02:00:00Z".into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    }
}

fn state() -> RunState {
    let mut s = RunState::new(RUN);
    let events = vec![
        env(1, EventType::RunCreated, None, json!({})),
        env(
            2,
            EventType::SessionStarted,
            None,
            json!({"agentId": "b-agent", "engine": "cline", "modelId": "m-sonnet", "contextTokens": 1_000_000}),
        ),
        env(
            3,
            EventType::SessionStarted,
            None,
            json!({"agentId": "a-agent", "engine": "cline", "modelId": "m-opus"}),
        ),
        env(
            4,
            EventType::TurnStarted,
            Some("b-agent"),
            json!({"turn": 1}),
        ),
        env(
            5,
            EventType::ToolSummary,
            Some("b-agent"),
            json!({"toolCallId": "call-1", "title": "editor: calc.py", "kind": "edit", "status": "pending"}),
        ),
    ];
    for e in events {
        s.apply(e);
    }
    s
}

#[test]
fn rows_carry_what_the_session_and_the_turns_said_in_id_order() {
    let v = state().view();
    let r = rows(&v);
    let ids: Vec<&str> = r.iter().map(|a| a.agent_id.as_str()).collect();
    assert_eq!(ids, ["a-agent", "b-agent"]);
    let b = &r[1];
    assert_eq!(b.engine.as_deref(), Some("cline"));
    assert_eq!(b.model_id.as_deref(), Some("m-sonnet"));
    assert_eq!(b.turns, 1);
    assert_eq!(b.last, "editor: calc.py");
    assert_eq!(r[0].turns, 0);
    assert_eq!(v.windows(), [("m-sonnet".to_string(), 1_000_000)]);
}

#[test]
fn every_agent_is_working_until_the_run_is_complete_and_completed_after() {
    let mut s = state();
    let working = rows(&s.view());
    assert!(working.iter().all(|a| a.group == AgentGroup::Working));
    assert_eq!(group_counts(&working), [(AgentGroup::Working, 2)]);
    s.apply(env(
        9,
        EventType::GoalStatus,
        None,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    let done = rows(&s.view());
    assert!(done.iter().all(|a| a.group == AgentGroup::Completed));
    assert_eq!(group_counts(&done), [(AgentGroup::Completed, 2)]);
    assert_eq!(AgentGroup::Working.title(), "Working");
    assert_eq!(AgentGroup::Completed.title(), "Completed");
}

#[test]
fn a_run_with_no_session_has_no_rows() {
    let mut s = RunState::new(RUN);
    s.apply(env(1, EventType::RunCreated, None, json!({})));
    assert!(rows(&s.view()).is_empty());
    assert!(group_counts(&[]).is_empty());
}
