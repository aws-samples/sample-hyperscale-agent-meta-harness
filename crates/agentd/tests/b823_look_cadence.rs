//! When the next look of the copy task starts. With S the start of the last look, E the end of
//! its git work, D = E - S and I the timer of the run, the next look starts at
//!
//! ```text
//! max(S + I, E + min(9 * D, 300 s))
//! ```
//!
//! so the looks take a tenth of the time at the most while D is at most 33 s, and a look that takes longer is followed by five minutes
//! without one. At the default timer of 15 s: D = 0.2 s, the next look 15 s after the start (1.3% of the time); D = 3 s, 30 s after the
//! start (10%); D = 30 s, 300 s after the start (10%); D = 120 s, 420 s after the start (29%). Before, the spacing was also capped at five
//! timer periods, 75 s at this timer: a look of 30 s came back after 75 s and took 29% of the time, a look of 120 s 62%.
//!
//! A look that failed counts with the time it took too (before, nothing was spaced after it): a look whose git was given up after five
//! minutes is not started again at once. After a look that failed the timer is the retry time when that is shorter.
#![allow(clippy::unwrap_used)]

use agentd::direct::snapshot::{
    COPY_FLOOR, COPY_RETRY, COPY_SPACING, COPY_SPACING_CAP, COPY_SPACING_MAX, Cadence, CopyClock,
    failed_look_counts,
};
use std::time::{Duration, Instant};

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// How long after the start of a look whose git work took `d` the next look starts under `c`: the tick of the timer comes one period after
/// the start, or when the look has ended if that is later; then the task waits for [`Cadence::delay`].
fn next_start(c: &Cadence, d: Duration, failed: bool) -> Duration {
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    if failed {
        clock.failed(t0, t0 + d);
    } else {
        clock.looked(t0, t0 + d, Some(4_096));
    }
    let tick = t0 + c.period(failed).unwrap().max(d);
    (tick + c.delay(&clock, false, tick)).duration_since(t0)
}

#[test]
fn b823_look_the_constants_of_the_cadence_are_the_ones_the_pages_state() {
    assert_eq!(COPY_FLOOR, s(15));
    assert_eq!(COPY_RETRY, s(15));
    assert_eq!(COPY_SPACING, 9);
    assert_eq!(COPY_SPACING_MAX, s(300));
}

#[test]
fn b823_look_the_next_look_at_the_default_timer_for_four_sizes_of_repository() {
    let c = Cadence::new(Some(s(15)));
    // (the git work of a look, when the next one starts counted from the start of this one)
    for (d, next) in [
        (ms(200), s(15)),
        (s(3), s(30)),
        (s(30), s(300)),
        (s(120), s(420)),
    ] {
        assert_eq!(next_start(&c, d, false), next, "git work of {d:?}");
    }
}

#[test]
fn b823_look_the_looks_take_a_tenth_of_the_time_while_one_takes_at_most_33_seconds() {
    for every in [15, 60, 600, 3_600] {
        let c = Cadence::new(Some(s(every)));
        for d in [ms(200), s(3), s(30), s(33)] {
            let next = next_start(&c, d, false);
            assert!(
                d.as_secs_f64() / next.as_secs_f64() <= 0.1 + 1e-9,
                "a timer of {every} s and git work of {d:?}: the next look starts after {next:?}"
            );
            assert!(next >= s(every), "never sooner than the timer");
        }
    }
}

#[test]
fn b823_look_a_look_of_two_minutes_is_followed_by_five_minutes_without_one_whatever_the_timer() {
    for every in [15, 60, 300] {
        let c = Cadence::new(Some(s(every)));
        assert_eq!(
            next_start(&c, s(120), false),
            s(420),
            "a timer of {every} s: nine times 120 s is 18 minutes, the cap is five"
        );
    }
    // Under an hourly timer the timer decides.
    assert_eq!(
        next_start(&Cadence::new(Some(s(3_600))), s(120), false),
        s(3_600)
    );
}

#[test]
fn b823_look_a_longer_timer_moves_the_next_look_and_not_the_spacing() {
    let minute = Cadence::new(Some(s(60)));
    for (d, next) in [
        (ms(200), s(60)),
        (s(3), s(60)),
        (s(30), s(300)),
        (s(120), s(420)),
    ] {
        assert_eq!(next_start(&minute, d, false), next, "git work of {d:?}");
    }
}

#[test]
fn b823_look_a_look_that_failed_counts_with_the_time_it_took() {
    let c = Cadence::new(Some(s(15)));
    for (d, next) in [
        (ms(200), s(15)),
        (s(3), s(30)),
        (s(30), s(300)),
        (s(120), s(420)),
        // The git work of a look that was given up at its limit of five minutes.
        (s(300), s(600)),
    ] {
        assert_eq!(next_start(&c, d, true), next, "a failed look of {d:?}");
    }
    // Under an hourly timer a look that failed is tried again after the retry time, and after the spacing when that is longer.
    let hour = Cadence::new(Some(s(3_600)));
    assert_eq!(next_start(&hour, ms(200), true), s(15));
    assert_eq!(next_start(&hour, s(3), true), s(30));
    assert_eq!(next_start(&hour, s(30), true), s(300));
}

#[test]
fn b823_look_what_a_failed_look_counts_ends_with_its_git_work() {
    // The git work took 0.2 s and the store of the copy failed 30 s later: the store is not counted, as for a copy that was stored.
    let c = Cadence::new(Some(s(15)));
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    clock.failed(t0, t0 + ms(200));
    assert_eq!(c.delay(&clock, false, t0 + s(30)), Duration::ZERO);
    // A tool call is still floored from the start of the look.
    assert_eq!(c.delay(&clock, true, t0 + s(4)), s(11));
}

