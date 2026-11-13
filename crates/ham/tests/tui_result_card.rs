//! The status card of the run screen beside the label of a finished run: the facts the harness read at the end sit
//! directly under the `Outcome` line, one line each; the whole label of a harness stop is shown, not cut at the card's width, and the brake card
//! of the rule inside one round names its reason in plain words. Nothing says verified, tested, gated or evaluated.
#![allow(clippy::unwrap_used)]

use ham::tui::{App, DirectSettings, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;
const RED: &str = "The last entry of the run log is a red run: pytest exited 1 and 2 failed.";
const STOP: &str = "The harness ended the run: Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared.";
const STOP_LABEL: &str = "Cline stopped: the harness ended the run (Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared)";

fn screen(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        let mut x = 0;
        while x < w {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn env(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: u64::from(n),
        ts: "2026-10-14T20:00:00Z".into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    }
}

fn on_screen(events: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let mut all = vec![
        env(
            1,
            EventType::RunCreated,
            None,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            2,
            EventType::SessionStarted,
            None,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        ),
        env(
            3,
            EventType::GoalStatus,
            None,
            json!({"status": "active", "phase": "green"}),
        ),
    ];
    all.extend(events);
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in all {
        let s = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
        by_shard.entry(s).or_default().push(e);
    }
    app.update(Msg::Pages(Ok(by_shard
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect())));
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "complete", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(d)));
    app
}

fn completed(n: u32, extra: Value) -> Envelope {
    let mut p =
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        p[k] = x;
    }
    env(n, EventType::RunCompleted, None, p)
}

fn flat(s: &str) -> String {
    s.chars()
        .map(|c| {
            if ('\u{2500}'..='\u{257f}').contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn a_red_last_log_entry_shows_directly_under_the_outcome_label_and_the_label_is_unchanged() {
    let app = on_screen(vec![completed(
        10,
        json!({"observations": [{"kind": "last_log_entry_red", "text": RED, "command": "pytest", "exit": "1", "failed": "2"}]}),
    )]);
    let text = screen(&app, 120, 40);
    let rows: Vec<&str> = text.lines().collect();
    let at = rows
        .iter()
        .position(|l| l.contains("Outcome Cline reported done"))
        .unwrap_or_else(|| panic!("no outcome row:\n{text}"));
    let next = rows[at + 1];
    assert!(
        next.contains(RED),
        "the fact is on the row under the label:\n{text}"
    );
    assert!(
        next.contains("! "),
        "a red log entry wears the warning marker: {next:?}"
    );
    let low = next.to_lowercase();
    for w in ["verified", "tested", "gated", "evaluated"] {
        assert!(!low.contains(w), "{w} in {next:?}");
    }
}

#[test]
fn a_run_with_no_observation_shows_no_such_line() {
    let app = on_screen(vec![completed(10, json!({}))]);
    let text = screen(&app, 120, 40);
    assert!(text.contains("Outcome Cline reported done"), "{text}");
    assert!(!text.contains("last entry of the run log"), "{text}");
}

#[test]
fn a_kind_this_client_does_not_know_is_a_plain_line_without_the_marker() {
    let app = on_screen(vec![completed(
        10,
        json!({"observations": [{"kind": "something_new", "text": "A newer writer's fact."}]}),
    )]);
    let text = screen(&app, 120, 40);
    let row = text
        .lines()
        .find(|l| l.contains("A newer writer's fact."))
        .unwrap();
    assert!(!row.contains("! "), "{row:?}");
}

#[test]
fn the_whole_label_of_a_harness_stop_is_shown_at_a_narrow_width() {
    let app = on_screen(vec![
        env(
            9,
            EventType::TurnProgress,
            Some("direct"),
            json!({"turn": 1, "kind": "goal-loop", "phase": "brake", "rule": "round-no-progress", "text": STOP}),
        ),
        env(
            10,
            EventType::RunCompleted,
            None,
            json!({"status": "cleared", "outcome": "stopped_other", "label": STOP_LABEL, "detail": STOP}),
        ),
    ]);
    let text = flat(&screen(&app, 80, 60));
    assert!(
        text.contains(STOP_LABEL),
        "the label is not cut at the card's width:\n{text}"
    );
}

#[test]
fn the_brake_card_of_the_round_rule_says_why_in_plain_words() {
    let app = on_screen(vec![
        env(
            9,
            EventType::TurnProgress,
            Some("direct"),
            json!({"turn": 1, "kind": "goal-loop", "phase": "brake", "rule": "round-no-progress", "text": STOP}),
        ),
        env(
            10,
            EventType::RunCompleted,
            None,
            json!({"status": "cleared", "outcome": "stopped_other", "label": STOP_LABEL, "detail": STOP}),
        ),
    ]);
    let text = flat(&screen(&app, 120, 60));
    assert!(text.contains(STOP), "the brake's own sentence: {text}");
    assert!(
        text.contains("Cline kept calling tools inside one round, but no file changed and no new text appeared, so the harness ended the run."),
        "{text}"
    );
    assert!(!text.contains("no words for"), "{text}");
    assert!(text.contains(&format!("ham continue {RUN}")), "{text}");
}
