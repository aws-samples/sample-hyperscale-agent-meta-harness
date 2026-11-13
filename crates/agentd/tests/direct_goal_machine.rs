//! The rules of the goal-loop supervisor, pure: which round ends a run, when a "continue" is
//! sent, when a done claim counts. No file, no engine, no clock of its own: the tests move time by hand. The scenarios are the ones the
//! plugin fit study ran against the real engine with a fake model (S, T, U, W, Y, P).
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::machine::{Brake, Decision, Fp, Limits, Machine, Obs, Rule};
use agentd::direct::goal::rounds::RoundEnd;
use std::time::{Duration, Instant};

struct Clock(Instant);

impl Clock {
    fn new() -> Self {
        Self(Instant::now())
    }
    fn at(&self, s: u64) -> Instant {
        self.0 + Duration::from_secs(s)
    }
    fn ms(&self, ms: u64) -> Instant {
        self.0 + Duration::from_millis(ms)
    }
}

fn end(n: u64, status: &str, iterations: u32, text: &str) -> RoundEnd {
    RoundEnd {
        n,
        status: status.to_string(),
        iterations,
        text: text.to_string(),
        error: None,
    }
}

fn limits() -> Limits {
    Limits::default()
}

/// A round that ran: start, then end with the given fingerprint.
#[allow(clippy::too_many_arguments)]
fn round(
    m: &mut Machine,
    c: &Clock,
    s: u64,
    n: u64,
    status: &str,
    iterations: u32,
    text: &str,
    fp: Fp,
) {
    m.observe(c.at(s), Obs::Start { n });
    m.observe(
        c.at(s),
        Obs::End {
            end: end(n, status, iterations, text),
            fp,
        },
    );
}

fn fp(s: &str) -> Fp {
    Fp::Is(s.to_string())
}

fn brake(d: Decision) -> Brake {
    match d {
        Decision::Brake(b) => b,
        other => panic!("expected a brake, got {other:?}"),
    }
}

#[test]
fn defaults_are_the_numbers_of_the_decision_record() {
    let l = Limits::default();
    assert_eq!(l.blocked_rounds, 2);
    assert_eq!(l.stall_rounds, 3);
    assert_eq!(l.idle_rounds, 5);
    assert_eq!(l.kick_after, Duration::from_secs(15));
    assert_eq!(l.dead_grace, Duration::from_secs(2));
    assert_eq!(l.fallback_quiet, Duration::from_secs(15));
    assert_eq!(l.cleared_confirm, Duration::from_secs(2));
    assert_eq!(l.kicks_max, 2);
    assert_eq!(l.settle, Duration::from_secs(3));
}

// ---- the done claim ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_done_claim_counts_only_when_the_round_that_made_it_ended_with_a_message_and_a_quiet_moment_passed()
 {
    let c = Clock::new();
    let mut m = Machine::new(limits()).with_base(Some("t0".into()));
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    round(
        &mut m,
        &c,
        1,
        2,
        "completed",
        3,
        "Not sure yet; running the checks.",
        fp("t2"),
    );
    // The verification round starts, the model calls the completion tool, the round is still open.
    m.observe(c.at(2), Obs::Start { n: 3 });
    m.observe(
        c.at(3),
        Obs::Candidate {
            summary: Some("all done".into()),
        },
    );
    assert_eq!(
        m.decide(c.at(3)),
        Decision::Wait,
        "the round that made the call has not ended"
    );
    m.observe(
        c.at(4),
        Obs::End {
            end: end(3, "completed", 2, "Goal marked complete."),
            fp: Fp::Same,
        },
    );
    assert_eq!(
        m.decide(c.at(5)),
        Decision::Wait,
        "settling: a new round could still start"
    );
    assert_eq!(
        m.decide(c.at(7)),
        Decision::Done {
            summary: Some("all done".into())
        }
    );
}

#[test]
fn a_done_claim_whose_round_was_aborted_is_not_a_done_claim() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    m.observe(c.at(1), Obs::Start { n: 2 });
    m.observe(c.at(2), Obs::Candidate { summary: None });
    m.observe(
        c.at(3),
        Obs::End {
            end: end(2, "aborted", 5, "Goal marked complete."),
            fp: Fp::Same,
        },
    );
    let b = brake(m.decide(c.at(4)));
    assert_eq!(b.rule, Rule::Unfinished);
    assert!(
        b.detail.starts_with("The harness ended the run:"),
        "{}",
        b.detail
    );
    assert!(b.detail.contains("aborted"), "{}", b.detail);
}

