//! A position is an absolute transcript line and a display cell; the text comes from `TextLines`, one entry per laid-out row with a tag
//! that says how the row continues the one before it (a new line, a soft wrap at a space, a soft wrap inside a word). The selection
//! includes both end cells, takes a wide character whole, and returns the exact text, or `None` when a row it needs is not there. The
//! module reads no terminal; painting works on a ratatui `Buffer`.
#![allow(clippy::unwrap_used)]

use ham::tui::select::{
    Join, Pos, Selection, TextLines, Viewport, cell_width, display_units, text_cells,
};
use ham::tui::transcript::{CONTENT_X, Transcript, TranscriptState};
use ham_core::chat::{Block, ChatModel, Origin, Role, Source};
use proptest::prelude::*;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::StatefulWidget;

fn p(line: usize, col: usize) -> Pos {
    Pos { line, col }
}

fn sel(a: Pos, b: Pos) -> Selection {
    Selection::from_points(a, b)
}

fn hard(rows: &[&str]) -> TextLines {
    TextLines::from_rows(0, rows.iter().map(|r| (*r, Join::Hard)))
}

// ---- widths -----------------------------------------------------------------------------------------------------------------------

#[test]
fn characters_take_zero_one_or_two_cells() {
    let cases: [(char, usize); 11] = [
        ('a', 1),
        (' ', 1),
        ('é', 1),
        ('你', 2),
        ('ｱ', 1),
        ('Ａ', 2),
        ('😀', 2),
        ('\u{301}', 0),
        ('\u{200b}', 0),
        ('\u{fe0f}', 0),
        ('\u{200d}', 0),
    ];
    for (c, w) in cases {
        assert_eq!(cell_width(c), w, "U+{:04X}", c as u32);
    }
}

#[test]
fn a_text_takes_the_cells_a_terminal_gives_it() {
    assert_eq!(text_cells(""), 0);
    assert_eq!(text_cells("abc"), 3);
    assert_eq!(text_cells("你好a"), 5);
    assert_eq!(text_cells("e\u{301}"), 1, "a combining mark adds nothing");
    assert_eq!(text_cells("😀😀"), 4);
    // A family emoji is joined by zero-width joiners into one two-cell symbol.
    assert_eq!(text_cells("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"), 2);
}

#[test]
fn display_units_split_a_text_where_a_layout_may_cut() {
    assert!(display_units("").is_empty());
    assert_eq!(
        display_units("a\u{4f60}e\u{301}"),
        vec![(0, 1, 1), (1, 4, 2), (4, 7, 1)]
    );
    let family = "\u{1f468}\u{200d}\u{1f469}";
    assert_eq!(
        display_units(&format!("x{family}y")),
        vec![
            (0, 1, 1),
            (1, 1 + family.len(), 2),
            (1 + family.len(), 2 + family.len(), 1)
        ]
    );
    // A leading mark joins the first character.
    assert_eq!(display_units("\u{301}x"), vec![(0, 3, 1)]);
}

// ---- the selection ----------------------------------------------------------------------------------------------------------------

#[test]
fn positions_order_by_line_then_cell() {
    assert!(p(1, 9) < p(2, 0));
    assert!(p(2, 3) < p(2, 4));
    assert_eq!(p(0, 0), Pos::default());
}

#[test]
fn the_ends_may_come_in_either_order() {
    let a = sel(p(3, 4), p(1, 2));
    let b = sel(p(1, 2), p(3, 4));
    assert_eq!(a.bounds(), (p(1, 2), p(3, 4)));
    assert_eq!(a.bounds(), b.bounds());
    assert_eq!(a.anchor(), p(3, 4));
    assert_eq!(a.head(), p(1, 2));
}

