//! The Model box of the New run screen is a window onto the model list on a short terminal.
//!
//! The box used to take one row for every model, so each added model took a row from the prompt box: with eight models an 80x24 terminal
//! would leave the prompt no line of text. `model_window(total, picked, height, backend_rows)` gives the first row shown and how many rows
//! show. It reads the height only, so the list does not jump when a message appears under it; it follows the picked row; and it never
//! underflows on a very short terminal. While rows are hidden the title of the box says which rows show (`Model 1-6 of 8 (...)`), and the
//! rows' mouse targets name the model in the list, not the row of the window.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::model_window::{model_box_title, model_window};
use ham::tui::{App, DirectSettings, Effect, Msg, OwnerLookup};
use ham_core::backends::{BackendInfo, BackendView, Health};
use ham_core::models::{ModelPrefs, Models};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

const NAMES: [&str; 8] = [
    "one", "two", "three", "four", "five", "six", "seven", "eight",
];

// ---- the pure rule --------------------------------------------------------------------------------------------------------------

#[test]
fn b862_the_model_window_shows_six_rows_at_24_and_all_from_26() {
    for picked in 0..8 {
        let (first, count) = model_window(8, picked, 24, 0);
        assert_eq!(
            count, 6,
            "24 rows show six of eight models (picked {picked})"
        );
        assert!(
            (first..first + count).contains(&picked),
            "the picked row {picked} is in the window {first}..{}",
            first + count
        );
        assert!(first + count <= 8, "no row past the list (picked {picked})");
        for height in [26u16, 27, 40, 60, 200] {
            assert_eq!(
                model_window(8, picked, height, 0),
                (0, 8),
                "from 26 rows up all eight show (height {height}, picked {picked})"
            );
        }
        // 25 rows: seven of eight.
        assert_eq!(model_window(8, picked, 25, 0).1, 7);
        // Six models at 24 rows: the whole list, as before the window existed.
        assert_eq!(model_window(6, picked.min(5), 24, 0), (0, 6));
    }
}

#[test]
fn b862_the_window_follows_the_pick_and_never_underflows() {
    // A terminal shorter than the fixed rows, with the Backend box: one row, the picked one.
    for picked in 0..8 {
        assert_eq!(model_window(8, picked, 10, 3), (picked, 1));
        assert_eq!(model_window(8, picked, 0, 0), (picked, 1));
        assert_eq!(model_window(8, picked, 18, 0), (picked, 1));
    }
    // The Backend box takes its rows from the list: 24 rows with it show three models.
    assert_eq!(model_window(8, 0, 24, 3).1, 3);
    // The window keeps the pick near its middle and stops at both ends of the list.
    assert_eq!(model_window(8, 0, 24, 0), (0, 6));
    assert_eq!(model_window(8, 3, 24, 0), (0, 6));
    assert_eq!(model_window(8, 4, 24, 0), (1, 6));
    assert_eq!(model_window(8, 5, 24, 0), (2, 6));
    assert_eq!(model_window(8, 7, 24, 0), (2, 6));
    // A pick past the list (it cannot happen; the function still answers inside the list) and an empty list.
    let (first, count) = model_window(8, 99, 24, 0);
    assert_eq!((first, count), (2, 6));
    assert_eq!(model_window(0, 0, 24, 0), (0, 0));
    assert_eq!(model_window(1, 0, 3, 0), (0, 1));
    // Every height, every pick, both list sizes: the window is inside the list, holds the pick and has at least one row.
    for total in [1usize, 6, 8, 12] {
        for height in 0..80u16 {
            for backend_rows in [0u16, 3] {
                for picked in 0..total {
                    let (first, count) = model_window(total, picked, height, backend_rows);
                    assert!(count >= 1 && count <= total, "{total} {picked} {height}");
                    assert!(first + count <= total, "{total} {picked} {height}");
                    assert!(
                        (first..first + count).contains(&picked),
                        "{total} {picked} {height}: {first}+{count}"
                    );
                }
            }
        }
    }
}

#[test]
fn b862_the_title_names_the_rows_shown_only_while_rows_are_hidden() {
    assert_eq!(
        model_box_title(0, 8, 8),
        "Model (↑↓ picks for this run, d saves it as your default)"
    );
    assert_eq!(
        model_box_title(0, 6, 8),
        "Model 1-6 of 8 (↑↓ picks for this run, d saves it as your default)"
    );
    assert_eq!(
        model_box_title(2, 6, 8),
        "Model 3-8 of 8 (↑↓ picks for this run, d saves it as your default)"
    );
    assert_eq!(
        model_box_title(4, 1, 8),
        "Model 5 of 8 (↑↓ picks for this run, d saves it as your default)"
    );
}

