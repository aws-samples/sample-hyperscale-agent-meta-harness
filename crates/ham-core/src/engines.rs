//! The engine-neutral surface of the chat client.
//!
//! This is the one module of the client that names an engine: the display names of the engines this build knows about, the table of
//! which of them a person can choose (Cline and OpenCode; the others are "not available in this release"), the engine each run is on and
//! the texts that say so. The table is data: a new engine is one more row, and the chooser, the flag, the setting, the panel and the
//! sentences read it. Everything else in `ham` reads the
//! **capability record** of a session instead: one `session.capabilities` event per session that agentd writes from the engine's own
//! handshake (ACP words: modes, config options, advertised commands) plus the harness's own lists (the controls `message.next` and
//! `message.now`, the feature `transcript.v1`). The popup, the status line and the `/engines` panel are built from that record, so a second
//! engine with other modes and commands renders with no code change. A test counts the engine names in the client's
//! sources and fails above the recorded count (`tests/engine_names.rs`).
//!
//! The record is written by agentd, a process the engine cannot reach, but it travels as an agent-authority event, so every string in it
//! is cleaned ([`display_line`]), redacted and cut before the client keeps it, and a command word the client would forward must have the
//! shape of a plain word. A run with no record is *unknown*, not old: a missing record hides nothing.

use crate::redact::redact_tokens;
use crate::registry::{self, Availability, Effects, Entry, Kind, RunArg, TextArg};
use crate::sanitize::display_line;
use crate::state::{RunState, RunView};
use harness_protocol::direct::{KeyKind, keys_for};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::transcript::SessionCapabilitiesPayload;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// What the panel and the popup say of an engine this release does not run.
pub const NOT_AVAILABLE: &str = "not available in this release";
/// The label of a run whose capability record has not arrived (or never will: a run on an older image).
pub const CAPABILITIES_UNKNOWN: &str = "capabilities unknown";
/// What the box says when the run cannot take a message (an image without message control, or a run that is ending). A template: read it
/// with [`say_for`].
pub const NOT_WAITING: &str =
    "{engine} is not waiting for an answer; /cancel stops the run and keeps the work";
/// The title of the box in the same case (a template).
pub const NOT_WAITING_TITLE: &str = "Not available: {engine} is not waiting; /cancel stops the run";
/// The offer on a finished run when the box holds text.
pub const FOLLOW_UP_OFFER: &str =
    "{engine} is finished. Enter = start a follow-up run from this result.";
/// What the follow-up is not.
pub const FOLLOW_UP_NOTE: &str =
    "The new {engine} starts a new session and sees a summary of this chat, not the whole history.";

/// `template` with `{engine}` replaced by the display name of the default engine: for a sentence that no run is behind yet (the help
/// text, a command typed outside a run). A sentence about a run goes through [`say_for`].
pub fn say(template: &str) -> String {
    say_for(None, template)
}

/// `template` with `{engine}` replaced by the display name of the engine `id` names, which is how the client puts the engine of a run into a
/// sentence without writing it. `None`, and an id this build does not know, give the default engine.
pub fn say_for(id: Option<&str>, template: &str) -> String {
    let engine = id.and_then(find).unwrap_or_else(default_engine);
    engine.say(template)
}

/// The control word of an engine that takes a message as an interrupt.
pub const NOW_CONTROL: &str = harness_protocol::messages::MESSAGE_NOW_CONTROL;
/// The control word of an engine that takes a message at the end of its turn.
pub const NEXT_CONTROL: &str = harness_protocol::messages::MESSAGE_NEXT_CONTROL;

/// The longest text of one record field the client keeps.
const FIELD_MAX: usize = 60;
/// The most advertised commands, modes and options one record keeps.
const LIST_MAX: usize = 64;
/// The longest command word the client lists or forwards.
const COMMAND_MAX: usize = 32;
/// The most distinct advertised commands this process lists in popups over its life.
const INTERN_MAX: usize = 256;

/// Whether a person can choose an engine in this release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineState {
    /// The engine runs: `--engine`, `default_engine` and the composer offer it.
    Available,
    NotAvailable,
}

