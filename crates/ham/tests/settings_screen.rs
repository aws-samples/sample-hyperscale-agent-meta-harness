//! The Cline rows of the Settings screen: between the run rows and the interface rows the
//! screen draws the group of the selected engine, one row for each key of the engine's table (`harness_protocol::direct::CLINE_KEYS`) with the
//! words of `ham_core::engines`. Enter writes the explicit next value of a choice or a list (a level is never skipped, `off` and `xhigh` are offered
//! because the key table does not have it), opens a one-line box for a number; `u` removes the key; `e` opens the engine file. Every write goes
//! through the comment-keeping patcher and the shared check (`ham::engine_cmd`), so the file keeps its comments and a value out of range never
//! reaches it. The screen's "Locked by the harness" texts are the protocol's denied table.
//!
//! The screen holds no file: `settings::perform` is the one place that reads or writes, and these tests play the event loop by handing each
//! `Effect::Settings` to it and the answer back to the app.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{Clock, press};
use crossterm::event::KeyCode;
use ham::tui::settings::{self, Job};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use ham_core::settings::{LOCKED, ROWS};
use harness_protocol::direct::{CLINE_KEYS, DENIED, KeyKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The owner's file: three connection keys, two comment lines and a comment after a value.
const OWNER: &str = "# ham settings\n# keep this note\napiEndpoint = \"https://abc.execute-api.us-east-1.amazonaws.com/direct/v1\"\nregion = \"us-east-1\"\nawsProfile = \"ham-test\" # the profile\n";

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

    fn file(&self) -> PathBuf {
        self.root.join("cfg/ham/config.toml")
    }

    fn read(&self) -> String {
        std::fs::read_to_string(self.file()).unwrap_or_default()
    }

    /// The engine file of the Cline engine.
    fn engine_file(&self) -> PathBuf {
        self.root.join("cfg/ham/engines/cline.toml")
    }

    fn read_engine(&self) -> String {
        std::fs::read_to_string(self.engine_file()).unwrap_or_default()
    }

    fn put_engine(&self, text: &str) {
        std::fs::create_dir_all(self.root.join("cfg/ham/engines")).unwrap();
        std::fs::write(self.engine_file(), text).unwrap();
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

/// The height of the window that holds the whole list of the Settings screen.
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

const CLINE_ROWS: [&str; 5] = [
    "Reasoning effort",
    "Command time limit",
    "Compaction",
    "Compaction trigger",
    "Tools switched off",
];

/// The value column of a row: the text after the label up to the next wide gap.
fn value_of(app: &App, label: &str) -> String {
    let line = row_of(app, label);
    let rest = line
        .trim_start()
        .trim_start_matches('▶')
        .trim_start()
        .strip_prefix(label)
        .unwrap()
        .trim_start();
    rest.split("  ").next().unwrap().trim().to_string()
}

#[test]
fn the_screen_draws_the_engine_group_between_the_run_rows_and_the_interface_rows() {
    let w = World::new("eng-group", Some(OWNER));
    let (app, _) = open(&w);
    let text = screen(&app);
    let lines: Vec<&str> = text.lines().collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|l| l.trim() == needle)
            .unwrap_or_else(|| panic!("no line {needle:?}:\n{text}"))
    };
    let (run, group, interface) = (at("Run"), at("Cline engine"), at("Interface"));
    assert!(run < group && group < interface, "{text}");
    for label in CLINE_ROWS {
        let row = lines
            .iter()
            .position(|l| row_label(l).as_deref() == Some(label))
            .unwrap_or_else(|| panic!("no row {label}:\n{text}"));
        assert!(
            group < row && row < interface,
            "{label} is not in the engine group:\n{text}"
        );
    }
    // The values in force while no file sets them, each with its origin and when a change applies.
    assert_eq!(value_of(&app, "Reasoning effort"), "model default");
    assert_eq!(value_of(&app, "Command time limit"), "600 s");
    assert_eq!(value_of(&app, "Compaction"), "agentic");
    assert_eq!(value_of(&app, "Compaction trigger"), "0.90");
    assert_eq!(value_of(&app, "Tools switched off"), "none");
    let row = row_of(&app, "Reasoning effort");
    assert!(row.contains("default") && row.contains("next run"), "{row}");
    // Reading changes nothing and makes no folder.
    assert_eq!(w.read(), OWNER);
    assert!(!w.root.join("cfg/ham/engines").exists());
}

