//! The tag explorer: an overlay on the Runs screen, opened with `t`, built from the rows the screen already holds. It
//! takes every key while it is open: `Space` picks a tag (picks are AND), `/` opens the find line, `Enter` applies the picks to the Runs list
//! (with no pick, the tag under the cursor), `d` removes the tag under the cursor from every held run after a question, `r` reads every page, `Esc`
//! clears the find and then closes. The drawings are those of design 9.2 and 9.3 (`snapshots/tui_tags__*.snap`); counts cover the held rows
//! and the title says whether the list was read in full.
#![allow(clippy::unwrap_used)]

mod tagsfx;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::version_tag::split_tag;
use ham::tui::{Effect, Msg, Screen, TagsOrigin};
use ham_core::runtag::wire::{TagEditResponse, TagEditResult};
use harness_protocol::api::v1::RunSummary;
use serde_json::json;
use tagsfx::*;

/// The design's Runs screen with the overlay open, `project:billing-export` picked and the cursor on `topic:csv-export`.
fn open_design() -> ham::tui::App {
    let mut app = design();
    ch(&mut app, 't');
    ch(&mut app, ' ');
    for _ in 0..3 {
        ch(&mut app, 'j');
    }
    app
}

fn width_of(line: &str) -> usize {
    ratatui::text::Span::raw(line.to_string()).width()
}

// ---- the drawings -----------------------------------------------------------------------------------------------------------------

#[test]
fn explorer_layout_at_80x24_120x40_and_200x60() {
    let app = open_design();
    for (w, h) in [(80u16, 24u16), (120, 40), (200, 60)] {
        let text = screen_text(&app, w, h);
        insta::assert_snapshot!(format!("explorer_{w}x{h}"), text);
        for line in text.lines() {
            assert!(width_of(line) <= usize::from(w), "{w}x{h}: {line:?}");
        }
        assert_eq!(text.lines().count(), usize::from(h), "{w}x{h}");
        // Row 0 ends with the version tag, which the snapshots leave off.
        let zero = row_zero(&app, w);
        assert!(
            zero.starts_with("ham \u{b7} Tags") && split_tag(&zero).1.is_some(),
            "{w}: {zero:?}"
        );
        assert!(width_of(&zero) <= usize::from(w), "{w}: {zero:?}");
    }
}

#[test]
fn the_title_says_whether_the_list_was_read_in_full() {
    // Every page read: the number of runs and the time of the read.
    let mut app = design();
    ch(&mut app, 't');
    let text = screen_text(&app, 100, 30);
    assert!(
        text.contains("\u{250c} tags \u{b7} all 212 runs loaded at 14:05 "),
        "{text}"
    );
    // Only the newest page, with more behind it: the title says so and offers the reload.
    let runs: Vec<RunSummary> = (0..100)
        .map(|n| s3_only(&id(n), "2026-10-20T12:00:00Z"))
        .collect();
    let mut app = ham::tui::App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
    app.set_now(unix(NOW));
    read_first(&mut app, runs, true, vec![]);
    ch(&mut app, 't');
    let text = screen_text(&app, 100, 30);
    assert!(
        text.contains("\u{250c} tags \u{b7} newest 100 runs only - r loads all "),
        "{text}"
    );
    // The first page with no next token is the whole list.
    let runs: Vec<RunSummary> = (0..7)
        .map(|n| s3_only(&id(n), "2026-10-20T12:00:00Z"))
        .collect();
    let mut app = ham::tui::App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
    app.set_now(unix(NOW));
    read_first(&mut app, runs, false, vec![]);
    ch(&mut app, 't');
    let text = screen_text(&app, 100, 30);
    assert!(text.contains("all 7 runs loaded at 14:05"), "{text}");
}

#[test]
fn a_timer_read_that_leaves_a_gap_takes_the_all_claim_back_and_one_that_overlaps_keeps_it() {
    let old: Vec<RunSummary> = (0..30)
        .map(|n| s3_only(&id(n), &format!("2026-10-20T12:{:02}:00Z", 59 - n)))
        .collect();
    let mut app = app_with(old.clone(), vec![]);
    // A newest page with a next token that holds runs the screen already knows: nothing can be missing between, so the claim stays.
    let mut page: Vec<RunSummary> = vec![s3_only(&id(900), "2026-10-20T13:30:00Z")];
    page.extend(old.iter().take(10).cloned());
    read_first(&mut app, page, true, vec![]);
    ch(&mut app, 't');
    let text = screen_text(&app, 100, 30);
    assert!(text.contains("all 31 runs loaded at 14:05"), "{text}");
    key(&mut app, KeyCode::Esc);
    // A newest page that shares no run with the held list and has more behind it: a gap may lie between, so the title no longer says all.
    let fresh: Vec<RunSummary> = (0..100)
        .map(|n| s3_only(&id(1000 + n), "2026-10-21T10:00:00Z"))
        .collect();
    read_first(&mut app, fresh, true, vec![]);
    ch(&mut app, 't');
    let text = screen_text(&app, 100, 30);
    assert!(
        text.contains("newest 100 runs only - r loads all"),
        "{text}"
    );
}