/// One engine of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Engine {
    /// The lower-case word a person types.
    pub id: &'static str,
    pub name: &'static str,
    pub state: EngineState,
}

impl Engine {
    /// `template` with `{engine}` replaced by this engine's display name.
    pub fn say(&self, template: &str) -> String {
        template.replace("{engine}", self.name)
    }

    /// The heading of the evidence file above the quote of the engine's last message: `## <Name>'s last message`.
    pub fn evidence_heading(&self) -> String {
        self.say("## {engine}'s last message")
    }

    /// True for the engine a run uses when nothing else is chosen.
    pub fn is_default(&self) -> bool {
        self.id == DEFAULT_ENGINE_ID
    }
}

/// The id of the engine a run uses when the request names none.
pub const DEFAULT_ENGINE_ID: &str = "cline";

/// The table, in the order the panel and the chooser list it: the engines a person can choose first, then the rest of the roadmap.
const ROADMAP: [Engine; 5] = [
    Engine {
        id: "cline",
        name: "Cline",
        state: EngineState::Available,
    },
    Engine {
        id: "opencode",
        name: "OpenCode",
        state: EngineState::Available,
    },
    Engine {
        id: "pi",
        name: "Pi",
        state: EngineState::NotAvailable,
    },
    Engine {
        id: "codex",
        name: "Codex",
        state: EngineState::NotAvailable,
    },
    Engine {
        id: "kiro",
        name: "Kiro CLI",
        state: EngineState::NotAvailable,
    },
];

/// Every engine this build knows about, in the table's order.
pub fn roadmap() -> &'static [Engine] {
    &ROADMAP
}

/// The engines a person can choose, in the table's order.
pub fn available() -> Vec<&'static Engine> {
    ROADMAP
        .iter()
        .filter(|e| e.state == EngineState::Available)
        .collect()
}

/// The engine a run uses when nothing else is chosen.
pub fn default_engine() -> &'static Engine {
    find(DEFAULT_ENGINE_ID).unwrap_or(&ROADMAP[0])
}

/// The engine of the table whose id is `id`.
pub fn find(id: &str) -> Option<&'static Engine> {
    ROADMAP.iter().find(|e| e.id == id)
}

/// The ids of the engines a person can choose, joined for a message: `cline, opencode`.
pub fn choices_text() -> String {
    available()
        .iter()
        .map(|e| e.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why a typed engine is not a choice.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChoiceError {
    #[error("unknown engine {word:?}; the engines are {choices}")]
    Unknown { word: String, choices: String },
    #[error("{id} is not available in this release; the engines are {choices}")]
    NotAvailable { id: &'static str, choices: String },
}

/// A choice that is wrong, with the layer that made it (`--engine`, `default_engine in config.toml`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{layer}: {source}")]
pub struct LayerError {
    pub layer: &'static str,
    #[source]
    pub source: ChoiceError,
}

/// The longest typed word an error repeats.
const WORD_MAX: usize = 40;

/// The engine a person typed: the id, in any case and with blanks around it. Only an engine that is [`EngineState::Available`] is a choice.
pub fn choose(word: &str) -> Result<&'static Engine, ChoiceError> {
    let id = word.trim().to_ascii_lowercase();
    match find(&id) {
        Some(e) if e.state == EngineState::Available => Ok(e),
        Some(e) => Err(ChoiceError::NotAvailable {
            id: e.id,
            choices: choices_text(),
        }),
        None => Err(ChoiceError::Unknown {
            word: word.trim().chars().take(WORD_MAX).collect(),
            choices: choices_text(),
        }),
    }
}

/// Where a run's engine came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Flag,
    File,
    Default,
}

/// The engine of a run: the `--engine` flag, else `default_engine` of the file, else the default engine. A value that is set and wrong is an
/// error that names its layer and never falls through to a weaker one; a flag that is right is not held back by a wrong file.
pub fn resolve(
    flag: Option<&str>,
    file: Option<&str>,
) -> Result<(&'static Engine, Layer), LayerError> {
    if let Some(word) = flag {
        let e = choose(word).map_err(|source| LayerError {
            layer: "--engine",
            source,
        })?;
        return Ok((e, Layer::Flag));
    }
    if let Some(word) = file {
        let e = choose(word).map_err(|source| LayerError {
            layer: "default_engine in config.toml",
            source,
        })?;
        return Ok((e, Layer::File));
    }
    Ok((default_engine(), Layer::Default))
}

/// The engine a harness-written `session.started` event names, if the event is one and the engine is in the table.
pub fn engine_in(e: &Envelope) -> Option<&'static Engine> {
    if e.event_type != EventType::SessionStarted || e.authority != Authority::Harness {
        return None;
    }
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    value
        .get("engine")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| find(&id.trim().to_ascii_lowercase()))
}

