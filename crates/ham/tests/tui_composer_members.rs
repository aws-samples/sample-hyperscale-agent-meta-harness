//! The Options box and the start options of package REQ1: `--goal on|off` (the composer's goal switch), `--research`, `--no-research`,
//! `--allow-third-party` and `--attach <file>`. The box is read as the words of the command line and goes through the same parser, so a run
//! started from the composer is the run the equivalent command line starts.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Screen};
use ham_core::command::{Command, DirectArgs, parse};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

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

fn draw(app: &App, w: u16, h: u16) -> String {
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
            draw(app, 100, 40)
        );
    };
    args.clone()
}

#[test]
fn the_goal_switch_and_the_research_and_attachment_options_make_the_command_lines_request() {
    for options in [
        vec!["--goal", "off"],
        vec!["--goal", "on"],
        vec!["--research"],
        vec!["--no-research"],
        vec!["--allow-third-party"],
        vec!["--attach", "notes.md"],
        vec!["--attach", "a.md", "--attach", "b.csv"],
        vec![
            "--goal",
            "off",
            "--no-research",
            "--allow-third-party",
            "--attach",
            "a.md",
            "--no-rules",
        ],
    ] {
        let mut app = at_options();
        type_text(&mut app, &options.join(" "));
        let args = submitted(&mut app);
        assert_eq!(args, from_argv(&options), "{options:?}");
    }
    // TRIGGER PRESENT: the box did change the submitted request.
    let mut app = at_options();
    type_text(&mut app, "--goal off --no-research");
    let args = submitted(&mut app);
    assert_eq!((args.goal, args.research), (Some(false), Some(false)));
    println!(
        "COMPOSER-MEMBERS TRIGGER PRESENT: Options '--goal off --no-research' -> goal=Some(false) research=Some(false)"
    );
}

#[test]
fn an_attached_name_with_a_space_in_quotes_stays_whole() {
    let mut app = at_options();
    type_text(&mut app, "--attach \"my notes.md\"");
    assert_eq!(submitted(&mut app).attach, ["my notes.md"]);
}

#[test]
fn an_empty_options_box_names_the_new_options_and_a_stray_word_lists_them_all() {
    // The hint is one line in a box a third of the screen wide: it names the two options a person reaches for first.
    let app = at_options();
    let text = draw(&app, 120, 40);
    for want in ["--goal", "--attach"] {
        assert!(text.contains(want), "{want} missing:\n{text}");
    }
    // The error of a word the box does not take lists everything it takes.
    let mut app = at_options();
    type_text(&mut app, "stray");
    assert!(ctrl_s(&mut app).is_empty());
    let text = draw(&app, 200, 40);
    for want in [
        "--goal on|off",
        "--research",
        "--no-research",
        "--allow-third-party",
        "--attach <file>",
    ] {
        assert!(text.contains(want), "{want} missing:\n{text}");
    }
}

#[test]
fn a_bad_line_shows_why_and_starts_nothing() {
    for (typed, want) in [
        ("--goal", "--goal"),
        ("--goal maybe", "on or off"),
        ("--goal off --goal on", "--goal"),
        ("--research --no-research", "opposite"),
        ("--attach", "--attach"),
        ("--attach --no-rules", "--attach"),
    ] {
        let mut app = at_options();
        type_text(&mut app, typed);
        let effects = ctrl_s(&mut app);
        assert!(effects.is_empty(), "{typed:?} started a run: {effects:?}");
        let text = draw(&app, 120, 40);
        assert!(text.contains("error:"), "{typed:?} gave no error:\n{text}");
        assert!(text.contains(want), "{typed:?}: {want:?} missing:\n{text}");
    }
}

#[test]
fn the_options_stay_with_the_draft() {
    let mut app = at_options();
    type_text(&mut app, "--goal off");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(submitted(&mut app).goal, Some(false), "the switch was lost");
}

// ---- `--compute <word>`: the machine of one run, from the Options box -----------------------------------------------------

