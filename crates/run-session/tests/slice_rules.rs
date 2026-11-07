//! The pure rules of the slice loop. How long the next session may run and whether it is the last (`plan_timing`), how a session's end is
//! read (`classify`), and what the loop does after it (`decide`). The walk of the real state machine over these rules is `slice_loop.rs`.
#![allow(clippy::unwrap_used)]

use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::direct::{
    DEFAULT_MAX_RUN_S, DirectOutcome, MAX_EMPTY_SLICES, MAX_LOST_SLICES, MAX_MAX_RUN_S,
    MIN_MAX_RUN_S,
};
use run_session::slices::{
    EndKind, EndReason, LoopState, Timing, Verdict, classify, decide, plan_timing, slice_cap_s,
    total_s,
};
use serde_json::json;

const FULL: u32 = 27_900;

fn turn(finish: &str, handoff: Option<bool>) -> TurnCompleteRequest {
    let mut v = json!({"runId": "r", "agentId": "direct", "turn": 1, "finishReason": finish});
    if let Some(h) = handoff {
        v["handoff"] = json!(h);
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn b688_a_day_of_full_sessions_is_three_and_the_third_is_the_last() {
    let t1 = plan_timing(DEFAULT_MAX_RUN_S, 0, FULL).unwrap();
    assert_eq!(
        t1,
        Timing {
            engine_s: FULL,
            last: false
        }
    );
    let t2 = plan_timing(DEFAULT_MAX_RUN_S, 28_800, FULL).unwrap();
    assert_eq!(
        t2,
        Timing {
            engine_s: FULL,
            last: false
        }
    );
    let t3 = plan_timing(DEFAULT_MAX_RUN_S, 57_600, FULL).unwrap();
    assert_eq!(
        t3,
        Timing {
            engine_s: FULL,
            last: true
        },
        "nothing fits after the third"
    );
}

#[test]
fn b688_a_total_shorter_than_one_session_shortens_the_session_and_makes_it_the_last() {
    let t = plan_timing(3_600, 0, FULL).unwrap();
    assert_eq!(
        t,
        Timing {
            engine_s: 2_700,
            last: true
        },
        "the 900 s reserve comes off the total"
    );
    let least = plan_timing(MIN_MAX_RUN_S, 0, FULL).unwrap();
    assert_eq!(
        least,
        Timing {
            engine_s: 900,
            last: true
        }
    );
}

#[test]
fn b688_no_session_is_planned_when_less_than_a_useful_one_remains() {
    assert_eq!(
        plan_timing(DEFAULT_MAX_RUN_S, u64::from(DEFAULT_MAX_RUN_S), FULL),
        None
    );
    assert_eq!(
        plan_timing(
            DEFAULT_MAX_RUN_S,
            u64::from(DEFAULT_MAX_RUN_S - 1_799),
            FULL
        ),
        None,
        "reserve 900 and a least slice 900 are 1,800"
    );
    assert!(
        plan_timing(
            DEFAULT_MAX_RUN_S,
            u64::from(DEFAULT_MAX_RUN_S - 1_800),
            FULL
        )
        .is_some()
    );
}

#[test]
fn b688_a_test_limit_of_ten_minutes_shortens_every_session_and_its_least_with_it() {
    // The live proof: HAMH_SLICE_DEADLINE_S=600.
    assert_eq!(slice_cap_s(28_800, Some(600)), 600);
    assert_eq!(slice_cap_s(28_800, None), FULL);
    assert_eq!(slice_cap_s(28_800, Some(0)), FULL, "zero is no limit");
    assert_eq!(
        slice_cap_s(28_800, Some(40_000)),
        FULL,
        "a test limit never lengthens a session"
    );
    assert_eq!(
        slice_cap_s(3_600, None),
        2_700,
        "a shorter microVM lifetime shortens the session"
    );
    let t = plan_timing(4_500, 0, 600).unwrap();
    assert_eq!(
        t,
        Timing {
            engine_s: 600,
            last: false
        }
    );
    let last = plan_timing(4_500, 2_160, 600).unwrap();
    assert!(
        last.last,
        "2,340 s left: 600 + 900 reserve leaves 840, under the 1,500 another session needs: {last:?}"
    );
}

#[test]
fn b688_the_total_is_the_requests_when_it_is_usable_else_the_default() {
    assert_eq!(total_s(None), DEFAULT_MAX_RUN_S);
    assert_eq!(total_s(Some(36_000)), 36_000);
    assert_eq!(total_s(Some(MIN_MAX_RUN_S)), MIN_MAX_RUN_S);
    assert_eq!(total_s(Some(MAX_MAX_RUN_S)), MAX_MAX_RUN_S);
    assert_eq!(
        total_s(Some(5)),
        DEFAULT_MAX_RUN_S,
        "a request member another version stored out of range reads as the default"
    );
    assert_eq!(total_s(Some(MAX_MAX_RUN_S + 1)), DEFAULT_MAX_RUN_S);
}

#[test]
fn b688_a_sessions_end_is_read_from_its_outcome_and_the_flag_and_a_missing_result_is_a_loss() {
    use DirectOutcome as O;
    let handoff = turn("stopped_timeout", Some(true));
    let plain = turn("stopped_timeout", None);
    assert_eq!(
        classify(Some(&handoff), Some(O::StoppedTimeout), 1),
        EndKind::Handoff
    );
    assert_eq!(
        classify(Some(&plain), Some(O::StoppedTimeout), 1),
        EndKind::Deadline,
        "the last session's deadline is the total time"
    );
    assert_eq!(
        classify(Some(&plain), Some(O::ReportedDone), 1),
        EndKind::Done
    );
    assert_eq!(
        classify(Some(&plain), Some(O::StoppedSilence), 2),
        EndKind::Done
    );
    assert_eq!(classify(None, None, 1), EndKind::Lost);
    assert_eq!(
        classify(
            Some(&turn("stopped_setup_failed", None)),
            Some(O::StoppedSetupFailed),
            2
        ),
        EndKind::RestoreFailed
    );
    assert_eq!(
        classify(
            Some(&turn("stopped_setup_failed", None)),
            Some(O::StoppedSetupFailed),
            1
        ),
        EndKind::Done
    );
    // without a summary the finish reason names the outcome
    assert_eq!(classify(Some(&handoff), None, 1), EndKind::Handoff);
    assert_eq!(
        classify(Some(&turn("stopped_timeout", None)), None, 1),
        EndKind::Deadline
    );
}

fn start() -> LoopState {
    LoopState::default()
}

#[test]
fn b688_a_result_ends_the_loop_and_clears_the_counts() {
    let mut s = start();
    s.lost = 2;
    s.empty = 2;
    assert_eq!(
        decide(&s, EndKind::Done, true, true),
        Verdict::Deliver {
            lost: 0,
            empty: 0,
            reason: None
        }
    );
    assert_eq!(
        decide(&s, EndKind::Deadline, true, true),
        Verdict::Deliver {
            lost: 0,
            empty: 0,
            reason: Some(EndReason::TotalTime)
        }
    );
}

#[test]
fn b688_a_handoff_with_progress_and_time_left_starts_the_next_session() {
    let mut s = start();
    s.lost = 2;
    s.empty = 1;
    assert_eq!(
        decide(&s, EndKind::Handoff, true, true),
        Verdict::Next { lost: 0, empty: 0 }
    );
}

#[test]
fn b688_a_handoff_with_no_time_left_ends_the_run_at_its_total() {
    assert_eq!(
        decide(&start(), EndKind::Handoff, true, false),
        Verdict::Deliver {
            lost: 0,
            empty: 0,
            reason: Some(EndReason::TotalTime)
        }
    );
}

#[test]
fn b688_empty_handoffs_in_a_row_stall_the_run_at_the_limit_and_a_change_clears_them() {
    let mut s = start();
    for n in 1..MAX_EMPTY_SLICES {
        match decide(&s, EndKind::Handoff, false, true) {
            Verdict::Next { lost, empty } => {
                assert_eq!((lost, empty), (0, n));
                s.empty = empty;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        decide(&s, EndKind::Handoff, false, true),
        Verdict::Deliver {
            lost: 0,
            empty: MAX_EMPTY_SLICES,
            reason: Some(EndReason::NoProgress)
        }
    );
    assert_eq!(
        decide(&s, EndKind::Handoff, true, true),
        Verdict::Next { lost: 0, empty: 0 }
    );
}

#[test]
fn b688_lost_sessions_in_a_row_stall_the_run_at_the_limit() {
    let mut s = start();
    for n in 1..MAX_LOST_SLICES {
        match decide(&s, EndKind::Lost, false, true) {
            Verdict::Next { lost, .. } => {
                assert_eq!(lost, n);
                s.lost = lost;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        decide(&s, EndKind::Lost, false, true),
        Verdict::Deliver {
            lost: MAX_LOST_SLICES,
            empty: 0,
            reason: Some(EndReason::SessionLost)
        }
    );
    assert_eq!(
        decide(&s, EndKind::RestoreFailed, false, true),
        Verdict::Deliver {
            lost: MAX_LOST_SLICES,
            empty: 0,
            reason: Some(EndReason::RestoreFailed)
        }
    );
}

#[test]
fn b688_a_lost_session_with_no_time_left_delivers_as_lost_not_as_out_of_time() {
    assert_eq!(
        decide(&start(), EndKind::Lost, true, false),
        Verdict::Deliver {
            lost: 1,
            empty: 0,
            reason: Some(EndReason::SessionLost)
        }
    );
}

#[test]
fn b688_the_loop_state_reads_what_the_machine_starts_with_and_writes_what_the_choice_reads() {
    let init: LoopState =
        serde_json::from_value(json!({"slice": 1, "lost": 0, "empty": 0, "continue": false}))
            .unwrap();
    assert_eq!(init, LoopState::default());
    let none: LoopState = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        none.slice, 1,
        "a machine started before the loop has no member: slice 1"
    );
    let out = serde_json::to_value(LoopState {
        slice: 2,
        lost: 1,
        empty: 0,
        cont: true,
        end_reason: None,
    })
    .unwrap();
    assert_eq!(
        out,
        json!({"slice": 2, "lost": 1, "empty": 0, "continue": true, "endReason": null})
    );
    let reason = serde_json::to_value(LoopState {
        end_reason: Some(EndReason::NoProgress),
        ..LoopState::default()
    })
    .unwrap();
    assert_eq!(reason["endReason"], json!("no_progress"));
}

#[test]
fn b688_every_end_reason_has_its_wire_word() {
    let all = [
        (EndReason::TotalTime, "total_time"),
        (EndReason::SessionLost, "session_lost"),
        (EndReason::RestoreFailed, "restore_failed"),
        (EndReason::NoProgress, "no_progress"),
        (EndReason::RunEnded, "run_ended"),
    ];
    for (reason, word) in all {
        assert_eq!(reason.code(), word);
        // the code is also the serde word the machine passes along
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(word));
    }
}

#[test]
fn b688_the_brief_is_the_tail_of_the_last_message_cut_at_a_character() {
    use harness_protocol::direct::MAX_SLICE_BRIEF_BYTES;
    use run_session::slices::brief_of;
    assert_eq!(brief_of(""), None);
    assert_eq!(brief_of("  \n\t "), None, "a blank message is no brief");
    assert_eq!(
        brief_of("  Done with step 2.\n").as_deref(),
        Some("Done with step 2.")
    );
    // 24,002 bytes: the cut would fall inside a two-byte character, so it moves on to the next boundary
    let long = format!("x{}x", "é".repeat(12_000));
    assert_eq!(long.len(), 24_002);
    let brief = brief_of(&long).unwrap();
    assert!(brief.len() <= MAX_SLICE_BRIEF_BYTES, "{}", brief.len());
    assert_eq!(brief.len(), 16_383);
    assert!(brief.ends_with('x') && brief.starts_with('é'));
    // a message that fits is kept whole
    let fits = "y".repeat(MAX_SLICE_BRIEF_BYTES);
    assert_eq!(brief_of(&fits).unwrap(), fits);
}

#[test]
fn b688_the_test_limit_variable_reads_whole_seconds_from_60_to_28800_and_refuses_the_rest() {
    use run_session::lambda::slice_deadline_from_lookup;
    let with = |v: Option<&str>| {
        let v = v.map(str::to_string);
        slice_deadline_from_lookup(&move |k| {
            assert_eq!(k, "HAMH_SLICE_DEADLINE_S");
            v.clone()
        })
    };
    assert_eq!(with(None), Ok(None));
    assert_eq!(with(Some("  ")), Ok(None), "a blank value is no limit");
    assert_eq!(with(Some("600")), Ok(Some(600)));
    assert_eq!(with(Some(" 28800 ")), Ok(Some(28_800)));
    assert_eq!(with(Some("60")), Ok(Some(60)));
    for bad in ["59", "28801", "ten", "-5", "1.5", "10m"] {
        let e = with(Some(bad)).unwrap_err();
        assert!(
            e.contains("HAMH_SLICE_DEADLINE_S must be whole seconds from 60 to 28800"),
            "{bad}: {e}"
        );
    }
}

#[test]
fn b689_the_plain_completion_event_is_the_complete_form_of_the_one_that_takes_a_status() {
    use harness_protocol::goal::GoalStatus;
    use run_session::events::{run_completed, run_completed_as};
    let run = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
    let a = run_completed(
        run,
        1_790_000_000_000,
        DirectOutcome::ReportedDone,
        Some("detail".to_string()),
        vec![],
    );
    let b = run_completed_as(
        run,
        1_790_000_000_000,
        GoalStatus::Complete,
        DirectOutcome::ReportedDone,
        Some("detail".to_string()),
        vec![],
    );
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(&b).unwrap()
    );
}

#[test]
fn b688_a_delivery_plan_written_before_the_loop_existed_reads_as_slice_one_with_no_loop_reason() {
    use run_session::plan_delivery::PlannedDelivery;
    let p: PlannedDelivery = serde_json::from_value(json!({
        "deliver": false, "outcome": "reported_done", "label": "Cline reported done", "plannedAtMs": 1
    }))
    .unwrap();
    assert_eq!(p.slice, 1);
    assert_eq!(p.end_reason, None);
}

#[test]
fn b688_the_next_slice_op_is_logged_under_its_own_name() {
    use run_session::ops::{Op, SessionState};
    let op = Op::NextSlice {
        run_id: "01M4X3Q7R9S0V1W2X3Y4Z5A6B7".to_string(),
        staged: json!({}),
        session: SessionState::default(),
        loop_state: LoopState::default(),
    };
    assert_eq!(op.name(), "next-slice");
}
