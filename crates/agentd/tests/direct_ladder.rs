//! The failover ladder: the pure decisions of what to do after a prompt failed, without any engine, clock or
//! network. It answers "retry here", "switch to that route", "every route is down: wait" or "stop", from the failure class, the budgets
//! of the route and the history of the pass. The loop that carries the decisions out is covered by `direct_failover.rs`.
#![allow(clippy::unwrap_used)]

use agentd::acp::AcpError;
use agentd::direct::failover::{
    FRUITLESS_PROMPTS_MAX, Ladder, LadderPolicy, Next, Route, StopWhy, same_model,
};
use agentd::direct::retry::{RetryClass, RetryPolicy};
use harness_protocol::direct::DirectSwitchKind;
use std::time::{Duration, Instant};

const OPUS_US: &str = "us.anthropic.claude-opus-5-5";
const OPUS_GLOBAL: &str = "global.anthropic.claude-opus-5-5";
const SONNET_GLOBAL: &str = "global.anthropic.claude-sonnet-5-5";
const SONNET_US: &str = "us.anthropic.claude-sonnet-5-5";
const SOL_US: &str = "us.openai.gpt-6.1-sol";

fn route(id: &str) -> Route {
    Route {
        id: id.into(),
        region: "us-east-1".into(),
        context_tokens: 1_000_000,
        max_output_tokens: 128_000,
    }
}

fn routes(ids: &[&str]) -> Vec<Route> {
    ids.iter().map(|i| route(i)).collect()
}

fn five() -> Vec<Route> {
    routes(&[OPUS_US, OPUS_GLOBAL, SONNET_GLOBAL, SONNET_US, SOL_US])
}

/// 3 resumes on the preferred route, 1 on a fallback, a 120 s dwell before the return trial.
fn ladder(r: Vec<Route>) -> Ladder {
    Ladder::new(
        r,
        LadderPolicy::new(RetryPolicy::new(3, Duration::from_secs(5))),
    )
}

fn boom() -> AcpError {
    AcpError::Closed
}

fn at(base: Instant, secs: u64) -> Instant {
    base + Duration::from_secs(secs)
}

#[test]
fn the_same_model_is_told_from_the_other_by_its_base_id() {
    assert!(same_model(OPUS_US, OPUS_GLOBAL));
    assert!(same_model(SONNET_GLOBAL, SONNET_US));
    assert!(same_model("anthropic.claude-opus-5-5", OPUS_US));
    assert!(!same_model(OPUS_US, SONNET_US));
    assert!(!same_model(SOL_US, "us.openai.gpt-6-astra"));
    assert!(same_model("us.zai.glm-5.3", "global.zai.glm-5.3"));
    assert!(same_model("aws-mantle/zai.glm-5", "zai.glm-5"));
}

#[test]
fn the_preferred_route_keeps_the_three_resumes_of_the_first_slice() {
    let t = Instant::now();
    let mut l = ladder(five());
    for (n, secs) in [(1u32, 5u64), (2, 10), (3, 20)] {
        match l.failed(RetryClass::Transient, &boom(), t) {
            Next::Retry { wait } => assert_eq!(wait, Duration::from_secs(secs), "retry {n}"),
            other => panic!("retry {n}: {other:?}"),
        }
    }
    // The fourth failure spends the budget: the other profile of the same model is next.
    assert_eq!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::Switch {
            to: 1,
            kind: DirectSwitchKind::Route
        }
    );
}

#[test]
fn a_permanent_failure_gets_one_resume_on_the_preferred_route_and_none_on_a_fallback() {
    let t = Instant::now();
    let mut l = ladder(five());
    assert!(matches!(
        l.failed(RetryClass::Permanent, &boom(), t),
        Next::Retry { .. }
    ));
    assert_eq!(
        l.failed(RetryClass::Permanent, &boom(), t),
        Next::Switch {
            to: 1,
            kind: DirectSwitchKind::Route
        }
    );
    l.switched(1);
    // On the fallback the first failure after the switch is also the last: the route is not worth a second try.
    assert_eq!(
        l.failed(RetryClass::Permanent, &boom(), t),
        Next::Switch {
            to: 2,
            kind: DirectSwitchKind::Model
        }
    );
}

