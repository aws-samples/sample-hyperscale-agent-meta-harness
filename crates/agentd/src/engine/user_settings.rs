//! The user's engine settings, typed.
//!
//! `ham` reads `~/.config/ham/engines/<engine>.toml` on the Mac and sends the values in the optional `engineConfig` member of the start request.
//! `harness_protocol::direct::check_engine_config` is the one check of what that member may hold (`ham`, control-api and agentd all call it); this
//! module turns a member that passed it into [`UserSettings`]: enum members and numbers in the engine's own ranges. No text of the user's file is
//! kept, so no user string can reach a file, a command line or a variable: what agentd writes is made of the harness's own words
//! ([`UserTool::name`], [`Effort::flag`], the keys of the compaction block) and numbers.
//!
//! Where each setting goes (the Cline words live here and in `engine/direct.rs` and `engine/config.rs`, nowhere else in `src`):
//!
//! | setting | placed as | by |
//! |---|---|---|
//! | `reasoning.effort` | the argument `--thinking <level>`; `default` takes the level of the model spec (`DirectModelSpec.defaultReasoningEffort`, the model's catalog row), `off` adds none | `direct::direct_launch_argv` |
//! | `commands.timeout_seconds` | the variable `CLINE_RUN_COMMANDS_TIMEOUT_MS` (milliseconds) | `direct::launch_env` |
//! | `context.compaction`, `context.compaction_trigger` | `settings.compaction` of `providers.json` | `config::emit` through [`CompactionBlock`] |
//! | `tools.disable` | the union with the stage's list in the locked `global-settings.json` | [`UserSettings::disabled_tools`], `direct::lock_global_settings` |
//!
//! [`read_back`] proves, before the first model call, that the argument, the variable and the two files hold what the settings call for. It proves
//! that agentd wrote them. That the engine accepted them is proven by the probe records of the key table (the admission rule), and the engine's own
//! report where one exists (`_meta.harness.compaction`, `config tools --json`).

use super::config::providers_path;
use super::direct::{TIMEOUT_ENV, locked_settings_path};
use harness_protocol::direct::EngineConfig;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// The engine this module places settings for.
const ENGINE: &str = "cline";
/// The environment variable that names the locked global settings file (`direct::lock_global_settings` sets it).
const GLOBAL_SETTINGS_ENV: &str = "CLINE_GLOBAL_SETTINGS_PATH";
/// The argument that sets the reasoning level.
const THINKING_FLAG: &str = "--thinking";

// ---- the typed values -----------------------------------------------------------------------------------------------------------------

/// `reasoning.effort`: the six values of the key. `none`, `minimal` and `max` are refused by the shared check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Effort {
    /// The level of the model spec (the model's catalog row); with no level in the spec, no argument (the launch of 1.45.0).
    #[default]
    Default,
    /// No argument: the engine reasons as it does without the flag (GLM 5.3 cannot be switched off and reasons at its own lowest level).
    Off,
    Low,
    Medium,
    High,
    XHigh,
}

impl Effort {
    /// The word after `--thinking`; `None` for `default` and `off`, which add no argument themselves.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Self::Default | Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "off" => Some(Self::Off),
            _ => Self::from_level(s),
        }
    }

    /// The launch level a word names (`low`, `medium`, `high`, `xhigh`); any other word is no level.
    pub fn from_level(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            _ => None,
        }
    }
}

/// Where the level of a launch came from, as `summary.json` words it (`DirectSummaryEngine.reasoningSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningSource {
    /// The model spec's level (the catalog row), because the user kept `default` or set nothing.
    Default,
    /// The user's own level.
    Explicit,
    /// The user turned reasoning off.
    Off,
}

impl ReasoningSource {
    /// The word `summary.json` records.
    pub fn word(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Explicit => "explicit",
            Self::Off => "off",
        }
    }
}

