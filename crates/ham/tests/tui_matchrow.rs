//! The one row under the Ctrl-R bar: the matching prompt as pieces, the occurrences of the query marked, cut to the width of the
//! terminal around the first mark. The module is pure: text, mark ranges (in characters) and a width in, pieces out, none wider than the
//! width in terminal cells. Wide characters take two cells and combining marks none.
#![allow(clippy::unwrap_used)]

use ham::tui::matchrow::{ELLIPSIS, Piece, cut};
use ham::tui::select::text_cells;
use proptest::prelude::*;
use std::ops::Range;

/// One mark, from `start` to `end` (a struct literal: clippy reads `[a..b]` as a slip for `[a, b]`).
fn one(start: usize, end: usize) -> Vec<Range<usize>> {
    vec![Range { start, end }]
}

fn piece(text: &str, marked: bool) -> Piece {
    Piece {
        text: text.to_string(),
        marked,
    }
}

fn cells(pieces: &[Piece]) -> usize {
    pieces.iter().map(|p| text_cells(&p.text)).sum()
}

fn marked(pieces: &[Piece]) -> String {
    pieces
        .iter()
        .filter(|p| p.marked)
        .map(|p| p.text.as_str())
        .collect()
}

/// The text of the first marked piece.
fn first_marked(pieces: &[Piece]) -> String {
    pieces
        .iter()
        .find(|p| p.marked)
        .map(|p| p.text.clone())
        .unwrap_or_default()
}

fn joined(pieces: &[Piece]) -> String {
    pieces.iter().map(|p| p.text.as_str()).collect()
}

fn ellipsis() -> String {
    ELLIPSIS.to_string()
}

#[test]
fn text_that_fits_is_shown_whole_with_its_marks() {
    let got = cut("run the tests again", &one(8, 13), 40);
    assert_eq!(
        got,
        vec![
            piece("run the ", false),
            piece("tests", true),
            piece(" again", false)
        ]
    );
}

#[test]
fn every_occurrence_is_a_marked_piece() {
    let got = cut("tests and TESTS", &[0..5, 10..15], 40);
    assert_eq!(
        got,
        vec![
            piece("tests", true),
            piece(" and ", false),
            piece("TESTS", true)
        ]
    );
}

#[test]
fn text_without_marks_is_one_plain_piece_and_width_zero_shows_nothing() {
    assert_eq!(cut("plain", &[], 20), vec![piece("plain", false)]);
    assert!(cut("plain", &[], 0).is_empty());
    assert!(cut("", &[], 10).is_empty());
}

#[test]
fn a_long_text_is_cut_on_the_right_with_an_ellipsis_when_the_first_mark_is_near_the_start() {
    let text = format!("Fix the parser{}", " and more words".repeat(10));
    let got = cut(&text, &one(4, 7), 30);
    assert!(cells(&got) <= 30, "{got:?}");
    assert_eq!(marked(&got), "the");
    assert_eq!(got.first().unwrap().text, "Fix ", "{got:?}");
    let last = got.last().unwrap();
    assert_eq!(last.text, ellipsis());
    assert!(!last.marked);
}

#[test]
fn a_mark_late_in_a_long_text_is_brought_into_view_with_an_ellipsis_on_the_left() {
    let text = format!("{}needle{}", "x ".repeat(60), " y".repeat(30));
    let start = 120;
    let got = cut(&text, &one(start, start + 6), 40);
    assert!(cells(&got) <= 40, "{got:?}");
    assert_eq!(marked(&got), "needle", "{got:?}");
    assert_eq!(got.first().unwrap().text, ellipsis());
    assert_eq!(got.last().unwrap().text, ellipsis());
}

#[test]
fn the_end_of_a_text_shows_no_ellipsis_on_the_right() {
    let text = format!("{}needle", "x ".repeat(60));
    let got = cut(&text, &one(120, 126), 30);
    assert!(cells(&got) <= 30, "{got:?}");
    assert_eq!(marked(&got), "needle");
    assert_eq!(got.first().unwrap().text, ellipsis());
    assert_ne!(got.last().unwrap().text, ellipsis(), "{got:?}");
    assert!(joined(&got).ends_with("needle"));
}

