//! In the client: the lines of a storage notice that names the copy its session started from.
//!
//! A session after the first starts from a copy an earlier session saved. When its storage refuses a write before it stored a copy
//! of its own, the worker's notice names that copy in `storage.copyBefore`, with the session that saved it
//! (`crates/agentd/src/direct/storage_watch.rs`). The client tells it from a copy of the notice's own session by the session
//! number: a copy of an earlier session is the copy the session started from. It says first that the session had saved no copy, and
//! then names that copy and its command. The command, the id it takes and the `Storage` rows of the card are what they were.
#![allow(clippy::unwrap_used)]

use ham_core::notice::NOTICE_MAX_CHARS;
use ham_core::state::{RunState, RunView};
use ham_core::storage_notice::{self, REPLACED_COPY_DAYS};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const HEAD: &str = "29a235e2ea242b5e494b8dca989a4ea841132055";

/// ULIDs that sort by `n`.
fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, authority: &str, value: Value) -> Envelope {
    let agent = (authority == "agent").then_some("direct");
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": agent, "seq": n,
        "ts": format!("2026-11-21T01:{:02}:07Z", 30 + n), "type": ty, "authority": authority,
        "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn copy_before(session: u64, count: u64) -> Value {
    json!({"session": session, "count": count, "at": "2026-11-21T00:37:12Z", "headSha": HEAD, "versionId": "3sL4kqtJlcpXroDTDmJ+rmSpXd3dIbrHY+MTRCxf3vjVBH40Nr8X8gdRQBpUMLUo"})
}

/// The first notice of session `session`, as the worker writes it.
fn full(n: u32, session: u64, copy: Option<Value>) -> Envelope {
    let mut storage = json!({"session": session, "stretch": 1, "seen": "start", "usedMb": 165, "partial": false, "quotaMb": 1024});
    if let Some(c) = copy {
        storage["copyBefore"] = c;
    }
    event(
        n,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "storage_refused", "outageMinutes": null,
               "detail": "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).",
               "storage": storage}),
    )
}

/// A later notice of session `session`.
fn short(n: u32, session: u64, stretch: u64, first: u32, copy: Option<Value>) -> Envelope {
    let mut storage = json!({"session": session, "stretch": stretch, "seen": "prompt", "partial": false, "quotaMb": 1024, "first": id(first)});
    if let Some(c) = copy {
        storage["copyBefore"] = c;
    }
    event(
        n,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "storage_refused", "outageMinutes": null,
               "detail": "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session.",
               "storage": storage}),
    )
}

fn lines_of(e: &Envelope) -> Vec<String> {
    storage_notice::lines(&storage_notice::parse(e).unwrap(), RUN)
}

fn view_of(events: &[Envelope]) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e.clone());
    }
    s.view()
}

fn newest_line() -> String {
    format!(
        "The newest copy holds the files as they are, empty ones included: ham pull {RUN} --snapshot"
    )
}

#[test]
fn b906_the_client_names_the_copy_a_session_started_from_on_the_first_and_on_a_later_notice() {
    assert_eq!(REPLACED_COPY_DAYS, 30);
    // The first notice of session 2, which names copy 7 of session 1.
    let first = full(1, 2, Some(copy_before(1, 7)));
    assert_eq!(
        lines_of(&first),
        vec![
            "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).".to_string(),
            "On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up.".to_string(),
            "In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again.".to_string(),
            "A file that a command wrote meanwhile can be empty; ham does not know which.".to_string(),
            "This session had saved no copy before ham first saw this.".to_string(),
            "Copy 7 of session 1 (2026-11-21 00:37 UTC) is the copy this session started from.".to_string(),
            "It stays while the stage exists. If a newer copy of session 1 replaces it, it is kept for 30 days after that:".to_string(),
            format!("  ham pull {RUN} --before-refusal {}", id(1)),
            newest_line(),
        ]
    );
    // A later notice of the session: the count, the same copy with the command of the session's first notice, the newest copy.
    let later = short(5, 2, 2, 1, Some(copy_before(1, 7)));
    assert_eq!(
        lines_of(&later),
        vec![
            "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session.".to_string(),
            format!("The copy this session started from: ham pull {RUN} --before-refusal {}", id(1)),
            newest_line(),
        ]
    );
}