/// What a launch does about reasoning: the effort the argv is built with (never `Default` when a source is set: it was resolved) and where it came
/// from. `source` is `None` when nothing was asked of the engine (no setting and no level in the spec): the launch of 1.45.0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReasoningLaunch {
    pub effort: Effort,
    pub source: Option<ReasoningSource>,
}

impl ReasoningLaunch {
    /// A launch that adds and records nothing.
    pub const NONE: Self = Self {
        effort: Effort::Default,
        source: None,
    };
}

/// `commands.timeout_seconds`: one shell command's limit, 1 to 1,800 seconds (the engine's own range, `CLINE_RUN_COMMANDS_TIMEOUT_MS` from 1,000 to
/// 1,800,000 ms). Only [`CommandSeconds::new`] makes one, so a value outside the range does not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSeconds(u32);

impl CommandSeconds {
    pub const MIN: u32 = 1;
    pub const MAX: u32 = 1800;

    pub fn new(seconds: u32) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&seconds)
            .then_some(Self(seconds))
    }

    pub fn get(self) -> u32 {
        self.0
    }

    /// The limit in milliseconds, the unit of the variable. A number of at most 1,800 seconds times 1,000 fits a `u64` many times over; the checked
    /// product states it, and the fallback is the engine's own ceiling, which a value of this type never reaches.
    pub fn millis(self) -> u64 {
        u64::from(self.0)
            .checked_mul(1000)
            .unwrap_or(u64::from(Self::MAX) * 1000)
    }
}

/// `context.compaction`: how the engine shortens a conversation that reaches the trigger. `Off` ends a run that overflows its context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionMode {
    Agentic,
    Basic,
    Off,
}

/// `context.compaction_trigger`: the share of the context window at which the engine compacts, 0.10 to 0.99. Only [`Trigger::new`] makes one, so a
/// value that is not a finite number in the range does not exist (and `Eq` is sound: there is no NaN).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trigger(f64);

impl Eq for Trigger {}

impl Trigger {
    pub const MIN: f64 = 0.10;
    pub const MAX: f64 = 0.99;

    pub fn new(ratio: f64) -> Option<Self> {
        (ratio.is_finite() && (Self::MIN..=Self::MAX).contains(&ratio)).then_some(Self(ratio))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// A tool the user may switch off (`tools.disable`); the closed set of the key table, so the name written is the harness's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserTool {
    SpawnAgent,
    FetchWebContent,
}

impl UserTool {
    /// The tools of the key table, in the table's order.
    const ALL: [Self; 2] = [Self::SpawnAgent, Self::FetchWebContent];

    pub fn name(self) -> &'static str {
        match self {
            Self::SpawnAgent => "spawn_agent",
            Self::FetchWebContent => "fetch_web_content",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == s)
    }
}

/// The compaction block of `providers.json` (`settings.compaction`): only the keys the user's settings name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactionBlock {
    pub enabled: Option<bool>,
    pub strategy: Option<&'static str>,
    pub threshold_ratio: Option<Trigger>,
}

impl CompactionBlock {
    /// The block as JSON, made with `json!` over typed values: a bool, one of two fixed words, a number.
    pub fn to_json(&self) -> Value {
        let mut block = Map::new();
        if let Some(enabled) = self.enabled {
            block.insert("enabled".to_string(), json!(enabled));
        }
        if let Some(strategy) = self.strategy {
            block.insert("strategy".to_string(), json!(strategy));
        }
        if let Some(ratio) = self.threshold_ratio {
            block.insert("thresholdRatio".to_string(), json!(ratio.get()));
        }
        Value::Object(block)
    }
}

// ---- the settings -----------------------------------------------------------------------------------------------------------------------

/// The user's settings for one run. `Default` is a run with none: nothing is placed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserSettings {
    pub effort: Effort,
    pub command_timeout: Option<CommandSeconds>,
    pub compaction: Option<CompactionMode>,
    pub compaction_trigger: Option<Trigger>,
    /// Distinct, in the key table's order.
    pub disable_tools: Vec<UserTool>,
}