#[test]
fn extend_moves_the_head_and_keeps_the_anchor() {
    let mut s = Selection::new(p(2, 5));
    assert_eq!(s.bounds(), (p(2, 5), p(2, 5)));
    assert!(s.is_single_cell());
    s.extend(p(4, 1));
    assert_eq!(s.anchor(), p(2, 5));
    assert_eq!(s.head(), p(4, 1));
    assert!(!s.is_single_cell());
}

#[test]
fn both_end_cells_are_in_the_text() {
    let t = hard(&["hello world"]);
    assert_eq!(sel(p(0, 0), p(0, 4)).text(&t).unwrap(), "hello");
    assert_eq!(sel(p(0, 6), p(0, 10)).text(&t).unwrap(), "world");
    assert_eq!(sel(p(0, 4), p(0, 4)).text(&t).unwrap(), "o");
}

#[test]
fn a_selection_over_several_lines_takes_the_rest_of_the_first_whole_middle_lines_and_the_start_of_the_last()
 {
    let t = hard(&["alpha beta", "gamma", "delta epsilon"]);
    assert_eq!(
        sel(p(0, 6), p(2, 4)).text(&t).unwrap(),
        "beta\ngamma\ndelta"
    );
}

#[test]
fn a_soft_wrap_joins_the_rows_by_the_way_they_were_broken() {
    // One paragraph wrapped at a space, then a long word cut at the width, then a new line.
    let t = TextLines::from_rows(
        0,
        [
            ("see the", Join::Hard),
            ("docs at https://exa", Join::Space),
            ("mple.org/very/long", Join::Cut),
            ("next paragraph", Join::Hard),
        ],
    );
    assert_eq!(
        sel(p(0, 0), p(3, 13)).text(&t).unwrap(),
        "see the docs at https://example.org/very/long\nnext paragraph"
    );
    assert_eq!(
        t.all_text(),
        "see the docs at https://example.org/very/long\nnext paragraph"
    );
}

#[test]
fn a_start_beyond_the_end_of_a_line_and_an_end_beyond_it_clamp() {
    let t = hard(&["ab", "cd"]);
    assert_eq!(sel(p(0, 1), p(1, 40)).text(&t).unwrap(), "b\ncd");
    assert_eq!(sel(p(0, 40), p(1, 0)).text(&t).unwrap(), "\nc");
}

#[test]
fn an_empty_row_between_lines_is_an_empty_line() {
    let t = hard(&["one", "", "three"]);
    assert_eq!(sel(p(0, 0), p(2, 4)).text(&t).unwrap(), "one\n\nthree");
}

#[test]
fn a_row_that_is_not_in_the_lines_gives_nothing_not_a_prefix() {
    let t = TextLines::from_rows(10, [("ten", Join::Hard), ("eleven", Join::Hard)]);
    assert_eq!(t.base(), 10);
    assert_eq!(t.len(), 2);
    assert_eq!(sel(p(10, 0), p(11, 5)).text(&t).unwrap(), "ten\neleven");
    assert!(sel(p(9, 0), p(11, 5)).text(&t).is_none());
    assert!(sel(p(10, 0), p(12, 0)).text(&t).is_none());
    assert!(sel(p(0, 0), p(0, 3)).text(&TextLines::new(0)).is_none());
}

#[test]
fn a_wide_character_is_taken_whole_when_either_of_its_cells_is_selected() {
    // cells: a=0 你=1,2 好=3,4 b=5
    let t = hard(&["a你好b"]);
    assert_eq!(sel(p(0, 2), p(0, 3)).text(&t).unwrap(), "你好");
    assert_eq!(sel(p(0, 1), p(0, 1)).text(&t).unwrap(), "你");
    assert_eq!(sel(p(0, 2), p(0, 2)).text(&t).unwrap(), "你");
    assert_eq!(sel(p(0, 4), p(0, 5)).text(&t).unwrap(), "好b");
    assert_eq!(sel(p(0, 0), p(0, 0)).text(&t).unwrap(), "a");
}

