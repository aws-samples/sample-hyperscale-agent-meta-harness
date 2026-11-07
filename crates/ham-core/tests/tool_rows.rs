//! `tool.summary` carries the call's `title`, `kind` and `status`, and a `turn.progress` update names the call by
//! `toolCallId` and carries a newer `status`. The client folds them into one row per call.
//!
//! The fixture `fixtures/tool-events-recorded.ndjson` holds the payloads of a recorded Cline run
//! verbatim, for the four calls whose
//! `tool.summary` the dump kept whole; the envelope ids are minted again in file order. `agentd`'s collector writes the same
//! members today (`crates/agentd/src/direct/collector.rs`, `ToolSummary` and the `tool_call_update` progress event).

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::state::{RunState, ToolRow};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
const RECORDED: &str = include_str!("fixtures/tool-events-recorded.ndjson");

fn ulid(n: u64) -> String {
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

fn agent(n: u64, t: EventType, payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: ulid(n),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: n,
        ts: "2026-10-16T08:00:00Z".into(),
        event_type: t,
        authority: Authority::Agent,
        payload: Payload::Inline { value: payload },
    }
}

fn recorded() -> RunState {
    let mut s = RunState::new(RUN);
    for line in RECORDED.lines() {
        let e: Envelope = serde_json::from_str(line).unwrap();
        assert!(matches!(s.apply(e), ham_core::state::Applied::Accepted));
    }
    s
}

fn state_of(events: impl IntoIterator<Item = Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s
}

#[test]
fn the_recorded_run_has_one_tool_row_per_call_in_the_order_the_calls_began() {
    let v = recorded().view();
    let titles: Vec<&str> = v.tools.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(v.tools.len(), 4, "{titles:?}");
    assert!(titles[0].starts_with("read_files: {\"files\":[{\"path\":\"/mnt/workspace/"));
    assert!(titles[1].starts_with("editor: {\"path\":\"/mnt/workspace/"));
    assert!(titles[2].starts_with("editor: {\"old_text\":\"def test_ratio_by_zero_raises"));
    assert!(titles[3].starts_with("editor: {\"old_text\":\"def ratio(a: float"));
    let kinds: Vec<Option<&str>> = v.tools.iter().map(|r| r.kind.as_deref()).collect();
    assert_eq!(
        kinds,
        [Some("read"), Some("edit"), Some("edit"), Some("edit")]
    );
}

#[test]
fn a_status_update_wins_over_the_summarys_own_pending_status() {
    // In the recording the summary (status "pending") arrives after the "in_progress" update and before "completed".
    let v = recorded().view();
    for r in &v.tools {
        assert_eq!(r.status.as_deref(), Some("completed"), "{}", r.title);
    }
}

#[test]
fn a_call_whose_update_came_before_its_summary_keeps_the_later_status() {
    let v = state_of([
        agent(
            1,
            EventType::TurnProgress,
            json!({"toolCallId": "c1", "status": "in_progress"}),
        ),
        agent(
            2,
            EventType::ToolSummary,
            json!({"toolCallId": "c1", "title": "run_commands: pytest -q", "kind": "execute", "status": "pending"}),
        ),
    ])
    .view();
    assert_eq!(v.tools.len(), 1);
    assert_eq!(v.tools[0].title, "run_commands: pytest -q");
    assert_eq!(v.tools[0].status.as_deref(), Some("in_progress"));
}

#[test]
fn a_failed_call_stays_failed_and_a_late_pending_update_does_not_reopen_it() {
    let v = state_of([
        agent(
            1,
            EventType::ToolSummary,
            json!({"toolCallId": "c1", "title": "run_commands: make", "kind": "execute", "status": "pending"}),
        ),
        agent(
            2,
            EventType::TurnProgress,
            json!({"toolCallId": "c1", "status": "failed"}),
        ),
        agent(
            3,
            EventType::TurnProgress,
            json!({"toolCallId": "c1", "status": "pending"}),
        ),
    ])
    .view();
    assert_eq!(v.tools[0].status.as_deref(), Some("failed"));
}

