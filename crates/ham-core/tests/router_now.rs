//! `/now <text>` in the router: the interrupt message. The router hands the screen the word `now` with the payload as
//! its one argument, byte for byte after the single separating space, like `/send`; the screen asks for a second Enter before it
//! interrupts Cline (`crates/ham/tests/send_ui.rs`). A blank payload is a refusal that sends nothing.

#![allow(clippy::unwrap_used)]

use ham_core::router::{RouteContext, RouteOutcome, route};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn focus() -> RouteContext {
    RouteContext {
        focused_run: Some(RUN.into()),
        tainted: false,
    }
}

fn now_args(line: &str) -> Vec<String> {
    match route(line, &focus()) {
        RouteOutcome::Ui { word, args } => {
            assert_eq!(word, "now", "{line:?}");
            args
        }
        other => panic!("{line:?}: {other:?}"),
    }
}

#[test]
fn the_text_after_now_is_one_raw_argument() {
    assert_eq!(now_args("/now stop and do this"), ["stop and do this"]);
    // Quotes, backslashes and leading blanks beyond the separator are the user's text, not shell syntax.
    assert_eq!(
        now_args("/now \"quoted\" it's \\ -- --cancel"),
        ["\"quoted\" it's \\ -- --cancel"]
    );
    assert_eq!(now_args("/now   two"), ["  two"]);
    assert_eq!(now_args("/now a\nb"), ["a\nb"]);
    assert_eq!(now_args("ham now do it"), ["do it"]);
}

#[test]
fn now_without_text_sends_nothing() {
    for line in ["/now", "/now ", "/now \n"] {
        match route(line, &focus()) {
            RouteOutcome::Refused { reason, .. } => {
                assert!(reason.contains("/now needs the text"), "{line:?}: {reason}");
            }
            other => panic!("{line:?}: {other:?}"),
        }
    }
}
