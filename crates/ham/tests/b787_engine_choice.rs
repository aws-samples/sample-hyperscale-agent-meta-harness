//! The engine of a run on the screens of `ham`: the Engine box of the New run screen and the keys that move it, the engine the Start
//! carries, `default_engine` of the file as the place the box opens, `/engine <name>` for the runs of a session, and the words of a run's screens
//! naming the engine the run is on.
#![allow(clippy::unwrap_used)]

mod chat_support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Screen};
use harness_protocol::envelope::EventType;
use serde_json::json;
use support_screen::screen_of;

mod support_screen {
    use ham::tui::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    pub fn screen_of(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
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
        out
    }
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn composer() -> App {
    let mut app = App::new_direct(
        "nonce-1".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings {
            no_rules: true,
            ..DirectSettings::default()
        },
    );
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    app
}

fn type_prompt(app: &mut App) {
    for ch in "Add a percent_change function".chars() {
        press(app, KeyCode::Char(ch));
    }
}

/// Submits the composer the way the Start key does and returns the arguments the start carries.
fn submitted(app: &mut App) -> ham_core::command::DirectArgs {
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    match effects.as_slice() {
        [Effect::SubmitDirect { args, .. }] => args.clone(),
        other => panic!("{other:?}\n{}", screen_of(app, 120, 40)),
    }
}

/// Chooses `--s3-only` on the Target field, which needs no repository (these screens open with none).
fn s3_only(app: &mut App) {
    press(app, KeyCode::Tab);
    press(app, KeyCode::Right);
    press(app, KeyCode::Right);
}

fn row_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_default()
}

#[test]
fn b787_the_new_run_screen_has_an_engine_box_on_the_row_of_the_target_with_the_default_marked() {
    for (w, h) in [(80, 24), (120, 40), (200, 60)] {
        let text = screen_of(&composer(), w, h);
        let row = row_with(&text, "Engine (");
        assert!(
            row.contains("Target"),
            "{w}x{h}: the Engine box shares the Target's row:\n{text}"
        );
        assert!(
            text.contains("(•) Cline") && text.contains("( ) OpenCode"),
            "{w}x{h}: both engines, the default marked:\n{text}"
        );
    }
}

#[test]
fn b787_the_down_and_up_keys_on_the_target_field_move_the_engine_and_wrap() {
    let mut app = composer();
    type_prompt(&mut app);
    press(&mut app, KeyCode::Tab); // Target
    press(&mut app, KeyCode::Down);
    assert!(screen_of(&app, 120, 40).contains("(•) OpenCode"));
    press(&mut app, KeyCode::Down);
    assert!(
        screen_of(&app, 120, 40).contains("(•) Cline"),
        "past the last engine it wraps"
    );
    press(&mut app, KeyCode::Up);
    assert!(
        screen_of(&app, 120, 40).contains("(•) OpenCode"),
        "up wraps the other way"
    );
    // The left and right keys still move the target and leave the engine where it is.
    press(&mut app, KeyCode::Right);
    let text = screen_of(&app, 120, 40);
    assert!(
        text.contains("(•) --new-repo") && text.contains("(•) OpenCode"),
        "{text}"
    );
}

#[test]
fn b787_the_up_and_down_keys_on_other_fields_do_not_touch_the_engine() {
    let mut app = composer();
    type_prompt(&mut app);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Up);
    assert!(screen_of(&app, 120, 40).contains("(•) Cline"));
}

#[test]
fn b787_a_start_carries_the_engine_only_when_one_was_chosen() {
    let mut app = composer();
    type_prompt(&mut app);
    assert_eq!(
        submitted(&mut app).engine,
        None,
        "no choice: the file's default_engine decides at the start"
    );
    let mut app = composer();
    type_prompt(&mut app);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Down);
    assert_eq!(submitted(&mut app).engine.as_deref(), Some("opencode"));
    // Choosing the default engine back is a choice too: it beats a default_engine of the file.
    let mut app = composer();
    app.set_default_engine(Some("opencode"));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('n'));
    type_prompt(&mut app);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Up);
    assert_eq!(submitted(&mut app).engine.as_deref(), Some("cline"));
}

