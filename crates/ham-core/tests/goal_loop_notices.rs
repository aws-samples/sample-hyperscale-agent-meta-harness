//! The goal loop on the run screen: agentd's goal module writes
//! `turn.progress` events of kind `goal-loop` with a `phase`. Each phase has one plain line: the loop is on; the round counter (one line
//! that counts, not one line a round); the one continue that was sent; the goal-complete call (Cline's own tool call, never a claim that
//! the work was checked); the brake that ended the run, with the engine's `BLOCKED:` line apart; and three dim notes (off, fallback, rearm).
//! A line says what happened and never calls the work verified, tested, gated or evaluated. The label of the run stays the one of
//! `run.completed`.

#![allow(clippy::unwrap_used)]

use ham_core::chat::{BlockKind, ChatInputs, ChatModel, Role};
use ham_core::notice::{NoticeKind, goal_blocked_line, notice_of};
use ham_core::state::RunState;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-16T08:15:00Z",
        "type": ty, "authority": "agent", "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn goal(n: u32, phase: &str, extra: Value) -> Envelope {
    let mut v = json!({"turn": 1, "kind": "goal-loop", "phase": phase});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    event(n, "turn.progress", v)
}

fn on(n: u32) -> Envelope {
    goal(
        n,
        "on",
        json!({"text": "Goal loop on: the engine checks the goal after each round."}),
    )
}

fn round(n: u32, rounds: u64, status: &str) -> Envelope {
    goal(
        n,
        "round",
        json!({"rounds": rounds, "status": status,
               "text": format!("Goal loop: {rounds} rounds ended, the last one {status}.")}),
    )
}

fn brake(n: u32, rule: &str, text: &str) -> Envelope {
    goal(n, "brake", json!({"rule": rule, "text": text}))
}

fn only(e: &Envelope) -> ham_core::notice::Notice {
    notice_of(e).unwrap_or_else(|| panic!("no notice for {e:?}"))
}

/// Words a line written by the harness must never hold.
const BANNED: [&str; 4] = ["verified", "tested", "gated", "evaluated"];

fn no_claim(text: &str) {
    let lower = text.to_lowercase();
    for w in BANNED {
        assert!(!lower.contains(w), "{text:?} holds {w:?}");
    }
}

// ---- the lines --------------------------------------------------------------------------------------------------------------------

#[test]
fn the_loop_being_on_is_a_one_line_note() {
    let n = only(&on(1));
    assert_eq!(n.kind, NoticeKind::GoalLoop);
    assert_eq!(
        n.text,
        "Goal loop on: the engine checks the goal after each round."
    );
    assert_eq!(n.event_id, id(1));
    assert!(!n.kind.is_run_notice());
}

#[test]
fn the_round_counter_reads_the_count_from_the_field() {
    let n = only(&round(2, 7, "completed"));
    assert_eq!(n.kind, NoticeKind::GoalRound);
    assert_eq!(n.text, "Goal loop: 7 rounds ended, the last one completed.");
    // One round reads in the singular.
    assert_eq!(
        only(&round(3, 1, "completed")).text,
        "Goal loop: 1 round ended, the last one completed."
    );
    // The count comes from `rounds`, whatever the event's own text says.
    let e = goal(
        4,
        "round",
        json!({"rounds": 12, "status": "aborted", "text": "Goal loop: 99 rounds ended, the last one completed."}),
    );
    assert_eq!(
        only(&e).text,
        "Goal loop: 12 rounds ended, the last one aborted."
    );
}

#[test]
fn every_status_of_the_last_round_has_its_words() {
    for (status, words) in [
        ("completed", "completed"),
        ("aborted", "aborted"),
        ("failed", "failed"),
        ("unknown", "has an unknown status"),
    ] {
        assert_eq!(
            only(&round(1, 2, status)).text,
            format!("Goal loop: 2 rounds ended, the last one {words}.")
        );
    }
    // A status this client has no name for is shown cleaned, not dropped.
    let t = only(&round(1, 2, "weird\u{1b}[31m")).text;
    assert!(!t.contains('\u{1b}'), "{t:?}");
    assert!(
        t.starts_with("Goal loop: 2 rounds ended, the last one "),
        "{t:?}"
    );
}

#[test]
fn a_round_event_without_a_count_falls_back_to_its_own_text() {
    let e = goal(1, "round", json!({"text": "Goal loop: some rounds ended."}));
    let n = only(&e);
    assert_eq!(n.kind, NoticeKind::GoalRound);
    assert_eq!(n.text, "Goal loop: some rounds ended.");
}

#[test]
fn the_one_continue_that_was_sent_is_a_note() {
    let n = only(&goal(
        1,
        "kick",
        json!({"text": "Goal loop: the loop stopped; one continue is sent."}),
    ));
    assert_eq!(n.kind, NoticeKind::GoalLoop);
    assert_eq!(n.text, "Goal loop: continue sent.");
}