// ---- the screen -----------------------------------------------------------------------------------------------------------------

/// Eight selectable models `a.one` to `a.eight`; the second is the list's default.
fn eight() -> &'static Models {
    let rows: Vec<Value> = NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| {
            json!({"id": format!("a.{n}"), "name": format!("Model {n}"), "vendor": "v", "alias": n, "default": i == 1,
                   "baseModelId": format!("v.{n}"), "protocol": "converse", "region": "us-east-1", "contextTokens": 100000,
                   "maxOutputTokens": 8192, "smokeRecordId": "s6:x+K3:y"})
        })
        .collect();
    Box::leak(Box::new(
        Models::parse(&json!({"schemaVersion": 1, "models": rows}).to_string()).unwrap(),
    ))
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

/// The New run screen over the eight models, the Model field focused.
fn composer() -> App {
    let mut app = App::new_direct_with(
        eight(),
        "nonce-1".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings {
            prefs: ModelPrefs::default(),
            rules: None,
            no_rules: true,
        },
    );
    press(&mut app, KeyCode::Char('n'));
    for _ in 0..5 {
        press(&mut app, KeyCode::Tab); // target, repository, base ref, options, model
    }
    app
}

fn screen_text(app: &App, w: u16, h: u16) -> String {
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

/// The names of the model rows on the screen, top to bottom.
fn rows_shown(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.contains(" ctx ") && l.contains("Model "))
        .filter_map(|l| {
            NAMES
                .iter()
                .rev()
                .find(|n| l.contains(&format!("Model {n} ")))
                .map(|n| (*n).to_string())
        })
        .collect()
}

/// The row of the model list that carries the pick. The Target box has a `(•)` of its own, so the row is the one that also shows a window.
fn picked_row(text: &str) -> &str {
    text.lines()
        .find(|l| l.contains("(•)") && l.contains(" ctx "))
        .unwrap_or_default()
}

/// The inner rows of the box whose title starts with `title`.
fn box_rows(text: &str, title: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let top = lines
        .iter()
        .position(|l| l.contains(title))
        .unwrap_or_else(|| panic!("no box {title:?}:\n{text}"));
    lines[top + 1..]
        .iter()
        .take_while(|l| l.starts_with('│'))
        .count()
}

#[test]
fn b862_at_80x24_eight_models_show_six_rows_and_the_prompt_keeps_a_line_of_text() {
    let mut app = composer();
    let text = screen_text(&app, 80, 24);
    assert_eq!(
        rows_shown(&text),
        ["one", "two", "three", "four", "five", "six"],
        "{text}"
    );
    assert!(
        text.contains("Model 1-6 of 8 (↑↓ picks for this run, d saves it as your default)"),
        "the title says which rows show:\n{text}"
    );
    assert!(
        box_rows(&text, "Prompt (Enter adds a line)") >= 2,
        "the prompt box keeps text rows:\n{text}"
    );
    // The window follows the pick to the end of the list and back.
    press(&mut app, KeyCode::End);
    assert_eq!(app.chosen_model(), Some("a.eight"));
    let text = screen_text(&app, 80, 24);
    assert_eq!(
        rows_shown(&text),
        ["three", "four", "five", "six", "seven", "eight"],
        "{text}"
    );
    assert!(text.contains("Model 3-8 of 8 ("), "{text}");
    let picked = picked_row(&text);
    assert!(
        picked.contains("Model eight"),
        "the picked row is in view:\n{text}"
    );
    press(&mut app, KeyCode::Home);
    let text = screen_text(&app, 80, 24);
    assert_eq!(
        rows_shown(&text).first().map(String::as_str),
        Some("one"),
        "{text}"
    );
    // A message under the list does not move the window: the rule reads the height only.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    let before = rows_shown(&screen_text(&app, 80, 24));
    press(&mut app, KeyCode::Char('d'));
    app.update(ham::tui::Msg::DefaultSaved(Ok("a.three".into())));
    let text = screen_text(&app, 80, 24);
    assert!(
        text.contains("Saved Model three as your default"),
        "the message under the list:\n{text}"
    );
    assert_eq!(rows_shown(&text), before, "{text}");
}

