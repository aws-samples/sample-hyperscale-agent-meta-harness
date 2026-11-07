//! `/copy`, `y` and `/export` on the run screen. `/copy` and `y` copy
//! the last answer of Cline, or the selected block; a copy is one effect that carries the cleaned, masked text, and the result comes back
//! as a notice that names the path used. `/export` writes the conversation (not the cards) as Markdown: the screen builds the text
//! from the model, the effect writes the file in the download directory (or the place the person typed), and a pasted line never writes a file.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::copy::{self, Os, Prepared, Route, Runner};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Effect, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

struct Clock {
    base: Instant,
    ms: u64,
}

impl Clock {
    fn after(&mut self, ms: u64) -> Instant {
        self.ms += ms;
        self.base + Duration::from_millis(self.ms)
    }
}

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

fn message(n: u32, text: &str) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "message", "text": text}),
    )
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

fn detail() -> Msg {
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

/// The run on screen in browse mode with `extra` events after the start.
fn on_screen(extra: Vec<Envelope>) -> (App, Clock) {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(1_792_000_000);
    app.open_run(RUN);
    let mut events = start();
    events.extend(extra);
    app.update(pages(events));
    app.update(detail());
    assert_eq!(app.mode(), Mode::Browse);
    (
        app,
        Clock {
            base: Instant::now(),
            ms: 0,
        },
    )
}

fn press(app: &mut App, clock: &mut Clock, code: KeyCode, wait_ms: u64) -> Vec<Effect> {
    let at = clock.after(wait_ms);
    app.handle_key_at(KeyEvent::new(code, KeyModifiers::NONE), at)
}

fn type_line(app: &mut App, clock: &mut Clock, text: &str) -> Vec<Effect> {
    press(app, clock, KeyCode::Char('i'), 50);
    assert_eq!(app.mode(), Mode::Typing);
    clock.after(5_000);
    for c in text.chars() {
        let effects = press(app, clock, KeyCode::Char(c), 50);
        assert!(effects.is_empty(), "typing {c:?} caused {effects:?}");
    }
    clock.after(300);
    press(app, clock, KeyCode::Enter, 10)
}

fn copies(effects: &[Effect]) -> Vec<&Prepared> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Copy(p) => Some(p),
            _ => None,
        })
        .collect()
}

fn exports(effects: &[Effect]) -> Vec<&Effect> {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Export { .. }))
        .collect()
}

/// Two answers with a tool call between them, so that they are two blocks (message chunks that follow each other make one block).
fn two_answers() -> Vec<Envelope> {
    vec![
        message(10, "First answer from Cline."),
        env(
            11,
            EventType::ToolSummary,
            Some("direct"),
            json!({"toolCallId": "t1", "title": "run_commands: ls", "kind": "execute", "status": "completed"}),
        ),
        message(12, "Second answer with `code` in it."),
    ]
}

// ---- /copy and y ------------------------------------------------------------------------------------------------------------------

#[test]
fn slash_copy_copies_the_last_answer_of_cline() {
    let (mut app, mut clock) = on_screen(two_answers());
    let effects = type_line(&mut app, &mut clock, "/copy");
    let c = copies(&effects);
    assert_eq!(c.len(), 1, "{effects:?}");
    assert_eq!(c[0].text(), "Second answer with `code` in it.");
    assert!(!c[0].masked());
}

#[test]
fn slash_copy_before_any_answer_says_so_and_copies_nothing() {
    let (mut app, mut clock) = on_screen(vec![]);
    let effects = type_line(&mut app, &mut clock, "/copy");
    assert!(copies(&effects).is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(text.contains("Nothing to copy."), "{text}");
}

#[test]
fn y_copies_the_last_answer_when_no_block_is_selected_and_the_selected_block_when_one_is() {
    let (mut app, mut clock) = on_screen(two_answers());
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 400);
    let c = copies(&effects);
    assert_eq!(c.len(), 1, "{effects:?}");
    assert_eq!(c[0].text(), "Second answer with `code` in it.");
    // Tab selects the first block of the conversation (the prompt), a second Tab the next one.
    press(&mut app, &mut clock, KeyCode::Tab, 400);
    press(&mut app, &mut clock, KeyCode::Tab, 400);
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 400);
    let c = copies(&effects);
    assert_eq!(c.len(), 1, "{effects:?}");
    assert_eq!(c[0].text(), "First answer from Cline.", "{effects:?}");
}