#[test]
fn usable_down_to_40_columns() {
    // The longest tag a person can have: 48 characters.
    let long = "project:abcdefghijklmnop-abcdefghijklmnop-abcdef";
    assert_eq!(long.len(), 48);
    let mut app = app_with(
        vec![s3_only(&id(1), "2026-10-20T12:00:00Z")],
        vec![entry(&id(1), Some(&[long, "tech:go"]))],
    );
    ch(&mut app, 't');
    for w in [40u16, 59, 79, 99, 100, 120] {
        let text = screen_text(&app, w, 24);
        for line in text.lines() {
            assert!(width_of(line) <= usize::from(w), "{w}: {line:?}");
        }
        // The tag column is as wide as the longest tag, at most 48, and gives way to the room the screen has: prefix 3, the gap 1, RUNS 5
        // and, from 100 columns, the gap 1 and LAST 5, inside the border.
        let inner = usize::from(w) - 2;
        let room = inner - 3 - 1 - 5 - if w >= 100 { 6 } else { 0 };
        let want = if room >= 48 {
            long.to_string()
        } else {
            format!("{}\u{2026}", &long[..room - 1])
        };
        let row = text
            .lines()
            .find(|l| l.contains(&want[..want.len().min(12)]) && l.starts_with("\u{2502}"))
            .unwrap_or_else(|| panic!("{w}: no row for the long tag in\n{text}"));
        assert!(row.contains(&want), "{w}: {row:?} lacks {want:?}");
        // The count is right-aligned to the column the layout gives it.
        let counts: Vec<&str> = row.trim_matches('\u{2502}').split_whitespace().collect();
        assert!(counts.contains(&"1"), "{w}: {row:?}");
    }
    // Below the usable width nothing overflows and nothing panics.
    for w in [39u16, 30, 20, 12, 8, 4] {
        let text = screen_text(&app, w, 10);
        for line in text.lines() {
            assert!(width_of(line) <= usize::from(w), "{w}: {line:?}");
        }
    }
}

#[test]
fn empty_state_and_old_backend_rows() {
    // No tag anywhere (an older backend, runs with no repository): the empty drawing of design 9.3.
    let runs: Vec<RunSummary> = (0..212)
        .map(|n| s3_only(&id(n), "2026-10-01T12:00:00Z"))
        .collect();
    let mut app = app_with(runs, vec![]);
    ch(&mut app, 't');
    insta::assert_snapshot!("empty_80x24", screen_text(&app, 80, 24));
    // Rows with no `tags` member show the project their repository gives and nothing else, and the runs with none count as untagged.
    let runs = vec![
        repo(&id(1), "2026-10-20T12:03:00Z", "calc"),
        repo(&id(2), "2026-10-20T12:02:00Z", "calc"),
        repo(&id(3), "2026-10-20T12:01:00Z", "shop"),
        s3_only(&id(4), "2026-10-20T12:00:00Z"),
        s3_only(&id(5), "2026-10-20T11:59:00Z"),
    ];
    let mut app = app_with(runs, vec![]);
    ch(&mut app, 't');
    let text = screen_text(&app, 100, 30);
    assert_eq!(
        table(&text),
        [
            ("project:calc".to_string(), 2),
            ("project:shop".to_string(), 1)
        ],
        "{text}"
    );
    let (note, _) = bottom_two(&text);
    assert_eq!(note, "5 runs \u{b7} 2 tags \u{b7} 2 have no tags");
    assert!(!text.to_lowercase().contains("error"), "{text}");
}

#[test]
fn a_tag_with_an_escape_byte_is_never_drawn() {
    let a = id(1);
    let mut app = app_with(
        vec![s3_only(&a, "2026-10-20T12:00:00Z")],
        vec![entry(
            &a,
            Some(&["topic:fine", "topic:ev\u{1b}[31mil", "topic:bell\u{7}x"]),
        )],
    );
    ch(&mut app, 't');
    let text = screen_text(&app, 100, 30);
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
    assert!(!text.contains("evil") && !text.contains("bell"), "{text}");
    assert_eq!(table(&text), [("topic:fine".to_string(), 1)], "{text}");
}

// ---- a small world ----------------------------------------------------------------------------------------------------------------

#[test]
fn t_opens_the_explorer_from_the_runs_list_and_esc_closes_it() {
    let (mut app, _) = small();
    assert!(ch(&mut app, 't').is_empty());
    let text = screen(&app);
    assert!(text.starts_with("ham \u{b7} Tags"), "{text}");
    assert_eq!(
        table(&text),
        [
            pair("tech:rust", 2),
            pair("topic:alpha", 2),
            pair("topic:beta", 1)
        ],
        "runs descending, then the newest run, then the text:\n{text}"
    );
    assert_eq!(cursor_tag(&text).as_deref(), Some("tech:rust"));
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    let text = screen(&app);
    assert!(text.starts_with("ham \u{b7} Runs"), "{text}");
    assert!(!text.contains("ham \u{b7} Tags"), "{text}");
    // `t` is a letter of the filter line, not a key, while that line is open.
    ch(&mut app, '/');
    ch(&mut app, 't');
    assert!(screen(&app).starts_with("ham \u{b7} Runs"));
}

