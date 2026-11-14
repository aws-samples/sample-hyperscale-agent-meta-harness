//! The command line: `ham` alone opens the TUI; `ham "<prompt>"... --direct`
//! starts a run and watches it; `ham watch`, `ham pull`, `ham clone`, `ham link`, `ham runs`,
//! `ham doctor` and the commands of `crate::commands` (`cancel`, `pr`). On a terminal
//! without `--json` a watched run opens in the TUI dashboard; otherwise output is lines (plain or
//! JSON). Exit codes: `ham_core::exit` (run states for a watched run, 1 errors, 2 usage).
//! Local state is the user's own files (`config.toml`, the rules and skills directories, `continue.md`) and downloads (invariant 5);
//! errors name the failing call and never carry a request body,
//! a presigned query or a credential.
//!
//! The commands that are not a stream or a start ([`execute`]) write to an [`Output`] and return their exit code, so the shell and the
//! chat box run the same code: the shell with [`Output::live`], the box with [`run_captured`], which gives back the text as data.
//! `watch` and a new run stay with the shell: they own the terminal until they end.

use crate::api::{Api, ApiError};
use crate::clone::{Git, RunRequest, clone_run};
use crate::commands::{self, AnswerOptions};
use crate::config_cmd;
use crate::http::{ControlPlane, redact_url};
use crate::https::HttpsPlane;
use crate::output::{Output, Stream};
use crate::pull::{card, pull_with, range_bytes, select};
use crate::push::{PushError, PushRequest, push_run};
use crate::sigv4::Signer;
use crate::start::{launch_nonce, start_direct};
use crate::tui::{App, DirectSettings};
use crate::watch::{WatchOptions, exit_of, pulled_json, watch};
use ham_core::command::{Command, DirectArgs, Invocation, PullArtifact, TargetArg, parse};
use ham_core::config::{Config, download_dir};
use ham_core::exit::{ERROR, USAGE, exit_code};
use ham_core::render::{FinalCard, final_card};
use ham_core::request::resolve_run_settings;
use ham_core::sanitize::sanitize_line;
use ham_core::state::RunState;
use harness_protocol::api::v1::{RunSummary, RunTarget};
use serde_json::json;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Polling interval for the run in focus.
pub const FOCUSED_POLL: Duration = Duration::from_secs(1);

type CliApi = Api<crate::backends::Backends<HttpsPlane>>;

fn fail(msg: impl std::fmt::Display) -> i32 {
    eprintln!("ham: {msg}");
    ERROR
}

/// [`fail`] onto an [`Output`].
fn fail_out(out: &Output, msg: impl std::fmt::Display) -> i32 {
    out.err_line(format!("ham: {msg}"));
    ERROR
}

/// A command that cannot run in this place: the usage exit code and the reason on standard error.
fn not_here(out: &Output, msg: &str) -> i32 {
    out.err_line(format!("usage: {msg}"));
    USAGE
}

/// The process environment, through the fallback to the earlier names of the client's variables (`ham_core::envnames`).
fn env(k: &str) -> Option<String> {
    ham_core::envnames::os_env(k)
}

