//! Agent event recording and posting.
#![allow(clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::events::{EventRecorder, EventSink, spawn_flusher};
use agentd::logging::MemoryLog;
use harness_protocol::api::internal::{
    EventsBatchRequest, EventsBatchResponse, HeartbeatRequest, PendingControl,
};
use harness_protocol::envelope::{Authority, EventType, Payload, validate};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct FakeSink {
    batches: Mutex<Vec<EventsBatchRequest>>,
    heartbeats: Mutex<Vec<HeartbeatRequest>>,
    fail: bool,
}

impl EventSink for FakeSink {
    fn post_events<'a>(
        &'a self,
        batch: EventsBatchRequest,
    ) -> BoxFuture<'a, Result<EventsBatchResponse, String>> {
        let n = batch.events.len() as u32;
        let fail = self.fail;
        if !fail {
            self.batches.lock().unwrap().push(batch);
        }
        Box::pin(async move {
            if fail {
                Err("internal API unreachable".into())
            } else {
                Ok(EventsBatchResponse {
                    accepted: n,
                    pending: vec![PendingControl::Cancel],
                })
            }
        })
    }
    fn heartbeat<'a>(&'a self, req: HeartbeatRequest) -> BoxFuture<'a, Result<(), String>> {
        let fail = self.fail;
        self.heartbeats.lock().unwrap().push(req);
        Box::pin(async move { if fail { Err("down".into()) } else { Ok(()) } })
    }
}

#[test]
fn recorded_events_are_valid_agent_envelopes_in_sequence() {
    let rec = EventRecorder::new("run1", "a1");
    rec.record(EventType::TurnStarted, json!({"turn": 1}));
    rec.record(EventType::ToolSummary, json!({"title": "Edit src/calc.py"}));
    let events = rec.drain();
    assert_eq!(events.len(), 2);
    for (i, e) in events.iter().enumerate() {
        validate(e).unwrap();
        assert_eq!(e.authority, Authority::Agent);
        assert_eq!(e.run_id, "run1");
        assert_eq!(e.agent_id.as_deref(), Some("a1"));
        assert_eq!(e.seq, i as u64 + 1);
    }
    assert_ne!(events[0].event_id, events[1].event_id);
    assert!(rec.drain().is_empty(), "drain empties the buffer");
    rec.record(EventType::TurnEnded, json!({}));
    assert_eq!(rec.drain()[0].seq, 3, "sequence continues across drains");
}

#[test]
fn an_oversized_payload_is_replaced_by_a_truncation_marker() {
    let rec = EventRecorder::new("run1", "a1");
    rec.record(EventType::TurnProgress, json!({"text": "x".repeat(20_000)}));
    let e = &rec.drain()[0];
    validate(e).unwrap();
    match &e.payload {
        Payload::Inline { value } => {
            assert_eq!(value["truncated"], true);
            assert!(value["bytes"].as_u64().unwrap() > 16 * 1024);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_flusher_posts_batches_and_heartbeats_and_keeps_pending_controls() {
    let rec = EventRecorder::new("run1", "a1");
    let sink = Arc::new(FakeSink::default());
    let log = MemoryLog::new();
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_millis(50),
        Duration::from_millis(80),
        3,
        log.clone(),
        cancel,
    );
    rec.record(EventType::TurnStarted, json!({"turn": 3}));
    tokio::time::sleep(Duration::from_millis(200)).await;
    rec.record(EventType::TurnEnded, json!({"turn": 3}));
    let pending = flusher.stop().await;
    let batches = sink.batches.lock().unwrap();
    let total: usize = batches.iter().map(|b| b.events.len()).sum();
    assert_eq!(total, 2, "every recorded event is delivered by stop()");
    assert!(
        batches
            .iter()
            .all(|b| b.run_id == "run1" && b.agent_id == "a1")
    );
    let hb = sink.heartbeats.lock().unwrap();
    assert!(!hb.is_empty());
    assert_eq!(hb[0].turn, 3);
    assert!(pending.contains(&PendingControl::Cancel));
    // A Cancel (or Steer now) from the internal API cancels the running turn.
    assert!(*cancelled.borrow(), "the cancel signal fired");
}

#[tokio::test]
async fn an_urgent_event_is_posted_at_once_and_not_at_the_next_tick() {
    // The rules digest of a turn rides `turn.started` (verify row R10). A microVM lost a second after the
    // manifest was assembled must not take the digest with it, so the start is posted without waiting for the
    // next 2 s tick (here: a 30 s tick).
    let rec = EventRecorder::new("run1", "a1");
    let sink = Arc::new(FakeSink::default());
    let log = MemoryLog::new();
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        1,
        log,
        cancel,
    );
    // The interval's first tick is immediate; let it pass.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let delivered = || -> usize {
        sink.batches
            .lock()
            .unwrap()
            .iter()
            .map(|b| b.events.len())
            .sum()
    };
    rec.record(
        EventType::ToolSummary,
        json!({"title": "an ordinary event waits for the tick"}),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(delivered(), 0, "an ordinary event waits for the tick");
    rec.record_urgent(
        EventType::TurnStarted,
        json!({"turn": 1, "rulesDigest": "ab".repeat(32)}),
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        delivered(),
        2,
        "the urgent event and everything buffered before it went out at once"
    );
    let order: Vec<u64> = sink
        .batches
        .lock()
        .unwrap()
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.seq))
        .collect();
    assert_eq!(order, vec![1, 2], "sequence order is kept");
    flusher.stop().await;
}

