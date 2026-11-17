//!
//! The command time limit of the second engine never reached the pinned engine because every check looked at what agentd wrote and none
//! started the engine with it.
//!
//! What [`standalone`] is not: the whole startup of a direct run. What `direct/mod.rs` adds after [`base_env`] (the mypy cache folder, the package
//! proxy, cargo, containers, the sandbox's tool names, what the modules add at startup) is not in it.
//!
//! The block also tells git which folders of the run are safe to work in: the engine's own git settings,
//! in git's own variables, so git reads them in every git command that keeps the engine's environment and no file of the user's or of the
//! repository is written. A command that sets `GIT_CONFIG_COUNT` itself replaces them for that command, and the process that serves a
//! local clone or fetch never gets them, which decides with a git older than the worker image's ([`add_git_safe_directories`]).

use super::direct::{direct_launch_argv, launch_env, run_limit, wait_env};
use super::layout::{check_kill_switches, config_layout, config_root_env, lock_settings};
use super::user_settings::{CommandSeconds, Effort, UserSettings, check_command_limit};
use crate::emit::{EmitContext, acp_model_value, aws_env, emit_engine_for, emit_profile};
use harness_core::engine_profile::{fixtures_dir, load_profile, resolve_launch};
use harness_protocol::common::Transport;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The AWS key names no engine launch carries: the engine reaches AWS through the `agent` profile only (root invariant 2).
const AWS_KEY_NAMES: [&str; 4] = [
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
];

/// What [`base_env`] is made of.
#[derive(Clone, Debug)]
pub struct BaseEnv<'a> {
    pub home: &'a Path,
    pub tmp: &'a Path,
    /// The run's repository (the checkout the engine works in).
    pub repo: &'a Path,
    pub engine: &'a str,
    /// The profile's launch environment, rendered (`ResolvedLaunch.env`).
    pub profile_env: &'a BTreeMap<String, String>,
    /// What the engine's emitter added (`Emitted.env`).
    pub built_in: BTreeMap<String, String>,
    pub settings: &'a UserSettings,
    /// The AWS environment of the `agent` profile (`emit::aws_env`).
    pub aws: BTreeMap<String, String>,
    /// The profile's `envRemove`.
    pub env_remove: &'a [String],
}

