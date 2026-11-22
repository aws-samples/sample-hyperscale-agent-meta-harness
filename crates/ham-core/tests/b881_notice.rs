//! In the client: what `ham` prints when a run says that its session's storage refused a write, and the `Storage` rows of the
//! card of a run that ended.
//!
//! The worker's notice is a `turn.progress` event of kind `notify` with the reason `storage_refused` and a `storage` member
//! (`crates/agentd/src/direct/storage_watch.rs`). The client builds its lines from the member, so that no command is cut, and counts
//! the notices while it folds the run's events, so that the card says it for every session and however the run ended.
#![allow(clippy::unwrap_used)]

use ham_core::chat::{ChatInputs, ChatModel, FinalMessage};
use ham_core::notice::{NOTICE_MAX_CHARS, NoticeKind, notice_of};
use ham_core::render::{FinalCard, final_card};
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
        "ts": format!("2026-11-21T{:02}:{:02}:07Z", (30 + n) / 60, (30 + n) % 60), "type": ty, "authority": authority,
        "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn copy_before(session: u32, count: u32) -> Value {
    json!({"session": session, "count": count, "at": "2026-11-21T00:37:12Z", "headSha": HEAD, "versionId": "3sL4kqtJlcpXroDTDmJ+rmSpXd3dIbrHY+MTRCxf3vjVBH40Nr8X8gdRQBpUMLUo"})
}

/// The first notice of a session, as the worker writes it.
fn full(n: u32, session: u32, used: Option<u64>, partial: bool, copy: Option<Value>) -> Envelope {
    let mut storage = json!({"session": session, "stretch": 1, "seen": "tool", "partial": partial, "quotaMb": 1024});
    if let Some(u) = used {
        storage["usedMb"] = json!(u);
    }
    if let Some(c) = copy {
        storage["copyBefore"] = c;
    }
    event(
        n,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "storage_refused", "outageMinutes": null,
               "detail": "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device). On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up. In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again.",
               "storage": storage}),
    )
}

/// A later notice of a session.
fn short(n: u32, session: u32, stretch: u32, first: u32, copy: Option<Value>) -> Envelope {
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

fn view_of(events: &[Envelope]) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e.clone());
    }
    s.view()
}

fn completed(n: u32, label: &str, detail: Option<&str>) -> Envelope {
    event(
        n,
        "run.completed",
        "harness",
        json!({"status": "complete", "outcome": "reported_done", "label": label, "detail": detail}),
    )
}

fn blocked(n: u32) -> Envelope {
    event(
        n,
        "goal.blocked",
        "harness",
        json!({"reason": "the run's session was lost and no new session could start"}),
    )
}

fn lines_of(e: &Envelope) -> Vec<String> {
    storage_notice::lines(&storage_notice::parse(e).unwrap(), RUN)
}

// ---- test 38: the lines of the first and of a later notice ------------------------------------------------------------------------

