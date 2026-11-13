//! The run chat view. `tui::chat` draws the screen of one run into a `Rect`: a header line (run, model, state, elapsed time,
//! tokens, cost, how old the last read is), an optional banner, the transcript,
//! the rows of messages still on their way, the pinned input box, a
//! notice row and one footer line. The box is drawn by whatever implements `InputPane` (the frozen `ChatBox` signatures `rows_at` and
//! `render`); these tests use a double. The module reads no events and holds no `App`: the TUI-core owner wires keys, clicks and the
//! facts (strings the app already has, such as the delivery card with its one-line link) in the branch that replaces the Dashboard.
#![allow(clippy::unwrap_used)]

use ham::tui::chat::{
    Areas, Card, ChatFacts, ChatView, Header, InputPane, PendingRow, PendingStatus, ScrollAction,
    Step, areas,
};
use ham_core::chat::{Block, ChatModel, Origin, Role, Source};
use ham_core::notice::NoticeKind;
use ham_core::render::{LinkLine, link_line};
use proptest::prelude::*;
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::Rect;
use ratatui::widgets::{Block as UiBlock, Paragraph};
use ratatui::{Frame, TerminalOptions, Viewport};
use std::cell::Cell;

/// The input box double: it shows its text and a caret, wraps nothing, and records how it was asked to draw.
#[derive(Default)]
struct FakeBox {
    text: String,
    rendered: Cell<Option<(Rect, bool)>>,
    asked_width: Cell<usize>,
    rows: usize,
}

impl FakeBox {
    fn with(text: &str, rows: usize) -> Self {
        Self {
            text: text.to_string(),
            rows,
            ..Self::default()
        }
    }
}

impl InputPane for FakeBox {
    fn rows_at(&self, inner_width: usize) -> usize {
        self.asked_width.set(inner_width);
        self.rows.max(1)
    }

    fn render(&self, f: &mut Frame, area: Rect, block: UiBlock<'_>, focused: bool) {
        self.rendered.set(Some((area, focused)));
        let caret = if focused { "\u{258c}" } else { "" };
        f.render_widget(
            Paragraph::new(format!("{}{caret}", self.text)).block(block),
            area,
        );
    }
}

fn header() -> Header {
    Header {
        run_id: "01M5287J4KQ8N3X7C9D2E6F1AB".into(),
        model: Some("Claude Sonnet 5.5".into()),
        state: "active".into(),
        elapsed_s: Some(12 * 60 + 40),
        tokens: Some(113_000),
        cost: Some("$0.14".into()),
        as_of_turn: None,
        updated_ago_s: Some(2),
    }
}

const FULL: &str = "ham \u{b7} Run 01M5287J4KQ8N3X7C9D2E6F1AB \u{b7} Claude Sonnet 5.5 \u{b7} active \u{b7} 12m40s \u{b7} 113k tok \u{b7} $0.14 \u{b7} updated 2s ago";

#[test]
fn the_header_line_has_the_run_the_model_the_state_the_time_the_tokens_the_cost_and_the_age() {
    assert_eq!(header().line(200), FULL);
}

#[test]
fn a_narrow_header_gives_up_its_tail_first_and_never_exceeds_the_width() {
    let h = header();
    // Each line is what the width one column short of the line before it leaves: the age goes first, the model last.
    let lines = [
        FULL.to_string(),
        FULL.replace(" \u{b7} updated 2s ago", ""),
        FULL.replace(" \u{b7} updated 2s ago", "")
            .replace(" \u{b7} $0.14", ""),
        FULL.replace(" \u{b7} updated 2s ago", "")
            .replace(" \u{b7} $0.14", "")
            .replace(" \u{b7} 113k tok", ""),
        FULL.replace(" \u{b7} updated 2s ago", "")
            .replace(" \u{b7} $0.14", "")
            .replace(" \u{b7} 113k tok", "")
            .replace(" \u{b7} 12m40s", ""),
        "ham \u{b7} Run 01M5287J4KQ8N3X7C9D2E6F1AB \u{b7} active".to_string(),
    ];
    for (i, want) in lines.iter().enumerate() {
        let w = want.chars().count();
        assert_eq!(&h.line(w), want, "width {w}");
        if let Some(next) = lines.get(i + 1) {
            assert_eq!(&h.line(w - 1), next, "width {}", w - 1);
        }
    }
    for w in 0..120 {
        assert!(h.line(w).chars().count() <= w, "width {w}: {}", h.line(w));
    }
}

