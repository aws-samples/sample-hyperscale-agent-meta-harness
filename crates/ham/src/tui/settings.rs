//! The Settings screen: `s` on the Runs list and `/settings` in the message box open
//! it. It shows every setting of the registry (`ham_core::settings::ROWS`) with the value in force, where the value came from and when a change
//! takes effect, and the line of the classes the harness locks. The screen holds no file and reads no environment: [`perform`] is the one place
//! that does, and the event loop (or a test) hands it each [`Job`] and gives the [`Done`] back to the app. A screen row is never drawn by hand:
//! the groups, labels, help, defaults, flags, variables and the kind of value all come from the registry, and every change goes through the
//! comment-keeping writer of `ham config` (`config_cmd::apply_set` and `apply_unset`), so a file keeps its comments and its order.
//!
//! - **Enter** writes the explicit opposite of the value in force for a switch (a switch that shows `stage default` is turned off), cycles the
//!   mouse setting (on, off, auto), opens a one-line box for a number or a text, and opens the model list for the model. **`u`** removes the
//!   key from the file, so the default is back and a later change of the default reaches the user. **`e`** hands the terminal to the editor
//!   (`ham config edit` as a child, the way `/rules edit` runs) and reads the file again when it comes back.
//! - **Previews.** The first write that adds a table to the file shows the one-way-door note and waits for a second Enter. The six keys that
//!   decide where requests go, who signs them, where files land, which backend takes a start that names none or which backend may take
//!   customer content (`SECURITY_KEYS`) keep the second Enter with the key name typed: the preview
//!   and the confirmation are the router's (`prepare_argv`, `Confirmation`), the ones the message box uses for the same `config set`.
//! - **A file that cannot be read as TOML** opens the screen read-only with the findings; `e` is still offered, because that is how it is
//!   repaired. A file with a bad key is written around and shows its findings. A file written by a newer `ham` is read and refused for writing.
//! - **Takes effect now.** The interface rows (`ui.notify`, `ui.bell`) are applied to the running app as soon as the write is done; the rows of
//!   a run apply to the next run, the connection keys after a restart. The row says which.
//!
//! The engine rows: between the run rows and the interface rows the screen draws the group of the selected engine, one row for each key of its
//! table. They are read from the engine file (`ham_core::engine_file`, one read per load) and written through the same comment-keeping patcher and
//! shared check as `ham engine <name> set` (`crate::engine_cmd`): Enter writes the next level of a choice, the next subset of a list, or opens a
//! one-line box for a number; `u` removes the key; `e` opens the engine file. A value that the check would refuse never reaches the file. The module
//! names no engine: the words come from `ham_core::engines`.

use super::app::{Effect, Screen};
use super::mouse::{HitMap, Target};
use crate::config_cmd::{self, Effective, Loaded};
use crate::engine_cmd;
use crate::output::CmdOutput;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham_core::config::{
    Config, MascotMode, bell_on, get_key, mascot_mode, mascot_motion_on, notify_on,
};
use ham_core::config_write::{door_note, plan_set, plan_unset};
use ham_core::engines;
use ham_core::models::Models;
use ham_core::router::{
    ConfirmKey, ConfirmStep, Confirmation, Preview, RouteOutcome, Wait, prepare_argv,
};
use ham_core::sanitize::sanitize_line;
use ham_core::settings::{DefaultValue, Group, Key, Kind, LOCKED, Row, Severity};
use harness_protocol::direct::{KeyKind, keys_for};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::cell::{Cell, RefCell};

/// Where the environment is read from: the process's, in the app; a test's own.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What a write does to a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// `ham config set <key> <value>`.
    Set(String),
    /// `ham config unset <key>`.
    Unset,
}

/// The work of the screen, done by [`perform`] off the keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    /// Read the file and the environment.
    Load,
    /// Change one key in the file, then read it again.
    Apply { key: Key, op: Op },
    /// Change one key of the selected engine's file (`path` is its dotted name in the engine's table), then read both files again.
    ApplyEngine { path: &'static str, op: Op },
}

/// What a write did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// False for a no-op: the file was not written.
    pub wrote: bool,
}

/// The answer to a [`Job`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Done {
    Loaded(Result<Snapshot, String>),
    Applied {
        key: Key,
        op: Op,
        result: Result<Written, String>,
        /// The file as it is now.
        snapshot: Result<Snapshot, String>,
    },
    AppliedEngine {
        path: &'static str,
        op: Op,
        result: Result<Written, String>,
        snapshot: Result<Snapshot, String>,
    },
}

/// A setting as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    pub key: Key,
    /// The value in force, worded for the screen (`on`, `off`, `45 min`, a model's name, `stage default`).
    pub shown: String,
    /// The value in force as the registry holds it (`true`, a full model id).
    pub raw: String,
    /// `file`, `default`, `env HAMH_NOTIFY`.
    pub origin: String,
    pub applies: &'static str,
    /// Some layer above the default sets it.
    pub set: bool,
    /// The file's own value, as `ham config get` prints it.
    pub in_file: Option<String>,
    /// The switch in force, for a switch; a switch with no default of its own counts as on (the stage's default).
    pub on: Option<bool>,
}

/// The file and the environment as the screen needs them, read by [`perform`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub exists: bool,
    /// The text of the file (empty when there is none or it could not be read as TOML).
    pub text: String,
    /// Why the file cannot be used as TOML at all, one finding per line: the screen is read-only then.
    pub unusable: Option<String>,
    /// A newer `ham` wrote the file: nothing in it may be changed.
    pub newer: bool,
    /// What the reader found wrong or odd, one line each with its line number.
    pub findings: Vec<String>,
    /// One row for each key of the registry, in registry order.
    pub rows: Vec<RowView>,
    /// The terminal notification and the bell as they stand now.
    pub notify: bool,
    pub bell: bool,
    /// The mascot setting in force (`ui.mascot`, with `HAMH_NO_MASCOT` over it).
    pub mascot: MascotMode,
    /// Whether the smoke under the mascot's feet moves (`ui.mascot_motion`).
    pub mascot_motion: bool,
    /// The file's `default_model` and `default_repo`, which the composer reads.
    pub default_model: Option<String>,
    pub default_repo: Option<String>,
    /// The selected engine's settings file (`None` until [`snapshot`] has read it).
    pub engine: Option<EngineSnapshot>,
}

/// One row of the engine's settings as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineRowView {
    /// Where the key is in the engine's table (`keys_for`).
    pub index: usize,
    /// The dotted name of the key in the engine file.
    pub path: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    /// The value in force worded for the screen (`high`, `900 s`, `model default`).
    pub shown: String,
    /// The value in force as the table holds it (`high`, `900`, the default when the file sets nothing; a list is joined with commas).
    pub raw: String,
    /// `file` or `default`.
    pub origin: &'static str,
    pub applies: &'static str,
    /// The file sets the key.
    pub set: bool,
}

/// The engine settings file as the screen needs it, read once by [`snapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineSnapshot {
    pub engine: &'static str,
    pub exists: bool,
    /// The text of the file (empty when there is none or it cannot be used as TOML).
    pub text: String,
    /// Why the file cannot be changed at all (not TOML, too large, written for a newer `ham`, not a regular file), one finding per line: the rows
    /// are read-only then.
    pub unusable: Option<String>,
    /// What the check found wrong, one line each with its line number.
    pub findings: Vec<String>,
    pub rows: Vec<EngineRowView>,
}