#[test]
fn b881_the_client_has_its_lines_for_the_first_and_for_a_later_notice() {
    assert_eq!(REPLACED_COPY_DAYS, 30);
    let first = full(1, 1, Some(165), false, Some(copy_before(1, 2)));
    assert_eq!(
        lines_of(&first),
        vec![
            "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device).".to_string(),
            "On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up.".to_string(),
            "In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again.".to_string(),
            "A file that a command wrote meanwhile can be empty; ham does not know which.".to_string(),
            "Copy 2 (2026-11-21 00:37 UTC) is the last copy saved before ham first saw this. It is kept for 30 days after a newer copy replaced it:".to_string(),
            format!("  ham pull {RUN} --before-refusal {}", id(1)),
            format!("The newest copy holds the files as they are, empty ones included: ham pull {RUN} --snapshot"),
        ]
    );
    // The notice line of the run is the first of them.
    let n = notice_of(&first).unwrap();
    assert_eq!(n.kind, NoticeKind::Notify);
    assert_eq!(n.text, lines_of(&first)[0]);
    assert!(!n.text.contains("Notice from the run"), "{}", n.text);

    // Without a copy: one sentence in place of the copy's two lines, and the command for the newest copy stays.
    let no_copy = full(1, 1, Some(165), false, None);
    assert_eq!(
        lines_of(&no_copy)[4..],
        [
            "This session had saved no copy before ham first saw this.".to_string(),
            format!(
                "The newest copy holds the files as they are, empty ones included: ham pull {RUN} --snapshot"
            ),
        ]
    );
    // A walk cut short and a size that was not measured.
    assert_eq!(
        lines_of(&full(1, 1, Some(165), true, None))[0],
        "The session's storage refused a write while its files add up to at least 165 MiB of 1024 MiB (no space left on device)."
    );
    assert_eq!(
        lines_of(&full(1, 1, None, false, None))[0],
        "The session's storage refused a write (no space left on device); the size of its files could not be measured."
    );

    // A later notice: the count, the command of the session's first notice, the newest copy.
    let later = short(5, 1, 2, 1, Some(copy_before(1, 2)));
    assert_eq!(
        lines_of(&later),
        vec![
            "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session.".to_string(),
            format!("The last copy saved before ham first saw it: ham pull {RUN} --before-refusal {}", id(1)),
            format!("The newest copy holds the files as they are, empty ones included: ham pull {RUN} --snapshot"),
        ]
    );
    assert_eq!(lines_of(&short(5, 1, 2, 1, None)).len(), 2);

    // Every line fits the notice width with the longest values: 26-character ids, a count of ten digits, sizes of ten digits. No
    // command is cut.
    let mut big = full(
        1,
        4_000_000_000,
        Some(9_999_999_999),
        true,
        Some(copy_before(4_000_000_000, 4_000_000_000)),
    );
    if let harness_protocol::envelope::Payload::Inline { value } = &mut big.payload {
        value["storage"]["quotaMb"] = json!(9_999_999_999u64);
    }
    let mut big_later = short(
        5,
        4_000_000_000,
        4_000_000_000,
        1,
        Some(copy_before(1, 4_000_000_000)),
    );
    if let harness_protocol::envelope::Payload::Inline { value } = &mut big_later.payload {
        value["storage"]["stretch"] = json!(4_000_000_000u64);
    }
    for e in [&first, &no_copy, &later, &big, &big_later] {
        let lines = lines_of(e);
        for l in &lines {
            assert!(
                l.chars().count() <= NOTICE_MAX_CHARS,
                "{} characters: {l}",
                l.chars().count()
            );
            assert!(!l.contains('\u{2026}'), "cut: {l}");
        }
        assert!(
            lines
                .iter()
                .any(|l| l.ends_with(&format!("ham pull {RUN} --snapshot"))),
            "{lines:?}"
        );
    }
    assert!(
        lines_of(&big)
            .iter()
            .any(|l| l == &format!("  ham pull {RUN} --before-refusal {}", id(1)))
    );

    // A notice made by the stage's test switch says so, so that no record of a test reads as a real refusal.
    let mut by_switch = full(1, 1, Some(12), false, None);
    if let harness_protocol::envelope::Payload::Inline { value } = &mut by_switch.payload {
        value["storage"]["testSwitch"] = json!(true);
    }
    assert_eq!(
        lines_of(&by_switch).last().unwrap(),
        "This notice was made by the stage's test switch; the storage refused no write."
    );
}

// ---- test 39: a notice without the member -----------------------------------------------------------------------------------------

