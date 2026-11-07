//! Protocol 1.42.0: the stall limits of the goal loop and the switch of its
//! final-message turn ride in five optional members of the start request's `goal` object; protocol 1.46.0 adds a sixth, `newCalls`, the allowance of
//! calls that bring something new. Each is left off the wire when the user keeps the
//! default, so a request that keeps the defaults is byte for byte the request of an older client; the object refuses unknown members, so a
//! member must exist to be accepted at all. [`check_goal`] is the one check that `ham`, control-api (400 `invalid-goal`) and agentd call.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    GOAL_COMMAND_GRACE_S, GOAL_FINAL_MESSAGE, GOAL_NEW_CALLS, GOAL_ROUND_CALLS,
    GOAL_ROUND_MIN_CALLS, GOAL_ROUND_WINDOW_S, GoalOptions, GoalProblem, check_goal,
};
use serde_json::{Value, json};

fn parse(goal: Value) -> Result<StartDirectRunRequest, serde_json::Error> {
    serde_json::from_value(json!({
        "prompt": "add a function", "target": {"mode": "s3-only"}, "model": "us.openai.gpt-6.1-sol", "rules": [],
        "goal": goal
    }))
}

fn with(f: impl FnOnce(&mut GoalOptions)) -> GoalOptions {
    let mut g = GoalOptions::switch(true);
    f(&mut g);
    g
}

#[test]
fn the_defaults_are_the_numbers_of_the_stage() {
    assert_eq!(GOAL_ROUND_CALLS.default, 30);
    assert_eq!(GOAL_ROUND_WINDOW_S.default, 1200);
    assert_eq!(GOAL_ROUND_MIN_CALLS.default, 10);
    assert_eq!(GOAL_COMMAND_GRACE_S.default, 7200);
    let on = GOAL_FINAL_MESSAGE;
    assert!(on, "the last turn is on unless the user switches it off");
}

#[test]
fn a_switch_has_none_of_the_new_members_and_stays_what_it_was_on_the_wire() {
    assert_eq!(GoalOptions::switch(false), GoalOptions::OFF);
    for enabled in [true, false] {
        let wire = serde_json::to_value(GoalOptions::switch(enabled)).unwrap();
        assert_eq!(
            wire,
            json!({"enabled": enabled}),
            "byte for byte the old member"
        );
    }
}

#[test]
fn the_five_members_round_trip_in_camel_case_and_only_when_set() {
    let wire = json!({"enabled": true, "roundCalls": 45, "roundWindowS": 1800, "roundMinCalls": 12, "commandGraceS": 3600, "finalMessage": false});
    let req = parse(wire.clone()).unwrap();
    let goal = req.goal.unwrap();
    assert_eq!(goal.round_calls, Some(45));
    assert_eq!(goal.round_window_s, Some(1800));
    assert_eq!(goal.round_min_calls, Some(12));
    assert_eq!(goal.command_grace_s, Some(3600));
    assert_eq!(goal.final_message, Some(false));
    assert_eq!(serde_json::to_value(goal).unwrap(), wire);
    // one member alone leaves the others off the wire
    let one = parse(json!({"enabled": true, "roundCalls": 45}))
        .unwrap()
        .goal
        .unwrap();
    assert_eq!(
        serde_json::to_value(one).unwrap(),
        json!({"enabled": true, "roundCalls": 45})
    );
}