impl EngineSnapshot {
    /// The screen's view of the engine file of `engine` under `env`.
    pub fn of(env: Env<'_>, engine: &'static str) -> EngineSnapshot {
        let shown = ham_core::engine_file::shown_path(engine);
        let (file, load_error) = match engine_cmd::load(env, engine) {
            Ok(f) => (Some(f), None),
            Err(e) => (None, Some(sanitize_line(&e))),
        };
        let reading = file.as_ref().map(|f| f.read(engine));
        let unusable = load_error.or_else(|| {
            let why: Vec<String> = reading
                .iter()
                .flat_map(|r| &r.problems)
                .filter(|p| {
                    matches!(
                        p.code.as_str(),
                        "syntax"
                            | "nul"
                            | "bom"
                            | "not_utf8"
                            | "too_large"
                            | "too_deep"
                            | "newer_schema"
                    )
                })
                .map(|p| p.render(&shown))
                .collect();
            (!why.is_empty()).then(|| why.join("\n"))
        });
        let findings = reading
            .iter()
            .flat_map(|r| &r.problems)
            .map(|p| sanitize_line(&format!("error: {}", p.render(&shown))))
            .collect();
        let rows = keys_for(engine)
            .unwrap_or(&[])
            .iter()
            .zip(engines::setting_texts(engine))
            .enumerate()
            .map(|(index, (row, text))| {
                let value = reading.as_ref().and_then(|r| r.value(row.path));
                let raw = value.map_or_else(
                    || row.default.to_string(),
                    ham_core::engine_file::value_text,
                );
                EngineRowView {
                    index,
                    path: row.path,
                    label: text.label,
                    help: text.help,
                    shown: match value {
                        Some(_) => sanitize_line(&engines::shown_value(row.path, &raw)),
                        None => text.default_shown.to_string(),
                    },
                    raw,
                    origin: if value.is_some() { "file" } else { "default" },
                    applies: "next run",
                    set: value.is_some(),
                }
            })
            .collect();
        EngineSnapshot {
            engine,
            exists: file
                .as_ref()
                .is_some_and(ham_core::engine_file::EngineFile::exists),
            text: file
                .as_ref()
                .map(|f| f.text().to_string())
                .unwrap_or_default(),
            unusable,
            findings,
            rows,
        }
    }

    fn read_only(&self) -> bool {
        self.unusable.is_some()
    }
}

impl Snapshot {
    /// The screen's view of `loaded` under `env`.
    pub fn of(loaded: &Loaded, env: Env<'_>, models: &Models) -> Snapshot {
        let cfg: &Config = &loaded.read.config;
        let unusable = loaded.refusal().map(|why| sanitize_line(&why));
        let findings = loaded
            .read
            .findings
            .iter()
            .map(|f| {
                let label = if f.severity == Severity::Error {
                    "error"
                } else {
                    "warning"
                };
                sanitize_line(&format!("{label}: {}", f.render()))
            })
            .collect();
        Snapshot {
            exists: loaded.exists(),
            text: loaded.text().to_string(),
            unusable,
            newer: loaded.read.writes_blocked(),
            findings,
            rows: Key::ALL
                .iter()
                .map(|k| RowView::of(*k, cfg, env, models))
                .collect(),
            notify: notify_on(env, cfg),
            bell: bell_on(cfg),
            mascot: mascot_mode(env, cfg),
            mascot_motion: mascot_motion_on(cfg),
            default_model: cfg.default_model.clone(),
            default_repo: cfg.default_repo.clone(),
            engine: None,
        }
    }

    fn row(&self, key: Key) -> Option<&RowView> {
        self.rows.iter().find(|r| r.key == key)
    }

    /// No write can be made: the file cannot be read as TOML.
    fn read_only(&self) -> bool {
        self.unusable.is_some()
    }
}

impl RowView {
    fn of(key: Key, cfg: &Config, env: Env<'_>, models: &Models) -> RowView {
        let row = key.row();
        let e = config_cmd::effective(cfg, env, key);
        let in_file = get_key(cfg, row.name).ok().flatten();
        RowView {
            key,
            shown: shown_value(row, &e, models),
            raw: raw_value(row, &e),
            origin: e.origin.label(),
            applies: e.applies.label(),
            set: e.set,
            in_file,
            on: (row.kind == Kind::Bool)
                .then(|| bool_in_force(row, &e))
                .flatten(),
        }
    }
}

/// The switch in force: the value a layer set, else the registry's default. `None` for a switch the stage decides.
fn bool_in_force(row: &Row, e: &Effective) -> Option<bool> {
    if e.set {
        return e.value.parse::<bool>().ok();
    }
    match row.default {
        DefaultValue::Bool(b) => Some(b),
        _ => None,
    }
}

/// The value in force as the registry holds it: what a layer set, else the registry's own default (a full model id, `true`), else nothing.
fn raw_value(row: &Row, e: &Effective) -> String {
    if e.set {
        return e.value.clone();
    }
    match row.default {
        DefaultValue::Bool(b) => b.to_string(),
        DefaultValue::Minutes(m) | DefaultValue::Number(m) => m.to_string(),
        DefaultValue::Text(t) => t.to_string(),
        DefaultValue::Absent => String::new(),
    }
}

/// The value as the screen words it.
fn shown_value(row: &Row, e: &Effective, models: &Models) -> String {
    let text = if !e.set {
        e.value.clone()
    } else {
        match row.kind {
            Kind::Bool | Kind::Mouse => match e.value.as_str() {
                "true" => "on".to_string(),
                "false" => "off".to_string(),
                other => other.to_string(),
            },
            Kind::Model => models
                .find(&e.value)
                .map_or_else(|| e.value.clone(), |m| m.name.clone()),
            Kind::Minutes { .. } => format!("{} min", e.value),
            Kind::Number { .. } => row
                .kind
                .number_text(&e.value)
                .unwrap_or_else(|| e.value.clone()),
            Kind::Text | Kind::Mascot | Kind::Flow | Kind::Review | Kind::Compute => {
                e.value.clone()
            }
        }
    };
    sanitize_line(&text)
}

/// Reads the file and the environment. A file that cannot be read at all (no config location, not a regular file) is the error.
pub fn snapshot(env: Env<'_>) -> Result<Snapshot, String> {
    let loaded = config_cmd::load_file(env)?;
    let mut snap = Snapshot::of(&loaded, env, ham_core::models::builtin());
    snap.engine = Some(EngineSnapshot::of(env, engines::selected().id));
    Ok(snap)
}

/// Does one [`Job`] against the file `env` points at. The only place of the screen that reads or writes.
pub fn perform(env: Env<'_>, job: Job) -> Done {
    match job {
        Job::Load => Done::Loaded(snapshot(env)),
        Job::Apply { key, op } => {
            let name = key.row().name;
            let result = match &op {
                Op::Set(value) => config_cmd::apply_set(env, name, value),
                Op::Unset => config_cmd::apply_unset(env, name),
            }
            .map(|a| Written { wrote: a.wrote });
            Done::Applied {
                key,
                op,
                result,
                snapshot: snapshot(env),
            }
        }
        Job::ApplyEngine { path, op } => {
            let engine = engines::selected().id;
            let result = match &op {
                Op::Set(value) => engine_cmd::apply_set(env, engine, path, value),
                Op::Unset => engine_cmd::apply_unset(env, engine, path),
            }
            .map(|a| Written { wrote: a.wrote });
            Done::AppliedEngine {
                path,
                op,
                result,
                snapshot: snapshot(env),
            }
        }
    }
}

// ---- state ---------------------------------------------------------------------------------------------------------------------------

/// What the keys act on besides the state itself.
#[derive(Clone, Copy, Debug)]
pub struct Cx {
    /// The app's clock for key times, in milliseconds (the confirmation's 300 ms guard reads it).
    pub now_ms: u64,
    /// The key is calm: not part of a burst, so it may write or confirm.
    pub calm: bool,
}

/// What a key does to the screen's place in the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Stay,
    /// Esc on the list: back to the screen it was opened from.
    Back,
    /// `q`: the app's quit request.
    Quit,
}

/// A line of the list: a setting, or the line of the locked classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Row(usize),
    /// A row of the selected engine's settings.
    Engine(usize),
    Locked,
}

/// The groups in the order the screen draws them: the rows of a run first, the connection keys, which decide where requests go, last.
const GROUP_ORDER: [Group; 5] = [
    Group::Run,
    Group::Interface,
    Group::Paths,
    Group::Limits,
    Group::Connection,
];

/// What a one-line box changes: a key of `config.toml`, or a row of the engine's settings (its place in the snapshot's rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoxTarget {
    Client(Key),
    Engine(usize),
}

/// A one-line box for a number or a text.
#[derive(Clone, Debug)]
struct TextBox {
    target: BoxTarget,
    text: Vec<char>,
    at: usize,
    error: Option<String>,
}

