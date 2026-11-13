//! The registry of the client's settings: one [`Row`] for each key of `config.toml`. The reader (`config::read_config`), `ham config`, the
//! run-settings resolver (`request::resolve_run_settings`) and the reference table (`ham config docs`, the user guide) take their facts from
//! these rows; no list of keys is written anywhere else. A key is a variant of [`Key`], so a `match` on it that misses a variant does not
//! compile and no catch-all arm can send an unnamed key to another key's field.
//!
//! The five rows at the end
//! are the limits of the brake that ends a run which polls and changes nothing, the grace of a running command and the
//! switch of the last turn for a final message; they ride in the request's `goal` member. The row after them is
//! read by `ham rules init` and rides in no request: it picks which of the three texts of the lifecycle contract becomes the rule `10-lifecycle.md`.
//! The row after them is the switch of the review round; it rides in the request's `review` member, which also carries the
//! body of the user's `review.md`. The row after it picks the kind of machine the session runs on; it rides in the request's `compute`
//! member, which `auto` (the default) leaves out. The next row (`run.stall_new_calls`, protocol 1.46.0) is the allowance of tool calls that bring something new,
//! which the brake of the first five rows does not count; it rides in the request's `goal` member, and 0 counts every call. The last four rows
//! are switches that turn off a default of the
//! session; each is on unless the file says `false`, which is the only value that reaches a request (the first three in the `build` member, the last in the `goal` member).
//!
//! The last row (`ui.mascot_motion`) is the switch of the smoke that moves under the mascot's feet in the title band and in the empty Runs list; it is on unless the file says `false`, and
//! it rides in no request.
//!
//! The first nine rows are the keys of v4.02, with the names, types, ranges, environment variables and spellings they always had. The
//! six after them (`run.goal`, `run.rules`, `run.skills`, `research.enabled`, `ui.notify`, `ui.bell`) and the ones added later (`ui.mascot`,
//! `run.report_command_minutes`) live in the tables `[run]`,
//! `[research]` and `[ui]`; a file that holds one also holds `configVersion = 1`, which `ham` writes with the first of them. A row
//! exists only together with a reader that consumes it (rule R-consumer): every row here is read by a start of a run, by the terminal
//! app or, for `run.lifecycle_flow`, by `ham rules init`. The keys of the engine file and the keys that wait for a backend are added with their readers.
//!
//! The module names no engine: the rows of an engine's settings are not client keys.

use crate::config::{
    Config, LifecycleFlow, MascotMode, MouseMode, ResearchTable, RunTable, UiTable,
};
use harness_protocol::direct::ComputeMode;
/// A mebibyte: the unit of the three research budget keys.
const MIB: u64 = 1024 * 1024;
use harness_protocol::direct::{
    DENIED, GOAL_COMMAND_GRACE_S, GOAL_FINAL_MESSAGE, GOAL_NEW_CALLS, GOAL_ROUND_CALLS,
    GOAL_ROUND_MIN_CALLS, GOAL_ROUND_WINDOW_S, ReviewMode,
};
use harness_protocol::direct::{
    LIMIT_FRESH_SESSIONS, LIMIT_INTERRUPTS, LIMIT_QUESTIONS, LIMIT_RESEARCH_FILE_BYTES,
    LIMIT_RESEARCH_RUN_BYTES, LIMIT_RESEARCH_UNPACKED_BYTES,
};

/// The `configVersion` this build writes and understands. A file with a higher number was written by a newer `ham`.
pub const CONFIG_VERSION: u32 = 1;

/// The largest `config.toml` the reader looks at.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// The deepest nesting of arrays and inline tables the reader parses. The `toml` crate stops at 80 levels, but a level costs several
/// kilobytes of stack in an unoptimized build, so the reader refuses a deeper value before it parses. No key of the file needs more than 2.
pub const MAX_NESTING: usize = 16;

/// One setting of the client. The variants follow the registry order, which is the order of [`ROWS`] and of `config::KEYS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    ApiEndpoint,
    Region,
    AwsProfile,
    DefaultRepo,
    DownloadDir,
    DefaultModel,
    InputWaitMinutes,
    Mouse,
    ReportTests,
    RunGoal,
    RunRules,
    RunSkills,
    ResearchEnabled,
    UiNotify,
    UiBell,
    UiMascot,
    RunReportCommandMinutes,
    RunStallCalls,
    RunStallWindowMinutes,
    RunStallMinCalls,
    RunCommandGraceMinutes,
    RunFinalMessage,
    RunLifecycleFlow,
    RunReviewRound,
    RunCompute,
    RunStallNewCalls,
    RunCargoNoDebugInfo,
    RunCargoNoIncremental,
    RunExcludeTargetDir,
    RunOutsideCheckoutNote,
    UiDeprecationNotices,
    UiMascotMotion,
    RunMaxRunHours,
    RunProviderSharing,
    DefaultBackend,
    CustomerContent,
    UiCopyLimitKib,
    RunMaxInterrupts,
    RunMaxQuestions,
    RunMaxFreshSessions,
    ResearchMaxFileMib,
    ResearchMaxUnpackedMib,
    ResearchMaxRunMib,
    RunContainerSignals,
    DefaultEngine,
    UpdateSource,
}

impl Key {
    /// Every key, in registry order.
    pub const ALL: [Key; 46] = [
        Key::ApiEndpoint,
        Key::Region,
        Key::AwsProfile,
        Key::DefaultRepo,
        Key::DownloadDir,
        Key::DefaultModel,
        Key::InputWaitMinutes,
        Key::Mouse,
        Key::ReportTests,
        Key::RunGoal,
        Key::RunRules,
        Key::RunSkills,
        Key::ResearchEnabled,
        Key::UiNotify,
        Key::UiBell,
        Key::UiMascot,
        Key::RunReportCommandMinutes,
        Key::RunStallCalls,
        Key::RunStallWindowMinutes,
        Key::RunStallMinCalls,
        Key::RunCommandGraceMinutes,
        Key::RunFinalMessage,
        Key::RunLifecycleFlow,
        Key::RunReviewRound,
        Key::RunCompute,
        Key::RunStallNewCalls,
        Key::RunCargoNoDebugInfo,
        Key::RunCargoNoIncremental,
        Key::RunExcludeTargetDir,
        Key::RunOutsideCheckoutNote,
        Key::UiDeprecationNotices,
        Key::UiMascotMotion,
        Key::RunMaxRunHours,
        Key::RunProviderSharing,
        Key::DefaultBackend,
        Key::CustomerContent,
        Key::UiCopyLimitKib,
        Key::RunMaxInterrupts,
        Key::RunMaxQuestions,
        Key::RunMaxFreshSessions,
        Key::ResearchMaxFileMib,
        Key::ResearchMaxUnpackedMib,
        Key::ResearchMaxRunMib,
        Key::RunContainerSignals,
        Key::DefaultEngine,
        Key::UpdateSource,
    ];

