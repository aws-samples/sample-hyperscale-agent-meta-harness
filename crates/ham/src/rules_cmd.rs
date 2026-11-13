//! `ham rules init|edit|show` and the reading of the user's rules for a direct run.
//!
//! The rules live in `~/.config/ham/rules/` (one flat directory of `.md` files; `XDG_CONFIG_HOME` moves it with
//! `config.toml`), the nudge text in `~/.config/ham/continue.md` and the review prompt in `~/.config/ham/review.md`. `ham` reads them when a run starts and sends their
//! contents: a symlink is followed (a dotfile manager's link works), a dangling one is an error. Hidden entries (editor swap
//! files, `.DS_Store`) are skipped; any other entry that is not a valid rule stops the run with its name, because a rule the
//! user believes applies must never be dropped silently. Everything is checked with `ham_core::rules`, the validator
//! control-api runs again.
//!
//! `init` writes the default rules file and `continue.md` once and never overwrites a file the user edited, and installs the lifecycle pack: its contract as the rule
//! `10-lifecycle.md` and its seven skills in `~/.config/ham/skills/`.
//! The contract comes in
//! three texts, and the setting `run.lifecycle_flow` of `config.toml` picks one: `auto` (the default: the model chooses the
//! quick path or the eight steps), `full` or `quick`. A bad value in the file stops the install before anything is written. A pack file that is already
//! there is kept, except one that holds exactly an earlier text of the same pack file (`lifecycle/pack.history.json`): nobody edited it, so it is
//! replaced by the current text and the output says `updated ... (was pack X.Y.Z; not edited by you)`. The same holds for the default rules file: one that holds exactly
//! an earlier default (`crates/ham-core/assets/default-rules.history.json`) is replaced and the output says `updated ... (was an earlier default; not edited by you)`;
//! an edited one is kept and the output says so. `edit` opens a file in
//! `$VISUAL`, `$EDITOR`, else `vi`, then validates it; `show` prints what the next run would send, the skills included. None of them needs the
//! network, a config file or AWS credentials.

use ham_core::command::{Pack, RulesCommand};
use ham_core::config::{LifecycleFlow, lifecycle_flow};
use ham_core::exit::ERROR;
use ham_core::pack::{self, Installed};
use ham_core::rules::{
    CONTINUE_NAME, DEFAULT_CONTINUE, DEFAULT_RULES, DEFAULT_RULES_NAME, MAX_RULE_FILES, Problem,
    REVIEW_NAME, RuleError, continue_path, default_review_file, parse_continue, parse_review,
    review_path, rules_dir, sha256_hex, validate_rule_files, validate_rule_name,
};
use ham_core::skills::{self, SkillsError, Why};
use harness_protocol::api::v1::{ContinueSettings, RuleFile};
use harness_protocol::direct::{MAX_REQUEST_BYTES, MAX_SKILLS, SkillProblem, exceeds_request};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Where the files live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub dir: PathBuf,
    pub continue_md: PathBuf,
    /// The review prompt, beside `continue.md`.
    pub review_md: PathBuf,
}

pub fn paths(env: Env<'_>) -> Result<Paths, String> {
    match (rules_dir(env), continue_path(env), review_path(env)) {
        (Some(dir), Some(continue_md), Some(review_md)) => Ok(Paths {
            dir,
            continue_md,
            review_md,
        }),
        _ => Err("no config location: set XDG_CONFIG_HOME or HOME".into()),
    }
}

fn io_error(name: &str, e: &std::io::Error) -> RuleError {
    RuleError::new(name, Problem::Unreadable(e.to_string()))
}

