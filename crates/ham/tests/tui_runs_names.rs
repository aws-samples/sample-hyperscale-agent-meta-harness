//! The Runs screen with run names: the columns are RUN, STATUS and NAME first; CREATED, TARGET and PHASE join as the terminal
//! gets wider, each only while NAME keeps 30 columns. The NAME cell is cut by display width with an ellipsis; a seed (the name built from the
//! prompt) and a title (a backend without names) are drawn dim, a model's name plain, a run with neither a dim dash. A name that reached
//! the screen with an escape sequence is cleaned. The filter finds a word that is only in a name and underlines the characters drawn.
//! The hidden columns of the selected run are said in the note line under the table. Row 0 keeps the version tag at every width.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::runs_table::{RunsColumns, runs_columns};
use ham::tui::version_tag::split_tag;
use ham::tui::{App, DirectSettings, Msg, Screen};
use ham_core::runfilter::{IndexEntry, IndexedRead, RunsRead};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use serde_json::{Value, json};

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";
const R4: &str = "01M5A00000000000000000R004";

/// A name of exactly 60 characters, longer than the NAME cell at 80 columns (32) and shorter than the cell at 160 (53 plus the rest).
const LONG: &str = "Add pagination to the order list endpoint with cursor tokens";

/// Where the NAME cell starts: the border, the gutter, a gap, RUN (28), a gap, STATUS (14) and a gap.
const NAME_X: u16 = 1 + 1 + 1 + 28 + 1 + 14 + 1;
/// The table's header row and its first run row on screen (the title is row 0, the border row 1).
const HEADER_Y: u16 = 2;
const FIRST_Y: u16 = 3;

fn run(id: &str, status: &str, target: Value, created: &str) -> RunSummary {
    serde_json::from_value(
        json!({"runId": id, "status": status, "phase": "planning", "target": target,
        "createdAt": created, "ownerPrincipalArn": ARN}),
    )
    .unwrap()
}

fn s3(id: &str, status: &str) -> RunSummary {
    run(
        id,
        status,
        json!({"mode": "s3-only"}),
        "2026-10-20T12:00:00Z",
    )
}

fn entry(id: &str, title: Option<&str>, name: Option<&str>, source: Option<&str>) -> IndexEntry {
    IndexEntry {
        run_id: id.into(),
        title: title.map(str::to_string),
        model: None,
        name: name.map(str::to_string),
        name_source: source.map(str::to_string),
        tags: None,
    }
}