#[test]
fn y_in_the_message_box_is_a_letter_and_copies_nothing() {
    let (mut app, mut clock) = on_screen(two_answers());
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    clock.after(5_000);
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 50);
    assert!(copies(&effects).is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("y"));
}

#[test]
fn a_secret_shaped_value_never_reaches_the_clipboard() {
    // Built at run time: the shape of a GitHub token, not a real one. The conversation hides it already when it builds the block, and the
    // copy hides what is left (a second line): the text handed to the clipboard has no token in it either way.
    let key = format!("ghp_{}", "A1b2".repeat(9));
    let (mut app, mut clock) = on_screen(vec![message(10, &format!("The key is {key} for now."))]);
    assert!(
        !screen(&app).contains(&key),
        "TRIGGER: the token is not drawn"
    );
    let effects = press(&mut app, &mut clock, KeyCode::Char('y'), 400);
    let c = copies(&effects);
    assert_eq!(c.len(), 1, "{effects:?}");
    assert!(!c[0].text().contains(&key), "{}", c[0].text());
    assert!(c[0].text().starts_with("The key is "), "{}", c[0].text());
}

#[test]
fn the_result_of_a_copy_shows_in_the_notice_row_with_the_path_used() {
    let (mut app, _) = on_screen(two_answers());
    app.update(Msg::Copied(Ok("Copied 32 bytes with pbcopy.".into())));
    assert!(screen(&app).contains("Copied 32 bytes with pbcopy."));
    app.update(Msg::Copied(Err("No clipboard route took the text.".into())));
    assert!(screen(&app).contains("No clipboard route took the text."));
}

// ---- running a copy ---------------------------------------------------------------------------------------------------------------

/// The route a Mac with `pbcopy` installed gets.
fn pbcopy_routes() -> Vec<Route> {
    let routes = copy::plan(Os::MacOs, &|_| None, &|p| p == "pbcopy");
    assert_eq!(routes.len(), 2, "{routes:?}");
    routes[..1].to_vec()
}

struct Fake {
    ok: bool,
    calls: Vec<(Vec<String>, Vec<u8>)>,
}

impl Runner for Fake {
    fn run(&mut self, argv: &[&str], input: &[u8]) -> std::io::Result<bool> {
        self.calls.push((
            argv.iter().map(|s| (*s).to_string()).collect(),
            input.to_vec(),
        ));
        Ok(self.ok)
    }
}

#[test]
fn run_copy_hands_the_prepared_text_to_the_route_and_returns_the_notice() {
    let p = copy::prepare("hello there").unwrap();
    let routes = pbcopy_routes();
    let mut runner = Fake {
        ok: true,
        calls: vec![],
    };
    let mut term = Vec::new();
    let notice = ham::tui::run_copy(&p, &routes, &mut runner, &mut term).unwrap();
    assert!(
        notice.starts_with("Copied 11 bytes with pbcopy"),
        "{notice}"
    );
    assert_eq!(runner.calls.len(), 1);
    assert_eq!(runner.calls[0].1, b"hello there");
    assert!(
        term.is_empty(),
        "a tool route writes nothing to the terminal"
    );
}

#[test]
fn run_copy_writes_osc_52_only_on_the_osc_52_route_and_the_payload_is_the_text() {
    let p = copy::prepare("select me").unwrap();
    let mut runner = Fake {
        ok: false,
        calls: vec![],
    };
    let mut term = Vec::new();
    let notice = ham::tui::run_copy(&p, &[Route::Osc52], &mut runner, &mut term).unwrap();
    assert!(notice.contains("OSC 52"), "{notice}");
    assert!(runner.calls.is_empty());
    let sent = String::from_utf8(term).unwrap();
    assert_eq!(sent, String::from_utf8(copy::osc52(&p)).unwrap());
    assert!(sent.starts_with("\x1b]52;c;"));
    assert!(sent.contains(&copy::base64(b"select me")));
}

#[test]
fn run_copy_with_no_route_returns_the_words_of_the_error() {
    let p = copy::prepare("x y").unwrap();
    let mut runner = Fake {
        ok: false,
        calls: vec![],
    };
    let mut term = Vec::new();
    let e = ham::tui::run_copy(&p, &pbcopy_routes(), &mut runner, &mut term).unwrap_err();
    assert_eq!(e, "No clipboard route took the text.");
}

