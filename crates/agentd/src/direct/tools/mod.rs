//! Module `tools`: the tool server of agentd and the hook that connects it to a run. A stateless
//! MCP server on `127.0.0.1` offers the engine typed tools (`code_search` first), so that an engine with an HTTP MCP client can use them without a
//! command in its shell.
//!
//! - [`server`]: `POST /mcp`, the methods and limits, the cancel, the restart of the serving task, the loopback rules.
//! - [`backend`]: [`backend::ToolBackend`], the context a call gets, the cancel signal and the helpers that end a child's process group.
//! - [`backends`]: the list of tools, one line each.
//! - [`result`]: what an answer becomes on the wire (content text only, the cap on the serialized form, the failure words first).
//! - [`settings`]: `AGENTD_TOOLS_MODE`, `AGENTD_TOOLS_DISABLE`, `AGENTD_TOOLS_PORT`.
//! - [`stats`]: the counters the run summary and the connect check read.
//! - [`register`]: the engine's registration file, written after the port is bound and written again when the engine damages it, never through a link.
//! - [`connect`]: the connect check, the word `none`, `handshake-only` or `ok` that says what the engine did with the registration.
//! - [`summary`]: the note in `summary.json` (`DirectSummary.tools`).
//!
//! The hook ([`ToolsHook`], registered by `Hooks::standard` right after `research`):
//!
//! - `on_startup`: with no `[mcp]` table in the engine's profile (or `via = "none"`) it does nothing at all. Otherwise it takes the backends
//!   ([`backends::all`], minus the tools `AGENTD_TOOLS_DISABLE` names; none left means no server), binds the port (3130, any free port when taken),
//!   starts the server, asks it a `ping` (so that the file never names a port another process serves), renders the profile's template and writes
//!   the registration file, and puts `HAMH_TOOLS_URL` in the engine's environment. Any failure leaves the run as it was: one log line, a summary note
//!   with a reason, no server, no file, no variable. It never ends a run.
//! - `on_event`: the first `PromptSent` of each session starts a connect clock; `SessionEnded` and `EngineStopped` put the registration file back
//!   when the engine edited, replaced or deleted it (the engine rewrites it after every connect and reads it again at each new session).
//! - `on_shutdown` (every ending, a refused startup included) stops the server and fixes the note; `on_summary` copies it into the summary.
//!
//! Rules: this directory, its tests (`crates/agentd/tests/direct_tools*.rs`, `tools_support/`, `hook_support/`), the example
//! `crates/agentd/examples/tools_server.rs` with its echo backend (`examples/tools_server_support/`), and the module's section of
//! `src/direct/AGENTS.md` are the module's.

pub mod backend;
pub mod backends;
pub mod connect;
pub mod register;
pub mod result;
pub mod server;
pub mod settings;
pub mod stats;
pub mod summary;