#[test]
fn a_header_with_less_to_say_says_less() {
    let h = Header {
        run_id: "01M5287J4KQ8N3X7C9D2E6F1AB".into(),
        state: "starting".into(),
        ..Header::default()
    };
    assert_eq!(
        h.line(100),
        "ham \u{b7} Run 01M5287J4KQ8N3X7C9D2E6F1AB \u{b7} starting"
    );
    let short = Header {
        run_id: "R1".into(),
        state: "active".into(),
        ..Header::default()
    };
    assert_eq!(short.line(100), "ham \u{b7} Run R1 \u{b7} active");
}

#[test]
fn elapsed_time_tokens_and_age_are_written_compactly() {
    let mut h = header();
    for (s, want) in [
        (45, "45s"),
        (60, "1m00s"),
        (12 * 60 + 40, "12m40s"),
        (3600, "1h00m"),
        (3600 + 125, "1h02m"),
    ] {
        h.elapsed_s = Some(s);
        assert!(
            h.line(200).contains(&format!(" \u{b7} {want} \u{b7} ")),
            "{s}: {}",
            h.line(200)
        );
    }
    for (s, want) in [
        (0, "updated 0s ago"),
        (59, "updated 59s ago"),
        (120, "updated 2m ago"),
        (7300, "updated 2h ago"),
    ] {
        h.updated_ago_s = Some(s);
        assert!(h.line(200).ends_with(want), "{s}: {}", h.line(200));
    }
    h.tokens = Some(1_050_000);
    assert!(h.line(200).contains("1.05M tok"));
    h.tokens = Some(950);
    assert!(h.line(200).contains(" 950 tok "));
}

#[test]
fn header_text_from_the_engine_side_is_cleaned() {
    let h = Header {
        run_id: "R\u{1b}]0;t\u{7}1".into(),
        model: Some("m\u{202e}odel".into()),
        state: "act\u{1b}[31mive\n".into(),
        ..Header::default()
    };
    let l = h.line(200);
    assert_eq!(l, "ham \u{b7} Run R1 \u{b7} m<U+202E>odel \u{b7} active");
}

// ---- layout ------------------------------------------------------------------------------------------------------------------------

fn rects(a: &Areas) -> Vec<(&'static str, Rect)> {
    vec![
        ("header", a.header),
        ("banner", a.banner),
        ("transcript", a.transcript),
        ("pending", a.pending),
        ("input", a.input),
        ("notice", a.notice),
        ("footer", a.footer),
    ]
}

#[test]
fn the_layout_at_80_by_24_is_a_header_the_conversation_a_box_and_a_footer() {
    let a = areas(Rect::new(0, 0, 80, 24), false, 0, 1, false);
    assert_eq!(a.header, Rect::new(0, 0, 80, 1));
    assert_eq!(a.banner.height, 0);
    assert_eq!(a.transcript, Rect::new(0, 1, 80, 19));
    assert_eq!(a.pending.height, 0);
    assert_eq!(a.input, Rect::new(0, 20, 80, 3));
    assert_eq!(a.notice.height, 0);
    assert_eq!(a.footer, Rect::new(0, 23, 80, 1));
}

