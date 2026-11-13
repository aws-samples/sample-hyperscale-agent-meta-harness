//! The `ham` command surface.
//!
//! `ham "<prompt>" --repo cc:<name> --direct` (or `--new-repo cc:<name>`, or `--s3-only`) starts a run; every other form is a
//! subcommand. `--json` is accepted anywhere. [`Invocation::to_argv`] renders the canonical argv, which [`parse`] reads
//! back unchanged. Parse errors start with `usage:` and map to exit code 2.
//!
//! Where the documents leave a spelling open, this parser fixes it:
//! - `--prompt <text>` is an alternative to the positional prompt, for prompts that start with
//!   `-` or equal a subcommand name.
//! - `clone <runId> [dir]` downloads the run's zip like `pull` and turns its `repo.bundle` into a local
//!   git repository in `dir` (default `./<repo name>`); the work is `ham::clone`.
//! - `push <runId> <git-url> [--branch <name>]` builds that repository in a temporary directory and pushes the run's
//!   branch to the URL with the user's own git. A URL that carries a credential is a usage error that never repeats it
//!   (`crate::push`).

use crate::push::{check_branch, check_git_url, mask_userinfo};
use harness_protocol::direct::ComputeMode;
use harness_protocol::target::RepoTarget;
use std::collections::BTreeMap;
use std::fmt;

/// Subcommand names; a first argument equal to one of these is never a prompt.
pub const SUBCOMMANDS: &[&str] = &[
    "runs",
    "watch",
    "cancel",
    "models",
    "pr",
    "pull",
    "clone",
    "push",
    "link",
    "doctor",
    "rules",
    "config",
    "engine",
    "say",
    "new-session",
    "try-model",
    "finish",
    "continue",
    "resume",
    "lifecycle",
    "update",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Invocation {
    pub json: bool,
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// `ham "<prompt>" --repo ... --direct --model <id>`: the prompt, one model and the user's rules go to Cline.
    StartDirect(DirectArgs),
    /// `ham rules init|edit|show`: the user's rules files.
    Rules(RulesCommand),
    /// `ham lifecycle init <dir>`: the lifecycle pack (`AGENTS.md` and its skills) copied into a repository.
    Lifecycle(LifecycleCommand),
    /// `ham continue <runId> [<text>] [--repo cc:<name> | --s3-only]`: a new run that starts from the code a finished run
    /// delivered. The `DirectArgs` carry `continue_from`; an empty `prompt` means the client opens the editor for the text. The second member is
    /// the target the person named: `Some` wins; `None` means the new run goes where the source run went (the client reads the source run's
    /// target before it starts anything), and `DirectArgs.target` is then only the zip-alone placeholder the client overwrites.
    Continue(DirectArgs, Option<TargetArg>),
    /// `ham resume <runId>` or `ham resume --all [--dry-run]`: runs that stopped delivered (`paused`, `stalled`) go on as new runs from the
    /// code they delivered, with the request they were started with. `--all` takes every such run of the list that was not resumed already;
    /// `--dry-run` prints the plan and starts nothing.
    Resume {
        run_id: Option<String>,
        all: bool,
        dry_run: bool,
    },
    /// `ham config [get [<key>]|set <key> <value>|unset <key>|show|check [--strict]|edit|docs]`: the one config file, including
    /// `default_model`, and the verbs that read and repair it.
    Config(ConfigCommand),
    /// `ham engine <name> init|show|check|edit|set <key> <value>|unset <key>|import <file> [--replace]`: the settings file of one engine,
    /// `~/.config/ham/engines/<name>.toml` (the configuration program). The engine name is an argument; the parser names no engine.
    Engine(EngineCommand),
    /// `ham runs [--probes]`: `probes` also lists the runs a probe wrote
    /// (`kind: probe`), which are left out by default.
    Runs {
        probes: bool,
    },
    Watch {
        run_id: Option<String>,
    },
    Cancel {
        run_id: String,
    },
    /// `ham models`: the supported models (`engine/catalog/models.json`) with the default marked, offline.
    Models,
    /// `ham models --server`: the list the direct stage serves (`GET /v1/direct/models`), with its default marked.
    ModelsServer,
    Pr {
        run_id: String,
    },
    Pull {
        run_id: String,
        artifact: PullArtifact,
    },
    /// `ham clone <runId> [dir]`: the delivered zip's `repo.bundle` as a local git repository in `dir`.
    Clone {
        run_id: String,
        dir: Option<String>,
    },
    /// `ham push <runId> <git-url> [--branch <name>]`: the run's branch, built from the delivered zip, pushed to `url` with the user's
    /// own git. `branch` is the remote branch name; `None` means the run's own branch (`harness/<runId>`).
    Push {
        run_id: String,
        url: String,
        branch: Option<String>,
    },
    Link {
        run_id: String,
    },
    /// `ham doctor [--catalog-max-age <hours>]`: the setup checks. With the option, a newest model smoke record older than that many whole
    /// hours (1 or more) is a warning; without it the age is only stated.
    /// The registry's usage text for `doctor` stays empty on purpose, so that `/doctor` keeps running at once
    /// from the popup instead of waiting for arguments.
    Doctor {
        catalog_max_age_hours: Option<u64>,
    },
    /// `ham update [--check] [--yes] [--no-wait] [--wait <minutes>]`: the newest version on the main branch of the update source
    /// (`update_source`), the changes between, and the update of the backend and then the client through that commit's `setup.sh --update`,
    /// after the caller's active runs have ended. `check` only shows; `wait_minutes` is the limit of the wait (1 to 1440; `None` is 60).
    Update {
        check: bool,
        yes: bool,
        no_wait: bool,
        wait_minutes: Option<u32>,
    },
    /// `ham say <runId> '<text>'`: answer the run's open "Cline needs you" question with a message into the same Cline
    /// session. `text` is `-` when the caller is to read standard input.
    Say {
        run_id: String,
        text: String,
        request_id: Option<String>,
        no_wait: bool,
    },
    /// `ham new-session <runId> ['<text>']`: answer with a fresh Cline session on the same files, the user's text optional.
    NewSession {
        run_id: String,
        text: Option<String>,
        request_id: Option<String>,
        no_wait: bool,
    },
    /// `ham try-model <runId> <model> ['<text>']`: answer by switching to another model the question offers (an id or an alias).
    TryModel {
        run_id: String,
        model: String,
        text: Option<String>,
        request_id: Option<String>,
        no_wait: bool,
    },
    /// `ham finish <runId>`: answer by finishing the run with what exists.
    Finish {
        run_id: String,
        request_id: Option<String>,
        no_wait: bool,
    },
}

/// Target flag of a new run: exactly one of `--repo`, `--new-repo`, `--s3-only`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetArg {
    Repo(RepoTarget),
    NewRepo(RepoTarget),
    S3Only,
}