#[test]
fn a_fallback_route_gets_one_resume_for_a_transient_failure() {
    let t = Instant::now();
    let mut l = ladder(five());
    for _ in 0..3 {
        let _ = l.failed(RetryClass::Transient, &boom(), t);
    }
    let _ = l.failed(RetryClass::Transient, &boom(), t);
    l.switched(1);
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::Retry { .. }
    ));
    assert_eq!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::Switch {
            to: 2,
            kind: DirectSwitchKind::Model
        }
    );
}

/// Carries a decision out the way the loop does: a switch is confirmed by the engine at once.
fn drive(l: &mut Ladder, class: RetryClass, now: Instant) -> Next {
    let next = l.failed(class, &boom(), now);
    if let Next::Switch { to, .. } = &next {
        l.switched(*to);
    }
    next
}

#[test]
fn a_route_the_provider_refuses_is_left_at_once_and_never_visited_again() {
    let t = Instant::now();
    let mut l = ladder(five());
    assert_eq!(
        drive(&mut l, RetryClass::Unavailable, t),
        Next::Switch {
            to: 1,
            kind: DirectSwitchKind::Route
        }
    );
    // Every other route fails too; the refused preferred route is not in the pass.
    let mut visited = vec![1];
    loop {
        match drive(&mut l, RetryClass::Transient, t) {
            Next::Retry { .. } => {}
            Next::Switch { to, .. } => visited.push(to),
            Next::AllDown { .. } => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(visited, vec![1, 2, 3, 4]);
    assert!(l.excluded(0));
    // The next pass begins where the caller says, and the refused route stays out of it.
    assert_eq!(
        l.begin_pass(1, at(t, 400)),
        Some((1, DirectSwitchKind::Model))
    );
    assert!(l.excluded(0));
}

#[test]
fn a_failure_the_work_causes_stops_the_ladder() {
    let mut l = ladder(five());
    assert_eq!(
        l.failed(RetryClass::Never, &boom(), Instant::now()),
        Next::Stop(StopWhy::Never)
    );
}

#[test]
fn a_run_with_one_route_stops_when_its_budget_is_spent_as_in_the_first_slice() {
    let t = Instant::now();
    let mut l = ladder(routes(&[OPUS_US]));
    for _ in 0..3 {
        assert!(matches!(
            l.failed(RetryClass::Transient, &boom(), t),
            Next::Retry { .. }
        ));
    }
    assert_eq!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::Stop(StopWhy::Exhausted)
    );
}

#[test]
fn when_every_route_is_spent_the_ladder_waits_30_s_and_grows_by_half_to_300_s() {
    let t = Instant::now();
    let mut l = ladder(routes(&[OPUS_US, SOL_US]));
    // Spend the preferred route (4 failures) and the fallback (2).
    for _ in 0..3 {
        let _ = l.failed(RetryClass::Transient, &boom(), t);
    }
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::Switch { to: 1, .. }
    ));
    l.switched(1);
    let _ = l.failed(RetryClass::Transient, &boom(), t);
    let mut waits = Vec::new();
    match l.failed(RetryClass::Transient, &boom(), t) {
        Next::AllDown { wait, pass } => {
            assert_eq!(pass, 1);
            waits.push(wait.as_secs_f64());
        }
        other => panic!("{other:?}"),
    }
    // Each further outage wait comes from the schedule.
    for _ in 0..8 {
        waits.push(l.next_wait().as_secs_f64());
    }
    assert_eq!(
        waits,
        vec![
            30.0, 45.0, 67.5, 101.25, 151.875, 227.8125, 300.0, 300.0, 300.0
        ]
    );
}