/// The model list.
#[derive(Clone, Debug)]
struct Chooser {
    /// The models that can be chosen: ids and names.
    models: Vec<(String, String)>,
    /// The id of the model in force.
    current: String,
    cursor: usize,
}

/// A write waiting for its second Enter.
#[derive(Clone, Debug)]
struct Pending {
    key: Key,
    op: Op,
    text: String,
    expected: Option<String>,
    conf: Confirmation,
    hint: Option<String>,
}

#[derive(Clone, Debug)]
enum Mode {
    Browse,
    Text(TextBox),
    Choose(Chooser),
    Confirm(Box<Pending>),
}

/// The state of the screen.
#[derive(Clone, Debug)]
pub struct SettingsState {
    back: Screen,
    models: &'static Models,
    /// `None` while the file is being read.
    snapshot: Option<Result<Snapshot, String>>,
    cursor: usize,
    mode: Mode,
    notice: Option<String>,
    /// A write is out: keys wait for its answer.
    busy: bool,
    /// The editor has the terminal.
    editing: bool,
    /// Enter on the locked line showed the classes.
    locked_open: bool,
    /// The first line of the list drawn at the top, kept so the mark stays on the screen.
    scroll: Cell<usize>,
}

impl Default for SettingsState {
    fn default() -> Self {
        SettingsState {
            back: Screen::Runs,
            models: ham_core::models::builtin(),
            snapshot: None,
            cursor: 0,
            mode: Mode::Browse,
            notice: None,
            busy: false,
            editing: false,
            locked_open: false,
            scroll: Cell::new(0),
        }
    }
}

/// How a proposed change came out.
enum Proposal {
    /// The change is refused (a value the key does not take, a file that cannot be changed): the reason.
    Rejected(String),
    /// The file already says it: nothing is written.
    Unchanged(String),
    /// Nothing to ask: write it.
    Apply(Job),
    /// A preview waits for a second Enter.
    Ask(Box<Pending>),
}

const READING: &str = "Reading config.toml…";
/// What the notice row says when the screen opens on a machine with no file.
const NO_FILE: &str =
    "no config.toml yet: every setting is at its default; a change makes the file";

impl SettingsState {
    /// The screen opens: it remembers where to go back to and asks for the file.
    pub fn open(&mut self, back: Screen, models: &'static Models) {
        *self = SettingsState {
            back,
            models,
            ..SettingsState::default()
        };
    }

    /// The screen Esc goes back to.
    pub fn back(&self) -> Screen {
        self.back
    }