#[test]
fn picks_narrow_the_counts_and_apply_filters_the_runs() {
    let (mut app, ids) = small();
    ch(&mut app, 't');
    // Space on `tech:rust` picks it: the counts are those of the runs that hold it, a tag with none is hidden, picked tags come first.
    ch(&mut app, ' ');
    let text = screen(&app);
    assert_eq!(
        table(&text),
        [
            pair("tech:rust", 2),
            pair("topic:alpha", 1),
            pair("topic:beta", 1)
        ],
        "{text}"
    );
    assert_eq!(picked(&text), ["tech:rust"]);
    let (note, _) = bottom_two(&text);
    assert_eq!(note, "2 of 4 runs match +tech:rust \u{b7} 3 tags");
    // A second pick (picks are AND): only A holds both; `topic:beta` has no run left and is hidden.
    ch(&mut app, 'j');
    ch(&mut app, ' ');
    let text = screen(&app);
    assert_eq!(
        table(&text),
        [pair("tech:rust", 1), pair("topic:alpha", 1)],
        "{text}"
    );
    assert_eq!(picked(&text), ["tech:rust", "topic:alpha"]);
    // Space on a picked tag drops the pick.
    ch(&mut app, ' ');
    assert_eq!(picked(&screen(&app)), ["tech:rust"]);
    ch(&mut app, ' ');
    // Enter leaves the overlay with the Runs list holding exactly the runs that hold every pick, and the summary names them.
    assert!(key(&mut app, KeyCode::Enter).is_empty());
    assert_eq!(app.screen(), Screen::Runs);
    let text = screen(&app);
    assert_eq!(listed(&text, &ids), [ids[0].clone()], "{text}");
    let (note, _) = bottom_two(&text);
    assert!(
        note.starts_with("1 of 4 runs match +tech:rust +topic:alpha"),
        "{note:?}"
    );
    // Picks, words and the scope all narrow together: the word `beta` cannot be found among the runs that hold `tech:rust` and `topic:alpha`.
    ch(&mut app, '/');
    type_in(&mut app, "beta");
    let text = screen(&app);
    assert!(listed(&text, &ids).is_empty(), "{text}");
    assert!(
        bottom_two(&text)
            .0
            .starts_with("no run matches \"beta\" +tech:rust +topic:alpha (4 runs)"),
        "{text}"
    );
    // Esc on the Runs list clears the words, the scope and the picks together: the list is whole again.
    key(&mut app, KeyCode::Esc);
    let text = screen(&app);
    assert_eq!(listed(&text, &ids), ids.to_vec(), "{text}");
    assert!(bottom_two(&text).0.starts_with("4 runs"), "{text}");
}

#[test]
fn a_run_listed_by_the_picks_is_found_again_by_a_word_of_the_same_tag() {
    // Typing a word of a tag in the Runs `/` line finds the run too: the tags are in the searched words.
    let (mut app, ids) = small();
    ch(&mut app, '/');
    type_in(&mut app, "beta");
    let text = screen(&app);
    assert_eq!(listed(&text, &ids), [ids[2].clone()], "{text}");
}

#[test]
fn the_explorer_starts_with_the_picks_the_runs_list_already_has() {
    let (mut app, ids) = small();
    ch(&mut app, 't');
    ch(&mut app, 'j');
    ch(&mut app, ' ');
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        listed(&screen(&app), &ids),
        [ids[0].clone(), ids[1].clone()]
    );
    ch(&mut app, 't');
    let text = screen(&app);
    assert_eq!(picked(&text), ["topic:alpha"], "{text}");
    // Esc closes without applying: the Runs list keeps the picks it had.
    ch(&mut app, ' ');
    assert!(picked(&screen(&app)).is_empty());
    key(&mut app, KeyCode::Esc);
    assert_eq!(
        listed(&screen(&app), &ids),
        [ids[0].clone(), ids[1].clone()]
    );
}

#[test]
fn enter_with_no_pick_applies_the_marked_tag() {
    let (mut app, ids) = small();
    ch(&mut app, 't');
    ch(&mut app, 'j');
    ch(&mut app, 'j');
    assert_eq!(cursor_tag(&screen(&app)).as_deref(), Some("topic:beta"));
    key(&mut app, KeyCode::Enter);
    let text = screen(&app);
    assert_eq!(app.screen(), Screen::Runs);
    assert_eq!(listed(&text, &ids), [ids[2].clone()], "{text}");
    assert!(
        bottom_two(&text)
            .0
            .starts_with("1 of 4 runs match +topic:beta"),
        "{text}"
    );
}

#[test]
fn esc_clears_the_find_then_closes() {
    let (mut app, _) = small();
    ch(&mut app, 't');
    // `/` opens the find line: every character is its text, `q`, `d` and `t` too.
    assert!(ch(&mut app, '/').is_empty());
    type_in(&mut app, "alp");
    let text = screen(&app);
    assert_eq!(table(&text), [pair("topic:alpha", 2)], "{text}");
    assert_eq!(bottom_two(&text).1, "/alp", "{text}");
    // Enter keeps the text and closes the line: the table stays cut.
    key(&mut app, KeyCode::Enter);
    let text = screen(&app);
    assert_eq!(table(&text), [pair("topic:alpha", 2)], "{text}");
    assert_ne!(bottom_two(&text).1, "/alp", "{text}");
    assert_eq!(app.screen(), Screen::Runs);
    // Esc clears the find text and keeps the overlay open.
    key(&mut app, KeyCode::Esc);
    let text = screen(&app);
    assert_eq!(table(&text).len(), 3, "{text}");
    assert!(text.starts_with("ham \u{b7} Tags"), "{text}");
    // While the line is open, Esc clears the text and closes the line, and the overlay stays.
    ch(&mut app, '/');
    type_in(&mut app, "bet");
    key(&mut app, KeyCode::Esc);
    let text = screen(&app);
    assert_eq!(table(&text).len(), 3, "{text}");
    assert!(text.starts_with("ham \u{b7} Tags"), "{text}");
    // A second Esc closes it, and the Runs list is whole.
    key(&mut app, KeyCode::Esc);
    assert!(screen(&app).starts_with("ham \u{b7} Runs"));
    // The key `t` does what Esc does outside the find line.
    ch(&mut app, 't');
    ch(&mut app, 't');
    assert!(screen(&app).starts_with("ham \u{b7} Runs"));
}

