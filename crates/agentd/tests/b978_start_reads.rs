//! A read on the way to a run's start is asked again when the storage service's answer may be another one a moment later.
//! The real driver in process, the fake engine, the fake AWS of `direct_support`, whose bucket answers a GET as the test says:
//! 500 `InternalError` (what the run of the live rows of release 5.33 met), 503 `SlowDown` (a throttle), a connection that breaks
//! in the body, 403 `AccessDenied`, 404. No AWS call is made.
//!
//! The bounds are those of the uploads: `UPLOAD_RETRIES` further reads, with the pause of the worker's configuration that doubles
//! (10 ms in `direct_support`, so a test waits 70 ms where a session waits 14 s). An answer that stays what it is however often
//! it is asked (the object is not there, the access is refused) is one read with the sentence of one read. No test holds an
//! upper bound of time: the waits are held from below.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::UPLOAD_RETRIES;
use direct_support::{Direct, GetFailure, Opts, RUN};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::runtime::TurnOutcome;
use std::time::{Duration, Instant};

/// The first pause of the worker of `direct_support` before a store is tried again (`upload_retry_pause`); it doubles.
const PAUSE: Duration = Duration::from_millis(10);

const REQUEST: &str = "input/request.json";
const BUNDLE: &str = "input/repo.bundle";

/// How many GETs a read makes when every answer may get better: the first and the further ones.
fn reads() -> usize {
    usize::try_from(UPLOAD_RETRIES).unwrap() + 1
}

/// The pauses between `reads()` reads.
fn waits() -> Duration {
    let mut sum = Duration::ZERO;
    let mut wait = PAUSE;
    for _ in 0..UPLOAD_RETRIES {
        sum += wait;
        wait *= 2;
    }
    sum
}

/// The run started: the engine got its prompt and the turn came to its end.
async fn started(d: &Direct, what: &str) {
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{what}: {resp:?}");
    assert!(
        !d.prompts_received().is_empty(),
        "{what}: the engine got its prompt"
    );
    assert_ne!(
        d.summary().outcome,
        DirectOutcome::StoppedSetupFailed,
        "{what}"
    );
}

/// The run ended before the engine started: the sentence a person reads under "Cline did not start".
async fn not_started(d: &Direct, what: &str) -> String {
    let (_, resp) = d.invoke().await;
    assert!(
        d.prompts_received().is_empty(),
        "{what}: no model call: {resp:?}"
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedSetupFailed,
        "{what}: {}",
        s.label
    );
    assert_eq!(s.label, "Cline did not start", "{what}");
    s.detail.unwrap_or_default()
}

fn log_says(d: &Direct, text: &str) -> bool {
    d.log.lines().iter().any(|l| l.contains(text))
}

/// One temporary answer to the read of `key_part`, then the object: the run starts after two GETs and the log names the read.
async fn one_temporary_answer(key_part: &str, key: &str, named: &str, how: GetFailure) {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_gets_as(key_part, 1, how);
    let what = format!("{how:?} once to {key_part}");
    started(&d, &what).await;
    assert_eq!(d.aws.gets_of(key).len(), 2, "{what}: two GETs");
    assert!(
        log_says(
            &d,
            &format!("direct turn start read: {named} was read at try 2")
        ),
        "{what}: {:?}",
        d.log.lines()
    );
}

