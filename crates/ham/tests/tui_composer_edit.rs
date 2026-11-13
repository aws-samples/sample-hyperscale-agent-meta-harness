//! The composer's text boxes and the keys that used to lose what was typed:
//! the Prompt box shows a caret and scrolls with it; Esc leaves the typed prompt in place for the next `n`; Ctrl-C clears the box that
//! has text and quits only from an empty one, and a quit with unsent text says so once.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};

const RUN: &str = "01K6A0000000000000000000R1";

fn new_app() -> App {
    App::new_direct("n".into(), None, DirectSettings::default())
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn ctrl_c(app: &mut App) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        let code = if ch == '\n' {
            KeyCode::Enter
        } else {
            KeyCode::Char(ch)
        };
        assert!(press(app, code).is_empty(), "typing {ch:?} caused effects");
    }
}

/// The screen as text, and where the terminal's cursor is after the draw.
fn draw(app: &App, w: u16, h: u16) -> (String, (u16, u16)) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let pos = t.backend_mut().get_cursor_position().unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    (out, (pos.x, pos.y))
}

fn composer() -> App {
    let mut app = new_app();
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    app
}

/// The screen row (from 0) of the first line that holds `text`.
fn row_of(screen: &str, text: &str) -> usize {
    screen
        .lines()
        .position(|l| l.contains(text))
        .unwrap_or_else(|| panic!("{text:?} is not on the screen:\n{screen}"))
}

fn twenty_lines() -> String {
    (1..=20)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_twenty_line_prompt_shows_the_caret_and_the_lines_around_it_at_80x24() {
    let mut app = composer();
    type_text(&mut app, &twenty_lines());
    let (screen, (x, y)) = draw(&app, 80, 24);
    assert!(
        screen.contains("line 20"),
        "the last line is hidden:\n{screen}"
    );
    assert!(screen.contains("line 19"), "{screen}");
    assert!(
        !screen.contains("line 01"),
        "the box did not scroll:\n{screen}"
    );
    // The terminal's cursor is the caret: at the end of the last line, inside the box.
    assert_eq!(usize::from(y), row_of(&screen, "line 20"), "{screen}");
    assert_eq!(x, 1 + "line 20".len() as u16, "{screen}");
}

#[test]
fn up_scrolls_the_box_back_to_the_first_lines() {
    let mut app = composer();
    type_text(&mut app, &twenty_lines());
    for _ in 0..19 {
        press(&mut app, KeyCode::Up);
    }
    let (screen, (x, y)) = draw(&app, 80, 24);
    assert!(screen.contains("line 01"), "{screen}");
    assert!(!screen.contains("line 20"), "{screen}");
    assert_eq!(usize::from(y), row_of(&screen, "line 01"), "{screen}");
    assert_eq!(x, 1 + "line 01".len() as u16, "the caret kept its column");
}

#[test]
fn the_caret_moves_and_typing_inserts_where_it_is() {
    let mut app = composer();
    type_text(&mut app, "build a todo api");
    for _ in 0..8 {
        press(&mut app, KeyCode::Left);
    }
    type_text(&mut app, "small ");
    let (screen, (x, y)) = draw(&app, 100, 40);
    assert!(screen.contains("build a small todo api"), "{screen}");
    assert_eq!(usize::from(y), row_of(&screen, "build a small"), "{screen}");
    assert_eq!(x, 1 + "build a small ".len() as u16, "{screen}");
    press(&mut app, KeyCode::Home);
    type_text(&mut app, ">> ");
    press(&mut app, KeyCode::End);
    type_text(&mut app, "!");
    assert!(draw(&app, 100, 40).0.contains(">> build a small todo api!"));
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Delete);
    assert!(draw(&app, 100, 40).0.contains("> build a small todo api"));
}

#[test]
fn enter_adds_a_line_and_backspace_joins_it_again() {
    let mut app = composer();
    type_text(&mut app, "one\ntwo");
    let (screen, _) = draw(&app, 100, 40);
    assert!(screen.contains("one") && screen.contains("two"));
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Backspace);
    let (screen, (x, y)) = draw(&app, 100, 40);
    assert!(screen.contains("onetwo"), "{screen}");
    assert_eq!((usize::from(y), x), (row_of(&screen, "onetwo"), 1 + 3));
}

