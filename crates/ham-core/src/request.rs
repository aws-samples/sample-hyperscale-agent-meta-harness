//! Builds `POST /v1/direct/runs` bodies and their `Idempotency-Key`.

use crate::command::{DirectArgs, TargetArg};
use crate::config::{Config, RunTable};
use crate::engines::{self, Engine};
use crate::models::{self, ModelError, ModelPrefs, ModelSource, Models, resolve};
use crate::settings::Key;
use harness_protocol::api::v1::{ContinueSettings, RuleFile, RunTarget, StartDirectRunRequest};
use harness_protocol::common::Engine as WireEngine;
use harness_protocol::direct::{
    Attachment, BuildOptions, ComputeMode, ComputeOptions, ContinueFrom, EngineConfig, GoalOptions,
    ReviewMode, ReviewOptions, RunLimits, Skill,
};
use harness_protocol::research::{ResearchOptions, ThirdPartyMode};
use sha2::{Digest, Sha256};

pub fn run_target(t: &TargetArg) -> RunTarget {
    match t {
        TargetArg::Repo(repo) => RunTarget::Repo { repo: repo.clone() },
        TargetArg::NewRepo(repo) => RunTarget::NewRepo { repo: repo.clone() },
        TargetArg::S3Only => RunTarget::S3Only,
    }
}

/// What a direct run reads from disk: the rules files and, when the user has one, the nudge file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DirectInputs {
    pub rules: Vec<RuleFile>,
    pub nudge: Option<ContinueSettings>,
    /// The skills read from the skills directory; empty sends none.
    pub skills: Vec<Skill>,
    /// The files `--attach` named, read from disk; empty sends none.
    pub attachments: Vec<Attachment>,
    /// The settings of the engine file (the configuration program, `crate::engine_file`); `None` for a user with no file or a file that sets no key,
    /// and the request then has no `engineConfig` member.
    pub engine_config: Option<EngineConfig>,
    /// The user's review file as `rules::parse_review` read it; `None` when the user has none, and the request
    /// then carries no text and agentd uses its built-in prompt.
    pub review: Option<ReviewOptions>,
}

/// The direct run's request: the prompt exactly as typed, the target, the one model and the inputs. The model is the first set of
/// the `--model` flag, `prefs.env` (`HAMH_MODEL`), `prefs.config` (`default_model`) and the built-in default, Claude Sonnet 5.5
/// (`crate::models`); an explicit model always wins, and a value that is set and wrong stops the request. The wire always
/// carries the full model id, so control-api sees an explicit choice either way.
pub fn direct_run_request(
    args: &DirectArgs,
    inputs: DirectInputs,
    prefs: &ModelPrefs,
) -> Result<StartDirectRunRequest, ModelError> {
    let model = resolve(models::builtin(), args.model.as_deref(), prefs)?;
    Ok(StartDirectRunRequest {
        prompt: args.prompt.clone(),
        target: run_target(&args.target),
        base_ref: args.base.clone(),
        model: model.model.id.clone(),
        rules: inputs.rules,
        nudge: inputs.nudge,
        // 0 is never sent: a request without the member waits for nobody, as every request did.
        input_wait_s: args.input_wait.filter(|m| *m > 0).map(|m| m * 60),
        skills: inputs.skills,
        // The head is left out: control-api resolves the delivered head of the source run and stores it.
        continue_from: args.continue_from.as_ref().map(|run_id| ContinueFrom {
            run_id: run_id.clone(),
            head_sha: None,
        }),
        // Absent means the stage default: research on where the stage has the route, off where it has none.
        research: args.research.map(|enabled| ResearchOptions { enabled }),
        report_tests: None,
        report_step_s: None,
        // Absent means the stage default, which is on; `--goal off` is `{"enabled": false}`.
        goal: args.goal.map(GoalOptions::switch),
        third_party: args.allow_third_party.then_some(ThirdPartyMode::Allow),
        attachments: inputs.attachments,
        // The settings of the engine file, when the user has one that sets a key; otherwise the member is left out.
        engine_config: inputs.engine_config,
        // The user's review file as read; `ResolvedRun::request` replaces it with the switch when the setting says `off`.
        review: inputs.review,
        // Left out here: `ResolvedRun::request` lays the user's `run.compute` in, so a bare `direct_run_request` is the request of an older client.
        compute: None,
        // Left out here as well: `ResolvedRun::request` lays the user's build switches in.
        build: None,
        // Left out here too: `ResolvedRun::request` lays the user's total run time in.
        max_run_s: None,
        // Left out here as well: `ResolvedRun::request` lays the user's counts and research budgets in.
        limits: None,
        // Left out here: `ResolvedRun::request` lays the engine of the choice in, so a bare `direct_run_request` is the request of an older client.
        engine: None,
        // Left out here as well: `ResolvedRun::request` lays the user's provider data sharing in, so a bare request is the one of an older client.
        provider_sharing: None,
    })
}

