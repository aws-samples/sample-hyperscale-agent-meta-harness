//! The Settings screen. `s` on the Runs list and `/settings` in the message box
//! open it. Its rows come from the registry (`ham_core::settings::ROWS`), none is drawn by hand. Enter writes the explicit opposite of the
//! value in force for a switch, cycles the mouse setting, opens a one-line box for a number or a text and the model list for the model;
//! `u` removes the key so the default is back; `e` hands the terminal to the editor. Every write goes through the comment-keeping writer
//! of `ham config`, so a file keeps its comments and its order; the first write that adds a table shows the one-way-door note and waits for
//! a second Enter; the four connection keys and the download folder keep the second Enter with the key name typed; a file that cannot be
//! read as TOML opens the screen read-only with the finding.
//!
//! The screen holds no file and no environment: `settings::perform` is the one place that reads or writes, and these tests play the event
//! loop by handing each `Effect::Settings` to it and the answer back to the app.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{Clock, press};
use crossterm::event::KeyCode;
use ham::output::{Chunk, CmdOutput, Stream};
use ham::tui::settings::{self, Job, Op};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use ham_core::settings::{Key, LOCKED, ROWS};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The owner's file: three connection keys, two comment lines and a comment after a value.
const OWNER: &str = "# ham settings\n# keep this note\napiEndpoint = \"https://abc.execute-api.us-east-1.amazonaws.com/direct/v1\"\nregion = \"us-east-1\"\nawsProfile = \"ham-test\" # the profile\n";
const GOAL_OFF: &str = "configVersion = 1\n\n[run]\ngoal = false\n";

/// A scratch config folder and the extra environment variables a test sets.
struct World {
    root: PathBuf,
    vars: Vec<(String, String)>,
}

impl World {
    fn new(label: &str, content: Option<&str>) -> World {
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ham-settings-{label}-{}-{nanos}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(root.join("cfg/ham")).unwrap();
        let w = World {
            root,
            vars: Vec::new(),
        };
        if let Some(c) = content {
            std::fs::write(w.file(), c).unwrap();
        }
        w
    }

    fn var(mut self, k: &str, v: &str) -> World {
        self.vars.push((k.to_string(), v.to_string()));
        self
    }

    fn file(&self) -> PathBuf {
        self.root.join("cfg/ham/config.toml")
    }

    fn read(&self) -> String {
        std::fs::read_to_string(self.file()).unwrap_or_default()
    }

    fn put(&self, text: &str) {
        std::fs::write(self.file(), text).unwrap();
    }

    fn env(&self) -> impl Fn(&str) -> Option<String> + '_ {
        move |k: &str| {
            if k == "XDG_CONFIG_HOME" {
                return Some(self.root.join("cfg").display().to_string());
            }
            self.vars
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        }
    }
}

/// What the event loop does with the Settings effects: `perform` runs them against the world and the answer goes back to the app. The
/// other effects are returned.
fn feed(app: &mut App, w: &World, effects: Vec<Effect>) -> Vec<Effect> {
    let env = w.env();
    let mut rest = Vec::new();
    for e in effects {
        match e {
            Effect::Settings(job) => {
                let done = settings::perform(&env, job);
                rest.extend(app.update(Msg::Settings(done)));
            }
            other => rest.push(other),
        }
    }
    rest
}

/// A key at a person's pace (a calm one, after any preview has aged), then the effects it caused.
fn step(app: &mut App, clock: &mut Clock, code: KeyCode) -> Vec<Effect> {
    press(app, clock, code, 400)
}

/// Types `text` one character at a time.
fn type_in(app: &mut App, clock: &mut Clock, text: &str) {
    for c in text.chars() {
        let effects = press(app, clock, KeyCode::Char(c), 60);
        assert!(effects.is_empty(), "typing {c:?} caused {effects:?}");
    }
    clock.after(400);
}

fn new_app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(1_792_000_000);
    app
}

/// The Settings screen opened with `s` from the Runs list, and its file read.
fn open(w: &World) -> (App, Clock) {
    let mut app = new_app();
    let mut clock = Clock::new();
    clock.after(5_000);
    let effects = press(&mut app, &mut clock, KeyCode::Char('s'), 100);
    assert_eq!(
        effects,
        vec![Effect::Settings(Job::Load)],
        "s on the Runs list reads the file"
    );
    assert_eq!(app.screen(), Screen::Settings);
    let rest = feed(&mut app, w, effects);
    assert!(rest.is_empty(), "{rest:?}");
    (app, clock)
}

/// The label of a setting row on the screen (`   Goal loop    ...` or ` ▶ Goal loop    ...`), else `None`.
fn row_label(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix(" ▶ ")
        .or_else(|| line.strip_prefix("   "))?;
    let label = rest.split("  ").next()?.trim_end();
    (!label.is_empty() && !label.starts_with(' ')).then(|| label.to_string())
}

/// The row the mark is on, whole.
fn marked(app: &App) -> String {
    screen(app)
        .lines()
        .find(|l| l.contains('▶'))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The row whose label is `label`, as the screen draws it.
fn row_of(app: &App, label: &str) -> String {
    screen(app)
        .lines()
        .find(|l| row_label(l).as_deref() == Some(label))
        .unwrap_or_else(|| panic!("no row {label}:\n{}", screen(app)))
        .to_string()
}

/// Moves the mark down to the row `label`.
fn goto(app: &mut App, clock: &mut Clock, label: &str) {
    // From the top: the mark may be below the row.
    step(app, clock, KeyCode::Home);
    // The registry rows, the five of the engine and the locked line, and two spare.
    for _ in 0..(ROWS.len() + 8) {
        if row_label(&format!(" {}", marked(app))).as_deref() == Some(label) {
            return;
        }
        step(app, clock, KeyCode::Down);
    }
    panic!("the mark never reached {label}:\n{}", screen(app));
}

/// The height of the window that holds the whole list of the Settings screen: the registry rows, the 5 of the Cline engine, six group
/// headings and the locked line, and the title, a blank line and the key line (it was 47 with 32 rows, 51 with 36, 56 with 41, 57 with 42 and 58 with 43, 59 with 44 and 60 with 45: each
/// setting adds one line).
const WHOLE_LIST_ROWS: u16 = 61;

/// The screen at 120 columns and [`WHOLE_LIST_ROWS`] rows.
fn screen(app: &App) -> String {
    screen_text(app, 120, WHOLE_LIST_ROWS)
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
        // The version tag at the end of row 0 changes with every release (tui_version_tag.rs pins it); the snapshots hold the screen without it.
        out.push_str(if y == 0 {
            ham::tui::version_tag::split_tag(line.trim_end()).0
        } else {
            line.trim_end()
        });
        out.push('\n');
    }
    out
}

fn apply(key: Key, op: Op) -> Effect {
    Effect::Settings(Job::Apply { key, op })
}

fn set(key: Key, v: &str) -> Effect {
    apply(key, Op::Set(v.to_string()))
}

// ---- the rows ------------------------------------------------------------------------------------------------------------------------