/// The text of one file, refused before it is read when it is longer than a whole request can be.
fn read_text(path: &Path, name: &str) -> Result<String, RuleError> {
    let meta = std::fs::metadata(path).map_err(|e| io_error(name, &e))?;
    if meta.is_dir() {
        return Err(RuleError::new(
            name,
            Problem::NotRegular("directory".into()),
        ));
    }
    if !meta.is_file() {
        return Err(RuleError::new(
            name,
            Problem::NotRegular("special file".into()),
        ));
    }
    if exceeds_request(meta.len()) {
        return Err(RuleError::new(
            name,
            Problem::TooLarge {
                bytes: usize::try_from(meta.len()).unwrap_or(usize::MAX),
                max: MAX_REQUEST_BYTES,
            },
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| io_error(name, &e))?;
    String::from_utf8(bytes).map_err(|_| RuleError::new(name, Problem::NotUtf8))
}

/// The rule files at `path`: every visible entry of a directory (sorted by name), or the one file `path` names. Validated as a
/// set. A path that does not exist is an error; an empty directory is an empty list.
pub fn read_rules_path(path: &Path) -> Result<Vec<RuleFile>, RuleError> {
    let shown = path.display().to_string();
    let meta = std::fs::metadata(path).map_err(|e| io_error(&shown, &e))?;
    let mut files = Vec::new();
    if meta.is_dir() {
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(path)
            .map_err(|e| io_error(&shown, &e))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(|e| io_error(&shown, &e))?;
        names.sort();
        for name in names {
            let lossy = name.to_string_lossy().into_owned();
            if lossy.starts_with('.') {
                continue;
            }
            let Some(utf8) = name.to_str() else {
                return Err(RuleError::new(&lossy, Problem::BadName));
            };
            validate_rule_name(utf8).map_err(|p| RuleError::new(utf8, p))?;
            let content = read_text(&path.join(&name), utf8)?;
            files.push(ham_core::rules::rule_file(utf8, &content));
        }
    } else {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| RuleError::new(&shown, Problem::BadName))?;
        validate_rule_name(name).map_err(|p| RuleError::new(name, p))?;
        let content = read_text(path, name)?;
        files.push(ham_core::rules::rule_file(name, &content));
    }
    validate_rule_files(&files)?;
    Ok(files)
}

/// `continue.md` (or the file `--continue` names).
pub fn read_continue(path: &Path) -> Result<ContinueSettings, RuleError> {
    let text = read_text(path, CONTINUE_NAME)?;
    parse_continue(&text)
}

/// `review.md`: the review prompt of the review round, as the request member that carries it.
pub fn read_review(path: &Path) -> Result<harness_protocol::direct::ReviewOptions, RuleError> {
    let text = read_text(path, REVIEW_NAME)?;
    parse_review(&text)
}

/// What a direct run sends from disk: the rules (the default directory, or `rules` when given, or none for `--no-rules`) and the
/// nudge (`continue_file` when given, else `continue.md` when it exists).
pub fn direct_inputs(
    env: Env<'_>,
    rules: Option<&str>,
    no_rules: bool,
    continue_file: Option<&str>,
    skills_path: Option<&str>,
    no_skills: bool,
) -> Result<ham_core::request::DirectInputs, String> {
    let rule_files = if no_rules {
        Vec::new()
    } else if let Some(p) = rules {
        let files = read_rules_path(Path::new(p)).map_err(|e| e.to_string())?;
        if files.is_empty() {
            return Err(format!(
                "{p} holds no rule files; add a .md file, or start with --no-rules"
            ));
        }
        files
    } else {
        let p = paths(env)?;
        if !p.dir.exists() {
            return Err(no_rules_message(&p.dir));
        }
        match read_rules_path(&p.dir) {
            Ok(files) if !files.is_empty() => files,
            Ok(_) => return Err(no_rules_message(&p.dir)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let nudge = match continue_file {
        Some(p) => Some(read_continue(Path::new(p)).map_err(|e| e.to_string())?),
        None => {
            let p = paths(env)?;
            if p.continue_md.exists() {
                Some(read_continue(&p.continue_md).map_err(|e| e.to_string())?)
            } else {
                None
            }
        }
    };
    // The review prompt: sent when the user has the file, like the nudge; a missing file sends none and agentd uses its built-in prompt.
    let review = {
        let p = paths(env)?;
        if p.review_md.exists() {
            Some(read_review(&p.review_md).map_err(|e| e.to_string())?)
        } else {
            None
        }
    };
    // `--no-rules` sends none of the user's files, skills included; `--no-skills` sends no skills; `--skills <dir>` names the folder to read
    // instead of `~/.config/ham/skills/`. A skill that cannot be read stops the run before anything is sent, like a bad rule.
    let skill_set = if no_rules || no_skills {
        Vec::new()
    } else if let Some(p) = skills_path {
        skills::read_skills_path(Path::new(p)).map_err(|e| skills_error(&e, Some(Path::new(p))))?
    } else {
        skills::read_default(env)
            .map_err(|e| skills_error(&e, skills::skills_dir(env).as_deref()))?
    };
    Ok(ham_core::request::DirectInputs {
        rules: rule_files,
        nudge,
        skills: skill_set,
        attachments: Vec::new(),
        engine_config: None,
        review,
    })
}

/// Why the inputs of a run could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputsError {
    /// The rules, the nudge, the skills or an attached file: the words of the problem.
    Files(String),
    /// The engine file was refused (the shared check) or could not be read: the words, which end with "Nothing was sent.". The command line exits
    /// 2 for it, as it does for any file the user must correct before a run can start.
    EngineFile(String),
}

impl std::fmt::Display for InputsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputsError::Files(s) | InputsError::EngineFile(s) => f.write_str(s),
        }
    }
}

impl From<InputsError> for String {
    fn from(e: InputsError) -> String {
        e.to_string()
    }
}