/// How long an interactive `ham` asks a run to wait for the user's answer when Cline needs input, unless the user says otherwise: the
/// 30 minutes of `harness_protocol::direct::DEFAULT_CLIENT_INPUT_WAIT_S`.
pub const DEFAULT_INTERACTIVE_WAIT_MINUTES: u32 = 30;

/// Where the wait of a run came from, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitSource {
    /// `--input-wait <minutes>`.
    Flag,
    /// `HAMH_INPUT_WAIT`.
    Env,
    /// `input_wait_minutes` in `config.toml`.
    Config,
    /// Nothing was set: 30 minutes for an interactive client, none otherwise.
    Default,
}

impl WaitSource {
    pub fn label(self) -> &'static str {
        match self {
            WaitSource::Flag => "--input-wait",
            WaitSource::Env => "HAMH_INPUT_WAIT",
            WaitSource::Config => "input_wait_minutes in config.toml",
            WaitSource::Default => "the default",
        }
    }
}

/// The layers below the flag, and whether the client is interactive (a terminal on both ends, so the dashboard can show the question).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WaitPrefs {
    /// `HAMH_INPUT_WAIT` as set; blank is unset.
    pub env: Option<String>,
    /// `input_wait_minutes` of the config file.
    pub config: Option<u32>,
    pub interactive: bool,
}

/// A wait that names no valid number of minutes, with the layer it came from.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}: takes whole minutes from 0 to {}, got {value:?}", .layer.label(), crate::command::MAX_INPUT_WAIT_MINUTES)]
pub struct WaitError {
    pub layer: WaitSource,
    pub value: String,
}

/// The wait in minutes for a new run, with the layer that set it: the flag, `HAMH_INPUT_WAIT`, `input_wait_minutes`, then 30 for an
/// interactive client and 0 for any other. A value that is set and wrong is an error naming its layer; it never falls through.
pub fn input_wait_minutes(
    flag: Option<u32>,
    prefs: &WaitPrefs,
) -> Result<(u32, WaitSource), WaitError> {
    let max = crate::command::MAX_INPUT_WAIT_MINUTES;
    let check = |m: u32, source: WaitSource| {
        if m <= max {
            Ok((m, source))
        } else {
            Err(WaitError {
                layer: source,
                value: m.to_string(),
            })
        }
    };
    if let Some(m) = flag {
        return check(m, WaitSource::Flag);
    }
    if let Some(v) = prefs
        .env
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return match v.parse::<u32>() {
            Ok(m) => check(m, WaitSource::Env),
            Err(_) => Err(WaitError {
                layer: WaitSource::Env,
                value: v.to_string(),
            }),
        };
    }
    if let Some(m) = prefs.config {
        return check(m, WaitSource::Config);
    }
    let m = if prefs.interactive {
        DEFAULT_INTERACTIVE_WAIT_MINUTES
    } else {
        0
    };
    Ok((m, WaitSource::Default))
}

/// `args` with the wait the layers decide, for [`direct_run_request`].
pub fn with_input_wait(args: &DirectArgs, prefs: &WaitPrefs) -> Result<DirectArgs, WaitError> {
    let (minutes, _) = input_wait_minutes(args.input_wait, prefs)?;
    Ok(DirectArgs {
        input_wait: Some(minutes),
        ..args.clone()
    })
}

/// Where a resolved setting came from, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A flag of this start (`--model`, `--goal`, `--no-rules` ...).
    Flag,
    /// An environment variable, named.
    Env(&'static str),
    /// `config.toml`.
    File,
    /// Nothing set it: the built-in default, or the stage's own when the request leaves the member out.
    Default,
}