#[test]
fn b862_from_26_rows_all_eight_models_show_under_the_plain_title() {
    let app = composer();
    for (w, h) in [(80u16, 26u16), (120, 40), (200, 60)] {
        let text = screen_text(&app, w, h);
        assert_eq!(rows_shown(&text), NAMES, "{w}x{h}:\n{text}");
        assert!(
            text.contains("Model (↑↓ picks for this run, d saves it as your default)"),
            "{w}x{h}:\n{text}"
        );
        assert!(!text.contains(" of 8"), "{w}x{h}:\n{text}");
    }
}

#[test]
fn b862_a_very_short_terminal_shows_the_picked_row_and_draws_without_a_panic() {
    let mut app = composer();
    press(&mut app, KeyCode::End);
    for h in [1u16, 2, 5, 9, 12, 16, 18, 19, 20] {
        let text = screen_text(&app, 80, h);
        if h >= 19 {
            let picked = picked_row(&text);
            assert!(picked.contains("Model eight"), "{h} rows:\n{text}");
        }
    }
    let text = screen_text(&app, 80, 19);
    assert_eq!(rows_shown(&text), ["eight"], "{text}");
    assert!(text.contains("Model 8 of 8 ("), "{text}");
}

fn click(app: &mut App, x: u16, y: u16) -> Vec<Effect> {
    let at = Instant::now();
    let mut effects = Vec::new();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        effects.extend(app.handle_mouse_at(
            MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            },
            at,
        ));
    }
    effects
}

#[test]
fn b862_a_click_on_a_row_of_the_window_picks_the_model_that_row_shows() {
    let mut app = composer();
    press(&mut app, KeyCode::End);
    // The window is three to eight; the first row of the window shows model three.
    let text = screen_text(&app, 80, 24);
    let y = text
        .lines()
        .position(|l| l.contains("Model three "))
        .unwrap_or_else(|| panic!("model three is not on the screen:\n{text}")) as u16;
    click(&mut app, 8, y);
    assert_eq!(
        app.chosen_model(),
        Some("a.three"),
        "the row names the model in the list, not the row of the window"
    );
    // The row under the last row of the window is the border: a click there picks nothing.
    let text = screen_text(&app, 80, 24);
    let lines: Vec<&str> = text.lines().collect();
    let last = lines
        .iter()
        .rposition(|l| l.contains(" ctx ") && l.contains("Model "))
        .unwrap() as u16;
    let before = app.chosen_model().map(str::to_string);
    click(&mut app, 8, last + 1);
    assert_eq!(app.chosen_model().map(str::to_string), before);
}

// ---- with the Backend box -----------------------------------------------------------------------------------------------------------

/// Tells the app that the client holds two backends, so the New run screen draws its Backend box (three rows).
fn with_two_backends(app: &mut App) {
    let info = |alias: &str| BackendInfo {
        alias: alias.into(),
        health: Health::Up,
        version: Some("5.05".into()),
        stale: false,
        customer_content: true,
        profile: Some(alias.into()),
    };
    app.set_backends(OwnerLookup(Arc::new(|_| None)), Some("work".into()));
    app.update(Msg::Backends(Some(BackendView {
        backends: vec![info("work"), info("spare")],
    })));
}

/// The rows of a model list on the screen: the lines of a box that show a window size.
fn model_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| l.starts_with('│') && l.contains(" ctx "))
        .collect()
}

