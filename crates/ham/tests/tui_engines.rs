//! The engine-neutral screen. `/engines` lists
//! `cline` as selected and the roadmap engines as "not available in this release"; the popup, the status words and the forwarding of a
//! typed `//word` follow the capability record of the focused run, so two recorded handshakes render differently with no code change.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::tui::ext::Job;
use ham::tui::{App, Effect};
use serde_json::{Value, json};

fn record(file: &str) -> Value {
    let path = format!(
        "{}/../ham-core/tests/fixtures/capability/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn with_record(file: &str) -> App {
    let mut events = base_events();
    events.push(capability_event(4, record(file)));
    ready(events)
}

fn first_row(text: &str, word: &str) -> String {
    text.lines()
        .find(|l| l.trim_start_matches(['\u{2502}', ' ']).starts_with(word))
        .unwrap_or_else(|| panic!("no row starts with {word}:\n{text}"))
        .to_string()
}

#[test]
fn engines_lists_cline_selected_and_every_other_engine_as_not_available() {
    let mut app = with_record("first-engine.json");
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/engines");
    assert!(effects.is_empty(), "no request: {effects:?}");
    let text = screen(&app, 120, 40);
    assert!(text.contains("Engines"), "{text}");
    let cline = first_row(&text, "cline");
    assert!(cline.contains("selected"), "{cline}");
    assert!(!cline.contains("not available in this release"), "{cline}");
    assert!(
        cline.contains("Cline 3.0.65") || cline.contains("cline 3.0.65"),
        "{cline}"
    );
    let mut others = 0;
    for engine in ["pi", "opencode", "codex", "kiro"] {
        let row = first_row(&text, engine);
        assert!(row.contains("not available in this release"), "{row}");
        others += 1;
    }
    assert_eq!(others, 4);
    // The box is emptied: the line was sent.
    assert_eq!(app.chat_text().as_deref(), Some(""));
}

#[test]
fn without_a_record_the_panel_says_capabilities_unknown() {
    let mut app = ready(base_events());
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/engines");
    let text = screen(&app, 120, 40);
    let cline = first_row(&text, "cline");
    assert!(
        cline.contains("selected") && cline.contains("capabilities unknown"),
        "{cline}"
    );
}

#[test]
fn the_popup_follows_the_record_with_no_code_change() {
    // The second engine advertises commands; the popup lists them under a double slash.
    let mut app = with_record("second-engine.json");
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "/");
    let text = screen(&app, 100, 40);
    assert!(
        text.contains("/cancel"),
        "the built commands are listed: {text}"
    );
    // A second slash asks for the engine's own commands.
    type_text(&mut app, &mut clock, "/");
    let text = screen(&app, 100, 40);
    assert!(text.contains("//review"), "{text}");
    assert!(text.contains("//init"), "{text}");
    assert!(!text.contains("/cancel"), "only the engine's: {text}");
    // /help lists them too.
    let mut app = with_record("second-engine.json");
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/help");
    assert!(
        screen(&app, 120, 50).contains("//review"),
        "{}",
        screen(&app, 120, 50)
    );
    // The first engine advertises none.
    let mut app = with_record("first-engine.json");
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "//");
    let text = screen(&app, 100, 40);
    assert!(!text.contains("//review"), "{text}");
    assert!(
        !text.contains("/cancel"),
        "no engine commands, nothing to list: {text}"
    );
}

#[test]
fn a_command_the_record_does_not_offer_is_not_in_the_popup() {
    let mut r = record("first-engine.json");
    r["harness"]["controls"] = json!(["message.next"]);
    let mut events = base_events();
    events.push(capability_event(4, r));
    let mut app = ready(events);
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "/no");
    let text = screen(&app, 100, 40);
    assert!(!text.contains("/now"), "{text}");
    let mut app = with_record("first-engine.json");
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "/no");
    assert!(screen(&app, 100, 40).contains("/now"));
}

#[test]
fn the_status_line_follows_the_record() {
    let header = |app: &App| screen(app, 140, 30).lines().next().unwrap().to_string();
    assert!(
        header(&with_record("first-engine.json")).contains("mode act"),
        "{}",
        header(&with_record("first-engine.json"))
    );
    assert!(header(&with_record("second-engine.json")).contains("mode build"));
    assert!(!header(&ready(base_events())).contains("mode "));
}

#[test]
fn a_typed_double_slash_command_the_engine_advertises_is_sent_to_it_with_one_slash() {
    let mut app = with_record("second-engine.json");
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "//review the diff");
    let sent: Vec<_> = jobs(&effects)
        .into_iter()
        .filter_map(|j| match j {
            Job::SendMessage { request, .. } => Some(request.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "{effects:?}");
    assert_eq!(sent[0].text, "/review the diff");
    assert_eq!(sent[0].mode, harness_protocol::messages::MessageMode::Next);
}

#[test]
fn a_double_slash_the_engine_does_not_advertise_is_refused_and_nothing_is_sent() {
    let mut app = with_record("first-engine.json");
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "//review the diff");
    assert!(jobs(&effects).is_empty(), "{effects:?}");
    let text = screen(&app, 100, 40);
    assert!(
        text.contains("/send"),
        "the refusal names the escape: {text}"
    );
    assert_eq!(app.chat_text().as_deref(), Some("//review the diff"));
}

#[test]
fn pasted_text_is_never_forwarded_as_an_engine_command() {
    let mut app = with_record("second-engine.json");
    let mut clock = Clock::new();
    let at = clock.after(50);
    app.handle_paste_at("//review everything".to_string(), at);
    let effects = enter(&mut app, &mut clock, 400);
    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, Effect::Ext(Job::SendMessage { .. }))),
        "{effects:?}"
    );
    assert!(
        screen(&app, 100, 40).contains("pasted"),
        "{}",
        screen(&app, 100, 40)
    );
    let _ = KeyCode::Enter;
}

#[test]
fn a_mode_change_in_the_stored_conversation_shows_in_the_status_words() {
    let mut app = with_record("first-engine.json");
    let header = |app: &App| screen(app, 140, 30).lines().next().unwrap().to_string();
    assert!(header(&app).contains("mode act"), "{}", header(&app));
    app.update(answer(
        ham_core::stored::Read::First,
        page(
            "live",
            vec![
                msg(0, 0, "a", "Switching to plan mode."),
                rec(
                    0,
                    1,
                    "current_mode_update",
                    json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
                ),
            ],
            None,
            Some("c"),
        ),
    ));
    assert!(header(&app).contains("mode plan"), "{}", header(&app));
}
