//! The model in the title and the status card never depends on one event (live proof A1, textstats leg: the card lacked the model name in 1
//! of 10 legs because `session.started` was missed). A session that names its model wins; until one does, the title and the card take the
//! model the person asked for when the run was started from the composer, else the model the run index holds for the run (the list route's
//! rows carry it), and the app reads the list once to get it when a run shows events and no model.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Msg};
use ham_core::runfilter::{IndexEntry, IndexedRead, Refresh, RunsRead};
use harness_protocol::api::v1::{EventsPage, StartRunResponse};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const OPUS: &str = "us.anthropic.claude-opus-5-5";

fn screen(app: &App) -> String {
    let (w, h) = (120u16, 40u16);
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

/// The events of a run whose `session.started` was never read.
fn without_session() -> Vec<Envelope> {
    vec![
        env(
            1,
            EventType::RunCreated,
            None,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            3,
            EventType::GoalStatus,
            None,
            json!({"status": "active", "phase": "green"}),
        ),
    ]
}

fn with_session(model: &str) -> Vec<Envelope> {
    let mut e = without_session();
    e.push(env(
        2,
        EventType::SessionStarted,
        None,
        json!({"agentId": "direct", "engine": "cline", "modelId": model}),
    ));
    e
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

fn first_line(app: &App) -> String {
    screen(app).lines().next().unwrap().to_string()
}

/// A run started from the composer with the model on its screen (`which` presses Right that many times on the Model field).
fn started_from_the_composer(opus: bool) -> App {
    let mut app = App::new_direct(
        "n".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings::default(),
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    for c in "Add a percent_change function".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    if opus {
        for _ in 0..5 {
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        for _ in 0..10 {
            if app.chosen_model() == Some(OPUS) {
                break;
            }
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert_eq!(
            app.chosen_model(),
            Some(OPUS),
            "the Model field offers Opus"
        );
        for _ in 0..5 {
            app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        }
    }
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(
        matches!(effects.as_slice(), [Effect::SubmitDirect { .. }]),
        "{effects:?}"
    );
    let started: StartRunResponse =
        serde_json::from_value(json!({"runId": RUN, "status": "active"})).unwrap();
    app.update(Msg::Started(Ok(started)));
    app
}

#[test]
fn a_run_started_from_the_composer_names_the_model_that_was_asked_for_before_any_session_does() {
    let mut app = started_from_the_composer(false);
    assert!(
        first_line(&app).contains("Claude Sonnet 5.5"),
        "{}",
        first_line(&app)
    );
    app.update(pages(without_session()));
    let text = screen(&app);
    assert!(first_line(&app).contains("Claude Sonnet 5.5"), "{text}");
    assert!(
        text.contains("Model  Claude Sonnet 5.5"),
        "the status card too: {text}"
    );
    let _ = SONNET;
}

#[test]
fn the_model_that_was_asked_for_is_the_one_on_the_screen_at_the_start_not_the_default() {
    let app = started_from_the_composer(true);
    assert!(
        first_line(&app).contains("Claude Opus 5.5"),
        "{}",
        first_line(&app)
    );
}

#[test]
fn a_session_that_names_its_model_wins_over_the_one_that_was_asked_for() {
    let mut app = started_from_the_composer(true);
    app.update(pages(with_session(SONNET)));
    let line = first_line(&app);
    assert!(
        line.contains("Claude Sonnet 5.5") && !line.contains("Opus"),
        "a run that switched models names the one it ran on: {line}"
    );
}

#[test]
fn a_run_opened_from_the_list_takes_the_model_the_run_index_holds() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(without_session()));
    assert!(!first_line(&app).contains("Claude"), "{}", first_line(&app));
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::First {
            runs: vec![],
            has_more: false,
        },
        index: vec![IndexEntry {
            run_id: RUN.into(),
            title: Some("Add".into()),
            model: Some(SONNET.into()),
            name: None,
            name_source: None,
            tags: None,
        }],
    })));
    assert!(
        first_line(&app).contains("Claude Sonnet 5.5"),
        "{}",
        first_line(&app)
    );
}

#[test]
fn a_run_that_shows_events_and_no_model_reads_the_list_once_to_get_it() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(without_session()));
    let mut asked = 0;
    for _ in 0..30 {
        asked += app
            .tick()
            .iter()
            .filter(|e| matches!(e, Effect::RefreshRuns(Refresh::Timer)))
            .count();
    }
    assert_eq!(asked, 1, "once, while the read is out");
    // The answer comes without the run: it is asked again later, not in a loop.
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::First {
            runs: vec![],
            has_more: false,
        },
        index: vec![],
    })));
    let mut again = 0;
    for _ in 0..30 {
        again += app
            .tick()
            .iter()
            .filter(|e| matches!(e, Effect::RefreshRuns(Refresh::Timer)))
            .count();
    }
    assert!(again <= 1, "not every second: {again}");
}

#[test]
fn a_run_whose_session_named_the_model_never_reads_the_list_for_it() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(with_session(SONNET)));
    for _ in 0..30 {
        assert!(
            app.tick()
                .iter()
                .all(|e| !matches!(e, Effect::RefreshRuns(_)))
        );
    }
}
