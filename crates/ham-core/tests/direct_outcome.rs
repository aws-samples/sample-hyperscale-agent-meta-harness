//! The direct run's result as the client shows it: the harness writes `outcome`, `label` and
//! `detail` on `run.completed`, and the card, the status line and the exit code say exactly that. "Cline reported done" and
//! "Cline stopped: <reason>" are the only claims; the earlier line's Evidence block (gates, tests, coverage, findings, verdicts)
//! is not shown, and no rendering says the harness verified, tested, gated or evaluated anything.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::exit::exit_code;
use ham_core::render::{CardUsage, FinalCard, final_card, status_line};
use ham_core::state::{RunState, RunView};
use harness_protocol::direct::{DirectOutcome, banned_claim_word};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::GoalStatus;
use serde_json::{Value, json};

/// The model of the fixture run: Claude Sonnet 5.5, a row of the shipped `engine/catalog/models.json`.
const MODEL_ID: &str = "global.anthropic.claude-sonnet-5-5";
const MODEL_TEXT: &str = "Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)";

fn ev(n: u8, t: EventType, authority: Authority, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: u64::from(n),
        ts: "2026-10-14T02:00:00Z".into(),
        event_type: t,
        authority,
        payload: Payload::Inline { value: v },
    }
}

fn harness(n: u8, t: EventType, v: Value) -> Envelope {
    ev(n, t, Authority::Harness, None, v)
}