/// The engine a run is on, read from its events (the first `session.started` of the harness that names an engine in the table), for code that
/// holds the events and not yet a view. The default engine when there is none.
pub fn of_events<'a>(events: impl IntoIterator<Item = &'a Envelope>) -> &'static Engine {
    events
        .into_iter()
        .find_map(engine_in)
        .unwrap_or_else(default_engine)
}

/// The engine a run is on: the one its `session.started` event named. A run with no session yet, and an engine this build lacks, read as the
/// default engine, which is also what a request with no engine member runs.
pub fn of_view(view: &RunView) -> &'static Engine {
    view.agents
        .values()
        .find_map(|a| a.engine.as_deref())
        .and_then(|id| find(&id.trim().to_ascii_lowercase()))
        .unwrap_or_else(default_engine)
}

// ---- the settings of an engine -------------------------------------------------------------------
//
// The key table of an engine (names, types, ranges, defaults) is a Rust const in `harness-protocol` (`keys_for`), the same one that the check of
// control-api and agentd uses. The words of the keys, which a person reads on the Settings screen and in the template that `ham engine <name>
// init` writes, live here, the one module that names an engine; a parity test fails when the two disagree.

/// What `ham engine <name> ...` answers for an engine that has no key table in this build.
pub const NO_SETTINGS_SCHEMA: &str = "no settings schema in this build yet";

/// What `ham engine <name> ...` answers for an engine a person can choose that has no key table yet: the file is not read, nothing from it is
/// sent, and the engine is chosen by the flag or the setting.
pub fn no_settings_words(e: &Engine) -> String {
    e.say(&format!(
        "{{engine}} has no settings of its own in this release: nothing is read from engines/{id}.toml and nothing from it is sent. \
         Choose the engine for a run with --engine {id}, or set default_engine with `ham config set default_engine {id}`.",
        id = e.id
    ))
}

/// What `ham engine <name>` says: whether the engine can be chosen and how, and what its settings file offers.
pub fn describe(e: &Engine) -> String {
    let mut out = match e.state {
        EngineState::Available => e.say(&format!(
            "{{engine}} ({id}) is available. Choose it for one run with --engine {id}, for the runs of a TUI session with /engine {id}, \
             or for good with `ham config set default_engine {id}`.",
            id = e.id
        )),
        EngineState::NotAvailable => format!("{} ({}) is {NOT_AVAILABLE}.", e.name, e.id),
    };
    if e.state == EngineState::Available {
        out.push('\n');
        if has_settings(e.id) {
            out.push_str(&format!(
                "Its settings file is engines/{id}.toml: ham engine {id} show, init, check, edit, set, unset or import.",
                id = e.id
            ));
        } else {
            out.push_str(&no_settings_words(e));
        }
    }
    out
}

/// True when this build has a key table for the engine.
pub fn has_settings(id: &str) -> bool {
    keys_for(id).is_some()
}

/// The words of one key of an engine's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingText {
    /// The dotted name in the engine file (`reasoning.effort`), as the key table names it.
    pub path: &'static str,
    /// The row's name on the Settings screen, at most 24 characters.
    pub label: &'static str,
    /// What the key does, in one or two sentences.
    pub help: &'static str,
    /// What the screen shows while the file does not set the key.
    pub default_shown: &'static str,
}

