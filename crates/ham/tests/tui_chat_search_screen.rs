//! Search inside one chat, wired to the run screen: `/` in browse mode opens a bar above the box,
//! every character is the query's while it is open (an `n` or a `q` too), the bar counts the matches (`/pytest  1/2`), the view jumps to
//! the current one, Enter accepts so that `n` and `N` walk the matches, Esc cancels (the view goes back) or, once accepted, clears the
//! marks. A match in a folded card opens it. New events update the matches; leaving the run drops the search.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

fn draw(app: &App, w: u16, h: u16) -> (String, (u16, u16)) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let pos = t.backend_mut().get_cursor_position().unwrap();
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
    (out, (pos.x, pos.y))
}

fn screen(app: &App, w: u16, h: u16) -> String {
    draw(app, w, h).0
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
        "objective": objective, "tokensUsed": 0, "costUsd": 0.0
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

/// A conversation that mentions `pytest` twice (once said, once run) and a lot of other lines around it.
fn long_conversation() -> Vec<Envelope> {
    let mut e = start_events();
    e.push(said(10, "I will run pytest first."));
    e.push(call(11, "t0", "run_commands: pytest -q"));
    e.push(done(12, "t0"));
    for i in 0..40u32 {
        e.push(said(100 + 3 * i, &format!("Step {i:02} is done.")));
        e.push(call(
            101 + 3 * i,
            &format!("t{}", i + 1),
            &format!("tool number {i:02}"),
        ));
        e.push(done(102 + 3 * i, &format!("t{}", i + 1)));
    }
    e
}

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

fn ch(app: &mut App, c: char) -> Vec<Effect> {
    press(app, KeyCode::Char(c))
}

fn type_in(app: &mut App, text: &str) {
    for c in text.chars() {
        let effects = ch(app, c);
        assert!(effects.is_empty(), "{c:?} caused {effects:?}");
    }
}

/// The row of the search bar (`/query  1/2`), if one is drawn.
fn bar(text: &str) -> Option<&str> {
    text.lines()
        .find(|l| l.starts_with('/') && !l.starts_with("//"))
}

#[test]
fn slash_opens_the_bar_above_the_box_and_the_matches_are_counted() {
    let mut app = on_screen(long_conversation());
    assert!(bar(&screen(&app, 100, 30)).is_none());
    assert!(ch(&mut app, '/').is_empty());
    type_in(&mut app, "pytest");
    let text = screen(&app, 100, 30);
    let line = bar(&text).unwrap_or_else(|| panic!("no bar:\n{text}"));
    assert!(line.starts_with("/pytest"), "{line:?}");
    assert!(line.trim_end().ends_with("/2"), "{line:?}");
    // The bar sits between the conversation and the box.
    let at = text.lines().position(|l| l == line).unwrap();
    let boxed = text
        .lines()
        .position(|l| l.contains("Message to Cline"))
        .unwrap();
    assert!(at < boxed, "{text}");
    // The view jumped to a match: the first mention is far above the end of the conversation.
    assert!(text.contains("pytest"), "{text}");
}

#[test]
fn the_cursor_sits_after_the_query_while_it_is_typed() {
    let mut app = on_screen(long_conversation());
    ch(&mut app, '/');
    type_in(&mut app, "pyt");
    let (text, (x, y)) = draw(&app, 100, 30);
    let row = text.lines().position(|l| l.starts_with("/pyt")).unwrap();
    assert_eq!(usize::from(y), row, "{text}");
    assert_eq!(x, 4, "{text}");
}

#[test]
fn every_character_is_the_querys_while_the_bar_is_open_hotkeys_included() {
    let mut app = on_screen(long_conversation());
    ch(&mut app, '/');
    // `q` would quit, `n` would open the composer, `c` would start a cancel: here they are text.
    for c in ['q', 'n', 'c', 'a', 'f'] {
        assert!(ch(&mut app, c).is_empty(), "{c}");
    }
    assert_eq!(app.screen(), Screen::Dashboard);
    let text = screen(&app, 100, 30);
    let line = bar(&text).unwrap_or_else(|| panic!("no bar:\n{text}"));
    assert!(line.starts_with("/qncaf"), "{line:?}");
    assert!(line.contains("no matches"), "{line:?}");
}

#[test]
fn enter_accepts_and_n_and_shift_n_walk_the_matches() {
    let mut app = on_screen(long_conversation());
    ch(&mut app, '/');
    type_in(&mut app, "pytest");
    let first = bar(&screen(&app, 100, 30)).unwrap().trim_end().to_string();
    press(&mut app, KeyCode::Enter);
    // After Enter `n` is the next match, not a new run.
    assert!(ch(&mut app, 'n').is_empty());
    assert_eq!(app.screen(), Screen::Dashboard);
    let next = bar(&screen(&app, 100, 30)).unwrap().trim_end().to_string();
    assert_ne!(first, next);
    assert!(ch(&mut app, 'N').is_empty());
    // Stepping past an end wraps and says so: the bar reads `2/2` again, with `wrapped` after it.
    let back = bar(&screen(&app, 100, 30)).unwrap().trim_end().to_string();
    assert!(
        back.starts_with(&first),
        "{back:?} does not start with {first:?}"
    );
}

#[test]
fn esc_clears_an_accepted_search_and_n_is_a_new_run_again() {
    let mut app = on_screen(long_conversation());
    ch(&mut app, '/');
    type_in(&mut app, "pytest");
    press(&mut app, KeyCode::Enter);
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert_eq!(app.screen(), Screen::Dashboard, "Esc went to the Runs list");
    assert!(bar(&screen(&app, 100, 30)).is_none());
    ch(&mut app, 'n');
    assert_eq!(app.screen(), Screen::Composer);
}

#[test]
fn esc_while_typing_puts_the_view_back_at_the_end() {
    let mut app = on_screen(long_conversation());
    let before = screen(&app, 100, 30);
    assert!(before.contains("tool number 39"), "{before}");
    ch(&mut app, '/');
    type_in(&mut app, "pytest");
    let during = screen(&app, 100, 30);
    assert!(
        !during.contains("tool number 39"),
        "the view did not move:\n{during}"
    );
    press(&mut app, KeyCode::Esc);
    let after = screen(&app, 100, 30);
    assert!(after.contains("tool number 39"), "{after}");
    assert!(bar(&after).is_none(), "{after}");
}

#[test]
fn a_match_inside_a_folded_card_opens_it() {
    let mut app = on_screen(long_conversation());
    assert!(!screen(&app, 100, 30).contains("AGENT"));
    ch(&mut app, '/');
    // The agent row `direct  cline ...` is in the folded agents card.
    type_in(&mut app, "ENGINE");
    let text = screen(&app, 100, 30);
    assert!(text.contains("AGENT"), "the card did not open:\n{text}");
}

#[test]
fn new_events_update_the_matches() {
    let mut app = on_screen(long_conversation());
    ch(&mut app, '/');
    type_in(&mut app, "pytest");
    press(&mut app, KeyCode::Enter);
    let text = screen(&app, 100, 30);
    assert!(bar(&text).unwrap().trim_end().ends_with("/2"), "{text}");
    app.update(pages(vec![said(900, "pytest passes now.")]));
    let text = screen(&app, 100, 30);
    assert!(bar(&text).unwrap().trim_end().ends_with("/3"), "{text}");
}

#[test]
fn opening_another_run_drops_the_search() {
    let mut app = on_screen(long_conversation());
    ch(&mut app, '/');
    type_in(&mut app, "pytest");
    press(&mut app, KeyCode::Enter);
    app.open_run("01K6A0000000000000000000R2");
    assert!(bar(&screen(&app, 100, 30)).is_none());
}

#[test]
fn a_slash_in_typing_mode_is_text() {
    let mut app = on_screen(long_conversation());
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.mode(), Mode::Typing);
    ch(&mut app, '/');
    assert_eq!(app.chat_text().as_deref(), Some("/"));
    assert!(bar(&screen(&app, 100, 30)).is_none_or(|l| !l.contains("pytest")));
}

#[test]
fn the_footer_names_the_search_keys_while_it_is_open() {
    let mut app = on_screen(long_conversation());
    ch(&mut app, '/');
    type_in(&mut app, "pytest");
    let text = screen(&app, 100, 30);
    let footer = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
    assert!(
        footer.contains("Enter accept") && footer.contains("Esc cancel"),
        "{footer:?}"
    );
    press(&mut app, KeyCode::Enter);
    let text = screen(&app, 100, 30);
    let footer = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
    assert!(
        footer.contains("n next") && footer.contains("N previous"),
        "{footer:?}"
    );
}
