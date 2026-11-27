//! The read of the run's base bundle on the way to a later session's start. A session that restores a copy which needs the
//! bundle the run started from downloads that bundle too one 500 to that download
//! ended the start. The real driver in process, the fake engine, the fake AWS of `direct_support`. No AWS call is made.
//!
//! The bounds are those of the uploads (`UPLOAD_RETRIES`, the upload pause of `direct_support`: 10 ms, which doubles). A base that
//! is not there is one read with the sentence of one read. No test holds an upper bound of time.
#![allow(clippy::unwrap_used)]

mod b860_thin_support;
mod common;
mod direct_support;

use agentd::direct::UPLOAD_RETRIES;
use b860_thin_support::{base_form, later_session, record_of, stored_copy, summary_of, whole_form};
use direct_support::{Direct, Opts, RUN, needs_the_base};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::runtime::TurnOutcome;

const BASE: &str = "input/repo.bundle";
const SENTENCE: &str = "worktree: the copy to restore needs the run's base bundle: s3 GET s3://";

/// Session 2 of a run, in a new machine, before it is invoked: it restores the copy session 1 stored, and that copy names a
/// commit it needs, so the session reads the run's base (the first test of `b860_restore.rs` holds that read).
async fn session_two() -> Direct {
    let d1 = Direct::start(base_form(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    }))
    .await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let saved = stored_copy(&d1, 1);
    assert!(
        needs_the_base(&saved),
        "TRIGGER PRESENT: the copy to restore names a commit it needs"
    );
    let head = record_of(&d1, 1)["headSha"].as_str().unwrap().to_string();
    let base = d1.summary().git.base_sha.clone().unwrap();
    later_session(
        whole_form(Opts {
            scenario: "end_turn_done",
            ..Opts::default()
        }),
        &d1,
        &keys::snapshot_bundle_key_t(RUN, 1),
        saved,
        &head,
        &base,
    )
    .await
}

fn reads() -> usize {
    usize::try_from(UPLOAD_RETRIES).unwrap() + 1
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_500_once_to_the_read_of_the_base_is_asked_again_and_the_session_starts() {
    let d2 = session_two().await;
    d2.aws.fail_gets(BASE, 1);
    let (_, resp) = d2.invoke().await;
    assert_ne!(
        summary_of(&d2, 2).outcome,
        DirectOutcome::StoppedSetupFailed,
        "{resp:?}"
    );
    assert!(
        !d2.prompts_received().is_empty(),
        "the engine of session 2 started: {resp:?}"
    );
    // The base was asked for twice, whole, and nothing else of it was read.
    assert_eq!(
        d2.aws.gets_of(&keys::repo_bundle_key(RUN)),
        vec![None, None]
    );
    assert!(
        d2.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn start read: the run's base bundle was read at try 2")),
        "{:?}",
        d2.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_500_every_time_to_the_read_of_the_base_ends_the_start_and_names_the_tries() {
    let d2 = session_two().await;
    d2.aws.fail_gets(BASE, u32::MAX);
    let (_, resp) = d2.invoke().await;
    assert!(d2.prompts_received().is_empty(), "no model call: {resp:?}");
    let s = summary_of(&d2, 2);
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{}", s.label);
    let detail = s.detail.unwrap_or_default();
    assert_eq!(
        d2.aws.gets_of(&keys::repo_bundle_key(RUN)).len(),
        reads(),
        "{detail}"
    );
    assert!(detail.starts_with(SENTENCE), "{detail}");
    assert!(
        detail.contains("returned 500 InternalError: We encountered an internal error."),
        "{detail}"
    );
    assert!(
        detail.ends_with(&format!("(the read was tried {} times)", reads())),
        "{detail}"
    );
}

/// The base is not there: one GET, and the sentence of one read.
#[tokio::test(flavor = "multi_thread")]
async fn b978_a_base_that_is_not_there_is_asked_once() {
    let d2 = session_two().await;
    d2.aws.remove(&keys::repo_bundle_key(RUN));
    let (_, resp) = d2.invoke().await;
    assert!(d2.prompts_received().is_empty(), "no model call: {resp:?}");
    let s = summary_of(&d2, 2);
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{}", s.label);
    let detail = s.detail.unwrap_or_default();
    assert_eq!(
        d2.aws.gets_of(&keys::repo_bundle_key(RUN)).len(),
        1,
        "{detail}"
    );
    assert!(detail.starts_with(SENTENCE), "{detail}");
    assert!(detail.contains("returned 404"), "{detail}");
    assert!(!detail.contains("tried"), "{detail}");
}
