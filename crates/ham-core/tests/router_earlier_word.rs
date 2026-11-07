// COMPAT-CAH: this file types the earlier command word `cah` into the message box on purpose.
//! The earlier command word in the message box: for the window `cah` starts a command attempt like `ham` does, and the rule
//! for the literal word `ham` is the one: an unknown word after `ham` is refused with the closest names and the hint to
//! use `/send`, a line that does not start at column zero of its first line is chat, and the repository-plus-`--direct` start form still works.
#![allow(clippy::unwrap_used)]

use ham_core::router::{RouteContext, RouteOutcome, is_command_attempt, route};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn focus() -> RouteContext {
    RouteContext {
        focused_run: Some(RUN.to_string()),
        tainted: false,
    }
}

fn none() -> RouteContext {
    RouteContext::default()
}

fn refused(o: &RouteOutcome) -> (&str, &[String]) {
    match o {
        RouteOutcome::Refused {
            reason,
            suggestions,
        } => (reason, suggestions),
        other => panic!("not refused: {other:?}"),
    }
}

fn message(o: &RouteOutcome) -> (&str, bool) {
    match o {
        RouteOutcome::Message { text, literal } => (text, *literal),
        other => panic!("not a message: {other:?}"),
    }
}

#[test]
fn the_earlier_word_is_a_command_attempt_next_to_ham() {
    for rest in [
        "models",
        "doctor",
        "config get region",
        "cancel 01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "rules show",
    ] {
        assert_eq!(
            route(&format!("cah {rest}"), &focus()),
            route(&format!("ham {rest}"), &focus()),
            "cah {rest}"
        );
    }
    assert_eq!(
        route("cah\tmodels", &focus()),
        route("ham\tmodels", &focus())
    );
    assert_eq!(
        route("cah   doctor", &focus()),
        route("ham   doctor", &focus())
    );
    let start = r#"cah "Add a test" --s3-only --direct"#;
    assert!(matches!(
        route(start, &focus()),
        RouteOutcome::StartForm { .. }
    ));
    assert_eq!(
        route(start, &focus()),
        route(&start.replacen("cah", "ham", 1), &focus())
    );
    assert!(is_command_attempt("cah push abc"));
    assert!(is_command_attempt("cah -x"));
    assert!(is_command_attempt("cah \"p\" --s3-only"));
}

#[test]
fn an_unknown_word_after_the_earlier_word_is_refused_like_one_after_ham() {
    let cah = route("cah radio logs are in /var", &none());
    let ham = route("ham radio logs are in /var", &none());
    let (reason, _) = refused(&cah);
    assert!(
        reason.contains("no command \"radio\"")
            && reason.contains("To send it as chat, use /send <text>"),
        "{reason}"
    );
    assert_eq!(cah, ham);
}

#[test]
fn text_that_only_starts_like_the_earlier_word_is_chat() {
    for text in [
        "cah",
        "cah ",
        "Cah push",
        "CAH push",
        "cahoots",
        "cahpush",
        "cah\u{a0}push",
        " cah push",
        "line one\ncah push",
        "cah, thanks",
    ] {
        assert_eq!(message(&route(text, &focus())), (text, false), "{text:?}");
        assert!(!is_command_attempt(text), "{text:?}");
    }
}

/// The rule for the literal word `ham` does not change with the window: "ham radio", a tab, column zero, text over several lines and the
/// repository-plus-`--direct` start form.
#[test]
fn the_rule_for_the_word_ham_is_the_one_of_decision_0178() {
    let o = route("ham radio logs are in /var", &none());
    let (reason, suggestions) = refused(&o);
    assert!(reason.contains("no command \"radio\""), "{reason}");
    assert!(
        reason.contains("To send it as chat, use /send <text>"),
        "{reason}"
    );
    assert!(suggestions.len() <= 3);
    assert!(is_command_attempt("ham\tmodels"));
    for chat in [
        " ham models",
        "\tham models",
        "first line\nham models",
        "ham",
        "ham ",
        "hammer",
        "hamlet is a play",
        "Ham models",
        "HAM models",
    ] {
        assert_eq!(message(&route(chat, &focus())), (chat, false), "{chat:?}");
        assert!(!is_command_attempt(chat), "{chat:?}");
    }
    // The first line decides: an attempt on the first line stays one when more lines follow, and then it is refused as an unknown word.
    assert!(matches!(
        route("ham radio\nsecond line", &none()),
        RouteOutcome::Refused { .. }
    ));
    // An unknown word on a line with exactly one repository option and `--direct` is a start form.
    assert!(matches!(
        route("ham radio --repo cc:hamh-e2e-demo --direct", &none()),
        RouteOutcome::StartForm { .. }
    ));
    let two = route(
        "ham radio --repo cc:hamh-e2e-demo --new-repo cc:hamh-e2e-new-20261031 --direct",
        &none(),
    );
    let (reason, _) = refused(&two);
    assert!(reason.contains("no command"), "{reason}");
}
