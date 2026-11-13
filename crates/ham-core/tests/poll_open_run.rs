//! While a run is open the harness shard (shard 0) is read from its start on every cycle.
//!
//! A shard's cursor is the largest event id read, and the server returns the ids above it, so an event committed later with a smaller id
//! is never returned again. On the harness shard that event can be `session.started` (the live A1 proof), and in principle any other
//! harness event: a completion, a link. The shard holds about ten small events, so reading all of it each cycle costs little and makes the
//! client immune to every id order on it. The plan counts only ids it has not seen as activity, so these reads do not keep the poll hot.
//! After the run has ended the plan keeps reading from the start for a few more cycles (a writer may still be finishing), then returns to
//! the sweep cadence that `poll_sweep.rs` pins.
#![allow(clippy::unwrap_used)]

use ham_core::poll::{
    CEILING_BASES, END_GRACE_CYCLES, FULL_READ_MAX_EVENTS, GRACE_EMPTY_CYCLES, PollPlan,
    SWEEP_EVERY, SWEEP_UNKNOWN_EVERY,
};
use ham_core::state::{Applied, RunState};
use harness_protocol::api::v1::EventsPage;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};
use std::time::Duration;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

fn ev(id: &str, ty: &str, authority: &str, payload: Value) -> Envelope {
    let mut v = json!({
        "schemaVersion": 1, "eventId": id, "runId": RUN, "seq": 1,
        "ts": "2026-10-17T10:17:14Z", "type": ty, "authority": authority,
        "payload": {"kind": "inline", "value": payload}
    });
    if authority == "agent" {
        v["agentId"] = json!("direct");
    }
    serde_json::from_value(v).unwrap()
}

fn harness(id: &str, ty: &str, payload: Value) -> Envelope {
    ev(id, ty, "harness", payload)
}

fn created() -> Envelope {
    harness(
        "01M54NV70TDDBYBP3FF9SJ9HY3",
        "run.created",
        json!({"target": {"mode": "s3-only"}}),
    )
}

/// The two start events of the failed A1 leg: one millisecond, `session.started` BELOW `goal.status`.
fn active() -> Envelope {
    harness(
        "01M54NV7WZNFNY8X64P2JYSD13",
        "goal.status",
        json!({"status": "active", "phase": "green"}),
    )
}

fn started() -> Envelope {
    harness(
        "01M54NV7WZDM9X50G7ZW811TYT",
        "session.started",
        json!({"agentId": "direct", "engine": "cline", "modelId": SONNET}),
    )
}

/// The delivery events, ids above the start events.
fn delivering() -> Envelope {
    harness(
        "01M54NWP1HAE98T6YBSBBX75Q4",
        "goal.status",
        json!({"status": "active", "phase": "delivering"}),
    )
}

fn completed() -> Envelope {
    harness(
        "01M54NWP1NX6WTJJX1MW2DNYDA",
        "run.completed",
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    )
}

fn base() -> Duration {
    Duration::from_secs(1)
}

fn at(cycle: u32) -> Duration {
    Duration::from_secs(u64::from(cycle))
}

fn page(events: Vec<Envelope>) -> EventsPage {
    let next_after = events.iter().map(|e| e.event_id.clone()).max();
    EventsPage { events, next_after }
}

fn state_with(events: Vec<Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    s.apply_page(0, page(events));
    s
}

fn shard0(cursors: &[(u8, Option<String>)]) -> Option<String> {
    cursors.iter().find(|(s, _)| *s == 0).unwrap().1.clone()
}

/// What the real server returns for shard 0: the events of `store` whose ids are above `after`, in id order.
fn server_page(store: &[Envelope], after: Option<&str>) -> EventsPage {
    let mut events: Vec<Envelope> = store
        .iter()
        .filter(|e| after.is_none_or(|a| e.event_id.as_str() > a))
        .cloned()
        .collect();
    events.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    let next_after = events.last().map(|e| e.event_id.clone());
    EventsPage { events, next_after }
}

/// One cycle of the real client against `store`: plan the reads, answer shard 0 from the store, record, apply.
fn cycle(plan: &mut PollPlan, state: &mut RunState, store: &[Envelope], cycle: u32) -> Duration {
    let cursors = plan.cursors(state, at(cycle));
    let pages: Vec<(u8, EventsPage)> = cursors
        .iter()
        .map(|(s, after)| {
            if *s == 0 {
                (*s, server_page(store, after.as_deref()))
            } else {
                (
                    *s,
                    EventsPage {
                        events: vec![],
                        next_after: None,
                    },
                )
            }
        })
        .collect();
    plan.record(at(cycle), &pages);
    for (shard, p) in pages {
        state.apply_page(shard, p);
    }
    plan.interval()
}