    /// The file as the last answer read it.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        match &self.snapshot {
            Some(Ok(s)) => Some(s),
            _ => None,
        }
    }

    /// A line for the notice row.
    pub fn say(&mut self, text: &str) {
        self.notice = Some(sanitize_line(text));
    }

    fn snap(&self) -> Option<&Snapshot> {
        self.snapshot()
    }

    fn items(&self) -> Vec<Item> {
        let Some(snap) = self.snap() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for g in GROUP_ORDER {
            for (i, r) in snap.rows.iter().enumerate() {
                if r.key.row().group == g {
                    out.push(Item::Row(i));
                }
            }
            // The rows of the engine's settings follow the rows of a run.
            if g == Group::Run
                && let Some(e) = &snap.engine
            {
                out.extend((0..e.rows.len()).map(Item::Engine));
            }
        }
        out.push(Item::Locked);
        out
    }

    fn current(&self) -> Option<Item> {
        self.items().get(self.cursor).copied()
    }

    fn clamp(&mut self) {
        let last = self.items().len().saturating_sub(1);
        self.cursor = self.cursor.min(last);
    }

    /// The wheel and the arrow keys: the mark moves `by` lines.
    pub fn step(&mut self, by: isize) {
        if !matches!(self.mode, Mode::Browse) || self.locked_open {
            return;
        }
        let last = self.items().len().saturating_sub(1);
        let to = self.cursor.saturating_add_signed(by);
        self.cursor = to.min(last);
    }

    /// A paste into the box that is open; false when there is none (the app says the screen has no text box).
    pub fn paste(&mut self, text: &str) -> bool {
        let Mode::Text(t) = &mut self.mode else {
            return false;
        };
        for c in text.chars().map(|c| if c == '\n' { ' ' } else { c }) {
            if !c.is_control() {
                t.text.insert(t.at, c);
                t.at += 1;
            }
        }
        t.error = None;
        true
    }

    /// Ctrl-C with text in the box empties it; true when it did.
    pub fn clear_box(&mut self) -> bool {
        match &mut self.mode {
            Mode::Text(t) if !t.text.is_empty() => {
                t.text.clear();
                t.at = 0;
                t.error = None;
                true
            }
            _ => false,
        }
    }

    /// The answer to a [`Job`]. Returns the key that was written, when a write changed the file.
    pub fn done(&mut self, done: Done) -> Option<Key> {
        match done {
            Done::Loaded(snapshot) => {
                self.busy = false;
                if matches!(&snapshot, Ok(s) if !s.exists) && self.notice.is_none() {
                    self.notice = Some(NO_FILE.to_string());
                }
                self.snapshot = Some(snapshot);
                self.mode = Mode::Browse;
                self.clamp();
                None
            }
            Done::AppliedEngine {
                path,
                op,
                result,
                snapshot,
            } => {
                self.busy = false;
                self.mode = Mode::Browse;
                let fresh = snapshot
                    .as_ref()
                    .ok()
                    .and_then(|s| s.engine.as_ref())
                    .and_then(|e| e.rows.iter().find(|r| r.path == path))
                    .cloned();
                self.snapshot = Some(snapshot);
                self.clamp();
                let label = fresh.as_ref().map_or(path, |r| r.label);
                self.notice = Some(match result {
                    Ok(w) if w.wrote => match (&op, &fresh) {
                        (Op::Set(_), Some(r)) => {
                            format!("Saved {label} = {} · takes effect: {}", r.shown, r.applies)
                        }
                        (Op::Unset, Some(r)) => format!(
                            "{label} is back to its default · takes effect: {}",
                            r.applies
                        ),
                        _ => format!("Saved {label}"),
                    },
                    Ok(_) => format!("{label}: nothing to change; the file was not written"),
                    Err(e) => format!("not saved: {}", sanitize_line(&e)),
                });
                None
            }
            Done::Applied {
                key,
                op,
                result,
                snapshot,
            } => {
                self.busy = false;
                self.mode = Mode::Browse;
                let label = key.row().label;
                let fresh = snapshot.as_ref().ok().and_then(|s| s.row(key)).cloned();
                self.snapshot = Some(snapshot);
                self.clamp();
                match result {
                    Ok(w) if w.wrote => {
                        let tail = fresh.as_ref().map(env_wins).unwrap_or_default();
                        self.notice = Some(match (&op, &fresh) {
                            (Op::Set(_), Some(r)) => format!(
                                "Saved {label} = {} · takes effect: {}{tail}",
                                r.shown, r.applies
                            ),
                            (Op::Unset, Some(r)) => format!(
                                "{label} is back to its default · takes effect: {}{tail}",
                                r.applies
                            ),
                            _ => format!("Saved {label}"),
                        });
                        Some(key)
                    }
                    Ok(_) => {
                        self.notice = Some(format!(
                            "{label}: nothing to change; the file was not written"
                        ));
                        None
                    }
                    Err(e) => {
                        self.notice = Some(format!("not saved: {}", sanitize_line(&e)));
                        None
                    }
                }
            }
        }
    }

    /// The editor child came back (`Msg::CommandDone` of `ham config edit`): the file is read again and what the check said is shown.
    /// `None` when the command is not ours.
    pub fn edit_done(&mut self, argv: &[String], out: &CmdOutput) -> Option<Vec<Effect>> {
        let word = |n: usize| argv.get(n).map(String::as_str);
        let ours = (word(0) == Some("config") && word(1) == Some("edit"))
            || (word(0) == Some("engine") && word(2) == Some("edit"));
        if !(self.editing && ours) {
            return None;
        }
        self.editing = false;
        let lines: Vec<String> = out
            .lines()
            .iter()
            .map(|l| sanitize_line(l))
            .filter(|l| !l.trim().is_empty())
            .collect();
        let said = if out.code == 0 {
            lines.last().cloned().unwrap_or_default()
        } else {
            lines
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ")
        };
        self.notice = Some(format!("editor closed · {said}"));
        Some(vec![Effect::Settings(Job::Load)])
    }

    // ---- keys ----

    /// One key. The effects are the work to do; the exit says whether the screen is left.
    pub fn key(&mut self, key: KeyEvent, cx: &Cx) -> (Vec<Effect>, Exit) {
        if self.busy {
            return (Vec::new(), Exit::Stay);
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Browse => self.browse_key(key, cx),
            Mode::Text(t) => self.text_key(t, key, cx),
            Mode::Choose(c) => self.choose_key(c, key, cx),
            Mode::Confirm(p) => self.confirm_key(*p, key, cx),
        }
    }

    fn browse_key(&mut self, key: KeyEvent, cx: &Cx) -> (Vec<Effect>, Exit) {
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if self.locked_open => {
                self.locked_open = false;
                stay()
            }
            KeyCode::Esc => (Vec::new(), Exit::Back),
            KeyCode::Char('q') if !ctrl => (Vec::new(), Exit::Quit),
            KeyCode::Up | KeyCode::Char('k') => {
                self.step(-1);
                stay()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.step(1);
                stay()
            }
            KeyCode::PageUp => {
                self.step(-10);
                stay()
            }
            KeyCode::PageDown => {
                self.step(10);
                stay()
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.step(isize::MIN / 2);
                stay()
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.step(isize::MAX / 2);
                stay()
            }
            KeyCode::Char('e') if !ctrl && !self.locked_open => {
                self.editing = true;
                self.notice = Some("the editor has the terminal…".to_string());
                // On a row of the engine's settings the editor opens the engine file; elsewhere it opens `config.toml`.
                let argv = match (self.current(), self.snap().and_then(|s| s.engine.as_ref())) {
                    (Some(Item::Engine(_)), Some(e)) => {
                        vec![
                            "engine".to_string(),
                            e.engine.to_string(),
                            "edit".to_string(),
                        ]
                    }
                    _ => vec!["config".to_string(), "edit".to_string()],
                };
                (vec![Effect::RunTerminal { argv }], Exit::Stay)
            }
            KeyCode::Char('u') if !ctrl && !self.locked_open => self.unset_current(cx),
            KeyCode::Enter if cx.calm => self.enter_current(cx),
            _ => stay(),
        }
    }

    /// A message about a file the screen cannot write to, or `None` when it can.
    fn blocked(&self) -> Option<String> {
        match &self.snapshot {
            None => Some("config.toml is still being read".to_string()),
            Some(Err(e)) => Some(format!("config.toml could not be read: {e}")),
            Some(Ok(s)) if s.read_only() => Some(
                "config.toml cannot be changed here until it can be read; press e to edit the file"
                    .to_string(),
            ),
            Some(Ok(_)) => None,
        }
    }

    /// A message about the engine file the screen cannot write to, or `None` when it can.
    fn blocked_engine(&self) -> Option<String> {
        let e = self.snap().and_then(|s| s.engine.as_ref())?;
        e.read_only().then(|| {
            format!(
                "{} cannot be changed here until it can be read; press e to edit the file",
                ham_core::engine_file::shown_path(e.engine)
            )
        })
    }

    fn enter_current(&mut self, cx: &Cx) -> (Vec<Effect>, Exit) {
        let Some(item) = self.current() else {
            return stay();
        };
        let row = match item {
            Item::Locked => {
                self.locked_open = !self.locked_open;
                return stay();
            }
            Item::Engine(j) => return self.enter_engine(j, cx),
            Item::Row(i) => i,
        };
        if self.locked_open {
            return stay();
        }
        if let Some(why) = self.blocked() {
            self.notice = Some(why);
            return stay();
        }
        let Some(view) = self.snap().and_then(|s| s.rows.get(row)).cloned() else {
            return stay();
        };
        let key = view.key;
        match key.row().kind {
            Kind::Bool => {
                // The opposite of what is in force; a switch the stage decides counts as on.
                let want = !view.on.unwrap_or(true);
                self.propose_now(key, Op::Set(want.to_string()), cx)
            }
            Kind::Mouse => {
                let next = match view.in_file.as_deref() {
                    None | Some("true") => "false",
                    Some("false") => "auto",
                    Some(_) => "true",
                };
                self.propose_now(key, Op::Set(next.to_string()), cx)
            }
            Kind::Mascot => {
                // auto, then ascii, then off, then auto again.
                let next = match view.in_file.as_deref() {
                    None | Some("auto") => "ascii",
                    Some("ascii") => "off",
                    Some(_) => "auto",
                };
                self.propose_now(key, Op::Set(next.to_string()), cx)
            }
            Kind::Flow => {
                // auto, then full, then quick, then auto again.
                let next = match view.in_file.as_deref() {
                    None | Some("auto") => "full",
                    Some("full") => "quick",
                    Some(_) => "auto",
                };
                self.propose_now(key, Op::Set(next.to_string()), cx)
            }
            Kind::Review => {
                // auto, then off, then auto again.
                let next = match view.in_file.as_deref() {
                    None | Some("auto") => "off",
                    Some(_) => "auto",
                };
                self.propose_now(key, Op::Set(next.to_string()), cx)
            }
            Kind::Compute => {
                // auto, then instances, then microvm, then auto again.
                let next = match view.in_file.as_deref() {
                    None | Some("auto") => "instances",
                    Some("instances") => "microvm",
                    Some(_) => "auto",
                };
                self.propose_now(key, Op::Set(next.to_string()), cx)
            }
            Kind::Minutes { .. } | Kind::Number { .. } | Kind::Text => {
                let text: Vec<char> = view.in_file.unwrap_or_default().chars().collect();
                let at = text.len();
                self.mode = Mode::Text(TextBox {
                    target: BoxTarget::Client(key),
                    text,
                    at,
                    error: None,
                });
                stay()
            }
            Kind::Model => {
                let models: Vec<(String, String)> = self
                    .models
                    .models()
                    .iter()
                    .filter(|m| m.selectable())
                    .map(|m| (m.id.clone(), m.name.clone()))
                    .collect();
                let current = self
                    .models
                    .find(&view.raw)
                    .map(|m| m.id.clone())
                    .unwrap_or_default();
                let cursor = models
                    .iter()
                    .position(|(id, _)| *id == current)
                    .unwrap_or(0);
                self.mode = Mode::Choose(Chooser {
                    models,
                    current,
                    cursor,
                });
                stay()
            }
        }
    }

    fn unset_current(&mut self, cx: &Cx) -> (Vec<Effect>, Exit) {
        if let Some(Item::Engine(j)) = self.current() {
            if let Some(why) = self.blocked_engine() {
                self.notice = Some(why);
                return stay();
            }
            let Some(path) = self.engine_row(j).map(|r| r.path) else {
                return stay();
            };
            return self.propose_engine_now(path, Op::Unset);
        }
        let Some(Item::Row(i)) = self.current() else {
            return stay();
        };
        if let Some(why) = self.blocked() {
            self.notice = Some(why);
            return stay();
        }
        let Some(key) = self.snap().and_then(|s| s.rows.get(i)).map(|r| r.key) else {
            return stay();
        };
        self.propose_now(key, Op::Unset, cx)
    }

    /// Proposes a change from the list and acts on the outcome (a rejection is a notice).
    fn propose_now(&mut self, key: Key, op: Op, cx: &Cx) -> (Vec<Effect>, Exit) {
        match self.propose(key, &op, cx.now_ms) {
            Proposal::Rejected(why) | Proposal::Unchanged(why) => {
                self.notice = Some(why);
                stay()
            }
            Proposal::Apply(job) => {
                self.busy = true;
                (vec![Effect::Settings(job)], Exit::Stay)
            }
            Proposal::Ask(p) => {
                self.mode = Mode::Confirm(p);
                stay()
            }
        }
    }

    /// The row of the engine's settings at place `j`.
    fn engine_row(&self, j: usize) -> Option<&EngineRowView> {
        self.snap()?.engine.as_ref()?.rows.get(j)
    }

    /// Enter on a row of the engine's settings: a choice goes to its next level, a list to its next subset, a number opens the box.
    fn enter_engine(&mut self, j: usize, _cx: &Cx) -> (Vec<Effect>, Exit) {
        if self.locked_open {
            return stay();
        }
        if let Some(why) = self.blocked_engine() {
            self.notice = Some(why);
            return stay();
        }
        let Some(e) = self.snap().and_then(|s| s.engine.as_ref()) else {
            return stay();
        };
        let (engine, Some(row)) = (e.engine, e.rows.get(j).cloned()) else {
            return stay();
        };
        let Some(def) = keys_for(engine).and_then(|rows| rows.get(row.index)) else {
            return stay();
        };
        match def.kind {
            KeyKind::Choice(set) => {
                let at = set.iter().position(|m| *m == row.raw).unwrap_or(0);
                let next = set[(at + 1) % set.len()];
                self.propose_engine_now(row.path, Op::Set(next.to_string()))
            }
            KeyKind::Set(members) => {
                let have: Vec<&str> = row
                    .raw
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .collect();
                let mask = members
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| have.contains(m))
                    .fold(0usize, |acc, (bit, _)| acc | (1 << bit));
                // The subsets in order of their bits: none, the first, the second, both, and round again.
                let next = (mask + 1) % (1usize << members.len());
                let picked: Vec<&str> = members
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| next & (1 << bit) != 0)
                    .map(|(_, m)| *m)
                    .collect();
                self.propose_engine_now(row.path, Op::Set(picked.join(",")))
            }
            KeyKind::Int { .. } | KeyKind::Float { .. } => {
                let text: Vec<char> = if row.set {
                    row.raw.chars().collect()
                } else {
                    Vec::new()
                };
                let at = text.len();
                self.mode = Mode::Text(TextBox {
                    target: BoxTarget::Engine(j),
                    text,
                    at,
                    error: None,
                });
                stay()
            }
        }
    }

    /// Proposes a change to a key of the engine's file from the list and acts on the outcome (a rejection is a notice).
    fn propose_engine_now(&mut self, path: &'static str, op: Op) -> (Vec<Effect>, Exit) {
        match self.propose_engine(path, &op) {
            Proposal::Rejected(why) | Proposal::Unchanged(why) => {
                self.notice = Some(why);
                stay()
            }
            Proposal::Apply(job) => {
                self.busy = true;
                (vec![Effect::Settings(job)], Exit::Stay)
            }
            // A change to the engine file asks nothing: it is the person's own file and the change is previewed by the row itself.
            Proposal::Ask(_) => stay(),
        }
    }

    /// What changing `path` of the engine's file by `op` comes to, planned on the text as read with the same patcher and the same check as
    /// `ham engine <name> set`: a value the check would refuse is rejected here with its words, and a change that changes nothing says so.
    fn propose_engine(&self, path: &'static str, op: &Op) -> Proposal {
        let Some(e) = self.snap().and_then(|s| s.engine.as_ref()) else {
            return Proposal::Rejected("the engine file is still being read".to_string());
        };
        let planned = match op {
            Op::Set(value) => engine_cmd::plan_set(e.engine, &e.text, path, value),
            Op::Unset => engine_cmd::plan_unset(e.engine, &e.text, path),
        };
        let plan = match planned {
            Ok(p) => p,
            // The first line is the refusal itself (`engines/<name>.toml line 3: key ... must be ...`); the rest only says that nothing was written.
            Err(msg) => {
                return Proposal::Rejected(sanitize_line(msg.lines().next().unwrap_or_default()));
            }
        };
        if plan.text.is_some() {
            return Proposal::Apply(Job::ApplyEngine {
                path,
                op: op.clone(),
            });
        }
        let row = e.rows.iter().find(|r| r.path == path);
        let label = row.map_or(path, |r| r.label);
        Proposal::Unchanged(match op {
            Op::Set(_) => format!(
                "{label} is already {} in the file; nothing was written",
                row.map_or(String::new(), |r| r.shown.clone())
            ),
            Op::Unset => format!("{label} is already at its default"),
        })
    }

    /// What changing `key` by `op` comes to: checked against the file as read, with the same checks, previews and confirmations as the same
    /// `ham config set` typed in the message box.
    fn propose(&self, key: Key, op: &Op, now_ms: u64) -> Proposal {
        let Some(snap) = self.snap() else {
            return Proposal::Rejected("config.toml is still being read".to_string());
        };
        let row = key.row();
        let planned = match op {
            Op::Set(value) => plan_set(&snap.text, row.name, value),
            Op::Unset => plan_unset(&snap.text, row.name),
        };
        let plan = match planned {
            Ok(p) => p,
            Err(e) => return Proposal::Rejected(sanitize_line(&e.to_string())),
        };
        if plan.text.is_none() {
            let tail = snap.row(key).map(env_wins).unwrap_or_default();
            return Proposal::Unchanged(match op {
                Op::Set(v) => format!(
                    "{} is already {} in the file; nothing was written{tail}",
                    row.label,
                    plain(row.kind, v)
                ),
                Op::Unset => format!("{} is already at its default{tail}", row.label),
            });
        }
        let argv: Vec<String> = match op {
            Op::Set(v) => ["config", "set", row.name, v.as_str()]
                .iter()
                .map(ToString::to_string)
                .collect(),
            Op::Unset => ["config", "unset", row.name]
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        let outcome = prepare_argv(&argv, false);
        let mut preview = match &outcome {
            RouteOutcome::Command { preview, .. } => preview.clone(),
            RouteOutcome::Refused { reason, .. } => {
                return Proposal::Rejected(sanitize_line(reason));
            }
            _ => None,
        };
        if preview.is_none() && plan.opens_the_door {
            preview = Some(Preview {
                text: super::commands::door_text(&door_note(crate::version::CLIENT, row.name)),
                confirm_with: None,
            });
        }
        let Some(preview) = preview else {
            return Proposal::Apply(Job::Apply {
                key,
                op: op.clone(),
            });
        };
        let RouteOutcome::Command {
            argv,
            invocation,
            class,
            ..
        } = outcome
        else {
            return Proposal::Rejected("this change cannot be previewed".to_string());
        };
        let asked = RouteOutcome::Command {
            argv,
            invocation,
            class,
            preview: Some(preview.clone()),
        };
        let Some(conf) = Confirmation::new(&asked, now_ms) else {
            return Proposal::Rejected("this change cannot be previewed".to_string());
        };
        Proposal::Ask(Box::new(Pending {
            key,
            op: op.clone(),
            text: sanitize_line(&preview.text),
            expected: preview.confirm_with,
            conf,
            hint: None,
        }))
    }

    fn text_key(&mut self, mut t: TextBox, key: KeyEvent, cx: &Cx) -> (Vec<Effect>, Exit) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                return stay();
            }
            KeyCode::Enter => {
                if !cx.calm {
                    self.mode = Mode::Text(t);
                    return stay();
                }
                let typed: String = t.text.iter().collect::<String>().trim().to_string();
                if typed.is_empty() {
                    t.error = Some(
                        "type a value; Esc leaves the box, and u on the row brings the default back"
                            .to_string(),
                    );
                    self.mode = Mode::Text(t);
                    return stay();
                }
                let proposal = match t.target {
                    BoxTarget::Client(key) => self.propose(key, &Op::Set(typed), cx.now_ms),
                    BoxTarget::Engine(j) => match self.engine_row(j).map(|r| r.path) {
                        Some(path) => self.propose_engine(path, &Op::Set(typed)),
                        None => Proposal::Rejected(
                            "that row is gone; the file was read again".to_string(),
                        ),
                    },
                };
                return match proposal {
                    Proposal::Rejected(why) => {
                        t.error = Some(why);
                        self.mode = Mode::Text(t);
                        stay()
                    }
                    Proposal::Unchanged(why) => {
                        self.notice = Some(why);
                        stay()
                    }
                    Proposal::Apply(job) => {
                        self.busy = true;
                        (vec![Effect::Settings(job)], Exit::Stay)
                    }
                    Proposal::Ask(p) => {
                        self.mode = Mode::Confirm(p);
                        stay()
                    }
                };
            }
            KeyCode::Left => t.at = t.at.saturating_sub(1),
            KeyCode::Right => t.at = (t.at + 1).min(t.text.len()),
            KeyCode::Home => t.at = 0,
            KeyCode::End => t.at = t.text.len(),
            KeyCode::Backspace if t.at > 0 => {
                t.at -= 1;
                t.text.remove(t.at);
                t.error = None;
            }
            KeyCode::Delete if t.at < t.text.len() => {
                t.text.remove(t.at);
                t.error = None;
            }
            KeyCode::Char('u') if ctrl => {
                t.text.clear();
                t.at = 0;
                t.error = None;
            }
            KeyCode::Char(c)
                if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) && !c.is_control() =>
            {
                t.text.insert(t.at, c);
                t.at += 1;
                t.error = None;
            }
            _ => {}
        }
        self.mode = Mode::Text(t);
        stay()
    }

    fn choose_key(&mut self, mut c: Chooser, key: KeyEvent, cx: &Cx) -> (Vec<Effect>, Exit) {
        let last = c.models.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                return stay();
            }
            KeyCode::Up | KeyCode::Char('k') => c.cursor = c.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => c.cursor = (c.cursor + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => c.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => c.cursor = last,
            KeyCode::Enter if cx.calm => {
                let Some((id, _)) = c.models.get(c.cursor).cloned() else {
                    self.mode = Mode::Browse;
                    return stay();
                };
                return self.propose_now(Key::DefaultModel, Op::Set(id), cx);
            }
            _ => {}
        }
        self.mode = Mode::Choose(c);
        stay()
    }

    fn confirm_key(&mut self, mut p: Pending, key: KeyEvent, cx: &Cx) -> (Vec<Effect>, Exit) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let pressed = match key.code {
            KeyCode::Esc => Some(ConfirmKey::Esc),
            KeyCode::Enter if cx.calm => Some(ConfirmKey::Enter),
            KeyCode::Backspace => Some(ConfirmKey::Backspace),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                Some(ConfirmKey::Char(c))
            }
            _ => None,
        };
        let Some(pressed) = pressed else {
            self.mode = Mode::Confirm(Box::new(p));
            return stay();
        };
        match p.conf.press(pressed, cx.now_ms) {
            ConfirmStep::Run => {
                self.busy = true;
                (
                    vec![Effect::Settings(Job::Apply {
                        key: p.key,
                        op: p.op,
                    })],
                    Exit::Stay,
                )
            }
            ConfirmStep::Cancelled => {
                self.notice = Some("cancelled; nothing was written".to_string());
                stay()
            }
            ConfirmStep::Spent => stay(),
            ConfirmStep::Waiting(wait) => {
                p.hint = match (wait, p.expected.as_deref()) {
                    (Wait::TypeAnswer, Some(w)) => Some(format!("type {w} to confirm, then Enter")),
                    (Wait::Mismatch, Some(w)) => {
                        Some(format!("that is not {w}; type it as written, then Enter"))
                    }
                    (Wait::Idle | Wait::TooSoon, _) => p.hint.take(),
                    _ => None,
                };
                self.mode = Mode::Confirm(Box::new(p));
                stay()
            }
        }
    }
}

