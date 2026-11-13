//! The stops Cline chooses itself: a prompt the engine ends `cancelled` when agentd wrote no `session/cancel`.
//! On Cline 3.0.71 that is its own brake, the loop guard (five identical consecutive tool calls) or the mistake limit (six failed turns in
//! a row); a really cut model stream never ends `cancelled`. The facts of
//! the attempt name the shape, and the shape is a hint for the explanation and for the one case worth a probe. Pure: no engine, no clock.
#![allow(clippy::unwrap_used)]

use agentd::direct::engine_stop::{
    AMBIGUOUS_UNDER, AttemptFacts, LOOP_GUARD_CALLS, MISTAKE_LIMIT_TURNS, Shape, StopReport,
    Verdict, describe, needs_probe, shape_of,
};
use std::time::Duration;

fn facts(completed: u64, failed_tail: u32, identical_tail: u32) -> AttemptFacts {
    AttemptFacts {
        tool_calls: completed + u64::from(failed_tail),
        completed,
        failed_tail,
        identical_tail,
    }
}

#[test]
fn the_numbers_are_clines_own() {
    assert_eq!(LOOP_GUARD_CALLS, 5, "the loop guard's hard threshold");
    assert_eq!(MISTAKE_LIMIT_TURNS, 6, "the consecutive-mistake limit");
    assert_eq!(AMBIGUOUS_UNDER, Duration::from_secs(120));
}

#[test]
fn five_identical_calls_at_the_tail_are_the_loop_guard() {
    assert_eq!(shape_of(&facts(40, 0, 5)), Shape::LoopGuard);
    assert_eq!(shape_of(&facts(40, 0, 9)), Shape::LoopGuard);
    assert_eq!(
        shape_of(&facts(40, 0, 4)),
        Shape::Unexplained,
        "Cline's soft warning is at three; four is not its stop"
    );
}

#[test]
fn six_failed_calls_at_the_tail_are_the_mistake_limit() {
    assert_eq!(shape_of(&facts(0, 6, 1)), Shape::MistakeLimit);
    assert_eq!(shape_of(&facts(3, 7, 1)), Shape::MistakeLimit);
    assert_eq!(shape_of(&facts(3, 5, 1)), Shape::Unexplained);
}

#[test]
fn the_loop_guard_is_read_first_when_both_tails_are_long() {
    assert_eq!(shape_of(&facts(0, 6, 6)), Shape::LoopGuard);
}

#[test]
fn nothing_at_the_tail_is_unexplained() {
    assert_eq!(shape_of(&facts(0, 0, 0)), Shape::Unexplained);
    assert_eq!(shape_of(&AttemptFacts::default()), Shape::Unexplained);
}

#[test]
fn only_an_unexplained_stop_with_no_completed_call_in_under_two_minutes_is_worth_a_probe() {
    let quick = Duration::from_secs(119);
    assert!(needs_probe(&facts(0, 0, 0), quick));
    assert!(needs_probe(&facts(0, 2, 2), Duration::ZERO));
    // The attempt got work done, or ran two minutes or more, or has a recognised shape: the provider is not under suspicion.
    assert!(!needs_probe(&facts(1, 0, 0), quick), "a completed call");
    assert!(!needs_probe(&facts(0, 0, 0), Duration::from_secs(120)));
    assert!(!needs_probe(&facts(0, 0, 0), Duration::from_secs(600)));
    assert!(!needs_probe(&facts(0, 0, 5), quick), "loop guard shape");
    assert!(!needs_probe(&facts(0, 6, 1), quick), "mistake limit shape");
}

#[test]
fn the_words_of_a_shape_are_stable_and_plain() {
    assert_eq!(Shape::LoopGuard.word(), "loop-guard");
    assert_eq!(Shape::MistakeLimit.word(), "mistake-limit");
    assert_eq!(Shape::Unexplained.word(), "unexplained");
    let loop_guard = describe(Shape::LoopGuard, &facts(40, 0, 7));
    assert!(
        loop_guard.contains("7 identical tool calls in a row"),
        "{loop_guard}"
    );
    let mistakes = describe(Shape::MistakeLimit, &facts(0, 6, 1));
    assert!(
        mistakes.contains("6 failed tool calls in a row"),
        "{mistakes}"
    );
    let none = describe(Shape::Unexplained, &facts(2, 0, 1));
    assert!(none.contains("2 completed tool calls"), "{none}");
    for text in [&loop_guard, &mistakes, &none] {
        assert!(!text.contains('\n') && text.chars().count() < 160, "{text}");
    }
}

// ---- the report: the one object a later reader of the stop takes ----------------------------------------

#[test]
fn a_report_carries_the_shape_the_facts_and_the_time_of_the_attempt() {
    let r = StopReport::new(facts(40, 0, 5), Duration::from_secs(180));
    assert_eq!(r.shape, Shape::LoopGuard);
    assert_eq!(r.facts, facts(40, 0, 5));
    assert_eq!(r.ran, Duration::from_secs(180));
    assert_eq!(r.hint(), "loop-guard");
    assert!(r.describe().contains("5 identical tool calls in a row"));
    assert!(!r.needs_probe());
}

#[test]
fn only_a_route_that_does_not_answer_makes_a_quick_unexplained_stop_a_provider_failure() {
    let quick = StopReport::new(facts(0, 0, 0), Duration::from_secs(3));
    assert!(quick.needs_probe());
    assert_eq!(
        quick.verdict(None),
        Verdict::EngineStop,
        "no probe was possible"
    );
    assert_eq!(
        quick.verdict(Some(false)),
        Verdict::EngineStop,
        "the route answered"
    );
    assert_eq!(quick.verdict(Some(true)), Verdict::ProviderFailure);
}

#[test]
fn a_probe_never_moves_a_stop_that_has_a_shape_or_made_progress_or_ran_long() {
    for (name, report) in [
        (
            "loop guard",
            StopReport::new(facts(0, 0, 5), Duration::from_secs(3)),
        ),
        (
            "mistake limit",
            StopReport::new(facts(0, 6, 1), Duration::from_secs(3)),
        ),
        (
            "progress",
            StopReport::new(facts(2, 0, 1), Duration::from_secs(3)),
        ),
        (
            "two minutes",
            StopReport::new(facts(0, 0, 0), Duration::from_secs(120)),
        ),
    ] {
        assert!(!report.needs_probe(), "{name}");
        assert_eq!(report.verdict(Some(true)), Verdict::EngineStop, "{name}");
    }
}
