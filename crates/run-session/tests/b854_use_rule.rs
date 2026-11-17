//! The use rule. `next-slice` starts the next session from the copy read back from a lost session's storage only when nothing
//! leaves a doubt; anything else keeps the run on the newest saved copy, and the copy read back stays in the bucket. One case per reason.
//! Lineage is never judged by a time stamp: the last case moves every time stamp and the decisions stay.
#![allow(clippy::unwrap_used)]

use run_session::recover::RecoveryResult;
use run_session::slices::{ReadNow, RecoveryDecision, use_recovery};
use serde_json::{Value, json};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const S: &str = "5555555555555555555555555555555555555555";
const R: &str = "6666666666666666666666666666666666666666";
const OTHER: &str = "4444444444444444444444444444444444444444";

fn key() -> String {
    format!("runs/{RUN}/agents/recovery/turns/1/recovery.bundle")
}

/// A read that stored a copy at `R` on top of the saved copy `S`, with nothing in doubt.
fn good() -> Value {
    json!({
        "schemaVersion": 1, "runId": RUN, "session": 1, "attemptId": "01M6B3S8K4D0YK5E9F7GR1XV2Q", "state": "copied", "cause": "no-heartbeat",
        "stop": {"startedAtMs": 1_000, "tookMs": 20, "stopped": true},
        "baseline": {"kind": "snapshot", "key": format!("runs/{RUN}/agents/direct/turns/1/snapshot.bundle"), "headSha": S, "count": 5},
        "copy": {"key": key(), "bundleSha256": "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=", "bundleBytes": 60, "headSha": R,
                 "treeSha": "7777777777777777777777777777777777777777", "parentSha": S,
                 "marker": {"schemaVersion": 1, "session": 1, "count": 5, "headSha": S},
                 "stable": true, "files": 12, "bytes": 3456},
        "comparison": {"state": "done", "sameAsBaseline": false, "regressed": 0, "rewritten": 0},
        "invoke": {"tries": 1}, "worker": {"uptimeS": 41, "agentdVersion": "0.1.0"},
        "startedAtMs": 1_000, "endedAtMs": 9_000
    })
}

fn now() -> ReadNow {
    ReadNow {
        key: Some(key()),
        object_head: Some(R.to_string()),
        baseline_head: Some(S.to_string()),
    }
}

fn decide(result: Value, now: &ReadNow) -> RecoveryDecision {
    let result: RecoveryResult = serde_json::from_value(result).unwrap();
    use_recovery(&result, now)
}

fn edited(pointer: &str, value: Value) -> Value {
    let mut v = good();
    *v.pointer_mut(pointer)
        .unwrap_or_else(|| panic!("{pointer}")) = value;
    v
}

fn removed(parent: &str, member: &str) -> Value {
    let mut v = good();
    v.pointer_mut(parent)
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove(member);
    v
}

/// Not used, for `reason`; the copy is still named.
fn refused(d: &RecoveryDecision, reason: Option<&str>) {
    assert!(!d.used, "{d:?}");
    assert_eq!(d.reason.as_deref(), reason, "{d:?}");
    assert_eq!(
        d.key.as_deref(),
        Some(key().as_str()),
        "a copy that is not used stays named: {d:?}"
    );
}

#[test]
fn b854_use_rule_the_complete_good_case_is_used() {
    let d = decide(good(), &now());
    assert_eq!(
        d,
        RecoveryDecision {
            tried: true,
            used: true,
            reason: None,
            key: Some(key()),
            head: Some(R.to_string()),
            newest_write_at: None,
            regressed: None,
            regressed_path: None,
        }
    );
    // A baseline whose record named no number: the marker's commit decides alone.
    assert!(decide(removed("/baseline", "count"), &now()).used);
}

#[test]
fn b854_use_rule_equal_trees_are_nothing_newer() {
    refused(
        &decide(edited("/comparison/sameAsBaseline", json!(true)), &now()),
        Some("nothing-newer"),
    );
}

#[test]
fn b854_use_rule_a_file_that_is_a_prefix_of_the_saved_copys_is_an_older_file() {
    let mut v = edited("/comparison/regressed", json!(2));
    v["comparison"]["regressedPaths"] = json!(["notes/log.txt", "b.txt"]);
    let d = decide(v, &now());
    refused(&d, Some("older-file"));
    assert_eq!(d.regressed, Some(2));
    assert_eq!(d.regressed_path.as_deref(), Some("notes/log.txt"));
}

#[test]
fn b854_use_rule_a_file_that_is_the_version_below_the_saved_copys_is_doubt() {
    // The storage holds the committed version of a file and the saved copy a later one. It is doubt and no proof; no word is settled for
    // it, so the decision carries none, and the saved copy wins.
    let mut v = edited("/comparison/rewritten", json!(1));
    v["comparison"]["rewrittenPaths"] = json!(["src/lib.rs"]);
    refused(&decide(v, &now()), None);
}

#[test]
fn b854_use_rule_a_comparison_that_was_not_made_is_doubt() {
    // Unknown, with the reason the worker gave; a state word of a later worker; no comparison at all. None reads as "nothing is older".
    refused(
        &decide(
            edited(
                "/comparison",
                json!({"state": "unknown", "reason": "baseline-unreadable"}),
            ),
            &now(),
        ),
        None,
    );
    refused(
        &decide(
            edited(
                "/comparison",
                json!({"state": "partly", "sameAsBaseline": false, "regressed": 0, "rewritten": 0}),
            ),
            &now(),
        ),
        None,
    );
    refused(&decide(removed("", "comparison"), &now()), None);
}

