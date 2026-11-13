//! `push` typed with no arguments: the box opens a preview with an
//! editable slot for the git URL. The run id is the focused run and the branch is the run's own, `harness/<runId>`. Enter on an empty slot is
//! refused, a URL that carries a credential is refused before anything runs (and the refusal never repeats it), and a good URL goes through the
//! same parser as `/push <url>`: the confirmation that asks for the last four characters of the run id follows, then the terminal is handed over.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::KeyCode;
use ham::tui::{App, Effect};

const URL: &str = "https://git.example.com/team/app.git";

fn last_four() -> &'static str {
    &RUN[RUN.len() - 4..]
}

/// The screen at a width where the preview line does not wrap.
fn wide(app: &App) -> String {
    text_of(&buffer(app, 220, 40))
}

/// What the confirmation asks for, as the preview words it.
fn asks_for_the_last_four(text: &str) -> bool {
    text.contains(&format!("Type {}, then press Enter", last_four()))
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(ToString::to_string).collect()
}

/// The run on screen with `/push` sent: the slot is open.
fn asking(line: &str) -> (App, Clock) {
    let (mut app, mut clock) = ready();
    let effects = send(&mut app, &mut clock, line);
    assert!(effects.is_empty(), "{effects:?}");
    (app, clock)
}

#[test]
fn push_with_no_arguments_opens_a_preview_with_an_editable_url_slot() {
    let (mut app, mut clock) = asking("/push");
    let text = screen(&app);
    assert!(!text.contains("needs its"), "the refusal is gone:\n{text}");
    assert!(!text.contains("Nothing was sent"), "{text}");
    assert!(text.contains(&format!("Push run {RUN}")), "{text}");
    assert!(
        text.contains(&format!("harness/{RUN}")),
        "the default branch is the run's own:\n{text}"
    );
    assert!(text.contains("git URL"), "{text}");
    // The box keeps what was typed; the URL goes into the slot and not into the box.
    assert_eq!(app.chat_text().as_deref(), Some("/push"));
    type_text(&mut app, &mut clock, URL);
    assert_eq!(
        app.chat_text().as_deref(),
        Some("/push"),
        "the URL went into the box"
    );
    assert!(screen(&app).contains(URL), "{}", screen(&app));
    // The slot is edited like a box: Backspace takes the last character and the caret can move.
    press(&mut app, &mut clock, KeyCode::Backspace, 50);
    press(&mut app, &mut clock, KeyCode::Char('t'), 50);
    clock.after(300);
    assert!(screen(&app).contains(URL), "{}", screen(&app));
}

#[test]
fn the_url_then_goes_through_the_same_confirmation_as_a_typed_push() {
    let (mut app, mut clock) = asking("/push");
    type_text(&mut app, &mut clock, URL);
    let effects = enter(&mut app, &mut clock);
    assert!(effects.is_empty(), "{effects:?}");
    let text = wide(&app);
    assert!(
        text.contains(&format!("Push run {RUN} to {URL} as branch harness/{RUN}")),
        "{text}"
    );
    assert!(asks_for_the_last_four(&text), "{text}");
    // Enter before the last four characters are typed runs nothing.
    clock.after(400);
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 10);
    assert!(effects.is_empty(), "{effects:?}");
    type_text(&mut app, &mut clock, last_four());
    let effects = enter(&mut app, &mut clock);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["push", RUN, URL])
        }]
    );
    assert_eq!(app.chat_text().as_deref(), Some(""));
}

#[test]
fn a_branch_given_on_the_line_is_the_branch_of_the_preview() {
    let (mut app, mut clock) = asking("/push --branch review/one");
    let text = screen(&app);
    assert!(text.contains("review/one"), "{text}");
    assert!(!text.contains(&format!("harness/{RUN}")), "{text}");
    type_text(&mut app, &mut clock, URL);
    enter(&mut app, &mut clock);
    // Characters typed less than 300 ms after the confirmation is drawn are ignored, like an Enter.
    clock.after(400);
    type_text(&mut app, &mut clock, last_four());
    let effects = enter(&mut app, &mut clock);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["push", RUN, "--branch", "review/one", URL])
        }]
    );
}

#[test]
fn an_empty_slot_is_refused_and_stays_open() {
    let (mut app, mut clock) = asking("/push");
    let effects = enter(&mut app, &mut clock);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(text.contains("Enter the git"), "{text}");
    assert!(
        !asks_for_the_last_four(&text),
        "no confirmation without a URL:\n{text}"
    );
    // The slot stays open and still takes the real URL.
    type_text(&mut app, &mut clock, URL);
    let effects = enter(&mut app, &mut clock);
    assert!(effects.is_empty());
    assert!(asks_for_the_last_four(&wide(&app)), "{}", wide(&app));
}

#[test]
fn a_url_that_carries_a_credential_is_refused_and_the_refusal_does_not_repeat_it() {
    let secret = ["pw", "4711"].concat();
    let bad = format!("https://user:{secret}@git.example.com/team/app.git");
    let (mut app, mut clock) = asking("/push");
    type_text(&mut app, &mut clock, &bad);
    let effects = enter(&mut app, &mut clock);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(text.contains("carries a credential"), "{text}");
    assert!(!asks_for_the_last_four(&text), "no confirmation:\n{text}");
    for line in text.lines().filter(|l| l.contains("credential")) {
        assert!(!line.contains(&secret), "the refusal repeats it: {line}");
    }
    // Nothing is stored: Esc closes the slot, and the box holds only the command word.
    press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert_eq!(app.chat_text().as_deref(), Some("/push"));
    let after = screen(&app);
    assert!(!after.contains(&secret), "{after}");
    assert!(!format!("{app:?}").contains(&secret));
}