/// Options of a direct run. A path option holds a path; the caller reads the files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectArgs {
    pub prompt: String,
    pub target: TargetArg,
    pub base: Option<String>,
    /// `--model <id|alias>` as typed; `None` means the user's default (`HAMH_MODEL`, then `config.toml`'s `default_model`, then
    /// Claude Sonnet 5.5). control-api refuses any id that is not a selectable `models.json` row.
    pub model: Option<String>,
    /// `--rules <path>`: a directory of rule files, or one `.md` file, instead of `~/.config/ham/rules/`.
    pub rules: Option<String>,
    /// `--no-rules`: start with no rules (without it, a missing or empty rules directory stops the run).
    pub no_rules: bool,
    /// `--continue <file>`: a nudge file instead of `~/.config/ham/continue.md`.
    pub continue_file: Option<String>,
    /// `--input-wait <minutes>` (0 to 43200): how long the run waits for the user's answer when Cline needs input. `None` means the
    /// weaker layers decide (`HAMH_INPUT_WAIT`, `input_wait_minutes` in `config.toml`, then 30 minutes in the TUI and none elsewhere);
    /// `Some(0)` asks for no wait.
    pub input_wait: Option<u32>,
    /// `--skills <dir>`: a directory of skill folders instead of `~/.config/ham/skills/`.
    pub skills: Option<String>,
    /// `--no-skills`: send no skills (`--no-rules` also sends none).
    pub no_skills: bool,
    /// `ham continue <runId>`: the finished run whose delivered code the run starts from. Never set by a new run's options.
    pub continue_from: Option<String>,
    /// `--goal on|off`: the goal loop of the engine's goal plugin. `None` leaves the stage default, which is on;
    /// `Some(false)` sends `goal: {"enabled": false}` and agentd then places nothing for the plugin and starts no loop.
    pub goal: Option<bool>,
    /// `--research` (`Some(true)`) or `--no-research` (`Some(false)`): the research route. `None` leaves the stage default, which
    /// is on where the stage has the route and off where it has none.
    pub research: Option<bool>,
    /// `--allow-third-party`: a delivered copy of a stored third-party file passes with a finding instead of ending the run
    /// without a zip.
    pub allow_third_party: bool,
    /// `--attach <file>`, repeatable: files of the user's own that Cline reads as context. The caller reads them
    /// (`crate::attachments::read_attachments`).
    pub attach: Vec<String>,
    /// `--compute auto|microvm|instances`: the kind of machine the session runs on, for this run. `None` leaves `run.compute` of the file,
    /// then the stage's own policy.
    pub compute: Option<ComputeMode>,
    /// `--max-run-hours <hours>`: the most hours this run may take in all, across the sessions it renews. `None` leaves `run.max_run_hours`
    /// of the file, then the stage's own 24 hours.
    pub max_run_hours: Option<u32>,
    /// `--engine <name>`: the engine of this run, an id of `crate::engines` in lower case. `None` leaves `default_engine` of the file, then
    /// the default engine.
    pub engine: Option<String>,
    /// `--provider-sharing on|off`: agree, for this run, that the prompts and answers of a run on a model that needs it go to the model's
    /// provider. `None` leaves `run.provider_sharing` of the file, then off.
    pub provider_sharing: Option<bool>,
}

/// `ham rules` subcommands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RulesCommand {
    /// Write the default rules file and `continue.md` where none exist, and install the lifecycle pack: its contract as the rule
    /// `10-lifecycle.md` and its seven skills in the skills directory. `ham rules init` and `ham rules init --pack lifecycle` parse to this and to
    /// `InitPack { Lifecycle, false, false }`, which install the same files.
    Init,
    /// `ham rules init --no-skills` (also `--pack lifecycle --no-skills`): the contract and the default rules without the skills, the
    /// install without the skills.
    InitNoSkills,
    /// `ham rules init --pack lifecycle [--skills-as-rules] [--skills]`: install a pack. The contract becomes the rule
    /// `10-lifecycle.md`; `skills_as_rules` also renders each skill body as a rule file (for measurement, and then no skills folder unless
    /// `skills` is also given); `skills` also installs the pack's skills in the skills directory, which later runs send (a bare
    /// `--pack lifecycle` installs them too).
    InitPack {
        pack: Pack,
        skills_as_rules: bool,
        skills: bool,
    },
    /// Open a rules file (`None`: `00-my-rules.md`; `continue`: the nudge file) in `$VISUAL`, `$EDITOR`, else `vi`.
    Edit { which: Option<String> },
    /// Print what the next run would send.
    Show {
        rules: Option<String>,
        continue_file: Option<String>,
    },
}

/// The packs `ham rules init --pack` installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pack {
    /// The lifecycle pack v1 (`lifecycle/pack/` in this repository): the development flow as a contract and seven skills.
    Lifecycle,
    /// No pack: `ham rules init --pack none` writes the pointer-only default rules and the nudge file, as it did before the pack existed.
    None,
}

impl Pack {
    pub fn name(self) -> &'static str {
        match self {
            Pack::Lifecycle => "lifecycle",
            Pack::None => "none",
        }
    }
}

/// `ham lifecycle` subcommands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleCommand {
    /// Copy the pack into `dir`: `AGENTS.md` and `.agents/skills/`, and a `.kiro/skills` link with `kiro`.
    Init { dir: String, kiro: bool },
}

/// `ham config` subcommands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigCommand {
    /// Every key and its value.
    List,
    Get {
        key: String,
    },
    Set {
        key: String,
        value: String,
    },
    Unset {
        key: String,
    },
    /// Every key with the value in force, where it came from and when a change takes effect; then the settings the harness locks.
    Show,
    /// The checks of the file, nothing sent. `strict` makes a warning an error.
    Check {
        strict: bool,
    },
    /// Opens the user's editor on the file and checks what it left.
    Edit,
    /// The reference table of the keys.
    Docs,
    /// `ham config migrate [--dry-run]`: copies the files of the folder of the earlier command name into `~/.config/ham`; with `--dry-run` it
    /// only says what it would copy.
    Migrate {
        dry_run: bool,
    },
}

/// `ham engine <name> ...`: the settings file of one engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineCommand {
    /// The engine's id as the roadmap spells it. Only a plain word: it is a file name in `engines/`, never a path.
    pub engine: String,
    pub action: EngineAction,
}

/// The forms of `ham engine <name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineAction {
    /// `ham engine <name>` with no action: says whether the engine can be chosen and how, and what its settings file offers. In the
    /// TUI, `/engine <name>` chooses the engine for the runs the session starts.
    Choose,
    /// Writes a template: `schema = 1` and every key commented out with what it takes.
    Init,
    /// The path, the size, the keys the next run would send, the SHA-256 of the file and the settings hash the card will show.
    Show,
    /// The checks of the file, nothing sent. Exit 2 when the file is refused.
    Check,
    /// Opens the user's editor on the file and checks what it left.
    Edit,
    Set {
        key: String,
        value: String,
    },
    Unset {
        key: String,
    },
    /// Validates `file` and copies it to the engine's file; refuses to replace an existing one without `replace`.
    Import {
        file: String,
        replace: bool,
    },
}

/// `ham pull --artifact` values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullArtifact {
    Zip,
    Partial,
    Evidence,
    /// `--snapshot`: the last snapshot bundle of a live run, saved by agentd about every 10 minutes while the files change. It is
    /// not an artifact of the run, so it is never chosen with `--artifact`.
    Snapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "usage: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError(msg.into()))
}

/// Tokens after the command name, with `--json` removed where it stands as a flag.
struct Args {
    tokens: Vec<String>,
    pos: usize,
}

impl Args {
    fn next(&mut self) -> Option<String> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += usize::from(t.is_some());
        t
    }

    fn value(&mut self, flag: &str) -> Result<String, ParseError> {
        self.next()
            .map_or_else(|| err(format!("{flag} needs a value")), Ok)
    }
}

fn parse_target(flag: &str, v: &str) -> Result<RepoTarget, ParseError> {
    v.parse().map_err(|e| ParseError(format!("{flag}: {e}")))
}

