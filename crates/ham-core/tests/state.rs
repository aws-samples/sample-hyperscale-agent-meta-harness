//! Reducers over the event log.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::state::{Applied, RunState};
use harness_protocol::api::v1::EventsPage;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::event_shard;
use proptest::prelude::*;
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";

fn ulid(n: u64) -> String {
    // Monotonic ULIDs: 10 time chars from a fixed epoch plus a 16-char counter.
    const A: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut s = String::from("01J9ZZZZZZ");
    let mut v = n;
    let mut tail = [b'0'; 16];
    for i in (0..16).rev() {
        tail[i] = A[(v % 32) as usize];
        v /= 32;
    }
    s.push_str(std::str::from_utf8(&tail).unwrap());
    s
}

fn ev(n: u64, t: EventType, authority: Authority, agent: Option<&str>, payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: ulid(n),
        run_id: RUN.into(),
        task_id: Some("T1".into()),
        agent_id: agent.map(str::to_string),
        seq: n,
        ts: "2026-09-25T02:00:00Z".into(),
        event_type: t,
        authority,
        payload: Payload::Inline { value: payload },
    }
}

/// A run-level harness event: no task id, as run-tasks and delivery publish them.
fn h(n: u64, t: EventType, p: Value) -> Envelope {
    let mut e = ev(n, t, Authority::Harness, None, p);
    e.task_id = None;
    e
}
fn a(n: u64, t: EventType, p: Value) -> Envelope {
    ev(n, t, Authority::Agent, Some("A1"), p)
}

/// A complete run of the direct line: Cline works, the harness delivers and says how Cline stopped.
fn happy_run() -> Vec<Envelope> {
    vec![
        h(
            1,
            EventType::RunCreated,
            json!({"target": "cc:hamh-e2e-python-calc-20260925"}),
        ),
        h(
            2,
            EventType::SessionStarted,
            json!({"agentId": "A1", "engine": "cline", "modelId": "us.anthropic.claude-sonnet-5"}),
        ),
        a(3, EventType::TurnStarted, json!({"turn": 1})),
        a(
            4,
            EventType::ToolSummary,
            json!({"toolCallId": "call-1", "title": "Edit src/calc.py", "kind": "edit", "status": "pending"}),
        ),
        a(
            5,
            EventType::TurnEnded,
            json!({"turn": 1, "class": "completed"}),
        ),
        h(
            6,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "delivering"}),
        ),
        h(
            7,
            EventType::DeliveryPr,
            json!({"url": "https://git-codecommit.example/pr/7"}),
        ),
        h(
            8,
            EventType::DeliveryReady,
            json!({"artifactId": "ART1", "sha256": "a".repeat(64), "sizeBytes": 1234, "prUrl": "https://git-codecommit.example/pr/7"}),
        ),
        h(
            9,
            EventType::GoalStatus,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        h(10, EventType::RunCompleted, json!({"status": "complete"})),
    ]
}

fn state_of(events: impl IntoIterator<Item = Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s
}

#[test]
fn a_complete_run_shows_status_and_links() {
    let v = state_of(happy_run()).view();
    assert_eq!(v.status, Some(GoalStatus::Complete));
    assert_eq!(v.phase, Some(GoalPhase::Delivering));
    assert_eq!(
        v.pr_url.as_deref(),
        Some("https://git-codecommit.example/pr/7")
    );
    let zip = v.zip.expect("zip link");
    assert_eq!((zip.artifact_id.as_str(), zip.size_bytes), ("ART1", 1234));
    assert!(v.completed);
    assert_eq!(v.agents["A1"].engine.as_deref(), Some("cline"));
    assert_eq!(v.agents["A1"].turns, 1);
}

#[test]
fn run_created_alone_is_active_planning() {
    let v = state_of(happy_run().into_iter().take(1)).view();
    assert_eq!(v.status, Some(GoalStatus::Active));
    assert!(v.pr_url.is_none() && v.zip.is_none() && !v.completed);
}