    /// The row of this key.
    pub fn row(self) -> &'static Row {
        &ROWS[self as usize]
    }

    /// The key a name stands for: the name in the file (`apiEndpoint`, `run.goal`) or the name `ham config` prints (`api_endpoint`).
    pub fn lookup(name: &str) -> Option<Key> {
        ROWS.iter()
            .find(|r| r.name == name || r.file_name_is(name))
            .map(|r| r.key)
    }

    /// The slot of this key in `cfg`. A key of a table creates the table on the way; callers that only read use a copy.
    pub(crate) fn slot(self, cfg: &mut Config) -> Slot<'_> {
        match self {
            Key::ApiEndpoint => Slot::Text(&mut cfg.api_endpoint),
            Key::Region => Slot::Text(&mut cfg.region),
            Key::AwsProfile => Slot::Text(&mut cfg.aws_profile),
            Key::DefaultRepo => Slot::Text(&mut cfg.default_repo),
            Key::DownloadDir => Slot::Text(&mut cfg.download_dir),
            Key::DefaultModel => Slot::Text(&mut cfg.default_model),
            Key::InputWaitMinutes => Slot::Minutes(&mut cfg.input_wait_minutes),
            Key::Mouse => Slot::Mouse(&mut cfg.mouse),
            Key::ReportTests => Slot::Bool(&mut cfg.report_tests),
            Key::RunGoal => Slot::Bool(&mut cfg.run.get_or_insert_with(RunTable::default).goal),
            Key::RunRules => Slot::Bool(&mut cfg.run.get_or_insert_with(RunTable::default).rules),
            Key::RunSkills => Slot::Bool(&mut cfg.run.get_or_insert_with(RunTable::default).skills),
            Key::ResearchEnabled => Slot::Bool(
                &mut cfg
                    .research
                    .get_or_insert_with(ResearchTable::default)
                    .enabled,
            ),
            Key::UiNotify => Slot::Bool(&mut cfg.ui.get_or_insert_with(UiTable::default).notify),
            Key::UiBell => Slot::Bool(&mut cfg.ui.get_or_insert_with(UiTable::default).bell),
            Key::UiMascot => Slot::Mascot(&mut cfg.ui.get_or_insert_with(UiTable::default).mascot),
            Key::RunReportCommandMinutes => Slot::Minutes(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .report_command_minutes,
            ),
            Key::RunStallCalls => {
                Slot::Number(&mut cfg.run.get_or_insert_with(RunTable::default).stall_calls)
            }
            Key::RunStallWindowMinutes => Slot::Number(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .stall_window_minutes,
            ),
            Key::RunStallMinCalls => Slot::Number(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .stall_min_calls,
            ),
            Key::RunCommandGraceMinutes => Slot::Number(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .command_grace_minutes,
            ),
            Key::RunFinalMessage => {
                Slot::Bool(&mut cfg.run.get_or_insert_with(RunTable::default).final_message)
            }
            Key::RunLifecycleFlow => {
                Slot::Flow(&mut cfg.run.get_or_insert_with(RunTable::default).lifecycle_flow)
            }
            Key::RunReviewRound => {
                Slot::Review(&mut cfg.run.get_or_insert_with(RunTable::default).review_round)
            }
            Key::RunStallNewCalls => Slot::Number(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .stall_new_calls,
            ),
            Key::RunCompute => {
                Slot::Compute(&mut cfg.run.get_or_insert_with(RunTable::default).compute)
            }
            Key::RunCargoNoDebugInfo => Slot::Bool(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .cargo_no_debug_info,
            ),
            Key::RunCargoNoIncremental => Slot::Bool(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .cargo_no_incremental,
            ),
            Key::RunExcludeTargetDir => Slot::Bool(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .exclude_target_dir,
            ),
            Key::RunOutsideCheckoutNote => Slot::Bool(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .outside_checkout_note,
            ),
            Key::UiDeprecationNotices => Slot::Bool(
                &mut cfg
                    .ui
                    .get_or_insert_with(UiTable::default)
                    .deprecation_notices,
            ),
            Key::UiMascotMotion => {
                Slot::Bool(&mut cfg.ui.get_or_insert_with(UiTable::default).mascot_motion)
            }
            Key::RunMaxRunHours => {
                Slot::Number(&mut cfg.run.get_or_insert_with(RunTable::default).max_run_hours)
            }
            Key::DefaultBackend => Slot::Text(&mut cfg.default_backend),
            Key::CustomerContent => Slot::Bool(&mut cfg.customer_content),
            Key::UiCopyLimitKib => {
                Slot::Number(&mut cfg.ui.get_or_insert_with(UiTable::default).copy_limit_kib)
            }
            Key::RunMaxInterrupts => {
                Slot::Number(&mut cfg.run.get_or_insert_with(RunTable::default).max_interrupts)
            }
            Key::RunMaxQuestions => {
                Slot::Number(&mut cfg.run.get_or_insert_with(RunTable::default).max_questions)
            }
            Key::RunMaxFreshSessions => Slot::Number(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .max_fresh_sessions,
            ),
            Key::ResearchMaxFileMib => Slot::Number(
                &mut cfg
                    .research
                    .get_or_insert_with(ResearchTable::default)
                    .max_file_mib,
            ),
            Key::ResearchMaxUnpackedMib => Slot::Number(
                &mut cfg
                    .research
                    .get_or_insert_with(ResearchTable::default)
                    .max_unpacked_mib,
            ),
            Key::ResearchMaxRunMib => Slot::Number(
                &mut cfg
                    .research
                    .get_or_insert_with(ResearchTable::default)
                    .max_run_mib,
            ),
            Key::RunContainerSignals => Slot::Text(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .container_signals,
            ),
            Key::DefaultEngine => Slot::Text(&mut cfg.default_engine),
            Key::UpdateSource => Slot::Text(&mut cfg.update_source),
            Key::RunProviderSharing => Slot::Bool(
                &mut cfg
                    .run
                    .get_or_insert_with(RunTable::default)
                    .provider_sharing,
            ),
        }
    }
}

