//! The Runs list that scrolls (the window of `runs_view`, PageUp, PageDown, Home, End, `g`, `G`, the wheel, a click on a scrolled row) and the
//! run tags (the note under the table, the explorer on `t`, `p`) on one screen. Each side has its own tests; these are the ones that need both: a
//! list of 200 runs that carry tags, a window that follows the marker, and the tags that belong to the run the marker is on, never to the
//! first run of the list or to the first row of the window.
//!
//! The screen is 100 x 30, so the table has 24 rows for runs. The 200 runs are `id(1)` (newest, first in the list) to `id(200)`. Odd runs are
//! in the repository `odd` and hold `tech:rust`; every third run holds `topic:alpha`; each run holds `topic:r<its number>`, which names it.
#![allow(clippy::unwrap_used)]

mod tagsfx;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::{App, Effect};
use ham_core::runfilter::IndexEntry;
use harness_protocol::api::v1::RunSummary;
use std::time::{Duration, Instant};
use tagsfx::{ch, cursor_tag, entry, id, key, repo, screen, table};

/// The rows of the table at 100 x 30: 30 less the title, the status row and the help row, less the border and the header of the box.
const WINDOW: usize = 24;
const SLOW: Duration = Duration::from_millis(400);

fn tags_of(n: usize) -> Vec<String> {
    let mut tags = vec![
        format!("topic:r{n}"),
        format!("project:{}", if n % 2 == 1 { "odd" } else { "even" }),
    ];
    if n % 2 == 1 {
        tags.push("tech:rust".to_string());
    }
    if n.is_multiple_of(3) {
        tags.push("topic:alpha".to_string());
    }
    tags
}

fn run(n: usize) -> RunSummary {
    let repo_name = if n % 2 == 1 { "odd" } else { "even" };
    repo(&id(n), "2026-10-16T08:00:00Z", repo_name)
}

fn index_entry(n: usize) -> IndexEntry {
    let tags = tags_of(n);
    let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
    entry(&id(n), Some(refs.as_slice()))
}

/// Runs `from` to `to`, both included, in that order.
fn runs(from: usize, to: usize) -> Vec<RunSummary> {
    (from..=to).map(run).collect()
}

fn index(from: usize, to: usize) -> Vec<IndexEntry> {
    (from..=to).map(index_entry).collect()
}

fn two_hundred() -> App {
    let app = tagsfx::app_with(runs(1, 200), index(1, 200));
    // The event loop draws after every event, so a key never reaches a screen that was not drawn: a page needs the height of the window.
    let _ = screen(&app);
    app
}

/// The numbers of the runs on screen, top to bottom.
fn shown(text: &str) -> Vec<usize> {
    let prefix = id(0);
    let prefix = &prefix[..prefix.len() - 8];
    text.lines()
        .filter_map(|l| {
            let at = l.find(prefix)?;
            l[at + prefix.len()..][..8].parse().ok()
        })
        .collect()
}

/// The number of the run the marker `▶` stands before, if the marker is on screen.
fn marked(text: &str) -> Option<usize> {
    text.lines()
        .find(|l| l.contains('▶'))
        .and_then(|l| shown(l).first().copied())
}

fn range(from: usize, to: usize) -> Vec<usize> {
    (from..=to).collect()
}

/// The tags in the note row (` · tags a b c`), or none when the note has no tags part. The note is the second row from the bottom.
fn note_tags(text: &str) -> Vec<String> {
    let note = text
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .nth(1)
        .unwrap_or_default();
    let Some(at) = note.find(" · tags ") else {
        return vec![];
    };
    let rest = &note[at + " · tags ".len()..];
    let rest = rest.split(" · ").next().unwrap_or_default();
    rest.split_whitespace().map(str::to_string).collect()
}

fn press(app: &mut App, code: KeyCode, times: usize) {
    for _ in 0..times {
        assert!(key(app, code).is_empty());
    }
}

fn mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16, at: Instant) -> Vec<Effect> {
    app.handle_mouse_at(
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        at,
    )
}