// ---- the state knows the run has ended --------------------------------------------------------------------------------------

#[test]
fn the_state_knows_when_the_run_has_ended() {
    assert!(!state_with(vec![created(), active(), delivering()]).ended());
    assert!(state_with(vec![created(), active(), completed()]).ended());
    let aborted = harness(
        "01M54NWP1NX6WTJJX1MW2DNYDB",
        "run.aborted",
        json!({"status": "FAILED", "error": "States.TaskFailed", "cause": "x"}),
    );
    assert!(state_with(vec![created(), aborted]).ended());
}

#[test]
fn a_run_completed_from_an_agent_does_not_end_the_run() {
    let mut s = RunState::new(RUN);
    let forged = ev(
        "01M54NWP1NX6WTJJX1MW2DNYDC",
        "run.completed",
        "agent",
        json!({"status": "complete"}),
    );
    assert!(matches!(s.apply(forged), Applied::Rejected(_)));
    assert!(!s.ended(), "only the harness ends a run (invariant 4)");
}

// ---- an open run: shard 0 from its start, every cycle -----------------------------------------------------------------------

#[test]
fn while_the_run_is_open_the_harness_shard_is_read_from_its_start_every_cycle() {
    for named in [false, true] {
        let mut plan = PollPlan::new(base());
        let mut events = vec![created(), active()];
        if named {
            events.push(started());
        }
        let state = state_with(events);
        assert_eq!(state.model_seen(), named);
        for n in 1..=SWEEP_EVERY * 2 {
            let cursors = plan.cursors(&state, at(n));
            assert_eq!(
                shard0(&cursors),
                None,
                "cycle {n} (model named: {named}) reads shard 0 from its start"
            );
        }
    }
}

#[test]
fn only_the_harness_shard_is_read_from_its_start_the_agent_shards_keep_their_cursors() {
    let mut plan = PollPlan::new(base());
    let mut state = state_with(vec![created(), active(), started()]);
    let agent_shard = harness_protocol::store::event_shard(
        harness_protocol::envelope::Authority::Agent,
        Some("direct"),
    );
    state.apply_page(
        agent_shard,
        page(vec![ev(
            "01M54NV8S024CG8EZX4406SS4W",
            "turn.started",
            "agent",
            json!({"turn": 1}),
        )]),
    );
    // Cycle 1 reads every shard; the plan learns the agent's shard from session.started on the pages it records.
    let first = plan.cursors(&state, at(1));
    plan.record(at(1), &[(0, page(vec![started()]))]);
    assert_eq!(first.len(), 32);
    let second = plan.cursors(&state, at(2));
    assert_eq!(shard0(&second), None);
    let agent = second.iter().find(|(s, _)| *s == agent_shard).unwrap();
    assert_eq!(agent.1.as_deref(), Some("01M54NV8S024CG8EZX4406SS4W"));
}

#[test]
fn an_event_that_lands_below_the_cursor_is_found_on_the_next_cycle_whatever_it_is() {
    // The failed A1 leg: goal.status (larger id) is committed and read first; session.started (smaller id) a few ms later.
    for late in [started(), completed()] {
        let mut plan = PollPlan::new(base());
        let mut state = RunState::new(RUN);
        let mut store = vec![created(), delivering()];
        cycle(&mut plan, &mut state, &store, 1);
        assert_eq!(state.cursor(0), Some(delivering().event_id.as_str()));
        // `completed` has the larger id of the two; make the late one smaller than the cursor in both cases.
        let mut late = late;
        late.event_id = "01M54NV7WZDM9X50G7ZW811TYT".into();
        store.push(late.clone());
        cycle(&mut plan, &mut state, &store, 2);
        assert!(
            state.view().events == 3,
            "the late {} is held after one more cycle",
            late.event_type.as_str()
        );
        assert_eq!(
            state.cursor(0),
            Some(delivering().event_id.as_str()),
            "the cursor never moves back"
        );
    }
}

