//! Drag to select in the transcript, copy on release. A press in the
//! conversation and a drag select the text under the pointer, drawn reversed; the release copies exactly the selected text (one
//! `Effect::Copy`): a wrapped paragraph comes back as it was written (a soft wrap at a space is a space, a word cut at the width joins
//! with nothing, a new line stays one), rows beyond the screen are reached by dragging past the pane edge, and a new press, a resize or
//! a changed row drops the selection. Escape sequences in engine text are inert: nothing is written to the terminal from the transcript.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Effect, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

fn buffer(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

fn text_of(buf: &Buffer) -> String {
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

/// The screen at `w` x `h`, drawn (so the hit map is the one of this frame).
fn screen(app: &App, w: u16, h: u16) -> String {
    text_of(&buffer(app, w, h))
}

/// The column and row where `needle` starts.
fn pos_of(text: &str, needle: &str) -> (u16, u16) {
    for (y, line) in text.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            return (line[..at].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} is not on the screen:\n{text}");
}

/// The cells of the buffer drawn reversed, as `(x, y)`, outside the footer line and the role gutter.
fn reversed_cells(buf: &Buffer) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for y in 0..buf.area.height {
        for x in 9..buf.area.width {
            if buf[(x, y)].modifier.contains(Modifier::REVERSED) {
                out.push((x, y));
            }
        }
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

fn tool(n: u32, title: &str) -> Envelope {
    env(
        n,
        EventType::ToolSummary,
        Some("direct"),
        json!({"toolCallId": format!("t{n}"), "title": title, "kind": "execute", "status": "completed"}),
    )
}

fn on_screen(extra: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(1_792_000_000);
    app.open_run(RUN);
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
    events.extend(extra);
    app.update(pages(events));
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

/// A writer the test can read back: what the terminal backend would send.
#[derive(Clone, Default)]
struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Pointer {
    base: Instant,
    ms: u64,
}

impl Pointer {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            ms: 0,
        }
    }

    fn event(&mut self, app: &mut App, kind: MouseEventKind, x: u16, y: u16) -> Vec<Effect> {
        self.ms += 30;
        app.handle_mouse_at(
            MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            },
            self.base + Duration::from_millis(self.ms),
        )
    }

    fn press(&mut self, app: &mut App, x: u16, y: u16) -> Vec<Effect> {
        self.event(app, MouseEventKind::Down(MouseButton::Left), x, y)
    }

    fn drag(&mut self, app: &mut App, x: u16, y: u16) -> Vec<Effect> {
        self.event(app, MouseEventKind::Drag(MouseButton::Left), x, y)
    }

    fn release(&mut self, app: &mut App, x: u16, y: u16) -> Vec<Effect> {
        self.event(app, MouseEventKind::Up(MouseButton::Left), x, y)
    }

    /// Press at `from`, drag to `to`, release there; the effects of the release and the copies of the whole gesture.
    fn select(&mut self, app: &mut App, from: (u16, u16), to: (u16, u16)) -> Vec<String> {
        let mut effects = self.press(app, from.0, from.1);
        effects.extend(self.drag(app, to.0, to.1));
        effects.extend(self.release(app, to.0, to.1));
        copies(&effects)
    }
}

fn copies(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Copy(p) => Some(p.text().to_string()),
            _ => None,
        })
        .collect()
}

// ---- one row ----------------------------------------------------------------------------------------------------------------------

#[test]
fn a_drag_over_a_word_copies_exactly_those_cells_and_draws_them_reversed() {
    let mut app = on_screen(vec![message(10, "Second answer with `code` in it.")]);
    let text = screen(&app, 120, 40);
    let (x0, y) = pos_of(&text, "Second answer");
    let mut p = Pointer::new();
    let got = p.select(&mut app, (x0, y), (x0 + 12, y));
    assert_eq!(got, vec!["Second answer".to_string()]);
    // The selected cells are drawn reversed and no others in the conversation.
    let buf = buffer(&app, 120, 40);
    let reversed = reversed_cells(&buf);
    let want: Vec<(u16, u16)> = (x0..=x0 + 12).map(|x| (x, y)).collect();
    assert_eq!(reversed, want);
}

#[test]
fn a_drag_to_the_left_selects_the_same_text() {
    let mut app = on_screen(vec![message(10, "Second answer with `code` in it.")]);
    let text = screen(&app, 120, 40);
    let (x0, y) = pos_of(&text, "Second answer");
    let mut p = Pointer::new();
    let got = p.select(&mut app, (x0 + 12, y), (x0, y));
    assert_eq!(got, vec!["Second answer".to_string()]);
}

