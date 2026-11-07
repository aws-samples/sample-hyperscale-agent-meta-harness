//! The name row of the run screen: one row directly under the header that shows the run's name, dim while it is a seed and plain
//! once a model has written it. The header line itself is untouched (the live drivers read it). The row is taken last, after the pending
//! rows, so a short screen drops it first and the conversation keeps its three rows; it is absent without a name and below 20 columns.
//! A run started from the composer shows its seed at once, and a name that a read brings in replaces the seed on the screen.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::{RUN, finished_events, screen};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::chat::{ChatFacts, ChatView, Header, InputPane, areas, areas_named};
use ham::tui::{App, DirectSettings, Msg};
use ham_core::runname::seed_name;
use harness_protocol::api::v1::StartRunResponse;
use proptest::prelude::*;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::{Block as UiBlock, Paragraph};
use serde_json::json;

const PROMPT: &str = "Add a percent_change function to the calculator with tests";

struct Box1;

impl InputPane for Box1 {
    fn rows_at(&self, _inner_width: usize) -> usize {
        1
    }

    fn render(&self, f: &mut Frame, area: Rect, block: UiBlock<'_>, _focused: bool) {
        f.render_widget(Paragraph::new("").block(block), area);
    }
}

fn header() -> Header {
    Header {
        run_id: "01M5287J4KQ8N3X7C9D2E6F1AB".into(),
        model: Some("Claude Sonnet 5.5".into()),
        state: "active".into(),
        ..Header::default()
    }
}

fn buffer_of(name: Option<(&str, bool)>, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let head = header();
    let mut facts = ChatFacts::new(&head, "footer");
    facts.name = name;
    let view = ChatView::new();
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| {
        view.draw(f, f.area(), &facts, &Box1);
    })
    .unwrap();
    t.backend().buffer().clone()
}

fn line(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    let mut out = String::new();
    let mut x = 0;
    while x < buf.area.width {
        let sym = buf[(x, y)].symbol();
        out.push_str(sym);
        x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
    }
    out.trim_end().to_string()
}

#[test]
fn the_name_row_sits_under_the_header_and_above_the_banner() {
    let a = areas_named(Rect::new(0, 0, 100, 30), true, 2, 3, true, 0, true);
    assert_eq!(a.header, Rect::new(0, 0, 100, 1));
    assert_eq!(a.name, Rect::new(0, 1, 100, 1));
    assert_eq!(a.banner, Rect::new(0, 2, 100, 1));
    assert_eq!(a.transcript.y, 3);
    assert_eq!(a.footer, Rect::new(0, 29, 100, 1));
    // Without a banner the transcript starts right under the name.
    let b = areas_named(Rect::new(0, 0, 80, 24), false, 0, 1, false, 0, true);
    assert_eq!(b.name, Rect::new(0, 1, 80, 1));
    assert_eq!(b.banner.height, 0);
    assert_eq!(b.transcript.y, 2);
}

#[test]
fn without_a_name_the_layout_is_exactly_the_old_one() {
    for (area, banner, pending, rows, notice) in [
        (Rect::new(0, 0, 80, 24), false, 0, 1, false),
        (Rect::new(0, 0, 100, 30), true, 2, 3, true),
        (Rect::new(0, 0, 60, 8), true, 3, 4, true),
    ] {
        let old = areas(area, banner, pending, rows, notice);
        assert_eq!(old.name.height, 0, "{old:?}");
        assert_eq!(
            areas_named(area, banner, pending, rows, notice, 0, false),
            old
        );
    }
}

#[test]
fn a_short_screen_drops_the_name_row_first_and_the_conversation_keeps_its_three_rows() {
    // 8 rows: the footer, the header and the three conversation rows leave three for a box that wants four.
    let a = areas_named(Rect::new(0, 0, 60, 8), true, 3, 4, true, 0, true);
    assert_eq!(a.name.height, 0, "{a:?}");
    assert!(a.transcript.height >= 3, "{a:?}");
    // The same screen without the name row gives every other part exactly what it had.
    let b = areas_named(Rect::new(0, 0, 60, 8), true, 3, 4, true, 0, false);
    assert_eq!(
        (a.input, a.notice, a.banner, a.pending),
        (b.input, b.notice, b.banner, b.pending)
    );
    // A tall box on a 24-row terminal: the box keeps a third of the screen, and the name row comes after the pending rows.
    let tall = areas_named(Rect::new(0, 0, 80, 24), false, 0, 40, false, 0, true);
    assert_eq!(tall.name.height, 1);
    assert!(
        tall.transcript.height >= 3 && tall.input.height <= 24 / 3 + 2,
        "{tall:?}"
    );
}

proptest! {
    // A failing run (a mutation run makes it fail on purpose) leaves no regression file in the repository.
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_areas_with_a_name_tile_the_screen_and_never_take_the_conversations_rows(
        w in 1u16..220, h in 1u16..80, banner in any::<bool>(), pending in 0u16..8, rows in 0u16..40, notice in any::<bool>(), reserved in 0u16..6
    ) {
        let whole = Rect::new(0, 0, w, h);
        let a = areas_named(whole, banner, pending, rows, notice, reserved, true);
        let parts = [a.header, a.name, a.banner, a.transcript, a.pending, a.reserved, a.input, a.notice, a.footer];
        let mut covered = vec![0u8; usize::from(w) * usize::from(h)];
        for r in parts {
            prop_assert!(r.right() <= w && r.bottom() <= h, "{r:?} outside {whole:?}");
            for y in r.y..r.bottom() {
                for x in r.x..r.right() {
                    covered[usize::from(y) * usize::from(w) + usize::from(x)] += 1;
                }
            }
        }
        prop_assert!(covered.iter().all(|c| *c == 1), "a gap or an overlap in {a:?}");
        prop_assert!(a.name.height <= 1);
        // The name row exists only when the conversation still has its three rows.
        if a.name.height == 1 && h >= 5 {
            prop_assert!(a.transcript.height >= 3, "{a:?}");
        }
        // The old parts get the rows they got without the name row (a part with no rows only moves with the rows above it).
        let b = areas_named(whole, banner, pending, rows, notice, reserved, false);
        prop_assert_eq!((a.input, a.notice, a.footer, a.header), (b.input, b.notice, b.footer, b.header));
        prop_assert_eq!(
            (a.banner.height, a.pending.height, a.reserved.height),
            (b.banner.height, b.pending.height, b.reserved.height)
        );
    }
}