#[test]
fn a_late_completion_is_shown_as_soon_as_it_is_read() {
    // Everything but the completion was read; the completion is committed with an id below the cursor.
    let mut plan = PollPlan::new(base());
    let mut state = RunState::new(RUN);
    let mut store = vec![created(), active(), delivering()];
    cycle(&mut plan, &mut state, &store, 1);
    assert!(!state.view().completed);
    let mut late = completed();
    late.event_id = "01M54NV7WZDM9X50G7ZW811TYS".into(); // below the cursor
    store.push(late);
    cycle(&mut plan, &mut state, &store, 2);
    let view = state.view();
    assert!(view.completed, "the completion is not hidden by the cursor");
    assert_eq!(view.result.unwrap().label, "Cline reported done");
}

#[test]
fn the_reads_do_not_keep_the_poll_hot_but_a_new_or_a_late_id_does() {
    let mut plan = PollPlan::new(base());
    let mut state = RunState::new(RUN);
    let mut store = vec![created(), active()];
    let mut wait = base();
    // Nothing new: the wait grows to the ceiling although every cycle returns the whole shard.
    for n in 1..=GRACE_EMPTY_CYCLES + 8 {
        wait = cycle(&mut plan, &mut state, &store, n);
    }
    assert_eq!(wait, base() * CEILING_BASES, "a quiet open run backs off");
    // A late id that the state does not hold counts as activity once.
    let mut late = started();
    late.event_id = "01M54NV7WZDM9X50G7ZW811TYT".into();
    store.push(late);
    let next = GRACE_EMPTY_CYCLES + 9;
    assert_eq!(cycle(&mut plan, &mut state, &store, next), base());
    let mut again = base();
    for n in next + 1..=next + GRACE_EMPTY_CYCLES + 8 {
        again = cycle(&mut plan, &mut state, &store, n);
    }
    assert_eq!(
        again,
        base() * CEILING_BASES,
        "the same id is not activity twice"
    );
}

// ---- after the run has ended ------------------------------------------------------------------------------------------------

#[test]
fn after_the_end_is_seen_the_harness_shard_is_read_from_the_start_for_a_grace_then_on_the_sweep_cadence()
 {
    let mut plan = PollPlan::new(base());
    let open = state_with(vec![created(), active(), delivering()]);
    // Two open cycles first: the plan sees the run open, then sees it end at cycle 3.
    plan.cursors(&open, at(1));
    plan.cursors(&open, at(2));
    let ended = state_with(vec![created(), active(), delivering(), completed()]);
    assert!(
        !ended.model_seen(),
        "no model is named: the sweep cadence is every {SWEEP_UNKNOWN_EVERY} cycles"
    );
    for n in 3..3 + END_GRACE_CYCLES + SWEEP_UNKNOWN_EVERY * 4 {
        let in_grace = n < 3 + END_GRACE_CYCLES;
        let sweep = n % SWEEP_UNKNOWN_EVERY == 0;
        let c = plan.cursors(&ended, at(n));
        assert_eq!(
            shard0(&c).is_none(),
            in_grace || sweep,
            "cycle {n}: grace {in_grace}, sweep {sweep}"
        );
    }
}

#[test]
fn a_run_that_is_over_when_it_is_first_seen_has_no_grace() {
    let mut plan = PollPlan::new(base());
    let ended = state_with(vec![created(), active(), completed()]);
    let c = plan.cursors(&ended, at(1));
    // Cycle 1 of a real watch reads from the start anyway (no cursor is held); here a cursor is held, and the cycle is not a sweep.
    assert!(shard0(&c).is_some());
}

// ---- a shard that is not small ----------------------------------------------------------------------------------------------

#[test]
fn a_harness_shard_with_many_events_falls_back_to_the_cursor() {
    let mut plan = PollPlan::new(base());
    let mut state = RunState::new(RUN);
    // More distinct harness events than a run ever writes: the plan stops reading the whole shard.
    let many: Vec<Envelope> = (0..=FULL_READ_MAX_EVENTS)
        .map(|i| {
            harness(
                &format!("01M54NV7WZ{:016}", i + 1),
                "goal.status",
                json!({"status": "active"}),
            )
        })
        .collect();
    let cursors = plan.cursors(&state, at(1));
    assert_eq!(shard0(&cursors), None);
    plan.record(at(1), &[(0, page(many.clone()))]);
    state.apply_page(0, page(many));
    let c = plan.cursors(&state, at(2));
    assert!(
        shard0(&c).is_some(),
        "{} events on the harness shard: read after the cursor",
        FULL_READ_MAX_EVENTS + 1
    );
}