fn interactive() -> bool {
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// The config file (absent is empty), its path for messages. A file with a bad key is refused with the line of each problem.
fn load_config() -> Result<(Config, String), String> {
    config_cmd::load(&env).map(|(cfg, path)| (cfg, path.display().to_string()))
}

/// The config for `ham doctor`: the keys that could be read from a file with a bad key, so the checks that need the stage still run and
/// the bad lines are listed with them. A file that leaves nothing to connect with (not TOML, too large, no readable `apiEndpoint`) is the
/// whole report: the lines of what is wrong, and exit 1 (`Err` holds that exit code).
fn doctor_config(json: bool) -> Result<(Config, String), i32> {
    let loaded = config_cmd::load_file(&env).map_err(fail)?;
    let shown = loaded.shown();
    let usable = loaded.refusal().is_none()
        && (loaded.read.findings.is_empty() || loaded.read.config.api_endpoint.is_some());
    if usable {
        return Ok((loaded.read.config, shown));
    }
    let report = crate::doctor::Doctor {
        checks: crate::doctor::config_checks(&env),
        allowlist: Vec::new(),
    };
    Err(crate::doctor::report(&report, json, &Output::live()))
}

/// The client's backends from the config: `default` from the three top-level keys and one more for each `[[backend]]` table. A config with
/// no table has the one backend it always had, and a failure to build it ends the start with the words it always had; with several, a backend
/// that cannot be built is a `Down` entry and the others work.
async fn connect(cfg: &Config, shown: &str) -> Result<CliApi, String> {
    if let Some(problem) = ham_core::backends::check_backends(cfg).into_iter().next() {
        return Err(format!("{problem}. Your config file is {shown}."));
    }
    let defs = ham_core::backends::backend_defs(cfg);
    if let [only] = defs.as_slice()
        && only.api_endpoint.as_deref().is_none_or(str::is_empty)
    {
        return Err(format!(
            "No stage URL is set. You set api_endpoint to your stage URL with `ham config set api_endpoint <url>`. Your stage URL ends in /direct. Your config file is {shown}."
        ));
    }
    let backends = crate::backends::Backends::connect(
        defs,
        cfg.default_backend.clone(),
        crate::backends::https_factory(),
    )
    .await?;
    backends.install_title_version();
    Ok(Api::new(Arc::new(backends)))
}

/// The one-stage client of the top-level keys, for `ham-admin`, which administers a single stage.
async fn connect_single(cfg: &Config, shown: &str) -> Result<Api<HttpsPlane>, String> {
    let Some(endpoint) = cfg.api_endpoint.as_deref().filter(|e| !e.is_empty()) else {
        return Err(format!(
            "No stage URL is set. You set api_endpoint to your stage URL with `ham config set api_endpoint <url>`. Your stage URL ends in /direct. Your config file is {shown}."
        ));
    };
    let signer = Signer::from_profile(cfg.aws_profile.as_deref(), cfg.region.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    let plane = HttpsPlane::new(endpoint, signer).map_err(|e| format!("apiEndpoint: {e}"))?;
    Ok(Api::new(Arc::new(plane)))
}

fn downloads(cfg: &Config) -> PathBuf {
    download_dir(&env, cfg)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn nonce() -> Result<String, String> {
    launch_nonce(env("HAMH_IDEMPOTENCY_NONCE").as_deref())
}

/// The name the program had before the rename. `setup.sh` makes a link of this name to `ham` for the window of the earlier name.
pub const EARLIER_COMMAND: &str = "cah"; // COMPAT-CAH

/// Whether a program started as `program` (the first element of its arguments, a path or a bare name) is started under its earlier name: the file
/// name is [`EARLIER_COMMAND`]. The name only adds one notice; the program behaves as `ham` does. The test driver binary asks it too.
pub fn started_under_earlier_name(program: Option<&str>) -> bool {
    program
        .and_then(|p| Path::new(p).file_name())
        .is_some_and(|n| n == EARLIER_COMMAND)
}

/// The notice made when the program is started under its earlier name.
const ALIAS_NOTICE: &str =
    "cah is the old name of ham and will stop working in a later release. Use ham."; // COMPAT-CAH

/// COMPAT-CAH: the notices of the window of the earlier names, once, on standard error, before anything is drawn: the program started as `cah`,
/// each earlier variable name that is set, and the folder of the earlier name when it is the one read. `HAMH_NO_DEPRECATION_NOTICES` and the
/// setting `ui.deprecation_notices` silence all of them. The window ends at VERSION 5.50.
pub fn announce_earlier_names(started_under_earlier_name: bool, args: &[String]) {
    let raw = |k: &str| std::env::var(k).ok();
    let mut lines: Vec<String> = Vec::new();
    if started_under_earlier_name {
        lines.push(ALIAS_NOTICE.to_string());
    }
    lines.extend(ham_core::envnames::notices(&raw));
    // `ham config migrate` is the remedy the folder notice names: it needs no notice to point at itself.
    let migrating = args
        .windows(2)
        .any(|w| w[0] == "config" && w[1] == "migrate");
    if !migrating && let Some(line) = ham_core::config_dir::read_notice(&raw) {
        lines.push(line);
    }
    if lines.is_empty() || ham_core::envnames::silenced_by_env(&raw) {
        return;
    }
    // The setting is read from the file the folder rule picks; a file that cannot be read leaves the notices on.
    if let Ok(loaded) = config_cmd::load_file(&env)
        && !ham_core::config::deprecation_notices_on(&raw, &loaded.read.config)
    {
        return;
    }
    for line in lines {
        eprintln!("{line}");
    }
}

/// Runs `ham` with `args` (without the program name) and returns the exit code.
pub async fn main_with(args: &[String]) -> i32 {
    // `ham --version` and `ham -V`, by themselves: the client's own version, answered before any configuration, credential or network is read
    // (the notices of the window of the earlier names come first and read one key of the config file, only when there is a notice to print).
    if let [flag] = args
        && VERSION_FLAGS.contains(&flag.as_str())
    {
        Output::live().out_line(version_line());
        return 0;
    }
    // `ham --help`, `ham -h`, `ham help [<command>]` and `ham <command> --help`: the usage, answered before any configuration, credential or
    // network is read, like the version.
    if let Some(help) = ham_core::help::request(args) {
        return answer_help(&help, &Output::live());
    }
    // `--backend <alias>` and `--customer-content` stand outside the command grammar and are taken out first.
    let (flags, stripped) = match ham_core::backends::take_flags(args) {
        Ok(taken) => taken,
        Err(e) => {
            eprintln!("usage: {e}");
            return USAGE;
        }
    };
    let args = stripped.as_slice();
    if args.is_empty() && interactive() {
        return tui_session(flags.backend).await;
    }
    let inv = match parse(args) {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("{e}");
            return USAGE;
        }
    };
    // The rules commands, `ham config` and `ham models` read and write local files and the compiled-in model list only: no config
    // file, network or credentials.
    if let Some(code) = execute_local(&inv, &env, &Output::live()) {
        return code;
    }
    // `ham doctor` runs on a file with a bad key and lists what is wrong with it; every other command that needs the stage refuses the file.
    let loaded = if matches!(inv.command, Command::Doctor { .. }) {
        match doctor_config(inv.json) {
            Ok(c) => c,
            Err(code) => return code,
        }
    } else {
        match load_config() {
            Ok(c) => c,
            Err(e) => return fail(e),
        }
    };
    let (cfg, shown) = loaded;
    // `ham update`: the off switch and the refusals that need no network are answered before any connection is made.
    if matches!(inv.command, Command::Update { .. }) {
        match crate::update::source_of(&cfg) {
            Ok(ham_core::update::Source::Off) => {
                eprintln!("ham update: {}", crate::update::off_text(&shown));
                return crate::update::exit::OFF;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("usage: {e}");
                return USAGE;
            }
        }
        let check = matches!(inv.command, Command::Update { check: true, .. });
        if let Some(why) = crate::update::backend_refusal(&cfg, check) {
            eprintln!("ham update: {why}");
            return crate::update::exit::FAILED;
        }
    }
    let chunk = match range_bytes(env("HAMH_RANGE_BYTES").as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("usage: {e}");
            return USAGE;
        }
    };
    let api = match connect(&cfg, &shown).await {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    // Which backend the command is aimed at, and the customer-content check of a start.
    let kind = match &inv.command {
        Command::StartDirect(_) => crate::backends::Aim::Start,
        Command::Continue(a, _) => a
            .continue_from
            .as_deref()
            .map_or(crate::backends::Aim::Start, crate::backends::Aim::Continue),
        _ => crate::backends::Aim::Other,
    };
    let api =
        match crate::backends::aim(&api, flags.backend.as_deref(), flags.customer_content, kind)
            .await
        {
            Ok(a) => a,
            Err(e) => return fail(e),
        };
    let dir = downloads(&cfg);
    dispatch(inv, &api, &cfg, dir, chunk).await
}

/// The flags that print the client's version when they are the only argument.
pub const VERSION_FLAGS: [&str; 2] = ["--version", "-V"];

/// The flags that ask for help (`ham_core::help::HELP_FLAGS`, which `tests/help_cli.rs` holds equal).
pub const HELP_FLAGS: [&str; 2] = ["--help", "-h"];

/// Prints the help a request asks for on standard output and returns 0; for `ham help <word>` with a word that is no command, the usage error on
/// standard error and 2.
fn answer_help(help: &ham_core::help::HelpRequest, out: &Output) -> i32 {
    use ham_core::help::{HelpRequest, command_text, top_text, unknown_text};
    match help {
        HelpRequest::Top => out.out_line(top_text().trim_end()),
        HelpRequest::Command(name) => {
            out.out_line(command_text(name).unwrap_or_default().trim_end())
        }
        HelpRequest::Unknown(word) => {
            out.err_line(format!("usage: {}", unknown_text(word)));
            return USAGE;
        }
    }
    0
}

/// What `ham --version` prints: `ham v4.01`.
pub fn version_line() -> String {
    format!("ham v{}", crate::version::CLIENT)
}

/// How a command reads the environment (`HOME`, `XDG_CONFIG_HOME`, `VISUAL`, `HAMH_*`).
pub type EnvFn<'a> = &'a (dyn Fn(&str) -> Option<String> + Send + Sync);

/// Where a message given as `-` comes from; `None` means there is no standard input to read (the chat box).
pub type StdinFn<'a> = &'a (dyn Fn() -> Result<String, String> + Send + Sync);

/// What a command needs besides the invocation.
pub struct ExecContext<'a, P: ControlPlane> {
    pub api: &'a Api<P>,
    pub cfg: &'a Config,
    /// Where `pull`, `clone` and `push` put the zip.
    pub downloads: &'a Path,
    /// The size of one download range (`HAMH_RANGE_BYTES`; 16 MiB by default).
    pub chunk: u64,
    pub env: EnvFn<'a>,
    pub stdin: Option<StdinFn<'a>>,
}

impl<P: ControlPlane> std::fmt::Debug for ExecContext<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecContext")
            .field("downloads", &self.downloads)
            .field("chunk", &self.chunk)
            .finish_non_exhaustive()
    }
}

/// The text of an answer: as typed, or what `stdin` reads (to its end, without the final line break) when the argument is `-`.
fn read_text(arg: &str, stdin: Option<StdinFn<'_>>) -> Result<String, String> {
    if arg != "-" {
        return Ok(arg.to_string());
    }
    let Some(read) = stdin else {
        return Err("a message of - is read from standard input, which is not available here; type the message itself".into());
    };
    read().map(|t| t.trim_end_matches(['\n', '\r']).to_string())
}

/// The message of `ham say -`: standard input, to its end.
fn read_stdin() -> Result<String, String> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
        .map_err(|e| format!("reading the message from standard input: {e}"))?;
    Ok(text)
}

/// `ham rules`, `ham config` and `ham models`: they need no network, no config file and no credentials. `None` for any other command.
fn execute_local(inv: &Invocation, env: EnvFn<'_>, out: &Output) -> Option<i32> {
    // COMPAT-CAH: a command that writes the user's files is refused, before an editor opens, while only the folder of the earlier command name
    // holds them; `ham config migrate` is the way out.
    if inv.command.writes_config_folder()
        && let Some(text) = ham_core::config_dir::write_refusal(env)
    {
        out.err_line(format!("ham: {text}"));
        return Some(USAGE);
    }
    let json = inv.json;
    let (mut o, mut e) = (out.writer(Stream::Out), out.writer(Stream::Err));
    Some(match &inv.command {
        Command::Rules(r) => crate::rules_cmd::run(r, env, json, &mut o, &mut e),
        Command::Lifecycle(l) => crate::lifecycle_cmd::run(l, json, &mut o, &mut e),
        Command::Config(c) => config_cmd::run_config(c, env, json, &mut o, &mut e),
        Command::Engine(c) => crate::engine_cmd::run(c, env, json, &mut o, &mut e),
        Command::Models => config_cmd::run_models(env, json, &mut o, &mut e),
        _ => return None,
    })
}