/// The settings of the engine's file (`engine` is the id of the run's engine), read once and checked by the shared check: `None` for a user with
/// no file or a file that sets no key (the request then has no `engineConfig` member), the refusal for a file the check refuses. An engine that
/// has no key table in this build has no file that is read, so nothing of it is sent.
pub fn engine_settings(
    env: Env<'_>,
    engine: &str,
) -> Result<Option<harness_protocol::direct::EngineConfig>, InputsError> {
    if !ham_core::engines::has_settings(engine) {
        return Ok(None);
    }
    let shown = ham_core::engine_file::shown_path(engine);
    let file = crate::engine_cmd::load(env, engine)
        .map_err(|e| InputsError::EngineFile(format!("{e}. Nothing was sent.")))?;
    let reading = file.read(engine);
    if reading.ok() {
        Ok(reading.config)
    } else {
        Err(InputsError::EngineFile(reading.refusal(&shown)))
    }
}

/// What a run reads from disk for `args`, whose switches `ham_core::request::resolve_run_settings` has already settled (`run.rules` and
/// `run.skills` of the file are folded into `no_rules` and `no_skills`): the rules, the nudge, the skills, the files `--attach` names and the
/// settings of the engine file. The three places that start a run call this, so none of them applies a switch by hand.
pub fn direct_inputs_for(
    env: Env<'_>,
    args: &ham_core::command::DirectArgs,
) -> Result<ham_core::request::DirectInputs, InputsError> {
    let inputs = direct_inputs(
        env,
        args.rules.as_deref(),
        args.no_rules,
        args.continue_file.as_deref(),
        args.skills.as_deref(),
        args.no_skills,
    )
    .map_err(InputsError::Files)?;
    let mut inputs = with_attachments(inputs, &args.attach).map_err(InputsError::Files)?;
    let engine = args
        .engine
        .as_deref()
        .unwrap_or(ham_core::engines::DEFAULT_ENGINE_ID);
    inputs.engine_config = engine_settings(env, engine)?;
    Ok(inputs)
}

/// `inputs` with the files `--attach` names read from disk: UTF-8 text, as many and as large as one request carries.
/// A file that cannot be read, or breaks a rule of the shared validator, stops the run before anything is sent, like a bad rule. `--no-rules` does not
/// drop them: the user named each one.
pub fn with_attachments(
    mut inputs: ham_core::request::DirectInputs,
    paths: &[String],
) -> Result<ham_core::request::DirectInputs, String> {
    inputs.attachments =
        ham_core::attachments::read_attachments(paths).map_err(|e| e.to_string())?;
    Ok(inputs)
}

