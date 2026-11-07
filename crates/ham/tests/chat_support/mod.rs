//! Shared set-up for the screen tests of the chat box (history, paste placeholder, editor): a run on screen in typing mode, a clock that
//! types at a person's pace, and a screen reader. Included with `mod chat_support;`.
#![allow(dead_code, clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Effect, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const RUN: &str = "01K6A0000000000000000000R1";
pub const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
pub const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;
pub const PROMPT: &str = "Build a todo API with tests";

pub struct Clock {
    base: Instant,
    ms: u64,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            ms: 0,
        }
    }

    pub fn after(&mut self, ms: u64) -> Instant {
        self.ms += ms;
        self.base + Duration::from_millis(self.ms)
    }
}

pub fn buffer(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

pub fn text_of(buf: &Buffer) -> String {
    let (w, h) = (buf.area.width, buf.area.height);
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

pub fn screen(app: &App) -> String {
    text_of(&buffer(app, 120, 40))
}

pub fn env(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
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

pub fn pages(events: Vec<Envelope>) -> Msg {
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

pub fn start_events() -> Vec<Envelope> {
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

/// A blocking question that waits for the person (the run accepts a message).
pub fn question() -> Envelope {
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

pub fn detail() -> Msg {
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": PROMPT, "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

pub fn press(app: &mut App, clock: &mut Clock, code: KeyCode, wait_ms: u64) -> Vec<Effect> {
    press_mod(app, clock, code, KeyModifiers::NONE, wait_ms)
}

pub fn press_mod(
    app: &mut App,
    clock: &mut Clock,
    code: KeyCode,
    mods: KeyModifiers,
    wait_ms: u64,
) -> Vec<Effect> {
    let at = clock.after(wait_ms);
    app.handle_key_at(KeyEvent::new(code, mods), at)
}

pub fn ctrl(app: &mut App, clock: &mut Clock, c: char) -> Vec<Effect> {
    press_mod(app, clock, KeyCode::Char(c), KeyModifiers::CONTROL, 60)
}

/// The run on screen with `extra` events, in typing mode, ready for a calm typist.
pub fn ready_with(extra: Vec<Envelope>) -> (App, Clock) {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(1_792_000_000);
    app.open_run(RUN);
    let mut events = start_events();
    events.extend(extra);
    app.update(pages(events));
    app.update(detail());
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    assert_eq!(app.mode(), Mode::Typing);
    // The 5 s after the `i` keep the first typed key out of its burst.
    clock.after(5_000);
    (app, clock)
}

pub fn ready() -> (App, Clock) {
    ready_with(vec![])
}

/// Types `text` one key at a time at a person's pace, then waits so that the next Enter is calm.
pub fn type_text(app: &mut App, clock: &mut Clock, text: &str) {
    for c in text.chars() {
        let effects = press(app, clock, KeyCode::Char(c), 50);
        assert!(effects.is_empty(), "typing {c:?} caused {effects:?}");
    }
    clock.after(300);
}

pub fn enter(app: &mut App, clock: &mut Clock) -> Vec<Effect> {
    press(app, clock, KeyCode::Enter, 10)
}

/// Types `text` and sends it with Enter.
pub fn send(app: &mut App, clock: &mut Clock, text: &str) -> Vec<Effect> {
    type_text(app, clock, text);
    let effects = enter(app, clock);
    clock.after(400);
    effects
}