#[test]
fn a_combining_mark_stays_with_its_letter_and_never_starts_a_selection() {
    let t = hard(&["xe\u{301}y"]);
    assert_eq!(sel(p(0, 1), p(0, 1)).text(&t).unwrap(), "e\u{301}");
    assert_eq!(sel(p(0, 2), p(0, 2)).text(&t).unwrap(), "y");
    assert_eq!(sel(p(0, 0), p(0, 1)).text(&t).unwrap(), "xe\u{301}");
}

#[test]
fn a_mark_with_no_letter_before_it_goes_with_the_first_character() {
    let t = hard(&["\u{301}xy", "\u{200b}"]);
    assert_eq!(sel(p(0, 0), p(0, 0)).text(&t).unwrap(), "\u{301}x");
    assert_eq!(sel(p(0, 1), p(0, 1)).text(&t).unwrap(), "y");
    // A row of nothing but a zero-width character has no cell; a selection that covers the row takes it.
    assert_eq!(
        sel(p(0, 0), p(1, 0)).text(&t).unwrap(),
        "\u{301}xy\n\u{200b}"
    );
}

#[test]
fn an_emoji_sequence_is_one_unit() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let t = hard(&[&format!("a{family}b")]);
    assert_eq!(sel(p(0, 1), p(0, 1)).text(&t).unwrap(), family);
    assert_eq!(
        sel(p(0, 2), p(0, 3)).text(&t).unwrap(),
        format!("{family}b")
    );
}

#[test]
fn the_cells_to_highlight_follow_the_same_rules() {
    let t = hard(&["a你好b", "xyz", "", "end"]);
    let s = sel(p(0, 2), p(3, 1));
    assert_eq!(
        s.cells_on(&t, 0),
        Some(1..6),
        "from the wide character whole"
    );
    assert_eq!(s.cells_on(&t, 1), Some(0..3));
    assert_eq!(s.cells_on(&t, 2), None, "an empty row has nothing to paint");
    assert_eq!(s.cells_on(&t, 3), Some(0..2));
    assert_eq!(s.cells_on(&t, 4), None);
    let one = sel(p(0, 5), p(0, 40));
    assert_eq!(one.cells_on(&t, 0), Some(5..6));
}

// ---- the lines of a drawn buffer --------------------------------------------------------------------------------------------------

fn buffer_with(rows: &[&str], w: u16) -> Buffer {
    let area = Rect::new(0, 0, w, rows.len() as u16);
    let mut b = Buffer::empty(area);
    for (y, r) in rows.iter().enumerate() {
        b.set_string(0, y as u16, r, ratatui::style::Style::default());
    }
    b
}

#[test]
fn lines_read_from_a_buffer_skip_the_cells_behind_a_wide_character_and_trim_the_end() {
    let b = buffer_with(&["abc   ", "你好   ", "      ", "x😀y   "], 10);
    let t = TextLines::from_buffer(&b, Rect::new(0, 0, 10, 4), 100);
    assert_eq!(t.base(), 100);
    assert_eq!(t.len(), 4);
    assert_eq!(t.line(100), Some("abc"));
    assert_eq!(t.line(101), Some("你好"));
    assert_eq!(t.line(102), Some(""));
    assert_eq!(t.line(103), Some("x😀y"));
    assert_eq!(t.line(104), None);
    assert_eq!(t.join(101), Some(Join::Hard));
}

#[test]
fn lines_read_from_a_buffer_take_only_the_content_rectangle() {
    let b = buffer_with(&["you    hello there!|", "       second row   |"], 20);
    let t = TextLines::from_buffer(&b, Rect::new(7, 0, 12, 2), 0);
    assert_eq!(t.line(0), Some("hello there!"));
    assert_eq!(t.line(1), Some("second row"));
}

#[test]
fn a_rectangle_that_leaves_the_buffer_is_cut_to_it() {
    let b = buffer_with(&["abcdef"], 6);
    let t = TextLines::from_buffer(&b, Rect::new(4, 0, 30, 5), 0);
    assert_eq!(t.len(), 1);
    assert_eq!(t.line(0), Some("ef"));
}