#[test]
fn the_confirmation_shows_the_url_with_its_user_info_masked() {
    // `ssh://git@host/...` is allowed (an ssh account name); the confirmation draws the URL as `mask_userinfo` writes it.
    let url = "ssh://git@git.example.com/team/app.git";
    let (mut app, mut clock) = asking("/push");
    type_text(&mut app, &mut clock, url);
    enter(&mut app, &mut clock);
    let text = wide(&app);
    let shown = ham_core::push::mask_userinfo(url);
    assert!(
        text.contains(&format!("Push run {RUN} to {shown} as branch")),
        "{text}"
    );
}

#[test]
fn esc_closes_the_slot_and_keeps_the_command_word_in_the_box() {
    let (mut app, mut clock) = asking("/push");
    type_text(&mut app, &mut clock, URL);
    let effects = press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert!(effects.is_empty());
    assert_eq!(app.chat_text().as_deref(), Some("/push"));
    let text = screen(&app);
    assert!(!text.contains("git URL"), "the slot stayed:\n{text}");
    // Enter again opens a fresh, empty slot.
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 400);
    assert!(effects.is_empty());
    let text = screen(&app);
    assert!(text.contains("git URL"), "{text}");
    assert!(!text.contains(URL), "the old URL came back:\n{text}");
}

#[test]
fn a_pasted_url_goes_into_the_slot_and_is_not_run() {
    let (mut app, mut clock) = asking("/push");
    let at = clock.after(100);
    assert!(app.handle_paste_at(URL.to_string(), at).is_empty());
    assert_eq!(app.chat_text().as_deref(), Some("/push"));
    assert!(screen(&app).contains(URL), "{}", screen(&app));
    // Enter inside the quiet time that follows a paste is not an answer.
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 50);
    assert!(effects.is_empty());
    assert!(!asks_for_the_last_four(&wide(&app)), "{}", wide(&app));
    // Later, a calm Enter takes it to the confirmation.
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 400);
    assert!(effects.is_empty());
    assert!(asks_for_the_last_four(&wide(&app)), "{}", wide(&app));
}

#[test]
fn ctrl_c_clears_the_box_and_closes_the_slot() {
    let (mut app, mut clock) = asking("/push");
    assert!(screen(&app).contains("git URL"), "TRIGGER: no slot is open");
    type_text(&mut app, &mut clock, URL);
    let effects = ctrl(&mut app, &mut clock, 'c');
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert!(!screen(&app).contains("git URL"), "{}", screen(&app));
}

#[test]
fn the_cursor_is_in_the_slot_while_it_is_open() {
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};
    let (mut app, mut clock) = asking("/push");
    assert!(screen(&app).contains("git URL"), "TRIGGER: no slot is open");
    type_text(&mut app, &mut clock, "abc");
    assert_eq!(app.chat_text().as_deref(), Some("/push"), "TRIGGER");
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let pos = t.backend_mut().get_cursor_position().unwrap();
    let text = screen(&app);
    let row = text
        .lines()
        .position(|l| l.contains("abc"))
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(usize::from(pos.y), row, "{text}");
    let line = text.lines().nth(row).unwrap();
    let col = line[..line.find("abc").unwrap()].chars().count() + 3;
    assert_eq!(usize::from(pos.x), col, "{text}");
}

#[test]
fn the_slot_fits_at_80_by_24_with_the_box_and_the_footer() {
    let (app, _clock) = asking("/push");
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let text = text_of(t.backend().buffer());
    assert!(text.contains("git URL"), "{text}");
    assert!(text.contains("Message to Cline"), "{text}");
    assert!(text.contains("Enter send"), "{text}");
    for line in text.lines() {
        assert!(line.chars().count() <= 80, "{line:?}");
    }
}

#[test]
fn the_push_my_code_chip_opens_the_same_slot() {
    // `push my code` is a fixed phrase: its chip prepares `push` for the focused run, which lacks a URL like a typed `/push` does.
    for phrase in ["push my code", "push this run"] {
        let (mut app, mut clock) = ready();
        send(&mut app, &mut clock, phrase);
        assert!(
            screen(&app).contains("Command?"),
            "TRIGGER: no chip for {phrase:?}\n{}",
            screen(&app)
        );
        let effects = press(&mut app, &mut clock, KeyCode::Enter, 400);
        assert!(effects.is_empty(), "{effects:?}");
        let text = screen(&app);
        assert!(
            !text.contains("usage"),
            "{phrase}: the chip was refused:\n{text}"
        );
        assert!(text.contains("git URL"), "{phrase}:\n{text}");
        assert!(text.contains(&format!("Push run {RUN}")), "{text}");
        // The draft stays, and the URL goes into the slot like after `/push`.
        assert_eq!(app.chat_text().as_deref(), Some(phrase));
        clock.after(400);
        type_text(&mut app, &mut clock, URL);
        let effects = enter(&mut app, &mut clock);
        assert!(effects.is_empty());
        assert!(asks_for_the_last_four(&wide(&app)), "{}", wide(&app));
    }
}