#[test]
fn find_words_prefix_and_kind_and_picks_stay_visible() {
    let ids = [id(1), id(2), id(3), id(4), id(5)];
    let tags: [&[&str]; 5] = [
        &["topic:csv-export", "tech:python"],
        &["tech:csv", "project:billing"],
        &["topic:billing-csv"],
        &["tech:python"],
        &["project:billing", "tech:python"],
    ];
    let runs: Vec<RunSummary> = ids
        .iter()
        .enumerate()
        .map(|(n, i)| s3_only(i, &format!("2026-10-20T12:0{}:00Z", 9 - n)))
        .collect();
    let index = ids
        .iter()
        .zip(tags)
        .map(|(i, t)| entry(i, Some(t)))
        .collect();
    let mut app = app_with(runs, index);
    ch(&mut app, 't');
    let shown = |app: &ham::tui::App| -> Vec<String> {
        let mut v: Vec<String> = table(&screen(app)).into_iter().map(|(t, _)| t).collect();
        v.sort();
        v
    };
    ch(&mut app, '/');
    type_in(&mut app, "csv");
    assert_eq!(
        shown(&app),
        ["tech:csv", "topic:billing-csv", "topic:csv-export"]
    );
    // Two words must both occur in the tag; case does not matter.
    ch(&mut app, ' ');
    type_in(&mut app, "EXPORT");
    assert_eq!(shown(&app), ["topic:csv-export"]);
    key(&mut app, KeyCode::Esc);
    // A word that starts with ^ must start the tag.
    ch(&mut app, '/');
    type_in(&mut app, "^topic:csv");
    assert_eq!(shown(&app), ["topic:csv-export"]);
    key(&mut app, KeyCode::Esc);
    // The kind with its colon lists one kind.
    ch(&mut app, '/');
    type_in(&mut app, "project:");
    assert_eq!(shown(&app), ["project:billing"]);
    key(&mut app, KeyCode::Esc);
    // A picked tag that the find would hide stays on top.
    ch(&mut app, '/');
    type_in(&mut app, "py");
    assert_eq!(shown(&app), ["tech:python"]);
    key(&mut app, KeyCode::Enter);
    ch(&mut app, ' ');
    key(&mut app, KeyCode::Esc);
    ch(&mut app, '/');
    type_in(&mut app, "billing");
    let text = screen(&app);
    let rows = rows(&text);
    assert_eq!(rows[0].tag, "tech:python", "the pick stays first: {text}");
    assert!(rows[0].picked);
    assert!(rows.iter().any(|r| r.tag == "project:billing"), "{text}");
}

#[test]
fn a_refresh_while_open_keeps_the_cursor_on_its_tag() {
    let (mut app, ids) = small();
    let all: Vec<RunSummary> = ids
        .iter()
        .enumerate()
        .map(|(n, i)| s3_only(i, &format!("2026-10-20T12:0{}:00Z", 3 - n)))
        .collect();
    ch(&mut app, 't');
    ch(&mut app, 'j');
    assert_eq!(cursor_tag(&screen(&app)).as_deref(), Some("topic:alpha"));
    // The timer's read of the newest page brings new tags: `tech:go` is the most used and comes first; the cursor stays on `topic:alpha`.
    read_first(
        &mut app,
        all.clone(),
        false,
        vec![
            entry(&ids[0], Some(&["tech:go", "tech:rust", "topic:alpha"])),
            entry(&ids[1], Some(&["tech:go", "topic:alpha"])),
            entry(&ids[2], Some(&["tech:go", "tech:rust", "topic:beta"])),
            entry(&ids[3], Some(&["tech:go"])),
        ],
    );
    let text = screen(&app);
    assert_eq!(
        table(&text)[0],
        pair("tech:go", 4),
        "the counts are the new ones: {text}"
    );
    assert_eq!(cursor_tag(&text).as_deref(), Some("topic:alpha"), "{text}");
    assert!(
        text.starts_with("ham \u{b7} Tags"),
        "the overlay stays open: {text}"
    );
    // The tag under the cursor vanishes: the cursor goes to the nearest row.
    read_first(
        &mut app,
        all,
        false,
        vec![
            entry(&ids[0], Some(&["tech:go", "tech:rust"])),
            entry(&ids[1], Some(&["tech:go"])),
            entry(&ids[2], Some(&["tech:go", "tech:rust", "topic:beta"])),
            entry(&ids[3], Some(&["tech:go"])),
        ],
    );
    let text = screen(&app);
    let rows = rows(&text);
    assert_eq!(rows.iter().filter(|r| r.cursor).count(), 1, "{text}");
    let at = rows.iter().position(|r| r.cursor).unwrap();
    assert!(at < rows.len(), "{text}");
}

