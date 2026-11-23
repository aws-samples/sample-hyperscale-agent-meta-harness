//! The JSON view of `ham watch` carries the notices that the session's storage refused a write as `storageNotices`. Line mode
//! prints the lines of each notice; a script that follows the JSON view learned of a refusal only from the card after the run.
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

fn notify(n: u32, reason: &str, detail: &str, storage: Option<Value>) -> Envelope {
    let mut value = json!({"turn": 1, "kind": "notify", "reason": reason, "outageMinutes": null, "detail": detail});
    if let Some(storage) = storage {
        value["storage"] = storage;
    }
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n,
        "ts": format!("2026-11-21T00:{:02}:07Z", 30 + n), "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn notice(n: u32, storage: Value) -> Envelope {
    notify(
        n,
        "storage_refused",
        "The session's storage refused a write.",
        Some(storage),
    )
}

fn copy_before() -> Value {
    json!({"session": 1, "count": 2, "at": "2026-11-21T00:37:12Z", "headSha": SHA, "versionId": "v1"})
}

/// A run of two sessions: the first notice of session 1 with a copy named, a later notice of that session, and the first notice of
/// session 2, which names no copy and whose size is a lower bound.
fn refused() -> RunState {
    let mut s = RunState::new(RUN);
    s.apply(notice(
        1,
        json!({"session": 1, "stretch": 1, "seen": "tool", "usedMb": 165, "partial": false, "quotaMb": 1024,
               "copyBefore": copy_before()}),
    ));
    s.apply(notice(
        4,
        json!({"session": 1, "stretch": 2, "seen": "prompt", "partial": false, "quotaMb": 1024, "first": id(1),
               "copyBefore": copy_before()}),
    ));
    s.apply(notice(
        6,
        json!({"session": 2, "stretch": 1, "seen": "start", "usedMb": 40, "partial": true, "quotaMb": 1024}),
    ));
    s
}

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[test]
fn b884_the_json_view_carries_the_storage_notices_and_only_a_view_that_has_them() {
    let state = refused();
    let with = view_json(&state.view());
    assert_eq!(
        with["storageNotices"],
        json!([
            {"at": "2026-11-21T00:31:07Z", "id": id(1), "session": 1, "stretch": 1,
             "text": "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).",
             "copyBefore": {"session": 1, "count": 2, "at": "2026-11-21T00:37:12Z"}},
            {"at": "2026-11-21T00:34:07Z", "id": id(4), "session": 1, "stretch": 2,
             "text": "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session.",
             "copyBefore": {"session": 1, "count": 2, "at": "2026-11-21T00:37:12Z"}},
            {"at": "2026-11-21T00:36:07Z", "id": id(6), "session": 2, "stretch": 1,
             "text": "The session's storage refused a write while its files add up to at least 40 MiB of 1024 MiB (no space left on device)."},
        ])
    );
    // Each entry's `text` is the first line line mode prints for the same notice.
    let lines = LineMemo::new().lines(&state.view(), 0);
    for entry in with["storageNotices"].as_array().unwrap() {
        let text = entry["text"].as_str().unwrap().to_string();
        assert!(lines.contains(&text), "{text}: {lines:?}");
    }
    // An entry holds its six members and no other: neither the commit nor the bucket's version of the copy is in the view.
    for entry in with["storageNotices"].as_array().unwrap() {
        let mut expected = vec!["at", "id", "session", "stretch", "text"];
        if let Some(copy) = entry.get("copyBefore") {
            expected.insert(1, "copyBefore");
            assert_eq!(keys(copy), vec!["at", "count", "session"], "{entry}");
        }
        assert_eq!(keys(entry), expected, "{entry}");
    }

    // The same run with notices of another kind in the place of the three: no member, and the view has the same members otherwise.
    let mut other = RunState::new(RUN);
    for n in [1, 4, 6] {
        other.apply(notify(
            n,
            "command_limit",
            "The run's command time limit is not confirmed for this session.",
            None,
        ));
    }
    let without = view_json(&other.view());
    assert!(without.get("storageNotices").is_none(), "{without}");
    let mut rest = keys(&with);
    rest.retain(|k| k != "storageNotices");
    assert_eq!(rest, keys(&without), "only the member is new");
    // A view of a run without any event has no member either.
    assert!(
        view_json(&RunState::new(RUN).view())
            .get("storageNotices")
            .is_none()
    );
}

#[test]
fn b884_the_member_holds_the_newest_notices_the_view_keeps_and_the_other_notices_keep_their_members()
 {
    // More notices than the view keeps the lines of: the member holds the newest ones, in the order they came.
    let kept = u32::try_from(ham_core::storage_notice::MAX_RECENT).unwrap();
    let mut many = RunState::new(RUN);
    many.apply(notice(
        1,
        json!({"session": 1, "stretch": 1, "seen": "tool", "usedMb": 165, "partial": false, "quotaMb": 1024}),
    ));
    for n in 2..=kept + 4 {
        many.apply(notice(
            n,
            json!({"session": 1, "stretch": n, "seen": "prompt", "partial": false, "quotaMb": 1024, "first": id(1)}),
        ));
    }
    let v = view_json(&many.view());
    let got = v["storageNotices"].as_array().unwrap();
    assert_eq!(got.len(), ham_core::storage_notice::MAX_RECENT);
    assert_eq!(got[0]["id"], id(5));
    assert_eq!(got[0]["stretch"], 5);
    assert_eq!(got[got.len() - 1]["id"], id(kept + 4));

    // A notice about a copy and a notice about a disk keep their own members beside it.
    let mut all = refused();
    all.apply(notify(
        7,
        "copy_not_saved",
        "A copy of the work is late; ham keeps trying.",
        None,
    ));
    all.apply(notify(
        8,
        "disk_low",
        "The workspace folder is nearly full: 920 MiB used against a stop mark of 960 MiB, over 90 percent of it. Nothing is deleted.",
        None,
    ));
    let v = view_json(&all.view());
    assert_eq!(v["copyNotices"].as_array().map(Vec::len), Some(1));
    assert_eq!(v["diskNotices"].as_array().map(Vec::len), Some(1));
    assert_eq!(v["storageNotices"].as_array().map(Vec::len), Some(3));
}

/// `stretch` counts the notices of one session, so a run of two sessions can have two entries with the same number. Each entry
/// carries the session of its notice as `session`; the session of the copy it names stays in `copyBefore`.
#[test]
fn b884_an_entry_carries_the_session_of_its_notice() {
    let view = view_json(&refused().view());
    let entries = view["storageNotices"].as_array().unwrap();
    let got: Vec<(Option<u64>, Option<u64>)> = entries
        .iter()
        .map(|e| (e["session"].as_u64(), e["stretch"].as_u64()))
        .collect();
    assert_eq!(
        got,
        vec![(Some(1), Some(1)), (Some(1), Some(2)), (Some(2), Some(1))],
        "{}",
        view["storageNotices"]
    );
    // The notice of session 2 names no copy: its session is the entry's own member, with no `copyBefore` beside it.
    assert!(entries[2].get("copyBefore").is_none(), "{}", entries[2]);
    assert_eq!(entries[0]["copyBefore"]["session"], 1);
}
