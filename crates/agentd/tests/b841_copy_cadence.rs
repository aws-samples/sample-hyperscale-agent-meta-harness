//! Two parts of the copy cadence must not grow with the timer a user lengthens
//! (`run.copy_every_minutes`), because neither saves the cost the key is for. A copy that failed is tried again after the default minute
//! at the latest, whatever the timer: before, the next try was the next tick, so a failure at minute 60 of an hourly timer left the work
//! without a copy until minute 120. And the spacing after a slow copy is capped in absolute time, five minutes, whatever the timer: before,
//! the cap was five timer periods, five hours under an hourly timer. With the default timer of one minute both numbers are what they were.
//! The whole-turn test of the retry is `b841_copy_a_copy_that_failed_is_tried_again_before_the_next_tick_of_a_long_timer` in
//! `b823_copies_run.rs`.
#![allow(clippy::unwrap_used)]

use agentd::direct::snapshot::{
    COPY_RETRY, COPY_SPACING_CAP, COPY_SPACING_MAX, Cadence, CopyClock,
};
use harness_protocol::direct::{DEFAULT_SNAPSHOT_EVERY_S, MAX_SNAPSHOT_EVERY_S};
use std::time::{Duration, Instant};

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn b841_copy_the_retry_time_is_the_default_timer_and_the_most_spacing_is_five_of_them() {
    assert_eq!(COPY_RETRY, s(u64::from(DEFAULT_SNAPSHOT_EVERY_S)));
    assert_eq!(COPY_SPACING_MAX, COPY_RETRY * COPY_SPACING_CAP);
    assert_eq!(COPY_SPACING_MAX, s(300), "five minutes, as the pages state");
}

#[test]
fn b841_copy_a_look_that_failed_is_tried_again_after_the_default_minute_whatever_the_timer() {
    let hour = Cadence::new(Some(s(u64::from(MAX_SNAPSHOT_EVERY_S))));
    assert_eq!(
        hour.period(false),
        Some(s(3_600)),
        "a look that stored a copy, or found nothing new, is followed by the timer of the run"
    );
    assert_eq!(
        hour.period(true),
        Some(s(60)),
        "a copy that failed at minute 60 is tried again at minute 61, not at minute 120"
    );
    // The default timer: what it was.
    let minute = Cadence::new(Some(s(60)));
    assert_eq!(
        (minute.period(false), minute.period(true)),
        (Some(s(60)), Some(s(60)))
    );
    // A timer shorter than the retry time (the tests set one): the timer.
    assert_eq!(Cadence::new(Some(ms(300))).period(true), Some(ms(300)));
    // The looks are off: no timer at all.
    assert_eq!(Cadence::new(None).period(true), None);
    assert_eq!(Cadence::new(None).period(false), None);
    // What a test set in place of the minute.
    assert_eq!(hour.with_retry(ms(300)).period(true), Some(ms(300)));
    assert_eq!(hour.with_retry(ms(300)).period(false), Some(s(3_600)));
}

#[test]
fn b841_copy_the_spacing_after_a_slow_copy_is_capped_at_five_minutes_whatever_the_timer() {
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    // Git took 34 minutes to make this copy: nine times that is over five hours.
    let git = s(34 * 60);
    clock.looked(t0, t0 + git, Some(900_000_000));
    for every in [60, 600, 3_600] {
        let c = Cadence::new(Some(s(every)));
        assert_eq!(
            c.delay(&clock, false, t0 + git),
            s(300),
            "a timer of {every} s: the next look is held back five minutes at the most"
        );
        assert_eq!(c.delay(&clock, false, t0 + git + s(300)), Duration::ZERO);
    }
    // A timer shorter than a minute (the tests set one) keeps five of its own periods.
    assert_eq!(
        Cadence::new(Some(s(10))).delay(&clock, false, t0 + git),
        s(50)
    );
    // Under the cap nothing changes: 20 s of git is followed by 180 s without a look, under any timer.
    clock.looked(t0, t0 + s(20), Some(900_000_000));
    assert_eq!(
        Cadence::new(Some(s(3_600))).delay(&clock, false, t0 + s(20)),
        s(180)
    );
}