/// A direct run up to its pull request and zip, then `completed` as the run's last event.
fn direct_run(completed: Value) -> RunState {
    let mut s = RunState::new("R1");
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": MODEL_ID}),
    ));
    s.apply(harness(
        3,
        EventType::DeliveryPr,
        json!({"url": "https://example/pr/7"}),
    ));
    s.apply(harness(
        4,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    s.apply(harness(
        5,
        EventType::DeliveryReady,
        json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
    ));
    s.apply(harness(6, EventType::RunCompleted, completed));
    s
}

fn completed(outcome: DirectOutcome, detail: Option<&str>) -> Value {
    let mut v =
        json!({"status": "complete", "outcome": outcome.as_str(), "label": outcome.label()});
    if let Some(d) = detail {
        v["detail"] = json!(d);
    }
    v
}

fn card() -> FinalCard {
    FinalCard {
        local_path: "/downloads/python-calc-R1-abc1234.zip".into(),
        presigned_url: Some("https://s3.example/presigned".into()),
        expires_at: Some("2026-10-14T03:00:00Z".into()),
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

/// The old line's five evidence lines and their heading.
const EVIDENCE_LINES: [&str; 6] = [
    "Evidence",
    "  gates",
    "  tests",
    "  coverage",
    "  findings",
    "  verdicts",
];

fn assert_no_evidence(text: &str) {
    for want_absent in EVIDENCE_LINES {
        assert!(
            !text.lines().any(|l| l.starts_with(want_absent)),
            "a direct run shows no {want_absent:?} line:\n{text}"
        );
    }
}

fn view(outcome: DirectOutcome, detail: Option<&str>) -> RunView {
    direct_run(completed(outcome, detail)).view()
}

#[test]
fn the_view_keeps_the_outcome_word_and_the_label_exactly_as_the_harness_wrote_them() {
    for outcome in DirectOutcome::ALL {
        let v = view(outcome, None);
        let r = v.result.as_ref().unwrap_or_else(|| panic!("{outcome:?}"));
        assert_eq!(r.outcome.as_deref(), Some(outcome.as_str()));
        assert_eq!(r.label, outcome.label(), "{outcome:?}");
        assert_eq!(r.detail, None);
    }
}

#[test]
fn a_label_a_later_harness_adds_is_shown_as_delivered() {
    let v = direct_run(json!({"status": "complete", "outcome": "stopped_brand_new",
        "label": "Cline stopped: a reason this client has never seen"}))
    .view();
    let r = v.result.unwrap();
    assert_eq!(
        r.label,
        "Cline stopped: a reason this client has never seen"
    );
    assert_eq!(r.outcome.as_deref(), Some("stopped_brand_new"));
}

#[test]
fn an_outcome_word_without_a_label_gets_the_frozen_label_and_a_label_alone_is_kept() {
    let only_word = direct_run(json!({"status": "complete", "outcome": "stopped_silence"})).view();
    assert_eq!(
        only_word.result.unwrap().label,
        DirectOutcome::StoppedSilence.label()
    );
    let only_label =
        direct_run(json!({"status": "complete", "label": "Cline reported done"})).view();
    let r = only_label.result.unwrap();
    assert_eq!(r.label, "Cline reported done");
    assert_eq!(r.outcome, None);
}

#[test]
fn the_direct_card_names_the_label_and_shows_no_evidence_lines() {
    for outcome in DirectOutcome::ALL {
        let text = final_card(&view(outcome, None), &card());
        assert!(
            text.lines()
                .any(|l| l == format!("Outcome   {}", outcome.label())),
            "{outcome:?}:\n{text}"
        );
        assert_no_evidence(&text);
        assert!(
            !text.contains("n/a"),
            "no n/a placeholder stands for a missing evidence line:\n{text}"
        );
        for kept in [
            "Run       R1 complete (delivered)",
            &format!("Model     {MODEL_TEXT}"),
            "PR        https://example/pr/7",
            "Zip       /downloads/python-calc-R1-abc1234.zip",
            "Clone     ham clone R1",
            "Tokens    123456  USD 1.25",
        ] {
            assert!(text.contains(kept), "{kept}:\n{text}");
        }
    }
}

/// The usage line of a direct card is what the stage reported: tokens and cost, or nothing. A "Tokens 0  USD 0.00" line for a run
/// that used hundreds of thousands of tokens is a fake zero (D's observation on the stage digest 181af04b).
#[test]
fn the_direct_card_prints_the_usage_the_stage_reported_and_no_line_when_it_reported_none() {
    let v = view(DirectOutcome::ReportedDone, None);
    let with = final_card(&v, &card());
    assert!(
        with.lines().any(|l| l == "Tokens    123456  USD 1.25"),
        "{with}"
    );
    // The line is the last one, after the clone hint.
    assert_eq!(with.lines().last(), Some("Tokens    123456  USD 1.25"));

    let none = final_card(
        &v,
        &FinalCard {
            usage: None,
            ..card()
        },
    );
    assert!(
        !none.contains("Tokens") && !none.contains("USD"),
        "no usage reported, no usage line:\n{none}"
    );
    assert!(!none.contains("0.00"), "no fake zero:\n{none}");
    for kept in [
        "Run       R1 complete (delivered)",
        "Outcome   Cline reported done",
        "PR        https://example/pr/7",
        "Clone     ham clone R1",
    ] {
        assert!(none.contains(kept), "{kept}:\n{none}");
    }

    // A cost the price table cannot give (a model it has no row for) is "unknown", not 0.00.
    let unknown = final_card(
        &v,
        &FinalCard {
            usage: Some(CardUsage {
                tokens: 9_000,
                usd: None,
            }),
            ..card()
        },
    );
    assert_eq!(unknown.lines().last(), Some("Tokens    9000  USD unknown"));
    assert_eq!(banned_claim_word(&unknown), None);
}

#[test]
fn the_card_puts_the_detail_under_the_label_when_there_is_one() {
    let text = final_card(
        &view(
            DirectOutcome::StoppedSilence,
            Some("no output for 900 s; pull request not opened: push refused"),
        ),
        &card(),
    );
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| *l == "Outcome   Cline stopped: no output for the silence limit")
        .unwrap();
    assert_eq!(
        lines[at + 1],
        "Detail    no output for 900 s; pull request not opened: push refused"
    );
    let none = final_card(&view(DirectOutcome::ReportedDone, None), &card());
    assert!(!none.contains("Detail"), "{none}");
}

#[test]
fn the_status_line_of_a_direct_run_ends_with_the_label_and_drops_the_gate_counts() {
    let line = status_line(&view(DirectOutcome::ReportedDone, None));
    assert_eq!(
        line,
        "run R1 complete (delivered) agents 1 model Claude Sonnet 5.5 pr https://example/pr/7 outcome: Cline reported done"
    );
    let stopped = status_line(&view(DirectOutcome::StoppedMaxTokens, None));
    assert!(
        stopped.ends_with("outcome: Cline stopped: it reached the output limit"),
        "{stopped}"
    );
    // Before the harness has said how it ended there is no outcome, and still no gate counts.
    let mut s = RunState::new("R1");
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": "m"}),
    ));
    assert_eq!(
        status_line(&s.view()),
        "run R1 active (planning) agents 1 model m"
    );
}

