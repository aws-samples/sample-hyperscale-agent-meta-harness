//! The flow a run declared: agentd reads the `Flow:` trailer of the final task commit's message, else the first line of
//! the engine's last message, and records `quick`, `full` or `none` in `summary.json`. The reading is a fact beside the label: no outcome,
//! label or check depends on it.
//!
//! The deliveries here are fakes: a real git repository with the commits a run would have left, and the in-process direct driver with the fake
//! ACP engine, whose wrapper can make a commit before it works (`WRAP_COMMIT_MESSAGE`).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::flow::of_run;
use agentd::workspace::{WsError, commit_message, head, prepare_local};
use common::{git, python_calc, scratch};
use direct_support::{Direct, Opts};
use harness_protocol::direct::{DirectFlow, DirectOutcome};

const TRAILER_FULL: &str = "Add percent_change\n\nThe function and its tests.\n\nTest: python3 -m unittest pkg.test_mod (3 tests)\nFlow: full";

fn commit(repo: &std::path::Path, file: &str, message: &str) -> String {
    std::fs::write(repo.join(file), format!("{message}\n")).unwrap();
    git(repo, &["add", file]);
    git(repo, &["commit", "-q", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

// ---- the commit message and the reading over a real repository --------------------------------------------------------------

#[tokio::test]
async fn the_message_of_a_commit_is_read_whole_and_a_name_that_is_no_commit_id_is_refused() {
    let d = scratch("b646-message");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run1", "a1", &src)
        .await
        .unwrap();
    let sha = commit(&wt.repo, "one.txt", TRAILER_FULL);
    let m = commit_message(&wt, &sha).await.unwrap();
    assert_eq!(m, TRAILER_FULL);
    for bad in [
        "",
        "HEAD",
        "main",
        "--output=x",
        "abc123",
        &format!("{sha};ls"),
    ] {
        assert!(
            matches!(commit_message(&wt, bad).await, Err(WsError::Git(_))),
            "{bad:?}"
        );
    }
    // A sha the repository does not have is an error, not an empty message.
    assert!(commit_message(&wt, &"0".repeat(40)).await.is_err());
}

#[tokio::test]
async fn the_trailer_of_the_final_task_commit_names_the_flow() {
    let d = scratch("b646-trailer");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run2", "a2", &src)
        .await
        .unwrap();
    let base = head(&wt).await.unwrap();
    commit(&wt.repo, "a.txt", "Start\n\nBody.\n\nFlow: quick");
    let last = commit(&wt.repo, "b.txt", TRAILER_FULL);
    // The newest task commit counts, not an earlier one.
    let flow = of_run(&wt, Some(&last), Some(&base), "Done. Flow: quick").await;
    assert_eq!(flow, DirectFlow::Full);
}

#[tokio::test]
async fn a_trailer_beats_the_last_message_and_the_last_message_is_the_fallback() {
    let d = scratch("b646-fallback");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run3", "a3", &src)
        .await
        .unwrap();
    let base = head(&wt).await.unwrap();
    let plain = commit(&wt.repo, "a.txt", "Add the function\n\nWhat and why.");
    assert_eq!(
        of_run(&wt, Some(&plain), Some(&base), "Flow: quick\n\nDone.").await,
        DirectFlow::Quick
    );
    assert_eq!(
        of_run(&wt, Some(&plain), Some(&base), "All done.").await,
        DirectFlow::Undeclared
    );
    // A line under a sentence of the same paragraph is no trailer, and the last message says nothing either.
    let sloppy = commit(
        &wt.repo,
        "b.txt",
        "Add the function\n\nIt is done.\nFlow: quick",
    );
    assert_eq!(
        of_run(&wt, Some(&sloppy), Some(&base), "All done.").await,
        DirectFlow::Undeclared
    );
}

#[tokio::test]
async fn the_commit_the_run_started_from_is_not_a_task_commit() {
    // A repository whose own history ends with a Flow line: the user's commit is not the run's declaration.
    let d = scratch("b646-base");
    let src = python_calc(&d);
    git(
        &src,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Release\n\nNotes.\n\nFlow: full",
        ],
    );
    let wt = prepare_local(&d.join("ws"), "run4", "a4", &src)
        .await
        .unwrap();
    let base = head(&wt).await.unwrap();
    assert_eq!(
        of_run(&wt, Some(&base), Some(&base), "All done.").await,
        DirectFlow::Undeclared
    );
    // Without a commit to read, the last message still speaks.
    assert_eq!(
        of_run(&wt, None, Some(&base), "Flow: quick\nDone.").await,
        DirectFlow::Quick
    );
    // A commit that cannot be read leaves the last message in charge.
    assert_eq!(
        of_run(&wt, Some(&"0".repeat(40)), Some(&base), "Flow: full").await,
        DirectFlow::Full
    );
}

#[tokio::test]
async fn a_very_long_message_is_read_from_its_end() {
    let d = scratch("b646-long");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run5", "a5", &src)
        .await
        .unwrap();
    let base = head(&wt).await.unwrap();
    // A message over the size of one argument, so it goes in through a file.
    let body = "x".repeat(300_000);
    std::fs::write(wt.repo.join("a.txt"), "a\n").unwrap();
    git(&wt.repo, &["add", "a.txt"]);
    let msg = d.join("long-message.txt");
    std::fs::write(&msg, format!("Big change\n\n{body}\n\nFlow: quick\n")).unwrap();
    git(&wt.repo, &["commit", "-q", "-F", msg.to_str().unwrap()]);
    let sha = git(&wt.repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        of_run(&wt, Some(&sha), Some(&base), "").await,
        DirectFlow::Quick
    );
}

// ---- the whole run ---------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_run_whose_engine_declared_nothing_records_none_and_the_label_is_the_one_it_always_was() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let s = d.summary();
    assert_eq!(s.flow, Some(DirectFlow::Undeclared), "{s:?}");
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.label, "Cline reported done");
    s.validate().unwrap();
    // The evidence the packager renders carries the same word.
    let e = s.to_evidence(harness_protocol::direct::HeadSource::Final);
    assert_eq!(e.flow, Some(DirectFlow::Undeclared));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trailer_of_the_commit_the_engine_made_is_the_flow_of_the_run() {
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_COMMIT_MESSAGE", TRAILER_FULL.to_string())],
        // The last message declares the other path; the trailer wins.
        scenario: "declares_flow",
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let s = d.summary();
    assert_eq!(s.flow, Some(DirectFlow::Full), "{s:?}");
    // TRIGGER PRESENT: the engine's commit is in the history, below the one the harness may have added.
    let log = git(&d.worktree(), &["log", "--format=%s"]);
    assert!(log.contains("Add percent_change"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_line_of_the_last_message_is_read_when_the_commit_has_no_trailer() {
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_COMMIT_MESSAGE", "Add the notes\n\nWhy.".to_string())],
        scenario: "declares_flow",
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.summary().flow, Some(DirectFlow::Quick));

    // No commit by the engine at all: the harness's own commit never counts, and the last message is read.
    let d = Direct::start(Opts {
        scenario: "declares_flow",
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.summary().flow, Some(DirectFlow::Quick));
}