#[test]
fn a_done_claim_whose_round_ended_with_no_message_is_not_a_done_claim() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    m.observe(c.at(1), Obs::Start { n: 2 });
    m.observe(c.at(2), Obs::Candidate { summary: None });
    m.observe(
        c.at(3),
        Obs::End {
            end: end(2, "completed", 2, "   \n  "),
            fp: Fp::Same,
        },
    );
    let b = brake(m.decide(c.at(4)));
    assert_eq!(b.rule, Rule::Unfinished);
    assert!(b.detail.contains("no message"), "{}", b.detail);
}

#[test]
fn a_new_round_during_the_settling_moment_cancels_the_done_claim_until_it_ends() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    m.observe(c.at(1), Obs::Start { n: 2 });
    m.observe(c.at(2), Obs::Candidate { summary: None });
    m.observe(
        c.at(3),
        Obs::End {
            end: end(2, "completed", 2, "Goal marked complete."),
            fp: Fp::Same,
        },
    );
    m.observe(c.at(4), Obs::Start { n: 3 });
    assert_eq!(
        m.decide(c.at(10)),
        Decision::Wait,
        "a round is open: the claim is judged by the last round that ends"
    );
    m.observe(
        c.at(11),
        Obs::End {
            end: end(3, "completed", 1, "Anything else?"),
            fp: Fp::Same,
        },
    );
    assert_eq!(m.decide(c.at(15)), Decision::Done { summary: None });
}

#[test]
fn without_the_observer_a_done_claim_needs_a_quiet_engine_and_a_goal_that_left_the_state_file() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    m.observe(
        c.at(0),
        Obs::Candidate {
            summary: Some("all done".into()),
        },
    );
    m.observe(c.at(0), Obs::Line);
    m.goal_seen(c.at(0), false);
    assert_eq!(m.decide(c.at(14)), Decision::Wait);
    m.observe(c.at(14), Obs::Line);
    assert_eq!(
        m.decide(c.at(20)),
        Decision::Wait,
        "a line at 14 s restarted the quiet"
    );
    assert_eq!(
        m.decide(c.at(29)),
        Decision::Done {
            summary: Some("all done".into())
        }
    );
}

#[test]
fn without_the_observer_a_done_claim_with_the_goal_still_active_is_not_done() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    m.observe(c.at(0), Obs::Candidate { summary: None });
    m.goal_seen(c.at(0), true);
    assert_eq!(m.decide(c.at(60)), Decision::Wait);
}

// ---- BLOCKED twice ----------------------------------------------------------------------------------------------------------

#[test]
fn two_identical_blocked_lines_in_a_row_end_the_run_and_the_detail_quotes_the_line() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        6,
        "Tried three fixes.\nBLOCKED: need the API key",
        fp("t1"),
    );
    assert_eq!(
        m.decide(c.at(1)),
        Decision::Wait,
        "one BLOCKED line is not enough"
    );
    round(
        &mut m,
        &c,
        1,
        2,
        "completed",
        1,
        "BLOCKED: need the API key\n",
        fp("t1"),
    );
    let b = brake(m.decide(c.at(2)));
    assert_eq!(b.rule, Rule::Blocked);
    assert!(
        b.detail.starts_with("The harness ended the run:"),
        "{}",
        b.detail
    );
    assert!(b.detail.contains("2 rounds in a row"), "{}", b.detail);
    assert!(
        b.detail.contains("\"BLOCKED: need the API key\""),
        "{}",
        b.detail
    );
}

#[test]
fn the_blocked_comparison_ignores_case_and_surrounding_spaces() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        2,
        "x\n  BLOCKED: Need the API key  ",
        fp("t1"),
    );
    round(
        &mut m,
        &c,
        1,
        2,
        "completed",
        2,
        "BLOCKED: need the api key",
        fp("t2"),
    );
    assert_eq!(brake(m.decide(c.at(2))).rule, Rule::Blocked);
}

#[test]
fn blocked_lines_that_differ_do_not_end_the_run() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    for i in 1..=6u64 {
        round(
            &mut m,
            &c,
            i,
            i,
            "completed",
            2,
            &format!("BLOCKED: need the API key v{i}"),
            fp(&format!("t{i}")),
        );
        assert_eq!(m.decide(c.at(i)), Decision::Wait, "round {i}");
    }
}

