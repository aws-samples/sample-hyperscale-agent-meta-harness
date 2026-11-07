//! The transcript widget of the run chat view. It draws a `ChatModel` into a `Rect`: a role gutter, a fold marker, wrapped
//! text, a right-aligned tool status, a scrollbar column, follow mode with a "Jump to bottom" row, and a wrap cache keyed by block, width
//! and fold state. Layout work is counted, never timed. Text from the engine is content: it is drawn right of the gutter, goes through
//! the sanitizer again at layout, and cannot draw a role word, a border or a prompt.
#![allow(clippy::unwrap_used)]

use ham::tui::transcript::{CONTENT_X, GUTTER, Transcript, TranscriptState, wrap_text};
use ham_core::chat::{
    Block, ChatInputs, ChatModel, GapCause, Origin, PROGRESS_FEED_LABEL, Role, Source,
};
use ham_core::notice::NoticeKind;
use ham_core::state::RunState;
use harness_protocol::envelope::Envelope;
use proptest::prelude::*;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::StatefulWidget;
use ratatui::{Terminal, TerminalOptions, Viewport};

const RECORDED: &str = include_str!("../../ham-core/tests/fixtures/tool-events-recorded.ndjson");

fn msg(i: usize, text: &str) -> Block {
    Block::message(&format!("m{i}"), Role::Engine, Origin::Engine, text)
}

/// The person's own words: drawn as written, where an engine message is Markdown.
fn said(i: usize, text: &str) -> Block {
    Block::message(&format!("u{i}"), Role::You, Origin::User, text)
}

fn model(blocks: Vec<Block>) -> ChatModel {
    ChatModel::from_blocks(Source::FullHistory, "cline", blocks)
}

fn render_in(m: &ChatModel, s: &mut TranscriptState, area: Rect, buf: &mut Buffer) {
    Transcript::new(m).render(area, buf, s);
}

fn render(m: &ChatModel, s: &mut TranscriptState, w: u16, h: u16) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    render_in(m, s, area, &mut buf);
    buf
}

