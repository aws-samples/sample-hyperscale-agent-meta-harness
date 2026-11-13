//! The card of a goal-loop brake on the run screen: when the goal module ended the run by one of
//! its stopping rules, the chat view shows an alert card after the status: which rule, how far the loop got, Cline's own `BLOCKED:` line and the
//! `ham continue` command. A run that no rule stopped has no such card. No line calls the work verified, tested, gated or evaluated,
//! and the label of the run is the one of `run.completed`.
#![allow(clippy::unwrap_used)]

use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

fn draw(app: &App, w: u16, h: u16) -> (String, Terminal<TestBackend>) {
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
    (out, t)
}

fn screen(app: &App) -> String {
    draw(app, 120, 40).0
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

fn pages(events: Vec<Envelope>) -> Msg {
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in events {
        let s = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
        by_shard.entry(s).or_default().push(e);
    }
    Msg::Pages(Ok(by_shard
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect()))
}

fn detail() -> Msg {
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z",
                    "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

fn start() -> Vec<Envelope> {
    vec![
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
    ]
}

fn goal(n: u32, phase: &str, extra: Value) -> Envelope {
    let mut v = json!({"turn": 1, "kind": "goal-loop", "phase": phase});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    env(n, EventType::TurnProgress, Some("direct"), v)
}

fn on_screen(events: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let mut all = start();
    all.extend(events);
    app.update(pages(all));
    app.update(detail());
    assert_eq!(app.mode(), Mode::Browse);
    app
}

const BLOCKED: &str = "The harness ended the run: Cline ended 2 rounds in a row with the same last line. Its words: \"BLOCKED: the API key is missing\"";

fn braked() -> Vec<Envelope> {
    vec![
        goal(10, "on", json!({})),
        goal(11, "round", json!({"rounds": 3, "status": "completed"})),
        goal(
            12,
            "brake",
            json!({"rule": "blocked-twice", "text": BLOCKED}),
        ),
        env(
            13,
            EventType::RunCompleted,
            None,
            json!({"outcome": "stopped_other",
                   "label": "Cline stopped: the harness ended the run (Cline ended 2 rounds in a row with the same last line)",
                   "detail": BLOCKED}),
        ),
    ]
}

#[test]
fn a_brake_shows_a_card_with_the_rule_the_rounds_the_engines_line_and_the_follow_up_command() {
    let app = on_screen(braked());
    let text = screen(&app);
    assert!(
        text.contains("goal loop stopped by the harness"),
        "no card title:\n{text}"
    );
    assert!(
        text.contains("Cline ended 2 rounds in a row with the same last line."),
        "{text}"
    );
    assert!(
        text.contains("The goal loop ran 3 rounds; the last one completed."),
        "{text}"
    );
    assert!(
        text.contains("Last words: BLOCKED: the API key is missing"),
        "{text}"
    );
    assert!(text.contains(&format!("ham continue {RUN}")), "{text}");
    // The label of the run is still the one of run.completed.
    assert!(
        text.contains("Cline stopped: the harness ended the run"),
        "{text}"
    );
    println!(
        "TUI-GOAL-BRAKE TRIGGER PRESENT: card title, rounds line, BLOCKED line and ham continue shown"
    );
}

#[test]
fn a_loop_that_no_rule_stopped_has_no_brake_card() {
    let app = on_screen(vec![
        goal(10, "on", json!({})),
        goal(11, "round", json!({"rounds": 1, "status": "completed"})),
        goal(12, "done", json!({"summary": "Done."})),
    ]);
    let text = screen(&app);
    assert!(!text.contains("goal loop stopped by the harness"), "{text}");
    assert!(!text.contains("ham continue"), "{text}");
}

#[test]
fn the_card_says_nothing_about_the_work_being_checked() {
    let app = on_screen(braked());
    let card: Vec<String> = screen(&app)
        .lines()
        .skip_while(|l| !l.contains("goal loop stopped by the harness"))
        .take(12)
        .map(str::to_lowercase)
        .collect();
    assert!(!card.is_empty());
    for l in card {
        for w in ["verified", "tested", "gated", "evaluated"] {
            assert!(!l.contains(w), "{l:?}");
        }
    }
}
