//! `ham watch` waits for the outcome label. The harness writes `goal.status` complete, `delivery.ready` and
//! `run.completed` (the outcome label) in that order, so a watch that ends when the zip is announced can print its final card before the
//! label arrives. The watch reads on for a bounded time while a run is complete with its zip announced and `run.completed` has not
//! come; a run that ends without `run.completed` (cleared, aborted) is not waited for.
//!
//! These tests use the watch as it is called today (`watch`, the default bound). The bound itself is in `watch_label_bound.rs`.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::watch::{WatchOptions, exit_of, watch};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::envelope::EventType;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::{Fake, MODEL, REGION, creds};

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 4,
        base: Duration::from_millis(10),
    })
}

fn opts(json: bool) -> WatchOptions {
    WatchOptions {
        interval: Duration::from_millis(20),
        json,
        download_dir: Some(support::scratch("watch-label")),
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

/// The harness side of a delivered direct run up to `delivery.ready`, without `run.completed`: the session, `complete`, the zip announced.
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

/// The stand-in player: writes `run.completed` with the label `after` the delivery.
fn label_after(fake: &Arc<Fake>, run: &str, after: Duration) -> tokio::task::JoinHandle<()> {
    let (fake, run) = (fake.clone(), run.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        fake.mem.harness(
            &run,
            EventType::RunCompleted,
            json!({"status": "complete", "outcome": DirectOutcome::ReportedDone.as_str(),
                   "label": DirectOutcome::ReportedDone.label()}),
        );
    })
}

/// done-when 1: a stand-in writes `run.completed` 1.5 s after `goal.status`; the final card carries the label.
#[tokio::test]
async fn the_final_card_carries_the_label_that_arrives_1_5_s_after_the_zip() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "label late").await;
    deliver_without_label(&fake, &run);
    let late = label_after(&fake, &run, Duration::from_millis(1500));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let began = Instant::now();
    let done = watch(&api(&fake), &run, &opts(false), &mut out, &mut err)
        .await
        .unwrap();
    let took = began.elapsed();
    late.await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        took >= Duration::from_millis(1400),
        "the watch ended after {took:?}, before the label was written"
    );
    assert!(done.view.completed, "run.completed was read:\n{text}");
    assert!(
        text.lines().any(|l| l == "Outcome   Cline reported done"),
        "the card carries the label:\n{text}"
    );
    let status = text.lines().rfind(|l| l.starts_with("run ")).unwrap();
    assert!(status.ends_with("outcome: Cline reported done"), "{text}");
    assert!(done.pulled.is_some(), "the zip downloads after the wait");
    assert_eq!(exit_of(&done.view), 0);
    assert!(
        err.is_empty(),
        "nothing was missing: {}",
        String::from_utf8_lossy(&err)
    );
}

/// The same with `--json`: the last object is read after the label arrives.
#[tokio::test]
async fn the_json_watch_carries_the_label_that_arrives_late() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "label late json").await;
    deliver_without_label(&fake, &run);
    let late = label_after(&fake, &run, Duration::from_millis(1500));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(&api(&fake), &run, &opts(true), &mut out, &mut err)
        .await
        .unwrap();
    late.await.unwrap();
    let last: Value =
        serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(last["run"]["outcome"], "reported_done");
    assert_eq!(last["run"]["label"], "Cline reported done");
    assert_eq!(last["run"]["completed"], true);
    assert_eq!(last["exitCode"], 0);
}

/// A harness-cleared run never gets `run.completed`: the watch does not wait for it (the default bound is 30 s; this ends at once).
#[tokio::test]
async fn a_cleared_run_is_not_waited_for() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "label cleared").await;
    fake.mem
        .harness(&run, EventType::GoalStatus, json!({"status": "cleared"}));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let began = Instant::now();
    let done = watch(&api(&fake), &run, &opts(false), &mut out, &mut err)
        .await
        .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(exit_of(&done.view), 15);
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
}

/// `run.aborted` is a run that ended without `run.completed` too, even when the goal had reached complete with its zip announced.
#[tokio::test]
async fn an_aborted_run_is_not_waited_for() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "label aborted").await;
    deliver_without_label(&fake, &run);
    fake.mem.harness(
        &run,
        EventType::RunAborted,
        json!({"status": "ABORTED", "error": "HarnessOperatorStop", "cause": "operator stop",
               "stoppedAt": "2026-10-15T20:00:00Z"}),
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let began = Instant::now();
    let done = watch(&api(&fake), &run, &opts(false), &mut out, &mut err)
        .await
        .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert!(done.view.aborted.is_some());
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
}

/// A run whose `run.completed` is already written does not wait at all.
#[tokio::test]
async fn a_run_that_already_has_its_label_does_not_wait() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "label there").await;
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let began = Instant::now();
    let done = watch(&api(&fake), &run, &opts(false), &mut out, &mut err)
        .await
        .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert!(done.view.completed);
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
}
