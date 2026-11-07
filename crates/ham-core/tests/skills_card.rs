//! The skills of a run on the card.
//!
//! agentd's skills module writes one `turn.progress` event of kind `skills` after it placed the run's skills and, where the fork can list
//! them, after the engine's own listing showed each one (`crates/agentd/src/direct/skills/mod.rs`, `skills_event`): each skill with its
//! `name`, `digest`, `files` and `bytes`, and `preflight {ran, ms}`. It holds no instruction text. The run view carries it as
//! `RunView::skills`; the run screen draws it as a folded card, `ham watch` prints it once, and the transcript has one plain notice for
//! it instead of "Event skills". Nothing here says the work was verified, tested, gated or evaluated.
#![allow(clippy::unwrap_used)]

use ham_core::notice::{NoticeKind, notice_of};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const BANNED: [&str; 4] = ["verified", "tested", "gated", "evaluated"];

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-17T08:15:00Z",
        "type": "turn.progress", "authority": "agent", "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

/// 64 hex digits that start with `lead` repeated.
fn digest(lead: char) -> String {
    std::iter::repeat_n(lead, 12)
        .chain(std::iter::repeat_n('0', 52))
        .collect()
}

/// The event as agentd writes it (`skills_event`): one entry per placed skill.
fn skills_event(n: u32, items: &[(&str, char, u64, u64)], ms: Option<u64>) -> Envelope {
    event(
        n,
        json!({
            "turn": 1, "kind": "skills",
            "skills": items.iter().map(|(name, lead, files, bytes)| json!({
                "name": name, "digest": digest(*lead), "files": files, "bytes": bytes
            })).collect::<Vec<_>>(),
            "preflight": {"ran": ms.is_some(), "ms": ms},
        }),
    )
}

fn view_of(events: Vec<Envelope>) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s.view()
}

fn pack_like() -> Vec<(&'static str, char, u64, u64)> {
    vec![
        ("lifecycle-research", 'a', 1, 1203),
        ("lifecycle-check", 'b', 2, 18940),
        ("my-skill", 'c', 1, 87),
    ]
}

fn no_claim(text: &str) {
    let lower = text.to_lowercase();
    for w in BANNED {
        assert!(!lower.contains(w), "{text:?} holds {w:?}");
    }
}

#[test]
fn a_run_that_placed_skills_has_a_card_with_each_name_digest_and_size() {
    let v = view_of(vec![skills_event(5, &pack_like(), Some(82))]);
    let c = v.skills.expect("a card");
    assert_eq!(c.request_id, id(5));
    assert_eq!(c.rows.len(), 3);
    assert_eq!(c.rows[0].name, "lifecycle-research");
    assert_eq!(c.rows[0].digest, digest('a'));
    assert_eq!((c.rows[0].files, c.rows[0].bytes), (1, 1203));
    assert_eq!((c.rows[1].files, c.rows[1].bytes), (2, 18940));
    assert_eq!(c.preflight_ms, Some(82));
    assert_eq!(c.title(), "skills · 3 placed");
    let lines = c.lines(100).join("\n");
    // The first 12 digits of the digest, as `ham rules show` prints them.
    assert!(
        lines.contains("lifecycle-research") && lines.contains("sha256 aaaaaaaaaaaa"),
        "{lines}"
    );
    assert!(
        lines.contains("lifecycle-check") && lines.contains("sha256 bbbbbbbbbbbb"),
        "{lines}"
    );
    assert!(
        lines.contains("1 file, 1,203 bytes"),
        "a count and a size with the file noun in the singular: {lines}"
    );
    assert!(lines.contains("2 files, 18,940 bytes"), "{lines}");
    assert!(
        !lines.contains(&digest('a')),
        "the card shows the short digest, not all 64 digits: {lines}"
    );
    println!(
        "SKILLS-CARD TRIGGER PRESENT: skills=3 title={:?} lines={}",
        c.title(),
        c.lines(100).len()
    );
}

