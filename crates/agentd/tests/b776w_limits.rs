//! Wave 2, rows C9, C30, C35 and C24: the `limits` member of the start request reaches the parts of agentd it moves, and a member left out
//! keeps the stage's value. Interrupts, questions and fresh sessions are per-run counts; the research budgets move the three byte limits of the
//! research module; the wall time of a code search is a stage setting.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_messages_world;
mod direct_support;

use agentd::direct::classify::Classified;
use agentd::direct::input::{AssessFacts, Assessment, InputPolicy, RunInputState, assess};
use agentd::direct::messages::{Mode, State};
use agentd::direct::request::check_limits_member;
use agentd::direct::research::settings::Settings as ResearchSettings;
use agentd::direct::tools::settings::Settings as ToolSettings;
use direct_messages_world::*;
use harness_protocol::api::internal::FaultHint;
use harness_protocol::direct::{DirectOutcome, RunLimits};
use harness_protocol::runtime::TurnOutcome;
use std::time::Duration;

const MIB: u64 = 1024 * 1024;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ---- C35: questions and fresh sessions -----------------------------------------------------------------------------------------------

fn no_changes() -> Classified {
    Classified {
        outcome: DirectOutcome::StoppedNoChanges,
        turn_outcome: TurnOutcome::Failed,
        detail: None,
        fault_hint: None::<FaultHint>,
    }
}

fn facts() -> AssessFacts {
    AssessFacts {
        client_alive: true,
        prompt_answered: true,
        attempts: 2,
        tool_calls: 41,
        files_changed: Some(0),
        stage_engine_stops: 0,
        run_s: 412,
        ..AssessFacts::default()
    }
}

#[test]
fn b776w_c35_the_defaults_are_three_questions_and_three_user_sessions_and_one_automatic_session() {
    let p = InputPolicy::new(secs(1800));
    assert_eq!(
        (p.max_requests, p.max_user_sessions, p.max_auto_sessions),
        (3, 3, 1)
    );
    assert_eq!(
        p.clone().with_limits(&RunLimits::default()),
        p,
        "a member left out keeps the stage values"
    );
}

#[test]
fn b776w_c35_a_questions_member_moves_the_fourth_question_from_finish_to_ask() {
    let used3 = RunInputState {
        requests: 3,
        ..RunInputState::default()
    };
    let left = secs(27_000);
    let stock = InputPolicy::new(secs(1800));
    assert!(
        matches!(
            assess(&no_changes(), &facts(), &used3, &stock, left),
            Assessment::Finish
        ),
        "TRIGGER PRESENT: with the stock three the fourth stop ends the run"
    );
    let limits = RunLimits {
        questions: Some(10),
        ..RunLimits::default()
    };
    let raised = stock.clone().with_limits(&limits);
    assert_eq!(raised.max_requests, 10);
    assert!(matches!(
        assess(&no_changes(), &facts(), &used3, &raised, left),
        Assessment::Ask(_)
    ));
    // The question after the tenth ends the run again: a count of 10 is a count.
    let used10 = RunInputState {
        requests: 10,
        ..RunInputState::default()
    };
    assert!(matches!(
        assess(&no_changes(), &facts(), &used10, &raised, left),
        Assessment::Finish
    ));
}

#[test]
fn b776w_c35_zero_questions_means_the_run_never_asks_and_never_means_without_limit() {
    let none = RunLimits {
        questions: Some(0),
        ..RunLimits::default()
    };
    let p = InputPolicy::new(secs(1800)).with_limits(&none);
    assert_eq!(p.max_requests, 0);
    assert!(matches!(
        assess(
            &no_changes(),
            &facts(),
            &RunInputState::default(),
            &p,
            secs(27_000)
        ),
        Assessment::Finish
    ));
}

#[test]
fn b776w_c35_fresh_sessions_move_only_the_users_count_and_not_the_automatic_one() {
    let limits = RunLimits {
        fresh_sessions: Some(9),
        ..RunLimits::default()
    };
    let p = InputPolicy::new(secs(1800)).with_limits(&limits);
    assert_eq!(p.max_user_sessions, 9);
    assert_eq!(p.max_auto_sessions, 1, "the automatic brake does not move");
    assert_eq!(p.max_requests, 3);
}

// ---- the request check -----------------------------------------------------------------------------------------------------------------

#[test]
fn b776w_c9_agentd_checks_the_member_again_and_names_the_member_and_never_the_value() {
    assert_eq!(check_limits_member(None), Ok(()));
    let good = RunLimits {
        interrupts: Some(20),
        research_run_bytes: Some(512 * MIB),
        ..RunLimits::default()
    };
    assert_eq!(check_limits_member(Some(&good)), Ok(()));
    let bad = RunLimits {
        questions: Some(4242),
        ..RunLimits::default()
    };
    let e = check_limits_member(Some(&bad)).unwrap_err();
    assert!(e.contains("questions") && e.contains("0 to 1000"), "{e}");
    assert!(!e.contains("4242"), "{e}");
}

// ---- C30: research budgets -------------------------------------------------------------------------------------------------------------

