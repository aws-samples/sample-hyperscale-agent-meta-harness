//! In line mode: `ham watch` prints what a run says about its session's storage refusing a write, once for each notice, in the
//! client's own lines. Line mode has no conversation to show a notice in, so it prints the lines itself.
#![allow(clippy::unwrap_used)]

use ham::watch::LineMemo;
use ham_core::state::RunState;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn notice(n: u32, storage: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n,
        "ts": format!("2026-11-21T00:{:02}:07Z", 30 + n), "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": {"turn": 1, "kind": "notify", "reason": "storage_refused", "outageMinutes": null,
            "detail": "The session's storage refused a write.", "storage": storage}}
    }))
    .unwrap()
}

#[test]
fn b881_line_mode_prints_the_notice_lines_once() {
    let mut s = RunState::new(RUN);
    s.apply(notice(
        1,
        json!({"session": 1, "stretch": 1, "seen": "tool", "usedMb": 165, "partial": false, "quotaMb": 1024,
               "copyBefore": {"session": 1, "count": 2, "at": "2026-11-21T00:37:12Z",
                              "headSha": "29a235e2ea242b5e494b8dca989a4ea841132055", "versionId": "v1"}}),
    ));
    let mut memo = LineMemo::new();
    let first = memo.lines(&s.view(), 0);
    let expected = vec![
        "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).".to_string(),
        "On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up.".to_string(),
        "In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again.".to_string(),
        "A file that a command wrote meanwhile can be empty; ham does not know which.".to_string(),
        "Copy 2 (2026-11-21 00:37 UTC) is the last copy saved before ham first saw this. It is kept for 30 days after a newer copy replaced it:".to_string(),
        format!("  ham pull {RUN} --before-refusal {}", id(1)),
        format!("The newest copy holds the files as they are, empty ones included: ham pull {RUN} --snapshot"),
    ];
    assert_eq!(first, expected);
    // The same view again prints nothing; a later notice prints its own lines and not the first one's again.
    assert!(memo.lines(&s.view(), 1).is_empty());
    s.apply(notice(
        4,
        json!({"session": 1, "stretch": 2, "seen": "prompt", "partial": false, "quotaMb": 1024, "first": id(1),
               "copyBefore": {"session": 1, "count": 2, "at": "2026-11-21T00:37:12Z",
                              "headSha": "29a235e2ea242b5e494b8dca989a4ea841132055", "versionId": "v1"}}),
    ));
    assert_eq!(
        memo.lines(&s.view(), 2),
        vec![
            "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session.".to_string(),
            format!("The last copy saved before ham first saw it: ham pull {RUN} --before-refusal {}", id(1)),
            format!("The newest copy holds the files as they are, empty ones included: ham pull {RUN} --snapshot"),
        ]
    );
    assert!(memo.lines(&s.view(), 3).is_empty());
    // A watch that starts later is told of each notice the view still holds, once.
    let mut late = LineMemo::new();
    assert_eq!(late.lines(&s.view(), 0).len(), 10);
    assert!(late.lines(&s.view(), 1).is_empty());
}