#[test]
fn b881_a_notice_without_the_member_falls_back_to_the_workers_sentence() {
    let bare = event(
        1,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "storage_refused",
               "detail": "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device). It ended by itself in measurements."}),
    );
    assert!(storage_notice::parse(&bare).is_none());
    let n = notice_of(&bare).unwrap();
    assert_eq!(
        n.text,
        "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device). It ended by itself in measurements."
    );
    assert!(!n.text.contains("storage_refused"), "{}", n.text);
    // With neither the member nor a sentence, one plain sentence.
    let empty = event(
        2,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "storage_refused"}),
    );
    assert_eq!(
        notice_of(&empty).unwrap().text,
        "The session's storage refused a write."
    );
    // A member that is no object, or that carries no session, is no member.
    for storage in [
        json!("x"),
        json!({}),
        json!({"session": "one"}),
        json!({"session": 0}),
    ] {
        let odd = event(
            3,
            "turn.progress",
            "agent",
            json!({"turn": 1, "kind": "notify", "reason": "storage_refused", "detail": "The worker's sentence.", "storage": storage}),
        );
        assert!(storage_notice::parse(&odd).is_none(), "{odd:?}");
        assert_eq!(notice_of(&odd).unwrap().text, "The worker's sentence.");
    }
    // An id in `first` that is not an event id is not printed into a command: the notice's own id is.
    let mut odd_first = short(5, 1, 2, 1, Some(copy_before(1, 2)));
    if let harness_protocol::envelope::Payload::Inline { value } = &mut odd_first.payload {
        value["storage"]["first"] = json!("x; echo hello");
    }
    assert_eq!(
        lines_of(&odd_first)[1],
        format!(
            "The last copy saved before ham first saw it: ham pull {RUN} --before-refusal {}",
            id(5)
        )
    );
    // A copy whose commit or time is not what a copy carries is no copy.
    let mut odd_copy = full(
        1,
        1,
        Some(165),
        false,
        Some(
            json!({"session": 1, "count": 2, "at": "yesterday", "headSha": HEAD, "versionId": "v"}),
        ),
    );
    assert_eq!(
        lines_of(&odd_copy)[4],
        "This session had saved no copy before ham first saw this."
    );
    if let harness_protocol::envelope::Payload::Inline { value } = &mut odd_copy.payload {
        value["storage"]["copyBefore"] = json!({"session": 1, "count": 2, "at": "2026-11-21T00:37:12Z", "headSha": "not-a-commit", "versionId": "v"});
    }
    assert_eq!(
        lines_of(&odd_copy)[4],
        "This session had saved no copy before ham first saw this."
    );
}

// ---- test 40: the stop's notice without its reason word ---------------------------------------------------------------------------

#[test]
fn b881_the_stops_notice_is_printed_without_its_reason_word() {
    let stop = event(
        1,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "disk_full", "outageMinutes": null,
               "detail": "The session's storage refused a write while its files add up to 158 MiB of 1024 MiB, root 5343 MiB free. Prompts can fail while the storage refuses writes. Send a message with `ham say` to go on once the storage takes writes again (Cline may not be able to answer while it refuses them), or end the run with `ham finish` and keep what exists."}),
    );
    let n = notice_of(&stop).unwrap();
    assert!(
        !n.text.contains("disk_full") && !n.text.contains("Notice from the run"),
        "{}",
        n.text
    );
    assert!(
        n.text.starts_with("The session's storage refused a write while its files add up to 158 MiB of 1024 MiB, root 5343 MiB free. Prompts can fail"),
        "{}",
        n.text
    );
    assert!(n.text.chars().count() <= NOTICE_MAX_CHARS);
    // The stop of a disk that is full reads its worker's sentence the same way.
    let full_disk = event(
        2,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "disk_full", "detail": "The session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free. Prompts fail until space is freed."}),
    );
    assert!(
        notice_of(&full_disk)
            .unwrap()
            .text
            .starts_with("The session disk is full, workspace 975 MiB")
    );
    // Without a sentence of the worker, one plain sentence.
    let bare = event(
        3,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "disk_full"}),
    );
    assert_eq!(
        notice_of(&bare).unwrap().text,
        "The run stopped on a disk error."
    );
    // A reason this client does not know keeps its form.
    let other = event(
        4,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "made_up_x", "detail": "The workspace folder is nearly full."}),
    );
    assert_eq!(
        notice_of(&other).unwrap().text,
        "Notice from the run (made_up_x). The workspace folder is nearly full."
    );
}

// ---- test 42: the card of a run that ended -----------------------------------------------------------------------------------------

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

fn storage_rows_of(card: &str) -> Vec<String> {
    let lines: Vec<&str> = card.lines().collect();
    let Some(at) = lines.iter().position(|l| l.starts_with("Storage   ")) else {
        return Vec::new();
    };
    let mut rows = vec![lines[at].to_string()];
    rows.extend(
        lines[at + 1..]
            .iter()
            .take_while(|l| l.starts_with("          "))
            .map(|l| l.to_string()),
    );
    rows
}