#[test]
fn rows_are_generated_from_the_registry_and_the_locked_line_names_the_seven_classes() {
    let w = World::new("rows", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    let text = screen(&app);
    assert!(
        text.lines().next().unwrap().starts_with("ham · Settings"),
        "{text}"
    );
    // One row for every row of the registry, once each, and no other row.
    let labels: Vec<String> = text.lines().filter_map(row_label).collect();
    for r in &ROWS {
        let n = labels.iter().filter(|l| l.as_str() == r.label).count();
        assert_eq!(n, 1, "{} is on the screen {n} times:\n{text}", r.label);
    }
    let others: Vec<&String> = labels
        .iter()
        .filter(|l| !ROWS.iter().any(|r| r.label == l.as_str()))
        .collect();
    // The only other rows are the five of the selected engine's settings and the locked line.
    assert_eq!(
        others,
        [
            "Reasoning effort",
            "Command time limit",
            "Compaction",
            "Compaction trigger",
            "Tools switched off",
            "Locked by the harness"
        ],
        "{text}"
    );
    assert_eq!(ROWS.len(), 46);
    // The groups of the registry head the rows.
    for g in ["Run", "Interface", "Paths", "Run limits", "Connection"] {
        assert!(
            text.lines().any(|l| l.trim() == g && l.starts_with(' ')),
            "group {g}:\n{text}"
        );
    }
    // Each value says where it came from and when a change takes effect.
    let goal = row_of(&app, "Goal loop");
    assert!(goal.contains("stage default"), "{goal}");
    assert!(goal.contains("default"), "{goal}");
    assert!(goal.contains("next run"), "{goal}");
    let region = row_of(&app, "Region");
    assert!(
        region.contains("us-east-1") && region.contains("file"),
        "{region}"
    );
    assert!(region.contains("restart"), "{region}");

    // The locked line names the seven classes in one line.
    let locked = row_of(&app, "Locked by the harness");
    assert_eq!(LOCKED.len(), 7);
    for c in &LOCKED {
        assert!(locked.contains(c.label), "{} is not in {locked:?}", c.label);
    }
    // Enter on it shows each class with its reason; Esc closes the list before it leaves the screen.
    goto(&mut app, &mut clock, "Locked by the harness");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    let text = screen(&app);
    for c in &LOCKED {
        assert!(text.contains(c.name), "{} is not listed:\n{text}", c.name);
        assert!(text.contains(c.reason), "{}:\n{text}", c.reason);
    }
    assert!(step(&mut app, &mut clock, KeyCode::Esc).is_empty());
    assert_eq!(app.screen(), Screen::Settings, "Esc closed the list only");
    assert!(!screen(&app).contains(LOCKED[0].reason));
    assert_eq!(w.read(), OWNER, "reading changes nothing");
}

#[test]
fn settings_snapshots_at_80x24_and_120x47() {
    // A file with two tables and a comment, and no file at all.
    let configured = format!("{OWNER}{GOAL_OFF}\n[ui]\nbell = false\n");
    for (label, content) in [("configured", Some(configured.as_str())), ("none", None)] {
        let w = World::new(&format!("snap-{label}"), content);
        let (mut app, mut clock) = open(&w);
        for (cols, rows) in [(80u16, 24u16), (120, WHOLE_LIST_ROWS)] {
            insta::assert_snapshot!(
                format!("settings_{label}_{cols}x{rows}"),
                screen_text(&app, cols, rows)
            );
        }
        // The list is taller than an 80x24 window (the engine's group), so the end of it is a second window: the Connection group and the
        // locked line at 80 columns, which the first window leaves below its edge.
        step(&mut app, &mut clock, KeyCode::End);
        insta::assert_snapshot!(
            format!("settings_{label}_80x24_end"),
            screen_text(&app, 80, 24)
        );
    }
}

// ---- the locked line on a narrow or short window -----------------------------------------------------------------------------------------

/// The columns the label of the locked line takes, with its mark and the gap: the summary starts here, and a second row that
/// continued it would start with this many blanks.
const LOCKED_LABEL_COLUMNS: usize = 3 + 21 + 2;

/// The locked summary for the first `k` classes in the form the line writes, so a test can say how many names fit.
fn summary_of(k: usize) -> String {
    let names: Vec<&str> = LOCKED.iter().take(k).map(|c| c.label).collect();
    match k {
        7 => names.join(" · "),
        0 => format!("{} classes", LOCKED.len()),
        _ => format!("{} · +{} more", names.join(" · "), LOCKED.len() - k),
    }
}

/// The text of the screen with its runs of blanks folded to one space and its rows joined: a reason that wraps reads as one sentence.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_locked_line_is_one_row_at_every_width_and_counts_the_classes_it_does_not_name() {
    let w = World::new("locked-widths", Some(OWNER));
    let (app, _) = open(&w);
    for cols in 40..=200u16 {
        let text = screen_text(&app, cols, WHOLE_LIST_ROWS);
        let rows: Vec<&str> = text.lines().collect();
        let at: Vec<usize> = (0..rows.len())
            .filter(|&i| rows[i].contains("Locked by the harness"))
            .collect();
        assert_eq!(at.len(), 1, "{cols} columns:\n{text}");
        let line = rows[at[0]];
        // Nothing continues it on a second row: the row below is blank in the label column or is another part of the screen.
        let below = rows.get(at[0] + 1).copied().unwrap_or("");
        assert!(
            !below.starts_with(&" ".repeat(LOCKED_LABEL_COLUMNS)) || below.trim().is_empty(),
            "{cols} columns: the locked line wraps:\n{text}"
        );
        // No separator dangles at the end of it.
        assert!(!line.trim_end().ends_with('·'), "{cols} columns: {line:?}");
        // It says as many classes as fit the room, in the order of the registry, and counts the rest.
        let summary = line.split("Locked by the harness").nth(1).unwrap().trim();
        let room = usize::from(cols)
            .saturating_sub(LOCKED_LABEL_COLUMNS)
            .max(10);
        let k = (0..=LOCKED.len())
            .rev()
            .find(|&k| summary_of(k).chars().count() <= room)
            .unwrap_or(0);
        assert_eq!(summary, summary_of(k), "{cols} columns, room {room}");
    }
    // The widths the screens are drawn at in the snapshots.
    assert!(
        screen_text(&app, 80, WHOLE_LIST_ROWS)
            .lines()
            .any(|l| l.trim_end().ends_with("telemetry · +4 more")),
        "80 columns name three classes and count four"
    );
    assert!(
        screen_text(&app, 120, WHOLE_LIST_ROWS)
            .lines()
            .any(|l| l.trim_end().ends_with("network · owned elsewhere")),
        "120 columns name all seven"
    );
}

/// The five rows of the Cline engine's settings in the order of the engine's table (`engines::setting_texts`).
const ENGINE_LABELS: [&str; 5] = [
    "Reasoning effort",
    "Command time limit",
    "Compaction",
    "Compaction trigger",
    "Tools switched off",
];

/// The last line of `text` that is not blank: the key line of the screen.
fn key_line(text: &str) -> &str {
    text.lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or_default()
}

