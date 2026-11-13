//! The watch counts tool calls, looks at the tree now and then and ends the run after N calls in a row with an unchanged tree and no new
//! text, or after a window of wall-clock time with an unchanged tree. Pure: the tests move time and the tree by hand.
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::machine::Rule;
use agentd::direct::goal::watch::{Command, START_MARGIN, UNFOLDED_MAX, Watch, WatchLimits};
use harness_protocol::direct::{DirectOutcome, HARNESS_STOP_PREFIX};
use std::time::{Duration, Instant};

fn limits() -> WatchLimits {
    WatchLimits {
        calls: 30,
        window: Duration::from_secs(20 * 60),
        min_calls: 10,
        who: "Cline",
    }
}

struct Clock(Instant);

impl Clock {
    fn at(&self, s: u64) -> Instant {
        self.0 + Duration::from_secs(s)
    }
}

fn watch(c: &Clock) -> Watch {
    Watch::new(limits(), c.at(0), Some("t0".into()))
}

fn calls(w: &mut Watch, c: &Clock, n: u32, from_s: u64) {
    for i in 0..n {
        w.tool_call(c.at(from_s + u64::from(i)));
    }
}

#[test]
fn defaults_are_the_numbers_of_the_orders() {
    let s = agentd::direct::goal::settings::GoalSettings::from_lookup(&|_| None);
    assert_eq!(s.watch_limits("x").calls, 30);
    assert_eq!(s.watch_limits("x").window, Duration::from_secs(1200));
    assert_eq!(s.watch_limits("x").min_calls, 10);
    assert_eq!(s.look_every, Duration::from_secs(5));
}

#[test]
fn thirty_varied_calls_with_an_unchanged_tree_and_no_text_end_the_run() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    calls(&mut w, &c, 29, 1);
    assert_eq!(
        w.look(c.at(40), Some("t0".into())),
        None,
        "29 calls are not enough"
    );
    w.tool_call(c.at(41));
    let b = w
        .look(c.at(46), Some("t0".into()))
        .expect("30 calls in a row");
    assert_eq!(b.rule, Rule::RoundNoProgress);
    assert!(b.detail.starts_with(HARNESS_STOP_PREFIX), "{}", b.detail);
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&b.detail)),
        "Cline stopped: the harness ended the run (Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared)"
    );
}

#[test]
fn the_count_in_the_detail_is_the_number_of_calls_seen_when_the_tree_was_looked_at() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    calls(&mut w, &c, 41, 1);
    let b = w.look(c.at(50), Some("t0".into())).unwrap();
    assert!(b.detail.contains("41 tool calls in a row"), "{}", b.detail);
}

#[test]
fn new_text_starts_the_count_again_but_not_the_clock() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    calls(&mut w, &c, 25, 1);
    w.text();
    calls(&mut w, &c, 25, 30);
    assert_eq!(
        w.look(c.at(60), Some("t0".into())),
        None,
        "25 calls since the last text"
    );
    assert_eq!(w.calls_in_a_row(), 25);
    // the window counts every call since the tree last changed, text or not
    let mut w = watch(&c);
    calls(&mut w, &c, 6, 1);
    w.text();
    calls(&mut w, &c, 6, 10);
    let b = w
        .look(c.at(20 * 60 + 1), Some("t0".into()))
        .expect("12 calls over the window");
    assert!(
        b.detail
            .contains("12 of 12 tool calls counted as no progress"),
        "{}",
        b.detail
    );
}

#[test]
fn a_change_of_the_tree_starts_both_counts_again() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    calls(&mut w, &c, 29, 1);
    assert_eq!(w.look(c.at(40), Some("t0".into())), None);
    w.tool_call(c.at(41));
    // the thirtieth call came with an edit: the tree changed, nothing is stuck
    assert_eq!(w.look(c.at(46), Some("t1".into())), None);
    assert_eq!(w.calls_in_a_row(), 0);
    calls(&mut w, &c, 29, 50);
    assert_eq!(w.look(c.at(90), Some("t1".into())), None);
    w.tool_call(c.at(91));
    assert!(w.look(c.at(96), Some("t1".into())).is_some());
}

