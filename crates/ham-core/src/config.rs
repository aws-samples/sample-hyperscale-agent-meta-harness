//! The client file, the one config file of the client itself: `$XDG_CONFIG_HOME/ham/config.toml`, else
//! `~/.config/ham/config.toml`. The rules, the skills and (planned) the engine files are other files of the user beside it.
//! Its keys are the rows of the registry in [`crate::settings`]: the nine keys of v4.02 at the top
//! and six more in the tables `[run]`, `[research]` and
//! `[ui]`, which `ham` writes together with `configVersion = 1`. Nothing else is read: an unknown key or table is an error that names
//! its line and the nearest known name, and the same text from a newer `ham` is read for the keys this build knows (see [`read_config`]).
//!
//! In the file the legacy keys are camelCase (`apiEndpoint`, `defaultModel`); `ham config` also takes the snake_case names (`api_endpoint`,
//! `default_model`), and a hand-written `default_model = "..."`, `input_wait_minutes` and `report_tests` are read. A key of a table is
//! written and named in snake case, dotted for `ham config` (`run.goal`).
//!
//! `mouse` says whether the TUI asks the terminal for mouse reports: `true` (the default), `false` or `"auto"` (a
//! list of verified terminals decides). A bare boolean is what the file holds for the first two.

pub use crate::command::MAX_INPUT_WAIT_MINUTES;
use crate::settings::{
    CONFIG_VERSION, Code, Finding, Findings, Key, Kind, MAX_CONFIG_BYTES, MAX_NESTING, ROWS, Row,
    Severity, Slot, TABLES, table_of_key,
};
use harness_protocol::direct::{ComputeMode, ReviewMode};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::path::PathBuf;
use toml::Spanned;
use toml::de::{DeTable, DeValue};

/// The `mouse` setting: ask the terminal for mouse reports (`On`, also the default when the key is absent), never (`Off`), or let the
/// TUI's list of verified terminals decide (`Auto`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseMode {
    Auto,
    On,
    Off,
}

impl MouseMode {
    /// The setting a word names: `true`, `on`, `yes` or `1`; `false`, `off`, `no` or `0`; `auto`. Any case, surrounding space ignored.
    pub fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => Some(Self::On),
            "false" | "off" | "no" | "0" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// The word `ham config get mouse` prints.
    pub fn word(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "true",
            Self::Off => "false",
        }
    }
}

impl Serialize for MouseMode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => s.serialize_str("auto"),
            Self::On => s.serialize_bool(true),
            Self::Off => s.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for MouseMode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Words;
        impl Visitor<'_> for Words {
            type Value = MouseMode;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("true, false or \"auto\"")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<MouseMode, E> {
                Ok(if v { MouseMode::On } else { MouseMode::Off })
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<MouseMode, E> {
                // Only the word `auto` is a string setting; the booleans are written without quotes.
                if v == "auto" {
                    Ok(MouseMode::Auto)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }
        }
        d.deserialize_any(Words)
    }
}

/// The `ui.mascot` setting: the ham mascot in the look the environment asks for (`Auto`, also the default when the key is absent), in the
/// ASCII look whatever the environment says (`Ascii`, for a terminal that draws block characters double wide), or not at all (`Off`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MascotMode {
    Auto,
    Ascii,
    Off,
}

impl MascotMode {
    /// The setting a word names: `auto`, `ascii` or `off`. Any case, surrounding space ignored. No other word is one.
    pub fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "ascii" => Some(Self::Ascii),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// The word `ham config get ui.mascot` prints and the file holds.
    pub fn word(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ascii => "ascii",
            Self::Off => "off",
        }
    }
}

/// The `run.lifecycle_flow` setting: which text of the lifecycle contract `ham rules init` installs as the rule `10-lifecycle.md`.
/// `Auto` (also the default when the key is absent) installs the flow statement, the quick path and the eight steps, and the model picks; `Full` installs
/// the eight steps alone; `Quick` installs the quick path alone. The texts are `lifecycle/contract.src` rendered by `lifecycle/contract.py`; a text rule
/// cannot make a model classify a task correctly, so `Full` and `Quick` are the way to fix the flow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleFlow {
    #[default]
    Auto,
    Full,
    Quick,
}

impl LifecycleFlow {
    /// The three values, in the order the setting names them.
    pub const ALL: [LifecycleFlow; 3] = [Self::Auto, Self::Full, Self::Quick];

    /// The setting a word names: `auto`, `full` or `quick`. Any case, surrounding space ignored. No other word is one.
    pub fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "full" => Some(Self::Full),
            "quick" => Some(Self::Quick),
            _ => None,
        }
    }

    /// The word `ham config get run.lifecycle_flow` prints and the file holds.
    pub fn word(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Full => "full",
            Self::Quick => "quick",
        }
    }
}

/// The keys of `[run]`: what every run asks for unless a flag says otherwise.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTable {
    /// The goal loop (`--goal on|off`); unset leaves the stage's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<bool>,
    /// `false` sends no rules and no skills, like `--no-rules`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<bool>,
    /// `false` sends no skills, like `--no-skills`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<bool>,
    /// Whole minutes one command of the clean-checkout test report may run before it is stopped, 0 to 60. Unset and 0 leave the stage's
    /// own limit (5 minutes unless the stage says otherwise); the request carries it as `reportStepS`, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_command_minutes: Option<u32>,
    /// Tool calls in a row, with no changed file and no new text, that end a run; unset leaves the stage's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_calls: Option<u32>,
    /// Minutes with no changed file that end a run when at least `stall_min_calls` calls came in them; unset leaves the stage's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_window_minutes: Option<u32>,
    /// The least number of calls inside the stall minutes for them to count; unset leaves the stage's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_min_calls: Option<u32>,
    /// Between two changed files, this many tool calls that bring something new (an input no earlier call had, at least 3 lines in the answer that no earlier answer had, and
    /// the call succeeded) are not counted toward `stall_calls` and `stall_min_calls`; 0 counts every call; unset leaves the stage's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_new_calls: Option<u32>,
    /// Minutes with no changed file that a running command may hold a run for; 0 does not look for running commands; unset leaves the stage's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_grace_minutes: Option<u32>,
    /// `false` ends a run at once when a stall ends it, without the last turn for a final message; unset leaves the stage's default (on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<bool>,
    /// Which text of the lifecycle contract `ham rules init` installs: `auto` unless this says `full` or `quick`. Read by `ham rules init`; a run
    /// sends the rule file as it is and carries no request member for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_flow: Option<LifecycleFlow>,
    /// Whether the harness asks the engine to review its own delivery once after it reports done: `auto`, the default, or `off`. A run
    /// carries it as the `review` member of the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_round: Option<ReviewMode>,
    /// Which machine the session runs on: `auto`, the default, `microvm` or `instances`. A run carries `microvm` and `instances` as the
    /// `compute` member of the request; `auto` sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeMode>,
    /// The user's container signals, as `ham config` prints them: entries separated by `;`, each a signal or the word `default`, or the word
    /// `none`. A run carries the entries as `compute.containerSignals`, also for `auto`; unset sends none and the stage's built-in list decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_signals: Option<String>,
    /// `false` leaves cargo's own debug-information setting in the session: the harness then sets neither `CARGO_PROFILE_DEV_DEBUG` nor
    /// `CARGO_PROFILE_TEST_DEBUG`. Unset and `true` keep the default. A run carries `false` as `build.cargoNoDebugInfo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_no_debug_info: Option<bool>,
    /// `false` leaves cargo's own incremental setting in the session: the harness does not set `CARGO_INCREMENTAL`. A run carries `false` as
    /// `build.cargoNoIncremental`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_no_incremental: Option<bool>,
    /// `false` leaves `target/` out of the repository's local exclude list. A run carries `false` as `build.excludeTargetDir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_target_dir: Option<bool>,
    /// `false` leaves the sentence about work outside the checkout out of the goal prompts. A run carries the value as
    /// `goal.outsideCheckoutNote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside_checkout_note: Option<bool>,
    /// Whole hours a run may take in all, 1 to 720. A run whose session ends before it is done continues in a new session until this time is
    /// used up. Unset leaves the stage's own limit (24 hours); the request carries it as `maxRunS`, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_hours: Option<u32>,
    /// Immediate (`/now`) messages one run may make; unset leaves the stage's count (5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_interrupts: Option<u32>,
    /// Questions one run may put to the user; 0 never asks; unset leaves the stage's count (3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_questions: Option<u32>,
    /// Fresh sessions the user may ask for in one run; unset leaves the stage's count (3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fresh_sessions: Option<u32>,
    /// `true` agrees that the prompts and answers of a run on a model that needs it go to the model's provider. Off is
    /// the default and sends no member; on is carried as `providerSharing: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_sharing: Option<bool>,
}

