//! The wait for helper agents, whole: the real `goal` module in the real driver against the fake ACP engine, whose helper-agent call
//! the script leaves open (`"open": true` in `goal.json`: the engine announces a call of `spawn_agent` and does not answer it while its run is
//! open, as in the two gate runs the two hour guard ended). The limit is set to seconds by the stage variable
//! `AGENTD_GOAL_HELPER_SILENCE_S`; the module cancels the prompt, the engine aborts the call, the module tells the engine in the same session,
//! and the main agent finishes with its own message. Pure checks of the limits and the replay of the recorded runs are in
//! `direct_goal_b777.rs`.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, Hooks};
use common::plugin_folder;
use direct_support::{Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::runtime::TurnOutcome;
use std::collections::BTreeMap;
use std::sync::Arc;

fn settings(extra: &[(&str, &str)]) -> GoalSettings {
    let mut m: BTreeMap<String, String> = [
        (
            "AGENTD_GOAL_PLUGIN_DIR",
            plugin_folder().display().to_string(),
        ),
        ("AGENTD_GOAL_POLL_MS", "20".to_string()),
        ("AGENTD_GOAL_SETTLE_MS", "150".to_string()),
        ("AGENTD_GOAL_DEAD_GRACE_MS", "150".to_string()),
        ("AGENTD_GOAL_KICK_AFTER_S", "1".to_string()),
        ("AGENTD_GOAL_DRAIN_S", "3".to_string()),
        ("AGENTD_GOAL_CLEARED_CONFIRM_MS", "200".to_string()),
        ("AGENTD_GOAL_FALLBACK_QUIET_S", "1".to_string()),
        ("AGENTD_GOAL_CLAIM_WAIT_MS", "300".to_string()),
        ("AGENTD_GOAL_EVENTS_EVERY_S", "0".to_string()),
        ("AGENTD_GOAL_LOOK_EVERY_MS", "50".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in extra {
        m.insert((*k).to_string(), (*v).to_string());
    }
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

async fn start(scenario: &'static str) -> Direct {
    Direct::start(Opts {
        scenario,
        timeout_s: 90,
        ..Opts::default()
    })
    .await
}

async fn run(d: &Direct, s: GoalSettings) -> harness_protocol::runtime::InvocationResponse {
    let goal = GoalHook::for_engine("cline", s).expect("the plugin folder is in the repository");
    let modules: Vec<Arc<dyn DirectHook>> = vec![goal];
    let deps = Deps {
        hooks: Some(Hooks::new(modules)),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn cancels_written(d: &Direct) -> usize {
    d.fake_engine_log()
        .lines()
        .filter(|l| *l == "notification session/cancel")
        .count()
}

/// The `goal-loop` progress events of one phase, in order.
fn events(d: &Direct, phase: &str) -> Vec<serde_json::Value> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["phase"] == phase)
        .collect()
}

/// How many times the notice reached the engine (one `session/prompt` line of the engine's log carries its first words).
fn notices_sent(d: &Direct) -> usize {
    d.fake_engine_log()
        .lines()
        .filter(|l| l.starts_with("in ") && l.contains("The harness cancelled your helper agents."))
        .count()
}

const SHORT: [(&str, &str); 1] = [("AGENTD_GOAL_HELPER_SILENCE_S", "2")];

#[tokio::test(flavor = "multi_thread")]
async fn b777_a_helper_that_never_answers_is_cancelled_and_the_main_agent_ends_cline_reported_done()
{
    let d = start("goal_helper_stall").await;
    let t0 = std::time::Instant::now();
    let resp = run(&d, settings(&SHORT)).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:#?}",
        d.engine_trace(),
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}\n{}",
        s.label,
        d.engine_trace()
    );
    assert!(s.label.starts_with("Cline reported done"), "{}", s.label);
    // TRIGGER PRESENT: the script holds the first run open for 60 s with the helper call unanswered, so the run was cut by the module's
    // cancel and not by the end of the hold
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(45),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(cancels_written(&d), 1, "one cancel ended the stalled wait");
    let stalls = events(&d, "helper-stall");
    assert_eq!(stalls.len(), 1, "{stalls:?}");
    assert_eq!(stalls[0]["rule"], "silence");
    assert_eq!(stalls[0]["cancelled"], true);
    assert_eq!(stalls[0]["told"], true);
    assert_eq!(stalls[0]["open"], 1);
    assert_eq!(stalls[0]["count"], 1);
    assert_eq!(notices_sent(&d), 1, "the engine was told once");
    // the main agent finished the work after the notice
    assert!(d.worktree().join("notes/review.txt").exists());
    // the goal was not given up: no brake, and the run ended by the engine's own completion
    assert!(events(&d, "brake").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn b777_a_notice_that_the_unwinding_engine_dropped_is_sent_once_more_and_the_run_still_ends_done()
 {
    let d = start("goal_helper_swallow").await;
    let resp = run(&d, settings(&SHORT)).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}\n{}",
        s.label,
        d.engine_trace()
    );
    // TRIGGER PRESENT: the engine dropped the first notice at once with usage all zero and did nothing
    assert!(
        d.fake_engine_log()
            .contains("prompt dropped: the cancelled run is still unwinding"),
        "{}",
        d.engine_trace()
    );
    assert_eq!(notices_sent(&d), 2, "sent again once, and only once");
    assert!(d.worktree().join("notes/review.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn b777_helpers_that_return_in_time_are_not_cancelled() {
    let d = start("goal_helper_returns").await;
    // the stage's own defaults: 25 minutes of silence, 75 minutes of wait
    let resp = run(&d, settings(&[])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}\n{}",
        s.label,
        d.engine_trace()
    );
    assert_eq!(cancels_written(&d), 0);
    assert!(events(&d, "helper-stall").is_empty());
    assert_eq!(notices_sent(&d), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn b777_with_both_limits_at_zero_a_silent_helper_is_left_alone_until_the_run_ends_by_itself()
{
    let d = start("goal_helper_returns").await;
    let resp = run(
        &d,
        settings(&[
            ("AGENTD_GOAL_HELPER_SILENCE_S", "0"),
            ("AGENTD_GOAL_HELPER_WAIT_S", "0"),
        ]),
    )
    .await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(cancels_written(&d), 0);
    assert!(events(&d, "helper-stall").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn b777_helpers_that_stall_again_after_two_notices_end_the_run_with_the_harness_words() {
    let d = start("goal_helper_again").await;
    let resp = run(&d, settings(&SHORT)).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}\n{}",
        s.label,
        d.engine_trace()
    );
    assert!(
        s.label
            .starts_with("Cline stopped: the harness ended the run (Cline made no progress: helper agent calls stayed open after the harness cancelled them 2 times"),
        "{}",
        s.label
    );
    // two cancels with a notice each, then the third stall is the brake
    let stalls = events(&d, "helper-stall");
    assert_eq!(stalls.len(), 2, "{stalls:?}");
    assert_eq!(stalls[0]["count"], 1);
    assert_eq!(stalls[1]["count"], 2);
    assert_eq!(notices_sent(&d), 2);
    assert_eq!(cancels_written(&d), 3);
    let brakes = events(&d, "brake");
    assert_eq!(brakes.len(), 1, "{brakes:?}");
    assert_eq!(brakes[0]["rule"], "round-no-progress");
}

#[tokio::test(flavor = "multi_thread")]
async fn b777_a_stall_with_less_than_a_minute_left_cancels_the_wait_but_sends_no_notice() {
    // a run of 40 seconds: the stall comes at about 2 s, and a notice needs a minute of the run to be left
    let d = Direct::start(Opts {
        scenario: "goal_helper_stall",
        timeout_s: 40,
        ..Opts::default()
    })
    .await;
    let _ = run(&d, settings(&SHORT)).await;
    let stalls = events(&d, "helper-stall");
    assert_eq!(stalls.len(), 1, "{stalls:?}\n{}", d.engine_trace());
    assert_eq!(stalls[0]["cancelled"], true);
    assert_eq!(stalls[0]["told"], false, "{stalls:?}");
    assert_eq!(cancels_written(&d), 1);
    assert_eq!(notices_sent(&d), 0, "{}", d.engine_trace());
    assert!(
        !d.worktree().join("notes/review.txt").exists(),
        "the notice round did not run"
    );
}