// The screen was once built to fit an 80x24 window whole (the 16 rows the registry had then, their four group headings and the locked line
// took exactly the 21 body rows) and a test said so. The registry has 24 rows now. The group of
// the engine's settings adds 5 rows and a heading: 27 lines in all with 16 registry rows, 36 with 24 and the heading of the stall limits. The window cannot hold the
// screen any more; what it keeps is that every row and the whole locked line come into view, whole, one after
// the other as the mark walks the list, with the title above and the key line below on every window.
#[test]
fn the_80x24_window_scrolls_and_brings_every_row_and_the_whole_locked_line_into_view() {
    for (label, content) in [("configured", Some(OWNER)), ("none", None)] {
        let w = World::new(&format!("window-{label}"), content);
        let (mut app, mut clock) = open(&w);
        // The rows in the order the screen draws them, read from a window tall enough for all of it: those of the registry, the five of the
        // engine and the locked line, each once and nothing else.
        let tall = screen_text(&app, 120, WHOLE_LIST_ROWS);
        let order: Vec<String> = tall.lines().filter_map(row_label).collect();
        let mut want: Vec<String> = ROWS
            .iter()
            .map(|r| r.label.to_string())
            .chain(ENGINE_LABELS.iter().map(|l| (*l).to_string()))
            .chain(["Locked by the harness".to_string()])
            .collect();
        let mut got = order.clone();
        got.sort();
        want.sort();
        assert_eq!(got, want, "{label}: the rows the screen draws:\n{tall}");
        assert_eq!(order[0], "Model", "{label}:\n{tall}");
        for (n, expected) in order.iter().enumerate() {
            let text = screen_text(&app, 80, 24);
            assert!(
                text.starts_with("ham · Settings"),
                "{label}: the title is on the window at row {n}:\n{text}"
            );
            assert!(
                key_line(&text).ends_with("Esc back · q quit"),
                "{label}: the key line is on the window at row {n}:\n{text}"
            );
            let marks: Vec<&str> = text.lines().filter(|l| l.contains('▶')).collect();
            assert_eq!(
                marks.len(),
                1,
                "{label}: one marked line at row {n}:\n{text}"
            );
            assert_eq!(
                row_label(marks[0]).as_deref(),
                Some(expected.as_str()),
                "{label}: the marked line is the row {expected} whole:\n{text}"
            );
            for line in text.lines() {
                assert!(
                    !line.trim_end().ends_with('·'),
                    "{label}: a separator dangles in {line:?} at row {n}:\n{text}"
                );
            }
            if expected == "Locked by the harness" {
                assert!(
                    marks[0].contains("credentials · provider · telemetry · +4 more"),
                    "{label}: the whole locked line:\n{text}"
                );
            }
            if n == 0 {
                assert!(
                    text.lines().any(|l| l.trim() == "Run"),
                    "{label}: the first window opens on the Run group:\n{text}"
                );
            }
            if n + 1 < order.len() {
                step(&mut app, &mut clock, KeyCode::Down);
            }
        }
        // End is the same place the walk ended on.
        step(&mut app, &mut clock, KeyCode::Home);
        step(&mut app, &mut clock, KeyCode::End);
        assert!(
            marked(&app).starts_with("▶ Locked by the harness"),
            "{label}: {}",
            marked(&app)
        );
    }
}

/// The heading of each group of the screen and the first row under it, in the order the screen draws them.
const GROUP_STARTS: [(&str, &str); 5] = [
    ("Run", "Model"),
    ("Cline engine", "Reasoning effort"),
    ("Interface", "Mouse"),
    ("Paths", "Default repository"),
    ("Connection", "Service address"),
];

/// Presses `key` until the marked line of the `rows`-high, 80-column window is the row `label`, drawing after every key as a terminal does,
/// and returns that window.
fn press_until_marked(
    app: &mut App,
    clock: &mut Clock,
    key: KeyCode,
    label: &str,
    rows: u16,
) -> String {
    // The registry rows, the five of the engine and the locked line, and two spare.
    for _ in 0..(ROWS.len() + 8) {
        let text = screen_text(app, 80, rows);
        let on_it = text
            .lines()
            .find(|l| l.contains('▶'))
            .is_some_and(|l| row_label(l).as_deref() == Some(label));
        if on_it {
            return text;
        }
        step(app, clock, key);
    }
    panic!(
        "the mark never reached {label}:\n{}",
        screen_text(app, 80, rows)
    );
}

/// True when the line above the marked line of `text` is the heading `heading`.
fn heading_above_mark(text: &str, heading: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .position(|l| l.contains('▶'))
        .and_then(|at| at.checked_sub(1))
        .is_some_and(|above| lines[above].trim() == heading)
}

// A window that scrolls shows the row the mark is on; a row that opens a group has its heading directly above it, whether the mark came down
// to it or up to it. (The 80x24 window began to scroll with the engine's group: End and then Home left the Run heading above the window.)
#[test]
fn a_row_that_opens_a_group_has_its_heading_above_it_whether_the_mark_came_down_or_up() {
    let w = World::new("headings", Some(OWNER));
    for rows in [24u16, 15, 12] {
        let (mut app, mut clock) = open(&w);
        for (heading, first) in GROUP_STARTS {
            let text = press_until_marked(&mut app, &mut clock, KeyCode::Down, first, rows);
            assert!(
                heading_above_mark(&text, heading),
                "{rows} rows, down to {first}: the heading {heading} is above it:\n{text}"
            );
        }
        step(&mut app, &mut clock, KeyCode::End);
        for (heading, first) in GROUP_STARTS.iter().rev() {
            let text = press_until_marked(&mut app, &mut clock, KeyCode::Up, first, rows);
            assert!(
                heading_above_mark(&text, heading),
                "{rows} rows, up to {first}: the heading {heading} is above it:\n{text}"
            );
        }
        // Home after End is the same place as the first arrival.
        step(&mut app, &mut clock, KeyCode::End);
        step(&mut app, &mut clock, KeyCode::Home);
        let home = screen_text(&app, 80, rows);
        assert!(
            heading_above_mark(&home, "Run"),
            "{rows} rows, Home after End: the Run heading is above Model:\n{home}"
        );
    }
}

#[test]
fn enter_on_the_locked_line_shows_every_class_and_reason_on_an_80x24_window() {
    let w = World::new("locked-80x24", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Locked by the harness");
    // The mark on the line: it stays on the window whole, and the window is the one a terminal opens at.
    let before = screen_text(&app, 80, 24);
    assert!(before.contains("▶ Locked by the harness"), "{before}");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("▶ Locked by the harness"), "{text}");
    let all = flat(&text);
    for c in &LOCKED {
        assert!(
            all.contains(&format!("{} {}", c.name, c.reason)),
            "{} is not listed with its reason on 80x24:\n{text}",
            c.name
        );
    }
    // Esc closes the list and gives back the window it opened from, filled: no blank tail where the reasons were. (While the whole screen
    // fitted the window this was the top of the list; with the engine's group the list is taller than the window, and the window it
    // opened from is the last full one, ending on the locked line.)
    assert!(step(&mut app, &mut clock, KeyCode::Esc).is_empty());
    let closed = screen_text(&app, 80, 24);
    assert_eq!(
        closed, before,
        "closing the list gives back the window it opened from"
    );
    assert!(
        closed
            .lines()
            .skip(1)
            .take(21)
            .last()
            .is_some_and(|l| l.contains("Locked by the harness")),
        "the window ends on the locked line:\n{closed}"
    );
    assert!(
        closed.contains("▶ Locked by the harness"),
        "the mark is still on the line:\n{closed}"
    );
    let blank_rows = closed.lines().filter(|l| l.trim().is_empty()).count();
    assert!(
        blank_rows <= 2,
        "{blank_rows} blank rows after the list closed:\n{closed}"
    );
}