impl UserSettings {
    /// The typed settings of a member that passed `check_engine_config`. A value of the wrong kind or outside its range (a member that was not
    /// checked) is an error that names the key and never the value, never a default.
    pub fn from_checked(cfg: &EngineConfig) -> Result<Self, String> {
        let bad = |path: &str| {
            format!("engine settings: {path} holds a value this build does not accept")
        };
        let mut s = Self::default();
        if let Some(v) = cfg.get("reasoning.effort") {
            s.effort = v
                .as_str()
                .and_then(Effort::parse)
                .ok_or_else(|| bad("reasoning.effort"))?;
        }
        if let Some(v) = cfg.get("commands.timeout_seconds") {
            s.command_timeout = Some(
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .and_then(CommandSeconds::new)
                    .ok_or_else(|| bad("commands.timeout_seconds"))?,
            );
        }
        if let Some(v) = cfg.get("context.compaction") {
            s.compaction = Some(match v.as_str() {
                Some("agentic") => CompactionMode::Agentic,
                Some("basic") => CompactionMode::Basic,
                Some("off") => CompactionMode::Off,
                _ => return Err(bad("context.compaction")),
            });
        }
        if let Some(v) = cfg.get("context.compaction_trigger") {
            s.compaction_trigger = Some(
                v.as_f64()
                    .and_then(Trigger::new)
                    .ok_or_else(|| bad("context.compaction_trigger"))?,
            );
        }
        if let Some(v) = cfg.get("tools.disable") {
            let mut asked = Vec::new();
            for entry in v.as_array().ok_or_else(|| bad("tools.disable"))? {
                asked.push(
                    entry
                        .as_str()
                        .and_then(UserTool::parse)
                        .ok_or_else(|| bad("tools.disable"))?,
                );
            }
            s.disable_tools = UserTool::ALL
                .into_iter()
                .filter(|t| asked.contains(t))
                .collect();
        }
        Ok(s)
    }

    /// Whether nothing is placed: no argument, no variable, no compaction key, no tool.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The reasoning of the launch: the user's own level wins, `off` adds no argument, and `default` (or no setting) takes
    /// `model_default`, the level of the model spec. A word of the spec that is not a launch level counts as no level.
    pub fn reasoning_launch(&self, model_default: Option<&str>) -> ReasoningLaunch {
        match self.effort {
            Effort::Off => ReasoningLaunch {
                effort: Effort::Off,
                source: Some(ReasoningSource::Off),
            },
            Effort::Default => match model_default.and_then(Effort::from_level) {
                Some(effort) => ReasoningLaunch {
                    effort,
                    source: Some(ReasoningSource::Default),
                },
                None => ReasoningLaunch::NONE,
            },
            effort => ReasoningLaunch {
                effort,
                source: Some(ReasoningSource::Explicit),
            },
        }
    }

    /// The compaction block the settings call for; `None` leaves `providers.json` as the harness wrote it before.
    pub fn compaction_block(&self) -> Option<CompactionBlock> {
        let enabled = self
            .compaction
            .map(|mode| !matches!(mode, CompactionMode::Off));
        let strategy = match self.compaction {
            Some(CompactionMode::Agentic) => Some("agentic"),
            Some(CompactionMode::Basic) => Some("basic"),
            _ => None,
        };
        (enabled.is_some() || self.compaction_trigger.is_some()).then_some(CompactionBlock {
            enabled,
            strategy,
            threshold_ratio: self.compaction_trigger,
        })
    }

    /// The tools to switch off: the stage's list as it is, then the user's tools that the stage does not already name. A user cannot remove a
    /// stage entry; there is no way to say so.
    pub fn disabled_tools(&self, stage: &[String]) -> Vec<String> {
        let mut out = stage.to_vec();
        for tool in &self.disable_tools {
            if !out.iter().any(|n| n == tool.name()) {
                out.push(tool.name().to_string());
            }
        }
        out
    }
}

// ---- the read-back ----------------------------------------------------------------------------------------------------------------------

