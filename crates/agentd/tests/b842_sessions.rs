//! A session that follows another one of the same run (a session that was lost and restarted, or one that reached its time limit).
//!
//! * The copies of a run are numbered within the run: a later session goes on counting from the newest copy an earlier session recorded.
//! * A session makes no copy until a file changes: the work it restored is saved already.
//! * A session that works quietly writes a beat into the run's events for each beat interval (a minute) without any event, and says at
//!   its start which session it is and how often it beats, so a reader of the events can tell a session that stopped answering from one
//!   that runs a long command.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::snapshot::Snapshotter;
use agentd::events::{EventRecorder, EventSink, spawn_flusher};
use agentd::logging::MemoryLog;
use common::{git, python_calc, scratch};
use direct_support::{BUCKET, Direct, Opts, RUN};
use harness_protocol::api::internal::{EventsBatchRequest, EventsBatchResponse, HeartbeatRequest};
use harness_protocol::direct::{DirectSlice, s3 as keys, validate_direct_invocation};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use harness_protocol::runtime::{RepoSource, TurnOutcome};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Runs session 1 to its end and returns what the service hands to session 2: the bundle, its head and the run's base.
async fn first_session() -> (Vec<u8>, String, String) {
    let d1 = Direct::start(Opts::default()).await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d1.summary();
    let head = s.git.head_sha.clone().expect("session 1 delivered a head");
    let base = s.git.base_sha.clone().expect("session 1 has a base");
    let bundle = d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("session 1 bundled its work");
    (bundle, head, base)
}

/// Session 2 of the run in a new machine, with the copies made while the engine works switched off: its one copy is the copy of its end.
async fn second_session(bundle: Vec<u8>, head: &str, base: &str) -> Direct {
    let mut d2 = Direct::start(Opts::default()).await;
    d2.aws.put(&keys::bundle_key_t(RUN, 1), bundle);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d2.req.snapshot_every_s = Some(0);
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.to_string()),
        restore_head: Some(head.to_string()),
        sessions_before: Some(1),
        brief: None,
    });
    validate_direct_invocation(&d2.req).expect("the invocation of session 2 is valid");
    d2
}

fn record_of(d: &Direct, session: u32) -> serde_json::Value {
    serde_json::from_slice(
        &d.aws
            .get(&keys::snapshot_json_key_t(RUN, session))
            .expect("snapshot.json"),
    )
    .unwrap()
}

fn saved_copies(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .events()
        .into_iter()
        .filter(|(t, _)| *t == EventType::TurnProgress)
        .filter_map(|(_, v)| v.get("savedCopy").cloned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn b842_a_later_session_goes_on_counting_the_copies_of_the_run() {
    let (bundle, head, base) = first_session().await;
    let d2 = second_session(bundle, &head, &base).await;
    // What session 1 recorded for its newest copy: the fourth of the run.
    d2.aws.put(
        &keys::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1, "headSha": head, "baseSha": base, "at": "2026-11-14T07:03:31Z", "count": 4,
        }))
        .unwrap(),
    );
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let r = record_of(&d2, 2);
    assert_eq!(
        r["count"], 5,
        "the first copy of session 2 is the fifth copy of the run: {r}"
    );
    let copies = saved_copies(&d2);
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0]["count"], 5);
    assert_eq!(copies[0]["session"], 2);
    // The summary of the session still counts the copies the session itself stored.
    let summary: harness_protocol::direct::DirectSummary =
        serde_json::from_slice(&d2.aws.get(&keys::summary_key_t(RUN, 2)).unwrap()).unwrap();
    assert_eq!(summary.snapshots.count, 1, "{:?}", summary.snapshots);
}

#[tokio::test(flavor = "multi_thread")]
async fn b842_a_later_session_of_a_run_that_recorded_no_copy_starts_the_count_at_one() {
    let (bundle, head, base) = first_session().await;
    let d2 = second_session(bundle, &head, &base).await;
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(record_of(&d2, 2)["count"], 1);
}

