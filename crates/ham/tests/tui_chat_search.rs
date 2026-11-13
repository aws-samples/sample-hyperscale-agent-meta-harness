//! Search inside one chat, the TUI side. `TranscriptState` gets a way to open and fold a block, to bring the row that holds a
//! given non-blank character into view, and to paint marks over matches (also over a match the wrap cut in two). `ChatSearchUi` joins
//! `ham_core::search::ChatSearch` to the transcript: it opens a folded block that holds a match, puts the scroll position back when the
//! search is cancelled, and closes the blocks it opened. `SearchBar` is the one-line bar. Layout work is counted, never timed.
#![allow(clippy::unwrap_used)]

use ham::tui::chat_search::{ChatSearchUi, SearchBar, mark_of};
use ham::tui::transcript::{
    CONTENT_X, CURRENT_MARK, Mark, OTHER_MARK, Transcript, TranscriptState,
};
use ham_core::chat::{Block, ChatModel, Origin, Role, Source};
use ham_core::search::{Key, Phase};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{StatefulWidget, Widget};
use std::collections::HashMap;

fn msg(i: usize, text: &str) -> Block {
    Block::message(&format!("m{i}"), Role::Engine, Origin::Engine, text)
}

fn model(blocks: Vec<Block>) -> ChatModel {
    ChatModel::from_blocks(Source::FullHistory, "cline", blocks)
}

const AREA: Rect = Rect::new(0, 0, 60, 10);

fn draw(m: &ChatModel, st: &mut TranscriptState, area: Rect) -> Buffer {
    let mut buf = Buffer::empty(area);
    Transcript::new(m).render(area, &mut buf, st);
    buf
}

