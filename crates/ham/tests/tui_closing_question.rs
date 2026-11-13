//! A question that closes while the person is answering it: the run screen is polled while the box has the focus,
//! so the next poll that carries the end of the question shows it at once: the card goes, the notice row says how it ended, and the box
//! says that the question closed, so that the half-written answer is not sent as one by mistake. The draft is kept and Enter then sends
//! it as an ordinary message (Cline is not waiting for an answer, so the box says so and keeps the text).
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Effect, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const RUN: &str = "01K6A0000000000000000000R1";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

fn screen(app: &App) -> String {
    let (w, h) = (120, 40);
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

fn question() -> Envelope {
    env(
        40,
        EventType::TurnProgress,
        Some("direct"),
        json!({
            "turn": 1, "kind": "needs-input",
            "text": "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)",
            "requestId": REQ, "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
            "stopOutcome": "stopped_engine_cancelled",
            "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                         "lastError": "x", "runS": 412, "sessions": 1},
            "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": true,
            "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
            "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-14T20:30:00Z"
        }),
    )
}

fn closed(n: u32, how: &str) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "input-closed", "text": "closed", "requestId": REQ, "how": how, "waitedS": 1800}),
    )
}

/// The run on screen with its question open, in typing mode.
fn typing_an_answer() -> (App, Instant) {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(vec![
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
        question(),
    ]));
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(d)));
    let base = Instant::now();
    let mut ms = 0u64;
    ms += 50;
    app.handle_key_at(
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        base + Duration::from_millis(ms),
    );
    assert_eq!(app.mode(), Mode::Typing);
    ms += 5_000;
    for c in "half an answer".chars() {
        ms += 50;
        app.handle_key_at(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            base + Duration::from_millis(ms),
        );
    }
    assert_eq!(app.chat_text().as_deref(), Some("half an answer"));
    (app, base + Duration::from_millis(ms + 2_000))
}

#[test]
fn while_the_question_is_open_the_box_says_it_is_an_answer() {
    let (app, _) = typing_an_answer();
    let text = screen(&app);
    assert!(text.contains("Cline needs you"), "{text}");
    assert!(!text.contains("the question closed"), "{text}");
}

#[test]
fn the_poll_that_carries_the_end_of_the_question_shows_it_while_the_box_has_the_focus() {
    let (mut app, _) = typing_an_answer();
    // The next poll brings the end of the question: nobody answered in time.
    let effects = app.update(pages(vec![closed(41, "expired")]));
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Answer { .. })),
        "{effects:?}"
    );
    let text = screen(&app);
    // The card is gone and the notice row says how it ended.
    assert!(!text.contains("Cline needs you"), "{text}");
    assert!(text.contains("window closed: nobody answered"), "{text}");
    // The box says that the question closed, and what Enter does now.
    assert!(
        text.contains("the question closed") && text.contains("Enter sends this as a message"),
        "{text}"
    );
    // The person is still typing, and the half-written answer is still there.
    assert_eq!(app.mode(), Mode::Typing);
    assert_eq!(app.chat_text().as_deref(), Some("half an answer"));
}

#[test]
fn every_way_a_question_ends_is_named_in_the_box() {
    for (how, words) in [
        ("cancelled", "the run was cancelled"),
        ("deadline", "the run reached its time limit"),
        ("superseded", "the question was replaced by a newer one"),
        ("answered:finish", "answered: finish"),
    ] {
        let (mut app, _) = typing_an_answer();
        app.update(pages(vec![closed(41, how)]));
        let text = screen(&app);
        assert!(text.contains("the question closed"), "{how}:\n{text}");
        assert!(text.contains(words), "{how}:\n{text}");
    }
}

#[test]
fn enter_after_the_end_sends_no_answer_and_keeps_the_text() {
    let (mut app, at) = typing_an_answer();
    app.update(pages(vec![closed(41, "expired")]));
    let effects = app.handle_key_at(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), at);
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Answer { .. })),
        "{effects:?}"
    );
    // The title says "Enter sends this as a message", and it does: a message for after the turn, with its pending row.
    let sent: Vec<_> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Ext(ham::tui::ext::Job::SendMessage { request, .. }) => {
                Some(request.text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(sent, ["half an answer"], "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert!(screen(&app).contains("half an answer"));
}

#[test]
fn the_banner_goes_when_the_box_is_emptied() {
    let (mut app, at) = typing_an_answer();
    app.update(pages(vec![closed(41, "expired")]));
    assert!(screen(&app).contains("the question closed"));
    // Ctrl-C clears a box that holds text.
    app.handle_key_at(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), at);
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert!(!screen(&app).contains("the question closed"));
}

#[test]
fn a_question_that_closes_when_the_box_is_empty_adds_no_banner_to_the_box() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(vec![
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
        question(),
    ]));
    app.update(pages(vec![closed(41, "expired")]));
    let text = screen(&app);
    assert!(text.contains("window closed: nobody answered"), "{text}");
    assert!(!text.contains("the question closed"), "{text}");
}
