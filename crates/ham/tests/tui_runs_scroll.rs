//! The Runs screen with more runs than rows.
//! The table is a window onto the list the filter
//! shows. The first row it draws moves the least amount that keeps the marked run on screen; Up, Down, `j` and `k` move one run, PageUp and
//! PageDown a window less one row, Home, End, `g` and `G` go to the ends, a wheel notch moves by the lines the notch carries, and a click on
//! a row selects the run that row shows, whatever the offset.
//!
//! The screen is 100 x 30: the table has 24 rows for runs (the box has two border rows and a header row). 200 runs, newest first, are
//! `R001` to `R200`; the tests read the screen text, never the offset.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::mouse::WheelSettings;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use std::time::{Duration, Instant};

const ARN: &str = "arn:aws:sts::123456789012:assumed-role/Tester/tester";
const PREFIX: &str = "01M5A00000000000000000R";
/// The rows of the table at 100 x 30: 30 less the title, the status row and the help row, less the border and the header of the box.
const WINDOW: usize = 24;
const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;

fn id(n: usize) -> String {
    format!("{PREFIX}{n:03}")
}

/// Run `n` (from 1). Odd runs are in the repository `odd`, even runs in `even`.
fn run(n: usize) -> RunSummary {
    let repo = if n % 2 == 1 { "odd" } else { "even" };
    serde_json::from_value(json!({
        "runId": id(n), "status": "complete", "phase": "planning",
        "target": {"mode": "repo", "repo": format!("cc:{repo}")},
        "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN,
    }))
    .unwrap()
}

/// Runs `from` to `to`, both included, in that order.
fn runs(from: usize, to: usize) -> Vec<RunSummary> {
    (from..=to).map(run).collect()
}

fn app_with(list: Vec<RunSummary>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(list)));
    assert_eq!(app.screen(), Screen::Runs);
    // The event loop draws after every event, so a key never reaches a screen that was not drawn: a page needs the height of the window.
    let _ = screen(&app);
    app
}

fn two_hundred() -> App {
    app_with(runs(1, 200))
}