const CLINE_TEXTS: [SettingText; 5] = [
    SettingText {
        path: "reasoning.effort",
        label: "Reasoning effort",
        help: "How hard the model thinks before it answers. default is the level chosen for the model, off switches it off, low to xhigh set it. Higher takes longer.",
        default_shown: "model default",
    },
    SettingText {
        path: "commands.timeout_seconds",
        label: "Command time limit",
        help: "How long one shell command may run before Cline stops it, from 1 to 1800 seconds. Cline's own limit is 600.",
        default_shown: "600 s",
    },
    SettingText {
        path: "context.compaction",
        label: "Compaction",
        help: "How Cline shortens a long conversation: agentic summarizes it, basic trims it, off ends a run that overflows its context.",
        default_shown: "agentic",
    },
    SettingText {
        path: "context.compaction_trigger",
        label: "Compaction trigger",
        help: "The share of the context window that starts a compaction, from 0.10 to 0.99. It is unused while compaction is off.",
        default_shown: "0.90",
    },
    SettingText {
        path: "tools.disable",
        label: "Tools switched off",
        help: "Tools Cline may not use in this run. The list adds to the stage's own list and never removes from it.",
        default_shown: "none",
    },
];

/// The one key of OpenCode's table. Its default is the harness's: OpenCode's own limit is two minutes and every launch carries this one.
const OPENCODE_TEXTS: [SettingText; 1] = [SettingText {
    path: "commands.timeout_seconds",
    label: "Command time limit",
    help: "How long one shell command may run before OpenCode stops it, from 1 to 1800 seconds. The default is 600.",
    default_shown: "600 s",
}];

/// The words of every key of the engine's table, in the table's order; empty for an engine without a table.
pub fn setting_texts(id: &str) -> &'static [SettingText] {
    match find(id) {
        Some(e) if e.id == DEFAULT_ENGINE_ID => &CLINE_TEXTS,
        Some(e) if e.id == "opencode" => &OPENCODE_TEXTS,
        _ => &[],
    }
}

/// The name of the group of an engine's rows on the Settings screen: `Cline engine`.
pub fn settings_group_label(id: &str) -> String {
    find(id).map_or_else(|| "Engine".to_string(), |e| format!("{} engine", e.name))
}

/// A stored value of a key as the screen and `ham engine <name> show` word it: the default level of the reasoning effort reads `model default`,
/// never `off`; a number of seconds has its unit; a trigger has two decimals; a list is joined with commas and spaces, and an empty list is `none`.
pub fn shown_value(path: &str, raw: &str) -> String {
    match path {
        "reasoning.effort" if raw == "default" => "model default".to_string(),
        "commands.timeout_seconds" => format!("{raw} s"),
        "context.compaction_trigger" => raw
            .parse::<f64>()
            .map_or_else(|_| raw.to_string(), |x| format!("{x:.2}")),
        "tools.disable" => {
            let parts: Vec<&str> = raw
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect();
            if parts.is_empty() {
                "none".to_string()
            } else {
                parts.join(", ")
            }
        }
        _ => raw.to_string(),
    }
}

/// The file `ham engine <name> init` writes: `schema = 1` and every key commented out, each with what it takes, the way `ham rules init` writes the
/// rules. It reads as a file that sets nothing. `None` for an engine without a table.
pub fn settings_template(id: &str) -> Option<String> {
    let rows = keys_for(id)?;
    let name = find(id)?.name;
    let mut out = format!(
        "# Settings for the {name} engine, sent with every run you start.\n\
         # Remove the # before a line to use it. A setting you leave out keeps the engine's own value.\n\
         # `ham engine {id} check` shows what a run would send. Comments stay on this computer.\n\
         schema = 1\n"
    );
    let mut table = "";
    for row in rows {
        let (t, key) = row.path.split_once('.').unwrap_or(("", row.path));
        if t != table {
            out.push_str(&format!("\n# [{t}]\n"));
            table = t;
        }
        let example = match row.kind {
            KeyKind::Choice(_) => format!("\"{}\"", row.default),
            KeyKind::Int { .. } | KeyKind::Float { .. } | KeyKind::Set(_) => {
                row.default.to_string()
            }
        };
        out.push_str(&format!(
            "# {key} = {example}   # {}\n",
            row.kind.describe()
        ));
    }
    Some(out)
}