#[test]
fn b823_look_a_timer_that_a_test_set_keeps_five_of_its_periods_as_the_cap() {
    assert_eq!(COPY_SPACING_CAP, 5);
    let every = ms(300);
    let c = Cadence::new(Some(every)).with_space_max(every * COPY_SPACING_CAP);
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    clock.looked(t0, t0 + s(1), Some(4_096));
    assert_eq!(c.delay(&clock, false, t0 + s(1)), ms(1_500));
    // Without it the cap is the five minutes of every run.
    assert_eq!(
        Cadence::new(Some(every)).delay(&clock, false, t0 + s(1)),
        s(9)
    );
}

// ---- what a failed look counts ---------------------------------------------------------------------------------------------------

/// The text of the copy task, with every run of white space made one blank.
fn copy_task() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/direct/mod.rs"))
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A look that failed before any git ran cost the engine nothing, however long it waited: the next try comes with the retry tick. Before,
/// its whole wait counted as git work: a tree that did not answer for its 5 s put the next look 50 s after the start, and a look that
/// waited five minutes for the copy before it was followed by five minutes without a look.
#[test]
fn b823_look_a_failed_look_that_never_started_git_holds_no_look_back() {
    let c = Cadence::new(Some(s(15)));
    let t0 = Instant::now();
    // The file system of the work tree did not answer within its 5 s.
    let (from, to) = failed_look_counts(t0, None, None, t0 + s(5));
    assert_eq!((from, to), (t0, t0));
    let mut clock = CopyClock::default();
    clock.failed(from, to);
    assert_eq!(c.delay(&clock, false, t0 + s(15)), Duration::ZERO);
    // The copy before it had not ended, and the look gave up after five minutes of waiting.
    let (from, to) = failed_look_counts(t0, None, None, t0 + s(300));
    let mut clock = CopyClock::default();
    clock.failed(from, to);
    assert_eq!(c.delay(&clock, false, t0 + s(300)), Duration::ZERO);
    // A tool call is still floored from the start of the look.
    assert_eq!(c.delay(&clock, true, t0 + s(4)), s(11));
}

#[test]
fn b823_look_a_failed_look_counts_its_git_work_from_where_that_work_began() {
    let c = Cadence::new(Some(s(15)));
    let t0 = Instant::now();
    // The tree answered after 2 s, the git work ran 30 s and was given up: nine times those 30 s pass after it.
    let (from, to) = failed_look_counts(t0, Some(t0 + s(2)), None, t0 + s(32));
    assert_eq!((from, to), (t0 + s(2), t0 + s(32)));
    let mut clock = CopyClock::default();
    clock.failed(from, to);
    assert_eq!(c.delay(&clock, false, t0 + s(32)), s(270));
    // The git work ended after 0.2 s and the store of the copy failed 30 s later: the store is not counted.
    let (from, to) = failed_look_counts(t0, Some(t0 + s(2)), Some(t0 + ms(2_200)), t0 + s(32));
    assert_eq!((from, to), (t0 + s(2), t0 + ms(2_200)));
    let mut clock = CopyClock::default();
    clock.failed(from, to);
    assert_eq!(c.delay(&clock, false, t0 + s(32)), Duration::ZERO);
}

/// The copy task counts a failed look by that rule, and marks the start of the git work where the git work starts: after the question
/// to the file system and the wait for the copy before it.
#[test]
fn b823_look_the_copy_task_counts_a_failed_look_by_its_git_work() {
    let task = copy_task();
    assert!(
        task.contains(
            "bound.git_starts(); let looked = snapshotter.snapshot_until(&bound.git).await;"
        ),
        "the mark is set right before the git work"
    );
    assert!(task.contains("failed_look_counts("));
    assert!(
        task.contains("let (from, to) = bound.counted(started); clock.failed(from, to)"),
        "the clock of the copies takes what the rule counts"
    );
    assert!(!task.contains("fn counted_until"));
}

// ---- who wakes the task while the copy of a stop is tried again ---------------------------------------------------------------------

/// While the copy task tries the copy of a stop again, its looks have no floor and no spacing: the retry tick comes every 5 s. A tool
/// call must not wake the task then, or one look would follow each tool call, each with its full git work, for as long as the copies
/// fail. Nothing is lost by it: the retry tick comes sooner than the floor of a tool call's look.
#[test]
fn b823_look_no_tool_call_wakes_the_task_while_it_tries_the_copy_of_a_stop_again() {
    let c = Cadence::new(Some(s(15)));
    let clock = CopyClock::default();
    assert!(c.wakes(&clock));
    assert!(c.wakes_now(&clock, false));
    assert!(!c.wakes_now(&clock, true));
    // What never wakes the task does not start to: copies switched off, and a last bundle over 8 MiB.
    assert!(!Cadence::new(None).wakes_now(&clock, false));
    let t0 = Instant::now();
    let mut large = CopyClock::default();
    large.looked(t0, t0 + ms(200), Some(9 * 1024 * 1024));
    assert!(!c.wakes_now(&large, false));
    assert!(agentd::direct::snapshot::STOP_COPY_RETRY < COPY_FLOOR);
    // The copy task asks that question before it takes a tool call's wake, and its retry looks stay without a wait.
    let task = copy_task();
    assert!(
        task.contains(
            "() = wake.notified(), if cadence.wakes_now(&clock, trying_again) => Why::ToolCall,"
        ),
        "the copy task takes no wake of a tool call while it tries a stop copy again"
    );
    assert!(task.contains("if !matches!(why, Why::Ending(_)) && !trying_again {"));
}