fn draw(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        let mut x = 0;
        while x < w {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn screen(app: &App) -> String {
    draw(app, WIDTH, HEIGHT)
}

/// The numbers of the runs on screen, top to bottom.
fn shown(text: &str) -> Vec<usize> {
    text.lines()
        .filter_map(|l| {
            let at = l.find(PREFIX)?;
            l[at + PREFIX.len()..][..3].parse().ok()
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

fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn press(app: &mut App, code: KeyCode, times: usize) {
    for _ in 0..times {
        assert!(key(app, code).is_empty());
    }
}

fn ch(app: &mut App, c: char) -> Vec<Effect> {
    key(app, KeyCode::Char(c))
}

fn type_in(app: &mut App, text: &str) {
    for c in text.chars() {
        assert!(ch(app, c).is_empty(), "{c:?} caused an effect");
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

/// `notches` wheel notches over the table, `gap` apart (a gap of 400 ms is a slow wheel: no acceleration).
fn wheel(
    app: &mut App,
    kind: MouseEventKind,
    notches: usize,
    gap: Duration,
    t0: Instant,
) -> Instant {
    let mut at = t0;
    for _ in 0..notches {
        mouse(app, kind, 20, 10, at);
        at += gap;
    }
    at
}

const SLOW: Duration = Duration::from_millis(400);

/// The column and row where `needle` starts on the screen text.
fn pos_of(text: &str, needle: &str) -> (u16, u16) {
    for (y, line) in text.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            return (line[..at].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} is not on the screen:\n{text}");
}

fn click(app: &mut App, x: u16, y: u16, at: Instant) -> Vec<Effect> {
    let mut effects = mouse(app, MouseEventKind::Down(MouseButton::Left), x, y, at);
    effects.extend(mouse(app, MouseEventKind::Up(MouseButton::Left), x, y, at));
    effects
}

// ---- the arrow keys ------------------------------------------------------------------------------------------------------------

#[test]
fn the_first_screen_shows_a_window_of_the_list_with_the_marker_on_the_first_run() {
    // The trigger: the list is longer than the table, so something has to decide which runs show.
    let app = two_hundred();
    let text = screen(&app);
    assert_eq!(shown(&text), range(1, WINDOW), "{text}");
    assert_eq!(marked(&text), Some(1), "{text}");
}

#[test]
fn pressing_down_a_hundred_times_shows_the_marker_row_and_the_101st_run() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let text = screen(&app);
    assert_eq!(
        marked(&text),
        Some(101),
        "the marker is off screen:\n{text}"
    );
    assert!(shown(&text).contains(&101), "{text}");
    // The view moved the least it had to: the 101st run is the last row of the window.
    assert_eq!(shown(&text), range(78, 101), "{text}");
    // Enter opens the run the marker is on.
    let effects = key(&mut app, KeyCode::Enter);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Poll { run_id, .. } if *run_id == id(101))),
        "{effects:?}"
    );
}

#[test]
fn j_and_k_move_one_run_as_the_arrow_keys_do() {
    let mut app = two_hundred();
    for _ in 0..40 {
        ch(&mut app, 'j');
    }
    let text = screen(&app);
    assert_eq!(marked(&text), Some(41), "{text}");
    assert_eq!(shown(&text), range(18, 41), "{text}");
    for _ in 0..40 {
        ch(&mut app, 'k');
    }
    let text = screen(&app);
    assert_eq!(marked(&text), Some(1), "{text}");
    assert_eq!(shown(&text), range(1, WINDOW), "{text}");
}

#[test]
fn down_through_the_last_row_of_the_window_scrolls_by_exactly_one_row() {
    let mut app = two_hundred();
    let first = screen(&app);
    // 23 presses put the marker on the 24th and last row: nothing scrolls.
    press(&mut app, KeyCode::Down, WINDOW - 1);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(WINDOW), "{text}");
    assert_eq!(shown(&text), shown(&first), "{text}");
    // The 24th press goes one row under the window: the window moves by one.
    press(&mut app, KeyCode::Down, 1);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(WINDOW + 1), "{text}");
    assert_eq!(shown(&text), range(2, WINDOW + 1), "{text}");
}

#[test]
fn up_inside_the_window_does_not_move_the_view_and_up_above_it_moves_it_by_one_row() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, WINDOW);
    assert_eq!(shown(&screen(&app)), range(2, WINDOW + 1));
    // The marker walks up inside the window: the rows stay.
    press(&mut app, KeyCode::Up, WINDOW - 1);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(2), "{text}");
    assert_eq!(shown(&text), range(2, WINDOW + 1), "{text}");
    // One more is above the window.
    press(&mut app, KeyCode::Up, 1);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(1), "{text}");
    assert_eq!(shown(&text), range(1, WINDOW), "{text}");
}

#[test]
fn down_at_the_last_run_stays_and_the_window_shows_the_end_of_the_list() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 250);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(200), "{text}");
    assert_eq!(shown(&text), range(177, 200), "{text}");
}

// ---- pages and ends ------------------------------------------------------------------------------------------------------------

#[test]
fn page_down_and_page_up_move_the_selection_and_the_view_a_window_less_one_row() {
    let mut app = two_hundred();
    key(&mut app, KeyCode::PageDown);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(WINDOW), "{text}");
    assert_eq!(shown(&text), range(WINDOW, 2 * WINDOW - 1), "{text}");
    key(&mut app, KeyCode::PageDown);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(2 * WINDOW - 1), "{text}");
    assert_eq!(
        shown(&text),
        range(2 * WINDOW - 1, 3 * WINDOW - 2),
        "{text}"
    );
    key(&mut app, KeyCode::PageUp);
    key(&mut app, KeyCode::PageUp);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(1), "{text}");
    assert_eq!(shown(&text), range(1, WINDOW), "{text}");
}

