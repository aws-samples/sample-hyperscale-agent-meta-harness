//! With: the client shows what a run says about the disks of its session in the worker's own sentence.
//!
//! The worker's "nearly full" notice (`disk_low`) was stored by the service and shown only on the run screen, with the reason word in
//! front of it; `ham watch` and its JSON view carried none of it. The note about a large workspace folder (`disk_note`, two rows)
//! travels the same way. These tests pin the notice line, the run view's list, the run screen's block and the card.
#![allow(clippy::unwrap_used)]

use ham_core::chat::{ChatInputs, ChatModel, FinalMessage};
use ham_core::notice::{self, NOTICE_MAX_CHARS, NoticeKind, notice_of};
use ham_core::render::{FinalCard, final_card};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const W4: &str = "The workspace folder is nearly full: 920 MiB used against a stop mark of 960 MiB, over 90 percent of it. Nothing is deleted.";
const ROOT: &str = "The session's root volume is running low: 400 MiB free, below the 512 MiB mark. Nothing is deleted.";
const N1: &str = "The workspace folder holds 920 MiB. In measurements this storage at times refused writes from 300 MiB up and none up to 200 MiB. The run goes on.";
const N2: &str = "A new run with --compute instances, where the stage has such machines, keeps the workspace folder on the machine's own disk. That machine runs no containers.";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, authority: &str, value: Value) -> Envelope {
    let agent = (authority == "agent").then_some("direct");
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": agent, "seq": n,
        "ts": format!("2026-11-21T08:{:02}:08Z", n % 60), "type": ty, "authority": authority,
        "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn notify(n: u32, reason: &str, detail: Option<&str>) -> Envelope {
    event(
        n,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": reason, "outageMinutes": null, "detail": detail}),
    )
}

fn completed(n: u32) -> Envelope {
    event(
        n,
        "run.completed",
        "harness",
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done", "detail": "The engine ended its turn."}),
    )
}

fn view_of(events: &[Envelope]) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e.clone());
    }
    s.view()
}

fn card(v: &RunView) -> String {
    final_card(
        v,
        &FinalCard {
            local_path: String::new(),
            presigned_url: None,
            expires_at: None,
            usage: None,
            label_missing_after: None,
            tests: None,
            flow: None,
            session: None,
            outside_prompts: None,
        },
    )
}

/// T11: the worker's sentence alone, without "Notice from the run (...)".
#[test]
fn b891_a_disk_notice_reads_the_workers_sentence_alone() {
    for (reason, sentence) in [
        ("disk_low", W4),
        ("disk_low", ROOT),
        ("disk_note", N1),
        ("disk_note", N2),
    ] {
        let n = notice_of(&notify(4, reason, Some(sentence))).unwrap();
        assert_eq!(n.text, sentence, "{reason}");
        assert_eq!(n.kind, NoticeKind::Notify);
        assert!(sentence.chars().count() <= NOTICE_MAX_CHARS);
    }
    // Without a sentence each reason has its own words.
    assert_eq!(
        notice_of(&notify(4, "disk_low", None)).unwrap().text,
        "The run said its disk is running low."
    );
    assert_eq!(
        notice_of(&notify(4, "disk_note", Some("  "))).unwrap().text,
        "The run said its workspace folder is large."
    );
    assert_eq!(
        notice::DISK_LOW_FALLBACK,
        "The run said its disk is running low."
    );
    assert_eq!(
        notice::DISK_NOTE_FALLBACK,
        "The run said its workspace folder is large."
    );
}

/// That is what a `ham` from before this change shows of a row.
#[test]
fn b891_a_reason_without_an_arm_keeps_its_form_and_leaves_127_characters_under_nine() {
    let reason = "made_up_x";
    assert_eq!(reason.chars().count(), "disk_note".chars().count());
    let whole = "x".repeat(127);
    let n = notice_of(&notify(4, reason, Some(&whole))).unwrap();
    assert_eq!(n.text, format!("Notice from the run (made_up_x). {whole}"));
    assert_eq!(n.text.chars().count(), NOTICE_MAX_CHARS);
    let longer = "y".repeat(128);
    let n = notice_of(&notify(4, reason, Some(&longer))).unwrap();
    assert_eq!(n.text.chars().count(), NOTICE_MAX_CHARS);
    assert!(n.text.ends_with('\u{2026}'), "{}", n.text);
    // So the older client shows the first row as far as its second size and the second row as far as the machine's own disk.
    let old = |row: &str| -> String {
        let shown: String = row.chars().take(126).collect();
        format!("Notice from the run (disk_note). {shown}\u{2026}")
    };
    assert!(
        old(N1).ends_with("none up to 200 Mi\u{2026}"),
        "{}",
        old(N1)
    );
    assert!(
        old(N2).ends_with("the machine's own disk. T\u{2026}"),
        "{}",
        old(N2)
    );
}