fn stay() -> (Vec<Effect>, Exit) {
    (Vec::new(), Exit::Stay)
}

/// A value as the screen words it for a notice.
fn plain(kind: Kind, v: &str) -> String {
    match (kind, v) {
        (Kind::Bool | Kind::Mouse, "true") => "on".to_string(),
        (Kind::Bool | Kind::Mouse, "false") => "off".to_string(),
        _ => sanitize_line(v),
    }
}

/// ` · HAMH_NOTIFY is set and still wins` when a variable decides what is in force whatever the file says.
fn env_wins(r: &RowView) -> String {
    r.origin
        .strip_prefix("env ")
        .map(|var| format!(" · {var} is set and still wins over the file"))
        .unwrap_or_default()
}

// ---- drawing -------------------------------------------------------------------------------------------------------------------------

/// The width of the label column.
const LABEL_W: usize = 20;
/// The width of the name of the locked line, which is longer than a label.
const LOCKED_W: usize = 21;
/// The width of the "takes effect" column (`next composer`).
const APPLIES_W: usize = 13;
/// The space between the columns after the value.
const GUTTER: usize = 2;
/// The smallest value column worth drawing; below it the "takes effect" column goes first, then the origin.
const VALUE_MIN: usize = 12;

/// `text` cut to `width` terminal columns, with an ellipsis when something was cut, and padded with spaces up to `width` when `pad` is set.
fn fit(text: &str, width: usize, pad: bool) -> String {
    let mut out = String::new();
    let mut used = 0;
    let total = Span::raw(text).width();
    if total <= width {
        out.push_str(text);
        used = total;
    } else if width > 0 {
        for c in text.chars() {
            let w = Span::raw(c.to_string()).width();
            if used + w + 1 > width {
                break;
            }
            out.push(c);
            used += w;
        }
        out.push('…');
        used += 1;
    }
    if pad {
        out.push_str(&" ".repeat(width.saturating_sub(used)));
    }
    out
}

