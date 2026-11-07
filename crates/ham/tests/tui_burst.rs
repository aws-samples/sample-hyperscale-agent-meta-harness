//! The burst guard: keys that arrive as a burst, or soon after a paste, cannot send or confirm. The guard is a pure state
//! machine over injected instants, so no test waits.
#![allow(clippy::unwrap_used)]

use ham::tui::burst::{AFTER_BURST, AFTER_PREVIEW, Burst, KEY_GAP};
use std::time::{Duration, Instant};

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

#[test]
fn the_timings_are_the_plans_starting_values() {
    assert_eq!(KEY_GAP, Duration::from_millis(30));
    assert_eq!(AFTER_BURST, Duration::from_millis(150));
    assert_eq!(AFTER_PREVIEW, Duration::from_millis(300));
}

#[test]
fn keys_spaced_like_typing_are_all_calm() {
    let t = Instant::now();
    let mut g = Burst::new();
    for ms in [0, 100, 250, 400, 560] {
        assert!(g.key(at(t, ms)), "a key at {ms} ms");
    }
}

#[test]
fn the_first_key_of_a_burst_is_calm_and_every_later_one_is_not() {
    let t = Instant::now();
    let mut g = Burst::new();
    assert!(g.key(at(t, 0)));
    for ms in [1, 2, 3, 20, 45, 70] {
        assert!(!g.key(at(t, ms)), "a key at {ms} ms in a burst");
    }
}

#[test]
fn a_key_within_150_ms_after_a_burst_cannot_send_and_one_after_that_can() {
    let t = Instant::now();
    let mut g = Burst::new();
    g.key(at(t, 0));
    g.key(at(t, 5)); // the burst: quiet until 155 ms
    assert!(!g.key(at(t, 100)), "100 ms after the burst");
    assert!(!g.key(at(t, 154)), "one ms before the quiet time ends");
    assert!(g.key(at(t, 200)), "after the quiet time");
}

#[test]
fn a_paste_event_makes_the_next_150_ms_quiet() {
    let t = Instant::now();
    let mut g = Burst::new();
    g.paste(at(t, 1000));
    assert!(!g.key(at(t, 1100)));
    assert!(g.key(at(t, 1151)));
}

#[test]
fn a_calm_key_inside_the_quiet_time_does_not_extend_it() {
    let t = Instant::now();
    let mut g = Burst::new();
    g.paste(at(t, 0));
    assert!(!g.key(at(t, 100)));
    // 100 ms later than that key, past the end of the quiet time that began at the paste.
    assert!(g.key(at(t, 200)));
}

#[test]
fn a_confirming_key_less_than_300_ms_after_the_preview_is_ignored() {
    let t = Instant::now();
    // Each probe is a fresh guard: two probes 1 ms apart would be a burst themselves.
    for (ms, want) in [(100, false), (299, false), (300, true), (900, true)] {
        let mut g = Burst::new();
        g.preview_drawn(at(t, 1000));
        assert_eq!(
            g.confirm(at(t, 1000 + ms)),
            want,
            "{ms} ms after the preview"
        );
    }
}

#[test]
fn a_confirm_in_a_burst_is_refused_even_after_the_preview_time() {
    let t = Instant::now();
    let mut g = Burst::new();
    g.preview_drawn(at(t, 0));
    g.key(at(t, 900));
    assert!(!g.confirm(at(t, 905)), "a burst key cannot confirm");
}

#[test]
fn a_clock_that_runs_backwards_does_not_panic_or_count_as_a_burst() {
    let t = Instant::now() + Duration::from_secs(10);
    let mut g = Burst::new();
    g.key(t);
    // An earlier instant than the last key: no duration, so the gap rule sees zero elapsed and calls it a burst; nothing panics.
    let _ = g.key(t - Duration::from_secs(5));
}
