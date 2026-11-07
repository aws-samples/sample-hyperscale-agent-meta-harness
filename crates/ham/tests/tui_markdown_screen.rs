//! Markdown, code and diff rendering in the transcript. What Cline writes as a message is
//! Markdown and is drawn as styled rows: headings in bold, emphasis, code in its colour, lists with bullets, links as text with the address
//! in parentheses (never a hyperlink); the person's words, tool calls and notices are drawn as written; a diff block is coloured with the
//! changed words reversed. Escape sequences stay inert. Search marks and a selection work on the rows as drawn.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::keys::Mode;
use ham::tui::transcript::{Transcript, TranscriptState};
use ham::tui::{App, Msg};
use ham_core::chat::{Block, ChatModel, Origin, Role, Source};
use harness_protocol::api::v1::RunDetail;
use harness_protocol::envelope::{Envelope, EventType};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::StatefulWidget;
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde_json::json;
use std::time::{Duration, Instant};

fn message(n: u32, text: &str) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "message", "text": text}),
    )
}

fn browsing(extra: Vec<Envelope>) -> (App, Clock) {
    let (mut app, mut clock) = ready_with(extra);
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    assert_eq!(app.mode(), Mode::Browse);
    (app, clock)
}

/// The cell of `needle` on the buffer: the first match, as (x, y).
fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
    let text = text_of(buf);
    for (y, line) in text.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            return (line[..at].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} is not on the screen:\n{text}");
}

fn has(buf: &Buffer, needle: &str, m: Modifier) -> bool {
    let (x, y) = find(buf, needle);
    (0..needle.chars().count() as u16).all(|i| buf[(x + i, y)].modifier.contains(m))
}

const SOURCE: &str =
    "# Title\n\nSome **bold** and `code` here.\n\n- one\n- two\n\n[docs](https://x.test/a)";

#[test]
fn an_engine_message_is_drawn_as_markdown() {
    let (app, _) = browsing(vec![message(10, SOURCE)]);
    let buf = buffer(&app, 120, 40);
    let text = text_of(&buf);
    for want in [
        "Title",
        "Some bold and code here.",
        "\u{2022} one",
        "\u{2022} two",
        "docs (https://x.test/a)",
    ] {
        assert!(text.contains(want), "{want:?} missing:\n{text}");
    }
    for gone in ["# Title", "**", "`code`", "[docs]"] {
        assert!(
            !text.contains(gone),
            "{gone:?} should be rendered away:\n{text}"
        );
    }
    assert!(has(&buf, "Title", Modifier::BOLD));
    assert!(has(&buf, "bold", Modifier::BOLD));
    assert!(!has(&buf, "Some", Modifier::BOLD));
    let (x, y) = find(&buf, "code here");
    assert_eq!(buf[(x, y)].fg, Color::Yellow, "code has its colour");
}

#[test]
fn the_persons_words_and_the_tool_calls_are_drawn_as_written() {
    let (mut app, _) = browsing(vec![env(
        11,
        EventType::ToolSummary,
        Some("direct"),
        json!({"toolCallId": "t1", "title": "run_commands: echo **x**", "kind": "execute", "status": "completed"}),
    )]);
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a **todo** API", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(d)));
    let text = screen(&app);
    assert!(text.contains("Build a **todo** API"), "{text}");
    assert!(text.contains("echo **x**"), "{text}");
}

#[test]
fn a_link_is_text_and_no_hyperlink_sequence_reaches_the_terminal() {
    let (app, _) = browsing(vec![message(10, SOURCE)]);
    let sink = Sink::default();
    let mut t = Terminal::with_options(
        CrosstermBackend::new(sink.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 120, 40)),
        },
    )
    .unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let sent = String::from_utf8_lossy(&sink.0.lock().unwrap()).to_string();
    assert!(sent.contains("docs"), "TRIGGER: the link text was drawn");
    assert!(
        !sent.contains("\u{1b}]8"),
        "an OSC 8 hyperlink was written: {sent:?}"
    );
}

/// A writer the test can read back.
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

#[test]
fn escape_sequences_in_a_markdown_message_stay_inert() {
    let (app, _) = browsing(vec![message(
        10,
        "**before** \u{1b}]52;c;aGVsbG8=\u{7} \u{1b}[31mafter\u{1b}[0m",
    )]);
    let buf = buffer(&app, 120, 40);
    let text = text_of(&buf);
    assert!(text.contains("before") && text.contains("after"), "{text}");
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
    assert!(has(&buf, "before", Modifier::BOLD));
}