#[test]
fn the_compute_option_makes_the_command_lines_request() {
    for word in ["auto", "microvm", "instances"] {
        let mut app = at_options();
        type_text(&mut app, &format!("--compute {word}"));
        let args = submitted(&mut app);
        assert_eq!(args, from_argv(&["--compute", word]), "{word}");
        assert_eq!(args.compute.map(|c| c.word()), Some(word));
    }
    // With other options in the box, in any order.
    let mut app = at_options();
    type_text(&mut app, "--goal off --compute instances --attach a.md");
    assert_eq!(
        submitted(&mut app),
        from_argv(&[
            "--goal",
            "off",
            "--compute",
            "instances",
            "--attach",
            "a.md"
        ])
    );
    // TRIGGER PRESENT: the box did change the submitted request.
    let mut app = at_options();
    type_text(&mut app, "--compute instances");
    let args = submitted(&mut app);
    assert_eq!(args.compute.map(|c| c.word()), Some("instances"));
    println!(
        "COMPOSER-COMPUTE TRIGGER PRESENT: Options '--compute instances' -> compute=Some(instances)"
    );
}

#[test]
fn a_bad_compute_line_names_the_three_words_and_starts_nothing() {
    for (typed, want) in [
        ("--compute", "auto, microvm or instances"),
        ("--compute gpu", "auto, microvm or instances"),
        ("--compute Instances", "auto, microvm or instances"),
        ("--compute --goal off", "auto, microvm or instances"),
        ("--compute microvm --compute instances", "twice"),
    ] {
        let mut app = at_options();
        type_text(&mut app, typed);
        let effects = ctrl_s(&mut app);
        assert!(effects.is_empty(), "{typed:?} started a run: {effects:?}");
        let text = draw(&app, 200, 40);
        assert!(text.contains("error:"), "{typed:?} gave no error:\n{text}");
        assert!(text.contains(want), "{typed:?}: {want:?} missing:\n{text}");
    }
}

#[test]
fn a_stray_word_lists_compute_among_the_words_the_box_takes() {
    let mut app = at_options();
    type_text(&mut app, "stray");
    assert!(ctrl_s(&mut app).is_empty());
    let text = draw(&app, 240, 40);
    assert!(text.contains("--compute auto|microvm|instances"), "{text}");
}

/// The words the empty Options box may show, in the order it shows them.
const HINT_WORDS: [&str; 7] = [
    "--no-rules",
    "--rules",
    "--continue",
    "--input-wait",
    "--goal",
    "--attach",
    "--compute",
];

#[test]
fn the_empty_box_names_compute_where_it_fits_and_never_shows_half_a_word() {
    let app = at_options();
    assert!(
        draw(&app, 240, 40).contains("--compute"),
        "a wide terminal shows --compute in the hint"
    );
    for width in [80u16, 90, 100, 110, 120, 130, 140, 160, 200, 240] {
        let text = draw(&app, width, 40);
        let line = text
            .lines()
            .find(|l| l.contains("--no-rules"))
            .unwrap_or_else(|| panic!("no hint line at {width}:\n{text}"));
        let hint = &line[line.find("--no-rules").unwrap()..];
        let hint = hint.trim_end_matches(|c: char| c == '│' || c.is_whitespace());
        for part in hint.split(" · ") {
            assert!(
                HINT_WORDS.contains(&part),
                "the hint at {width} columns ends in {part:?}, which is no whole word:\n{line}"
            );
        }
        // The first words of the line are always the old ones: only room decides how many follow.
        assert!(hint.starts_with("--no-rules · --rules"), "{hint:?}");
    }
    // The two words a person reaches for first stay in view at the width the older test uses.
    let text = draw(&app, 120, 40);
    for want in ["--goal", "--attach"] {
        assert!(
            text.contains(want),
            "{want} missing at 120 columns:\n{text}"
        );
    }
}

// ---- `--max-run-hours <hours>`: the total time of one run, from the Options box ------------------------------------------------

#[test]
fn b688_the_max_run_hours_option_makes_the_command_lines_request() {
    let mut app = at_options();
    type_text(&mut app, "--max-run-hours 6");
    let args = submitted(&mut app);
    assert_eq!(args, from_argv(&["--max-run-hours", "6"]));
    assert_eq!(args.max_run_hours, Some(6));
}

#[test]
fn b688_a_max_run_hours_option_with_no_value_names_the_range_and_starts_nothing() {
    for typed in ["--max-run-hours", "--max-run-hours --goal off"] {
        let mut app = at_options();
        type_text(&mut app, typed);
        let effects = ctrl_s(&mut app);
        assert!(effects.is_empty(), "{typed:?} started a run: {effects:?}");
        let text = draw(&app, 200, 40);
        assert!(
            text.contains("whole hours from 1 to 720"),
            "{typed:?}:\n{text}"
        );
    }
}
