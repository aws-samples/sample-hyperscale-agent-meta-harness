//! The "press c again" and "press f again" questions of the run screen. The first `f`
//! asks for a second one in the notice row; the first `c` shows the same prepared preview as `/cancel`, with
//! the run frozen in it, and the second `c` counts only 300 ms after the preview. A key other than the one asked for withdraws the question, and
//! the notice or the preview goes with it: the screen must not keep asking for a second key that now only asks again.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::KeyCode;
use ham::tui::keys::Mode;
use ham::tui::{App, Effect};

/// The run on screen in browse mode, with `extra` events.
fn browsing(extra: Vec<harness_protocol::envelope::Envelope>) -> (App, Clock) {
    let (mut app, mut clock) = ready_with(extra);
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    assert_eq!(app.mode(), Mode::Browse);
    (app, clock)
}

const OTHER_KEYS: [KeyCode; 6] = [
    KeyCode::Char('k'),
    KeyCode::Char('j'),
    KeyCode::Char('z'),
    KeyCode::Tab,
    KeyCode::Up,
    KeyCode::Char(' '),
];

#[test]
fn another_key_after_c_withdraws_the_question_and_clears_the_notice() {
    for other in OTHER_KEYS {
        let (mut app, mut clock) = browsing(vec![]);
        press(&mut app, &mut clock, KeyCode::Char('c'), 400);
        let asked = screen(&app);
        assert!(
            asked.contains("press c again to cancel run"),
            "TRIGGER: {other:?}\n{asked}"
        );
        press(&mut app, &mut clock, other, 400);
        let after = screen(&app);
        assert!(!after.contains("press c again"), "{other:?}\n{after}");
    }
}

#[test]
fn another_key_after_f_withdraws_the_question_and_clears_the_notice() {
    for other in OTHER_KEYS {
        let (mut app, mut clock) = browsing(vec![question()]);
        press(&mut app, &mut clock, KeyCode::Char('f'), 400);
        let asked = screen(&app);
        assert!(
            asked.contains("press f again to finish run"),
            "TRIGGER: {other:?}\n{asked}"
        );
        press(&mut app, &mut clock, other, 400);
        let after = screen(&app);
        assert!(!after.contains("press f again"), "{other:?}\n{after}");
    }
}

#[test]
fn the_second_c_still_cancels_and_the_second_f_still_finishes() {
    let (mut app, mut clock) = browsing(vec![]);
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Cancel { .. })),
        "{effects:?}"
    );
    let (mut app, mut clock) = browsing(vec![question()]);
    press(&mut app, &mut clock, KeyCode::Char('f'), 400);
    let effects = press(&mut app, &mut clock, KeyCode::Char('f'), 400);
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Answer { .. })),
        "{effects:?}"
    );
}

#[test]
fn c_then_f_replaces_the_cancel_question_with_the_finish_question() {
    let (mut app, mut clock) = browsing(vec![question()]);
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    press(&mut app, &mut clock, KeyCode::Char('f'), 400);
    let text = screen(&app);
    assert!(!text.contains("press c again"), "{text}");
    assert!(text.contains("press f again to finish run"), "{text}");
}

#[test]
fn a_notice_that_is_not_the_question_stays_when_another_key_comes() {
    let (mut app, mut clock) = browsing(vec![]);
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    // Something else took the notice row (the answer of a copy, say); the next key must not wipe it.
    app.update(ham::tui::Msg::Copied(Ok(
        "Copied 5 bytes with pbcopy.".into()
    )));
    assert!(screen(&app).contains("Copied 5 bytes with pbcopy."));
    press(&mut app, &mut clock, KeyCode::Char('k'), 400);
    assert!(screen(&app).contains("Copied 5 bytes with pbcopy."));
}

// ---- `c c` shows the preview of `/cancel` and its second key waits 300 ms -----------------------------------

/// The line a typed `/cancel` shows in its preview.
fn typed_cancel_preview() -> String {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/cancel");
    let text = screen(&app);
    text.lines()
        .find(|l| l.contains("Cancel run"))
        .unwrap_or_else(|| panic!("no preview:\n{text}"))
        .trim_matches(|c: char| c == '\u{2502}' || c == ' ')
        .to_string()
}