/// One command an engine advertises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    pub name: String,
    pub description: Option<String>,
}

/// What the engine of a session said it can do, in the client's words.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Capabilities {
    known: bool,
    engine: String,
    session: u32,
    agent: Option<(String, Option<String>)>,
    mode_current: Option<String>,
    modes: Vec<String>,
    options: Vec<(String, String)>,
    commands: Vec<CommandInfo>,
    controls: Vec<String>,
    features: Vec<String>,
    image: bool,
    truncated: bool,
}

/// `text` as one clean, redacted line of at most [`FIELD_MAX`] characters; `None` when nothing is left.
fn field(text: &str) -> Option<String> {
    let clean = redact_tokens(&display_line(text));
    let clean = clean.trim();
    if clean.is_empty() {
        return None;
    }
    Some(clean.chars().take(FIELD_MAX).collect())
}

/// A command word: lower-case letters, digits, `-` and `_`, starting with a letter, at most [`COMMAND_MAX`] characters.
fn is_command_word(w: &str) -> bool {
    let mut chars = w.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && w.chars().count() <= COMMAND_MAX
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

fn scalar_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => field(s),
        serde_json::Value::Bool(b) => Some(if *b { "on" } else { "off" }.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Capabilities {
    /// A run with no capability record: nothing is known, and nothing is hidden because of it.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// The newest record of the run: the highest session, and the newest event of that session. [`Capabilities::unknown`] when the run has
    /// none.
    pub fn from_state(state: &RunState) -> Self {
        let mut best: Option<(u32, &str, SessionCapabilitiesPayload)> = None;
        for e in state.accepted() {
            if e.event_type != EventType::SessionCapabilities {
                continue;
            }
            let Payload::Inline { value } = &e.payload else {
                continue;
            };
            let Ok(p) = serde_json::from_value::<SessionCapabilitiesPayload>(value.clone()) else {
                continue;
            };
            let newer = best
                .as_ref()
                .is_none_or(|(s, id, _)| (p.session, e.event_id.as_str()) >= (*s, *id));
            if newer {
                best = Some((p.session, e.event_id.as_str(), p));
            }
        }
        best.map_or_else(Self::unknown, |(_, _, p)| Self::from_payload(&p))
    }

    /// The client's reading of one record.
    pub fn from_payload(p: &SessionCapabilitiesPayload) -> Self {
        let modes_current = p.modes.as_ref().and_then(|m| m.current.as_deref());
        let option_mode = p
            .config_options
            .iter()
            .find(|o| o.id == "mode")
            .and_then(|o| o.current.as_ref())
            .and_then(scalar_text);
        let mut commands = Vec::new();
        for c in &p.commands {
            if is_command_word(&c.name)
                && commands.len() < LIST_MAX
                && !commands.iter().any(|x: &CommandInfo| x.name == c.name)
            {
                commands.push(CommandInfo {
                    name: c.name.clone(),
                    description: c.description.as_deref().and_then(field),
                });
            }
        }
        let options = p
            .config_options
            .iter()
            .take(LIST_MAX)
            .filter_map(|o| {
                let value = o.current.as_ref().and_then(scalar_text)?;
                Some((field(&o.id)?, value))
            })
            .collect();
        Self {
            known: true,
            engine: field(&p.engine).unwrap_or_default(),
            session: p.session,
            agent: p
                .agent
                .as_ref()
                .and_then(|a| Some((field(&a.name)?, a.version.as_deref().and_then(field)))),
            mode_current: modes_current.and_then(field).or(option_mode),
            modes: p
                .modes
                .iter()
                .flat_map(|m| m.available.iter())
                .take(LIST_MAX)
                .filter_map(|m| field(&m.id))
                .collect(),
            options,
            commands,
            controls: harness_words(&p.harness.controls),
            features: harness_words(&p.harness.features),
            image: p
                .prompt_capabilities
                .as_ref()
                .and_then(|c| c.get("image"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            truncated: p.truncated,
        }
    }

    /// Applies a later update of the stored conversation to the snapshot: a
    /// `current_mode_update` sets the current mode, an `available_commands_update` replaces the advertised commands (a word that is not a
    /// plain command word is dropped). Another kind, a value of another type, or a run with no record changes nothing.
    pub fn apply_update(&mut self, kind: &str, data: &serde_json::Value) {
        if !self.known {
            return;
        }
        match kind {
            "current_mode_update" => {
                if let Some(mode) = data
                    .get("currentModeId")
                    .and_then(serde_json::Value::as_str)
                    .and_then(field)
                {
                    self.mode_current = Some(mode);
                }
            }
            "available_commands_update" => {
                let Some(list) = data
                    .get("availableCommands")
                    .and_then(serde_json::Value::as_array)
                else {
                    return;
                };
                let mut commands: Vec<CommandInfo> = Vec::new();
                for c in list {
                    let Some(name) = c.get("name").and_then(serde_json::Value::as_str) else {
                        continue;
                    };
                    if is_command_word(name)
                        && commands.len() < LIST_MAX
                        && !commands.iter().any(|x| x.name == name)
                    {
                        commands.push(CommandInfo {
                            name: name.to_string(),
                            description: c
                                .get("description")
                                .and_then(serde_json::Value::as_str)
                                .and_then(field),
                        });
                    }
                }
                self.commands = commands;
            }
            _ => {}
        }
    }

    /// A record arrived for this run.
    pub fn is_known(&self) -> bool {
        self.known
    }

    /// The engine's display label from the record (empty when unknown).
    pub fn engine_label(&self) -> String {
        self.engine.clone()
    }

    /// `name version` of the engine as its handshake gave it.
    pub fn agent_text(&self) -> Option<String> {
        let (name, version) = self.agent.as_ref()?;
        Some(match version {
            Some(v) => format!("{name} {v}"),
            None => name.clone(),
        })
    }

    /// The session the record describes (1 for the first, 2 after a fresh session).
    pub fn session(&self) -> u32 {
        self.session
    }

    pub fn current_mode(&self) -> Option<String> {
        self.mode_current.clone()
    }

    pub fn modes(&self) -> Vec<String> {
        self.modes.clone()
    }

    pub fn commands(&self) -> &[CommandInfo] {
        &self.commands
    }

    pub fn image_prompts(&self) -> bool {
        self.image
    }

    /// Sections were dropped from the record to fit the event.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Whether the session offers `need`, a control or a feature word. A run with no record offers everything: its absence says nothing.
    pub fn offers(&self, need: &str) -> bool {
        !self.known
            || self.controls.iter().any(|c| c == need)
            || self.features.iter().any(|f| f == need)
    }

    /// The words the status line adds for this session: the mode and the plain options (`mode act · auto_approve off`). Empty when
    /// unknown. The model and provider are left to the header, which names the model.
    pub fn status_words(&self) -> String {
        if !self.known {
            return String::new();
        }
        let mut parts = Vec::new();
        if let Some(m) = &self.mode_current {
            parts.push(format!("mode {m}"));
        }
        for (id, value) in self
            .options
            .iter()
            .filter(|(id, _)| !matches!(id.as_str(), "mode" | "model" | "provider"))
            .take(2)
        {
            parts.push(format!("{id} {value}"));
        }
        parts.join(" \u{b7} ")
    }

    /// The line to send to the engine for a typed `//word rest` when the record advertises `word`: `/word rest`. A single slash is a
    /// local command, never forwarded; a word the record does not list is never forwarded.
    pub fn forward(&self, line: &str) -> Option<String> {
        let rest = line.trim_start().strip_prefix("//")?;
        let word: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        if !is_command_word(&word) || !self.commands.iter().any(|c| c.name == word) {
            return None;
        }
        Some(format!("/{rest}"))
    }
}

fn harness_words(words: &[String]) -> Vec<String> {
    words
        .iter()
        .filter_map(|w| field(w))
        .take(LIST_MAX)
        .collect()
}

fn interned() -> &'static Mutex<HashMap<(String, String), &'static Entry>> {
    static TABLE: OnceLock<Mutex<HashMap<(String, String), &'static Entry>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The popup entry of an advertised command. Its name carries the second slash (`/review`, drawn `//review`); entries are made once per
/// distinct command and kept for the life of the process (at most [`INTERN_MAX`]), because the popup lists `&'static Entry`.
fn engine_entry(c: &CommandInfo) -> Option<&'static Entry> {
    let key = (c.name.clone(), c.description.clone().unwrap_or_default());
    let mut table = interned().lock().ok()?;
    if let Some(e) = table.get(&key) {
        return Some(e);
    }
    if table.len() >= INTERN_MAX {
        return None;
    }
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let help: String = c
        .description
        .clone()
        .unwrap_or_else(|| "A command of the engine".to_string())
        .chars()
        .take(80)
        .collect();
    let entry: &'static Entry = Box::leak(Box::new(Entry {
        name: leak(format!("/{}", c.name)),
        aliases: &[],
        kind: Kind::TuiOnly,
        usage: "[<text>]",
        help: leak(help),
        effects: Effects::NONE,
        run_arg: RunArg::None,
        min_args: 0,
        text: TextArg::Optional,
        lead: 0,
        ask: None,
        needs: None,
        status: Availability::Built,
    }));
    table.insert(key, entry);
    Some(entry)
}

/// The popup for the text after the first slash. The built commands the session offers come first, in registry order; then the engine's
/// own commands, named with a second slash. A prefix that starts with `/` (the user typed `//`) lists the engine's commands only.
pub fn popup(caps: &Capabilities, prefix: &str) -> Vec<&'static Entry> {
    let (engine_only, p) = match prefix.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, prefix),
    };
    let mut out = if engine_only {
        Vec::new()
    } else {
        registry::popup_with(p, &|need| caps.offers(need))
    };
    let p = p.to_ascii_lowercase();
    out.extend(
        caps.commands()
            .iter()
            .filter(|c| c.name.starts_with(&p))
            .filter_map(engine_entry),
    );
    out
}