impl Origin {
    /// The word `ham config show` and the Settings screen print.
    pub fn label(self) -> String {
        match self {
            Origin::Flag => "flag".to_string(),
            Origin::Env(name) => format!("env {name}"),
            Origin::File => "file".to_string(),
            Origin::Default => "default".to_string(),
        }
    }
}

/// The model a start uses and the layer that chose it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChosenModel {
    pub id: String,
    pub name: String,
    pub source: ModelSource,
}

/// A layer of a run setting that holds a value this build refuses, named by layer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Wait(#[from] WaitError),
    /// `run.container_signals` of a `Config` that did not come through the reader; the reader and `ham config set` refuse such a value first.
    #[error("run.container_signals: {0}")]
    ContainerSignals(String),
    /// `--engine` or `default_engine` names no engine a person can choose; the layer is named.
    #[error(transparent)]
    Engine(#[from] engines::LayerError),
}

/// What the flags, the environment and `config.toml` decide for one run (see [`resolve_run_settings`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRun {
    /// The arguments with the settings applied: the wait, the goal loop, research, and the rules and skills switches.
    pub args: DirectArgs,
    /// The two weaker layers of the model choice, for [`direct_run_request`].
    pub prefs: ModelPrefs,
    /// `report_tests` of the file: only `false` reaches the request.
    pub report_tests: Option<bool>,
    /// `run.report_command_minutes` of the file in seconds; 0 and unset send nothing, so the stage's own limit applies.
    pub report_step_s: Option<u32>,
    /// The limits of the stall brake and the final-message switch of the file: what is set reaches the request's `goal` member.
    pub goal_limits: GoalLimits,
    /// `run.review_round` of the file: `Off` reaches the request as the `review` member that switches the round off; `Auto` and unset send
    /// only what the user's review file says.
    pub review_round: Option<ReviewMode>,
    /// The machine kind of this run: `--compute` over `run.compute` of the file. `Microvm` and `Instances` reach the request as the `compute`
    /// member; `Auto` and unset send none, so the stage's own policy decides and the request is the one an older client sends.
    pub compute: Option<ComputeMode>,
    /// `run.container_signals` of the file as the entries of the list: they reach the request inside `compute`, also when the mode is `auto`.
    /// Unset sends none, so the stage's built-in list decides and the request is the one an older client sends.
    pub container_signals: Option<Vec<String>>,
    /// The most seconds this run may take in all: `--max-run-hours` over `run.max_run_hours` of the file, in seconds. Unset sends no member, so the
    /// stage's own 24 hours applies and the request is the one an older client sends.
    pub max_run_s: Option<u32>,
    /// Provider data sharing for this run: `--provider-sharing` over `run.provider_sharing` of the file, off when neither is set. Only `true` reaches the
    /// request, as `providerSharing`, so the request of a user who never turns it on is the one an older client sends.
    pub provider_sharing: bool,
    /// The three build switches of the file: only a `false`
    /// reaches the request, as the `build` member.
    pub build: BuildOptions,
    /// The six keys that move limits of the worker:
    /// what is set reaches the request as the `limits` member, the
    /// research budgets in bytes. Nothing set sends no member.
    pub limits: RunLimits,
    /// The engine of this run: `--engine` over `default_engine` of the file over the default engine.
    pub engine: &'static Engine,
    pub model: ChosenModel,
    origins: Vec<(Key, Origin)>,
}

impl ResolvedRun {
    /// Where the value of `key` came from; `None` for a key that takes no part in starting a run.
    pub fn origin(&self, key: Key) -> Option<Origin> {
        self.origins
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, o)| *o)
    }

    /// Every run key with its origin, in registry order.
    pub fn origins(&self) -> &[(Key, Origin)] {
        &self.origins
    }

    /// The id the request's `engine` member carries: `None` for the default engine, so a run on it sends the bytes an older client sends (and a
    /// stage that predates the member keeps taking it), and the id of any other engine.
    pub fn engine_member(&self) -> Option<&'static str> {
        (!self.engine.is_default()).then_some(self.engine.id)
    }

    /// The request for `inputs` (what the caller read from disk), with the report switch applied (only `false` is sent) and the stall limits
    /// of the file laid into the `goal` member.
    pub fn request(&self, inputs: DirectInputs) -> Result<StartDirectRunRequest, ModelError> {
        direct_run_request(&self.args, inputs, &self.prefs)
            .map(|r| {
                let mut r = with_report_setting(r, self.report_tests);
                r.report_step_s = self.report_step_s;
                r
            })
            .map(|r| with_goal_limits(r, &self.goal_limits))
            .map(|r| with_review_setting(r, self.review_round))
            .map(|r| with_compute_choice(r, self.compute, self.container_signals.as_deref()))
            .map(|r| with_max_run_setting(r, self.max_run_s))
            .map(|r| with_provider_sharing_setting(r, self.provider_sharing))
            .map(|r| with_build_settings(r, &self.build))
            .map(|r| with_limits_setting(r, &self.limits))
            .map(|r| with_engine_choice(r, self.engine))
    }
}

