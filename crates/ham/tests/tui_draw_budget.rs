//! A transcript of 100,000 lines draws within a fixed budget.
//!
//! The run screen holds 17,000 steps of Cline: a message of five lines and a tool call each, 102,000 lines at 200 columns. The first draw
//! lays the blocks out; the draws after it (the end, a page up, a jump to the top, a fold) must not lay the whole transcript out again.
//! The budgets are for a debug build on the build host; `HAMH_DRAW_BUDGET_MS` replaces the budget of one draw (the red run of this
//! test sets it to 1 ms, to show that the check can fail).
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;
const STEPS: u32 = 17_000;
const LINES_PER_STEP: u32 = 6;

/// The most a draw may take, in milliseconds: the first one lays the transcript out, the later ones scroll over what is laid out.
const FIRST_DRAW_MS: u64 = 3_000;
const LATER_DRAW_MS: u64 = 500;

fn budget(default_ms: u64) -> Duration {
    let ms = std::env::var("HAMH_DRAW_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
}

fn env(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A{n:021}"),
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

fn big_run() -> App {
    let mut events = vec![
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
    for i in 0..STEPS {
        let text = format!(
            "Step {i:05} begins.\nIt reads the file and checks the result against the test.\nThe test failed at first, so the code changes.\nNow the test passes, and the next one is written.\nOn to step {}.",
            i + 1
        );
        events.push(env(
            100 + 3 * i,
            EventType::TurnProgress,
            Some("direct"),
            json!({"turn": 1, "kind": "message", "text": text}),
        ));
        events.push(env(
            101 + 3 * i,
            EventType::ToolSummary,
            Some("direct"),
            json!({"toolCallId": format!("t{i}"), "title": format!("run_commands: pytest -q tests/test_{i:05}.py"), "kind": "execute", "status": "pending"}),
        ));
        events.push(env(
            102 + 3 * i,
            EventType::TurnProgress,
            Some("direct"),
            json!({"toolCallId": format!("t{i}"), "status": "completed"}),
        ));
    }
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let started = Instant::now();
    app.update(pages(events));
    eprintln!("ingest: {:?}", started.elapsed());
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(d)));
    assert_eq!(app.mode(), Mode::Browse);
    app
}

/// Draws once at 200x60 and returns what it took and the text of the screen.
fn draw(app: &App) -> (Duration, String) {
    let (w, h) = (200, 60);
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    let started = Instant::now();
    t.draw(|f| app.draw(f)).unwrap();
    let took = started.elapsed();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    (took, out)
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn a_transcript_of_a_hundred_thousand_lines_draws_within_the_budget() {
    let lines = u64::from(STEPS) * u64::from(LINES_PER_STEP);
    assert!(lines >= 100_000, "TRIGGER ABSENT: only {lines} lines");
    eprintln!("TRIGGER PRESENT: {lines} transcript lines");
    let mut app = big_run();

    let (first, text) = draw(&app);
    eprintln!("first draw: {first:?}");
    assert!(
        text.contains("On to step 17000."),
        "the end of the transcript is not on screen"
    );
    assert!(
        first < budget(FIRST_DRAW_MS),
        "the first draw took {first:?}"
    );

    // Scrolling over a transcript that is already laid out.
    let mut worst = Duration::ZERO;
    for (what, code) in [
        ("page up", KeyCode::PageUp),
        ("page up", KeyCode::PageUp),
        ("home", KeyCode::Home),
        ("page down", KeyCode::PageDown),
        ("end", KeyCode::End),
    ] {
        key(&mut app, code);
        let (took, _) = draw(&app);
        eprintln!("{what}: {took:?}");
        worst = worst.max(took);
        assert!(took < budget(LATER_DRAW_MS), "{what} took {took:?}");
    }
    // The top of the transcript is the prompt and the first step.
    key(&mut app, KeyCode::Home);
    let (_, text) = draw(&app);
    assert!(
        text.contains("Build a todo API with tests"),
        "the top is not the prompt"
    );
    assert!(text.contains("Step 00000 begins."));
    eprintln!("worst later draw: {worst:?}");
}