#[test]
fn a_click_without_a_drag_selects_and_copies_nothing() {
    let mut app = on_screen(vec![message(10, "Second answer with `code` in it.")]);
    let text = screen(&app, 120, 40);
    let (x0, y) = pos_of(&text, "Second answer");
    let mut p = Pointer::new();
    let mut effects = p.press(&mut app, x0 + 2, y);
    effects.extend(p.release(&mut app, x0 + 2, y));
    assert!(copies(&effects).is_empty(), "{effects:?}");
    assert!(reversed_cells(&buffer(&app, 120, 40)).is_empty());
}

// ---- rows that continue each other --------------------------------------------------------------------------------------------------

const PARAGRAPH: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango";

#[test]
fn a_paragraph_wrapped_at_spaces_copies_as_it_was_written() {
    let mut app = on_screen(vec![message(10, PARAGRAPH)]);
    let text = screen(&app, 40, 30);
    let (x0, y0) = pos_of(&text, "alpha");
    let (xt, yt) = pos_of(&text, "tango");
    assert!(
        yt > y0 + 1,
        "TRIGGER: the paragraph is wrapped over several rows:\n{text}"
    );
    let mut p = Pointer::new();
    let got = p.select(&mut app, (x0, y0), (xt + 4, yt));
    assert_eq!(got, vec![PARAGRAPH.to_string()]);
}

const LONG_PATH: &str =
    "src/module_one/handlers/request_processing/validators/schema_checks/helpers_and_more.py";

#[test]
fn a_word_cut_at_the_width_copies_without_a_space() {
    let mut app = on_screen(vec![message(10, LONG_PATH)]);
    let text = screen(&app, 40, 30);
    let (x0, y0) = pos_of(&text, "src/module_one");
    let rows: Vec<&str> = text.lines().collect();
    let last = rows
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| l.contains(".py"))
        .map(|(y, l)| (l.find(".py").unwrap() as u16 + 2, y as u16))
        .unwrap();
    assert!(
        last.1 > y0,
        "TRIGGER: the path is cut over several rows:\n{text}"
    );
    let mut p = Pointer::new();
    let got = p.select(&mut app, (x0, y0), last);
    assert_eq!(got, vec![LONG_PATH.to_string()]);
}

#[test]
fn rows_of_different_blocks_are_joined_by_a_new_line() {
    let mut app = on_screen(vec![
        message(10, "First answer from Cline."),
        tool(11, "run_commands: ls"),
        message(12, "Second answer with `code` in it."),
    ]);
    let text = screen(&app, 120, 40);
    let (x0, y0) = pos_of(&text, "First answer");
    let (x1, y1) = pos_of(&text, "Second answer");
    let mut p = Pointer::new();
    let got = p.select(&mut app, (x0, y0), (x1 + 5, y1));
    assert_eq!(got.len(), 1);
    assert!(
        got[0].starts_with("First answer from Cline.\n") && got[0].ends_with("\nSecond"),
        "{:?}",
        got[0]
    );
    assert!(got[0].contains("run_commands: ls"), "{:?}", got[0]);
}

// ---- reaching rows off the screen -----------------------------------------------------------------------------------------------------

fn many_steps(n: u32) -> Vec<Envelope> {
    (0..n)
        .map(|i| tool(100 + i, &format!("step {i:03}")))
        .collect()
}

#[test]
fn dragging_past_the_top_edge_scrolls_and_the_copy_reaches_rows_that_were_off_screen() {
    let mut app = on_screen(many_steps(120));
    let text = screen(&app, 100, 30);
    let (x_last, y_last) = pos_of(&text, "step 119");
    let (_, y_first) = pos_of(&text, "step ");
    let pane_rows = usize::from(y_last - y_first) + 1;
    let mut p = Pointer::new();
    // The press is on the last character of the last row.
    p.press(&mut app, x_last + 7, y_last);
    // The pointer goes above the pane and stays there: each report scrolls the conversation.
    let mut effects = Vec::new();
    for _ in 0..4 {
        effects.extend(p.drag(&mut app, x_last, 0));
    }
    let _ = screen(&app, 100, 30);
    effects.extend(p.release(&mut app, x_last, 0));
    let got = copies(&effects);
    assert_eq!(got.len(), 1, "{effects:?}");
    let steps = got[0].lines().filter(|l| l.starts_with("step ")).count();
    assert!(
        steps > pane_rows,
        "the copy holds {steps} rows, more than the {pane_rows} rows of the pane:\n{}",
        got[0]
    );
    assert!(got[0].ends_with("step 119"), "{:?}", got[0]);
}

