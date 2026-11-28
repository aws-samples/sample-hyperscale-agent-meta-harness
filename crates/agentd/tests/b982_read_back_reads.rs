//! The two reads of a read-back are asked again when the storage service's answer may be another one a moment later.
//!
//! When a session is stopped from outside, the service has its storage read back: a new machine stores a copy of the work
//! tree and then compares it with the run's newest saved copy. The comparison reads that saved copy from the bucket, and the
//! bundle the run started from when the saved copy needs it and the storage no longer holds it. Each read was one GET. One
//! temporary answer left the comparison unknown, and the service starts no session from a copy whose comparison is unknown: the
//! run went on from the saved copy, without the work the stopped session had written after it.
//!
//! Now both reads have the bounds of the start reads (`S3Objects::get_to_file_retrying`): `UPLOAD_RETRIES` further GETs
//! with the upload pause, which doubles (10 ms in `direct_support`), for a 5xx, a throttle and a request that did not get its
//! whole answer. An answer that cannot get better (not there, refused) keeps one GET and the reason of before. The copy read back
//! is stored before either read, and stays stored whatever they come to (root invariant 9). The whole read keeps its one limit,
//! the request's `budgetS`.
//!
//! The stand-in is `b854_support`: session 1 of a run ended, a file was written after its last stored copy, and a second agentd
//! with the same storage is the new machine. The bucket of `direct_support` answers a GET as the test says. No AWS call is made.
#![allow(clippy::unwrap_used)]

mod b854_support;
mod common;
mod direct_support;

use agentd::direct::UPLOAD_RETRIES;
use b854_support::{ATTEMPT, Machine, Stopped, new_machine, stopped_session};
use direct_support::{GetFailure, RUN, needs_the_base};
use harness_protocol::direct::{RecoveryAnswer, s3 as keys};
use std::time::{Duration, Instant};

/// The first pause of the worker of `direct_support` before a store is tried again (`upload_retry_pause`); it doubles.
const PAUSE: Duration = Duration::from_millis(10);

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

fn saved_key() -> String {
    keys::snapshot_bundle_key_t(RUN, 1)
}

fn base_key() -> String {
    keys::repo_bundle_key(RUN)
}

fn log_says(m: &Machine, text: &str) -> bool {
    m.log.lines().iter().any(|l| l.contains(text))
}

/// The copy read back is stored and named, whatever the comparison came to.
fn the_copy_is_stored(s: &Stopped, answer: &RecoveryAnswer) {
    assert!(answer.recovered, "{answer:?}");
    assert!(answer.copy.is_some(), "{answer:?}");
    assert!(
        s.d.aws.get(&s.key()).is_some(),
        "the copy read back is in the bucket"
    );
}

/// The comparison was made, and it found the file that was written after the saved copy.
fn the_comparison_was_made(answer: &RecoveryAnswer, what: &str) {
    let cmp = answer.comparison.clone().expect("the comparison");
    assert!(
        cmp.done(),
        "{what}: the comparison is unknown ({:?}): the service starts no session from this copy",
        cmp.reason
    );
    assert!(!cmp.same_as_baseline, "{what}: {cmp:?}");
    assert_eq!((cmp.regressed, cmp.rewritten), (0, 0), "{what}: {cmp:?}");
}

/// The comparison is unknown because the saved copy could not be read.
fn the_comparison_is_unknown(answer: &RecoveryAnswer, what: &str) {
    let cmp = answer.comparison.clone().expect("the comparison says why");
    assert!(!cmp.done(), "{what}: {cmp:?}");
    assert_eq!(cmp.reason.as_deref(), Some("baseline-unreadable"), "{what}");
}

/// A stopped session whose storage no longer holds the bundle the run started from: the read-back has to take it from the bucket.
async fn stopped_without_the_base() -> Stopped {
    let s = stopped_session().await;
    assert!(
        needs_the_base(&s.d.aws.get(&saved_key()).unwrap()),
        "TRIGGER PRESENT: the saved copy of the stopped session names a commit it needs"
    );
    std::fs::rename(
        s.d.worktree().join(".git"),
        s.d.dir.join("history-moved-away"),
    )
    .unwrap();
    s
}

// ---- the saved copy for the comparison -----------------------------------------------------------------------------------------