#[test]
fn a_banner_a_notice_pending_rows_and_a_taller_box_each_take_their_rows_from_the_conversation() {
    let a = areas(Rect::new(0, 0, 100, 30), true, 2, 3, true);
    assert_eq!(a.header.y, 0);
    assert_eq!(a.banner, Rect::new(0, 1, 100, 1));
    assert_eq!(a.transcript.y, 2);
    assert_eq!(a.pending.height, 2);
    assert_eq!(a.input.height, 5);
    assert_eq!(a.notice.height, 1);
    assert_eq!(a.footer, Rect::new(0, 29, 100, 1));
    assert_eq!(a.transcript.bottom(), a.pending.y);
    assert_eq!(a.pending.bottom(), a.input.y);
    assert_eq!(a.input.bottom(), a.notice.y);
    assert_eq!(a.notice.bottom(), a.footer.y);
}

#[test]
fn the_box_never_takes_more_than_a_third_of_the_screen_and_the_conversation_keeps_three_rows() {
    let a = areas(Rect::new(0, 0, 80, 24), false, 0, 40, false);
    assert!(a.input.height <= 24 / 3 + 2, "{}", a.input.height);
    assert!(a.transcript.height >= 3);
    // A short screen drops pending rows, then shrinks the box to one row, then the banner, before it takes the conversation's rows.
    let small = areas(Rect::new(0, 0, 60, 8), true, 3, 4, true);
    assert!(small.transcript.height >= 1, "{small:?}");
    assert_eq!(small.pending.height, 0);
    assert_eq!(small.input.height, 3);
}

proptest! {
    #[test]
    fn the_areas_tile_the_screen_without_overlap(
        w in 1u16..220, h in 1u16..80, banner in any::<bool>(), pending in 0u16..8, rows in 0u16..40, notice in any::<bool>()
    ) {
        let whole = Rect::new(0, 0, w, h);
        let a = areas(whole, banner, pending, rows, notice);
        let mut covered = vec![0u8; usize::from(w) * usize::from(h)];
        for (name, r) in rects(&a) {
            prop_assert!(r.right() <= w && r.bottom() <= h, "{name} {r:?} outside {whole:?}");
            for y in r.y..r.bottom() {
                for x in r.x..r.right() {
                    covered[usize::from(y) * usize::from(w) + usize::from(x)] += 1;
                }
            }
        }
        prop_assert!(covered.iter().all(|c| *c <= 1), "overlap in {a:?}");
        // The footer is the last row whenever there is a row for it.
        if h >= 2 {
            prop_assert_eq!(a.footer.bottom(), h);
        }
    }
}

// ---- cards -------------------------------------------------------------------------------------------------------------------------

fn card(id: &str, title: &str, lines: &[&str], folded: bool, alert: bool) -> Card {
    Card {
        id: id.into(),
        title: title.into(),
        lines: lines.iter().map(ToString::to_string).collect(),
        folded,
        alert,
    }
}

fn base_blocks() -> Vec<Block> {
    vec![
        Block::message("p", Role::You, Origin::User, "Build a todo API with tests."),
        Block::message(
            "m1",
            Role::Engine,
            Origin::Engine,
            "I'll start by reading the repo.",
        ),
        Block::tool_call(
            "t1",
            "run_commands: pytest -q",
            Some("execute"),
            Some("completed"),
            "",
        ),
        Block::message("m2", Role::Engine, Origin::Engine, "Tests pass."),
    ]
}

fn model(blocks: Vec<Block>) -> ChatModel {
    ChatModel::from_blocks(Source::FullHistory, "cline", blocks)
}

#[test]
fn a_card_is_a_harness_block_with_a_head_and_a_body_and_its_own_fold_state() {
    let mut v = ChatView::new();
    v.set_model(model(base_blocks()));
    v.set_cards(&[
        card(
            "status",
            "status",
            &["active (planning)", "Model  Claude Sonnet 5.5"],
            true,
            false,
        ),
        card(
            "question",
            "Cline needs you \u{b7} about 28 min left",
            &["The model reached its output limit"],
            false,
            true,
        ),
    ]);
    let blocks = &v.model().blocks;
    assert_eq!(blocks.len(), 6);
    let s = &blocks[4];
    assert_eq!(
        (s.id.as_str(), s.head.as_str(), s.folded, s.origin, s.role),
        ("card:status", "status", true, Origin::Harness, Role::Notice)
    );
    assert_eq!(s.body, "active (planning)\nModel  Claude Sonnet 5.5");
    let q = &blocks[5];
    assert_eq!(q.id, "card:question");
    assert!(!q.folded);
    assert_eq!(
        q.kind,
        ham_core::chat::BlockKind::Notice(NoticeKind::NeedsInput)
    );
    assert_eq!(
        s.kind,
        ham_core::chat::BlockKind::Notice(NoticeKind::RunStatus)
    );
}

