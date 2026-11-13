//! The bound of the label wait of `ham watch`. A watch that sees a run complete with its zip announced and
//! no `run.completed` reads on for at most the settle bound. It waits for one thing: the harness's `run.completed` record, which carries
//! the outcome label. When the bound passes without it, the watch ends, says on stderr that the label did not arrive, and the card has an
//! Outcome line that says the same. A failing read during the wait does not extend it.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::watch::{COMPLETED_SETTLE, WatchOptions, awaiting_completed, exit_of, watch_settled};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::EventType;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::{Fake, MODEL, REGION, creds};

const BOUND: Duration = Duration::from_millis(600);

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 2,
        base: Duration::from_millis(10),
    })
}

fn opts(json: bool) -> WatchOptions {
    WatchOptions {
        interval: Duration::from_millis(20),
        json,
        download_dir: Some(support::scratch("watch-label-bound")),
    }
}

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, &format!("k-{}", prompt.replace(' ', "-")))
        .await
        .unwrap()
        .run_id
}

/// The harness side of a delivered direct run up to `delivery.ready`, without `run.completed`.
fn deliver_without_label(fake: &Fake, run: &str) {
    let bytes = support::zip_bytes(3000);
    let size = bytes.len();
    let (artifact, sha) = fake.mem.deliverable(run, bytes);
    fake.mem.harness(
        run,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
    );
    fake.mem.harness(
        run,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "delivering"}),
    );
    fake.mem.harness(
        run,
        EventType::DeliveryReady,
        json!({"artifactId": artifact, "sha256": sha, "sizeBytes": size}),
    );
}

fn view_of(fake: &Fake, run: &str) -> RunView {
    // The fixture view for the truth table below, built from the events the fake holds.
    let mut s = RunState::new(run);
    for e in fake.mem.inner.lock().unwrap().events.iter().cloned() {
        if e.run_id == run {
            s.apply(e);
        }
    }
    s.view()
}

/// The watch's output, which runs `fault` when the watch writes its first line (FLK3). The first line is the status of the run as the first
/// cycle read it: complete, with its zip announced and no `run.completed`. The watch starts the wait for that record in the same step, before
/// it reads again, so a fault installed here is met only by reads of the wait, however long the first cycle took. A timer task cannot promise
/// that: on a loaded host the first cycle ran past the timer, met the fault, and the watch never ended (2 of 20 runs of the 503 test with 100 busy
/// processes, 10 of 20 of the hanging-read test).
struct FaultOnFirstLine<F: FnOnce()> {
    fault: Option<F>,
}

impl<F: FnOnce()> FaultOnFirstLine<F> {
    fn new(fault: F) -> Self {
        Self { fault: Some(fault) }
    }
}