#[test]
fn a_long_line_wraps_in_the_box_and_the_caret_follows() {
    let mut app = composer();
    let long = "w".repeat(150);
    type_text(&mut app, &long);
    let (screen, (x, y)) = draw(&app, 80, 40);
    // 78 columns inside the border: 78 + 72 characters on two rows, the caret after the last.
    assert!(
        screen.lines().any(|l| l.contains(&"w".repeat(78))),
        "{screen}"
    );
    assert_eq!(x, 1 + 72);
    assert!(
        screen
            .lines()
            .nth(usize::from(y))
            .unwrap()
            .contains(&"w".repeat(72))
    );
}

#[test]
fn the_repository_and_base_ref_boxes_have_a_caret_and_take_no_spaces() {
    let mut app = composer();
    press(&mut app, KeyCode::Tab); // Target
    press(&mut app, KeyCode::Tab); // Repository
    type_text(&mut app, "cc:my repo");
    let (screen, (x, y)) = draw(&app, 100, 40);
    assert!(screen.contains("cc:myrepo"), "{screen}");
    assert_eq!(usize::from(y), row_of(&screen, "cc:myrepo"), "{screen}");
    assert_eq!(x, 1 + "cc:myrepo".len() as u16);
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    type_text(&mut app, "-");
    assert!(draw(&app, 100, 40).0.contains("cc:myre-po"));
}

#[test]
fn esc_in_the_composer_keeps_the_typed_prompt_for_the_next_n() {
    let mut app = composer();
    type_text(&mut app, "build a todo api\nwith tests");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    let (screen, (x, y)) = draw(&app, 100, 40);
    assert!(screen.contains("build a todo api"), "{screen}");
    assert!(screen.contains("with tests"), "{screen}");
    // The caret is where it was left.
    assert_eq!(usize::from(y), row_of(&screen, "with tests"), "{screen}");
    assert_eq!(x, 1 + "with tests".len() as u16);
}

#[test]
fn a_started_run_empties_the_composer_for_the_next_prompt() {
    let mut app = composer();
    type_text(&mut app, "build a todo api");
    app.update(Msg::Started(Ok(
        harness_protocol::api::v1::StartRunResponse {
            run_id: RUN.into(),
            status: harness_protocol::goal::GoalStatus::Active,
        },
    )));
    press(&mut app, KeyCode::Esc); // the dashboard goes back to the Runs screen
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    assert!(!draw(&app, 100, 40).0.contains("build a todo api"));
}

#[test]
fn ctrl_c_with_text_in_the_prompt_clears_it_and_the_app_stays() {
    let mut app = composer();
    type_text(&mut app, "build a todo api");
    assert!(ctrl_c(&mut app).is_empty(), "Ctrl-C quit with a draft");
    assert_eq!(app.screen(), Screen::Composer);
    assert!(!draw(&app, 100, 40).0.contains("build a todo api"));
    // The box is empty and nothing else is unsent: the next Ctrl-C quits.
    assert_eq!(ctrl_c(&mut app), vec![Effect::Quit]);
}

#[test]
fn ctrl_c_clears_only_the_focused_box() {
    let mut app = composer();
    type_text(&mut app, "keep this prompt");
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab); // Repository
    type_text(&mut app, "cc:drop-me");
    assert!(ctrl_c(&mut app).is_empty());
    let screen = draw(&app, 100, 40).0;
    assert!(screen.contains("keep this prompt"), "{screen}");
    assert!(!screen.contains("cc:drop-me"), "{screen}");
}

#[test]
fn quitting_with_an_unsent_prompt_says_so_once_then_quits() {
    let mut app = composer();
    type_text(&mut app, "build a todo api");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    // Ctrl-C, `q`: each warns first when a prompt is unsent, and the next one quits.
    assert!(ctrl_c(&mut app).is_empty());
    let screen = draw(&app, 100, 30).0;
    assert!(screen.contains("unsent"), "no warning:\n{screen}");
    assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Effect::Quit]);
    // Any other key in between starts the warning over.
    let mut app = composer();
    type_text(&mut app, "x");
    press(&mut app, KeyCode::Esc);
    assert!(press(&mut app, KeyCode::Char('q')).is_empty());
    press(&mut app, KeyCode::Char('j'));
    assert!(
        press(&mut app, KeyCode::Char('q')).is_empty(),
        "warned again"
    );
    assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Effect::Quit]);
}

#[test]
fn a_composer_with_nothing_typed_does_not_stop_a_quit() {
    let mut app = composer();
    press(&mut app, KeyCode::Esc);
    assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Effect::Quit]);
    let mut app = composer();
    assert_eq!(ctrl_c(&mut app), vec![Effect::Quit]);
}