#[test]
fn a_round_between_two_blocked_lines_that_is_not_blocked_resets_the_count() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        3,
        "BLOCKED: need the key",
        fp("a"),
    );
    round(
        &mut m,
        &c,
        1,
        2,
        "completed",
        3,
        "Working on a workaround.",
        fp("b"),
    );
    round(
        &mut m,
        &c,
        2,
        3,
        "completed",
        3,
        "BLOCKED: need the key",
        fp("c"),
    );
    assert_eq!(m.decide(c.at(3)), Decision::Wait);
}

#[test]
fn a_blocked_line_must_start_the_last_non_empty_line() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        2,
        "BLOCKED: need the key\nand some more words",
        fp("a"),
    );
    round(
        &mut m,
        &c,
        1,
        2,
        "completed",
        2,
        "BLOCKED: need the key\nand some more words",
        fp("b"),
    );
    assert_eq!(m.decide(c.at(2)), Decision::Wait);
}

// ---- stalls ---------------------------------------------------------------------------------------------------------------------

#[test]
fn three_text_only_rounds_with_an_unchanged_tree_end_the_run() {
    let c = Clock::new();
    let mut m = Machine::new(limits()).with_base(Some("t0".into()));
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        5,
        "I did the work.",
        fp("t1"),
    );
    for i in 2..=3u64 {
        round(
            &mut m,
            &c,
            i,
            i,
            "completed",
            1,
            "Not sure yet; checking.",
            Fp::Same,
        );
        assert_eq!(m.decide(c.at(i)), Decision::Wait, "round {i}");
    }
    round(
        &mut m,
        &c,
        4,
        4,
        "completed",
        1,
        "Not sure yet; checking.",
        Fp::Same,
    );
    // two text-only idle rounds so far plus this one
    let b = brake(m.decide(c.at(5)));
    assert_eq!(b.rule, Rule::TextOnly);
    assert!(b.detail.contains("3 rounds in a row"), "{}", b.detail);
    assert!(
        b.detail.starts_with("The harness ended the run:"),
        "{}",
        b.detail
    );
}

#[test]
fn a_changed_tree_resets_the_stall_counters() {
    let c = Clock::new();
    let mut m = Machine::new(limits()).with_base(Some("t0".into()));
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        5,
        "I did the work.",
        fp("t1"),
    );
    round(&mut m, &c, 1, 2, "completed", 1, "checking", Fp::Same);
    round(&mut m, &c, 2, 3, "completed", 1, "checking", Fp::Same);
    round(&mut m, &c, 3, 4, "completed", 1, "now with a fix", fp("t2"));
    round(&mut m, &c, 4, 5, "completed", 1, "checking", Fp::Same);
    round(&mut m, &c, 5, 6, "completed", 1, "checking", Fp::Same);
    assert_eq!(
        m.decide(c.at(6)),
        Decision::Wait,
        "two text-only rounds since the last change"
    );
}

#[test]
fn an_unknown_fingerprint_counts_as_progress_never_as_nothing_changed() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    for i in 1..=8u64 {
        round(&mut m, &c, i, i, "completed", 1, "checking", Fp::Unknown);
        assert_eq!(m.decide(c.at(i)), Decision::Wait, "round {i}");
    }
}

#[test]
fn the_first_round_is_compared_with_the_tree_at_the_start() {
    let c = Clock::new();
    let mut m = Machine::new(limits()).with_base(Some("t0".into()));
    // a work round that changed nothing at all counts as idle
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        1,
        "Done.",
        Fp::Is("t0".into()),
    );
    round(&mut m, &c, 1, 2, "completed", 1, "Done.", Fp::Same);
    round(&mut m, &c, 2, 3, "completed", 1, "Done.", Fp::Same);
    assert_eq!(brake(m.decide(c.at(3))).rule, Rule::TextOnly);
}

#[test]
fn rounds_with_tool_calls_and_no_change_end_the_run_after_five() {
    let c = Clock::new();
    let mut m = Machine::new(limits()).with_base(Some("t0".into()));
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        6,
        "I did the work.",
        fp("t1"),
    );
    for i in 2..=5u64 {
        round(
            &mut m,
            &c,
            i,
            i,
            "completed",
            3,
            "The checks still pass; not sure.",
            Fp::Same,
        );
        assert_eq!(m.decide(c.at(i)), Decision::Wait, "round {i}");
    }
    round(
        &mut m,
        &c,
        6,
        6,
        "completed",
        3,
        "The checks still pass; not sure.",
        Fp::Same,
    );
    let b = brake(m.decide(c.at(7)));
    assert_eq!(b.rule, Rule::NoProgress);
    assert!(b.detail.contains("5 rounds in a row"), "{}", b.detail);
}

