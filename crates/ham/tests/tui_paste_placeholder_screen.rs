//! The large-paste placeholder in the message box. A paste over 150 characters, or of
//! three lines or more, shows as `[Pasted n lines]` (a second one of the same size is `#2`) and the text is kept aside; Tab expands the
//! placeholder under the caret; Backspace at its end and Delete at its start take the whole token; sending, the history and the editor
//! see the text that was pasted. The token is drawn in its own colour. A pasted draft is tainted as before.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::KeyCode;
use ham::tui::Effect;
use ratatui::style::Color;

fn long() -> String {
    "word ".repeat(40)
}

const THREE: &str = "alpha\nbeta\ngamma";

#[test]
fn a_paste_over_150_characters_is_a_placeholder_and_taints_the_draft() {
    let (mut app, _) = ready();
    assert!(long().chars().count() > 150);
    app.handle_paste(long());
    assert_eq!(app.chat_text().as_deref(), Some("[Pasted 1 line]"));
    assert!(app.draft_is_tainted());
}

#[test]
fn a_paste_of_three_lines_is_a_placeholder_with_the_count_and_a_short_one_stays_inline() {
    let (mut app, _) = ready();
    app.handle_paste(THREE.to_string());
    assert_eq!(app.chat_text().as_deref(), Some("[Pasted 3 lines]"));
    let (mut app, _) = ready();
    app.handle_paste("hello there".to_string());
    assert_eq!(app.chat_text().as_deref(), Some("hello there"));
    let (mut app, _) = ready();
    app.handle_paste("one\ntwo".to_string());
    assert_eq!(app.chat_text().as_deref(), Some("one\ntwo"));
}

#[test]
fn a_second_paste_of_the_same_size_is_numbered() {
    let (mut app, mut clock) = ready();
    app.handle_paste(THREE.to_string());
    type_text(&mut app, &mut clock, " and ");
    app.handle_paste("1\n2\n3".to_string());
    assert_eq!(
        app.chat_text().as_deref(),
        Some("[Pasted 3 lines] and [Pasted 3 lines #2]")
    );
}

#[test]
fn the_placeholder_is_drawn_in_its_own_colour() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "see ");
    app.handle_paste(long());
    let buf = buffer(&app, 120, 40);
    let text = text_of(&buf);
    let (y, line) = text
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("[Pasted 1 line]"))
        .unwrap();
    // Columns, not bytes: the box has a border of wide characters.
    let col = |needle: &str| line[..line.find(needle).unwrap()].chars().count();
    let x = col("[Pasted");
    let plain = col("see");
    assert_eq!(buf[(x as u16, y as u16)].fg, Color::Cyan, "{text}");
    assert_eq!(buf[(x as u16 + 14, y as u16)].fg, Color::Cyan);
    assert_ne!(buf[(plain as u16, y as u16)].fg, Color::Cyan);
}

#[test]
fn tab_expands_the_placeholder_under_the_caret_and_the_caret_follows_the_text() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "see ");
    app.handle_paste(THREE.to_string());
    press(&mut app, &mut clock, KeyCode::Tab, 60);
    assert_eq!(app.chat_text().as_deref(), Some("see alpha\nbeta\ngamma"));
    type_text(&mut app, &mut clock, "!");
    assert_eq!(app.chat_text().as_deref(), Some("see alpha\nbeta\ngamma!"));
}

#[test]
fn tab_away_from_a_placeholder_does_nothing() {
    let (mut app, mut clock) = ready();
    app.handle_paste(THREE.to_string());
    type_text(&mut app, &mut clock, " tail words here");
    let effects = press(&mut app, &mut clock, KeyCode::Tab, 60);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(
        app.chat_text().as_deref(),
        Some("[Pasted 3 lines] tail words here")
    );
}

#[test]
fn backspace_at_the_end_of_a_token_takes_the_whole_token() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "hi ");
    app.handle_paste(long());
    press(&mut app, &mut clock, KeyCode::Backspace, 60);
    assert_eq!(app.chat_text().as_deref(), Some("hi "));
}

#[test]
fn delete_at_the_start_of_a_token_takes_the_whole_token() {
    let (mut app, mut clock) = ready();
    app.handle_paste(long());
    type_text(&mut app, &mut clock, " tail");
    press(&mut app, &mut clock, KeyCode::Home, 60);
    press(&mut app, &mut clock, KeyCode::Delete, 60);
    assert_eq!(app.chat_text().as_deref(), Some(" tail"));
}

#[test]
fn a_message_is_sent_with_the_text_that_was_pasted() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    type_text(&mut app, &mut clock, "fix this: ");
    app.handle_paste(THREE.to_string());
    clock.after(400);
    let effects = enter(&mut app, &mut clock);
    let sent: Vec<&str> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Answer { answer, .. } => answer.text.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(sent, vec!["fix this: alpha\nbeta\ngamma"], "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some(""));
}

#[test]
fn the_history_holds_the_text_that_was_pasted_not_the_token() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    app.handle_paste(THREE.to_string());
    clock.after(400);
    enter(&mut app, &mut clock);
    press(&mut app, &mut clock, KeyCode::Up, 60);
    assert_eq!(app.chat_text().as_deref(), Some("alpha\nbeta\ngamma"));
}

#[test]
fn a_recalled_text_replaces_the_pastes_and_the_draft_comes_back_whole() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/help");
    app.handle_paste(THREE.to_string());
    press(&mut app, &mut clock, KeyCode::Up, 60);
    assert_eq!(app.chat_text().as_deref(), Some("/help"));
    press(&mut app, &mut clock, KeyCode::Down, 60);
    // The draft comes back with the text of the paste in it (the token is not needed any more).
    assert_eq!(app.chat_text().as_deref(), Some("alpha\nbeta\ngamma"));
}
