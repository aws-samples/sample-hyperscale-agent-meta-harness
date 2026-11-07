//! The status line and its live meters, and the reconnect overlay. `StatusLine` words what the header of the run chat view
//! says: model, state, elapsed time, tokens and cost (labelled "as of turn k" unless the stream carries a live meter), a context bar,
//! and the age of the last read; it gives up its tail on a narrow screen. `Link` decides when a failing poll shows "connection lost,
//! retrying": after a grace of 1 second, or 5 on the first connect, and it clears on the next good read. Time is passed in, never read,
//! so the tests drive a virtual clock.

#![allow(clippy::unwrap_used)]

use ham_core::meters::{
    Context, GRACE_FIRST_MS, GRACE_LATER_MS, Link, LinkState, StatusLine, age_s, elapsed_s,
    tokens_text,
};
use proptest::prelude::*;

// ---- clocks -------------------------------------------------------------------------------------------------------------------------

#[test]
fn elapsed_runs_from_the_start_and_stops_at_the_end() {
    assert_eq!(elapsed_s(Some(1000), None, 1760), Some(760));
    assert_eq!(
        elapsed_s(Some(1000), Some(1500), 1760),
        Some(500),
        "frozen at the end"
    );
    assert_eq!(
        elapsed_s(Some(1000), Some(1500), 1200),
        Some(200),
        "not past now"
    );
    assert_eq!(
        elapsed_s(Some(1000), None, 900),
        Some(0),
        "a clock that runs behind never goes negative"
    );
    assert_eq!(elapsed_s(Some(1000), Some(900), 2000), Some(0));
    assert_eq!(elapsed_s(None, None, 5), None);
    assert_eq!(elapsed_s(None, Some(4), 5), None);
}

#[test]
fn the_age_of_the_last_read_counts_seconds_up_to_now() {
    assert_eq!(age_s(Some(100), 102), Some(2));
    assert_eq!(age_s(Some(100), 100), Some(0));
    assert_eq!(age_s(Some(100), 90), Some(0));
    assert_eq!(age_s(None, 100), None);
}

#[test]
fn token_counts_are_short() {
    for (n, want) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1k"),
        (1_500, "1.5k"),
        (9_999, "10k"),
        (10_000, "10k"),
        (113_456, "113k"),
        (999_999, "1M"),
        (1_000_000, "1M"),
        (1_300_000, "1.3M"),
        (25_000_000, "25M"),
    ] {
        assert_eq!(tokens_text(n), want, "{n}");
    }
}

// ---- the line -----------------------------------------------------------------------------------------------------------------------

fn full() -> StatusLine {
    StatusLine {
        model: Some("Claude Sonnet 5.5".into()),
        state: "active".into(),
        elapsed_s: Some(12 * 60 + 40),
        tokens: Some(113_456),
        cost: Some("$0.14".into()),
        as_of_turn: Some(3),
        live: false,
        context: Some(Context {
            used: 42_000,
            window: 100_000,
        }),
        age_s: Some(2),
    }
}

const WIDE: &str = "Claude Sonnet 5.5 · active · 12m40s · 113k tok · $0.14 · as of turn 3 · ctx [####------] 42% · updated 2s ago";

#[test]
fn the_whole_line_has_every_meter() {
    assert_eq!(full().line(200), WIDE);
    assert_eq!(full().line(WIDE.chars().count()), WIDE);
}

#[test]
fn totals_without_a_live_meter_say_which_turn_they_are_of() {
    let mut s = full();
    s.as_of_turn = None;
    assert!(
        s.line(200)
            .contains("113k tok · $0.14 · as of the last turn"),
        "{}",
        s.line(200)
    );
    s.live = true;
    let live = s.line(200);
    assert!(live.contains("113k tok · $0.14 · ctx"), "{live}");
    assert!(!live.contains("as of"), "{live}");
}

#[test]
fn the_line_gives_up_its_tail_in_order_and_never_cuts_a_meter_in_two_before_it_must() {
    let s = full();
    let at = |w: usize| s.line(w);
    let wide = WIDE.chars().count();
    // One column short: the bar gives way to the plain percentage.
    assert_eq!(
        at(wide - 1),
        "Claude Sonnet 5.5 · active · 12m40s · 113k tok · $0.14 · as of turn 3 · ctx 42% · updated 2s ago"
    );
    let compact = at(wide - 1).chars().count();
    assert_eq!(
        at(compact - 1),
        "Claude Sonnet 5.5 · active · 12m40s · 113k tok · $0.14 · as of turn 3 · ctx 42%"
    );
    let no_age = at(compact - 1).chars().count();
    assert_eq!(
        at(no_age - 1),
        "Claude Sonnet 5.5 · active · 12m40s · 113k tok · $0.14 · as of turn 3"
    );
    let no_ctx = at(no_age - 1).chars().count();
    assert_eq!(at(no_ctx - 1), "Claude Sonnet 5.5 · active · 12m40s");
    let no_totals = at(no_ctx - 1).chars().count();
    assert_eq!(at(no_totals - 1), "Claude Sonnet 5.5 · active");
    let no_elapsed = at(no_totals - 1).chars().count();
    assert_eq!(at(no_elapsed - 1), "active");
    // Under the state's own width the line is cut with an ellipsis.
    assert_eq!(at(4), "act…");
    assert_eq!(at(0), "");
}