#[test]
fn b906_a_copy_of_the_notices_own_session_and_a_notice_without_a_copy_keep_their_lines() {
    // Session 2 stored copy 8 itself before the refusal: the lines are the ones a copy always had.
    let own = full(1, 2, Some(copy_before(2, 8)));
    assert_eq!(
        lines_of(&own)[4..],
        [
            "Copy 8 (2026-11-21 00:37 UTC) is the last copy saved before ham first saw this. It is kept for 30 days after a newer copy replaced it:".to_string(),
            format!("  ham pull {RUN} --before-refusal {}", id(1)),
            newest_line(),
        ]
    );
    let own_later = short(5, 2, 2, 1, Some(copy_before(2, 8)));
    assert_eq!(
        lines_of(&own_later)[1..],
        [
            format!(
                "The last copy saved before ham first saw it: ham pull {RUN} --before-refusal {}",
                id(1)
            ),
            newest_line(),
        ]
    );
    // A later session without a copy named, and a first session: one sentence, and no command for a copy from before.
    for session in [1, 2] {
        let none = full(1, session, None);
        assert_eq!(
            lines_of(&none)[4..],
            [
                "This session had saved no copy before ham first saw this.".to_string(),
                newest_line(),
            ],
            "session {session}"
        );
    }
    // A member that names a session after the notice's own is no copy the session started from: it is read as a copy before.
    let odd = full(1, 2, Some(copy_before(3, 9)));
    assert!(
        lines_of(&odd)[4].starts_with(
            "Copy 9 (2026-11-21 00:37 UTC) is the last copy saved before ham first saw this."
        ),
        "{:?}",
        lines_of(&odd)
    );
}

#[test]
fn b906_every_line_fits_the_notice_width_with_the_longest_values_and_no_command_is_cut() {
    let big = full(
        1,
        4_000_000_000,
        Some(copy_before(3_999_999_999, 4_000_000_000)),
    );
    let mut big_later = short(
        5,
        4_000_000_000,
        4_000_000_000,
        1,
        Some(copy_before(3_999_999_999, 4_000_000_000)),
    );
    if let harness_protocol::envelope::Payload::Inline { value } = &mut big_later.payload {
        value["storage"]["stretch"] = json!(4_000_000_000u64);
    }
    for e in [&big, &big_later] {
        let lines = lines_of(e);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("the copy this session started from")
                    || l.starts_with("The copy this session started from")),
            "TRIGGER PRESENT is not shown: the notice names no copy a session started from: {lines:?}"
        );
        for l in &lines {
            assert!(
                l.chars().count() <= NOTICE_MAX_CHARS,
                "{} characters: {l}",
                l.chars().count()
            );
            assert!(!l.contains('\u{2026}'), "cut: {l}");
        }
        // The two commands are whole, each on a line that ends with it.
        assert!(
            lines
                .iter()
                .any(|l| l.ends_with(&format!("ham pull {RUN} --before-refusal {}", id(1)))),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.ends_with(&format!("ham pull {RUN} --snapshot"))),
            "{lines:?}"
        );
    }
}

#[test]
fn b906_the_json_entry_and_the_card_carry_the_copy_the_session_started_from() {
    // The entry of the JSON view of `ham watch`: the session of the notice, the copy with the session that saved it, its number
    // and its time, and the id that `ham pull --before-refusal` takes. A script tells the copy a session started from by the two
    // sessions: the copy's is the earlier one.
    let first = full(1, 2, Some(copy_before(1, 7)));
    let parsed = storage_notice::parse(&first).unwrap();
    assert_eq!(parsed.session, 2);
    assert_eq!(
        storage_notice::json_entry(&parsed),
        json!({
            "at": "2026-11-21T01:31:07Z", "id": id(1), "session": 2, "stretch": 1,
            "text": "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).",
            "copyBefore": {"session": 1, "count": 7, "at": "2026-11-21T00:37:12Z"},
        })
    );
    // The card of the run when it has ended: the command of the copy from before, with the id of the run's first notice.
    let done = event(
        9,
        "run.completed",
        "harness",
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done", "detail": null}),
    );
    let view = view_of(&[first, done.clone()]);
    let rows = storage_notice::card_rows(&view.storage, RUN);
    assert!(
        rows.iter().any(|r| r.trim_start()
            == format!(
                "ham pull {RUN} --before-refusal {} downloads the last copy saved before ham first saw it.",
                id(1)
            )),
        "{rows:?}"
    );
    // How long that copy is kept: session 1 writes no newer copy at its key once it has ended, so the copy stays while the stage
    // exists. The row says that, and not the 30 days of a copy that its own session goes on to replace.
    assert!(
        rows.iter().any(|r| r.trim_start()
            == "It stays while the stage exists. If a newer copy of session 1 replaces it, it is kept for 30 days after that."),
        "{rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("after a newer copy replaced it")),
        "{rows:?}"
    );
    // The card of a run whose first notice names a copy of the notice's own session keeps the row it had.
    let own = full(1, 2, Some(copy_before(2, 8)));
    let rows = storage_notice::card_rows(&view_of(&[own, done]).storage, RUN);
    assert!(
        rows.iter()
            .any(|r| r.trim_start() == "It is kept 30 days after a newer copy replaced it."),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("while the stage exists")),
        "{rows:?}"
    );
}
