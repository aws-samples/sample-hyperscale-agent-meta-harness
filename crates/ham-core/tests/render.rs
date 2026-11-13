//! Rendered text: the delivery card and status line.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::render::{
    CardUsage, FinalCard, final_card, final_card_shown, phase_shown, status_line,
};
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use serde_json::json;

fn ev(n: u8, t: EventType, v: serde_json::Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: None,
        seq: u64::from(n),
        ts: "2026-09-25T02:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

fn done() -> RunState {
    let mut s = RunState::new("R1");
    s.apply(ev(1, EventType::RunCreated, json!({})));
    s.apply(ev(
        2,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": "global.anthropic.claude-sonnet-5-5"}),
    ));
    s.apply(ev(
        12,
        EventType::DeliveryPr,
        json!({"url": "https://example/pr/7"}),
    ));
    s.apply(ev(
        13,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    s.apply(ev(
        14,
        EventType::DeliveryReady,
        json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
    ));
    s.apply(ev(
        15,
        EventType::RunCompleted,
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    ));
    s
}

fn card() -> FinalCard {
    FinalCard {
        local_path: "/downloads/python-calc-R1-abc1234.zip".into(),
        presigned_url: Some("https://s3.example/presigned".into()),
        expires_at: Some("2026-09-25T03:00:00Z".into()),
        usage: Some(CardUsage {
            tokens: 123_456,
            usd: Some(1.25),
        }),
        label_missing_after: None,
        tests: None,
        flow: None,
        session: None,
    }
}

#[test]
fn final_card_snapshot() {
    insta::assert_snapshot!("final_card", final_card(&done().view(), &card()));
}

/// The card names the command that turns the delivered zip into a local git repository, after the
/// download link; a run with no zip has nothing to clone.
#[test]
fn the_card_names_the_clone_command_when_a_zip_was_delivered() {
    let text = final_card(&done().view(), &card());
    assert!(text.contains("\nClone     ham clone R1\n"), "{text}");
    let (link, clone) = (
        text.find("\nLink ").unwrap(),
        text.find("\nClone ").unwrap(),
    );
    assert!(link < clone, "{text}");
    let mut s = RunState::new("R1");
    s.apply(ev(1, EventType::RunCreated, json!({})));
    let none = final_card(&s.view(), &card());
    assert!(!none.contains("Clone"), "{none}");
}

#[test]
fn status_line_snapshots() {
    insta::assert_snapshot!("status_complete", status_line(&done().view()));
    let mut s = RunState::new("R1");
    s.apply(ev(1, EventType::RunCreated, json!({})));
    insta::assert_snapshot!("status_active", status_line(&s.view()));
}

/// The card claims nothing the harness did not do: no evidence block, no gate counts, and no usage line when the stage reported none.
#[test]
fn the_card_has_no_evidence_block_and_no_fake_usage() {
    let none = FinalCard {
        usage: None,
        ..card()
    };
    let text = final_card(&done().view(), &none);
    for banned in ["Evidence", "gates", "verdicts", "coverage", "Tokens"] {
        assert!(!text.contains(banned), "{banned:?} in {text}");
    }
    assert!(!status_line(&done().view()).contains("gates"));
}

// ---- the phase of a finished run (the Runs list said `delivering` on every finished run) ------------------------------------

/// Every status. A status added to `GoalStatus` stops this match from compiling until its phase rule is decided here.
const STATUSES: [GoalStatus; 8] = [
    GoalStatus::Active,
    GoalStatus::Paused,
    GoalStatus::Blocked,
    GoalStatus::UsageLimited,
    GoalStatus::BudgetLimited,
    GoalStatus::Complete,
    GoalStatus::Cleared,
    GoalStatus::Stalled,
];

/// Every phase with the word the service stores for it.
const PHASES: [(GoalPhase, &str); 8] = [
    (GoalPhase::Planning, "planning"),
    (GoalPhase::Red, "red"),
    (GoalPhase::Green, "green"),
    (GoalPhase::Refactor, "refactor"),
    (GoalPhase::Gating, "gating"),
    (GoalPhase::Evaluating, "evaluating"),
    (GoalPhase::Integrating, "integrating"),
    (GoalPhase::Delivering, "delivering"),
];

/// The rule, written out per status: the phase of a run that is over only says where the last write happened, so it is not shown.
fn is_over(s: GoalStatus) -> bool {
    match s {
        GoalStatus::Complete | GoalStatus::Cleared => true,
        GoalStatus::Active
        | GoalStatus::Paused
        | GoalStatus::Blocked
        | GoalStatus::UsageLimited
        | GoalStatus::BudgetLimited
        | GoalStatus::Stalled => false,
    }
}

/// The place of each variant. Both matches are exhaustive, so a variant added to the protocol stops this file from compiling until the
/// tables above and the rule below are extended for it.
fn status_place(s: GoalStatus) -> usize {
    match s {
        GoalStatus::Active => 0,
        GoalStatus::Paused => 1,
        GoalStatus::Blocked => 2,
        GoalStatus::UsageLimited => 3,
        GoalStatus::BudgetLimited => 4,
        GoalStatus::Complete => 5,
        GoalStatus::Cleared => 6,
        GoalStatus::Stalled => 7,
    }
}

fn phase_place(p: GoalPhase) -> usize {
    match p {
        GoalPhase::Planning => 0,
        GoalPhase::Red => 1,
        GoalPhase::Green => 2,
        GoalPhase::Refactor => 3,
        GoalPhase::Gating => 4,
        GoalPhase::Evaluating => 5,
        GoalPhase::Integrating => 6,
        GoalPhase::Delivering => 7,
    }
}

#[test]
fn the_tables_hold_every_status_and_every_phase_once_and_the_words_are_the_ones_the_service_stores()
{
    assert_eq!(STATUSES.map(status_place), [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(
        PHASES.map(|(p, _)| phase_place(p)),
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
    for (phase, word) in PHASES {
        assert_eq!(serde_json::to_value(phase).unwrap(), json!(word));
    }
}

#[test]
fn a_finished_run_shows_no_phase_whatever_phase_it_stopped_in() {
    for status in STATUSES.into_iter().filter(|s| is_over(*s)) {
        for (phase, word) in PHASES {
            assert_eq!(
                phase_shown(Some(status), Some(phase)),
                None,
                "{status:?} in {word}"
            );
        }
        assert_eq!(phase_shown(Some(status), None), None, "{status:?}");
    }
}

#[test]
fn a_run_that_is_not_over_shows_the_phase_it_is_in_or_stopped_in() {
    for status in STATUSES.into_iter().filter(|s| !is_over(*s)) {
        for (phase, word) in PHASES {
            assert_eq!(
                phase_shown(Some(status), Some(phase)),
                Some(word),
                "{status:?} in {word}"
            );
        }
    }
}

#[test]
fn a_phase_that_is_not_known_reads_a_dash_and_a_status_that_is_not_known_still_shows_the_phase() {
    for status in STATUSES.into_iter().filter(|s| !is_over(*s)) {
        assert_eq!(phase_shown(Some(status), None), Some("-"), "{status:?}");
    }
    assert_eq!(phase_shown(None, Some(GoalPhase::Green)), Some("green"));
    assert_eq!(phase_shown(None, None), Some("-"));
}

fn card_of(status: &str, phase: &str) -> RunState {
    let mut s = RunState::new("R1");
    s.apply(ev(1, EventType::RunCreated, json!({})));
    s.apply(ev(
        2,
        EventType::GoalStatus,
        json!({"status": status, "phase": phase}),
    ));
    s
}

/// The command line keeps the record as the service stored it; only the run screen's card leaves the phase of a finished run out.
#[test]
fn the_card_on_the_run_screen_leaves_out_the_phase_of_a_finished_run_and_the_command_line_card_keeps_it()
 {
    let view = done().view();
    let (line, shown) = (final_card(&view, &card()), final_card_shown(&view, &card()));
    assert_eq!(
        line.lines().next(),
        Some("Run       R1 complete (delivered)")
    );
    assert_eq!(shown.lines().next(), Some("Run       R1 complete"));
    // Nothing else on the card changes.
    assert_eq!(
        line.lines().skip(1).collect::<Vec<_>>(),
        shown.lines().skip(1).collect::<Vec<_>>()
    );
}

#[test]
fn the_card_on_the_run_screen_keeps_the_phase_of_a_run_that_is_not_over() {
    for (status, phase) in [
        ("active", "green"),
        ("blocked", "delivering"),
        ("usage_limited", "refactor"),
    ] {
        let view = card_of(status, phase).view();
        let first = format!("Run       R1 {status} ({phase})");
        assert_eq!(
            final_card_shown(&view, &card()).lines().next(),
            Some(first.as_str())
        );
        assert_eq!(
            final_card(&view, &card()).lines().next(),
            Some(first.as_str())
        );
    }
    for status in ["complete", "cleared"] {
        let view = card_of(status, "green").view();
        let first = format!("Run       R1 {status}");
        assert_eq!(
            final_card_shown(&view, &card()).lines().next(),
            Some(first.as_str()),
            "{status}"
        );
    }
}