#[test]
fn the_goal_complete_call_is_named_and_claims_nothing() {
    let n = only(&goal(
        1,
        "done",
        json!({"text": "Goal loop: Cline called the goal-complete tool and its round ended with a message."}),
    ));
    assert_eq!(n.kind, NoticeKind::GoalLoop);
    assert_eq!(n.text, "Goal loop: Cline called the goal-complete tool.");
    no_claim(&n.text);
}

#[test]
fn the_summary_of_the_goal_complete_call_is_cline_s_own_words() {
    let n = only(&goal(
        1,
        "done",
        json!({"summary": "Added percent_change and its tests."}),
    ));
    assert_eq!(
        n.text,
        "Goal loop: Cline called the goal-complete tool. Cline's summary: Added percent_change and its tests."
    );
    // A long summary is cut to one line; control characters and tokens are gone.
    let long = "x".repeat(300);
    let n = only(&goal(2, "done", json!({"summary": long})));
    assert!(
        n.text.chars().count() <= ham_core::notice::NOTICE_MAX_CHARS,
        "{}",
        n.text
    );
    let n = only(&goal(
        3,
        "done",
        json!({"summary": "ok\u{1b}]0;x\u{7} done"}),
    ));
    assert!(
        !n.text.contains('\u{1b}') && !n.text.contains('\u{7}'),
        "{:?}",
        n.text
    );
}

#[test]
fn a_brake_says_the_harness_ended_the_run_and_the_rule() {
    for rule in [
        "text-only-stall",
        "no-progress",
        "dead-loop",
        "unfinished-claim",
        "goal-cleared",
    ] {
        let text = format!("The harness ended the run: the {rule} rule applied.");
        let e = brake(1, rule, &text);
        let n = only(&e);
        assert_eq!(n.kind, NoticeKind::GoalBrake, "{rule}");
        assert_eq!(n.text, text, "{rule}");
        assert_eq!(goal_blocked_line(&e), None, "{rule}");
        no_claim(&n.text);
    }
}

#[test]
fn a_brake_after_two_blocked_rounds_shows_the_engine_s_blocked_line_apart() {
    let e = brake(
        1,
        "blocked-twice",
        "The harness ended the run: the engine reported BLOCKED twice. Its words: BLOCKED: the stage has no network",
    );
    let n = only(&e);
    assert_eq!(n.kind, NoticeKind::GoalBrake);
    assert_eq!(
        n.text,
        "The harness ended the run: the engine reported BLOCKED twice."
    );
    assert_eq!(
        goal_blocked_line(&e).as_deref(),
        Some("BLOCKED: the stage has no network")
    );
    // The quoted line may sit in quotation marks.
    let e = brake(
        2,
        "blocked-twice",
        "The harness ended the run: blocked twice. Its words: \"BLOCKED: need a credential\"",
    );
    assert_eq!(
        goal_blocked_line(&e).as_deref(),
        Some("BLOCKED: need a credential")
    );
    // A quoted line that is not a BLOCKED line is not shown as one.
    let e = brake(
        3,
        "blocked-twice",
        "The harness ended the run: blocked twice. Its words: all fine",
    );
    assert_eq!(goal_blocked_line(&e), None);
    // No quoted line, no BLOCKED line.
    let e = brake(
        4,
        "blocked-twice",
        "The harness ended the run: blocked twice.",
    );
    assert_eq!(goal_blocked_line(&e), None);
}

#[test]
fn the_blocked_line_is_cleaned_and_cut_like_any_line() {
    let long = "y".repeat(400);
    let e = brake(
        1,
        "blocked-twice",
        &format!("The harness ended the run: blocked twice. Its words: BLOCKED: {long}\u{1b}[2J"),
    );
    let line = goal_blocked_line(&e).unwrap();
    assert!(line.starts_with("BLOCKED: "), "{line:?}");
    assert!(!line.contains('\u{1b}'));
    assert!(
        line.chars().count() <= ham_core::notice::NOTICE_MAX_CHARS,
        "{line:?}"
    );
}

#[test]
fn the_three_quiet_notes_show_the_event_s_words() {
    for (phase, text) in [
        ("off", "Goal loop off: the stage switched it off."),
        (
            "fallback",
            "Goal loop: the round log is missing; the loop is watched without it.",
        ),
        (
            "rearm",
            "Goal loop: the goal is set again for the new session.",
        ),
    ] {
        let n = only(&goal(1, phase, json!({"text": text})));
        assert_eq!(n.kind, NoticeKind::GoalQuiet, "{phase}");
        assert_eq!(n.text, text, "{phase}");
        no_claim(&n.text);
    }
}