#[test]
fn a_search_mark_lands_on_the_word_as_it_is_drawn() {
    let (mut app, mut clock) = browsing(vec![message(10, "Some **bold** words and `bold` code")]);
    press(&mut app, &mut clock, KeyCode::Char('/'), 200);
    for c in "bold".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 80);
    }
    press(&mut app, &mut clock, KeyCode::Enter, 200);
    let buf = buffer(&app, 120, 40);
    // Every cell that carries a search mark reads "bold".
    let mut marked = String::new();
    for y in 0..buf.area.height {
        for x in 9..buf.area.width {
            if matches!(buf[(x, y)].bg, Color::Yellow | Color::LightRed) {
                marked.push_str(buf[(x, y)].symbol());
            }
        }
    }
    assert_eq!(
        marked,
        "boldbold",
        "the marks are on the matches:\n{}",
        text_of(&buf)
    );
}

#[test]
fn a_selection_copies_the_text_as_drawn() {
    let (mut app, _) = browsing(vec![message(10, SOURCE)]);
    let buf = buffer(&app, 120, 40);
    let (x0, y) = find(&buf, "Title");
    let base = Instant::now();
    let mut ms = 0u64;
    let mut mouse = |app: &mut App, kind, x: u16| {
        ms += 30;
        app.handle_mouse_at(
            MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            base + Duration::from_millis(ms),
        )
    };
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x0);
    mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), x0 + 4);
    let effects = mouse(&mut app, MouseEventKind::Up(MouseButton::Left), x0 + 4);
    let copies: Vec<String> = effects
        .iter()
        .filter_map(|e| match e {
            ham::tui::Effect::Copy(p) => Some(p.text().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(copies, vec!["Title".to_string()], "{effects:?}");
}

// ---- diffs, through the widget ----------------------------------------------------------------------------------------------------

#[test]
fn a_diff_block_is_coloured_and_the_changed_word_is_reversed() {
    let diff = "--- a/src/a.py\n+++ b/src/a.py\n@@ -1,3 +1,3 @@\n context line\n-old line here\n+new line here\n";
    let m = ChatModel::from_blocks(
        Source::FullHistory,
        "cline",
        vec![Block::diff("d1", "src/a.py", diff)],
    );
    let mut st = TranscriptState::new();
    st.toggle_fold(&m, "d1");
    let area = Rect::new(0, 0, 80, 14);
    let mut buf = Buffer::empty(area);
    Transcript::new(&m).render(area, &mut buf, &mut st);
    let text = text_of(&buf);
    assert!(
        text.contains("new line here") && text.contains("old line here"),
        "{text}"
    );
    let (x, y) = find(&buf, "+new line here");
    assert_eq!(buf[(x, y)].fg, Color::Green, "{text}");
    let (x, y) = find(&buf, "-old line here");
    assert_eq!(buf[(x, y)].fg, Color::Red, "{text}");
    // Only the word that changed is reversed.
    let (nx, ny) = find(&buf, "new line");
    assert!(
        buf[(nx, ny)].modifier.contains(Modifier::REVERSED),
        "{text}"
    );
    let (lx, ly) = find(&buf, "line here");
    assert!(
        !buf[(lx, ly)].modifier.contains(Modifier::REVERSED),
        "{text}"
    );
}

#[test]
fn text_that_is_not_a_diff_in_a_diff_block_is_shown_as_it_is() {
    let m = ChatModel::from_blocks(
        Source::FullHistory,
        "cline",
        vec![Block::diff("d1", "notes.txt", "just some words\nand more")],
    );
    let mut st = TranscriptState::new();
    st.toggle_fold(&m, "d1");
    let area = Rect::new(0, 0, 80, 10);
    let mut buf = Buffer::empty(area);
    Transcript::new(&m).render(area, &mut buf, &mut st);
    let text = text_of(&buf);
    assert!(
        text.contains("just some words") && text.contains("and more"),
        "{text}"
    );
}

#[test]
fn a_thought_and_a_notice_are_not_markdown() {
    let m = ChatModel::from_blocks(
        Source::FullHistory,
        "cline",
        vec![
            Block::message("a", Role::Engine, Origin::Engine, "plain **engine** text"),
            Block::thought("t", "I think **so**"),
        ],
    );
    let mut st = TranscriptState::new();
    let area = Rect::new(0, 0, 80, 10);
    let mut buf = Buffer::empty(area);
    Transcript::new(&m).render(area, &mut buf, &mut st);
    st.toggle_fold(&m, "t");
    let mut buf = Buffer::empty(area);
    Transcript::new(&m).render(area, &mut buf, &mut st);
    let text = text_of(&buf);
    assert!(
        text.contains("plain engine text"),
        "the message is Markdown:\n{text}"
    );
    assert!(
        text.contains("I think **so**"),
        "the thought is as written:\n{text}"
    );
}