/// Every answer to the read of `key_part` is a 500: the start ends after `reads()` GETs and their waits, and the sentence says so.
/// The waits are held where the worker makes them: each GET arrives at the stand-in no sooner after the one before it than the
/// pause between the two (the pause, twice it, four times it), so a worker that passes no pause to the read fails here. The
/// time of the whole start holds that only by chance: with no pause at all a start took 61 and 67 ms when this was written,
/// a few milliseconds under the waits of 70 ms, and a start that is a little slower passes.
async fn only_temporary_answers(key_part: &str, key: &str, starts_with: &str) {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_gets(key_part, u32::MAX);
    let began = Instant::now();
    let detail = not_started(&d, key_part).await;
    let took = began.elapsed();
    assert_eq!(d.aws.gets_of(key).len(), reads(), "{key_part}: {detail}");
    assert!(detail.starts_with(starts_with), "{detail}");
    assert!(
        detail.contains("returned 500 InternalError: We encountered an internal error."),
        "{detail}"
    );
    assert!(
        detail.ends_with(&format!("(the read was tried {} times)", reads())),
        "{detail}"
    );
    assert!(
        took >= waits(),
        "{key_part}: the reads waited {:?} between them, {took:?} passed",
        waits()
    );
    let arrivals = d.aws.get_arrivals_of(key);
    assert_eq!(arrivals.len(), reads(), "{key_part}: {arrivals:?}");
    let mut wait = PAUSE;
    for (n, (earlier, later)) in arrivals.iter().zip(arrivals.iter().skip(1)).enumerate() {
        let gap = later.duration_since(*earlier);
        assert!(
            gap >= wait,
            "{key_part}: GET {} arrived {gap:?} after GET {}, and the pause between the two is {wait:?}",
            n + 2,
            n + 1
        );
        wait *= 2;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_500_once_to_the_read_of_the_request_is_asked_again_and_the_run_starts() {
    one_temporary_answer(
        REQUEST,
        &keys::request_key(RUN),
        "request.json",
        GetFailure::Status500,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_throttle_once_to_the_read_of_the_request_is_asked_again_and_the_run_starts() {
    one_temporary_answer(
        REQUEST,
        &keys::request_key(RUN),
        "request.json",
        GetFailure::SlowDown,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_connection_that_breaks_once_in_the_read_of_the_request_is_asked_again_and_the_run_starts()
 {
    one_temporary_answer(
        REQUEST,
        &keys::request_key(RUN),
        "request.json",
        GetFailure::ResetBody,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_500_every_time_to_the_read_of_the_request_ends_the_start_and_names_the_tries() {
    only_temporary_answers(
        REQUEST,
        &keys::request_key(RUN),
        "request.json: s3 GET s3://",
    )
    .await;
}

/// The object is not there: one GET, and the sentence of one read.
#[tokio::test(flavor = "multi_thread")]
async fn b978_a_request_that_is_not_there_is_asked_once() {
    let d = Direct::start(Opts::default()).await;
    d.aws.remove(&keys::request_key(RUN));
    let detail = not_started(&d, "no request").await;
    assert_eq!(d.aws.gets_of(&keys::request_key(RUN)).len(), 1, "{detail}");
    assert!(detail.starts_with("request.json: s3 GET s3://"), "{detail}");
    assert!(detail.contains("returned 404 NoSuchKey"), "{detail}");
    assert!(!detail.contains("tried"), "{detail}");
}

/// The access is refused: one GET, and the sentence of one read.
#[tokio::test(flavor = "multi_thread")]
async fn b978_a_refused_read_of_the_request_is_asked_once() {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_gets_as(REQUEST, u32::MAX, GetFailure::Denied);
    let detail = not_started(&d, "refused").await;
    assert_eq!(d.aws.gets_of(&keys::request_key(RUN)).len(), 1, "{detail}");
    assert!(
        detail.contains("returned 403 AccessDenied: Access Denied"),
        "{detail}"
    );
    assert!(!detail.contains("tried"), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_500_once_to_the_read_of_the_bundle_is_asked_again_and_the_run_starts() {
    one_temporary_answer(
        BUNDLE,
        &keys::repo_bundle_key(RUN),
        "the bundle to restore",
        GetFailure::Status500,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_500_every_time_to_the_read_of_the_bundle_ends_the_start_and_names_the_tries() {
    only_temporary_answers(
        BUNDLE,
        &keys::repo_bundle_key(RUN),
        "worktree: s3 GET s3://",
    )
    .await;
}

/// The bundle is not there: one GET, and the sentence of one read.
#[tokio::test(flavor = "multi_thread")]
async fn b978_a_bundle_that_is_not_there_is_asked_once() {
    let d = Direct::start(Opts {
        no_bundle: true,
        ..Opts::default()
    })
    .await;
    let detail = not_started(&d, "no bundle").await;
    assert_eq!(
        d.aws.gets_of(&keys::repo_bundle_key(RUN)).len(),
        1,
        "{detail}"
    );
    assert!(detail.starts_with("worktree: s3 GET s3://"), "{detail}");
    assert!(detail.contains("returned 404"), "{detail}");
    assert!(!detail.contains("tried"), "{detail}");
}
