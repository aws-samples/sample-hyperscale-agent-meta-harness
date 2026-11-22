//! The commits of the test support's fixtures carry fixed dates.
//!
//! git stamps a commit with the second of the clock unless both of its dates are given. The fixture of a test was then another
//! repository in every second: each stand-in store of a test held a starting repository of its own, and whether the copies of a
//! later session left out what the run's base holds followed the second the stores were made in.
//! The runner of git in `common` names both dates, so the same fixture is the same commits whenever it is made, every stand-in
//! store of a test holds the same starting repository, and the form of a later session's copies is the release's.
//!
//! The test support reads no clock of its own and git has no setting that moves the time of a commit, so each test here waits
//! until the clock shows the next second (one second at the most) between the two things it compares.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use common::{git, python_calc, scratch};
use direct_support::{BUCKET, Direct, Opts, RUN, checkout_of_copy, needs_the_base};
use harness_protocol::bundle_header;
use harness_protocol::direct::{DirectSlice, s3 as keys, validate_direct_invocation};
use harness_protocol::runtime::{RepoSource, TurnOutcome};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The second the clock shows now.
fn second_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Returns once the clock shows a later second than `second`.
async fn past(second: u64) {
    while second_now() <= second {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The heads that the header of the bundle a run starts from names, as the worker reads them for the form of its copies.
fn heads_of_the_base(d: &Direct) -> Vec<String> {
    let bundle = d
        .aws
        .get(&keys::repo_bundle_key(RUN))
        .expect("the bundle the run starts from");
    let cut = bundle.len().min(65_536);
    bundle_header::heads(&bundle[..cut]).expect("a bundle header")
}

#[tokio::test(flavor = "multi_thread")]
async fn b892_a_fixture_made_in_a_later_second_is_the_same_commits() {
    let dir = scratch("b892-dates");
    let one = python_calc(&dir.join("one"));
    let made = second_now();
    past(made).await;
    assert!(
        second_now() > made,
        "TRIGGER PRESENT: the clock still shows the second the first fixture was made in"
    );
    let two = python_calc(&dir.join("two"));
    assert_eq!(
        git(&one, &["rev-parse", "HEAD"]),
        git(&two, &["rev-parse", "HEAD"]),
        "the same fixture made one second later is another repository:\n{}\n\n{}",
        git(&one, &["cat-file", "commit", "HEAD"]),
        git(&two, &["cat-file", "commit", "HEAD"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b892_every_stand_in_store_holds_the_same_starting_repository() {
    let d1 = Direct::start(Opts::default()).await;
    let made = second_now();
    past(made).await;
    let d2 = Direct::start(Opts::default()).await;
    let (first, second) = (heads_of_the_base(&d1), heads_of_the_base(&d2));
    assert!(
        !first.is_empty(),
        "TRIGGER PRESENT: the bundle a run starts from names no head"
    );
    assert_eq!(
        first, second,
        "two stand-in stores of one test hold two starting repositories"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b892_the_copies_of_a_later_session_in_a_new_store_leave_out_what_the_runs_base_holds() {
    // Session 1 of a run, to its end.
    let d1 = Direct::start(Opts::default()).await;
    let made = second_now();
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d1.summary();
    let head = s.git.head_sha.clone().expect("session 1 delivered a head");
    let base = s.git.base_sha.clone().expect("session 1 has a base");
    let bundle = d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("session 1 bundled its work");

    // Session 2 in a new machine and a new stand-in store, made in a later second. Nothing of store 1 is handed on but what the
    // service hands to a later session: the bundle of session 1. The bundle the run started from is the one this store was made with.
    past(made).await;
    let mut d2 = Direct::start(Opts::default()).await;
    d2.aws.put(&keys::bundle_key_t(RUN, 1), bundle);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    // No copy while the engine works: the one copy of the session is the copy of its end.
    d2.req.snapshot_every_s = Some(0);
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base),
        restore_head: Some(head),
        sessions_before: Some(1),
        brief: None,
    });
    validate_direct_invocation(&d2.req).expect("the invocation of session 2 is valid");
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");

    let copy = d2
        .aws
        .get(&keys::snapshot_bundle_key_t(RUN, 2))
        .expect("TRIGGER PRESENT: session 2 stored no copy");
    assert!(
        needs_the_base(&copy),
        "the copy of a later session holds the whole history: its store was made with another starting repository than the run's, so the form of the copy followed the clock"
    );
    // The copy and the starting repository of this store are the work together.
    let checkout = checkout_of_copy(
        &d2.aws,
        &d2.dir,
        &keys::snapshot_bundle_key_t(RUN, 2),
        "b892-copy",
    );
    assert!(
        checkout.join("src/calc.py").is_file(),
        "the checkout of the copy holds the files of the run's starting repository"
    );
}