#[test]
fn a_new_pass_starts_clean_on_the_route_the_caller_picked() {
    let t = Instant::now();
    let mut l = ladder(routes(&[OPUS_US, SOL_US]));
    for _ in 0..3 {
        let _ = l.failed(RetryClass::Transient, &boom(), t);
    }
    let _ = l.failed(RetryClass::Transient, &boom(), t);
    l.switched(1);
    let _ = l.failed(RetryClass::Transient, &boom(), t);
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::AllDown { .. }
    ));
    // The probe found the fallback healthy: the pass begins there, with a fresh budget, and the preferred route is the next one.
    assert_eq!(l.begin_pass(1, at(t, 60)), None);
    assert_eq!(l.pass(), 1);
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), at(t, 61)),
        Next::Retry { .. }
    ));
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), at(t, 62)),
        Next::Switch { to: 0, .. }
    ));
}

#[test]
fn beginning_a_pass_on_another_route_asks_for_a_switch() {
    let t = Instant::now();
    let mut l = ladder(routes(&[OPUS_US, SOL_US]));
    assert_eq!(
        l.begin_pass(1, t),
        Some((1, DirectSwitchKind::Model)),
        "the engine is on route 0 and must move"
    );
    l.switched(1);
    assert_eq!(l.begin_pass(0, t), Some((0, DirectSwitchKind::Return)));
}

#[test]
fn an_excluded_preferred_route_gets_no_return_trial() {
    let t = Instant::now();
    let mut l = ladder(five());
    // The preferred route fails at t0 and is left; the run settles on the global profile.
    let _ = l.failed(RetryClass::Unavailable, &boom(), t);
    l.switched(1);
    // 30 s later the global profile fails: the dwell of 120 s has not passed, the cursor moves on.
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), at(t, 30)),
        Next::Retry { .. }
    ));
    // The preferred route is excluded (refused), so no return trial can happen at all.
    let next = l.failed(RetryClass::Transient, &boom(), at(t, 200));
    assert_eq!(
        next,
        Next::Switch {
            to: 2,
            kind: DirectSwitchKind::Model
        }
    );
}

#[test]
fn a_transient_failure_of_the_preferred_route_is_retried_after_the_dwell_once_per_pass() {
    let t = Instant::now();
    let mut l = ladder(five());
    for _ in 0..3 {
        let _ = l.failed(RetryClass::Transient, &boom(), t);
    }
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::Switch { to: 1, .. }
    ));
    l.switched(1);
    let _ = l.failed(RetryClass::Transient, &boom(), at(t, 10));
    // Route 1 is spent at t+60: less than 120 s since the preferred route failed, so the cursor goes on to route 2.
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), at(t, 60)),
        Next::Switch { to: 2, .. }
    ));
    l.switched(2);
    let _ = l.failed(RetryClass::Transient, &boom(), at(t, 100));
    // Route 2 is spent at t+130: the dwell has passed, the preferred route gets one more try.
    assert_eq!(
        l.failed(RetryClass::Transient, &boom(), at(t, 130)),
        Next::Switch {
            to: 0,
            kind: DirectSwitchKind::Return
        }
    );
    l.switched(0);
    // A return trial has no resumes: one submission, then the cursor continues at route 3, not at route 1.
    assert_eq!(
        l.failed(RetryClass::Transient, &boom(), at(t, 140)),
        Next::Switch {
            to: 3,
            kind: DirectSwitchKind::Model
        }
    );
    l.switched(3);
    let _ = l.failed(RetryClass::Transient, &boom(), at(t, 200));
    // Route 3 is spent at t+300: the preferred route was already tried again in this pass.
    assert_eq!(
        l.failed(RetryClass::Transient, &boom(), at(t, 300)),
        Next::Switch {
            to: 4,
            kind: DirectSwitchKind::Model
        }
    );
}

#[test]
fn a_permanent_failure_right_after_a_switch_excludes_that_route_for_the_session() {
    let t = Instant::now();
    let mut l = ladder(five());
    let _ = l.failed(RetryClass::Unavailable, &boom(), t);
    l.switched(1);
    // The first submission on a fresh route is refused: the history or the settings do not suit it.
    assert_eq!(
        l.failed(RetryClass::Permanent, &boom(), t),
        Next::Switch {
            to: 2,
            kind: DirectSwitchKind::Model
        }
    );
    assert!(l.excluded(1));
    assert!(!l.excluded(2));
}

