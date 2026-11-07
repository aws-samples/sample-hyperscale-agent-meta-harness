//! An event keeps a payload of up to 16 KiB instead of 4 KiB (one DynamoDB item holds 400 KB), and a backlog of large events is posted
//! in batches that stay far under the 6 MB a Lambda function takes, so no number of events can make the post too large to send.
#![allow(clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::events::{EventRecorder, EventSink, spawn_flusher};
use agentd::logging::MemoryLog;
use harness_protocol::api::internal::{EventsBatchRequest, EventsBatchResponse, HeartbeatRequest};
use harness_protocol::envelope::{EventType, MAX_INLINE_PAYLOAD_BYTES, Payload, validate};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Sink {
    batches: Mutex<Vec<EventsBatchRequest>>,
}

impl EventSink for Sink {
    fn post_events<'a>(
        &'a self,
        batch: EventsBatchRequest,
    ) -> BoxFuture<'a, Result<EventsBatchResponse, String>> {
        let n = batch.events.len() as u32;
        self.batches.lock().unwrap().push(batch);
        Box::pin(async move {
            Ok(EventsBatchResponse {
                accepted: n,
                pending: vec![],
            })
        })
    }
    fn heartbeat<'a>(&'a self, _req: HeartbeatRequest) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn b776_the_inline_payload_limit_is_16_kib() {
    assert_eq!(MAX_INLINE_PAYLOAD_BYTES, 16 * 1024);
}

#[test]
fn b776_an_event_payload_of_10_kib_is_kept_whole() {
    let rec = EventRecorder::new("run1", "a1");
    rec.record(EventType::TurnProgress, json!({"text": "x".repeat(10_000)}));
    let e = &rec.drain()[0];
    validate(e).unwrap();
    match &e.payload {
        Payload::Inline { value } => {
            assert!(value.get("truncated").is_none(), "{value}");
            assert_eq!(value["text"].as_str().unwrap().len(), 10_000);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b776_a_backlog_of_large_events_is_posted_in_batches_under_4_mib() {
    let rec = EventRecorder::new("run1", "a1");
    let sink = Arc::new(Sink::default());
    let log = MemoryLog::new();
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    // 600 events of about 12 KiB are over 7 MB in one post.
    for i in 0..600 {
        rec.record(
            EventType::TurnProgress,
            json!({"n": i, "text": "y".repeat(12_000)}),
        );
    }
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_millis(50),
        Duration::from_secs(60),
        1,
        log.clone(),
        cancel,
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    flusher.stop().await;
    let batches = sink.batches.lock().unwrap();
    let total: usize = batches.iter().map(|b| b.events.len()).sum();
    assert_eq!(total, 600, "every event is posted once");
    assert!(batches.len() >= 2, "{} batches", batches.len());
    for b in batches.iter() {
        let bytes = serde_json::to_vec(b).unwrap().len();
        assert!(
            bytes <= 4 * 1024 * 1024 + 16 * 1024,
            "a batch is {bytes} bytes"
        );
    }
    let order: Vec<u64> = batches
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.seq))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "the order is kept");
}

#[derive(Default)]
struct CancelSink {
    posts: Mutex<Vec<usize>>,
}

impl EventSink for CancelSink {
    fn post_events<'a>(
        &'a self,
        batch: EventsBatchRequest,
    ) -> BoxFuture<'a, Result<EventsBatchResponse, String>> {
        self.posts.lock().unwrap().push(batch.events.len());
        Box::pin(async move {
            Ok(EventsBatchResponse {
                accepted: 0,
                pending: vec![harness_protocol::api::internal::PendingControl::Cancel],
            })
        })
    }
    fn heartbeat<'a>(&'a self, _req: HeartbeatRequest) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b776_a_turn_with_no_events_still_posts_an_empty_batch_to_poll_for_controls() {
    let rec = EventRecorder::new("run1", "a1");
    let sink = Arc::new(CancelSink::default());
    let log = MemoryLog::new();
    let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
    let flusher = spawn_flusher(
        rec,
        sink.clone(),
        Duration::from_millis(30),
        Duration::from_secs(60),
        1,
        log,
        cancel,
    );
    tokio::time::timeout(Duration::from_secs(5), cancelled.changed())
        .await
        .expect("the cancel that the server queued reaches the turn")
        .unwrap();
    assert!(*cancelled.borrow());
    flusher.stop().await;
    assert!(sink.posts.lock().unwrap().iter().all(|n| *n == 0));
}