/// The engine's environment as a direct run starts it, before the package proxy, cargo, containers and the modules add theirs: `HOME`, `TMPDIR`
/// and `USER`; the profile's launch environment; the emitter's; the user's engine settings ([`launch_env`]); the bound of a wait on a background
/// job ([`wait_env`]); the AWS profile; then the removals (the profile's and the four AWS key names); `PYTHONDONTWRITEBYTECODE`; and git's
/// settings for the run's own folders ([`add_git_safe_directories`]). A later source wins over an earlier one.
pub fn base_env(b: BaseEnv<'_>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = [
        ("HOME", b.home.display().to_string()),
        ("TMPDIR", b.tmp.display().to_string()),
        ("USER", "agent".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    env.extend(b.profile_env.clone());
    env.extend(b.built_in);
    env.extend(launch_env(b.engine, b.settings));
    if let Some((var, seconds)) = wait_env(b.engine, b.settings) {
        env.insert(var.to_string(), seconds);
    }
    env.extend(b.aws);
    for k in b.env_remove.iter().map(String::as_str).chain(AWS_KEY_NAMES) {
        env.remove(k);
    }
    env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    add_git_safe_directories(&mut env, b.repo, b.tmp);
    env
}

/// git's variable for the number of settings its environment carries, and the stems of the names of each pair.
pub const GIT_CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";
const GIT_CONFIG_KEY: &str = "GIT_CONFIG_KEY_";
const GIT_CONFIG_VALUE: &str = "GIT_CONFIG_VALUE_";
/// git's setting for a folder it works in whoever the folder belongs to.
pub const SAFE_DIRECTORY: &str = "safe.directory";
/// The largest count of settings git takes from the environment (its `INT_MAX`).
pub const GIT_CONFIG_COUNT_MAX: usize = 2_147_483_647;

/// The folders of a run that git is told are safe to work in, as values of `safe.directory`: the run's repository, what is inside
/// it, and what is inside the run's temp folder. A folder that is no absolute path gives no value (git warns about one).
///
/// Why they are needed. git works in a repository only when its folders belong to the user it runs as. The checkout is on the session
/// storage, and agentd gives the checkout and its `.git` to the engine's user by name, so the repository itself passes. What the engine's
/// user makes on that storage later can be reported as root's (measured for a file the engine's own test run wrote there). The review
/// round asks for a scratch worktree under the temp folder (`git worktree add --detach "$(mktemp -d)/base" BASE`), and git refused every
/// command in the scratch copy (`detected dubious ownership`): the worktree's folder inside the repository's `.git`
/// (`.git/worktrees/<name>`) is made on that storage by the engine's user, and with that one folder root's git refuses in the same
/// words. A submodule and a repository the project's tests make inside the checkout can meet the same mismatch.
///
/// Why these forms. git compares the setting with the work tree of the repository it is about to use. A scratch worktree has a name of
/// its own, so the temp folder needs git's leading-path form (`<folder>/*`, git 2.46 and newer; the worker image has a newer one). The
/// repository is named itself and with the same form for what is inside it. Nothing else is named and the wildcard `*` is never used: a
/// repository anywhere else on the machine is checked as before. Every folder named here is the run's own: only the engine's user, and
/// agentd for it, write there. For a repository at one of these paths git's owner check no longer decides; nothing else about a
/// repository is checked or changed. agentd's own git calls do not read the settings (they carry their own guards and a cleaned
/// environment), and no ownership or mode is changed.
pub fn git_safe_directories(repo: &Path, tmp: &Path) -> Vec<String> {
    let mut values = Vec::new();
    if repo.is_absolute() {
        values.push(repo.display().to_string());
        values.push(format!("{}/*", repo.display()));
    }
    if tmp.is_absolute() {
        values.push(format!("{}/*", tmp.display()));
    }
    values
}

/// Adds [`git_safe_directories`] to an engine's environment in git's own variables for settings (`GIT_CONFIG_COUNT` with the pairs
/// `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`), which git reads like `-c` on its command line in every git command that keeps the
/// engine's environment. Two cases do not read them. A command that sets `GIT_CONFIG_COUNT` itself replaces the sequence for that
/// command. And the process that serves a local clone, fetch or `ls-remote`: git starts it without these variables. In git 2.47.3
/// that process refuses a source whose folders show another owner. In git 2.50.1 (the git of the worker image's base image pin) it
/// serves such a source without the owner check, and `git clone <path>` checks the source in the clone process, which keeps the
/// variables: a clone of a scratch worktree passes there through `<repo>/*`.
/// A sequence that is already in the environment stays and the values go after it. The count is read as git reads it: white space before
/// the digits is skipped, and git refuses anything else that is no number and a count above its bound ([`GIT_CONFIG_COUNT_MAX`]). A count
/// git would refuse, also with the values added, is left as it is with nothing added: git then refuses every command of that environment
/// in its own words. No file is written: the user's git settings in the rules, in the repository and in the home folder stay as they
/// are, and git adds these values to theirs.
pub fn add_git_safe_directories(env: &mut BTreeMap<String, String>, repo: &Path, tmp: &Path) {
    let within_bound = |n: &usize| *n <= GIT_CONFIG_COUNT_MAX;
    let first = match env.get(GIT_CONFIG_COUNT).map(String::as_str) {
        None | Some("") => 0,
        Some(count) => {
            let Some(n) = count
                .trim_start()
                .parse::<usize>()
                .ok()
                .filter(within_bound)
            else {
                return;
            };
            n
        }
    };
    let values = git_safe_directories(repo, tmp);
    let Some(count) = first.checked_add(values.len()).filter(within_bound) else {
        return;
    };
    for (n, value) in (first..count).zip(&values) {
        env.insert(format!("{GIT_CONFIG_KEY}{n}"), SAFE_DIRECTORY.to_string());
        env.insert(format!("{GIT_CONFIG_VALUE}{n}"), value.clone());
    }
    if count > first {
        env.insert(GIT_CONFIG_COUNT.to_string(), count.to_string());
    }
}

/// What [`standalone`] builds a launch from.
#[derive(Clone, Copy, Debug)]
pub struct Standalone<'a> {
    pub engine: &'a str,
    /// The folder of the engine profiles (`engine/profiles`). Templates and fixtures are read from the folder two levels above it, the root the
    /// profile's own paths start from.
    pub profiles_dir: &'a Path,
    /// A folder of the caller's own; `data`, `config`, `home`, `tmp` and `repo` are made in it.
    pub root: &'a Path,
    pub model: &'a str,
    pub region: &'a str,
    /// `commands.timeout_seconds` as a user would set it; `None` is a run with no engine settings.
    pub command_timeout_s: Option<u32>,
    /// A stand-in knob the product never sets: the model endpoint of a local stand-in.
    pub endpoint: Option<&'a str>,
    /// A stand-in knob the product never sets: how long the engine keeps a folder's services without session activity.
    pub idle_timeout: Option<&'a str>,
}

/// A whole engine launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// The files the emitter wrote.
    pub files: Vec<PathBuf>,
    /// The line the engine writes to its error log when the launch's command time limit is in force; `None` for a launch that has no such sign.
    pub ready_line: Option<String>,
}