/// The longest `--input-wait` and `input_wait_minutes`: the longest wait the API accepts (`MAX_INPUT_WAIT_S`, the longest run), in minutes. The
/// agent cuts every window to the time its session has left.
pub const MAX_INPUT_WAIT_MINUTES: u32 = harness_protocol::direct::MAX_INPUT_WAIT_S / 60;

/// `--input-wait <minutes>`: a whole number from 0 (do not wait) to [`MAX_INPUT_WAIT_MINUTES`].
fn parse_input_wait(v: &str) -> Result<u32, ParseError> {
    match v.parse::<u32>() {
        Ok(m) if m <= MAX_INPUT_WAIT_MINUTES => Ok(m),
        _ => err(format!(
            "--input-wait takes whole minutes from 0 to {MAX_INPUT_WAIT_MINUTES}, got {v:?}"
        )),
    }
}

/// `--max-run-hours <hours>`: a whole number of hours from 1 to the protocol's longest total time (720).
fn parse_max_run_hours(v: &str) -> Result<u32, ParseError> {
    let max = harness_protocol::direct::MAX_MAX_RUN_S / 3600;
    match v.parse::<u32>() {
        Ok(h) if (1..=max).contains(&h) => Ok(h),
        _ => err(format!(
            "--max-run-hours takes whole hours from 1 to {max}, got {v:?}"
        )),
    }
}

/// `--engine <name>`: the id of an engine a person can choose, read in any case and stored in lower case. The table of
/// `crate::engines` decides, so a new engine needs no change here.
fn parse_engine(v: &str) -> Result<String, ParseError> {
    match crate::engines::choose(v) {
        Ok(e) => Ok(e.id.to_string()),
        Err(e) => err(format!("--engine: {e}")),
    }
}

/// `--catalog-max-age <hours>`: a whole number of hours, 1 or more. There is no upper bound; the client multiplies with saturation.
fn parse_catalog_max_age(v: &str) -> Result<u64, ParseError> {
    match v.parse::<u64>() {
        Ok(h) if h >= 1 && v.bytes().all(|b| b.is_ascii_digit()) => Ok(h),
        _ => err(format!(
            "--catalog-max-age takes whole hours, 1 or more, got {v:?}"
        )),
    }
}

/// Whether a start is a new run or a follow-up (`ham continue`): the options are the same, the target and the base are not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartMode {
    New,
    Continue,
}

fn parse_start(first: Option<String>, a: Args) -> Result<Command, ParseError> {
    parse_start_mode(first, a, StartMode::New, None)
}

/// `ham continue <runId> [<text>] [options]`: the run id first, then the options of a direct run except the base, `--new-repo` and
/// `--direct` (a follow-up is always a direct run). It takes `--repo cc:<name>` or `--s3-only` as the target, which wins; without either the
/// new run goes where the source run went. No text means the client opens the editor for the text.
fn parse_continue(mut a: Args) -> Result<Command, ParseError> {
    let Some(run_id) = a.next() else {
        return err(
            "ham continue needs <runId> (the id ham runs prints); then the text for the coding agent, or nothing to write it in your editor",
        );
    };
    if run_id.is_empty()
        || run_id.len() > 64
        || !run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return err(
            "ham continue takes the run id first (the id ham runs prints), then the text for the coding agent",
        );
    }
    parse_start_mode(None, a, StartMode::Continue, Some(run_id))
}

fn parse_start_mode(
    first: Option<String>,
    mut a: Args,
    mode: StartMode,
    continue_from: Option<String>,
) -> Result<Command, ParseError> {
    let mut prompt: Option<String> = None;
    let mut targets: Vec<TargetArg> = Vec::new();
    let mut direct = mode == StartMode::Continue;
    let mut base: Option<String> = None;
    let mut model: Option<String> = None;
    let mut rules: Option<String> = None;
    let mut no_rules = false;
    let mut skills: Option<String> = None;
    let mut no_skills = false;
    let mut continue_file: Option<String> = None;
    let mut input_wait: Option<u32> = None;
    let mut goal: Option<bool> = None;
    let mut research_on = false;
    let mut research_off = false;
    let mut allow_third_party = false;
    let mut attach: Vec<String> = Vec::new();
    let mut compute: Option<ComputeMode> = None;
    let mut max_run_hours: Option<u32> = None;
    let mut engine: Option<String> = None;
    let mut provider_sharing: Option<bool> = None;
    let mut pending = first;
    while let Some(tok) = pending.take().or_else(|| a.next()) {
        if mode == StartMode::Continue && tok == "--new-repo" {
            return err(
                "ham continue cannot make a new repository: --new-repo is not an option of continue. A follow-up goes to the repository the run used (leave the target out, or name it with --repo cc:<name>) or to a zip alone (--s3-only)",
            );
        }
        if mode == StartMode::Continue && matches!(tok.as_str(), "--base" | "--direct") {
            return err(format!(
                "ham continue starts a run from the code the run delivered; {tok} is not an option of continue"
            ));
        }
        match tok.as_str() {
            "--prompt" => {
                if prompt.replace(a.value("--prompt")?).is_some() {
                    return err("the prompt is given twice");
                }
            }
            // The chat box writes a payload that starts with a dash as `--text` (as for `say`); only a follow-up reads it.
            "--text" if mode == StartMode::Continue => {
                if prompt.replace(a.value("--text")?).is_some() {
                    return err("the prompt is given twice");
                }
            }
            "--repo" => targets.push(TargetArg::Repo(parse_target(
                "--repo",
                &a.value("--repo")?,
            )?)),
            "--new-repo" => targets.push(TargetArg::NewRepo(parse_target(
                "--new-repo",
                &a.value("--new-repo")?,
            )?)),
            "--s3-only" => targets.push(TargetArg::S3Only),
            "--base" => base = Some(a.value("--base")?),
            "--direct" => direct = true,
            "--model" => {
                if model.replace(a.value("--model")?).is_some() {
                    return err("--model is given twice; a direct run uses one model");
                }
            }
            "--no-rules" => no_rules = true,
            "--no-skills" => no_skills = true,
            "--skills" => skills = Some(a.value("--skills")?),
            "--continue" => continue_file = Some(a.value("--continue")?),
            "--rules" => rules = Some(a.value("--rules")?),
            "--input-wait" => {
                let minutes = parse_input_wait(&a.value("--input-wait")?)?;
                if input_wait.replace(minutes).is_some() {
                    return err("--input-wait is given twice");
                }
            }
            "--goal" => {
                let on = match a.value("--goal")?.as_str() {
                    "on" => true,
                    "off" => false,
                    other => return err(format!("--goal takes on or off, got {other:?}")),
                };
                if goal.replace(on).is_some() {
                    return err("--goal is given twice");
                }
            }
            "--research" => research_on = true,
            "--no-research" => research_off = true,
            "--allow-third-party" => allow_third_party = true,
            "--attach" => attach.push(a.value("--attach")?),
            "--compute" => {
                let word = a.value("--compute")?;
                let Some(mode) = ComputeMode::parse(&word) else {
                    return err(format!(
                        "--compute takes auto, microvm or instances, got {word:?}"
                    ));
                };
                if compute.replace(mode).is_some() {
                    return err("--compute is given twice");
                }
            }
            "--max-run-hours" => {
                let hours = parse_max_run_hours(&a.value("--max-run-hours")?)?;
                if max_run_hours.replace(hours).is_some() {
                    return err("--max-run-hours is given twice");
                }
            }
            "--engine" => {
                let id = parse_engine(&a.value("--engine")?)?;
                if engine.replace(id).is_some() {
                    return err("--engine is given twice");
                }
            }
            "--provider-sharing" => {
                let on = match a.value("--provider-sharing")?.as_str() {
                    "on" => true,
                    "off" => false,
                    other => {
                        return err(format!("--provider-sharing takes on or off, got {other:?}"));
                    }
                };
                if provider_sharing.replace(on).is_some() {
                    return err("--provider-sharing is given twice");
                }
            }
            t if t.starts_with('-') => return err(format!("unknown option {t:?} for a new run")),
            _ => {
                if prompt.replace(tok).is_some() {
                    return err(if mode == StartMode::Continue {
                        "ham continue takes one text for the coding agent; quote it"
                    } else {
                        "a new run takes one prompt; quote it"
                    });
                }
            }
        }
    }
    let prompt = match (mode, prompt) {
        (_, Some(p)) if !p.is_empty() => p,
        (StartMode::Continue, _) => String::new(),
        _ => return err("a prompt is required: ham \"<prompt>\" --repo cc:<name> --direct"),
    };
    let mut explicit_target: Option<TargetArg> = None;
    let target = if mode == StartMode::Continue {
        if targets.len() > 1 {
            return err(
                "ham continue takes one target: give --repo cc:<name> or --s3-only, not both and not twice",
            );
        }
        explicit_target = targets.pop();
        explicit_target.clone().unwrap_or(TargetArg::S3Only)
    } else {
        if targets.len() != 1 {
            return err("exactly one of --repo, --new-repo, --s3-only is required");
        }
        targets.remove(0)
    };
    let attached = !attach.is_empty();
    if !direct {
        for (given, flag) in [
            (goal.is_some(), "--goal"),
            (research_on, "--research"),
            (research_off, "--no-research"),
            (allow_third_party, "--allow-third-party"),
            (attached, "--attach"),
            (model.is_some(), "--model"),
            (no_rules, "--no-rules"),
            (skills.is_some(), "--skills"),
            (no_skills, "--no-skills"),
            (continue_file.is_some(), "--continue"),
            (input_wait.is_some(), "--input-wait"),
            (compute.is_some(), "--compute"),
            (max_run_hours.is_some(), "--max-run-hours"),
            (engine.is_some(), "--engine"),
            (provider_sharing.is_some(), "--provider-sharing"),
        ] {
            if given {
                return err(format!("{flag} is an option of a direct run; add --direct"));
            }
        }
        return err("a new run needs --direct: ham \"<prompt>\" --repo cc:<name> --direct");
    }
    if no_rules && rules.is_some() {
        return err("--no-rules and --rules say opposite things; give one");
    }
    if no_skills && skills.is_some() {
        return err("--no-skills and --skills say opposite things; give one");
    }
    if research_on && research_off {
        return err("--research and --no-research say opposite things; give one");
    }
    let args = DirectArgs {
        prompt,
        target,
        base,
        model,
        rules,
        no_rules,
        continue_file,
        input_wait,
        skills,
        no_skills,
        continue_from,
        goal,
        research: match (research_on, research_off) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        },
        allow_third_party,
        attach,
        compute,
        max_run_hours,
        engine,
        provider_sharing,
    };
    Ok(if mode == StartMode::Continue {
        Command::Continue(args, explicit_target)
    } else {
        Command::StartDirect(args)
    })
}