/// The visible folders of a skills folder, sorted: the skills a run would read from it.
fn skill_folders(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// The text of a skills error. A run takes at most [`MAX_SKILLS`] skills and the pack's seven are among them once `ham rules init` has put
/// them in the folder; the validator's own words for the overflow ("9 skills; the limit is 8") say neither where the skills come from nor
/// what to do, so the message adds both. Every other problem keeps its words.
fn skills_error(e: &SkillsError, dir: Option<&Path>) -> String {
    let base = e.to_string();
    let (Why::Invalid(SkillProblem::TooManySkills { count, max }), Some(dir)) = (&e.why, dir)
    else {
        return base;
    };
    let names = skill_folders(dir);
    let (from_pack, own): (Vec<&String>, Vec<&String>) =
        names.iter().partition(|n| pack::is_skill_name(n));
    let whose = if from_pack.is_empty() {
        format!("{count} skills of yours")
    } else {
        format!(
            "{count} skills: {} are the lifecycle pack's (installed by `ham rules init`) and {} are yours ({})",
            from_pack.len(),
            own.len(),
            own.iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "{base}. {} holds {whose}. A run takes at most {max}. Remove the folders you do not need from {}, or start the run with --no-skills to send none.",
        dir.display(),
        dir.display()
    )
}

fn no_rules_message(dir: &Path) -> String {
    format!(
        "no rules found in {}: run `ham rules init` to write the default rules, or start with --no-rules",
        dir.display()
    )
}

fn fail(err: &mut dyn Write, msg: impl std::fmt::Display) -> i32 {
    let _ = writeln!(err, "ham: {msg}");
    ERROR
}

/// Creates `path` with `text` unless it exists; true when it wrote.
fn create_once(path: &Path, text: &str) -> Result<bool, String> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => f
            .write_all(text.as_bytes())
            .map(|()| true)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn ensure_dir(dir: &Path, what: &str) -> Result<(), String> {
    if dir.exists() && !dir.is_dir() {
        return Err(format!(
            "{} exists and is not a directory; move it away so ham can keep {what} there",
            dir.display()
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// `ham rules init`: the default rules file where the directory holds none, and `continue.md` where there is none. A default that is already
/// there is brought up to date when it holds exactly an earlier default (nobody edited it); an edited one is kept.
fn init(p: &Paths, out: &mut dyn Write) -> Result<(), String> {
    ensure_dir(&p.dir, "the rules")?;
    let default = p.dir.join(DEFAULT_RULES_NAME);
    let has_rules = std::fs::read_dir(&p.dir)
        .map_err(|e| format!("{}: {e}", p.dir.display()))?
        .filter_map(Result::ok)
        .any(|e| !e.file_name().to_string_lossy().starts_with('.'));
    let say = |out: &mut dyn Write, line: String| {
        let _ = writeln!(out, "{line}");
    };
    if has_rules {
        if std::fs::symlink_metadata(&default).is_ok() {
            match pack::refresh_default_rules(&default)? {
                Installed::Updated { .. } => say(
                    out,
                    format!(
                        "updated {} (was an earlier default; not edited by you)",
                        default.display()
                    ),
                ),
                Installed::KeptEdited => say(
                    out,
                    format!(
                        "kept {} (edited by you; not changed; the current default has sha256 {})",
                        default.display(),
                        sha256_hex(DEFAULT_RULES)
                    ),
                ),
                Installed::Same | Installed::Created => say(
                    out,
                    format!("kept {} (already there; not changed)", default.display()),
                ),
            }
        } else {
            say(
                out,
                format!(
                    "kept {} (the rules directory already holds your own files; the default is not added)",
                    default.display()
                ),
            );
        }
    } else if create_once(&default, DEFAULT_RULES)? {
        say(
            out,
            format!(
                "created {} ({} bytes): your rules, sent with every run. Change them with: ham rules edit",
                default.display(),
                DEFAULT_RULES.len()
            ),
        );
    }
    if let Some(parent) = p.continue_md.parent() {
        ensure_dir(parent, "the nudge file")?;
    }
    if create_once(&p.continue_md, DEFAULT_CONTINUE)? {
        say(
            out,
            format!(
                "created {}: what Cline is told when it stops without saying it is done. Change it with: ham rules edit continue",
                p.continue_md.display()
            ),
        );
    } else {
        say(
            out,
            format!(
                "kept {} (already there; not changed)",
                p.continue_md.display()
            ),
        );
    }
    // The review prompt: written once, never over a file the user edited.
    if let Some(parent) = p.review_md.parent() {
        ensure_dir(parent, "the review file")?;
    }
    if create_once(&p.review_md, &default_review_file())? {
        say(
            out,
            format!(
                "created {}: what the engine is asked once after it reports done. Change it with: ham rules edit review; switch the round off with: ham config set run.review_round off",
                p.review_md.display()
            ),
        );
    } else {
        say(
            out,
            format!(
                "kept {} (already there; not changed)",
                p.review_md.display()
            ),
        );
    }
    Ok(())
}

/// What `ham rules init` installs in the rules folder beyond the default rules file (the skills folder is a separate argument).
#[derive(Clone, Copy, Debug, Default)]
struct PackOpts {
    /// The contract as the rule `10-lifecycle.md`.
    contract: bool,
    /// Which text of the contract: the setting `run.lifecycle_flow`.
    flow: LifecycleFlow,
    /// Each skill body as an always-on rule file (`2n-lifecycle-<step>.md`).
    skills_as_rules: bool,
}

/// How much of the pack's skills a `ham rules init` writes to the skills folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkillsMode {
    /// None: `--no-skills`, or the skill bodies as rules instead.
    No,
    /// The default of a bare `ham rules init`: the seven skills when they fit. When the skills folder cannot take them (it is a link, or
    /// the skills of the user's own leave fewer than seven of the eight a run takes), the rest of the install goes ahead and the output
    /// says what was left out and why, because an install the user did not ask for in so many words must not leave their runs refused.
    Default,
    /// `--skills`: the seven skills, or nothing written at all and the reason as the error.
    Explicit,
}

fn visible_entries(dir: &Path) -> Result<Vec<String>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    Ok(std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect())
}

/// Whether the pack's skills can go into the skills folder `dir`, read before anything is written.
enum Room {
    Fits,
    /// The folder cannot take them as the pack writes them: a link, or a file where the folder belongs. The text says what and what to do.
    Blocked(String),
    /// The skills of the user's own (`own`) and the pack's seven would pass the [`MAX_SKILLS`] a run takes.
    Full {
        own: Vec<String>,
    },
}

fn skills_room(dir: &Path) -> Room {
    if let Ok(m) = std::fs::symlink_metadata(dir) {
        if m.file_type().is_symlink() {
            return Room::Blocked(format!(
                "{} is a symbolic link; move it away so the pack is not written through it",
                dir.display()
            ));
        }
        if !m.is_dir() {
            return Room::Blocked(format!("{} exists and is not a directory", dir.display()));
        }
    }
    let own: Vec<String> = skill_folders(dir)
        .into_iter()
        .filter(|n| !pack::is_skill_name(n))
        .collect();
    if own.len() + pack::skill_names().len() > MAX_SKILLS {
        Room::Full { own }
    } else {
        Room::Fits
    }
}

/// The sentence for a skills folder that cannot take all seven: whose skills are in the way and how many a run takes.
fn no_room_message(dir: &Path, own: &[String]) -> String {
    let seven = pack::skill_names().len();
    format!(
        "{} already holds {} skills of yours ({}); a run takes at most {MAX_SKILLS} and the pack's {seven} need {seven} of them",
        dir.display(),
        own.len(),
        own.join(", "),
    )
}

/// The line that says which text of the contract is installed and how to pick another.
fn flow_line(flow: LifecycleFlow) -> String {
    let what = match flow {
        LifecycleFlow::Auto => {
            "the run takes the quick path for a small change in an existing repository and the eight steps for anything else"
        }
        LifecycleFlow::Full => "the run takes the eight steps on every task",
        LifecycleFlow::Quick => "the run takes the quick path on every task",
    };
    format!(
        "flow {} (run.lifecycle_flow): {what}. Change it with: ham config set run.lifecycle_flow auto|full|quick, then ham rules init",
        flow.word()
    )
}

/// The flow `ham rules init` installs: `run.lifecycle_flow` of `config.toml`, else `auto`. A file with an error (a bad value among them) is
/// refused with the findings and the three values, so the contract the user asked for is never replaced by another one without a word.
fn chosen_flow(env: Env<'_>) -> Result<LifecycleFlow, String> {
    let loaded = crate::config_cmd::load_file(env)?;
    if let Some(why) = loaded.refusal() {
        return Err(format!("{}: {why}", loaded.shown()));
    }
    if loaded.read.has_errors() {
        return Err(format!(
            "{} has a problem, so no flow can be chosen (run.lifecycle_flow takes auto, full or quick):\n{}\nham: fix it with: ham config edit",
            loaded.shown(),
            loaded.read.render_errors()
        ));
    }
    Ok(lifecycle_flow(&loaded.read.config))
}

/// `ham rules init` with the lifecycle pack. The whole install is planned before the first file is written: a plan that would pass the
/// limits (the rule file count), or a skills folder asked for with `--skills` that is a link or has no room, writes nothing.
fn init_pack(
    p: &Paths,
    skills: Option<(&Path, SkillsMode)>,
    opts: PackOpts,
    out: &mut dyn Write,
) -> Result<(), String> {
    let mut wanted: Vec<RuleFile> = Vec::new();
    if opts.contract {
        wanted.push(pack::contract_rule_for(opts.flow));
    }
    if opts.skills_as_rules {
        wanted.extend(pack::skills_as_rules());
    }
    // 1. the plan
    if p.dir.exists() && !p.dir.is_dir() {
        return Err(format!(
            "{} exists and is not a directory; move it away so ham can keep the rules there",
            p.dir.display()
        ));
    }
    let existing = visible_entries(&p.dir)?;
    let default_new = existing.is_empty();
    let creates: Vec<&str> = wanted
        .iter()
        .map(|r| r.path.as_str())
        .filter(|n| !existing.iter().any(|e| e == n))
        .collect();
    let after = existing.len() + usize::from(default_new) + creates.len();
    if after > MAX_RULE_FILES {
        return Err(format!(
            "the install would leave {after} rule files in {}; the limit is {MAX_RULE_FILES}. Remove a rule file, or start with --pack none",
            p.dir.display()
        ));
    }
    if p.dir.exists() {
        let mut all = read_rules_path(&p.dir).map_err(|e| e.to_string())?;
        if default_new {
            all.push(ham_core::rules::rule_file(
                DEFAULT_RULES_NAME,
                DEFAULT_RULES,
            ));
        }
        all.extend(
            wanted
                .iter()
                .filter(|r| creates.contains(&r.path.as_str()))
                .cloned(),
        );
        validate_rule_files(&all).map_err(|e| e.to_string())?;
    }
    // The skills: written when there is room; otherwise `--skills` stops here and a bare install says what it left out and what to do.
    let mut left_out: Option<String> = None;
    let skills_dir = match skills {
        Some((dir, mode)) => match skills_room(dir) {
            Room::Fits => Some(dir),
            Room::Blocked(why) if mode == SkillsMode::Explicit => return Err(why),
            Room::Blocked(why) => {
                left_out = Some(format!(
                    "{why}. The contract is installed. To add the skills, fix that and run `ham rules init` again"
                ));
                None
            }
            Room::Full { own } if mode == SkillsMode::Explicit => {
                return Err(format!(
                    "the install would leave {} skills in {}; {}. Remove skills of yours you do not need, or install the contract alone with `ham rules init --no-skills`",
                    own.len() + pack::skill_names().len(),
                    dir.display(),
                    no_room_message(dir, &own)
                ));
            }
            Room::Full { own } => {
                left_out = Some(format!(
                    "{}. The contract is installed. To add the skills, remove skills of yours you do not need and run `ham rules init` again; `ham rules init --no-skills` installs the contract alone",
                    no_room_message(dir, &own)
                ));
                None
            }
        },
        None => None,
    };
    // 2. the install
    init(p, out)?;
    let digest = pack::digest();
    let _ = writeln!(
        out,
        "lifecycle pack {} ({}):",
        pack::version(),
        &digest[..12]
    );
    if opts.contract {
        let _ = writeln!(out, "{}", flow_line(opts.flow));
    }
    for rule in &wanted {
        let path = p.dir.join(&rule.path);
        match pack::install_rule(&p.dir, &rule.path, &rule.content).map_err(|e| e.to_string())? {
            Installed::Created => {
                let _ = writeln!(
                    out,
                    "created {} ({} bytes): sent with every run beside your own rules",
                    path.display(),
                    rule.content.len()
                );
            }
            Installed::Same => {
                let _ = writeln!(out, "kept {} (already there; not changed)", path.display());
            }
            Installed::KeptEdited => {
                let _ = writeln!(
                    out,
                    "kept {} (edited by you; not changed; the pack's text has sha256 {})",
                    path.display(),
                    rule.sha256
                );
            }
            Installed::Updated { was } => {
                let _ = writeln!(
                    out,
                    "updated {} (was pack {was}; not edited by you)",
                    path.display()
                );
            }
        }
    }
    if let Some(dir) = skills_dir {
        install_skills(dir, out)?;
    }
    if let Some(why) = left_out {
        let _ = writeln!(out, "skills not installed: {why}");
    }
    Ok(())
}

/// The pack's skills as folders of `dir` (`~/.config/ham/skills/`): a file that is already there is kept, unless it holds an earlier text of
/// the pack, which nobody edited, and is replaced; a missing one is written again. One line per skill says which of these happened.
fn install_skills(dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    let all = pack::skills();
    let (mut created, mut replaced) = (0usize, 0usize);
    for s in &all {
        let (mut wrote, mut same) = (0usize, 0usize);
        let mut edited: Vec<&str> = Vec::new();
        let mut updated: Vec<(&str, String)> = Vec::new();
        for f in &s.files {
            match pack::install_skill_file(dir, &s.name, &f.path, &f.content)? {
                Installed::Created => wrote += 1,
                Installed::Same => same += 1,
                Installed::KeptEdited => edited.push(&f.path),
                Installed::Updated { was } => updated.push((&f.path, was)),
            }
        }
        created += wrote;
        replaced += updated.len();
        let files = s.files.len();
        if wrote == files {
            let _ = writeln!(
                out,
                "created {} ({files} file{})",
                s.name,
                if files == 1 { "" } else { "s" }
            );
        } else if wrote > 0 {
            let _ = writeln!(
                out,
                "created {} ({wrote} of {files} files; the others were there)",
                s.name
            );
        } else if edited.is_empty() && updated.is_empty() && same == files {
            let _ = writeln!(out, "kept {} (already there; not changed)", s.name);
        }
        for (path, was) in updated {
            let _ = writeln!(
                out,
                "updated {}/{path} (was pack {was}; not edited by you)",
                s.name
            );
        }
        for path in edited {
            let _ = writeln!(out, "kept {}/{path} (edited by you; not changed)", s.name);
        }
    }
    let _ = writeln!(
        out,
        "{} skills in {} ({} files written); a run sends them as its skills",
        all.len(),
        dir.display(),
        created + replaced,
    );
    Ok(())
}

/// `$VISUAL`, else `$EDITOR`, else `vi`; blank values count as unset.
pub(crate) fn editor(env: Env<'_>) -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|k| env(k))
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

/// Runs the user's editor on `file`, with the terminal inherited. The editor string is run by `sh`, as git does, so
/// `code --wait` and a quoted path work. When this process's own output goes to a file (the chat box runs the command as a child and
/// shows that file afterwards), the editor draws on the controlling terminal instead ([`crate::tty::editor_stdio`]).
pub(crate) fn run_editor(editor: &str, file: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg("ham-editor")
        .arg(file);
    crate::tty::editor_stdio(&mut cmd);
    let status = cmd
        .status()
        .map_err(|e| format!("could not start the editor {editor:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the editor {editor:?} ended with {}",
            status
                .code()
                .map_or_else(|| "a signal".to_string(), |c| format!("exit status {c}"))
        ))
    }
}

/// `ham rules edit [<file.md>|continue|review]`.
fn edit(p: &Paths, which: Option<&str>, env: Env<'_>, out: &mut dyn Write) -> Result<(), String> {
    let editing_continue = which == Some("continue") || which == Some(CONTINUE_NAME);
    let editing_review = which == Some("review") || which == Some(REVIEW_NAME);
    let file = if editing_continue {
        p.continue_md.clone()
    } else if editing_review {
        p.review_md.clone()
    } else {
        let name = which.unwrap_or(DEFAULT_RULES_NAME);
        validate_rule_name(name).map_err(|pr| RuleError::new(name, pr).to_string())?;
        p.dir.join(name)
    };
    // A missing file starts from init (the defaults), a new named rule from an empty file.
    if !file.exists() {
        if editing_continue || editing_review || which.is_none() {
            let mut sink = Vec::new();
            init(p, &mut sink)?;
        } else {
            ensure_dir(&p.dir, "the rules")?;
            create_once(&file, "")?;
        }
    }
    run_editor(&editor(env), &file)?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (bytes, sha) = {
        let text = read_text(&file, &name).map_err(|e| e.to_string())?;
        (text.len(), sha256_hex(&text))
    };
    let check = if editing_continue {
        read_continue(&file).map(|_| ())
    } else if editing_review {
        read_review(&file).map(|_| ())
    } else {
        read_rules_path(&p.dir).map(|_| ())
    };
    if let Err(e) = check {
        return Err(format!(
            "{e}\nham: fix it with: ham rules edit{}",
            if editing_continue {
                " continue".to_string()
            } else if editing_review {
                " review".to_string()
            } else {
                which.map(|w| format!(" {w}")).unwrap_or_default()
            }
        ));
    }
    let _ = writeln!(
        out,
        "saved {} ({bytes} bytes, sha256 {sha}); the next run uses it, a run already started keeps the text it began with",
        file.display()
    );
    Ok(())
}

fn show_json(
    dir: &Path,
    files: &[RuleFile],
    nudge: Option<(&Path, &ContinueSettings)>,
    review: Option<(&Path, &harness_protocol::direct::ReviewOptions)>,
    skills_dir: Option<&Path>,
    found: &[harness_protocol::direct::Skill],
) -> serde_json::Value {
    json!({
        "skillsDir": skills_dir.map(|d| d.display().to_string()),
        "skills": found.iter().map(|s| json!({
            "name": s.name,
            "sha256": harness_protocol::direct::skill_sha256(s),
            "files": s.files.len(),
            "bytes": s.files.iter().map(|f| f.size).sum::<u64>(),
        })).collect::<Vec<_>>(),
        "rulesDir": dir.display().to_string(),
        "rules": files.iter().map(|f| json!({
            "path": f.path, "bytes": f.content.len(), "sha256": f.sha256, "content": f.content,
        })).collect::<Vec<_>>(),
        "continue": nudge.map(|(path, c)| json!({
            "path": path.display().to_string(), "maxNudges": c.max_nudges, "sha256": c.sha256, "text": c.text,
        })),
        "review": review.map(|(path, r)| json!({
            "path": path.display().to_string(), "guardS": r.guard_s, "sha256": r.sha256, "text": r.text,
        })),
    })
}

/// `ham rules show [--rules <path>] [--continue <file>]`.
fn show(
    p: &Paths,
    env: Env<'_>,
    rules: Option<&str>,
    continue_file: Option<&str>,
    json_out: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let dir: PathBuf = rules.map_or_else(|| p.dir.clone(), PathBuf::from);
    if rules.is_none() && !dir.exists() {
        return Err(no_rules_message(&dir));
    }
    let files = read_rules_path(&dir).map_err(|e| e.to_string())?;
    if files.is_empty() {
        return Err(no_rules_message(&dir));
    }
    let cont_path: PathBuf = continue_file.map_or_else(|| p.continue_md.clone(), PathBuf::from);
    let nudge = if continue_file.is_some() || cont_path.exists() {
        Some(read_continue(&cont_path).map_err(|e| e.to_string())?)
    } else {
        None
    };
    // The review prompt the next run would send; a missing file is fine, agentd then uses its own prompt.
    let review = if p.review_md.exists() {
        Some(read_review(&p.review_md).map_err(|e| e.to_string())?)
    } else {
        None
    };
    // The skills the next run would send (`~/.config/ham/skills/`); a skill that cannot be read is the same error a run would stop with.
    let skills_dir = skills::skills_dir(env);
    let found = skills::read_default(env).map_err(|e| skills_error(&e, skills_dir.as_deref()))?;
    if json_out {
        let v = show_json(
            &dir,
            &files,
            nudge.as_ref().map(|c| (cont_path.as_path(), c)),
            review.as_ref().map(|r| (p.review_md.as_path(), r)),
            skills_dir.as_deref(),
            &found,
        );
        let _ = writeln!(out, "{v}");
        return Ok(());
    }
    let _ = writeln!(out, "rules: {}", dir.display());
    for f in &files {
        let _ = writeln!(
            out,
            "  {}  {} bytes  sha256 {}",
            f.path,
            f.content.len(),
            f.sha256
        );
    }
    match &nudge {
        Some(c) => {
            let _ = writeln!(
                out,
                "nudge: {}  max_nudges {}  sha256 {}",
                cont_path.display(),
                c.max_nudges,
                c.sha256
            );
        }
        None => {
            let _ = writeln!(
                out,
                "nudge: none ({} is absent; no nudge is sent)",
                cont_path.display()
            );
        }
    }
    match &review {
        Some(r) => {
            let _ = writeln!(
                out,
                "review: {}  guard {}  sha256 {}",
                p.review_md.display(),
                r.guard_s
                    .map_or("the stage's own".to_string(), |g| format!("{} min", g / 60)),
                r.sha256.as_deref().unwrap_or("none")
            );
        }
        None => {
            let _ = writeln!(
                out,
                "review: none ({} is absent; the harness uses its own review prompt)",
                p.review_md.display()
            );
        }
    }
    match &skills_dir {
        Some(d) if !found.is_empty() => {
            let _ = writeln!(out, "skills: {}", d.display());
            for s in &found {
                let _ = writeln!(
                    out,
                    "  {}  {} files  {} bytes  sha256 {}",
                    s.name,
                    s.files.len(),
                    s.files.iter().map(|f| f.size).sum::<u64>(),
                    harness_protocol::direct::skill_sha256(s)
                );
            }
        }
        Some(d) => {
            let _ = writeln!(
                out,
                "skills: none ({} holds no skill; no skills are sent)",
                d.display()
            );
        }
        None => {}
    }
    for f in &files {
        let _ = writeln!(out, "\n----- {} -----\n{}", f.path, f.content.trim_end());
    }
    if let Some(c) = &nudge {
        let _ = writeln!(out, "\n----- {CONTINUE_NAME} -----\n{}", c.text.trim_end());
    }
    if let Some(t) = review.as_ref().and_then(|r| r.text.as_deref()) {
        let _ = writeln!(out, "\n----- {REVIEW_NAME} -----\n{}", t.trim_end());
    }
    Ok(())
}

/// Runs a `ham rules` command; returns the exit code. Output goes to `out`, errors to `err` (the shell passes standard error; the chat
/// box passes a captured one, so nothing is drawn over the screen). `edit` hands the terminal to the editor.
pub fn run(
    cmd: &RulesCommand,
    env: Env<'_>,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let p = match paths(env) {
        Ok(p) => p,
        Err(e) => return fail(err, e),
    };
    // The pack's install: the contract always, and the skills as `mode` says (their folder is resolved only when they are wanted).
    let install =
        |mode: SkillsMode, skills_as_rules: bool, out: &mut dyn Write| -> Result<(), String> {
            let dir =
                if mode == SkillsMode::No {
                    None
                } else {
                    Some(skills::skills_dir(env).ok_or_else(|| {
                        "no config location: set XDG_CONFIG_HOME or HOME".to_string()
                    })?)
                };
            let flow = chosen_flow(env)?;
            init_pack(
                &p,
                dir.as_deref().map(|d| (d, mode)),
                PackOpts {
                    contract: true,
                    flow,
                    skills_as_rules,
                },
                out,
            )
        };
    let result = match cmd {
        // The default of `ham rules init` is the lifecycle pack: the contract beside the default rules and the
        // seven skills.
        RulesCommand::Init => install(SkillsMode::Default, false, out),
        RulesCommand::InitNoSkills => install(SkillsMode::No, false, out),
        RulesCommand::InitPack {
            pack: Pack::None, ..
        } => init(&p, out),
        // `--skills-as-rules` renders the skill bodies as rules: it writes no skills folder unless `--skills` is also given. A bare
        // `--pack lifecycle` is the pack's default install, the same as a bare `ham rules init`.
        RulesCommand::InitPack {
            pack: Pack::Lifecycle,
            skills_as_rules,
            skills: with_skills,
        } => {
            let mode = if *with_skills {
                SkillsMode::Explicit
            } else if *skills_as_rules {
                SkillsMode::No
            } else {
                SkillsMode::Default
            };
            install(mode, *skills_as_rules, out)
        }
        RulesCommand::Edit { which } => edit(&p, which.as_deref(), env, out),
        RulesCommand::Show {
            rules,
            continue_file,
        } => show(
            &p,
            env,
            rules.as_deref(),
            continue_file.as_deref(),
            json_out,
            out,
        ),
    };
    match result {
        Ok(()) => 0,
        Err(e) => fail(err, e),
    }
}