/// Runs one command and writes what it prints to `out`; returns the exit code. The same code runs for the shell (`out` is
/// [`Output::live`]) and for the chat box ([`run_captured`]). `watch` and a new run are not run here: `watch` follows a run until
/// it ends (the box opens the run instead) and a start is billable (the box opens the New run form). `rules edit` and the git
/// prompts of `push` use the terminal: the caller must have handed it over.
pub async fn execute<P: ControlPlane>(
    inv: &Invocation,
    ctx: &ExecContext<'_, P>,
    out: &Output,
) -> i32 {
    if let Some(code) = execute_local(inv, ctx.env, out) {
        return code;
    }
    let (api, json) = (ctx.api, inv.json);
    match &inv.command {
        Command::StartDirect(_) => not_here(
            out,
            "a new run starts from the New run form after you confirm it, never from a typed line",
        ),
        Command::Watch { .. } => not_here(
            out,
            "watch follows a run until it ends; the terminal app opens the run instead",
        ),
        Command::Continue(..) => not_here(
            out,
            "a follow-up run is billable and starts from a shell: ham continue <runId> '<prompt>'",
        ),
        Command::Update { .. } => not_here(
            out,
            "ham update runs from a shell: it waits for your runs and hands the terminal to setup",
        ),
        // Answered by `execute_local` above.
        Command::Rules(_)
        | Command::Lifecycle(_)
        | Command::Config(_)
        | Command::Engine(_)
        | Command::Models => 0,
        Command::Pull { run_id, artifact } if *artifact == PullArtifact::Snapshot => {
            crate::snapshot_cmd::pull_snapshot(api, run_id, json, ctx.downloads, out).await
        }
        Command::Pull { run_id, artifact } => {
            cmd_pull(api, run_id, artifact, json, ctx.downloads, ctx.chunk, out).await
        }
        Command::Clone { run_id, dir } => {
            cmd_clone(
                api,
                run_id,
                dir.as_deref(),
                json,
                ctx.downloads,
                ctx.chunk,
                out,
            )
            .await
        }
        Command::Push {
            run_id,
            url,
            branch,
        } => {
            cmd_push(
                api,
                run_id,
                url,
                branch.as_deref(),
                json,
                ctx.downloads,
                ctx.chunk,
                out,
            )
            .await
        }
        Command::Link { run_id } => cmd_link(api, run_id, json, out).await,
        Command::Resume {
            run_id,
            all,
            dry_run,
        } => {
            let sharing_on = sharing_setting(ctx.cfg);
            cmd_resume(
                api,
                run_id.as_deref(),
                *all,
                *dry_run,
                sharing_on,
                json,
                out,
            )
            .await
        }
        Command::Runs { probes } => cmd_runs(api, *probes, json, out).await,
        Command::Cancel { run_id } => commands::cmd_cancel(api, run_id, json, out).await,
        Command::Say {
            run_id,
            text,
            request_id,
            no_wait,
        } => {
            let opts = AnswerOptions {
                request_id: request_id.clone(),
                no_wait: *no_wait,
                ..AnswerOptions::default()
            };
            match read_text(text, ctx.stdin) {
                Ok(t) => commands::cmd_say(api, run_id, &t, &opts, json, out).await,
                Err(e) => fail_out(out, e),
            }
        }
        Command::NewSession {
            run_id,
            text,
            request_id,
            no_wait,
        } => {
            let opts = AnswerOptions {
                request_id: request_id.clone(),
                no_wait: *no_wait,
                ..AnswerOptions::default()
            };
            let text = match text.as_deref().map(|t| read_text(t, ctx.stdin)).transpose() {
                Ok(t) => t,
                Err(e) => return fail_out(out, e),
            };
            commands::cmd_new_session(api, run_id, text.as_deref(), &opts, json, out).await
        }
        Command::TryModel {
            run_id,
            model,
            text,
            request_id,
            no_wait,
        } => {
            let opts = AnswerOptions {
                request_id: request_id.clone(),
                no_wait: *no_wait,
                ..AnswerOptions::default()
            };
            let text = match text.as_deref().map(|t| read_text(t, ctx.stdin)).transpose() {
                Ok(t) => t,
                Err(e) => return fail_out(out, e),
            };
            commands::cmd_try_model(api, run_id, model, text.as_deref(), &opts, json, out).await
        }
        Command::Finish {
            run_id,
            request_id,
            no_wait,
        } => {
            let opts = AnswerOptions {
                request_id: request_id.clone(),
                no_wait: *no_wait,
                ..AnswerOptions::default()
            };
            commands::cmd_finish(api, run_id, &opts, json, out).await
        }
        Command::ModelsServer => cmd_models_server(api, ctx.cfg, ctx.env, json, out).await,
        Command::Pr { run_id } => commands::cmd_pr(api, run_id, json, out).await,
        Command::Doctor {
            catalog_max_age_hours,
        } => {
            let endpoint = ctx.cfg.api_endpoint.clone().unwrap_or_default();
            let profile = ctx
                .cfg
                .aws_profile
                .clone()
                .unwrap_or_else(|| "default".into());
            let now = crate::doctor::unix_now();
            let mut checks = match (api.plane().backend_view(), api.target()) {
                // Several backends: the checks of each, every line led by its alias, and a versions line of its own.
                (Some(view), None) => {
                    let defs = ham_core::backends::backend_defs(ctx.cfg);
                    let targets: Vec<crate::doctor::DoctorTarget> = view
                        .backends
                        .iter()
                        .map(|info| {
                            let def = defs.iter().find(|d| d.alias == info.alias);
                            crate::doctor::DoctorTarget {
                                alias: info.alias.clone(),
                                endpoint: def
                                    .and_then(|d| d.api_endpoint.clone())
                                    .unwrap_or_default(),
                                profile: def
                                    .and_then(|d| d.aws_profile.clone())
                                    .unwrap_or_else(|| "default".into()),
                            }
                        })
                        .collect();
                    crate::doctor::checks_many(
                        api,
                        &targets,
                        *catalog_max_age_hours,
                        now,
                        crate::doctor::DOCTOR_DEADLINE,
                    )
                    .await
                }
                _ => {
                    crate::doctor::checks_at(api, &endpoint, &profile, *catalog_max_age_hours, now)
                        .await
                }
            };
            // Provider data sharing: the setting and the account's Bedrock data retention mode, one line for each backend.
            let sharing_on = sharing_setting(ctx.cfg);
            match (api.plane().backend_view(), api.target()) {
                (Some(view), None) => {
                    for info in &view.backends {
                        if let Some(mut c) =
                            crate::doctor::sharing_check(&api.on(&info.alias), sharing_on).await
                        {
                            c.detail = format!("{}: {}", info.alias, c.detail);
                            checks.checks.push(c);
                        }
                    }
                }
                _ => {
                    if let Some(c) = crate::doctor::sharing_check(api, sharing_on).await {
                        checks.checks.push(c);
                    }
                }
            }
            // What is wrong with the config file comes first; a healthy file adds no line.
            let mut lines = crate::doctor::config_checks(ctx.env);
            lines.append(&mut checks.checks);
            checks.checks = lines;
            crate::doctor::report(&checks, json, out)
        }
    }
}

/// [`execute`] with the text kept as data: what the chat box shows in its result panel.
pub async fn run_captured<P: ControlPlane>(
    inv: &Invocation,
    ctx: &ExecContext<'_, P>,
) -> crate::output::CmdOutput {
    let out = Output::capture();
    let code = execute(inv, ctx, &out).await;
    out.finish(code)
}