/// The output-token cap and the context window of the stand-in model: a direct run takes both from the request's model.
const STAND_IN_LIMITS: (u32, u32) = (64_000, 1_000_000);

/// A whole engine launch in `root`, built from the calls a direct run makes, in its order, without the run's modules, proxy, cargo and sandbox:
/// the layout, the emitter, the profile's launch, the direct run's arguments, [`base_env`], the locked settings, the root variable, the kill
/// switch check, the command time limit read-back and the engine's last word on its environment (no tool server). It names the `agent` profile
/// in the environment and writes no AWS profile: the caller puts its own stand-in credentials file at the path `AWS_CONFIG_FILE` names. No network,
/// no AWS call.
pub fn standalone(s: &Standalone<'_>) -> Result<Launch, String> {
    let (data_dir, config_dir, home, tmp, repo) = (
        s.root.join("data"),
        s.root.join("config"),
        s.root.join("home"),
        s.root.join("tmp"),
        s.root.join("repo"),
    );
    for dir in [&data_dir, &config_dir, &home, &tmp, &repo] {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let profile = load_profile(&s.profiles_dir.join(format!("{}.toml", s.engine)))
        .map_err(|e| format!("engine profile: {e}"))?;
    let templates_root = s
        .profiles_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(s.profiles_dir);
    let layout = config_layout(s.engine, &config_dir);
    layout
        .prepare()
        .map_err(|e| format!("engine config root: {e}"))?;
    let command_timeout = match s.command_timeout_s {
        Some(n) => Some(CommandSeconds::new(n).ok_or_else(|| {
            format!(
                "the command time limit must be {} to {} seconds",
                CommandSeconds::MIN,
                CommandSeconds::MAX
            )
        })?),
        None => None,
    };
    let settings = UserSettings {
        command_timeout,
        ..UserSettings::default()
    };
    let ectx = EmitContext {
        engine: s.engine.to_string(),
        model: s.model.to_string(),
        region: s.region.to_string(),
        data_dir: data_dir.clone(),
        config_dir: config_dir.clone(),
        agentd_exe: PathBuf::from("/opt/harness/bin/agentd"),
        socket: s.root.join("creds.sock"),
        max_output_tokens: Some(STAND_IN_LIMITS.0),
        context_tokens: Some(STAND_IN_LIMITS.1),
        compaction: None,
        command_limit_ms: run_limit(s.engine, &settings).map(CommandSeconds::millis),
    };
    let built_in =
        emit_engine_for(&ectx, &profile.version).map_err(|e| format!("engine config: {e}"))?;
    let vars: BTreeMap<&str, String> = [
        ("workspace", repo.display().to_string()),
        ("dataDir", data_dir.display().to_string()),
        ("configDir", config_dir.display().to_string()),
        ("model", s.model.to_string()),
        ("region", s.region.to_string()),
        (
            "fixtures",
            fixtures_dir(templates_root, &profile.name)
                .display()
                .to_string(),
        ),
    ]
    .into();
    emit_profile(&profile, templates_root, &vars)
        .map_err(|e| format!("profile config emitters: {e}"))?;
    let launch = resolve_launch(&profile, Transport::Acp, &vars)
        .map_err(|e| format!("engine launch: {e}"))?;
    let argv = direct_launch_argv(s.engine, launch.argv.clone(), Effort::Default);
    let files = built_in.files.clone();
    let mut env = base_env(BaseEnv {
        home: &home,
        tmp: &tmp,
        repo: &repo,
        engine: s.engine,
        profile_env: &launch.env,
        built_in: built_in.env,
        settings: &settings,
        aws: aws_env(&ectx, &config_dir.join("aws/config")),
        env_remove: &launch.env_remove,
    });
    if let Some(k) = &launch.model_env {
        env.insert(k.clone(), acp_model_value(s.engine, s.model));
    }
    if s.endpoint.is_some() || s.idle_timeout.is_some() {
        super::config::stand_in(s.engine, &mut env, s.endpoint, s.idle_timeout)
            .map_err(|e| format!("stand-in knobs: {e}"))?;
    }
    let locked = lock_settings(s.engine, &layout, &[], &mut env)
        .map_err(|e| format!("engine settings: {e}"))?;
    let (root_var, root_value) = config_root_env(&layout);
    env.insert(root_var.to_string(), root_value);
    check_kill_switches(s.engine, &env, locked.as_deref())?;
    check_command_limit(s.engine, &config_dir, &env, &settings)?;
    super::config::finalize_env(
        s.engine,
        &profile.version,
        profile.mcp.as_ref(),
        templates_root,
        &[],
        &mut env,
    )
    .map_err(|e| format!("engine environment: {e}"))?;
    let ready_line = super::config::ready_line(s.engine, &env);
    Ok(Launch {
        argv,
        env,
        cwd: repo,
        files,
        ready_line,
    })
}