#[test]
fn agent_authored_status_and_delivery_events_change_nothing() {
    let mut events = happy_run().into_iter().take(5).collect::<Vec<_>>();
    events.push(a(50, EventType::GoalStatus, json!({"status": "complete"})));
    events.push(a(51, EventType::DeliveryReady, json!({"artifactId": "AGENT-ART", "sha256": "b".repeat(64), "sizeBytes": 1, "prUrl": "https://example.test/other-pr"})));
    events.push(a(
        52,
        EventType::DeliveryPr,
        json!({"url": "https://example.test/other-pr"}),
    ));
    events.push(a(
        53,
        EventType::RunCompleted,
        json!({"status": "complete"}),
    ));
    let s = state_of(events);
    let v = s.view();
    assert_eq!(v.status, Some(GoalStatus::Active));
    assert!(v.pr_url.is_none() && v.zip.is_none() && !v.completed);
    assert_eq!(v.untrusted, 4);
    assert_eq!(s.rejected().len(), 4);
}

#[test]
fn apply_reports_accepted_duplicate_rejected_and_other_run() {
    let mut s = RunState::new(RUN);
    let e = h(1, EventType::RunCreated, json!({}));
    assert_eq!(s.apply(e.clone()), Applied::Accepted);
    assert_eq!(s.apply(e.clone()), Applied::Duplicate);
    let mut bad = h(2, EventType::GoalStatus, json!({}));
    bad.event_id = "not-a-ulid".into();
    assert!(matches!(s.apply(bad), Applied::Rejected(_)));
    let mut other = h(3, EventType::DeliveryPr, json!({}));
    other.run_id = "01JOTHERRUNXXXXXXXXXXXXXXX".into();
    assert_eq!(s.apply(other), Applied::OtherRun);
}

#[test]
fn agent_text_is_sanitized_in_the_view() {
    let s = state_of([
        h(1, EventType::RunCreated, json!({})),
        a(
            2,
            EventType::ToolSummary,
            json!({"title": "Open \u{1b}]8;;https://example.test\u{7}here\u{1b}]8;;\u{7}\u{1b}]0;t\u{7}"}),
        ),
    ]);
    let line = &s.view().agents["A1"].last_summary;
    assert_eq!(line, "Open here");
}

/// Agentd writes `tool.summary` with `title`, `kind`, `status` and `toolCallId` (`collector.rs`), never `text` or `summary`.
#[test]
fn a_tool_summary_in_the_emitters_shape_shows_its_title_in_the_agents_last_cell() {
    let s = state_of([
        h(1, EventType::RunCreated, json!({})),
        a(
            2,
            EventType::ToolSummary,
            json!({"toolCallId": "tooluse_gniAK1LXeq4zX1OO6V6nzF", "title": "run_commands: pytest -q", "kind": "execute", "status": "pending"}),
        ),
    ]);
    assert_eq!(
        s.view().agents["A1"].last_summary,
        "run_commands: pytest -q"
    );
}

#[test]
fn goal_blocked_is_shown() {
    let s = state_of([
        h(1, EventType::RunCreated, json!({})),
        h(
            2,
            EventType::GoalBlocked,
            json!({"reason": "the session ended \u{1b}[31mbefore any work was saved"}),
        ),
    ]);
    let v = s.view();
    assert_eq!(v.status, Some(GoalStatus::Blocked));
    assert_eq!(
        v.blocked.as_ref().map(|b| b.reason.as_str()),
        Some("the session ended before any work was saved")
    );
}

#[test]
fn terminal_status_is_not_reopened_by_later_status_events() {
    let mut events = happy_run();
    events.push(h(
        99,
        EventType::GoalStatus,
        json!({"status": "active", "phase": "green"}),
    ));
    assert_eq!(state_of(events).view().status, Some(GoalStatus::Complete));
}

