//! Pasted text: the terminal app asks for bracketed paste, so a paste arrives as one `Event::Paste` instead
//! of one key press per character. On a screen with no text box it does nothing (before, a paste that held `cc` cancelled the run,
//! `q` quit, and `ff` finished it); in a text box it inserts the text; the same letters typed key by key still act as hotkeys.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Effect, Screen};
use harness_protocol::api::v1::StartDirectRunRequest;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const RUN: &str = "01K6A0000000000000000000R1";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

fn new_app() -> App {
    App::new_direct("n".into(), None, DirectSettings::default())
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

thread_local! {
    static LAST_KEY: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// A key that arrives 400 ms after the one before it (the first one now): a person's pace, past the 300 ms a second `c` waits for the
/// preview of the cancel.
fn press_slowly(app: &mut App, c: KeyCode) -> Vec<Effect> {
    let at = LAST_KEY.with(|last| {
        let next = last.get().map_or_else(std::time::Instant::now, |t| {
            t + std::time::Duration::from_millis(400)
        });
        last.set(Some(next));
        next
    });
    app.handle_key_at(KeyEvent::new(c, KeyModifiers::NONE), at)
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

/// The Dashboard of a run (no events read yet), with the notice row empty.
fn dashboard() -> App {
    let mut app = new_app();
    app.open_run(RUN);
    assert_eq!(app.screen(), Screen::Dashboard);
    app
}

#[test]
fn a_paste_on_the_runs_screen_does_nothing_and_says_so() {
    for text in ["q", "n", "r", "ccqff", "j\nk\nq"] {
        let mut app = new_app();
        let effects = app.handle_paste(text.to_string());
        assert!(effects.is_empty(), "paste {text:?} caused {effects:?}");
        assert_eq!(app.screen(), Screen::Runs, "paste {text:?} left the screen");
        let screen = screen_text(&app, 80, 24);
        assert!(
            screen.contains("paste ignored"),
            "paste {text:?} gave no notice:\n{screen}"
        );
    }
}

#[test]
fn a_paste_on_the_dashboard_cannot_cancel_quit_or_finish() {
    let mut app = dashboard();
    for text in ["cc", "q", "ff", "ccqff", "c\nc", "a", "C", "o", "d"] {
        let effects = app.handle_paste(text.to_string());
        assert!(effects.is_empty(), "paste {text:?} caused {effects:?}");
        assert_eq!(app.screen(), Screen::Dashboard, "paste {text:?} moved");
    }
    // The paste armed nothing: the first `c` of a typed pair only asks for the second.
    assert!(press_slowly(&mut app, KeyCode::Char('c')).is_empty());
    let screen = screen_text(&app, 100, 30);
    assert!(screen.contains("press c again to cancel"), "{screen}");
    // The second typed `c` cancels: the keyboard path is unchanged.
    let cancel = press_slowly(&mut app, KeyCode::Char('c'));
    assert!(matches!(cancel.as_slice(), [Effect::Cancel { run_id }] if run_id == RUN));
}

#[test]
fn a_paste_between_two_typed_keys_does_not_complete_the_pair() {
    let mut app = dashboard();
    assert!(press_slowly(&mut app, KeyCode::Char('c')).is_empty());
    // The paste is not the second `c`: the typed pair is broken, not completed.
    assert!(app.handle_paste("c".to_string()).is_empty());
    assert!(press_slowly(&mut app, KeyCode::Char('c')).is_empty());
}

#[test]
fn text_typed_key_by_key_still_acts_as_hotkeys() {
    let mut app = new_app();
    assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Effect::Quit]);
}

#[test]
fn a_paste_into_the_composer_prompt_inserts_the_text_whole() {
    let mut app = new_app();
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    let effects = app.handle_paste("first line\r\nsecond\tline\rthird".to_string());
    assert!(effects.is_empty(), "{effects:?}");
    let screen = screen_text(&app, 100, 40);
    for want in ["first line", "second    line", "third"] {
        assert!(screen.contains(want), "missing {want:?}:\n{screen}");
    }
    // Typing goes on after the pasted text.
    press(&mut app, KeyCode::Char('!'));
    assert!(screen_text(&app, 100, 40).contains("third!"));
}

#[test]
fn a_paste_strips_escape_sequences_and_controls() {
    let mut app = new_app();
    press(&mut app, KeyCode::Char('n'));
    app.handle_paste("a\u{1b}[1mb\u{1b}]0;windowname\u{7}c\u{8}d".to_string());
    let screen = screen_text(&app, 100, 40);
    assert!(screen.contains("abcd"), "{screen}");
    assert!(!screen.contains('\u{1b}'), "{screen}");
    assert!(!screen.contains("windowname"), "{screen}");
}

#[test]
fn a_paste_into_a_one_line_field_drops_line_breaks_and_spaces() {
    let mut app = new_app();
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Tab); // Target
    press(&mut app, KeyCode::Tab); // Repository
    app.handle_paste(" cc:my \n repo ".to_string());
    assert!(screen_text(&app, 100, 40).contains("cc:myrepo"));
}

#[test]
fn a_paste_on_a_field_without_a_text_box_is_ignored_and_never_acts_as_keys() {
    let mut app = new_app();
    press(&mut app, KeyCode::Char('n'));
    // Tab to the Model field, where `d` saves the default model.
    for _ in 0..5 {
        press(&mut app, KeyCode::Tab);
    }
    let effects = app.handle_paste("dd \n".to_string());
    assert!(
        effects.is_empty(),
        "a paste on the Model field: {effects:?}"
    );
    // The Target field: a space or an arrow key changes it; a paste does not.
    let mut app = new_app();
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Tab);
    let before = screen_text(&app, 100, 40);
    assert!(app.handle_paste("   ".to_string()).is_empty());
    let after = screen_text(&app, 100, 40);
    assert_eq!(
        before.lines().find(|l| l.contains("(•)")),
        after.lines().find(|l| l.contains("(•)")),
        "the target changed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paste_through_the_event_loop_does_not_cancel_the_run_or_end_the_app() {
    let fake = Fake::start().await;
    let api = Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let run = api
        .start_direct_run(
            &serde_json::from_value::<StartDirectRunRequest>(
                json!({"prompt": "paste", "target": {"mode": "s3-only"}, "model": SONNET}),
            )
            .unwrap(),
            "k-paste",
        )
        .await
        .unwrap()
        .run_id;
    let mut app = new_app();
    let first = app.open_run(&run);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    // The bracketed frame is one event; the paste of `ccqff` must be harmless, then a typed `q` ends the loop.
    tx.send(Event::Paste("ccqff".into())).unwrap();
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let dir = support::scratch("tui-paste-loop");
    let still_running = tokio::time::timeout(
        Duration::from_millis(600),
        ham::tui::event_loop(
            &mut term,
            api.clone(),
            dir.clone(),
            app,
            first,
            rx,
            Duration::from_millis(50),
        ),
    )
    .await;
    assert!(
        still_running.is_err(),
        "the loop ended on a pasted q: {still_running:?}"
    );
    assert!(
        !fake
            .mem
            .seen()
            .iter()
            .any(|s| s.method == "POST" && s.path.ends_with("/cancel")),
        "a pasted cc cancelled the run"
    );
    drop(tx);
}