// ---- the transcript, drawn ----------------------------------------------------------------------------------------------------------

fn conversation() -> ChatModel {
    ChatModel::from_blocks(
        Source::FullHistory,
        "cline",
        vec![
            Block::message("p", Role::You, Origin::User, "Build a todo API"),
            Block::message("a1", Role::Engine, Origin::Engine, "I will run the tests."),
            Block::tool_call(
                "t1",
                "run_commands: pytest -q",
                Some("execute"),
                Some("completed"),
                "12 passed\nhidden detail line",
            ),
            Block::message("a2", Role::Engine, Origin::Engine, "All green."),
        ],
    )
}

struct Drawn {
    buf: Buffer,
    top: usize,
}

fn draw(m: &ChatModel, st: &mut TranscriptState, w: u16, h: u16) -> Drawn {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    Transcript::new(m).render(area, &mut buf, st);
    Drawn {
        buf,
        top: st.top_line(),
    }
}

fn content(w: u16, h: u16) -> Rect {
    Rect::new(CONTENT_X, 0, w - CONTENT_X - 1, h)
}

#[test]
fn a_selection_across_a_folded_block_takes_what_is_on_screen_and_not_the_hidden_body() {
    let m = conversation();
    let mut st = TranscriptState::new();
    let d = draw(&m, &mut st, 60, 10);
    let lines = TextLines::from_buffer(&d.buf, content(60, 10), d.top);
    let all = lines.all_text();
    assert!(all.contains("Build a todo API"), "{all}");
    assert!(all.contains("run_commands: pytest -q"), "{all}");
    assert!(!all.contains("hidden detail line"), "folded: {all}");
    // From the middle of the first answer to the start of the last one.
    let v = Viewport {
        content: content(60, 10),
        first_line: d.top,
    };
    let from = v.pos(CONTENT_X + 2, 1);
    let to = v.pos(CONTENT_X + 3, 3);
    let got = sel(from, to).text(&lines).unwrap();
    assert!(got.starts_with("will run the tests."), "{got}");
    assert!(got.contains("run_commands: pytest -q"), "{got}");
    assert!(got.ends_with("All "), "{got}");
    assert!(!got.contains("hidden"), "{got}");
    assert!(!got.contains("cline"), "no role word is ever copied: {got}");
    assert!(!got.contains("you"), "{got}");
}

#[test]
fn an_open_block_gives_its_body_to_the_selection() {
    let m = conversation();
    let mut st = TranscriptState::new();
    st.toggle_fold(&m, "t1");
    let d = draw(&m, &mut st, 60, 10);
    let lines = TextLines::from_buffer(&d.buf, content(60, 10), d.top);
    let v = Viewport {
        content: content(60, 10),
        first_line: d.top,
    };
    let got = sel(v.pos(CONTENT_X, 0), v.pos(CONTENT_X + 40, 9))
        .text(&lines)
        .unwrap();
    assert!(got.contains("12 passed\nhidden detail line"), "{got}");
}

#[test]
fn the_viewport_turns_a_screen_cell_into_a_position_and_clamps_outside_it() {
    let v = Viewport {
        content: Rect::new(9, 4, 30, 6),
        first_line: 200,
    };
    assert_eq!(v.pos(9, 4), p(200, 0));
    assert_eq!(v.pos(12, 6), p(202, 3));
    assert_eq!(v.pos(2, 6), p(202, 0), "the gutter is column 0");
    assert_eq!(
        v.pos(100, 6),
        p(202, 30),
        "past the right edge is the end of the row"
    );
    assert_eq!(v.pos(12, 0), p(200, 3), "above is the first row");
    assert_eq!(v.pos(12, 40), p(205, 3), "below is the last row");
}

