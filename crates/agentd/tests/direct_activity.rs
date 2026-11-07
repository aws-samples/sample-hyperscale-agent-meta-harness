//! The silence clock across the stages of one run: a person deciding holds it, and a new stage starts
//! it clean. The clock is tokio's and paused, so a 31 minute wait takes no wall time.
//!
//! Each test names the failure it prevents: the watchdog must not end a run because a person took their time (hold), must not
//! count that time as the provider's silence (max gap), and must not stay switched off for a whole run because a stalled stage
//! left a tool call `in_progress` (reset).
#![allow(clippy::unwrap_used)]

use agentd::acp::{Activity, DEFAULT_PROVIDER_SILENCE_S, WatchLimits, Watched, watch};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;

const MIN: Duration = Duration::from_secs(60);

fn limits() -> WatchLimits {
    WatchLimits::new(
        Duration::from_secs(8 * 3600),
        Duration::from_secs(u64::from(DEFAULT_PROVIDER_SILENCE_S)),
    )
}

fn tool_running(id: &str) -> Value {
    json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": "run_commands: make test", "kind": "execute", "status": "in_progress"})
}

#[tokio::test(start_paused = true)]
async fn without_a_hold_a_31_minute_wait_is_a_stall() {
    // TRIGGER PRESENT: the control for the next test. The same wait with no hold must end as a stall, so the hold test below
    // proves the hold and not a watchdog that was never going to fire.
    let a = Activity::new();
    a.prompt_sent();
    a.prompt_ended();
    let out = watch(tokio::time::sleep(31 * MIN), limits(), &a).await;
    assert!(matches!(out, Watched::Stalled { .. }), "{out:?}");
}

#[tokio::test(start_paused = true)]
async fn a_held_wait_of_31_minutes_is_not_a_stall() {
    let a = Activity::new();
    a.prompt_sent();
    a.prompt_ended();
    a.hold();
    let out = watch(tokio::time::sleep(31 * MIN), limits(), &a).await;
    assert!(
        matches!(out, Watched::Done(())),
        "a person deciding is not the provider's silence: {out:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn silent_for_is_zero_while_held_however_long_it_lasts() {
    let a = Activity::new();
    a.prompt_sent();
    a.hold();
    tokio::time::advance(31 * MIN).await;
    assert_eq!(a.silent_for(), Duration::ZERO);
    assert_eq!(a.max_silent_gap(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn the_held_wait_never_counts_toward_the_longest_silence_even_when_a_late_line_ends_it() {
    let a = Activity::new();
    a.prompt_sent();
    tokio::time::advance(Duration::from_secs(10)).await;
    a.line_read();
    assert_eq!(a.max_silent_gap(), Duration::from_secs(10));
    a.hold();
    tokio::time::advance(31 * MIN).await;
    // A late notification the pump reads while the person is still deciding: the 31 minutes since the last line are theirs.
    a.line_read();
    assert_eq!(
        a.max_silent_gap(),
        Duration::from_secs(10),
        "the human's wait must not become the run's longest silence"
    );
    assert_eq!(a.last_gap(), Duration::ZERO);
    a.release();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(a.silent_for(), Duration::from_secs(5));
    assert_eq!(a.max_silent_gap(), Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn release_restarts_the_silence_clock_from_the_release() {
    let a = Activity::new();
    a.prompt_sent();
    a.hold();
    tokio::time::advance(31 * MIN).await;
    a.release();
    assert_eq!(
        a.silent_for(),
        Duration::ZERO,
        "the clock starts at the release, not at the last engine line"
    );
    let t0 = Instant::now();
    let out = watch(std::future::pending::<()>(), limits(), &a).await;
    let Watched::Stalled { silent } = out else {
        panic!("{out:?}")
    };
    assert!(silent >= Duration::from_secs(900), "{silent:?}");
    assert!(
        t0.elapsed() < Duration::from_secs(910),
        "the stall comes 900 s after the release, not at once: {:?}",
        t0.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_left_running_by_a_stalled_stage_does_not_switch_off_the_next_stages_watchdog() {
    let a = Activity::new();
    a.prompt_sent();
    a.observe_update(&tool_running("t1"));
    // The stage ended by a stall or a dropped prompt future, so `prompt_ended` never ran and the call is still open.
    assert_eq!(a.tools_in_flight(), 1);
    tokio::time::advance(20 * MIN).await;
    assert_eq!(
        a.silent_for(),
        Duration::ZERO,
        "TRIGGER PRESENT: an open running call silences the watchdog"
    );
    a.reset_for_stage();
    a.prompt_sent();
    assert_eq!(a.tools_in_flight(), 0, "the new stage starts with no call");
    let out = watch(std::future::pending::<()>(), limits(), &a).await;
    assert!(matches!(out, Watched::Stalled { .. }), "{out:?}");
}

#[tokio::test(start_paused = true)]
async fn reset_for_stage_clears_cancel_sent() {
    let a = Activity::new();
    a.prompt_sent();
    a.cancel_written();
    assert!(a.cancel_sent());
    a.reset_for_stage();
    assert!(
        !a.cancel_sent(),
        "a sticky cancel flag would mislabel the next stage's stop"
    );
}

#[tokio::test(start_paused = true)]
async fn reset_for_stage_restarts_the_clock_keeps_the_prompt_armed_and_the_longest_silence() {
    let a = Activity::new();
    assert_eq!(
        a.max_silent_gap_s(),
        None,
        "not armed before the first prompt"
    );
    a.prompt_sent();
    tokio::time::advance(Duration::from_secs(30)).await;
    a.line_read();
    assert_eq!(a.last_gap(), Duration::from_secs(30));
    tokio::time::advance(Duration::from_secs(100)).await;
    assert_eq!(a.silent_for(), Duration::from_secs(100));
    a.reset_for_stage();
    assert_eq!(a.silent_for(), Duration::ZERO, "last = now");
    assert_eq!(
        a.max_silent_gap_s(),
        Some(30),
        "armed stays, the longest silence is kept for the driver to read"
    );
    assert_eq!(
        a.last_gap(),
        Duration::ZERO,
        "the previous stage's gap is not this stage's"
    );
    assert_eq!(a.error_gap(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_stage_never_starts_held() {
    // A hold that outlived its window (a driver bug) must not switch the watchdog off for a whole stage.
    let a = Activity::new();
    a.prompt_sent();
    a.hold();
    tokio::time::advance(31 * MIN).await;
    a.reset_for_stage();
    assert_eq!(a.silent_for(), Duration::ZERO, "last = now");
    tokio::time::advance(Duration::from_secs(901)).await;
    assert!(
        a.silent_for() >= Duration::from_secs(900),
        "the watchdog is live again after the reset: {:?}",
        a.silent_for()
    );
}

#[tokio::test(start_paused = true)]
async fn hold_and_release_are_idempotent_and_a_release_without_a_hold_only_restarts_the_clock() {
    let a = Activity::new();
    a.prompt_sent();
    a.hold();
    a.hold();
    tokio::time::advance(10 * MIN).await;
    a.release();
    a.release();
    assert_eq!(a.silent_for(), Duration::ZERO);
    tokio::time::advance(Duration::from_secs(7)).await;
    assert_eq!(a.silent_for(), Duration::from_secs(7));
    let b = Activity::new();
    b.prompt_sent();
    tokio::time::advance(Duration::from_secs(40)).await;
    b.release();
    assert_eq!(b.silent_for(), Duration::ZERO);
}