#[tokio::test]
async fn a_failing_sink_never_fails_and_is_logged() {
    let rec = EventRecorder::new("run1", "a1");
    let sink = Arc::new(FakeSink {
        fail: true,
        ..Default::default()
    });
    let log = MemoryLog::new();
    let flusher = spawn_flusher(
        rec.clone(),
        sink,
        Duration::from_millis(20),
        Duration::from_millis(20),
        1,
        log.clone(),
        tokio::sync::watch::channel(false).0,
    );
    rec.record(EventType::TurnStarted, json!({}));
    tokio::time::sleep(Duration::from_millis(80)).await;
    let pending = flusher.stop().await;
    assert!(pending.is_empty());
    assert!(
        log.lines().iter().any(|l| l.contains("events post failed")),
        "{:?}",
        log.lines()
    );
}

// Paused clock: ticks at 0, 50, ..., 200 ms, then stop() at 225 ms with the next tick at 250 ms,
// so every counted post is a tick and none races the final flush.
#[tokio::test(start_paused = true)]
async fn an_idle_flusher_polls_for_controls_with_an_empty_batch() {
    let rec = EventRecorder::new("run1", "a1");
    let sink = Arc::new(FakeSink::default());
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_millis(50),
        Duration::from_secs(60),
        1,
        MemoryLog::new(),
        cancel,
    );
    tokio::time::sleep(Duration::from_millis(225)).await;
    assert!(*cancelled.borrow(), "a Cancel reached an idle turn");
    let polls = {
        let batches = sink.batches.lock().unwrap();
        assert!(!batches.is_empty());
        assert!(batches.iter().all(|b| b.events.is_empty()));
        assert_eq!(batches.len(), 5, "one poll per 50 ms tick");
        batches.len()
    };
    let pending = flusher.stop().await;
    // stop() posts only recorded events: an empty final poll would take controls meant for the
    // next turn.
    assert_eq!(sink.batches.lock().unwrap().len(), polls);
    assert!(pending.contains(&PendingControl::Cancel));
}

// Paused clock: ticks at 0, 50 and 100 ms, TurnEnded and stop() at 120 ms.
#[tokio::test(start_paused = true)]
async fn the_final_flush_leaves_controls_queued_for_the_next_turn() {
    let rec = EventRecorder::new("run1", "a1");
    let sink = Arc::new(FakeSink::default());
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_millis(50),
        Duration::from_secs(60),
        1,
        MemoryLog::new(),
        tokio::sync::watch::channel(false).0,
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    rec.record(EventType::TurnEnded, json!({"turn": 1}));
    flusher.stop().await;
    let batches = sink.batches.lock().unwrap();
    let (last, ticks) = batches.split_last().unwrap();
    assert_eq!(last.take_controls, Some(false), "final flush");
    assert!(!ticks.is_empty());
    assert!(ticks.iter().all(|b| b.take_controls.is_none()));
}

#[test]
fn an_urgent_record_with_a_given_id_keeps_that_id_in_sequence() {
    // The request id of a `needs-input` question IS its event id: agentd knows it before the event exists.
    const ID: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
    let rec = EventRecorder::new("run1", "direct");
    rec.record(EventType::TurnStarted, json!({"turn": 1}));
    rec.record_urgent_with_id(
        EventType::TurnProgress,
        ID,
        json!({"kind": "needs-input", "requestId": ID}),
    );
    rec.record(EventType::TurnEnded, json!({"turn": 1}));
    let events = rec.drain();
    assert_eq!(events.len(), 3);
    assert_eq!(events[1].event_id, ID);
    assert_ne!(events[0].event_id, ID);
    assert_ne!(events[2].event_id, ID);
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
    for e in &events {
        validate(e).unwrap();
        assert_eq!(e.authority, Authority::Agent);
    }
}

#[tokio::test]
async fn an_urgent_record_with_a_given_id_is_posted_at_once_with_that_id() {
    const ID: &str = "01M5B8T4W2N6QJ3C7D9EH1FZ0A";
    let rec = EventRecorder::new("run1", "direct");
    let sink = Arc::new(FakeSink::default());
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        1,
        MemoryLog::new(),
        tokio::sync::watch::channel(false).0,
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    rec.record_urgent_with_id(EventType::TurnProgress, ID, json!({"kind": "needs-input"}));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let ids: Vec<String> = sink
        .batches
        .lock()
        .unwrap()
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.event_id.clone()))
        .collect();
    assert_eq!(
        ids,
        [ID],
        "posted at once, not at the 30 s tick, with the given id"
    );
    flusher.stop().await;
}

#[test]
fn an_oversized_payload_with_a_given_id_still_keeps_the_id() {
    const ID: &str = "01M5C9V5X3P7RK4D8E0GJ2HA1B";
    let rec = EventRecorder::new("run1", "direct");
    rec.record_urgent_with_id(
        EventType::TurnProgress,
        ID,
        json!({"text": "x".repeat(20_000)}),
    );
    let e = &rec.drain()[0];
    validate(e).unwrap();
    assert_eq!(e.event_id, ID);
    match &e.payload {
        Payload::Inline { value } => assert_eq!(value["truncated"], true),
        other => panic!("{other:?}"),
    }
}