/// The shell: `watch` and a new run keep the terminal and print through their own writers; every other command is [`execute`] with
/// a live output.
async fn dispatch(inv: Invocation, api: &CliApi, cfg: &Config, dir: PathBuf, chunk: u64) -> i32 {
    let json = inv.json;
    if let Command::Update {
        check,
        yes,
        no_wait,
        wait_minutes,
    } = &inv.command
    {
        return cmd_update(api, cfg, dir, *check, *yes, json, *no_wait, *wait_minutes).await;
    }
    match &inv.command {
        Command::StartDirect(args) => cmd_start_direct(api, args, json, dir, cfg, None).await,
        Command::Continue(args, explicit) => {
            cmd_continue(api, args, explicit.as_ref(), json, dir, cfg).await
        }
        Command::Watch { run_id } => {
            let run_id = match run_id {
                Some(r) => r.clone(),
                None if api.target().is_none() && api.plane().backend_view().is_some() => {
                    return fail(
                        "with several backends, name the run to watch, or the backend with --backend <alias>",
                    );
                }
                None => match api.list_runs().await {
                    // The newest of the user's runs: a probe run is not one.
                    Ok(runs) => match runs.into_iter().find(|r| !r.is_probe()) {
                        Some(r) => r.run_id,
                        None => return fail("no runs to watch"),
                    },
                    Err(e) => return fail(e),
                },
            };
            watch_run(api, &run_id, json, dir, cfg).await
        }
        _ => {
            let ctx = ExecContext {
                api,
                cfg,
                downloads: &dir,
                chunk,
                env: &env,
                stdin: Some(&read_stdin),
            };
            execute(&inv, &ctx, &Output::live()).await
        }
    }
}

/// `ham "<prompt>" --direct --model <id>`: the settings are resolved and the rules, the nudge and the skills are read from
/// disk first, so a bad setting or file stops the run before anything is sent; then one `POST /v1/direct/runs` and the same watch as any run.
async fn cmd_start_direct(
    api: &CliApi,
    args: &DirectArgs,
    json: bool,
    dir: PathBuf,
    cfg: &Config,
    follows: Option<&FollowNote>,
) -> i32 {
    // The settings first, in the order flag, environment, file, default: a model or a wait that names nothing stops the start before the
    // rules are read or anything is sent. A terminal on both ends and no --json shows the dashboard, so the wait defaults to 30 minutes
    // there and to none for any other client (design decision B).
    let resolved = match resolve_run_settings(cfg, &env, args, interactive() && !json) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let chosen = (
        resolved.model.id.clone(),
        resolved.model.name.clone(),
        resolved.model.source,
    );
    // The rules, the nudge, the skills and the files `--attach` names are read now, so a file that cannot be sent stops the
    // start before anything is.
    let inputs = match crate::rules_cmd::direct_inputs_for(&env, &resolved.args) {
        Ok(i) => i,
        // A file the user has to correct before a run can start: the usage exit code, and nothing was sent.
        Err(e @ crate::rules_cmd::InputsError::EngineFile(_)) => {
            eprintln!("ham: {e}");
            return USAGE;
        }
        Err(e) => return fail(e),
    };
    let request = match resolved.request(inputs) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let nonce = match nonce() {
        Ok(n) => n,
        Err(e) => return fail(e),
    };
    // Provider data sharing in the account: `run.provider_sharing` on, or `--provider-sharing on`, is the user's agreement, so a mode that does
    // not let the model run is switched now, without a question, and said in one line; a switch that fails stops the start before anything is sent.
    // The start goes through the client the check aimed: the account that was read and switched is the one of the backend the run starts on.
    let cleared =
        match crate::sharing::preflight(api, &request, crate::sharing::agreement_of(&resolved))
            .await
        {
            Ok(c) => c,
            Err(stop) => return fail(stop),
        };
    if let Some(line) = &cleared.note {
        eprintln!("{line}");
    }
    let started = match start_direct(&cleared.api, &request, &nonce).await {
        Ok(s) => s,
        // A stage that is being updated says so in one sentence, which is shown alone
        Err(e) if crate::api::stage_updating(&e) => return fail(crate::api::start_failure(&e)),
        Err(e) => return fail(format!("POST /v1/direct/runs: {e}")),
    };
    if json {
        let mut start = json!({"runId": started.run_id, "status": started.status, "idempotencyKey": started.idempotency_key,
                   "model": chosen.0, "modelSource": chosen.2.label()});
        // The engine is named only when the user chose a second one: the line of a run on the default engine is the line it always was.
        if let Some(engine) = resolved.engine_member() {
            start["engine"] = json!(engine);
        }
        // A follow-up says which target the new run has and where that came from.
        if let Some(note) = follows {
            start["target"] = note.target.clone();
            start["targetSource"] = json!(note.source);
        }
        println!("{start}");
    } else {
        println!("started run {}", started.run_id);
        eprintln!(
            "model: {} ({}), from {}",
            chosen.1,
            chosen.0,
            chosen.2.label()
        );
    }
    let _ = std::io::stdout().flush();
    watch_run(api, &started.run_id, json, dir, cfg).await
}

/// What `ham continue` adds to the start line of `--json`: the target of the new run as the service stores it (`{"mode":"repo","repo":"cc:calc"}`
/// or `{"mode":"s3-only"}`) and where it came from (`run` or `flag`).
struct FollowNote {
    target: serde_json::Value,
    source: &'static str,
}

/// The words of the usage error when no text was given and nothing can ask for it.
const CONTINUE_NEEDS: &str =
    "ham continue needs the prompt for the coding agent: ham continue <runId> '<prompt>'";

/// `ham continue <runId> [<prompt>] [--repo cc:<name> | --s3-only]`: a new run that starts from the code a finished run
/// delivered. The prompt is the argument; with none, a terminal opens the editor on an empty draft, and any other place is a usage error
/// before anything is sent. The target is the one on the line, else the one the source run had: the client reads the run for it before the
/// editor opens, and a run it cannot read, or whose target the service does not continue, stops the command there (it never falls back to a
/// zip alone). control-api checks that the run is the caller's and finished, resolves the head it delivered and stores it; the rules and
/// skills travel as for any run.
async fn cmd_continue(
    api: &CliApi,
    args: &DirectArgs,
    explicit: Option<&TargetArg>,
    json: bool,
    dir: PathBuf,
    cfg: &Config,
) -> i32 {
    let mut args = args.clone();
    let needs_editor = args.prompt.trim().is_empty();
    if needs_editor && !crate::tty::can_ask() {
        eprintln!("usage: {CONTINUE_NEEDS}");
        return USAGE;
    }
    let source = args.continue_from.clone().unwrap_or_default();
    args.target = match explicit {
        Some(t) => t.clone(),
        None => match source_target(api, &source).await {
            Ok(t) => t,
            Err(msg) => return fail(msg),
        },
    };
    let note = FollowNote {
        target: serde_json::to_value(ham_core::request::run_target(&args.target))
            .unwrap_or_default(),
        source: if explicit.is_some() { "flag" } else { "run" },
    };
    if !json {
        eprintln!(
            "{}",
            ham_core::followup::continue_target_line(&args.target, &source, explicit.is_some())
        );
    }
    if needs_editor {
        match prompt_from_editor() {
            Ok(text) => args.prompt = text,
            Err(msg) => {
                eprintln!("usage: {msg}");
                return USAGE;
            }
        }
    }
    cmd_start_direct(api, &args, json, dir, cfg, Some(&note)).await
}

/// The target a follow-up of `run_id` goes to when the line names none: the run's own (`GET /v1/runs/{id}`), as the service continues it.
async fn source_target(api: &CliApi, run_id: &str) -> Result<TargetArg, String> {
    let detail = api.get_run(run_id).await.map_err(|e| {
        format!(
            "GET /v1/runs/{run_id}: {e}. A follow-up goes where the run it continues went; to choose the target yourself add --s3-only or --repo cc:<name>."
        )
    })?;
    ham_core::followup::follow_up_target(&detail.summary.target, run_id)
}

