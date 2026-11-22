//! The copy of a stop that holds the whole history is restored by the next session.
//!
//! A saved copy has two forms. The form of the release leaves out what the bundle the run started from holds, and a reader
//! takes that bundle first. A session with no floor writes copies that hold the whole history, and such a copy clones alone.
//! `b842_stop_copy_numbers` restores the copy of a stop in the first form. Here the worker that is asked to end writes whole copies
//! (`AGENTD_DIRECT_COPY_FORM=whole`), and the session that follows restores the copy of the stop from a store that does not hold
//! the run's starting repository: the copy needs nothing beside it, and the new session holds the work, the file written after
//! the last copy and before the signal included.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::snapshot::BundleForm;
use common::git;
use direct_support::{BUCKET, Direct, Opts, RUN, needs_the_base};
use harness_protocol::direct::{DirectSlice, s3 as keys, validate_direct_invocation};
use harness_protocol::runtime::{RepoSource, TurnOutcome};
use std::time::{Duration, Instant};

/// The file that is in no copy when the worker is asked to end.
const LATE: &str = "written after the last copy and before the signal\n";

/// Every version of the `snapshot.json` of session 1 in the store, oldest first.
fn records(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

/// Waits until `seen` holds, at most `limit`; `false` when it never did.
async fn wait_for(limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while !seen() {
        if began.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[tokio::test(flavor = "multi_thread")]
async fn b892_the_copy_of_a_stop_that_holds_the_whole_history_is_restored_by_the_next_session() {
    // Session 1: a real worker process whose engine edits two files and is then silent. Its copies hold the whole history.
    let mut d1 = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "40".into())],
        agentd_env: vec![
            ("AGENTD_DIRECT_COPY_FORM", "whole".to_string()),
            // The timer is an hour away: the copies are the ones the engine's tool calls ask for, and the one of the stop.
            ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "3600000".to_string()),
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
        ],
        ..Opts::default()
    })
    .await;
    let (code, text) = d1.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(60), || !records(&d1).is_empty()).await,
        "TRIGGER PRESENT: session 1 saved no copy after the engine's first tool call; log:\n{}",
        d1.process.as_ref().unwrap().log_text()
    );
    // The engine's edits are two tool calls: the copy the second one asks for is stored before the test goes on.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let before = records(&d1).len();

    // The worker is asked to end while the engine works, with work that is in no copy yet.
    std::fs::write(d1.worktree().join("late.txt"), LATE).unwrap();
    d1.process.as_ref().unwrap().signal("TERM");
    assert!(
        wait_for(Duration::from_secs(20), || records(&d1).len() > before).await,
        "TRIGGER PRESENT: no copy after the termination signal; log:\n{}",
        d1.process.as_ref().unwrap().log_text()
    );
    // The process is not the first one of its machine here: it ends as the signal asks, after the copy.
    let began = Instant::now();
    let code = loop {
        if let Some(code) = d1.process.as_mut().unwrap().ended() {
            break Some(code);
        }
        if began.elapsed() > Duration::from_secs(20) {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        code,
        Some(Some(143)),
        "log:\n{}",
        d1.process.as_ref().unwrap().log_text()
    );
    let stop = records(&d1).pop().unwrap();
    assert!(
        stop.get("final").is_none(),
        "the engine had not stopped: this is no copy of the end: {stop}"
    );
    let stop_head = stop["headSha"]
        .as_str()
        .expect("the record names its commit")
        .to_string();
    let base = stop["baseSha"]
        .as_str()
        .expect("the record names the base of the run")
        .to_string();
    let stop_bundle = d1
        .aws
        .get(&keys::snapshot_bundle_key(RUN))
        .expect("the bundle of the copy of the stop");
    assert!(
        !needs_the_base(&stop_bundle),
        "TRIGGER PRESENT: the copy of the stop leaves out what the run's base holds, and this test is about the copy that holds the whole history; log:\n{}",
        d1.process.as_ref().unwrap().log_text()
    );

    // The machine of session 1 is gone. The service starts session 2 from the last copy session 1 saved: the copy of the stop.
    // The store of the new session holds that copy and its record, and no starting repository.
    let mut d2 = Direct::start(Opts {
        edit_cfg: Some(Box::new(|cfg| {
            cfg.direct.copy_form = BundleForm::Whole;
        })),
        ..Opts::default()
    })
    .await;
    d2.aws.remove(&keys::repo_bundle_key(RUN));
    d2.aws
        .put(&keys::snapshot_bundle_key_t(RUN, 1), stop_bundle);
    d2.aws.put(
        &keys::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&stop).unwrap(),
    );
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::snapshot_bundle_key_t(RUN, 1)),
    };
    d2.req.snapshot_every_s = Some(0);
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base),
        restore_head: Some(stop_head.clone()),
        sessions_before: Some(1),
        brief: None,
    });
    validate_direct_invocation(&d2.req).expect("the invocation of session 2 is valid");
    let (_, resp) = d2.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d2.aws.gets_of(&keys::repo_bundle_key(RUN)),
        Vec::<Option<String>>::new(),
        "the restore of a copy that holds the whole history asks for nothing of the run's starting repository"
    );

    // The new session holds the work of the stopped one: its history stands on the commit of the copy, which holds the engine's
    // edits and the file that was in no earlier copy.
    let wt = d2.worktree();
    git(&wt, &["merge-base", "--is-ancestor", &stop_head, "HEAD"]);
    assert_eq!(
        git(&wt, &["show", &format!("{stop_head}:late.txt")]),
        LATE.trim_end(),
        "the commit of the copy holds the file written before the signal"
    );
    assert!(
        git(&wt, &["show", &format!("{stop_head}:src/calc.py")]).contains("percent_change"),
        "the commit of the copy holds the engine's edit"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("late.txt")).unwrap(),
        LATE,
        "the work tree of session 2 holds the file written before the signal"
    );
}