fn click(app: &mut App, x: u16, y: u16, at: Instant) -> Vec<Effect> {
    let mut effects = mouse(app, MouseEventKind::Down(MouseButton::Left), x, y, at);
    effects.extend(mouse(app, MouseEventKind::Up(MouseButton::Left), x, y, at));
    effects
}

/// The column and row where `needle` starts on the screen text.
fn pos_of(text: &str, needle: &str) -> (u16, u16) {
    for (y, line) in text.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            return (line[..at].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} is not on the screen:\n{text}");
}

fn wheel(app: &mut App, kind: MouseEventKind, notches: usize, t0: Instant) -> Instant {
    let mut at = t0;
    for _ in 0..notches {
        mouse(app, kind, 20, 10, at);
        at += SLOW;
    }
    at
}

// ---- the window and the tags of the marked run --------------------------------------------------------------------------------------

#[test]
fn the_first_screen_is_a_window_of_the_tagged_list_and_the_note_holds_the_tags_of_the_first_run() {
    // The trigger: more runs than rows, every run with tags.
    let app = two_hundred();
    let text = screen(&app);
    assert_eq!(shown(&text), range(1, WINDOW), "{text}");
    assert_eq!(marked(&text), Some(1), "{text}");
    let tags = note_tags(&text);
    assert!(tags.contains(&"topic:r1".to_string()), "{tags:?}\n{text}");
    assert!(!tags.contains(&"topic:r2".to_string()), "{tags:?}");
}

#[test]
fn down_a_hundred_runs_and_the_note_holds_the_tags_of_the_101st_run() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(101), "{text}");
    assert_eq!(shown(&text), range(78, 101), "{text}");
    let tags = note_tags(&text);
    // The tags of the marked run: its own, not those of the first run of the list (r1) or of the first row of the window (r78).
    assert!(tags.contains(&"topic:r101".to_string()), "{tags:?}\n{text}");
    assert!(
        tags.contains(&"tech:rust".to_string()),
        "run 101 is odd: {tags:?}"
    );
    assert!(
        tags.iter().all(|t| t != "topic:r1" && t != "topic:r78"),
        "{tags:?}"
    );
}

#[test]
fn page_down_through_a_tagged_list_scrolls_and_the_note_follows_the_marked_run() {
    let mut app = two_hundred();
    key(&mut app, KeyCode::PageDown);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(WINDOW), "{text}");
    assert_eq!(shown(&text), range(WINDOW, 2 * WINDOW - 1), "{text}");
    assert!(
        note_tags(&text).contains(&format!("topic:r{WINDOW}")),
        "{text}"
    );
    key(&mut app, KeyCode::PageDown);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(2 * WINDOW - 1), "{text}");
    assert!(
        note_tags(&text).contains(&format!("topic:r{}", 2 * WINDOW - 1)),
        "{text}"
    );
    // End, Home, and the same run the marker is on after PageUp: each note holds the tags of its own run.
    key(&mut app, KeyCode::End);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(200), "{text}");
    assert_eq!(shown(&text), range(177, 200), "{text}");
    assert!(
        note_tags(&text).contains(&"topic:r200".to_string()),
        "{text}"
    );
    key(&mut app, KeyCode::Home);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(1), "{text}");
    assert!(note_tags(&text).contains(&"topic:r1".to_string()), "{text}");
}

#[test]
fn a_click_on_a_scrolled_row_of_a_tagged_list_selects_then_opens_the_run_that_row_shows() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let text = screen(&app);
    assert_eq!(shown(&text), range(78, 101), "setup: {text}");
    let (x, y) = pos_of(&text, &id(90));
    let t0 = Instant::now();
    assert!(click(&mut app, x + 4, y, t0).is_empty());
    let text = screen(&app);
    assert_eq!(marked(&text), Some(90), "{text}");
    assert_eq!(
        shown(&text),
        range(78, 101),
        "a click moves no row:\n{text}"
    );
    assert!(
        note_tags(&text).contains(&"topic:r90".to_string()),
        "{text}"
    );
    let effects = click(&mut app, x + 4, y, t0 + Duration::from_millis(200));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Poll { run_id, .. } if *run_id == id(90))),
        "{effects:?}"
    );
}