#[test]
fn several_rounds_read_together_credit_the_change_to_the_last_round_so_no_brake_fires_inside_the_batch()
 {
    let c = Clock::new();
    let mut m = Machine::new(limits()).with_base(Some("t0".into()));
    // five fast text-only rounds arrive in one read; the tree changed somewhere in them: the last carries the new fingerprint
    for i in 1..=4u64 {
        m.observe(c.ms(i * 10), Obs::Start { n: i });
        m.observe(
            c.ms(i * 10),
            Obs::End {
                end: end(i, "completed", 1, "checking"),
                fp: Fp::Same,
            },
        );
    }
    m.observe(c.ms(50), Obs::Start { n: 5 });
    m.observe(
        c.ms(50),
        Obs::End {
            end: end(5, "completed", 1, "fixed it"),
            fp: fp("t1"),
        },
    );
    assert_eq!(m.decide(c.ms(60)), Decision::Wait);
}

// ---- the dead loop and the "continue" ------------------------------------------------------------------------------------------------

#[test]
fn a_round_that_ended_aborted_gets_one_continue_after_the_grace_and_not_before() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    m.observe(c.at(1), Obs::Start { n: 2 });
    m.observe(
        c.at(5),
        Obs::End {
            end: end(2, "aborted", 5, ""),
            fp: Fp::Same,
        },
    );
    assert_eq!(
        m.decide(c.at(5)),
        Decision::Wait,
        "the grace has not passed"
    );
    assert_eq!(m.decide(c.at(8)), Decision::Kick);
    m.kicked(c.at(8));
    assert_eq!(m.decide(c.at(9)), Decision::Wait, "one kick at a time");
}

#[test]
fn a_completed_round_with_no_new_round_gets_a_continue_after_fifteen_seconds() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    assert_eq!(
        m.decide(c.at(14)),
        Decision::Wait,
        "the plugin queues its follow-up within a moment: 15 s is a dead loop"
    );
    assert_eq!(m.decide(c.at(15)), Decision::Kick);
}

#[test]
fn a_round_in_flight_is_never_kicked() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    m.observe(c.at(1), Obs::Start { n: 2 });
    assert!(m.open_round());
    assert_eq!(m.decide(c.at(10_000)), Decision::Wait);
}

#[test]
fn a_goal_that_is_no_longer_active_is_not_kicked() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    m.goal_seen(c.at(1), false);
    assert_ne!(m.decide(c.at(30)), Decision::Kick);
}

#[test]
fn a_continue_that_starts_a_round_lets_the_next_dead_loop_be_kicked_again_until_the_limit() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    assert_eq!(m.decide(c.at(20)), Decision::Kick);
    m.kicked(c.at(20));
    // the continue starts a round that is stopped by the engine's loop guard again, with nothing changed
    round(&mut m, &c, 21, 2, "aborted", 5, "", Fp::Same);
    assert_eq!(m.decide(c.at(24)), Decision::Kick, "the second continue");
    m.kicked(c.at(24));
    round(&mut m, &c, 25, 3, "aborted", 5, "", Fp::Same);
    let b = brake(m.decide(c.at(28)));
    assert_eq!(
        b.rule,
        Rule::DeadLoop,
        "no third continue: the loop stopped twice with no change"
    );
    assert!(b.detail.contains("2 times in a row"), "{}", b.detail);
    assert!(b.detail.contains("aborted"), "{}", b.detail);
}

#[test]
fn a_change_of_the_tree_gives_the_continues_back() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    assert_eq!(m.decide(c.at(20)), Decision::Kick);
    m.kicked(c.at(20));
    round(&mut m, &c, 21, 2, "completed", 6, "fixed", fp("t2"));
    assert_eq!(m.decide(c.at(40)), Decision::Kick);
    m.kicked(c.at(40));
    round(&mut m, &c, 41, 3, "completed", 6, "fixed more", fp("t3"));
    assert_eq!(
        m.decide(c.at(60)),
        Decision::Kick,
        "progress between the continues: still the first of its kind"
    );
}