/// Every row of the buffer, trailing spaces cut, scrollbar column included.
fn rows(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (a.y..a.bottom())
        .map(|y| {
            (a.x..a.right())
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The rows without the last column (the scrollbar's), trailing spaces cut.
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

fn text(buf: &Buffer) -> String {
    rows(buf).join("\n") + "\n"
}

/// The first row of a block: the role word in the gutter, the empty fold column, then the text.
fn first(word: &str, t: &str) -> String {
    format!("{word:<9}{t}")
}

/// A row that continues a block: the gutter and the fold column are empty.
fn cont(t: &str) -> String {
    format!("{:9}{t}", "")
}

/// A tool row of a view `w` columns wide: the marker in column 7, the head from column 9, the status ending in column `w - 2`.
fn tool_line(marker: char, head: &str, status: &str, w: usize) -> String {
    let left = format!("{:<7}{marker} {head}", "tool");
    let pad = (w - 1) - left.chars().count() - status.chars().count();
    format!("{left}{}{status}", " ".repeat(pad))
}

#[test]
fn the_role_word_sits_in_the_gutter_and_the_text_starts_at_the_content_column() {
    assert_eq!((GUTTER, CONTENT_X), (7, 9));
    let m = model(vec![
        Block::message("p", Role::You, Origin::User, "Build a todo API with tests"),
        msg(1, "I'll start by reading the repo."),
        Block::notice(
            "n",
            NoticeKind::ProviderRetry,
            "Provider error (temporary): try 1 of 4 in 5 s.",
        ),
    ]);
    let mut s = TranscriptState::new();
    let r = lines(&render(&m, &mut s, 60, 6));
    assert_eq!(r[0], first("you", "Build a todo API with tests"));
    assert_eq!(r[1], first("cline", "I'll start by reading the repo."));
    assert_eq!(
        r[2],
        first("notice", "Provider error (temporary): try 1 of 4 in 5 s.")
    );
}

#[test]
fn a_long_message_wraps_at_the_content_width_and_continuation_rows_have_an_empty_gutter() {
    let m = model(vec![msg(
        1,
        "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau",
    )]);
    let mut s = TranscriptState::new();
    // 40 wide: one column for the scrollbar, nine for the gutter, 30 for text.
    let r = lines(&render(&m, &mut s, 40, 6));
    assert_eq!(r[0], first("cline", "alpha beta gamma delta epsilon"));
    assert_eq!(r[1], cont("zeta eta theta iota kappa"));
    assert_eq!(r[2], cont("lambda mu nu xi omicron pi rho"));
    assert_eq!(r[3], cont("sigma tau"));
    for line in &r {
        assert!(line.chars().count() <= 39);
    }
}

#[test]
fn a_tool_row_shows_its_title_and_a_right_aligned_status_and_no_marker_when_it_has_no_body() {
    let m = model(vec![
        Block::tool_call(
            "t1",
            "run_commands: pytest -q",
            Some("execute"),
            Some("completed"),
            "",
        ),
        Block::tool_call(
            "t2",
            "editor: src/todo/api.py",
            Some("edit"),
            Some("failed"),
            "",
        ),
        Block::tool_call(
            "t3",
            "search_codebase: vault",
            Some("search"),
            Some("in_progress"),
            "",
        ),
        Block::tool_call("t4", "read_files", Some("read"), Some("pending"), ""),
    ]);
    let mut s = TranscriptState::new();
    let r = lines(&render(&m, &mut s, 50, 6));
    assert_eq!(r[0], tool_line(' ', "run_commands: pytest -q", "done", 50));
    assert_eq!(
        r[1],
        tool_line(' ', "editor: src/todo/api.py", "failed", 50)
    );
    assert_eq!(
        r[2],
        tool_line(' ', "search_codebase: vault", "running", 50)
    );
    assert_eq!(r[3], tool_line(' ', "read_files", "pending", 50));
}

#[test]
fn a_tool_title_that_is_too_long_is_cut_with_an_ellipsis_and_keeps_its_status() {
    let m = model(vec![Block::tool_call(
        "t1",
        "run_commands: cd /mnt/workspace/01M50ECFDK08WB7MQE1TBHBZE4/direct/repo && git log --oneline | head; git status",
        Some("execute"),
        Some("completed"),
        "",
    )]);
    let mut s = TranscriptState::new();
    let r = lines(&render(&m, &mut s, 50, 3));
    assert!(
        r[0].starts_with("tool     run_commands: cd /mnt/workspace/"),
        "{}",
        r[0]
    );
    assert!(r[0].ends_with("\u{2026} done"), "{}", r[0]);
    assert_eq!(r[0].chars().count(), 49);
}

#[test]
fn folding_shows_the_body_under_the_head_and_a_click_on_the_marker_does_the_same() {
    let m = model(vec![
        Block::tool_call(
            "t1",
            "run_commands: pytest -q",
            Some("execute"),
            Some("completed"),
            "3 passed\n0 failed",
        ),
        msg(2, "Tests pass."),
    ]);
    let mut s = TranscriptState::new();
    let folded = lines(&render(&m, &mut s, 50, 6));
    assert_eq!(
        folded[0],
        tool_line('\u{25b8}', "run_commands: pytest -q", "done", 50)
    );
    assert_eq!(folded[1], first("cline", "Tests pass."));
    s.toggle_fold(&m, "t1");
    let open = lines(&render(&m, &mut s, 50, 6));
    assert_eq!(
        open[0],
        tool_line('\u{25be}', "run_commands: pytest -q", "done", 50)
    );
    assert_eq!(open[1], cont("3 passed"));
    assert_eq!(open[2], cont("0 failed"));
    assert_eq!(open[3], first("cline", "Tests pass."));
    s.toggle_fold(&m, "t1");
    assert_eq!(
        lines(&render(&m, &mut s, 50, 6))[1],
        first("cline", "Tests pass.")
    );
    // The click: a press on the marker cell toggles; a press on the text of a message does nothing.
    let hit = s.hit(7, 0).expect("the marker cell is a hit");
    assert_eq!((hit.block_id.as_str(), hit.on_fold_marker), ("t1", true));
    assert!(s.click(&m, 7, 0));
    assert_eq!(lines(&render(&m, &mut s, 50, 6))[1], cont("3 passed"));
    let msg_hit = s.hit(12, 3).expect("the message row is a hit");
    assert_eq!(
        (msg_hit.block_id.as_str(), msg_hit.on_fold_marker),
        ("m2", false)
    );
    assert!(!s.click(&m, 12, 3));
    assert_eq!(s.hit(12, 20), None, "below the content there is nothing");
}

#[test]
fn a_message_without_a_head_has_nothing_to_fold() {
    let m = model(vec![msg(1, "line one\nline two")]);
    let mut s = TranscriptState::new();
    render(&m, &mut s, 40, 4);
    s.toggle_fold(&m, "m1");
    let r = lines(&render(&m, &mut s, 40, 4));
    assert_eq!(r[0], first("cline", "line one"));
    assert_eq!(r[1], cont("line two"));
}

#[test]
fn follow_mode_shows_the_newest_blocks_and_scrolling_up_pauses_it_with_a_jump_row() {
    let blocks: Vec<Block> = (0..100).map(|i| msg(i, &format!("block {i:03}"))).collect();
    let m = model(blocks);
    let mut s = TranscriptState::new();
    let r = lines(&render(&m, &mut s, 40, 10));
    assert!(s.is_following());
    assert_eq!(r[9], first("cline", "block 099"));
    assert_eq!(r[0], first("cline", "block 090"));
    s.scroll_up(5);
    assert!(!s.is_following());
    let r = rows(&render(&m, &mut s, 40, 10));
    assert_eq!(
        r[0].trim_end_matches(['\u{2502}', '\u{2503}']).trim_end(),
        first("cline", "block 085")
    );
    assert!(r[9].ends_with("[ Jump to bottom \u{2193} ]"), "{}", r[9]);
    // Three blocks arrive while the view is paused.
    let more: Vec<Block> = (0..103).map(|i| msg(i, &format!("block {i:03}"))).collect();
    let m2 = model(more);
    let r = rows(&render(&m2, &mut s, 40, 10));
    assert_eq!(
        r[0].trim_end_matches(['\u{2502}', '\u{2503}']).trim_end(),
        first("cline", "block 085"),
        "the view does not move"
    );
    assert!(
        r[9].ends_with("[ Jump to bottom \u{2193} 3 new ]"),
        "{}",
        r[9]
    );
    assert_eq!(s.new_blocks(), 3);
    s.to_bottom();
    assert!(s.is_following());
    let r = lines(&render(&m2, &mut s, 40, 10));
    assert_eq!(r[9], first("cline", "block 102"));
    assert!(!r[9].contains("Jump"));
    assert_eq!(s.new_blocks(), 0);
}

#[test]
fn scrolling_down_to_the_end_by_hand_follows_again() {
    let m = model((0..40).map(|i| msg(i, &format!("block {i:02}"))).collect());
    let mut s = TranscriptState::new();
    render(&m, &mut s, 40, 10);
    s.scroll_up(12);
    render(&m, &mut s, 40, 10);
    assert!(!s.is_following());
    s.scroll_down(1000);
    render(&m, &mut s, 40, 10);
    assert!(s.is_following());
    s.page_up();
    render(&m, &mut s, 40, 10);
    assert!(!s.is_following());
    s.to_top();
    let r = lines(&render(&m, &mut s, 40, 10));
    assert_eq!(r[0], first("cline", "block 00"));
    assert_eq!(s.top_line(), 0);
}

#[test]
fn a_view_that_fits_never_scrolls_and_shows_no_scrollbar_or_jump_row() {
    let m = model(vec![msg(1, "one"), msg(2, "two")]);
    let mut s = TranscriptState::new();
    let buf = render(&m, &mut s, 30, 8);
    s.scroll_up(5);
    let again = render(&m, &mut s, 30, 8);
    assert_eq!(text(&buf), text(&again));
    assert!(!text(&again).contains("Jump"));
    assert_eq!(buf[(29, 0)].symbol(), " ");
}

#[test]
fn the_scrollbar_column_and_the_range_text_show_where_the_view_is() {
    let m = model((0..100).map(|i| msg(i, &format!("block {i:03}"))).collect());
    let mut s = TranscriptState::new();
    let buf = render(&m, &mut s, 40, 10);
    assert_eq!(s.range_text(), "lines 91-100 of 100");
    let thumb: Vec<&str> = (0..10).map(|y| buf[(39, y)].symbol()).collect();
    assert_eq!(
        thumb.iter().filter(|c| **c == "\u{2503}").count(),
        1,
        "{thumb:?}"
    );
    assert_eq!(thumb[9], "\u{2503}", "following sits at the bottom");
    s.to_top();
    let buf = render(&m, &mut s, 40, 10);
    assert_eq!(s.range_text(), "lines 1-10 of 100");
    assert_eq!(buf[(39, 0)].symbol(), "\u{2503}");
    assert_eq!(buf[(39, 8)].symbol(), "\u{2502}", "the track");
    assert_eq!(
        buf[(39, 9)].symbol(),
        "]",
        "the jump row covers the last cell of the bar"
    );
}

#[test]
fn a_100000_line_transcript_scrolls_to_any_offset() {
    // 2,500 blocks of 40 lines: offsets past 65,535 must work (a ratatui Paragraph scroll is 16 bits).
    let blocks: Vec<Block> = (0..2500)
        .map(|i| {
            let body: Vec<String> = (0..40).map(|j| format!("b{i:04} l{j:02}")).collect();
            msg(i, &body.join("\n"))
        })
        .collect();
    let m = model(blocks);
    let mut s = TranscriptState::new();
    render(&m, &mut s, 60, 24);
    assert_eq!(s.total_lines(), 100_000);
    s.scroll_to_line(70_000);
    let r = lines(&render(&m, &mut s, 60, 24));
    assert_eq!(r[0], first("cline", "b1750 l00"));
    assert_eq!(r[1], cont("b1750 l01"));
    assert!(r[23].starts_with(&cont("b1750 l23")), "{}", r[23]);
    assert_eq!(s.top_line(), 70_000);
    s.scroll_to_line(99_990);
    let r = lines(&render(&m, &mut s, 60, 24));
    assert_eq!(r[23], cont("b2499 l39"), "the last line sits at the bottom");
    assert!(
        s.is_following(),
        "the end of the transcript is follow mode again"
    );
}

#[test]
fn resizing_rewraps_and_loses_no_word() {
    let words: Vec<String> = (0..60).map(|i| format!("w{i:02}")).collect();
    let m = model(vec![msg(1, &words.join(" "))]);
    let mut s = TranscriptState::new();
    let wide = lines(&render(&m, &mut s, 100, 30));
    let narrow = lines(&render(&m, &mut s, 30, 30));
    let count = |r: &[String]| r.iter().filter(|l| !l.is_empty()).count();
    assert!(count(&narrow) > count(&wide));
    let got = |r: &[String]| -> Vec<String> {
        r.iter()
            .flat_map(|l| l.split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .filter(|w| w != "cline")
            .collect()
    };
    assert_eq!(got(&wide), words);
    assert_eq!(got(&narrow), words);
}

#[test]
fn layout_work_is_counted_and_a_scroll_or_an_append_rewraps_only_what_changed() {
    // 499 blocks that never change, then a tail block and, later, one appended block.
    let mk = |tail: &str, extra: bool| -> ChatModel {
        let mut blocks: Vec<Block> = (0..499)
            .map(|i| msg(i, &format!("block {i}\nsecond line of {i}\nthird line")))
            .collect();
        blocks.push(Block::message("tail", Role::Engine, Origin::Engine, tail));
        if extra {
            blocks.push(Block::message(
                "extra",
                Role::Engine,
                Origin::Engine,
                "appended",
            ));
        }
        model(blocks)
    };
    let m = mk("the last block", false);
    let mut s = TranscriptState::new();
    render(&m, &mut s, 120, 40);
    assert_eq!(s.wrap_computations(), 500, "each block is laid out once");
    render(&m, &mut s, 120, 40);
    assert_eq!(s.wrap_computations(), 500, "a second draw reuses the cache");
    s.scroll_up(30);
    render(&m, &mut s, 120, 40);
    s.scroll_up(300);
    render(&m, &mut s, 120, 40);
    assert_eq!(s.wrap_computations(), 500, "scrolling lays out nothing");
    // A block is appended.
    let m2 = mk("the last block", true);
    render(&m2, &mut s, 120, 40);
    assert_eq!(s.wrap_computations(), 501);
    // The newest block grows (a streaming message): only it is laid out again.
    let m3 = mk("the last block, now with a good deal more text in it", true);
    render(&m3, &mut s, 120, 40);
    assert_eq!(s.wrap_computations(), 502);
    // Another width lays every block out again.
    render(&m3, &mut s, 90, 40);
    assert_eq!(s.wrap_computations(), 502 + 501);
    // A fold change lays out that block only.
    let mut blocks: Vec<Block> = (0..3).map(|i| msg(i, "x")).collect();
    blocks.push(Block::tool_call(
        "t",
        "title",
        None,
        Some("completed"),
        "body",
    ));
    let m4 = model(blocks);
    let mut s2 = TranscriptState::new();
    render(&m4, &mut s2, 50, 10);
    let before = s2.wrap_computations();
    s2.toggle_fold(&m4, "t");
    render(&m4, &mut s2, 50, 10);
    assert_eq!(s2.wrap_computations(), before + 1);
}

#[test]
fn the_progress_feed_label_is_the_first_row_and_a_full_history_has_none() {
    let mut state = RunState::new("01JABCDEFGHJKMNPQRSTVWXYZ0");
    for line in RECORDED.lines() {
        state.apply(serde_json::from_str::<Envelope>(line).unwrap());
    }
    let feed = ChatModel::from_progress_feed(
        &state,
        &ChatInputs {
            prompt: Some("Build a todo API"),
            ..ChatInputs::default()
        },
    );
    let mut s = TranscriptState::new();
    let r = lines(&render(&feed, &mut s, 70, 30));
    assert_eq!(r[0], PROGRESS_FEED_LABEL);
    assert_eq!(r[1], first("you", "Build a todo API"));
    assert!(r.iter().any(|l| l.starts_with("tool   ")), "{r:?}");
    let full = model(vec![msg(1, "hello")]);
    let mut s2 = TranscriptState::new();
    assert_eq!(
        lines(&render(&full, &mut s2, 70, 4))[0],
        first("cline", "hello")
    );
}

#[test]
fn a_gap_is_drawn_as_a_notice_row() {
    let m = model(vec![
        msg(1, "before"),
        Block::gap("g", GapCause::Unreadable, None),
        msg(2, "after"),
    ]);
    let mut s = TranscriptState::new();
    let r = lines(&render(&m, &mut s, 50, 5));
    assert_eq!(r[1], first("notice", "[chunk unreadable]"));
}

#[test]
fn the_selected_block_wears_a_highlighted_gutter_and_keys_move_it() {
    let m = model(vec![
        msg(1, "one"),
        Block::tool_call("t", "title", None, Some("completed"), "body"),
        msg(3, "three"),
    ]);
    let mut s = TranscriptState::new();
    render(&m, &mut s, 40, 6);
    assert_eq!(s.selected_id(), None);
    s.select_next(&m);
    assert_eq!(s.selected_id(), Some("m1"));
    s.select_next(&m);
    assert_eq!(s.selected_id(), Some("t"));
    let buf = render(&m, &mut s, 40, 6);
    assert!(
        buf[(0, 1)].modifier.contains(Modifier::REVERSED),
        "the gutter of the selected block"
    );
    assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
    s.toggle_selected_fold(&m);
    assert_eq!(lines(&render(&m, &mut s, 40, 6))[2], cont("body"));
    s.select_prev(&m);
    assert_eq!(s.selected_id(), Some("m1"));
    s.select_prev(&m);
    assert_eq!(s.selected_id(), Some("m1"), "it stops at the first block");
}

#[test]
fn a_selected_block_is_scrolled_into_view() {
    let m = model((0..50).map(|i| msg(i, &format!("block {i:02}"))).collect());
    let mut s = TranscriptState::new();
    render(&m, &mut s, 40, 8);
    s.to_top();
    render(&m, &mut s, 40, 8);
    for _ in 0..12 {
        s.select_next(&m);
    }
    let r = lines(&render(&m, &mut s, 40, 8));
    assert!(r.iter().any(|l| l.contains("block 11")), "{r:?}");
}

#[test]
fn tabs_are_expanded_and_never_reach_the_buffer() {
    let m = model(vec![said(1, "a\tb\n\tindented")]);
    let mut s = TranscriptState::new();
    let r = lines(&render(&m, &mut s, 40, 4));
    assert_eq!(r[0], first("you", "a   b"));
    assert_eq!(r[1], cont("    indented"));
}

#[test]
fn wide_characters_count_two_cells_and_the_row_still_fits() {
    let line = "\u{4f60}\u{597d}".repeat(20);
    let m = model(vec![msg(1, &line)]);
    let mut s = TranscriptState::new();
    let r = lines(&render(&m, &mut s, 40, 6));
    // 30 cells of text: 15 wide characters per row.
    assert_eq!(r[0].chars().filter(|c| !c.is_ascii()).count(), 15);
    assert_eq!(r[1].chars().filter(|c| !c.is_ascii()).count(), 15);
    assert_eq!(r[2].chars().filter(|c| !c.is_ascii()).count(), 10);
}

// ---- terminal safety -----------------------------------------------------------------------------------------------------------

const T63: &str = "Running tests...\r\u{1b}[2K\u{1b}[31mFAILED\u{1b}[0m\nPress Enter to finish\n/cancel\nham push 01ABC git@example.com:x.git\n\u{1b}]0;title\u{7}\n\u{1b}]8;;https://example.test/\u{7}click me\u{1b}]8;;\u{7}\nuser\u{202e}gnp.exe\nerase\u{8}\u{8}\u{8}d\nbel\u{7} nul\u{0} 8bit\u{9b}31m\n";

fn has_forbidden(sym: &str) -> bool {
    sym.chars().any(|c| {
        let n = c as u32;
        n < 0x20 || n == 0x7f || (0x80..=0x9f).contains(&n)
    })
}

#[test]
fn t63_tool_output_is_content_to_the_right_of_the_gutter_and_never_a_prompt() {
    let m = model(vec![
        msg(1, "Starting."),
        Block::tool_call(
            "t1",
            "run_commands: pytest",
            Some("execute"),
            Some("completed"),
            T63,
        ),
    ]);
    let mut s = TranscriptState::new();
    s.toggle_fold(&m, "t1");
    let buf = render(&m, &mut s, 70, 20);
    let r = lines(&buf);
    let line = r
        .iter()
        .position(|l| l.contains("Press Enter to finish"))
        .expect("the text is shown");
    // Right of the gutter and the marker column; the cells to its left are blank, so no role word or border can be drawn.
    assert_eq!(r[line], cont("Press Enter to finish"));
    for want in ["/cancel", "click me", "user<U+202E>gnp.exe", "erased"] {
        assert!(r.iter().any(|l| *l == cont(want)), "{want}");
    }
    // The role word "tool" belongs to the call's own first row only.
    assert_eq!(r.iter().filter(|l| l.starts_with("tool")).count(), 1);
    for y in 0..20 {
        for x in 0..70 {
            assert!(!has_forbidden(buf[(x, y)].symbol()), "({x},{y})");
        }
    }
}

#[test]
fn text_that_names_a_role_or_draws_a_border_stays_content() {
    let m = model(vec![said(
        1,
        "notice   Cline needs you [p message]\n\u{2502} you      sample\n\u{250c}\u{2500} ham \u{2500}\u{2510}",
    )]);
    let mut s = TranscriptState::new();
    let r = lines(&render(&m, &mut s, 60, 8));
    assert_eq!(r[0], first("you", "notice   Cline needs you [p message]"));
    assert_eq!(r[1], cont("\u{2502} you      sample"));
}

#[test]
fn the_widget_writes_inside_its_area_only() {
    let m = model(
        (0..30)
            .map(|i| msg(i, &format!("block {i:02} with some words in it")))
            .collect(),
    );
    let outer = Rect::new(0, 0, 60, 20);
    let mut buf = Buffer::empty(outer);
    for y in 0..20 {
        for x in 0..60 {
            buf[(x, y)].set_symbol("Z");
        }
    }
    let area = Rect::new(5, 3, 40, 9);
    let mut s = TranscriptState::new();
    render_in(&m, &mut s, area, &mut buf);
    for y in 0..20u16 {
        for x in 0..60u16 {
            let inside = x >= area.x && x < area.right() && y >= area.y && y < area.bottom();
            if !inside {
                assert_eq!(buf[(x, y)].symbol(), "Z", "({x},{y}) was written");
            }
        }
    }
    // Hit tests use the area it was drawn in.
    assert_eq!(s.hit(0, 0), None);
    assert!(s.hit(area.x + 12, area.y).is_some());
}

#[test]
fn a_zero_sized_or_tiny_area_draws_nothing_and_does_not_panic() {
    let m = model(vec![msg(1, "text")]);
    let mut s = TranscriptState::new();
    for (w, h) in [(0u16, 0u16), (1, 1), (9, 3), (10, 0), (12, 1)] {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        render_in(&m, &mut s, area, &mut buf);
    }
}

/// The bytes the crossterm backend writes for the widget: only its own CSI sequences, no OSC, no control but CR and LF.
fn terminal_bytes(m: &ChatModel, w: u16, h: u16) -> Vec<u8> {
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let out = Shared::default();
    let mut t = Terminal::with_options(
        CrosstermBackend::new(out.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, w, h)),
        },
    )
    .unwrap();
    let mut s = TranscriptState::new();
    t.draw(|f| {
        let area = f.area();
        f.render_stateful_widget(Transcript::new(m), area, &mut s);
    })
    .unwrap();
    drop(t);
    let bytes = out.0.lock().unwrap().clone();
    drop(out);
    bytes
}

fn assert_only_backend_sequences(bytes: &[u8]) {
    let text = String::from_utf8(bytes.to_vec()).expect("the backend writes UTF-8");
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let n = c as u32;
        if c == '\u{1b}' {
            assert_eq!(chars.get(i + 1), Some(&'['), "a non-CSI escape at {i}");
            let mut j = i + 2;
            while j < chars.len() && (0x20..=0x3f).contains(&(chars[j] as u32)) {
                j += 1;
            }
            let fin = chars.get(j).copied().unwrap_or('\0');
            assert!(
                matches!(
                    fin,
                    'H' | 'm' | 'J' | 'K' | 'h' | 'l' | 'G' | 'A' | 'B' | 'C' | 'D'
                ),
                "CSI final {fin:?}"
            );
            i = j + 1;
            continue;
        }
        assert!(
            !(n < 0x20 && c != '\r' && c != '\n') && n != 0x7f && !(0x80..=0x9f).contains(&n),
            "control {n:#x} at {i}"
        );
        assert!(
            !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
            "bidi {n:#x}"
        );
        i += 1;
    }
}

#[test]
fn control_text_in_every_field_writes_only_the_backends_sequences() {
    const CONTROL_TEXT: [&str; 8] = [
        "\u{1b}]0;title\u{7}",
        "\u{1b}]8;;https://example.test\u{7}click\u{1b}]8;;\u{7}",
        "\u{1b}P+q544e\u{1b}\\",
        "\u{9b}31m\u{9d}0;t\u{9c}",
        "\u{1b}[2J\u{1b}[H\u{1b}[?1049h",
        "\u{7}\u{8}\u{b}\u{c}\u{7f}",
        "\u{202e}desrever\u{202c}",
        "\u{1b}c\u{1b}(0",
    ];
    for h in CONTROL_TEXT {
        let mut b = Block::tool_call("t", h, Some(h), Some(h), h);
        b.head = h.to_string();
        b.body = h.to_string();
        b.status = Some(h.to_string());
        let mut raw = msg(2, h);
        raw.body = h.to_string();
        let m = model(vec![
            raw,
            b,
            Block::message("p", Role::You, Origin::User, h),
        ]);
        let mut s = TranscriptState::new();
        s.toggle_fold(&m, "t");
        for (w, hh) in [(60, 12), (120, 30)] {
            assert_only_backend_sequences(&terminal_bytes(&m, w, hh));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn no_buffer_cell_holds_a_control_character_for_any_text(s in any::<String>(), t in any::<String>()) {
        let mut tool = Block::tool_call("t", &t, Some(&t), Some(&t), &s);
        tool.head = t.clone();
        tool.body = s.clone();
        let mut raw = msg(1, &s);
        raw.body = s.clone();
        let m = model(vec![raw, tool, Block::message("p", Role::You, Origin::User, &t)]);
        let mut st = TranscriptState::new();
        st.toggle_fold(&m, "t");
        let buf = render(&m, &mut st, 50, 16);
        for y in 0..16u16 {
            for x in 0..50u16 {
                prop_assert!(!has_forbidden(buf[(x, y)].symbol()), "({}, {})", x, y);
            }
        }
    }

    #[test]
    fn wrapped_rows_fit_the_width_and_lose_nothing(s in "[ -~\\t\\n\u{4f60}\u{597d}\u{e9}]{0,300}", w in 4usize..60) {
        let rows = wrap_text(&s, w);
        let width = |r: &str| r.chars().map(|c| if c.is_ascii() || c == '\u{e9}' { 1 } else { 2 }).sum::<usize>();
        for r in &rows {
            prop_assert!(width(r) <= w, "{:?} is wider than {}", r, w);
        }
        let flat = |t: &str| t.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        prop_assert_eq!(flat(&rows.join("\n")), flat(&s));
    }
}

#[test]
fn wrap_text_breaks_at_spaces_keeps_indentation_and_cuts_a_long_word() {
    assert_eq!(wrap_text("one two three", 7), ["one two", "three"]);
    assert_eq!(
        wrap_text("    indented text that wraps", 14),
        ["    indented", "    text that", "    wraps"]
    );
    assert_eq!(wrap_text("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    assert_eq!(wrap_text("a\n\nb", 10), ["a", "", "b"]);
    assert_eq!(wrap_text("", 10), [""]);
    assert_eq!(wrap_text("x", 0), Vec::<String>::new());
    assert_eq!(wrap_text("a\tb", 10), ["a   b"]);
}

// ---- snapshots ------------------------------------------------------------------------------------------------------------------------

fn fixture_blocks() -> Vec<Block> {
    vec![
        Block::message(
            "p",
            Role::You,
            Origin::User,
            "Build a todo API with tests. Use FastAPI and keep the data in memory.",
        ),
        msg(
            1,
            "I'll start by reading the repo and writing the failing tests first. Then I'll add the routes one at a time and run the suite after each.",
        ),
        Block::tool_call(
            "t1",
            "run_commands: pytest -q",
            Some("execute"),
            Some("failed"),
            "FAILED tests/test_api.py::test_create - 404\n1 failed, 0 passed",
        ),
        Block::tool_call(
            "t2",
            "editor: src/todo/api.py",
            Some("edit"),
            Some("completed"),
            "",
        ),
        Block::tool_call(
            "t3",
            "run_commands: pytest -q",
            Some("execute"),
            Some("completed"),
            "3 passed in 0.4s",
        ),
        Block::thought(
            "th",
            "The route needs the list to be shared between requests.",
        ),
        Block::diff(
            "d",
            "src/todo/api.py",
            "@@ -1,3 +1,5 @@\n-todos = None\n+todos: list[dict] = []\n+\n+@app.get(\"/todos\")",
        ),
        Block::plan(
            "pl",
            &[
                ("completed", "write the tests"),
                ("in_progress", "add the routes"),
                ("pending", "run the suite"),
            ],
        ),
        Block::notice(
            "n1",
            NoticeKind::ProviderSwitch,
            "Switched route (switch 1): global.anthropic.claude-sonnet-5-5 to us.anthropic.claude-sonnet-5-5, after a temporary failure.",
        ),
        Block::gap("g", GapCause::Unreadable, None),
        Block::unknown("u", "future_update", "{\"a\":1}"),
        msg(
            2,
            "Tests pass. I added the /todos routes and three tests; the suite passes.",
        ),
        Block::delivery(
            "delivery",
            &[
                ("Outcome".into(), "Cline reported done".into()),
                ("Model".into(), "Claude Sonnet 5.5".into()),
                (
                    "Zip".into(),
                    "/Users/me/Downloads/run.zip (sha256 abababababab, 2048 bytes)".into(),
                ),
            ],
        ),
    ]
}

#[test]
fn snapshots_of_the_fixture_conversation() {
    let m = model(fixture_blocks());
    for (w, h) in [(80u16, 24u16), (120, 40), (200, 60)] {
        let mut s = TranscriptState::new();
        s.toggle_fold(&m, "t1");
        s.toggle_fold(&m, "d");
        let t = text(&render(&m, &mut s, w, h));
        insta::assert_snapshot!(format!("transcript_fixture_{w}x{h}"), t);
    }
}

#[test]
fn snapshot_of_the_scrolled_fixture_with_the_jump_row() {
    let m = model(fixture_blocks());
    let mut s = TranscriptState::new();
    render(&m, &mut s, 80, 10);
    s.scroll_up(7);
    let t = text(&render(&m, &mut s, 80, 10));
    insta::assert_snapshot!("transcript_fixture_scrolled_80x10", t);
}

#[test]
fn snapshot_of_the_progress_feed_of_the_recorded_run() {
    let mut state = RunState::new("01JABCDEFGHJKMNPQRSTVWXYZ0");
    for line in RECORDED.lines() {
        state.apply(serde_json::from_str::<Envelope>(line).unwrap());
    }
    let feed = ChatModel::from_progress_feed(
        &state,
        &ChatInputs {
            prompt: Some("Add percent_change(a, b) to calc.py with tests."),
            ..ChatInputs::default()
        },
    );
    for (w, h) in [(80u16, 24u16), (120, 40)] {
        let mut s = TranscriptState::new();
        let t = text(&render(&feed, &mut s, w, h));
        insta::assert_snapshot!(format!("transcript_feed_{w}x{h}"), t);
    }
}
