//! With: the numbers of a run's copies where three changes meet.
//!
//! * The invocation of every session carries the copy interval of the run's request (`snapshotEveryS`), here 15 minutes.
//! * A worker that receives a termination signal while the engine works saves a copy at once.
//! * The copies of a run are numbered within the run, and a session that the service starts for a lost one restores the last
//!   saved copy and goes on counting.
//!
//! One run: session 1 ends with four copies on record. Session 2, a real `agentd serve`, restores its work under the 15-minute timer,
//! saves the copies its tool calls ask for and one more when it is asked to end. Session 3 restores that last copy. No number of the
//! run is used twice, none starts again at one, and each session's record says where the next one goes on.
//!
//! A run has one bucket, and the three sessions are three stand-in buckets here. Each of them is given the bundle the run started
//! from: the copies of sessions 2 and 3 leave out what it holds, and session 3 takes it first to restore the copy of the stop.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{BUCKET, Direct, Opts, RUN, needs_the_base};
use harness_protocol::direct::{DirectSlice, s3 as keys, validate_direct_invocation};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{RepoSource, TurnOutcome};
use std::time::{Duration, Instant};

/// The copy interval of the run's request: `run.copy_every_minutes = 15`.
const EVERY_S: u32 = 900;
/// The number of the newest copy session 1 recorded.
const BEFORE: u64 = 4;

/// Runs session 1 to its end and returns what the service hands to the next session: the bundle, its head and the run's base.
/// The last member is the bundle the run started from. It is no hand-over: it is one object of the run's bucket, which every
/// session of the run reads there.
async fn first_session() -> (Vec<u8>, String, String, Vec<u8>) {
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
    let started_from = d1
        .aws
        .get(&keys::repo_bundle_key(RUN))
        .expect("the bundle the run started from");
    (bundle, head, base, started_from)
}

/// Every version of the `snapshot.json` of `session` in the bucket, oldest first.
fn records(d: &Direct, session: u32) -> Vec<serde_json::Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key_t(RUN, session))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

