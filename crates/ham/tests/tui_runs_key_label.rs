//! The Runs screen says what its `p` key does and where the selected run stands in the list. The key line carries `p this repo`
//! (or `p only cc:calc-service`, when the line has room), `p s3-only` for a run that has no repository and, once the limit is on,
//! `p all runs`; it keeps `q quit` on an 80-column screen. The bottom border of the table reads `run 12 of 769` at its right end (the note line
//! under the table is full at 80 columns: the count, the tags and the columns the width hides). Every check runs at 80x24, 120x40 and 200x60.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::mouse::Target;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

const ARN: &str = "arn:aws:sts::1:assumed-role/A/b";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";
const R4: &str = "01M5A00000000000000000R004";
const R5: &str = "01M5A00000000000000000R005";
const R6: &str = "01M5A00000000000000000R006";
const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 60)];

fn run(id: &str, status: &str, target: Value) -> RunSummary {
    serde_json::from_value(
        json!({"runId": id, "status": status, "phase": "planning", "target": target,
        "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN}),
    )
    .unwrap()
}

fn repo(name: &str) -> Value {
    json!({"mode": "repo", "repo": format!("cc:{name}")})
}

/// Six runs, newest first: R1 and R6 in calc-service, R2 and R3 in web-shop, R4 in api-gateway, R5 with no repository.
fn six() -> Vec<RunSummary> {
    vec![
        run(R1, "complete", repo("calc-service")),
        run(R2, "active", repo("web-shop")),
        run(R3, "active", repo("web-shop")),
        run(R4, "complete", repo("api-gateway")),
        run(R5, "complete", json!({"mode": "s3-only"})),
        run(R6, "complete", repo("calc-service")),
    ]
}

fn runs_app(runs: Vec<RunSummary>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(runs)));
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn screen_at(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        let mut x = 0;
        while x < w {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The bottom border of the table: the last row that starts with the corner `└`.
fn table_bottom(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| l.starts_with('\u{2514}'))
        .unwrap_or_else(|| panic!("no table border in\n{text}"))
        .to_string()
}

/// The note line and the key line: the last two rows with text.
fn bottom_two(text: &str) -> (String, String) {
    let rows: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let n = rows.len();
    (rows[n - 2].to_string(), rows[n - 1].to_string())
}

fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(app: &mut App, c: char) -> Vec<Effect> {
    key(app, KeyCode::Char(c))
}

fn type_in(app: &mut App, text: &str) {
    for c in text.chars() {
        ch(app, c);
    }
}

fn down(app: &mut App, n: usize) {
    for _ in 0..n {
        key(app, KeyCode::Down);
    }
}

fn cols(s: &str) -> usize {
    ratatui::text::Span::raw(s).width()
}

#[test]
fn the_key_line_names_the_repository_p_would_pick_and_keeps_q_quit_at_every_size() {
    for (w, h) in SIZES {
        let app = runs_app(six());
        let (_, help) = bottom_two(&screen_at(&app, w, h));
        assert!(
            cols(&help) <= usize::from(w),
            "{w}x{h}: {help:?} is wider than the screen"
        );
        assert!(
            help.ends_with("r refresh \u{b7} q quit"),
            "{w}x{h}: {help:?}"
        );
        if w == 80 {
            // The shortest of the wordings that fits: `p only cc:calc-service` needs 11 more columns than the line has.
            assert_eq!(
                help,
                "n new run \u{b7} Enter open \u{b7} \u{2191}\u{2193} \u{b7} / find \u{b7} t tags \u{b7} p this repo \u{b7} r refresh \u{b7} q quit",
                "{w}x{h}"
            );
            assert_eq!(cols(&help), 80);
        } else {
            assert!(
                help.contains(
                    "t tags \u{b7} p only cc:calc-service \u{b7} s settings \u{b7} r refresh"
                ),
                "{w}x{h}: {help:?}"
            );
        }
        assert!(!help.contains("p project"), "{w}x{h}: {help:?}");
    }
}

#[test]
fn a_run_with_no_repository_names_s3_only() {
    for (w, h) in SIZES {
        let mut app = runs_app(six());
        down(&mut app, 4);
        let (_, help) = bottom_two(&screen_at(&app, w, h));
        if w == 80 {
            assert_eq!(
                help,
                "n new run \u{b7} Enter open \u{b7} \u{2191}\u{2193} \u{b7} / find \u{b7} t tags \u{b7} p s3-only \u{b7} r refresh \u{b7} q quit",
                "{w}x{h}"
            );
        } else {
            assert!(
                help.contains("t tags \u{b7} p only s3-only \u{b7} s settings"),
                "{w}x{h}: {help:?}"
            );
        }
        assert!(!help.contains("this repo"), "{w}x{h}: {help:?}");
    }
}

#[test]
fn the_label_follows_the_selected_run() {
    let mut app = runs_app(six());
    let (_, a) = bottom_two(&screen_at(&app, 200, 60));
    assert!(a.contains("p only cc:calc-service"), "{a:?}");
    down(&mut app, 1);
    let (_, b) = bottom_two(&screen_at(&app, 200, 60));
    assert!(b.contains("p only cc:web-shop"), "{b:?}");
}

#[test]
fn once_the_limit_is_on_the_key_says_all_runs_and_the_line_keeps_q_quit() {
    for (w, h) in SIZES {
        let mut app = runs_app(six());
        down(&mut app, 1);
        assert!(ch(&mut app, 'p').is_empty());
        let (note, help) = bottom_two(&screen_at(&app, w, h));
        assert!(
            note.starts_with("2 of 6 runs in cc:web-shop"),
            "{w}x{h}: {note:?}"
        );
        assert!(
            cols(&help) <= usize::from(w),
            "{w}x{h}: {help:?} is wider than the screen"
        );
        assert!(
            help.ends_with("r refresh \u{b7} q quit"),
            "{w}x{h}: {help:?}"
        );
        if w == 80 {
            assert_eq!(
                help,
                "Enter open \u{b7} \u{2191}\u{2193} \u{b7} / edit \u{b7} Esc clears \u{b7} t tags \u{b7} p all runs \u{b7} r refresh \u{b7} q quit",
                "{w}x{h}"
            );
        } else {
            assert!(
                help.contains("t tags \u{b7} p show all runs \u{b7}"),
                "{w}x{h}: {help:?}"
            );
        }
        // Pressing it again lifts the limit and the label offers the repository again.
        ch(&mut app, 'p');
        let (_, help) = bottom_two(&screen_at(&app, w, h));
        assert!(
            help.contains("p this repo") || help.contains("p only cc:web-shop"),
            "{w}x{h}: {help:?}"
        );
    }
}

#[test]
fn words_that_narrow_the_list_leave_room_for_the_short_wording_at_80_columns() {
    // The narrowed line is one column longer than the plain one, so `p this repo` does not fit it at 80 columns: `p repo` does.
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    key(&mut app, KeyCode::Enter);
    let (_, help) = bottom_two(&screen_at(&app, 80, 24));
    assert_eq!(
        help,
        "Enter open \u{b7} \u{2191}\u{2193} \u{b7} / edit \u{b7} Esc clears \u{b7} t tags \u{b7} p repo \u{b7} r refresh \u{b7} q quit"
    );
    let (_, help) = bottom_two(&screen_at(&app, 200, 60));
    assert!(help.contains("p only cc:calc-service"), "{help:?}");
}

#[test]
fn the_p_chip_is_the_label_and_a_click_on_it_presses_p() {
    for (w, h) in SIZES {
        let app = runs_app(six());
        let text = screen_at(&app, w, h);
        let (_, help) = bottom_two(&text);
        let map = app.hit_map();
        let chip = map
            .regions()
            .iter()
            .find(|r| r.target == Target::Key(KeyCode::Char('p')))
            .unwrap_or_else(|| panic!("{w}x{h}: no chip for p"));
        let label = if w == 80 {
            "p this repo"
        } else {
            "p only cc:calc-service"
        };
        assert_eq!(usize::from(chip.rect.width), cols(label), "{w}x{h}");
        let start: String = help.chars().skip(usize::from(chip.rect.x)).collect();
        assert!(start.starts_with(label), "{w}x{h}: {start:?}");
    }
}

#[test]
fn the_table_border_says_which_run_of_how_many_is_selected() {
    for (w, h) in SIZES {
        let mut app = runs_app(six());
        let text = screen_at(&app, w, h);
        let border = table_bottom(&text);
        assert!(
            border.ends_with("run 1 of 6 \u{2500}\u{2518}")
                || border.ends_with("run 1 of 6 \u{2518}"),
            "{w}x{h}: {border:?}"
        );
        // The position sits at the right end of the border, which stays as wide as the screen.
        assert_eq!(cols(&border), usize::from(w), "{w}x{h}: {border:?}");
        down(&mut app, 2);
        let border = table_bottom(&screen_at(&app, w, h));
        assert!(border.contains(" run 3 of 6 "), "{w}x{h}: {border:?}");
        // The note line keeps what it said: the count, with no position in it.
        let (note, _) = bottom_two(&screen_at(&app, w, h));
        assert!(note.starts_with("6 runs"), "{w}x{h}: {note:?}");
        assert!(!note.contains("run 3 of"), "{w}x{h}: {note:?}");
    }
}

#[test]
fn with_words_the_position_is_among_the_runs_shown() {
    for (w, h) in SIZES {
        let mut app = runs_app(six());
        ch(&mut app, '/');
        type_in(&mut app, "calc");
        key(&mut app, KeyCode::Enter);
        let border = table_bottom(&screen_at(&app, w, h));
        assert!(border.contains(" run 1 of 2 "), "{w}x{h}: {border:?}");
        ch(&mut app, 'j');
        let border = table_bottom(&screen_at(&app, w, h));
        assert!(border.contains(" run 2 of 2 "), "{w}x{h}: {border:?}");
        let (note, _) = bottom_two(&screen_at(&app, w, h));
        assert!(
            note.starts_with("2 of 6 runs match \"calc\""),
            "{w}x{h}: {note:?}"
        );
    }
}

#[test]
fn a_list_that_scrolls_ends_at_run_n_of_n() {
    let runs: Vec<RunSummary> = (1..=200)
        .map(|n| {
            run(
                &format!("01M5A{n:021}"),
                "complete",
                json!({"mode": "s3-only"}),
            )
        })
        .collect();
    for (w, h) in SIZES {
        let mut app = runs_app(runs.clone());
        key(&mut app, KeyCode::End);
        let border = table_bottom(&screen_at(&app, w, h));
        assert!(border.contains(" run 200 of 200 "), "{w}x{h}: {border:?}");
        key(&mut app, KeyCode::Home);
        let border = table_bottom(&screen_at(&app, w, h));
        assert!(border.contains(" run 1 of 200 "), "{w}x{h}: {border:?}");
    }
}

#[test]
fn no_position_when_nothing_is_selected() {
    for (w, h) in SIZES {
        let mut app = runs_app(six());
        ch(&mut app, '/');
        type_in(&mut app, "zzz");
        key(&mut app, KeyCode::Enter);
        let text = screen_at(&app, w, h);
        let border = table_bottom(&text);
        assert!(!border.contains("run "), "{w}x{h}: {border:?}");
        let (note, help) = bottom_two(&text);
        assert!(
            note.starts_with("no run matches \"zzz\" (6 runs)"),
            "{w}x{h}: {note:?}"
        );
        assert!(help.ends_with("q quit"), "{w}x{h}: {help:?}");
    }
    // An empty list has no position either.
    let app = runs_app(vec![]);
    assert!(!table_bottom(&screen_at(&app, 100, 24)).contains("run "));
}

/// A list of two runs: the first on a repository whose name is `name`, the second on `calc`.
fn runs_on(name: &str) -> Vec<RunSummary> {
    vec![
        run(R1, "complete", repo(name)),
        run(R2, "complete", repo("calc")),
    ]
}

/// A repository name of `n` characters (letters and digits).
fn name_of(n: usize) -> String {
    "abcdefghijklmnopqrstuvwxyz0123456789"
        .chars()
        .cycle()
        .take(n)
        .collect()
}

#[test]
fn the_label_cuts_a_project_of_25_characters_and_spells_out_one_of_24_at_every_size() {
    // `cc:` and 21 characters are 24: whole. `cc:` and 22 are 25: 23 characters and an ellipsis. The longest name a repository has (100
    // characters) is cut the same way. A repository name is ASCII only, so a character is a column.
    let at_24 = name_of(21);
    let at_25 = name_of(22);
    let cut_25 = format!("cc:{}\u{2026}", &at_25[..20]);
    let longest = name_of(100);
    let cut_100 = format!("cc:{}\u{2026}", &longest[..20]);
    for (name, label, wide) in [
        (&at_24, format!("p only cc:{at_24}"), 31usize),
        (&at_25, format!("p only {cut_25}"), 31),
        (&longest, format!("p only {cut_100}"), 31),
    ] {
        for (w, h) in SIZES {
            let app = runs_app(runs_on(name));
            let (_, help) = bottom_two(&screen_at(&app, w, h));
            assert!(
                cols(&help) <= usize::from(w),
                "{w}x{h}, {} characters: {help:?} is wider than the screen",
                name.len()
            );
            assert!(
                help.ends_with("r refresh \u{b7} q quit"),
                "{w}x{h}, {} characters: {help:?}",
                name.len()
            );
            if w == 80 {
                // Room for 11 columns: the shortest wording that names a repository.
                assert!(help.contains(" p this repo \u{b7} "), "{w}x{h}: {help:?}");
            } else {
                // Room for 38 columns at 120 and for more at 200: the first wording is 31 columns and fits.
                assert_eq!(cols(&label), wide);
                assert!(
                    help.contains(&format!(" \u{b7} {label} \u{b7} s settings")),
                    "{w}x{h}, {} characters: {help:?}",
                    name.len()
                );
            }
        }
    }
}

#[test]
fn the_chips_of_a_cut_label_cover_its_columns_and_the_quit_chip_sits_under_its_words() {
    let name = name_of(40);
    let label = format!("p only cc:{}\u{2026}", &name[..20]);
    let app = runs_app(runs_on(&name));
    let text = screen_at(&app, 120, 40);
    let (_, help) = bottom_two(&text);
    assert!(help.contains(&label), "{help:?}");
    let map = app.hit_map();
    for (code, words) in [
        (KeyCode::Char('p'), label.as_str()),
        (KeyCode::Char('q'), "q quit"),
    ] {
        let chip = map
            .regions()
            .iter()
            .find(|r| r.target == Target::Key(code))
            .unwrap_or_else(|| panic!("no chip for {code:?}"));
        let at = help
            .find(words)
            .unwrap_or_else(|| panic!("{words:?} not in {help:?}"));
        assert_eq!(
            usize::from(chip.rect.x),
            cols(&help[..at]),
            "{code:?}: {help:?}"
        );
        assert_eq!(usize::from(chip.rect.width), cols(words), "{code:?}");
    }
}