/// `text` wrapped at spaces to `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        if !cur.is_empty() && Span::raw(format!("{cur} {word}")).width() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    lines.push(cur);
    lines
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// The keys of the footer line and what each does, for the line and for the clickable regions.
type Footer = Vec<(&'static str, Option<(KeyCode, &'static str)>)>;

impl SettingsState {
    fn footer(&self) -> Footer {
        let enter = Some((KeyCode::Enter, "Enter"));
        let esc = Some((KeyCode::Esc, "Esc"));
        let chip = |c: char, name: &'static str| Some((KeyCode::Char(c), name));
        match &self.mode {
            Mode::Text(_) => vec![("Enter save", enter), ("Esc cancel", esc)],
            Mode::Choose(_) => vec![
                ("↑↓ select", None),
                ("Enter choose", enter),
                ("Esc cancel", esc),
            ],
            Mode::Confirm(p) => {
                if p.expected.is_some() {
                    vec![
                        ("type the name", None),
                        ("Enter confirm", enter),
                        ("Esc cancel", esc),
                    ]
                } else {
                    vec![("Enter confirm", enter), ("Esc cancel", esc)]
                }
            }
            Mode::Browse => {
                if self.locked_open {
                    return vec![("Esc close", esc), ("q quit", chip('q', "q"))];
                }
                match (&self.snapshot, self.current()) {
                    (Some(Ok(s)), _) if s.read_only() => vec![
                        ("e edit the file", chip('e', "e")),
                        ("Esc back", esc),
                        ("q quit", chip('q', "q")),
                    ],
                    (Some(Ok(_)), Some(Item::Locked)) => vec![
                        ("Enter show why", enter),
                        ("e edit the file", chip('e', "e")),
                        ("Esc back", esc),
                        ("q quit", chip('q', "q")),
                    ],
                    (Some(Ok(_)), _) => vec![
                        ("Enter change", enter),
                        ("u use the default", chip('u', "u")),
                        ("e edit the file", chip('e', "e")),
                        ("Esc back", esc),
                        ("q quit", chip('q', "q")),
                    ],
                    _ => vec![("Esc back", esc), ("q quit", chip('q', "q"))],
                }
            }
        }
    }