#[test]
fn the_cline_rows_toggle_and_cycle_through_the_patcher() {
    let w = World::new("eng-cycle", Some(OWNER));
    w.put_engine("# my engine note\nschema = 1\n");
    let (mut app, mut clock) = open(&w);

    // A choice: Enter writes the next level; the cycle is the table's levels, `off` and `xhigh` included.
    goto(&mut app, &mut clock, "Reasoning effort");
    let levels: Vec<&str> = match CLINE_KEYS
        .iter()
        .find(|k| k.path == "reasoning.effort")
        .unwrap()
        .kind
    {
        KeyKind::Choice(set) => set.to_vec(),
        other => panic!("{other:?}"),
    };
    assert_eq!(levels, ["default", "off", "low", "medium", "high", "xhigh"]);
    let mut seen = Vec::new();
    for _ in 0..7 {
        let effects = step(&mut app, &mut clock, KeyCode::Enter);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Settings(Job::ApplyEngine { .. })]
            ),
            "{effects:?}"
        );
        let rest = feed(&mut app, &w, effects);
        assert!(rest.is_empty(), "{rest:?}");
        let file = w.read_engine();
        let level = file
            .lines()
            .find_map(|l| l.strip_prefix("effort = "))
            .map(|v| v.trim_matches('"').to_string());
        seen.push(level.unwrap());
        assert!(
            file.starts_with("# my engine note\nschema = 1\n"),
            "the comment stays:\n{file}"
        );
    }
    assert_eq!(
        seen,
        ["off", "low", "medium", "high", "xhigh", "default", "off"]
    );
    let row = row_of(&app, "Reasoning effort");
    assert!(row.contains("off") && row.contains("file"), "{row}");

    // `u` removes the key: the row is back to its default and the file is as it was before the first write.
    let effects = step(&mut app, &mut clock, KeyCode::Char('u'));
    feed(&mut app, &w, effects);
    assert_eq!(value_of(&app, "Reasoning effort"), "model default");
    assert!(!w.read_engine().contains("effort"), "{}", w.read_engine());
    assert!(
        w.read_engine()
            .starts_with("# my engine note\nschema = 1\n")
    );

    // Another choice: agentic, basic, off.
    goto(&mut app, &mut clock, "Compaction");
    let mut seen = Vec::new();
    for _ in 0..4 {
        let effects = step(&mut app, &mut clock, KeyCode::Enter);
        feed(&mut app, &w, effects);
        let file = w.read_engine();
        seen.push(
            file.lines()
                .find_map(|l| l.strip_prefix("compaction = "))
                .unwrap()
                .trim_matches('"')
                .to_string(),
        );
    }
    assert_eq!(seen, ["basic", "off", "agentic", "basic"]);

    // A list: the four subsets of the two tools, in order, back to none.
    goto(&mut app, &mut clock, "Tools switched off");
    let mut seen = Vec::new();
    for _ in 0..5 {
        let effects = step(&mut app, &mut clock, KeyCode::Enter);
        feed(&mut app, &w, effects);
        seen.push(
            w.read_engine()
                .lines()
                .find_map(|l| l.strip_prefix("disable = "))
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(
        seen,
        [
            "[\"spawn_agent\"]",
            "[\"fetch_web_content\"]",
            "[\"spawn_agent\", \"fetch_web_content\"]",
            "[]",
            "[\"spawn_agent\"]"
        ]
    );
    // The writes of the engine rows never touch config.toml.
    assert_eq!(w.read(), OWNER);
    // And the file the screen wrote is one the check accepts.
    let r = ham_core::engine_file::read_text(&w.read_engine(), "cline");
    assert!(r.ok(), "{:?}\n{}", r.problems, w.read_engine());
}

#[test]
fn a_number_row_opens_a_box_and_a_value_out_of_range_stays_open_with_the_reason() {
    let w = World::new("eng-number", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    goto(&mut app, &mut clock, "Command time limit");
    assert!(
        step(&mut app, &mut clock, KeyCode::Enter).is_empty(),
        "Enter opens the box"
    );
    type_in(&mut app, &mut clock, "1801");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert!(
        effects.is_empty(),
        "a value out of range is not sent: {effects:?}"
    );
    let text = screen(&app);
    assert!(text.contains("a whole number from 1 to 1800"), "{text}");
    assert!(!w.engine_file().exists(), "nothing was written");
    // The box is still open: clear it and type a good value.
    for _ in 0..4 {
        step(&mut app, &mut clock, KeyCode::Backspace);
    }
    type_in(&mut app, &mut clock, "900");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    let rest = feed(&mut app, &w, effects);
    assert!(rest.is_empty(), "{rest:?}");
    assert_eq!(
        w.read_engine(),
        "schema = 1\n\n[commands]\ntimeout_seconds = 900\n"
    );
    assert_eq!(value_of(&app, "Command time limit"), "900 s");
    // The trigger takes a number with a fraction.
    goto(&mut app, &mut clock, "Compaction trigger");
    step(&mut app, &mut clock, KeyCode::Enter);
    type_in(&mut app, &mut clock, "0.8");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    feed(&mut app, &w, effects);
    assert!(
        w.read_engine().contains("compaction_trigger = 0.8\n"),
        "{}",
        w.read_engine()
    );
    assert_eq!(value_of(&app, "Compaction trigger"), "0.80");
}

#[test]
fn an_unreadable_engine_file_opens_its_rows_read_only_with_the_finding_and_e_opens_the_file() {
    let w = World::new("eng-unreadable", Some(OWNER));
    let bad = "schema = 1\n[reasoning\n";
    w.put_engine(bad);
    let (mut app, mut clock) = open(&w);
    let text = screen(&app);
    assert!(
        text.contains("engines/cline.toml") && text.contains("line 2"),
        "{text}"
    );
    goto(&mut app, &mut clock, "Reasoning effort");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(w.read_engine(), bad);
    // `e` on an engine row opens the engine file, not config.toml.
    let effects = step(&mut app, &mut clock, KeyCode::Char('e'));
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: vec!["engine".into(), "cline".into(), "edit".into()]
        }]
    );
    // On a row of config.toml it is still `ham config edit`.
    goto(&mut app, &mut clock, "Goal loop");
    let effects = step(&mut app, &mut clock, KeyCode::Char('e'));
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: vec!["config".into(), "edit".into()]
        }]
    );
}