/// The drawn rows without the scrollbar column, trailing spaces cut.
fn lines(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (a.y..a.bottom())
        .map(|y| {
            (a.x..a.right() - 1)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn shows(buf: &Buffer, needle: &str) -> bool {
    lines(buf).iter().any(|l| l.contains(needle))
}

/// 60 one-row messages; "needle" is in 10, 30 and 50, and block 40 is a folded tool call with a phrase in its body.
fn long() -> ChatModel {
    let mut blocks = Vec::new();
    for i in 0..60 {
        if i == 40 {
            blocks.push(Block::tool_call(
                "t40",
                "run_commands: ls",
                Some("execute"),
                Some("completed"),
                "first line\nfound the deep inside marker here\nlast line",
            ));
        } else if [10, 30, 50].contains(&i) {
            blocks.push(msg(i, &format!("line {i} needle")));
        } else {
            blocks.push(msg(i, &format!("line {i} plain")));
        }
    }
    model(blocks)
}

fn type_str(ui: &mut ChatSearchUi, st: &mut TranscriptState, m: &ChatModel, text: &str) {
    for c in text.chars() {
        ui.key(Key::Char(c), st, m);
        draw(m, st, AREA);
    }
}

// ---- the transcript's new reach -----------------------------------------------------------------------------------------------------

#[test]
fn a_block_can_be_opened_and_closed_by_id() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    assert_eq!(st.is_folded(&m, "t40"), Some(true));
    assert_eq!(
        st.is_folded(&m, "m1"),
        None,
        "a message has nothing to fold"
    );
    assert_eq!(st.is_folded(&m, "nope"), None);
    assert!(st.open_fold(&m, "t40"));
    assert!(!st.open_fold(&m, "t40"), "already open");
    assert_eq!(st.is_folded(&m, "t40"), Some(false));
    assert!(st.close_fold(&m, "t40"));
    assert!(!st.close_fold(&m, "t40"));
    assert_eq!(st.is_folded(&m, "t40"), Some(true));
    assert!(!st.open_fold(&m, "m1"));
}

#[test]
fn reveal_text_centres_the_row_that_holds_the_character() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    // Block 30 reads "line 30 needle": "line" and "30" come first (6 characters), so the 7th character is the "n".
    st.reveal_text("m30", 6);
    let buf = draw(&m, &mut st, AREA);
    assert!(shows(&buf, "line 30 needle"), "{:?}", lines(&buf));
    let at = lines(&buf)
        .iter()
        .position(|l| l.contains("line 30 needle"))
        .unwrap();
    assert!((3..=6).contains(&at), "row {at} is near the middle of 10");
}

#[test]
fn reveal_text_finds_the_wrapped_row_and_clamps_past_the_end() {
    let words: Vec<String> = (0..40).map(|i| format!("w{i:02}")).collect();
    let m = model(vec![msg(0, &words.join(" "))]);
    let area = Rect::new(0, 0, 30, 4);
    let mut st = TranscriptState::new();
    draw(&m, &mut st, area);
    st.scroll_to_line(0);
    draw(&m, &mut st, area);
    // Each word has three non-blank characters; word 30 starts at character 90.
    st.reveal_text("m0", 90);
    let buf = draw(&m, &mut st, area);
    assert!(shows(&buf, "w30"), "{:?}", lines(&buf));
    st.reveal_text("m0", 1_000_000);
    let buf = draw(&m, &mut st, area);
    assert!(shows(&buf, "w39"), "the last row: {:?}", lines(&buf));
    st.reveal_text("nope", 0);
    draw(&m, &mut st, area);
}

fn marks_of(id: &str, spans: &[(usize, usize, bool)]) -> HashMap<String, Vec<Mark>> {
    let mut h = HashMap::new();
    h.insert(
        id.to_string(),
        spans
            .iter()
            .map(|&(nb_start, nb_len, current)| Mark {
                nb_start,
                nb_len,
                current,
            })
            .collect(),
    );
    h
}

fn bg_at(buf: &Buffer, x: u16, y: u16) -> Option<ratatui::style::Color> {
    buf[(x, y)].style().bg
}

#[test]
fn marks_paint_the_cells_of_the_match_and_nothing_else() {
    let m = model(vec![msg(0, "hello needle world needle")]);
    let area = Rect::new(0, 0, 50, 3);
    let mut st = TranscriptState::new();
    // "hello" is 5 characters; "needle" 6; "world" 5 more.
    st.set_marks(marks_of("m0", &[(5, 6, true), (16, 6, false)]));
    let buf = draw(&m, &mut st, area);
    let x0 = CONTENT_X;
    assert_eq!(buf[(x0 + 6, 0)].symbol(), "n");
    for x in x0 + 6..x0 + 12 {
        assert_eq!(bg_at(&buf, x, 0), CURRENT_MARK.bg, "x {x}");
    }
    assert_ne!(bg_at(&buf, x0 + 5, 0), CURRENT_MARK.bg, "the space before");
    assert_ne!(bg_at(&buf, x0 + 12, 0), CURRENT_MARK.bg, "the space after");
    for x in x0 + 19..x0 + 25 {
        assert_eq!(bg_at(&buf, x, 0), OTHER_MARK.bg, "x {x}");
    }
    assert_ne!(CURRENT_MARK.bg, OTHER_MARK.bg);
    st.clear_marks();
    let buf = draw(&m, &mut st, area);
    assert_ne!(bg_at(&buf, x0 + 6, 0), CURRENT_MARK.bg);
}

#[test]
fn a_mark_the_wrap_cut_in_two_is_painted_on_both_rows() {
    let m = model(vec![msg(0, "aaaa bbbb cccc dddd")]);
    // Ten text columns: "aaaa bbbb" / "cccc dddd".
    let area = Rect::new(0, 0, CONTENT_X + 11, 3);
    let mut st = TranscriptState::new();
    st.set_marks(marks_of("m0", &[(4, 8, false)]));
    let buf = draw(&m, &mut st, area);
    assert!(lines(&buf)[0].ends_with("aaaa bbbb"), "{:?}", lines(&buf));
    assert_eq!(lines(&buf)[1].trim_start(), "cccc dddd");
    let x0 = CONTENT_X;
    for x in x0 + 5..x0 + 9 {
        assert_eq!(bg_at(&buf, x, 0), OTHER_MARK.bg, "row 0 x {x}");
    }
    for x in x0..x0 + 4 {
        assert_eq!(bg_at(&buf, x, 1), OTHER_MARK.bg, "row 1 x {x}");
    }
    assert_ne!(bg_at(&buf, x0 + 4, 0), OTHER_MARK.bg, "the space");
    assert_ne!(bg_at(&buf, x0 + 5, 1), OTHER_MARK.bg, "dddd is not part");
}

#[test]
fn a_mark_over_wide_characters_paints_every_cell() {
    let m = model(vec![msg(0, "日本語 needle")]);
    let area = Rect::new(0, 0, 40, 3);
    let mut st = TranscriptState::new();
    st.set_marks(marks_of("m0", &[(1, 2, false)]));
    let buf = draw(&m, &mut st, area);
    // 日 takes columns 0-1, 本 and 語 take 2-5: the marked 本語 are columns 2 to 5.
    let x0 = CONTENT_X;
    for x in x0 + 2..x0 + 6 {
        assert_eq!(bg_at(&buf, x, 0), OTHER_MARK.bg, "x {x}");
    }
    assert_ne!(bg_at(&buf, x0, 0), OTHER_MARK.bg);
    assert_ne!(bg_at(&buf, x0 + 6, 0), OTHER_MARK.bg);
}

#[test]
fn marks_count_the_head_before_the_body_of_an_open_block() {
    let m = model(vec![Block::tool_call(
        "t1",
        "run tests",
        Some("execute"),
        Some("completed"),
        "alpha beta",
    )]);
    let area = Rect::new(0, 0, 50, 4);
    let mut st = TranscriptState::new();
    st.open_fold(&m, "t1");
    // The head has 8 non-blank characters, "alpha" 5: "beta" starts at 13.
    st.set_marks(marks_of("t1", &[(13, 4, true)]));
    let buf = draw(&m, &mut st, area);
    assert_eq!(lines(&buf)[1].trim_start(), "alpha beta");
    let x0 = CONTENT_X;
    for x in x0 + 6..x0 + 10 {
        assert_eq!(bg_at(&buf, x, 1), CURRENT_MARK.bg, "x {x}");
    }
    assert_ne!(bg_at(&buf, x0, 1), CURRENT_MARK.bg);
    assert_ne!(bg_at(&buf, x0, 0), CURRENT_MARK.bg, "the head row");
}

// ---- search over the transcript -----------------------------------------------------------------------------------------------------

#[test]
fn cancelling_puts_the_scroll_position_back() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(20);
    draw(&m, &mut st, AREA);
    assert_eq!(st.top_line(), 20);
    assert!(!st.is_following());

    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "needle");
    let moved = st.top_line();
    assert_ne!(moved, 20, "the first match is not in view");
    ui.key(Key::Esc, &mut st, &m);
    draw(&m, &mut st, AREA);
    assert_eq!(st.top_line(), 20);
    assert!(!ui.is_active());
}

