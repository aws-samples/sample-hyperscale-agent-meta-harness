//! Wave 2, row C3: the test report is bounded by the run's own deadline, not by a one-hour ceiling. The step limit goes to 27,900 s (the
//! length of one slice), the stage budget variable accepts the same range, and the deadline rule still clamps the whole report.
#![allow(clippy::unwrap_used)]

use agentd::direct::report::{Settings, report_budget};
use harness_protocol::direct::{MAX_REPORT_STEP_S, report_step_s_ok};
use std::time::Duration;

fn vars(budget: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
    move |k: &str| match k {
        "AGENTD_REPORT_BUDGET_S" => budget.map(str::to_string),
        _ => None,
    }
}

#[test]
fn b776w_c3_a_stage_budget_above_an_hour_is_taken() {
    let s = Settings::from_vars(&vars(Some("7200")));
    assert_eq!(s.budget, Duration::from_secs(7200));
    let d = Settings::from_vars(&vars(None));
    assert_eq!(d.budget, Duration::from_secs(480));
    assert_eq!(d.step, Duration::from_secs(300));
}

#[test]
fn b776w_c3_a_run_step_limit_of_four_hours_lengthens_the_budget_by_the_same_time() {
    let s = Settings::from_vars(&vars(None)).with_step_s(Some(14_400));
    assert_eq!(s.step, Duration::from_secs(14_400));
    // The stage keeps 180 s of headroom over its step limit: 480 - 300.
    assert_eq!(s.budget, Duration::from_secs(14_400 + 180));
}

#[test]
fn b776w_c3_the_ceiling_is_the_slice_and_the_deadline_still_clamps_the_report() {
    assert_eq!(MAX_REPORT_STEP_S, 27_900);
    assert!(report_step_s_ok(27_900));
    assert!(!report_step_s_ok(27_901));
    assert!(!report_step_s_ok(0));
    let s = Settings::from_vars(&vars(None)).with_step_s(Some(27_900));
    // Plenty of time left: the whole budget.
    assert_eq!(
        report_budget(&s, Duration::from_secs(60_000)),
        Some(Duration::from_secs(27_900 + 180))
    );
    // Little time left: what remains after the 180 s margin.
    assert_eq!(
        report_budget(&s, Duration::from_secs(5_000)),
        Some(Duration::from_secs(5_000 - 180))
    );
    // Under the 60 s minimum after the margin the report is skipped.
    assert_eq!(report_budget(&s, Duration::from_secs(200)), None);
}