#[test]
fn a_tree_that_could_not_be_read_never_ends_a_run_and_starts_the_counts_again() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    calls(&mut w, &c, 80, 1);
    assert_eq!(w.look(c.at(100), None), None);
    assert_eq!(w.calls_in_a_row(), 0);
}

#[test]
fn the_first_look_with_no_baseline_is_a_change_not_a_stall() {
    let c = Clock(Instant::now());
    let mut w = Watch::new(limits(), c.at(0), None);
    calls(&mut w, &c, 40, 1);
    assert_eq!(w.look(c.at(50), Some("t0".into())), None);
    calls(&mut w, &c, 30, 60);
    assert!(w.look(c.at(100), Some("t0".into())).is_some());
}

#[test]
fn twenty_minutes_with_an_unchanged_tree_end_a_run_that_kept_calling_but_not_one_long_command() {
    let c = Clock(Instant::now());
    // twelve calls spread over the window
    let mut w = watch(&c);
    for i in 0..12u64 {
        w.tool_call(c.at(i * 110));
    }
    assert_eq!(w.look(c.at(19 * 60), Some("t0".into())), None, "19 minutes");
    let b = w
        .look(c.at(20 * 60), Some("t0".into()))
        .expect("20 minutes");
    assert_eq!(b.rule, Rule::RoundNoProgress);
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&b.detail)),
        "Cline stopped: the harness ended the run (Cline made no progress in 20 minutes: no file changed and 12 of 12 tool calls counted as no progress)"
    );
    // a build or a test run that takes the whole window is one or two calls, not a poll
    let mut w = watch(&c);
    calls(&mut w, &c, 9, 1);
    assert_eq!(w.look(c.at(25 * 60), Some("t0".into())), None);
}

#[test]
fn a_round_boundary_starts_everything_again() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    calls(&mut w, &c, 29, 1);
    w.boundary(c.at(40));
    calls(&mut w, &c, 29, 41);
    assert_eq!(w.look(c.at(80), Some("t0".into())), None);
    // and the window starts at the boundary
    let mut w = watch(&c);
    calls(&mut w, &c, 12, 1);
    w.boundary(c.at(15 * 60));
    calls(&mut w, &c, 12, 15 * 60 + 1);
    assert_eq!(
        w.look(c.at(21 * 60), Some("t0".into())),
        None,
        "6 minutes after the boundary"
    );
}

#[test]
fn the_tree_is_looked_at_only_after_a_new_call_and_not_more_often_than_asked() {
    let c = Clock(Instant::now());
    let every = Duration::from_secs(5);
    let mut w = watch(&c);
    assert!(!w.wants_look(c.at(1), every), "no call yet");
    w.tool_call(c.at(1));
    assert!(w.wants_look(c.at(1), every), "a call and no look yet");
    assert_eq!(w.look(c.at(1), Some("t0".into())), None);
    assert!(!w.wants_look(c.at(2), every), "no new call");
    w.tool_call(c.at(2));
    assert!(!w.wants_look(c.at(3), every), "looked 2 s ago");
    assert!(w.wants_look(c.at(6), every));
    // 0 means every poll
    assert!(w.wants_look(c.at(2), Duration::ZERO));
}

/// The two texts the live judges (A12, L-A12 in `crates/ham-e2e`) and the client card read; the judges' stand-ins hold the same strings.
#[test]
fn the_detail_texts_are_the_ones_the_live_judges_and_the_card_read() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    calls(&mut w, &c, 30, 1);
    assert_eq!(
        w.look(c.at(40), Some("t0".into())).unwrap().detail,
        "The harness ended the run: Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared."
    );
    let mut w = watch(&c);
    for i in 0..12u64 {
        w.tool_call(c.at(i * 110));
    }
    assert_eq!(
        w.look(c.at(20 * 60), Some("t0".into())).unwrap().detail,
        "The harness ended the run: Cline made no progress in 20 minutes: no file changed and 12 of 12 tool calls counted as no progress."
    );
}