/// The settings of a run, from the flags in `args`, the environment `env` and the config file `cfg`: one function for the three places
/// that start a run (`ham "<prompt>" --direct`, the composer and the follow-up), so a key of the file reaches all three. For each key the
/// first layer that is set wins: the flag, `HAMH_MODEL` and `HAMH_INPUT_WAIT`, the file, then the default (the stage's own for the goal
/// loop and research, which leave their members out). A value that is set and wrong is an error that names its layer and never falls
/// through to a weaker one. `interactive` says a terminal shows the dashboard, which decides the wait of a run when nothing sets it.
///
/// - `run.goal` and `research.enabled` become `args.goal` and `args.research` unless a flag set them;
/// - `run.rules = false` is `--no-rules` and `run.skills = false` is `--no-skills`, unless `--rules <dir>` or `--skills <dir>` names a folder;
/// - the wait is the number [`input_wait_minutes`] decides; `report_tests` and `run.report_command_minutes` (as seconds) are the file's.
pub fn resolve_run_settings(
    cfg: &Config,
    env: &dyn Fn(&str) -> Option<String>,
    args: &DirectArgs,
    interactive: bool,
) -> Result<ResolvedRun, SettingsError> {
    resolve_run_settings_in(models::builtin(), cfg, env, args, interactive)
}