#[test]
fn the_index_is_built_again_only_when_the_held_rows_changed() {
    let (mut app, ids) = small();
    ch(&mut app, 't');
    let built = app.tag_index_builds();
    let all: Vec<RunSummary> = ids
        .iter()
        .enumerate()
        .map(|(n, i)| s3_only(i, &format!("2026-10-20T12:0{}:00Z", 3 - n)))
        .collect();
    // The same rows again (the timer, every five seconds): nothing is built.
    read_first(
        &mut app,
        all.clone(),
        false,
        vec![
            entry(&ids[0], Some(&["tech:rust", "topic:alpha"])),
            entry(&ids[1], Some(&["topic:alpha"])),
            entry(&ids[2], Some(&["tech:rust", "topic:beta"])),
            entry(&ids[3], Some(&[])),
        ],
    );
    assert_eq!(app.tag_index_builds(), built);
    // A changed tag builds it once.
    read_first(
        &mut app,
        all,
        false,
        vec![
            entry(&ids[0], Some(&["tech:rust", "topic:alpha", "tech:go"])),
            entry(&ids[1], Some(&["topic:alpha"])),
            entry(&ids[2], Some(&["tech:rust", "topic:beta"])),
            entry(&ids[3], Some(&[])),
        ],
    );
    assert_eq!(app.tag_index_builds(), built + 1);
}

#[test]
fn ctrl_l_is_left_to_the_global_handler() {
    let (mut app, _) = small();
    ch(&mut app, 't');
    assert!(!app.take_repaint());
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
    assert!(app.take_repaint(), "Ctrl-L asks for the repaint everywhere");
    assert!(
        screen(&app).starts_with("ham \u{b7} Tags"),
        "the overlay stays open"
    );
}

#[test]
fn ctrl_c_quits_from_the_explorer_as_everywhere() {
    let (mut app, _) = small();
    ch(&mut app, 't');
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(effects.contains(&Effect::Quit), "{effects:?}");
}

#[test]
fn the_explorer_takes_every_other_key_and_the_runs_list_under_it_hears_none() {
    let (mut app, ids) = small();
    ch(&mut app, 't');
    // `q` quits on the Runs list; here it does nothing. `n` opens the composer there; here it does nothing. Enter on a row would open a run.
    assert!(ch(&mut app, 'q').is_empty());
    assert!(ch(&mut app, 'n').is_empty());
    assert!(ch(&mut app, 'p').is_empty());
    assert!(ch(&mut app, 's').is_empty());
    assert_eq!(app.screen(), Screen::Runs);
    let text = screen(&app);
    assert!(text.starts_with("ham \u{b7} Tags"), "{text}");
    assert!(
        listed(&text, &ids).is_empty(),
        "no run row shows through: {text}"
    );
}

#[test]
fn a_paste_goes_into_the_find_line_and_is_ignored_everywhere_else_in_the_explorer() {
    let (mut app, _) = small();
    ch(&mut app, 't');
    // Not in the find line: a paste is text and never keys, so `d` and `q` in it do nothing.
    assert!(app.handle_paste("dq\nr".to_string()).is_empty());
    let text = screen(&app);
    assert_eq!(table(&text).len(), 3, "{text}");
    assert!(text.starts_with("ham \u{b7} Tags"), "{text}");
    // In the find line it is the text, on one line, cleaned.
    ch(&mut app, '/');
    assert!(app.handle_paste("al\np\u{1b}[31mh".to_string()).is_empty());
    let text = screen(&app);
    assert_eq!(table(&text), [pair("topic:alpha", 2)], "{text}");
    assert_eq!(bottom_two(&text).1, "/al ph", "{text}");
}

// ---- remove everywhere ------------------------------------------------------------------------------------------------------------

/// 85 runs that hold `topic:old` (every third also `tech:keep`) and 3 that hold only `tech:keep`.
fn many() -> (ham::tui::App, Vec<String>) {
    let mut runs = Vec::new();
    let mut index = Vec::new();
    let mut old = Vec::new();
    for n in 0..85usize {
        let run = id(n);
        runs.push(s3_only(&run, "2026-10-20T12:00:00Z"));
        let tags: &[&str] = if n % 3 == 0 {
            &["topic:old", "tech:keep"]
        } else {
            &["topic:old"]
        };
        index.push(entry(&run, Some(tags)));
        old.push(run);
    }
    for n in 85..88usize {
        let run = id(n);
        runs.push(s3_only(&run, "2026-10-19T12:00:00Z"));
        index.push(entry(&run, Some(&["tech:keep"])));
    }
    (app_with(runs, index), old)
}