#[test]
fn a_switch_the_engine_refuses_excludes_the_route_and_picks_another() {
    let t = Instant::now();
    let mut l = ladder(five());
    for _ in 0..4 {
        let _ = l.failed(RetryClass::Transient, &boom(), t);
    }
    assert_eq!(
        l.refused(1, t),
        Next::Switch {
            to: 2,
            kind: DirectSwitchKind::Model
        }
    );
    assert!(l.excluded(1));
}

#[test]
fn when_every_route_is_excluded_nothing_remains() {
    let t = Instant::now();
    let mut l = ladder(routes(&[OPUS_US, OPUS_GLOBAL]));
    assert!(matches!(
        l.failed(RetryClass::Unavailable, &boom(), t),
        Next::Switch { to: 1, .. }
    ));
    l.switched(1);
    assert_eq!(
        l.failed(RetryClass::Unavailable, &boom(), t),
        Next::Stop(StopWhy::NoRouteRemains)
    );
}

#[test]
fn the_effective_limits_are_the_smallest_of_the_routes() {
    let mut r = five();
    r[4].max_output_tokens = 131_072;
    r[2].context_tokens = 1_050_000;
    r[3].context_tokens = 900_000;
    let (out, ctx) = agentd::direct::failover::effective_limits(&r);
    assert_eq!((out, ctx), (128_000, 900_000));
    let one = vec![route(SOL_US)];
    assert_eq!(
        agentd::direct::failover::effective_limits(&one),
        (128_000, 1_000_000)
    );
}

#[test]
fn the_switch_names_the_failure_that_caused_it() {
    let t = Instant::now();
    let mut l = ladder(five());
    let _ = l.failed(RetryClass::Unavailable, &boom(), t);
    assert_eq!(l.last_class(), "unavailable");
    let mut l = ladder(five());
    let _ = l.failed(RetryClass::Permanent, &boom(), t);
    assert_eq!(l.last_class(), "permanent");
    let mut l = ladder(five());
    let _ = l.failed(RetryClass::Transient, &boom(), t);
    assert_eq!(l.last_class(), "transient");
}

// ---- Cline's own stops stay out of the ladder ------------------------------------------------------------------------

#[test]
fn an_engine_stop_is_never_answered_with_a_switch_a_resume_or_an_exclusion() {
    let t = Instant::now();
    let mut l = ladder(five());
    assert_eq!(
        l.failed(RetryClass::EngineStop, &boom(), t),
        Next::Stop(StopWhy::Never)
    );
    assert!(!l.excluded(0));
    assert_eq!(l.current(), 0);
    assert_eq!(RetryClass::EngineStop.as_str(), "engine_stop");
    assert_eq!(
        RetryPolicy::new(3, Duration::from_secs(5)).allowed(RetryClass::EngineStop),
        0
    );
}

#[test]
fn an_engine_stop_on_a_fresh_fallback_route_does_not_make_the_next_permanent_failure_exclude_it() {
    let t = Instant::now();
    let mut l = ladder(five());
    let _ = l.failed(RetryClass::Unavailable, &boom(), t);
    l.switched(1);
    // The route ran a prompt that Cline stopped itself; the route answered, so a later refusal is not "the first submission refused".
    l.engine_stopped();
    let _ = l.failed(RetryClass::Permanent, &boom(), t);
    assert!(!l.excluded(1), "a deliberate stop proved the route works");
}

// ---- the sanity bound on switches: fruitless switches, then hold -----------------------------------------------------------------------

/// A ladder that has switched once and whose first attempt there just ended.
fn after_switch(r: Vec<Route>, to: usize) -> Ladder {
    let mut l = ladder(r);
    l.switched(to);
    l
}