// ---- dropping the selection ---------------------------------------------------------------------------------------------------------

#[test]
fn a_new_press_drops_the_selection() {
    let mut app = on_screen(vec![message(10, "Second answer with `code` in it.")]);
    let text = screen(&app, 120, 40);
    let (x0, y) = pos_of(&text, "Second answer");
    let mut p = Pointer::new();
    p.select(&mut app, (x0, y), (x0 + 12, y));
    assert!(!reversed_cells(&buffer(&app, 120, 40)).is_empty());
    p.press(&mut app, x0 + 20, y);
    p.release(&mut app, x0 + 20, y);
    assert!(reversed_cells(&buffer(&app, 120, 40)).is_empty());
}

#[test]
fn a_resize_that_changes_the_rows_drops_the_selection() {
    let mut app = on_screen(vec![message(10, PARAGRAPH)]);
    let text = screen(&app, 120, 40);
    let (x0, y) = pos_of(&text, "alpha");
    let mut p = Pointer::new();
    p.select(&mut app, (x0, y), (x0 + 20, y));
    assert!(!reversed_cells(&buffer(&app, 120, 40)).is_empty());
    // A narrower window lays the paragraph out again: the rows under the selection are not the same.
    app.resized();
    let narrow = buffer(&app, 40, 30);
    assert!(reversed_cells(&narrow).is_empty(), "{}", text_of(&narrow));
}

#[test]
fn a_row_that_changes_under_the_selection_drops_it() {
    let mut app = on_screen(vec![message(10, "Second answer with `code` in it.")]);
    let text = screen(&app, 120, 40);
    let (x0, y) = pos_of(&text, "Second answer");
    let mut p = Pointer::new();
    p.select(&mut app, (x0, y), (x0 + 12, y));
    assert!(!reversed_cells(&buffer(&app, 120, 40)).is_empty());
    // Cline goes on writing the same message: its row is not the one that was selected.
    app.update(pages(vec![message(11, " More words follow.")]));
    let after = buffer(&app, 120, 40);
    assert!(reversed_cells(&after).is_empty(), "{}", text_of(&after));
}

#[test]
fn rows_added_at_the_end_keep_the_selection() {
    let mut app = on_screen(vec![message(10, "Second answer with `code` in it.")]);
    let text = screen(&app, 120, 40);
    let (x0, y) = pos_of(&text, "Second answer");
    let mut p = Pointer::new();
    p.select(&mut app, (x0, y), (x0 + 12, y));
    app.update(pages(vec![tool(11, "run_commands: ls")]));
    let after = buffer(&app, 120, 40);
    assert!(!reversed_cells(&after).is_empty(), "{}", text_of(&after));
}

// ---- escape sequences stay inert -----------------------------------------------------------------------------------------------------------

#[test]
fn an_osc_52_sequence_in_engine_text_is_shown_inert_and_nothing_is_written() {
    let hostile = "before \u{1b}]52;c;aGVsbG8=\u{7} after";
    let mut app = on_screen(vec![message(10, hostile)]);
    // Drawn through the real terminal backend, the bytes that reach the terminal hold no OSC 52 sequence.
    let sink = Sink::default();
    let backend = CrosstermBackend::new(sink.clone());
    let mut t = Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 100, 30)),
        },
    )
    .unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let sent = String::from_utf8_lossy(&sink.0.lock().unwrap()).to_string();
    assert!(sent.contains("before"), "TRIGGER: the message was drawn");
    assert!(
        !sent.contains("\u{1b}]52"),
        "OSC 52 reached the terminal: {sent:?}"
    );
    // The text on screen has no escape and no payload sequence.
    let text = screen(&app, 100, 30);
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
    // Selecting it copies clean text.
    let (x0, y) = pos_of(&text, "before");
    let mut p = Pointer::new();
    let got = p.select(&mut app, (x0, y), (x0 + 30, y));
    assert_eq!(got.len(), 1);
    assert!(
        !got[0].contains('\u{1b}') && !got[0].contains('\u{7}'),
        "{:?}",
        got[0]
    );
    assert!(got[0].starts_with("before"), "{:?}", got[0]);
}