#[test]
fn a_list_taller_than_the_window_keeps_the_locked_line_and_the_first_class_on_it() {
    let w = World::new("locked-short", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Locked by the harness");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    for rows in [15u16, 12, 9] {
        let text = screen_text(&app, 80, rows);
        assert!(
            text.contains("▶ Locked by the harness"),
            "the line is on a {rows}-row window:\n{text}"
        );
        assert!(
            text.contains(LOCKED[0].name),
            "the first class follows it on a {rows}-row window:\n{text}"
        );
    }
}

#[test]
fn the_screen_reads_the_file_before_it_shows_rows() {
    let mut app = new_app();
    let mut clock = Clock::new();
    clock.after(5_000);
    press(&mut app, &mut clock, KeyCode::Char('s'), 100);
    let text = screen(&app);
    assert!(text.contains("Reading"), "{text}");
    assert!(
        !text.contains("Goal loop"),
        "no row before the answer:\n{text}"
    );
}

// ---- opening and leaving -----------------------------------------------------------------------------------------------------------------

#[test]
fn s_opens_the_screen_from_runs_and_slash_settings_from_a_message_box() {
    // From the Runs list, and Esc goes back to it.
    let w = World::new("open", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    assert!(step(&mut app, &mut clock, KeyCode::Esc).is_empty());
    assert_eq!(app.screen(), Screen::Runs);

    // The filter line takes the s as text and opens nothing.
    let mut app = new_app();
    let mut clock = Clock::new();
    clock.after(5_000);
    press(&mut app, &mut clock, KeyCode::Char('/'), 100);
    let effects = press(&mut app, &mut clock, KeyCode::Char('s'), 100);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.screen(), Screen::Runs);

    // From the message box of a run: the typed word opens the screen and Esc comes back to the run.
    let (mut app, mut clock) = chat_support::ready();
    let effects = chat_support::send(&mut app, &mut clock, "/settings");
    assert_eq!(effects, vec![Effect::Settings(Job::Load)], "{effects:?}");
    assert_eq!(app.screen(), Screen::Settings);
    assert_eq!(app.chat_text().as_deref(), Some(""), "the line was sent");
    feed(&mut app, &w, effects);
    assert!(screen(&app).contains("Goal loop"));
    assert!(step(&mut app, &mut clock, KeyCode::Esc).is_empty());
    assert_eq!(app.screen(), Screen::Dashboard);
    assert!(
        screen(&app).starts_with(&format!("ham · Run {}", chat_support::RUN)),
        "{}",
        screen(&app)
    );
}

#[test]
fn q_quits_from_the_list_and_is_text_in_a_box() {
    let w = World::new("quit", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    assert_eq!(
        step(&mut app, &mut clock, KeyCode::Char('q')),
        vec![Effect::Quit]
    );
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Download folder");
    step(&mut app, &mut clock, KeyCode::Enter);
    type_in(&mut app, &mut clock, "qz");
    assert!(
        screen(&app).contains("qz"),
        "q is a letter in the box:\n{}",
        screen(&app)
    );
}

// ---- Enter and u: the writes ---------------------------------------------------------------------------------------------------------------

#[test]
fn enter_on_a_bool_row_writes_the_opposite_explicit_value_and_keeps_comments() {
    let w = World::new("toggle", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Goal loop");
    // The first table of the file waits for a second Enter with the one-way-door note; nothing is written by showing it.
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    assert_eq!(w.read(), OWNER);
    assert!(
        screen(&app).contains("This adds the first table to config.toml"),
        "{}",
        screen(&app)
    );
    // The second Enter writes the explicit opposite of "stage default (on)": off.
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::RunGoal, "false")]);
    assert!(feed(&mut app, &w, effects).is_empty());
    // The file has its three keys, both comment lines and the comment after a value, byte for byte, and the new table at the end.
    assert_eq!(w.read(), format!("{OWNER}{GOAL_OFF}"));
    let row = row_of(&app, "Goal loop");
    assert!(row.contains("off") && row.contains("file"), "{row}");
    assert!(screen(&app).contains("Saved Goal loop"), "{}", screen(&app));
    // An explicit false is turned the other way: on, written as true.
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(
        effects,
        vec![set(Key::RunGoal, "true")],
        "no door the second time"
    );
    feed(&mut app, &w, effects);
    assert_eq!(
        w.read(),
        format!("{OWNER}configVersion = 1\n\n[run]\ngoal = true\n")
    );
    assert!(row_of(&app, "Goal loop").contains("on"));

    // A switch whose default is on turns off, and one that is off turns on.
    let w = World::new("toggle2", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Test report");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(
        effects,
        vec![set(Key::ReportTests, "false")],
        "a key of the first nine opens no door"
    );
    feed(&mut app, &w, effects);
    assert!(w.read().contains("reportTests = false\n"), "{}", w.read());
    assert!(!w.read().contains("configVersion"), "{}", w.read());
    goto(&mut app, &mut clock, "Notifications");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert!(effects.is_empty(), "the first table: the door line waits");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::UiNotify, "true")]);
}

#[test]
fn u_unsets_and_the_file_returns_to_its_original_bytes() {
    let w = World::new("unset", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Goal loop");
    step(&mut app, &mut clock, KeyCode::Enter);
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    feed(&mut app, &w, effects);
    assert_eq!(w.read(), format!("{OWNER}{GOAL_OFF}"));
    // u removes the key, its table and configVersion: the owner's file, byte for byte.
    let effects = step(&mut app, &mut clock, KeyCode::Char('u'));
    assert_eq!(effects, vec![apply(Key::RunGoal, Op::Unset)]);
    feed(&mut app, &w, effects);
    assert_eq!(w.read(), OWNER);
    let row = row_of(&app, "Goal loop");
    assert!(
        row.contains("stage default") && row.contains("default"),
        "{row}"
    );
    // A key that is not in the file is already at its default: nothing is sent and nothing is written.
    let effects = step(&mut app, &mut clock, KeyCode::Char('u'));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app).contains("already"), "{}", screen(&app));
    assert_eq!(w.read(), OWNER);
}

#[test]
fn the_door_line_shows_once_in_the_preview_for_the_first_new_table_only() {
    let w = World::new("door", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Goal loop");
    // The preview names the version and the way back, and Esc withdraws it with nothing written.
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    let text = screen(&app);
    assert!(
        text.contains(&format!(
            "ham versions before v{} refuse this file",
            ham::version::CLIENT
        )),
        "{text}"
    );
    // The preview wraps at the width of the screen; the words are the same.
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("ham config unset run.goal"), "{text}");
    assert!(step(&mut app, &mut clock, KeyCode::Esc).is_empty());
    assert_eq!(
        app.screen(),
        Screen::Settings,
        "Esc withdrew the preview only"
    );
    assert_eq!(w.read(), OWNER);
    assert!(!screen(&app).contains("This adds the first table"));
    // Confirmed, the first table is written. A key of a second table has no preview at all: the door is open already.
    step(&mut app, &mut clock, KeyCode::Enter);
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    feed(&mut app, &w, effects);
    goto(&mut app, &mut clock, "Bell");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::UiBell, "false")]);
    assert!(!screen(&app).contains("This adds the first table"));
    feed(&mut app, &w, effects);
    assert_eq!(w.read(), format!("{OWNER}{GOAL_OFF}\n[ui]\nbell = false\n"));
}