/// Reads the launch argv back and compares it with the reasoning the launch was meant to carry: the `--thinking` level when there is one, no
/// `--thinking` when the user turned it off. A launch that asked nothing (`source` is `None`) is not looked at: a profile may name its own level. The
/// words name the part and never the level.
pub fn check_reasoning_launch(argv: &[String], launch: &ReasoningLaunch) -> Result<(), String> {
    let at = argv.iter().rposition(|a| a == THINKING_FLAG);
    let got = at.and_then(|i| argv.get(i + 1)).map(String::as_str);
    match (launch.source, launch.effort.flag()) {
        (None, _) => Ok(()),
        (Some(_), Some(level)) if got == Some(level) => Ok(()),
        (Some(ReasoningSource::Off), None) if at.is_none() => Ok(()),
        _ => Err(
            "engine settings: the engine's launch does not hold the reasoning level".to_string(),
        ),
    }
}

/// What the startup placed, for [`read_back`]: where the files are, the launch argv and the environment the engine will get, and the stage's own
/// list of tools switched off.
#[derive(Clone, Copy, Debug)]
pub struct Placed<'a> {
    pub engine: &'a str,
    pub config_dir: &'a Path,
    pub data_dir: &'a Path,
    pub argv: &'a [String],
    pub env: &'a BTreeMap<String, String>,
    pub stage_disabled: &'a [String],
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Reads the placement back and compares it with what the settings call for: the `--thinking` level in the argv, the command time limit in the
/// environment, `settings.compaction` and `updatedAt` in `providers.json`, and in the locked global settings the two locked keys and the union of the
/// tools switched off, read from the file the engine's variable names. Only what the user set is looked at: a stage may set its own default for
/// the variable or the level. The error names the part that differs and never a value.
pub fn read_back(placed: &Placed<'_>, settings: &UserSettings) -> Result<(), String> {
    if placed.engine != ENGINE {
        return Err(
            "engine settings: this build cannot place settings for that engine".to_string(),
        );
    }
    if let Some(level) = settings.effort.flag() {
        let at = placed.argv.iter().rposition(|a| a == THINKING_FLAG);
        let got = at.and_then(|i| placed.argv.get(i + 1));
        if got.map(String::as_str) != Some(level) {
            return Err(
                "engine settings: the engine's launch does not hold the reasoning level"
                    .to_string(),
            );
        }
    }
    if let Some(limit) = settings.command_timeout
        && placed.env.get(TIMEOUT_ENV).map(String::as_str)
            != Some(limit.millis().to_string().as_str())
    {
        return Err(
            "engine settings: the engine's environment does not hold the command time limit"
                .to_string(),
        );
    }
    let provider_problem = || {
        "engine settings: the provider file does not hold the compaction settings that were written"
            .to_string()
    };
    let providers = read_json(&providers_path(placed.data_dir)).ok_or_else(provider_problem)?;
    let entry = &providers["providers"]["bedrock"];
    // Without the datetime the engine drops the whole file with no error.
    if !entry["updatedAt"].as_str().is_some_and(|t| !t.is_empty()) {
        return Err(provider_problem());
    }
    let want = settings.compaction_block().map(|b| b.to_json());
    if entry["settings"].get("compaction") != want.as_ref() {
        return Err(provider_problem());
    }
    let locked_problem =
        || "engine settings: the locked global settings do not hold what was written".to_string();
    let locked = locked_settings_path(placed.config_dir);
    if placed.env.get(GLOBAL_SETTINGS_ENV).map(String::as_str) != locked.to_str() {
        return Err(locked_problem());
    }
    let doc = read_json(&locked).ok_or_else(locked_problem)?;
    let want_tools = json!(settings.disabled_tools(placed.stage_disabled));
    if doc["telemetryOptOut"] != json!(true)
        || doc["autoUpdateEnabled"] != json!(false)
        || doc["disabledTools"] != want_tools
    {
        return Err(locked_problem());
    }
    Ok(())
}