#[test]
fn a_switch_whose_first_attempt_fails_fast_with_no_completed_call_is_fruitless() {
    let mut l = after_switch(five(), 1);
    assert_eq!(l.fruitless(), 0);
    l.attempt_ended(0, Duration::from_secs(60));
    assert_eq!(l.fruitless(), 1, "60 s is within the minute");
    assert!(!l.holding());
}

#[test]
fn a_slow_failure_or_one_with_a_completed_call_is_not_fruitless() {
    let mut l = after_switch(five(), 1);
    l.attempt_ended(0, Duration::from_secs(61));
    assert_eq!(l.fruitless(), 0, "the route held on for more than a minute");
    l.switched(2);
    l.attempt_ended(1, Duration::from_secs(5));
    assert_eq!(l.fruitless(), 0, "the route did a call");
}

#[test]
fn only_the_first_attempt_after_a_switch_counts() {
    let mut l = after_switch(five(), 1);
    l.attempt_ended(0, Duration::from_secs(1));
    l.attempt_ended(0, Duration::from_secs(1));
    l.attempt_ended(0, Duration::from_secs(1));
    assert_eq!(
        l.fruitless(),
        1,
        "the resumes on the route are not new switches"
    );
}

#[test]
fn an_attempt_that_completed_a_call_resets_the_count_even_without_a_switch() {
    let mut l = ladder(five());
    l.switched(1);
    l.attempt_ended(0, Duration::from_secs(1));
    l.switched(2);
    l.attempt_ended(0, Duration::from_secs(1));
    assert_eq!(l.fruitless(), 2);
    l.attempt_ended(3, Duration::from_secs(90));
    assert_eq!(l.fruitless(), 0);
}

#[test]
fn an_engine_stop_is_not_a_fruitless_switch() {
    let mut l = after_switch(five(), 1);
    l.engine_stopped();
    l.attempt_ended(0, Duration::from_secs(1));
    assert_eq!(
        l.fruitless(),
        0,
        "the first attempt after the switch was Cline's own stop, already accounted for"
    );
}

#[test]
fn as_many_fruitless_switches_as_usable_routes_put_the_ladder_on_hold() {
    let mut l = ladder(five());
    for k in 1..=5usize {
        l.switched(k % 5);
        l.attempt_ended(0, Duration::from_secs(1));
        assert_eq!(l.holding(), k == 5, "after {k} fruitless switches");
    }
}

#[test]
fn the_limit_is_the_number_of_usable_routes_and_never_more_than_nine() {
    let twelve: Vec<Route> = (0..12)
        .map(|i| route(&format!("us.vendor{i}.model-{i}")))
        .collect();
    let mut l = ladder(twelve);
    for k in 1..=9usize {
        l.switched(k);
        l.attempt_ended(0, Duration::from_secs(1));
        assert_eq!(l.holding(), k == 9, "after {k}");
    }
    // Routes the provider refused are not usable: three routes left, three fruitless switches.
    let mut l = ladder(five());
    l.exclude(3);
    l.exclude(4);
    for k in 1..=3usize {
        l.switched(k % 3);
        l.attempt_ended(0, Duration::from_secs(1));
        assert_eq!(l.holding(), k == 3, "after {k} with three usable routes");
    }
}