#[test]
fn a_security_row_needs_the_second_enter_and_the_typed_name() {
    let w = World::new("security", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Region");
    // Enter opens a one-line box with the file's value; the value is typed over it.
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    assert!(screen(&app).contains("us-east-1"), "{}", screen(&app));
    for _ in 0.."us-east-1".len() {
        press(&mut app, &mut clock, KeyCode::Backspace, 60);
    }
    type_in(&mut app, &mut clock, "us-west-2");
    // The first Enter shows what changes and what must be typed; nothing is written.
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    let text = screen(&app);
    assert!(text.contains("Change region to us-west-2"), "{text}");
    assert!(text.contains("region"), "{text}");
    assert_eq!(w.read(), OWNER);
    // Enter with nothing typed, and with the wrong word, writes nothing.
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    type_in(&mut app, &mut clock, "regio");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    assert_eq!(w.read(), OWNER);
    // The key name typed in full, then Enter: the write.
    type_in(&mut app, &mut clock, "n");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::Region, "us-west-2")]);
    feed(&mut app, &w, effects);
    assert!(
        w.read().contains("region = \"us-west-2\"\n"),
        "{}",
        w.read()
    );
    assert!(w.read().starts_with("# ham settings\n# keep this note\n"));
    assert!(row_of(&app, "Region").contains("us-west-2"));

    // Esc withdraws the preview; the other security rows ask the same way, and so does u on one of them.
    for label in ["Service address", "AWS profile", "Download folder"] {
        let (mut app, mut clock) = open(&w);
        goto(&mut app, &mut clock, label);
        step(&mut app, &mut clock, KeyCode::Enter);
        type_in(&mut app, &mut clock, "x");
        step(&mut app, &mut clock, KeyCode::Enter);
        assert!(screen(&app).contains("type"), "{label}:\n{}", screen(&app));
        assert!(step(&mut app, &mut clock, KeyCode::Esc).is_empty());
    }
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "AWS profile");
    assert!(step(&mut app, &mut clock, KeyCode::Char('u')).is_empty());
    assert!(screen(&app).contains("aws_profile"), "{}", screen(&app));
}

#[test]
fn a_number_is_typed_in_a_one_line_box_and_a_wrong_one_stays_open_with_the_reason() {
    let w = World::new("number", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Answer wait");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    type_in(&mut app, &mut clock, "43201");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    let text = screen(&app);
    assert!(
        text.contains("43201") && text.contains("43200"),
        "the box stays open and says why:\n{text}"
    );
    assert_eq!(w.read(), OWNER);
    // Fixed, it is written in the legacy form the setup script reads.
    for _ in 0..5 {
        press(&mut app, &mut clock, KeyCode::Backspace, 60);
    }
    type_in(&mut app, &mut clock, "45");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::InputWaitMinutes, "45")]);
    feed(&mut app, &w, effects);
    assert!(
        w.read().ends_with("inputWaitMinutes = 45\n"),
        "{}",
        w.read()
    );
    assert!(row_of(&app, "Answer wait").contains("45"));
    // Esc leaves the box with nothing written.
    step(&mut app, &mut clock, KeyCode::Enter);
    type_in(&mut app, &mut clock, "9");
    assert!(step(&mut app, &mut clock, KeyCode::Esc).is_empty());
    assert_eq!(app.screen(), Screen::Settings);
    assert!(w.read().ends_with("inputWaitMinutes = 45\n"));
}

/// The five stall keys of a run set in the file: numbers of calls and minutes.
const STALL_SET: &str = "stall_calls = 45\nstall_window_minutes = 90\nstall_min_calls = 12\ncommand_grace_minutes = 180\n";

#[test]
fn a_stall_key_that_is_set_is_drawn_with_its_value_and_its_unit() {
    let w = World::new("stall-set", Some(&format!("{OWNER}{GOAL_OFF}{STALL_SET}")));
    let (app, _) = open(&w);
    for (label, shown) in [
        ("Stall: calls", "45 calls"),
        ("Stall: minutes", "90 minutes"),
        ("Stall: least calls", "12 calls"),
        ("Command grace", "180 minutes"),
    ] {
        let row = row_of(&app, label);
        assert!(row.contains(shown), "{label}: {row}");
        assert!(row.contains("file"), "{label} names the file: {row}");
    }
}

#[test]
fn a_stall_key_is_typed_in_the_box_and_a_value_outside_its_range_stays_open_with_the_reason() {
    let w = World::new("stall-box", Some(&format!("{OWNER}{GOAL_OFF}")));
    let before = w.read();
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Stall: calls");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    // 4 is below the least of 5, and 1001 is above the most of 1000: the box stays open, says the range and writes nothing
    for wrong in ["4", "1001"] {
        type_in(&mut app, &mut clock, wrong);
        assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
        let text = screen(&app);
        assert!(
            text.contains("from 5 to 1000"),
            "{wrong} is refused with the range:\n{text}"
        );
        assert_eq!(w.read(), before, "{wrong} wrote nothing");
        for _ in 0..wrong.len() {
            press(&mut app, &mut clock, KeyCode::Backspace, 60);
        }
    }
    // a value in the range is written to the [run] table and drawn with its unit
    type_in(&mut app, &mut clock, "45");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::RunStallCalls, "45")]);
    feed(&mut app, &w, effects);
    assert!(
        w.read().contains("[run]") && w.read().contains("stall_calls = 45"),
        "{}",
        w.read()
    );
    assert!(row_of(&app, "Stall: calls").contains("45 calls"));
    // and a number of minutes is typed the same way
    goto(&mut app, &mut clock, "Command grace");
    step(&mut app, &mut clock, KeyCode::Enter);
    type_in(&mut app, &mut clock, "90");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::RunCommandGraceMinutes, "90")]);
    feed(&mut app, &w, effects);
    assert!(row_of(&app, "Command grace").contains("90 minutes"));
}

#[test]
fn a_stall_key_that_is_not_set_is_drawn_with_the_unit_of_its_default() {
    // the same words as a value the user set ("45 calls"): the unit is part of the default too
    let w = World::new("stall-default", Some(&format!("{OWNER}{GOAL_OFF}")));
    let (app, _) = open(&w);
    for (label, shown) in [
        ("Stall: calls", "30 calls"),
        ("Stall: minutes", "20 minutes"),
        ("Stall: least calls", "10 calls"),
        ("Command grace", "120 minutes"),
    ] {
        let row = row_of(&app, label);
        assert!(row.contains(shown), "{label}: {row}");
        assert!(
            row.contains("default"),
            "{label} says it is the default: {row}"
        );
    }
}

#[test]
fn one_is_singular_and_the_zero_of_the_grace_reads_off() {
    let file = format!(
        "{OWNER}{GOAL_OFF}stall_min_calls = 1\nstall_window_minutes = 1\ncommand_grace_minutes = 0\n"
    );
    let w = World::new("stall-one", Some(&file));
    let (app, _) = open(&w);
    let least = row_of(&app, "Stall: least calls");
    assert!(
        least.contains("1 call") && !least.contains("1 calls"),
        "{least}"
    );
    let minutes = row_of(&app, "Stall: minutes");
    assert!(
        minutes.contains("1 minute") && !minutes.contains("1 minutes"),
        "{minutes}"
    );
    let grace = row_of(&app, "Command grace");
    assert!(
        grace.contains("off") && !grace.contains("0 minutes"),
        "{grace}"
    );
}