#[test]
fn a_denied_key_in_the_engine_file_shows_as_a_finding_and_the_rows_stay_usable_for_unset() {
    let w = World::new("eng-denied", Some(OWNER));
    w.put_engine("schema = 1\n[aws]\nprofile = \"x\"\n[reasoning]\neffort = \"low\"\n");
    let (app, _) = open(&w);
    let text = screen(&app);
    assert!(
        text.contains("aws.profile") && text.contains("line 3"),
        "{text}"
    );
    // The file is read for what it does set; the refused key is the finding, not a crash.
    assert_eq!(value_of(&app, "Reasoning effort"), "low");
}

#[test]
fn the_locked_line_text_equals_the_harness_protocol_table() {
    let w = World::new("eng-locked", Some(OWNER));
    let (mut app, mut clock) = open(&w);
    let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = flat(&row_of(&app, "Locked by the harness"));
    assert_eq!(DENIED.len(), 7);
    for class in DENIED {
        assert!(
            line.contains(class.label),
            "{} is not in the locked line:\n{line}",
            class.label
        );
    }
    goto(&mut app, &mut clock, "Locked by the harness");
    assert!(step(&mut app, &mut clock, KeyCode::Enter).is_empty());
    let open_list = flat(&screen_text(&app, 200, 60));
    for class in DENIED {
        assert!(
            open_list.contains(&flat(class.reason)),
            "the reason of {} is not on the screen:\n{open_list}",
            class.id
        );
    }
    // The client's table is the protocol's, class for class.
    for (client, table) in LOCKED.iter().zip(DENIED) {
        assert_eq!((client.label, client.reason), (table.label, table.reason));
    }
    // The screen's registry rows are untouched by the new group.
    assert_eq!(ROWS.len(), 46);
}

// ---- the OpenCode group -------------------------------------------------------------------------------------------------------

#[test]
fn b800_the_screen_of_opencode_has_one_engine_row_and_writes_the_file_of_that_engine() {
    let config = format!("{OWNER}default_engine = \"opencode\"\n");
    let w = World::new("b800-opencode", Some(&config));
    let (mut app, mut clock) = open(&w);
    let text = screen(&app);
    assert!(
        text.lines().any(|l| l.trim() == "OpenCode engine"),
        "{text}"
    );
    assert!(!text.lines().any(|l| l.trim() == "Cline engine"), "{text}");
    // One row, the command time limit; the other four rows of the Cline group are not settings of this engine.
    for label in [
        "Reasoning effort",
        "Compaction",
        "Compaction trigger",
        "Tools switched off",
    ] {
        assert!(
            !text.lines().any(|l| row_label(l).as_deref() == Some(label)),
            "{label} is a row of the OpenCode group:\n{text}"
        );
    }
    assert_eq!(value_of(&app, "Command time limit"), "600 s");
    let row = row_of(&app, "Command time limit");
    assert!(row.contains("default") && row.contains("next run"), "{row}");

    // A value out of range stays in the box with the reason; a good one is written to the file of this engine only.
    goto(&mut app, &mut clock, "Command time limit");
    assert!(
        step(&mut app, &mut clock, KeyCode::Enter).is_empty(),
        "Enter opens the box"
    );
    type_in(&mut app, &mut clock, "1801");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    assert!(
        effects.is_empty(),
        "a value out of range is not sent: {effects:?}"
    );
    assert!(
        screen(&app).contains("a whole number from 1 to 1800"),
        "{}",
        screen(&app)
    );
    let engines = w.root.join("cfg/ham/engines");
    assert!(!engines.exists(), "nothing was written");
    for _ in 0..4 {
        step(&mut app, &mut clock, KeyCode::Backspace);
    }
    type_in(&mut app, &mut clock, "900");
    let effects = step(&mut app, &mut clock, KeyCode::Enter);
    let rest = feed(&mut app, &w, effects);
    assert!(rest.is_empty(), "{rest:?}");
    assert_eq!(
        std::fs::read_to_string(engines.join("opencode.toml")).unwrap(),
        "schema = 1\n\n[commands]\ntimeout_seconds = 900\n"
    );
    assert!(
        !w.engine_file().exists(),
        "the file of the other engine was written"
    );
    assert_eq!(value_of(&app, "Command time limit"), "900 s");
}