#[test]
fn pages_advance_the_shard_cursor() {
    let mut s = RunState::new(RUN);
    let page = EventsPage {
        events: happy_run().into_iter().take(3).collect(),
        next_after: Some(ulid(3)),
    };
    s.apply_page(0, page);
    assert_eq!(s.cursor(0), Some(ulid(3).as_str()));
    assert_eq!(s.cursor(5), None);
    s.apply_page(
        0,
        EventsPage {
            events: vec![],
            next_after: None,
        },
    );
    assert_eq!(
        s.cursor(0),
        Some(ulid(3).as_str()),
        "an empty page keeps the cursor"
    );
    let shard = event_shard(Authority::Agent, Some("A1"));
    s.apply_page(
        shard,
        EventsPage {
            events: vec![a(5, EventType::TurnStarted, json!({}))],
            next_after: Some(ulid(5)),
        },
    );
    assert_eq!(s.cursor(shard), Some(ulid(5).as_str()));
}

proptest! {
    #[test]
    fn view_converges_under_duplicates_reordering_and_gaps(
        order in Just(happy_run()).prop_shuffle(),
        dups in proptest::collection::vec(0usize..10, 0..10),
        split in 0usize..10,
    ) {
        let expected = state_of(happy_run()).view();
        let mut s = RunState::new(RUN);
        // Gapped: a prefix of the shuffled order first, then duplicates, then the rest.
        for e in order.iter().take(split) { s.apply(e.clone()); }
        for d in &dups { s.apply(order[*d].clone()); }
        for e in order.iter().skip(split) { s.apply(e.clone()); }
        prop_assert_eq!(s.view(), expected);
    }
}

