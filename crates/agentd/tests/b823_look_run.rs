//! (look for changes every 15 seconds), over a whole turn with the timer of the product: nothing of the cadence is set here. The
//! engine makes its tool calls and is then silent for 50 s, as during one long command: no tool call ends, so only the timer looks at the
//! tree. A file written during that time is read by a look that starts within the 15 s of the timer, and is in a stored copy while the
//! engine still works. The timer is judged by the time the copy's own record names (the second before its look read the tree), not by
//! the wall clock of the test: on a loaded build host the look, its store and the clone this test makes of the copy take seconds.
//! Measured on a live stage before the change, with the timer at one minute: one long command lost 50 s of file changes when its session
//! was stopped, because a session that is stopped saves nothing afterwards.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts, RUN, checkout_of_copy_bytes};
use harness_protocol::direct::s3 as keys;
use harness_protocol::runtime::TurnOutcome;
use std::time::{Duration, Instant};

/// How many copies the bucket holds (every version of `snapshot.json`; the fake store has a version at once).
fn stored(d: &Direct) -> usize {
    d.aws.versions_of(&keys::snapshot_json_key(RUN)).len()
}

/// The record of copy `n` (1 is the first).
fn record(d: &Direct, n: usize) -> serde_json::Value {
    let (_, record) = &d.aws.versions_of(&keys::snapshot_json_key(RUN))[n - 1];
    serde_json::from_slice(record).unwrap()
}

/// Whether copy `n` is the copy of the session's end.
fn is_final(d: &Direct, n: usize) -> bool {
    record(d, n).get("final").is_some()
}

/// The second in which the look of copy `n` began to read the tree: its record names the second before (`snapshot::cut_unix`).
fn read_at(d: &Direct, n: usize) -> i64 {
    agentd::time::parse_rfc3339(record(d, n)["at"].as_str().unwrap()).unwrap() + 1
}

/// The most seconds from a write to the second in which the look that copies it reads the tree: the timer, one second because both times
/// are whole seconds, and two for a tick that a loaded host delivers late.
const LOOK_WITHIN_S: i64 = harness_protocol::direct::DEFAULT_SNAPSHOT_EVERY_S as i64 + 3;

/// Whether copy `n` holds the file `name`: the bundle of that copy, checked out on its `snapshot` branch as a reader does.
fn holds(d: &Direct, n: usize, name: &str) -> bool {
    let (_, bundle) = &d.aws.versions_of(&keys::snapshot_bundle_key(RUN))[n - 1];
    checkout_of_copy_bytes(&d.aws, &d.dir, bundle, &format!("look-{n}-{name}"))
        .join(name)
        .is_file()
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_look_a_file_written_during_one_long_command_is_read_by_a_look_within_the_timer_and_stored()
 {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "50".into())],
        ..Opts::default()
    })
    .await;
    let watcher = async {
        let t = Instant::now();
        // The copy that the first tool call asked for.
        while stored(&d) == 0 {
            if t.elapsed() > Duration::from_secs(60) {
                return Err("no copy was stored for the tool calls".to_string());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut gaps = Vec::new();
        for name in ["long-1.txt", "long-2.txt"] {
            // Written right after a copy was stored, the worst moment: the next look is a whole timer away.
            let mut seen = stored(&d);
            std::fs::write(d.worktree().join(name), format!("{name}\n")).unwrap();
            let written = Instant::now();
            let written_unix = agentd::time::now_unix();
            loop {
                let n = stored(&d);
                if n > seen {
                    let at = written.elapsed();
                    seen = n;
                    if holds(&d, n, name) {
                        gaps.push((name, at, is_final(&d, n), read_at(&d, n) - written_unix));
                        break;
                    }
                }
                if written.elapsed() > Duration::from_secs(45) {
                    return Err(format!(
                        "no copy holds {name} 45 s after it was written; the bucket has {n} copies"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(gaps)
    };
    let ((_, resp), gaps) = tokio::join!(d.invoke(), watcher);
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let gaps =
        gaps.unwrap_or_else(|e| panic!("TRIGGER PRESENT: the engine was silent for 50 s: {e}"));
    assert_eq!(gaps.len(), 2);
    for (name, gap, of_the_end, look_after_s) in gaps {
        assert!(
            !of_the_end,
            "{name} was first stored by the copy of the end, {gap:?} after it was written: no look of the timer saved it while the \
             engine worked"
        );
        assert!(
            look_after_s <= LOOK_WITHIN_S,
            "the look that copied {name} read the tree {look_after_s} s after the file was written (its copy was stored after {gap:?}); \
             the timer is 15 s"
        );
    }
}