fn app(runs: Vec<RunSummary>, index: Vec<IndexEntry>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(runs),
        index,
    })));
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn buffer(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

/// The text of columns `from..to` of row `y`: a wide character takes its first cell and the cell after it is skipped.
fn text_in(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
    let mut out = String::new();
    let mut x = from;
    while x < to.min(buf.area.width) {
        let sym = buf[(x, y)].symbol();
        out.push_str(sym);
        x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
    }
    out
}

fn row(buf: &Buffer, y: u16) -> String {
    text_in(buf, y, 0, buf.area.width)
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn type_in(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

fn cols(name: Option<u16>, created: bool, target: bool, phase: bool) -> RunsColumns {
    RunsColumns {
        name,
        created,
        target,
        phase,
    }
}

#[test]
fn the_planner_gives_the_column_set_and_the_name_width_at_every_tier_edge() {
    for (w, want) in [
        (30, cols(None, false, false, false)),
        (59, cols(None, false, false, false)),
        (60, cols(Some(12), false, false, false)),
        (80, cols(Some(32), false, false, false)),
        (98, cols(Some(50), false, false, false)),
        (99, cols(Some(30), true, false, false)),
        (100, cols(Some(31), true, false, false)),
        (120, cols(Some(51), true, false, false)),
        (123, cols(Some(54), true, false, false)),
        (124, cols(Some(30), true, true, false)),
        (136, cols(Some(42), true, true, false)),
        (137, cols(Some(30), true, true, true)),
        (160, cols(Some(53), true, true, true)),
        (200, cols(Some(93), true, true, true)),
    ] {
        assert_eq!(runs_columns(w), want, "terminal width {w}");
    }
}

#[test]
fn columns_per_width_on_the_screen_follow_the_planner_and_run_then_status_stay_adjacent() {
    let runs = vec![s3(R1, "complete")];
    let index = vec![entry(R1, Some("A title"), Some(LONG), Some("ai"))];
    let a = app(runs, index);
    for w in [59u16, 60, 80, 98, 99, 100, 120, 123, 124, 136, 137, 160] {
        let want = runs_columns(w);
        let buf = buffer(&a, w, 24);
        let header = row(&buf, HEADER_Y);
        for (label, shown) in [
            ("NAME", want.name.is_some()),
            ("CREATED", want.created),
            ("TARGET", want.target),
            ("PHASE", want.phase),
            ("RUN", true),
            ("STATUS", true),
        ] {
            assert_eq!(header.contains(label), shown, "{label} at {w}: {header}");
        }
        // RUN is followed by STATUS with only blanks between them, at every width.
        let (run_at, status_at) = (header.find("RUN").unwrap(), header.find("STATUS").unwrap());
        assert!(run_at < status_at, "{header}");
        assert!(
            header[run_at + 3..status_at].chars().all(|c| c == ' '),
            "{header}"
        );
        // The first word after the id on a run row is its status.
        let line = row(&buf, FIRST_Y);
        let after = line.split(R1).nth(1).unwrap().split_whitespace().next();
        assert_eq!(after, Some("complete"), "{line}");
        // NAME starts where the plan says, as wide as the plan says, and the frame is whole.
        if let Some(n) = want.name {
            assert_eq!(text_in(&buf, HEADER_Y, NAME_X, NAME_X + 4), "NAME", "{w}");
            let cell = text_in(&buf, FIRST_Y, NAME_X, NAME_X + n);
            let shown = LONG.chars().count();
            if usize::from(n) >= shown {
                assert!(cell.starts_with(LONG), "{w}: {cell:?}");
            } else {
                let cut: String = LONG.chars().take(usize::from(n) - 1).collect();
                assert_eq!(cell, format!("{cut}\u{2026}"), "{w}");
            }
        }
        assert_eq!(buf[(w - 1, FIRST_Y)].symbol(), "│", "the frame at {w}");
    }
}

#[test]
fn an_ascii_name_and_a_cjk_name_are_cut_to_the_cell_by_display_width_and_end_with_an_ellipsis() {
    let cjk = "日本語のログイン画面のバグを修正する作業";
    assert_eq!(cjk.chars().count(), 20);
    let a = app(
        vec![s3(R1, "complete"), s3(R2, "complete")],
        vec![
            entry(R1, None, Some(LONG), Some("ai")),
            entry(R2, None, Some(cjk), Some("ai")),
        ],
    );
    let buf = buffer(&a, 80, 24);
    // The NAME cell at 80 columns is 32 wide.
    let ascii = text_in(&buf, FIRST_Y, NAME_X, NAME_X + 32);
    let want: String = LONG.chars().take(31).collect::<String>() + "\u{2026}";
    assert_eq!(ascii, want);
    // Each CJK character takes two columns: the cell holds the first 15 (30 columns) and the ellipsis, never half a character.
    // 15 characters take 30 columns and the ellipsis one: the 32nd column of the cell is blank.
    let wide = text_in(&buf, FIRST_Y + 1, NAME_X, NAME_X + 32)
        .trim_end()
        .to_string();
    let first15: String = cjk.chars().take(15).collect();
    assert_eq!(wide, format!("{first15}\u{2026}"));
    assert!(
        ratatui::text::Span::raw(wide.clone()).width() <= 32,
        "{wide}"
    );
    for y in [FIRST_Y, FIRST_Y + 1] {
        assert_eq!(
            buf[(79, y)].symbol(),
            "│",
            "row {y} overflowed: {}",
            row(&buf, y)
        );
    }
}

#[test]
fn a_seed_and_a_title_are_dim_a_model_name_is_plain_and_a_run_with_neither_is_a_dim_dash() {
    let a = app(
        vec![
            s3(R1, "complete"),
            s3(R2, "complete"),
            s3(R3, "complete"),
            s3(R4, "complete"),
        ],
        vec![
            entry(
                R1,
                Some("Add pagination"),
                Some("Order list pagination with cursors"),
                Some("ai"),
            ),
            entry(
                R2,
                Some("Fix the login redirect loop"),
                Some("Fix the login redirect loop"),
                Some("seed"),
            ),
            // A backend without names: the row has a title and no name.
            entry(R3, Some("Old backend title of the run"), None, None),
            // No index entry at all.
        ],
    );
    let buf = buffer(&a, 80, 24);
    let dim = |y: u16| buf[(NAME_X, y)].modifier.contains(Modifier::DIM);
    assert_eq!(
        text_in(&buf, FIRST_Y, NAME_X, NAME_X + 20),
        "Order list paginatio"
    );
    assert!(!dim(FIRST_Y), "a model name is plain");
    assert_eq!(text_in(&buf, FIRST_Y + 1, NAME_X, NAME_X + 8), "Fix the ");
    assert!(dim(FIRST_Y + 1), "a seed is dim");
    assert_eq!(
        text_in(&buf, FIRST_Y + 2, NAME_X, NAME_X + 12),
        "Old backend "
    );
    assert!(dim(FIRST_Y + 2), "a title stands in dim");
    assert_eq!(text_in(&buf, FIRST_Y + 3, NAME_X, NAME_X + 1), "-");
    assert!(dim(FIRST_Y + 3), "the dash is dim");
}

#[test]
fn a_stored_name_with_a_raw_escape_is_not_drawn() {
    let hostile = "Fix \u{1b}[31mlogin\u{1b}[0m bug in the OAuth flow \u{1b}]52;c;AAAA\u{7}";
    let a = app(
        vec![s3(R1, "complete")],
        vec![entry(R1, None, Some(hostile), Some("ai"))],
    );
    let buf = buffer(&a, 120, 24);
    for y in 0..24 {
        for x in 0..120 {
            let sym = buf[(x, y)].symbol();
            assert!(
                !sym.contains('\u{1b}') && !sym.contains('\u{7}'),
                "({x},{y})"
            );
        }
    }
    let line = row(&buf, FIRST_Y);
    assert!(line.contains("Fix login bug in the OAuth flow"), "{line}");
    assert!(!line.contains("[31m") && !line.contains("52;c"), "{line}");
}

#[test]
fn the_filter_finds_a_word_that_is_only_in_a_name_and_underlines_the_characters_drawn() {
    let name = "Fix the login redirect loop";
    let mut a = app(
        vec![s3(R1, "complete"), s3(R2, "complete")],
        vec![
            entry(R1, Some("Build a thing"), Some(LONG), Some("ai")),
            entry(R2, Some("Other work"), Some(name), Some("ai")),
        ],
    );
    key(&mut a, KeyCode::Char('/'));
    type_in(&mut a, "redirect");
    let buf = buffer(&a, 100, 24);
    let text: String = (0..24).map(|y| row(&buf, y)).collect::<Vec<_>>().join("\n");
    assert!(text.contains(R2) && !text.contains(R1), "{text}");
    let at = u16::try_from(name.find("redirect").unwrap()).unwrap();
    let underlined: Vec<u16> = (0..100)
        .filter(|&x| buf[(x, FIRST_Y)].modifier.contains(Modifier::UNDERLINED))
        .collect();
    let want: Vec<u16> = (NAME_X + at..NAME_X + at + 8).collect();
    assert_eq!(underlined, want, "{}", row(&buf, FIRST_Y));
}

#[test]
fn a_match_beyond_the_cut_underlines_nothing_and_the_ellipsis_is_never_underlined() {
    let mut a = app(
        vec![s3(R1, "complete")],
        vec![entry(R1, Some("Build a thing"), Some(LONG), Some("ai"))],
    );
    key(&mut a, KeyCode::Char('/'));
    type_in(&mut a, "tokens");
    let buf = buffer(&a, 80, 24);
    assert!(
        row(&buf, FIRST_Y).contains(R1),
        "the run is found by its name"
    );
    for x in 0..80 {
        assert!(
            !buf[(x, FIRST_Y)].modifier.contains(Modifier::UNDERLINED),
            "column {x}: {}",
            row(&buf, FIRST_Y)
        );
    }
}

#[test]
fn a_match_that_straddles_the_cut_underlines_only_the_characters_drawn_and_never_the_ellipsis() {
    let mut a = app(
        vec![s3(R1, "complete")],
        vec![entry(R1, Some("Build a thing"), Some(LONG), Some("ai"))],
    );
    key(&mut a, KeyCode::Char('/'));
    type_in(&mut a, "list");
    let buf = buffer(&a, 80, 24);
    // "list" is characters 28 to 31 of the name; the cell shows 31 characters and the ellipsis, so "lis" is drawn and the ellipsis follows it.
    let underlined: Vec<u16> = (0..80)
        .filter(|&x| buf[(x, FIRST_Y)].modifier.contains(Modifier::UNDERLINED))
        .collect();
    assert_eq!(underlined, (NAME_X + 28..NAME_X + 31).collect::<Vec<u16>>());
    assert_eq!(buf[(NAME_X + 31, FIRST_Y)].symbol(), "\u{2026}");
}

#[test]
fn the_note_says_the_hidden_columns_of_the_selected_run() {
    // An active run: a run that is over shows no phase (`tui_phase_shown.rs`).
    let a = app(
        vec![run(
            R1,
            "active",
            json!({"mode": "repo", "repo": "cc:calc"}),
            "2026-10-20T12:41:03Z",
        )],
        // The run holds no tags (the person removed the project the repository gives), so the note adds nothing before the hidden columns:
        // puts the selected run's tags between the count and these columns, `tests/runs_note_tags.rs` pins that.
        vec![IndexEntry {
            tags: Some(Vec::new()),
            ..entry(R1, Some("T"), Some("Order list pagination"), Some("ai"))
        }],
    );
    let note = |w: u16, h: u16| row(&buffer(&a, w, h), h - 2);
    let n80 = note(80, 24);
    for want in [
        "target cc:calc",
        "phase planning",
        "created 2026-10-20T12:41:03Z",
    ] {
        assert!(n80.contains(want), "{want:?} not in {n80:?}");
    }
    // At 100 columns CREATED is a column; at 130 TARGET is too; at 160 nothing is hidden.
    let n100 = note(100, 24);
    assert!(
        n100.contains("target cc:calc") && n100.contains("phase planning"),
        "{n100}"
    );
    assert!(!n100.contains("created"), "{n100}");
    let n130 = note(130, 24);
    assert!(n130.contains("phase planning"), "{n130}");
    assert!(
        !n130.contains("target cc:calc") && !n130.contains("created"),
        "{n130}"
    );
    let n160 = note(160, 24);
    assert!(
        !n160.contains("phase planning")
            && !n160.contains("target cc:calc")
            && !n160.contains("created"),
        "{n160}"
    );
    assert!(n160.contains("1 runs"), "the count stays: {n160}");
}

#[test]
fn row_zero_still_ends_with_the_version_tag() {
    let a = app(
        vec![s3(R1, "complete")],
        vec![entry(R1, None, Some(LONG), Some("ai"))],
    );
    for w in [60u16, 80, 120] {
        let line = row(&buffer(&a, w, 24), 0);
        let (title, tag) = split_tag(&line);
        assert!(tag.is_some(), "no tag at {w}: {line:?}");
        assert_eq!(title, "ham · Runs", "{w}");
    }
}