// ---- /export ----------------------------------------------------------------------------------------------------------------------

fn only_export(effects: &[Effect]) -> (String, Option<String>, usize, String) {
    let e = exports(effects);
    assert_eq!(e.len(), 1, "{effects:?}");
    match e[0] {
        Effect::Export {
            run_id,
            arg,
            stamp,
            content,
            blocks,
        } => {
            assert_eq!(run_id, RUN);
            (content.clone(), arg.clone(), *blocks, stamp.clone())
        }
        _ => unreachable!(),
    }
}

#[test]
fn slash_export_builds_the_markdown_of_the_conversation_in_order_without_the_cards() {
    let (mut app, mut clock) = on_screen(two_answers());
    let effects = type_line(&mut app, &mut clock, "/export");
    let (content, arg, blocks, stamp) = only_export(&effects);
    assert_eq!(arg, None);
    assert!(blocks >= 3, "the prompt and both answers: {blocks}");
    assert!(stamp.starts_with("2026") && stamp.ends_with('Z'), "{stamp}");
    // The order of the blocks is the order of the conversation.
    let p = content.find("Build a todo API with tests").unwrap();
    let a = content.find("First answer from Cline.").unwrap();
    let b = content.find("Second answer with `code` in it.").unwrap();
    assert!(p < a && a < b, "{content}");
    // The cards of the run screen are not part of the conversation.
    assert!(!content.contains("Model  "), "{content}");
    assert!(content.contains(RUN), "{content}");
}

#[test]
fn slash_export_with_a_file_name_passes_it_on() {
    let (mut app, mut clock) = on_screen(two_answers());
    let effects = type_line(&mut app, &mut clock, "/export notes.md");
    let (_, arg, _, _) = only_export(&effects);
    assert_eq!(arg.as_deref(), Some("notes.md"));
}

#[test]
fn a_pasted_export_line_never_writes_a_file() {
    let (mut app, mut clock) = on_screen(two_answers());
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    app.handle_paste("/export notes.md".to_string());
    clock.after(5_000);
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 10);
    assert!(exports(&effects).is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(text.contains("does not write files"), "{text}");
}

#[test]
fn the_result_of_an_export_shows_in_the_notice_row() {
    let (mut app, _) = on_screen(two_answers());
    app.update(Msg::Exported(Ok(
        "Exported 3 blocks (1.2 KiB) to /tmp/x.md.".into(),
    )));
    assert!(screen(&app).contains("Exported 3 blocks (1.2 KiB) to /tmp/x.md."));
    app.update(Msg::Exported(Err(
        "/tmp/x.md already exists; nothing was written".into(),
    )));
    assert!(screen(&app).contains("already exists; nothing was written"));
}

#[test]
fn run_export_writes_the_content_unchanged_in_the_download_directory_and_never_replaces() {
    let dir = std::env::temp_dir().join(format!("ham-export-screen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let content = "# chat\n\n> hello\n";
    let first = ham::tui::run_export(&dir, None, RUN, "20261016T213000Z", content, 1).unwrap();
    assert!(first.contains("Exported 1 block"), "{first}");
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), content);
    // A second export in the same second gets its own name.
    ham::tui::run_export(&dir, None, RUN, "20261016T213000Z", content, 1).unwrap();
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    // A name the person typed that is taken is an error and nothing changes.
    let name = dir.join("mine.md");
    ham::tui::run_export(&dir, Some(name.to_str().unwrap()), RUN, "s", content, 1).unwrap();
    let e =
        ham::tui::run_export(&dir, Some(name.to_str().unwrap()), RUN, "s", "other", 1).unwrap_err();
    assert!(e.contains("already exists"), "{e}");
    assert_eq!(std::fs::read_to_string(&name).unwrap(), content);
}

#[test]
fn run_export_creates_the_default_directory_when_it_is_missing() {
    let base =
        std::env::temp_dir().join(format!("ham-export-screen-missing-{}", std::process::id()));
    let dir = base.join("downloads");
    let msg = ham::tui::run_export(&dir, None, RUN, "20261016T213000Z", "# x\n", 1).unwrap();
    assert!(msg.contains("Exported"), "{msg}");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}