#[tokio::test]
async fn remove_asks_then_sends_batches_of_forty() {
    let (mut app, old) = many();
    ch(&mut app, 't');
    assert_eq!(cursor_tag(&screen(&app)).as_deref(), Some("topic:old"));
    // `d` asks; `n` cancels and sends nothing.
    assert!(ch(&mut app, 'd').is_empty());
    assert_eq!(
        bottom_two(&screen(&app)).0,
        "Remove topic:old from 85 runs? y/n"
    );
    assert!(ch(&mut app, 'n').is_empty());
    let text = screen(&app);
    assert!(!bottom_two(&text).0.contains("Remove"), "{text}");
    assert!(table(&text).contains(&pair("topic:old", 85)), "{text}");
    // `d` then `y` sends the batches of the held runs that hold the tag, in order, 40, 40 and 5.
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'y');
    let [Effect::EditTags { origin, batches }] = effects.as_slice() else {
        panic!("one effect, got {effects:?}");
    };
    assert_eq!(
        origin,
        &TagsOrigin::Remove {
            tag: "topic:old".into()
        }
    );
    assert_eq!(
        batches.iter().map(|b| b.edits.len()).collect::<Vec<_>>(),
        [40, 40, 5]
    );
    let sent: Vec<&str> = batches
        .iter()
        .flat_map(|b| b.edits.iter().map(|e| e.run_id.as_str()))
        .collect();
    assert_eq!(sent, old.iter().map(String::as_str).collect::<Vec<_>>());
    for e in batches.iter().flat_map(|b| &b.edits) {
        assert!(e.add.is_empty() && e.remove == ["topic:old"], "{e:?}");
    }
    // The stand-in transport answers each batch: every run took the edit and keeps what else it held.
    let (api, plane) = tagsfx::api();
    let held: std::collections::HashSet<String> =
        (0..85usize).filter(|n| n % 3 == 0).map(id).collect();
    for b in batches {
        let req = serde_json::to_value(b).unwrap();
        plane.push(
            200,
            all_ok(&req, |run| {
                if held.contains(run) {
                    vec!["tech:keep".to_string()]
                } else {
                    Vec::new()
                }
            }),
        );
    }
    let n = perform_edits(&mut app, &api, &effects).await;
    assert_eq!(n, 1);
    let calls = plane.requests();
    assert_eq!(calls.len(), 3, "{calls:?}");
    for (call, want) in calls.iter().zip([40, 40, 5]) {
        assert_eq!(call.0, "POST");
        assert_eq!(call.1, "/v1/tags/edit");
        assert_eq!(call.2["edits"].as_array().unwrap().len(), want);
    }
    // The held rows took the answers: the tag is gone from the table without a reload, and the note says so.
    let text = screen(&app);
    assert!(
        !table(&text).iter().any(|(t, _)| t == "topic:old"),
        "{text}"
    );
    assert!(
        table(&text).contains(&pair("tech:keep", 32)),
        "29 of the 85 and the 3 others: {text}"
    );
    assert_eq!(bottom_two(&text).0, "removed topic:old from 85 runs");
}

#[tokio::test]
async fn remove_reports_a_partial_failure_and_a_missing_route() {
    // 40 runs hold the tag; the server changes 37 and refuses 3 with a conflict.
    let mut runs = Vec::new();
    let mut index = Vec::new();
    for n in 0..40usize {
        let run = id(n);
        runs.push(s3_only(&run, "2026-10-20T12:00:00Z"));
        index.push(entry(&run, Some(&["topic:old"])));
    }
    let mut app = app_with(runs, index);
    ch(&mut app, 't');
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'y');
    let (api, plane) = tagsfx::api();
    let mut results: Vec<serde_json::Value> = (0..37usize)
        .map(|n| json!({"runId": id(n), "status": "ok", "tags": []}))
        .collect();
    results
        .extend((37..40usize).map(|n| json!({"runId": id(n), "status": "conflict", "tags": []})));
    plane.push(200, json!({ "results": results }));
    perform_edits(&mut app, &api, &effects).await;
    let text = screen(&app);
    assert!(
        bottom_two(&text).0.starts_with("37 of 40 runs changed"),
        "{text}"
    );
    assert!(
        table(&text).contains(&pair("topic:old", 3)),
        "the three the server kept: {text}"
    );
    // A stage without the route answers 404: the note says so and nothing in the rows moves.
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'y');
    plane.push(
        404,
        json!({"code": "not-found", "message": "no such route"}),
    );
    perform_edits(&mut app, &api, &effects).await;
    let text = screen(&app);
    assert!(
        bottom_two(&text)
            .0
            .contains("this service cannot edit tags yet"),
        "{text}"
    );
    assert!(table(&text).contains(&pair("topic:old", 3)), "{text}");
}

#[tokio::test]
async fn a_failed_batch_stops_the_rest_and_the_rows_keep_what_was_confirmed() {
    let (mut app, _) = many();
    ch(&mut app, 't');
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'y');
    let (api, plane) = tagsfx::api();
    let held: std::collections::HashSet<String> =
        (0..85usize).filter(|n| n % 3 == 0).map(id).collect();
    let first = {
        let [Effect::EditTags { batches, .. }] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        serde_json::to_value(&batches[0]).unwrap()
    };
    plane.push(
        200,
        all_ok(&first, |run| {
            if held.contains(run) {
                vec!["tech:keep".to_string()]
            } else {
                Vec::new()
            }
        }),
    );
    plane.push(500, json!({"code": "internal", "message": "boom"}));
    perform_edits(&mut app, &api, &effects).await;
    // The second request failed as a whole: the third was never sent.
    assert_eq!(plane.requests().len(), 2, "{:?}", plane.requests());
    let text = screen(&app);
    assert!(
        bottom_two(&text).0.starts_with("40 of 85 runs changed"),
        "{text}"
    );
    // The first batch's rows lost the tag; the rest still hold it.
    assert!(table(&text).contains(&pair("topic:old", 45)), "{text}");
    // While it was out, `d` and `r` were refused; now the overlay is free again.
    let again = ch(&mut app, 'd');
    assert!(again.is_empty());
    assert!(
        bottom_two(&screen(&app))
            .0
            .starts_with("Remove topic:old from 45 runs")
    );
}