/// The keys of `[research]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchTable {
    /// Web research on or off (`--research`, `--no-research`); unset leaves the stage's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The most MiB one file of the research material may hold; unset leaves the stage's limit (1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_mib: Option<u32>,
    /// The most MiB one unpacked repository may take; unset leaves the stage's limit (200 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unpacked_mib: Option<u32>,
    /// The most MiB of research material in a run; unset leaves the stage's limit (300 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_mib: Option<u32>,
}

/// The keys of `[ui]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTable {
    /// The terminal notification when a run waits and the window is behind; `HAMH_NOTIFY` wins when it is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
    /// The bell on the same events; on unless this says `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bell: Option<bool>,
    /// The ham mascot beside the title and in the empty Runs list: `auto` unless this says `ascii` or `off`; `HAMH_NO_MASCOT=1` wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mascot: Option<MascotMode>,
    /// The notices about the old command name, the old variable names and the old config folder; on unless this says `false`;
    /// `HAMH_NO_DEPRECATION_NOTICES=1` turns them off whatever the file says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation_notices: Option<bool>,
    /// The smoke under the mascot's feet moves, in the title band and in the empty Runs list; on unless this says `false`. `ui.mascot` and
    /// `HAMH_NO_MASCOT` still hide the whole mascot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mascot_motion: Option<bool>,
    /// The most KiB one copy to the clipboard carries; 64 unless this says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_limit_kib: Option<u32>,
}

/// One `[[backend]]` table: a stage in another AWS account that the client reads and starts runs on. The three top-level keys
/// `apiEndpoint`, `region` and `awsProfile` stay the backend named `default`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendTable {
    /// The short name that commands and the screens use (`--backend work`): lower-case letters, digits, `-` and `_`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
    /// Whether this backend may receive a run that is marked as customer content; absent means no.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_content: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<String>,
    /// The model a direct run uses when `--model` and `HAMH_MODEL` are not given (a full `models.json` id once set by `ham`).
    #[serde(
        default,
        alias = "default_model",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_model: Option<String>,
    /// The engine a direct run uses when `--engine` is not given (an id of `crate::engines`, stored in lower case once set by `ham`). `None`
    /// means the default engine.
    #[serde(
        default,
        alias = "default_engine",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_engine: Option<String>,
    /// The git repository `ham update` reads: an `https://` URL, a `file:///` path or `off`. `None` means the public repository
    /// (`crate::update::DEFAULT_SOURCE`).
    #[serde(
        default,
        alias = "update_source",
        skip_serializing_if = "Option::is_none"
    )]
    pub update_source: Option<String>,
    /// How many minutes an interactive `ham` asks a run to wait for the user's answer when Cline needs input (0 to 43200; 0 is no
    /// wait). `None` means the TUI's 30 minutes and no wait for a script.
    #[serde(
        default,
        alias = "input_wait_minutes",
        skip_serializing_if = "Option::is_none"
    )]
    pub input_wait_minutes: Option<u32>,
    /// Whether a run makes the clean-checkout test report; `None` is the default, which is on. Only `false` reaches the
    /// request (`reportTests: false`).
    #[serde(
        default,
        alias = "report_tests",
        skip_serializing_if = "Option::is_none"
    )]
    pub report_tests: Option<bool>,
    /// Whether the TUI asks the terminal for mouse reports; `None` is the default, which is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse: Option<MouseMode>,
    /// The format of the file: written with the first key beyond the nine of v4.02, so that an older `ham` that refuses the file and a
    /// newer `ham` that reads it can tell which one it is. `None` while only the nine keys are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version: Option<u32>,
    /// The backend a start goes to when the command names none: `default` or the alias of a `[[backend]]` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_backend: Option<String>,
    /// Whether the `default` backend may receive a run marked as customer content; absent means no.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_content: Option<bool>,
    /// `[run]`, `[research]` and `[ui]` come after every top-level key, which the file format needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research: Option<ResearchTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiTable>,
    /// The `[[backend]]` tables, in file order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Vec<BackendTable>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config.toml: {0}")]
    Parse(String),
    #[error("unknown config key {key:?}; the keys are {}", known.join(", "))]
    UnknownKey { key: String, known: Vec<String> },
    #[error("{key}: {reason}")]
    Invalid { key: String, reason: String },
    /// The file was written by a newer `ham`; this build changes nothing in it.
    #[error(
        "config.toml was written by a newer ham (configVersion {version}); update ham to change it. Nothing was written"
    )]
    Newer { version: u32 },
}

