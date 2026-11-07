//! The dashboard keys and the repeated cancel press. A run opens in typing mode, so a first `c` is a letter in the message box; the
//! typing footer now says to press Esc first, with the hotkeys that follow it when the line has room. A second `c` that comes less than
//! 300 ms after the first (a key held down, a double tap) is still ignored on purpose, but no longer silently: the preview that is up says
//! the press came too soon. A second `c` a second later cancels once. The 300 ms delay itself is pinned by `tui_confirm_notice.rs`.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::KeyCode;
use ham::tui::keys::Mode;
use ham::tui::{App, Effect};

const SIZES: [(u16, u16); 4] = [(80, 24), (100, 30), (120, 40), (200, 60)];

/// The run on screen in browse mode.
fn browsing() -> (App, Clock) {
    let (mut app, mut clock) = ready();
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    assert_eq!(app.mode(), Mode::Browse);
    (app, clock)
}

fn screen_at(app: &App, w: u16, h: u16) -> String {
    text_of(&buffer(app, w, h))
}

/// The footer: the last row with text.
fn footer(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap()
        .to_string()
}

/// The text with its box-drawing characters dropped and its white space squashed, so a sentence that wrapped in a panel can be found.
fn squashed(text: &str) -> String {
    text.replace(['│', '┌', '┐', '└', '┘', '─'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn cancels(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Cancel { .. }))
        .count()
}

const TOO_SOON: &str = "too soon: a second c within 0.3 s of the first is ignored; press c again";

// ---- the footer of typing mode says to press Esc first ---------------------------------------------------------------------------

#[test]
fn in_typing_mode_a_c_is_a_letter_and_the_footer_says_esc_first_then_c_c() {
    for (w, h) in SIZES {
        let (mut app, mut clock) = ready();
        assert_eq!(app.mode(), Mode::Typing);
        // TRIGGER PRESENT: the c is text, it starts no cancel and shows no preview.
        let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
        assert!(effects.is_empty(), "{w}x{h}: {effects:?}");
        assert_eq!(app.chat_text().as_deref(), Some("c"), "{w}x{h}");
        let text = screen_at(&app, w, h);
        assert!(!text.contains("Cancel run"), "{w}x{h}:\n{text}");
        let line = footer(&text);
        assert!(
            line.chars().count() <= usize::from(w),
            "{w}x{h}: {line:?} is wider than the screen"
        );
        assert!(
            line.starts_with("Enter send \u{b7} Ctrl-J new line \u{b7} Esc browse"),
            "{w}x{h}: {line:?}"
        );
        assert!(
            line.ends_with("Ctrl-C clear, then quit"),
            "{w}x{h}: {line:?}"
        );
        match w {
            // 80 columns have room for the short form only.
            80 => assert_eq!(
                line,
                "Enter send \u{b7} Ctrl-J new line \u{b7} Esc browse (c c cancel) \u{b7} Ctrl-C clear, then quit",
                "{w}x{h}"
            ),
            _ => assert!(
                line.contains("Esc browse, then a artifacts, c c cancel \u{b7} Ctrl-C"),
                "{w}x{h}: {line:?}"
            ),
        }
    }
}

#[test]
fn the_footer_of_a_question_keeps_its_own_words() {
    let (app, _clock) = ready_with(vec![question()]);
    let line = footer(&screen_at(&app, 200, 60));
    assert!(
        line.contains("Enter answer \u{b7} Esc browse, then p message"),
        "{line:?}"
    );
    assert!(!line.contains("c c cancel"), "{line:?}");
}

#[test]
fn in_browse_mode_the_footer_is_the_usual_one_and_c_asks() {
    for (w, h) in SIZES {
        let (mut app, mut clock) = browsing();
        let line = footer(&screen_at(&app, w, h));
        assert!(
            line.contains("a artifacts \u{b7} c cancel \u{b7} Esc runs"),
            "{w}x{h}: {line:?}"
        );
        let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
        assert!(effects.is_empty(), "{w}x{h}: {effects:?}");
        assert!(
            squashed(&screen_at(&app, w, h)).contains("press c again to cancel run"),
            "{w}x{h}"
        );
    }
}

// ---- a second c that comes too soon is explained, never dropped silently ---------------------------------------------------------

#[test]
fn a_second_c_inside_300_ms_is_explained_on_the_screen_and_the_preview_stays() {
    for (w, h) in SIZES {
        let (mut app, mut clock) = browsing();
        press(&mut app, &mut clock, KeyCode::Char('c'), 400);
        let before = squashed(&screen_at(&app, w, h));
        assert!(
            before.contains("press c again to cancel run") && !before.contains("too soon"),
            "TRIGGER ABSENT {w}x{h}: {before}"
        );
        let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 100);
        assert!(
            effects.is_empty(),
            "{w}x{h}: a c 100 ms later cancelled: {effects:?}"
        );
        let after = squashed(&screen_at(&app, w, h));
        assert!(after.contains(TOO_SOON), "{w}x{h}: {after}");
        assert!(
            after.contains("press c again to cancel run") && after.contains("Cancel run"),
            "the preview stays: {w}x{h}: {after}"
        );
        // 250 ms after that it is 350 ms after the preview: this c counts, and the explanation goes with the preview.
        let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 250);
        assert_eq!(cancels(&effects), 1, "{w}x{h}: {effects:?}");
        let done = squashed(&screen_at(&app, w, h));
        assert!(!done.contains("too soon"), "{w}x{h}: {done}");
        assert!(!done.contains("Cancel run"), "{w}x{h}: {done}");
    }
}