// ---- a command that is running --------------------------------------------------
//
// Cline started the repository's full test script in the background (about 25 minutes) and polled it with `sleep` calls, 595 seconds
// apiece. The watch counted every poll: ten calls over twenty minutes with no file changed ended the run, and Cline never wrote its final
// message. Evidence that a command of the engine's user is running (`Command::Active`, sampled by the module every few seconds) pauses
// both limits: the polls made in that time are not counted and the time is not counted against the window. A tree that stays unchanged
// for the grace (two hours by default) still ends the run, so a command that hangs cannot hold it for ever.
//
// The evidence says how old the sessions are that used the CPU (`Active { age }`). A call is taken back only when those sessions are older
// than the call by `START_MARGIN`: the model waits for a command it started with an earlier call. The command a call starts and runs to its
// end is the model's own work, not a wait (the review of the first version: a loop that ran the failing test suite 40 times was never ended).

const TICK: u64 = 5;

/// A job the model started long ago: its sessions are older than any call of the tests.
const BACKGROUND: Command = Command::Active {
    age: Duration::from_secs(10 * 24 * 3600),
};

fn active(age_s: u64) -> Command {
    Command::Active {
        age: Duration::from_secs(age_s),
    }
}

/// What the module does every `TICK` seconds: the tool calls that came in the last tick are told to the watch, then the evidence of the tick,
/// then the look at the tree when the watch asks for one. Returns the second of the first brake and the brake.
fn drive(
    w: &mut Watch,
    c: &Clock,
    polls: &[u64],
    to_s: u64,
    evidence: impl Fn(u64) -> Command,
) -> Option<(u64, agentd::direct::goal::machine::Brake)> {
    let mut t = 0;
    while t <= to_s {
        for p in polls.iter().filter(|p| **p + TICK > t && **p <= t) {
            w.tool_call(c.at(*p));
        }
        w.command(c.at(t), evidence(t));
        if w.wants_look(c.at(t), Duration::from_secs(TICK))
            && let Some(b) = w.look(c.at(t), Some("t0".into()))
        {
            return Some((t, b));
        }
        t += TICK;
    }
    None
}

fn grace_watch(c: &Clock, grace_s: u64) -> Watch {
    Watch::new(limits(), c.at(0), Some("t0".into()))
        .with_command_grace(Duration::from_secs(grace_s))
}

/// T2: three `sleep` calls of ten minutes and a few reads between them, 25 minutes in all.
const T2_POLLS: [u64; 12] = [
    0, 300, 330, 600, 900, 905, 910, 1200, 1210, 1220, 1230, 1500,
];

#[test]
fn polls_made_while_a_command_runs_are_not_counted_and_the_run_goes_on() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    assert!(drive(&mut w, &c, &T2_POLLS, 26 * 60, |_| BACKGROUND).is_none());
    assert_eq!(w.calls_in_a_row(), 0, "the polls were waiting, not polling");
}

#[test]
fn the_same_polls_with_no_command_running_still_end_the_run() {
    // TRIGGER PRESENT: the call pattern of the test above, and nothing running: the window ends the run at twenty minutes.
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    let (at, b) = drive(&mut w, &c, &T2_POLLS, 26 * 60, |_| Command::Idle)
        .expect("10 calls over 20 minutes, nothing running");
    assert!(at >= 20 * 60, "{at}");
    assert_eq!(b.rule, Rule::RoundNoProgress);
    assert!(
        b.detail.contains("tool calls counted as no progress"),
        "{}",
        b.detail
    );
}

#[test]
fn evidence_that_could_not_be_read_never_protects() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    assert!(drive(&mut w, &c, &T2_POLLS, 26 * 60, |_| Command::Unknown).is_some());
}