/// [`resolve_run_settings`] over another model list (tests): the chosen model is refused here, before any request exists, when its row needs provider data
/// sharing and the run has it off. The refusal names the model, the reason and the three ways to turn the setting on.
pub fn resolve_run_settings_in(
    models: &Models,
    cfg: &Config,
    env: &dyn Fn(&str) -> Option<String>,
    args: &DirectArgs,
    interactive: bool,
) -> Result<ResolvedRun, SettingsError> {
    let prefs = ModelPrefs {
        env: env("HAMH_MODEL"),
        config: cfg.default_model.clone(),
    };
    let chosen = resolve(models, args.model.as_deref(), &prefs)?;
    let run = cfg.run.clone().unwrap_or_default();
    let provider_sharing = args
        .provider_sharing
        .or(run.provider_sharing)
        .unwrap_or(false);
    chosen
        .model
        .check_sharing(provider_sharing)
        .map_err(ModelError::NeedsSharing)?;
    let model = ChosenModel {
        id: chosen.model.id.clone(),
        name: chosen.model.name.clone(),
        source: chosen.source,
    };
    let wait = WaitPrefs {
        env: env("HAMH_INPUT_WAIT"),
        config: cfg.input_wait_minutes,
        interactive,
    };
    let (minutes, wait_source) = input_wait_minutes(args.input_wait, &wait)?;
    let research = cfg.research.clone().unwrap_or_default();
    let mut out = args.clone();
    out.input_wait = Some(minutes);
    out.goal = args.goal.or(run.goal);
    out.research = args.research.or(research.enabled);
    out.no_rules = args.no_rules || (args.rules.is_none() && run.rules == Some(false));
    out.no_skills = args.no_skills || (args.skills.is_none() && run.skills == Some(false));
    let layer = |flag: bool, file: bool| {
        if flag {
            Origin::Flag
        } else if file {
            Origin::File
        } else {
            Origin::Default
        }
    };
    let origins = vec![
        (
            Key::DefaultModel,
            match chosen.source {
                ModelSource::Flag => Origin::Flag,
                ModelSource::Env => Origin::Env(crate::envnames::answered_by(env, "HAMH_MODEL")),
                ModelSource::Config => Origin::File,
                ModelSource::BuiltIn | ModelSource::Given => Origin::Default,
            },
        ),
        (
            Key::InputWaitMinutes,
            match wait_source {
                WaitSource::Flag => Origin::Flag,
                WaitSource::Env => {
                    Origin::Env(crate::envnames::answered_by(env, "HAMH_INPUT_WAIT"))
                }
                WaitSource::Config => Origin::File,
                WaitSource::Default => Origin::Default,
            },
        ),
        (Key::ReportTests, layer(false, cfg.report_tests.is_some())),
        (
            Key::RunReportCommandMinutes,
            layer(false, run.report_command_minutes.is_some()),
        ),
        (Key::RunGoal, layer(args.goal.is_some(), run.goal.is_some())),
        (
            Key::RunRules,
            layer(args.no_rules || args.rules.is_some(), run.rules.is_some()),
        ),
        (
            Key::RunSkills,
            layer(
                args.no_skills || args.skills.is_some(),
                run.skills.is_some(),
            ),
        ),
        (
            Key::ResearchEnabled,
            layer(args.research.is_some(), research.enabled.is_some()),
        ),
    ];
    let mut origins = origins;
    let goal_limits = GoalLimits::of(&run);
    for (key, set) in [
        (Key::RunStallCalls, run.stall_calls.is_some()),
        (
            Key::RunStallWindowMinutes,
            run.stall_window_minutes.is_some(),
        ),
        (Key::RunStallMinCalls, run.stall_min_calls.is_some()),
        (
            Key::RunCommandGraceMinutes,
            run.command_grace_minutes.is_some(),
        ),
        (Key::RunFinalMessage, run.final_message.is_some()),
        (Key::RunReviewRound, run.review_round.is_some()),
        (Key::RunStallNewCalls, run.stall_new_calls.is_some()),
        (Key::RunCargoNoDebugInfo, run.cargo_no_debug_info.is_some()),
        (
            Key::RunCargoNoIncremental,
            run.cargo_no_incremental.is_some(),
        ),
        (Key::RunExcludeTargetDir, run.exclude_target_dir.is_some()),
        (
            Key::RunOutsideCheckoutNote,
            run.outside_checkout_note.is_some(),
        ),
        (Key::RunMaxInterrupts, run.max_interrupts.is_some()),
        (Key::RunMaxQuestions, run.max_questions.is_some()),
        (Key::RunMaxFreshSessions, run.max_fresh_sessions.is_some()),
        (Key::ResearchMaxFileMib, research.max_file_mib.is_some()),
        (
            Key::ResearchMaxUnpackedMib,
            research.max_unpacked_mib.is_some(),
        ),
        (Key::ResearchMaxRunMib, research.max_run_mib.is_some()),
    ] {
        origins.push((key, layer(false, set)));
    }
    let compute = args.compute.or(run.compute);
    origins.push((
        Key::RunCompute,
        layer(args.compute.is_some(), run.compute.is_some()),
    ));
    let container_signals = match run.container_signals.as_deref() {
        Some(text) => Some(
            crate::container_signals::parse_setting(text)
                .map_err(SettingsError::ContainerSignals)?,
        ),
        None => None,
    };
    origins.push((
        Key::RunContainerSignals,
        layer(false, run.container_signals.is_some()),
    ));
    let max_run_hours = args.max_run_hours.or(run.max_run_hours);
    origins.push((
        Key::RunMaxRunHours,
        layer(args.max_run_hours.is_some(), run.max_run_hours.is_some()),
    ));
    let (engine, engine_layer) =
        engines::resolve(args.engine.as_deref(), cfg.default_engine.as_deref())?;
    out.engine = Some(engine.id.to_string());
    origins.push((
        Key::DefaultEngine,
        match engine_layer {
            engines::Layer::Flag => Origin::Flag,
            engines::Layer::File => Origin::File,
            engines::Layer::Default => Origin::Default,
        },
    ));
    origins.push((
        Key::RunProviderSharing,
        layer(
            args.provider_sharing.is_some(),
            run.provider_sharing.is_some(),
        ),
    ));
    origins.sort_by_key(|(k, _)| *k);
    Ok(ResolvedRun {
        args: out,
        prefs,
        report_tests: cfg.report_tests,
        report_step_s: run
            .report_command_minutes
            .filter(|m| *m > 0)
            .map(|m| m * 60),
        goal_limits,
        review_round: run.review_round,
        compute,
        container_signals,
        max_run_s: max_run_hours.map(|h| h * 3600),
        provider_sharing,
        build: BuildOptions {
            cargo_no_debug_info: run.cargo_no_debug_info.filter(|on| !on),
            cargo_no_incremental: run.cargo_no_incremental.filter(|on| !on),
            exclude_target_dir: run.exclude_target_dir.filter(|on| !on),
        },
        limits: RunLimits {
            interrupts: run.max_interrupts,
            questions: run.max_questions,
            fresh_sessions: run.max_fresh_sessions,
            research_file_bytes: research.max_file_mib.map(mib),
            research_unpacked_bytes: research.max_unpacked_mib.map(mib),
            research_run_bytes: research.max_run_mib.map(mib),
        },
        engine,
        model,
        origins,
    })
}