#[test]
fn the_early_press_does_not_move_the_start_of_the_300_ms() {
    let (mut app, mut clock) = browsing();
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    press(&mut app, &mut clock, KeyCode::Char('c'), 200);
    // 150 ms after the early press is 350 ms after the preview, not 150: it counts.
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 150);
    assert_eq!(cancels(&effects), 1, "{effects:?}");
}

#[test]
fn the_explanation_goes_with_the_preview() {
    for withdraw in [KeyCode::Esc, KeyCode::Char('k'), KeyCode::Tab] {
        let (mut app, mut clock) = browsing();
        press(&mut app, &mut clock, KeyCode::Char('c'), 400);
        press(&mut app, &mut clock, KeyCode::Char('c'), 100);
        assert!(
            squashed(&screen(&app)).contains("too soon"),
            "TRIGGER ABSENT: {withdraw:?}"
        );
        press(&mut app, &mut clock, withdraw, 400);
        let text = squashed(&screen(&app));
        assert!(!text.contains("too soon"), "{withdraw:?}: {text}");
        assert!(!text.contains("Cancel run"), "{withdraw:?}: {text}");
        // The next c asks again, with no explanation left over.
        let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
        assert!(effects.is_empty(), "{withdraw:?}: {effects:?}");
        let text = squashed(&screen(&app));
        assert!(
            text.contains("press c again to cancel run") && !text.contains("too soon"),
            "{withdraw:?}: {text}"
        );
    }
}

#[test]
fn a_c_held_down_is_explained_once_and_cancels_nothing() {
    let (mut app, mut clock) = browsing();
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    // Key repeat: a c every 30 ms for 270 ms.
    for _ in 0..9 {
        let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 30);
        assert!(effects.is_empty(), "{effects:?}");
    }
    let text = squashed(&screen(&app));
    assert_eq!(text.matches("too soon").count(), 1, "{text}");
}

#[test]
fn two_c_presses_a_second_apart_cancel_once() {
    let (mut app, mut clock) = browsing();
    let first = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    assert!(first.is_empty(), "{first:?}");
    let second = press(&mut app, &mut clock, KeyCode::Char('c'), 1000);
    assert!(
        matches!(second.as_slice(), [Effect::Cancel { run_id }] if run_id == RUN),
        "{second:?}"
    );
    // A third press a second later starts a new question; it does not cancel again.
    let third = press(&mut app, &mut clock, KeyCode::Char('c'), 1000);
    assert!(third.is_empty(), "{third:?}");
    assert!(squashed(&screen(&app)).contains("press c again to cancel run"));
}

#[test]
fn the_300_ms_delay_itself_is_unchanged() {
    // A c 299 ms after the preview is early; one 300 ms after counts (the pins of tui_confirm_notice.rs, read at the edge).
    let (mut app, mut clock) = browsing();
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    let early = press(&mut app, &mut clock, KeyCode::Char('c'), 299);
    assert!(early.is_empty(), "{early:?}");
    let (mut app, mut clock) = browsing();
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    let on_time = press(&mut app, &mut clock, KeyCode::Char('c'), 300);
    assert_eq!(cancels(&on_time), 1, "{on_time:?}");
}