#[test]
fn b862_with_the_backend_box_the_list_gives_up_rows_and_the_prompt_keeps_its_own() {
    // The list this build ships, on a client with two backends: the Backend box takes three rows, and at 24 rows the list shows the three
    // models around the picked one. Before the window the list asked for all its rows and the prompt box was squeezed.
    let mut app = App::new_direct(
        "nonce-1".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings {
            prefs: ModelPrefs::default(),
            rules: None,
            no_rules: true,
        },
    );
    with_two_backends(&mut app);
    press(&mut app, KeyCode::Char('n'));
    let total = ham_core::models::builtin().models().len();
    let picked = ham_core::models::builtin()
        .models()
        .iter()
        .position(|m| Some(m.id.as_str()) == app.chosen_model())
        .unwrap();
    let text = screen_text(&app, 80, 24);
    assert!(
        text.contains("Backend (←→ picks for this run)"),
        "TRIGGER PRESENT: the Backend box is drawn:\n{text}"
    );
    let (first, count) = model_window(total, picked, 24, 3);
    assert_eq!(count, 3, "24 rows less the kept rows and the Backend box");
    let rows = model_lines(&text);
    assert_eq!(rows.len(), 3, "three model rows:\n{text}");
    assert!(
        text.contains(&model_box_title(first, count, total)),
        "the title says which rows show:\n{text}"
    );
    assert_eq!(
        rows.iter().filter(|l| l.contains("(•)")).count(),
        1,
        "the picked model is among them:\n{text}"
    );
    assert!(
        box_rows(&text, "Prompt (Enter adds a line)") >= 2,
        "the prompt box keeps text rows:\n{text}"
    );
    assert!(text.contains("[ Submit ]"), "{text}");
    // Three rows more, and the whole list shows under the plain title.
    let text = screen_text(&app, 80, 27);
    if total <= 6 {
        assert_eq!(model_lines(&text).len(), total, "{text}");
        assert!(
            text.contains("Model (↑↓ picks for this run, d saves it as your default)"),
            "{text}"
        );
    }
    // Eight models with the Backend box at 24 rows: three of eight, and the pick still shows after End.
    let mut app = App::new_direct_with(
        eight(),
        "nonce-1".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings {
            prefs: ModelPrefs::default(),
            rules: None,
            no_rules: true,
        },
    );
    with_two_backends(&mut app);
    press(&mut app, KeyCode::Char('n'));
    for _ in 0..6 {
        press(&mut app, KeyCode::Tab); // target, repository, base ref, options, backend, model
    }
    press(&mut app, KeyCode::End);
    assert_eq!(app.chosen_model(), Some("a.eight"));
    let text = screen_text(&app, 80, 24);
    assert_eq!(rows_shown(&text), ["six", "seven", "eight"], "{text}");
    assert!(text.contains("Model 6-8 of 8 ("), "{text}");
    assert!(picked_row(&text).contains("Model eight"), "{text}");
}

// ---- the name column ------------------------------------------------------------------------------------------------------------

/// Selectable models `a.<name>` with the names given; the first is the list's default.
fn list_of(names: &[&str]) -> &'static Models {
    let rows: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            json!({"id": format!("a.{n}"), "name": format!("Model {n}"), "vendor": "v", "alias": n, "default": i == 0,
                   "baseModelId": format!("v.{n}"), "protocol": "converse", "region": "us-east-1", "contextTokens": 100000,
                   "maxOutputTokens": 8192, "smokeRecordId": "s6:x+K3:y"})
        })
        .collect();
    Box::leak(Box::new(
        Models::parse(&json!({"schemaVersion": 1, "models": rows}).to_string()).unwrap(),
    ))
}

/// The New run screen over `models`.
fn composer_over(models: &'static Models) -> App {
    let mut app = App::new_direct_with(
        models,
        "nonce-1".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings {
            prefs: ModelPrefs::default(),
            rules: None,
            no_rules: true,
        },
    );
    press(&mut app, KeyCode::Char('n'));
    app
}

/// The row of the Model box that shows the model named `name`.
fn model_line<'a>(text: &'a str, name: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(" ctx ") && l.contains(&format!(" Model {name} ")))
        .unwrap_or_else(|| panic!("no row of Model {name} in\n{text}"))
}

#[test]
fn b862_the_name_column_follows_the_longest_name_and_is_never_under_seven() {
    // The longest name of this list has five characters: the column is seven wide, the width it had before a name could carry a version.
    let text = screen_text(&composer_over(list_of(&["one", "three"])), 80, 26);
    assert!(
        model_line(&text, "one").contains(") one     Model one "),
        "a name of three characters, four of padding and the gap:\n{text}"
    );
    assert!(
        model_line(&text, "three").contains(") three   Model three "),
        "{text}"
    );
    // A name of seven characters, the longest this build ships, fills the column and moves nothing.
    let text = screen_text(&composer_over(list_of(&["one", "sol-6.1"])), 80, 26);
    assert!(
        model_line(&text, "one").contains(") one     Model one "),
        "{text}"
    );
    assert!(
        model_line(&text, "sol-6.1").contains(") sol-6.1 Model sol-6.1 "),
        "{text}"
    );
    // A longer name widens the column for every row of the list, so the rows stay in line, as in the listing of `ham models`.
    let text = screen_text(&composer_over(list_of(&["one", "model-1.2"])), 80, 26);
    assert!(
        model_line(&text, "one").contains(") one       Model one "),
        "a nine-character name in the list gives every row a nine-character column:\n{text}"
    );
    assert!(
        model_line(&text, "model-1.2").contains(") model-1.2 Model model-1.2 "),
        "{text}"
    );
    assert_eq!(ham_core::models::ALIAS_COLUMN_MIN, 7);
}