impl<F: FnOnce()> Write for FaultOnFirstLine<F> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(fault) = self.fault.take() {
            fault();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How many reads of the run's events the stage has seen so far.
fn events_reads(fake: &Fake, run: &str) -> usize {
    fake.mem
        .seen()
        .iter()
        .filter(|r| r.method == "GET" && r.path.contains(&format!("{run}/events")))
        .count()
}

/// The longest a test lets a watch run. The watch of these tests ends at its bound of 0.6 s; a watch that does not end is a failure, not a
/// stuck run.
const WATCH_LIMIT: Duration = Duration::from_secs(30);

#[test]
fn the_bound_is_a_sanity_bound_of_30_seconds() {
    assert_eq!(COMPLETED_SETTLE, Duration::from_secs(30));
}

#[tokio::test]
async fn only_a_complete_run_with_its_zip_and_no_run_completed_awaits_the_label() {
    let fake = Fake::start().await;
    // Active run: not finished.
    let run = new_run(&fake, "await active").await;
    assert!(!awaiting_completed(&view_of(&fake, &run)));
    // Complete with its zip announced and no run.completed: awaits it.
    deliver_without_label(&fake, &run);
    assert!(awaiting_completed(&view_of(&fake, &run)));
    // The label arrived: nothing to await.
    fake.mem.harness(
        &run,
        EventType::RunCompleted,
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    );
    assert!(!awaiting_completed(&view_of(&fake, &run)));
    // Cleared: finished, never awaits.
    let cleared = new_run(&fake, "await cleared").await;
    fake.mem.harness(
        &cleared,
        EventType::GoalStatus,
        json!({"status": "cleared"}),
    );
    assert!(!awaiting_completed(&view_of(&fake, &cleared)));
    // Aborted after a delivery: never awaits.
    let aborted = new_run(&fake, "await aborted").await;
    deliver_without_label(&fake, &aborted);
    fake.mem.harness(
        &aborted,
        EventType::RunAborted,
        json!({"status": "ABORTED", "error": "HarnessOperatorStop", "cause": "stop",
               "stoppedAt": "2026-10-15T20:00:00Z"}),
    );
    assert!(!awaiting_completed(&view_of(&fake, &aborted)));
    // Blocked, no run.completed: not finished, so nothing to await either.
    let blocked = new_run(&fake, "await blocked").await;
    fake.mem.harness(
        &blocked,
        EventType::GoalStatus,
        json!({"status": "blocked"}),
    );
    assert!(!awaiting_completed(&view_of(&fake, &blocked)));
}

/// done-when 2: a missing record ends the wait at the bound, and the card says the label is missing.
#[tokio::test]
async fn a_label_that_never_comes_ends_the_wait_at_the_bound_and_the_card_says_so() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "bound missing").await;
    deliver_without_label(&fake, &run);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let began = Instant::now();
    let done = watch_settled(&api(&fake), &run, &opts(false), BOUND, &mut out, &mut err)
        .await
        .unwrap();
    let took = began.elapsed();
    assert!(
        took >= BOUND,
        "the watch gave up after {took:?}, before the bound"
    );
    assert!(took < Duration::from_secs(10), "{took:?}");
    let (text, err) = (
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    );
    assert!(!done.view.completed && done.view.result.is_none());
    assert!(
        text.lines().any(|l| l.starts_with("Outcome   not received")
            && l.contains("run.completed")
            && l.contains("0.6 s")),
        "the card says the label is missing:\n{text}"
    );
    assert!(
        err.contains("run.completed did not arrive within 0.6 s"),
        "stderr says so too: {err}"
    );
    assert!(done.pulled.is_some(), "the zip downloads anyway");
    assert_eq!(
        exit_of(&done.view),
        0,
        "the run is complete; a missing label is not a failure"
    );
}

/// The bound counts from the moment the run is complete with its zip announced, not from the start of the watch.
#[tokio::test]
async fn the_wait_starts_when_the_zip_is_announced() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "bound starts late").await;
    let (a, r) = (api(&fake), run.clone());
    let watcher = tokio::spawn(async move {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let began = Instant::now();
        let done = watch_settled(&a, &r, &opts(false), BOUND, &mut out, &mut err)
            .await
            .unwrap();
        (began, Instant::now(), done, String::from_utf8(err).unwrap())
    });
    // The run works for twice the bound, then delivers.
    tokio::time::sleep(BOUND * 2).await;
    let delivered = Instant::now();
    deliver_without_label(&fake, &run);
    let (began, ended, done, err) = watcher.await.unwrap();
    assert!(ended - began >= BOUND * 2 + BOUND, "{:?}", ended - began);
    assert!(ended - delivered >= BOUND, "{:?}", ended - delivered);
    assert!(!done.view.completed);
    assert!(err.contains("did not arrive"), "{err}");
}

/// A label that arrives inside the bound ends the wait at once, with no missing-label note.
#[tokio::test]
async fn a_label_inside_the_bound_ends_the_wait_with_no_note() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "bound label in time").await;
    deliver_without_label(&fake, &run);
    let (f, r) = (fake.clone(), run.clone());
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        f.mem.harness(
            &r,
            EventType::RunCompleted,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        );
    });
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let began = Instant::now();
    let done = watch_settled(
        &api(&fake),
        &run,
        &opts(false),
        Duration::from_secs(20),
        &mut out,
        &mut err,
    )
    .await
    .unwrap();
    late.await.unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert!(done.view.completed);
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.lines().any(|l| l == "Outcome   Cline reported done"),
        "{text}"
    );
    assert!(!text.contains("not received"), "{text}");
}