/// A key's place in a [`Config`], typed by what it holds.
pub(crate) enum Slot<'a> {
    Text(&'a mut Option<String>),
    Bool(&'a mut Option<bool>),
    Minutes(&'a mut Option<u32>),
    /// A whole number of the kind [`Kind::Number`] (calls or minutes).
    Number(&'a mut Option<u32>),
    Mouse(&'a mut Option<MouseMode>),
    Mascot(&'a mut Option<MascotMode>),
    Flow(&'a mut Option<LifecycleFlow>),
    Review(&'a mut Option<ReviewMode>),
    Compute(&'a mut Option<ComputeMode>),
}

impl Slot<'_> {
    pub(crate) fn is_set(&self) -> bool {
        match self {
            Slot::Text(s) => s.is_some(),
            Slot::Bool(s) => s.is_some(),
            Slot::Minutes(s) | Slot::Number(s) => s.is_some(),
            Slot::Mouse(s) => s.is_some(),
            Slot::Mascot(s) => s.is_some(),
            Slot::Flow(s) => s.is_some(),
            Slot::Review(s) => s.is_some(),
            Slot::Compute(s) => s.is_some(),
        }
    }

    pub(crate) fn clear(self) {
        match self {
            Slot::Text(s) => *s = None,
            Slot::Bool(s) => *s = None,
            Slot::Minutes(s) | Slot::Number(s) => *s = None,
            Slot::Mouse(s) => *s = None,
            Slot::Mascot(s) => *s = None,
            Slot::Flow(s) => *s = None,
            Slot::Review(s) => *s = None,
            Slot::Compute(s) => *s = None,
        }
    }

    /// The value as `ham config get` prints it.
    pub(crate) fn printed(&self) -> Option<String> {
        match self {
            Slot::Text(s) => s.as_ref().map(ToString::to_string),
            Slot::Bool(s) => s.map(|b| b.to_string()),
            Slot::Minutes(s) | Slot::Number(s) => s.map(|m| m.to_string()),
            Slot::Mouse(s) => s.map(|m| m.word().to_string()),
            Slot::Mascot(s) => s.map(|m| m.word().to_string()),
            Slot::Flow(s) => s.map(|m| m.word().to_string()),
            Slot::Review(s) => s.map(|m| m.word().to_string()),
            Slot::Compute(s) => s.map(|m| m.word().to_string()),
        }
    }
}

/// What a key holds and how a value is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One line of text with no control characters.
    Text,
    /// A model id or alias, checked against the model list when set and stored as the full id.
    Model,
    /// Whole minutes from 0 to `max`.
    Minutes { max: u32 },
    /// A whole number from `min` to `max` of `unit` (`calls` or `minutes`). A key whose `min` is 0 reads 0 as "off"
    /// ([`Kind::number_text`]).
    Number {
        min: u32,
        max: u32,
        unit: &'static str,
    },
    /// A TOML boolean; `ham config set` takes on, off, true, false, yes, no, 1 and 0.
    Bool,
    /// A TOML boolean or the word `auto`.
    Mouse,
    /// One of the words `auto`, `ascii` and `off`.
    Mascot,
    /// One of the words `auto`, `full` and `quick`: the text of the lifecycle contract that `ham rules init` installs.
    Flow,
    /// One of the words `auto` and `off`: whether the harness asks the engine to review its own delivery once.
    Review,
    /// One of the words `auto`, `microvm` and `instances`: the kind of machine the session runs on.
    Compute,
    /// Container signals: entries separated by semicolons, each a signal or the word `default`, or the one word `none`. Stored as text.
    Signals,
    /// The id of an engine a person can choose (`crate::engines`), stored as the lower-case id.
    Engine,
}

impl Kind {
    /// A short word for the kind.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Model => "model",
            Kind::Minutes { .. } => "minutes",
            Kind::Number { .. } => "number",
            Kind::Bool => "bool",
            Kind::Mouse => "mouse",
            Kind::Mascot => "mascot",
            Kind::Flow => "flow",
            Kind::Review => "review",
            Kind::Compute => "compute",
            Kind::Signals => "signals",
            Kind::Engine => "engine",
        }
    }

    /// A whole number of a [`Kind::Number`] as a person reads it, whether it is the default or a value the user set: the value with its unit
    /// ("45 calls"), the unit in the singular for 1 ("1 minute"), and "off" for the 0 of a key whose least value is 0 (the command grace).
    /// `None` for a kind that is not a number.
    pub fn number_text(self, value: &str) -> Option<String> {
        let Kind::Number { min, unit, .. } = self else {
            return None;
        };
        if min == 0 && value == "0" {
            return Some("off".to_string());
        }
        let unit = if value == "1" {
            unit.strip_suffix('s').unwrap_or(unit)
        } else {
            unit
        };
        Some(format!("{value} {unit}"))
    }

    /// The type column of the reference table.
    pub fn type_text(self) -> String {
        match self {
            Kind::Text => "text".into(),
            Kind::Model => "model id or alias".into(),
            Kind::Minutes { max } => format!("whole minutes, 0 to {max}"),
            Kind::Number { min, max, unit } => format!("whole {unit}, {min} to {max}"),
            Kind::Bool => "true or false".into(),
            Kind::Mouse => "true, false or \"auto\"".into(),
            Kind::Mascot => "auto, ascii or off".into(),
            Kind::Flow => "auto, full or quick".into(),
            Kind::Review => "auto or off".into(),
            Kind::Compute => "auto, microvm or instances".into(),
            Kind::Signals => "signals separated by semicolons, default or none".into(),
            Kind::Engine => "an engine id".into(),
        }
    }
}

/// The default of a key where there is one the client can state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultValue {
    /// The client has no default: the stage decides, or the value depends on the terminal.
    Absent,
    Bool(bool),
    Minutes(u32),
    /// The default of a [`Kind::Number`] key.
    Number(u32),
    Text(&'static str),
}

/// When a changed value takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    Restart,
    NextComposer,
    NextRun,
    Now,
}

impl Applies {
    pub fn label(self) -> &'static str {
        match self {
            Applies::Restart => "restart",
            Applies::NextComposer => "next composer",
            Applies::NextRun => "next run",
            Applies::Now => "now",
        }
    }
}

/// Which layers take part and in which order. One rule (flag, environment, file, default) with the exceptions v4.02 has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// The flag, then the environment variable, then the file, then the default.
    FlagEnvFile,
    /// The flag, then the file, then the default; there is no environment variable.
    FlagFile,
    /// The file only.
    FileOnly,
    /// The file, then the environment variable: the Region and the profile read the file first.
    FileOverEnv,
    /// The environment variable, then the file; no flag.
    EnvOverFile,
    /// The environment variable can only switch the setting off, whatever the file says.
    EnvForcesOff,
}

/// Where the Settings screen groups a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Connection,
    Paths,
    Run,
    Interface,
    /// The limits of the stall brake and the switch of its last turn: advanced settings of a run, drawn after the paths.
    Limits,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Connection => "Connection",
            Group::Paths => "Paths",
            Group::Run => "Run",
            Group::Interface => "Interface",
            Group::Limits => "Run limits",
        }
    }
}