/// The prompt of a follow-up written in the user's editor (`$VISUAL`, else `$EDITOR`); only on a terminal.
fn prompt_from_editor() -> Result<String, String> {
    const NEEDS: &str = CONTINUE_NEEDS;
    // The chat box runs this command as a child whose output goes to a file, so the editor may draw on the controlling terminal.
    if !crate::tty::can_ask() {
        return Err(NEEDS.to_string());
    }
    let editor = ham_core::editor::Editor::from_env().map_err(|e| format!("{NEEDS} ({e})"))?;
    let run = |c: &mut std::process::Command| {
        crate::tty::editor_stdio(c);
        c.status()
    };
    match ham_core::editor::edit_with(&editor, "", &ham_core::editor::default_dir(), run) {
        Ok(ham_core::editor::Outcome::Edited { text, .. }) if !text.trim().is_empty() => Ok(text),
        Ok(_) => Err(format!("{NEEDS}: the editor returned no prompt")),
        Err(e) => Err(format!("{NEEDS} ({e})")),
    }
}

/// `ham models --server`: the list the direct stage serves, with the default it marks, and the model the next run would use.
async fn cmd_models_server<P: ControlPlane>(
    api: &Api<P>,
    cfg: &Config,
    env: EnvFn<'_>,
    json: bool,
    out: &Output,
) -> i32 {
    let list = match api.direct_models().await {
        Ok(l) => l,
        Err(e) => return fail_out(out, format!("GET /v1/direct/models: {e}")),
    };
    if let Some(note) = config_cmd::default_drift(&list) {
        out.err_line(format!("ham: {note}"));
    }
    let (mut o, mut e) = (out.writer(Stream::Out), out.writer(Stream::Err));
    config_cmd::print_models(&list, env, cfg, json, &mut o, &mut e)
}

/// The TUI dashboard on a terminal, else line output until the run is finished.
async fn watch_run(api: &CliApi, run_id: &str, json: bool, dir: PathBuf, cfg: &Config) -> i32 {
    if !json && interactive() {
        return tui_dashboard(api, run_id, dir, cfg).await;
    }
    let opts = WatchOptions {
        interval: FOCUSED_POLL,
        json,
        download_dir: Some(dir),
    };
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    match watch(api, run_id, &opts, &mut out, &mut err).await {
        Ok(done) => exit_of(&done.view),
        Err(e) => fail(e),
    }
}

async fn tui_dashboard(api: &CliApi, run_id: &str, dir: PathBuf, cfg: &Config) -> i32 {
    let nonce = match nonce() {
        Ok(n) => n,
        Err(e) => return fail(e),
    };
    let mut app = tui_app(nonce, cfg);
    let mut first = app.open_run(run_id);
    // The chat opens without the Runs screen: the list is read once, so that the history has the prompts of earlier runs.
    first.extend(app.load_earlier_runs());
    let modes = crate::tui::session::Modes::for_session(&env, cfg.mouse);
    match crate::tui::run(api.clone(), dir, app, first, modes).await {
        Ok(view) => view.map_or_else(|| exit_code(None), |v| exit_of(&v)),
        Err(e) => fail(format!("terminal: {e}")),
    }
}

/// The switches of `[ui]` on the app: a run that starts to wait while the window is not in front is announced with a terminal
/// notification when `HAMH_NOTIFY` is on (or, with the variable unset, `ui.notify`), the bell rings unless `ui.bell` is `false`, the mascot
/// follows `ui.mascot` and `HAMH_NO_MASCOT`, its smoke moves unless `ui.mascot_motion` is `false`, and a copy to the clipboard carries up to
/// `ui.copy_limit_kib` KiB (64 by default).
pub fn apply_ui_settings(app: &mut App, env: EnvFn<'_>, cfg: &Config) {
    app.set_notify(ham_core::config::notify_on(env, cfg));
    app.set_bell(ham_core::config::bell_on(cfg));
    app.set_mascot(
        ham_core::config::mascot_mode(env, cfg),
        crate::tui::mascot::look_from_env(|key| env(key)),
    );
    app.set_mascot_motion(ham_core::config::mascot_motion_on(cfg));
    app.set_copy_limit(ham_core::config::copy_limit_bytes(cfg));
}

/// The mascot's switches for this environment and file (`ui.mascot`, `HAMH_NO_MASCOT`; `tui/head.rs`): the title band and the picture in
/// the empty Runs table on, in the look the environment asks for (`NO_COLOR`, `TERM=dumb` and a locale that is not UTF-8 ask for ASCII);
/// `ascii` forces the ASCII look; `off`, or `HAMH_NO_MASCOT=1` whatever the file says, hides both.
pub fn head_from(env: EnvFn<'_>, cfg: &Config) -> crate::tui::head::Head {
    crate::tui::head::Head::for_mode(
        ham_core::config::mascot_mode(env, cfg),
        crate::tui::mascot::look_from_env(|key| env(key)),
    )
}

/// The TUI of a direct run: its composer starts direct runs on the user's model (`HAMH_MODEL`, then the config file's
/// `default_model`, else Claude Sonnet 5.5) and `d` on its Model field saves a pick as the default.
fn tui_app(nonce: String, cfg: &Config) -> App {
    let mut app = App::new_direct(
        nonce,
        cfg.default_repo.clone(),
        DirectSettings {
            prefs: config_cmd::model_prefs(&env, cfg),
            rules: None,
            no_rules: false,
        },
    );
    // The engine a new run starts on is `default_engine` of the file, unless the person chooses another.
    app.set_default_engine(cfg.default_engine.as_deref());
    // `o` opens a link with this program (`HAMH_OPEN_COMMAND`), else `open` on macOS and `xdg-open` elsewhere.
    app.set_open_command(env("HAMH_OPEN_COMMAND"));
    // The expiry of the download link is written in the local zone of this machine.
    app.set_clock(crate::link::local_clock);
    // A command previewed in the message box does not run if the config file changed since.
    app.set_config_revision(crate::tui::commands::config_revision);
    // `ui.notify` and `ui.bell`, with `HAMH_NOTIFY` winning over the file when it is set.
    apply_ui_settings(&mut app, &env, cfg);
    app.set_tmux(env("TMUX").is_some_and(|v| !v.is_empty()));
    // A run opens with its message box focused (plan 3.3): letters are text until Esc gives the hotkeys back.
    app.set_run_opens_typing(true);
    // The mouse of the session as the guard will ask for it, so `/mouse` knows where it stands; a session with the mouse on starts with the
    // one-line hint on selecting text with the terminal itself.
    let on = crate::tui::session::wants_mouse(&env, cfg.mouse);
    app.set_mouse(
        crate::tui::MouseState {
            on,
            forced_off: crate::tui::session::mouse_forced_off(&env),
        },
        Some(crate::tui::session::selection_hint(&env)),
    );
    // `C` clones into the directory `ham` was started in, as `ham clone <run>` does.
    app.set_clone_dir(std::env::current_dir().ok());
    app
}