#[test]
fn new_cards_replace_the_old_ones_and_a_new_model_keeps_the_cards() {
    let mut v = ChatView::new();
    v.set_model(model(base_blocks()));
    v.set_cards(&[card("status", "status", &["a"], true, false)]);
    v.set_cards(&[card("delivery", "delivery", &["Zip  x"], false, false)]);
    let ids: Vec<&str> = v.model().blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["p", "m1", "t1", "m2", "card:delivery"]);
    let mut more = base_blocks();
    more.push(Block::message("m3", Role::Engine, Origin::Engine, "More."));
    v.set_model(model(more));
    let ids: Vec<&str> = v.model().blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["p", "m1", "t1", "m2", "m3", "card:delivery"]);
    v.set_cards(&[]);
    assert_eq!(v.model().blocks.len(), 5);
}

#[test]
fn an_unchanged_set_of_cards_lays_out_nothing_again() {
    let mut v = ChatView::new();
    v.set_model(model(base_blocks()));
    let cards = [card("status", "status", &["a", "b"], false, false)];
    v.set_cards(&cards);
    let h = header();
    let facts = ChatFacts::new(&h, "footer");
    let fake = FakeBox::with("", 1);
    let backend = TestBackend::new(80, 24);
    let mut t = Terminal::new(backend).unwrap();
    t.draw(|f| v.draw(f, f.area(), &facts, &fake)).unwrap();
    let first = v.wrap_computations();
    v.set_cards(&cards);
    t.draw(|f| v.draw(f, f.area(), &facts, &fake)).unwrap();
    assert_eq!(v.wrap_computations(), first);
}

#[test]
fn a_card_line_fits_the_transcript_so_the_delivery_link_line_never_wraps() {
    // 80 columns: 9 for the gutter and the fold column, 1 for the scrollbar.
    assert_eq!(ChatView::card_width(80), 70);
    assert_eq!(ChatView::card_width(200), 190);
    assert_eq!(ChatView::card_width(9), 0);
}

// ---- pending rows ------------------------------------------------------------------------------------------------------------------