/// One setting: where it lives in the file, what it holds, and the facts every consumer needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub key: Key,
    /// `(table, name)` in the file; the table is empty for a top-level key.
    pub file: (&'static str, &'static str),
    /// The name `ham config` prints and takes (the file name in snake case; dotted for a key of a table).
    pub name: &'static str,
    /// The snake spelling the file also accepts, for the three keys that always had one.
    pub file_alias: Option<&'static str>,
    pub kind: Kind,
    pub default: DefaultValue,
    /// The default as the reference table and the Settings screen say it.
    pub default_text: &'static str,
    /// The flag that sets it for one run; empty when there is none.
    pub flag: &'static str,
    /// The environment variable that takes part; empty when there is none.
    pub env: &'static str,
    pub order: Order,
    pub applies: Applies,
    /// The request member the value goes into; `None` when the key adds nothing to a request.
    pub wire: Option<&'static str>,
    /// Changing it needs a second Enter and the key typed out: it decides where requests go, who signs them or where files land.
    pub security: bool,
    /// The `configVersion` that introduced it; 0 for the keys v4.02 reads, which need no version in the file.
    pub since: u32,
    pub group: Group,
    pub label: &'static str,
    pub help: &'static str,
}

impl Row {
    /// True when `name` is this row's name in the file, dotted for a key of a table (`run.goal`).
    fn file_name_is(&self, name: &str) -> bool {
        match self.file {
            ("", n) => n == name,
            (t, n) => name
                .strip_prefix(t)
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|rest| rest == n),
        }
    }

    /// What 0 means for a key of whole minutes, in the words of the error that names the range: the answer wait does not wait, the others
    /// leave the limit to the stage.
    pub fn zero_means(&self) -> &'static str {
        if self.key == Key::InputWaitMinutes {
            "do not wait"
        } else {
            "use the stage default"
        }
    }

    /// The name in the file, dotted for a key of a table.
    pub fn file_name(&self) -> String {
        match self.file {
            ("", n) => n.to_string(),
            (t, n) => format!("{t}.{n}"),
        }
    }
}

const fn row(
    key: Key,
    file: (&'static str, &'static str),
    name: &'static str,
    kind: Kind,
    label: &'static str,
    help: &'static str,
) -> Row {
    Row {
        key,
        file,
        name,
        file_alias: None,
        kind,
        default: DefaultValue::Absent,
        default_text: "none",
        flag: "",
        env: "",
        order: Order::FileOnly,
        applies: Applies::NextRun,
        wire: None,
        security: false,
        since: 0,
        group: Group::Run,
        label,
        help,
    }
}