/// The settable keys as `(file name, name `ham config` prints)`. A key of a table is dotted in both columns.
pub const KEYS: [(&str, &str); 46] = [
    ("apiEndpoint", "api_endpoint"),
    ("region", "region"),
    ("awsProfile", "aws_profile"),
    ("defaultRepo", "default_repo"),
    ("downloadDir", "download_dir"),
    ("defaultModel", "default_model"),
    ("inputWaitMinutes", "input_wait_minutes"),
    ("mouse", "mouse"),
    ("reportTests", "report_tests"),
    ("run.goal", "run.goal"),
    ("run.rules", "run.rules"),
    ("run.skills", "run.skills"),
    ("research.enabled", "research.enabled"),
    ("ui.notify", "ui.notify"),
    ("ui.bell", "ui.bell"),
    ("ui.mascot", "ui.mascot"),
    ("run.report_command_minutes", "run.report_command_minutes"),
    ("run.stall_calls", "run.stall_calls"),
    ("run.stall_window_minutes", "run.stall_window_minutes"),
    ("run.stall_min_calls", "run.stall_min_calls"),
    ("run.command_grace_minutes", "run.command_grace_minutes"),
    ("run.final_message", "run.final_message"),
    ("run.lifecycle_flow", "run.lifecycle_flow"),
    ("run.review_round", "run.review_round"),
    ("run.compute", "run.compute"),
    ("run.stall_new_calls", "run.stall_new_calls"),
    ("run.cargo_no_debug_info", "run.cargo_no_debug_info"),
    ("run.cargo_no_incremental", "run.cargo_no_incremental"),
    ("run.exclude_target_dir", "run.exclude_target_dir"),
    ("run.outside_checkout_note", "run.outside_checkout_note"),
    ("ui.deprecation_notices", "ui.deprecation_notices"),
    ("ui.mascot_motion", "ui.mascot_motion"),
    ("run.max_run_hours", "run.max_run_hours"),
    ("run.provider_sharing", "run.provider_sharing"),
    ("defaultBackend", "default_backend"),
    ("customerContent", "customer_content"),
    ("ui.copy_limit_kib", "ui.copy_limit_kib"),
    ("run.max_interrupts", "run.max_interrupts"),
    ("run.max_questions", "run.max_questions"),
    ("run.max_fresh_sessions", "run.max_fresh_sessions"),
    ("research.max_file_mib", "research.max_file_mib"),
    ("research.max_unpacked_mib", "research.max_unpacked_mib"),
    ("research.max_run_mib", "research.max_run_mib"),
    ("run.container_signals", "run.container_signals"),
    ("defaultEngine", "default_engine"),
    ("updateSource", "update_source"),
];

/// The key a name stands for, in either spelling, or the error that lists the choices.
fn canonical(key: &str) -> Result<Key, ConfigError> {
    Key::lookup(key).ok_or_else(|| ConfigError::UnknownKey {
        key: key.to_string(),
        known: KEYS
            .iter()
            .map(|(_, printed)| (*printed).to_string())
            .collect(),
    })
}

/// True while a key beyond the nine of v4.02 is set: the file then needs `configVersion`.
pub fn version_wanted(cfg: &Config) -> bool {
    let mut copy = cfg.clone();
    cfg.backend.as_ref().is_some_and(|b| !b.is_empty())
        || Key::ALL
            .iter()
            .any(|k| k.row().since > 0 && k.slot(&mut copy).is_set())
}

/// Makes `cfg` the form the file has: a table with no key is no table, and `configVersion` is there exactly while a new key is set (a
/// version a newer `ham` wrote is left alone).
fn normalize(cfg: &mut Config) {
    cfg.run = cfg.run.take().filter(|t| *t != RunTable::default());
    cfg.research = cfg
        .research
        .take()
        .filter(|t| *t != ResearchTable::default());
    cfg.ui = cfg.ui.take().filter(|t| *t != UiTable::default());
    cfg.backend = cfg.backend.take().filter(|b| !b.is_empty());
    if version_wanted(cfg) {
        cfg.config_version.get_or_insert(CONFIG_VERSION);
    } else if cfg.config_version.is_some_and(|v| v <= CONFIG_VERSION) {
        cfg.config_version = None;
    }
}

fn refuse_newer(cfg: &Config) -> Result<(), ConfigError> {
    match cfg.config_version {
        Some(version) if version > CONFIG_VERSION => Err(ConfigError::Newer { version }),
        _ => Ok(()),
    }
}

/// The value of a key, in either spelling; `None` when it is not set.
pub fn get_key(cfg: &Config, key: &str) -> Result<Option<String>, ConfigError> {
    let key = canonical(key)?;
    let mut copy = cfg.clone();
    Ok(key.slot(&mut copy).printed())
}

/// The bool a word names: `true`, `on`, `yes` or `1`; `false`, `off`, `no` or `0` (any case, surrounding space ignored).
fn on_off(word: &str) -> Option<bool> {
    match word.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// `cfg` with the key set. `default_model` takes a model id or alias and stores the id (a model that is unknown or has no passing smoke
/// record is refused, with the choices); a switch takes on or off; every other value must be one line of visible text.
pub fn set_key(cfg: &Config, key: &str, value: &str) -> Result<Config, ConfigError> {
    let key = canonical(key)?;
    let row = key.row();
    let invalid = |reason: String| ConfigError::Invalid {
        key: row.name.to_string(),
        reason,
    };
    refuse_newer(cfg)?;
    let mut out = cfg.clone();
    match (row.kind, key.slot(&mut out)) {
        (Kind::Minutes { max }, Slot::Minutes(slot)) => {
            *slot = Some(
                value
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|m| *m <= max)
                    .ok_or_else(|| {
                        invalid(format!(
                            "takes whole minutes from 0 ({}) to {max}, got {value:?}",
                            row.zero_means()
                        ))
                    })?,
            );
        }
        (Kind::Number { min, max, unit }, Slot::Number(slot)) => {
            *slot = Some(
                value
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .ok_or_else(|| {
                        invalid(format!(
                            "takes a whole number of {unit} from {min} to {max}, got {value:?}"
                        ))
                    })?,
            );
        }
        (Kind::Bool, Slot::Bool(slot)) => {
            *slot = Some(
                on_off(value).ok_or_else(|| invalid(format!("takes on or off, got {value:?}")))?,
            );
        }
        (Kind::Mouse, Slot::Mouse(slot)) => {
            *slot = Some(
                MouseMode::parse(value)
                    .ok_or_else(|| invalid(format!("takes true, false or auto, got {value:?}")))?,
            );
        }
        (Kind::Mascot, Slot::Mascot(slot)) => {
            *slot = Some(
                MascotMode::parse(value)
                    .ok_or_else(|| invalid(format!("takes auto, ascii or off, got {value:?}")))?,
            );
        }
        (Kind::Flow, Slot::Flow(slot)) => {
            *slot = Some(
                LifecycleFlow::parse(value)
                    .ok_or_else(|| invalid(format!("takes auto, full or quick, got {value:?}")))?,
            );
        }
        (Kind::Review, Slot::Review(slot)) => {
            *slot = Some(
                review_mode(value)
                    .ok_or_else(|| invalid(format!("takes auto or off, got {value:?}")))?,
            );
        }
        (Kind::Compute, Slot::Compute(slot)) => {
            *slot = Some(compute_word(value).ok_or_else(|| {
                invalid(format!("takes auto, microvm or instances, got {value:?}"))
            })?);
        }
        (Kind::Signals, Slot::Text(slot)) => {
            let entries = crate::container_signals::parse_setting(value).map_err(invalid)?;
            *slot = Some(crate::container_signals::setting_text(&entries));
        }
        (Kind::Engine, Slot::Text(slot)) => {
            *slot = Some(
                crate::engines::choose(value)
                    .map(|e| e.id.to_string())
                    .map_err(|e| invalid(e.to_string()))?,
            );
        }
        (Kind::Model, Slot::Text(slot)) => {
            let models = crate::models::builtin();
            if value.trim().is_empty() {
                return Err(invalid(format!(
                    "takes a model id or alias; choose one of: {}",
                    models.choices().join(", ")
                )));
            }
            *slot = Some(
                crate::models::resolve_given(models, value)
                    .map(|r| r.model.id.clone())
                    .map_err(|e| invalid(e.to_string()))?,
            );
        }
        (Kind::Text, Slot::Text(slot)) => {
            let v = value.trim();
            if v.is_empty() || v.chars().any(char::is_control) {
                return Err(invalid(
                    "takes one line of text with no control characters".into(),
                ));
            }
            *slot = Some(v.to_string());
        }
        // The registry gives every row the slot of its kind; a row that disagrees is a bug caught by the registry test.
        (kind, _) => {
            return Err(invalid(format!(
                "is registered as {} but not stored as one",
                kind.word()
            )));
        }
    }
    if key == Key::DefaultBackend {
        let wanted = out.default_backend.clone().unwrap_or_default();
        let aliases = crate::backends::aliases_of(&out);
        if !aliases.contains(&wanted) {
            return Err(invalid(format!(
                "names `{wanted}`, which is no backend; the backends are {}",
                aliases.join(", ")
            )));
        }
    }
    normalize(&mut out);
    Ok(out)
}