#[test]
fn a_single_skill_reads_in_the_singular() {
    let v = view_of(vec![skills_event(1, &[("only-one", 'd', 1, 10)], None)]);
    assert_eq!(v.skills.unwrap().title(), "skills · 1 placed");
}

#[test]
fn a_run_without_the_event_or_with_nothing_in_it_has_no_card() {
    assert!(view_of(vec![]).skills.is_none());
    assert!(
        view_of(vec![skills_event(1, &[], Some(5))])
            .skills
            .is_none()
    );
    // An event of the kind with no list, or a list of the wrong type, or a list with no usable entry.
    for value in [
        json!({"turn": 1, "kind": "skills"}),
        json!({"turn": 1, "kind": "skills", "skills": "lifecycle-tdd"}),
        json!({"turn": 1, "kind": "skills", "skills": [{"digest": "x"}, 7, null, {"name": "  "}]}),
    ] {
        assert!(
            view_of(vec![event(1, value.clone())]).skills.is_none(),
            "{value}"
        );
    }
    // Another kind of progress event is not skills.
    let other = event(1, json!({"turn": 1, "kind": "research", "searches": 3}));
    assert!(view_of(vec![other]).skills.is_none());
}

#[test]
fn the_last_event_is_the_one_the_card_shows() {
    let v = view_of(vec![
        skills_event(1, &[("old-skill", 'a', 1, 1)], None),
        skills_event(
            2,
            &[("new-skill", 'b', 1, 2), ("second", 'c', 1, 3)],
            Some(40),
        ),
    ]);
    let c = v.skills.unwrap();
    assert_eq!(c.request_id, id(2));
    let names: Vec<&str> = c.rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["new-skill", "second"]);
}

#[test]
fn the_card_says_whether_the_engine_listed_the_skills_before_the_first_prompt() {
    let ran = view_of(vec![skills_event(1, &pack_like(), Some(82))])
        .skills
        .unwrap();
    let text = ran.lines(100).join("\n");
    assert!(text.contains("listed all 3"), "{text}");
    assert!(text.contains("82 ms"), "{text}");
    let not = view_of(vec![skills_event(1, &pack_like(), None)])
        .skills
        .unwrap();
    let text = not.lines(100).join("\n");
    assert!(text.contains("not available"), "{text}");
    assert!(!text.contains("listed all"), "{text}");
    // Both say where the skills are and what the engine does with them, in words that claim nothing about the work.
    for t in [&ran.lines(100).join("\n"), &text] {
        assert!(t.contains("read-only"), "{t}");
        assert!(t.contains("matches"), "{t}");
    }
}

#[test]
fn every_line_fits_the_width_it_is_given() {
    let long = "a-skill-with-a-very-long-name-that-goes-on-and-on-and-on-and-on-1234";
    let v = view_of(vec![skills_event(
        1,
        &[(long, 'e', 12, 123_456_789), ("lifecycle-tdd", 'f', 1, 5)],
        Some(1234),
    )]);
    let c = v.skills.unwrap();
    for width in [12usize, 20, 40, 76, 120] {
        for l in c.lines(width) {
            assert!(l.chars().count() <= width, "{width}: {l:?}");
        }
    }
}

#[test]
fn text_from_the_event_is_made_safe_before_it_reaches_a_screen() {
    let v = view_of(vec![event(
        1,
        json!({"turn": 1, "kind": "skills", "preflight": {"ran": true, "ms": 3},
               "skills": [{"name": "\u{1b}[31mred\u{7}-skill\u{1b}]0;title\u{7}", "digest": "zz\u{1b}[0m", "files": 1, "bytes": 2}]}),
    )]);
    let c = v.skills.unwrap();
    let all = format!("{}\n{}", c.title(), c.lines(100).join("\n"));
    assert!(
        all.chars().all(|ch| ch == '\n' || !ch.is_control()),
        "{all:?}"
    );
    assert!(all.contains("red-skill"), "{all}");
    assert!(
        all.contains("sha256 ?"),
        "a digest that is not hex is shown as unknown: {all}"
    );
}