fn screen(v: &ChatView, facts: &ChatFacts<'_>, input: &FakeBox, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| v.draw(f, f.area(), facts, input)).unwrap();
    let b = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let line: String = (0..w).map(|x| b[(x, y)].symbol().to_string()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn a_pending_message_shows_above_the_box_with_its_status() {
    let mut v = ChatView::new();
    v.set_model(model(base_blocks()));
    let h = header();
    let rows = [
        PendingRow {
            text: "please also add a README".into(),
            status: PendingStatus::WaitingForTurn,
        },
        PendingRow {
            text: "and run the linter".into(),
            status: PendingStatus::Sending,
        },
        PendingRow {
            text: "use pytest".into(),
            status: PendingStatus::Delivered { turn: 3 },
        },
    ];
    let mut facts = ChatFacts::new(&h, "Enter send");
    facts.pending = &rows;
    let fake = FakeBox::with("", 1);
    let s = screen(&v, &facts, &fake, 80, 24);
    let lines: Vec<&str> = s.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains("please also add a README"))
        .expect("shown");
    assert_eq!(
        lines[at],
        "you      please also add a README \u{2014} waits for this turn to end"
    );
    assert_eq!(
        lines[at + 1],
        "you      and run the linter \u{2014} sending"
    );
    assert_eq!(
        lines[at + 2],
        "you      use pytest \u{2014} delivered in turn 3"
    );
    // The rows sit directly above the box.
    assert!(lines[at + 3].starts_with('\u{250c}'), "{}", lines[at + 3]);
}

#[test]
fn a_failed_pending_message_says_why_and_a_long_one_is_cut_to_the_width() {
    let mut v = ChatView::new();
    v.set_model(model(base_blocks()));
    let h = header();
    let rows = [
        PendingRow {
            text: "x".repeat(200),
            status: PendingStatus::Failed("engine busy".into()),
        },
        PendingRow {
            text: "multi\nline".into(),
            status: PendingStatus::Sending,
        },
    ];
    let mut facts = ChatFacts::new(&h, "f");
    facts.pending = &rows;
    let s = screen(&v, &facts, &FakeBox::with("", 1), 60, 20);
    for l in s.lines() {
        assert!(l.chars().count() <= 60, "{l}");
    }
    assert!(s.contains("not delivered: engine busy"), "{s}");
    assert!(s.contains("you      multi line \u{2014} sending"), "{s}");
}

// ---- drawing -----------------------------------------------------------------------------------------------------------------------

#[test]
fn the_box_is_pinned_at_the_bottom_focused_and_asked_for_its_rows_at_the_inner_width() {
    let mut v = ChatView::new();
    v.set_model(model(base_blocks()));
    let h = header();
    let mut facts = ChatFacts::new(&h, "Esc browse \u{b7} Enter send");
    facts.focused = true;
    facts.box_title = "Message to Cline \u{b7} Enter = after this turn";
    let fake = FakeBox::with("hello", 2);
    let s = screen(&v, &facts, &fake, 80, 24);
    let (area, focused) = fake.rendered.get().expect("the box was drawn");
    assert!(focused);
    assert_eq!(
        area,
        Rect::new(0, 24 - 1 - 4, 80, 4),
        "two rows of text and two borders, above the footer"
    );
    assert_eq!(fake.asked_width.get(), 78);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[23], "Esc browse \u{b7} Enter send");
    assert!(
        lines[19].contains("Message to Cline \u{b7} Enter = after this turn"),
        "{}",
        lines[19]
    );
    assert!(lines[20].contains("hello\u{258c}"), "{}", lines[20]);
    // An unfocused box is drawn unfocused.
    facts.focused = false;
    let fake2 = FakeBox::with("hello", 1);
    screen(&v, &facts, &fake2, 80, 24);
    assert_eq!(fake2.rendered.get().map(|r| r.1), Some(false));
}

#[test]
fn the_header_banner_notice_and_footer_are_drawn_where_the_layout_puts_them() {
    let mut v = ChatView::new();
    v.set_model(model(base_blocks()));
    let h = header();
    let mut facts = ChatFacts::new(&h, "a artifacts \u{b7} c c cancel \u{b7} q quit");
    facts.banner = Some("\u{27f3} reconnecting; the run keeps going in AWS");
    facts.notice = Some("answered: fresh session");
    let s = screen(&v, &facts, &FakeBox::with("", 1), 100, 24);
    let lines: Vec<&str> = s.lines().collect();
    // The whole run id is 16 characters longer than its short form was: the age of the last read goes first at 100 columns.
    assert_eq!(lines[0], FULL.replace(" \u{b7} updated 2s ago", ""));
    assert_eq!(
        lines[1],
        "\u{27f3} reconnecting; the run keeps going in AWS"
    );
    assert_eq!(lines[22], "answered: fresh session");
    assert_eq!(lines[23], "a artifacts \u{b7} c c cancel \u{b7} q quit");
}