/// The registry, in the order of [`Key`].
pub static ROWS: [Row; 46] = [
    Row {
        applies: Applies::Restart,
        security: true,
        group: Group::Connection,
        ..row(
            Key::ApiEndpoint,
            ("", "apiEndpoint"),
            "api_endpoint",
            Kind::Text,
            "Service address",
            "The /v1 address of your stage: https, or http to this machine for tests.",
        )
    },
    Row {
        default_text: "AWS chain",
        env: "AWS_REGION",
        order: Order::FileOverEnv,
        applies: Applies::Restart,
        security: true,
        group: Group::Connection,
        ..row(
            Key::Region,
            ("", "region"),
            "region",
            Kind::Text,
            "Region",
            "The AWS Region that signs requests. The file wins over AWS_REGION.",
        )
    },
    Row {
        default_text: "AWS chain",
        env: "AWS_PROFILE",
        order: Order::FileOverEnv,
        applies: Applies::Restart,
        security: true,
        group: Group::Connection,
        ..row(
            Key::AwsProfile,
            ("", "awsProfile"),
            "aws_profile",
            Kind::Text,
            "AWS profile",
            "The profile that supplies credentials. The file wins over AWS_PROFILE.",
        )
    },
    Row {
        flag: "--repo",
        order: Order::FlagFile,
        applies: Applies::NextComposer,
        group: Group::Paths,
        ..row(
            Key::DefaultRepo,
            ("", "defaultRepo"),
            "default_repo",
            Kind::Text,
            "Default repository",
            "Fills the composer's Repository box, for example cc:my-service.",
        )
    },
    Row {
        default_text: "current folder",
        env: "HAMH_DOWNLOAD_DIR",
        order: Order::EnvOverFile,
        applies: Applies::Restart,
        security: true,
        group: Group::Paths,
        ..row(
            Key::DownloadDir,
            ("", "downloadDir"),
            "download_dir",
            Kind::Text,
            "Download folder",
            "Where zips are saved. HAMH_DOWNLOAD_DIR wins over the file.",
        )
    },
    Row {
        file_alias: Some("default_model"),
        default: DefaultValue::Text(crate::models::DEFAULT_MODEL_ID),
        default_text: "Claude Sonnet 5.5",
        flag: "--model",
        env: "HAMH_MODEL",
        order: Order::FlagEnvFile,
        wire: Some("model"),
        ..row(
            Key::DefaultModel,
            ("", "defaultModel"),
            "default_model",
            Kind::Model,
            "Model",
            "The model a run uses when you name none. Stored as the full model id.",
        )
    },
    Row {
        file_alias: Some("input_wait_minutes"),
        default_text: "30 (terminal), 0 (script)",
        flag: "--input-wait",
        env: "HAMH_INPUT_WAIT",
        order: Order::FlagEnvFile,
        wire: Some("inputWaitS"),
        ..row(
            Key::InputWaitMinutes,
            ("", "inputWaitMinutes"),
            "input_wait_minutes",
            Kind::Minutes {
                max: crate::command::MAX_INPUT_WAIT_MINUTES,
            },
            "Answer wait",
            "Minutes a run waits for your answer when it needs input; 0 does not wait.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        env: "HAMH_NO_MOUSE",
        order: Order::EnvForcesOff,
        applies: Applies::Restart,
        group: Group::Interface,
        ..row(
            Key::Mouse,
            ("", "mouse"),
            "mouse",
            Kind::Mouse,
            "Mouse",
            "Whether the terminal app asks for mouse reports: true, false or auto.",
        )
    },
    Row {
        file_alias: Some("report_tests"),
        default: DefaultValue::Bool(true),
        default_text: "on",
        wire: Some("reportTests"),
        ..row(
            Key::ReportTests,
            ("", "reportTests"),
            "report_tests",
            Kind::Bool,
            "Test report",
            "Whether a run makes the clean-checkout test report. Only off is sent.",
        )
    },
    Row {
        default_text: "stage default (on)",
        flag: "--goal on|off",
        order: Order::FlagFile,
        wire: Some("goal"),
        since: 1,
        ..row(
            Key::RunGoal,
            ("run", "goal"),
            "run.goal",
            Kind::Bool,
            "Goal loop",
            "Keep the run going until it reports done. Off asks for no loop.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        flag: "--no-rules, --rules <dir>",
        order: Order::FlagFile,
        wire: Some("rules"),
        since: 1,
        ..row(
            Key::RunRules,
            ("run", "rules"),
            "run.rules",
            Kind::Bool,
            "Rules",
            "Send your rules with a run. Off is --no-rules and sends no skills either.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        flag: "--no-skills, --skills <dir>",
        order: Order::FlagFile,
        wire: Some("skills"),
        since: 1,
        ..row(
            Key::RunSkills,
            ("run", "skills"),
            "run.skills",
            Kind::Bool,
            "Skills",
            "Send your skills with a run. Off is --no-skills.",
        )
    },
    Row {
        default_text: "stage default",
        flag: "--research, --no-research",
        order: Order::FlagFile,
        wire: Some("research"),
        since: 1,
        ..row(
            Key::ResearchEnabled,
            ("research", "enabled"),
            "research.enabled",
            Kind::Bool,
            "Web research",
            "Whether a run may search the web. The stage decides when the key is unset.",
        )
    },
    Row {
        default: DefaultValue::Bool(false),
        default_text: "off",
        env: "HAMH_NOTIFY",
        order: Order::EnvOverFile,
        applies: Applies::Now,
        since: 1,
        group: Group::Interface,
        ..row(
            Key::UiNotify,
            ("ui", "notify"),
            "ui.notify",
            Kind::Bool,
            "Notifications",
            "Terminal notification when a run waits and the window is behind. HAMH_NOTIFY wins when set.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        applies: Applies::Now,
        since: 1,
        group: Group::Interface,
        ..row(
            Key::UiBell,
            ("ui", "bell"),
            "ui.bell",
            Kind::Bool,
            "Bell",
            "Ring the bell when a run starts to wait for you.",
        )
    },
    Row {
        default_text: "auto",
        env: "HAMH_NO_MASCOT",
        order: Order::EnvForcesOff,
        applies: Applies::Now,
        since: 1,
        group: Group::Interface,
        ..row(
            Key::UiMascot,
            ("ui", "mascot"),
            "ui.mascot",
            Kind::Mascot,
            "Mascot",
            "The character beside the title on a tall terminal and in the empty Runs list: auto, ascii (for a terminal that draws block characters double wide) or off. HAMH_NO_MASCOT=1 turns it off.",
        )
    },
    Row {
        default_text: "5 (the stage's own limit)",
        wire: Some("reportStepS"),
        since: 1,
        ..row(
            Key::RunReportCommandMinutes,
            ("run", "report_command_minutes"),
            "run.report_command_minutes",
            Kind::Minutes {
                max: harness_protocol::direct::MAX_REPORT_STEP_S / 60,
            },
            "Test command limit",
            "Minutes one command of the clean-checkout test report may run before it is stopped. 0 uses the stage's own limit.",
        )
    },
    Row {
        default: DefaultValue::Number(GOAL_ROUND_CALLS.default),
        default_text: "30 calls",
        wire: Some("goal"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunStallCalls,
            ("run", "stall_calls"),
            "run.stall_calls",
            Kind::Number {
                min: GOAL_ROUND_CALLS.min,
                max: GOAL_ROUND_CALLS.max,
                unit: "calls",
            },
            "Stall: calls",
            "Tool calls in a row, with no changed file and no new text, that end the run. A command that is running does not count.",
        )
    },
    Row {
        default: DefaultValue::Number(GOAL_ROUND_WINDOW_S.default / 60),
        default_text: "20 minutes",
        wire: Some("goal"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunStallWindowMinutes,
            ("run", "stall_window_minutes"),
            "run.stall_window_minutes",
            Kind::Number {
                min: GOAL_ROUND_WINDOW_S.min.div_ceil(60),
                max: GOAL_ROUND_WINDOW_S.max / 60,
                unit: "minutes",
            },
            "Stall: minutes",
            "Minutes with no changed file that end the run when at least the least calls came in them. The time a command ran is left out.",
        )
    },
    Row {
        default: DefaultValue::Number(GOAL_ROUND_MIN_CALLS.default),
        default_text: "10 calls",
        wire: Some("goal"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunStallMinCalls,
            ("run", "stall_min_calls"),
            "run.stall_min_calls",
            Kind::Number {
                min: GOAL_ROUND_MIN_CALLS.min,
                max: GOAL_ROUND_MIN_CALLS.max,
                unit: "calls",
            },
            "Stall: least calls",
            "The least number of tool calls inside the stall minutes for them to count; one long build is one or two calls.",
        )
    },
    Row {
        default: DefaultValue::Number(GOAL_COMMAND_GRACE_S.default / 60),
        default_text: "120 minutes",
        wire: Some("goal"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunCommandGraceMinutes,
            ("run", "command_grace_minutes"),
            "run.command_grace_minutes",
            Kind::Number {
                min: 0,
                max: GOAL_COMMAND_GRACE_S.max / 60,
                unit: "minutes",
            },
            "Command grace",
            "Minutes with no changed file that a running command may hold the run for. 0 does not look for running commands.",
        )
    },
    Row {
        default: DefaultValue::Bool(GOAL_FINAL_MESSAGE),
        default_text: "on",
        wire: Some("goal"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunFinalMessage,
            ("run", "final_message"),
            "run.final_message",
            Kind::Bool,
            "Final message",
            "Give the engine one last turn to write its final message before a stall ends the run. Off ends the run at once.",
        )
    },
    Row {
        default_text: "auto",
        since: 1,
        ..row(
            Key::RunLifecycleFlow,
            ("run", "lifecycle_flow"),
            "run.lifecycle_flow",
            Kind::Flow,
            "Lifecycle flow",
            "Which text of the lifecycle contract `ham rules init` installs: auto (the run picks the quick path for a small change in an existing repository, else the eight steps), full or quick. Run `ham rules init` after you change it.",
        )
    },
    Row {
        default_text: "auto",
        wire: Some("review"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunReviewRound,
            ("run", "review_round"),
            "run.review_round",
            Kind::Review,
            "Review round",
            "After the engine reports done with a changed file, the harness asks it once to review its own delivery as the maintainer who must merge it. auto runs the round, off never does. The prompt is the file review.md (ham rules edit review).",
        )
    },
    Row {
        default_text: "auto",
        flag: "--compute auto|microvm|instances",
        order: Order::FlagFile,
        wire: Some("compute"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunCompute,
            ("run", "compute"),
            "run.compute",
            Kind::Compute,
            "Machine",
            "Which machine the session runs on: microvm (the normal 2 vCPU session), instances (one EC2 machine per session with many CPUs and a large disk, where the stage has them) or auto (the stage decides). The first call of an instances session takes about a minute and a half longer.",
        )
    },
    Row {
        default: DefaultValue::Number(GOAL_NEW_CALLS.default),
        default_text: "100 calls",
        wire: Some("goal"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunStallNewCalls,
            ("run", "stall_new_calls"),
            "run.stall_new_calls",
            Kind::Number {
                min: 0,
                max: GOAL_NEW_CALLS.max,
                unit: "calls",
            },
            "Stall: new calls",
            "Between two changed files, this many tool calls that bring something new (an input no earlier call had, at least 3 lines in its answer that no earlier answer had, and the call succeeded) are not counted toward the stall calls. 0 counts every call.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        wire: Some("build"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunCargoNoDebugInfo,
            ("run", "cargo_no_debug_info"),
            "run.cargo_no_debug_info",
            Kind::Bool,
            "No debug info",
            "On, the session builds cargo's dev and test profiles without debug information (CARGO_PROFILE_DEV_DEBUG=0 and CARGO_PROFILE_TEST_DEBUG=0), which keeps target/ small and links fast on a small machine. Off leaves the debug-information setting to cargo and the repository's own profiles.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        wire: Some("build"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunCargoNoIncremental,
            ("run", "cargo_no_incremental"),
            "run.cargo_no_incremental",
            Kind::Bool,
            "No incremental",
            "On, the session builds without incremental state (CARGO_INCREMENTAL=0), so target/ does not grow with every edit. Off leaves the incremental setting to cargo and the repository's own configuration.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        wire: Some("build"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunExcludeTargetDir,
            ("run", "exclude_target_dir"),
            "run.exclude_target_dir",
            Kind::Bool,
            "Exclude target/",
            "On, the harness lists target/ in the repository's local exclude file, so the commit it makes at the end of a run never takes up build output (cargo's, and the Maven or Gradle folder of that name). A file the repository already tracks stays tracked. Off leaves the list to the repository's own ignore files.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        wire: Some("goal"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunOutsideCheckoutNote,
            ("run", "outside_checkout_note"),
            "run.outside_checkout_note",
            Kind::Bool,
            "Outside-work note",
            "On, the goal prompts tell the engine that work outside this checkout is not a blocker: report it as NOT DONE lines and finish the rest. Off leaves the sentence out of the goal prompts.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        env: "HAMH_NO_DEPRECATION_NOTICES",
        order: Order::EnvForcesOff,
        applies: Applies::Restart,
        since: 1,
        group: Group::Interface,
        ..row(
            Key::UiDeprecationNotices,
            ("ui", "deprecation_notices"),
            "ui.deprecation_notices",
            Kind::Bool,
            "Old-name notices",
            "Print one notice on standard error when the old command name, an old variable name or the old config folder is used. HAMH_NO_DEPRECATION_NOTICES=1 turns the notices off.",
        )
    },
    Row {
        default: DefaultValue::Bool(true),
        default_text: "on",
        applies: Applies::Now,
        since: 1,
        group: Group::Interface,
        ..row(
            Key::UiMascotMotion,
            ("ui", "mascot_motion"),
            "ui.mascot_motion",
            Kind::Bool,
            "Mascot motion",
            "Move the smoke under the mascot's feet, one step a second, in the title band and in the empty Runs list. Off shows the still picture. ui.mascot = off hides the mascot whatever this says.",
        )
    },
    Row {
        default: DefaultValue::Number(harness_protocol::direct::DEFAULT_MAX_RUN_S / 3600),
        default_text: "24 hours",
        flag: "--max-run-hours <hours>",
        order: Order::FlagFile,
        wire: Some("maxRunS"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunMaxRunHours,
            ("run", "max_run_hours"),
            "run.max_run_hours",
            Kind::Number {
                min: harness_protocol::direct::MIN_MAX_RUN_S.div_ceil(3600),
                max: harness_protocol::direct::MAX_MAX_RUN_S / 3600,
                unit: "hours",
            },
            "Total run time",
            "The most hours one run may take in all. A session lasts up to 8 hours; a run that is not finished when its session ends continues in a new session from the work saved so far, until this time is used up. The run then stops as paused and ham resume continues it.",
        )
    },
    Row {
        default: DefaultValue::Bool(false),
        default_text: "off",
        flag: "--provider-sharing on|off",
        order: Order::FlagFile,
        wire: Some("providerSharing"),
        since: 1,
        group: Group::Run,
        ..row(
            Key::RunProviderSharing,
            ("run", "provider_sharing"),
            "run.provider_sharing",
            Kind::Bool,
            "Data sharing (3P)",
            "Share data with the model provider (3P sharing). On, the prompts and answers of a run on a model that needs it go to the model's provider, which may keep them up to 30 days for safety monitoring, and before such a run ham switches the AWS account's Bedrock data retention mode in the stage's Region to provider_data_share with your credentials when the mode does not let the model run (default or inherit); the switch stays after the run. Off, ham refuses such a model on the command line, and the terminal app asks first. A model that needs it is marked in `ham models`.",
        )
    },
    Row {
        applies: Applies::Restart,
        security: true,
        since: 1,
        group: Group::Connection,
        ..row(
            Key::DefaultBackend,
            ("", "defaultBackend"),
            "default_backend",
            Kind::Text,
            "Default backend",
            "The backend a new run starts on when you name none: default, or the alias of a [[backend]] table. With two or more backends and no default, a start names its backend.",
        )
    },
    Row {
        default: DefaultValue::Bool(false),
        default_text: "no",
        applies: Applies::Restart,
        security: true,
        since: 1,
        group: Group::Connection,
        ..row(
            Key::CustomerContent,
            ("", "customerContent"),
            "customer_content",
            Kind::Bool,
            "Customer content",
            "Whether the default backend may receive a run that is marked as customer content (--customer-content). The same key in a [[backend]] table declares that backend. A start marked as customer content on a backend not declared safe is refused.",
        )
    },
    Row {
        default: DefaultValue::Number(64),
        default_text: "64 KiB",
        applies: Applies::Restart,
        since: 1,
        group: Group::Interface,
        ..row(
            Key::UiCopyLimitKib,
            ("ui", "copy_limit_kib"),
            "ui.copy_limit_kib",
            Kind::Number {
                min: 1,
                max: 1_048_576,
                unit: "KiB",
            },
            "Copy limit",
            "The most KiB one copy to the clipboard carries (the y key, /copy and a mouse selection). A longer text is refused whole and never cut; /export keeps it. Raise it for a terminal that takes more.",
        )
    },
    Row {
        default: DefaultValue::Number(5),
        default_text: "5 interrupts",
        wire: Some("limits"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunMaxInterrupts,
            ("run", "max_interrupts"),
            "run.max_interrupts",
            Kind::Number {
                min: 0,
                max: LIMIT_INTERRUPTS.max as u32,
                unit: "interrupts",
            },
            "Interrupts",
            "How many immediate (/now) messages one run may make. Each cuts the turn that is running and ends the tool processes it started, so the count guards against an interrupt storm. After the last one, a /now message waits for the turn to end and goes out next. 0 allows none.",
        )
    },
    Row {
        default: DefaultValue::Number(3),
        default_text: "3 questions",
        wire: Some("limits"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunMaxQuestions,
            ("run", "max_questions"),
            "run.max_questions",
            Kind::Number {
                min: 0,
                max: LIMIT_QUESTIONS.max as u32,
                unit: "questions",
            },
            "Questions",
            "How many times one run may stop to ask you what to do. It only asks when you gave it a wait (--input-wait). The automatic brakes of the run do not move with this number. 0 means the run never asks.",
        )
    },
    Row {
        default: DefaultValue::Number(3),
        default_text: "3 fresh sessions",
        wire: Some("limits"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunMaxFreshSessions,
            ("run", "max_fresh_sessions"),
            "run.max_fresh_sessions",
            Kind::Number {
                min: 0,
                max: LIMIT_FRESH_SESSIONS.max as u32,
                unit: "fresh sessions",
            },
            "Fresh sessions",
            "How many fresh sessions you may ask for in one run when it stops and asks. The one automatic fresh session of the run is counted apart and does not move. 0 refuses your fresh-session answer.",
        )
    },
    Row {
        default: DefaultValue::Number(1),
        default_text: "1 MiB",
        wire: Some("limits"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::ResearchMaxFileMib,
            ("research", "max_file_mib"),
            "research.max_file_mib",
            Kind::Number {
                min: 1,
                max: (LIMIT_RESEARCH_FILE_BYTES.max / MIB) as u32,
                unit: "MiB",
            },
            "Research file size",
            "The most MiB one file of the research material may hold. A larger file of a repository is not placed in the context folder.",
        )
    },
    Row {
        default: DefaultValue::Number(200),
        default_text: "200 MiB",
        wire: Some("limits"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::ResearchMaxUnpackedMib,
            ("research", "max_unpacked_mib"),
            "research.max_unpacked_mib",
            Kind::Number {
                min: 1,
                max: (LIMIT_RESEARCH_UNPACKED_BYTES.max / MIB) as u32,
                unit: "MiB",
            },
            "Research repo size",
            "The most MiB one repository of the research material may take once unpacked. The worker's disk is small and the research module also keeps 256 MiB free, so a repository that does not fit is not placed.",
        )
    },
    Row {
        default: DefaultValue::Number(300),
        default_text: "300 MiB",
        wire: Some("limits"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::ResearchMaxRunMib,
            ("research", "max_run_mib"),
            "research.max_run_mib",
            Kind::Number {
                min: 1,
                max: (LIMIT_RESEARCH_RUN_BYTES.max / MIB) as u32,
                unit: "MiB",
            },
            "Research total size",
            "The most MiB of research material one run may hold in all.",
        )
    },
    Row {
        default_text: "default",
        wire: Some("compute"),
        since: 1,
        group: Group::Limits,
        ..row(
            Key::RunContainerSignals,
            ("run", "container_signals"),
            "run.container_signals",
            Kind::Signals,
            "Container signals",
            "The files that tell ham a repository starts containers in its checks (Testcontainers, docker compose). The Instances machine cannot run containers, so a repository with a signal keeps an auto run on the microVM, and a run that asks for Instances is refused with the signal named. One line of entries separated by semicolons. An entry is file names separated by commas, then optionally the word contains and a text, as in pom.xml, build.gradle contains testcontainers. A name with no slash matches at any depth; a name with a slash is read from the root of the repository. The word default stands for the built-in list, so default; Gemfile contains testcontainers adds one signal to it. The word none turns every signal off. The built-in list covers Testcontainers in Maven, Gradle, Go, Python, Node, .NET, Rust, Ruby, PHP and Elixir builds, compose files, and a .ham/test-command that mentions docker.",
        )
    },
    Row {
        file_alias: Some("default_engine"),
        default: DefaultValue::Text(crate::engines::DEFAULT_ENGINE_ID),
        default_text: crate::engines::DEFAULT_ENGINE_ID,
        flag: "--engine <name>",
        order: Order::FlagFile,
        wire: Some("engine"),
        since: 1,
        ..row(
            Key::DefaultEngine,
            ("", "defaultEngine"),
            "default_engine",
            Kind::Engine,
            "Engine",
            "The coding engine a run uses when you name none: one of the engines the /engines panel lists as available. A run on the default engine sends no engine member, so the request is the one an older client sends. The engine of one run is chosen with --engine, or in the Engine box of the New run screen.",
        )
    },
    Row {
        file_alias: Some("update_source"),
        default: DefaultValue::Text(crate::update::DEFAULT_SOURCE),
        default_text: crate::update::DEFAULT_SOURCE,
        applies: Applies::Now,
        security: true,
        since: 1,
        group: Group::Connection,
        ..row(
            Key::UpdateSource,
            ("", "updateSource"),
            "update_source",
            Kind::Text,
            "Update source",
            "The git repository ham update reads the newest version from: the head of its main branch, its VERSION file and its CHANGELOG.md. An https:// URL, or a file:/// path to a local mirror. The word off turns ham update off; it then makes no network call.",
        )
    },
];

/// The tables of the file, in registry order.
pub const TABLES: [&str; 3] = ["run", "research", "ui"];

/// The table of a key name used by the reader's hints: `Some("run")` for `goal`.
pub fn table_of_key(name: &str) -> Option<&'static str> {
    ROWS.iter()
        .find(|r| !r.file.0.is_empty() && r.file.1 == name)
        .map(|r| r.file.0)
}

// ---- the locked classes ----------------------------------------------------------------------------------------------------------

/// A class of settings the harness fixes whatever a file says. `ham config show` and the Settings screen print them
/// read-only with the reason; the refusal of a key of the class says the same words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockedClass {
    /// The class as the list names it.
    pub name: &'static str,
    /// One or two words for the one-line list of the Settings screen.
    pub label: &'static str,
    /// The reason, one short text.
    pub reason: &'static str,
    /// What the class protects (`root invariants 2 and 8`).
    pub protects: &'static str,
}

/// The seven classes, in the order of the design. The label, the reason and what the class protects are the protocol's own table
/// ([`harness_protocol::direct::DENIED`]), the text that the refusal of control-api and agentd uses, so the screen, `ham config show` and a
/// refusal say the same words by construction; the name is the client's longer name of the class.
pub const LOCKED: [LockedClass; 7] = [
    locked("Credentials and identity", 0),
    locked("Provider, model, endpoint", 1),
    locked("Telemetry and update", 2),
    locked("Processes and paths", 3),
    locked("Permissions", 4),
    locked("Network", 5),
    locked("Owned elsewhere", 6),
];

const _: () = assert!(
    DENIED.len() == LOCKED.len(),
    "the client lists as many locked classes as the protocol denies"
);

const fn locked(name: &'static str, at: usize) -> LockedClass {
    LockedClass {
        name,
        label: DENIED[at].label,
        reason: DENIED[at].reason,
        protects: DENIED[at].protects,
    }
}

// ---- findings --------------------------------------------------------------------------------------------------------------------

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

/// What a finding is about; [`Code::as_str`] is the stable word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    TooLarge,
    Nul,
    Bom,
    TooDeep,
    Syntax,
    UnknownKey,
    UnknownTable,
    MovedKey,
    BadType,
    OutOfRange,
    DuplicateKey,
    BadVersion,
    NewerVersion,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::TooLarge => "too_large",
            Code::Nul => "nul",
            Code::Bom => "bom",
            Code::TooDeep => "too_deep",
            Code::Syntax => "syntax",
            Code::UnknownKey => "unknown_key",
            Code::UnknownTable => "unknown_table",
            Code::MovedKey => "moved_key",
            Code::BadType => "bad_type",
            Code::OutOfRange => "out_of_range",
            Code::DuplicateKey => "duplicate_key",
            Code::BadVersion => "bad_version",
            Code::NewerVersion => "newer_version",
        }
    }

    /// True when the finding means the text cannot be used as TOML at all (too large, a NUL byte, a byte-order mark, nested too deep or
    /// not TOML): the keys it left are not the file's keys, so a verb that reads or changes the file refuses it. Every other finding is
    /// about one key or table and leaves the rest of the file readable.
    pub fn is_unreadable(self) -> bool {
        matches!(
            self,
            Code::TooLarge | Code::Nul | Code::Bom | Code::TooDeep | Code::Syntax
        )
    }
}