#[test]
fn b881_the_card_has_the_storage_rows() {
    // One notice with a copy.
    let one = view_of(&[
        full(1, 1, Some(165), false, Some(copy_before(1, 2))),
        completed(9, "Cline reported done", Some("The engine ended its turn.")),
    ]);
    let text = card(&one);
    assert_eq!(
        storage_rows_of(&text),
        vec![
            "Storage   ham saw the session's storage refuse writes once in this run (2026-11-21 00:31 UTC, session 1). A file a command wrote then can be empty.".to_string(),
            format!("          ham pull {RUN} --before-refusal {} downloads the last copy saved before ham first saw it.", id(1)),
            "          It is kept 30 days after a newer copy replaced it.".to_string(),
            format!("          ham pull {RUN} --snapshot downloads the newest copy, with the files as they are."),
        ]
    );
    // The rows follow the detail.
    let lines: Vec<&str> = text.lines().collect();
    let detail = lines
        .iter()
        .position(|l| l.starts_with("Detail    "))
        .unwrap();
    assert!(lines[detail + 1].starts_with("Storage   "), "{text}");

    // Three notices over two sessions: the count is of all sessions, the command is the one of the run's first notice.
    let three = view_of(&[
        full(1, 1, Some(165), false, Some(copy_before(1, 2))),
        short(3, 1, 2, 1, Some(copy_before(1, 2))),
        full(5, 2, Some(170), false, Some(copy_before(2, 6))),
        completed(9, "Cline reported done", None),
    ]);
    assert_eq!(
        storage_rows_of(&card(&three)),
        vec![
            "Storage   ham saw the session's storage refuse writes 3 times in this run (first 2026-11-21 00:31 UTC, session 1). A file a command wrote then can be empty.".to_string(),
            format!("          ham pull {RUN} --before-refusal {} downloads the last copy saved before ham first saw it.", id(1)),
            "          It is kept 30 days after a newer copy replaced it.".to_string(),
            format!("          ham pull {RUN} --snapshot downloads the newest copy, with the files as they are."),
        ]
    );
    assert_eq!(three.storage.count, 3);

    // A run whose last session wrote no summary: the service's own detail, and the rows all the same, from the events.
    let lost = view_of(&[
        full(1, 1, Some(165), false, Some(copy_before(1, 2))),
        completed(
            9,
            "Cline stopped: the harness ended the run (the run's session was lost)",
            Some("The harness ended the run: the run's session was lost."),
        ),
    ]);
    assert_eq!(storage_rows_of(&card(&lost)).len(), 4, "{}", card(&lost));

    // A blocked run has no result and no detail; the rows stand under its Run line.
    let stuck = view_of(&[
        full(1, 1, Some(165), false, Some(copy_before(1, 2))),
        blocked(8),
    ]);
    assert!(stuck.result.is_none() && stuck.has_ended(), "{stuck:?}");
    let rows = storage_rows_of(&card(&stuck));
    assert_eq!(rows.len(), 4, "{}", card(&stuck));
    assert!(
        rows[0]
            .starts_with("Storage   ham saw the session's storage refuse writes once in this run")
    );

    // A first notice without a copy: no command for a copy before, and the newest copy is named.
    let none_before = view_of(&[
        full(1, 1, Some(165), false, None),
        short(3, 1, 2, 1, None),
        completed(9, "Cline reported done", None),
    ]);
    assert_eq!(
        storage_rows_of(&card(&none_before)),
        vec![
            "Storage   ham saw the session's storage refuse writes 2 times in this run (first 2026-11-21 00:31 UTC, session 1). A file a command wrote then can be empty.".to_string(),
            "          No copy had been saved before ham first saw it.".to_string(),
            format!("          ham pull {RUN} --snapshot downloads the newest copy, with the files as they are."),
        ]
    );

    // A run without notices has no such row, and neither has a run that still works.
    let quiet = view_of(&[completed(9, "Cline reported done", None)]);
    assert!(storage_rows_of(&card(&quiet)).is_empty());
    assert_eq!(quiet.storage.count, 0);
    let working = view_of(&[full(1, 1, Some(165), false, Some(copy_before(1, 2)))]);
    assert!(!working.has_ended());
    assert!(
        storage_rows_of(&card(&working)).is_empty(),
        "{}",
        card(&working)
    );
    assert_eq!(working.storage.count, 1);

    // A notice without the member is not counted: the card rests on what the member says.
    let bare = event(
        1,
        "turn.progress",
        "agent",
        json!({"turn": 1, "kind": "notify", "reason": "storage_refused", "detail": "The worker's sentence."}),
    );
    assert_eq!(
        view_of(&[bare, completed(9, "Cline reported done", None)])
            .storage
            .count,
        0
    );

    // Every row fits 160 characters with the largest numbers.
    let mut events: Vec<Envelope> = vec![full(
        1,
        4_000_000_000,
        Some(165),
        false,
        Some(copy_before(1, 2)),
    )];
    events.extend((2..40).map(|n| short(n, 1, n, 1, None)));
    events.push(completed(99, "Cline reported done", None));
    let mut many = view_of(&events);
    assert_eq!(many.storage.count, 39);
    assert!(
        many.storage.recent.len() <= 16,
        "the view keeps the lines of the newest notices only"
    );
    many.storage.count = u32::MAX;
    let rows = storage_rows_of(&card(&many));
    for row in &rows {
        assert!(
            row.chars().count() <= 160,
            "{} characters: {row}",
            row.chars().count()
        );
    }
    // With numbers that long the second sentence of the first row has a row of its own; nothing is cut.
    assert_eq!(
        rows[1],
        "          A file a command wrote then can be empty."
    );
    assert!(
        rows[0].ends_with("(first 2026-11-21 00:31 UTC, session 4000000000)."),
        "{}",
        rows[0]
    );
    // With the largest numbers a run can have (256 sessions) and 999 notices the first row is one row of 160 characters.
    many.storage.count = 999;
    many.storage.first.as_mut().unwrap().session = 256;
    assert_eq!(storage_rows_of(&card(&many))[0].chars().count(), 160);

    // Notices made by the test switch are said to be so.
    let mut by_switch = full(1, 1, Some(12), false, None);
    if let harness_protocol::envelope::Payload::Inline { value } = &mut by_switch.payload {
        value["storage"]["testSwitch"] = json!(true);
    }
    let rows = storage_rows_of(&card(&view_of(&[
        by_switch,
        completed(9, "Cline reported done", None),
    ])));
    assert_eq!(
        rows.last().unwrap(),
        "          Made by the stage's test switch: the storage refused no write."
    );
}