/// `ham` with no arguments on a terminal: the Runs screen.
async fn tui_session(backend: Option<String>) -> i32 {
    let (cfg, shown) = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let api = match connect(&cfg, &shown).await {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    // `ham --backend <alias>` opens the composer on that backend.
    let chosen = match backend.as_deref().map(|b| api.plane().select(Some(b))) {
        Some(Err(local)) => return fail(local.to_string()),
        _ => backend,
    };
    let nonce = match nonce() {
        Ok(n) => n,
        Err(e) => return fail(e),
    };
    let mut app = tui_app(nonce, &cfg);
    // The Runs list names the backend of a run from the plane's run map; the composer starts on the backend `--backend` or `default_backend` names.
    crate::tui::attach_backends(
        &mut app,
        api.plane(),
        chosen.or_else(|| cfg.default_backend.clone()),
    );
    let first = app.start();
    let modes = crate::tui::session::Modes::for_session(&env, cfg.mouse);
    match crate::tui::run(api, downloads(&cfg), app, first, modes).await {
        Ok(_) => 0,
        Err(e) => fail(format!("terminal: {e}")),
    }
}

/// `ham update`: the newest version on main of the update source, the wait for the caller's runs, and the `setup.sh --update` of
/// that commit (`crate::update`).
#[allow(clippy::too_many_arguments)]
async fn cmd_update<P: ControlPlane>(
    api: &Api<P>,
    cfg: &Config,
    downloads: PathBuf,
    check: bool,
    yes: bool,
    json: bool,
    no_wait: bool,
    wait_minutes: Option<u32>,
) -> i32 {
    // `main_with` answered the off switch and a source it refuses before any connection was made, so a URL is what is left here.
    let Ok(ham_core::update::Source::Url(source)) = crate::update::source_of(cfg) else {
        return crate::update::exit::OFF;
    };
    let Some(exe) = crate::update::running_exe() else {
        eprintln!("ham update: could not find the program that runs");
        return crate::update::exit::FAILED;
    };
    let ask = |question: &str| -> Option<String> {
        eprint!("{question}");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok().map(|_| line)
    };
    let ctx = crate::update::Context {
        source,
        downloads,
        git: crate::update::Git::system(),
        exe,
        terminal: interactive(),
        ask: &ask,
        backend_recheck: crate::update::BACKEND_RECHECK,
    };
    let req = crate::update::request(check, yes, json, no_wait, wait_minutes);
    crate::update::run(api, &req, &ctx, &Output::live()).await
}

async fn cmd_pull<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    which: &PullArtifact,
    json: bool,
    dir: &Path,
    chunk: u64,
    out: &Output,
) -> i32 {
    let pulled = match pull_with(api, run_id, which, dir, chunk).await {
        Ok(p) => p,
        // A run without a zip is told where its work is, in words for its status.
        Err(crate::pull::PullError::NoArtifact(_)) if *which == PullArtifact::Zip => {
            return fail_out(out, no_zip_text(api, run_id).await);
        }
        Err(e) => return fail_out(out, e),
    };
    let mut state = RunState::new(run_id);
    if let Err(e) = api.poll(&mut state).await {
        return fail_out(out, format!("reading events: {e}"));
    }
    let view = state.view();
    let detail = api.get_run(run_id).await.ok();
    // The clean-checkout test report of the zip, beside the label and never in it: the card carries its `Tests` line. The
    // zip is read once; the card and the `--json` member use the same summary.
    let report = crate::report::summary_of_pulled(&pulled);
    // The flow the run declared, from the same zip: a fact beside the label, shown as the `Flow` row and the `flow` member of `--json`.
    let flow = crate::flow::flow_of_pulled(&pulled);
    let c = crate::flow::with_flow_of(
        crate::report::with_summary(card(detail.as_ref(), Some(&pulled)), report.as_ref()),
        flow,
    );
    // The download link is a bearer capability: the shell prints the card with the whole link, a captured output (the chat panel) shows
    // the link without its signed query and carries the whole link apart for the key press.
    let secret = pulled.url.url.clone();
    if json {
        let mut v = pulled_json(&pulled);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("runId".into(), json!(run_id));
            obj.insert("prUrl".into(), json!(view.pr_url));
            crate::watch::insert_result(obj, &view);
            // A run the stage reported no usage for has neither member (never a fake 0).
            if let Some(u) = c.usage {
                obj.insert("tokens".into(), json!(u.tokens));
                obj.insert("usd".into(), json!(u.usd));
            }
            if let Some(s) = &report {
                obj.insert("testReport".into(), json!(s));
            }
            if let Some(f) = flow {
                obj.insert("flow".into(), json!(f.word()));
            }
        }
        let mut shown = v.clone();
        if let Some(obj) = shown.as_object_mut() {
            obj.insert("url".into(), json!(redact_url(&secret)));
        }
        out.out_bearer_line(&v.to_string(), &shown.to_string(), &secret);
    } else {
        let shown = FinalCard {
            presigned_url: c.presigned_url.as_deref().map(redact_url),
            ..c.clone()
        };
        out.out_bearer_line(&final_card(&view, &c), &final_card(&view, &shown), &secret);
    }
    0
}

/// `ham clone <runId> [dir]`: the run's zip like `ham pull` (into `downloads`), then its `repo.bundle` as a
/// git repository in `into` (default `./<repo name>`), with no remote and a tree that equals the zip's `src/`.
async fn cmd_clone<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    into: Option<&str>,
    json: bool,
    downloads: &Path,
    chunk: u64,
    out: &Output,
) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => return fail_out(out, format!("the working directory: {e}")),
    };
    let req = RunRequest {
        run_id,
        dest: into.map(Path::new),
        downloads,
        cwd: &cwd,
        chunk,
    };
    let mut notice = |n: &str| out.err_line(format!("ham: {}", sanitize_line(n)));
    let (cloned, pulled) = match clone_run(api, &Git::system(), &req, &mut notice).await {
        Ok(r) => r,
        // A run that delivered nothing has no repository to clone; its work is in its saved copy.
        Err(crate::clone::CloneError::NoDelivery(_)) => {
            return fail_out(out, no_zip_text(api, run_id).await);
        }
        Err(e) => return fail_out(out, sanitize_line(&e.to_string())),
    };
    for w in &cloned.warnings {
        out.err_line(format!("ham: warning: {}", sanitize_line(w)));
    }
    if json {
        out.out_line(
            json!({"runId": run_id, "dir": cloned.dir.display().to_string(), "branch": cloned.branch, "head": cloned.head,
                "commits": cloned.commits, "files": cloned.files, "partial": cloned.partial, "treeEqualsZip": true,
                "zip": {"path": cloned.zip.display().to_string(), "sha256": cloned.zip_sha256,
                        "artifactId": pulled.artifact.artifact_id, "resumedFrom": pulled.resumed_from},
                "warnings": cloned.warnings}),
        );
    } else {
        out.out_line(clone_text(run_id, &cloned));
    }
    0
}

/// `ham push <runId> <git-url> [--branch <name>]`: the run's code in another git repository, pushed with the user's own
/// git (see `crate::push`). Standard output has the result (a line, or one JSON object); git's own lines are on stderr.
#[allow(clippy::too_many_arguments)]
async fn cmd_push<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    url: &str,
    branch: Option<&str>,
    json: bool,
    downloads: &Path,
    chunk: u64,
    out: &Output,
) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => return fail_out(out, format!("the working directory: {e}")),
    };
    let req = PushRequest {
        run_id,
        url,
        branch,
        downloads,
        cwd: &cwd,
        chunk,
    };
    let mut notice = |n: &str| out.err_line(format!("ham: {}", sanitize_line(n)));
    let pushed = match push_run(api, &Git::system(), &req, &mut notice).await {
        Ok(p) => p,
        Err(e @ PushError::Usage(_)) => {
            out.err_line(sanitize_line(&format!("usage: {e}")));
            return USAGE;
        }
        Err(e) => return fail_out(out, sanitize_line(&e.to_string())),
    };
    if let Some(w) = &pushed.leftover {
        out.err_line(format!("ham: warning: {}", sanitize_line(w)));
    }
    let c = &pushed.cloned;
    if json {
        out.out_line(
            json!({"runId": run_id, "url": url, "branch": pushed.branch, "pushed": true, "head": c.head,
                "commits": c.commits, "files": c.files, "partial": c.partial,
                "zip": {"path": c.zip.display().to_string(), "sha256": c.zip_sha256,
                        "artifactId": pushed.pulled.artifact.artifact_id}}),
        );
    } else {
        out.out_line(push_text(run_id, &pushed));
    }
    0
}

/// The plain output of `ham push`: where the run's branch went, what it holds, and what was left on this machine.
fn push_text(run_id: &str, p: &crate::push::Pushed) -> String {
    let c = &p.cloned;
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let partial = if c.partial {
        " (PARTIAL: the run did not complete; this is its partial delivery)"
    } else {
        ""
    };
    format!(
        "pushed run {run_id} to {} as branch {}{partial}\n  head {} ({}, {}), built from the zip's repo.bundle\n  the temporary repository beside the zip was removed; the zip is kept at {} (sha256 {})",
        sanitize_line(&p.url),
        sanitize_line(&p.branch),
        &c.head[..c.head.len().min(12)],
        plural(c.commits, "commit", "commits"),
        plural(c.files, "file", "files"),
        sanitize_line(&c.zip.display().to_string()),
        &c.zip_sha256[..c.zip_sha256.len().min(12)],
    )
}