#[test]
fn cancelling_from_the_bottom_follows_again() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    assert!(st.is_following());
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "needle");
    assert!(
        !st.is_following(),
        "a match above the end pauses follow mode"
    );
    ui.key(Key::Esc, &mut st, &m);
    draw(&m, &mut st, AREA);
    assert!(st.is_following());
}

#[test]
fn a_view_at_the_bottom_starts_at_the_newest_match_and_n_walks_back_round() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "needle");
    let buf = draw(&m, &mut st, AREA);
    assert!(shows(&buf, "line 50 needle"), "{:?}", lines(&buf));
    ui.key(Key::Enter, &mut st, &m);
    assert_eq!(ui.search().phase(), Phase::Active);

    // n goes forward and wraps to the first match; each stop is on screen.
    for want in ["line 10 needle", "line 30 needle", "line 50 needle"] {
        ui.next(&mut st, &m);
        let buf = draw(&m, &mut st, AREA);
        assert!(shows(&buf, want), "{want}: {:?}", lines(&buf));
    }
    ui.prev(&mut st, &m);
    let buf = draw(&m, &mut st, AREA);
    assert!(shows(&buf, "line 30 needle"));
}

#[test]
fn the_current_match_wears_the_current_mark_and_the_others_the_other_mark() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "line");
    // Every message starts with "line": the visible ones are marked, one of them as the current match.
    let buf = draw(&m, &mut st, AREA);
    let cur = (0..buf.area.height)
        .filter(|&y| bg_at(&buf, CONTENT_X, y) == CURRENT_MARK.bg)
        .count();
    let other = (0..buf.area.height)
        .filter(|&y| bg_at(&buf, CONTENT_X, y) == OTHER_MARK.bg)
        .count();
    assert_eq!(cur, 1);
    assert!(other >= 2, "other matches on screen: {other}");
    // Accepting keeps the marks; Esc clears them.
    ui.key(Key::Enter, &mut st, &m);
    let buf = draw(&m, &mut st, AREA);
    assert!((0..buf.area.height).any(|y| bg_at(&buf, CONTENT_X, y) == CURRENT_MARK.bg));
    ui.key(Key::Esc, &mut st, &m);
    let buf = draw(&m, &mut st, AREA);
    assert!(!(0..buf.area.height).any(|y| {
        let b = bg_at(&buf, CONTENT_X, y);
        b == CURRENT_MARK.bg || b == OTHER_MARK.bg
    }));
}