/// The stall limits, the allowance of calls that bring something new and the final-message switch of `[run]` in the file.
/// Unset members stay unset: the request then leaves them out and the stage's own value applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GoalLimits {
    pub calls: Option<u32>,
    pub window_minutes: Option<u32>,
    pub min_calls: Option<u32>,
    pub grace_minutes: Option<u32>,
    pub final_message: Option<bool>,
    pub new_calls: Option<u32>,
    /// `run.outside_checkout_note`: whether the goal prompts carry the sentence about work outside the checkout.
    pub outside_note: Option<bool>,
}

impl GoalLimits {
    /// The limits the `[run]` table holds.
    pub fn of(run: &RunTable) -> Self {
        Self {
            calls: run.stall_calls,
            window_minutes: run.stall_window_minutes,
            min_calls: run.stall_min_calls,
            grace_minutes: run.command_grace_minutes,
            final_message: run.final_message,
            new_calls: run.stall_new_calls,
            outside_note: run.outside_checkout_note,
        }
    }

    fn is_set(&self) -> bool {
        *self != Self::default()
    }
}

/// `request` with the user's stall limits laid into its `goal` member: minutes become seconds, a member the user did not set stays out, and
/// a run whose goal loop is switched off (`--goal off`, `run.goal = false`) carries none of them, since nothing would read them. Without a
/// limit the request is the one it was.
pub fn with_goal_limits(
    request: StartDirectRunRequest,
    limits: &GoalLimits,
) -> StartDirectRunRequest {
    let mut request = request;
    let enabled = request.goal.is_none_or(|g| g.enabled);
    if !limits.is_set() || !enabled {
        return request;
    }
    request.goal = Some(GoalOptions {
        enabled: true,
        round_calls: limits.calls,
        round_window_s: limits.window_minutes.map(|m| m * 60),
        round_min_calls: limits.min_calls,
        command_grace_s: limits.grace_minutes.map(|m| m * 60),
        final_message: limits.final_message,
        new_calls: limits.new_calls,
        outside_checkout_note: limits.outside_note,
    });
    request
}

/// `request` with the user's `run.review_round`: `off` replaces the `review` member with the one that switches the round off (the user's
/// review file is then not sent, since nothing would read it); `auto` and no setting leave the member as the file made it, which is no member for a
/// user with no review file.
pub fn with_review_setting(
    request: StartDirectRunRequest,
    review_round: Option<ReviewMode>,
) -> StartDirectRunRequest {
    let mut request = request;
    if review_round == Some(ReviewMode::Off) {
        request.review = Some(ReviewOptions::off());
    }
    request
}

/// `request` with the machine kind of the run: `microvm` and `instances` become the `compute` member; `auto` and no setting leave it out, so
/// the request is byte for byte the one an older client sends and the stage's own policy decides.
pub fn with_compute_setting(
    request: StartDirectRunRequest,
    compute: Option<ComputeMode>,
) -> StartDirectRunRequest {
    with_compute_choice(request, compute, None)
}

/// `request` with the machine kind and the user's container signals: the `compute` member is sent for `microvm` and `instances`, and also
/// for `auto` when a list is set, because the stage must see the list. `auto` with no list leaves the member out.
pub fn with_compute_choice(
    request: StartDirectRunRequest,
    compute: Option<ComputeMode>,
    container_signals: Option<&[String]>,
) -> StartDirectRunRequest {
    let mut request = request;
    request.compute = ComputeOptions::new(
        compute.unwrap_or(ComputeMode::Auto),
        container_signals.map(<[String]>::to_vec),
    );
    request
}