#[test]
fn the_lifecycle_flow_row_steps_auto_full_quick_auto_and_writes_the_word_under_run() {
    // The setting that picks the text of the lifecycle contract has a row in the Run group; Enter goes to the next word. The first key of a
    // table adds the table to the file, so the first Enter shows the door line and the second writes (as the Goal loop row does).
    let w = World::new("flow-cycle", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Lifecycle flow");
    let row = row_of(&app, "Lifecycle flow");
    assert!(row.contains("auto") && row.contains("default"), "{row}");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert!(effects.is_empty(), "the first table: the door line waits");
    assert!(
        screen(&app).contains("This adds the first table to config.toml"),
        "{}",
        screen(&app)
    );
    for word in ["full", "quick", "auto"] {
        let effects = step(&mut app, &mut clock, KeyCode::Enter);
        assert_eq!(
            effects,
            vec![set(Key::RunLifecycleFlow, word)],
            "next is {word}"
        );
        feed(&mut app, &w, effects);
        let row = row_of(&app, "Lifecycle flow");
        assert!(row.contains(word) && row.contains("file"), "{row}");
    }
    let text = w.read();
    assert!(
        text.contains("[run]") && text.contains("lifecycle_flow = \"auto\"\n"),
        "{text}"
    );
    assert!(row_of(&app, "Lifecycle flow").contains("next run"));
}

#[test]
fn the_machine_row_steps_auto_instances_microvm_auto_and_writes_the_word_under_run() {
    // The setting that picks the machine of a session has a row in the Run limits group; Enter goes to the next word. The first key of a
    // table adds the table to the file, so the first Enter shows the door line and the second writes.
    let w = World::new("compute-cycle", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Machine");
    let row = row_of(&app, "Machine");
    assert!(row.contains("auto") && row.contains("default"), "{row}");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert!(effects.is_empty(), "the first table: the door line waits");
    for word in ["instances", "microvm", "auto"] {
        let effects = step(&mut app, &mut clock, KeyCode::Enter);
        assert_eq!(effects, vec![set(Key::RunCompute, word)], "next is {word}");
        feed(&mut app, &w, effects);
        let row = row_of(&app, "Machine");
        assert!(row.contains(word) && row.contains("file"), "{row}");
    }
    let text = w.read();
    assert!(
        text.contains("[run]") && text.contains("compute = \"auto\"\n"),
        "{text}"
    );
    assert!(row_of(&app, "Machine").contains("next run"));
}

#[test]
fn the_mouse_row_cycles_and_the_model_row_opens_the_model_list() {
    let w = World::new("cycle", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Mouse");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(
        effects,
        vec![set(Key::Mouse, "false")],
        "on, the default, goes to off"
    );
    feed(&mut app, &w, effects);
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::Mouse, "auto")]);
    feed(&mut app, &w, effects);
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::Mouse, "true")]);
    feed(&mut app, &w, effects);
    assert!(w.read().contains("mouse = true\n"), "{}", w.read());
    assert!(row_of(&app, "Mouse").contains("restart"));

    // The model row lists the models that can be chosen, with the one in force marked; Enter on another writes its full id.
    goto(&mut app, &mut clock, "Model");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    let text = screen(&app);
    let list = ham_core::models::builtin();
    let choosable: Vec<_> = list.models().iter().filter(|m| m.selectable()).collect();
    assert!(choosable.len() >= 2, "the test needs two models");
    for m in &choosable {
        assert!(text.contains(&m.name), "{} is not listed:\n{text}", m.name);
    }
    // The model in force (the built-in default) is marked; the one after it in the list is the pick.
    let at = choosable.iter().position(|m| m.default).unwrap();
    assert!(
        marked(&app).contains(&choosable[at].name),
        "{}",
        screen(&app)
    );
    let pick = choosable[at + 1];
    // Move to the model that is not the default and choose it.
    let mut guard = 0;
    while !marked(&app).contains(&pick.name) && guard < 20 {
        step(&mut app, &mut clock, KeyCode::Down);
        guard += 1;
    }
    assert!(marked(&app).contains(&pick.name), "{}", screen(&app));
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::DefaultModel, &pick.id)]);
    feed(&mut app, &w, effects);
    assert!(
        w.read()
            .contains(&format!("defaultModel = \"{}\"\n", pick.id)),
        "{}",
        w.read()
    );
    assert!(row_of(&app, "Model").contains(&pick.name));
    // Esc in the list leaves it without a write.
    step(&mut app, &mut clock, KeyCode::Enter);
    assert!(step(&mut app, &mut clock, KeyCode::Esc).is_empty());
    assert_eq!(app.screen(), Screen::Settings);
}

// ---- a file that cannot be used --------------------------------------------------------------------------------------------------------

#[test]
fn an_unreadable_or_invalid_file_opens_read_only_with_the_finding() {
    for (label, content) in [
        ("syntax", "region = \n"),
        ("nul", "region = \"a\"\n\0\n"),
        (
            "deep",
            &format!("x = {}1{}\n", "[".repeat(40), "]".repeat(40)),
        ),
    ] {
        let w = World::new(&format!("bad-{label}"), Some(content));
        let (mut app, mut clock) = open(&w);
        let text = screen(&app);
        assert!(
            text.contains("line") || text.contains("NUL") || text.contains("nested"),
            "{label}:\n{text}"
        );
        assert!(
            text.contains("read-only") || text.contains("cannot be changed"),
            "{label}:\n{text}"
        );
        // There is no write path: Enter and u send nothing, and the file is as it was.
        for code in [KeyCode::Enter, KeyCode::Char('u')] {
            let effects = step(&mut app, &mut clock, code);
            assert!(effects.is_empty(), "{label}: {code:?} sent {effects:?}");
        }
        assert_eq!(w.read(), content, "{label}");
        // The editor is still offered, because that is how the file is repaired; the hand-over is the same child `ham config edit` runs.
        let effects = step(&mut app, &mut clock, KeyCode::Char('e'));
        assert_eq!(
            effects,
            vec![Effect::RunTerminal {
                argv: vec!["config".into(), "edit".into()]
            }],
            "{label}"
        );
        assert_eq!(w.read(), content);
    }
}

#[test]
fn a_file_with_a_bad_key_is_written_around_and_shows_its_finding() {
    let w = World::new(
        "badkey",
        Some(&format!(
            "{OWNER}configVersion = 1\n\n[run]\ngoall = true\n"
        )),
    );
    let (mut app, mut clock) = open(&w);
    assert!(screen(&app).contains("goall"), "{}", screen(&app));
    goto(&mut app, &mut clock, "Rules");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::RunRules, "false")]);
    feed(&mut app, &w, effects);
    assert!(
        w.read().contains("goall = true"),
        "the bad key is left where it is:\n{}",
        w.read()
    );
    assert!(w.read().contains("rules = false"));
}

#[test]
fn a_file_from_a_newer_ham_is_read_and_never_changed() {
    let newer = format!("{OWNER}configVersion = 2\n\n[ui]\nbell = false\nsparkle = true\n");
    let w = World::new("newer", Some(&newer));
    let (mut app, mut clock) = open(&w);
    assert!(screen(&app).contains("newer"), "{}", screen(&app));
    goto(&mut app, &mut clock, "Rules");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app).contains("newer"), "{}", screen(&app));
    assert_eq!(w.read(), newer);
}

