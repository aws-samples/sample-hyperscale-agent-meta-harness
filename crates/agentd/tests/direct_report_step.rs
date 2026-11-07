//! The step limit of the clean-checkout test report can come from the request. The stage limit (`AGENTD_REPORT_STEP_S`, 300 s) and the
//! stage budget (`AGENTD_REPORT_BUDGET_S`, 480 s) stay the defaults. A limit in the request replaces the stage's step limit, lengthens the whole
//! budget by the same time (so the one long command has room to run) and asks the driver for a matching ceiling, never more than the run's
//! deadline allows. The pure rules are tested here; `direct_report_card.rs` runs the whole agent.
#![allow(clippy::unwrap_used)]

use agentd::direct::hooks::{Limits, report_ceiling};
use agentd::direct::report::{CEILING_GRACE, Settings, ceiling_for};
use std::time::Duration;

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn stage(budget: u64, step: u64) -> Settings {
    Settings {
        enabled: true,
        budget: s(budget),
        step: s(step),
        tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
    }
}

#[test]
fn without_a_limit_in_the_request_the_stage_settings_are_the_settings() {
    let st = stage(480, 300);
    assert_eq!(st.with_step_s(None), st);
}

#[test]
fn saying_the_default_limit_out_loud_changes_nothing() {
    let st = stage(480, 300);
    assert_eq!(st.with_step_s(Some(300)), st);
}

#[test]
fn a_longer_limit_lengthens_the_whole_budget_by_the_same_time() {
    let longer = stage(480, 300).with_step_s(Some(600));
    assert_eq!((longer.step, longer.budget), (s(600), s(780)));
    // The headroom of the stage over its own step limit is kept: 600 s of it here.
    let wide = stage(900, 300).with_step_s(Some(900));
    assert_eq!((wide.step, wide.budget), (s(900), s(1500)));
    let hour = stage(480, 300).with_step_s(Some(3600));
    assert_eq!((hour.step, hour.budget), (s(3600), s(3780)));
}

#[test]
fn a_shorter_limit_keeps_the_stage_budget() {
    let shorter = stage(480, 300).with_step_s(Some(60));
    assert_eq!((shorter.step, shorter.budget), (s(60), s(480)));
}

#[test]
fn a_stage_whose_step_limit_is_over_its_budget_does_not_underflow() {
    let odd = stage(100, 300).with_step_s(Some(400));
    assert_eq!((odd.step, odd.budget), (s(400), s(400)));
}

#[test]
fn a_limit_outside_one_second_to_the_session_bound_is_not_used() {
    let st = stage(480, 300);
    assert_eq!(st.with_step_s(Some(0)), st);
    assert_eq!(st.with_step_s(Some(27_901)), st);
}

#[test]
fn the_enabled_switch_of_the_stage_is_never_changed_by_a_limit() {
    let off = Settings {
        enabled: false,
        ..stage(480, 300)
    };
    assert!(!off.with_step_s(Some(600)).enabled);
}

#[test]
fn the_ceiling_the_module_asks_for_is_its_budget_and_a_grace_for_the_upload() {
    assert_eq!(CEILING_GRACE, s(30));
    assert_eq!(ceiling_for(&stage(480, 300).with_step_s(Some(600))), s(810));
}

#[test]
fn the_driver_keeps_its_own_ceiling_unless_a_module_asks_for_more() {
    let l = Limits::default();
    assert_eq!(l.report, s(480));
    // No ask, or an ask below the driver's own: the driver's 480 s.
    assert_eq!(report_ceiling(&l, None, s(27_000)), s(480));
    assert_eq!(report_ceiling(&l, Some(s(200)), s(27_000)), s(480));
    // An ask above it is granted while the run's deadline allows it.
    assert_eq!(report_ceiling(&l, Some(s(810)), s(27_000)), s(810));
    assert_eq!(report_ceiling(&l, Some(s(810)), s(600)), s(600));
    // Close to the deadline the driver's own ceiling is the floor, as it always was.
    assert_eq!(report_ceiling(&l, Some(s(810)), s(100)), s(480));
    assert_eq!(report_ceiling(&l, Some(s(810)), s(0)), s(480));
}
