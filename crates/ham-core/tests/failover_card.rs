//! The delivery card and the status line of a run that switched model or route. agentd writes the sentence `Ran on <model id> after
//! <n> switch(es).` first in the `detail` of `run.completed`, and then, joined with a single space, `Every route was down for the
//! last <s> s.` when every route was down, and the classifier's cause, which starts with a lowercase letter and has no full stop. The
//! client shows the model the run ended on by its display name, keeps the model the user asked for on its own line, and writes the
//! sentences as sentences. The wire text is not changed: `RunView::result` keeps the harness's words.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::render::{FinalCard, final_card, status_line};
use ham_core::state::{RunState, RunView};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

/// The model the user asked for in every fixture: Claude Sonnet 5.5, a row of the shipped `engine/catalog/models.json`.
const REQUESTED_ID: &str = "global.anthropic.claude-sonnet-5-5";
const REQUESTED_TEXT: &str = "Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)";

/// The `detail` of the A12 run's `run.completed`, as the harness wrote it.
const A12_DETAIL: &str = "Ran on us.zai.glm-5.3 after 9 switches. the engine ended the prompt cancelled with no cancel request (mistake limit or a cut model stream)";

fn harness(n: u8, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: None,
        seq: u64::from(n),
        ts: "2026-10-15T02:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

/// A delivered direct run on `REQUESTED_ID` whose `run.completed` carries `detail`.
fn view(outcome: DirectOutcome, detail: &str) -> RunView {
    let mut s = RunState::new("R1");
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": REQUESTED_ID}),
    ));
    s.apply(harness(
        3,
        EventType::GoalStatus,
        json!({"status": "complete", "phase": "delivering"}),
    ));
    s.apply(harness(
        4,
        EventType::DeliveryReady,
        json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
    ));
    s.apply(harness(
        5,
        EventType::RunCompleted,
        json!({"status": "complete", "outcome": outcome.as_str(), "label": outcome.label(), "detail": detail}),
    ));
    s.view()
}

fn card_of(detail: &str) -> String {
    final_card(
        &view(DirectOutcome::StoppedEngineCancelled, detail),
        &FinalCard {
            local_path: "/downloads/s3-only-R1.zip".into(),
            ..FinalCard::default()
        },
    )
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

fn position(lines: &[&str], line: &str, text: &str) -> usize {
    lines
        .iter()
        .position(|l| *l == line)
        .unwrap_or_else(|| panic!("no line {line:?} in:\n{text}"))
}

#[test]
fn the_a12_card_shows_the_model_the_run_ended_on_and_the_model_it_asked_for() {
    let text = card_of(A12_DETAIL);
    let l = lines(&text);
    let detail = position(
        &l,
        "Detail    Ran on GLM 5.3 after 9 switches. The engine ended the prompt cancelled with no cancel request (mistake limit or a cut model stream).",
        &text,
    );
    // The Model row is the model the run ended on, and the requested model follows it, before the pull request.
    assert_eq!(
        l[detail + 1],
        "Model     GLM 5.3 (us.zai.glm-5.3)",
        "{text}"
    );
    assert_eq!(
        l[detail + 2],
        format!("Requested {REQUESTED_TEXT}"),
        "{text}"
    );
    assert!(l[detail + 3].starts_with("PR        "), "{text}");
    assert!(
        !text.contains("Ran on us.zai.glm-5.3"),
        "the raw model id is not shown in the Detail row:\n{text}"
    );
}

#[test]
fn the_status_line_says_where_the_run_ended_and_still_ends_with_the_outcome() {
    let v = view(DirectOutcome::StoppedEngineCancelled, A12_DETAIL);
    let s = status_line(&v);
    assert!(
        s.contains(" model Claude Sonnet 5.5 ran on GLM 5.3 after 9 switches"),
        "{s}"
    );
    assert!(
        s.ends_with(&format!(
            "outcome: {}",
            DirectOutcome::StoppedEngineCancelled.label()
        )),
        "{s}"
    );
}

#[test]
fn what_the_harness_wrote_is_kept_in_the_view() {
    let v = view(DirectOutcome::StoppedEngineCancelled, A12_DETAIL);
    assert_eq!(
        v.result.unwrap().detail.as_deref(),
        Some(A12_DETAIL),
        "the view keeps the harness's words; only the card and the status line are written for a person"
    );
}

#[test]
fn one_switch_is_singular_and_a_detail_with_only_that_sentence_gets_nothing_added() {
    let text = card_of("Ran on us.zai.glm-5.3 after 1 switch.");
    assert!(
        text.lines()
            .any(|l| l == "Detail    Ran on GLM 5.3 after 1 switch."),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l == "Model     GLM 5.3 (us.zai.glm-5.3)"),
        "{text}"
    );
    let s = status_line(&view(
        DirectOutcome::StoppedEngineCancelled,
        "Ran on us.zai.glm-5.3 after 1 switch.",
    ));
    assert!(s.contains(" ran on GLM 5.3 after 1 switch "), "{s}");
}

#[test]
fn another_route_of_the_requested_model_is_shown_with_its_id_so_the_two_lines_differ() {
    let text =
        card_of("Ran on us.anthropic.claude-sonnet-5-5 after 1 switch. the model provider failed");
    for line in [
        "Detail    Ran on Claude Sonnet 5.5 (us.anthropic.claude-sonnet-5-5) after 1 switch. The model provider failed.",
        "Model     Claude Sonnet 5.5 (us.anthropic.claude-sonnet-5-5)",
        &format!("Requested {REQUESTED_TEXT}"),
    ] {
        assert!(text.lines().any(|l| l == line), "no {line:?} in:\n{text}");
    }
    let s = status_line(&view(
        DirectOutcome::StoppedEngineCancelled,
        "Ran on us.anthropic.claude-sonnet-5-5 after 1 switch.",
    ));
    assert!(
        s.contains(" ran on Claude Sonnet 5.5 (us.anthropic.claude-sonnet-5-5) after 1 switch "),
        "{s}"
    );
}

