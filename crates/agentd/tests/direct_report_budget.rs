//! The report's time budget follows the run's deadline: the run machine's task limit is 28,500 s and the
//! longest run is 27,900 s, so a report that may take its full budget after a run that used its whole deadline has no margin. The clock
//! here is the `time_left` the driver hands to the report step (the run's deadline minus now); the tests give it any value.
#![allow(clippy::unwrap_used)]

use agentd::direct::report::{DEADLINE_MARGIN, MIN_BUDGET, Settings, report_budget};
use std::time::Duration;

fn settings(budget_s: u64) -> Settings {
    Settings {
        enabled: true,
        budget: Duration::from_secs(budget_s),
        step: Duration::from_secs(300),
        tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
    }
}

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn the_margin_and_the_floor_are_the_owners_numbers() {
    assert_eq!(DEADLINE_MARGIN, s(180));
    assert_eq!(MIN_BUDGET, s(60));
}

#[test]
fn a_clock_100_seconds_before_the_deadline_skips_the_report() {
    assert_eq!(report_budget(&settings(480), s(100)), None);
    assert_eq!(report_budget(&settings(480), s(0)), None);
}

#[test]
fn the_floor_is_60_seconds_of_what_is_left_after_the_margin() {
    // 180 s margin + 59 s left: skipped. 180 + 60: a budget of 60 s.
    assert_eq!(report_budget(&settings(480), s(239)), None);
    assert_eq!(report_budget(&settings(480), s(240)), Some(s(60)));
}

#[test]
fn a_clock_1000_seconds_before_the_deadline_shortens_a_longer_budget_to_what_is_left() {
    // 1000 - 180 = 820 s left: a budget of 900 s is cut to 820 s.
    assert_eq!(report_budget(&settings(900), s(1000)), Some(s(820)));
}

#[test]
fn the_overall_budget_is_never_more_than_480_seconds_with_the_defaults() {
    assert_eq!(report_budget(&settings(480), s(1000)), Some(s(480)));
    assert_eq!(report_budget(&settings(480), s(27_900)), Some(s(480)));
    // Close to the deadline the budget is what is left, below the default.
    assert_eq!(report_budget(&settings(480), s(500)), Some(s(320)));
}