/// `cfg` with the key removed. A table left with no key goes with its header, and `configVersion` goes with the last new key.
pub fn unset_key(cfg: &Config, key: &str) -> Result<Config, ConfigError> {
    let key = canonical(key)?;
    refuse_newer(cfg)?;
    let mut out = cfg.clone();
    key.slot(&mut out).clear();
    normalize(&mut out);
    Ok(out)
}

/// Config file location from an environment lookup: `config.toml` in the folder [`crate::config_dir::dirs`] decides on, `~/.config/ham`
/// (or under `XDG_CONFIG_HOME`), or, while only the folder of the earlier command name holds the files, that one, read and never written.
// COMPAT-CAH: the choice between the two folders ends with the window (VERSION 5.50).
pub fn config_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    crate::config_dir::dirs(env).map(|d| d.in_use().join("config.toml"))
}

/// Download directory: `HAMH_DOWNLOAD_DIR`, else `downloadDir` from the config.
pub fn download_dir(env: &dyn Fn(&str) -> Option<String>, cfg: &Config) -> Option<PathBuf> {
    env("HAMH_DOWNLOAD_DIR")
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.download_dir.clone())
        .map(PathBuf::from)
}

/// Whether the terminal notification is on: `HAMH_NOTIFY` when it is set (not blank), with its exact parse (`1`, `true` or `on`, lower
/// case; anything else is off), else `ui.notify`, else off.
pub fn notify_on(env: &dyn Fn(&str) -> Option<String>, cfg: &Config) -> bool {
    match env("HAMH_NOTIFY").filter(|v| !v.trim().is_empty()) {
        Some(v) => matches!(v.as_str(), "1" | "true" | "on"),
        None => cfg.ui.as_ref().and_then(|u| u.notify).unwrap_or(false),
    }
}

/// Whether the bell rings: on unless `ui.bell` says `false`.
pub fn bell_on(cfg: &Config) -> bool {
    cfg.ui.as_ref().and_then(|u| u.bell).unwrap_or(true)
}

/// Whether the smoke under the mascot's feet moves: on unless `ui.mascot_motion` says `false`. No flag and no environment variable take part.
pub fn mascot_motion_on(cfg: &Config) -> bool {
    cfg.ui
        .as_ref()
        .and_then(|u| u.mascot_motion)
        .unwrap_or(true)
}

/// The KiB one copy to the clipboard carries when `ui.copy_limit_kib` says nothing.
pub const DEFAULT_COPY_LIMIT_KIB: u32 = 64;

/// The most bytes one copy to the clipboard carries: `ui.copy_limit_kib` of the file, else 64 KiB. No flag and no environment variable take part.
pub fn copy_limit_bytes(cfg: &Config) -> usize {
    let kib = cfg
        .ui
        .as_ref()
        .and_then(|u| u.copy_limit_kib)
        .unwrap_or(DEFAULT_COPY_LIMIT_KIB);
    usize::try_from(kib)
        .unwrap_or(usize::MAX)
        .saturating_mul(1024)
}

/// The lifecycle flow in force: `run.lifecycle_flow` of the file, else `auto`. No environment variable and no flag takes part (the setting is read by
/// `ham rules init`, which installs the matching text of the lifecycle contract).
pub fn lifecycle_flow(cfg: &Config) -> LifecycleFlow {
    cfg.run
        .as_ref()
        .and_then(|r| r.lifecycle_flow)
        .unwrap_or_default()
}

/// The word of `run.review_round`: `auto` or `off`, any case, surrounding space ignored. No other word is one.
fn review_mode(word: &str) -> Option<ReviewMode> {
    ReviewMode::parse(&word.trim().to_ascii_lowercase())
}

/// The review round setting in force: `run.review_round` of the file, else `auto`. No environment variable and no flag takes part.
pub fn review_round(cfg: &Config) -> ReviewMode {
    cfg.run
        .as_ref()
        .and_then(|r| r.review_round)
        .unwrap_or(ReviewMode::Auto)
}

/// The word of `run.compute`: `auto`, `microvm` or `instances`, any case, surrounding space ignored. No other word is one.
pub fn compute_word(word: &str) -> Option<ComputeMode> {
    ComputeMode::parse(&word.trim().to_ascii_lowercase())
}

/// The compute setting in force: `run.compute` of the file, else `auto`. The flag `--compute` is applied on top of it by `request::resolve_run_settings`;
/// no environment variable takes part.
pub fn compute_mode(cfg: &Config) -> ComputeMode {
    cfg.run
        .as_ref()
        .and_then(|r| r.compute)
        .unwrap_or(ComputeMode::Auto)
}

/// The mascot setting in force: `HAMH_NO_MASCOT` set to `1`, `true`, `yes` or `on` (any case) is `Off` whatever the file says; any other
/// value of it, and none, leaves the file's `ui.mascot`, else `Auto`. The variable can only switch the mascot off.
pub fn mascot_mode(env: &dyn Fn(&str) -> Option<String>, cfg: &Config) -> MascotMode {
    let forced_off = env("HAMH_NO_MASCOT").is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    });
    if forced_off {
        return MascotMode::Off;
    }
    cfg.ui
        .as_ref()
        .and_then(|u| u.mascot)
        .unwrap_or(MascotMode::Auto)
}

/// Whether the notices about the earlier names (of the command, of the variables and of the config folder) are printed: `ui.deprecation_notices`
/// of the file, on unless it says `false`, and never when `HAMH_NO_DEPRECATION_NOTICES` is on (the variable can only switch them off).
pub fn deprecation_notices_on(env: &dyn Fn(&str) -> Option<String>, cfg: &Config) -> bool {
    if crate::envnames::silenced_by_env(env) {
        return false;
    }
    cfg.ui
        .as_ref()
        .and_then(|u| u.deprecation_notices)
        .unwrap_or(true)
}

// ---- the reader ------------------------------------------------------------------------------------------------------------------

/// What reading `config.toml` found: the keys it could read, the findings with their lines, and whether the file came from a newer `ham`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigRead {
    /// The keys that were read. With errors among the findings this holds the valid ones; do not act on it.
    pub config: Config,
    pub findings: Findings,
    newer: Option<u32>,
    run_blocked: bool,
}

impl ConfigRead {
    /// An error finding: the file is not valid for this build.
    pub fn has_errors(&self) -> bool {
        self.findings.has_errors()
    }