#[test]
fn the_conversation_scrolls_follows_and_pauses_through_the_view() {
    let blocks: Vec<Block> = (0..60)
        .map(|i| {
            Block::message(
                &format!("m{i}"),
                Role::Engine,
                Origin::Engine,
                &format!("block {i:02}"),
            )
        })
        .collect();
    let mut v = ChatView::new();
    v.set_model(model(blocks.clone()));
    let h = header();
    let facts = ChatFacts::new(&h, "f");
    let fake = FakeBox::with("", 1);
    let s = screen(&v, &facts, &fake, 60, 14);
    assert!(s.contains("block 59"));
    assert!(v.is_following());
    v.scroll(ScrollAction::PageUp);
    let s = screen(&v, &facts, &fake, 60, 14);
    assert!(!v.is_following());
    assert!(!s.contains("block 59"));
    assert!(s.contains("Jump to bottom"), "{s}");
    let mut more = blocks;
    more.push(Block::message(
        "m60",
        Role::Engine,
        Origin::Engine,
        "block 60",
    ));
    v.set_model(model(more));
    let s = screen(&v, &facts, &fake, 60, 14);
    assert!(s.contains("1 new"), "{s}");
    v.scroll(ScrollAction::Bottom);
    let s = screen(&v, &facts, &fake, 60, 14);
    assert!(s.contains("block 60"));
    v.scroll(ScrollAction::Top);
    let s = screen(&v, &facts, &fake, 60, 14);
    assert!(s.contains("block 00"));
    v.scroll(ScrollAction::LineDown);
    v.scroll(ScrollAction::LineUp);
    v.scroll(ScrollAction::PageDown);
    screen(&v, &facts, &fake, 60, 14);
    assert!(v.range_text().starts_with("lines "));
}

#[test]
fn selection_fold_and_click_go_to_the_transcript() {
    let mut v = ChatView::new();
    v.set_model(model(vec![
        Block::message("p", Role::You, Origin::User, "go"),
        Block::tool_call(
            "t1",
            "run_commands: pytest -q",
            Some("execute"),
            Some("completed"),
            "3 passed",
        ),
    ]));
    let h = header();
    let facts = ChatFacts::new(&h, "f");
    let fake = FakeBox::with("", 1);
    let s = screen(&v, &facts, &fake, 60, 14);
    assert!(!s.contains("3 passed"));
    v.select(Step::Next);
    v.select(Step::Next);
    assert_eq!(v.selected_id().as_deref(), Some("t1"));
    v.toggle_selected_fold();
    let s = screen(&v, &facts, &fake, 60, 14);
    assert!(s.contains("3 passed"), "{s}");
    v.select(Step::Prev);
    assert_eq!(v.selected_id().as_deref(), Some("p"));
    // The transcript starts on row 1 (under the header): a press on the marker of the tool row folds it.
    assert!(v.click(7, 1 + 1));
    let s = screen(&v, &facts, &fake, 60, 14);
    assert!(!s.contains("3 passed"), "{s}");
    assert!(!v.click(30, 0), "the header is not part of the transcript");
}

#[test]
fn the_view_counts_layouts_so_a_redraw_of_500_blocks_lays_out_nothing() {
    let blocks: Vec<Block> = (0..500)
        .map(|i| {
            Block::message(
                &format!("m{i}"),
                Role::Engine,
                Origin::Engine,
                &format!("block {i}\nsecond line\nthird"),
            )
        })
        .collect();
    let mut v = ChatView::new();
    v.set_model(model(blocks));
    let h = header();
    let facts = ChatFacts::new(&h, "f");
    let fake = FakeBox::with("", 1);
    screen(&v, &facts, &fake, 200, 60);
    assert_eq!(v.wrap_computations(), 500);
    v.scroll(ScrollAction::PageUp);
    screen(&v, &facts, &fake, 200, 60);
    screen(&v, &facts, &fake, 200, 60);
    assert_eq!(v.wrap_computations(), 500);
}

// ---- safety ------------------------------------------------------------------------------------------------------------------------

fn terminal_bytes(v: &ChatView, facts: &ChatFacts<'_>, fake: &FakeBox, w: u16, h: u16) -> Vec<u8> {
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
    t.draw(|f| v.draw(f, f.area(), facts, fake)).unwrap();
    drop(t);
    let bytes = out.0.lock().unwrap().clone();
    drop(out);
    bytes
}

fn assert_only_backend_sequences(bytes: &[u8]) {
    let text = String::from_utf8(bytes.to_vec()).expect("UTF-8");
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
            "control {n:#x}"
        );
        assert!(
            !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
            "bidi {n:#x}"
        );
        i += 1;
    }
}