#[test]
fn a_selection_scrolled_out_of_view_still_gives_its_text_from_the_full_lines() {
    // The full layout is known (first_line 0, 40 rows); the view shows rows 30..40.
    let rows: Vec<(String, Join)> = (0..40)
        .map(|i| (format!("row {i:02}"), Join::Hard))
        .collect();
    let t = TextLines::from_rows(0, rows);
    let s = sel(p(5, 0), p(34, 5));
    let got = s.text(&t).unwrap();
    assert!(got.starts_with("row 05\nrow 06"));
    assert!(got.ends_with("row 34"));
    assert_eq!(got.lines().count(), 30);
}

// ---- is the selection still where the user put it -----------------------------------------------------------------------------------

#[test]
fn the_fingerprint_of_a_range_changes_when_a_row_in_it_changes_and_only_then() {
    let a = hard(&["one", "two", "three", "four"]);
    let f = a.fingerprint(1, 2);
    assert_eq!(f, hard(&["one", "two", "three", "four"]).fingerprint(1, 2));
    assert_eq!(
        f,
        hard(&["X", "two", "three", "Y", "more"]).fingerprint(1, 2),
        "rows outside the range do not count"
    );
    assert_ne!(f, hard(&["one", "TWO", "three", "four"]).fingerprint(1, 2));
    let soft = TextLines::from_rows(
        0,
        [
            ("one", Join::Hard),
            ("two", Join::Hard),
            ("three", Join::Space),
            ("four", Join::Hard),
        ],
    );
    assert_ne!(f, soft.fingerprint(1, 2), "how a row joins counts");
    assert_ne!(
        f,
        hard(&["one", "two"]).fingerprint(1, 2),
        "a row that is gone"
    );
}

#[test]
fn a_selection_is_still_current_while_the_rows_it_covers_are_unchanged() {
    let t = hard(&["one", "two", "three", "four"]);
    let s = sel(p(1, 0), p(2, 3));
    let f = s.fingerprint(&t);
    // The conversation grows at the end: the selection stays.
    assert_eq!(
        s.fingerprint(&hard(&["one", "two", "three", "four", "five"])),
        f
    );
    // A fold opened above shifts the rows: the same line numbers hold other text.
    assert_ne!(
        s.fingerprint(&hard(&["one", "NEW", "two", "three", "four"])),
        f
    );
    // A resize wraps the rows again.
    let narrow = TextLines::from_rows(
        0,
        [
            ("one", Join::Hard),
            ("tw", Join::Hard),
            ("o", Join::Cut),
            ("three", Join::Hard),
        ],
    );
    assert_ne!(s.fingerprint(&narrow), f);
}

// ---- painting -----------------------------------------------------------------------------------------------------------------------

fn reversed(b: &Buffer, x: u16, y: u16) -> bool {
    b[(x, y)].modifier.contains(Modifier::REVERSED)
}

#[test]
fn the_selected_cells_are_reversed_and_no_others() {
    let b0 = buffer_with(&["a你好b  ", "xyz    ", "       ", "end    "], 12);
    let content = Rect::new(2, 0, 8, 4);
    let t = TextLines::from_buffer(&b0, Rect::new(0, 0, 12, 4), 0);
    // Move the text two cells right so the content rectangle is not at x 0.
    let mut b = buffer_with(&["  a你好b  ", "  xyz    ", "         ", "  end    "], 12);
    let s = sel(p(0, 2), p(3, 1));
    s.paint(&t, &mut b, content, 0);
    let on: Vec<(u16, u16)> = (0..4)
        .flat_map(|y| (0..12).map(move |x| (x, y)))
        .filter(|(x, y)| reversed(&b, *x, *y))
        .collect();
    let want: Vec<(u16, u16)> = [
        (3, 0),
        (4, 0),
        (5, 0),
        (6, 0),
        (7, 0), // 你好b: cells 1..5 of the row, shifted by 2
        (2, 1),
        (3, 1),
        (4, 1),
        (2, 3),
        (3, 3),
    ]
    .to_vec();
    assert_eq!(on, want);
}