#[tokio::test]
async fn d_and_r_wait_while_a_removal_is_out() {
    let (mut app, _) = many();
    ch(&mut app, 't');
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'y');
    assert!(matches!(effects.as_slice(), [Effect::EditTags { .. }]));
    assert!(
        ch(&mut app, 'd').is_empty(),
        "no second question while one is out"
    );
    let text = screen(&app);
    assert!(
        !bottom_two(&text).0.starts_with("Remove"),
        "no second question is asked while one removal is out: {text}"
    );
    assert!(
        bottom_two(&text).0.contains("wait"),
        "the note says to wait: {text}"
    );
    assert!(ch(&mut app, 'r').is_empty(), "no reload while one is out");
    let (api, plane) = tagsfx::api();
    plane.push(500, json!({"code": "internal", "message": "boom"}));
    perform_edits(&mut app, &api, &effects).await;
    // Free again: `r` asks for every page.
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
}

#[test]
fn y_in_capitals_confirms_and_any_other_key_cancels_the_question_and_is_consumed() {
    let (mut app, _) = many();
    ch(&mut app, 't');
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'Y');
    assert!(
        matches!(effects.as_slice(), [Effect::EditTags { .. }]),
        "{effects:?}"
    );
    let (mut app, _) = many();
    ch(&mut app, 't');
    ch(&mut app, 'd');
    // A letter that is not y cancels the question; the letter does nothing else (here `j` would have moved the cursor).
    assert!(ch(&mut app, 'j').is_empty());
    let text = screen(&app);
    assert!(!bottom_two(&text).0.contains("Remove"), "{text}");
    assert_eq!(cursor_tag(&text).as_deref(), Some("topic:old"), "{text}");
    ch(&mut app, 'd');
    assert!(key(&mut app, KeyCode::Esc).is_empty());
    let text = screen(&app);
    assert!(
        text.starts_with("ham \u{b7} Tags"),
        "Esc cancels the question and leaves the overlay open: {text}"
    );
    assert!(!bottom_two(&text).0.contains("Remove"), "{text}");
}

#[test]
fn d_is_refused_until_every_run_is_loaded_and_r_asks_for_every_page() {
    let runs: Vec<RunSummary> = (0..100)
        .map(|n| s3_only(&id(n), "2026-10-20T12:00:00Z"))
        .collect();
    let index = (0..100)
        .map(|n| entry(&id(n), Some(&["topic:old"])))
        .collect();
    let mut app = ham::tui::App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
    app.set_now(unix(NOW));
    read_first(&mut app, runs.clone(), true, index);
    ch(&mut app, 't');
    assert!(ch(&mut app, 'd').is_empty());
    let text = screen(&app);
    assert!(
        bottom_two(&text).0.contains("press r"),
        "d says what to do first: {text}"
    );
    assert!(!bottom_two(&text).0.contains("Remove"), "{text}");
    // `r` reads every page; once they are applied `d` works.
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    let index = (0..100)
        .map(|n| entry(&id(n), Some(&["topic:old"])))
        .collect();
    read_all(&mut app, runs, index);
    ch(&mut app, 'd');
    assert_eq!(
        bottom_two(&screen(&app)).0,
        "Remove topic:old from 100 runs? y/n"
    );
}

#[tokio::test]
async fn a_read_that_was_in_flight_when_the_edit_landed_does_not_bring_the_tag_back() {
    let (mut app, _) = many();
    let all_runs: Vec<RunSummary> = (0..88usize)
        .map(|n| {
            s3_only(
                &id(n),
                if n < 85 {
                    "2026-10-20T12:00:00Z"
                } else {
                    "2026-10-19T12:00:00Z"
                },
            )
        })
        .collect();
    ch(&mut app, 't');
    // The 5 s timer asks for the newest page; its answer will be older than the edit that lands first.
    for _ in 0..5 {
        app.tick();
    }
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'y');
    let (api, plane) = tagsfx::api();
    for b in match effects.as_slice() {
        [Effect::EditTags { batches, .. }] => batches.clone(),
        other => panic!("{other:?}"),
    } {
        let req = serde_json::to_value(&b).unwrap();
        plane.push(200, all_ok(&req, |_| Vec::new()));
    }
    perform_edits(&mut app, &api, &effects).await;
    assert!(!table(&screen(&app)).iter().any(|(t, _)| t == "topic:old"));
    // The read that was out before the edit answers now with the tags it saw then.
    let stale: Vec<_> = (0..88usize)
        .map(|n| {
            let tags: &[&str] = if n < 85 {
                &["topic:old"]
            } else {
                &["tech:keep"]
            };
            entry(&id(n), Some(tags))
        })
        .collect();
    read_first(&mut app, all_runs.clone(), false, stale);
    let text = screen(&app);
    assert!(
        !table(&text).iter().any(|(t, _)| t == "topic:old"),
        "the removed tag did not come back: {text}"
    );
    // The next read is newer than the edit and is believed: a tag that the server says is there is there.
    let fresh: Vec<_> = (0..88usize)
        .map(|n| {
            let tags: &[&str] = if n == 0 { &["topic:old"] } else { &[] };
            entry(&id(n), Some(tags))
        })
        .collect();
    read_first(&mut app, all_runs, false, fresh);
    let text = screen(&app);
    assert!(table(&text).contains(&pair("topic:old", 1)), "{text}");
}

