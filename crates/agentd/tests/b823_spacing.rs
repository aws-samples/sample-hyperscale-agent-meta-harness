//! What the spacing of the copy task counts. It counts the time git took to make a copy, never the
//! uploads; it never holds a look back longer than five minutes, whatever the timer;
//! and a look that failed counts with the time its git work took, so one that
//! failed fast holds nothing back. Before, one
//! request to the bucket that hung made the task wait nine times as long with no look at the tree, while the run was told
//! "ham keeps trying". The whole-turn tests of the same rules are in `b823_copies_run.rs` (`b823_a_slow_upload_does_not_space_the_next_copy`,
//! `b823_after_a_copy_that_failed_the_next_look_comes_at_the_next_tick`).
#![allow(clippy::unwrap_used)]

use agentd::direct::snapshot::{COPY_SPACING, COPY_SPACING_CAP, Cadence, CopyClock};
use std::time::{Duration, Instant};

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Four tries of one upload that never answered: 4 x 120 s for the requests and 2 + 4 + 8 s of pauses.
const FOUR_TRIES_S: u64 = 494;

#[test]
fn b823_a_look_whose_store_failed_does_not_hold_the_next_one_back() {
    let c = Cadence::new(Some(s(60)));
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    // Its git work took 0.2 s; the four tries of its upload are not counted.
    clock.failed(t0, t0 + ms(200));
    // The timer's next tick looks at once: the run was told "ham keeps trying".
    assert_eq!(c.delay(&clock, false, t0 + s(FOUR_TRIES_S)), Duration::ZERO);
    // A tool call asks at once too: its floor counts from the start of the look, long ago.
    assert_eq!(c.delay(&clock, true, t0 + s(FOUR_TRIES_S)), Duration::ZERO);
}

#[test]
fn b823_the_spacing_is_capped_at_five_minutes() {
    let c = Cadence::new(Some(s(60)));
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    // Git took 100 s to make this copy: nine times that is 900 s. The cap is five minutes.
    clock.looked(t0, t0 + s(100), Some(900_000_000));
    assert_eq!(c.delay(&clock, false, t0 + s(100)), s(300));
    assert_eq!(c.delay(&clock, true, t0 + s(100)), s(300));
    assert_eq!(c.delay(&clock, false, t0 + s(400)), Duration::ZERO);
    // Under the cap nothing changes: 20 s of git is followed by 180 s without a look.
    clock.looked(t0, t0 + s(20), Some(900_000_000));
    assert_eq!(c.delay(&clock, false, t0 + s(20)), s(180));
    // A timer the user lengthened does not lengthen the cap: five minutes at the most (before, five periods of this timer, 900 s
    // here as nine times the 100 s of git).
    let slow = Cadence::new(Some(s(600)));
    clock.looked(t0, t0 + s(100), Some(900_000_000));
    assert_eq!(slow.delay(&clock, false, t0 + s(100)), s(300));
    // The default timer of 15 s has the same cap (five of its periods would be 75 s).
    let default = Cadence::new(Some(s(15)));
    assert_eq!(default.delay(&clock, false, t0 + s(100)), s(300));
    // A timer that a test set keeps five of its own periods (`Cadence::with_space_max`, set by the copy task).
    let quick = Cadence::new(Some(s(10))).with_space_max(s(10) * COPY_SPACING_CAP);
    assert_eq!(quick.delay(&clock, false, t0 + s(100)), s(50));
}

#[test]
fn b823_the_constants_of_the_spacing_are_the_ones_the_pages_state() {
    assert_eq!(COPY_SPACING, 9);
    assert_eq!(COPY_SPACING_CAP, 5);
}

#[test]
fn b823_a_look_that_failed_fast_is_still_floored_for_a_tool_call() {
    // A bucket that refuses at once must not make every tool call copy the tree again.
    let c = Cadence::new(Some(s(60)));
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    clock.failed(t0, t0 + ms(50));
    assert_eq!(c.delay(&clock, true, t0 + s(4)), s(11));
    assert_eq!(c.delay(&clock, false, t0 + s(4)), Duration::ZERO);
}

#[test]
fn b823_a_failed_look_keeps_the_size_of_the_last_stored_bundle() {
    let c = Cadence::new(Some(s(60)));
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    clock.looked(
        t0,
        t0 + ms(50),
        Some(agentd::direct::snapshot::WAKE_MAX_BUNDLE_BYTES + 1),
    );
    assert!(!c.wakes(&clock));
    clock.failed(t0 + s(60), t0 + s(60));
    assert!(
        !c.wakes(&clock),
        "a long history stays on the timer after a failed copy"
    );
}