#[test]
fn a_ladder_on_hold_does_not_switch_or_resume_it_waits_and_probes() {
    let t = Instant::now();
    let mut l = ladder(five());
    l.force_hold();
    assert!(l.holding());
    // A ladder not on hold would resume three times on the preferred route and then switch to the other profile. On hold the first failure
    // goes straight to the wait: one prompt per healthy probe, so a provider whose probes pass while its prompts fail is not fed history.
    match l.failed(RetryClass::Transient, &boom(), t) {
        Next::AllDown { wait, pass } => {
            assert_eq!(wait, Duration::from_secs(30));
            assert_eq!(pass, 1);
        }
        other => panic!("{other:?}"),
    }
    match l.failed(RetryClass::Transient, &boom(), t) {
        Next::AllDown { wait, pass } => {
            assert_eq!(wait, Duration::from_millis(45_000));
            assert_eq!(pass, 2);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn on_hold_the_probe_asks_for_the_preferred_route_and_nothing_else() {
    let mut l = ladder(five());
    assert_eq!(
        l.probe_targets(),
        [0, 1, 2, 3, 4],
        "not on hold: every usable route"
    );
    l.force_hold();
    assert_eq!(l.probe_targets(), [0]);
    l.exclude(0);
    assert_eq!(
        l.probe_targets(),
        [1, 2, 3, 4],
        "the preferred route is refused: the hold has nothing to wait for there"
    );
}

#[test]
fn a_new_pass_keeps_the_count_and_only_a_completed_call_releases_the_hold() {
    let t = Instant::now();
    let mut l = ladder(five());
    for k in 1..=5usize {
        l.switched(k % 5);
        l.attempt_ended(0, Duration::from_secs(1));
    }
    assert!(l.holding());
    // A probe answered and a pass begins again: the count is not cleared by a pass, or a flapping provider would earn nine switches a pass.
    let _ = l.begin_pass(0, t);
    assert!(l.holding());
    assert_eq!(l.fruitless(), 5);
    // The run did real work again: the circuit closes.
    l.attempt_ended(2, Duration::from_secs(40));
    assert!(!l.holding());
    assert_eq!(l.fruitless(), 0);
}

#[test]
fn a_hold_is_not_a_stop_when_a_route_is_still_usable() {
    let t = Instant::now();
    let mut l = ladder(routes(&[OPUS_US, OPUS_GLOBAL]));
    l.force_hold();
    // Only an exclusion of every route stops the ladder.
    l.exclude(1);
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::AllDown { .. }
    ));
    l.exclude(0);
    assert_eq!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::Stop(StopWhy::NoRouteRemains)
    );
}

// ---- the bound on prompts that achieve nothing ---------------------------------------------------

#[test]
fn twelve_prompts_that_completed_no_tool_call_put_the_ladder_on_hold() {
    let mut l = ladder(five());
    for k in 1..=12u32 {
        // Whatever the prompt took: a timeout of 6 minutes counts like a quick refusal.
        l.attempt_ended(0, Duration::from_secs(if k % 2 == 0 { 400 } else { 2 }));
        assert_eq!(l.fruitless_prompts(), k);
        assert_eq!(l.holding(), k == 12, "after {k} prompts");
    }
    assert_eq!(FRUITLESS_PROMPTS_MAX, 12);
}

#[test]
fn a_completed_tool_call_clears_the_prompt_count_and_the_hold() {
    let mut l = ladder(five());
    for _ in 0..11 {
        l.attempt_ended(0, Duration::from_secs(1));
    }
    l.attempt_ended(1, Duration::from_secs(90));
    assert_eq!(l.fruitless_prompts(), 0);
    for _ in 0..12 {
        l.attempt_ended(0, Duration::from_secs(1));
    }
    assert!(l.holding());
    l.attempt_ended(3, Duration::from_secs(300));
    assert!(!l.holding());
    assert_eq!(l.fruitless_prompts(), 0);
}

#[test]
fn an_engine_stop_is_not_a_fruitless_prompt() {
    let mut l = ladder(five());
    for _ in 0..30 {
        l.engine_stopped();
    }
    assert_eq!(
        l.fruitless_prompts(),
        0,
        "the engine's own stop never reaches attempt_ended"
    );
}

// ---- the probe before a switch -------------------------------------------------------------------------------------------------------

#[test]
fn a_target_that_did_not_answer_its_probe_is_used_up_for_the_pass_and_the_next_is_picked() {
    let t = Instant::now();
    let mut l = ladder(five());
    for _ in 0..3 {
        let _ = l.failed(RetryClass::Transient, &boom(), t);
    }
    let first = l.failed(RetryClass::Transient, &boom(), t);
    assert_eq!(
        first,
        Next::Switch {
            to: 1,
            kind: DirectSwitchKind::Route
        }
    );
    // The caller probed route 1: down. It is not switched to, not excluded, and the pass goes on.
    assert_eq!(
        l.unanswered(1, t),
        Next::Switch {
            to: 2,
            kind: DirectSwitchKind::Model
        }
    );
    assert!(!l.excluded(1));
    assert_eq!(l.current(), 0, "the engine never moved");
}