#[tokio::test]
async fn b842_a_session_that_restored_a_copy_makes_no_copy_until_a_file_changes() {
    let dir = scratch("b842-restored");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut first = Snapshotter::new(&data, &repo, &base).await.unwrap();
    std::fs::write(repo.join("src/a.py"), "a = 1\n").unwrap();
    let copy = first.snapshot().await.unwrap().expect("the tree changed");

    // The next session, in a new machine: the repository is restored from that copy's bundle and stands at the copy's commit.
    let restored = dir.join("restored");
    git(
        &dir,
        &[
            "clone",
            "-q",
            "-b",
            "snapshot",
            copy.bundle.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    assert_eq!(git(&restored, &["rev-parse", "HEAD"]), copy.commit);
    let data2 = dir.join("agent-data-2");
    std::fs::create_dir_all(&data2).unwrap();
    let mut next = Snapshotter::new(&data2, &restored, &base).await.unwrap();
    assert!(
        next.snapshot().await.unwrap().is_none(),
        "no file changed since the copy the session restored, so there is nothing new to save"
    );
    std::fs::write(restored.join("src/b.py"), "b = 2\n").unwrap();
    let second = next.snapshot().await.unwrap().expect("a file changed");
    assert_eq!(
        git(
            &dir,
            &[
                "--git-dir",
                data2.join("snapshot.git").to_str().unwrap(),
                "rev-parse",
                &format!("{}^", second.commit),
            ],
        ),
        copy.commit,
        "the new copy stands on the copy the session restored"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b842_a_session_says_at_its_start_which_session_it_is_and_how_often_it_writes_a_beat() {
    // The default scenario works for well under one beat interval of the product (60 s) and ends.
    let d = Direct::start(Opts::default()).await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let events = d.aws.events();
    let started = events
        .iter()
        .find(|(t, _)| *t == EventType::TurnStarted)
        .map(|(_, v)| v.clone())
        .expect("turn.started");
    assert_eq!(
        started["beatS"], 60,
        "the beat interval is a minute, whatever the heartbeat interval of the test: {started}"
    );
    assert_eq!(started["session"], 1, "{started}");
    assert!(
        !events.iter().any(|(_, v)| v.get("alive").is_some()),
        "a session that ended within one beat interval wrote no beat"
    );
}

#[derive(Default)]
struct Sink {
    events: Mutex<Vec<Envelope>>,
}

impl EventSink for Sink {
    fn post_events<'a>(
        &'a self,
        batch: EventsBatchRequest,
    ) -> BoxFuture<'a, Result<EventsBatchResponse, String>> {
        let n = batch.events.len() as u32;
        self.events.lock().unwrap().extend(batch.events);
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

fn is_beat(e: &Envelope) -> bool {
    matches!(&e.payload, Payload::Inline { value } if value.get("alive").is_some())
}

#[tokio::test]
async fn b842_a_quiet_recorder_gets_a_beat_at_each_beat_interval_and_a_busy_one_gets_none() {
    let rec = EventRecorder::new("run1", "direct");
    rec.beat_when_quiet(Duration::from_millis(200));
    assert_eq!(
        rec.beat_s(),
        Some(1),
        "turn.started names whole seconds, rounded up"
    );
    let sink = Arc::new(Sink::default());
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_millis(20),
        Duration::from_secs(3_600),
        2,
        MemoryLog::new(),
        cancel,
    );
    // Busy for 600 ms: an event every 20 ms.
    for n in 0..30 {
        rec.record(
            EventType::ToolSummary,
            json!({"title": format!("step {n}")}),
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Quiet for a second: five beat intervals.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    flusher.stop().await;
    let events = sink.events.lock().unwrap().clone();
    let last_step = events
        .iter()
        .rposition(|e| e.event_type == EventType::ToolSummary)
        .expect("the steps were posted");
    assert!(
        !events[..last_step].iter().any(is_beat),
        "no beat while the recorder took events"
    );
    let beats: Vec<&Envelope> = events[last_step..].iter().filter(|e| is_beat(e)).collect();
    assert!(
        (2..=6).contains(&beats.len()),
        "a quiet second with a beat every 0.2 s: {} beats",
        beats.len()
    );
    for b in beats {
        let Payload::Inline { value } = &b.payload else {
            panic!("a beat is an inline event");
        };
        // A client that does not know the member shows nothing for the event: no kind, no text, no tool call.
        assert!(
            value.get("kind").is_none()
                && value.get("text").is_none()
                && value.get("toolCallId").is_none(),
            "{value}"
        );
        assert_eq!(
            value["alive"]["session"], 2,
            "the beat names the session: {value}"
        );
        assert_eq!(b.event_type, EventType::TurnProgress);
    }

    // A recorder that was not asked for beats writes none, however quiet it is.
    let plain = EventRecorder::new("run1", "direct");
    assert_eq!(plain.beat_s(), None);
    let sink = Arc::new(Sink::default());
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    let flusher = spawn_flusher(
        plain.clone(),
        sink.clone(),
        Duration::from_millis(20),
        Duration::from_millis(50),
        1,
        MemoryLog::new(),
        cancel,
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    flusher.stop().await;
    assert!(sink.events.lock().unwrap().is_empty());
}

// ---- the review round: the beat keeps its promise, a waiting session writes none, and the count steps over a hidden record ----------

/// A sink that keeps when each batch arrived.
#[derive(Default)]
struct StampSink {
    events: Mutex<Vec<(std::time::Instant, Envelope)>>,
}

impl EventSink for StampSink {
    fn post_events<'a>(
        &'a self,
        batch: EventsBatchRequest,
    ) -> BoxFuture<'a, Result<EventsBatchResponse, String>> {
        let n = batch.events.len() as u32;
        let at = std::time::Instant::now();
        self.events
            .lock()
            .unwrap()
            .extend(batch.events.into_iter().map(|e| (at, e)));
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

#[tokio::test]
async fn b842_the_longest_time_between_two_events_of_a_live_session_is_one_beat_interval() {
    const BEAT: Duration = Duration::from_millis(400);
    let rec = EventRecorder::new("run1", "direct");
    rec.beat_when_quiet(BEAT);
    let sink = Arc::new(StampSink::default());
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_millis(10),
        Duration::from_secs(3_600),
        1,
        MemoryLog::new(),
        cancel,
    );
    // The session is quiet until its first beat; a moment after it the engine reports a tool call and the command then runs quietly.
    let waited = std::time::Instant::now();
    while !sink.events.lock().unwrap().iter().any(|(_, e)| is_beat(e)) {
        assert!(
            waited.elapsed() < Duration::from_secs(5),
            "a quiet recorder wrote no beat in 5 s"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    rec.record(
        EventType::ToolSummary,
        json!({"title": "a long quiet command"}),
    );
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    flusher.stop().await;
    let events = sink.events.lock().unwrap().clone();
    let call = events
        .iter()
        .position(|(_, e)| e.event_type == EventType::ToolSummary)
        .expect("the tool call was posted");
    assert!(
        events.len() > call + 1,
        "no event followed the tool call in 1.3 s of quiet"
    );
    // The promise of `beatS`: while the session lives, two events are never further apart than one beat interval. Half an interval is
    // allowed for the flush tick and a busy host; the timer of the first version waited almost two intervals here.
    let limit = BEAT + BEAT / 2;
    for pair in events[call..].windows(2) {
        let gap = pair[1].0.duration_since(pair[0].0);
        assert!(
            gap <= limit,
            "{} ms passed between two events of a live session; the beat interval is {} ms",
            gap.as_millis(),
            BEAT.as_millis()
        );
    }
}

#[tokio::test]
async fn b842_a_session_that_waits_for_an_answer_writes_no_beat_until_the_question_is_closed() {
    const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
    let rec = EventRecorder::new("run1", "direct");
    rec.beat_when_quiet(Duration::from_millis(200));
    let sink = Arc::new(Sink::default());
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    let flusher = spawn_flusher(
        rec.clone(),
        sink.clone(),
        Duration::from_millis(20),
        Duration::from_secs(3_600),
        1,
        MemoryLog::new(),
        cancel,
    );
    // A notice that blocks nothing does not stop the beats: the session works on.
    rec.record(
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "needs-input", "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT0A", "reason": "provider_outage",
               "title": "t", "blocking": false, "options": [], "canContinue": false, "waitS": 0}),
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    let beats = |sink: &Sink| {
        sink.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| is_beat(e))
            .count()
    };
    let before = beats(&sink);
    assert!(before >= 1, "a notice that blocks nothing stops no beat");
    // The session asks its question and waits for seven beat intervals. The run list finds an open question in the newest 200 events
    // of the agent: a beat a minute would push the question out of that tail after 200 minutes of a wait that may last 30 days.
    rec.record(
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "needs-input", "requestId": REQ, "reason": "engine_stop", "title": "t", "blocking": true,
               "options": ["finish"], "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-15T14:11:07Z"}),
    );
    tokio::time::sleep(Duration::from_millis(1_400)).await;
    let during = beats(&sink) - before;
    // The question closes; the session is quiet for five more intervals and beats again.
    rec.record(
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "input-closed", "requestId": REQ, "how": "expired", "waitedS": 1800, "text": "closed"}),
    );
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    flusher.stop().await;
    let after = beats(&sink) - before - during;
    assert_eq!(
        during, 0,
        "a session that waits for the user's answer wrote {during} beat(s) behind its open question"
    );
    assert!(
        (2..=6).contains(&after),
        "a quiet second after the question closed, with a beat every 0.2 s: {after} beats"
    );
}

/// Session 3 of the run in a new machine: it restores the final bundle of session 1, because session 2 was lost before it saved a copy.
async fn third_session(bundle: Vec<u8>, head: &str, base: &str) -> Direct {
    let mut d3 = Direct::start(Opts::default()).await;
    d3.aws.put(&keys::bundle_key_t(RUN, 1), bundle);
    d3.req.turn = 3;
    d3.req.runtime_session_id = format!("{RUN}-direct-g00000003");
    d3.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d3.req.snapshot_every_s = Some(0);
    d3.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.to_string()),
        restore_head: Some(head.to_string()),
        sessions_before: Some(2),
        brief: None,
    });
    validate_direct_invocation(&d3.req).expect("the invocation of session 3 is valid");
    d3
}

#[tokio::test(flavor = "multi_thread")]
async fn b842_the_count_steps_over_a_session_whose_missing_record_the_store_answers_with_403() {
    let (bundle, head, base) = first_session().await;
    let d3 = third_session(bundle, &head, &base).await;
    // Session 1 handed over with four copies; session 2 was lost before its first. The worker's role may list the bucket only under its
    // own prefixes, so the store answers 403 for the record session 2 never wrote.
    d3.aws.put(
        &keys::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1, "headSha": head, "baseSha": base, "at": "2026-11-14T07:03:31Z", "count": 4,
        }))
        .unwrap(),
    );
    d3.aws.absent_gets_answer_403("snapshot.json");
    let (_, resp) = d3.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let r = record_of(&d3, 3);
    assert_eq!(
        r["count"], 5,
        "the first copy of session 3 is the fifth copy of the run, the number after session 1's newest: {r}"
    );
    let copies = saved_copies(&d3);
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0]["count"], 5);
    assert_eq!(copies[0]["session"], 3);
}