#[test]
fn b881_the_conversation_shows_the_notice_lines_and_the_storage_row_of_the_delivery() {
    let mut s = RunState::new(RUN);
    for e in [
        full(1, 1, Some(165), false, Some(copy_before(1, 2))),
        completed(9, "Cline reported done", Some("The engine ended its turn.")),
    ] {
        s.apply(e);
    }
    let chat = ChatModel::from_progress_feed(
        &s,
        &ChatInputs {
            prompt: Some("Add percent_change"),
            final_message: FinalMessage::Text("Done."),
            zip_path: None,
        },
    );
    let bodies: Vec<String> = chat.blocks.iter().map(|b| b.body.clone()).collect();
    // The notice and its follow-up lines, in order, each a line of its own.
    let at = bodies
        .iter()
        .position(|b| {
            b.starts_with("The session's storage refused a write while its files add up to 165 MiB")
        })
        .unwrap_or_else(|| panic!("{bodies:?}"));
    assert_eq!(
        bodies[at + 1],
        "On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up.",
        "{bodies:?}"
    );
    assert!(
        bodies[at + 2].starts_with("In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again."),
        "{bodies:?}"
    );
    assert_eq!(
        bodies[at + 3],
        "A file that a command wrote meanwhile can be empty; ham does not know which.",
        "{bodies:?}"
    );
    assert!(
        !bodies.iter().any(|b| b.contains("cannot read why")),
        "{bodies:?}"
    );
    assert!(
        bodies[at + 4].starts_with(
            "Copy 2 (2026-11-21 00:37 UTC) is the last copy saved before ham first saw this."
        ),
        "{bodies:?}"
    );
    assert_eq!(
        bodies[at + 5].trim(),
        format!("ham pull {RUN} --before-refusal {}", id(1))
    );
    assert!(
        bodies[at + 6].ends_with(&format!("ham pull {RUN} --snapshot")),
        "{bodies:?}"
    );
    // The delivery block has the Storage rows after the detail.
    let delivery = bodies
        .iter()
        .find(|b| b.contains("Outcome"))
        .unwrap_or_else(|| panic!("{bodies:?}"));
    let lines: Vec<&str> = delivery.lines().collect();
    let detail = lines.iter().position(|l| l.starts_with("Detail")).unwrap();
    assert!(
        lines[detail + 1]
            .starts_with("Storage  ham saw the session's storage refuse writes once in this run"),
        "{delivery}"
    );
    assert!(lines[detail + 2].contains("--before-refusal"), "{delivery}");
}