    /// The file came from a newer `ham`: nothing in it may be changed.
    pub fn writes_blocked(&self) -> bool {
        self.newer.is_some()
    }

    /// No run may start: the file has an error, or a newer `ham` wrote a key or table outside `[ui]` that this build does not know and
    /// that may steer a run.
    pub fn run_blocked(&self) -> bool {
        self.has_errors() || self.run_blocked
    }

    /// The error findings, one per line (at most ten, then a count).
    pub fn render_errors(&self) -> String {
        let all: Vec<String> = self.findings.errors().map(Finding::render).collect();
        let mut text = all.iter().take(10).cloned().collect::<Vec<_>>().join("\n");
        if all.len() > 10 {
            text.push_str(&format!("\n(and {} more)", all.len() - 10));
        }
        text
    }

    /// The text of the refusal to start a run on a newer file.
    fn render_blocked(&self) -> String {
        let version = self.newer.unwrap_or(0);
        let lines: Vec<String> = self
            .findings
            .warnings()
            .filter(|f| f.code != Code::NewerVersion)
            .map(Finding::render)
            .collect();
        format!(
            "this file was written by a newer ham (configVersion {version}) and holds keys this ham does not know that may steer a run:\n{}\nUpdate ham, or remove those keys",
            lines.join("\n")
        )
    }
}

fn sane(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A name for a message: plain names in backticks, anything else as "a key with unusual characters", so that no escape sequence or
/// control character of the file reaches a terminal.
fn shown(name: &str) -> String {
    if sane(name) {
        format!("`{name}`")
    } else {
        "a key with unusual characters".to_string()
    }
}

fn normalized(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// The nearest of `candidates` to `typed`, when it is near enough to be a guess (at most half the longer name apart). Case, `_` and `-`
/// do not count, so `api_endpoint` finds `apiEndpoint`.
fn nearest<'a>(typed: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    if !sane(typed) {
        return None;
    }
    let t = normalized(typed);
    candidates
        .map(|c| (distance(&t, &normalized(c)), c))
        .filter(|&(d, c)| d * 2 <= t.chars().count().max(normalized(c).chars().count()))
        .min_by_key(|&(d, _)| d)
        .map(|(_, c)| c.to_string())
}

/// 1-based line and column (in characters) of a byte offset.
pub(crate) fn position(text: &str, offset: usize) -> (usize, usize) {
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let before = &text[..at];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

/// The deepest nesting of `[` and `{` outside strings and comments, and the byte offset where it first passes `limit`. Table headers count
/// (`[run]` is one level, `[[x]]` two); a value that is not nested deeper than the limit costs the parser a bounded stack.
pub(crate) fn nesting_past(text: &str, limit: usize) -> Option<usize> {
    #[derive(PartialEq)]
    enum In {
        Code,
        Basic,
        BasicMulti,
        Literal,
        LiteralMulti,
        Comment,
    }
    let b = text.as_bytes();
    let (mut state, mut depth, mut i) = (In::Code, 0usize, 0usize);
    while i < b.len() {
        match state {
            In::Code => match b[i] {
                b'#' => state = In::Comment,
                b'"' if b[i..].starts_with(b"\"\"\"") => {
                    state = In::BasicMulti;
                    i += 2;
                }
                b'"' => state = In::Basic,
                b'\'' if b[i..].starts_with(b"'''") => {
                    state = In::LiteralMulti;
                    i += 2;
                }
                b'\'' => state = In::Literal,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > limit {
                        return Some(i);
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            },
            In::Comment => {
                if b[i] == b'\n' {
                    state = In::Code;
                }
            }
            In::Basic | In::BasicMulti => {
                if b[i] == b'\\' {
                    i += 1;
                } else if state == In::Basic && (b[i] == b'"' || b[i] == b'\n') {
                    state = In::Code;
                } else if state == In::BasicMulti && b[i..].starts_with(b"\"\"\"") {
                    state = In::Code;
                    i += 2;
                }
            }
            In::Literal | In::LiteralMulti => {
                if state == In::Literal && (b[i] == b'\'' || b[i] == b'\n') {
                    state = In::Code;
                } else if state == In::LiteralMulti && b[i..].starts_with(b"'''") {
                    state = In::Code;
                    i += 2;
                }
            }
        }
        i += 1;
    }
    None
}

/// What the file holds, for a message: the TOML type, never the value.
fn found(v: &DeValue<'_>) -> &'static str {
    match v {
        DeValue::String(_) => "a string",
        DeValue::Integer(_) => "an integer",
        DeValue::Float(_) => "a float",
        DeValue::Boolean(_) => "a boolean",
        DeValue::Datetime(_) => "a date or time",
        DeValue::Array(_) => "an array",
        DeValue::Table(_) => "a table",
    }
}

/// The name of a key of the parsed document.
fn name_of<'a>(k: &'a Spanned<Cow<'_, str>>) -> &'a str {
    k.get_ref()
}

/// A whole number of a TOML integer, when it fits an `i64`.
fn whole(i: &toml::de::DeInteger<'_>) -> Option<i64> {
    i64::from_str_radix(i.as_str(), i.radix()).ok()
}

/// A value of the file, checked against the kind of its row.
enum Stored {
    Text(String),
    Bool(bool),
    Mouse(MouseMode),
    Mascot(MascotMode),
    Flow(LifecycleFlow),
    Review(ReviewMode),
    Compute(ComputeMode),
    Minutes(u32),
    Number(u32),
}

fn check(row: &Row, value: &DeValue<'_>) -> Result<Stored, (Code, String)> {
    let name = row.name;
    let bad = |takes: &str| {
        (
            Code::BadType,
            format!("`{name}` takes {takes}; the file has {}", found(value)),
        )
    };
    match (row.kind, value) {
        (Kind::Text | Kind::Model | Kind::Engine, DeValue::String(s)) => {
            Ok(Stored::Text(s.to_string()))
        }
        (Kind::Text | Kind::Model | Kind::Engine, _) => Err(bad("text in quotes")),
        (Kind::Bool, DeValue::Boolean(b)) => Ok(Stored::Bool(*b)),
        (Kind::Bool, _) => Err(bad("true or false")),
        (Kind::Mouse, DeValue::Boolean(b)) => Ok(Stored::Mouse(if *b {
            MouseMode::On
        } else {
            MouseMode::Off
        })),
        (Kind::Mouse, DeValue::String(s)) if s == "auto" => Ok(Stored::Mouse(MouseMode::Auto)),
        (Kind::Mouse, _) => Err(bad("true, false or \"auto\"")),
        (Kind::Mascot, DeValue::String(s)) => if matches!(s.as_ref(), "auto" | "ascii" | "off") {
            MascotMode::parse(s).map(Stored::Mascot)
        } else {
            None
        }
        .ok_or_else(|| bad("\"auto\", \"ascii\" or \"off\"")),
        (Kind::Mascot, _) => Err(bad("\"auto\", \"ascii\" or \"off\"")),
        (Kind::Flow, DeValue::String(s)) => if matches!(s.as_ref(), "auto" | "full" | "quick") {
            LifecycleFlow::parse(s).map(Stored::Flow)
        } else {
            None
        }
        .ok_or_else(|| bad("\"auto\", \"full\" or \"quick\"")),
        (Kind::Flow, _) => Err(bad("\"auto\", \"full\" or \"quick\"")),
        (Kind::Review, DeValue::String(s)) => ReviewMode::parse(s)
            .map(Stored::Review)
            .ok_or_else(|| bad("\"auto\" or \"off\"")),
        (Kind::Review, _) => Err(bad("\"auto\" or \"off\"")),
        (Kind::Compute, DeValue::String(s)) => ComputeMode::parse(s)
            .map(Stored::Compute)
            .ok_or_else(|| bad("\"auto\", \"microvm\" or \"instances\"")),
        (Kind::Compute, _) => Err(bad("\"auto\", \"microvm\" or \"instances\"")),
        (Kind::Signals, DeValue::String(s)) => crate::container_signals::parse_setting(s)
            .map(|entries| Stored::Text(crate::container_signals::setting_text(&entries)))
            .map_err(|e| {
                (
                    Code::BadType,
                    format!("`{name}` does not hold a list of container signals: {e}"),
                )
            }),
        (Kind::Signals, _) => Err(bad(
            "text in quotes: signals separated by semicolons, default or none",
        )),
        (Kind::Minutes { max }, DeValue::Integer(i)) => whole(i)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n <= max)
            .map(Stored::Minutes)
            .ok_or_else(|| {
                (
                    Code::OutOfRange,
                    format!("`{name}` takes whole minutes from 0 to {max}"),
                )
            }),
        (Kind::Minutes { max }, _) => Err(bad(&format!("whole minutes from 0 to {max}"))),
        (Kind::Number { min, max, unit }, DeValue::Integer(i)) => whole(i)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (min..=max).contains(n))
            .map(Stored::Number)
            .ok_or_else(|| {
                (
                    Code::OutOfRange,
                    format!("`{name}` takes a whole number of {unit} from {min} to {max}"),
                )
            }),
        (Kind::Number { min, max, unit }, _) => Err(bad(&format!(
            "a whole number of {unit} from {min} to {max}"
        ))),
    }
}

fn assign(cfg: &mut Config, key: Key, value: Stored) {
    match (key.slot(cfg), value) {
        (Slot::Text(slot), Stored::Text(v)) => *slot = Some(v),
        (Slot::Bool(slot), Stored::Bool(v)) => *slot = Some(v),
        (Slot::Mouse(slot), Stored::Mouse(v)) => *slot = Some(v),
        (Slot::Mascot(slot), Stored::Mascot(v)) => *slot = Some(v),
        (Slot::Flow(slot), Stored::Flow(v)) => *slot = Some(v),
        (Slot::Review(slot), Stored::Review(v)) => *slot = Some(v),
        (Slot::Compute(slot), Stored::Compute(v)) => *slot = Some(v),
        (Slot::Minutes(slot), Stored::Minutes(v)) => *slot = Some(v),
        (Slot::Number(slot), Stored::Number(v)) => *slot = Some(v),
        // `check` yields the variant of the row's kind and the registry test pins each row's slot to its kind.
        _ => debug_assert!(false, "{key:?}: a value of another kind than its slot"),
    }
}

/// The rows of one table.
fn rows_in(table: &str) -> impl Iterator<Item = &'static Row> + '_ {
    ROWS.iter().filter(move |r| r.file.0 == table)
}

/// The top-level row a name stands for: the file name or the snake spelling the file accepts.
fn legacy_row(name: &str) -> Option<&'static Row> {
    ROWS.iter()
        .find(|r| r.file.0.is_empty() && (r.file.1 == name || r.file_alias == Some(name)))
}