#[test]
fn painting_stays_inside_the_content_rectangle_and_the_visible_rows() {
    let t = TextLines::from_rows(0, (0..30).map(|i| (format!("line number {i}"), Join::Hard)));
    let mut b = Buffer::empty(Rect::new(0, 0, 20, 6));
    for y in 0..6 {
        b.set_string(
            0,
            y,
            "........................",
            ratatui::style::Style::default(),
        );
    }
    // The view starts at line 10 and is 6 rows tall; the selection starts above it and ends below it.
    sel(p(2, 0), p(25, 3)).paint(&t, &mut b, Rect::new(3, 0, 10, 6), 10);
    for y in 0..6 {
        for x in 0..20 {
            let inside = (3..13).contains(&x);
            assert_eq!(reversed(&b, x, y), inside, "({x},{y})");
        }
    }
}

// ---- properties ---------------------------------------------------------------------------------------------------------------------

fn arb_char() -> impl Strategy<Value = char> {
    prop::sample::select(vec![
        'a', 'b', 'c', ' ', '-', '你', '好', '😀', 'é', '\u{301}', '\u{200d}', '\u{fe0f}', 'Ａ',
    ])
}

fn arb_join() -> impl Strategy<Value = Join> {
    prop::sample::select(vec![Join::Hard, Join::Space, Join::Cut])
}

fn arb_lines() -> impl Strategy<Value = TextLines> {
    prop::collection::vec((prop::collection::vec(arb_char(), 0..10), arb_join()), 1..8).prop_map(
        |rows| {
            TextLines::from_rows(
                3,
                rows.into_iter()
                    .map(|(cs, j)| (cs.into_iter().collect::<String>(), j)),
            )
        },
    )
}

proptest! {
    #[test]
    fn the_selected_text_is_a_run_of_the_whole_text_whichever_way_it_was_dragged(
        lines in arb_lines(),
        a in (3usize..11, 0usize..14),
        b in (3usize..11, 0usize..14),
    ) {
        let (a, b) = (p(a.0, a.1), p(b.0, b.1));
        let n = lines.len();
        let in_range = |q: Pos| q.line >= 3 && q.line < 3 + n;
        let forward = sel(a, b).text(&lines);
        let backward = sel(b, a).text(&lines);
        prop_assert_eq!(&forward, &backward);
        if in_range(a) && in_range(b) {
            let got = forward.unwrap();
            let all = lines.all_text();
            prop_assert!(all.contains(&got), "{got:?} is not a run of {all:?}");
            prop_assert!(got.chars().count() <= all.chars().count());
        } else {
            prop_assert!(forward.is_none());
        }
    }

    #[test]
    fn the_whole_range_is_the_whole_text(lines in arb_lines()) {
        let last = 3 + lines.len() - 1;
        let s = sel(p(3, 0), p(last, usize::MAX / 2));
        prop_assert_eq!(s.text(&lines).unwrap(), lines.all_text());
    }

    #[test]
    fn highlighted_cells_lie_inside_their_row_and_start_and_end_on_whole_characters(
        lines in arb_lines(),
        a in (3usize..11, 0usize..14),
        b in (3usize..11, 0usize..14),
    ) {
        let s = sel(p(a.0, a.1), p(b.0, b.1));
        for line in 3..3 + lines.len() {
            if let Some(r) = s.cells_on(&lines, line) {
                let text = lines.line(line).unwrap();
                prop_assert!(r.start < r.end && r.end <= text_cells(text), "{r:?} in {text:?}");
                let mut edges = vec![0usize];
                let mut buf = String::new();
                for c in text.chars() {
                    buf.push(c);
                    edges.push(text_cells(&buf));
                }
                prop_assert!(edges.contains(&r.start) && edges.contains(&r.end), "{r:?} edges {edges:?} in {text:?}");
            }
        }
    }
}
