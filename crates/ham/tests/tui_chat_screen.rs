//! The run screen is the chat view: the header line, the conversation (the prompt, what Cline
//! said, the tool calls), the cards at its end (the status and agents folded, the question, the blocked panel, the delivery card), the
//! rows of the message on its way, the pinned message box, the notice row and the footer. The browse keys move through the conversation:
//! the arrow keys, PgUp, PgDn and Home/End scroll it, Tab and Shift-Tab select a block and Space folds or opens the selected one.
//! The result panel and the preview of a command sit between the conversation and the box.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Effect, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

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

fn detail(objective: &str) -> Msg {
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": objective, "tokensUsed": 113000, "costUsd": 0.14
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

fn start_events() -> Vec<Envelope> {
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

fn said(n: u32, text: &str) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "message", "text": text}),
    )
}

fn call(n: u32, id: &str, title: &str) -> Envelope {
    env(
        n,
        EventType::ToolSummary,
        Some("direct"),
        json!({"toolCallId": id, "title": title, "kind": "execute", "status": "pending"}),
    )
}

fn done(n: u32, id: &str) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"toolCallId": id, "status": "completed"}),
    )
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

/// The run on screen in browse mode with `events` read and the prompt known.
fn on_screen(events: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(events));
    app.update(detail("Build a todo API with tests"));
    assert_eq!(app.mode(), Mode::Browse);
    app
}

fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn row_of(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on the screen:\n{text}"))
}

fn conversation() -> Vec<Envelope> {
    let mut e = start_events();
    e.push(said(10, "I will write the failing test first."));
    e.push(call(11, "t1", "run_commands: pytest -q"));
    e.push(done(12, "t1"));
    e
}

#[test]
fn the_run_screen_starts_with_the_header_line_of_the_chat_view() {
    let app = on_screen(conversation());
    let text = screen(&app, 100, 30);
    let first = text.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("ham · Run {RUN} · Claude Sonnet 5.5")),
        "{first:?}\n{text}"
    );
    assert!(first.contains("active"), "{first:?}");
}

#[test]
fn the_conversation_shows_the_prompt_what_cline_said_and_its_tool_calls() {
    let app = on_screen(conversation());
    let text = screen(&app, 100, 30);
    assert!(
        text.contains("you      Build a todo API with tests"),
        "{text}"
    );
    assert!(
        text.contains("cline    I will write the failing test first."),
        "{text}"
    );
    assert!(text.contains("tool     run_commands: pytest -q"), "{text}");
    assert!(
        row_of(&text, "you      Build") < row_of(&text, "cline    I will"),
        "{text}"
    );
}

#[test]
fn the_message_box_is_pinned_at_the_bottom_above_the_footer() {
    let app = on_screen(conversation());
    let text = screen(&app, 100, 30);
    let lines: Vec<&str> = text.lines().collect();
    let title = row_of(&text, "Message to Cline");
    assert!(
        title >= lines.len() - 5,
        "the box is not at the bottom:\n{text}"
    );
    let footer = lines.iter().rposition(|l| !l.trim().is_empty()).unwrap();
    assert!(footer > title, "{text}");
    assert!(lines[footer].contains("a artifacts"), "{:?}", lines[footer]);
}

#[test]
fn the_box_title_carries_the_typing_marker_only_while_typing() {
    let mut app = on_screen(conversation());
    let text = screen(&app, 100, 30);
    assert!(text.contains("Message to Cline"), "{text}");
    assert!(!text.contains("▶ Message to Cline"), "{text}");
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.mode(), Mode::Typing);
    let text = screen(&app, 100, 30);
    // The marker shows where the focus is without colour or bold, as it did on the old dashboard.
    assert!(text.contains("▶ Message to Cline"), "{text}");
    press(&mut app, KeyCode::Esc);
    let text = screen(&app, 100, 30);
    assert!(!text.contains("▶ Message to Cline"), "{text}");
}

#[test]
fn opening_a_run_asks_for_its_detail_once_so_that_the_prompt_can_show() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let effects = app.open_run(RUN);
    let asks = |e: &[Effect]| {
        e.iter()
            .filter(|e| matches!(e, Effect::LoadDetail { run_id } if run_id == RUN))
            .count()
    };
    assert_eq!(asks(&effects), 1, "{effects:?}");
    // The same run again is not a new open: nothing is asked twice.
    assert_eq!(asks(&app.open_run(RUN)), 0);
}

#[test]
fn the_open_question_is_a_card_at_the_end_of_the_conversation_with_its_keys() {
    let mut events = conversation();
    events.push(question());
    let app = on_screen(events);
    let text = screen(&app, 100, 40);
    assert!(text.contains("Cline needs you"), "{text}");
    assert!(text.contains("p message"), "{text}");
    assert!(
        row_of(&text, "cline    I will write") < row_of(&text, "Cline needs you"),
        "{text}"
    );
}

#[test]
fn the_status_is_a_card_and_the_agents_are_a_folded_card_that_space_opens() {
    let mut app = on_screen(conversation());
    let text = screen(&app, 100, 40);
    assert!(text.contains("status"), "{text}");
    assert!(text.contains("active (green)"), "{text}");
    assert!(text.contains("agents 1"), "{text}");
    // The agent rows are inside the folded card: not shown until it is opened.
    assert!(!text.contains("AGENT"), "{text}");
    // Shift-Tab selects the last block, which is that card; Space opens it.
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Char(' '));
    let text = screen(&app, 100, 40);
    assert!(text.contains("AGENT"), "{text}");
    assert!(text.contains("direct"), "{text}");
}

#[test]
fn page_up_leaves_the_end_and_says_so_and_end_follows_again() {
    let mut events = start_events();
    for i in 0..30u32 {
        events.push(said(100 + 3 * i, &format!("Step {i:02} is done.")));
        events.push(call(
            101 + 3 * i,
            &format!("t{i}"),
            &format!("tool number {i:02}"),
        ));
        events.push(done(102 + 3 * i, &format!("t{i}")));
    }
    let mut app = on_screen(events);
    let text = screen(&app, 100, 30);
    assert!(text.contains("tool number 29"), "{text}");
    assert!(!text.contains("Step 00"), "{text}");
    press(&mut app, KeyCode::PageUp);
    let text = screen(&app, 100, 30);
    assert!(!text.contains("tool number 29"), "{text}");
    press(&mut app, KeyCode::Home);
    let text = screen(&app, 100, 30);
    assert!(text.contains("Build a todo API"), "{text}");
    press(&mut app, KeyCode::End);
    let text = screen(&app, 100, 30);
    assert!(text.contains("tool number 29"), "{text}");
}

#[test]
fn the_notice_row_and_a_reconnect_banner_show_on_the_chat_view() {
    let mut app = on_screen(conversation());
    // The overlay shows once the failures have lasted past the grace of one second.
    app.update(Msg::Pages(Err("timeout".into())));
    app.tick();
    app.update(Msg::Pages(Err("timeout".into())));
    let text = screen(&app, 100, 30);
    assert!(
        text.contains("Connection lost, retrying (1 s): timeout"),
        "{text}"
    );
    assert!(
        text.contains("the run keeps going in AWS"),
        "the banner lost its explanation:\n{text}"
    );
}