struct Reader<'a> {
    text: &'a str,
    cfg: Config,
    findings: Findings,
    newer: Option<u32>,
    run_blocked: bool,
    /// Where each legacy key was first seen, for the same key in both spellings: the key, its line and the name used.
    seen: Vec<(Key, usize, String)>,
}

/// The keys of a `[[backend]]` table, in the order the reference lists them.
const BACKEND_KEYS: [&str; 5] = [
    "alias",
    "apiEndpoint",
    "region",
    "awsProfile",
    "customerContent",
];

impl Reader<'_> {
    /// The `[[backend]]` tables. Every table needs a valid, unique alias that is not `default` and an `apiEndpoint`; the other keys are
    /// optional. A fault is an error with the line of its key or table.
    fn backends(&mut self, at: usize, value: &DeValue<'_>) {
        let DeValue::Array(items) = value else {
            let message = format!(
                "`backend` is a list of tables; write each as a [[backend]] section, not as {}",
                found(value)
            );
            self.error(Code::BadType, at, Some("backend"), message);
            return;
        };
        let mut seen: Vec<(String, usize)> = Vec::new();
        let mut out: Vec<BackendTable> = Vec::new();
        for item in items {
            let table_at = item.span().start;
            let DeValue::Table(entries) = item.get_ref() else {
                let message = format!(
                    "`backend` is a list of tables; write each as a [[backend]] section, not as {}",
                    found(item.get_ref())
                );
                self.error(Code::BadType, table_at, Some("backend"), message);
                continue;
            };
            let mut t = BackendTable::default();
            let mut sorted: Vec<_> = entries.iter().collect();
            sorted.sort_by_key(|(k, _)| k.span().start);
            for (k, v) in sorted {
                let key_at = k.span().start;
                let key_name = name_of(k);
                match (key_name, v.get_ref()) {
                    ("alias", DeValue::String(s)) => t.alias = Some(s.to_string()),
                    ("apiEndpoint", DeValue::String(s)) => t.api_endpoint = Some(s.to_string()),
                    ("region", DeValue::String(s)) => t.region = Some(s.to_string()),
                    ("awsProfile", DeValue::String(s)) => t.aws_profile = Some(s.to_string()),
                    ("customerContent", DeValue::Boolean(b)) => t.customer_content = Some(*b),
                    ("alias" | "apiEndpoint" | "region" | "awsProfile", other) => {
                        let message = format!(
                            "`{key_name}` in [[backend]] takes text in quotes; the file has {}",
                            found(other)
                        );
                        self.error(Code::BadType, key_at, Some(key_name), message);
                    }
                    ("customerContent", other) => {
                        let message = format!(
                            "`customerContent` in [[backend]] takes true or false; the file has {}",
                            found(other)
                        );
                        self.error(Code::BadType, key_at, Some(key_name), message);
                    }
                    (other, _) => {
                        let suggestion = nearest(other, BACKEND_KEYS.iter().copied());
                        let mut message = format!("unknown key {} in [[backend]]", shown(other));
                        if let Some(s) = &suggestion {
                            message.push_str(&format!("; did you mean `{s}`?"));
                        }
                        self.unknown(Code::UnknownKey, key_at, other, false, message, suggestion);
                    }
                }
            }
            match t.alias.as_deref() {
                None => self.error(
                    Code::BadType,
                    table_at,
                    Some("alias"),
                    "a [[backend]] table needs an `alias`, the short name that commands use (--backend <alias>)"
                        .to_string(),
                ),
                Some(a) if a == crate::backends::DEFAULT_ALIAS => self.error(
                    Code::BadType,
                    table_at,
                    Some("alias"),
                    "the alias `default` is reserved for the top-level keys apiEndpoint, region and awsProfile; give this backend another alias"
                        .to_string(),
                ),
                Some(a) if !crate::backends::is_alias(a) => self.error(
                    Code::BadType,
                    table_at,
                    Some("alias"),
                    format!(
                        "`alias` takes 1 to {} characters: lower-case letters, digits, `-` and `_`, starting with a letter or digit",
                        crate::backends::ALIAS_MAX
                    ),
                ),
                Some(a) => {
                    let line_now = position(self.text, table_at).0;
                    if let Some((_, line)) = seen.iter().find(|(s, _)| s == a) {
                        let message = format!(
                            "the alias {} repeats the [[backend]] on line {line}; an alias is used once",
                            shown(a)
                        );
                        self.error(Code::DuplicateKey, table_at, Some("alias"), message);
                    } else {
                        seen.push((a.to_string(), line_now));
                    }
                }
            }
            if t.api_endpoint.as_deref().is_none_or(str::is_empty) {
                self.error(
                    Code::BadType,
                    table_at,
                    Some("apiEndpoint"),
                    "a [[backend]] table needs an `apiEndpoint`, the /v1 address of its stage"
                        .to_string(),
                );
            }
            out.push(t);
        }
        self.cfg.backend = Some(out);
    }

    fn add(
        &mut self,
        severity: Severity,
        code: Code,
        offset: usize,
        key: Option<&str>,
        message: String,
        suggestion: Option<String>,
    ) {
        let (line, column) = position(self.text, offset);
        self.findings.push(Finding {
            severity,
            code,
            line: Some(line),
            column: Some(column),
            key: key.filter(|k| sane(k)).map(str::to_string),
            suggestion,
            message,
        });
    }

    fn error(&mut self, code: Code, offset: usize, key: Option<&str>, message: String) {
        self.add(Severity::Error, code, offset, key, message, None);
    }

    /// An unknown name: an error at this build's version, a warning in a file from a newer `ham` (which blocks a run unless the name
    /// lies in `[ui]`).
    fn unknown(
        &mut self,
        code: Code,
        offset: usize,
        name: &str,
        in_ui: bool,
        message: String,
        suggestion: Option<String>,
    ) {
        let severity = if self.newer.is_some() {
            if !in_ui {
                self.run_blocked = true;
            }
            Severity::Warning
        } else {
            Severity::Error
        };
        self.add(severity, code, offset, Some(name), message, suggestion);
    }

    fn version(&mut self, at: usize, value: &DeValue<'_>) {
        let n = match value {
            DeValue::Integer(i) => whole(i)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n >= 1),
            other => {
                let message = format!(
                    "`configVersion` takes a whole number from 1; the file has {}",
                    found(other)
                );
                self.error(Code::BadType, at, Some("configVersion"), message);
                return;
            }
        };
        match n {
            Some(n) => {
                self.cfg.config_version = Some(n);
                if n > CONFIG_VERSION {
                    self.newer = Some(n);
                }
            }
            None => self.error(
                Code::BadVersion,
                at,
                Some("configVersion"),
                "`configVersion` takes a whole number from 1".to_string(),
            ),
        }
    }

    fn store(&mut self, row: &Row, at: usize, value: &DeValue<'_>) {
        match check(row, value) {
            Ok(v) => assign(&mut self.cfg, row.key, v),
            Err((code, message)) => self.error(code, at, Some(row.name), message),
        }
    }

    fn table(&mut self, name: &str, at: usize, value: &DeValue<'_>) {
        let DeValue::Table(entries) = value else {
            let message =
                format!("`{name}` is a table; write it as a [{name}] section, not as a value");
            self.error(Code::BadType, at, Some(name), message);
            return;
        };
        let in_ui = name == "ui";
        let mut sorted: Vec<_> = entries.iter().collect();
        sorted.sort_by_key(|(k, _)| k.span().start);
        for (k, v) in sorted {
            let at = k.span().start;
            let key_name = name_of(k);
            if let Some(row) = rows_in(name).find(|r| r.file.1 == key_name) {
                self.store(row, at, v.get_ref());
                continue;
            }
            let is_table = matches!(v.get_ref(), DeValue::Table(_));
            if !is_table
                && let Some(row) = ROWS.iter().find(|r| {
                    r.file.0.is_empty()
                        && (r.file.1 == key_name
                            || r.file_alias == Some(key_name)
                            || r.name == key_name)
                })
            {
                let message = format!(
                    "{} is a top-level key; move it above the first [table] line, where `{}` belongs",
                    shown(key_name),
                    row.file.1
                );
                self.unknown(Code::MovedKey, at, key_name, in_ui, message, None);
                continue;
            }
            let suggestion = nearest(key_name, rows_in(name).map(|r| r.file.1));
            let mut message = if is_table {
                format!("unknown table {} in [{name}]", shown(key_name))
            } else {
                format!("unknown key {} in [{name}]", shown(key_name))
            };
            if let Some(s) = &suggestion {
                message.push_str(&format!("; did you mean `{s}`?"));
            } else if let Some(t) = table_of_key(key_name).filter(|t| *t != name) {
                message.push_str(&format!("; it is a key of [{t}]"));
            }
            let code = if is_table {
                Code::UnknownTable
            } else {
                Code::UnknownKey
            };
            self.unknown(code, at, key_name, in_ui, message, suggestion);
        }
    }

    fn top_level(&mut self, doc: &DeTable<'_>) {
        let mut sorted: Vec<_> = doc.iter().collect();
        sorted.sort_by_key(|(k, _)| k.span().start);
        // The version first: whether the file is newer decides how its unknown names count.
        if let Some((k, v)) = sorted.iter().find(|(k, _)| name_of(k) == "configVersion") {
            self.version(k.span().start, v.get_ref());
        }
        for (k, v) in sorted {
            let at = k.span().start;
            let name = name_of(k);
            if name == "configVersion" {
                continue;
            }
            if TABLES.contains(&name) {
                self.table(name, at, v.get_ref());
                continue;
            }
            if name == "backend" {
                self.backends(at, v.get_ref());
                continue;
            }
            if let Some(row) = legacy_row(name) {
                if let Some((_, line, first)) = self.seen.iter().find(|(key, _, _)| *key == row.key)
                {
                    let message = format!(
                        "{} repeats {} (line {line}); a key is written once",
                        shown(name),
                        shown(first)
                    );
                    self.error(Code::DuplicateKey, at, Some(name), message);
                    continue;
                }
                self.seen
                    .push((row.key, position(self.text, at).0, name.to_string()));
                self.store(row, at, v.get_ref());
                continue;
            }
            if matches!(v.get_ref(), DeValue::Table(_)) {
                let suggestion = nearest(name, TABLES.iter().copied());
                let mut message = if sane(name) {
                    format!("unknown table [{name}]")
                } else {
                    "unknown table with unusual characters".to_string()
                };
                if let Some(s) = &suggestion {
                    message.push_str(&format!("; did you mean [{s}]?"));
                }
                self.unknown(Code::UnknownTable, at, name, false, message, suggestion);
                continue;
            }
            let belongs = table_of_key(name);
            let top: Vec<&str> = ROWS
                .iter()
                .filter(|r| r.file.0.is_empty())
                .flat_map(|r| std::iter::once(r.file.1).chain(r.file_alias))
                .chain(std::iter::once("configVersion"))
                .collect();
            let suggestion = if belongs.is_some() {
                None
            } else {
                nearest(name, top.iter().copied())
            };
            let mut message = format!("unknown key {}", shown(name));
            if let Some(t) = belongs {
                message.push_str(&format!(
                    "; it is a key of [{t}], so write it below the [{t}] line"
                ));
            } else if let Some(s) = &suggestion {
                message.push_str(&format!("; did you mean `{s}`?"));
            }
            self.unknown(Code::UnknownKey, at, name, false, message, suggestion);
        }
        self.check_default_backend(doc);
    }

    /// `defaultBackend` must be `default` or the alias of a `[[backend]]` table of the same file.
    fn check_default_backend(&mut self, doc: &DeTable<'_>) {
        let Some(wanted) = self.cfg.default_backend.clone() else {
            return;
        };
        if wanted == crate::backends::DEFAULT_ALIAS {
            return;
        }
        let known = crate::backends::aliases_of(&self.cfg);
        if known.contains(&wanted) {
            return;
        }
        let at = doc
            .iter()
            .find(|(k, _)| name_of(k) == "defaultBackend")
            .map_or(0, |(k, _)| k.span().start);
        let message = format!(
            "`defaultBackend` names `{}`, which is no backend; the backends are {}",
            wanted.chars().take(40).collect::<String>(),
            known.join(", ")
        );
        self.error(Code::OutOfRange, at, Some("defaultBackend"), message);
    }
}