#[tokio::test]
async fn the_json_watch_without_a_label_says_so_on_stderr_and_has_no_label_member() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "bound json").await;
    deliver_without_label(&fake, &run);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch_settled(&api(&fake), &run, &opts(true), BOUND, &mut out, &mut err)
        .await
        .unwrap();
    let last: Value =
        serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
    assert!(last["run"].get("label").is_none(), "{last}");
    assert_eq!(last["run"]["completed"], false);
    assert_eq!(last["exitCode"], 0);
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("run.completed did not arrive within 0.6 s")
    );
}

/// Reads that fail during the wait do not extend it: the watch ends at the bound with the view it has. The fault (every read of the events
/// answers 503) is installed by the watch's own first line, so it is met by the reads of the wait and by no read of the first cycle. The test
/// task is stalled for 250 ms before the watch starts, as a loaded host would, and that changes nothing: the old timer of 150 ms hit the first
/// cycle on every run with the stall (FLK3).
#[tokio::test]
async fn failing_reads_during_the_wait_do_not_extend_it() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "bound faults").await;
    deliver_without_label(&fake, &run);
    let f = fake.clone();
    let mut out =
        FaultOnFirstLine::new(move || f.faults.events_503.store(u32::MAX, Ordering::SeqCst));
    let mut err = Vec::new();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let began = Instant::now();
    let done = tokio::time::timeout(
        WATCH_LIMIT,
        watch_settled(&api(&fake), &run, &opts(false), BOUND, &mut out, &mut err),
    )
    .await
    .expect("the watch ended")
    .unwrap();
    let took = began.elapsed();
    assert!(took >= BOUND && took < Duration::from_secs(10), "{took:?}");
    assert!(!done.view.completed);
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("run.completed did not arrive"), "{err}");
    // The trigger: reads of the wait did fail. Each answer of 503 takes one off the count that was set.
    let served = u32::MAX - fake.faults.events_503.load(Ordering::SeqCst);
    assert!(served >= 1, "no read met the fault during the wait");
    assert!(
        err.contains("ham: reading events:") && err.contains("retrying in"),
        "the watch did not report a failed read: {err}"
    );
}

/// A read that hangs during the wait is cut at the bound: the watch ends at the bound, not when the read comes back. The slow answers (5 s for
/// every read of the events) start with the watch's first line, so no read of the first cycle is slow (FLK3, as above).
#[tokio::test]
async fn a_read_that_hangs_during_the_wait_is_cut_at_the_bound() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "bound hang").await;
    deliver_without_label(&fake, &run);
    let reads_at_fault = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (f, r, at) = (fake.clone(), run.clone(), reads_at_fault.clone());
    let mut out = FaultOnFirstLine::new(move || {
        at.store(events_reads(&f, &r), Ordering::SeqCst);
        f.faults.events_delay_ms.store(5_000, Ordering::SeqCst);
    });
    let mut err = Vec::new();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let began = Instant::now();
    let done = tokio::time::timeout(
        WATCH_LIMIT,
        watch_settled(&api(&fake), &run, &opts(false), BOUND, &mut out, &mut err),
    )
    .await
    .expect("the watch ended")
    .unwrap();
    let took = began.elapsed();
    fake.faults.events_delay_ms.store(0, Ordering::SeqCst);
    assert!(took >= BOUND, "{took:?}");
    assert!(
        took < Duration::from_secs(3),
        "the hanging read held the watch for {took:?}"
    );
    assert!(!done.view.completed);
    let err = String::from_utf8(err).unwrap();
    assert!(
        err.contains("run.completed did not arrive within 0.6 s"),
        "{err}"
    );
    // The trigger: a read of the events began after the slow answers were installed and was cut. A cycle waits for all its reads, so a watch whose
    // reads hang has begun one cycle's reads (one per shard at most) and no more; a stage that answered would have been read again and again.
    let after = events_reads(&fake, &run) - reads_at_fault.load(Ordering::SeqCst);
    assert!(
        (1..=usize::from(harness_protocol::store::EVENT_SHARDS)).contains(&after),
        "{after} reads of the events began after the slow answers were installed; one cycle's reads were expected"
    );
}
