//! The copies of the work over whole turns, in process (the fake AWS and the fake engine of `direct_support`). However a turn ends,
//! the snapshot pair in the bucket is the end state of the work, saved before the session's own bundle and before anything can refuse a
//! delivery; while the engine works, a tool call that changed the tree is copied without waiting for the timer.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use common::git;
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::TurnOutcome;
use std::path::PathBuf;
use std::time::Duration;

/// `snapshot.json` as the bucket holds it now.
fn record(d: &Direct) -> Option<serde_json::Value> {
    d.aws
        .get(&keys::snapshot_json_key(RUN))
        .map(|b| serde_json::from_slice(&b).unwrap())
}

/// A clone of the snapshot bundle in the bucket, on its `snapshot` branch (the command the client prints).
fn clone_of_snapshot(d: &Direct, name: &str) -> PathBuf {
    let bundle = d.dir.join(format!("{name}.bundle"));
    std::fs::write(
        &bundle,
        d.aws
            .get(&keys::snapshot_bundle_key(RUN))
            .expect("snapshot.bundle"),
    )
    .unwrap();
    let clone = d.dir.join(name);
    git(
        &d.dir,
        &[
            "clone",
            "-q",
            "-b",
            "snapshot",
            bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    clone
}

/// The `savedCopy` members of the run's progress events, in order.
fn saved_copies(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .events()
        .into_iter()
        .filter(|(t, _)| *t == EventType::TurnProgress)
        .filter_map(|(_, v)| v.get("savedCopy").cloned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_when_the_engine_stops_the_snapshot_is_the_final_head() {
    // The copies while the engine works are off, so the one copy is the copy of the end: such a run had no snapshot.
    let d = Direct::start(Opts {
        snapshot_every_s: Some(0),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let head = resp.last_commit_sha.clone().unwrap();
    let s = d.summary();

    let r = record(&d).expect("the end of the turn saved a copy");
    assert_eq!(r["schemaVersion"], 1);
    assert_eq!(r["final"], true, "{r}");
    assert_eq!(r["headSha"], head.as_str(), "the copy is the final head");
    assert_eq!(r["baseSha"], s.git.base_sha.clone().unwrap().as_str());
    assert_eq!(r["count"], 1);
    let clone = clone_of_snapshot(&d, "final");
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert!(
        std::fs::read_to_string(clone.join("src/calc.py"))
            .unwrap()
            .contains("percent_change")
    );
    // It is the commit the session's own bundle carries.
    assert_eq!(s.git.head_sha.as_deref(), Some(head.as_str()));
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());

    // The summary counts it and the run was told once, after both objects were stored.
    assert_eq!(s.snapshots.count, 1, "{:?}", s.snapshots);
    assert_eq!(s.snapshots.last_at.as_deref(), r["at"].as_str());
    assert_eq!(s.snapshots.failures, 0);
    let copies = saved_copies(&d);
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0]["final"], true);
    assert_eq!(copies[0]["headSha"], head.as_str());
    assert_eq!(copies[0]["count"], 1);
    assert_eq!(copies[0]["at"], r["at"]);
    assert_eq!(copies[0]["session"], 1);
    // The event has no kind, no text and no tool call id: a client that does not know the member shows nothing for it.
    for (t, v) in d.aws.events() {
        if t == EventType::TurnProgress && v.get("savedCopy").is_some() {
            assert!(
                v.get("kind").is_none() && v.get("text").is_none() && v.get("toolCallId").is_none(),
                "{v}"
            );
        }
    }
    // The engine's repository has no snapshot ref.
    assert!(git(&d.worktree(), &["branch", "--list", "snapshot"]).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_session_bundle_that_cannot_upload_still_leaves_the_final_copy() {
    let d = Direct::start(Opts::default()).await;
    // The delivery's input cannot be stored: the copy of the work does not depend on it. (The timer is the default, 60 s.)
    d.aws.fail_puts("turns/1/bundle", u32::MAX);
    let (_, resp) = d.invoke().await;
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_none(),
        "TRIGGER PRESENT: the session's bundle is not in the bucket"
    );
    assert!(resp.bundle_uri.is_none());
    let r = record(&d).expect("the final copy");
    assert_eq!(r["final"], true);
    let clone = clone_of_snapshot(&d, "final");
    assert!(
        std::fs::read_to_string(clone.join("src/calc.py"))
            .unwrap()
            .contains("percent_change")
    );
    // The copy holds the engine's edits as one commit on the base.
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        r["headSha"].as_str().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_run_the_user_stopped_keeps_its_final_copy() {
    let mut opts = Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        ..Opts::default()
    };
    opts.timeout_s = 60;
    let d = Direct::start(opts).await;
    d.aws.cancel_on(EventType::TurnStarted);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Cancelled);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedByUser);
    let r = record(&d).expect("a stop by the user saves the end state");
    assert_eq!(r["final"], true);
    assert_eq!(r["headSha"].as_str(), s.git.head_sha.as_deref());
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_an_engine_that_died_keeps_its_final_copy() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_DIE_ON_PROMPT", "3".into())],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedEngineDied);
    let r = record(&d).expect("an engine that died leaves the end state");
    assert_eq!(r["final"], true);
    assert_eq!(r["headSha"].as_str(), s.git.head_sha.as_deref());
    // The last copy the run was told of is the copy of the end; an earlier one (after a tool call) is not final.
    let copies = saved_copies(&d);
    assert_eq!(copies.last().unwrap()["final"], true);
    assert_eq!(copies.last().unwrap()["count"], r["count"]);
    assert!(
        copies[..copies.len() - 1]
            .iter()
            .all(|c| c.get("final").is_none()),
        "{copies:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_with_the_copies_switched_off_the_end_state_is_still_saved() {
    let d = Direct::start(Opts {
        snapshot_every_s: Some(0),
        engine_env: vec![("FAKE_ACP_SILENCE_S", "2".into())],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    let copies = saved_copies(&d);
    assert_eq!(
        copies.len(),
        1,
        "no copy while the engine worked, one at the end: {copies:?}"
    );
    assert_eq!(copies[0]["final"], true);
    assert_eq!(
        record(&d).unwrap()["headSha"].as_str(),
        resp.last_commit_sha.as_deref()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_tool_call_that_changed_the_tree_is_copied_without_waiting_for_the_timer() {
    // The timer is an hour away; the engine edits two files and then stays silent for four seconds before it ends its turn.
    let d = Direct::start(Opts {
        scenario: "writes_files",
        snapshot_every_s: Some(3_600),
        engine_env: vec![("FAKE_ACP_SILENCE_S", "4".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_floor = Some(Duration::from_millis(50))
        })),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let copies = saved_copies(&d);
    assert!(
        copies.len() >= 2,
        "a copy after the tool calls and the copy of the end: {copies:?}"
    );
    let first = &copies[0];
    assert!(
        first.get("final").is_none(),
        "the first copy was made while the engine worked: {first}"
    );
    assert_eq!(first["count"], 1);
    let last = copies.last().unwrap();
    assert_eq!(last["final"], true);
    assert_eq!(last["headSha"].as_str(), resp.last_commit_sha.as_deref());
    // The first version of the record in the bucket is the copy made while the engine worked: it holds the edits already.
    let versions = d.aws.versions_of(&keys::snapshot_json_key(RUN));
    assert!(versions.len() >= 2, "{}", versions.len());
    let v1: serde_json::Value = serde_json::from_slice(&versions[0].1).unwrap();
    assert!(v1.get("final").is_none(), "{v1}");
    assert_eq!(v1["headSha"], first["headSha"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_copy_that_cannot_be_saved_is_said_once_and_named_in_the_turns_error() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "3".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(400))
        })),
        ..Opts::default()
    })
    .await;
    d.aws.fail_puts("snapshot.bundle", u32::MAX);
    let (_, resp) = d.invoke().await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "a copy that fails never ends the turn"
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert!(
        s.snapshots.failures >= 2,
        "the copies while it worked and the copy of the end: {:?}",
        s.snapshots
    );
    assert_eq!(s.snapshots.count, 0);
    assert!(
        saved_copies(&d).is_empty(),
        "no stored copy, so the run is told of none"
    );
    // The run is told once, not once per failure.
    let said: Vec<serde_json::Value> = d
        .aws
        .progress("notify")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["reason"] == "copy_not_saved")
        .collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(
        said[0]["detail"],
        "A copy of the work could not be saved; ham keeps trying."
    );
    // The end of the turn names the copy it could not store.
    let error = s.error.clone().unwrap_or_default();
    assert!(error.contains("final copy:"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_run_that_never_had_a_checkout_saves_no_copy() {
    // The repository bundle is missing: the run fails before the engine starts and before there is any work tree.
    let d = Direct::start(Opts {
        no_bundle: true,
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedSetupFailed);
    assert!(record(&d).is_none());
    assert!(d.aws.get(&keys::snapshot_bundle_key(RUN)).is_none());
    assert!(saved_copies(&d).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_session_that_cannot_set_up_its_copies_says_so_and_names_it_at_the_end() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        ..Opts::default()
    })
    .await;
    // The place of the private repository is taken by a file, so the repository cannot be made.
    let agent_data = d.app.cfg.data_root.join(RUN).join("direct");
    std::fs::create_dir_all(&agent_data).unwrap();
    std::fs::write(agent_data.join("snapshot.git"), b"in the way").unwrap();
    let (_, resp) = d.invoke().await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "a session without copies still ends its turn"
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert!(
        record(&d).is_none(),
        "TRIGGER PRESENT: no copy could be made"
    );
    assert!(s.snapshots.failures >= 1, "{:?}", s.snapshots);
    // The run is told once that this session saves no copies.
    let said: Vec<serde_json::Value> = d
        .aws
        .progress("notify")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["reason"] == "copy_not_saved")
        .collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(
        said[0]["detail"],
        "Copies of the work cannot be saved in this session. The work is kept only if the run delivers it."
    );
    // The end of the turn names the copy it could not make.
    let error = s.error.clone().unwrap_or_default();
    assert!(error.contains("final copy:"), "{error}");
    // The delivery's own input is there all the same.
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_copy_of_the_end_that_takes_too_long_gives_way_to_the_delivery() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        snapshot_every_s: Some(0),
        edit_cfg: Some(Box::new(|c| {
            c.direct.final_copy_budget = Duration::from_millis(500)
        })),
        ..Opts::default()
    })
    .await;
    // The store takes 20 s to answer the upload of the copy; the session's own bundle goes up at once.
    d.aws.delay_puts("snapshot.bundle", Duration::from_secs(20));
    let began = std::time::Instant::now();
    let (_, resp) = d.invoke().await;
    let took = began.elapsed();
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_some(),
        "the bundle that delivery reads is stored"
    );
    assert!(
        took < Duration::from_secs(20),
        "the end of the turn did not wait for the copy: {took:?}"
    );
    let error = s.error.clone().unwrap_or_default();
    assert!(
        error.contains("final copy: not confirmed within"),
        "{error}"
    );
    assert!(saved_copies(&d).is_empty(), "no stored copy is announced");
    // Nothing tries again after the end, so the run is not told that ham keeps trying.
    let said: Vec<serde_json::Value> = d
        .aws
        .progress("notify")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["reason"] == "copy_not_saved")
        .collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(
        said[0]["detail"],
        "The last copy of the work could not be saved."
    );
}

/// The engine replaces `.git` by a file that names a directory of its own. agentd makes no end commit and no session bundle there
/// (`a_repository_the_engine_redirected_gets_no_commit_and_no_bundle_from_agentd`), so the copy of the end is what keeps the work: the tree as
/// it is, in a commit that holds the whole tree, made without the engine's settings.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_checkout_whose_git_directory_moved_still_gets_a_copy_of_its_end() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("WRAP_GIT_REDIRECT", "1".into())],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert!(
        std::fs::symlink_metadata(d.worktree().join(".git"))
            .unwrap()
            .file_type()
            .is_file(),
        "TRIGGER: `.git` is a file"
    );
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_none(),
        "no session bundle: {resp:?}"
    );
    let s = d.summary();
    let r = record(&d).unwrap_or_else(|| {
        panic!(
            "TRIGGER PRESENT: no copy of the end of the work the engine left; error {:?}",
            s.error
        )
    });
    assert_eq!(r["final"], true, "{r}");
    let clone = clone_of_snapshot(&d, "clone");
    let left = std::fs::read_to_string(d.worktree().join("src/calc.py")).unwrap();
    assert_eq!(
        std::fs::read_to_string(clone.join("src/calc.py")).unwrap(),
        left,
        "the copy holds the file as the engine left it"
    );
    assert!(
        !d.agent_data().join("home/filter-used").exists(),
        "agentd's git ran the engine's clean filter"
    );
    assert!(
        !s.error.clone().unwrap_or_default().contains("final copy:"),
        "{:?}",
        s.error
    );
    assert_eq!(saved_copies(&d).last().unwrap()["final"], true);
}

