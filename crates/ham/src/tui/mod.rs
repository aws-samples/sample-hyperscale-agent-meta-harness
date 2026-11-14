//! The Ratatui TUI: [`App`] holds the screens and turns keys and API answers into
//! [`Effect`]s; [`perform`] runs one effect against the API; [`run`] owns the terminal (the [`session::TerminalSession`]: raw mode, the
//! alternate screen, bracketed paste, focus events and, when wanted, mouse reports, all given back on exit, Ctrl-C, a signal and a
//! panic) and the 1 s poll timer.

mod app;
pub mod burst;
pub mod chat;
pub mod chat_search;
pub mod commands;
pub mod copy;
mod engine_config_card;
pub mod export;
pub mod ext;
mod goal_brake;
pub mod handoff;
pub mod head;
pub mod input;
pub mod keys;
pub mod markdown;
pub mod mascot;
pub mod matchrow;
pub mod mouse;
pub mod reader;
mod rung;
pub mod runs_table;
pub mod runs_view;
pub mod select;
pub mod session;
pub mod settings;
mod skills_card;
pub mod transcript;
mod ui;
pub mod version_tag;

pub use app::{
    App, ArtifactList, ClockText, CloneState, CommandSink, Composer, DirectPick, DirectSettings,
    DownloadState, Effect, Field, ListLine, MouseState, Msg, Opened, OwnerLookup, PickSource,
    Reply, Screen, SharingAsk, Submit, TagsOrigin, TargetKind,
};

use crate::api::Api;
use crate::http::ControlPlane;
use crate::pull::download;
use crossterm::event::Event;
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, CrosstermBackend};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::TryRecvError;

/// Tells the app what the plane knows about its backends before the first frame: who holds a run, which backend the composer
/// starts on, whether each backend is declared safe for customer content (a one-backend client included), and the backends' state, so the
/// composer knows from the start whether it holds one backend or several.
pub fn attach_backends<P: ControlPlane>(
    app: &mut App,
    plane: &std::sync::Arc<P>,
    pick: Option<String>,
) {
    let owner = std::sync::Arc::clone(plane);
    app.set_backends(
        OwnerLookup(std::sync::Arc::new(move |id| owner.backend_of(id))),
        pick,
    );
    app.set_declared(plane.declared_safe());
    app.update(Msg::Backends(plane.backend_view()));
}

/// Runs one effect; `Quit` yields nothing. Errors become the message text shown on screen.
pub async fn perform<P: ControlPlane>(api: &Api<P>, dir: &Path, effect: Effect) -> Option<Msg> {
    perform_with_env(api, dir, effect, &ham_core::envnames::os_env).await
}