/// The plain output of `ham clone`: where, which branch and commit, the proof, the zip, and how to publish.
fn clone_text(run_id: &str, c: &crate::clone::Cloned) -> String {
    let dir = sanitize_line(&c.dir.display().to_string());
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let partial = if c.partial {
        " (PARTIAL: the run did not complete; this is its partial delivery)"
    } else {
        ""
    };
    format!(
        "cloned run {run_id} into {dir}{partial}\n  branch {} at {} ({}, {})\n  tree equals the zip's src/ (path, blob id and mode of all {} files)\n  zip kept at {} (sha256 {})\n  no remote is set. To publish it: git -C {dir} remote add origin <url> && git -C {dir} push -u origin {}\n  or push the run to another repository in one step, with your own git credentials: ham push {run_id} <git-url>",
        c.branch,
        &c.head[..c.head.len().min(12)],
        plural(c.commits, "commit", "commits"),
        plural(c.files, "file", "files"),
        c.files,
        sanitize_line(&c.zip.display().to_string()),
        &c.zip_sha256[..c.zip_sha256.len().min(12)],
        c.branch
    )
}

/// What a command that needs a zip says for a run that has none: the sentence for the run's status (asked once, here) with the
/// command that downloads the latest saved copy of its work. A status that cannot be read is not guessed.
async fn no_zip_text<P: ControlPlane>(api: &Api<P>, run_id: &str) -> String {
    let status = api.get_run(run_id).await.ok().map(|d| d.summary.status);
    ham_core::saved_copy::no_zip(run_id, status)
}

/// The local time of a link's expiry for the panel (`06:12 MDT`); the stage's own text when it is no time.
fn expiry_text(expires_at: &str) -> String {
    crate::doctor::parse_utc(expires_at)
        .and_then(|t| i64::try_from(t).ok())
        .map_or_else(|| sanitize_line(expires_at), crate::link::local_clock)
}

/// `ham link <runId>`: a fresh download URL. The shell prints it. A captured output keeps it out of the text: the panel reads
/// "Download link ready, expires 06:12 MDT; u copies, o opens" and the URL goes apart to [`crate::output::CmdOutput::bearer`].
async fn cmd_link<P: ControlPlane>(api: &Api<P>, run_id: &str, json: bool, out: &Output) -> i32 {
    let list = match api.list_artifacts(run_id).await {
        Ok(l) => l,
        Err(e) => return fail_out(out, e),
    };
    let a = match select(&list, &PullArtifact::Zip) {
        Ok(a) => a,
        // No zip, so no link; the run's work is in its saved copy.
        Err(_) => return fail_out(out, no_zip_text(api, run_id).await),
    };
    match api.download_url(run_id, &a.artifact_id).await {
        Ok(u) => {
            let shell_text = if json {
                json!({"runId": run_id, "artifactId": a.artifact_id, "url": u.url, "expiresAt": u.expires_at}).to_string()
            } else {
                format!("{} (expires {})", u.url, u.expires_at)
            };
            let panel_text = format!(
                "Download link ready, expires {}; u copies, o opens",
                expiry_text(&u.expires_at)
            );
            out.out_bearer_line(&shell_text, &panel_text, &u.url);
            0
        }
        Err(e) => fail_out(out, e),
    }
}

/// What a resume of a list of runs did: the outcome of each run in the order given, and for a run that started after ham looked at its account
/// the line about that account, with the id of the new run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resumed {
    pub outcomes: Vec<ham_core::resume::Outcome>,
    pub notes: Vec<(String, String)>,
}

/// What the service said to one resume: the id of the run that started, or the plain words for the refusal. The service starts the request the
/// run was started with, so the check of provider data sharing comes first (`crate::sharing::preflight_stored`): its stop is the outcome, and
/// nothing is sent; its line comes back beside the outcome of a run that started.
pub async fn resume_one<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    model: crate::sharing::StoredModel<'_>,
    sharing_on: bool,
) -> (ham_core::resume::Outcome, Option<String>) {
    let outcome = |result| ham_core::resume::Outcome {
        run_id: run_id.to_string(),
        result,
    };
    let cleared = match crate::sharing::preflight_stored(api, run_id, model, sharing_on).await {
        Ok(c) => c,
        Err(stop) => return (outcome(Err(stop)), None),
    };
    match cleared.api.resume_run(run_id).await {
        Ok(r) => (outcome(Ok(r.run_id)), cleared.note),
        Err(e) => {
            let why = match e {
                ApiError::Http { status, error } => {
                    ham_core::resume::explain(status, &error.code, &error.message)
                }
                other => ham_core::resume::explain(0, "", &other.to_string()),
            };
            (outcome(Err(cleared.failed(why))), None)
        }
    }
}

/// Resumes `run_ids`, [`ham_core::resume::MAX_AT_ONCE`] at a time, and returns what each did in the order given. `models` holds the model of
/// each run when the caller read the rows of the run list (a run it does not hold has a row that names no model); `None` leaves the read of
/// each row to the check.
pub async fn resume_many<P: ControlPlane>(
    api: &Api<P>,
    run_ids: &[String],
    models: Option<&std::collections::BTreeMap<String, String>>,
    sharing_on: bool,
) -> Resumed {
    use crate::sharing::StoredModel;
    let mut out = Resumed::default();
    for wave in run_ids.chunks(ham_core::resume::MAX_AT_ONCE) {
        let one = |i: usize| async move {
            match wave.get(i) {
                Some(id) => {
                    let model = match models {
                        Some(known) => StoredModel::Row(known.get(id).map(String::as_str)),
                        None => StoredModel::Unread,
                    };
                    Some(resume_one(api, id, model, sharing_on).await)
                }
                None => None,
            }
        };
        let (a, b, c, d) = tokio::join!(one(0), one(1), one(2), one(3));
        for (outcome, note) in [a, b, c, d].into_iter().flatten() {
            if let (Ok(new_run), Some(note)) = (&outcome.result, note) {
                out.notes.push((new_run.clone(), note));
            }
            out.outcomes.push(outcome);
        }
    }
    out
}

/// `run.provider_sharing` of the config: the user's standing agreement to provider data sharing.
pub fn sharing_setting(cfg: &Config) -> bool {
    cfg.run
        .as_ref()
        .and_then(|r| r.provider_sharing)
        .unwrap_or(false)
}