/// The `copy_not_saved` notices of the run, in order.
fn copy_notices(d: &Direct) -> Vec<String> {
    d.aws
        .progress("notify")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["reason"] == "copy_not_saved")
        .map(|v| v["detail"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A look whose uploads failed does not hold the next look back: the next tick of the timer tries again, as the run was told. Before, the
/// copy task waited nine times the whole look, the failed uploads included (4,446 s after a look of 494 s in the product).
#[tokio::test(flavor = "multi_thread")]
async fn b823_after_a_copy_that_failed_the_next_look_comes_at_the_next_tick() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "10".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
            c.direct.upload_retry_pause = Duration::from_millis(500);
        })),
        ..Opts::default()
    })
    .await;
    // The four tries of the first copy's bundle answer 500 (about 3.5 s with the pauses of 0.5, 1 and 2 s); the store takes every PUT after.
    d.aws.fail_puts("snapshot.bundle", 4);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let copies = saved_copies(&d);
    assert!(
        copies.iter().any(|c| c.get("final").is_none()),
        "TRIGGER PRESENT: after the failed copy no copy was made while the engine worked on for about 6 s with a 300 ms timer: \
         {copies:?}; notices {:?}",
        copy_notices(&d)
    );
    assert_eq!(
        copy_notices(&d),
        vec!["A copy of the work could not be saved; ham keeps trying.".to_string()]
    );
}

