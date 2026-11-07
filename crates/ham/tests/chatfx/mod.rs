//! Shared set-up of the chat-client screen tests: a run on screen in typing mode, keys with their own clock, the screen as
//! text, the events of a working or finished direct run, pages of the stored conversation and messages as the server shows them. Each test
//! file that uses it declares `mod chatfx;`.
#![allow(dead_code, clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::ext::{Done, Job};
use ham::tui::{App, DirectSettings, Effect, Msg};
use ham_core::transcript::TranscriptPage;
use harness_protocol::api::v1::EventsPage;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const RUN: &str = "01K6A0000000000000000000R1";
pub const INC: &str = "01M5A2R7K3C9XJ4D8E6F000001";
pub const T0: &str = "2026-10-16T08:15:00Z";

/// Keys with their own clock, so the 30 ms burst gap and the 300 ms confirm delay are exercised without sleeping.
pub struct Clock {
    base: Instant,
    ms: u64,
}

impl Clock {
    /// A clock for a test: 5 s after the clock that opened the run screen (`ready`), so the first key is not part of a burst.
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            ms: 5_000,
        }
    }

    /// The clock `ready` uses.
    pub fn early() -> Self {
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

pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub fn press(app: &mut App, clock: &mut Clock, code: KeyCode, wait_ms: u64) -> Vec<Effect> {
    let at = clock.after(wait_ms);
    app.handle_key_at(key(code), at)
}

/// Types `text` at a calm pace (one key every 50 ms), then waits 200 ms.
pub fn type_text(app: &mut App, clock: &mut Clock, text: &str) {
    for c in text.chars() {
        let at = clock.after(50);
        let effects = app.handle_key_at(key(KeyCode::Char(c)), at);
        assert!(effects.is_empty(), "typing {c:?} caused {effects:?}");
    }
    clock.after(200);
}

pub fn enter(app: &mut App, clock: &mut Clock, wait_ms: u64) -> Vec<Effect> {
    press(app, clock, KeyCode::Enter, wait_ms)
}

/// Types `line` and sends it with Enter; returns the effects of the Enter.
pub fn send(app: &mut App, clock: &mut Clock, line: &str) -> Vec<Effect> {
    type_text(app, clock, line);
    enter(app, clock, 10)
}

pub fn screen(app: &App, w: u16, h: u16) -> String {
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

fn harness(seq: u64, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{seq:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq,
        ts: T0.into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

fn agent(seq: u64, t: EventType, v: Value) -> Envelope {
    Envelope {
        authority: Authority::Agent,
        agent_id: Some("direct".into()),
        ..harness(seq, t, v)
    }
}

/// The events every run has: created, the session started on the first engine, active.
pub fn base_events() -> Vec<Envelope> {
    vec![
        harness(
            1,
            EventType::RunCreated,
            json!({"target": {"mode": "s3-only"}}),
        ),
        harness(
            2,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": ham_core::models::DEFAULT_MODEL_ID}),
        ),
        harness(
            3,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        ),
    ]
}

/// The events that end a run: complete, delivered, the harness's completion record. After [`base_events`] they make a finished run.
pub fn completion_events() -> Vec<Envelope> {
    vec![
        harness(
            10,
            EventType::GoalStatus,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        harness(
            11,
            EventType::DeliveryReady,
            json!({"artifactId": "01M5A2R7K3C9XJ4D8E6F0000AA", "sha256": "ab".repeat(32), "sizeBytes": 1234}),
        ),
        harness(
            12,
            EventType::RunCompleted,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]
}

/// The events of a run that is done.
pub fn finished_events() -> Vec<Envelope> {
    let mut e = base_events();
    e.extend(completion_events());
    e
}

/// A `session.capabilities` event of the direct agent carrying `record`.
pub fn capability_event(seq: u64, record: Value) -> Envelope {
    agent(seq, EventType::SessionCapabilities, record)
}

pub fn feed(app: &mut App, events: Vec<Envelope>) {
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in events {
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
}

/// The run on screen in typing mode with `events` loaded; the effects of opening it are returned too.
pub fn ready_with(events: Vec<Envelope>) -> (App, Vec<Effect>) {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let opened = app.open_run(RUN);
    feed(&mut app, events);
    let mut clock = Clock::early();
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    (app, opened)
}

pub fn ready(events: Vec<Envelope>) -> App {
    ready_with(events).0
}

// ---- the stored conversation ------------------------------------------------------------------------------------------------

/// A record as the transcript route returns it.
pub fn rec(seq: u64, n: u64, kind: &str, data: Value) -> Value {
    let dir = if matches!(kind, "prompt" | "cancel") {
        "out"
    } else {
        "in"
    };
    json!({"seq": seq, "n": n, "incarnation": INC, "origin": "engine", "t": format!("2026-10-16T08:15:{:02}Z", n % 60),
           "dir": dir, "kind": kind, "data": data})
}

pub fn msg(seq: u64, n: u64, id: &str, text: &str) -> Value {
    rec(
        seq,
        n,
        "agent_message_chunk",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": text}}),
    )
}

pub fn tool(seq: u64, n: u64, id: &str, title: &str, status: &str) -> Value {
    rec(
        seq,
        n,
        "tool_call",
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "execute", "status": status}),
    )
}

/// A page of the route.
pub fn page(
    state: &str,
    records: Vec<Value>,
    prev: Option<u64>,
    next: Option<&str>,
) -> TranscriptPage {
    serde_json::from_value(json!({
        "runId": RUN, "state": state, "records": records, "gaps": [], "completeness": [],
        "prev": prev, "next": next, "more": false, "skippedKeys": 0
    }))
    .unwrap()
}

pub fn with_gap(mut p: TranscriptPage, code: &str, from: u64, to: u64) -> TranscriptPage {
    let kind = match code {
        "missing-chunk" | "tail-missing" => "missing",
        _ => "unreadable",
    };
    p.gaps =
        serde_json::from_value(json!([{"kind": kind, "code": code, "fromSeq": from, "toSeq": to}]))
            .unwrap();
    p
}

/// The answer to a transcript request.
pub fn answer(read: ham_core::stored::Read, p: TranscriptPage) -> Msg {
    Msg::Ext(Done::Transcript {
        run_id: RUN.into(),
        read,
        result: Ok(p),
    })
}

/// The transcript request among `effects`, if any.
pub fn transcript_job(
    effects: &[Effect],
) -> Option<(
    ham_core::transcript::TranscriptQuery,
    ham_core::stored::Read,
)> {
    effects.iter().find_map(|e| match e {
        Effect::Ext(Job::Transcript { query, read, .. }) => Some((query.clone(), *read)),
        _ => None,
    })
}

pub fn jobs(effects: &[Effect]) -> Vec<&Job> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Ext(j) => Some(j),
            _ => None,
        })
        .collect()
}