#[test]
fn a_meter_that_is_missing_leaves_no_gap() {
    let s = StatusLine {
        state: "complete".into(),
        ..StatusLine::default()
    };
    assert_eq!(s.line(80), "complete");
    let s = StatusLine {
        state: "active".into(),
        tokens: Some(1_500),
        as_of_turn: Some(1),
        ..StatusLine::default()
    };
    assert_eq!(s.line(80), "active · 1.5k tok · as of turn 1");
    let s = StatusLine {
        state: "active".into(),
        cost: Some("USD unknown".into()),
        live: true,
        ..StatusLine::default()
    };
    assert_eq!(s.line(80), "active · USD unknown");
    assert_eq!(StatusLine::default().line(80), "");
}

#[test]
fn the_context_bar_rounds_and_is_bounded() {
    let with = |used: u64, window: u64| {
        let mut s = StatusLine {
            state: "a".into(),
            ..StatusLine::default()
        };
        s.context = Some(Context { used, window });
        s.line(80)
    };
    assert_eq!(with(0, 100), "a · ctx [----------] 0%");
    assert_eq!(with(4, 100), "a · ctx [----------] 4%");
    assert_eq!(with(5, 100), "a · ctx [#---------] 5%");
    assert_eq!(with(33, 99), "a · ctx [###-------] 33%");
    assert_eq!(with(94, 100), "a · ctx [#########-] 94%");
    assert_eq!(with(95, 100), "a · ctx [##########] 95%");
    assert_eq!(with(100, 100), "a · ctx [##########] 100%");
    assert_eq!(
        with(250, 100),
        "a · ctx [##########] 100%",
        "more than the window reads as full"
    );
    assert_eq!(with(50, 0), "a", "no window, no meter");
}

#[test]
fn text_from_outside_is_cleaned() {
    let s = StatusLine {
        model: Some("Claude\u{1b}[2J Opus\n5.5".into()),
        state: "act\u{7}ive".into(),
        cost: Some("$0.14\u{1b}]0;title\u{7}".into()),
        tokens: Some(10),
        as_of_turn: Some(1),
        ..StatusLine::default()
    };
    let line = s.line(200);
    assert_eq!(
        line,
        "Claude Opus 5.5 · active · 10 tok · $0.14 · as of turn 1"
    );
    assert!(line.chars().all(|c| !c.is_control()));
}

proptest! {
    #[test]
    fn any_line_fits_its_width_and_never_shows_unlabelled_totals(
        w in 0usize..140,
        live in any::<bool>(),
        turn in proptest::option::of(0u32..99),
        tokens in proptest::option::of(0u64..5_000_000),
        used in 0u64..300,
        window in 0u64..200,
    ) {
        let s = StatusLine {
            model: Some("Claude Sonnet 5.5".into()),
            state: "active".into(),
            elapsed_s: Some(3 * 3600 + 5),
            tokens,
            cost: Some("$1.00".into()),
            as_of_turn: turn,
            live,
            context: Some(Context { used, window }),
            age_s: Some(9),
        };
        let line = s.line(w);
        prop_assert!(line.chars().count() <= w, "{line:?} in {w}");
        if line.contains(" tok") && !live {
            prop_assert!(line.contains("as of"), "{line:?}");
        }
    }
}

// ---- the link -----------------------------------------------------------------------------------------------------------------------

#[test]
fn the_grace_is_one_second_and_five_on_the_first_connect() {
    assert_eq!(GRACE_LATER_MS, 1_000);
    assert_eq!(GRACE_FIRST_MS, 5_000);
}

#[test]
fn after_a_first_good_read_a_failing_poll_shows_the_overlay_after_one_second() {
    let mut l = Link::new();
    l.on_ok(0);
    assert_eq!(l.state(500), LinkState::Connected);
    l.on_err(10_000);
    assert_eq!(l.state(10_000), LinkState::Quiet);
    assert_eq!(l.state(10_999), LinkState::Quiet);
    assert!(l.overlay(10_999).is_none());
    assert_eq!(
        l.state(11_000),
        LinkState::Lost {
            for_ms: 1_000,
            first_connect: false
        }
    );
    assert_eq!(
        l.overlay(11_000).as_deref(),
        Some("Connection lost, retrying (1 s)")
    );
    // More failures do not move the start.
    l.on_err(11_500);
    l.on_err(12_000);
    assert_eq!(
        l.overlay(22_300).as_deref(),
        Some("Connection lost, retrying (12 s)")
    );
    assert_eq!(
        l.overlay(22_300 + 60_000).as_deref(),
        Some("Connection lost, retrying (1 min 12 s)")
    );
}