#[test]
fn a_refresh_of_a_tagged_scrolled_list_keeps_the_marked_run_and_shows_its_new_tags() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    // Five newer runs arrive at the top, and the marked run gets one more tag.
    let mut list = runs(201, 205);
    list.reverse();
    list.extend(runs(1, 200));
    let mut idx = index(1, 205);
    let marked_entry = idx.iter_mut().find(|e| e.run_id == id(101)).unwrap();
    marked_entry
        .tags
        .as_mut()
        .unwrap()
        .push("topic:late".into());
    tagsfx::read_all(&mut app, list, idx);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(101), "{text}");
    assert!(shown(&text).contains(&101), "{text}");
    assert_eq!(shown(&text).len(), WINDOW, "{text}");
    assert!(
        note_tags(&text).contains(&"topic:late".to_string()),
        "{text}"
    );
}

// ---- the explorer over a scrolled list ----------------------------------------------------------------------------------------------

#[test]
fn the_explorer_opened_on_a_scrolled_list_gives_the_same_screen_back_on_esc() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let before = screen(&app);
    assert_eq!(marked(&before), Some(101), "setup: {before}");
    ch(&mut app, 't');
    let open = screen(&app);
    assert!(open.contains("ham · Tags"), "{open}");
    // The table counts the held runs, not the window: `tech:rust` is on 100 of the 200.
    assert!(
        table(&open).contains(&("tech:rust".to_string(), 100)),
        "{open}"
    );
    assert!(open.contains("all 200 runs loaded at"), "{open}");
    // Move in the explorer, pick a tag, take it away again, then leave.
    press(&mut app, KeyCode::Down, 3);
    ch(&mut app, ' ');
    ch(&mut app, ' ');
    key(&mut app, KeyCode::Esc);
    let after = screen(&app);
    assert_eq!(after, before, "the Runs list is as it was");
}

#[test]
fn t_again_leaves_the_explorer_as_esc_does() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let before = screen(&app);
    ch(&mut app, 't');
    ch(&mut app, 't');
    assert_eq!(screen(&app), before);
}

#[test]
fn keys_and_the_wheel_in_the_explorer_do_not_move_the_runs_window() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let before = screen(&app);
    ch(&mut app, 't');
    let open = screen(&app);
    let cursor = cursor_tag(&open);
    // The list keys are the explorer's own while it is open: none of them reaches the Runs list.
    for code in [
        KeyCode::PageDown,
        KeyCode::PageUp,
        KeyCode::End,
        KeyCode::Home,
    ] {
        key(&mut app, code);
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
    ch(&mut app, 'g');
    let t0 = Instant::now();
    wheel(&mut app, MouseEventKind::ScrollDown, 6, t0);
    let moved = screen(&app);
    assert!(
        moved.contains("ham · Tags"),
        "the explorer stays up:\n{moved}"
    );
    assert!(
        cursor_tag(&moved) != cursor || cursor.is_none(),
        "the wheel moves the explorer's cursor: {cursor:?}\n{moved}"
    );
    key(&mut app, KeyCode::Esc);
    assert_eq!(screen(&app), before, "the Runs list did not move");
}

#[test]
fn a_pick_applied_from_a_scrolled_list_keeps_the_marked_run_on_screen_and_esc_brings_the_whole_list_back_around_it()
 {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    ch(&mut app, 't');
    // `tech:rust` is held by the 100 odd runs, and run 101 is odd. Move the cursor to it and pick it.
    let mut guard = 0;
    while cursor_tag(&screen(&app)).as_deref() != Some("tech:rust") {
        key(&mut app, KeyCode::Down);
        guard += 1;
        assert!(guard < 300, "tech:rust is not in the table");
    }
    ch(&mut app, ' ');
    key(&mut app, KeyCode::Enter);
    let text = screen(&app);
    assert!(text.contains("100 of 200 runs match +tech:rust"), "{text}");
    assert_eq!(marked(&text), Some(101), "{text}");
    assert_eq!(shown(&text).len(), WINDOW, "{text}");
    assert!(shown(&text).iter().all(|n| n % 2 == 1), "{text}");
    assert!(shown(&text).contains(&101), "{text}");
    // The window of the narrowed list is a window of 24 odd runs: PageDown moves 23 of them.
    key(&mut app, KeyCode::PageDown);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(101 + 2 * (WINDOW - 1)), "{text}");
    assert!(shown(&text).iter().all(|n| n % 2 == 1), "{text}");
    assert!(
        note_tags(&text).contains(&format!("topic:r{}", 101 + 2 * (WINDOW - 1))),
        "{text}"
    );
    // Esc clears the pick: the whole list is back and the marked run is on screen.
    key(&mut app, KeyCode::Esc);
    let text = screen(&app);
    let at = marked(&text).unwrap_or_else(|| panic!("no marker:\n{text}"));
    assert_eq!(at, 101 + 2 * (WINDOW - 1), "{text}");
    assert!(shown(&text).contains(&at), "{text}");
    assert_eq!(shown(&text).len(), WINDOW, "{text}");
}