#[test]
fn b787_default_engine_of_the_file_is_where_the_box_opens_and_is_not_sent_by_the_screen() {
    let mut app = App::new_direct(
        "n".into(),
        None,
        DirectSettings {
            no_rules: true,
            ..DirectSettings::default()
        },
    );
    app.set_default_engine(Some("opencode"));
    press(&mut app, KeyCode::Char('n'));
    let text = screen_of(&app, 120, 40);
    assert!(
        text.contains("(•) OpenCode") && text.contains("( ) Cline"),
        "{text}"
    );
    type_prompt(&mut app);
    s3_only(&mut app);
    assert_eq!(
        submitted(&mut app).engine,
        None,
        "the start reads the file itself"
    );
    // A word that is no engine falls back to the default engine on the screen; the start refuses it with the layer named.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_default_engine(Some("nonesuch"));
    press(&mut app, KeyCode::Char('n'));
    assert!(screen_of(&app, 120, 40).contains("(•) Cline"));
}

#[test]
fn b787_slash_engine_chooses_the_engine_for_the_runs_of_the_session_and_the_composer_opens_on_it() {
    let (mut app, mut clock) = chat_support::ready();
    let effects = chat_support::send(&mut app, &mut clock, "/engine opencode");
    assert!(effects.is_empty(), "{effects:?}");
    let text = chat_support::screen(&app);
    assert!(
        text.contains("New runs start on OpenCode")
            && text.contains("your choice for this session"),
        "{text}"
    );
    chat_support::send(&mut app, &mut clock, "/new");
    assert_eq!(app.screen(), Screen::Composer);
    let text = chat_support::screen(&app);
    assert!(
        text.contains("(•) OpenCode") && text.contains("( ) Cline"),
        "{text}"
    );
    type_prompt(&mut app);
    s3_only(&mut app);
    assert_eq!(submitted(&mut app).engine.as_deref(), Some("opencode"));
}

#[test]
fn b787_slash_engine_with_a_word_that_is_no_choice_changes_nothing_and_says_why() {
    let (mut app, mut clock) = chat_support::ready();
    // `/engine pi` is the form of the settings command and reaches the shell; the screen's word is the engine chooser for one name.
    let effects = chat_support::send(&mut app, &mut clock, "/engine pi");
    let text = chat_support::screen(&app);
    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, Effect::SubmitDirect { .. })),
        "{effects:?}"
    );
    assert!(
        text.contains("not available in this release") || text.contains("pi"),
        "{text}"
    );
    chat_support::send(&mut app, &mut clock, "/new");
    assert!(chat_support::screen(&app).contains("(•) Cline"));
}

fn opencode_run(extra: Vec<harness_protocol::envelope::Envelope>) -> (App, chat_support::Clock) {
    let mut events = vec![chat_support::env(
        9,
        EventType::SessionStarted,
        None,
        json!({"agentId": "direct", "engine": "opencode", "modelId": chat_support::MODEL}),
    )];
    events.extend(extra);
    chat_support::ready_with(events)
}

#[test]
fn b787_the_message_box_of_a_run_names_the_engine_the_run_is_on() {
    let (app, _) = opencode_run(vec![]);
    let text = chat_support::screen(&app);
    assert!(text.contains("Message to OpenCode"), "{text}");
    assert!(!text.contains("Message to Cline"), "{text}");
    let (app, _) = chat_support::ready();
    let text = chat_support::screen(&app);
    assert!(
        text.contains("Message to Cline"),
        "a run on the default engine reads as it always did:\n{text}"
    );
}

#[test]
fn b787_the_question_card_and_the_not_waiting_notice_name_the_engine_of_the_run() {
    let (app, _) = opencode_run(vec![chat_support::question()]);
    let text = chat_support::screen(&app);
    assert!(text.contains("OpenCode needs you"), "{text}");
    assert!(!text.contains("Cline needs you"), "{text}");
}