/// One temporary answer to the read of the saved copy, then the object: two GETs, the comparison is made, and the log names the read.
async fn one_temporary_answer_to_the_saved_copy(how: GetFailure) {
    let what = format!("{how:?} once to the saved copy");
    let s = stopped_session().await;
    let before = s.d.aws.gets_of(&saved_key()).len();
    s.d.aws.fail_gets_as(&saved_key(), 1, how);
    let m = new_machine(&s, "b982-saved").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    the_copy_is_stored(&s, &answer);
    assert_eq!(
        s.d.aws.gets_of(&saved_key()).len(),
        before + 2,
        "{what}: two GETs"
    );
    the_comparison_was_made(&answer, &what);
    assert!(
        log_says(
            &m,
            "direct recover session 1: the saved copy was read at try 2"
        ),
        "{what}: {:?}",
        m.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_500_once_to_the_read_of_the_saved_copy_is_asked_again_and_the_comparison_is_made() {
    one_temporary_answer_to_the_saved_copy(GetFailure::Status500).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_throttle_once_to_the_read_of_the_saved_copy_is_asked_again_and_the_comparison_is_made()
 {
    one_temporary_answer_to_the_saved_copy(GetFailure::SlowDown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_connection_that_breaks_once_in_the_read_of_the_saved_copy_is_asked_again_and_the_comparison_is_made()
 {
    one_temporary_answer_to_the_saved_copy(GetFailure::ResetBody).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_500_every_time_to_the_read_of_the_saved_copy_leaves_the_comparison_unknown_and_names_the_tries()
 {
    let s = stopped_session().await;
    let before = s.d.aws.gets_of(&saved_key()).len();
    s.d.aws.fail_gets(&saved_key(), u32::MAX);
    let m = new_machine(&s, "b982-saved-down").await;
    let began = Instant::now();
    let answer = m.recover(&s.request(ATTEMPT)).await;
    let took = began.elapsed();
    // The copy read back was stored before the read, and stays stored.
    the_copy_is_stored(&s, &answer);
    the_comparison_is_unknown(&answer, "a 500 every time");
    assert_eq!(s.d.aws.gets_of(&saved_key()).len(), before + reads());
    let lines = m.log.lines();
    let line = lines
        .iter()
        .find(|l| l.starts_with("direct recover session 1: the saved copy: "))
        .unwrap_or_else(|| panic!("no line of the saved copy: {lines:?}"));
    assert!(
        line.contains("returned 500 InternalError: We encountered an internal error."),
        "{line}"
    );
    assert!(
        line.ends_with(&format!("(the read was tried {} times)", reads())),
        "{line}"
    );
    assert!(
        took >= waits(),
        "the reads waited {:?} between them, {took:?} passed",
        waits()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_saved_copy_that_is_not_there_is_asked_once() {
    let s = stopped_session().await;
    let before = s.d.aws.gets_of(&saved_key()).len();
    s.d.aws.remove(&saved_key());
    let m = new_machine(&s, "b982-saved-gone").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    the_copy_is_stored(&s, &answer);
    the_comparison_is_unknown(&answer, "a saved copy that is not there");
    assert_eq!(
        s.d.aws.gets_of(&saved_key()).len(),
        before + 1,
        "an answer that cannot get better is asked for once"
    );
    assert!(!log_says(&m, "the read was tried"), "{:?}", m.log.lines());
}

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_refused_read_of_the_saved_copy_is_asked_once() {
    let s = stopped_session().await;
    let before = s.d.aws.gets_of(&saved_key()).len();
    s.d.aws
        .fail_gets_as(&saved_key(), u32::MAX, GetFailure::Denied);
    let m = new_machine(&s, "b982-saved-refused").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    the_copy_is_stored(&s, &answer);
    the_comparison_is_unknown(&answer, "a refused read of the saved copy");
    assert_eq!(s.d.aws.gets_of(&saved_key()).len(), before + 1);
    assert!(!log_says(&m, "the read was tried"), "{:?}", m.log.lines());
}

// ---- the bundle the run started from -------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_500_once_to_the_read_of_the_base_is_asked_again_and_the_comparison_is_made() {
    let s = stopped_without_the_base().await;
    let before = s.d.aws.gets_of(&base_key()).len();
    s.d.aws.fail_gets(&base_key(), 1);
    let m = new_machine(&s, "b982-base").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    the_copy_is_stored(&s, &answer);
    assert_eq!(
        s.d.aws.gets_of(&base_key()).len(),
        before + 2,
        "two GETs of the bundle the run started from"
    );
    let cmp = answer.comparison.clone().expect("the comparison");
    assert!(
        cmp.done(),
        "the comparison is unknown ({:?}) after one temporary answer to the read of the base",
        cmp.reason
    );
    assert!(!cmp.same_as_baseline, "{cmp:?}");
    assert!(
        log_says(
            &m,
            "direct recover session 1: the bundle the run started from was read at try 2"
        ),
        "{:?}",
        m.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_500_every_time_to_the_read_of_the_base_leaves_the_comparison_unknown_and_names_the_tries()
 {
    let s = stopped_without_the_base().await;
    let before = s.d.aws.gets_of(&base_key()).len();
    s.d.aws.fail_gets(&base_key(), u32::MAX);
    let m = new_machine(&s, "b982-base-down").await;
    let began = Instant::now();
    let answer = m.recover(&s.request(ATTEMPT)).await;
    let took = began.elapsed();
    the_copy_is_stored(&s, &answer);
    the_comparison_is_unknown(&answer, "a 500 every time to the read of the base");
    assert_eq!(s.d.aws.gets_of(&base_key()).len(), before + reads());
    // The log line of this failure begins with what git said of the saved copy and is cut at the length of a detail line, so
    // the count of the tries is held by the GETs above and the waits below.
    assert!(
        log_says(&m, "direct recover session 1: the saved copy: "),
        "{:?}",
        m.log.lines()
    );
    assert!(
        took >= waits(),
        "the reads waited {:?} between them, {took:?} passed",
        waits()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b982_a_base_that_is_not_there_is_asked_once() {
    let s = stopped_without_the_base().await;
    let before = s.d.aws.gets_of(&base_key()).len();
    s.d.aws.remove(&base_key());
    let m = new_machine(&s, "b982-base-gone").await;
    let answer = m.recover(&s.request(ATTEMPT)).await;
    the_copy_is_stored(&s, &answer);
    the_comparison_is_unknown(&answer, "a base that is not there");
    assert_eq!(
        s.d.aws.gets_of(&base_key()).len(),
        before + 1,
        "an answer that cannot get better is asked for once"
    );
    assert!(!log_says(&m, "the read was tried"), "{:?}", m.log.lines());
}
