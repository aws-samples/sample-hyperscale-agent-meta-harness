//! The retry of a blocked run as the client words it: the idempotency key of `POST /v1/runs/{runId}/retry`, the box title, the hint
//! and the confirmation, the notice after the start, the plain words for each refusal the route can give, the note that marks a linked run in
//! the Runs list, and the registry entry of `/retry`. The words never name the engine (they go through `engines::say`) and never carry a control
//! character from the service.

#![allow(clippy::unwrap_used)]

use ham_core::registry::{Availability, EffectClass, Effects, Kind, RunArg, TextArg, find, popup};
use ham_core::retry::{confirmation, explain, header, link_note, start_key, started};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const NEW: &str = "01M4ZHFBV5SDX7J92E6YRH4QTX";

// ---- the key --------------------------------------------------------------------------------------------------------------------

#[test]
fn the_key_is_retry_and_the_blocked_runs_id() {
    assert_eq!(start_key(RUN), format!("retry-{RUN}"));
    assert_eq!(
        start_key(RUN),
        start_key(RUN),
        "the same press, the same key"
    );
    assert_ne!(start_key(RUN), start_key(NEW));
    // The service takes 1 to 128 visible ASCII characters.
    let key = start_key(RUN);
    assert!((1..=128).contains(&key.len()));
    assert!(key.bytes().all(|b| (0x21..=0x7e).contains(&b)), "{key}");
}

// ---- the words ------------------------------------------------------------------------------------------------------------------

#[test]
fn the_header_names_the_run_by_its_first_eight_characters_and_says_the_run_is_billable() {
    assert_eq!(header(RUN), "Retry blocked run 01M4ZHFB: new billable run");
    assert_eq!(header("01K6"), "Retry blocked run 01K6: new billable run");
}

#[test]
fn the_confirmation_says_what_the_second_enter_starts() {
    let text = confirmation(RUN, Some("cc:calc"), false);
    assert!(text.contains(RUN), "{text}");
    assert!(text.contains("billable"), "{text}");
    assert!(text.contains("same request"), "{text}");
    assert!(text.contains("Target: cc:calc"), "{text}");
    assert!(
        text.contains("linked to run"),
        "the new run's link to this one is said: {text}"
    );
    assert!(!text.contains('{'), "no placeholder is left: {text}");
    assert!(
        !text.contains("stays in the box"),
        "no text in the box, nothing to keep: {text}"
    );
}

#[test]
fn the_confirmation_says_the_text_in_the_box_is_kept_and_not_sent() {
    let text = confirmation(RUN, Some("s3-only"), true);
    assert!(
        text.contains("Your text stays in the box and is not sent"),
        "{text}"
    );
    assert!(text.contains("Target: s3-only"), "{text}");
}

#[test]
fn the_confirmation_does_not_guess_a_target_it_does_not_know() {
    let text = confirmation(RUN, None, false);
    assert!(!text.contains("Target:"), "{text}");
    let blank = confirmation(RUN, Some("  "), false);
    assert!(!blank.contains("Target:"), "{blank}");
}

#[test]
fn the_confirmation_cleans_what_the_service_sent() {
    let text = confirmation("01M4\u{1b}[31mX", Some("cc:calc\u{7}"), false);
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
}

#[test]
fn the_notice_after_the_start_names_both_runs_and_the_kept_text() {
    let plain = started(RUN, NEW, false);
    assert!(plain.contains(RUN) && plain.contains(NEW), "{plain}");
    assert!(!plain.contains("your text"), "{plain}");
    let kept = started(RUN, NEW, true);
    assert!(kept.contains("your text is in the box"), "{kept}");
}

// ---- the refusals ---------------------------------------------------------------------------------------------------------------

