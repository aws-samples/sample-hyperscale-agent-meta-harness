//! The ham mascot on the Runs screen: the picture in the empty table. It shows once the service has said the user has no runs, only when the
//! table has room (the large form first, then the head with its flame and smoke, then the short head), it never moves a text row, and
//! `Head.empty_table` (the user's `ui.mascot = off`) hides it. The smoke under the large form and under the head moves one step on each tick of
//! the app's timer (`App::tick`) unless `ui.mascot_motion` is off; the smoke of the title band (a terminal of 33 rows or more) moves on every
//! screen, in step with the picture. The pure parts are in `tui/mascot.rs`; these tests drive the screen. The look is injected (`Head.look`):
//! the tests do not read the process environment.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::head::Head;
use ham::tui::mascot::{self, Form, Look};
use ham::tui::{App, DirectSettings, Msg, Screen};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::json;

const NOTE: &str = "no runs yet: press n to start one";

fn head(band: bool, empty_table: bool, look: Look) -> Head {
    Head {
        band,
        empty_table,
        look,
    }
}

fn fresh() -> App {
    App::new_direct("n".into(), None, DirectSettings::default())
}

/// The service answered with this list.
fn answered(runs: Vec<RunSummary>) -> App {
    let mut app = fresh();
    app.update(Msg::Runs(Ok(runs)));
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn one_run() -> Vec<RunSummary> {
    vec![
        serde_json::from_value(
            json!({"runId": "01M5A00000000000000000R001", "status": "complete", "phase": "planning",
            "target": {"mode": "s3-only"}, "createdAt": "2026-10-16T08:00:00Z",
            "ownerPrincipalArn": "arn:aws:sts::800000000022:assumed-role/Admin/testuser"}),
        )
        .unwrap(),
    ]
}

fn buffer(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

fn rows(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn screen(app: &App, w: u16, h: u16) -> String {
    rows(&buffer(app, w, h)).join("\n")
}

/// The non-blank rows of the form in `look`, as they are found on a screen.
fn art(form: Form, look: Look) -> Vec<String> {
    mascot::plain(form, look)
        .into_iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn shows(screen: &str, form: Form, look: Look) -> bool {
    let rows = art(form, look);
    assert!(rows.len() >= 2, "the {form:?} form has no rows to look for");
    rows.iter().all(|row| screen.contains(row.as_str()))
}

/// No form is on the screen.
fn none(screen: &str, look: Look) -> bool {
    !shows(screen, Form::Large, look)
        && !shows(screen, Form::Head, look)
        && !shows(screen, Form::HeadShort, look)
}

/// The Runs body of a compact terminal `w` x `h`: the title row above it, the status and help rows below.
fn body(w: u16, h: u16) -> Rect {
    Rect::new(0, 1, w, h - 3)
}

/// The rows of the title band on a terminal `h` rows tall: 5 from 33 rows, else 3 (31 and 32 rows).
fn band_rows(h: u16) -> u16 {
    if h >= 33 { 5 } else { 3 }
}

/// The Runs body when the band is on: the band's rows above it, the status and help rows below.
fn body_banded(w: u16, h: u16) -> Rect {
    Rect::new(0, band_rows(h), w, h - band_rows(h) - 2)
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn the_default_head_shows_the_empty_table_picture_and_no_band() {
    let h = Head::default();
    assert!(!h.band, "tests and the library start without the band");
    assert!(h.empty_table);
    assert_eq!(h.look, Look::Blocks);
}

#[test]
fn a_session_that_has_not_heard_from_the_service_shows_no_mascot() {
    // The first frame is drawn before the list arrives: a user who has runs must not see the mascot flash by.
    let text = screen(&fresh(), 80, 24);
    assert!(text.contains(NOTE), "{text}");
    assert!(none(&text, Look::Blocks), "{text}");
}

#[test]
fn an_empty_answer_shows_the_large_form_at_80x24() {
    let text = screen(&answered(vec![]), 80, 24);
    assert!(shows(&text, Form::Large, Look::Blocks), "{text}");
    assert!(text.contains(NOTE), "{text}");
    assert!(text.contains("ham · Runs"), "{text}");
    assert!(text.contains("n new run · Enter open"), "{text}");
}

#[test]
fn the_large_form_is_drawn_in_the_middle_of_the_empty_table() {
    // 78 x 18 of room, 14 x 12 of mascot: 32 columns and 3 rows of air before it.
    let (form, at) = mascot::placement(body(80, 24), Look::Blocks).unwrap();
    assert_eq!(form, Form::Large);
    assert_eq!(at, Rect::new(33, 6, 14, 12));
    let screen = rows(&buffer(&answered(vec![]), 80, 24));
    for (i, line) in mascot::plain(Form::Large, Look::Blocks).iter().enumerate() {
        let got: String = screen[usize::from(at.y) + i]
            .chars()
            .skip(usize::from(at.x))
            .take(usize::from(at.width))
            .collect();
        assert_eq!(&got, line, "row {i}");
    }
}

#[test]
fn the_ascii_look_draws_the_same_places_with_plain_characters() {
    let mut app = answered(vec![]);
    app.set_head(head(false, true, Look::Ascii));
    let text = screen(&app, 80, 24);
    assert!(shows(&text, Form::Large, Look::Ascii), "{text}");
    assert!(!shows(&text, Form::Large, Look::Blocks), "{text}");
    let (_, at) = mascot::placement(body(80, 24), Look::Ascii).unwrap();
    assert_eq!(at, Rect::new(33, 6, 14, 12));
}

#[test]
fn the_mascot_moves_no_existing_cell() {
    for (w, h) in [
        (80, 24),
        (100, 30),
        (120, 40),
        (200, 60),
        (80, 12),
        (30, 24),
    ] {
        let before = buffer(&fresh(), w, h);
        let after = buffer(&answered(vec![]), w, h);
        let (_, at) = mascot::placement(body(w, h), Look::Blocks).unwrap();
        assert!(at.width > 0 && at.height > 0, "{w}x{h}");
        for y in 0..h {
            for x in 0..w {
                let inside = x >= at.x && x < at.right() && y >= at.y && y < at.bottom();
                if !inside {
                    assert_eq!(
                        after[(x, y)],
                        before[(x, y)],
                        "{w}x{h}: cell ({x},{y}) changed"
                    );
                }
            }
        }
    }
}

#[test]
fn the_large_form_stays_in_the_empty_table_when_the_band_is_on() {
    for (w, h) in [(120, 40), (200, 60), (100, 31)] {
        let mut before = fresh();
        before.set_head(head(true, true, Look::Blocks));
        let mut after = answered(vec![]);
        after.set_head(head(true, true, Look::Blocks));
        let text = screen(&after, w, h);
        assert!(shows(&text, Form::Large, Look::Blocks), "{w}x{h}\n{text}");
        let (form, at) = mascot::placement(body_banded(w, h), Look::Blocks).unwrap();
        assert_eq!(form, Form::Large, "{w}x{h}");
        let (before, after) = (buffer(&before, w, h), buffer(&after, w, h));
        for y in 0..h {
            for x in 0..w {
                let inside = x >= at.x && x < at.right() && y >= at.y && y < at.bottom();
                if !inside {
                    assert_eq!(
                        after[(x, y)],
                        before[(x, y)],
                        "{w}x{h}: cell ({x},{y}) changed"
                    );
                }
            }
        }
        assert!(
            at.y >= 5,
            "{w}x{h}: the picture starts below the band and the table header, at row {}",
            at.y
        );
    }
}

#[test]
fn the_large_form_sits_below_the_table_header() {
    let (_, at) = mascot::placement(body(80, 24), Look::Blocks).unwrap();
    let text = rows(&buffer(&answered(vec![]), 80, 24));
    let header = text.iter().position(|r| r.contains("STATUS")).unwrap();
    assert!(
        usize::from(at.y) > header,
        "the mascot starts at row {} but the header is row {header}",
        at.y
    );
    assert!(at.bottom() <= 22, "the mascot ends before the status row");
}

#[test]
fn the_form_follows_the_room_of_the_terminal() {
    let at = |w: u16, h: u16| mascot::form_for(Rect::new(2, 2, w, h));
    // The large form is 14 x 12 with a margin of two columns and one row on each side.
    assert_eq!(
        at(18, 14),
        Some(Form::Large),
        "exactly the large form and its margin"
    );
    assert_eq!(at(200, 60), Some(Form::Large));
    assert_eq!(
        at(17, 14),
        Some(Form::Head),
        "one column short of the large margin"
    );
    assert_eq!(
        at(18, 13),
        Some(Form::Head),
        "one row short of the large margin"
    );
    // The head is 8 x 5 with the same margin: 12 columns and 7 rows. One row less is the short head (8 x 3: 12 columns and 5 rows).
    assert_eq!(
        at(12, 7),
        Some(Form::Head),
        "exactly the head and its margin"
    );
    assert_eq!(
        at(12, 6),
        Some(Form::HeadShort),
        "one row short of the head's margin"
    );
    assert_eq!(
        at(12, 5),
        Some(Form::HeadShort),
        "exactly the short head and its margin"
    );
    assert_eq!(at(11, 7), None);
    assert_eq!(at(11, 5), None);
    assert_eq!(at(12, 4), None);
    assert_eq!(at(0, 0), None);
}

#[test]
fn a_short_terminal_gets_the_head_and_a_tiny_one_gets_none() {
    let look = Look::Blocks;
    // The table has h - 3 rows and its room is 3 less than that: 80x20 has 14 rows of room (the large form), 80x19 has 13 (the head), 80x13
    // has 7 (the head), 80x12 has 6 (the short head), 80x11 has 5 (the short head), 80x10 has 4 (none).
    let large = screen(&answered(vec![]), 80, 20);
    assert!(shows(&large, Form::Large, look), "{large}");
    let small = screen(&answered(vec![]), 80, 19);
    assert!(
        shows(&small, Form::Head, look) && !shows(&small, Form::Large, look),
        "{small}"
    );
    let seven = screen(&answered(vec![]), 80, 13);
    assert!(shows(&seven, Form::Head, look), "{seven}");
    let six = screen(&answered(vec![]), 80, 12);
    assert!(
        shows(&six, Form::HeadShort, look) && !shows(&six, Form::Head, look),
        "{six}"
    );
    let least = screen(&answered(vec![]), 80, 11);
    assert!(
        shows(&least, Form::HeadShort, look) && !shows(&least, Form::Head, look),
        "{least}"
    );
    let tiny = screen(&answered(vec![]), 80, 10);
    assert!(none(&tiny, look), "{tiny}");
    assert!(tiny.contains(NOTE), "{tiny}");
    // Width: 20 columns leave 18 of room, which is the large form and its margin; 19 columns leave 17, so the head; 13 leave none.
    assert!(shows(&screen(&answered(vec![]), 20, 24), Form::Large, look));
    let narrow = screen(&answered(vec![]), 19, 24);
    assert!(
        shows(&narrow, Form::Head, look) && !shows(&narrow, Form::Large, look),
        "{narrow}"
    );
    assert!(none(&screen(&answered(vec![]), 13, 24), look));
}

#[test]
fn a_list_with_runs_or_an_error_shows_no_mascot() {
    let with_run = screen(&answered(one_run()), 80, 24);
    assert!(none(&with_run, Look::Blocks), "{with_run}");
    let mut failed = answered(vec![]);
    failed.update(Msg::Runs(Err("HTTP 403: not authorized (http)".into())));
    let text = screen(&failed, 80, 24);
    assert!(text.contains("cannot list runs: HTTP 403"), "{text}");
    assert!(none(&text, Look::Blocks), "{text}");
    // The next good answer brings it back.
    failed.update(Msg::Runs(Ok(vec![])));
    assert!(shows(&screen(&failed, 80, 24), Form::Large, Look::Blocks));
}

#[test]
fn a_filter_that_matches_nothing_is_not_the_empty_state() {
    let mut app = answered(one_run());
    key(&mut app, KeyCode::Char('/'));
    for c in "zzzzzz".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    let text = screen(&app, 80, 24);
    assert!(none(&text, Look::Blocks), "{text}");
}

#[test]
fn no_other_screen_shows_the_mascot_picture() {
    let mut app = answered(vec![]);
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    let text = screen(&app, 80, 24);
    assert!(none(&text, Look::Blocks), "{text}");
}

#[test]
fn the_switch_hides_the_picture_in_the_empty_table() {
    let mut app = answered(vec![]);
    app.set_head(head(false, false, Look::Blocks));
    let text = screen(&app, 80, 24);
    assert!(none(&text, Look::Blocks), "{text}");
    assert!(text.contains(NOTE), "{text}");
    // And the screen is the one of a session that has not heard from the service, but for its status row.
    app.set_head(head(false, true, Look::Blocks));
    assert!(shows(&screen(&app, 80, 24), Form::Large, Look::Blocks));
}

// ---- the smoke moves with the timer -----------------------------------------------------------------------------------------------

/// The rows of the picture that hold smoke: the last three terminal rows of the large form (pixel rows 18 to 23).
const SMOKE_ROWS: std::ops::Range<u16> = 9..12;

/// The empty Runs table of an 80x24 terminal as a buffer, and where the large form stands in it.
fn picture(app: &App, look: Look) -> (Buffer, Rect) {
    let (_, at) = mascot::placement(body(80, 24), look).unwrap();
    (buffer(app, 80, 24), at)
}

/// The cells of `after` that differ from `before`, as (x, y).
fn differing(before: &Buffer, after: &Buffer) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for y in 0..before.area.height {
        for x in 0..before.area.width {
            if before[(x, y)] != after[(x, y)] {
                out.push((x, y));
            }
        }
    }
    out
}

/// One tick of the app's timer, with the effects it asks for ignored (the Runs list is read again only every fifth).
fn tick(app: &mut App) {
    let _ = app.tick();
}

#[test]
fn a_tick_moves_the_smoke_and_nothing_else_in_both_looks() {
    for look in [Look::Blocks, Look::Ascii] {
        let mut app = answered(vec![]);
        app.set_head(head(false, true, look));
        let (first, at) = picture(&app, look);
        assert_eq!(app.mascot_frame(), 0, "{look:?}");
        let mut previous = first.clone();
        for step in 1..=2 {
            tick(&mut app);
            assert_eq!(app.mascot_frame(), step, "{look:?}");
            let (now, _) = picture(&app, look);
            let moved = differing(&previous, &now);
            assert!(!moved.is_empty(), "{look:?} step {step}: nothing moved");
            for (x, y) in &moved {
                let row = y.checked_sub(at.y);
                assert!(
                    row.is_some_and(|r| SMOKE_ROWS.contains(&r)) && *x >= at.x && *x < at.right(),
                    "{look:?} step {step}: cell ({x},{y}) changed outside the smoke"
                );
            }
            previous = now;
        }
        // The third tick brings the first frame back, cell for cell.
        tick(&mut app);
        assert_eq!(app.mascot_frame(), 0, "{look:?}");
        let (again, _) = picture(&app, look);
        assert!(differing(&first, &again).is_empty(), "{look:?}");
    }
}

#[test]
fn the_frame_is_counted_by_the_app_and_a_draw_does_not_advance_it() {
    let mut app = answered(vec![]);
    let (a, _) = picture(&app, Look::Blocks);
    let (b, _) = picture(&app, Look::Blocks);
    assert!(differing(&a, &b).is_empty(), "two draws of one tick differ");
    tick(&mut app);
    tick(&mut app);
    tick(&mut app);
    tick(&mut app);
    assert_eq!(app.mascot_frame(), 1, "four ticks are frame 1 of 3");
}

#[test]
fn with_motion_off_the_picture_is_always_the_first_frame() {
    let mut app = answered(vec![]);
    app.set_mascot_motion(false);
    let (still, _) = picture(&app, Look::Blocks);
    for _ in 0..7 {
        tick(&mut app);
        assert_eq!(app.mascot_frame(), 0);
        let (now, _) = picture(&app, Look::Blocks);
        assert!(differing(&still, &now).is_empty());
    }
    // And it is the picture of the README: the same cells as a session that never ticked.
    let (fresh, _) = picture(&answered(vec![]), Look::Blocks);
    assert!(differing(&fresh, &still).is_empty());
    // Switching it on again starts the smoke from the first frame.
    app.set_mascot_motion(true);
    assert_eq!(app.mascot_frame(), 0);
    tick(&mut app);
    assert_eq!(app.mascot_frame(), 1);
}

#[test]
fn the_smoke_stands_still_until_the_mascot_is_on_the_screen() {
    // Ticks before the service has answered, with a list of runs, with an error, and with the picture switched off do not advance the smoke:
    // the picture starts at its first frame whenever it appears.
    let mut app = fresh();
    for _ in 0..4 {
        tick(&mut app);
    }
    app.update(Msg::Runs(Ok(vec![])));
    assert_eq!(app.mascot_frame(), 0, "the picture appears at frame 0");
    tick(&mut app);
    assert_eq!(app.mascot_frame(), 1);
    app.update(Msg::Runs(Ok(one_run())));
    tick(&mut app);
    assert_eq!(app.mascot_frame(), 0, "a list with a run shows no picture");
    app.update(Msg::Runs(Ok(vec![])));
    assert_eq!(
        app.mascot_frame(),
        0,
        "it starts again from the first frame"
    );
    let mut off = answered(vec![]);
    off.set_head(head(false, false, Look::Blocks));
    tick(&mut off);
    assert_eq!(off.mascot_frame(), 0, "ui.mascot = off");
}

#[test]
fn another_screen_does_not_count_the_smoke() {
    let mut app = answered(vec![]);
    tick(&mut app);
    assert_eq!(app.mascot_frame(), 1);
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    tick(&mut app);
    assert_eq!(app.mascot_frame(), 0);
}

#[test]
fn the_head_in_a_short_table_moves_its_smoke_and_the_short_head_stands_still() {
    // The head (80x19: 13 rows of room) has a smoke row of its own: one tick changes only the cells of its last terminal row. The short head
    // (80x11) has no smoke, so the screen is the same at every tick.
    let mut app = answered(vec![]);
    let before = buffer(&app, 80, 19);
    let (_, at) = mascot::placement(body(80, 19), Look::Blocks).unwrap();
    tick(&mut app);
    let after = buffer(&app, 80, 19);
    let moved = differing(&before, &after);
    assert!(!moved.is_empty(), "the head's smoke did not move");
    for (x, y) in &moved {
        assert!(
            *y == at.y + 4 && *x >= at.x && *x < at.right(),
            "cell ({x},{y}) changed outside the head's smoke row"
        );
    }
    let mut app = answered(vec![]);
    let before = buffer(&app, 80, 11);
    tick(&mut app);
    let after = buffer(&app, 80, 11);
    assert!(shows(
        &rows(&after).join("\n"),
        Form::HeadShort,
        Look::Blocks
    ));
    assert!(differing(&before, &after).is_empty());
}

#[test]
fn a_tick_asks_nothing_of_the_service_for_the_smoke() {
    // The smoke adds no timer and no call: the four ticks before the Runs list is read again (every fifth tick) make no effect, so the only
    // thing that happens on them is the next frame at the next draw.
    let mut app = answered(vec![]);
    for n in 1..=4 {
        assert!(app.tick().is_empty(), "tick {n} made an effect");
    }
    assert_eq!(app.mascot_frame(), 4 % mascot::FRAMES);
}

// ---- the smoke in the title band ----------------------------------------------------------------------------------------------------

/// A banded app of this look.
fn banded(look: Look) -> App {
    let mut app = answered(one_run());
    app.set_head(head(true, true, look));
    app
}

/// The cells of the band's smoke row on a terminal of 33 rows or more: the head's last row, columns 0 to 7 of row 4.
fn in_smoke_row(x: u16, y: u16) -> bool {
    y == 4 && x < 8
}

#[test]
fn the_five_row_band_moves_its_smoke_with_the_ticks_on_a_screen_with_runs_in_both_looks() {
    for look in [Look::Blocks, Look::Ascii] {
        let mut app = banded(look);
        assert_eq!(app.mascot_frame(), 0, "{look:?}");
        let first = buffer(&app, 100, 40);
        let mut previous = first.clone();
        for step in 1..=2usize {
            tick(&mut app);
            assert_eq!(app.mascot_frame(), step, "{look:?}");
            let now = buffer(&app, 100, 40);
            let moved = differing(&previous, &now);
            assert!(!moved.is_empty(), "{look:?} step {step}: nothing moved");
            for (x, y) in &moved {
                assert!(
                    in_smoke_row(*x, *y),
                    "{look:?} step {step}: cell ({x},{y}) changed outside the band's smoke"
                );
            }
            // The head is the one of this frame, found in its 8 x 5 cells.
            let screen = rows(&now);
            for (i, line) in mascot::plain_at(Form::Head, look, step).iter().enumerate() {
                let got: String = screen[i].chars().take(8).collect();
                assert_eq!(&got, line, "{look:?} step {step} row {i}");
            }
            previous = now;
        }
        tick(&mut app);
        assert_eq!(app.mascot_frame(), 0, "{look:?}");
        assert!(
            differing(&first, &buffer(&app, 100, 40)).is_empty(),
            "{look:?}: the third tick brings the first frame back"
        );
    }
}

#[test]
fn the_band_moves_on_every_screen_not_only_the_runs_list() {
    let mut app = banded(Look::Blocks);
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    let before = buffer(&app, 100, 40);
    tick(&mut app);
    assert_eq!(app.mascot_frame(), 1, "the composer counts the ticks too");
    let after = buffer(&app, 100, 40);
    let moved = differing(&before, &after);
    assert!(!moved.is_empty(), "the band's smoke did not move");
    assert!(moved.iter().all(|(x, y)| in_smoke_row(*x, *y)), "{moved:?}");
}

#[test]
fn the_short_band_has_no_smoke_so_the_screen_stands_still() {
    // 31 and 32 rows keep the three-row band with the short head: nothing moves.
    for h in [31u16, 32] {
        let mut app = banded(Look::Blocks);
        let before = buffer(&app, 100, h);
        tick(&mut app);
        tick(&mut app);
        let after = buffer(&app, 100, h);
        assert!(differing(&before, &after).is_empty(), "{h} rows");
        let screen = rows(&after);
        for (i, line) in mascot::plain(Form::HeadShort, Look::Blocks)
            .iter()
            .enumerate()
        {
            let got: String = screen[i].chars().take(8).collect();
            assert_eq!(&got, line, "{h} rows: row {i}");
        }
        assert!(
            screen[3].starts_with('┌'),
            "{h} rows: the table starts on row 3, no flame under the short head: {:?}",
            screen[3]
        );
    }
}

#[test]
fn with_motion_off_the_band_shows_the_first_frame_everywhere() {
    let mut app = banded(Look::Blocks);
    app.set_mascot_motion(false);
    let still = buffer(&app, 100, 40);
    for _ in 0..7 {
        tick(&mut app);
        assert_eq!(app.mascot_frame(), 0);
        assert!(differing(&still, &buffer(&app, 100, 40)).is_empty());
    }
    let screen = rows(&still);
    for (i, line) in mascot::plain(Form::Head, Look::Blocks).iter().enumerate() {
        let got: String = screen[i].chars().take(8).collect();
        assert_eq!(&got, line, "row {i}");
    }
}

#[test]
fn ui_mascot_off_counts_no_ticks_and_draws_no_band() {
    let mut app = answered(one_run());
    app.set_head(Head::OFF);
    for _ in 0..4 {
        tick(&mut app);
    }
    assert_eq!(app.mascot_frame(), 0);
    let screen = rows(&buffer(&app, 100, 40)).join("\n");
    assert!(
        none(&screen, Look::Blocks) && none(&screen, Look::Ascii),
        "{screen}"
    );
}

#[test]
fn the_band_and_the_picture_in_the_empty_table_are_always_in_the_same_frame() {
    for look in [Look::Blocks, Look::Ascii] {
        let mut app = answered(vec![]);
        app.set_head(head(true, true, look));
        for n in 0..7usize {
            let (_, at) = mascot::placement(body_banded(100, 40), look).unwrap();
            let screen = rows(&buffer(&app, 100, 40));
            let frame = n % mascot::FRAMES;
            assert_eq!(app.mascot_frame(), frame, "{look:?} tick {n}");
            for (i, line) in mascot::plain_at(Form::Head, look, frame).iter().enumerate() {
                let got: String = screen[i].chars().take(8).collect();
                assert_eq!(&got, line, "{look:?} tick {n}: band row {i}");
            }
            for (i, line) in mascot::plain_at(Form::Large, look, frame)
                .iter()
                .enumerate()
            {
                let got: String = screen[usize::from(at.y) + i]
                    .chars()
                    .skip(usize::from(at.x))
                    .take(usize::from(at.width))
                    .collect();
                assert_eq!(&got, line, "{look:?} tick {n}: picture row {i}");
            }
            tick(&mut app);
        }
    }
}

#[test]
fn the_body_of_a_screen_does_not_move_when_the_band_does() {
    // The body below the five-row band is the same at every frame.
    let mut app = banded(Look::Blocks);
    let first = buffer(&app, 100, 40);
    tick(&mut app);
    let second = buffer(&app, 100, 40);
    for y in 5..40 {
        for x in 0..100 {
            assert_eq!(first[(x, y)], second[(x, y)], "cell ({x},{y})");
        }
    }
}