#[test]
fn the_time_a_command_ran_is_paused_not_forgiven() {
    // 5 minutes of polling with nothing running, 10 minutes with a command, then nothing running again: the window needs 15 more minutes of
    // unprotected waiting, so the run ends at minute 30 and not at minute 20 or minute 35.
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    let mut polls: Vec<u64> = (1..=12).collect();
    polls.extend((15..40).map(|m| m * 60));
    let ev = |t: u64| {
        if (300..900).contains(&t) {
            BACKGROUND
        } else {
            Command::Idle
        }
    };
    let (at, b) = drive(&mut w, &c, &polls, 40 * 60, ev).expect("the window ends the run");
    assert!((30 * 60..31 * 60).contains(&at), "ended at {at} s");
    assert!(
        b.detail.contains("tool calls counted as no progress"),
        "{}",
        b.detail
    );
}

#[test]
fn polls_before_the_first_active_sample_are_counted_and_the_ones_it_covers_are_not() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    for t in 1..=3 {
        w.tool_call(c.at(t));
    }
    w.command(c.at(5), Command::Idle);
    for t in 6..=10 {
        w.tool_call(c.at(t));
    }
    w.command(c.at(11), BACKGROUND);
    assert_eq!(
        w.calls_in_a_row(),
        3,
        "the five calls of the active interval are waiting"
    );
}

#[test]
fn a_command_that_never_ends_still_ends_the_run_at_the_grace() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 3600);
    let (at, b) =
        drive(&mut w, &c, &[1], 2 * 3600, |_| BACKGROUND).expect("one hour with an unchanged tree");
    assert!((3600..3610).contains(&at), "ended at {at} s");
    assert_eq!(b.rule, Rule::RoundNoProgress);
    assert_eq!(
        b.detail,
        "The harness ended the run: Cline made no progress in 1 hour: a command was running, but no file changed."
    );
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&b.detail)),
        "Cline stopped: the harness ended the run (Cline made no progress in 1 hour: a command was running, but no file changed)"
    );
}

#[test]
fn a_changed_file_gives_the_grace_back() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 3600);
    w.tool_call(c.at(1));
    let mut t = 5;
    while t < 3000 {
        w.command(c.at(t), BACKGROUND);
        t += 5;
    }
    // the command wrote a file: the tree changed, nothing is stuck, both clocks start again
    w.tool_call(c.at(3001));
    w.command(c.at(3005), BACKGROUND);
    assert_eq!(w.look(c.at(3005), Some("t1".into())), None);
    assert_eq!(w.protected(), Duration::ZERO);
    let mut t = 3010;
    while t < 3005 + 3600 {
        w.command(c.at(t), BACKGROUND);
        assert_eq!(w.look(c.at(t), Some("t1".into())), None, "at {t} s");
        t += 5;
    }
    w.command(c.at(3005 + 3600), BACKGROUND);
    assert!(w.look(c.at(3005 + 3600), Some("t1".into())).is_some());
}

#[test]
fn the_grace_is_never_shorter_than_the_window() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 30);
    let (at, _) = drive(&mut w, &c, &[1], 2 * 3600, |_| BACKGROUND).expect("the window ends it");
    assert!((20 * 60..20 * 60 + 10).contains(&at), "ended at {at} s");
}

#[test]
fn an_interval_of_active_evidence_is_credited_at_most_a_minute() {
    // a gap in the samples (the module samples only while calls are pending) is not a command that ran through it
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 3600);
    w.tool_call(c.at(1));
    w.command(c.at(5), BACKGROUND);
    assert_eq!(w.protected(), Duration::from_secs(5));
    w.command(c.at(5 + 3000), BACKGROUND);
    assert_eq!(w.protected(), Duration::from_secs(65));
}

#[test]
fn the_tree_is_looked_at_when_the_grace_is_due_though_no_call_came() {
    let c = Clock(Instant::now());
    let every = Duration::from_secs(TICK);
    let mut w = grace_watch(&c, 3600);
    w.tool_call(c.at(1));
    let mut t = 5;
    while t < 3000 {
        w.command(c.at(t), BACKGROUND);
        t += 5;
    }
    assert_eq!(w.look(c.at(30), Some("t0".into())), None);
    assert!(
        !w.wants_look(c.at(3000), every),
        "no new call and the grace is not due"
    );
    w.command(c.at(3600), BACKGROUND);
    assert!(
        w.wants_look(c.at(3601), every),
        "the grace is due: the tree must be read before the run is ended"
    );
}

