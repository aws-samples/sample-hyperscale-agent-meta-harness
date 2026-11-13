//! The label of a run that ended with a detail: the 14 outcomes and their frozen labels stay as
//! they are, and `stopped_other` whose detail begins "The harness ended the run:" reads as a stop by the harness instead of "the engine
//! reported an unknown stop reason". The label is a pure function of the outcome and the detail; it never uses a word of
//! `BANNED_CLAIM_WORDS` and never quotes the engine's own words.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{DirectOutcome, HARNESS_STOP_PREFIX, banned_claim_word};

const OTHER: &str = "Cline stopped: the engine reported an unknown stop reason";

fn harness(reason: &str) -> String {
    format!("{HARNESS_STOP_PREFIX} {reason}")
}

#[test]
fn without_a_detail_every_outcome_keeps_its_frozen_label() {
    for o in DirectOutcome::ALL {
        assert_eq!(o.label_for(None), o.label(), "{o:?}");
        assert_eq!(o.label_for(Some("")), o.label(), "{o:?}");
        assert_eq!(o.label_for(Some("silence 900 s")), o.label(), "{o:?}");
    }
}

#[test]
fn a_stop_by_the_harness_names_the_harness_and_the_rule() {
    let d = harness("3 rounds in a row had no tool call and changed no file.");
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&d)),
        "Cline stopped: the harness ended the run (3 rounds in a row had no tool call and changed no file)"
    );
    let d = harness(
        "the goal loop stopped 2 times in a row with no change to the files in between (the last round ended aborted).",
    );
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&d)),
        "Cline stopped: the harness ended the run (the goal loop stopped 2 times in a row with no change to the files in between (the last round ended aborted))"
    );
}

#[test]
fn the_engines_own_quoted_words_are_not_in_the_label() {
    let d = harness(
        "the engine ended 2 rounds in a row with the same last line. Its words: \"BLOCKED: waiting for the key\"",
    );
    let label = DirectOutcome::StoppedOther.label_for(Some(&d));
    assert_eq!(
        label,
        "Cline stopped: the harness ended the run (the engine ended 2 rounds in a row with the same last line)"
    );
    assert!(!label.contains("BLOCKED"), "{label}");
}

#[test]
fn only_stopped_other_with_that_prefix_changes() {
    let d = harness("3 rounds in a row changed no file.");
    for o in DirectOutcome::ALL {
        if o != DirectOutcome::StoppedOther {
            assert_eq!(o.label_for(Some(&d)), o.label(), "{o:?}");
        }
    }
    // the prefix must start the detail, and another detail leaves the label alone
    for other in [
        "silence 900 s",
        "something. The harness ended the run: late",
        " not first: The harness ended the run: x",
        "the harness ended the run: lower case",
    ] {
        assert_eq!(
            DirectOutcome::StoppedOther.label_for(Some(other)),
            OTHER,
            "{other}"
        );
    }
}

#[test]
fn a_prefix_with_no_reason_reads_as_the_plain_harness_stop() {
    for d in [
        HARNESS_STOP_PREFIX.to_string(),
        format!("{HARNESS_STOP_PREFIX}   "),
        harness("..."),
    ] {
        assert_eq!(
            DirectOutcome::StoppedOther.label_for(Some(&d)),
            "Cline stopped: the harness ended the run",
            "{d:?}"
        );
    }
}

#[test]
fn a_reason_with_a_claim_word_or_a_long_line_or_control_characters_is_made_safe() {
    // a banned word in the reason: the label drops the reason
    for word in [
        "verified",
        "tested",
        "gated",
        "evaluated",
        "Tested",
        "VERIFIED",
    ] {
        let d = harness(&format!("the checks were {word} twice."));
        let label = DirectOutcome::StoppedOther.label_for(Some(&d));
        assert_eq!(label, "Cline stopped: the harness ended the run", "{word}");
        assert_eq!(banned_claim_word(&label), None);
    }
    // a long reason is cut, on one line
    let d = harness(&"word ".repeat(100));
    let label = DirectOutcome::StoppedOther.label_for(Some(&d));
    assert!(
        label.chars().count() <= "Cline stopped: the harness ended the run ()".len() + 120 + 1,
        "{label}"
    );
    assert!(
        label.contains("word word") && label.contains("…)"),
        "{label}"
    );
    assert!(!label.contains('\n'));
    // newlines, tabs and escape characters become single spaces or vanish
    let d = harness("two\nlines\tand\u{1b}[31m red");
    let label = DirectOutcome::StoppedOther.label_for(Some(&d));
    assert!(!label.chars().any(char::is_control), "{label:?}");
    assert!(
        label.starts_with("Cline stopped: the harness ended the run (two lines"),
        "{label}"
    );
}

#[test]
fn no_label_for_any_outcome_or_detail_uses_a_claim_word() {
    let details = [
        None,
        Some(harness("a round was verified.")),
        Some(harness("x")),
        Some("tested gated evaluated".to_string()),
    ];
    for o in DirectOutcome::ALL {
        for d in &details {
            let label = o.label_for(d.as_deref());
            assert_eq!(banned_claim_word(&label), None, "{o:?} {d:?} {label}");
        }
    }
}