#[test]
fn a_pick_that_leaves_the_marked_run_out_puts_the_window_on_the_nearest_run() {
    let mut app = two_hundred();
    // Run 100 is even; `tech:rust` is held by odd runs only.
    press(&mut app, KeyCode::Down, 99);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(100), "{text}");
    ch(&mut app, 't');
    let mut guard = 0;
    while cursor_tag(&screen(&app)).as_deref() != Some("tech:rust") {
        key(&mut app, KeyCode::Down);
        guard += 1;
        assert!(guard < 300, "tech:rust is not in the table");
    }
    ch(&mut app, ' ');
    key(&mut app, KeyCode::Enter);
    let text = screen(&app);
    let at = marked(&text).unwrap_or_else(|| panic!("no marker:\n{text}"));
    assert!(
        at % 2 == 1,
        "the marker is on a run that holds the tag: {at}\n{text}"
    );
    assert!(shown(&text).contains(&at), "{text}");
    assert_eq!(shown(&text).len(), WINDOW, "{text}");
}

// ---- the project scope with tags ----------------------------------------------------------------------------------------------------

#[test]
fn the_project_scope_with_tags_keeps_the_marked_run_on_screen_and_its_tags_in_the_note() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 150);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(151), "{text}");
    // Run 151 is odd: `p` shows the 100 odd runs.
    ch(&mut app, 'p');
    let text = screen(&app);
    assert_eq!(marked(&text), Some(151), "{text}");
    assert_eq!(shown(&text).len(), WINDOW, "{text}");
    assert!(shown(&text).iter().all(|n| n % 2 == 1), "{text}");
    assert!(
        note_tags(&text).contains(&"topic:r151".to_string()),
        "{text}"
    );
    // The explorer counts what is held, narrowed or not: the 100 even runs are not on the list, and their project is in the table.
    ch(&mut app, 't');
    let open = screen(&app);
    assert!(
        table(&open).contains(&("project:even".to_string(), 100)),
        "{open}"
    );
    key(&mut app, KeyCode::Esc);
    let back = screen(&app);
    assert_eq!(marked(&back), Some(151), "{back}");
    ch(&mut app, 'p');
    let text = screen(&app);
    assert_eq!(marked(&text), Some(151), "{text}");
    assert!(shown(&text).contains(&151), "{text}");
}

#[test]
fn a_new_read_while_the_explorer_is_open_leaves_the_runs_window_where_it_was() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let before = screen(&app);
    ch(&mut app, 't');
    // The 5 s timer reads the list again while the explorer is up: the same runs, the same tags.
    tagsfx::read_all(&mut app, runs(1, 200), index(1, 200));
    let open = screen(&app);
    assert!(open.contains("ham · Tags"), "{open}");
    key(&mut app, KeyCode::Esc);
    assert_eq!(screen(&app), before);
    // A read that sends one more run is a refresh the Runs list takes as it does with no explorer.
    let mut list = runs(201, 201);
    list.extend(runs(1, 200));
    let mut idx = index(201, 201);
    idx.extend(index(1, 200));
    tagsfx::read_all(&mut app, list, idx);
    let after = screen(&app);
    assert_eq!(marked(&after), Some(101), "{after}");
    assert!(after.contains("201 runs"), "{after}");
}
