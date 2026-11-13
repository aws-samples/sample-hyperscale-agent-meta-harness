//! The words the client shows about a run name the engine the run is on, read from the run's own `session.started` event, and the words of
//! a run on the default engine are the words they always were. Where no run exists yet the default engine is named.

#![allow(clippy::unwrap_used)]

use ham_core::alert::{notification_bytes, notification_bytes_for};
use ham_core::chat::{ChatInputs, ChatModel};
use ham_core::messages::{explain_refusal, explain_refusal_for, status_line, status_line_for};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use harness_protocol::messages::{MessageMode, MessageState, MessageView};
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, authority: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-16T08:15:00Z",
        "type": ty, "authority": authority, "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn started(engine: &str) -> Envelope {
    event(
        1,
        "session.started",
        "harness",
        json!({"agentId": "direct", "engine": engine, "modelId": "m"}),
    )
}

fn progress(n: u32, extra: Value) -> Envelope {
    let mut v = json!({"turn": 1});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    event(n, "turn.progress", "agent", v)
}

fn view_of(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

/// The bodies of the notice blocks of the conversation the client builds from the run's events: the goal-loop lines are shown there and are not
/// run notices of the view.
fn notice_texts(events: Vec<Envelope>) -> Vec<String> {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    let model = ChatModel::from_progress_feed(&s, &ChatInputs::default());
    model.blocks.iter().map(|b| b.body.clone()).collect()
}

fn pending(mode: MessageMode, state: MessageState) -> MessageView {
    MessageView {
        message_id: "01M5A2R7K3C9XJ4D8E6FQ0WT1N".into(),
        seq: 2,
        mode,
        state,
        text: "use the same style".into(),
        epoch: Some("sess-2".into()),
        epoch_no: 2,
        reason: None,
        detail: None,
        delivered_turn: None,
        confirms: None,
        superseded_by: None,
        accepted_at: "2026-10-16T14:02:11Z".into(),
        updated_at: "2026-10-16T14:02:11Z".into(),
        history: vec![],
    }
}

#[test]
fn b787_a_message_row_names_the_engine_of_the_run_and_the_old_call_names_the_default_engine() {
    let now = pending(MessageMode::Now, MessageState::Pending);
    let next = pending(MessageMode::Next, MessageState::Pending);
    let taking = pending(MessageMode::Next, MessageState::Dispatching);
    assert_eq!(
        status_line_for(Some("opencode"), &now, 2),
        "waiting to interrupt OpenCode"
    );
    assert_eq!(
        status_line_for(Some("opencode"), &next, 2),
        "waiting for OpenCode to finish its turn"
    );
    assert_eq!(
        status_line_for(Some("opencode"), &taking, 2),
        "OpenCode is taking it now"
    );
    for m in [&now, &next, &taking] {
        assert_eq!(
            status_line(m, 2),
            status_line_for(Some("cline"), m, 2),
            "Cline's words are unchanged"
        );
        assert_eq!(status_line(m, 2), status_line_for(None, m, 2));
    }
    assert_eq!(status_line(&now, 2), "waiting to interrupt Cline");
}

#[test]
fn b787_a_refusal_names_the_engine_of_the_run() {
    let r = explain_refusal_for(Some("opencode"), 409, "no-message-control", "");
    assert!(
        r.text.starts_with("OpenCode is not waiting for an answer"),
        "{}",
        r.text
    );
    let r = explain_refusal_for(Some("opencode"), 409, "question-open", "");
    assert!(
        r.text.starts_with("OpenCode is waiting for your answer"),
        "{}",
        r.text
    );
    let r = explain_refusal_for(Some("opencode"), 429, "too-many-pending", "");
    assert!(
        r.text.contains("waiting for OpenCode already"),
        "{}",
        r.text
    );
    for code in ["no-message-control", "question-open", "too-many-pending"] {
        assert_eq!(
            explain_refusal(409, code, ""),
            explain_refusal_for(Some("cline"), 409, code, ""),
            "{code}: the default engine's words are unchanged"
        );
    }
    assert!(
        !explain_refusal_for(Some("opencode"), 409, "no-message-control", "")
            .text
            .contains("Cline")
    );
}

#[test]
fn b787_the_explanation_of_a_question_names_the_engine_of_the_run() {
    let q = |engine: &str| {
        let v = view_of(vec![
            started(engine),
            progress(
                10,
                json!({"kind": "needs-input", "text": "t", "requestId": id(10), "n": 1, "reason": "no_changes", "title": "",
                       "blocking": true, "options": ["finish"], "models": [], "canContinue": true, "waitS": 1800}),
            ),
        ]);
        v.input.waiting().expect("a waiting question").words.clone()
    };
    assert!(
        q("opencode").starts_with("OpenCode ended normally with a message"),
        "{}",
        q("opencode")
    );
    assert!(
        q("cline").starts_with("Cline ended normally with a message"),
        "{}",
        q("cline")
    );
}

#[test]
fn b787_the_goal_loop_notice_and_the_brake_card_name_the_engine_of_the_run() {
    let events = |engine: &str| {
        vec![
            started(engine),
            progress(
                2,
                json!({"kind": "goal-loop", "phase": "done", "summary": "all tests pass"}),
            ),
            progress(
                3,
                json!({"kind": "goal-loop", "phase": "round", "rounds": 3, "status": "completed"}),
            ),
            progress(
                4,
                json!({"kind": "goal-loop", "phase": "brake", "rule": "blocked-twice", "text": "The loop stopped."}),
            ),
        ]
    };
    let texts = notice_texts(events("opencode"));
    assert!(
        texts.contains(
            &"Goal loop: OpenCode called the goal-complete tool. OpenCode's summary: all tests pass"
                .to_string()
        ),
        "{texts:?}"
    );
    let v = view_of(events("opencode"));
    let card = v.goal_brake.as_ref().expect("a brake card");
    let text = card.lines(RUN, 100).join("\n") + &card.explain();
    assert!(
        text.contains("OpenCode ended two rounds in a row"),
        "{text}"
    );
    assert!(
        text.contains("OpenCode decides when its work is done"),
        "{text}"
    );
    assert!(!text.contains("Cline"), "{text}");
    assert!(
        notice_texts(events("cline")).contains(
            &"Goal loop: Cline called the goal-complete tool. Cline's summary: all tests pass"
                .to_string()
        )
    );
}

#[test]
fn b787_the_notification_of_a_run_that_needs_you_names_its_engine() {
    let ours = notification_bytes_for(Some("opencode"), RUN);
    assert_eq!(
        ours,
        b"\x1b]9;ham: OpenCode needs you (run 01M4ZHFB)\x07".to_vec()
    );
    assert_eq!(
        notification_bytes(RUN),
        notification_bytes_for(Some("cline"), RUN)
    );
    assert_eq!(notification_bytes(RUN), notification_bytes_for(None, RUN));
}

#[test]
fn b787_a_run_view_without_a_session_still_reads_as_the_default_engine() {
    let texts = notice_texts(vec![progress(
        2,
        json!({"kind": "goal-loop", "phase": "done"}),
    )]);
    assert!(
        texts.iter().any(|t| t.contains("Cline called")),
        "{texts:?}"
    );
}