#[test]
fn page_down_keeps_the_marker_on_its_row_of_the_window() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 10);
    let row = |text: &str| text.lines().position(|l| l.contains('▶')).unwrap();
    let before = row(&screen(&app));
    key(&mut app, KeyCode::PageDown);
    let text = screen(&app);
    assert_eq!(row(&text), before, "{text}");
    assert_eq!(marked(&text), Some(11 + WINDOW - 1), "{text}");
}

#[test]
fn page_down_stops_at_the_end_and_page_up_at_the_start() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::PageDown, 20);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(200), "{text}");
    assert_eq!(shown(&text), range(177, 200), "{text}");
    press(&mut app, KeyCode::PageUp, 20);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(1), "{text}");
    assert_eq!(shown(&text), range(1, WINDOW), "{text}");
}

#[test]
fn end_and_home_go_to_the_last_and_the_first_run() {
    let mut app = two_hundred();
    let first = screen(&app);
    key(&mut app, KeyCode::End);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(200), "{text}");
    assert_eq!(shown(&text), range(177, 200), "{text}");
    key(&mut app, KeyCode::Home);
    assert_eq!(screen(&app), first);
}

#[test]
fn g_and_capital_g_do_what_home_and_end_do() {
    let mut app = two_hundred();
    let first = screen(&app);
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
    let text = screen(&app);
    assert_eq!(marked(&text), Some(200), "{text}");
    assert_eq!(shown(&text), range(177, 200), "{text}");
    ch(&mut app, 'g');
    assert_eq!(screen(&app), first);
}

#[test]
fn g_with_control_or_alt_held_is_not_the_letter_and_does_nothing() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 5);
    let before = screen(&app);
    for (code, mods) in [
        (KeyCode::Char('g'), KeyModifiers::CONTROL),
        (KeyCode::Char('g'), KeyModifiers::ALT),
        (
            KeyCode::Char('G'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ),
    ] {
        assert!(app.handle_key(KeyEvent::new(code, mods)).is_empty());
        assert_eq!(
            screen(&app),
            before,
            "{code:?} with {mods:?} moved the selection"
        );
    }
}

// ---- the mouse wheel -----------------------------------------------------------------------------------------------------------

#[test]
fn a_wheel_notch_moves_the_selection_by_the_lines_of_the_notch() {
    let mut app = two_hundred();
    let t0 = Instant::now();
    mouse(&mut app, MouseEventKind::ScrollDown, 20, 10, t0);
    let text = screen(&app);
    assert_eq!(
        marked(&text),
        Some(4),
        "the default notch is 3 lines:\n{text}"
    );
    mouse(&mut app, MouseEventKind::ScrollUp, 20, 10, t0 + SLOW);
    assert_eq!(marked(&screen(&app)), Some(1));
}

#[test]
fn wheel_notches_scroll_the_table_to_the_marker_row_and_the_103rd_run() {
    let mut app = two_hundred();
    let before = screen(&app);
    wheel(
        &mut app,
        MouseEventKind::ScrollDown,
        34,
        SLOW,
        Instant::now(),
    );
    let text = screen(&app);
    assert_ne!(
        text, before,
        "34 wheel notches changed nothing on the screen"
    );
    assert_eq!(marked(&text), Some(1 + 34 * 3), "{text}");
    assert_eq!(shown(&text), range(80, 103), "{text}");
}

#[test]
fn ten_wheel_notches_change_what_the_screen_shows() {
    let mut app = two_hundred();
    let before = shown(&screen(&app));
    wheel(
        &mut app,
        MouseEventKind::ScrollDown,
        10,
        SLOW,
        Instant::now(),
    );
    let text = screen(&app);
    assert_ne!(shown(&text), before, "{text}");
    assert_eq!(marked(&text), Some(31), "{text}");
    assert!(shown(&text).contains(&31), "{text}");
}

