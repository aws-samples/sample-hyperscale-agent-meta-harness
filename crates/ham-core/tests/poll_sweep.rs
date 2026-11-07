//! A shard's cursor is the largest event id read from it, and the server returns the ids above it. An event that is written later with a
//! smaller id is never returned again once a bigger id was read.
//! The live A1 proof lost `session.started` that way in 1 of 10 legs: the card never named the model. The plan therefore reads the harness
//! shard again from its start every few cycles while no session has named its model, and now and then otherwise; a re-read changes nothing
//! the state already holds (events are keyed by id).
//!
//! This file pins that cadence for a run that has ENDED (a `run.completed` is held). While a run is open the plan reads the harness shard
//! from its start on every cycle (EVT1): `poll_open_run.rs`.
#![allow(clippy::unwrap_used)]

use ham_core::poll::{PollPlan, SWEEP_EVERY, SWEEP_UNKNOWN_EVERY};
use ham_core::state::RunState;
use harness_protocol::api::v1::EventsPage;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};
use std::time::Duration;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn ev(id: &str, ty: &str, agent: Option<&str>, payload: Value) -> Envelope {
    let mut v = json!({
        "schemaVersion": 1, "eventId": id, "runId": RUN, "seq": 1,
        "ts": "2026-10-16T08:15:00Z", "type": ty, "authority": if agent.is_some() { "agent" } else { "harness" },
        "payload": {"kind": "inline", "value": payload}
    });
    if let Some(a) = agent {
        v["agentId"] = json!(a);
    }
    serde_json::from_value(v).unwrap()
}

fn created() -> Envelope {
    ev(
        "01M5A2R7K3C9XJ4D8E6F000010",
        "run.created",
        None,
        json!({"target": {"mode": "s3-only"}}),
    )
}

fn status() -> Envelope {
    ev(
        "01M5A2R7K3C9XJ4D8E6F000030",
        "goal.status",
        None,
        json!({"status": "active", "phase": "green"}),
    )
}

/// The end of the run: the harness's own `run.completed`, written last, so it has the largest id of the shard.
fn completed() -> Envelope {
    ev(
        "01M5A2R7K3C9XJ4D8E6F000040",
        "run.completed",
        None,
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    )
}

fn started(id: &str, model: Option<&str>) -> Envelope {
    let mut p = json!({"agentId": "direct", "engine": "cline"});
    if let Some(m) = model {
        p["modelId"] = json!(m);
    }
    ev(id, "session.started", None, p)
}

fn state_with(events: Vec<Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    let next_after = events.iter().map(|e| e.event_id.clone()).max();
    s.apply_page(0, EventsPage { events, next_after });
    s
}

fn shard0(cursors: &[(u8, Option<String>)]) -> Option<String> {
    cursors.iter().find(|(s, _)| *s == 0).unwrap().1.clone()
}

fn base() -> Duration {
    Duration::from_secs(1)
}

#[test]
fn the_state_knows_when_a_session_has_named_its_model() {
    assert!(!state_with(vec![created(), status()]).model_seen());
    assert!(!state_with(vec![started("01M5A2R7K3C9XJ4D8E6F000020", None)]).model_seen());
    assert!(
        state_with(vec![started(
            "01M5A2R7K3C9XJ4D8E6F000020",
            Some("global.anthropic.claude-sonnet-5-5")
        )])
        .model_seen()
    );
}

#[test]
fn the_first_cycle_reads_every_shard_from_the_start() {
    let mut plan = PollPlan::new(base());
    let cursors = plan.cursors(&RunState::new(RUN), Duration::ZERO);
    assert_eq!(cursors.len(), 32);
    assert!(cursors.iter().all(|(_, c)| c.is_none()));
}

#[test]
fn once_the_run_has_ended_and_no_model_is_named_the_harness_shard_is_read_again_from_its_start_every_few_cycles()
 {
    let mut plan = PollPlan::new(base());
    let state = state_with(vec![created(), status(), completed()]);
    let mut swept = Vec::new();
    for cycle in 1..=SWEEP_UNKNOWN_EVERY * 3 {
        let cursors = plan.cursors(&state, Duration::from_secs(u64::from(cycle)));
        if shard0(&cursors).is_none() {
            swept.push(cycle);
        } else {
            assert_eq!(
                shard0(&cursors).as_deref(),
                Some("01M5A2R7K3C9XJ4D8E6F000040")
            );
        }
    }
    assert_eq!(
        swept,
        vec![
            SWEEP_UNKNOWN_EVERY,
            SWEEP_UNKNOWN_EVERY * 2,
            SWEEP_UNKNOWN_EVERY * 3
        ],
        "every {SWEEP_UNKNOWN_EVERY} cycles"
    );
}

#[test]
fn a_sweep_reads_only_the_harness_shard_from_the_start() {
    let mut plan = PollPlan::new(base());
    let state = state_with(vec![created(), status()]);
    for cycle in 1..SWEEP_UNKNOWN_EVERY {
        plan.cursors(&state, Duration::from_secs(u64::from(cycle)));
    }
    let cursors = plan.cursors(&state, Duration::from_secs(u64::from(SWEEP_UNKNOWN_EVERY)));
    for (shard, cursor) in cursors {
        if shard != 0 {
            // Another shard keeps the cursor it has (none is held in this state, so none is sent).
            assert_eq!(cursor, None);
        }
    }
}

#[test]
fn once_the_run_has_ended_and_a_model_is_named_the_sweep_slows_to_the_long_interval() {
    let mut plan = PollPlan::new(base());
    let state = state_with(vec![
        created(),
        started(
            "01M5A2R7K3C9XJ4D8E6F000020",
            Some("global.anthropic.claude-sonnet-5-5"),
        ),
        status(),
        completed(),
    ]);
    let mut swept = Vec::new();
    for cycle in 1..=SWEEP_EVERY * 2 {
        let cursors = plan.cursors(&state, Duration::from_secs(u64::from(cycle)));
        if shard0(&cursors).is_none() {
            swept.push(cycle);
        }
    }
    assert_eq!(swept, vec![SWEEP_EVERY, SWEEP_EVERY * 2]);
}

#[test]
fn an_event_with_a_smaller_id_than_the_cursor_is_found_by_the_sweep_and_the_state_keeps_one_copy_of_the_rest()
 {
    let mut plan = PollPlan::new(base());
    let mut state = state_with(vec![created(), status()]);
    // The server returns ids above the cursor: the late event (id ...020 < cursor ...030) is not in the page after the cursor.
    let after_cursor = vec![];
    state.apply_page(
        0,
        EventsPage {
            events: after_cursor,
            next_after: None,
        },
    );
    assert!(!state.model_seen());
    // The sweep asks from the start and the server returns every event of the shard, the late one in its place.
    let all = vec![
        created(),
        started(
            "01M5A2R7K3C9XJ4D8E6F000020",
            Some("global.anthropic.claude-sonnet-5-5"),
        ),
        status(),
    ];
    for cycle in 1..=SWEEP_UNKNOWN_EVERY {
        let cursors = plan.cursors(&state, Duration::from_secs(u64::from(cycle)));
        if shard0(&cursors).is_none() {
            let next_after = all.iter().map(|e| e.event_id.clone()).max();
            state.apply_page(
                0,
                EventsPage {
                    events: all.clone(),
                    next_after,
                },
            );
        }
    }
    assert!(state.model_seen());
    assert_eq!(
        state.view().events,
        3,
        "the events already held are not counted twice"
    );
    assert_eq!(
        state.cursor(0),
        Some("01M5A2R7K3C9XJ4D8E6F000030"),
        "the cursor never moves back"
    );
}