#[test]
fn b854_use_rule_a_marker_that_is_missing_or_names_another_copy_is_storage_older() {
    // No marker (missing, or one the worker could not trust).
    refused(
        &decide(removed("/copy", "marker"), &now()),
        Some("storage-older"),
    );
    // The marker names another commit than the saved copy's.
    refused(
        &decide(edited("/copy/marker/headSha", json!(OTHER)), &now()),
        Some("storage-older"),
    );
    // The marker names the commit and another number than the saved copy's record.
    refused(
        &decide(edited("/copy/marker/count", json!(4)), &now()),
        Some("storage-older"),
    );
}

#[test]
fn b854_use_rule_a_saved_copy_that_moved_since_the_read_is_superseded() {
    let moved = ReadNow {
        baseline_head: Some(OTHER.to_string()),
        ..now()
    };
    refused(&decide(good(), &moved), Some("superseded"));
    // It wins over every other doubt the read found: the worker compared with the copy before it.
    refused(
        &decide(removed("/copy", "marker"), &moved),
        Some("superseded"),
    );
    refused(
        &decide(edited("/comparison/regressed", json!(3)), &moved),
        Some("superseded"),
    );
    // A baseline that cannot be read now is no baseline that stood still.
    let gone = ReadNow {
        baseline_head: None,
        ..now()
    };
    refused(&decide(good(), &gone), Some("superseded"));
}

#[test]
fn b854_use_rule_a_tree_that_was_still_changing_is_unstable() {
    refused(
        &decide(edited("/copy/stable", json!(false)), &now()),
        Some("unstable"),
    );
}

#[test]
fn b854_use_rule_a_copy_without_a_parent_is_kept_and_not_started_from() {
    // The history could not be read (the engine removed `.git`): the whole tree is stored without a parent. No word is settled for it.
    let d = decide(removed("/copy", "parentSha"), &now());
    refused(&d, None);
    assert_eq!(d.head.as_deref(), Some(R));
}

#[test]
fn b854_use_rule_an_object_that_is_not_whole_is_not_used() {
    // The object's first bytes name another commit now, or none.
    for object_head in [Some(OTHER.to_string()), None] {
        let d = decide(
            good(),
            &ReadNow {
                object_head,
                ..now()
            },
        );
        refused(&d, Some("not-whole"));
    }
    // The object is not in the bucket: nothing to name, nothing to use.
    let d = decide(
        good(),
        &ReadNow {
            key: None,
            object_head: None,
            ..now()
        },
    );
    assert!(!d.used);
    assert_eq!(d.reason.as_deref(), Some("not-whole"));
    assert_eq!(d.key, None);
    // A result that says copied and holds no copy.
    let d = decide(removed("", "copy"), &now());
    assert!(!d.used);
    assert_eq!(d.reason.as_deref(), Some("not-whole"));
}

#[test]
fn b854_use_rule_missed_skipped_and_abandoned_results_are_never_used() {
    for (state, reason, tried) in [
        ("missed", Some("worker-refused"), true),
        ("missed", Some("timeout"), true),
        ("missed", Some("session-active"), true),
        ("missed", Some("stop-failed"), true),
        ("abandoned", Some("abandoned"), true),
        ("skipped", Some("off"), false),
        ("skipped", Some("instances"), false),
        ("skipped", Some("run-ended"), false),
    ] {
        // Even with every fact of a good copy beside it, a result that is not `copied` is not used.
        let mut v = good();
        v["state"] = json!(state);
        v["reason"] = json!(reason);
        if !tried {
            v.as_object_mut().unwrap().remove("attemptId");
        }
        let d = decide(v, &now());
        assert!(!d.used, "{state} {reason:?}");
        assert_eq!(d.reason.as_deref(), reason);
        assert_eq!(d.tried, tried, "{state}");
    }
    // An abandoned result without a word of its own is told as abandoned.
    let mut v = good();
    v["state"] = json!("abandoned");
    let d = decide(v, &now());
    assert!(!d.used);
    assert_eq!(d.reason.as_deref(), Some("abandoned"));
    // The copy an abandoned read left in the bucket is still named.
    assert_eq!(d.key.as_deref(), Some(key().as_str()));
}

#[test]
fn b854_use_rule_no_time_stamp_decides() {
    // The newest write of the tree is shown and never judged: a copy with a far older or far newer time is used all the same, and a
    // refused copy is not rescued by a new time.
    for at in ["2001-01-01T00:00:00Z", "2099-12-31T23:59:59Z"] {
        let mut v = good();
        v["copy"]["newestWriteAt"] = json!(at);
        v["startedAtMs"] = json!(1);
        v["endedAtMs"] = json!(9_999_999_999_999u64);
        let d = decide(v.clone(), &now());
        assert!(d.used, "{at}");
        assert_eq!(d.newest_write_at.as_deref(), Some(at));
        v["copy"]["marker"]["headSha"] = json!(OTHER);
        assert!(!decide(v, &now()).used, "{at}");
    }
}