#[test]
fn the_wheel_goes_back_up_the_way_it_came() {
    let mut app = two_hundred();
    let t = wheel(
        &mut app,
        MouseEventKind::ScrollDown,
        34,
        SLOW,
        Instant::now(),
    );
    wheel(&mut app, MouseEventKind::ScrollUp, 34, SLOW, t);
    let text = screen(&app);
    assert_eq!(marked(&text), Some(1), "{text}");
    assert_eq!(shown(&text), range(1, WINDOW), "{text}");
}

#[test]
fn the_wheel_step_is_the_setting() {
    let mut app = two_hundred();
    app.set_wheel(WheelSettings::new(5));
    mouse(&mut app, MouseEventKind::ScrollDown, 20, 10, Instant::now());
    assert_eq!(marked(&screen(&app)), Some(6));
}

#[test]
fn a_quick_run_of_notches_goes_further_than_a_slow_one() {
    let notches = 12;
    let mut slow = two_hundred();
    wheel(
        &mut slow,
        MouseEventKind::ScrollDown,
        notches,
        SLOW,
        Instant::now(),
    );
    let mut quick = two_hundred();
    wheel(
        &mut quick,
        MouseEventKind::ScrollDown,
        notches,
        Duration::from_millis(10),
        Instant::now(),
    );
    let slow_at = marked(&screen(&slow)).unwrap();
    let quick_at = marked(&screen(&quick)).unwrap();
    assert_eq!(slow_at, 1 + notches * 3);
    assert!(quick_at > slow_at, "quick {quick_at}, slow {slow_at}");
}

#[test]
fn the_wheel_stops_at_the_last_run() {
    let mut app = two_hundred();
    wheel(
        &mut app,
        MouseEventKind::ScrollDown,
        100,
        SLOW,
        Instant::now(),
    );
    let text = screen(&app);
    assert_eq!(marked(&text), Some(200), "{text}");
    assert_eq!(shown(&text), range(177, 200), "{text}");
}

// ---- clicks --------------------------------------------------------------------------------------------------------------------

#[test]
fn a_click_on_a_row_of_a_scrolled_view_selects_and_opens_the_run_that_row_shows() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let text = screen(&app);
    assert_eq!(
        shown(&text),
        range(78, 101),
        "setup: the view did not scroll:\n{text}"
    );
    let (x, y) = pos_of(&text, &id(90));
    let t0 = Instant::now();
    assert!(click(&mut app, x + 4, y, t0).is_empty());
    let text = screen(&app);
    assert_eq!(marked(&text), Some(90), "{text}");
    assert_eq!(
        shown(&text),
        range(78, 101),
        "a click inside the window moves no row:\n{text}"
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
fn a_click_on_the_first_and_on_the_last_row_of_a_scrolled_view_selects_those_runs() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let text = screen(&app);
    assert_eq!(
        shown(&text),
        range(78, 101),
        "setup: the view did not scroll:\n{text}"
    );
    let t0 = Instant::now();
    for (n, at) in [(78, t0), (101, t0 + SLOW), (78, t0 + 2 * SLOW)] {
        let text = screen(&app);
        let (x, y) = pos_of(&text, &id(n));
        click(&mut app, x + 4, y, at);
        assert_eq!(marked(&screen(&app)), Some(n), "{n}");
    }
}

#[test]
fn a_click_below_the_last_run_of_a_short_scrolled_list_does_nothing() {
    let mut app = app_with(runs(1, 30));
    key(&mut app, KeyCode::End);
    let text = screen(&app);
    assert_eq!(shown(&text), range(7, 30), "{text}");
    let (x, y) = pos_of(&text, &id(30));
    let effects = click(&mut app, x + 4, y + 1, Instant::now());
    assert!(effects.is_empty());
    assert_eq!(marked(&screen(&app)), Some(30));
}

// ---- the list changes under the view --------------------------------------------------------------------------------------------