#[test]
fn the_module_samples_only_while_something_is_pending() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 3600);
    assert!(
        !w.exposed(),
        "a run that has made no call has nothing to protect"
    );
    w.tool_call(c.at(1));
    assert!(w.exposed());
    w.boundary(c.at(2));
    assert!(!w.exposed(), "a boundary starts everything again");
}

// ---- a command the model waits for and a command it runs itself (the review of the first version) -------------------------------------
//
// A call that starts a command and waits for it in the foreground is the model's own work. The sessions that used the CPU are then as old
// as the call or younger, and the call is not taken back. The polls that come after a job was started are older than the job's start by more
// than `START_MARGIN`, and they are.

/// What the process table shows at second `t` when the model runs each of `calls` as a foreground command of `run_s` seconds that begins one
/// second after its call arrives: a session as old as that command is using the CPU, and between the commands nothing is.
fn foreground(calls: &[u64], run_s: u64) -> impl Fn(u64) -> Command + '_ {
    move |t| match calls.iter().rev().find(|a| **a < t) {
        Some(a) if t <= *a + run_s => active(t - a - 1),
        _ => Command::Idle,
    }
}

#[test]
fn thirty_foreground_commands_that_use_the_cpu_still_end_the_run_at_thirty_calls() {
    // TRIGGER PRESENT: a call every 20 s, each a test suite that runs 15 s and uses the CPU the whole time; no file changes and no text comes.
    let c = Clock(Instant::now());
    let calls: Vec<u64> = (0..40).map(|i| 10 + i * 20).collect();
    let mut w = grace_watch(&c, 7200);
    let (at, b) = drive(&mut w, &c, &calls, 30 * 60, foreground(&calls, 15))
        .expect("30 calls in a row, whatever the commands did");
    assert_eq!(b.rule, Rule::RoundNoProgress);
    assert!(b.detail.contains("30 tool calls in a row"), "{}", b.detail);
    assert_eq!(at, 10 + 29 * 20, "the 30th call");
    assert_eq!(
        w.protected(),
        Duration::ZERO,
        "none of the time was credited to a command the model ran itself"
    );
}

#[test]
fn foreground_commands_spread_over_twenty_minutes_still_end_the_run_at_the_window() {
    // thirteen calls 100 s apart, each a command of 90 s that uses the CPU: the window needs no call count
    let c = Clock(Instant::now());
    let calls: Vec<u64> = (0..14).map(|i| 5 + i * 100).collect();
    let mut w = grace_watch(&c, 7200);
    let (at, b) = drive(&mut w, &c, &calls, 30 * 60, foreground(&calls, 90))
        .expect("20 minutes with an unchanged tree");
    assert_eq!(at, 1205, "the 13th call is the first after the window");
    assert!(
        b.detail
            .contains("13 of 13 tool calls counted as no progress"),
        "{}",
        b.detail
    );
}

#[test]
fn the_call_that_starts_a_job_stays_counted_and_the_polls_after_it_are_taken_back() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    // second 0: the call that starts the job; its session begins at second 1, so it is 9 s old at second 10
    w.tool_call(c.at(0));
    w.command(c.at(10), active(9));
    assert_eq!(
        w.calls_in_a_row(),
        1,
        "the call that started the job is the model's own work"
    );
    assert_eq!(w.protected(), Duration::ZERO, "and no time is credited yet");
    // second 20: a poll (`sleep`); at second 30 the job is 29 s old and the poll 10 s
    w.tool_call(c.at(20));
    w.command(c.at(30), active(29));
    assert_eq!(
        w.calls_in_a_row(),
        1,
        "the poll is waiting for the job and is taken back; the start stays counted"
    );
    assert_eq!(
        w.protected(),
        Duration::from_secs(20),
        "the time since the last sample is credited"
    );
}

