//! The review-round notice of a run names the engine that ran: an OpenCode run's app notice read "The harness asked Cline
//! once to review its own delivery" while its EVIDENCE.md named OpenCode. The text of the `review-round` progress event comes from the engine of the
//! run's plan, as the packager's sentence does.

#![allow(clippy::unwrap_used)]

use agentd::direct::driver::review_round_text;
use harness_protocol::direct::{DirectReview, REVIEW_RAN};

fn ran() -> DirectReview {
    DirectReview {
        status: REVIEW_RAN.to_string(),
        reason: None,
        commit_before: None,
        commit_after: None,
        changed_files: Some(false),
        seconds: Some(12),
        input_tokens: None,
        output_tokens: None,
        result: None,
    }
}

#[test]
fn b801_the_review_round_notice_of_an_opencode_run_names_opencode() {
    let text = review_round_text(&ran(), "opencode");
    assert!(
        text.starts_with("The harness asked OpenCode once to review its own delivery"),
        "{text}"
    );
    assert!(!text.contains("Cline"), "{text}");
}

#[test]
fn b801_the_review_round_notice_of_a_cline_run_is_the_sentence_it_was() {
    let note = ran();
    assert_eq!(review_round_text(&note, "cline"), note.sentence());
}

#[test]
fn b801_a_skipped_round_names_the_engine_of_the_run_too() {
    let note = DirectReview::skipped(harness_protocol::direct::REVIEW_SKIP_BLOCKED);
    let text = review_round_text(&note, "opencode");
    assert!(
        text.contains("OpenCode ended its message with a BLOCKED line"),
        "{text}"
    );
}

#[test]
fn b801_an_unknown_engine_name_reads_as_the_default_engine() {
    let note = ran();
    assert_eq!(review_round_text(&note, "not an engine!"), note.sentence());
}