use crate::BoxFuture;
use crate::direct::compat;
use crate::direct::hooks::{
    CleanLaunch, DirectHook, Event, HookEnv, Shutdown, StartupCtx, SummaryCtx,
};
use crate::direct::tools::backend::{SpecFn, ToolBackend, ToolContext};
use crate::direct::tools::connect::{Connect, DEFAULT_WINDOW, Watch};
use crate::direct::tools::register::Target;
use crate::direct::tools::server::{ServerHandle, ServerState, Shared};
use crate::direct::tools::settings::{Mode, Settings};
use crate::direct::tools::stats::{Counters, Stats};
use crate::logging::LogSink;
use harness_protocol::engine_profile::{EngineMcp, McpVia};
use harness_protocol::tools::{DirectToolsNote, SERVER_NAME};
use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The tools of a run: `(hook environment, startup context) -> backends`. The product's is [`backends::all`]; a test gives its own.
pub type BackendFactory =
    Arc<dyn Fn(&HookEnv, &StartupCtx<'_>) -> Vec<Box<dyn ToolBackend>> + Send + Sync>;

/// The module, or `None` when `AGENTD_TOOLS_MODE` switches it off. `Hooks::standard` calls it once per run.
pub fn hook(env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    hook_for(env, Settings::from_env())
}

/// As [`hook`] with these settings. A mode word that is neither an on word nor an off word reads as off and the log says so.
pub fn hook_for(env: &HookEnv, settings: Settings) -> Option<Arc<dyn DirectHook>> {
    if settings.mode == Mode::Unrecognized {
        env.log
            .line("direct tools off: AGENTD_TOOLS_MODE is neither an on word nor an off word");
    }
    hook_with(settings)
}

/// The module for these settings, or `None` when they switch it off.
pub fn hook_with(settings: Settings) -> Option<Arc<dyn DirectHook>> {
    settings
        .enabled
        .then(|| Arc::new(ToolsHook::new(settings)) as Arc<dyn DirectHook>)
}

/// What `start_with` needs of the run: the hook environment, the run's directories, the engine's profile table and environment. The driver's
/// `StartupCtx` makes one in `on_startup`; a test makes its own.
#[derive(Debug)]
pub struct StartParts<'a> {
    pub env: &'a HookEnv,
    /// The repository of the run: the work tree the tools search.
    pub repo: &'a Path,
    pub data_dir: &'a Path,
    pub config_dir: &'a Path,
    /// Where the profile's template path is read from (`AGENTD_TEMPLATES_ROOT`).
    pub templates_root: &'a Path,
    /// `(uid, gid)` the engine runs as when agentd drops privileges.
    pub engine_uid: Option<(u32, u32)>,
    /// The `[mcp]` table of the engine's profile. `None`: an engine the layer has not been wired to.
    pub mcp: Option<&'a EngineMcp>,
    /// How a tool starts a command as the engine's user with a scrubbed environment.
    pub launch: CleanLaunch,
    /// The environment the engine will get: `HAMH_TOOLS_URL` is added when the registration is in place.
    pub engine_env: &'a mut BTreeMap<String, String>,
}

/// The registration as written: where, what and for whom. Kept so that the file can be put back.
#[derive(Debug)]
struct Registration {
    target: Target,
    /// The root directory of the target, opened before the engine ran and kept.
    root: OwnedFd,
    text: String,
    owner: Option<(u32, u32)>,
}

struct Run {
    handle: Option<ServerHandle>,
    shared: Arc<Shared>,
    reg: Arc<Registration>,
    list: Vec<String>,
}

#[derive(Default)]
struct State {
    run: Option<Run>,
    port: Option<u16>,
    /// The note of a module that decided to be off.
    off: Option<DirectToolsNote>,
    /// The note of a module whose server has been stopped.
    done: Option<DirectToolsNote>,
    done_stats: Option<Stats>,
    watch: Option<Watch>,
    timers: Vec<JoinHandle<()>>,
}

struct Inner {
    state: Mutex<State>,
    log: Mutex<Option<Arc<dyn LogSink>>>,
    t0: Instant,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn say(&self, line: &str) {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(l) = log {
            l.line(&format!("direct tools {line}"));
        }
    }

    /// Judges the clocks whose window is over.
    fn judge_due(&self) {
        let got = {
            let mut st = self.lock();
            let Some(stats) = st.run.as_ref().map(|r| r.shared.stats()) else {
                return;
            };
            let at = self.t0.elapsed();
            match st.watch.as_mut() {
                Some(w) => w.judge_due(at, &stats),
                None => return,
            }
        };
        for (_, word) in got {
            self.say(&format!("connect {}", word.as_str()));
        }
    }

    /// The note of the run as it stands (the server may still serve).
    fn live_note(st: &State) -> Option<DirectToolsNote> {
        let run = st.run.as_ref()?;
        let down = run.handle.as_ref().and_then(|h| match h.state() {
            ServerState::Down(why) => Some(why),
            ServerState::Up => None,
        });
        Some(summary::running(
            st.port.unwrap_or_default(),
            st.watch.as_ref().and_then(Watch::worst),
            run.list.clone(),
            &run.shared.stats(),
            down,
        ))
    }
}

/// The module.
pub struct ToolsHook {
    settings: Settings,
    factory: BackendFactory,
    window: Duration,
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ToolsHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolsHook").finish_non_exhaustive()
    }
}