#[test]
fn the_first_mark_stays_in_view_at_every_width_from_four_up() {
    let text = format!("{}needle{}", "ab ".repeat(40), " cd".repeat(40));
    let start = 120;
    for width in 4..=80 {
        let got = cut(&text, &[start..start + 6, start + 40..start + 46], width);
        assert!(cells(&got) <= width, "width {width}: {got:?}");
        let first = first_marked(&got);
        assert!(
            !first.is_empty() && "needle".starts_with(&first),
            "width {width}: {got:?}"
        );
    }
}

#[test]
fn a_width_with_no_room_for_ellipses_starts_at_the_first_mark() {
    let text = format!("{}needle{}", "ab ".repeat(40), " cd".repeat(40));
    for width in 1..=3 {
        let got = cut(&text, &one(120, 126), width);
        assert!(cells(&got) <= width, "width {width}: {got:?}");
        assert!(!marked(&got).is_empty(), "width {width}: {got:?}");
        assert!(
            "needle".starts_with(&marked(&got)),
            "width {width}: {got:?}"
        );
        assert!(
            got.iter().all(|p| p.text != ellipsis()),
            "width {width}: {got:?}"
        );
    }
}

#[test]
fn wide_characters_take_two_cells_and_never_overflow() {
    let text = "日本語のテキストの途中に needle がある 日本語のテキスト";
    let at = text.chars().position(|c| c == 'n').unwrap();
    for width in 4..=40 {
        let got = cut(text, &one(at, at + 6), width);
        assert!(cells(&got) <= width, "width {width}: {got:?}");
        assert!(
            "needle".starts_with(&marked(&got)) && !marked(&got).is_empty(),
            "width {width}: {got:?}"
        );
    }
}

#[test]
fn a_window_never_starts_on_a_combining_mark_that_has_no_letter_before_it() {
    // The accent is the first occurrence. At every width, narrow ones included, its letter comes with it.
    let text = "abcdef\u{301}xyz";
    for width in 1..=10 {
        let got = cut(text, &one(6, 7), width);
        assert!(cells(&got) <= width, "width {width}: {got:?}");
        let shown = joined(&got);
        let first = shown.trim_start_matches(ELLIPSIS).chars().next();
        assert_ne!(first, Some('\u{301}'), "width {width}: {got:?}");
        assert!(shown.contains("f\u{301}"), "width {width}: {got:?}");
    }
}

#[test]
fn marks_that_reach_past_the_text_are_clipped() {
    let got = cut("abc", &[1..10, 20..30], 10);
    assert_eq!(got, vec![piece("a", false), piece("bc", true)]);
}

#[test]
fn a_mark_wider_than_the_row_is_cut_at_the_row() {
    let text = "z".repeat(100);
    let got = cut(&text, &one(0, 100), 20);
    assert!(cells(&got) <= 20, "{got:?}");
    assert!(marked(&got).chars().all(|c| c == 'z'));
    assert!(marked(&got).chars().count() >= 18, "{got:?}");
}

proptest! {
    #[test]
    fn a_row_is_never_wider_than_the_width_and_keeps_the_first_mark(
        text in "[a-z 日本\u{23ce}\u{301}]{0,120}",
        marks in proptest::collection::vec((0usize..130, 1usize..8), 0..5),
        width in 0usize..70,
    ) {
        let n = text.chars().count();
        // Ranges in order that do not overlap, as the matcher gives them.
        let mut at = 0usize;
        let mut ranges = Vec::new();
        for (gap, len) in marks {
            let start = at + gap % 20;
            let end = start + len;
            if end > n { break; }
            ranges.push(start..end);
            at = end;
        }
        let got = cut(&text, &ranges, width);
        prop_assert!(cells(&got) <= width, "{:?} at {}", got, width);
        if width >= 4 && let Some(first) = ranges.first() {
            let chars: Vec<char> = text.chars().collect();
            let w = text_cells(&chars[first.start].to_string());
            if w <= width - 2 {
                prop_assert!(!marked(&got).is_empty(), "the first mark is in view: {:?} at {}", got, width);
            }
        }
    }
}