/// The client's two reason words are the worker's: read from the worker's source.
#[test]
fn b891_the_reason_words_are_the_workers() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agentd/src/direct");
    let disk = std::fs::read_to_string(root.join("disk.rs")).unwrap();
    assert!(
        disk.contains(&format!(
            "pub const NOTE_REASON: &str = \"{}\";",
            notice::DISK_NOTE
        )),
        "the worker's reason of the note"
    );
    let retry = std::fs::read_to_string(root.join("retry.rs")).unwrap();
    assert!(
        retry.contains(&format!(
            "collector.record_notify(\"{}\", None, &note);",
            notice::DISK_LOW
        )),
        "the worker's reason of the early notice"
    );
}

/// T12: the run view keeps both reasons in a list of their own, in event order and bounded; the list of copy notices is as it was.
#[test]
fn b891_the_run_view_keeps_the_disk_notices_in_order_and_bounded() {
    let v = view_of(&[
        notify(4, "disk_low", Some(W4)),
        notify(5, "disk_note", Some(N1)),
        notify(6, "disk_note", Some(N2)),
        notify(
            7,
            "copy_not_saved",
            Some("A copy of the work is late; ham keeps trying."),
        ),
        notify(8, "outage", None),
    ]);
    let got: Vec<(&str, &str, &str)> = v
        .disk_notices
        .iter()
        .map(|n| (n.reason, n.text.as_str(), n.event_id.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("disk_low", W4, id(4).as_str()),
            ("disk_note", N1, id(5).as_str()),
            ("disk_note", N2, id(6).as_str()),
        ]
    );
    assert_eq!(v.disk_notices[0].ts, "2026-11-21T08:04:08Z");
    // The copy notice is in its own list alone, and every notice is in the general list as before.
    assert_eq!(v.copy_notices.len(), 1);
    assert_eq!(
        v.copy_notices[0].text,
        "A copy of the work is late; ham keeps trying."
    );
    assert_eq!(v.notices.len(), 5);
    // An event applied twice is one notice.
    let twice = view_of(&[
        notify(4, "disk_low", Some(W4)),
        notify(4, "disk_low", Some(W4)),
    ]);
    assert_eq!(twice.disk_notices.len(), 1);
    // The newest 32 are kept.
    assert_eq!(notice::MAX_DISK_NOTICES, 32);
    let many: Vec<Envelope> = (1..=40)
        .map(|n| notify(n, "disk_low", Some(ROOT)))
        .collect();
    let v = view_of(&many);
    assert_eq!(v.disk_notices.len(), 32);
    assert_eq!(v.disk_notices[0].event_id, id(9));
    assert_eq!(v.disk_notices[31].event_id, id(40));
    // A view without such a notice has an empty list.
    assert!(
        view_of(&[notify(8, "outage", None)])
            .disk_notices
            .is_empty()
    );
}

/// T15: the run screen's block of each notice holds the worker's sentence alone.
#[test]
fn b891_the_run_screen_shows_each_sentence_alone() {
    let mut s = RunState::new(RUN);
    for e in [
        notify(4, "disk_low", Some(W4)),
        notify(5, "disk_note", Some(N1)),
        notify(6, "disk_note", Some(N2)),
        completed(9),
    ] {
        s.apply(e);
    }
    let chat = ChatModel::from_progress_feed(
        &s,
        &ChatInputs {
            prompt: Some("Fix the failing date test"),
            final_message: FinalMessage::Text("Done."),
            zip_path: None,
        },
    );
    let bodies: Vec<String> = chat.blocks.iter().map(|b| b.body.clone()).collect();
    let at = bodies
        .iter()
        .position(|b| b == W4)
        .unwrap_or_else(|| panic!("no block holds the nearly-full sentence alone: {bodies:?}"));
    assert_eq!(bodies[at + 1], N1, "{bodies:?}");
    assert_eq!(bodies[at + 2], N2, "{bodies:?}");
    assert!(
        !bodies.iter().any(|b| b.contains("Notice from the run")),
        "{bodies:?}"
    );
}

/// T16: the card of a finished run reports what happened; a run that only had these notices has the card it had without them.
#[test]
fn b891_the_card_shows_nothing_for_the_disk_notices() {
    let with = view_of(&[
        notify(4, "disk_low", Some(W4)),
        notify(5, "disk_note", Some(N1)),
        notify(6, "disk_note", Some(N2)),
        completed(9),
    ]);
    let without = view_of(&[completed(9)]);
    assert_eq!(with.disk_notices.len(), 3, "the view holds the notices");
    assert_eq!(card(&with), card(&without));
    let text = card(&with);
    assert!(!text.contains("workspace folder"), "{text}");
    assert!(!text.contains("Storage   "), "{text}");
}