#[test]
fn when_every_target_is_unanswered_the_ladder_waits() {
    let t = Instant::now();
    let mut l = ladder(routes(&[OPUS_US, OPUS_GLOBAL, SONNET_GLOBAL]));
    for _ in 0..3 {
        let _ = l.failed(RetryClass::Transient, &boom(), t);
    }
    let _ = l.failed(RetryClass::Transient, &boom(), t);
    let next = l.unanswered(1, t);
    assert!(matches!(next, Next::Switch { to: 2, .. }), "{next:?}");
    match l.unanswered(2, t) {
        Next::AllDown { wait, pass } => assert_eq!((wait, pass), (Duration::from_secs(30), 1)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unanswered_return_trial_starts_the_dwell_again() {
    let t = Instant::now();
    let mut l = ladder(routes(&[OPUS_US, OPUS_GLOBAL, SONNET_GLOBAL]));
    for _ in 0..4 {
        let _ = l.failed(RetryClass::Transient, &boom(), t);
    }
    l.switched(1);
    let _ = l.failed(RetryClass::Transient, &boom(), at(t, 130));
    // A resume was used on route 1; the next failure after the dwell sends the run back to the user's route.
    let back = l.failed(RetryClass::Transient, &boom(), at(t, 131));
    assert_eq!(
        back,
        Next::Switch {
            to: 0,
            kind: DirectSwitchKind::Return
        }
    );
    let next = l.unanswered(0, at(t, 131));
    assert_eq!(
        next,
        Next::Switch {
            to: 2,
            kind: DirectSwitchKind::Model
        }
    );
    assert!(!l.excluded(0));
}

// ---- a ladder that starts where the engine is -----------------------------------------------------------

#[test]
fn a_ladder_started_on_a_route_begins_there_with_the_budgets_of_a_fallback() {
    let t = Instant::now();
    let mut l = Ladder::new_at(
        five(),
        LadderPolicy::new(RetryPolicy::new(3, Duration::from_secs(5))),
        2,
    );
    assert_eq!(l.current(), 2);
    // A fallback route: one resume after a transient failure, none after a permanent one, and a permanent failure is not "the first
    // submission after a switch" (the engine was already there), so the route is not excluded.
    assert!(matches!(
        l.failed(RetryClass::Transient, &boom(), t),
        Next::Retry { .. }
    ));
    let next = l.failed(RetryClass::Permanent, &boom(), t);
    assert!(matches!(next, Next::Switch { .. }), "{next:?}");
    assert!(!l.excluded(2));
    // The user's route was not used in this ladder's pass: it is an ordinary candidate, and no return trial is due without a failure of its own.
    match next {
        Next::Switch { to, kind } => {
            assert_eq!(to, 3);
            assert_eq!(kind, DirectSwitchKind::Route);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn new_at_zero_is_new_and_an_index_out_of_range_is_zero() {
    let policy = || LadderPolicy::new(RetryPolicy::new(3, Duration::from_secs(5)));
    let mut a = Ladder::new(five(), policy());
    let mut b = Ladder::new_at(five(), policy(), 0);
    let mut c = Ladder::new_at(five(), policy(), 99);
    let t = Instant::now();
    for l in [&mut a, &mut b, &mut c] {
        assert_eq!(l.current(), 0);
        for _ in 0..3 {
            assert!(matches!(
                l.failed(RetryClass::Transient, &boom(), t),
                Next::Retry { .. }
            ));
        }
        assert_eq!(
            l.failed(RetryClass::Transient, &boom(), t),
            Next::Switch {
                to: 1,
                kind: DirectSwitchKind::Route
            }
        );
    }
}