#[test]
fn a_task_goal_reaching_complete_or_blocked_does_not_end_the_run() {
    let mut s = RunState::new(RUN);
    s.apply(h(1, EventType::RunCreated, json!({})));
    let task = |n: u64, t: EventType, p: Value| ev(n, t, Authority::Harness, None, p);
    s.apply(task(
        2,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "integrating"}),
    ));
    let v = s.view();
    assert_eq!(v.status, Some(GoalStatus::Active));
    assert_eq!(v.phase, Some(GoalPhase::Planning));
    s.apply(task(
        3,
        EventType::GoalBlocked,
        json!({"reason": "same failing test", "fingerprint": "fp"}),
    ));
    let v = s.view();
    assert_eq!(v.status, Some(GoalStatus::Active));
    assert!(v.blocked.is_none());
    s.apply(h(
        4,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    assert_eq!(s.view().status, Some(GoalStatus::Complete));
}

/// The live E2E reads the ladder's sessions from the accepted envelopes, in event-id order;
/// rejected and duplicate events are not among them.
#[test]
fn accepted_lists_the_accepted_envelopes_in_event_id_order() {
    let mut s = RunState::new(RUN);
    s.apply(h(
        3,
        EventType::SessionStarted,
        json!({"agentId": "A1", "engine": "cline", "modelId": "m2"}),
    ));
    s.apply(h(
        1,
        EventType::SessionStarted,
        json!({"agentId": "A1", "engine": "cline", "modelId": "m1"}),
    ));
    s.apply(h(
        1,
        EventType::SessionStarted,
        json!({"agentId": "A1", "engine": "cline", "modelId": "m1"}),
    ));
    s.apply(a(2, EventType::RunCompleted, json!({"status": "complete"})));
    let ids: Vec<String> = s.accepted().map(|e| e.event_id.clone()).collect();
    assert_eq!(ids, [ulid(1), ulid(3)]);
}

#[test]
fn an_aborted_root_shows_the_run_cleared_and_why_it_ended() {
    // The reaper publishes `goal.status` cleared, then `run.aborted` with the root
    // execution's end.
    let mut events: Vec<Envelope> = happy_run().into_iter().take(4).collect();
    events.push(h(
        5,
        EventType::GoalStatus,
        json!({"status": "cleared", "phase": "green"}),
    ));
    events.push(h(
        6,
        EventType::RunAborted,
        json!({"status": "ABORTED", "error": "HarnessOperatorStop",
               "cause": "operator: leg recorded FAIL \u{1b}[31m", "stoppedAt": "2026-09-29T21:30:26Z"}),
    ));
    let v = state_of(events.clone()).view();
    assert_eq!(v.status, Some(GoalStatus::Cleared));
    let a = v.aborted.clone().unwrap();
    assert_eq!(a.status, "ABORTED");
    assert_eq!(a.error.as_deref(), Some("HarnessOperatorStop"));
    let cause = a.cause.clone().unwrap();
    assert!(cause.starts_with("operator: leg recorded FAIL"), "{cause}");
    assert!(!cause.contains('\u{1b}'), "sanitized: {cause:?}");
    assert_eq!(a.line(), format!("ABORTED HarnessOperatorStop: {cause}"));
    // run.aborted alone (the goal.status page not read yet) still ends the run as cleared.
    let only: Vec<Envelope> = events
        .iter()
        .filter(|e| e.event_type != EventType::GoalStatus || e.event_id != ulid(5))
        .cloned()
        .collect();
    assert_eq!(state_of(only).view().status, Some(GoalStatus::Cleared));
    // An agent cannot author it.
    let mut s = state_of(happy_run().into_iter().take(4));
    let agent_authored = ev(
        7,
        EventType::RunAborted,
        Authority::Agent,
        Some("A1"),
        json!({"status": "ABORTED", "stoppedAt": "2026-09-29T21:30:26Z"}),
    );
    s.apply(agent_authored);
    let v = s.view();
    assert!(v.aborted.is_none());
    assert_ne!(v.status, Some(GoalStatus::Cleared));
    assert_eq!(v.untrusted, 1);
}

#[test]
fn events_of_the_earlier_line_are_accepted_and_change_nothing() {
    // The multi-agent line wrote gate results, verdicts, the provider-fault ladder's narration and the hidden-test admission. The
    // direct line writes none of them: a stage that still holds such a run shows its status, phase and agents as they always came.
    let base: Vec<Envelope> = happy_run().into_iter().take(5).collect();
    let before = state_of(base.clone()).view();
    let mut events = base;
    events.push(h(
        11,
        EventType::RouteChanged,
        json!({"from": {"profileId": "us.anthropic.claude-opus-5-5", "region": "us-east-1"},
               "to": {"profileId": "global.anthropic.claude-opus-5-5", "region": "us-east-1"},
               "level": 2, "class": "route_denied", "attempts": 1}),
    ));
    events.push(h(
        12,
        EventType::RunWaiting,
        json!({"reason": "provider-outage", "since": "2026-09-30T12:00:00Z"}),
    ));
    events.push(h(
        13,
        EventType::GateResult,
        json!({"gateId": "G2", "pass": false}),
    ));
    events.push(h(
        14,
        EventType::EvaluatorVerdict,
        json!({"modelId": "m", "verdict": "met"}),
    ));
    events.push(h(
        15,
        EventType::HiddenAdmission,
        json!({"stage": "witness", "status": "failed", "requirementIds": ["R7"]}),
    ));
    let after = state_of(events).view();
    assert_eq!(after.status, before.status);
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.agents.len(), before.agents.len());
    assert_eq!(after.untrusted, 0, "harness authority is accepted");
    assert!(after.aborted.is_none() && after.blocked.is_none());
    assert!(!after.completed);
    // An agent cannot author them.
    let mut s = state_of(happy_run().into_iter().take(4));
    s.apply(ev(
        16,
        EventType::RunWaiting,
        Authority::Agent,
        Some("A1"),
        json!({"reason": "provider-outage", "since": "2026-09-30T12:00:00Z"}),
    ));
    assert_eq!(s.view().untrusted, 1);
}