/// [`perform`] with the environment the starts of a run find the user's settings and rules through (a test passes its own): the composer's
/// start and the jobs of [`ext`]. Every other effect reads the environment of the process, as it always did.
pub async fn perform_with_env<P: ControlPlane>(
    api: &Api<P>,
    dir: &Path,
    effect: Effect,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Option<Msg> {
    Some(match effect {
        // Both reads of the list carry the run index: the title and the model of each run, joined to the row for the filter.
        Effect::LoadRuns => Msg::RunsIndexed(
            api.read_runs_indexed(ham_core::runfilter::Refresh::Manual)
                .await
                .map_err(|e| e.to_string()),
        ),
        Effect::RefreshRuns(how) => {
            Msg::RunsIndexed(api.read_runs_indexed(how).await.map_err(|e| e.to_string()))
        }
        Effect::SubmitDirect { args, nonce } => {
            submit_msg_with_env(api, &args, &nonce, None, env).await
        }
        Effect::SubmitDirectOn {
            args,
            nonce,
            backend,
        } => submit_msg_with_env(&api.on(&backend), &args, &nonce, Some(backend), env).await,
        Effect::SubmitAgreed {
            args,
            nonce,
            backend,
            shown,
        } => {
            let on = match &backend {
                Some(b) => api.on(b),
                None => api.clone(),
            };
            submit_agreed_with_env(&on, &args, &nonce, backend, &shown, env).await
        }
        Effect::SaveDefaultModel { model_id } => Msg::DefaultSaved(save_default_model(&model_id)),
        // The Settings screen reads and writes `config.toml` here, with the process's environment, and nowhere else.
        Effect::Settings(job) => Msg::Settings(settings::perform(&ham_core::envnames::os_env, job)),
        Effect::Poll { run_id, cursors } => {
            let (pages, error) = api.fetch_pages(&run_id, &cursors).await;
            Msg::Pages(match error {
                Some(e) => Err(e.to_string()),
                None => Ok(pages),
            })
        }
        Effect::Download {
            run_id,
            artifact_id,
        } => {
            let result = async {
                let list = api
                    .list_artifacts(&run_id)
                    .await
                    .map_err(|e| e.to_string())?;
                let a = list
                    .into_iter()
                    .find(|a| a.artifact_id == artifact_id)
                    .ok_or_else(|| format!("artifact {artifact_id} is not listed"))?;
                download(api, &run_id, &a, dir)
                    .await
                    .map_err(|e| e.to_string())
            }
            .await;
            Msg::Downloaded { run_id, result }
        }
        Effect::OpenUrl { program, url, what } => Msg::Opened(
            crate::open::open_url(&program, &url)
                .await
                .map(|()| Opened {
                    what,
                    expires_at: None,
                }),
        ),
        Effect::OpenBearer {
            program,
            bearer,
            what,
        } => Msg::Opened(
            crate::open::open_url(&program, bearer.url())
                .await
                .map(|()| Opened {
                    what,
                    expires_at: None,
                }),
        ),
        Effect::OpenDownload {
            program,
            run_id,
            artifact_id,
        } => {
            // A link made now, so a link that lapsed since the zip came down is never the one opened. It lives in this task only.
            let result = async {
                let link = api
                    .download_url(&run_id, &artifact_id)
                    .await
                    .map_err(|e| e.to_string())?;
                crate::open::open_url(&program, &link.url).await?;
                Ok(Opened {
                    what: "download link",
                    expires_at: Some(link.expires_at),
                })
            }
            .await;
            Msg::Opened(result)
        }
        Effect::LoadDetail { run_id } => {
            Msg::Detail(api.get_run(&run_id).await.map_err(|e| e.to_string()))
        }
        // The name of a run is a label: a read that fails is no event, the schedule asks again (`run_name.rs`).
        Effect::ReadRunName { run_id } => {
            let name = api.run_name_of(&run_id).await.ok().flatten();
            Msg::RunName { run_id, name }
        }
        Effect::Clone { run_id, cwd } => {
            // The shared clone flow of `ham clone <run>`: the zip is fetched by the pull rules into the download directory (one that
            // verifies is kept) and cloned into the working directory, else next to the zip. git runs on a blocking thread inside it.
            let cwd = cwd.unwrap_or_else(|| dir.to_path_buf());
            let os_env = ham_core::envnames::os_env;
            let result = match crate::pull::range_bytes(os_env("HAMH_RANGE_BYTES").as_deref()) {
                Ok(chunk) => crate::clone::clone_run(
                    api,
                    &crate::clone::Git::system(),
                    &crate::clone::RunRequest {
                        run_id: &run_id,
                        dest: None,
                        downloads: dir,
                        cwd: &cwd,
                        chunk,
                    },
                    // A partial delivery is flagged on the card by the answer itself.
                    &mut |_: &str| {},
                )
                .await
                .map(|(cloned, _)| cloned)
                .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            Msg::Cloned { run_id, result }
        }
        Effect::SetMouse { on } => {
            // The terminal's mouse modes go on or off now; the session records them so the restore at the end writes the right ones.
            let _ = session::set_mouse(on);
            return None;
        }
        Effect::LoadArtifacts { run_id } => {
            Msg::Artifacts(api.list_artifacts(&run_id).await.map_err(|e| e.to_string()))
        }
        Effect::DownloadArtifact {
            run_id,
            artifact_id,
        } => {
            let result = async {
                let list = api
                    .list_artifacts(&run_id)
                    .await
                    .map_err(|e| e.to_string())?;
                let a = list
                    .into_iter()
                    .find(|a| a.artifact_id == artifact_id)
                    .ok_or_else(|| format!("artifact {artifact_id} is not listed"))?;
                download(api, &run_id, &a, dir)
                    .await
                    .map_err(|e| e.to_string())
            }
            .await;
            Msg::ArtifactPulled(result)
        }
        Effect::Cancel { run_id } => Msg::Cancelled(
            api.cancel_run(&run_id)
                .await
                .map_err(|e| format!("cancel {run_id}: {e}")),
        ),
        Effect::Answer { run_id, answer } => Msg::Answered(
            api.answer_input(&run_id, &answer)
                .await
                .map_err(|e| e.to_string()),
        ),
        Effect::RunCommand { invocation, argv } => Msg::CommandDone {
            out: run_command(api, dir, &invocation).await,
            argv,
        },
        Effect::Ext(job) => Msg::Ext(ext::perform_with_env(api, job, env).await),
        // The edit route: the batches go one after the other, and a request that fails as a whole stops the ones behind it.
        Effect::EditTags { origin, batches } => {
            let mut results = Vec::with_capacity(batches.len());
            for batch in &batches {
                match api.edit_tags(batch).await {
                    Ok(answer) => results.push(Ok(answer)),
                    Err(e) => {
                        results.push(Err(crate::api::tags_failure(&e)));
                        break;
                    }
                }
            }
            Msg::TagsEdited { origin, results }
        }
        Effect::Bell => {
            // One BEL to the terminal; the alternate screen keeps it from disturbing the drawing.
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
            return None;
        }
        Effect::Notify(alert) => {
            // The alert builds its bytes from fixed words and the run id; the terminal decides whether it shows a notification.
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = alert.write_to(&mut out);
            let _ = out.flush();
            return None;
        }
        Effect::Copy(prepared) => Msg::Copied(
            tokio::task::spawn_blocking(move || {
                let mut out = std::io::stdout();
                run_copy(
                    &prepared,
                    &copy::plan_here(),
                    &mut copy::SystemRunner::default(),
                    &mut out,
                )
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string())),
        ),
        Effect::Export {
            run_id,
            arg,
            stamp,
            content,
            blocks,
        } => {
            let dir = dir.to_path_buf();
            Msg::Exported(
                tokio::task::spawn_blocking(move || {
                    run_export(&dir, arg.as_deref(), &run_id, &stamp, &content, blocks)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
            )
        }
        // The event loop runs the editor itself: it needs the terminal.
        Effect::EditDraft { .. } | Effect::RunTerminal { .. } => return None,
        Effect::Quit => return None,
    })
}

/// Edits `draft` with `editor` and says what came of it. `run` starts the editor (the loop hands it [`reader::suspend_for`], which gives the
/// real terminal to the child and takes it back); the answer is the message for the app.
pub fn run_editor_with<F>(
    editor: Result<ham_core::editor::Editor, ham_core::editor::EditError>,
    draft: &str,
    dir: &Path,
    run: F,
) -> Msg
where
    F: FnOnce(&mut std::process::Command) -> std::io::Result<std::process::ExitStatus>,
{
    match editor.and_then(|ed| ham_core::editor::edit_with_report(&ed, draft, dir, run)) {
        Ok(report) => Msg::Edited(Ok(report)),
        Err(e) => Msg::Edited(Err(e.to_string())),
    }
}

/// Clears the screen and makes the next draw paint every cell: the repaint of a terminal whose picture may be damaged (`App::take_repaint`), and the
/// fresh screen after a child that had the terminal. `Terminal::clear` is not used: in ratatui 0.30 it first asks the terminal
/// where its cursor is (ESC [ 6 n) and waits up to two seconds for the answer; a terminal that does not answer (a pseudo-terminal, a
/// multiplexer, a recording) makes it return an error before it resets the back buffer, and the draws that follow then paint only the
/// cells that changed, on a screen that is blank. Clearing the backend and swapping the buffers needs no answer.
pub fn clear_screen<B: Backend>(terminal: &mut Terminal<B>) {
    let _ = terminal.backend_mut().clear_region(ClearType::All);
    terminal.swap_buffers();
}

/// The edits and the terminal commands among `effects` run here, on the loop's thread, because the editor, git and ssh need the terminal:
/// the reader parks, the terminal goes to the child (the editor, or a child `ham` for `push` and `rules edit`, see [`handoff`]), and when
/// it ends the screen is cleared (the alternate screen the TUI comes back to is blank) so the next draw paints all of it. The app gets the
/// answer at once; what it asks for in turn comes back with the other effects.
fn run_edits<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    effects: Vec<Effect>,
) -> Vec<Effect> {
    let mut rest = Vec::new();
    for effect in effects {
        match effect {
            Effect::EditDraft { text } => {
                let answer = run_editor_with(
                    ham_core::editor::Editor::from_env(),
                    &text,
                    &ham_core::editor::default_dir(),
                    reader::suspend_for,
                );
                clear_screen(terminal);
                rest.extend(app.update(answer));
            }
            Effect::RunTerminal { argv } => {
                // nosemgrep: current-exe -- the child is this very binary, started with the arguments of the command the person confirmed; the path is never read back as input
                let out = match std::env::current_exe() {
                    Ok(program) => handoff::run_child_with(
                        &program,
                        &argv,
                        &ham_core::editor::default_dir(),
                        reader::suspend_for,
                    ),
                    Err(e) => {
                        handoff::failed(format!("could not find this program to run it: {e}\n"))
                    }
                };
                clear_screen(terminal);
                rest.extend(app.update(Msg::CommandDone { argv, out }));
            }
            other => rest.push(other),
        }
    }
    rest
}

/// Puts `prepared` on the clipboard by the first route of `routes` that works and returns the notice that names the path used. The text
/// was cleaned and masked by [`copy::prepare`]; OSC 52 is written to `terminal` only on that route.
pub fn run_copy(
    prepared: &copy::Prepared,
    routes: &[copy::Route],
    runner: &mut dyn copy::Runner,
    terminal: &mut dyn std::io::Write,
) -> Result<String, String> {
    copy::copy(prepared, routes, runner, terminal)
        .map(|outcome| outcome.notice())
        .map_err(|e| e.to_string())
}

/// Writes `content` (the Markdown of the conversation) where `arg` says, or into `dir` with a generated name, and returns the notice.
/// `dir` is made when it is missing; a file that is there is never replaced.
pub fn run_export(
    dir: &Path,
    arg: Option<&str>,
    run_id: &str,
    stamp: &str,
    content: &str,
    blocks: usize,
) -> Result<String, String> {
    // The download directory is the one place made on the way; a directory the person typed has to exist.
    if arg.is_none() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dest = export::resolve(arg, dir, home.as_deref());
    let path = export::write(&dest, run_id, stamp, content).map_err(|e| e.to_string())?;
    Ok(export::notice(&path, blocks, content.len()))
}

/// The composer is interactive, so its request asks the run to wait for the user's answer when Cline needs one: 30 minutes unless the user
/// set `HAMH_INPUT_WAIT` or `input_wait_minutes` (0 sends nothing and keeps the behaviour of every earlier run). A value that is set and
/// wrong is an error naming its layer. The arguments come back with every setting of the file applied
/// (`ham_core::request::resolve_run_settings`).
pub fn interactive_wait(
    args: &ham_core::command::DirectArgs,
    env: &dyn Fn(&str) -> Option<String>,
    config: &ham_core::config::Config,
) -> Result<ham_core::command::DirectArgs, String> {
    ham_core::request::resolve_run_settings(config, env, args, true)
        .map(|resolved| resolved.args)
        .map_err(|e| e.to_string())
}

/// The wall clock in Unix seconds (the app does no I/O, so the loop reads it for the card's minutes left).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// [`submit_msg_with_env`] as a result: the start's answer, or the words the composer shows (the question of provider data sharing is shown as the
/// refusal it replaces). The composer's start as a test drives it against a config file of its own (`tests/report_call_sites.rs`).
pub async fn submit_direct_with_env<P: ControlPlane>(
    api: &Api<P>,
    args: &ham_core::command::DirectArgs,
    nonce: &str,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<harness_protocol::api::v1::StartRunResponse, String> {
    match submit_msg_with_env(api, args, nonce, None, env).await {
        Msg::Started(r) => r,
        Msg::StartedWith { response, .. } => Ok(response),
        Msg::SharingAsk(ask) => Err(ham_core::models::SharingRefusal {
            id: ask.id,
            name: ask.model,
        }
        .to_string()),
        _ => Err("the start gave no answer".into()),
    }
}

/// `POST /v1/direct/runs` for the composer, with the environment given (the config directory, the home, the input wait): the user's rules from disk
/// (the environment's HOME or XDG_CONFIG_HOME, or the paths `args` names), the request with the model `args` carries, one signed call. A model that
/// needs provider data sharing while the run has not agreed (the setting off, no `--provider-sharing` in the Options box) gives the question of the
/// composer instead; a run that agreed has the account's mode checked and, if needed, switched first ([`crate::sharing::preflight`]), and the
/// line that says so comes back with the start. The account is the one of the backend the run starts on, and the start is sent to that backend.
pub async fn submit_msg_with_env<P: ControlPlane>(
    api: &Api<P>,
    args: &ham_core::command::DirectArgs,
    nonce: &str,
    backend: Option<String>,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Msg {
    submit(api, args, nonce, backend, None, env).await
}

/// [`submit_msg_with_env`] for the answer `y` to the question of provider data sharing: `args` carry the agreement for this run and `shown` is
/// what the question showed. The account is read again first; when it is no longer in the state the person saw, nothing is switched or started
/// and the answer is the question for the state it is in now ([`crate::sharing::preflight_agreed`]).
pub async fn submit_agreed_with_env<P: ControlPlane>(
    api: &Api<P>,
    args: &ham_core::command::DirectArgs,
    nonce: &str,
    backend: Option<String>,
    shown: &ham_core::sharing::Shown,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Msg {
    submit(api, args, nonce, backend, Some(shown), env).await
}

/// The question of the composer as a message: the start it holds is `args` as it was first submitted, without the agreement.
fn sharing_ask(
    args: &ham_core::command::DirectArgs,
    nonce: &str,
    backend: Option<String>,
    model: String,
    id: String,
    question: crate::sharing::Question,
) -> Msg {
    let mut held = args.clone();
    held.provider_sharing = None;
    Msg::SharingAsk(Box::new(SharingAsk {
        args: held,
        nonce: nonce.to_string(),
        backend,
        model,
        id,
        question: question.text,
        shown: question.shown,
    }))
}

async fn submit<P: ControlPlane>(
    api: &Api<P>,
    args: &ham_core::command::DirectArgs,
    nonce: &str,
    backend: Option<String>,
    shown: Option<&ham_core::sharing::Shown>,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Msg {
    let (cfg, _) = match crate::config_cmd::load(env) {
        Ok(c) => c,
        Err(e) => return Msg::Started(Err(e)),
    };
    // `args.model` is always the model on screen, so the weaker layers are not consulted for the model; every other setting of the file
    // is resolved by the one function the command line and the follow-up call too.
    let resolved = match ham_core::request::resolve_run_settings(&cfg, env, args, true) {
        Ok(r) => r,
        Err(ham_core::request::SettingsError::Model(
            ham_core::models::ModelError::NeedsSharing(refusal),
        )) if args.provider_sharing.is_none() => {
            return match crate::sharing::question(api, &refusal.name, &refusal.id).await {
                Ok(question) => {
                    sharing_ask(args, nonce, backend, refusal.name, refusal.id, question)
                }
                // A start with no backend to go to: the refusal of the start, and no question about an account nobody chose.
                Err(e) => Msg::Started(Err(e)),
            };
        }
        Err(e) => return Msg::Started(Err(e.to_string())),
    };
    let inputs = match crate::rules_cmd::direct_inputs_for(env, &resolved.args) {
        Ok(i) => i,
        Err(e) => return Msg::Started(Err(e.to_string())),
    };
    let request = match resolved.request(inputs) {
        Ok(r) => r,
        Err(e) => return Msg::Started(Err(e.to_string())),
    };
    let cleared = match shown {
        None => {
            match crate::sharing::preflight(api, &request, crate::sharing::agreement_of(&resolved))
                .await
            {
                Ok(c) => c,
                Err(stop) => return Msg::Started(Err(stop)),
            }
        }
        Some(shown) => match crate::sharing::preflight_agreed(api, &request, shown).await {
            Ok(crate::sharing::Agreed::Go(c)) => c,
            Ok(crate::sharing::Agreed::AskAgain(question)) => {
                let (model, id) = crate::sharing::sharing_model(&request).map_or_else(
                    || (request.model.clone(), request.model.clone()),
                    |m| (m.name.clone(), m.id.clone()),
                );
                return sharing_ask(args, nonce, backend, model, id, question);
            }
            Err(stop) => return Msg::Started(Err(stop)),
        },
    };
    // The start goes through the client the check aimed: the account that was read and switched is the one of the backend the run starts on.
    match crate::start::start_direct(&cleared.api, &request, nonce).await {
        Ok(started) => {
            let response = harness_protocol::api::v1::StartRunResponse {
                run_id: started.run_id,
                status: started.status,
            };
            match cleared.note {
                Some(note) => Msg::StartedWith { response, note },
                None => Msg::Started(Ok(response)),
            }
        }
        Err(e) => Msg::Started(Err(cleared.failed(crate::api::start_failure(&e)))),
    }
}

/// A command typed in the message box, run with the code of the shell and its text kept as data. The config
/// file is read now, so a command sees the settings as they are. A command that needs the terminal never gets here (the box refuses it),
/// and standard input is not offered: a message of `-` is refused by the command itself.
async fn run_command<P: ControlPlane>(
    api: &Api<P>,
    dir: &Path,
    invocation: &ham_core::command::Invocation,
) -> crate::output::CmdOutput {
    use crate::output::{Chunk, CmdOutput, Stream};
    let env = ham_core::envnames::os_env;
    let failed = |why: String| CmdOutput {
        chunks: vec![Chunk {
            stream: Stream::Err,
            text: format!("error: {why}\n"),
        }],
        code: ham_core::exit::ERROR,
        bearer: None,
    };
    let (cfg, _) = match crate::config_cmd::load(&env) {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let chunk = match crate::pull::range_bytes(env("HAMH_RANGE_BYTES").as_deref()) {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let ctx = crate::cli::ExecContext {
        api,
        cfg: &cfg,
        downloads: dir,
        chunk,
        env: &env,
        stdin: None,
    };
    crate::cli::run_captured(invocation, &ctx).await
}

/// `default_model` written into the config file through the comment-keeping writer (the composer's `d` key): the stored id comes back, and
/// the comments and the order of the file stay. `env` says where the file is.
pub fn save_default_model_with_env(
    env: &dyn Fn(&str) -> Option<String>,
    model_id: &str,
) -> Result<String, String> {
    let applied = crate::config_cmd::apply_set(env, "default_model", model_id)?;
    Ok(applied.value.unwrap_or_else(|| model_id.to_string()))
}

fn save_default_model(model_id: &str) -> Result<String, String> {
    save_default_model_with_env(&ham_core::envnames::os_env, model_id)
}

/// The TUI until the user quits; returns the focused run's view, if any. `modes` is what [`session::Modes::for_session`] made of the
/// environment and the config's `mouse` setting. A SIGTERM, SIGINT or SIGHUP ends the loop like a quit and then fails with the
/// signal's name, so a script sees that the TUI did not end on the user's word.
pub async fn run<P: ControlPlane>(
    api: Api<P>,
    dir: PathBuf,
    app: App,
    first: Vec<Effect>,
    modes: session::Modes,
) -> std::io::Result<Option<ham_core::state::RunView>> {
    let guard = session::TerminalSession::enter(modes)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<(Event, Instant)>();
    // The reader has its own thread (it waits for the terminal) and parks while a child, an editor, has the terminal; it ends when the
    // receiver is dropped. Each event carries the instant it was read, so the burst guard measures the gap between keys, not the time
    // the loop got round to them.
    // A continue after a stop reaches the loop as the terminal reporting focus: whatever was drawn on the terminal meanwhile is repainted.
    // The forwarder holds a weak sender, so only the reader keeps the channel open.
    let cont = session::forward_continue(event_tx.downgrade());
    reader::start(event_tx);
    let ended = tokio::select! {
        result = event_loop_timed(
            &mut terminal,
            api,
            dir,
            app,
            first,
            event_rx,
            Duration::from_secs(1),
        ) => Ok(result),
        signal = session::wait_for_signal() => Err(signal),
    };
    if let Some(cont) = cont {
        cont.abort();
    }
    // The terminal comes back before anything is printed about how the session ended.
    drop(terminal);
    guard.restore()?;
    match ended {
        Ok(result) => result.map_err(std::io::Error::other),
        Err(signal) => Err(std::io::Error::other(format!(
            "ended by {}",
            session::signal_name(signal)
        ))),
    }
}

/// The least time between two wake-ups of the loop for a drag held outside the pane: far below the 60 ms repeat, far above a spin.
const HOLD_FLOOR: Duration = Duration::from_millis(5);

/// What the loop does with one terminal event read at `at`: the effects, and whether the screen may have changed.
fn on_event(app: &mut App, event: Event, at: Instant) -> (Vec<Effect>, bool) {
    match event {
        Event::Key(key) => (app.handle_key_at(key, at), true),
        Event::Paste(text) => (app.handle_paste_at(text, at), true),
        Event::Resize(..) => {
            // A click between a resize and the next draw hits nothing.
            app.resized();
            (vec![], true)
        }
        // The pointer turns a press, a release, a drag and a wheel notch into what its key would do. A move with no button is not
        // asked for by the session.
        Event::Mouse(m) => (app.handle_mouse_at(m, at), true),
        // Losing focus changes nothing on the screen. Getting it back may mean the window was covered and uncovered, and the terminal can have lost
        // cells that ratatui, which writes only what changed, would never write again: the next draw repaints all of it.
        Event::FocusLost => {
            app.focus_lost();
            (vec![], false)
        }
        Event::FocusGained => {
            app.focus_gained();
            (vec![], true)
        }
    }
}

/// A key counted by the test hook: the debug build panics at the key `HAMH_TEST_PANIC_AFTER_KEYS` names, so the exit tests can prove
/// that a panic gives the terminal back. A release build has no such hook.
#[cfg(debug_assertions)]
fn test_hook_key(remaining: &mut Option<u32>) {
    if let Some(n) = remaining {
        *n = n.saturating_sub(1);
        assert!(*n > 0, "HAMH_TEST_PANIC_AFTER_KEYS reached");
    }
}

#[cfg(not(debug_assertions))]
fn test_hook_key(_: &mut Option<u32>) {}

/// [`event_loop_timed`] for events that carry no instant: those already queued are stamped with the instant of the start, each later
/// one when a forwarding task receives it, which is when it was sent. The tests drive the loop this way.
pub async fn event_loop<P: ControlPlane, B: Backend>(
    terminal: &mut Terminal<B>,
    api: Api<P>,
    dir: PathBuf,
    app: App,
    first: Vec<Effect>,
    mut events: UnboundedReceiver<Event>,
    tick: Duration,
) -> Result<Option<ham_core::state::RunView>, String> {
    let (stamped_tx, stamped_rx) = tokio::sync::mpsc::unbounded_channel::<(Event, Instant)>();
    // What is queued already was sent before the loop started: it carries the one instant of the start.
    let start = Instant::now();
    while let Ok(event) = events.try_recv() {
        let _ = stamped_tx.send((event, start));
    }
    let forward = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if stamped_tx.send((event, Instant::now())).is_err() {
                break;
            }
        }
    });
    let result = event_loop_timed(terminal, api, dir, app, first, stamped_rx, tick).await;
    forward.abort();
    result
}

/// Draws, then waits for a terminal event (read at the instant it carries), an API answer or the timer, until an effect is `Quit` or the key channel closes.
/// Effects run as tasks, so a slow call never blocks keys or drawing. Everything already queued is handled before the next draw, and
/// the screen is drawn only after something that can change it (an event, an answer, a tick), so a paste that arrives as a burst of
/// keys costs one draw, not one per key.
pub async fn event_loop_timed<P: ControlPlane, B: Backend>(
    terminal: &mut Terminal<B>,
    api: Api<P>,
    dir: PathBuf,
    mut app: App,
    first: Vec<Effect>,
    mut events: UnboundedReceiver<(Event, Instant)>,
    tick: Duration,
) -> Result<Option<ham_core::state::RunView>, String> {
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    let dispatch = |effects: Vec<Effect>, quit: &mut bool| {
        for e in effects {
            if e == Effect::Quit {
                *quit = true;
                continue;
            }
            let (api, dir, tx) = (api.clone(), dir.clone(), msg_tx.clone());
            tokio::spawn(async move {
                let reads_runs = matches!(e, Effect::LoadRuns | Effect::RefreshRuns(_));
                if let Some(m) = perform(&api, &dir, e).await {
                    let _ = tx.send(m);
                }
                // A client with several backends hears how each answered after every read of the list.
                if reads_runs && let Some(view) = api.plane().backend_view() {
                    let _ = tx.send(Msg::Backends(Some(view)));
                }
            });
        }
    };
    let mut quit = false;
    dispatch(first, &mut quit);
    let mut ticker = tokio::time::interval(tick);
    let mut keys_left = std::env::var("HAMH_TEST_PANIC_AFTER_KEYS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    let mut dirty = true;
    // The last time the loop woke for a drag held outside the pane.
    let mut last_hold = Instant::now();
    loop {
        if dirty {
            app.set_now(unix_now());
            // The terminal may have lost part of its picture (focus back, Ctrl-L, a continue): the clear and the draw that follows it are one
            // pass, however many such events were queued.
            if app.take_repaint() {
                clear_screen(terminal);
            }
            terminal
                .draw(|f| app.draw(f))
                .map_err(|e| format!("terminal: {e}"))?;
            dirty = false;
        }
        if quit {
            return Ok(app.view());
        }
        // A terminal reports a drag only when the pointer moves, so a pointer held still outside the conversation sends nothing: the loop wakes
        // by itself, once per `mouse::AUTOSCROLL_EVERY`, only while such a drag is held. The floor keeps it from spinning if the pointer and
        // the app ever disagreed about when that is.
        let hold = app
            .autoscroll_due(Instant::now())
            .map(|due| due.max(last_hold + HOLD_FLOOR));
        let hold_at = tokio::time::Instant::from_std(
            hold.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
        );
        tokio::select! {
            ev = events.recv() => match ev {
                // The key reader ended (the terminal is gone): nobody can type any more, so the loop ends as a quit does. Without
                // this arm the loop would draw and poll for ever, with no key left to stop it.
                None => quit = true,
                Some((mut event, mut at)) => loop {
                    if matches!(event, Event::Key(_)) {
                        test_hook_key(&mut keys_left);
                    }
                    let (effects, changed) = on_event(&mut app, event, at);
                    dirty |= changed;
                    let effects = run_edits(terminal, &mut app, effects);
                    dispatch(effects, &mut quit);
                    if quit {
                        break;
                    }
                    if app.take_flush() {
                        // A preview was armed: the keys already queued were typed before it was drawn, so none of
                        // them may answer it. A resize still counts.
                        loop {
                            match events.try_recv() {
                                Ok((Event::Resize(w, h), at)) => {
                                    on_event(&mut app, Event::Resize(w, h), at);
                                }
                                // The window coming back still repaints; a key typed before the preview was drawn is the only thing dropped.
                                Ok((Event::FocusGained, at)) => {
                                    on_event(&mut app, Event::FocusGained, at);
                                }
                                Ok(_) => {}
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => {
                                    quit = true;
                                    break;
                                }
                            }
                        }
                        break;
                    }
                    match events.try_recv() {
                        Ok(more) => (event, at) = more,
                        Err(TryRecvError::Empty) => break,
                        // The reader ended with events still queued: they were handled, and nobody can type any more.
                        Err(TryRecvError::Disconnected) => {
                            quit = true;
                            break;
                        }
                    }
                },
            },
            Some(m) = msg_rx.recv() => {
                let mut effects = app.update(m);
                while let Ok(more) = msg_rx.try_recv() {
                    effects.extend(app.update(more));
                }
                dirty = true;
                let effects = run_edits(terminal, &mut app, effects);
                dispatch(effects, &mut quit);
            }
            _ = ticker.tick() => {
                let effects = app.tick();
                dirty = true;
                let effects = run_edits(terminal, &mut app, effects);
                dispatch(effects, &mut quit);
            }
            _ = tokio::time::sleep_until(hold_at), if hold.is_some() => {
                last_hold = Instant::now();
                let effects = app.autoscroll_at(last_hold);
                dirty = true;
                let effects = run_edits(terminal, &mut app, effects);
                dispatch(effects, &mut quit);
            }
        }
    }
}