fn counts(records: &[serde_json::Value]) -> Vec<u64> {
    records
        .iter()
        .map(|r| r["count"].as_u64().expect("a record names its count"))
        .collect()
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

/// Each number is one more than the one before it.
fn counts_on(numbers: &[u64]) -> bool {
    numbers.windows(2).all(|w| w[1] == w[0] + 1)
}

#[tokio::test(flavor = "multi_thread")]
async fn b842_b823_b841_a_restarted_session_with_a_long_timer_and_a_stop_copy_goes_on_counting_the_copies()
 {
    let (bundle, head, base, started_from) = first_session().await;

    // Session 2 as the service starts it: a real worker process whose invocation names the interval of the run and the work to restore.
    let mut d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "40".into())],
        // The floor of a copy that a tool call asks for, short so the test does not wait 15 s; the timer is the invocation's own.
        agentd_env: vec![("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string())],
        ..Opts::default()
    })
    .await;
    // Every stand-in bucket is made with a repository of its own. Its commits carry fixed dates, so it is the repository
    // the run started from, whenever it is made; before, the copies of this session were whole or not as the clock read. The
    // object of the run is put here all the same: the bucket of a run holds that one object for all its sessions.
    d.aws.put(&keys::repo_bundle_key(RUN), started_from.clone());
    d.aws.put(&keys::bundle_key_t(RUN, 1), bundle);
    d.aws.put(
        &keys::snapshot_json_key_t(RUN, 1),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1, "headSha": head, "baseSha": base, "at": "2026-11-14T07:03:31Z", "count": BEFORE,
        }))
        .unwrap(),
    );
    d.req.turn = 2;
    d.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d.req.snapshot_every_s = Some(EVERY_S);
    d.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.clone()),
        restore_head: Some(head.clone()),
        sessions_before: Some(1),
        brief: None,
    });
    validate_direct_invocation(&d.req).expect("the invocation of session 2 is valid");
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");

    // The engine edits its files in two tool calls and is then silent: the copies those calls ask for. The timer is 15 minutes away.
    assert!(
        wait_for(Duration::from_secs(60), || !records(&d, 2).is_empty()).await,
        "TRIGGER PRESENT: session 2 saved no copy after the engine's first tool call; log:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let by_tool_calls = counts(&records(&d, 2));
    assert_eq!(
        by_tool_calls.first(),
        Some(&(BEFORE + 1)),
        "the first copy of session 2 is the next copy of the run: {by_tool_calls:?}"
    );

    // The worker is asked to end while the engine works, with work that is in no copy yet.
    std::fs::write(
        d.worktree().join("late.txt"),
        "written after the last copy and before the signal\n",
    )
    .unwrap();
    d.process.as_ref().unwrap().signal("TERM");
    let stored = wait_for(Duration::from_secs(20), || {
        records(&d, 2).len() > by_tool_calls.len()
    })
    .await;
    assert!(
        stored,
        "TRIGGER PRESENT: no copy after the termination signal; log:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    let of_session_2 = records(&d, 2);
    let numbers_2 = counts(&of_session_2);
    assert!(
        counts_on(&numbers_2),
        "the copies of session 2, the copy of the stop included, count on by one: {numbers_2:?}"
    );
    let stop = of_session_2.last().unwrap().clone();
    assert!(
        stop.get("final").is_none(),
        "the engine had not stopped: this is no copy of the end: {stop}"
    );
    // The process is not the first one of its machine here: it ends as the signal asks, after the copy.
    let began = Instant::now();
    let code = loop {
        if let Some(code) = d.process.as_mut().unwrap().ended() {
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
        d.process.as_ref().unwrap().log_text()
    );
    // What the run was told of session 2's copies names the same numbers, each once, and the session.
    let told = saved_copies(&d);
    let told_numbers: Vec<u64> = told.iter().map(|c| c["count"].as_u64().unwrap()).collect();
    assert!(
        told_numbers.iter().all(|n| numbers_2.contains(n)),
        "{told_numbers:?} of {numbers_2:?}"
    );
    assert!(counts_on(&told_numbers), "{told_numbers:?}");
    assert!(told.iter().all(|c| c["session"] == 2), "{told:?}");

    // The machine of session 2 is gone. The service starts session 3 from the last copy session 2 saved: the copy of the stop.
    let last_2 = *numbers_2.last().unwrap();
    let stop_head = stop["headSha"]
        .as_str()
        .expect("the record names its commit");
    let stop_bundle = d
        .aws
        .get(&keys::snapshot_bundle_key_t(RUN, 2))
        .expect("the bundle of the copy of the stop");
    assert!(
        needs_the_base(&stop_bundle),
        "TRIGGER PRESENT: the copy of the stop holds the whole history, so session 3 would restore it without the bundle the run started from; log:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    let mut d3 = Direct::start(Opts::default()).await;
    // The bucket of the same run: the bundle the run started from is there, and the restore of the copy takes it first.
    d3.aws.put(&keys::repo_bundle_key(RUN), started_from);
    d3.aws
        .put(&keys::snapshot_bundle_key_t(RUN, 2), stop_bundle);
    d3.aws.put(
        &keys::snapshot_json_key_t(RUN, 2),
        serde_json::to_vec(&stop).unwrap(),
    );
    d3.req.turn = 3;
    d3.req.runtime_session_id = format!("{RUN}-direct-g00000003");
    d3.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::snapshot_bundle_key_t(RUN, 2)),
    };
    d3.req.snapshot_every_s = Some(EVERY_S);
    d3.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base),
        restore_head: Some(stop_head.to_string()),
        sessions_before: Some(2),
        brief: None,
    });
    validate_direct_invocation(&d3.req).expect("the invocation of session 3 is valid");
    let (_, resp) = d3.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let numbers_3 = counts(&records(&d3, 3));
    assert_eq!(
        numbers_3.first(),
        Some(&(last_2 + 1)),
        "the first copy of session 3 follows the copy of the stop: {numbers_3:?} after {numbers_2:?}"
    );
    assert!(counts_on(&numbers_3), "{numbers_3:?}");
    // No number of the run names two copies.
    let mut all: Vec<u64> = numbers_2.iter().chain(numbers_3.iter()).copied().collect();
    let total = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), total, "{numbers_2:?} then {numbers_3:?}");
    assert!(all.iter().all(|n| *n > BEFORE), "{all:?}");
    // The summary of a session counts the copies of that session alone.
    let summary: harness_protocol::direct::DirectSummary =
        serde_json::from_slice(&d3.aws.get(&keys::summary_key_t(RUN, 3)).unwrap()).unwrap();
    assert_eq!(
        u64::from(summary.snapshots.count),
        numbers_3.last().unwrap() - last_2,
        "{:?}",
        summary.snapshots
    );
}