#[test]
fn the_row_shows_the_name_dim_for_a_seed_and_plain_for_a_model_name() {
    let seed = buffer_of(Some(("Add a percent change function", false)), 100, 24);
    assert_eq!(line(&seed, 1), "Add a percent change function");
    assert!(seed[(0, 1)].modifier.contains(Modifier::DIM));
    let ai = buffer_of(
        Some(("Percent change function for the calculator", true)),
        100,
        24,
    );
    assert_eq!(line(&ai, 1), "Percent change function for the calculator");
    assert!(!ai[(0, 1)].modifier.contains(Modifier::DIM));
}

#[test]
fn the_header_line_is_byte_for_byte_the_same_with_and_without_a_name() {
    let with = buffer_of(Some(("Fix the login redirect loop", true)), 100, 24);
    let without = buffer_of(None, 100, 24);
    assert_eq!(line(&with, 0), line(&without, 0));
    assert!(line(&without, 0).starts_with("ham \u{b7} Run 01M5287J4KQ8N3X7C9D2E6F1AB"));
    assert_eq!(line(&without, 1), "", "no name, no row");
}

#[test]
fn a_name_is_cut_by_columns_with_an_ellipsis_and_a_narrow_screen_has_no_name_row() {
    let long = "Add pagination to the order list endpoint with cursor tokens";
    let cut = buffer_of(Some((long, true)), 30, 24);
    let row = line(&cut, 1);
    assert_eq!(
        row,
        format!("{}\u{2026}", long.chars().take(29).collect::<String>())
    );
    let cjk = "日本語のログイン画面のバグを修正する作業";
    let wide = buffer_of(Some((cjk, true)), 31, 24);
    let row = line(&wide, 1);
    assert_eq!(
        row,
        format!("{}\u{2026}", cjk.chars().take(15).collect::<String>())
    );
    assert!(ratatui::text::Span::raw(row).width() <= 31);
    // Below 20 columns the row is absent: row 1 is the conversation.
    let narrow = buffer_of(Some(("Fix the bug", true)), 19, 24);
    assert!(!line(&narrow, 1).contains("Fix the bug"));
    let just = buffer_of(Some(("Fix the bug", true)), 20, 24);
    assert_eq!(line(&just, 1), "Fix the bug");
}

#[test]
fn an_escape_in_a_name_is_not_drawn() {
    let buf = buffer_of(Some(("Fix \u{1b}[31mthe\u{1b}[0m bug", true)), 100, 24);
    assert_eq!(line(&buf, 1), "Fix the bug");
}

// ---- the screen of the app --------------------------------------------------------------------------------------------------------

fn app_buffer(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

fn second_line(app: &App) -> String {
    screen(app, 120, 30).lines().nth(1).unwrap().to_string()
}

fn first_line(app: &App) -> String {
    let l = screen(app, 120, 30).lines().next().unwrap().to_string();
    ham::tui::version_tag::split_tag(&l).0.to_string()
}

fn started_from_the_composer() -> App {
    let mut app = App::new_direct(
        "n".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings::default(),
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    for c in PROMPT.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(
        matches!(effects.as_slice(), [ham::tui::Effect::SubmitDirect { .. }]),
        "{effects:?}"
    );
    let started: StartRunResponse =
        serde_json::from_value(json!({"runId": RUN, "status": "active"})).unwrap();
    app.update(Msg::Started(Ok(started)));
    app
}

#[test]
fn a_run_started_from_the_composer_shows_its_seed_dim_at_once_before_any_event_arrives() {
    let app = started_from_the_composer();
    let seed = seed_name(&ham_core::run_index::prompt_head(PROMPT).0);
    assert!(!seed.is_empty());
    assert_eq!(second_line(&app), seed, "{}", screen(&app, 120, 30));
    assert!(first_line(&app).starts_with(&format!("ham \u{b7} Run {RUN}")));
    assert!(
        app_buffer(&app, 120, 30)[(0, 1)]
            .modifier
            .contains(Modifier::DIM)
    );
}

#[test]
fn a_name_that_a_read_brings_replaces_the_seed_on_the_screen_and_is_drawn_plain() {
    let mut app = started_from_the_composer();
    let before = first_line(&app);
    app.update(Msg::RunName {
        run_id: RUN.into(),
        name: Some((
            "Percent change function for the calculator".into(),
            Some("ai".into()),
        )),
    });
    assert_eq!(
        second_line(&app),
        "Percent change function for the calculator"
    );
    assert!(
        !app_buffer(&app, 120, 30)[(0, 1)]
            .modifier
            .contains(Modifier::DIM)
    );
    assert_eq!(first_line(&app), before, "the header line does not move");
    // The events of a finished run do not take the name away.
    chatfx::feed(&mut app, finished_events());
    assert_eq!(
        second_line(&app),
        "Percent change function for the calculator"
    );
    assert!(first_line(&app).starts_with(&format!("ham \u{b7} Run {RUN}")));
}