#[test]
fn a_write_that_fails_says_so_and_the_screen_reads_the_file_again() {
    let w = World::new("fail", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Rules");
    // Someone breaks the file between the read and the key.
    w.put("region = \n");
    // The first table of the file: the preview first, then the write.
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::RunRules, "false")]);
    feed(&mut app, &w, effects);
    let text = screen(&app);
    assert!(text.contains("not saved"), "{text}");
    assert_eq!(w.read(), "region = \n", "nothing was written over it");
    // The screen now shows the file it found: read-only.
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
}

// ---- e: the editor -----------------------------------------------------------------------------------------------------------------------

#[test]
fn e_hands_the_terminal_to_the_editor_and_reads_the_file_again_when_it_comes_back() {
    let w = World::new("edit", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    let argv = vec!["config".to_string(), "edit".to_string()];
    let effects = step(&mut app, &mut clock, KeyCode::Char('e'));
    assert_eq!(effects, vec![Effect::RunTerminal { argv: argv.clone() }]);
    // The person adds a line in the editor; the child's answer brings the screen back to the file.
    w.put(&format!("{OWNER}mouse = false\n"));
    let out = CmdOutput {
        chunks: vec![Chunk {
            stream: Stream::Out,
            text: "/x/config.toml: ok\n".into(),
        }],
        code: 0,
        bearer: None,
    };
    let effects = app.update(Msg::CommandDone { argv, out });
    assert_eq!(effects, vec![Effect::Settings(Job::Load)]);
    assert_eq!(
        app.screen(),
        Screen::Settings,
        "the panel of a command is not for this screen"
    );
    feed(&mut app, &w, effects);
    let row = row_of(&app, "Mouse");
    assert!(row.contains("off") && row.contains("file"), "{row}");
    assert!(
        screen(&app).contains("ok"),
        "the check of the child is shown:\n{}",
        screen(&app)
    );
}

// ---- the interface rows take effect now -----------------------------------------------------------------------------------------------

fn waiting_run(waiting: bool) -> RunSummary {
    let mut v = json!({"runId": "01K6A0000000000000000000A1", "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"});
    if waiting {
        v["needsInput"] = json!({"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop", "title": "Cline stopped",
                                 "expiresAt": "2026-10-14T20:30:00Z"});
    }
    serde_json::from_value(v).unwrap()
}

fn rings(app: &mut App) -> usize {
    app.update(Msg::Runs(Ok(vec![waiting_run(false)])));
    let effects = app.update(Msg::Runs(Ok(vec![waiting_run(true)])));
    effects.iter().filter(|e| **e == Effect::Bell).count()
}

#[test]
fn ui_bell_false_turns_the_bell_off_and_notify_follows_env_over_file() {
    // TRIGGER PRESENT: a Runs list whose run starts to wait rings once while the bell is on.
    let w = World::new("bell", Some(OWNER));
    let (mut on, _) = open(&w);
    assert_eq!(rings(&mut on), 1, "the default rings");
    println!("SETTINGS TRIGGER PRESENT: the default bell rang once");

    // The Bell row turned off in the screen: the very next list that starts to wait does not ring (it takes effect now).
    let w = World::new("bell2", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Bell");
    assert!(row_of(&app, "Bell").contains("now"));
    step(&mut app, &mut clock, KeyCode::Enter);
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::UiBell, "false")]);
    feed(&mut app, &w, effects);
    assert_eq!(rings(&mut app), 0, "the bell is off");
    // u brings the default back, and the bell with it.
    let effects = step(&mut app, &mut clock, KeyCode::Char('u'));
    feed(&mut app, &w, effects);
    assert_eq!(rings(&mut app), 1, "the default is back");

    // HAMH_NOTIFY wins over the file: the row says so, a write to the file does not change what is in force, and the screen says that.
    let w = World::new(
        "notify",
        Some(&format!(
            "{OWNER}configVersion = 1\n\n[ui]\nnotify = false\n"
        )),
    )
    .var("HAMH_NOTIFY", "1");
    let (mut app, mut clock) = open(&w);
    let row = row_of(&app, "Notifications");
    assert!(
        row.contains("on") && row.contains("env HAMH_NOTIFY"),
        "{row}"
    );
    goto(&mut app, &mut clock, "Notifications");
    // The file already says off, and "the opposite of what is in force" is off: no write, and the notice names the variable.
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app).contains("HAMH_NOTIFY"), "{}", screen(&app));
    // The same file with the variable unset: the file value applies.
    let w2 = World::new(
        "notify2",
        Some(&format!(
            "{OWNER}configVersion = 1\n\n[ui]\nnotify = true\n"
        )),
    );
    let (app2, _) = open(&w2);
    let row = row_of(&app2, "Notifications");
    assert!(row.contains("on") && row.contains("file"), "{row}");
}

// ---- small terminals ------------------------------------------------------------------------------------------------------------------

#[test]
fn no_terminal_size_and_no_state_of_the_screen_draws_outside_the_buffer() {
    // The screen in each of its states, drawn on terminals from one cell up: the bands that do not fit are left out and nothing panics.
    let w = World::new("tiny", Some(OWNER));
    let broken = World::new("tiny-broken", Some("region = \n"));
    let mut states: Vec<(&str, App)> = Vec::new();
    {
        let mut app = new_app();
        let mut clock = Clock::new();
        press(&mut app, &mut clock, KeyCode::Char('s'), 100);
        states.push(("reading", app));
    }
    let (app, _) = open(&w);
    states.push(("list", app));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Locked by the harness");
    step(&mut app, &mut clock, KeyCode::Enter);
    states.push(("locked list", app));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Download folder");
    step(&mut app, &mut clock, KeyCode::Enter);
    type_in(
        &mut app,
        &mut clock,
        "an unusually long folder name that does not fit a small terminal at all",
    );
    states.push(("text box", app));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Region");
    step(&mut app, &mut clock, KeyCode::Enter);
    type_in(&mut app, &mut clock, "x");
    step(&mut app, &mut clock, KeyCode::Enter);
    states.push(("preview with a name to type", app));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Model");
    step(&mut app, &mut clock, KeyCode::Enter);
    states.push(("model list", app));
    let (app, _) = open(&broken);
    states.push(("read-only", app));
    for (name, app) in &states {
        for (cols, rows) in [
            (1u16, 1u16),
            (2, 2),
            (10, 3),
            (20, 4),
            (30, 5),
            (40, 10),
            (79, 8),
            (80, 24),
            (200, 60),
        ] {
            let text = screen_text(app, cols, rows);
            assert_eq!(
                text.lines().count(),
                usize::from(rows),
                "{name} at {cols}x{rows}"
            );
            // Where the key line fits (a footer of 70 columns on a screen of at least 8 rows) it is the last row, whatever is open.
            if cols >= 79 && rows >= 8 {
                let last = text.lines().last().unwrap_or_default();
                assert!(
                    last.contains("Esc"),
                    "{name} at {cols}x{rows}: the last row is {last:?}\n{text}"
                );
            }
        }
    }
}

// ---- the mark, the wheel, a paste and Ctrl-C ------------------------------------------------------------------------------------------