#[test]
fn an_update_with_no_summary_makes_an_untitled_row_the_summary_fills_later() {
    let before = state_of([agent(
        1,
        EventType::TurnProgress,
        json!({"toolCallId": "c1", "status": "completed"}),
    )])
    .view();
    assert_eq!(before.tools.len(), 1);
    assert_eq!(before.tools[0].title, "");
    assert_eq!(before.tools[0].status.as_deref(), Some("completed"));
    let after = state_of([
        agent(
            1,
            EventType::TurnProgress,
            json!({"toolCallId": "c1", "status": "completed"}),
        ),
        agent(
            2,
            EventType::ToolSummary,
            json!({"toolCallId": "c1", "title": "search_codebase: vault", "kind": "search", "status": "pending"}),
        ),
    ])
    .view();
    assert_eq!(after.tools.len(), 1, "the summary joins the same row");
    assert_eq!(after.tools[0].title, "search_codebase: vault");
    assert_eq!(after.tools[0].status.as_deref(), Some("completed"));
}

#[test]
fn the_message_progress_event_is_not_a_tool_update() {
    let v = state_of([agent(
        1,
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "message", "text": "I'll start"}),
    )])
    .view();
    assert!(v.tools.is_empty());
}

#[test]
fn a_summary_without_a_call_id_is_its_own_row() {
    let v = state_of([
        agent(
            1,
            EventType::ToolSummary,
            json!({"title": "editor: a", "kind": "edit", "status": "pending"}),
        ),
        agent(
            2,
            EventType::ToolSummary,
            json!({"title": "editor: b", "kind": "edit", "status": "pending"}),
        ),
    ])
    .view();
    let titles: Vec<&str> = v.tools.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, ["editor: a", "editor: b"]);
}

#[test]
fn a_summary_with_only_the_old_text_member_makes_no_row() {
    // The unit tests used to feed `text`, which the emitter never writes.
    let v = state_of([agent(
        1,
        EventType::ToolSummary,
        json!({"text": "Edit src/calc.py"}),
    )])
    .view();
    assert!(v.tools.is_empty());
    assert!(
        v.agents
            .get("direct")
            .is_none_or(|a| a.last_summary.is_empty())
    );
}

#[test]
fn tool_text_is_terminal_safe_and_the_row_type_is_plain_data() {
    let v = state_of([agent(
        1,
        EventType::ToolSummary,
        json!({"toolCallId": "c\u{1b}[31m1", "title": "run \u{1b}]0;title\u{7}ls\u{202e}", "kind": "exec\u{7}ute", "status": "pend\u{1b}[2Jing"}),
    )])
    .view();
    let r: &ToolRow = &v.tools[0];
    // The bidi override is shown as a marker (the title is text the engine chose), the other sequences are gone.
    assert_eq!(r.title, "run ls<U+202E>");
    assert_eq!(r.call_id, "c1");
    assert_eq!(r.kind.as_deref(), Some("execute"));
    assert_eq!(r.status.as_deref(), Some("pending"));
}

#[test]
fn the_dashboards_last_cell_shows_the_newest_tool_title() {
    let v = state_of([
        agent(
            1,
            EventType::TurnProgress,
            json!({"turn": 1, "kind": "message", "text": "Reading the repo"}),
        ),
        agent(
            2,
            EventType::ToolSummary,
            json!({"toolCallId": "c1", "title": "run_commands: pytest -q", "kind": "execute", "status": "pending"}),
        ),
    ])
    .view();
    assert_eq!(v.agents["direct"].last_summary, "run_commands: pytest -q");
}

#[test]
fn two_runs_of_the_same_events_in_any_order_give_the_same_rows() {
    let events: Vec<Envelope> = RECORDED
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut reversed = events.clone();
    reversed.reverse();
    let a = state_of(events).view();
    let b = state_of(reversed).view();
    assert_eq!(a.tools, b.tools);
}