#[test]
fn a_filter_that_shrinks_the_list_pulls_the_view_back_to_its_runs() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 150);
    assert_eq!(marked(&screen(&app)), Some(151));
    ch(&mut app, '/');
    type_in(&mut app, "R19");
    key(&mut app, KeyCode::Enter);
    let text = screen(&app);
    // Ten runs match, R190 to R199; the window starts at the first of them.
    assert_eq!(shown(&text), range(190, 199), "{text}");
    assert!(
        marked(&text).is_some(),
        "the marker is on one of them:\n{text}"
    );
    // Esc brings the whole list back, and the marked run is on screen.
    key(&mut app, KeyCode::Esc);
    let text = screen(&app);
    let at = marked(&text).unwrap_or_else(|| panic!("no marker:\n{text}"));
    assert!(shown(&text).contains(&at), "{text}");
    assert_eq!(shown(&text).len(), WINDOW, "{text}");
}

#[test]
fn the_project_scope_keeps_the_selected_run_on_screen_when_the_list_shrinks() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 150);
    assert_eq!(marked(&screen(&app)), Some(151));
    // Run 151 is odd: `p` shows the 100 odd runs, and run 151 stands at position 75 of them.
    ch(&mut app, 'p');
    let text = screen(&app);
    assert_eq!(marked(&text), Some(151), "{text}");
    assert_eq!(shown(&text).len(), WINDOW, "{text}");
    assert!(shown(&text).iter().all(|n| n % 2 == 1), "{text}");
    ch(&mut app, 'p');
    let text = screen(&app);
    assert_eq!(marked(&text), Some(151), "{text}");
    assert!(shown(&text).contains(&151), "{text}");
}

#[test]
fn a_refresh_keeps_the_selection_on_its_run_and_the_run_on_screen() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    // Five newer runs arrive at the top of the list.
    let mut list = runs(201, 205);
    list.reverse();
    list.extend(runs(1, 200));
    app.update(Msg::Runs(Ok(list)));
    let text = screen(&app);
    assert_eq!(marked(&text), Some(101), "{text}");
    assert!(shown(&text).contains(&101), "{text}");
    assert_eq!(shown(&text).len(), WINDOW, "{text}");
}

#[test]
fn a_refresh_that_leaves_the_selected_run_out_puts_the_view_on_the_nearest_run() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    app.update(Msg::Runs(Ok(runs(1, 30))));
    let text = screen(&app);
    // The old position is kept inside the new list: the last run. The table is not left blank.
    assert_eq!(marked(&text), Some(30), "{text}");
    assert_eq!(shown(&text), range(7, 30), "{text}");
}

#[test]
fn a_taller_terminal_fills_the_table_to_the_last_run() {
    let mut app = two_hundred();
    key(&mut app, KeyCode::End);
    assert_eq!(shown(&draw(&app, WIDTH, HEIGHT)), range(177, 200));
    // 20 more rows: the window is 44 rows tall and starts 20 runs earlier.
    let text = draw(&app, WIDTH, HEIGHT + 20);
    assert_eq!(shown(&text), range(157, 200), "{text}");
    assert_eq!(marked(&text), Some(200), "{text}");
}

#[test]
fn a_shorter_terminal_keeps_the_marker_on_screen() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let text = draw(&app, WIDTH, 20);
    // 20 rows leave 14 for runs.
    assert_eq!(marked(&text), Some(101), "{text}");
    assert_eq!(shown(&text).len(), 14, "{text}");
}

#[test]
fn the_note_and_the_help_line_are_the_same_on_a_scrolled_view() {
    let mut app = two_hundred();
    press(&mut app, KeyCode::Down, 100);
    let text = screen(&app);
    let mut rows = text.lines().rev().filter(|l| !l.trim().is_empty());
    let help = rows.next().unwrap();
    let note = rows.next().unwrap();
    assert!(note.starts_with("200 runs"), "{note:?}\n{text}");
    assert!(help.contains("q quit"), "{help:?}\n{text}");
    assert!(help.contains("Enter open"), "{help:?}\n{text}");
}