#[test]
fn a_continue_with_no_round_after_it_is_asked_again_after_the_quiet_time_and_counts() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    assert_eq!(m.decide(c.at(15)), Decision::Kick);
    m.kicked(c.at(15));
    assert_eq!(m.decide(c.at(29)), Decision::Wait);
    assert_eq!(m.decide(c.at(30)), Decision::Kick);
    m.kicked(c.at(30));
    assert_eq!(brake(m.decide(c.at(45))).rule, Rule::DeadLoop);
}

// ---- the state file ------------------------------------------------------------------------------------------------------------------

#[test]
fn a_goal_missing_from_the_state_file_with_no_claim_is_reported_once_it_is_confirmed() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    m.goal_seen(c.at(1), false);
    assert_eq!(
        m.decide(c.at(2)),
        Decision::Wait,
        "a read in the middle of a write looks the same"
    );
    m.goal_seen(c.at(4), false);
    let b = brake(m.decide(c.at(4)));
    assert_eq!(b.rule, Rule::Cleared);
    assert!(
        b.detail.starts_with("The harness ended the run:"),
        "{}",
        b.detail
    );
}

#[test]
fn a_goal_that_is_active_again_forgets_the_missing_reading() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        4,
        "I did the work.",
        fp("t1"),
    );
    m.goal_seen(c.at(1), false);
    m.goal_seen(c.at(2), true);
    m.goal_seen(c.at(4), true);
    assert_eq!(m.decide(c.at(4)), Decision::Wait);
}

#[test]
fn a_cleared_goal_with_a_done_claim_is_the_normal_end_not_a_report() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    m.observe(c.at(0), Obs::Start { n: 1 });
    m.goal_seen(c.at(1), false); // the plugin removes the goal before the tool result reaches the client
    m.observe(c.at(2), Obs::Candidate { summary: None });
    m.observe(
        c.at(3),
        Obs::End {
            end: end(1, "completed", 2, "Goal marked complete."),
            fp: Fp::Same,
        },
    );
    m.goal_seen(c.at(5), false);
    assert_eq!(m.decide(c.at(7)), Decision::Done { summary: None });
}

// ---- precedence ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_brake_that_fired_stays_the_answer() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        6,
        "BLOCKED: need the key",
        fp("a"),
    );
    round(
        &mut m,
        &c,
        1,
        2,
        "completed",
        1,
        "BLOCKED: need the key",
        fp("a"),
    );
    let first = brake(m.decide(c.at(2)));
    let again = brake(m.decide(c.at(3)));
    assert_eq!(first, again);
}

#[test]
fn blocked_wins_over_a_text_only_stall_that_fires_in_the_same_round() {
    let c = Clock::new();
    let mut m = Machine::new(limits()).with_base(Some("a".into()));
    round(
        &mut m,
        &c,
        0,
        1,
        "completed",
        1,
        "BLOCKED: need the key",
        Fp::Is("a".into()),
    );
    round(
        &mut m,
        &c,
        1,
        2,
        "completed",
        1,
        "BLOCKED: need the key",
        Fp::Same,
    );
    round(
        &mut m,
        &c,
        2,
        3,
        "completed",
        1,
        "BLOCKED: need the key",
        Fp::Same,
    );
    assert_eq!(brake(m.decide(c.at(3))).rule, Rule::Blocked);
}

// ---- the words of the rules and the claim flag ------------------------------------------------------------------------------------

#[test]
fn every_rule_has_its_own_short_word_for_events_and_logs() {
    let words: Vec<_> = [
        Rule::Blocked,
        Rule::TextOnly,
        Rule::NoProgress,
        Rule::DeadLoop,
        Rule::Unfinished,
        Rule::Cleared,
        Rule::RoundNoProgress,
    ]
    .into_iter()
    .map(Rule::word)
    .collect();
    assert_eq!(
        words,
        [
            "blocked-twice",
            "text-only-stall",
            "no-progress",
            "dead-loop",
            "unfinished-claim",
            "goal-cleared",
            "round-no-progress"
        ]
    );
    let mut unique = words.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), words.len());
}

#[test]
fn a_claim_seen_on_the_wire_is_a_candidate_and_only_the_first_one_counts() {
    let c = Clock::new();
    let mut m = Machine::new(limits());
    assert!(!m.has_candidate());
    m.observe(
        c.at(1),
        Obs::Candidate {
            summary: Some("first".into()),
        },
    );
    assert!(m.has_candidate());
    m.observe(
        c.at(2),
        Obs::Candidate {
            summary: Some("second".into()),
        },
    );
    assert!(m.has_candidate());
}