/// Parses the positional and option tokens of a subcommand against its flag table.
struct Sub {
    positional: Vec<String>,
    flags: BTreeMap<&'static str, Option<String>>,
}

fn scan(
    name: &str,
    mut a: Args,
    valued: &[&'static str],
    switches: &[&'static str],
    max_pos: usize,
) -> Result<Sub, ParseError> {
    let mut sub = Sub {
        positional: Vec::new(),
        flags: BTreeMap::new(),
    };
    while let Some(tok) = a.next() {
        if let Some(f) = valued.iter().find(|f| **f == tok) {
            let v = a.value(f)?;
            sub.flags.insert(f, Some(v));
        } else if let Some(f) = switches.iter().find(|f| **f == tok) {
            sub.flags.insert(f, None);
        } else if tok.starts_with('-') {
            return err(format!("unknown option {tok:?} for ham {name}"));
        } else if sub.positional.len() < max_pos {
            sub.positional.push(tok);
        } else {
            return err(format!("unexpected argument {tok:?} for ham {name}"));
        }
    }
    Ok(sub)
}

impl Sub {
    fn pos(&self, i: usize, name: &str, what: &str) -> Result<String, ParseError> {
        self.positional
            .get(i)
            .cloned()
            .map_or_else(|| err(format!("ham {name} needs <{what}>")), Ok)
    }
    fn opt(&self, f: &str) -> Option<String> {
        self.flags.get(f).cloned().flatten()
    }
    fn has(&self, f: &str) -> bool {
        self.flags.contains_key(f)
    }
}

/// The forms of `ham rules`, joined by ` | `: the text of the parser's errors and of `ham rules --help` (`crate::help`).
pub const RULES_FORMS: &str = "ham rules init [--pack lifecycle|none] [--skills-as-rules] [--skills|--no-skills] | ham rules edit [<file.md>|continue|review] | ham rules show [--rules <path>] [--continue <file>]";
/// The forms of `ham lifecycle`: the text of the parser's errors and of `ham lifecycle --help`.
pub const LIFECYCLE_FORMS: &str = "ham lifecycle init <dir> [--kiro]";
/// The forms of `ham config`: the text of the parser's errors and of `ham config --help`.
pub const CONFIG_FORMS: &str = "ham config [get [<key>]] | ham config set <key> <value> | ham config unset <key> | ham config show | ham config check [--strict] | ham config edit | ham config docs | ham config migrate [--dry-run]";
/// The forms of `ham engine`: the text of the parser's errors and of `ham engine --help`.
pub const ENGINE_FORMS: &str = "ham engine <name> | ham engine <name> init | ham engine <name> show | ham engine <name> check | ham engine <name> edit | ham engine <name> set <key> <value> | ham engine <name> unset <key> | ham engine <name> import <file> [--replace]";

fn parse_rules(mut a: Args) -> Result<Command, ParseError> {
    const FORMS: &str = RULES_FORMS;
    let Some(sub) = a.next() else {
        return err(FORMS);
    };
    Ok(Command::Rules(match sub.as_str() {
        "init" => {
            let s = scan(
                "rules init",
                a,
                &["--pack"],
                &["--skills-as-rules", "--skills", "--no-skills"],
                0,
            )?;
            let (skills_as_rules, skills, no_skills) = (
                s.has("--skills-as-rules"),
                s.has("--skills"),
                s.has("--no-skills"),
            );
            if skills && no_skills {
                return err("--skills and --no-skills say opposite things; give one");
            }
            // The lifecycle pack is the default: its options need no `--pack`, and `--pack none` is the pointer-only install of before.
            // `--no-skills` is the contract alone; beside `--skills-as-rules` it only repeats that the skill bodies go in as rules and no
            // skills folder is written, and beside `--pack none` it repeats that nothing of the pack is installed.
            match s.opt("--pack").as_deref() {
                None | Some("lifecycle") if no_skills && !skills_as_rules => {
                    RulesCommand::InitNoSkills
                }
                None if !skills_as_rules && !skills => RulesCommand::Init,
                None | Some("lifecycle") => RulesCommand::InitPack {
                    pack: Pack::Lifecycle,
                    skills_as_rules,
                    skills,
                },
                Some("none") => {
                    for (given, flag) in
                        [(skills_as_rules, "--skills-as-rules"), (skills, "--skills")]
                    {
                        if given {
                            return err(format!(
                                "{flag} installs part of the lifecycle pack and does not go with --pack none"
                            ));
                        }
                    }
                    RulesCommand::InitPack {
                        pack: Pack::None,
                        skills_as_rules: false,
                        skills: false,
                    }
                }
                Some(other) => {
                    return err(format!(
                        "unknown pack {other:?}; the packs are lifecycle and none: ham rules init [--pack lifecycle|none]"
                    ));
                }
            }
        }
        "edit" => RulesCommand::Edit {
            which: scan("rules edit", a, &[], &[], 1)?
                .positional
                .first()
                .cloned(),
        },
        "show" => {
            let s = scan("rules show", a, &["--rules", "--continue"], &[], 0)?;
            RulesCommand::Show {
                rules: s.opt("--rules"),
                continue_file: s.opt("--continue"),
            }
        }
        other => return err(format!("unknown rules command {other:?}; {FORMS}")),
    }))
}

fn parse_lifecycle(mut a: Args) -> Result<Command, ParseError> {
    const FORMS: &str = LIFECYCLE_FORMS;
    let Some(sub) = a.next() else {
        return err(FORMS);
    };
    match sub.as_str() {
        "init" => {
            let s = scan("lifecycle init", a, &[], &["--kiro"], 1)?;
            let dir = s.pos(0, "lifecycle init", "dir")?;
            if dir.is_empty() {
                return err("ham lifecycle init <dir> must name a directory, got an empty string");
            }
            Ok(Command::Lifecycle(LifecycleCommand::Init {
                dir,
                kiro: s.has("--kiro"),
            }))
        }
        other => err(format!("unknown lifecycle command {other:?}; {FORMS}")),
    }
}

fn parse_config_cmd(mut a: Args) -> Result<Command, ParseError> {
    const FORMS: &str = CONFIG_FORMS;
    let Some(sub) = a.next() else {
        return Ok(Command::Config(ConfigCommand::List));
    };
    Ok(Command::Config(match sub.as_str() {
        "get" => {
            let s = scan("config get", a, &[], &[], 1)?;
            match s.positional.first() {
                Some(key) => ConfigCommand::Get { key: key.clone() },
                None => ConfigCommand::List,
            }
        }
        "set" => {
            let s = scan("config set", a, &[], &[], 2)?;
            ConfigCommand::Set {
                key: s.pos(0, "config set", "key")?,
                value: s.pos(1, "config set", "value")?,
            }
        }
        "unset" => {
            let s = scan("config unset", a, &[], &[], 1)?;
            ConfigCommand::Unset {
                key: s.pos(0, "config unset", "key")?,
            }
        }
        "show" => {
            scan("config show", a, &[], &[], 0)?;
            ConfigCommand::Show
        }
        "check" => {
            let s = scan("config check", a, &[], &["--strict"], 0)?;
            ConfigCommand::Check {
                strict: s.has("--strict"),
            }
        }
        "edit" => {
            scan("config edit", a, &[], &[], 0)?;
            ConfigCommand::Edit
        }
        "docs" => {
            scan("config docs", a, &[], &[], 0)?;
            ConfigCommand::Docs
        }
        "migrate" => {
            let s = scan("config migrate", a, &[], &["--dry-run"], 0)?;
            ConfigCommand::Migrate {
                dry_run: s.has("--dry-run"),
            }
        }
        other => return err(format!("unknown config command {other:?}; {FORMS}")),
    }))
}

/// An engine word of `ham engine`: lower-case letters, digits, `-` and `_`, starting with a letter or a digit, at most 32 characters. It becomes the
/// name of a file in `engines/`, so nothing that could be a path is a word.
fn is_engine_word(w: &str) -> bool {
    let mut chars = w.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && w.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

fn parse_engine_cmd(mut a: Args) -> Result<Command, ParseError> {
    const FORMS: &str = ENGINE_FORMS;
    let Some(engine) = a.next() else {
        return err(FORMS);
    };
    if !is_engine_word(&engine) {
        return err(format!(
            "ham engine takes the name of an engine first (lower-case letters, digits, - and _), got {engine:?}; {FORMS}"
        ));
    }
    let Some(sub) = a.next() else {
        return Ok(Command::Engine(EngineCommand {
            engine,
            action: EngineAction::Choose,
        }));
    };
    let action = match sub.as_str() {
        "init" => {
            scan("engine init", a, &[], &[], 0)?;
            EngineAction::Init
        }
        "show" => {
            scan("engine show", a, &[], &[], 0)?;
            EngineAction::Show
        }
        "check" => {
            scan("engine check", a, &[], &[], 0)?;
            EngineAction::Check
        }
        "edit" => {
            scan("engine edit", a, &[], &[], 0)?;
            EngineAction::Edit
        }
        "set" => {
            let s = scan("engine set", a, &[], &[], 2)?;
            EngineAction::Set {
                key: s.pos(0, "engine set", "key")?,
                value: s.pos(1, "engine set", "value")?,
            }
        }
        "unset" => {
            let s = scan("engine unset", a, &[], &[], 1)?;
            EngineAction::Unset {
                key: s.pos(0, "engine unset", "key")?,
            }
        }
        "import" => {
            let s = scan("engine import", a, &[], &["--replace"], 1)?;
            EngineAction::Import {
                file: s.pos(0, "engine import", "file")?,
                replace: s.has("--replace"),
            }
        }
        other => return err(format!("unknown engine command {other:?}; {FORMS}")),
    };
    Ok(Command::Engine(EngineCommand { engine, action }))
}

/// The options and words of `say`, `new-session`, `try-model` and `finish`: up to `max_pos` positional words (the lone `-` is one:
/// it means standard input), `--text <text>` for words that start with `-`, `--request <id>` and `--no-wait`.
struct Answer {
    positional: Vec<String>,
    text_option: Option<String>,
    request_id: Option<String>,
    no_wait: bool,
}

fn scan_answer(
    name: &str,
    mut a: Args,
    max_pos: usize,
    text_option: bool,
) -> Result<Answer, ParseError> {
    let mut out = Answer {
        positional: Vec::new(),
        text_option: None,
        request_id: None,
        no_wait: false,
    };
    while let Some(tok) = a.next() {
        match tok.as_str() {
            "--request" => {
                if out.request_id.replace(a.value("--request")?).is_some() {
                    return err(format!("--request is given twice for ham {name}"));
                }
            }
            "--no-wait" => out.no_wait = true,
            "--text" if text_option => {
                if out.text_option.replace(a.value("--text")?).is_some() {
                    return err(format!("--text is given twice for ham {name}"));
                }
            }
            t if t.starts_with('-') && t != "-" => {
                return err(format!("unknown option {t:?} for ham {name}"));
            }
            _ if out.positional.len() < max_pos => out.positional.push(tok),
            _ => return err(format!("unexpected argument {tok:?} for ham {name}")),
        }
    }
    Ok(out)
}

impl Answer {
    fn run_id(&self, name: &str) -> Result<String, ParseError> {
        self.positional
            .first()
            .cloned()
            .map_or_else(|| err(format!("ham {name} needs <runId>")), Ok)
    }

    /// The text: the positional word at `index` or `--text`, not both.
    fn text(&self, name: &str, index: usize) -> Result<Option<String>, ParseError> {
        match (self.positional.get(index), &self.text_option) {
            (Some(_), Some(_)) => err(format!("the text of ham {name} is given twice")),
            (Some(t), None) | (None, Some(t)) => Ok(Some(t.clone())),
            (None, None) => Ok(None),
        }
    }
}

fn parse_answer(name: &str, a: Args) -> Result<Command, ParseError> {
    let (max_pos, text_option) = match name {
        "finish" => (1, false),
        "new-session" => (2, true),
        "try-model" => (3, true),
        _ => (2, true),
    };
    let s = scan_answer(name, a, max_pos, text_option)?;
    let run_id = s.run_id(name)?;
    let (request_id, no_wait) = (s.request_id.clone(), s.no_wait);
    Ok(match name {
        "say" => {
            let text = s.text(name, 1)?.filter(|t| !t.trim().is_empty());
            let Some(text) = text else {
                return err(
                    "ham say needs <text> (a message for the coding agent; - reads standard input)",
                );
            };
            Command::Say {
                run_id,
                text,
                request_id,
                no_wait,
            }
        }
        "new-session" => Command::NewSession {
            run_id,
            text: s.text(name, 1)?.filter(|t| !t.trim().is_empty()),
            request_id,
            no_wait,
        },
        "try-model" => {
            let Some(model) = s.positional.get(1).cloned() else {
                return err("ham try-model needs <model> (an id or an alias such as opus)");
            };
            Command::TryModel {
                run_id,
                model,
                text: s.text(name, 2)?.filter(|t| !t.trim().is_empty()),
                request_id,
                no_wait,
            }
        }
        _ => Command::Finish {
            run_id,
            request_id,
            no_wait,
        },
    })
}

fn parse_sub(name: &str, a: Args) -> Result<Command, ParseError> {
    let one = |a: Args| -> Result<String, ParseError> {
        scan(name, a, &[], &[], 1)?.pos(0, name, "runId")
    };
    Ok(match name {
        "runs" => Command::Runs {
            probes: scan(name, a, &[], &["--probes"], 0)?.has("--probes"),
        },
        "watch" => Command::Watch {
            run_id: scan(name, a, &[], &[], 1)?.positional.first().cloned(),
        },
        "cancel" => Command::Cancel { run_id: one(a)? },
        "models" => {
            let s = scan(name, a, &[], &["--server"], 0)?;
            if s.has("--server") {
                Command::ModelsServer
            } else {
                Command::Models
            }
        }
        "pr" => Command::Pr { run_id: one(a)? },
        "pull" => {
            let s = scan(name, a, &["--artifact"], &["--snapshot"], 1)?;
            if s.has("--snapshot") && s.has("--artifact") {
                return err(
                    "ham pull takes --snapshot or --artifact, not both: the snapshot is the work so far, an artifact is a delivered file",
                );
            }
            let artifact = match s.opt("--artifact").as_deref() {
                None if s.has("--snapshot") => PullArtifact::Snapshot,
                None | Some("zip") => PullArtifact::Zip,
                Some("partial") => PullArtifact::Partial,
                Some("evidence") => PullArtifact::Evidence,
                Some(v) => {
                    return err(format!(
                        "--artifact takes zip, partial or evidence, got {v:?}"
                    ));
                }
            };
            Command::Pull {
                run_id: s.pos(0, name, "runId")?,
                artifact,
            }
        }
        "clone" => {
            let s = scan(name, a, &[], &[], 2)?;
            let dir = s.positional.get(1).cloned();
            if dir.as_deref() == Some("") {
                return err("ham clone <dir> must name a directory, got an empty string");
            }
            Command::Clone {
                run_id: s.pos(0, name, "runId")?,
                dir,
            }
        }
        "push" => {
            let s = scan(name, a, &["--branch"], &[], 2)
                .map_err(|e| ParseError(mask_userinfo(&e.0)))?;
            let run_id = s.pos(0, name, "runId")?;
            // The first word must be a run id: a URL typed there is never repeated in an error.
            if !run_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                || run_id.is_empty()
                || run_id.len() > 64
            {
                return err(
                    "ham push takes the run id first (the id ham runs prints), then the git URL",
                );
            }
            let url = s.pos(1, name, "git-url")?;
            check_git_url(&url).map_err(|p| ParseError(format!("ham push: {p}")))?;
            let branch = s.opt("--branch");
            if let Some(b) = &branch {
                check_branch(b).map_err(|e| ParseError(format!("--branch {e}")))?;
            }
            Command::Push {
                run_id,
                url,
                branch,
            }
        }
        "link" => Command::Link { run_id: one(a)? },
        "doctor" => {
            let s = scan(name, a, &["--catalog-max-age"], &[], 0)?;
            Command::Doctor {
                catalog_max_age_hours: s
                    .opt("--catalog-max-age")
                    .map(|v| parse_catalog_max_age(&v))
                    .transpose()?,
            }
        }
        "update" => {
            let s = scan(
                name,
                a,
                &["--wait"],
                &["--check", "--yes", "-y", "--no-wait"],
                0,
            )?;
            update_command(&s)?
        }
        "rules" => parse_rules(a)?,
        "lifecycle" => parse_lifecycle(a)?,
        "continue" => parse_continue(a)?,
        "resume" => {
            let s = scan(name, a, &[], &["--all", "--dry-run"], 1)?;
            let (run_id, all) = (s.positional.first().cloned(), s.has("--all"));
            if run_id.is_some() == all {
                return err(
                    "ham resume takes one run id, or --all for every paused and stalled run",
                );
            }
            Command::Resume {
                run_id,
                all,
                dry_run: s.has("--dry-run"),
            }
        }
        "config" => parse_config_cmd(a)?,
        "engine" => parse_engine_cmd(a)?,
        "say" | "new-session" | "try-model" | "finish" => parse_answer(name, a)?,
        other => return err(format!("unknown command {other:?}")),
    })
}

/// The most minutes `ham update --wait` takes: one day.
pub const UPDATE_WAIT_MAX_MINUTES: u32 = 1440;

/// `ham update`: `--check` stands alone; `--no-wait` and `--wait` exclude each other.
fn update_command(s: &Sub) -> Result<Command, ParseError> {
    let (check, yes, no_wait) = (
        s.has("--check"),
        s.has("--yes") || s.has("-y"),
        s.has("--no-wait"),
    );
    let wait_minutes = match s.opt("--wait") {
        None => None,
        Some(v) => match v.parse::<u32>() {
            Ok(n) if (1..=UPDATE_WAIT_MAX_MINUTES).contains(&n) => Some(n),
            _ => {
                return err(format!(
                    "ham update --wait takes whole minutes from 1 to {UPDATE_WAIT_MAX_MINUTES}, not {v:?}"
                ));
            }
        },
    };
    if no_wait && wait_minutes.is_some() {
        return err("ham update takes --no-wait or --wait <minutes>, not both");
    }
    if check && (yes || no_wait || wait_minutes.is_some()) {
        return err(
            "ham update --check only shows the versions and the changes; it takes no other option",
        );
    }
    Ok(Command::Update {
        check,
        yes,
        no_wait,
        wait_minutes,
    })
}

/// Options of a new run that take a value (the target flags and `--prompt` are told apart in [`mistyped_command`]).
const START_VALUE_OPTIONS: [&str; 12] = [
    "--base",
    "--model",
    "--rules",
    "--skills",
    "--continue",
    "--input-wait",
    "--goal",
    "--attach",
    "--compute",
    "--max-run-hours",
    "--engine",
    "--provider-sharing",
];

/// The message for a first word that is no subcommand but one edit from one, such as `ham pul` or `ham psuh`. Any other
/// first word is the prompt of a new run, and the old messages for a start that lacks its target still say what is missing. The
/// word is refused only when nothing else says "this is a start": no target flag, no `--prompt`, and no second word. A word of two
/// letters or fewer, or one with a character other than a letter, a digit, `-` or `_`, is a prompt. The text names both readings.
fn mistyped_command(first: &str, rest: &[String]) -> Option<String> {
    let plain = first.len() >= 3
        && first.starts_with(|c: char| c.is_ascii_alphabetic())
        && first
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if !plain {
        return None;
    }
    let mut tokens = rest.iter().map(String::as_str);
    while let Some(t) = tokens.next() {
        match t {
            "--repo" | "--new-repo" | "--s3-only" | "--prompt" => return None,
            _ if START_VALUE_OPTIONS.contains(&t) => {
                tokens.next();
            }
            _ if t.starts_with('-') => {}
            _ => return None,
        }
    }
    let typed = first.to_ascii_lowercase();
    let mut near: Vec<(usize, &str)> = crate::registry::subcommand_names()
        .into_iter()
        .map(|n| (crate::registry::edit_distance(&typed, n), n))
        .filter(|(d, _)| *d <= 1)
        .collect();
    near.sort_by_key(|(d, _)| *d);
    if near.is_empty() {
        return None;
    }
    let names: Vec<&str> = near.into_iter().map(|(_, n)| n).collect();
    Some(format!(
        "no command \"{first}\"; closest: {}; to start a run with \"{first}\" as the prompt, add --repo, --new-repo or --s3-only and --direct",
        names.join(", ")
    ))
}

/// The options that take the next word as their value, wherever they stand (a value is never read as a flag such as `--json` or `--help`).
pub const VALUE_OPTIONS: [&str; 20] = [
    "--prompt",
    "--repo",
    "--new-repo",
    "--base",
    "--rules",
    "--model",
    "--artifact",
    "--continue",
    "--request",
    "--text",
    "--input-wait",
    "--branch",
    "--pack",
    "--skills",
    "--goal",
    "--attach",
    "--compute",
    "--max-run-hours",
    "--engine",
    "--provider-sharing",
];

/// For each word of `args`: true when it stands as the value of the option before it. `--skills` is a switch of `ham rules init --pack ..`
/// and takes a directory in every other form; a `--json` that is not a value is a flag and takes no value.
pub fn value_positions<S: AsRef<str>>(args: &[S]) -> Vec<bool> {
    let in_rules = args.first().is_some_and(|a| a.as_ref() == "rules");
    let mut out = Vec::with_capacity(args.len());
    let mut prev_takes_value = false;
    for a in args {
        let a = a.as_ref();
        out.push(prev_takes_value);
        prev_takes_value = if prev_takes_value || a == "--json" {
            false
        } else {
            VALUE_OPTIONS.contains(&a) && !(in_rules && a == "--skills")
        };
    }
    out
}

/// Parses `ham` arguments (without the program name).
pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Invocation, ParseError> {
    let mut json = false;
    let mut tokens = Vec::with_capacity(args.len());
    // `--json` is a flag wherever it stands, except as the value of the option before it.
    let is_value = value_positions(args);
    for (a, value) in args.iter().zip(is_value) {
        let a = a.as_ref();
        if a == "--json" && !value {
            json = true;
            continue;
        }
        tokens.push(a.to_string());
    }
    let Some(first) = tokens.first().cloned() else {
        return err(format!(
            "ham \"<prompt>\" (--repo cc:<name> | --new-repo cc:<name> | --s3-only) --direct | ham <command> ({})",
            crate::registry::subcommand_names().join(", ")
        ));
    };
    let rest = Args {
        tokens: tokens[1..].to_vec(),
        pos: 0,
    };
    // The registry is the list of subcommands (a test pins it equal to `SUBCOMMANDS`).
    let command = if crate::registry::subcommand(&first).is_some() {
        parse_sub(&first, rest)?
    } else {
        if let Some(message) = mistyped_command(&first, &rest.tokens) {
            return err(message);
        }
        parse_start(Some(first), rest)?
    };
    if json && matches!(command, Command::Update { check: false, .. }) {
        return err(
            "ham update --json goes with --check: the update itself hands the terminal to setup",
        );
    }
    Ok(Invocation { json, command })
}

/// The text of an answer: positional, or `--text` when it starts with `-` (the lone `-` stays positional and means standard input).
fn push_text(push: &mut impl FnMut(&[&str]), text: Option<&str>) {
    if let Some(t) = text {
        if t.starts_with('-') && t != "-" {
            push(&["--text", t]);
        } else {
            push(&[t]);
        }
    }
}

/// The start options of package REQ1 (`--goal`, `--research` / `--no-research`, `--allow-third-party`, `--attach`) of a direct run.
fn push_members(push: &mut impl FnMut(&[&str]), d: &DirectArgs) {
    if let Some(g) = d.goal {
        push(&["--goal", if g { "on" } else { "off" }]);
    }
    match d.research {
        Some(true) => push(&["--research"]),
        Some(false) => push(&["--no-research"]),
        None => {}
    }
    if d.allow_third_party {
        push(&["--allow-third-party"]);
    }
    for f in &d.attach {
        push(&["--attach", f]);
    }
    if let Some(c) = d.compute {
        push(&["--compute", c.word()]);
    }
    if let Some(h) = d.max_run_hours {
        push(&["--max-run-hours", &h.to_string()]);
    }
    if let Some(e) = &d.engine {
        push(&["--engine", e]);
    }
    if let Some(p) = d.provider_sharing {
        push(&["--provider-sharing", if p { "on" } else { "off" }]);
    }
}

fn push_answer_options(push: &mut impl FnMut(&[&str]), request_id: Option<&str>, no_wait: bool) {
    if let Some(r) = request_id {
        push(&["--request", r]);
    }
    if no_wait {
        push(&["--no-wait"]);
    }
}

impl Command {
    /// Whether the command writes in the folder of the user's own files (`config.toml`, `engines/`, `rules/`, `skills/`, `continue.md`,
    /// `review.md`). While only the folder of the earlier command name holds those files, such a command is refused (`config_dir::write_refusal`)
    /// before it opens an editor or builds anything. `config migrate` writes only the new folder, so it is not one.
    pub fn writes_config_folder(&self) -> bool {
        match self {
            Command::Config(ConfigCommand::Set { .. } | ConfigCommand::Unset { .. })
            | Command::Config(ConfigCommand::Edit) => true,
            Command::Engine(EngineCommand { action, .. }) => {
                !matches!(action, EngineAction::Show | EngineAction::Check)
            }
            Command::Rules(
                RulesCommand::Init
                | RulesCommand::InitNoSkills
                | RulesCommand::InitPack { .. }
                | RulesCommand::Edit { .. },
            ) => true,
            _ => false,
        }
    }
}

impl Invocation {
    /// The canonical argv; `parse(&inv.to_argv()) == Ok(inv)`.
    pub fn to_argv(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        let mut push = |xs: &[&str]| v.extend(xs.iter().map(|x| (*x).to_string()));
        match &self.command {
            Command::StartDirect(d) => {
                push(&["--prompt", &d.prompt]);
                match &d.target {
                    TargetArg::Repo(t) => push(&["--repo", &t.to_string()]),
                    TargetArg::NewRepo(t) => push(&["--new-repo", &t.to_string()]),
                    TargetArg::S3Only => push(&["--s3-only"]),
                }
                push(&["--direct"]);
                if let Some(m) = &d.model {
                    push(&["--model", m]);
                }
                if let Some(b) = &d.base {
                    push(&["--base", b]);
                }
                if let Some(r) = &d.rules {
                    push(&["--rules", r]);
                }
                if d.no_rules {
                    push(&["--no-rules"]);
                }
                if let Some(c) = &d.continue_file {
                    push(&["--continue", c]);
                }
                if let Some(w) = d.input_wait {
                    push(&["--input-wait", &w.to_string()]);
                }
                if let Some(s) = &d.skills {
                    push(&["--skills", s]);
                }
                if d.no_skills {
                    push(&["--no-skills"]);
                }
                push_members(&mut push, d);
            }
            Command::Continue(d, explicit) => {
                push(&["continue", d.continue_from.as_deref().unwrap_or_default()]);
                match explicit {
                    Some(TargetArg::Repo(t)) => push(&["--repo", &t.to_string()]),
                    Some(TargetArg::NewRepo(t)) => push(&["--new-repo", &t.to_string()]),
                    Some(TargetArg::S3Only) => push(&["--s3-only"]),
                    None => {}
                }
                if !d.prompt.is_empty() {
                    push(&["--prompt", &d.prompt]);
                }
                if let Some(m) = &d.model {
                    push(&["--model", m]);
                }
                if let Some(r) = &d.rules {
                    push(&["--rules", r]);
                }
                if d.no_rules {
                    push(&["--no-rules"]);
                }
                if let Some(c) = &d.continue_file {
                    push(&["--continue", c]);
                }
                if let Some(w) = d.input_wait {
                    push(&["--input-wait", &w.to_string()]);
                }
                if let Some(s) = &d.skills {
                    push(&["--skills", s]);
                }
                if d.no_skills {
                    push(&["--no-skills"]);
                }
                push_members(&mut push, d);
            }
            Command::Lifecycle(LifecycleCommand::Init { dir, kiro }) => {
                push(&["lifecycle", "init"]);
                if *kiro {
                    push(&["--kiro"]);
                }
                push(&[dir]);
            }
            Command::Rules(r) => match r {
                RulesCommand::Init => push(&["rules", "init"]),
                RulesCommand::InitNoSkills => push(&["rules", "init", "--no-skills"]),
                RulesCommand::InitPack {
                    pack,
                    skills_as_rules,
                    skills,
                } => {
                    push(&["rules", "init", "--pack", pack.name()]);
                    if *skills_as_rules {
                        push(&["--skills-as-rules"]);
                    }
                    if *skills {
                        push(&["--skills"]);
                    }
                }
                RulesCommand::Edit { which } => {
                    push(&["rules", "edit"]);
                    if let Some(w) = which {
                        push(&[w]);
                    }
                }
                RulesCommand::Show {
                    rules,
                    continue_file,
                } => {
                    push(&["rules", "show"]);
                    if let Some(r) = rules {
                        push(&["--rules", r]);
                    }
                    if let Some(c) = continue_file {
                        push(&["--continue", c]);
                    }
                }
            },
            Command::Runs { probes } => {
                push(&["runs"]);
                if *probes {
                    push(&["--probes"]);
                }
            }
            Command::Watch { run_id } => {
                push(&["watch"]);
                if let Some(r) = run_id {
                    push(&[r]);
                }
            }
            Command::Cancel { run_id } => push(&["cancel", run_id]),
            Command::Models => push(&["models"]),
            Command::ModelsServer => push(&["models", "--server"]),
            Command::Config(c) => match c {
                ConfigCommand::List => push(&["config"]),
                ConfigCommand::Get { key } => push(&["config", "get", key]),
                ConfigCommand::Set { key, value } => push(&["config", "set", key, value]),
                ConfigCommand::Unset { key } => push(&["config", "unset", key]),
                ConfigCommand::Show => push(&["config", "show"]),
                ConfigCommand::Check { strict } => {
                    push(&["config", "check"]);
                    if *strict {
                        push(&["--strict"]);
                    }
                }
                ConfigCommand::Edit => push(&["config", "edit"]),
                ConfigCommand::Docs => push(&["config", "docs"]),
                ConfigCommand::Migrate { dry_run } => {
                    push(&["config", "migrate"]);
                    if *dry_run {
                        push(&["--dry-run"]);
                    }
                }
            },
            Command::Engine(EngineCommand { engine, action }) => {
                push(&["engine", engine]);
                match action {
                    EngineAction::Choose => {}
                    EngineAction::Init => push(&["init"]),
                    EngineAction::Show => push(&["show"]),
                    EngineAction::Check => push(&["check"]),
                    EngineAction::Edit => push(&["edit"]),
                    EngineAction::Set { key, value } => push(&["set", key, value]),
                    EngineAction::Unset { key } => push(&["unset", key]),
                    EngineAction::Import { file, replace } => {
                        push(&["import", file]);
                        if *replace {
                            push(&["--replace"]);
                        }
                    }
                }
            }
            Command::Pr { run_id } => push(&["pr", run_id]),
            Command::Pull { run_id, artifact } => {
                push(&["pull", run_id]);
                match artifact {
                    PullArtifact::Zip => {}
                    PullArtifact::Partial => push(&["--artifact", "partial"]),
                    PullArtifact::Evidence => push(&["--artifact", "evidence"]),
                    PullArtifact::Snapshot => push(&["--snapshot"]),
                }
            }
            Command::Clone { run_id, dir } => {
                push(&["clone", run_id]);
                if let Some(d) = dir {
                    push(&[d]);
                }
            }
            Command::Push {
                run_id,
                url,
                branch,
            } => {
                push(&["push", run_id, url]);
                if let Some(b) = branch {
                    push(&["--branch", b]);
                }
            }
            Command::Link { run_id } => push(&["link", run_id]),
            Command::Resume {
                run_id,
                all,
                dry_run,
            } => {
                push(&["resume"]);
                if let Some(r) = run_id {
                    push(&[r]);
                }
                if *all {
                    push(&["--all"]);
                }
                if *dry_run {
                    push(&["--dry-run"]);
                }
            }
            Command::Update {
                check,
                yes,
                no_wait,
                wait_minutes,
            } => {
                push(&["update"]);
                if *check {
                    push(&["--check"]);
                }
                if *yes {
                    push(&["--yes"]);
                }
                if *no_wait {
                    push(&["--no-wait"]);
                }
                if let Some(m) = wait_minutes {
                    push(&["--wait", &m.to_string()]);
                }
            }
            Command::Doctor {
                catalog_max_age_hours,
            } => {
                push(&["doctor"]);
                if let Some(h) = catalog_max_age_hours {
                    push(&["--catalog-max-age", &h.to_string()]);
                }
            }
            Command::Say {
                run_id,
                text,
                request_id,
                no_wait,
            } => {
                push(&["say", run_id]);
                push_text(&mut push, Some(text.as_str()));
                push_answer_options(&mut push, request_id.as_deref(), *no_wait);
            }
            Command::NewSession {
                run_id,
                text,
                request_id,
                no_wait,
            } => {
                push(&["new-session", run_id]);
                push_text(&mut push, text.as_deref());
                push_answer_options(&mut push, request_id.as_deref(), *no_wait);
            }
            Command::TryModel {
                run_id,
                model,
                text,
                request_id,
                no_wait,
            } => {
                push(&["try-model", run_id, model]);
                push_text(&mut push, text.as_deref());
                push_answer_options(&mut push, request_id.as_deref(), *no_wait);
            }
            Command::Finish {
                run_id,
                request_id,
                no_wait,
            } => {
                push(&["finish", run_id]);
                push_answer_options(&mut push, request_id.as_deref(), *no_wait);
            }
        }
        if self.json {
            v.push("--json".into());
        }
        v
    }
}