/// Reads `text` as `config.toml`. Never fails: the result holds the keys that could be read and the findings, each with its line. The
/// text of a finding never holds a value of the file or a line of it.
///
/// - Over [`MAX_CONFIG_BYTES`], with a NUL byte, with a byte-order mark or nested deeper than [`MAX_NESTING`]: refused before parsing.
/// - A syntax error: one finding with its line and column.
/// - At this build's `configVersion` (or none) every unknown key or table, and every wrong type or range, is an error.
/// - Above it, unknown keys and tables are warnings: the known keys are read, writes are blocked ([`ConfigRead::writes_blocked`]), and a run
///   may not start when an unknown key or table lies outside `[ui]` ([`ConfigRead::run_blocked`]).
pub fn read_config(text: &str) -> ConfigRead {
    let mut r = Reader {
        text,
        cfg: Config::default(),
        findings: Findings::default(),
        newer: None,
        run_blocked: false,
        seen: Vec::new(),
    };
    if text.len() > MAX_CONFIG_BYTES {
        r.findings.push(Finding {
            severity: Severity::Error,
            code: Code::TooLarge,
            line: None,
            column: None,
            key: None,
            suggestion: None,
            message: format!(
                "the file is {} bytes; the most ham reads is {MAX_CONFIG_BYTES}",
                text.len()
            ),
        });
    } else if let Some(at) = text.find('\0') {
        r.error(
            Code::Nul,
            at,
            None,
            "the file holds a NUL byte; it is not a text file".to_string(),
        );
    } else if text.starts_with('\u{feff}') {
        r.error(
            Code::Bom,
            0,
            None,
            "the file starts with a byte-order mark; save it as plain UTF-8 without one"
                .to_string(),
        );
    } else if let Some(at) = nesting_past(text, MAX_NESTING) {
        r.error(
            Code::TooDeep,
            at,
            None,
            format!(
                "arrays and tables nest more than {MAX_NESTING} deep; no key of the file needs that"
            ),
        );
    } else {
        match DeTable::parse(text) {
            Err(e) => {
                let at = e.span().map_or(0, |s| s.start);
                r.error(Code::Syntax, at, None, e.message().to_string());
            }
            Ok(doc) => {
                r.top_level(doc.get_ref());
                if let Some(version) = r.newer {
                    let at = doc
                        .get_ref()
                        .iter()
                        .find(|(k, _)| name_of(k) == "configVersion")
                        .map_or(0, |(k, _)| k.span().start);
                    let message = format!(
                        "written by a newer ham (configVersion {version}); this ham reads the keys it knows and changes nothing in the file"
                    );
                    r.add(
                        Severity::Warning,
                        Code::NewerVersion,
                        at,
                        Some("configVersion"),
                        message,
                        None,
                    );
                }
            }
        }
    }
    r.findings.sort();
    ConfigRead {
        config: r.cfg,
        findings: r.findings,
        newer: r.newer,
        run_blocked: r.run_blocked,
    }
}