#[test]
fn a_phase_this_client_does_not_know_still_shows_a_line() {
    let n = only(&goal(
        1,
        "later-phase",
        json!({"text": "Goal loop: something new."}),
    ));
    assert_eq!(n.kind, NoticeKind::GoalLoop);
    assert_eq!(n.text, "Goal loop: something new.");
    let n = only(&goal(2, "later-phase", json!({})));
    assert_eq!(n.text, "Goal loop: later-phase.");
}

#[test]
fn text_from_the_stream_is_one_clean_line() {
    let n = only(&goal(
        1,
        "off",
        json!({"text": format!("Goal loop off: \u{1b}]52;c;AAAA\u{7}reason {}", "z".repeat(400))}),
    ));
    assert!(
        !n.text.contains('\u{1b}') && !n.text.contains('\u{7}'),
        "{:?}",
        n.text
    );
    assert!(n.text.chars().count() <= ham_core::notice::NOTICE_MAX_CHARS);
    assert!(!n.text.contains('\n'));
}

// ---- the conversation -------------------------------------------------------------------------------------------------------------

fn model_of(events: Vec<Envelope>) -> ChatModel {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    ChatModel::from_progress_feed(&s, &ChatInputs::default())
}

fn notices(m: &ChatModel) -> Vec<(String, String)> {
    m.blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Notice(_)))
        .map(|b| (b.id.clone(), b.body.clone()))
        .collect()
}

#[test]
fn the_round_counter_is_one_line_that_counts() {
    let m = model_of(vec![
        on(1),
        round(2, 1, "completed"),
        goal(3, "kick", json!({})),
        round(4, 2, "completed"),
        round(5, 3, "failed"),
    ]);
    let n = notices(&m);
    let rounds: Vec<_> = n
        .iter()
        .filter(|(_, t)| t.contains("rounds ended") || t.contains("round ended"))
        .collect();
    assert_eq!(rounds.len(), 1, "{n:?}");
    assert_eq!(
        rounds[0].1,
        "Goal loop: 3 rounds ended, the last one failed."
    );
    // It keeps the place of the first round event: after the on note, before the continue.
    let order: Vec<&str> = n.iter().map(|(_, t)| t.as_str()).collect();
    assert!(order[0].starts_with("Goal loop on"), "{order:?}");
    assert!(order[1].contains("rounds ended"), "{order:?}");
    assert_eq!(order[2], "Goal loop: continue sent.");
}

#[test]
fn the_notes_sit_in_the_conversation_in_the_order_they_happened() {
    let m = model_of(vec![
        on(1),
        goal(2, "done", json!({})),
        brake(
            3,
            "blocked-twice",
            "The harness ended the run: blocked twice. Its words: BLOCKED: no network",
        ),
    ]);
    let n = notices(&m);
    assert_eq!(n.len(), 4, "{n:?}");
    assert!(n[0].1.starts_with("Goal loop on"));
    assert_eq!(n[1].1, "Goal loop: Cline called the goal-complete tool.");
    assert_eq!(n[2].1, "The harness ended the run: blocked twice.");
    assert_eq!(n[3].1, "BLOCKED: no network");
    for b in &m.blocks {
        if let BlockKind::Notice(k) = b.kind {
            assert_eq!(b.role, Role::Notice);
            assert!(matches!(
                k,
                NoticeKind::GoalLoop | NoticeKind::GoalBrake | NoticeKind::GoalBlocked
            ));
        }
    }
    let kinds: Vec<NoticeKind> = m
        .blocks
        .iter()
        .filter_map(|b| match b.kind {
            BlockKind::Notice(k) => Some(k),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [
            NoticeKind::GoalLoop,
            NoticeKind::GoalLoop,
            NoticeKind::GoalBrake,
            NoticeKind::GoalBlocked
        ]
    );
}

#[test]
fn a_goal_loop_event_is_not_an_unknown_event_and_not_a_run_notice() {
    let m = model_of(vec![
        on(1),
        goal(
            2,
            "fallback",
            json!({"text": "Goal loop: the round log is missing."}),
        ),
    ]);
    assert!(
        m.blocks.iter().all(|b| b.kind != BlockKind::Unknown),
        "{:?}",
        m.blocks
    );
    let mut s = RunState::new(RUN);
    s.apply(on(1));
    s.apply(round(2, 1, "completed"));
    assert!(s.view().notices.is_empty(), "{:?}", s.view().notices);
}

#[test]
fn the_question_cards_of_the_recovery_ladder_are_unchanged() {
    // A needs-input event keeps its notice kind; the goal-loop lines do not touch it.
    let q = event(
        1,
        "turn.progress",
        json!({"turn": 1, "kind": "needs-input", "title": "Cline stopped its own prompt", "blocking": true,
               "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "n": 1, "reason": "engine_stop", "options": ["finish"],
               "models": [], "recommended": null, "canContinue": true, "waitS": 1800,
               "expiresAt": "2026-10-16T08:45:00Z", "files": [], "filesTotal": 0}),
    );
    assert_eq!(only(&q).kind, NoticeKind::NeedsInput);
}