/// The time an upload takes is not counted in the spacing: a copy whose bundle took 3 s to go up is followed by the next look at the next
/// tick. Before, the next look waited nine times the whole look (27 s).
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_slow_upload_does_not_space_the_next_copy() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "8".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            // The timer is shorter than the floor of a look a tool call asks for (15 s): the floor is set below the timer, as in production.
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
        })),
        ..Opts::default()
    })
    .await;
    // The first upload of a copy's bundle is answered after 3 s; every one after it at once.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(3), 1);
    // The records in the bucket, every version: the fake store has them at once (the run's events reach the plane in batches).
    let records = |d: &Direct| -> Vec<serde_json::Value> {
        d.aws
            .versions_of(&keys::snapshot_json_key(RUN))
            .iter()
            .map(|(_, b)| serde_json::from_slice(b).unwrap())
            .collect()
    };
    let watcher = async {
        let t = std::time::Instant::now();
        while records(&d).is_empty() {
            if t.elapsed() > Duration::from_secs(60) {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let first = std::time::Instant::now();
        std::fs::write(
            d.worktree().join("late.txt"),
            "written after the first copy\n",
        )
        .unwrap();
        loop {
            let copies = records(&d);
            if copies.len() >= 2 {
                return copies[1].get("final").is_none().then(|| first.elapsed());
            }
            if t.elapsed() > Duration::from_secs(60) {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let ((_, resp), gap) = tokio::join!(d.invoke(), watcher);
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let gap = gap.unwrap_or_else(|| {
        panic!(
            "TRIGGER PRESENT: no copy followed the slow upload while the engine worked: {:?}",
            saved_copies(&d)
        )
    });
    assert!(
        gap < Duration::from_secs(4),
        "the next copy came {gap:?} after the slow one"
    );
}

/// One request of the copy of the end that gets no answer leaves time for its retry. Before, the request limit (120 s in the product) was as
/// long as the whole budget of the copy, so no retry ran and the bucket kept an older copy than the work the session's bundle holds.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_copy_of_the_end_whose_first_request_hangs_is_stored_by_its_retry() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "4".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.final_copy_budget = Duration::from_secs(2);
            c.direct.upload_retry_pause = Duration::from_millis(100);
        })),
        ..Opts::default()
    })
    .await;
    let watcher = async {
        let t = std::time::Instant::now();
        // The record in the bucket: the fake store has it at once (the run's events reach the plane in batches).
        while record(&d).is_none() {
            assert!(t.elapsed() < Duration::from_secs(60), "no first copy");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // From now on the next PUT of a copy's bundle gets no answer for 30 s, and the engine's work goes on.
        d.aws
            .delay_next_puts("snapshot.bundle", Duration::from_secs(30), 1);
        std::fs::write(
            d.worktree().join("late.txt"),
            "written after the first copy\n",
        )
        .unwrap();
    };
    let ((_, resp), ()) = tokio::join!(d.invoke(), watcher);
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    let head = resp.last_commit_sha.clone().unwrap();
    let r = record(&d).expect("a copy");
    assert_eq!(
        r["headSha"].as_str(),
        Some(head.as_str()),
        "TRIGGER PRESENT: the copy in the bucket is an older one ({r}) than the final head; error {:?}",
        s.error
    );
    assert_eq!(r["final"], true, "{r}");
    let clone = clone_of_snapshot(&d, "clone");
    assert!(clone.join("late.txt").is_file());
    assert!(
        !s.error.clone().unwrap_or_default().contains("final copy:"),
        "{:?}",
        s.error
    );
    assert!(copy_notices(&d).is_empty(), "{:?}", copy_notices(&d));
}

