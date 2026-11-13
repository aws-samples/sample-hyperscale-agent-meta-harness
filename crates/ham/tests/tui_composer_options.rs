//! The Options box of the composer: the start options that the command line has and the composer lacked,
//! `--rules <dir>`, `--no-rules`, `--continue <file>` and `--input-wait <minutes>`, typed in a box beside the Base ref. The box is read
//! as the words of the command line and goes through the same parser, so a run started from the composer is the run the equivalent
//! command line starts. The Prompt, Repository, Base ref and Options boxes are editing boxes with a caret.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Screen};
use ham_core::command::{Command, DirectArgs, parse};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        let code = if ch == '\n' {
            KeyCode::Enter
        } else {
            KeyCode::Char(ch)
        };
        assert!(press(app, code).is_empty(), "typing {ch:?} caused effects");
    }
}

fn ctrl_s(app: &mut App) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
}

fn draw(app: &App, w: u16, h: u16) -> (String, (u16, u16)) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let pos = t.backend_mut().get_cursor_position().unwrap();
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
    (out, (pos.x, pos.y))
}

/// The composer with the prompt `build x`, target `--s3-only`, and the focus on the Options box.
fn at_options() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    type_text(&mut app, "build x");
    press(&mut app, KeyCode::Tab); // Target
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right); // --s3-only
    press(&mut app, KeyCode::Tab); // Repository
    press(&mut app, KeyCode::Tab); // Base ref
    press(&mut app, KeyCode::Tab); // Options
    app
}

/// What the command line makes of the same words.
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

fn submitted(app: &mut App) -> DirectArgs {
    let effects = ctrl_s(app);
    let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
        panic!(
            "expected one submit, got {effects:?}; the screen is:\n{}",
            draw(app, 100, 40).0
        );
    };
    args.clone()
}

#[test]
fn the_options_box_is_beside_the_base_ref_and_has_a_caret() {
    let mut app = at_options();
    type_text(&mut app, "--no-rules");
    let (text, (x, y)) = draw(&app, 100, 40);
    let row = text.lines().position(|l| l.contains("Options")).unwrap();
    let line = text.lines().nth(row).unwrap();
    assert!(
        line.contains("Base ref"),
        "Options is not on the Base ref row:\n{text}"
    );
    assert!(text.contains("--no-rules"), "{text}");
    // The terminal's cursor is after the typed words, inside the Options box.
    let col = line.split("Options").next().unwrap().chars().count();
    assert!(
        usize::from(x) > col,
        "{x} is not in the Options box (starts at {col}):\n{text}"
    );
    assert_eq!(usize::from(y), row + 1, "{text}");
}

#[test]
fn an_empty_options_box_shows_what_it_takes() {
    let app = at_options();
    let text = draw(&app, 100, 40).0;
    for want in ["--no-rules", "--rules", "--continue", "--input-wait"] {
        assert!(text.contains(want), "{want} missing:\n{text}");
    }
}

#[test]
fn every_combination_of_options_makes_the_same_request_as_the_command_line() {
    for options in [
        vec![],
        vec!["--no-rules"],
        vec!["--rules", "./my-rules"],
        vec!["--continue", "notes.md"],
        vec!["--input-wait", "10"],
        vec!["--rules", "r", "--continue", "c.md", "--input-wait", "0"],
        vec!["--no-rules", "--input-wait", "480"],
    ] {
        let mut app = at_options();
        type_text(&mut app, &options.join(" "));
        assert_eq!(submitted(&mut app), from_argv(&options), "{options:?}");
    }
}

#[test]
fn quotes_keep_a_word_with_a_space_whole() {
    let mut app = at_options();
    type_text(&mut app, "--rules \"my rules\" --continue 'a b.md'");
    assert_eq!(
        submitted(&mut app),
        from_argv(&["--rules", "my rules", "--continue", "a b.md"])
    );
}

#[test]
fn a_bad_option_line_shows_why_and_starts_nothing() {
    for (typed, want) in [
        ("--repo cc:other", "Options takes"),
        ("--model sol", "Options takes"),
        ("--rules", "--rules"),
        ("--rules a --no-rules", "--rules"),
        ("--input-wait 43201", "43201"),
        ("--rules \"unclosed", "quote"),
        ("stray words", "Options takes"),
    ] {
        let mut app = at_options();
        type_text(&mut app, typed);
        let effects = ctrl_s(&mut app);
        assert!(effects.is_empty(), "{typed:?} started a run: {effects:?}");
        let text = draw(&app, 100, 40).0;
        assert!(text.contains("error:"), "{typed:?} gave no error:\n{text}");
        assert!(text.contains(want), "{typed:?}: {want:?} missing:\n{text}");
    }
}

#[test]
fn a_forty_line_prompt_can_be_edited_anywhere_and_scrolls() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    press(&mut app, KeyCode::Char('n'));
    let prompt: String = (1..=40)
        .map(|i| format!("L{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    type_text(&mut app, &prompt);
    for _ in 0..39 {
        press(&mut app, KeyCode::Up);
    }
    press(&mut app, KeyCode::Home);
    type_text(&mut app, ">>");
    let (text, (_, y)) = draw(&app, 80, 24);
    assert!(text.contains(">>L01"), "{text}");
    assert!(!text.contains("L40"), "{text}");
    assert_eq!(
        usize::from(y),
        text.lines().position(|l| l.contains(">>L01")).unwrap()
    );
    press(&mut app, KeyCode::Tab); // Target
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    let args = submitted(&mut app);
    assert!(args.prompt.starts_with(">>L01\nL02"), "{:?}", args.prompt);
    assert!(args.prompt.ends_with("L39\nL40"));
}

#[test]
fn the_options_stay_with_the_draft_and_a_started_run_empties_them() {
    let mut app = at_options();
    type_text(&mut app, "--no-rules");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    press(&mut app, KeyCode::Char('n'));
    // The empty box shows a hint that names the options, so the draft is read from what it sends.
    assert!(submitted(&mut app).no_rules, "the options were lost");
    app.update(ham::tui::Msg::Started(Ok(
        harness_protocol::api::v1::StartRunResponse {
            run_id: "01K6A0000000000000000000R1".into(),
            status: harness_protocol::goal::GoalStatus::Active,
        },
    )));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('n'));
    // The composer is empty again: a second run typed from scratch carries none of the options.
    type_text(&mut app, "build x");
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(submitted(&mut app), from_argv(&[]));
}

#[test]
fn ctrl_c_clears_the_options_box_first_and_a_quit_counts_it_as_unsent() {
    let mut app = at_options();
    type_text(&mut app, "--no-rules");
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.handle_key(ctrl_c).is_empty());
    let (_, _) = draw(&app, 100, 40);
    // The prompt `build x` is still unsent: the first quit request only warns.
    assert!(app.handle_key(ctrl_c).is_empty());
    assert_eq!(app.handle_key(ctrl_c), vec![Effect::Quit]);
}