#[test]
fn a_match_in_a_folded_block_opens_it_and_cancel_closes_it_again() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    assert_eq!(st.is_folded(&m, "t40"), Some(true));

    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "deep inside");
    assert_eq!(st.is_folded(&m, "t40"), Some(false));
    let buf = draw(&m, &mut st, AREA);
    assert!(
        shows(&buf, "found the deep inside marker here"),
        "{:?}",
        lines(&buf)
    );

    ui.key(Key::Esc, &mut st, &m);
    assert_eq!(
        st.is_folded(&m, "t40"),
        Some(true),
        "cancel closes what the search opened"
    );
    draw(&m, &mut st, AREA);
    assert_eq!(st.top_line(), 0);
}

#[test]
fn accepting_keeps_the_block_open_and_a_block_the_user_had_open_stays_open() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);

    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "deep inside");
    ui.key(Key::Enter, &mut st, &m);
    ui.key(Key::Esc, &mut st, &m);
    assert_eq!(
        st.is_folded(&m, "t40"),
        Some(false),
        "accepted: the user read it there"
    );

    // Now the user holds the block open himself; a cancelled search must not close it.
    let mut ui = ChatSearchUi::new();
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "marker");
    ui.key(Key::Esc, &mut st, &m);
    assert_eq!(st.is_folded(&m, "t40"), Some(false));
}

#[test]
fn a_new_block_while_searching_updates_the_marks_without_moving_the_view() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "needle");
    ui.key(Key::Enter, &mut st, &m);
    let top = st.top_line();
    let mut blocks = m.blocks.clone();
    blocks.push(msg(99, "another needle arrives"));
    let m2 = model(blocks);
    ui.model_changed(&mut st, &m2);
    draw(&m2, &mut st, AREA);
    assert_eq!(st.top_line(), top);
    assert_eq!(ui.search().hits().len(), 4);
}

#[test]
fn mark_of_reads_the_scroll_state_and_the_block_at_the_top() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    let at_end = mark_of(&st, AREA);
    assert!(at_end.following);
    st.scroll_to_line(23);
    draw(&m, &mut st, AREA);
    st.select_next(&m);
    let mid = mark_of(&st, AREA);
    assert!(!mid.following);
    assert_eq!(mid.top_line, 23);
    assert_eq!(mid.anchor, Some(23), "one row per block up to block 23");
    assert_eq!(mid.selected.as_deref(), Some("m0"));
}