#[test]
fn before_any_good_read_the_grace_is_five_seconds_and_the_words_say_so() {
    let mut l = Link::new();
    assert_eq!(l.state(0), LinkState::Connected, "nothing has failed yet");
    l.on_err(2_000);
    assert_eq!(l.state(6_999), LinkState::Quiet);
    assert_eq!(
        l.state(7_000),
        LinkState::Lost {
            for_ms: 5_000,
            first_connect: true
        }
    );
    assert_eq!(
        l.overlay(7_000).as_deref(),
        Some("Cannot connect yet, retrying (5 s)")
    );
}

#[test]
fn a_good_read_clears_the_overlay_and_the_next_failure_starts_a_new_grace() {
    let mut l = Link::new();
    l.on_ok(0);
    l.on_err(1_000);
    assert!(l.overlay(3_000).is_some());
    l.on_ok(3_500);
    assert_eq!(l.state(3_500), LinkState::Connected);
    assert!(l.overlay(3_500).is_none());
    l.on_err(4_000);
    assert!(l.overlay(4_500).is_none(), "the grace starts again");
    assert!(l.overlay(5_000).is_some());
}

#[test]
fn a_failure_that_ends_inside_the_grace_never_shows() {
    let mut l = Link::new();
    l.on_ok(0);
    for i in 0..20u64 {
        l.on_err(i * 2_000);
        assert!(l.overlay(i * 2_000 + 900).is_none());
        l.on_ok(i * 2_000 + 900);
    }
}

#[test]
fn the_first_connect_keeps_its_long_grace_until_a_read_succeeds() {
    let mut l = Link::new();
    l.on_err(0);
    l.on_err(30_000);
    assert_eq!(
        l.state(30_000),
        LinkState::Lost {
            for_ms: 30_000,
            first_connect: true
        }
    );
    l.on_ok(31_000);
    l.on_err(40_000);
    assert_eq!(
        l.state(41_000),
        LinkState::Lost {
            for_ms: 1_000,
            first_connect: false
        }
    );
}

#[test]
fn the_age_of_the_last_good_read_follows_the_clock() {
    let mut l = Link::new();
    assert_eq!(l.age_s(5_000), None);
    l.on_ok(2_000);
    assert_eq!(l.age_s(2_000), Some(0));
    assert_eq!(l.age_s(4_999), Some(2));
    l.on_err(5_000);
    assert_eq!(
        l.age_s(12_000),
        Some(10),
        "a failing poll does not reset it"
    );
}

// ---- found by the second-opinion review ----------------------------------------------

#[test]
fn extreme_numbers_do_not_overflow() {
    assert!(tokens_text(u64::MAX).ends_with('M'));
    assert!(tokens_text(u64::MAX - 1).ends_with('M'));
    assert_eq!(elapsed_s(Some(i64::MIN), None, i64::MAX), Some(u64::MAX));
    assert_eq!(elapsed_s(Some(i64::MAX), None, i64::MIN), Some(0));
    assert_eq!(age_s(Some(i64::MIN), i64::MAX), Some(u64::MAX));
    let half = StatusLine {
        state: "a".into(),
        context: Some(Context {
            used: u64::MAX / 2,
            window: u64::MAX,
        }),
        ..StatusLine::default()
    };
    assert!(half.line(80).contains("50%"), "{}", half.line(80));
    let full = StatusLine {
        state: "a".into(),
        context: Some(Context {
            used: u64::MAX,
            window: u64::MAX,
        }),
        ..StatusLine::default()
    };
    assert!(full.line(80).contains("100%"), "{}", full.line(80));
}

#[test]
fn a_hundred_percent_means_the_window_is_full() {
    let with = |used: u64, window: u64| {
        let s = StatusLine {
            state: "a".into(),
            context: Some(Context { used, window }),
            ..StatusLine::default()
        };
        s.line(80)
    };
    assert_eq!(
        with(996, 1000),
        "a · ctx [##########] 99%",
        "99.6 is not full"
    );
    assert_eq!(with(999, 1000), "a · ctx [##########] 99%");
    assert_eq!(with(1000, 1000), "a · ctx [##########] 100%");
    assert_eq!(with(1001, 1000), "a · ctx [##########] 100%");
}