#[test]
fn no_line_of_the_card_claims_anything_about_the_work() {
    for ms in [None, Some(7)] {
        let c = view_of(vec![skills_event(1, &pack_like(), ms)])
            .skills
            .unwrap();
        no_claim(&c.title());
        no_claim(&c.lines(76).join("\n"));
    }
}

#[test]
fn a_very_long_list_is_cut_and_the_card_says_how_many_it_left_out() {
    // 40 entries without a digest fit an inline event of 4,096 bytes; the card keeps 32 rows (every skill a run may carry) and counts the rest.
    let items: Vec<Value> = (0..40)
        .map(|i| json!({"name": format!("skill-{i:02}"), "files": 1, "bytes": 1}))
        .collect();
    let e = event(
        1,
        json!({"turn": 1, "kind": "skills", "skills": items, "preflight": {"ran": false, "ms": null}}),
    );
    let c = view_of(vec![e]).skills.unwrap();
    assert!(c.rows.len() <= 32, "{}", c.rows.len());
    assert_eq!(c.total, 40);
    assert_eq!(c.title(), "skills · 40 placed");
    let text = c.lines(80).join("\n");
    assert!(text.contains("and 8 more"), "{text}");
}

#[test]
fn b776_every_skill_a_run_may_carry_has_a_row() {
    let names: Vec<String> = (0..32).map(|i| format!("skill-{i:02}")).collect();
    let items: Vec<(&str, char, u64, u64)> =
        names.iter().map(|n| (n.as_str(), 'a', 1, 1)).collect();
    let c = view_of(vec![skills_event(1, &items, None)]).skills.unwrap();
    assert_eq!((c.rows.len(), c.total), (32, 32));
    assert!(!c.lines(80).join("\n").contains("more"));
}

#[test]
fn b776_the_total_the_event_names_counts_skills_the_event_could_not_list() {
    // agentd lists as many skills as fit the 4,096 bytes of an inline event and names the total beside them.
    let mut e = skills_event(
        1,
        &[("one", 'a', 1, 1), ("two", 'b', 2, 2), ("three", 'c', 3, 3)],
        None,
    );
    let serde_json::Value::Object(_) = serde_json::to_value(&e).unwrap() else {
        unreachable!()
    };
    let mut v = serde_json::to_value(&e).unwrap();
    v["payload"]["value"]["total"] = json!(32);
    e = serde_json::from_value(v).unwrap();
    let c = view_of(vec![e]).skills.unwrap();
    assert_eq!((c.rows.len(), c.total), (3, 32));
    assert_eq!(c.title(), "skills · 32 placed");
    assert!(c.lines(80).join("\n").contains("and 29 more"));
}

#[test]
fn the_transcript_gets_one_plain_notice_and_not_event_skills() {
    let e = skills_event(1, &pack_like(), Some(82));
    let n = notice_of(&e).expect("a notice");
    assert_eq!(n.kind, NoticeKind::Notify);
    assert_eq!(
        n.text,
        "Skills: 3 placed (lifecycle-research, lifecycle-check, my-skill)."
    );
    assert!(!n.text.contains("Event skills"));
    no_claim(&n.text);
    // A long list keeps the line inside the notice limit.
    let names: Vec<String> = (0..8)
        .map(|i| format!("a-fairly-long-skill-name-{i}"))
        .collect();
    let items: Vec<(&str, char, u64, u64)> =
        names.iter().map(|n| (n.as_str(), 'a', 1, 1)).collect();
    let long = notice_of(&skills_event(2, &items, None)).unwrap();
    assert!(
        long.text.chars().count() <= ham_core::notice::NOTICE_MAX_CHARS,
        "{}",
        long.text
    );
    assert!(long.text.starts_with("Skills: 8 placed"), "{}", long.text);
}