#[test]
fn the_bar_shows_the_query_and_the_position_in_one_row() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "needle");
    let area = Rect::new(0, 0, 30, 1);
    let mut buf = Buffer::empty(area);
    SearchBar::new(ui.search()).render(area, &mut buf);
    let row: String = (0..30).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    assert_eq!(row.trim_end(), "/needle  3/3");
    // Closed: nothing is drawn.
    ui.key(Key::Esc, &mut st, &m);
    let mut buf = Buffer::empty(area);
    SearchBar::new(ui.search()).render(area, &mut buf);
    let row: String = (0..30).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    assert_eq!(row.trim(), "");
}

// ---- found by the second-opinion review ----------------------------------------------

#[test]
fn a_jump_that_has_not_been_drawn_yet_does_not_beat_a_cancel() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(20);
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    // Two keys before the next draw: a match (a jump is queued), then a query with none (a restore).
    ui.key(Key::Char('n'), &mut st, &m);
    ui.key(Key::Char('q'), &mut st, &m);
    ui.key(Key::Esc, &mut st, &m);
    draw(&m, &mut st, AREA);
    assert_eq!(st.top_line(), 20);
    assert!(!st.is_following());
}

#[test]
fn a_match_in_the_head_of_a_folded_block_opens_it_too() {
    let m = long();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "run_commands");
    assert_eq!(
        st.is_folded(&m, "t40"),
        Some(false),
        "the head may be cut at the fold; the whole head is shown open"
    );
    let buf = draw(&m, &mut st, AREA);
    assert!(shows(&buf, "run_commands: ls"));
}

/// Two folded tool calls with "needle" in their bodies, each among enough messages that the view can be scrolled to the top and is not
/// following the end.
fn two_tools() -> ChatModel {
    let tool = |id: &str, body: &str| {
        Block::tool_call(
            id,
            "run_commands: ls",
            Some("execute"),
            Some("completed"),
            body,
        )
    };
    let mut blocks = Vec::new();
    for i in 0..15 {
        blocks.push(msg(i, &format!("filler {i}")));
    }
    blocks.push(tool("t1", "one\nneedle in the first\nthree"));
    for i in 16..31 {
        blocks.push(msg(i, &format!("filler {i}")));
    }
    blocks.push(tool("t2", "four\nneedle in the second\nsix"));
    for i in 32..47 {
        blocks.push(msg(i, &format!("filler {i}")));
    }
    model(blocks)
}

#[test]
fn accepting_makes_the_open_block_the_users_and_later_steps_close_only_their_own() {
    let m = two_tools();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "needle");
    assert_eq!(
        st.is_folded(&m, "t1"),
        Some(false),
        "the first match opened its block"
    );
    ui.key(Key::Enter, &mut st, &m);
    // Stepping on opens the next block; the accepted one stays open for good.
    ui.next(&mut st, &m);
    draw(&m, &mut st, AREA);
    assert_eq!(st.is_folded(&m, "t1"), Some(false));
    assert_eq!(st.is_folded(&m, "t2"), Some(false));
    // Stepping back closes only the block the search opened after the accept.
    ui.prev(&mut st, &m);
    draw(&m, &mut st, AREA);
    assert_eq!(st.is_folded(&m, "t2"), Some(true));
    assert_eq!(st.is_folded(&m, "t1"), Some(false));
}

#[test]
fn opening_the_bar_again_while_typing_closes_the_block_the_first_one_opened() {
    let m = two_tools();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "needle in the second");
    assert_eq!(st.is_folded(&m, "t2"), Some(false));
    ui.open(&mut st, &m, AREA);
    assert_eq!(st.is_folded(&m, "t2"), Some(true));
    assert_eq!(ui.search().query(), "");
}