#[test]
fn every_refusal_the_route_gives_has_plain_words() {
    // (status, code, the service's message, words the explanation must hold)
    let cases = [
        (404, "not-found", "no such route", "does not offer retry"),
        (
            404,
            "not-found",
            "no run X for this caller",
            "no run X for this caller",
        ),
        (
            409,
            "run-active",
            "run R has not ended; a retry starts a blocked run again",
            "has not ended",
        ),
        (
            409,
            "run-not-blocked",
            "run R is complete; only a blocked run can be retried",
            "only a blocked run",
        ),
        (
            409,
            "no-delivered-head",
            "run B delivered no code to continue from",
            "start a new run",
        ),
        (
            409,
            "head-mismatch",
            "run B delivered commit abc; the request named def",
            "start a new run",
        ),
        (
            422,
            "unknown-model",
            "model m is not offered",
            "start a new run",
        ),
        (
            422,
            "stored-request-unreadable",
            "the stored request of run R cannot be read",
            "cannot be read",
        ),
        (
            422,
            "idempotency-key-reused",
            "this Idempotency-Key was used",
            "start a new run",
        ),
        (
            503,
            "unavailable",
            "the harness is unavailable; retry",
            "press the key again",
        ),
        (0, "", "connection reset", "connection reset"),
    ];
    for (status, code, message, holds) in cases {
        let words = explain(status, code, message);
        assert!(words.contains(holds), "{status} {code}: {words}");
        assert!(!words.contains('\n'), "one line: {words:?}");
    }
}

#[test]
fn a_service_that_keeps_failing_points_to_a_new_run() {
    // A missing grant of the stage, or a stored request that is gone, both look like "unavailable" to the caller; the key can be pressed again
    // for a moment, and after that the person is told what else to do.
    for status in [500, 502, 503] {
        let words = explain(status, "unavailable", "the harness is unavailable; retry");
        assert!(words.contains("press the key again"), "{status}: {words}");
        assert!(words.contains("if it keeps failing"), "{status}: {words}");
        assert!(words.contains("start a new run"), "{status}: {words}");
    }
}

#[test]
fn an_unknown_refusal_is_told_with_the_services_own_words_cleaned() {
    let words = explain(418, "teapot", "short and\u{1b}[2J stout");
    assert!(words.contains("418") && words.contains("teapot"), "{words}");
    assert!(!words.contains('\u{1b}'), "{words:?}");
}

// ---- the Runs list ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_linked_run_is_marked_with_the_run_it_continues() {
    assert_eq!(link_note(RUN), format!("continues run {RUN}"));
    let dirty = link_note("01M4\u{1b}[31mX");
    assert!(!dirty.contains('\u{1b}'), "{dirty:?}");
}

// ---- the word /retry ------------------------------------------------------------------------------------------------------------

#[test]
fn retry_is_a_word_of_the_terminal_app_that_starts_a_billable_run() {
    let e = find("retry").expect("the registry has /retry");
    assert_eq!(
        e.kind,
        Kind::TuiOnly,
        "no argv form: the command line has `ham continue`"
    );
    assert!(matches!(e.status, Availability::Built));
    assert_eq!(e.effects, Effects::BILLABLE);
    assert_eq!(
        e.class(),
        EffectClass::ChangesRun,
        "it asks for a second Enter"
    );
    assert_eq!(
        (e.run_arg, e.text, e.lead),
        (RunArg::None, TextArg::None, 0)
    );
    assert!(!e.has_args(), "the popup runs it as it stands");
    assert!(e.help.len() <= 80, "{}", e.help);
    assert!(popup("ret").iter().any(|p| p.name == "retry"));
    assert!(popup("").iter().any(|p| p.name == "retry"));
}

/// The notice after a retry started on the run screen is one row of at most 80 columns, with the longest id the service makes and with
/// the text kept.
#[test]
fn b832_the_notice_after_a_retry_that_opened_on_screen_fits_the_row() {
    let long_id = "01M4ZHFBV5SDX7J92E6YRH4QTW";
    for kept in [false, true] {
        let line = ham_core::retry::started_here(long_id, kept);
        assert!(
            line.chars().count() <= 80,
            "{} columns: {line}",
            line.chars().count()
        );
        assert!(line.contains(long_id) && line.contains("started"), "{line}");
        assert_eq!(line.contains("your text is in the box"), kept, "{line}");
    }
    // An id with a control character is made safe before it is counted.
    let dirty = ham_core::retry::started_here("01M4\u{1b}[31mX", true);
    assert!(!dirty.contains('\u{1b}'), "{dirty:?}");
}