/// A copy of the end whose four tries failed is tried once more after the session's own bundle is stored. Before, the bucket kept no copy
/// of the end at all, and the turn's error named it.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_copy_of_the_end_that_failed_is_tried_again_once_the_sessions_bundle_is_stored() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        snapshot_every_s: Some(0),
        edit_cfg: Some(Box::new(|c| {
            c.direct.upload_retry_pause = Duration::from_millis(100);
        })),
        ..Opts::default()
    })
    .await;
    d.aws.fail_puts("snapshot.bundle", 4);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_some(),
        "the session's bundle"
    );
    let r = record(&d).unwrap_or_else(|| {
        panic!(
            "TRIGGER PRESENT: no copy of the end after its first try failed; error {:?}",
            s.error
        )
    });
    assert_eq!(r["final"], true, "{r}");
    assert_eq!(r["headSha"].as_str(), resp.last_commit_sha.as_deref());
    assert_eq!(d.aws.put_attempts(&keys::snapshot_bundle_key(RUN)), 5);
    assert!(
        !s.error.clone().unwrap_or_default().contains("final copy:"),
        "{:?}",
        s.error
    );
    assert!(
        copy_notices(&d).is_empty(),
        "nothing failed in the end: {:?}",
        copy_notices(&d)
    );
}
