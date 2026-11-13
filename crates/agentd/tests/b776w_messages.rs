//! Wave 2, rows C8 and C9: the follow-up messages after the engine said done. The run deadline and the time that must remain bound them; the
//! old default of 8 message turns and 1,800 s of extra work is gone, and the interrupt count is a run member with the old number as its default.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::direct::messages::{Budget, Mode, Settings, State, Verdict};
use direct_messages_world::*;
use std::time::Duration;

#[test]
fn b776w_c8_the_standard_allowance_counts_neither_turns_nor_time() {
    let mut b = Budget::standard();
    b.note_stop(true);
    for k in 1..=60 {
        assert_eq!(
            b.verdict(Duration::from_secs(1_000_000)),
            Verdict::Allowed,
            "turn {k}"
        );
        assert!(b.turn_started());
        b.turn_ended(Duration::from_secs(3_600));
    }
    assert_eq!(b.turns(), 60);
    assert_eq!(b.exec(), Duration::from_secs(60 * 3_600));
}

#[test]
fn b776w_c8_only_the_time_that_must_remain_ends_the_follow_up() {
    let mut b = Budget::standard();
    b.note_stop(true);
    for _ in 0..20 {
        b.turn_started();
        b.turn_ended(Duration::from_secs(600));
    }
    assert_eq!(b.verdict(Duration::from_secs(299)), Verdict::TooLate);
    assert_eq!(b.verdict(Duration::from_secs(300)), Verdict::Allowed);
}

#[test]
fn b776w_c8_the_default_settings_carry_no_count() {
    let s = Settings::default();
    assert_eq!(s.max_turns, None);
    assert_eq!(s.max_exec, None);
    let none = |_: &str| -> Option<String> { None };
    let from_env = Settings::from_env(&none);
    assert_eq!(from_env.max_turns, None);
    assert_eq!(from_env.max_exec, None);
    assert_eq!(from_env.min_left, Duration::from_secs(300));
}

#[test]
fn b776w_c8_a_stage_may_still_set_a_guard_and_it_binds() {
    let get = |k: &str| match k {
        "AGENTD_MESSAGES_MAX_TURNS" => Some("2".to_string()),
        "AGENTD_MESSAGES_MAX_EXEC_S" => Some("90".to_string()),
        _ => None,
    };
    let s = Settings::from_env(&get);
    assert_eq!(s.max_turns, Some(2));
    assert_eq!(s.max_exec, Some(Duration::from_secs(90)));
    let mut b = Budget::new(s.max_turns, s.max_exec, s.min_left);
    b.note_stop(true);
    b.turn_started();
    b.turn_ended(Duration::from_secs(10));
    assert_eq!(b.verdict(Duration::from_secs(10_000)), Verdict::Allowed);
    b.turn_started();
    b.turn_ended(Duration::from_secs(10));
    assert_eq!(
        b.verdict(Duration::from_secs(10_000)),
        Verdict::ContinuationLimit
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b776w_c8_twelve_messages_after_done_are_all_delivered_with_the_default_settings() {
    let w = World::start(
        &directive(r#"{"work_ms": 300, "say": "DONE"}"#),
        &[],
        quick(),
        &pid_dir(),
    )
    .await;
    let run = w.run();
    w.pump();
    w.until("the first prompt", 20, |w| w.count("prompt-open") == 1)
        .await;
    for i in 1..=12 {
        w.send(
            &format!("m{i}"),
            &format!("message {i} @fake{{\"say\": \"DONE\"}}"),
            Mode::Next,
        )
        .await;
    }
    run.await.unwrap();
    w.assert_serial();
    let delivered = (1..=12)
        .filter(|i| {
            matches!(
                w.store.state(&format!("m{i}")),
                Some(State::Delivered { .. })
            )
        })
        .count();
    assert_eq!(delivered, 12, "{:?}", w.store.states());
    assert_eq!(
        w.d.prompts_received().len(),
        13,
        "the task and twelve messages"
    );
}