#[test]
fn b776w_c30_the_research_budgets_follow_the_member_and_the_others_stay() {
    let stage = ResearchSettings::default();
    assert_eq!(
        (
            stage.max_file_bytes,
            stage.max_unpacked_bytes,
            stage.max_run_bytes
        ),
        (MIB, 200 * MIB, 300 * MIB)
    );
    assert_eq!(stage.with_limits(&RunLimits::default()), stage);
    let l = RunLimits {
        research_file_bytes: Some(8 * MIB),
        research_unpacked_bytes: Some(600 * MIB),
        research_run_bytes: Some(900 * MIB),
        ..RunLimits::default()
    };
    let s = stage.with_limits(&l);
    assert_eq!(
        (s.max_file_bytes, s.max_unpacked_bytes, s.max_run_bytes),
        (8 * MIB, 600 * MIB, 900 * MIB)
    );
    // Everything else about the module is the stage's.
    assert_eq!(s.max_files, stage.max_files);
    assert_eq!(s.min_free_bytes, stage.min_free_bytes);
    assert_eq!(s.max_page_bytes, stage.max_page_bytes);
    // A single member moves only its own number.
    let one = stage.with_limits(&RunLimits {
        research_file_bytes: Some(2 * MIB),
        ..RunLimits::default()
    });
    assert_eq!(
        (one.max_file_bytes, one.max_unpacked_bytes),
        (2 * MIB, 200 * MIB)
    );
}

#[test]
fn b776w_c30_a_member_outside_the_range_is_cut_to_it_again_in_agentd() {
    let s = ResearchSettings::default().with_limits(&RunLimits {
        research_file_bytes: Some(10),
        research_unpacked_bytes: Some(u64::MAX),
        ..RunLimits::default()
    });
    assert_eq!(s.max_file_bytes, 64 * 1024);
    assert_eq!(s.max_unpacked_bytes, 1024 * MIB);
}

// ---- C24: the wall time of a code search -----------------------------------------------------------------------------------------------

fn tools(pairs: &'static [(&'static str, &'static str)]) -> ToolSettings {
    ToolSettings::from_lookup(&|k: &str| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| (*v).to_string())
    })
}

#[test]
fn b776w_c24_the_search_wall_time_is_ten_seconds_unless_the_stage_says_otherwise() {
    assert_eq!(tools(&[]).search_wall, secs(10));
    assert_eq!(
        tools(&[("AGENTD_TOOLS_SEARCH_WALL_S", "90")]).search_wall,
        secs(90)
    );
    // A value that cannot be used keeps the default; one outside 1 to 600 is cut to the range.
    assert_eq!(
        tools(&[("AGENTD_TOOLS_SEARCH_WALL_S", "soon")]).search_wall,
        secs(10)
    );
    assert_eq!(
        tools(&[("AGENTD_TOOLS_SEARCH_WALL_S", "0")]).search_wall,
        secs(1)
    );
    assert_eq!(
        tools(&[("AGENTD_TOOLS_SEARCH_WALL_S", "86400")]).search_wall,
        secs(600)
    );
}

// ---- C9: interrupts --------------------------------------------------------------------------------------------------------------------

/// The text of a message whose turn is long (`work_ms` of model work), so the next interrupt finds a prompt to cut.
fn long_turn(label: &str, work_ms: u64) -> String {
    format!(r#"{label} @fake{{"work_ms": {work_ms}}}"#)
}

#[tokio::test(flavor = "multi_thread")]
async fn b776w_c9_the_interrupt_cap_follows_the_member_and_a_member_left_out_keeps_the_stage_cap() {
    let w = World::start(&directive(r#"{"work_ms": 300}"#), &[], quick(), &pid_dir()).await;
    assert_eq!(w.hook.interrupt_cap(), 5, "the stage value");
    w.hook.apply_limits(&RunLimits::default());
    assert_eq!(w.hook.interrupt_cap(), 5, "a member left out");
    w.hook.apply_limits(&RunLimits {
        interrupts: Some(40),
        ..RunLimits::default()
    });
    assert_eq!(w.hook.interrupt_cap(), 40);
    w.hook.apply_limits(&RunLimits {
        interrupts: Some(0),
        ..RunLimits::default()
    });
    assert_eq!(w.hook.interrupt_cap(), 0, "0 is none, not unlimited");
}

#[tokio::test(flavor = "multi_thread")]
async fn b776w_c9_a_cap_of_one_interrupt_cuts_one_turn_and_the_next_now_message_waits() {
    let w = World::start(&directive(r#"{"work_ms": 1200}"#), &[], quick(), &pid_dir()).await;
    w.hook.apply_limits(&RunLimits {
        interrupts: Some(1),
        ..RunLimits::default()
    });
    let run = w.run();
    w.pump();
    w.until("the first prompt to be open", 20, |w| {
        w.count("prompt-open") == 1
    })
    .await;
    w.send("m0", &long_turn("long turn zero", 60_000), Mode::Next)
        .await;
    w.until("prompt 2 to be open", 30, |w| w.count("prompt-open") == 2)
        .await;
    w.send("m1", &long_turn("redirect 1", 4_000), Mode::Now)
        .await;
    w.until("interrupt 1 to take", 30, |w| {
        w.hook.stats().interrupts == 1
    })
    .await;
    w.until("prompt 3 to be open", 30, |w| w.count("prompt-open") == 3)
        .await;
    // TRIGGER PRESENT: one interrupt is spent, a prompt is open and has not ended.
    assert_eq!(w.count("prompt-end"), 2, "prompt 3 is still open");
    w.send("m2", r#"the second @fake{"say": "SECOND"}"#, Mode::Now)
        .await;
    let resp = run.await.unwrap();
    w.assert_serial();
    let log = w.log();
    assert_eq!(
        log.matches("\ncancel ").count(),
        1,
        "a second interrupt was made:\n{log}"
    );
    assert_eq!(w.hook.stats().interrupts, 1);
    assert_eq!(w.store.state("m2"), Some(State::Delivered { turn: 4 }));
    assert_eq!(
        w.d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{resp:?}"
    );
}