    /// The lines of the list (banner, groups, rows, the locked line) and the lines the marked row takes, first and last: the window keeps
    /// all of them in view (the locked row, once Enter has opened it, is the line and the classes under it).
    fn list_lines(&self, width: usize) -> (Vec<Line<'static>>, Marked) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        match &self.snapshot {
            None => {
                lines.push(Line::from(format!(" {READING}")));
                return (lines, (0, 0));
            }
            Some(Err(e)) => {
                for l in wrap(
                    &format!("Could not read config.toml: {}", sanitize_line(e)),
                    width.saturating_sub(2),
                ) {
                    lines.push(Line::from(format!(" {l}")));
                }
                return (lines, (0, 0));
            }
            Some(Ok(_)) => {}
        }
        let Some(snap) = self.snap() else {
            return (lines, (0, 0));
        };
        // The file's own news first: what keeps it read-only, what a newer ham did, and what the reader found wrong.
        if let Some(why) = &snap.unusable {
            lines.push(Line::styled(
                " config.toml cannot be changed here until it can be read (read-only):".to_string(),
                bold(),
            ));
            for l in why.lines().take(4) {
                lines.push(Line::from(format!(
                    "   {}",
                    fit(l, width.saturating_sub(4), false)
                )));
            }
        } else {
            if snap.newer {
                lines.push(Line::styled(
                    " config.toml was written by a newer ham: it is read, and changes are refused (update ham)".to_string(),
                    bold(),
                ));
            }
            let shown = snap.findings.len().min(3);
            for l in snap.findings.iter().take(shown) {
                lines.push(Line::from(format!(
                    "   {}",
                    fit(l, width.saturating_sub(4), false)
                )));
            }
            if snap.findings.len() > shown {
                lines.push(Line::from(format!(
                    "   (and {} more; ham config check lists them)",
                    snap.findings.len() - shown
                )));
            }
        }
        // The engine file's news in the same way: what keeps its rows read-only, and what the check found wrong.
        if let Some(e) = &snap.engine {
            let name = ham_core::engine_file::shown_path(e.engine);
            if let Some(why) = &e.unusable {
                lines.push(Line::styled(
                    format!(" {name} cannot be changed here until it can be read (read-only):"),
                    bold(),
                ));
                for l in why.lines().take(4) {
                    lines.push(Line::from(format!(
                        "   {}",
                        fit(l, width.saturating_sub(4), false)
                    )));
                }
            } else {
                let shown = e.findings.len().min(3);
                for l in e.findings.iter().take(shown) {
                    lines.push(Line::from(format!(
                        "   {}",
                        fit(l, width.saturating_sub(4), false)
                    )));
                }
                if e.findings.len() > shown {
                    lines.push(Line::from(format!(
                        "   (and {} more; ham engine {} check lists them)",
                        e.findings.len() - shown,
                        e.engine
                    )));
                }
            }
        }
        let items = self.items();
        let engine_rows: &[EngineRowView] = snap.engine.as_ref().map_or(&[], |e| e.rows.as_slice());
        let origin_w = snap
            .rows
            .iter()
            .map(|r| Span::raw(r.origin.clone()).width())
            .chain(engine_rows.iter().map(|r| Span::raw(r.origin).width()))
            .max()
            .unwrap_or(0)
            .max(7);
        let fixed = 3 + LABEL_W + 1;
        // The columns after the value are dropped, the "takes effect" one first, when the width does not leave the value its minimum.
        let (show_applies, show_origin) = {
            let with_origin = fixed + VALUE_MIN + GUTTER + origin_w;
            (
                width >= with_origin + GUTTER + APPLIES_W,
                width >= with_origin,
            )
        };
        let room = width
            .saturating_sub(fixed)
            .saturating_sub(if show_origin { GUTTER + origin_w } else { 0 })
            .saturating_sub(if show_applies { GUTTER + APPLIES_W } else { 0 })
            .max(4);
        // The value column is as wide as the longest value, so the columns after it sit next to it on a wide screen.
        let longest = snap
            .rows
            .iter()
            .map(|r| Span::raw(r.shown.clone()).width())
            .chain(
                engine_rows
                    .iter()
                    .map(|r| Span::raw(r.shown.clone()).width()),
            )
            .max()
            .unwrap_or(0);
        let value_w = room.min(longest.max(VALUE_MIN));
        let gutter = " ".repeat(GUTTER);
        let mut mark_line = 0;
        // Where the lines of the marked row begin: its own line, except for the first row under a group heading, which begins at the heading.
        // The window that follows the mark up to such a row then keeps the heading above it instead of cutting it off.
        let mut mark_first = 0;
        for g in GROUP_ORDER {
            let head = lines.len();
            lines.push(Line::styled(format!(" {}", g.label()), bold()));
            for (n, item) in items.iter().enumerate() {
                let Item::Row(i) = item else { continue };
                let r = &snap.rows[*i];
                if r.key.row().group != g {
                    continue;
                }
                let at = n == self.cursor;
                if at {
                    mark_line = lines.len();
                    mark_first = if mark_line == head + 1 {
                        head
                    } else {
                        mark_line
                    };
                }
                let lead = if at { " ▶ " } else { "   " };
                let mut text = format!(
                    "{lead}{} {}",
                    fit(r.key.row().label, LABEL_W, true),
                    fit(&r.shown, value_w, true)
                );
                if show_origin {
                    text.push_str(&format!("{gutter}{}", fit(&r.origin, origin_w, true)));
                }
                if show_applies {
                    text.push_str(&format!("{gutter}{}", fit(r.applies, APPLIES_W, false)));
                }
                let style = if at {
                    bold()
                } else if r.set {
                    Style::default()
                } else {
                    dim()
                };
                lines.push(Line::styled(text.trim_end().to_string(), style));
            }
            if g == Group::Run
                && let Some(e) = &snap.engine
            {
                let head = lines.len();
                lines.push(Line::styled(
                    format!(" {}", engines::settings_group_label(e.engine)),
                    bold(),
                ));
                for (j, r) in e.rows.iter().enumerate() {
                    let at = items.get(self.cursor) == Some(&Item::Engine(j));
                    if at {
                        mark_line = lines.len();
                        mark_first = if mark_line == head + 1 {
                            head
                        } else {
                            mark_line
                        };
                    }
                    let lead = if at { " ▶ " } else { "   " };
                    let mut text = format!(
                        "{lead}{} {}",
                        fit(r.label, LABEL_W, true),
                        fit(&r.shown, value_w, true)
                    );
                    if show_origin {
                        text.push_str(&format!("{gutter}{}", fit(r.origin, origin_w, true)));
                    }
                    if show_applies {
                        text.push_str(&format!("{gutter}{}", fit(r.applies, APPLIES_W, false)));
                    }
                    let style = if at {
                        bold()
                    } else if r.set {
                        Style::default()
                    } else {
                        dim()
                    };
                    lines.push(Line::styled(text.trim_end().to_string(), style));
                }
            }
        }
        // The locked line is one row at every width: the classes that fit and a count of the rest. After Enter each class follows with its reason.
        let at = items.get(self.cursor) == Some(&Item::Locked);
        if at {
            mark_line = lines.len();
            mark_first = mark_line;
        }
        let lead = if at { " ▶ " } else { "   " };
        let name = fit("Locked by the harness", LOCKED_W, true);
        let room = width.saturating_sub(3 + LOCKED_W + 2).max(10);
        lines.push(Line::styled(
            format!("{lead}{name}  {}", locked_summary(room)),
            if at { bold() } else { Style::default() },
        ));
        if self.locked_open {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                " No file or flag changes these; the harness sets them for every run:".to_string(),
                bold(),
            ));
            let name_w = LOCKED.iter().map(|c| c.name.len()).max().unwrap_or(10);
            for c in &LOCKED {
                let room = width.saturating_sub(3 + name_w + 2).max(10);
                for (n, part) in wrap(c.reason, room).iter().enumerate() {
                    let head = if n == 0 {
                        format!("   {:<name_w$}  ", c.name)
                    } else {
                        " ".repeat(3 + name_w + 2)
                    };
                    lines.push(Line::from(format!("{head}{part}")));
                }
            }
        }
        // The window keeps the locked line in view whole, and with it the list of classes once Enter has opened it.
        let last = if at && self.locked_open {
            lines.len() - 1
        } else {
            mark_line
        };
        (lines, (mark_first, last))
    }

    /// The rows under the list: the box, the model list's own hint or the preview, whichever is open.
    fn prompt_lines(&self, width: usize) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
        let room = width.saturating_sub(2).max(10);
        match &self.mode {
            Mode::Text(t) => {
                let (label, help) = match t.target {
                    BoxTarget::Client(key) => (key.row().label, key.row().help),
                    BoxTarget::Engine(j) => {
                        self.engine_row(j).map_or(("", ""), |r| (r.label, r.help))
                    }
                };
                let typed: String = sanitize_line(&t.text.iter().collect::<String>());
                let head = format!(" {label}: ");
                let before: String = t.text.iter().take(t.at).collect();
                let col =
                    Span::raw(head.clone()).width() + Span::raw(sanitize_line(&before)).width();
                let mut lines = vec![Line::styled(format!("{head}{typed}"), bold())];
                let hint = match &t.error {
                    Some(e) => format!(" {}", fit(&sanitize_line(e), room, false)),
                    None => format!(" {}", fit(help, room, false)),
                };
                lines.push(Line::from(hint));
                (lines, Some((u16::try_from(col).unwrap_or(u16::MAX), 0)))
            }
            Mode::Confirm(p) => {
                let mut lines: Vec<Line<'static>> = wrap(&p.text, room)
                    .into_iter()
                    .take(3)
                    .map(|l| Line::styled(format!(" {l}"), bold()))
                    .collect();
                let mut cursor = None;
                if let Some(want) = &p.expected {
                    let typed = sanitize_line(p.conf.typed());
                    let head = format!(" type {want} to confirm: ");
                    let col = Span::raw(head.clone()).width() + Span::raw(typed.clone()).width();
                    cursor = Some((
                        u16::try_from(col).unwrap_or(u16::MAX),
                        u16::try_from(lines.len()).unwrap_or(u16::MAX),
                    ));
                    lines.push(Line::from(format!("{head}{typed}")));
                }
                if let Some(h) = &p.hint {
                    lines.push(Line::from(format!(
                        " {}",
                        fit(&sanitize_line(h), room, false)
                    )));
                }
                (lines, cursor)
            }
            Mode::Browse | Mode::Choose(_) => (Vec::new(), None),
        }
    }

    /// The model list, drawn in place of the settings.
    fn chooser_lines(&self, c: &Chooser, width: usize) -> (Vec<Line<'static>>, Marked) {
        let mut lines = vec![Line::styled(
            " Model: the one a run uses when you name none".to_string(),
            bold(),
        )];
        let mut mark = 0;
        for (n, (id, name)) in c.models.iter().enumerate() {
            let at = n == c.cursor;
            if at {
                mark = lines.len();
            }
            let tag = if *id == c.current { "  (in force)" } else { "" };
            let window = self
                .models
                .find(id)
                .map(|m| {
                    format!(
                        "  context {}",
                        ham_core::tables::group_digits(u64::from(m.context_tokens))
                    )
                })
                .unwrap_or_default();
            let text = fit(
                &format!(
                    "{} {}{window}{tag}",
                    if at { " ▶" } else { "  " },
                    sanitize_line(name)
                ),
                width,
                false,
            );
            lines.push(Line::styled(
                text,
                if at { bold() } else { Style::default() },
            ));
        }
        (lines, (mark, mark))
    }
}