#[test]
fn control_text_in_every_fact_writes_only_the_backends_sequences() {
    for h in [
        "\u{1b}]0;title\u{7}",
        "\u{1b}]8;;https://example.test\u{7}click\u{1b}]8;;\u{7}",
        "\u{9b}31m\u{9d}0;t\u{9c}",
        "\u{1b}[2J\u{1b}[H",
        "\u{202e}desrever\u{202c}\u{7}\u{8}",
    ] {
        let head = Header {
            run_id: h.into(),
            model: Some(h.into()),
            state: h.into(),
            cost: Some(h.into()),
            ..Header::default()
        };
        let mut v = ChatView::new();
        v.set_model(model(base_blocks()));
        v.set_cards(&[card(h, h, &[h, h], false, true)]);
        let rows = [PendingRow {
            text: h.into(),
            status: PendingStatus::Failed(h.into()),
        }];
        let mut facts = ChatFacts::new(&head, h);
        facts.banner = Some(h);
        facts.notice = Some(h);
        facts.pending = &rows;
        facts.box_title = h;
        let fake = FakeBox::with(h, 1);
        for (w, hh) in [(80, 24), (200, 60)] {
            assert_only_backend_sequences(&terminal_bytes(&v, &facts, &fake, w, hh));
        }
    }
}

// ---- snapshots ---------------------------------------------------------------------------------------------------------------------

fn conversation() -> Vec<Block> {
    vec![
        Block::message(
            "p",
            Role::You,
            Origin::User,
            "Build a todo API with tests. Use FastAPI and keep the data in memory.",
        ),
        Block::message(
            "m1",
            Role::Engine,
            Origin::Engine,
            "I'll start by reading the repo and writing the failing tests first. Then I'll add the routes one at a time and run the suite after each.",
        ),
        Block::tool_call(
            "t1",
            "run_commands: pytest -q",
            Some("execute"),
            Some("failed"),
            "",
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
            "",
        ),
        Block::notice(
            "n1",
            NoticeKind::ProviderRetry,
            "Provider error (temporary): try 1 of 4 in 5 s. Throttled",
        ),
        Block::message(
            "m2",
            Role::Engine,
            Origin::Engine,
            "Tests pass. I added the /todos routes and three tests; the suite passes.",
        ),
    ]
}

fn snap(name: &str, v: &ChatView, facts: &ChatFacts<'_>, input: &FakeBox) {
    for (w, h) in [(80u16, 24u16), (120, 40), (200, 60)] {
        insta::assert_snapshot!(format!("{name}_{w}x{h}"), screen(v, facts, input, w, h));
    }
}

#[test]
fn snapshot_of_an_active_run() {
    let mut v = ChatView::new();
    v.set_model(model(conversation()));
    v.set_cards(&[card(
        "status",
        "status \u{b7} agents",
        &[
            "active (planning)",
            "Model   Claude Sonnet 5.5",
            "direct  cline  1.0M  3 turns",
        ],
        true,
        false,
    )]);
    let h = header();
    let mut facts = ChatFacts::new(
        &h,
        "Esc browse \u{b7} Enter send \u{b7} Ctrl-J newline \u{b7} / commands \u{b7} F1 help",
    );
    facts.focused = true;
    facts.box_title = "Message to Cline \u{b7} Enter = after this turn \u{b7} /now = interrupt";
    snap(
        "chat_active",
        &v,
        &facts,
        &FakeBox::with("please also add a README", 1),
    );
}

