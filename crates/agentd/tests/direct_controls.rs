//! Controls that reach a waiting run: the user's answer to a `needs-input` question rides the queue that
//! carries Cancel. `TurnControls` hands each `Instruct` to the driver over a bounded channel and applies the batch rule: a Cancel
//! in the same batch raises the cancel flag first and marks the instructions of that batch, so the driver acknowledges them
//! `input-ignored` and the run ends `stopped_by_user`. An unreadable control never costs the batch or the Cancel beside it.
#![allow(clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::events::{
    EventRecorder, EventSink, INSTRUCTION_CHANNEL_CAPACITY, Instruct, TurnControls,
    spawn_flusher_with,
};
use agentd::logging::MemoryLog;
use harness_protocol::api::internal::{
    EventsBatchRequest, EventsBatchResponse, HeartbeatRequest, PendingControl,
};
use harness_protocol::common::ApprovalDecision;
use harness_protocol::direct::{InstructAction, InstructOrigin};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

fn instruct(id: &str, action: InstructAction, text: Option<&str>) -> PendingControl {
    PendingControl::Instruct {
        request_id: id.to_string(),
        action,
        text: text.map(str::to_string),
        model: None,
        origin: InstructOrigin::User,
    }
}

fn answer(text: &str) -> PendingControl {
    instruct(REQ, InstructAction::ContinueWithPrompt, Some(text))
}

#[test]
fn the_instruction_receiver_can_be_taken_once() {
    let (controls, _cancel) = TurnControls::new();
    assert!(controls.take_instructions().is_some());
    assert!(
        controls.take_instructions().is_none(),
        "one driver owns the channel"
    );
}

#[test]
fn an_instruction_reaches_the_driver_with_every_field() {
    let (controls, cancel) = TurnControls::new();
    let mut rx = controls.take_instructions().unwrap();
    let report = controls.deliver(&[PendingControl::Instruct {
        request_id: REQ.to_string(),
        action: InstructAction::TryModel,
        text: Some("go on with the tests".to_string()),
        model: Some("us.anthropic.claude-opus-5-5".to_string()),
        origin: InstructOrigin::Analyzer,
    }]);
    assert_eq!(report.instructions, 1);
    assert_eq!(report.dropped, 0);
    assert!(!report.cancel);
    let got = rx.try_recv().unwrap();
    assert_eq!(
        got,
        Instruct {
            request_id: REQ.to_string(),
            action: InstructAction::TryModel,
            text: Some("go on with the tests".to_string()),
            model: Some("us.anthropic.claude-opus-5-5".to_string()),
            origin: InstructOrigin::Analyzer,
            cancelled_in_batch: false,
        }
    );
    assert!(rx.try_recv().is_err(), "exactly one");
    assert!(!*cancel.borrow(), "an instruction is not a cancel");
}

#[test]
fn instructions_arrive_in_the_order_they_were_delivered() {
    let (controls, _cancel) = TurnControls::new();
    let mut rx = controls.take_instructions().unwrap();
    controls.deliver(&[answer("one"), answer("two")]);
    controls.deliver(&[instruct(REQ, InstructAction::Finish, None)]);
    let texts: Vec<Option<String>> = (0..3).map(|_| rx.try_recv().unwrap().text).collect();
    assert_eq!(
        texts,
        [Some("one".to_string()), Some("two".to_string()), None]
    );
}

#[test]
fn a_cancel_in_the_same_batch_raises_the_flag_and_marks_the_instruction_whatever_the_order() {
    for batch in [
        vec![answer("keep going"), PendingControl::Cancel],
        vec![PendingControl::Cancel, answer("keep going")],
    ] {
        let (controls, cancel) = TurnControls::new();
        let mut rx = controls.take_instructions().unwrap();
        let report = controls.deliver(&batch);
        assert!(report.cancel);
        assert!(*cancel.borrow(), "the cancel flag is up");
        let got = rx.try_recv().unwrap();
        assert!(
            got.cancelled_in_batch,
            "the driver must acknowledge it input-ignored, not act on it"
        );
        assert_eq!(got.text.as_deref(), Some("keep going"));
    }
}

#[test]
fn an_instruction_of_a_later_batch_is_not_marked_by_an_earlier_cancel() {
    let (controls, cancel) = TurnControls::new();
    let mut rx = controls.take_instructions().unwrap();
    controls.deliver(&[PendingControl::Cancel]);
    controls.deliver(&[answer("too late")]);
    assert!(*cancel.borrow());
    assert!(
        !rx.try_recv().unwrap().cancelled_in_batch,
        "the batch rule is per batch: the driver sees the raised flag itself"
    );
}

#[test]
fn a_full_channel_drops_the_surplus_and_counts_it() {
    let (controls, _cancel) = TurnControls::new();
    let mut rx = controls.take_instructions().unwrap();
    assert_eq!(INSTRUCTION_CHANNEL_CAPACITY, 16);
    let batch: Vec<PendingControl> = (0..17).map(|i| answer(&format!("m{i}"))).collect();
    let report = controls.deliver(&batch);
    assert_eq!(report.instructions, 16);
    assert_eq!(report.dropped, 1);
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    assert_eq!(n, 16);
}