/// The first and the last line of the marked row in the list, both counted from the top of the list. The first row under a group heading
/// begins at the heading, so a window that comes up to it keeps the heading above it.
type Marked = (usize, usize);

/// What the locked line says in `room` columns: the classes of the registry in its order, as many as fit, then how many it leaves out
/// (`credentials · provider · telemetry · +4 more`), and when not even one fits, the count alone. The row never wraps, so it is one
/// row on every window; Enter lists each class with its reason.
fn locked_summary(room: usize) -> String {
    let all = LOCKED.len();
    let of = |k: usize| -> String {
        let names = LOCKED
            .iter()
            .take(k)
            .map(|c| c.label)
            .collect::<Vec<_>>()
            .join(" · ");
        match k {
            0 => format!("{all} classes"),
            k if k == all => names,
            _ => format!("{names} · +{} more", all - k),
        }
    };
    (0..=all)
        .rev()
        .map(of)
        .find(|s| Span::raw(s.clone()).width() <= room)
        .unwrap_or_else(|| of(0))
}

/// The line of the list drawn at the top of a window `rows` tall so that the marked row (`first` to `last`) is in view: the row stays
/// where it is when it already is; otherwise the window moves just far enough to show all of it, never so far that its first line leaves
/// the top (a row taller than the window shows its beginning). `top` is clamped to the last full window, so a list that has become
/// shorter (the classes closed, a taller terminal) fills the window from its end instead of leaving a blank tail.
fn window_top(top: usize, rows: usize, len: usize, (first, last): Marked) -> usize {
    let mut top = top.min(len.saturating_sub(rows));
    if first < top {
        top = first;
    } else if last >= top + rows {
        top = (last + 1 - rows).min(first);
    }
    top
}

/// The `height` rows of `area` from row `y`, cut to the screen: a terminal too short for every band draws the ones that fit.
fn band(area: Rect, y: u16, height: u16) -> Rect {
    Rect::new(area.x, y, area.width, height).intersection(area)
}

/// Draws the screen: the title, the list (or the model list), the box or the preview when one is open, a notice row and the key line.
pub(crate) fn draw(
    state: &SettingsState,
    f: &mut Frame,
    hits: &RefCell<HitMap>,
    split: &super::head::Split,
) {
    let area = split.area;
    let width = usize::from(area.width);
    if area.height < 3 {
        return;
    }
    let title = match state.snapshot() {
        Some(s) if !s.exists => "ham · Settings · config.toml (no file yet)",
        _ => "ham · Settings · config.toml",
    };
    f.render_widget(
        Paragraph::new(Line::styled(title.to_string(), bold())),
        split.title_over(band(area, area.y, 1)),
    );
    let (mut prompt, mut cursor) = state.prompt_lines(width);
    // On a short terminal the box or the preview gives up its first lines, not the key line: it keeps two rows of the list above it.
    let room = usize::from(area.height).saturating_sub(1 + 1 + 1 + 2);
    let cut = prompt.len().saturating_sub(room);
    if cut > 0 {
        prompt.drain(..cut);
        cursor = cursor.and_then(|(col, row)| {
            usize::from(row)
                .checked_sub(cut)
                .and_then(|r| u16::try_from(r).ok())
                .map(|r| (col, r))
        });
    }
    let prompt_h = u16::try_from(prompt.len()).unwrap_or(u16::MAX);
    // Title, then the list, the prompt, the notice and the key line.
    let fixed = 1 + prompt_h + 1 + 1;
    let body_h = area.height.saturating_sub(fixed).max(1);
    let (lines, mark) = match &state.mode {
        Mode::Choose(c) => state.chooser_lines(c, width),
        _ => state.list_lines(width),
    };
    // Keep the marked row on the screen, whole.
    let h = usize::from(body_h);
    let top = window_top(state.scroll.get(), h, lines.len(), mark);
    state.scroll.set(top);
    let body: Vec<Line<'static>> = lines.into_iter().skip(top).take(h).collect();
    f.render_widget(Paragraph::new(body), band(area, area.y + 1, body_h));
    let prompt_top = area.y + 1 + body_h;
    f.render_widget(Paragraph::new(prompt), band(area, prompt_top, prompt_h));
    if let Some((col, row)) = cursor
        && prompt_top + row < area.bottom()
    {
        f.set_cursor_position((
            area.x + col.min(area.width.saturating_sub(1)),
            prompt_top + row,
        ));
    }
    let notice_y = prompt_top + prompt_h;
    let notice = if state.busy {
        "writing config.toml…".to_string()
    } else {
        state.notice.clone().unwrap_or_default()
    };
    f.render_widget(
        Paragraph::new(Line::from(format!(
            " {}",
            fit(&sanitize_line(&notice), width.saturating_sub(2), false)
        ))),
        band(area, notice_y, 1),
    );
    let footer = state.footer();
    let key_y = notice_y + 1;
    let line = footer
        .iter()
        .map(|(t, _)| *t)
        .collect::<Vec<_>>()
        .join(" · ");
    f.render_widget(Paragraph::new(Line::from(line)), band(area, key_y, 1));
    // Each part of the key line that names a key is a region: a press does what the key does.
    let mut map = hits.borrow_mut();
    let mut x = usize::from(area.x);
    for (n, (text, key)) in footer.iter().enumerate() {
        if n > 0 {
            x += Span::raw(" · ").width();
        }
        let w = Span::raw(*text).width();
        if let Some((code, name)) = key {
            map.push(
                Rect::new(
                    u16::try_from(x).unwrap_or(u16::MAX),
                    key_y,
                    u16::try_from(w).unwrap_or(u16::MAX),
                    1,
                ),
                Target::Key(*code),
                name,
            );
        }
        x += w;
    }
}