impl ToolsHook {
    /// The product's module: the tools are [`backends::all`].
    pub fn new(settings: Settings) -> Self {
        Self::with_factory(settings, backends::all)
    }

    /// A module over these tools (tests).
    pub fn with_factory<F>(settings: Settings, factory: F) -> Self
    where
        F: Fn(&HookEnv, &StartupCtx<'_>) -> Vec<Box<dyn ToolBackend>> + Send + Sync + 'static,
    {
        Self {
            settings,
            factory: Arc::new(factory),
            window: DEFAULT_WINDOW,
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                log: Mutex::new(None),
                t0: Instant::now(),
            }),
        }
    }

    /// The time after the first prompt of a session in which the engine is expected to connect (30 s; a test shortens it).
    #[must_use]
    pub fn with_connect_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// The loopback port the server bound, once it has; it stays after the server stopped.
    pub fn port(&self) -> Option<u16> {
        self.inner.lock().port
    }

    /// The counters of the server (the last ones once it stopped).
    pub fn stats(&self) -> Option<Stats> {
        let st = self.inner.lock();
        st.run
            .as_ref()
            .map(|r| r.shared.stats())
            .or_else(|| st.done_stats.clone())
    }

    /// The worst connect word over the sessions judged so far.
    pub fn connect(&self) -> Option<Connect> {
        self.inner.lock().watch.as_ref().and_then(Watch::worst)
    }

    /// The note for `summary.json`: the final one once the server stopped, else what the run shows now, else the reason the module is off.
    /// `None` when the module decided nothing (a profile with no table).
    pub fn note(&self) -> Option<DirectToolsNote> {
        let st = self.inner.lock();
        st.done
            .clone()
            .or_else(|| Inner::live_note(&st))
            .or_else(|| st.off.clone())
    }

    /// Ends the serving task as a panic would (test hook).
    #[doc(hidden)]
    pub fn crash_server_for_tests(&self) {
        if let Some(h) = self
            .inner
            .lock()
            .run
            .as_ref()
            .and_then(|r| r.handle.as_ref())
        {
            h.crash_for_tests();
        }
    }

    /// What `on_shutdown` does (test hook): the connect clocks stop, the note is fixed and the server stops.
    #[doc(hidden)]
    pub async fn stop_for_tests(&self) {
        self.finish().await;
    }

    fn decide_off(&self, reason: &str) {
        self.inner.say(&format!("not started: {reason}"));
        self.inner.lock().off = Some(summary::off(reason));
    }

    /// Starts the module for a run. Never fails: every refusal is a log line and a note, and the run goes on without the tools.
    pub async fn start_with(&self, parts: StartParts<'_>, backends: Vec<Box<dyn ToolBackend>>) {
        let env = parts.env;
        *self.inner.log.lock().unwrap_or_else(|e| e.into_inner()) = Some(env.log.clone());
        if !self.settings.enabled {
            return;
        }
        // 1. the engine's profile: no table, or a table that takes no server, is an engine the layer has not been wired to; the module does nothing.
        let Some(mcp) = parts.mcp else {
            return;
        };
        if mcp.via == McpVia::None {
            return;
        }
        let Some(reg) = mcp.registration() else {
            self.decide_off("bad-profile");
            return;
        };
        // 2. the tools: the disabled ones are out; with none left there is nothing to offer.
        let built = backends.len();
        let backends = backends::retain_enabled(backends, &self.settings);
        if backends.is_empty() {
            // Nothing is built (this stage's list is empty) is expected; everything built was switched off by `AGENTD_TOOLS_DISABLE` is a choice.
            self.decide_off(if built == 0 {
                "no-tools"
            } else {
                "all-disabled"
            });
            return;
        }
        // 3. the registration's pieces, checked before anything is bound.
        let Some(max_chars) = usize::try_from(reg.max_result_chars).ok() else {
            self.decide_off("bad-profile");
            return;
        };
        let target = match register::target(reg.path, parts.data_dir, parts.config_dir) {
            Ok(t) => t,
            Err(_) => {
                self.decide_off("bad-registration");
                return;
            }
        };
        // 4. the port, then the server on it.
        let listener = match server::bind(self.settings.port).await {
            Ok(l) => l,
            Err(_) => {
                self.decide_off("bind-failed");
                return;
            }
        };
        let Ok(addr) = listener.local_addr() else {
            self.decide_off("bind-failed");
            return;
        };
        let port = addr.port();
        let launch = parts.launch.clone();
        let spec: SpecFn = Arc::new(move |argv, stderr| launch.spec(argv, stderr));
        let context_dir = parts.engine_env.get("HAMH_CONTEXT_DIR").map(PathBuf::from);
        let ctx = ToolContext::new(parts.repo.to_path_buf(), context_dir, spec, Counters::new());
        let shared = Shared::new(backends, ctx, env.log.clone(), max_chars);
        let list: Vec<String> = shared.listed().iter().map(|s| (*s).to_string()).collect();
        let handle = server::start(listener, shared.clone());
        // 5. the file names this port only when this server answers on it.
        if !handle.self_check().await {
            handle.stop().await;
            self.decide_off("self-check-failed");
            return;
        }
        let url = format!("http://127.0.0.1:{port}/mcp");
        let text = match read_template(parts.templates_root, reg.template)
            .and_then(|t| register::render(&t, SERVER_NAME, &url))
        {
            Ok(t) => t,
            Err(_) => {
                handle.stop().await;
                self.decide_off("bad-registration");
                return;
            }
        };
        let written = target.open_root().and_then(|root| {
            register::write(&target, &root, &text, parts.engine_uid).map(|n| (root, n))
        });
        let (root, removed) = match written {
            Ok(done) => done,
            Err(_) => {
                handle.stop().await;
                self.decide_off("registration-failed");
                return;
            }
        };
        // 6. the engine is told where the server is, and the run is on.
        // COMPAT-CAH-DATA: both prefixes (rules the user edited before the rename name the earlier one).
        compat::set_both(parts.engine_env, "TOOLS_URL", url);
        env.log.line(&format!(
            "direct tools server on 127.0.0.1:{port} ({} tool(s))",
            list.len()
        ));
        self.inner.say(&format!(
            "registration written ({} bytes, {removed} stale file(s) removed)",
            text.len()
        ));
        let mut st = self.inner.lock();
        st.port = Some(port);
        st.watch = Some(Watch::new(self.window));
        st.run = Some(Run {
            handle: Some(handle),
            shared,
            reg: Arc::new(Registration {
                target,
                root,
                text,
                owner: parts.engine_uid,
            }),
            list,
        });
    }

    /// The first prompt of a session starts its connect clock. The timer is made and kept under the same lock that `finish` takes, so a timer
    /// cannot slip out after the module's end.
    fn prompt_sent(&self, session: &str) {
        let started = {
            let mut st = self.inner.lock();
            let Some(stats) = st.run.as_ref().map(|r| r.shared.stats()) else {
                return;
            };
            let at = self.inner.t0.elapsed();
            let started = st
                .watch
                .as_mut()
                .is_some_and(|w| w.prompt_sent(session, at, &stats));
            if started && let Ok(rt) = tokio::runtime::Handle::try_current() {
                let (inner, window) = (self.inner.clone(), self.window);
                st.timers.push(rt.spawn(async move {
                    tokio::time::sleep(window).await;
                    inner.judge_due();
                }));
            }
            started
        };
        if started {
            // The engine reads its file when this prompt reaches it: whatever happened to the file since the last session ended is undone first.
            self.restore(false);
        }
    }

    /// A session ended or the engine's process was stopped: the clocks of the engine that went away are closed (see [`Watch::close`]).
    fn close_clocks(&self, only: Option<&str>) {
        let got = {
            let mut st = self.inner.lock();
            let Some(stats) = st.run.as_ref().map(|r| r.shared.stats()) else {
                return;
            };
            let at = self.inner.t0.elapsed();
            match st.watch.as_mut() {
                Some(w) => w.close(only, at, &stats),
                None => return,
            }
        };
        for (_, word) in got {
            self.inner.say(&format!("connect {}", word.as_str()));
        }
    }

    /// Puts the registration file back when it differs. `engine_gone`: the engine's process group was killed, so the lock it may have left is stale.
    fn restore(&self, engine_gone: bool) {
        let reg = self.inner.lock().run.as_ref().map(|r| r.reg.clone());
        let Some(reg) = reg else {
            return;
        };
        match register::restore(&reg.target, &reg.root, &reg.text, reg.owner, engine_gone) {
            Ok(done) => {
                if done.rewritten {
                    self.inner.say("registration restored");
                }
                if done.stale_removed > 0 {
                    self.inner
                        .say(&format!("{} stale file(s) removed", done.stale_removed));
                }
            }
            Err(why) => self
                .inner
                .say(&format!("registration could not be restored ({why})")),
        }
    }

    /// The end of the module: the clocks stop, the server stops, and then the counters and the connect words that are certain are fixed in the
    /// note (what the server counted is final only once it no longer serves).
    async fn finish(&self) {
        let (handle, shared, down) = {
            let mut st = self.inner.lock();
            for t in st.timers.drain(..) {
                t.abort();
            }
            let Some(run) = st.run.as_mut() else {
                return;
            };
            let down = run.handle.as_ref().and_then(|h| match h.state() {
                ServerState::Down(why) => Some(why),
                ServerState::Up => None,
            });
            (run.handle.take(), run.shared.clone(), down)
        };
        if let Some(h) = handle {
            h.stop().await;
        }
        let stats = shared.stats();
        let got = {
            let mut st = self.inner.lock();
            let Some(run) = st.run.take() else {
                return;
            };
            let got = match st.watch.as_mut() {
                Some(w) => w.finish(self.inner.t0.elapsed(), &stats),
                None => Vec::new(),
            };
            st.done = Some(summary::running(
                st.port.unwrap_or_default(),
                st.watch.as_ref().and_then(Watch::worst),
                run.list.clone(),
                &stats,
                down,
            ));
            st.done_stats = Some(stats);
            got
        };
        for (_, word) in got {
            self.inner.say(&format!("connect {}", word.as_str()));
        }
    }
}