#[test]
fn leaving_the_chat_clears_the_marks_and_any_queued_jump_and_keeps_the_folds() {
    let m = two_tools();
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    let mut ui = ChatSearchUi::new();
    ui.open(&mut st, &m, AREA);
    type_str(&mut ui, &mut st, &m, "needle");
    ui.key(Key::Enter, &mut st, &m);
    ui.next(&mut st, &m);
    ui.close(&mut st);
    assert!(!ui.is_active());
    let top = st.top_line();
    let buf = draw(&m, &mut st, AREA);
    assert_eq!(st.top_line(), top, "the queued jump is gone");
    assert!(!(0..buf.area.height).any(|y| {
        let b = bg_at(&buf, CONTENT_X, y);
        b == CURRENT_MARK.bg || b == OTHER_MARK.bg
    }));
}

#[test]
fn the_block_on_top_is_found_below_the_progress_feed_label() {
    let m = ChatModel::from_blocks(
        Source::ProgressFeed,
        "cline",
        vec![msg(0, "first"), msg(1, "second")],
    );
    assert!(m.label().is_some());
    let mut st = TranscriptState::new();
    draw(&m, &mut st, AREA);
    st.scroll_to_line(0);
    draw(&m, &mut st, AREA);
    assert_eq!(
        mark_of(&st, AREA).anchor,
        Some(0),
        "row 0 is the label, row 1 is block 0"
    );
}

/// The non-blank characters of the rows the widget drew for block text, in order (the gutter, the fold column and the scrollbar left out).
fn drawn_nonblank(buf: &Buffer, cw: u16) -> String {
    let a = buf.area;
    let mut out = String::new();
    for y in a.y..a.bottom() {
        for x in CONTENT_X..CONTENT_X + cw {
            out.push_str(buf[(x, y)].symbol());
        }
    }
    out.chars().filter(|c| !c.is_whitespace()).collect()
}

fn nonblank_of(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn the_non_blank_characters_of_the_rows_are_those_of_the_text_whatever_the_width_and_the_kind() {
    use ham_core::sanitize::{display_line, display_text};
    let long_line = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon";
    let blocks = vec![
        Block::message(
            "m",
            Role::Engine,
            Origin::Engine,
            &format!("{long_line}\n\n\tindented\ttabs and  double  spaces\n{long_line}"),
        ),
        Block::thought("th", "think about \u{200b}this\u{202e} and <that>"),
        Block::diff(
            "d",
            "src/lib.rs",
            "@@ -1,2 +1,2 @@\n-old line here\n+new line here with more words to wrap around the edge",
        ),
        Block::unknown(
            "u",
            "weird kind",
            "{\"a\": [1, 2, 3], \"b\": \"some longer text to wrap\"}",
        ),
        Block::plan(
            "p",
            &[
                ("completed", "write the failing test"),
                ("in_progress", "make it pass with a long description"),
                ("pending", "refactor"),
            ],
        ),
        Block::tool_call(
            "tc",
            "run_commands: pytest -q --maxfail=1 tests/test_api.py::test_create_with_a_long_name",
            Some("execute"),
            None,
            "collected 3 items\nFAILED test_api.py::test_create - AssertionError: expected 3 got 2",
        ),
    ];
    for b in blocks {
        for w in [14u16, 20, 31, 47, 80] {
            let m = model(vec![b.clone()]);
            let mut st = TranscriptState::new();
            st.open_fold(&m, &b.id);
            let area = Rect::new(0, 0, w, 200);
            let mut buf = Buffer::empty(area);
            Transcript::new(&m).render(area, &mut buf, &mut st);
            let cw = w - CONTENT_X - 1;
            let want = nonblank_of(&format!(
                "{}{}",
                display_line(&b.head),
                display_text(&b.body)
            ));
            assert_eq!(drawn_nonblank(&buf, cw), want, "{} at width {w}", b.id);
        }
    }
}