#[test]
fn a_batch_with_a_cancel_still_cancels_when_the_channel_is_full() {
    let (controls, cancel) = TurnControls::new();
    let _rx = controls.take_instructions().unwrap();
    let mut batch: Vec<PendingControl> = (0..20).map(|i| answer(&format!("m{i}"))).collect();
    batch.push(PendingControl::Cancel);
    let report = controls.deliver(&batch);
    assert!(
        *cancel.borrow(),
        "a flood of instructions never costs the Cancel"
    );
    assert!(report.cancel);
    assert_eq!(report.dropped, 4);
}

#[test]
fn a_dropped_receiver_drops_the_instruction_without_a_panic_and_without_a_cancel() {
    let (controls, cancel) = TurnControls::new();
    drop(controls.take_instructions().unwrap());
    let report = controls.deliver(&[answer("nobody listens")]);
    assert_eq!(report.dropped, 1);
    assert_eq!(report.instructions, 0);
    assert!(!*cancel.borrow());
}

#[test]
fn nobody_ever_taking_the_receiver_is_not_an_error() {
    // A run with no input window never takes it; the channel fills up and drops, which is harmless.
    let (controls, _cancel) = TurnControls::new();
    let report = controls.deliver(&[answer("a run that never waits")]);
    assert_eq!(report.instructions, 1);
    assert_eq!(report.dropped, 0);
}

#[test]
fn steer_and_approval_answers_are_still_ignored_in_a_direct_run() {
    let (controls, cancel) = TurnControls::new();
    let mut rx = controls.take_instructions().unwrap();
    let report = controls.deliver(&[
        PendingControl::Steer {
            text: "do it differently".to_string(),
            now: true,
        },
        PendingControl::ApprovalAnswer {
            approval_id: "ap-1".to_string(),
            decision: ApprovalDecision::Allow,
        },
    ]);
    assert_eq!(report.instructions, 0);
    assert_eq!(report.dropped, 0);
    assert!(!report.cancel);
    assert!(!*cancel.borrow());
    assert!(rx.try_recv().is_err());
}

#[test]
fn from_sender_controls_carry_instructions_too() {
    let (tx, cancel) = tokio::sync::watch::channel(false);
    let controls = TurnControls::from_sender(tx);
    let mut rx = controls.take_instructions().unwrap();
    controls.deliver(&[answer("hello")]);
    assert_eq!(rx.try_recv().unwrap().text.as_deref(), Some("hello"));
    assert!(!*cancel.borrow());
}

#[test]
fn an_unreadable_control_never_fails_the_batch_or_the_cancel_beside_it() {
    // What internal API sends today and tomorrow in one response: a kind this build does not know, an instruct with an action it
    // does not know, an instruct that is fine, and a Cancel. The response parses and keeps what can be read.
    let body = serde_json::json!({
        "accepted": 3,
        "pending": [
            {"kind": "future-thing", "x": 1},
            {"kind": "instruct", "requestId": REQ, "action": "dance"},
            {"kind": "instruct", "requestId": REQ, "action": "finish"},
            {"kind": "cancel"}
        ]
    });
    let resp: EventsBatchResponse = serde_json::from_value(body).unwrap();
    assert_eq!(resp.pending.len(), 2, "{:?}", resp.pending);
    let (controls, cancel) = TurnControls::new();
    let mut rx = controls.take_instructions().unwrap();
    controls.deliver(&resp.pending);
    assert!(
        *cancel.borrow(),
        "the Cancel beside the unreadable items arrived"
    );
    let got = rx.try_recv().unwrap();
    assert_eq!(got.action, InstructAction::Finish);
    assert_eq!(got.origin, InstructOrigin::User, "origin defaults to user");
    assert!(got.cancelled_in_batch);
}

/// A sink that answers its first post with the scripted controls and every later one with none.
struct ScriptedSink {
    first: Mutex<Option<Vec<PendingControl>>>,
}

impl EventSink for ScriptedSink {
    fn post_events<'a>(
        &'a self,
        batch: EventsBatchRequest,
    ) -> BoxFuture<'a, Result<EventsBatchResponse, String>> {
        let pending = self.first.lock().unwrap().take().unwrap_or_default();
        let accepted = batch.events.len() as u32;
        Box::pin(async move { Ok(EventsBatchResponse { accepted, pending }) })
    }
    fn heartbeat<'a>(&'a self, _req: HeartbeatRequest) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(start_paused = true)]
async fn the_flusher_hands_instructions_to_the_driver_and_logs_what_it_had_to_drop() {
    let rec = EventRecorder::new("run1", "direct");
    let batch: Vec<PendingControl> = (0..18).map(|i| answer(&format!("m{i}"))).collect();
    let sink = Arc::new(ScriptedSink {
        first: Mutex::new(Some(batch)),
    });
    let log = MemoryLog::new();
    let (controls, _cancel) = TurnControls::new();
    let mut rx = controls.take_instructions().unwrap();
    let flusher = spawn_flusher_with(
        rec,
        sink,
        Duration::from_millis(50),
        Duration::from_secs(60),
        1,
        log.clone(),
        controls.clone(),
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    flusher.stop().await;
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    assert_eq!(n, 16, "the driver got what fits");
    assert!(
        log.lines()
            .iter()
            .any(|l| l.contains("instruction") && l.contains("dropped") && l.contains('2')),
        "the flusher must say it dropped 2 instructions: {:?}",
        log.lines()
    );
}