/// `ham resume <runId>` and `ham resume --all [--dry-run]`. `--all` plans from the caller's list of runs, as the terminal app's `R` does,
/// and starts what the plan takes; `--dry-run` prints the plan and starts nothing. A named run is sent to the service as it stands: the service
/// decides, and its refusal is the answer. The exit code is 0 when every run in the plan started (or the plan was empty or a dry run) and 1 when
/// any call failed.
async fn cmd_resume<P: ControlPlane>(
    api: &Api<P>,
    run_id: Option<&str>,
    all: bool,
    dry_run: bool,
    sharing_on: bool,
    json: bool,
    out: &Output,
) -> i32 {
    // `--all` reads the rows of the run list, so the model of each run is known without another call; for a named run the check reads its row.
    let mut models: Option<std::collections::BTreeMap<String, String>> = None;
    let plan = if all {
        match api.list_run_rows().await {
            Ok(rows) => {
                models = Some(
                    rows.iter()
                        .filter_map(|r| Some((r.summary.run_id.clone(), r.model.clone()?)))
                        .collect(),
                );
                let runs: Vec<_> = rows.into_iter().map(|r| r.summary).collect();
                ham_core::resume::plan(&runs)
            }
            Err(e) => return fail_out(out, e),
        }
    } else {
        ham_core::resume::ResumePlan {
            resume: run_id.map(str::to_string).into_iter().collect(),
            skipped: Vec::new(),
        }
    };
    if dry_run {
        let planned: Vec<serde_json::Value> = plan
            .resume
            .iter()
            .map(|id| json!({"runId": id, "result": "would-resume", "mode": "follow_up"}))
            .chain(
                plan.skipped
                    .iter()
                    .map(|s| json!({"runId": s.run_id, "result": "skipped", "reason": s.why})),
            )
            .collect();
        if json {
            for v in planned {
                out.out_line(v);
            }
        } else {
            for id in &plan.resume {
                out.out_line(format!("run {id}: would start a follow-up run"));
            }
            for s in &plan.skipped {
                out.out_line(format!("run {}: skipped: {}", s.run_id, s.why));
            }
            if plan.resume.is_empty() && plan.skipped.is_empty() {
                out.out_line("no paused or stalled run to resume");
            }
        }
        return 0;
    }
    let Resumed { outcomes, notes } =
        resume_many(api, &plan.resume, models.as_ref(), sharing_on).await;
    // The line about the account of a run that started, on standard error like the line of any other start.
    for (_, line) in &notes {
        out.err_line(line);
    }
    if json {
        for v in ham_core::resume::report_json(&plan, &outcomes) {
            out.out_line(v);
        }
    } else {
        let lines = ham_core::resume::report_lines(&plan, &outcomes);
        if lines.is_empty() {
            out.out_line("no paused or stalled run to resume");
        }
        for line in lines {
            out.out_line(line);
        }
    }
    i32::from(outcomes.iter().any(|o| o.result.is_err()))
}

/// The configured `/v1` client from `config.toml` (for `ham-admin`, which shares it).
pub async fn configured_api() -> Result<Api<HttpsPlane>, String> {
    let (cfg, shown) = load_config()?;
    connect_single(&cfg, &shown).await
}

/// `ham runs [--probes]`: the caller's runs, newest first. The runs a probe wrote
/// (`kind: probe`) are left out, with a count on stderr, unless
/// `--probes` asks for them; then their line ends `  probe`. The runs are read as rows: a run that has a name prints it
/// after its creation time, and `--json` gives that run `name` and `nameSource`. Since a run with tags prints them after the name as
/// ` [a b c]`, and `--json` gives a run `tags` when its row has the member (an empty list too).
async fn cmd_runs<P: ControlPlane>(api: &Api<P>, probes: bool, json: bool, out: &Output) -> i32 {
    let mut rows = match api.list_run_rows().await {
        Ok(r) => r,
        Err(e) => return fail_out(out, e),
    };
    let all = rows.len();
    if !probes {
        rows.retain(|r| !r.summary.is_probe());
    }
    let hidden = all - rows.len();
    // With two backends or more each run names its backend: the second field of a line and a `backend` member of the JSON. A client with
    // one backend prints what it always printed.
    let multi = api.plane().backend_view().is_some();
    if json {
        let runs: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let mut v = run_json(r);
                if multi
                    && let (Some(alias), Some(obj)) =
                        (api.plane().backend_of(&r.summary.run_id), v.as_object_mut())
                {
                    obj.insert("backend".into(), json!(alias));
                }
                v
            })
            .collect();
        out.out_line(json!({ "runs": runs }));
    } else {
        for r in &rows {
            let line = runs_line_tagged(
                &r.summary,
                r.name.as_deref(),
                r.tags.as_deref().unwrap_or_default(),
            );
            out.out_line(if multi {
                line_with_backend(&line, api.plane().backend_of(&r.summary.run_id))
            } else {
                line
            });
        }
        if hidden > 0 {
            let (runs_word, pronoun) = if hidden == 1 {
                ("run", "it")
            } else {
                ("runs", "them")
            };
            out.err_line(format!(
                "{hidden} probe {runs_word} hidden; ham runs --probes shows {pronoun}"
            ));
        }
    }
    // One line for each backend that did not answer, with the way forward.
    if let Some(view) = api.plane().backend_view() {
        for banner in view.banners() {
            out.err_line(banner);
        }
    }
    0
}

/// A `ham runs` line with the backend of the run as its second field; the run id stays first.
fn line_with_backend(line: &str, alias: Option<String>) -> String {
    let alias = alias.unwrap_or_else(|| "?".to_string());
    match line.split_once("  ") {
        Some((id, rest)) => format!("{id}  {alias}  {rest}"),
        None => format!("{line}  {alias}"),
    }
}

/// The JSON of one run of `ham runs --json`: the summary as it always was, plus `name` and `nameSource` when the row has a name and `tags` when
/// the row has the member. No other member of the index row (`title`, `model`, `lastActivity`,
/// `outcome`) is added.
fn run_json(r: &ham_core::run_index::RunRow) -> serde_json::Value {
    let mut v = serde_json::to_value(&r.summary).unwrap_or_default();
    if let (Some(name), Some(obj)) = (&r.name, v.as_object_mut()) {
        obj.insert("name".into(), json!(name));
        if let Some(source) = &r.name_source {
            obj.insert("nameSource".into(), json!(source));
        }
    }
    if let (Some(tags), Some(obj)) = (&r.tags, v.as_object_mut()) {
        obj.insert("tags".into(), json!(clean_tags(tags)));
    }
    v
}

/// The tags of a row as the client may print them: each through `sanitize_line` (an escape sequence is removed), then the grammar of a typed tag
/// (a text with a space, or one the policy refuses, is left out, so a server cannot make a tag look like a trailer), equal tags once, at most 19.
pub fn clean_tags(tags: &[String]) -> Vec<String> {
    let cleaned: Vec<String> = tags.iter().map(|t| sanitize_line(t)).collect();
    ham_core::runtag::effective::effective("", &[], &cleaned, &[]).tags
}

/// One `ham runs` line: run id, status (phase), target and creation time, then why a cleared run
/// ended early (`ended early: <STATUS> <error>: <cause>`, the reaper's `aborted`), then `  probe` for
/// a run a probe wrote.
pub fn runs_line(r: &RunSummary) -> String {
    runs_line_named(r, None)
}

/// [`runs_line`] with the run's name: two spaces and the name follow the creation time, before the trailers, so every byte up to
/// and including the creation time is what it was and the id stays the first field. The name is display text, cleaned like every API string.
pub fn runs_line_named(r: &RunSummary, name: Option<&str>) -> String {
    runs_line_tagged(r, name, &[])
}

/// [`runs_line_named`] with the run's tags: two spaces and `[a b c]` follow the name (the creation time when the run has no name),
/// before the trailers, so every byte up to the name is what it was and the trailers still end the line. No tags, no bracket. The tags pass
/// [`clean_tags`] whoever the caller is.
pub fn runs_line_tagged(r: &RunSummary, name: Option<&str>, tags: &[String]) -> String {
    let v = serde_json::to_value(r).unwrap_or_default();
    let target = match &r.target {
        RunTarget::Repo { repo } => repo.to_string(),
        RunTarget::NewRepo { repo } => format!("new {repo}"),
        RunTarget::S3Only => "s3-only".to_string(),
    };
    let mut line = format!(
        "{}  {} ({})  {}  {}",
        sanitize_line(&r.run_id),
        v["status"].as_str().unwrap_or("-"),
        ham_core::render::phase_listed(Some(r.status), Some(r.phase)),
        sanitize_line(&target),
        sanitize_line(&r.created_at),
    );
    if let Some(name) = name.map(sanitize_line).filter(|n| !n.trim().is_empty()) {
        line.push_str("  ");
        line.push_str(name.trim());
    }
    let tags = clean_tags(tags);
    if !tags.is_empty() {
        line.push_str("  [");
        line.push_str(&tags.join(" "));
        line.push(']');
    }
    if let Some(a) = &r.aborted {
        line.push_str("  ended early: ");
        line.push_str(&ham_core::state::AbortedView::of(a).line());
    }
    if let Some(n) = &r.needs_input {
        line.push_str("  needs input: ");
        line.push_str(&sanitize_line(&n.title));
    }
    if r.is_probe() {
        line.push_str("  probe");
    }
    line
}