#[tokio::test]
async fn removing_a_picked_tag_that_no_held_run_holds_any_more_drops_the_pick() {
    let mut runs = Vec::new();
    let mut index = Vec::new();
    for n in 0..3usize {
        runs.push(s3_only(&id(n), "2026-10-20T12:00:00Z"));
        index.push(entry(&id(n), Some(&["topic:old", "tech:keep"])));
    }
    let mut app = app_with(runs, index);
    ch(&mut app, 't');
    // Pick `tech:keep` (the cursor is on the first row) and `topic:old`, then remove `topic:old`.
    ch(&mut app, ' ');
    ch(&mut app, 'j');
    ch(&mut app, ' ');
    assert_eq!(picked(&screen(&app)).len(), 2);
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'y');
    let (api, plane) = tagsfx::api();
    let req = match effects.as_slice() {
        [Effect::EditTags { batches, .. }] => serde_json::to_value(&batches[0]).unwrap(),
        other => panic!("{other:?}"),
    };
    plane.push(200, all_ok(&req, |_| vec!["tech:keep".to_string()]));
    perform_edits(&mut app, &api, &effects).await;
    let text = screen(&app);
    assert_eq!(picked(&text), ["tech:keep"], "{text}");
    assert_eq!(table(&text), [pair("tech:keep", 3)], "{text}");
}

#[test]
fn the_answer_is_counted_by_the_tags_it_carries() {
    // A run whose answer says `ok` but still lists the tag was not cleared; it is not counted as changed.
    let mut runs = Vec::new();
    let mut index = Vec::new();
    for n in 0..4usize {
        runs.push(s3_only(&id(n), "2026-10-20T12:00:00Z"));
        index.push(entry(&id(n), Some(&["topic:old"])));
    }
    let mut app = app_with(runs, index);
    ch(&mut app, 't');
    ch(&mut app, 'd');
    let effects = ch(&mut app, 'y');
    let [Effect::EditTags { origin, .. }] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    let results = vec![Ok(TagEditResponse {
        results: vec![
            TagEditResult::ok(&id(0), vec![]),
            TagEditResult::unchanged(&id(1), vec![]),
            TagEditResult::ok(&id(2), vec!["topic:old".to_string()]),
            TagEditResult::not_found(&id(3)),
        ],
    })];
    app.update(Msg::TagsEdited {
        origin: origin.clone(),
        results,
    });
    let text = screen(&app);
    assert!(
        bottom_two(&text).0.starts_with("2 of 4 runs changed"),
        "{text}"
    );
    assert!(table(&text).contains(&pair("topic:old", 2)), "{text}");
}

// ---- help lines -------------------------------------------------------------------------------------------------------------------

#[test]
fn help_lines_fit_80_columns() {
    let (mut app, _) = small();
    let text = screen_text(&app, 80, 24);
    // The `p` key names what a press does; the runs of this app have no repository, so the shortest wording that says so is `p s3-only`,
    // the same nine columns as `p project` took.
    let unfiltered = "n new run \u{b7} Enter open \u{b7} \u{2191}\u{2193} \u{b7} / find \u{b7} t tags \u{b7} p s3-only \u{b7} r refresh \u{b7} q quit";
    assert_eq!(unfiltered.chars().count(), 78);
    assert_eq!(bottom_two(&text).1, unfiltered, "{text}");
    ch(&mut app, '/');
    type_in(&mut app, "alpha");
    key(&mut app, KeyCode::Enter);
    let text = screen_text(&app, 80, 24);
    let filtered = "Enter open \u{b7} \u{2191}\u{2193} \u{b7} / edit \u{b7} Esc clears \u{b7} t tags \u{b7} p s3-only \u{b7} r refresh \u{b7} q quit";
    assert_eq!(filtered.chars().count(), 79);
    assert_eq!(bottom_two(&text).1, filtered, "{text}");
    // `s settings` joins from 91 columns, where the longer line still holds `q quit`.
    let (app, _) = small();
    assert!(
        !bottom_two(&screen_text(&app, 90, 24))
            .1
            .contains("s settings")
    );
    let wide = bottom_two(&screen_text(&app, 91, 24)).1;
    assert!(
        wide.contains("t tags \u{b7} p s3-only \u{b7} s settings \u{b7} r refresh \u{b7} q quit"),
        "{wide:?}"
    );
    // The explorer's own line.
    let (mut app, _) = small();
    ch(&mut app, 't');
    assert_eq!(
        bottom_two(&screen_text(&app, 80, 24)).1,
        "Space pick \u{b7} Enter show runs \u{b7} / find \u{b7} d remove \u{b7} r reload \u{b7} Esc back"
    );
}

// ---- the other readers of a run's words ---------------------------------------------------------------------------------------------

#[test]
fn the_open_anything_box_finds_a_run_by_a_word_of_its_tag() {
    let (mut app, ids) = small();
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    type_in(&mut app, "beta");
    let text = screen(&app);
    assert!(text.contains(&ids[2]), "{text}");
    assert!(!text.contains(&ids[1]), "{text}");
}
