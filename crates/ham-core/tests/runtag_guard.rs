//! The whole-reply checks of the tag judge. The first live run of the run-tags smoke probe found that the name guard
//! (`utility::guard_raw`), which refuses a blank-separated word of more than 40 characters, refused 16 of 152 valid tag replies: the model
//! writes the JSON list either as `["a", "b"]` or compact as `["a","b"]`, and the compact list is one word of 80 to 110 characters. A
//! tag reply is a list whose items are checked one by one (raw length, grammar, word shape), so the word rule is not its defence.
//!
//! `utility::guard_chars` is the name guard without the word rule; `runtag::judge::judge_reply` runs it on the whole text before the
//! per-tag judge, so a caller that holds the text of an answer has one function to call, and `utility::guard_raw` (run names) is unchanged.

#![allow(clippy::unwrap_used)]

mod runname_common;

use ham_core::runtag::grammar::Reason;
use ham_core::runtag::judge::{MAX_REPLY_BYTES, judge_reply, read_reply};
use ham_core::runtag::vocab::Vocabulary;
use ham_core::utility::{GuardReason, guard_chars, guard_raw};
use proptest::prelude::*;
use runname_common::{aws_key_id, cases, rep};

/// The compact layout of a real reply of the live run: one word of 98 characters.
const COMPACT: &str = r#"<tags>["topic:billing-export","topic:timeout-fix","tech:aws-lambda","tech:s3","topic:multipart-upload"]"#;
const SPACED: &str = r#"<tags>["topic:billing-export", "topic:timeout-fix", "tech:aws-lambda", "tech:s3", "topic:multipart-upload"]"#;

fn kept(text: &str) -> Vec<String> {
    judge_reply("stop_sequence", text, &Vocabulary::default(), &[]).kept
}

fn rejects(text: &str) -> Vec<Reason> {
    judge_reply("stop_sequence", text, &Vocabulary::default(), &[]).rejects
}

#[test]
fn guard_chars_is_the_name_guard_without_the_word_rule() {
    assert!(COMPACT.split_whitespace().any(|w| w.chars().count() > 40));
    assert_eq!(
        guard_raw(COMPACT),
        Err(GuardReason::LongToken),
        "names keep their guard"
    );
    assert_eq!(guard_chars(COMPACT), Ok(()));
    assert_eq!(guard_chars(SPACED), Ok(()));

    let esc = format!("note{}", '\u{1b}');
    let zero_width = format!("a{}b", '\u{200b}');
    let bidi = format!("a{}b", '\u{202e}');
    let tag_char = format!("a{}b", '\u{e0041}');
    let key = format!("see {}", aws_key_id());
    for (label, raw, reason) in [
        ("an escape byte", esc.as_str(), GuardReason::ControlChars),
        (
            "a zero-width character",
            zero_width.as_str(),
            GuardReason::InvisibleChars,
        ),
        (
            "a bidirectional override",
            bidi.as_str(),
            GuardReason::ControlChars,
        ),
        (
            "a tag character",
            tag_char.as_str(),
            GuardReason::InvisibleChars,
        ),
        ("a key shape", key.as_str(), GuardReason::CredentialShape),
    ] {
        assert_eq!(guard_chars(raw), Err(reason), "{label}");
        assert_eq!(
            guard_raw(raw),
            Err(reason),
            "{label}: the name guard says the same"
        );
    }
    // The order of the checks is the name guard's, with a long word in the way.
    let both = format!("{} {}", rep('a', 50), esc);
    assert_eq!(guard_chars(&both), Err(GuardReason::ControlChars));
    assert_eq!(guard_raw(&both), Err(GuardReason::ControlChars));
    assert_eq!(guard_chars(""), Ok(()));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases(256)))]

    #[test]
    fn guard_chars_differs_from_guard_raw_only_by_the_word_rule(raw in "(?s).{0,200}") {
        match guard_raw(&raw) {
            Ok(()) => prop_assert_eq!(guard_chars(&raw), Ok(())),
            Err(GuardReason::LongToken) => prop_assert_eq!(guard_chars(&raw), Ok(())),
            Err(other) => prop_assert_eq!(guard_chars(&raw), Err(other)),
        }
    }

    #[test]
    fn a_list_without_blanks_is_judged_like_the_same_list_with_blanks(items in prop::collection::vec("[a-z]{3,10}-[a-z]{3,10}", 1..6)) {
        let quoted: Vec<String> = items.iter().map(|i| format!("\"topic:{i}\"")).collect();
        let compact = format!("<tags>[{}]", quoted.join(","));
        let spaced = format!("<tags>[{}]", quoted.join(", "));
        prop_assert_eq!(kept(&compact), kept(&spaced));
        prop_assert_eq!(rejects(&compact), rejects(&spaced));
    }
}

#[test]
fn compact_and_spaced_lists_are_judged_alike() {
    let want = vec![
        "topic:billing-export",
        "topic:timeout-fix",
        "tech:aws-lambda",
        "tech:s3",
        "topic:multipart-upload",
    ];
    assert_eq!(kept(COMPACT), want);
    assert_eq!(kept(SPACED), want);
    assert_eq!(rejects(COMPACT), rejects(SPACED));
    assert_eq!(
        read_reply("stop_sequence", COMPACT),
        read_reply("stop_sequence", SPACED)
    );
}

