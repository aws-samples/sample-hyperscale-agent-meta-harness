//! The plain words of the "Cline needs you" card: the fourteen reason codes and `sanitize_block`, the
//! sanitizer for Cline's last message (at most 12 lines of 78 columns). The words describe what happened to the run and never claim
//! that the work is good or bad.
#![allow(clippy::unwrap_used)]

use ham_core::input::{ReasonWords, reason_words};
use ham_core::sanitize::sanitize_block;
use harness_protocol::direct::banned_claim_word;

const REASONS: [(&str, &str, &str); 14] = [
    (
        "engine_stop",
        "Cline stopped its own prompt",
        "Cline ended the prompt itself, twice. Its guard stops after 5 identical tool calls; its mistake limit stops after 6 failed turns. The provider is fine. After this stop, the same chat stops again after one identical call. A fresh session starts both counts from zero.",
    ),
    (
        "no_changes",
        "Cline ended without changing files",
        "Cline ended normally with a message, but no file differs from the start. Read its message: it may answer a question or give up on an impossible task.",
    ),
    (
        "no_message",
        "Cline sent an empty message",
        "Cline ended its turn with an empty message. You can send another prompt.",
    ),
    (
        "output_limit",
        "The model reached its output limit",
        "The model reached its maximum output length for one answer. You can send another prompt.",
    ),
    (
        "request_limit",
        "Cline reached its request limit",
        "Cline reached its model-request limit for one prompt. You can start a fresh session.",
    ),
    (
        "refused",
        "The answer was refused or blocked",
        "The model refused, or the provider's filter blocked the answer. You can change your prompt.",
    ),
    (
        "replay_rejected",
        "The provider rejected the saved conversation",
        "The harness already tried a fresh session once. More prompts in the same session fail the same way; choose a fresh session.",
    ),
    (
        "context_full",
        "The conversation exceeds the context window",
        "The conversation no longer fits the model's context window. The harness already tried a fresh session once; you can request another.",
    ),
    (
        "provider_error",
        "The model provider failed",
        "Retries and route switches could not fix the provider failure. You can finish with the existing work.",
    ),
    (
        "cap_hit",
        "Automatic recovery reached its prompt cap",
        "The harness stopped after 12 prompts in a row without a new tool call or changed file. You can supply another prompt.",
    ),
    (
        "engine_died",
        "The Cline process stopped",
        "The automatic fresh session did not help. Continuing is impossible; a fresh session starts a new process on the same files.",
    ),
    (
        "silence",
        "Cline stopped sending output",
        "Cline sent no output for 15 minutes, again after one resume. The process may be stuck; a fresh session starts a new process on the same files.",
    ),
    (
        "other_stop",
        "Cline gave an unknown stop reason",
        "The harness does not know Cline's stop reason. You can finish with the existing work.",
    ),
    (
        "provider_outage",
        "Every model route is down",
        "All routes have been down for 30 minutes. No answer is needed; probing continues until the time limit. `ham cancel` stops the run and delivers what exists.",
    ),
];

#[test]
fn every_reason_code_has_its_title_and_explanation() {
    for (code, title, explain) in REASONS {
        let w: ReasonWords = reason_words(code).unwrap_or_else(|| panic!("{code} has no words"));
        assert_eq!(ham_core::engines::say(w.title), title, "{code}");
        assert_eq!(ham_core::engines::say(w.explain), explain, "{code}");
    }
}

#[test]
fn an_unknown_code_has_no_words_so_the_event_title_is_shown() {
    assert_eq!(reason_words("dance"), None);
    assert_eq!(reason_words(""), None);
    // The codes are the wire words exactly: no other case or spelling.
    assert_eq!(reason_words("Engine_Stop"), None);
    assert_eq!(reason_words("engine-stop"), None);
}

#[test]
fn the_words_state_what_happened_and_never_judge_the_work() {
    for (code, title, explain) in REASONS {
        for text in [title, explain] {
            assert_eq!(banned_claim_word(text), None, "{code}: {text}");
        }
        assert!(!title.ends_with('.'), "{code}: titles carry no full stop");
        assert!(explain.ends_with('.'), "{code}: {explain}");
    }
}

#[test]
fn block_text_loses_escape_sequences_and_controls() {
    let lines = sanitize_block(
        "a\u{1b}]0;title\u{7}b\n\u{1b}[31mred\u{1b}[0m\u{202e}x",
        12,
        78,
    );
    assert_eq!(lines, vec!["ab".to_string(), "redx".to_string()]);
}

#[test]
fn block_text_keeps_its_line_breaks_and_trims_the_ends() {
    let lines = sanitize_block("\n  first line  \r\n\nthird\t line\n\n", 12, 78);
    assert_eq!(
        lines,
        vec![
            "first line".to_string(),
            String::new(),
            "third  line".to_string()
        ],
        "tabs become spaces, blank lines inside stay, leading and trailing blank lines go"
    );
    assert!(sanitize_block("", 12, 78).is_empty());
    assert!(sanitize_block(" \n\t\n ", 12, 78).is_empty());
}

#[test]
fn a_long_line_wraps_at_a_space_within_the_column_limit() {
    let lines = sanitize_block(
        "I will keep polling the build until it finishes, then append step 4 to PROGRESS.md and run the tests again.",
        12,
        40,
    );
    assert_eq!(
        lines,
        vec![
            "I will keep polling the build until it".to_string(),
            "finishes, then append step 4 to".to_string(),
            "PROGRESS.md and run the tests again.".to_string(),
        ]
    );
    assert!(lines.iter().all(|l| l.chars().count() <= 40));
}

#[test]
fn a_word_longer_than_the_limit_is_cut_at_the_limit() {
    let lines = sanitize_block(&"x".repeat(100), 12, 78);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].chars().count(), 78);
    assert_eq!(lines[1].chars().count(), 22);
}

#[test]
fn text_past_the_line_limit_ends_with_an_ellipsis_on_the_last_line() {
    let text: String = (1..=30).map(|n| format!("line {n}\n")).collect();
    let lines = sanitize_block(&text, 12, 78);
    assert_eq!(lines.len(), 12);
    assert_eq!(lines[0], "line 1");
    assert_eq!(lines[10], "line 11");
    assert_eq!(lines[11], "line 12…");
    assert!(lines.iter().all(|l| l.chars().count() <= 78));
    // Exactly at the limit: nothing is dropped, no ellipsis.
    let exact: String = (1..=12).map(|n| format!("line {n}\n")).collect();
    let lines = sanitize_block(&exact, 12, 78);
    assert_eq!(lines.len(), 12);
    assert_eq!(lines[11], "line 12");
}

#[test]
fn the_ellipsis_never_pushes_the_last_line_past_the_column_limit() {
    let text = format!("{}\nsecond\nthird", "y".repeat(78));
    let lines = sanitize_block(&text, 2, 78);
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|l| l.chars().count() <= 78), "{lines:?}");
    assert!(lines[1].ends_with('…'));
}

#[test]
fn zero_limits_give_nothing() {
    assert!(sanitize_block("text", 0, 78).is_empty());
    assert!(sanitize_block("text", 12, 0).is_empty());
}