/// The title of the `/engines` panel.
pub fn panel_title() -> &'static str {
    "Engines"
}

/// The rows of the `/engines` panel: a heading, then one row for each engine of the table. Every row but the heading is an engine. The row of
/// `current`, the engine the run is on, is `selected` and carries what the session said (or [`CAPABILITIES_UNKNOWN`] and the catalog's
/// defaults); another engine a person can choose says `available`; the rest say [`NOT_AVAILABLE`]. Rows are short enough not to wrap in a
/// 76-column panel.
pub fn panel_rows(caps: &Capabilities, current: &Engine) -> Vec<String> {
    let mut rows = vec!["Engines of this release:".to_string()];
    for e in roadmap() {
        match e.state {
            EngineState::Available if e.id != current.id => {
                rows.push(format!(
                    "{:<10} available \u{b7} choose it with --engine {}",
                    e.id, e.id
                ));
            }
            EngineState::Available => {
                let detail = if caps.is_known() {
                    let who = caps
                        .agent_text()
                        .or_else(|| field(&caps.engine))
                        .unwrap_or_else(|| e.name.to_string());
                    match caps.status_words() {
                        w if w.is_empty() => who,
                        w => format!("{who} \u{b7} {w}"),
                    }
                } else {
                    let defaults = crate::models::builtin()
                        .default_model()
                        .map(|m| format!("{} defaults", m.name))
                        .unwrap_or_default();
                    format!("{CAPABILITIES_UNKNOWN} \u{b7} {defaults}")
                };
                rows.push(format!("{:<10} selected \u{b7} {detail}", e.id));
            }
            EngineState::NotAvailable => rows.push(format!("{:<10} {NOT_AVAILABLE}", e.id)),
        }
    }
    rows
}