#[test]
fn the_mark_moves_with_the_arrow_keys_home_end_and_the_wheel() {
    let w = World::new("marks", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    assert!(marked(&app).starts_with("▶ Model"), "{}", marked(&app));
    step(&mut app, &mut clock, KeyCode::Down);
    assert!(
        marked(&app).starts_with("▶ Answer wait"),
        "{}",
        marked(&app)
    );
    step(&mut app, &mut clock, KeyCode::Char('j'));
    assert!(
        marked(&app).starts_with("▶ Test report"),
        "{}",
        marked(&app)
    );
    step(&mut app, &mut clock, KeyCode::Char('k'));
    step(&mut app, &mut clock, KeyCode::Up);
    assert!(marked(&app).starts_with("▶ Model"), "{}", marked(&app));
    // The mark stops at the first line; End goes to the locked line and the keys stop there.
    step(&mut app, &mut clock, KeyCode::Up);
    assert!(marked(&app).starts_with("▶ Model"), "{}", marked(&app));
    step(&mut app, &mut clock, KeyCode::End);
    assert!(
        marked(&app).starts_with("▶ Locked by the harness"),
        "{}",
        marked(&app)
    );
    step(&mut app, &mut clock, KeyCode::Down);
    assert!(
        marked(&app).starts_with("▶ Locked by the harness"),
        "{}",
        marked(&app)
    );
    step(&mut app, &mut clock, KeyCode::Home);
    assert!(marked(&app).starts_with("▶ Model"), "{}", marked(&app));
    step(&mut app, &mut clock, KeyCode::PageDown);
    assert!(
        // Ten lines down: the five rows of the engine's settings sit between the run rows and the interface rows, so it is no longer the
        // mascot row. The run rows are eleven since the data sharing setting and the default engine joined the lifecycle flow
        // and the test command limit, so it is the last of them, Engine, and not the first engine row, Reasoning effort.
        marked(&app).starts_with("▶ Engine") && !marked(&app).starts_with("▶ Reasoning effort"),
        "ten lines down: {}",
        marked(&app)
    );
    // The wheel moves the mark one line a notch.
    step(&mut app, &mut clock, KeyCode::Home);
    app.handle_wheel(true);
    assert!(
        marked(&app).starts_with("▶ Answer wait"),
        "{}",
        marked(&app)
    );
    app.handle_wheel(false);
    assert!(marked(&app).starts_with("▶ Model"), "{}", marked(&app));
    assert_eq!(w.read(), OWNER);
}

#[test]
fn a_paste_goes_into_the_open_box_and_ctrl_c_empties_it_before_it_quits() {
    let w = World::new("paste", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    // On the list a paste says the screen has no text box, and writes nothing.
    assert!(app.handle_paste("hello".into()).is_empty());
    assert!(screen(&app).contains("paste ignored"), "{}", screen(&app));
    goto(&mut app, &mut clock, "Default repository");
    step(&mut app, &mut clock, KeyCode::Enter);
    // In the box the paste is one line: a line break becomes a blank.
    assert!(app.handle_paste("cc:my\nservice".into()).is_empty());
    assert!(screen(&app).contains("cc:my service"), "{}", screen(&app));
    // Ctrl-C empties the box and leaves it open; with the box empty the next one is the app's quit.
    assert!(chat_support::ctrl(&mut app, &mut clock, 'c').is_empty());
    assert!(!screen(&app).contains("cc:my service"), "{}", screen(&app));
    assert_eq!(app.screen(), Screen::Settings);
    assert_eq!(
        chat_support::ctrl(&mut app, &mut clock, 'c'),
        vec![Effect::Quit]
    );
    assert_eq!(w.read(), OWNER);
}

// ---- what the running app reads, and a burst --------------------------------------------------------------------------------------------

#[test]
fn the_model_and_the_repository_written_in_the_screen_are_what_the_next_composer_opens_with() {
    let w = World::new("composer", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    // The model: chosen in the list, written as the default.
    goto(&mut app, &mut clock, "Model");
    step(&mut app, &mut clock, KeyCode::Enter);
    let list = ham_core::models::builtin();
    let choosable: Vec<_> = list.models().iter().filter(|m| m.selectable()).collect();
    // The list opens on the model in force (the built-in default); the one after it is the pick.
    let at = choosable.iter().position(|m| m.default).unwrap();
    let pick = choosable[at + 1];
    let mut guard = 0;
    while !marked(&app).contains(&pick.name) && guard < 20 {
        step(&mut app, &mut clock, KeyCode::Down);
        guard += 1;
    }
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::DefaultModel, &pick.id)]);
    feed(&mut app, &w, effects);
    // The repository: typed in the box.
    goto(&mut app, &mut clock, "Default repository");
    step(&mut app, &mut clock, KeyCode::Enter);
    type_in(&mut app, &mut clock, "cc:my-service");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::DefaultRepo, "cc:my-service")]);
    feed(&mut app, &w, effects);
    // The next New run form opens on both.
    step(&mut app, &mut clock, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    step(&mut app, &mut clock, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    let text = screen(&app);
    assert!(text.contains(&pick.name), "the composer's model:\n{text}");
    assert!(
        text.contains("cc:my-service"),
        "the composer's repository:\n{text}"
    );
    assert_eq!(app.chosen_model(), Some(pick.id.as_str()));
}

#[test]
fn an_enter_that_is_part_of_a_burst_writes_and_confirms_nothing() {
    let w = World::new("burst", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Test report");
    // Keys closer than the burst gap are a paste that arrived as keys: its Enter is not a decision.
    press(&mut app, &mut clock, KeyCode::Char('x'), 400);
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 5);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(w.read(), OWNER);
    // The same key at a person's pace writes.
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::ReportTests, "false")]);
    // A preview waits for a calm Enter too: a burst Enter neither confirms it nor withdraws it.
    feed(&mut app, &w, effects);
    goto(&mut app, &mut clock, "Goal loop");
    step(&mut app, &mut clock, KeyCode::Enter);
    press(&mut app, &mut clock, KeyCode::Char('x'), 400);
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 5);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        screen(&app).contains("This adds the first table"),
        "{}",
        screen(&app)
    );
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert_eq!(effects, vec![set(Key::RunGoal, "false")]);
}

#[test]
fn a_repository_the_person_typed_in_the_new_run_form_stays_when_the_default_changes() {
    let w = World::new("repo-typed", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    // The form is opened and the repository typed there before the default is written.
    step(&mut app, &mut clock, KeyCode::Esc);
    step(&mut app, &mut clock, KeyCode::Char('n'));
    step(&mut app, &mut clock, KeyCode::Tab);
    step(&mut app, &mut clock, KeyCode::Tab);
    type_in(&mut app, &mut clock, "cc:typed-here");
    step(&mut app, &mut clock, KeyCode::Esc);
    let effects = step(&mut app, &mut clock, KeyCode::Char('s'));
    feed(&mut app, &w, effects);
    goto(&mut app, &mut clock, "Default repository");
    step(&mut app, &mut clock, KeyCode::Enter);
    type_in(&mut app, &mut clock, "cc:other");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    feed(&mut app, &w, effects);
    step(&mut app, &mut clock, KeyCode::Esc);
    step(&mut app, &mut clock, KeyCode::Char('n'));
    assert!(screen(&app).contains("cc:typed-here"), "{}", screen(&app));
    assert!(!screen(&app).contains("cc:other"), "{}", screen(&app));
}