#[test]
fn a_run_that_the_harness_never_labelled_has_no_outcome_line_and_no_evidence() {
    let mut s = RunState::new("R1");
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::SessionStarted,
        json!({"agentId": "A1", "engine": "cline", "modelId": "m"}),
    ));
    s.apply(harness(
        3,
        EventType::DeliveryReady,
        json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2}),
    ));
    s.apply(harness(
        4,
        EventType::RunCompleted,
        json!({"status": "complete"}),
    ));
    let text = final_card(&s.view(), &card());
    assert_no_evidence(&text);
    assert!(!text.contains("Outcome"), "{text}");
}

#[test]
fn an_agent_cannot_write_the_label() {
    let mut s = direct_run(completed(DirectOutcome::StoppedSilence, None));
    // The same ULID range, so the agent-authored event sorts after the real one.
    s.apply(ev(
        9,
        EventType::RunCompleted,
        Authority::Agent,
        Some("direct"),
        completed(DirectOutcome::ReportedDone, Some("agent text")),
    ));
    let v = s.view();
    assert_eq!(v.untrusted, 1);
    assert_eq!(
        v.result.unwrap().label,
        DirectOutcome::StoppedSilence.label()
    );
}

#[test]
fn a_later_run_completed_without_a_label_keeps_the_result_an_earlier_one_gave() {
    let mut s = direct_run(completed(
        DirectOutcome::StoppedTimeout,
        Some("the deadline"),
    ));
    s.apply(harness(
        7,
        EventType::RunCompleted,
        json!({"status": "complete"}),
    ));
    let r = s.view().result.unwrap();
    assert_eq!(r.label, DirectOutcome::StoppedTimeout.label());
    assert_eq!(r.detail.as_deref(), Some("the deadline"));
}

#[test]
fn a_run_completed_with_a_harness_stop_detail_and_no_label_gets_the_harness_stop_label() {
    let v = direct_run(json!({"status": "complete", "outcome": "stopped_other",
        "detail": "The harness ended the run: 6 rounds in a row changed no file."}))
    .view();
    let r = v.result.unwrap();
    assert_eq!(
        r.label,
        "Cline stopped: the harness ended the run (6 rounds in a row changed no file)"
    );
    // a detail of any other kind keeps the frozen label of the outcome
    let v =
        direct_run(json!({"status": "complete", "outcome": "stopped_other", "detail": "exit 3"}))
            .view();
    assert_eq!(v.result.unwrap().label, DirectOutcome::StoppedOther.label());
}

#[test]
fn label_and_detail_are_cleaned_of_terminal_escapes_but_not_reworded() {
    let v = direct_run(json!({"status": "complete", "outcome": "stopped_other",
        "label": "Cline stopped: the \u{1b}]0;t\u{7}engine reported an unknown stop reason",
        "detail": "line one\u{1b}[31m red"}))
    .view();
    let r = v.result.unwrap();
    assert_eq!(
        r.label,
        "Cline stopped: the engine reported an unknown stop reason"
    );
    assert_eq!(r.detail.as_deref(), Some("line one red"));
}