/// The profile's template under the templates root. The path is relative and plain: it never leaves the root.
fn read_template(root: &Path, rel: &str) -> Result<String, String> {
    let rel = Path::new(rel);
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("template-path".to_string());
    }
    std::fs::read_to_string(root.join(rel)).map_err(|e| format!("template: {:?}", e.kind()))
}

impl DirectHook for ToolsHook {
    fn name(&self) -> &'static str {
        "tools"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let backends =
                if self.settings.enabled && ctx.mcp().is_some_and(|m| m.via == McpVia::File) {
                    (self.factory)(ctx.env, &*ctx)
                } else {
                    Vec::new()
                };
            let parts = StartParts {
                env: ctx.env,
                repo: ctx.repo,
                data_dir: ctx.data_dir,
                config_dir: ctx.config_dir,
                templates_root: &ctx.env.cfg.templates_root,
                engine_uid: ctx.engine_uid,
                mcp: ctx.mcp(),
                launch: ctx.clean_launch(),
                engine_env: &mut *ctx.engine_env,
            };
            self.start_with(parts, backends).await;
            Ok(())
        })
    }

    fn on_event(&self, event: &Event<'_>) {
        match event {
            Event::PromptSent(p) => self.prompt_sent(p.session_id),
            Event::SessionEnded(e) => {
                self.close_clocks(Some(e.session_id));
                self.restore(false);
            }
            Event::EngineStopped(_) => {
                self.close_clocks(None);
                self.restore(true);
            }
            _ => {}
        }
    }

    fn on_shutdown<'a>(&'a self, _end: &'a Shutdown<'a>) -> BoxFuture<'a, ()> {
        Box::pin(self.finish())
    }

    fn on_summary<'a>(&'a self, ctx: SummaryCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(note) = self.note() {
                ctx.summary.tools = Some(note);
            }
        })
    }
}