#[test]
fn a_poll_that_comes_less_than_the_margin_after_the_job_began_is_counted() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    // the job began at second 1, a poll arrives at second 3 and is sampled at second 8: the job is 7 s old, the poll 5 s
    w.tool_call(c.at(3));
    assert_eq!(
        START_MARGIN,
        Duration::from_secs(5),
        "the margin of the tests"
    );
    w.command(c.at(8), active(7));
    assert_eq!(
        w.calls_in_a_row(),
        1,
        "a call within the margin of the job's start is counted"
    );
    // the next poll is older than the margin: at second 15 the job is 14 s old and the poll 5 s
    w.tool_call(c.at(10));
    w.command(c.at(15), active(14));
    assert_eq!(w.calls_in_a_row(), 1, "the later poll is taken back");
}

#[test]
fn time_is_credited_only_while_the_newest_call_is_older_than_the_sessions_that_use_the_cpu() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    // a foreground command: the sessions that use the CPU began after the call
    w.tool_call(c.at(0));
    w.command(c.at(30), active(28));
    assert_eq!(w.protected(), Duration::ZERO);
    // the model polls: the job is older than the poll
    w.tool_call(c.at(40));
    w.command(c.at(45), active(43));
    assert_eq!(w.protected(), Duration::from_secs(15));
}

#[test]
fn a_long_poll_keeps_earning_time_while_no_new_call_comes() {
    // `sleep 595` is one call; the job began at second 0, the poll arrives at second 10 and is the newest call for ten minutes
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    w.tool_call(c.at(10));
    let mut t = 10;
    while t <= 600 {
        w.command(c.at(t), active(t));
        t += TICK;
    }
    assert!(
        w.protected() >= Duration::from_secs(590),
        "{:?}",
        w.protected()
    );
    assert_eq!(w.calls_in_a_row(), 0);
}

#[test]
fn a_boundary_forgets_the_pending_calls_but_not_when_the_newest_call_came() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    w.tool_call(c.at(0));
    w.boundary(c.at(3));
    w.command(c.at(20), active(100));
    assert_eq!(w.calls_in_a_row(), 0);
    assert_eq!(
        w.protected(),
        Duration::from_secs(17),
        "the poll that is still pending earns the time from the boundary"
    );
}

#[test]
fn the_calls_waiting_for_a_sample_are_bounded_and_the_ones_left_out_stay_counted() {
    let c = Clock(Instant::now());
    let mut w = grace_watch(&c, 7200);
    let extra = 10;
    for i in 0..(UNFOLDED_MAX + extra) {
        w.tool_call(c.at(u64::try_from(i).unwrap() / 100));
    }
    w.command(c.at(100), BACKGROUND);
    assert_eq!(
        usize::try_from(w.calls_in_a_row()).unwrap(),
        extra,
        "only the calls the watch could keep are taken back"
    );
}

#[test]
fn a_later_call_is_taken_back_whenever_an_earlier_one_is() {
    // four calls, 10 s apart, sampled at second 40: for every age of the sessions the calls taken back are the last ones
    let arrivals = [0u64, 10, 20, 30];
    for age in 0..=60u64 {
        let c = Clock(Instant::now());
        let mut w = grace_watch(&c, 7200);
        for a in arrivals {
            w.tool_call(c.at(a));
        }
        w.command(c.at(40), active(age));
        let waiting: Vec<bool> = arrivals
            .iter()
            .map(|a| Duration::from_secs(age) >= Duration::from_secs(40 - a) + START_MARGIN)
            .collect();
        assert!(
            waiting.windows(2).all(|p| p[0] <= p[1]),
            "a later call qualifies whenever an earlier one does: {waiting:?}"
        );
        assert_eq!(
            usize::try_from(w.calls_in_a_row()).unwrap(),
            waiting.iter().filter(|q| !**q).count(),
            "age {age} s: {waiting:?}"
        );
    }
}
