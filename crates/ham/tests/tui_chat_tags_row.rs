//! The tags on the name row of the run screen. The row under the header shows the run's name; the tags follow it, two
//! spaces later and dim: whole tags first, then `+N` for the rest. The name has priority: a name that fills the row leaves no room for a tag,
//! and a row that has no room on the screen is not drawn at all. A run with tags and no name draws no row (a row of its own would move every
//! row below it), so `/tag` also says in its note what the run holds. The header line is the same bytes with and without tags.
#![allow(clippy::unwrap_used)]

mod chatfx;
mod tagsfx;

use chatfx::{RUN, base_events, ready};
use ham::tui::chat::{ChatFacts, ChatView, Header, InputPane};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::{Block as UiBlock, Paragraph};
use tagsfx::{named, read_first, s3_only};

const NAME: &str = "Monthly CSV export with manifest";
const TAGS: [&str; 4] = [
    "project:billing-export",
    "topic:csv-export",
    "tech:python",
    "tech:s3",
];

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

fn buffer_of(
    name: Option<(&str, bool)>,
    tags: &[String],
    w: u16,
    h: u16,
) -> ratatui::buffer::Buffer {
    let head = header();
    let mut facts = ChatFacts::new(&head, "footer");
    facts.name = name;
    facts.tags = tags;
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

fn tags(n: usize) -> Vec<String> {
    TAGS.iter().take(n).map(|t| (*t).to_string()).collect()
}

#[test]
fn the_row_shows_the_name_then_dim_tags() {
    let buf = buffer_of(Some((NAME, true)), &tags(2), 120, 24);
    assert_eq!(
        line(&buf, 1),
        format!("{NAME}  project:billing-export topic:csv-export")
    );
    // A model's name is plain; the tags are dim.
    assert!(!buf[(0, 1)].modifier.contains(Modifier::DIM));
    let at = NAME.chars().count() as u16 + 2;
    for x in [at, at + 5, at + 20] {
        assert!(buf[(x, 1)].modifier.contains(Modifier::DIM), "column {x}");
    }
    // A seed is dim and so are its tags.
    let seed = buffer_of(Some((NAME, false)), &tags(1), 120, 24);
    assert!(seed[(0, 1)].modifier.contains(Modifier::DIM));
    assert!(seed[(at, 1)].modifier.contains(Modifier::DIM));
}

#[test]
fn whole_tags_then_plus_n_when_they_do_not_fit() {
    let all = tags(4);
    // 120 columns: all four.
    assert_eq!(
        line(&buffer_of(Some((NAME, true)), &all, 120, 24), 1),
        format!("{NAME}  project:billing-export topic:csv-export tech:python tech:s3")
    );
    // 80 columns: the name takes 32 and the gap 2, 46 are left: two whole tags (39) and ` +2` (42).
    assert_eq!(
        line(&buffer_of(Some((NAME, true)), &all, 80, 24), 1),
        format!("{NAME}  project:billing-export topic:csv-export +2")
    );
    // 60 columns: 26 are left: one tag (22) and ` +3` (25).
    assert_eq!(
        line(&buffer_of(Some((NAME, true)), &all, 60, 24), 1),
        format!("{NAME}  project:billing-export +3")
    );
    // 50 columns: 16 are left, no whole tag fits: the count alone.
    assert_eq!(
        line(&buffer_of(Some((NAME, true)), &all, 50, 24), 1),
        format!("{NAME}  +4")
    );
    // 35 columns: one column is left, not even `+4` fits.
    assert_eq!(line(&buffer_of(Some((NAME, true)), &all, 35, 24), 1), NAME);
    // One tag that fits exactly leaves no `+N`.
    let one = tags(1);
    assert_eq!(
        line(&buffer_of(Some((NAME, true)), &one, 56, 24), 1),
        format!("{NAME}  project:billing-export")
    );
}

#[test]
fn the_name_has_priority_and_a_row_with_no_room_is_not_drawn() {
    let long = "Add pagination to the order list endpoint with cursor tokens";
    // The name fills the row: it is cut as it was cut before and no tag is drawn.
    let buf = buffer_of(Some((long, true)), &tags(2), 30, 24);
    assert_eq!(
        line(&buf, 1),
        format!("{}\u{2026}", long.chars().take(29).collect::<String>())
    );
    // A name of exactly the width leaves no tag either.
    let exact = "x".repeat(40);
    assert_eq!(
        line(&buffer_of(Some((&exact, true)), &tags(2), 40, 24), 1),
        exact
    );
    // A screen too short for the row draws none (the conversation keeps its rows), with or without tags.
    let short_with = buffer_of(Some((NAME, true)), &tags(2), 60, 8);
    let short_without = buffer_of(Some((NAME, true)), &[], 60, 8);
    for y in 0..8 {
        assert_eq!(line(&short_with, y), line(&short_without, y), "row {y}");
    }
    // Below 20 columns there is no name row, so no tags.
    assert_eq!(
        line(&buffer_of(Some((NAME, true)), &tags(2), 19, 24), 1),
        ""
    );
}

#[test]
fn tags_without_a_name_draw_no_row_and_the_header_is_todays() {
    let with = buffer_of(None, &tags(3), 100, 24);
    let without = buffer_of(None, &[], 100, 24);
    for y in 0..24 {
        assert_eq!(line(&with, y), line(&without, y), "row {y}");
    }
    assert_eq!(line(&with, 1), "", "no name, no row");
    // The header line is the same bytes with and without tags on the name row.
    let a = buffer_of(Some((NAME, true)), &tags(3), 100, 24);
    let b = buffer_of(Some((NAME, true)), &[], 100, 24);
    assert_eq!(line(&a, 0), line(&b, 0));
    assert!(line(&a, 0).starts_with("ham \u{b7} Run 01M5287J4KQ8N3X7C9D2E6F1AB"));
    // Everything below the name row is the same too.
    for y in 2..24 {
        assert_eq!(line(&a, y), line(&b, y), "row {y}");
    }
}

#[test]
fn a_tag_is_cleaned_before_it_is_drawn() {
    let dirty = vec![
        "topic:ok".to_string(),
        "topic:ev\u{1b}[31mil\u{7}".to_string(),
    ];
    let buf = buffer_of(Some((NAME, true)), &dirty, 120, 24);
    let row = line(&buf, 1);
    assert!(!row.contains('\u{1b}') && !row.contains('\u{7}'), "{row:?}");
    assert!(row.starts_with(&format!("{NAME}  topic:ok")), "{row:?}");
}

// ---- the run screen of the app -----------------------------------------------------------------------------------------------------

#[test]
fn the_run_screen_takes_the_tags_of_the_held_row_and_a_later_read_changes_them() {
    let mut app = ready(base_events());
    // No held row for the run: no name and no tags on the screen.
    let text = chatfx::screen(&app, 120, 30);
    assert!(!text.contains("project:") && !text.contains(NAME), "{text}");
    read_first(
        &mut app,
        vec![s3_only(RUN, "2026-10-20T12:00:00Z")],
        false,
        vec![named(
            RUN,
            NAME,
            Some(&["project:billing-export", "tech:s3"]),
        )],
    );
    let text = chatfx::screen(&app, 120, 30);
    assert_eq!(
        text.lines().nth(1).unwrap(),
        format!("{NAME}  project:billing-export tech:s3"),
        "{text}"
    );
    // The list is read again (a manual read while the run is open): the row shows what the new read says, an empty list included.
    read_first(
        &mut app,
        vec![s3_only(RUN, "2026-10-20T12:00:00Z")],
        false,
        vec![named(RUN, NAME, Some(&[]))],
    );
    let text = chatfx::screen(&app, 120, 30);
    assert_eq!(text.lines().nth(1).unwrap(), NAME, "{text}");
    // An older backend sends no tags: the project the repository gives (here none, the run is s3-only).
    read_first(
        &mut app,
        vec![tagsfx::repo(RUN, "2026-10-20T12:00:00Z", "calc")],
        false,
        vec![named(RUN, NAME, None)],
    );
    let text = chatfx::screen(&app, 120, 30);
    assert_eq!(
        text.lines().nth(1).unwrap(),
        format!("{NAME}  project:calc"),
        "{text}"
    );
}
