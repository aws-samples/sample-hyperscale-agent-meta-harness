//! With in `ham watch`: line mode prints what a run says about the disks of its session, once for each notice and in the
//! worker's own sentence, and the JSON view carries the same notices as `diskNotices`. Before this, neither showed any of them.
#![allow(clippy::unwrap_used)]

use ham::watch::{LineMemo, view_json};
use ham_core::state::RunState;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const W4: &str = "The workspace folder is nearly full: 920 MiB used against a stop mark of 960 MiB, over 90 percent of it. Nothing is deleted.";
const N1: &str = "The workspace folder holds 920 MiB. In measurements this storage at times refused writes from 300 MiB up and none up to 200 MiB. The run goes on.";
const N2: &str = "A new run with --compute instances, where the stage has such machines, keeps the workspace folder on the machine's own disk. That machine runs no containers.";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn notify(n: u32, reason: &str, detail: &str) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n,
        "ts": format!("2026-11-21T08:00:{:02}Z", n), "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": {"turn": 1, "kind": "notify", "reason": reason, "outageMinutes": null, "detail": detail}}
    }))
    .unwrap()
}

fn large_session() -> RunState {
    let mut s = RunState::new(RUN);
    s.apply(notify(4, "disk_low", W4));
    s.apply(notify(5, "disk_note", N1));
    s.apply(notify(6, "disk_note", N2));
    s
}

/// T13: each sentence once, in event order, and not again at the next read.
#[test]
fn b891_line_mode_prints_each_disk_notice_once_in_event_order() {
    let mut s = large_session();
    let mut memo = LineMemo::new();
    assert_eq!(
        memo.lines(&s.view(), 0),
        vec![W4.to_string(), N1.to_string(), N2.to_string()]
    );
    assert!(
        memo.lines(&s.view(), 1).is_empty(),
        "the same view again prints nothing"
    );
    // A later session of the run says its note again: its events are new, so its two rows are printed, and nothing older.
    s.apply(notify(40, "disk_note", N1));
    s.apply(notify(41, "disk_note", N2));
    assert_eq!(
        memo.lines(&s.view(), 2),
        vec![N1.to_string(), N2.to_string()]
    );
    assert!(memo.lines(&s.view(), 3).is_empty());
    // A watch that starts later is told each notice the view holds, once.
    let mut late = LineMemo::new();
    assert_eq!(late.lines(&s.view(), 0).len(), 5);
    assert!(late.lines(&s.view(), 1).is_empty());
    // A folder between the two marks: the two rows alone, with its own size.
    let mut small = RunState::new(RUN);
    let n1_250 = N1.replace("holds 920 MiB", "holds 250 MiB");
    small.apply(notify(4, "disk_note", &n1_250));
    small.apply(notify(5, "disk_note", N2));
    assert_eq!(
        LineMemo::new().lines(&small.view(), 0),
        vec![n1_250, N2.to_string()]
    );
    // A run that said nothing about its disks prints nothing for them.
    assert!(
        LineMemo::new()
            .lines(&RunState::new(RUN).view(), 0)
            .is_empty()
    );
}

/// T14: the JSON view carries the notices with their time, reason and sentence; a view without them has no such member and is
/// otherwise the same view.
#[test]
fn b891_the_json_view_carries_the_disk_notices_and_only_a_view_that_has_them() {
    let with = view_json(&large_session().view());
    assert_eq!(
        with["diskNotices"],
        json!([
            {"at": "2026-11-21T08:00:04Z", "reason": "disk_low", "text": W4},
            {"at": "2026-11-21T08:00:05Z", "reason": "disk_note", "text": N1},
            {"at": "2026-11-21T08:00:06Z", "reason": "disk_note", "text": N2},
        ])
    );
    // The same run with a notice of another kind in the place of the three: no member, and the rest of the view is the same.
    let mut other = RunState::new(RUN);
    other.apply(notify(
        4,
        "command_limit",
        "The run's command time limit is not confirmed for this session.",
    ));
    other.apply(notify(
        5,
        "command_limit",
        "The run's command time limit is not confirmed for this session.",
    ));
    other.apply(notify(
        6,
        "command_limit",
        "The run's command time limit is not confirmed for this session.",
    ));
    let without = view_json(&other.view());
    assert!(without.get("diskNotices").is_none(), "{without}");
    let mut rest: Value = with.clone();
    rest.as_object_mut().unwrap().remove("diskNotices");
    assert_eq!(rest, without, "only the member is new");
    // A view of a run without any event has no member either.
    assert!(
        view_json(&RunState::new(RUN).view())
            .get("diskNotices")
            .is_none()
    );
    // The copy notices keep their own member.
    let mut both = large_session();
    both.apply(notify(
        7,
        "copy_not_saved",
        "A copy of the work is late; ham keeps trying.",
    ));
    let v = view_json(&both.view());
    assert_eq!(v["copyNotices"].as_array().map(Vec::len), Some(1));
    assert_eq!(v["diskNotices"].as_array().map(Vec::len), Some(3));
}