#[test]
fn a_wrong_type_or_an_unknown_member_is_refused_by_the_request_type() {
    for bad in [
        json!({"enabled": true, "roundCalls": -1}),
        json!({"enabled": true, "roundCalls": "30"}),
        json!({"enabled": true, "roundCalls": 30.5}),
        json!({"enabled": true, "roundWindowS": null, "stall": 3}),
        json!({"enabled": true, "finalMessage": "yes"}),
        json!({"enabled": true, "commandGraceS": 4294967296u64}),
        json!({"roundCalls": 30}),
    ] {
        assert!(parse(bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn the_bounds_are_the_ones_of_the_stage_settings() {
    let ok = |g: GoalOptions| check_goal(&g).unwrap();
    let bad = |g: GoalOptions| check_goal(&g).unwrap_err();
    ok(GoalOptions::switch(true));
    ok(GoalOptions::switch(false));
    for n in [5, 30, 1000] {
        ok(with(|g| g.round_calls = Some(n)));
    }
    for n in [0, 4, 1001] {
        assert_eq!(
            bad(with(|g| g.round_calls = Some(n))),
            GoalProblem::RoundCalls
        );
    }
    for n in [10, 1200, 14_400] {
        ok(with(|g| g.round_window_s = Some(n)));
    }
    for n in [0, 9, 14_401] {
        assert_eq!(
            bad(with(|g| g.round_window_s = Some(n))),
            GoalProblem::RoundWindow
        );
    }
    for n in [1, 10, 100] {
        ok(with(|g| g.round_min_calls = Some(n)));
    }
    for n in [0, 101] {
        assert_eq!(
            bad(with(|g| g.round_min_calls = Some(n))),
            GoalProblem::RoundMinCalls
        );
    }
    for n in [0, 60, 7200, 86_400] {
        ok(with(|g| g.command_grace_s = Some(n)));
    }
    for n in [1, 59, 86_401] {
        assert_eq!(
            bad(with(|g| g.command_grace_s = Some(n))),
            GoalProblem::CommandGrace
        );
    }
}

#[test]
fn the_first_problem_in_the_order_of_the_members_is_the_one_reported() {
    let g = with(|g| {
        g.command_grace_s = Some(1);
        g.round_calls = Some(1);
    });
    assert_eq!(check_goal(&g).unwrap_err(), GoalProblem::RoundCalls);
}

#[test]
fn a_problem_names_the_member_and_the_range_and_never_the_value() {
    let e = check_goal(&with(|g| g.round_window_s = Some(31_337))).unwrap_err();
    assert_eq!(e.key(), "roundWindowS");
    assert_eq!(e.code(), "out_of_range");
    let text = e.to_string();
    assert!(text.contains("roundWindowS"), "{text}");
    assert!(text.contains("10") && text.contains("14400"), "{text}");
    assert!(!text.contains("31337"), "{text}");
    // the member that has a word for "off" says so
    let grace = check_goal(&with(|g| g.command_grace_s = Some(7)))
        .unwrap_err()
        .to_string();
    assert!(
        grace.contains("0 (do not look for running commands)")
            && grace.contains("60")
            && grace.contains("86400"),
        "{grace}"
    );
}

#[test]
fn limits_with_the_loop_switched_off_are_accepted_and_unused() {
    let g = GoalOptions {
        enabled: false,
        round_calls: Some(40),
        ..GoalOptions::switch(false)
    };
    check_goal(&g).unwrap();
}

// ---- newCalls (1.46.0): the allowance of calls that bring something new ---------------------------------------------------------------

#[test]
fn the_allowance_of_calls_that_bring_something_new_is_100_between_1_and_1000() {
    assert_eq!(
        (
            GOAL_NEW_CALLS.default,
            GOAL_NEW_CALLS.min,
            GOAL_NEW_CALLS.max
        ),
        (100, 1, 1000)
    );
}

#[test]
fn new_calls_rides_in_camel_case_and_only_when_set() {
    let req = parse(json!({"enabled": true, "newCalls": 150})).unwrap();
    let goal = req.goal.unwrap();
    assert_eq!(goal.new_calls, Some(150));
    assert_eq!(
        serde_json::to_value(goal).unwrap(),
        json!({"enabled": true, "newCalls": 150})
    );
    // a request that does not set it is the request it was before 1.46.0
    assert_eq!(
        serde_json::to_value(GoalOptions::switch(true)).unwrap(),
        json!({"enabled": true})
    );
    assert!(parse(json!({"enabled": true, "newCalls": -1})).is_err());
    assert!(parse(json!({"enabled": true, "newCalls": "100"})).is_err());
    assert!(parse(json!({"enabled": true, "newCalls": 4294967296u64})).is_err());
}

#[test]
fn new_calls_counts_as_a_limit_and_zero_means_count_every_call() {
    assert!(!GoalOptions::switch(true).has_limits());
    assert!(with(|g| g.new_calls = Some(0)).has_limits());
    let ok = |n: u32| check_goal(&with(|g| g.new_calls = Some(n))).unwrap();
    for n in [0, 1, 100, 1000] {
        ok(n);
    }
    for n in [1001, u32::MAX] {
        assert_eq!(
            check_goal(&with(|g| g.new_calls = Some(n))).unwrap_err(),
            GoalProblem::NewCalls
        );
    }
}

#[test]
fn a_new_calls_problem_names_the_member_and_the_range_and_never_the_value() {
    let e = check_goal(&with(|g| g.new_calls = Some(31_337))).unwrap_err();
    assert_eq!(e.key(), "newCalls");
    assert_eq!(e.code(), "out_of_range");
    let text = e.to_string();
    assert!(
        text.contains("newCalls") && text.contains("0 (count every call)"),
        "{text}"
    );
    assert!(text.contains('1') && text.contains("1000"), "{text}");
    assert!(!text.contains("31337"), "{text}");
}

#[test]
fn new_calls_is_checked_after_the_older_members() {
    let g = with(|g| {
        g.new_calls = Some(5000);
        g.command_grace_s = Some(1);
    });
    assert_eq!(check_goal(&g).unwrap_err(), GoalProblem::CommandGrace);
}
