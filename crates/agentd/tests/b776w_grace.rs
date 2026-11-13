//! Wave 2, row C31 and the two-hour backstop: the longest a run may stay unchanged while a command or a helper agent call is open is the
//! `goal.commandGraceS` member (`ham` key `run.command_grace_minutes`). Two hours is its default, not a fixed number.
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::goal::watch::{Watch, WatchLimits, WireTools, feed_update};
use agentd::engine::tools::TitleStyle;
use harness_protocol::direct::{GOAL_COMMAND_GRACE_S, GoalOptions};
use serde_json::json;
use std::time::{Duration, Instant};

fn limits() -> WatchLimits {
    WatchLimits {
        calls: 30,
        window: Duration::from_secs(20 * 60),
        min_calls: 10,
        who: "Cline",
    }
}

fn tools() -> WireTools<'static> {
    WireTools {
        completion: "mark_goal_complete",
        delegate: Some("spawn_agent"),
        title: TitleStyle::NameColonArgs,
    }
}

/// A watch with a helper agent call that never gets an answer, built the way agentd builds it: from the settings of the run.
fn watch_with_open_helper(settings: &GoalSettings, t0: Instant) -> Watch {
    let mut w = Watch::new(limits(), t0, Some("t0".into()))
        .with_new_calls(settings.new_calls)
        .with_command_grace(settings.command_grace);
    feed_update(
        &mut w,
        t0 + Duration::from_secs(10),
        &json!({"sessionUpdate": "tool_call", "toolCallId": "lost", "kind": "other", "title": "spawn_agent", "status": "pending", "rawInput": {"prompt": "review"}}),
        tools(),
    );
    assert_eq!(
        w.delegates_open(),
        1,
        "TRIGGER PRESENT: the helper call is open"
    );
    w
}

fn none(_: &str) -> Option<String> {
    None
}

#[test]
fn b776w_c31_the_default_grace_is_two_hours_and_ends_a_run_with_an_open_helper_call() {
    let s = GoalSettings::from_lookup(&none);
    assert_eq!(s.command_grace, Duration::from_secs(7200));
    assert_eq!(GOAL_COMMAND_GRACE_S.default, 7200);
    let t0 = Instant::now();
    let mut w = watch_with_open_helper(&s, t0);
    assert_eq!(
        w.look(t0 + Duration::from_secs(7199), Some("t0".into())),
        None
    );
    let b = w
        .look(t0 + Duration::from_secs(7200), Some("t0".into()))
        .expect("the grace is due");
    assert!(
        b.detail.contains(
            "made no progress in 2 hours: a helper agent call stayed open, but no file changed"
        ),
        "{}",
        b.detail
    );
}

#[test]
fn b776w_c31_a_request_member_of_three_hours_moves_the_backstop() {
    let g = GoalOptions {
        command_grace_s: Some(3 * 3600),
        ..GoalOptions::switch(true)
    };
    let s = GoalSettings::from_lookup(&none).with_request(&g);
    assert_eq!(s.command_grace, Duration::from_secs(10_800));
    let t0 = Instant::now();
    let mut w = watch_with_open_helper(&s, t0);
    // The old fixed number no longer ends the run.
    assert_eq!(
        w.look(t0 + Duration::from_secs(7200), Some("t0".into())),
        None
    );
    assert_eq!(
        w.look(t0 + Duration::from_secs(10_799), Some("t0".into())),
        None
    );
    let b = w
        .look(t0 + Duration::from_secs(10_800), Some("t0".into()))
        .expect("the longer grace is due");
    assert!(
        b.detail.contains("made no progress in 3 hours"),
        "{}",
        b.detail
    );
}

#[test]
fn b776w_c31_a_shorter_member_ends_the_run_sooner_and_never_before_the_window() {
    let g = GoalOptions {
        command_grace_s: Some(1800),
        ..GoalOptions::switch(true)
    };
    let s = GoalSettings::from_lookup(&none).with_request(&g);
    assert_eq!(s.command_grace, Duration::from_secs(1800));
    let t0 = Instant::now();
    let mut w = watch_with_open_helper(&s, t0);
    assert_eq!(
        w.look(t0 + Duration::from_secs(1799), Some("t0".into())),
        None
    );
    let b = w
        .look(t0 + Duration::from_secs(1800), Some("t0".into()))
        .expect("the shorter grace is due");
    assert!(
        b.detail.contains("made no progress in 30 minutes"),
        "{}",
        b.detail
    );
}

#[test]
fn b776w_c31_a_grace_below_the_window_is_raised_to_the_window() {
    let g = GoalOptions {
        command_grace_s: Some(60),
        ..GoalOptions::switch(true)
    };
    let s = GoalSettings::from_lookup(&none).with_request(&g);
    assert_eq!(s.command_grace, Duration::from_secs(60));
    let t0 = Instant::now();
    let mut w = watch_with_open_helper(&s, t0);
    // The backstop never fires before the ordinary 20-minute window could.
    assert_eq!(
        w.look(t0 + Duration::from_secs(1199), Some("t0".into())),
        None
    );
    let b = w
        .look(t0 + Duration::from_secs(1200), Some("t0".into()))
        .expect("the window is the floor");
    assert!(
        b.detail.contains("made no progress in 20 minutes"),
        "{}",
        b.detail
    );
}

#[test]
fn b776w_c31_the_stage_variable_is_the_default_a_member_replaces() {
    let get = |k: &str| (k == "AGENTD_GOAL_COMMAND_GRACE_S").then(|| "5400".to_string());
    let s = GoalSettings::from_lookup(&get);
    assert_eq!(s.command_grace, Duration::from_secs(5400));
    let g = GoalOptions {
        command_grace_s: Some(7200),
        ..GoalOptions::switch(true)
    };
    assert_eq!(s.with_request(&g).command_grace, Duration::from_secs(7200));
    // A member left out keeps the stage value.
    assert_eq!(
        s.with_request(&GoalOptions::switch(true)).command_grace,
        Duration::from_secs(5400)
    );
}