/// Reads `text` as `config.toml` for a command that needs a valid file: the keys, or the findings that refuse it (one line each, with
/// its line number). A file from a newer `ham` is read only when nothing in it that this build does not know may steer a run.
pub fn parse_config(text: &str) -> Result<Config, ConfigError> {
    let read = read_config(text);
    if read.has_errors() {
        return Err(ConfigError::Parse(read.render_errors()));
    }
    if read.run_blocked() {
        return Err(ConfigError::Parse(read.render_blocked()));
    }
    Ok(read.config)
}

/// A read that found one thing wrong with the file as a whole and no key; `at` is its line and column when it has them.
fn refused(code: Code, at: Option<(usize, usize)>, message: String) -> ConfigRead {
    let mut findings = Findings::default();
    findings.push(Finding {
        severity: Severity::Error,
        code,
        line: at.map(|(line, _)| line),
        column: at.map(|(_, column)| column),
        key: None,
        suggestion: None,
        message,
    });
    ConfigRead {
        config: Config::default(),
        findings,
        newer: None,
        run_blocked: false,
    }
}

/// [`read_config`] for the bytes of the file. `bytes` is what the caller read, at most [`MAX_CONFIG_BYTES`] and one more, so a huge file is
/// never held in memory; `file_len` is the size on disk, which the message of a refusal by size names. Bytes that are not UTF-8 are an error
/// finding that names no content.
pub fn read_config_bytes(bytes: &[u8], file_len: u64) -> ConfigRead {
    if file_len > MAX_CONFIG_BYTES as u64 || bytes.len() > MAX_CONFIG_BYTES {
        return refused(
            Code::TooLarge,
            None,
            format!(
                "the file is {} bytes; the most ham reads is {MAX_CONFIG_BYTES}",
                file_len.max(bytes.len() as u64)
            ),
        );
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => read_config(text),
        Err(e) => {
            let valid = String::from_utf8_lossy(&bytes[..e.valid_up_to()]);
            refused(
                Code::Syntax,
                Some(position(&valid, valid.len())),
                "the file is not valid UTF-8 text; save it as plain UTF-8".to_string(),
            )
        }
    }
}

/// [`parse_config`] for the bytes of the file (see [`read_config_bytes`]).
pub fn parse_config_bytes(bytes: &[u8], file_len: u64) -> Result<Config, ConfigError> {
    let read = read_config_bytes(bytes, file_len);
    if read.has_errors() {
        return Err(ConfigError::Parse(read.render_errors()));
    }
    if read.run_blocked() {
        return Err(ConfigError::Parse(read.render_blocked()));
    }
    Ok(read.config)
}

pub fn render_config(cfg: &Config) -> String {
    // A struct of optional text, numbers and booleans, and tables of the same, always serializes.
    toml::to_string(cfg).unwrap_or_default()
}