/// One thing the reader found in `config.toml`, with the line it is on. The text never holds a value or a line of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub code: Code,
    /// 1-based line and column of the key (of the fault, for a syntax error).
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// The key or table the finding names, when its name is plain enough to print.
    pub key: Option<String>,
    /// The nearest known name.
    pub suggestion: Option<String>,
    pub message: String,
}

impl Finding {
    /// `line 2, column 1: unknown key `goall` in [run]; did you mean `goal`?`
    pub fn render(&self) -> String {
        match (self.line, self.column) {
            (Some(l), Some(c)) => format!("line {l}, column {c}: {}", self.message),
            (Some(l), None) => format!("line {l}: {}", self.message),
            _ => self.message.clone(),
        }
    }
}

/// The findings of one read, in line order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Findings(Vec<Finding>);

impl Findings {
    pub(crate) fn push(&mut self, f: Finding) {
        self.0.push(f);
    }

    /// Orders the findings by line, then column; findings without a line come first.
    pub(crate) fn sort(&mut self) {
        self.0.sort_by_key(|f| (f.line, f.column));
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Finding> {
        self.0.iter()
    }

    pub fn errors(&self) -> impl Iterator<Item = &Finding> {
        self.0.iter().filter(|f| f.severity == Severity::Error)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.0.iter().filter(|f| f.severity == Severity::Warning)
    }

    pub fn has_errors(&self) -> bool {
        self.errors().next().is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

// ---- the reference table ---------------------------------------------------------------------------------------------------------

fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

fn code_cell(text: &str) -> String {
    if text.is_empty() {
        "none".to_string()
    } else {
        format!("`{}`", cell(text))
    }
}

fn env_cell(row: &Row) -> String {
    if row.env.is_empty() {
        return "none".to_string();
    }
    let note = match row.order {
        Order::FileOverEnv => " (the file wins)",
        Order::EnvOverFile => " (wins over the file)",
        Order::EnvForcesOff => " (forces off)",
        Order::FlagEnvFile | Order::FlagFile | Order::FileOnly => "",
    };
    format!("`{}`{note}", row.env)
}

/// The reference table of the settings, generated from [`ROWS`]: key, type, default, flag, environment variable and when a change takes
/// effect. `ham config docs` prints it and the user guide holds the same text between two markers.
pub fn reference_markdown() -> String {
    let mut out = String::from(
        "| Key | Type | Default | Flag | Environment variable | Takes effect |\n| --- | --- | --- | --- | --- | --- |\n",
    );
    for row in &ROWS {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            row.file_name(),
            cell(&row.kind.type_text()),
            cell(row.default_text),
            code_cell(row.flag),
            env_cell(row),
            row.applies.label()
        ));
    }
    out
}