/// The stop table: a run Cline stopped halfway still has its zip and
/// pull request, so it ends `complete` and the process exits 0; `blocked` (12) is for a run that delivered nothing or whose
/// delivery failed validation, and it carries the label the harness had.
#[test]
fn a_run_cline_stopped_halfway_exits_0_and_a_run_with_nothing_to_deliver_exits_12() {
    for outcome in DirectOutcome::ALL {
        let delivered = view(outcome, None);
        assert_eq!(delivered.status, Some(GoalStatus::Complete), "{outcome:?}");
        assert_eq!(exit_code(delivered.status), 0, "{outcome:?}");
    }
    let mut s = RunState::new("R1");
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": "m"}),
    ));
    s.apply(harness(
        3,
        EventType::GoalBlocked,
        json!({"reason": "the session ended before any work was saved"}),
    ));
    s.apply(harness(
        4,
        EventType::GoalStatus,
        json!({"status": "blocked", "phase": "delivering"}),
    ));
    s.apply(harness(
        5,
        EventType::RunCompleted,
        json!({"status": "blocked", "outcome": "stopped_session_lost",
            "label": DirectOutcome::StoppedSessionLost.label()}),
    ));
    let v = s.view();
    assert_eq!(exit_code(v.status), 12);
    assert_eq!(
        v.result.unwrap().label,
        DirectOutcome::StoppedSessionLost.label()
    );
}

/// The card names the model the run used. The harness writes the model id on `session.started`; the card
/// shows the name the model list gives that id, with the id beside it, so a person and a script can both read it.
#[test]
fn the_card_names_the_model_the_run_used_between_the_outcome_and_the_pull_request() {
    for outcome in DirectOutcome::ALL {
        let text = final_card(&view(outcome, Some("a detail")), &card());
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| *l == format!("Model     {MODEL_TEXT}"))
            .unwrap_or_else(|| panic!("{outcome:?}:\n{text}"));
        assert!(lines[at - 1].starts_with("Detail    "), "{text}");
        assert!(lines[at + 1].starts_with("PR        "), "{text}");
    }
}

#[test]
fn every_model_of_the_shipped_list_is_shown_by_its_name_and_an_unknown_id_is_shown_bare() {
    for m in ham_core::models::builtin().list().models {
        let mut s = direct_run(completed(DirectOutcome::ReportedDone, None));
        s.apply(harness(
            8,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": m.id}),
        ));
        let text = final_card(&s.view(), &card());
        assert!(
            text.lines()
                .any(|l| l == format!("Model     {} ({})", m.name, m.id)),
            "{}:\n{text}",
            m.id
        );
    }
    let mut s = direct_run(completed(DirectOutcome::ReportedDone, None));
    s.apply(harness(
        8,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": "us.example.future-model-9"}),
    ));
    let text = final_card(&s.view(), &card());
    assert!(
        text.lines()
            .any(|l| l == "Model     us.example.future-model-9"),
        "{text}"
    );
}

#[test]
fn a_run_with_no_session_has_no_model_line_and_the_model_text_is_cleaned() {
    let mut s = RunState::new("R1");
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::RunCompleted,
        json!({"status": "blocked", "outcome": "stopped_setup_failed", "label": DirectOutcome::StoppedSetupFailed.label()}),
    ));
    assert!(!final_card(&s.view(), &card()).contains("Model"));
    assert!(!status_line(&s.view()).contains("model"));
    let mut t = direct_run(completed(DirectOutcome::ReportedDone, None));
    t.apply(harness(
        8,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": "m\u{1b}]0;t\u{7}odel"}),
    ));
    let text = final_card(&t.view(), &card());
    assert!(text.lines().any(|l| l == "Model     model"), "{text}");
}

#[test]
fn nothing_a_direct_run_shows_says_the_harness_checked_the_work() {
    for outcome in DirectOutcome::ALL {
        let v = view(outcome, Some("Added percent_change with tests."));
        for (what, text) in [
            ("card", final_card(&v, &card())),
            ("status line", status_line(&v)),
        ] {
            assert_eq!(
                banned_claim_word(&text),
                None,
                "{outcome:?} {what}:\n{text}"
            );
            assert!(!text.contains("verified by the harness"), "{what}: {text}");
        }
    }
    // The text `ham rules init` writes and the usage errors a person reads.
    for (what, text) in [
        ("default rules", ham_core::rules::DEFAULT_RULES.to_string()),
        (
            "default continue",
            ham_core::rules::DEFAULT_CONTINUE.to_string(),
        ),
        (
            "usage error",
            ham_core::command::parse(&["--no-such-flag".to_string()])
                .unwrap_err()
                .to_string(),
        ),
    ] {
        assert_eq!(banned_claim_word(&text), None, "{what}:\n{text}");
    }
}
