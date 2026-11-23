//! In `ham watch`: a storage notice of a later session that names the copy the session started from. Line mode prints the
//! client's lines for it, and the member `storageNotices` of the JSON view carries the copy with the session that saved it.
#![allow(clippy::unwrap_used)]

use ham::watch::{LineMemo, view_json};
use ham_core::state::RunState;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const SHA: &str = "29a235e2ea242b5e494b8dca989a4ea841132055";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn notice(n: u32, storage: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n,
        "ts": format!("2026-11-21T01:{:02}:07Z", 30 + n), "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": {"turn": 1, "kind": "notify", "reason": "storage_refused", "outageMinutes": null,
            "detail": "The session's storage refused a write.", "storage": storage}}
    }))
    .unwrap()
}

/// Copy 7 of the run, which session 1 saved and session 2 started from.
fn started_from() -> Value {
    json!({"session": 1, "count": 7, "at": "2026-11-21T00:37:12Z", "headSha": SHA, "versionId": "v7"})
}

/// Session 2 of a run: its first notice, at the marker of its start, and a later one. Both name the copy the session started from.
fn refused() -> RunState {
    let mut s = RunState::new(RUN);
    s.apply(notice(
        1,
        json!({"session": 2, "stretch": 1, "seen": "start", "usedMb": 165, "partial": false, "quotaMb": 1024,
               "copyBefore": started_from()}),
    ));
    s.apply(notice(
        4,
        json!({"session": 2, "stretch": 2, "seen": "prompt", "partial": false, "quotaMb": 1024, "first": id(1),
               "copyBefore": started_from()}),
    ));
    s
}

#[test]
fn b906_line_mode_prints_the_copy_the_session_started_from_with_its_command() {
    let state = refused();
    let lines = LineMemo::new().lines(&state.view(), 0);
    assert_eq!(
        lines,
        vec![
            "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).".to_string(),
            "On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up.".to_string(),
            "In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again.".to_string(),
            "A file that a command wrote meanwhile can be empty; ham does not know which.".to_string(),
            "This session had saved no copy before ham first saw this.".to_string(),
            "Copy 7 of session 1 (2026-11-21 00:37 UTC) is the copy this session started from.".to_string(),
            "It stays while the stage exists. If a newer copy of session 1 replaces it, it is kept for 30 days after that:".to_string(),
            format!("  ham pull {RUN} --before-refusal {}", id(1)),
            format!("The newest copy holds the files as they are, empty ones included: ham pull {RUN} --snapshot"),
            "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session.".to_string(),
            format!("The copy this session started from: ham pull {RUN} --before-refusal {}", id(1)),
            format!("The newest copy holds the files as they are, empty ones included: ham pull {RUN} --snapshot"),
        ]
    );
}

#[test]
fn b906_the_json_view_carries_the_copy_the_session_started_from() {
    let state = refused();
    let view = view_json(&state.view());
    assert_eq!(
        view["storageNotices"],
        json!([
            {"at": "2026-11-21T01:31:07Z", "id": id(1), "session": 2, "stretch": 1,
             "text": "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).",
             "copyBefore": {"session": 1, "count": 7, "at": "2026-11-21T00:37:12Z"}},
            {"at": "2026-11-21T01:34:07Z", "id": id(4), "session": 2, "stretch": 2,
             "text": "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session.",
             "copyBefore": {"session": 1, "count": 7, "at": "2026-11-21T00:37:12Z"}},
        ])
    );
    // The id of the first entry is the one the command of line mode names.
    let lines = LineMemo::new().lines(&state.view(), 0);
    let first_id = view["storageNotices"][0]["id"].as_str().unwrap();
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with(&format!("ham pull {RUN} --before-refusal {first_id}"))),
        "{lines:?}"
    );
}