#[test]
fn the_judge_refuses_a_reply_whose_whole_text_is_unsafe_and_says_why() {
    let list = r#"<tags>["topic:csv-export"]"#;
    let key = format!("Use {} please. {list}", aws_key_id());
    let esc = format!("Sure.{}\n{list}", '\u{1b}');
    let zero_width = format!("S{}ure.\n{list}", '\u{200b}');
    let bidi = format!("Sure {} \n{list}", '\u{202e}');
    for (label, text, reason) in [
        (
            "a key shape in the lead-in",
            key.as_str(),
            Reason::CredentialShape,
        ),
        (
            "an escape byte in the lead-in",
            esc.as_str(),
            Reason::ControlChars,
        ),
        (
            "a zero-width character in the lead-in",
            zero_width.as_str(),
            Reason::InvisibleChars,
        ),
        (
            "a bidirectional override in the lead-in",
            bidi.as_str(),
            Reason::ControlChars,
        ),
    ] {
        let judged = judge_reply("stop_sequence", text, &Vocabulary::default(), &[]);
        assert!(
            judged.kept.is_empty(),
            "{label}: the reply is refused and never cleaned"
        );
        assert_eq!(judged.rejects, vec![reason], "{label}");
        assert_eq!(read_reply("stop_sequence", text), Err(reason), "{label}");
    }
}

#[test]
fn a_long_word_outside_the_list_is_not_a_reason_to_refuse_and_is_never_read() {
    let text = format!(
        "{} <tags>[\"topic:csv-export\",\"tech:s3\"]",
        rep('a', 5000)
    );
    let judged = judge_reply("stop_sequence", &text, &Vocabulary::default(), &[]);
    assert_eq!(judged.kept, vec!["topic:csv-export", "tech:s3"]);
    assert!(judged.rejects.is_empty(), "{:?}", judged.rejects);
}

#[test]
fn a_long_element_drops_only_that_tag() {
    let text = format!(
        "<tags>[\"topic:csv-export\",\"topic:{}\",\"tech:s3\"]",
        rep('a', 70)
    );
    let judged = judge_reply("stop_sequence", &text, &Vocabulary::default(), &[]);
    assert_eq!(judged.kept, vec!["topic:csv-export", "tech:s3"]);
    assert_eq!(judged.rejects, vec![Reason::RawShape]);
}

#[test]
fn a_control_character_written_as_a_json_escape_drops_only_that_tag() {
    // The whole-text guard sees the six characters backslash, u, 0, 0, 1, b; the decoded string holds the escape byte.
    let text = r#"<tags>["topic:csv-export","topic:a\u001bb","tech:s3"]"#;
    let judged = judge_reply("stop_sequence", text, &Vocabulary::default(), &[]);
    assert_eq!(judged.kept, vec!["topic:csv-export", "tech:s3"]);
    assert_eq!(judged.rejects, vec![Reason::ControlChars]);
}

#[test]
fn an_oversized_reply_is_refused_before_it_is_read() {
    assert_eq!(MAX_REPLY_BYTES, 16 * 1024);
    let list = r#"<tags>["topic:csv-export"]"#;
    let at_limit = format!("{}{list}", " ".repeat(MAX_REPLY_BYTES - list.len()));
    assert_eq!(at_limit.len(), MAX_REPLY_BYTES);
    assert_eq!(kept(&at_limit), vec!["topic:csv-export"]);
    let over = format!("{}{list}", " ".repeat(MAX_REPLY_BYTES - list.len() + 1));
    let judged = judge_reply("stop_sequence", &over, &Vocabulary::default(), &[]);
    assert!(judged.kept.is_empty());
    assert_eq!(judged.rejects, vec![Reason::RawShape]);
    assert_eq!(read_reply("stop_sequence", &over), Err(Reason::RawShape));
}

#[test]
fn the_stop_reason_decides_before_the_text() {
    let esc = format!("{}<tags>[\"topic:csv-export\"]", '\u{1b}');
    let judged = judge_reply("max_tokens", &esc, &Vocabulary::default(), &[]);
    assert_eq!(judged.rejects, vec![Reason::StopMaxTokens]);
    assert_eq!(read_reply("refusal", &esc), Err(Reason::StopRefusal));
}

#[test]
fn read_reply_gives_the_strings_a_judge_reads_and_the_reason_a_judge_refuses_with() {
    assert_eq!(
        read_reply("stop_sequence", COMPACT).unwrap(),
        vec![
            "topic:billing-export",
            "topic:timeout-fix",
            "tech:aws-lambda",
            "tech:s3",
            "topic:multipart-upload"
        ]
    );
    assert_eq!(
        read_reply("stop_sequence", "no list here"),
        Err(Reason::BadJson)
    );
    assert_eq!(read_reply("stop_sequence", "  "), Err(Reason::NoText));
    assert_eq!(read_reply("stop_sequence", "<tags>"), Ok(vec![]));
    assert_eq!(
        read_reply("end_turn", COMPACT),
        Err(Reason::StopEndTurn),
        "only a stop sequence ends a good reply"
    );
}
