//! Ctrl-G in the message box: the draft in the user's editor. Ctrl-G asks the loop to
//! run `$VISUAL` or `$EDITOR` on the draft (with the text of every pasted block put back); the answer replaces the box and taints it,
//! an unchanged draft or a cancelled edit leaves the box alone (an editor that came back at once says why it may have), and an error
//! goes to the notice row. `run_editor_with` runs the edit with the terminal handed over by the closure it is given.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::KeyCode;
use ham::tui::keys::Mode;
use ham::tui::{Effect, Msg, run_editor_with};
use ham_core::editor::{Editor, Outcome, Report};
use std::time::Duration;

fn edit_requests(effects: &[Effect]) -> Vec<&str> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::EditDraft { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn report(outcome: Outcome, secs: u64) -> Msg {
    Msg::Edited(Ok(Report {
        outcome,
        elapsed: Duration::from_secs(secs),
    }))
}

#[test]
fn ctrl_g_asks_for_the_editor_with_the_draft_and_the_pasted_text_put_back() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "fix this: ");
    app.handle_paste("alpha\nbeta\ngamma".to_string());
    assert_eq!(
        app.chat_text().as_deref(),
        Some("fix this: [Pasted 3 lines]")
    );
    let effects = ctrl(&mut app, &mut clock, 'g');
    assert_eq!(
        edit_requests(&effects),
        vec!["fix this: alpha\nbeta\ngamma"],
        "{effects:?}"
    );
}

#[test]
fn ctrl_g_in_browse_mode_does_nothing() {
    let (mut app, mut clock) = ready();
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    assert_eq!(app.mode(), Mode::Browse);
    let effects = ctrl(&mut app, &mut clock, 'g');
    assert!(edit_requests(&effects).is_empty(), "{effects:?}");
}

#[test]
fn an_edited_draft_replaces_the_box_and_taints_it() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "hello");
    assert!(!app.draft_is_tainted());
    app.update(report(
        Outcome::Edited {
            text: "hello\nworld".into(),
            tainted: true,
        },
        9,
    ));
    assert_eq!(app.chat_text().as_deref(), Some("hello\nworld"));
    assert!(app.draft_is_tainted());
    assert_eq!(app.mode(), Mode::Typing);
}

#[test]
fn an_edited_command_shows_its_preview_before_it_runs() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "x");
    app.update(report(
        Outcome::Edited {
            text: "/doctor".into(),
            tainted: true,
        },
        9,
    ));
    clock.after(400);
    let effects = enter(&mut app, &mut clock);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RunCommand { .. })),
        "{effects:?}"
    );
    assert!(
        screen(&app).contains("Press Enter to run it"),
        "{}",
        screen(&app)
    );
}

#[test]
fn the_edited_text_replaces_the_pastes_of_the_draft() {
    let (mut app, mut clock) = ready();
    app.handle_paste("alpha\nbeta\ngamma".to_string());
    app.update(report(
        Outcome::Edited {
            text: "short".into(),
            tainted: true,
        },
        9,
    ));
    assert_eq!(app.chat_text().as_deref(), Some("short"));
    // Tab finds no placeholder any more.
    press(&mut app, &mut clock, KeyCode::Tab, 60);
    assert_eq!(app.chat_text().as_deref(), Some("short"));
}

#[test]
fn a_label_the_editor_brings_back_is_refused_at_the_send() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    app.handle_paste("alpha\nbeta\ngamma".to_string());
    // The edit puts the old label back as plain text, but its paste is gone.
    app.update(report(
        Outcome::Edited {
            text: "keep [Pasted 3 lines] here".into(),
            tainted: true,
        },
        9,
    ));
    clock.after(400);
    let effects = enter(&mut app, &mut clock);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        screen(&app).contains("A pasted block in this message is gone"),
        "{}",
        screen(&app)
    );
    assert_eq!(
        app.chat_text().as_deref(),
        Some("keep [Pasted 3 lines] here")
    );
}

#[test]
fn an_unchanged_draft_leaves_the_box_and_a_quick_return_gets_the_hint() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "hello");
    app.update(report(Outcome::Unchanged, 5));
    assert_eq!(app.chat_text().as_deref(), Some("hello"));
    assert!(!screen(&app).contains("returned at once"));
    app.update(Msg::Edited(Ok(Report {
        outcome: Outcome::Unchanged,
        elapsed: Duration::from_millis(200),
    })));
    assert_eq!(app.chat_text().as_deref(), Some("hello"));
    assert!(
        screen(&app).contains("The editor returned at once"),
        "{}",
        screen(&app)
    );
}

#[test]
fn a_cancelled_edit_leaves_the_box_alone() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "hello");
    app.update(report(Outcome::Cancelled, 5));
    assert_eq!(app.chat_text().as_deref(), Some("hello"));
}

#[test]
fn an_error_goes_to_the_notice_row_and_the_box_is_left_alone() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "hello");
    app.update(Msg::Edited(Err("no editor: set $VISUAL or $EDITOR".into())));
    assert_eq!(app.chat_text().as_deref(), Some("hello"));
    assert!(
        screen(&app).contains("no editor: set $VISUAL or $EDITOR"),
        "{}",
        screen(&app)
    );
}

// ---- running the edit -----------------------------------------------------------------------------------------------------------

fn scratch(label: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ham-editor-screen-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn an_editor_that_appends_a_line_returns_the_draft_with_that_line() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("append");
    let script = dir.join("append-editor.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'appended by the editor\\n' >> \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let editor = Editor::from_command(script.to_str().unwrap());
    let msg = run_editor_with(editor, "my draft", &dir, |cmd| cmd.status());
    let Msg::Edited(Ok(report)) = msg else {
        panic!("{msg:?}");
    };
    assert_eq!(
        report.outcome,
        Outcome::Edited {
            text: "my draft\nappended by the editor".into(),
            tainted: true,
        }
    );
}

#[test]
fn the_terminal_is_handed_over_by_the_closure_and_taken_back_after() {
    let dir = scratch("handover");
    let editor = Editor::from_command("true");
    let calls = std::cell::Cell::new(0);
    let msg = run_editor_with(editor, "same", &dir, |cmd| {
        calls.set(calls.get() + 1);
        assert_eq!(cmd.get_program(), "true");
        cmd.status()
    });
    assert_eq!(calls.get(), 1);
    assert!(matches!(
        msg,
        Msg::Edited(Ok(Report {
            outcome: Outcome::Unchanged,
            ..
        }))
    ));
}

#[test]
fn no_editor_is_an_error_message_not_a_crash() {
    let dir = scratch("none");
    let msg = run_editor_with(Editor::from_env_values(None, None), "x", &dir, |cmd| {
        cmd.status()
    });
    let Msg::Edited(Err(why)) = msg else {
        panic!("{msg:?}");
    };
    assert_eq!(why, "no editor: set $VISUAL or $EDITOR");
}
