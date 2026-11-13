//! The composer's model list and provider data sharing: a row that needs sharing is marked in the list, whether or not it can be picked yet. The
//! refusal itself is made by the one run-settings resolver that the command line, the composer and the follow-up share (`ham-core` tests), so the composer
//! shows the same words as the command line when the user submits a model that needs sharing while the setting is off.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Screen};
use ham_core::command::{Command, DirectArgs, parse};
use ham_core::models::{ModelPrefs, Models};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;

fn press(app: &mut App, c: KeyCode) {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE));
}

fn screen_text(app: &App, w: u16, h: u16) -> String {
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

fn models(smoked: bool) -> &'static Models {
    let mut shared = json!({"id": "a.shared", "name": "Shared", "vendor": "v", "alias": "shared", "baseModelId": "v.shared", "protocol": "converse",
                            "region": "us-east-1", "contextTokens": 100000, "maxOutputTokens": 8192, "requiresProviderSharing": true});
    if smoked {
        shared["smokeRecordId"] = json!("s6:x+K3:y");
    }
    Box::leak(Box::new(
        Models::parse(
            &json!({"schemaVersion": 1, "models": [
                {"id": "a.one", "name": "One", "vendor": "v", "alias": "one", "default": true, "baseModelId": "v.one", "protocol": "converse",
                 "region": "us-east-1", "contextTokens": 100000, "maxOutputTokens": 8192, "smokeRecordId": "s6:x+K3:y"},
                shared
            ]})
            .to_string(),
        )
        .unwrap(),
    ))
}

fn composer(models: &'static Models) -> App {
    let mut app = App::new_direct_with(
        models,
        "n".into(),
        None,
        DirectSettings {
            prefs: ModelPrefs::default(),
            rules: None,
            no_rules: true,
        },
    );
    press(&mut app, KeyCode::Char('n'));
    app
}

fn line_with(text: &str, needle: &str) -> String {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn b803_a_pending_row_that_needs_sharing_says_both() {
    let text = screen_text(&composer(models(false)), 200, 60);
    let row = line_with(&text, "a.shared");
    assert!(row.contains("needs provider data sharing"), "{text}");
    assert!(row.contains("pending"), "{text}");
    assert!(
        !line_with(&text, "a.one").contains("provider data sharing"),
        "{text}"
    );
}

#[test]
fn b803_a_selectable_row_that_needs_sharing_is_marked_and_can_be_picked() {
    let mut app = composer(models(true));
    let text = screen_text(&app, 120, 40);
    let row = line_with(&text, "a.shared");
    assert!(row.contains("needs provider data sharing"), "{text}");
    assert!(!row.contains("pending"), "{text}");
    for _ in 0..5 {
        press(&mut app, KeyCode::Tab);
    }
    press(&mut app, KeyCode::Down);
    assert_eq!(app.chosen_model(), Some("a.shared"));
}

#[test]
fn b803_the_mark_fits_on_an_80x24_screen() {
    let text = screen_text(&composer(models(true)), 80, 24);
    let row = line_with(&text, "shared");
    assert!(row.contains("provider data sharing"), "{text}");
}

// ---- the Options box takes `--provider-sharing on|off` -----------------------------------------------------------------------------------

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch));
    }
}

fn at_options() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    type_text(&mut app, "build x");
    press(&mut app, KeyCode::Tab); // Target
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right); // --s3-only
    for _ in 0..3 {
        press(&mut app, KeyCode::Tab); // Repository, Base ref, Options
    }
    app
}

fn from_argv(options: &[&str]) -> DirectArgs {
    let mut argv: Vec<String> = ["--prompt", "build x", "--s3-only"]
        .iter()
        .map(ToString::to_string)
        .collect();
    argv.extend(options.iter().map(ToString::to_string));
    argv.extend(["--direct", "--model", SONNET].map(ToString::to_string));
    match parse(&argv).unwrap().command {
        Command::StartDirect(args) => args,
        other => panic!("not a start: {other:?}"),
    }
}

fn submit(app: &mut App) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
}

#[test]
fn b803_the_provider_sharing_option_makes_the_command_lines_request() {
    for word in ["on", "off"] {
        let mut app = at_options();
        type_text(&mut app, &format!("--provider-sharing {word}"));
        let effects = submit(&mut app);
        let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
            panic!("expected one submit, got {effects:?}");
        };
        assert_eq!(*args, from_argv(&["--provider-sharing", word]));
        assert_eq!(args.provider_sharing, Some(word == "on"));
    }
}

#[test]
fn b803_a_provider_sharing_option_with_no_value_names_on_or_off_and_starts_nothing() {
    for typed in ["--provider-sharing", "--provider-sharing --goal off"] {
        let mut app = at_options();
        type_text(&mut app, typed);
        let effects = submit(&mut app);
        assert!(effects.is_empty(), "{typed:?} started a run: {effects:?}");
        let text = screen_text(&app, 200, 40);
        assert!(text.contains("on or off"), "{typed:?}:\n{text}");
    }
}