#[test]
fn a_run_that_came_back_to_the_requested_model_keeps_its_model_row_and_has_no_requested_row() {
    let detail =
        "Ran on global.anthropic.claude-sonnet-5-5 after 2 switches. the model provider failed";
    let text = card_of(detail);
    assert!(
        text.lines().any(|l| l
            == "Detail    Ran on Claude Sonnet 5.5 after 2 switches. The model provider failed."),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l == format!("Model     {REQUESTED_TEXT}")),
        "{text}"
    );
    assert!(!text.contains("Requested"), "{text}");
    let s = status_line(&view(DirectOutcome::StoppedEngineCancelled, detail));
    assert!(!s.contains(" ran on "), "the run ended where it began: {s}");
}

#[test]
fn a_model_the_list_does_not_know_is_shown_as_the_harness_wrote_it() {
    let text =
        card_of("Ran on us.example.future-model-9 after 3 switches. the model provider failed");
    for line in [
        "Detail    Ran on us.example.future-model-9 after 3 switches. The model provider failed.",
        "Model     us.example.future-model-9",
        &format!("Requested {REQUESTED_TEXT}"),
    ] {
        assert!(text.lines().any(|l| l == line), "no {line:?} in:\n{text}");
    }
}

#[test]
fn the_sentences_agentd_joins_start_with_capitals_and_the_cause_ends_with_a_full_stop() {
    // The outage sentence leads when the run never switched; the cause that follows it was joined in lowercase.
    let text =
        card_of("Every route was down for the last 120 s. the engine ended the prompt cancelled");
    assert!(
        text.lines().any(|l| l
            == "Detail    Every route was down for the last 120 s. The engine ended the prompt cancelled."),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l == format!("Model     {REQUESTED_TEXT}"))
    );
    // Both lead sentences, and a cause that already has its full stop.
    let text = card_of(
        "Ran on us.zai.glm-5.3 after 2 switches. Every route was down for the last 45 s. the model provider failed (503).",
    );
    assert!(
        text.lines().any(|l| l
            == "Detail    Ran on GLM 5.3 after 2 switches. Every route was down for the last 45 s. The model provider failed (503)."),
        "{text}"
    );
}

#[test]
fn a_detail_without_those_lead_sentences_is_shown_exactly_as_the_harness_wrote_it() {
    for detail in [
        "the engine stopped after 50 requests",
        "no output for 900 s; pull request not opened: push refused",
        "Ran on global.anthropic.claude-sonnet-5-5 after lots of switches. x",
        "ran on us.zai.glm-5.3 after 2 switches. x",
        "Ran on after 2 switches. x",
    ] {
        let text = card_of(detail);
        assert!(
            text.lines().any(|l| l == format!("Detail    {detail}")),
            "{detail:?}:\n{text}"
        );
        assert!(!text.contains("Requested"), "{detail:?}:\n{text}");
        assert!(
            text.lines()
                .any(|l| l == format!("Model     {REQUESTED_TEXT}")),
            "{detail:?}:\n{text}"
        );
    }
}

#[test]
fn a_route_of_a_listed_model_is_named_by_its_model_and_an_alias_still_shows_the_full_id() {
    use ham_core::render::{model_name, model_text};
    assert_eq!(
        model_name("us.anthropic.claude-sonnet-5-5"),
        "Claude Sonnet 5.5"
    );
    assert_eq!(
        model_text("us.anthropic.claude-sonnet-5-5"),
        "Claude Sonnet 5.5 (us.anthropic.claude-sonnet-5-5)"
    );
    assert_eq!(model_name("us.zai.glm-5.3"), "GLM 5.3");
    assert_eq!(
        model_text("sonnet"),
        REQUESTED_TEXT,
        "an alias shows the id of its row"
    );
    assert_eq!(
        model_text("us.example.future-model-9"),
        "us.example.future-model-9"
    );
    assert_eq!(
        model_name("us.example.future-model-9"),
        "us.example.future-model-9"
    );
}

#[test]
fn a_sentence_that_only_looks_like_a_lead_sentence_is_not_touched() {
    for detail in [
        "Every route was down for the last many s. x",
        "Every route was down for the last 5 min. x",
        "Every route was down for the last 5 s x",
        "Ran on us.zai.glm-5.3 after 0 switches. x",
        "Ran on us.zai.glm-5.3 after 2 switchesX. x",
        "Ran on us.zai.glm-5.3",
    ] {
        let text = card_of(detail);
        assert!(
            text.lines().any(|l| l == format!("Detail    {detail}")),
            "{detail:?}:\n{text}"
        );
    }
}

#[test]
fn a_lead_sentence_without_a_cause_gets_no_extra_sentence_and_a_cause_gets_its_end_mark() {
    let text = card_of("Every route was down for the last 7 s.");
    assert!(
        text.lines()
            .any(|l| l == "Detail    Every route was down for the last 7 s."),
        "{text}"
    );
    // A cause with its own end mark keeps it; one that starts with a digit or a quote has nothing to capitalise.
    for (cause, shown) in [
        ("what happened?", "What happened?"),
        ("what happened!", "What happened!"),
        ("503 from the provider", "503 from the provider."),
        ("\"quoted\" cause", "\"quoted\" cause."),
    ] {
        let text = card_of(&format!("Every route was down for the last 7 s. {cause}"));
        assert!(
            text.lines()
                .any(|l| l == format!("Detail    Every route was down for the last 7 s. {shown}")),
            "{cause:?}:\n{text}"
        );
    }
}