#[test]
fn the_first_c_shows_the_same_preview_as_slash_cancel_with_the_run_in_it() {
    let want = typed_cancel_preview();
    assert!(want.contains(RUN), "{want}");
    let (mut app, mut clock) = browsing(vec![]);
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    assert!(effects.is_empty(), "the first c cancelled: {effects:?}");
    let text = screen(&app);
    assert!(
        text.contains(&want),
        "the preview of /cancel is not on the screen:\n{want}\n{text}"
    );
    assert!(text.contains("Confirm"), "{text}");
    assert!(
        text.contains(&format!("press c again to cancel run {RUN}")),
        "{text}"
    );
}

#[test]
fn a_second_c_within_300_ms_of_the_preview_is_ignored_and_the_preview_stays() {
    let (mut app, mut clock) = browsing(vec![]);
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 100);
    assert!(
        effects.is_empty(),
        "a c 100 ms later cancelled: {effects:?}"
    );
    assert!(screen(&app).contains("Cancel run"), "{}", screen(&app));
    // 250 ms after that it is 350 ms after the preview: this c counts.
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 250);
    assert!(
        matches!(effects.as_slice(), [Effect::Cancel { run_id }] if run_id == RUN),
        "{effects:?}"
    );
    assert!(
        !screen(&app).contains("Cancel run"),
        "the preview stays after it ran"
    );
}

#[test]
fn a_c_held_down_cannot_confirm_inside_the_first_300_ms() {
    let (mut app, mut clock) = browsing(vec![]);
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    // Key repeat: a c every 30 ms for 270 ms.
    for _ in 0..9 {
        let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 30);
        assert!(effects.is_empty(), "{effects:?}");
    }
}

#[test]
fn esc_withdraws_the_preview_and_stays_on_the_run_screen() {
    let (mut app, mut clock) = browsing(vec![]);
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    let effects = press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(
        app.screen(),
        ham::tui::Screen::Dashboard,
        "Esc left the run"
    );
    assert!(!screen(&app).contains("Cancel run"), "{}", screen(&app));
    // The question is withdrawn: one c asks again and does not cancel.
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    assert!(effects.is_empty(), "{effects:?}");
}

#[test]
fn enter_does_not_confirm_the_hotkey_preview() {
    let (mut app, mut clock) = browsing(vec![]);
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    assert!(
        screen(&app).contains("Cancel run"),
        "TRIGGER: no preview is up:\n{}",
        screen(&app)
    );
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 400);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(!screen(&app).contains("Cancel run"), "{}", screen(&app));
}

#[test]
fn alt_c_is_the_letter_c_in_browse_mode_so_it_counts_only_after_300_ms() {
    use crossterm::event::KeyModifiers;
    let (mut app, mut clock) = browsing(vec![]);
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    // Esc and c written in one chunk are read as Alt-c, which the key table reads as the letter c in browse mode.
    let alt_c = |app: &mut App, clock: &mut Clock, ms| {
        chat_support::press_mod(app, clock, KeyCode::Char('c'), KeyModifiers::ALT, ms)
    };
    let soon = alt_c(&mut app, &mut clock, 100);
    assert!(soon.is_empty(), "Alt-c 100 ms later confirmed: {soon:?}");
    assert!(screen(&app).contains("Cancel run"), "{}", screen(&app));
    let effects = alt_c(&mut app, &mut clock, 400);
    assert!(
        matches!(effects.as_slice(), [Effect::Cancel { run_id }] if run_id == RUN),
        "{effects:?}"
    );
}

#[test]
fn the_other_hotkeys_are_not_held_back_by_the_burst_guard() {
    // Only the second c of `c c` waits for the preview delay. A hotkey that follows another key at once still acts: the live drivers press
    // `f` twice in two writes, and a paste cannot reach the hotkeys because a bracketed paste is text.
    let (mut app, mut clock) = browsing(vec![question()]);
    press(&mut app, &mut clock, KeyCode::Char('k'), 400);
    let effects = press(&mut app, &mut clock, KeyCode::Char('f'), 1);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        screen(&app).contains("press f again to finish run"),
        "{}",
        screen(&app)
    );
    let effects = press(&mut app, &mut clock, KeyCode::Char('f'), 1);
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Answer { .. })),
        "{effects:?}"
    );
}