/// `request` with the engine of the run: the `engine` member of the protocol names it when the run is on an engine other than the default,
/// and the member is left out for the default engine, so a run on it is byte for byte the request an older client sends (and a stage that predates
/// the member keeps taking it). The member is part of the request JSON, so it is part of the idempotency key: one prompt sent to two engines is two
/// starts. The ids of the table are engine names, and a test pins that every one is; an id that was not would panic here, never leave the member out and
/// start the run on the default engine.
pub fn with_engine_choice(
    request: StartDirectRunRequest,
    engine: &Engine,
) -> StartDirectRunRequest {
    let mut request = request;
    request.engine = if engine.is_default() {
        None
    } else {
        Some(WireEngine::named(engine.id))
    };
    request
}

/// `request` with the total time of the run: seconds in the `maxRunS` member, left out when neither the flag nor the file sets it, so the
/// request is byte for byte the one an older client sends and the stage's own limit applies.
pub fn with_max_run_setting(
    request: StartDirectRunRequest,
    max_run_s: Option<u32>,
) -> StartDirectRunRequest {
    let mut request = request;
    request.max_run_s = max_run_s;
    request
}

/// Whole MiB as bytes.
fn mib(n: u32) -> u64 {
    u64::from(n) * 1024 * 1024
}

/// `request` with the user's counts and research budgets: what is set becomes a member of `limits`; nothing set sends no `limits` member, so the
/// request is byte for byte the one an older client sends and the stage's own values apply.
pub fn with_limits_setting(
    request: StartDirectRunRequest,
    limits: &RunLimits,
) -> StartDirectRunRequest {
    let mut request = request;
    request.limits = (!limits.is_empty()).then_some(*limits);
    request
}

/// `request` with the user's provider data sharing: `true` becomes the `providerSharing` member; off leaves it out, so the request is byte for byte the
/// one an older client sends.
pub fn with_provider_sharing_setting(
    request: StartDirectRunRequest,
    on: bool,
) -> StartDirectRunRequest {
    let mut request = request;
    request.provider_sharing = on.then_some(true);
    request
}

/// `request` with the user's build switches: a switch set to `false` becomes a member of `build`; `true` and unset are the default and
/// are left out, so a request that keeps the defaults has no `build` member and is byte for byte the one an older client sends.
pub fn with_build_settings(
    request: StartDirectRunRequest,
    build: &BuildOptions,
) -> StartDirectRunRequest {
    let mut request = request;
    request.build = (!build.is_empty()).then_some(*build);
    request
}

/// SHA-256 hex over the request JSON and a per-launch nonce: a retry of one launch reuses it, a new launch or an edited rule does
/// not.
pub fn direct_idempotency_key(req: &StartDirectRunRequest, nonce: &str) -> String {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(req).unwrap_or_default());
    h.update([0u8]);
    h.update(nonce.as_bytes());
    hex(&h.finalize())
}

/// [`direct_idempotency_key`] for a start on one of several backends: the alias is part of the input, so the same request and nonce sent
/// to two backends are two starts, and a retry to the same backend sends the same key. `None` (a client with one backend) gives the key of
/// before.
pub fn direct_idempotency_key_on(
    req: &StartDirectRunRequest,
    nonce: &str,
    backend: Option<&str>,
) -> String {
    let Some(alias) = backend else {
        return direct_idempotency_key(req, nonce);
    };
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(req).unwrap_or_default());
    h.update([0u8]);
    h.update(nonce.as_bytes());
    h.update([0u8]);
    h.update(b"backend:");
    h.update(alias.as_bytes());
    hex(&h.finalize())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 15)]));
    }
    s
}

/// `request` with the user's `report_tests` setting: the clean-checkout test report is on by default, so only `false` is sent
/// (`reportTests: false`); on, or no setting, leaves the member out and the request as it was.
pub fn with_report_setting(
    request: harness_protocol::api::v1::StartDirectRunRequest,
    report_tests: Option<bool>,
) -> harness_protocol::api::v1::StartDirectRunRequest {
    let mut request = request;
    request.report_tests = (report_tests == Some(false)).then_some(false);
    request
}