#[test]
fn snapshot_of_a_run_that_waits_for_the_user() {
    let mut v = ChatView::new();
    v.set_model(model(conversation()));
    v.set_cards(&[
        card("status", "status \u{b7} agents", &["active (planning)"], true, false),
        card(
            "question",
            "Cline needs you \u{b7} about 28 min left to answer",
            &[
                "The model reached its output limit",
                "The model reached its maximum output length for one answer. You can send another prompt.",
                "tool calls 41 \u{b7} files changed 3",
                "p message \u{b7} s fresh session \u{b7} m model \u{b7} f f finish",
            ],
            false,
            true,
        ),
    ]);
    let mut h = header();
    h.state = "needs you".into();
    let mut facts = ChatFacts::new(
        &h,
        "p message \u{b7} s fresh \u{b7} m model \u{b7} f f finish \u{b7} a artifacts \u{b7} c c cancel \u{b7} q quit",
    );
    facts.notice = Some("answer sent \u{b7} waiting for Cline to take it");
    snap("chat_waiting", &v, &facts, &FakeBox::with("", 1));
}

#[test]
fn snapshot_of_a_delivered_run_with_the_one_line_link() {
    for (w, h) in [(80u16, 24u16), (120, 40), (200, 60)] {
        // The app fits the link line to the card's width, so it is one row at every size.
        let link = link_line(
            &LinkLine {
                host: "s3",
                name: "run-01M5287J4KQ8N3X7C9D2E6F1AB.zip",
                expires: Some("06:12 MDT"),
                expired: false,
                keys: "o open \u{b7} d download",
            },
            ChatView::card_width(w),
        );
        let mut v = ChatView::new();
        v.set_model(model(conversation()));
        v.set_cards(&[card(
            "delivery",
            "delivery",
            &[
                "Run       01M5287J4KQ8N3X7C9D2E6F1AB",
                "Outcome   Cline reported done",
                "Model     Claude Sonnet 5.5",
                "PR        see the pull request",
                "Zip       run.zip (sha256 abababababab, 2048 bytes)",
                &link,
                "Clone     C clone to the Mac",
                "Tokens    113000  USD 0.14",
            ],
            false,
            false,
        )]);
        let mut hd = header();
        hd.state = "complete".into();
        let facts = ChatFacts::new(
            &hd,
            "o open PR \u{b7} d download \u{b7} a artifacts \u{b7} Esc runs \u{b7} n new run \u{b7} q quit",
        );
        let fake = FakeBox::with("", 1);
        let text = screen(&v, &facts, &fake, w, h);
        let link_rows: Vec<&str> = text.lines().filter(|l| l.contains("Link ")).collect();
        assert_eq!(link_rows.len(), 1, "{text}");
        assert!(
            link_rows[0].contains("d download"),
            "the whole link line is on one row: {}",
            link_rows[0]
        );
        insta::assert_snapshot!(format!("chat_complete_{w}x{h}"), text);
    }
}

#[test]
fn snapshot_of_a_blocked_run_with_a_banner() {
    let mut v = ChatView::new();
    v.set_model(model(vec![
        Block::message("p", Role::You, Origin::User, "Build it."),
        Block::gap("g", ham_core::chat::GapCause::TailUnknown, None),
    ]));
    v.set_cards(&[card(
        "blocked",
        "blocked \u{b7} c cancel",
        &["the session ended before any work was saved"],
        false,
        true,
    )]);
    let mut h = header();
    h.state = "blocked".into();
    let mut facts = ChatFacts::new(
        &h,
        "c c cancel \u{b7} a artifacts \u{b7} Esc runs \u{b7} q quit",
    );
    facts.banner = Some(
        "\u{27f3} reconnecting \u{b7} the run keeps going in AWS; showing the last state read",
    );
    snap("chat_blocked", &v, &facts, &FakeBox::with("", 1));
}

#[test]
fn the_pending_rows_and_a_two_row_box_in_a_snapshot() {
    let mut v = ChatView::new();
    v.set_model(model(conversation()));
    let h = header();
    let rows = [PendingRow {
        text: "please also add a README".into(),
        status: PendingStatus::WaitingForTurn,
    }];
    let mut facts = ChatFacts::new(&h, "Esc browse \u{b7} Enter send");
    facts.pending = &rows;
    facts.focused = true;
    snap(
        "chat_pending",
        &v,
        &facts,
        &FakeBox::with("and a changelog\nand a license", 2),
    );
}
