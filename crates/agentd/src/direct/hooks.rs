//! The module seam of the direct driver. A module is a piece of the run that watches the engine or steers it and lives
//! in its own directory of `crate::direct` (`transcript`, `messages`, `goal`, `research`, `skills`, `continue_from`). It implements
//! [`DirectHook`], returns itself from its `hook()` constructor, and is registered by [`Hooks::standard`], which no module branch edits.
//! The driver calls the hooks at the points below and does nothing else for a module. With no module registered ([`Hooks::none`]) every
//! call is a no-op that awaits nothing and installs nothing, and the driver behaves as it did before the seam (the golden streams of
//! `tests/direct_golden.rs` pin that).
//!
//! Where the driver calls a hook, in the order of a run:
//!
//! | hook | call site | may change |
//! |---|---|---|
//! | [`DirectHook::on_restore`] | before the repository is restored from its bundle | the bundle URI |
//! | [`DirectHook::on_startup`] | rules placed, configuration directory protected, credential socket up, before the rules preflight | engine environment, session directory; may refuse the run |
//! | [`DirectHook::on_wire`] | every JSON-RPC message of the engine connection, both directions | nothing |
//! | [`DirectHook::on_event`] | session start and end, prompt sent and answered, the retry ladder, a cancel written, the engine stopped, the run ended | nothing |
//! | [`DirectHook::on_controls`] | a batch of controls from the internal API | nothing |
//! | [`DirectHook::on_turn_end`] | a prompt (with its retries) ended: is the run done? (also after a module's [`DriverCtl::interrupt`]) | the next prompt, the end of the run, or a hold ([`TurnVote::Wait`]) |
//! | [`DirectHook::on_seal`] | everything else says the run is over: the one atomic last question | the next prompt |
//! | [`DirectHook::on_shutdown`] | the engine is gone, before the tree is committed and uploaded | nothing |
//! | [`DirectHook::on_summary`] | `summary.json` is built, before it is checked and uploaded | the summary |
//! | [`DirectHook::on_report`] | the delivered head is committed, bundled and uploaded and `summary.json` is in the bucket; the package proxy, the container runtime and the flusher (the heartbeat) still run | one refused create-only report write may signal an upload conflict; otherwise nothing |
//! | [`DirectHook::on_finalize`] | just before `turn-complete` is posted | the `turn-complete` request |
//!
//! Rules every module follows (the driver enforces what it can):
//!
//! - A module never fails or hangs the run by accident. A panic is caught and the module is switched off for the rest of the run (one log
//!   line); an asynchronous hook that does not answer within its limit ([`Limits`]) is cut off and switched off. The only call that may
//!   stop a run is `on_startup` (an `Err`, a panic or a timeout end the run as "did not start" before any model call) and a
//!   [`TurnVote::Stop`] the module chose. The report module has one narrower hard result: when its signed create-only write finds
//!   `test-report.json` already present, it records one upload-conflict event and signals that object to the driver.
//! - [`DirectHook::on_wire`] and [`DirectHook::on_event`] are called on the read path of the engine connection. They copy and queue and
//!   never wait, compress, upload or take a lock another task holds for long.
//! - `on_shutdown` and `on_finalize` run for EVERY ending, including a failed startup, a cancel and a deadline: that is where a module
//!   flushes what it holds and closes what it opened (an admission window, a server). `on_turn_end` and `on_seal` run only while the
//!   run can still continue: the prompt was answered (or an interrupt was acknowledged), nothing failed before it, the user's cancel is
//!   not up and time is left.
//! - Votes: modules are asked in registration order. [`TurnVote::Stop`] ends the poll at once (modules after it are not asked, so a module
//!   that claims something when it votes claims nothing). A [`TurnVote::Prompt`] is kept, a later one replaces it, so the last module has
//!   priority among prompts; [`TurnVote::Done`] changes nothing. The order is in [`Hooks::standard`] and in `AGENTS.md`.
//! - Event ordering is by call: a module sees the events of a run in the order the driver raised them.

use crate::BoxFuture;
use crate::acp::{AcpError, Dir, Interrupt, WireTap};
use crate::config::Config;
use crate::creds::CredStore;
use crate::direct::classify::{Classified, RawEnd};
use crate::engine::LaunchSpec;
use crate::events::{EventRecorder, TurnControls};
use crate::logging::LogSink;
use crate::sandbox::Sandbox;
use crate::secrets::SecretSet;
use crate::server::App;
use harness_protocol::api::internal::{PendingControl, TurnCompleteRequest};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::common::Usage;
use harness_protocol::direct::{DirectInvocationRequest, DirectSummary, DirectSwitch, s3};
use harness_protocol::engine_profile::EngineMcp;
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

// ---- what a module is given ---------------------------------------------------------------------------------------------------

/// Everything a module may keep from the start of a run: who the run is, where its files and its bucket are, the session credentials
/// store (modules call S3 and the internal API with the run's own credentials), the event recorder and the log. Made once, at the top of
/// the invocation, and handed to every module constructor ([`Hooks::standard`]).
#[derive(Clone)]
pub struct HookEnv {
    pub run_id: String,
    pub agent_id: String,
    /// The turn of the invocation, which is the slice of the run (1 for a run of one session). From 2 on the work tree was restored from
    /// an earlier slice, not seeded from the run's base.
    pub turn: u32,
    /// The engine profile named in the request. It is data: no module branches on a product name.
    pub engine: String,
    pub region: String,
    pub bucket: String,
    /// The internal API of the run (`AGENTD_INTERNAL_API_ENDPOINT` replaces the invocation's URL in tests).
    pub internal_api_url: String,
    pub s3_endpoint: Option<String>,
    /// `<AGENTD_DATA_ROOT>/<run>/direct`: root-owned, outside the work tree.
    pub agent_data: PathBuf,
    /// agentd's settings. A module reads its own settings from `AGENTD_<MODULE>_*` environment variables, not from here.
    pub cfg: Arc<Config>,
    pub creds: Arc<CredStore>,
    pub rec: Arc<EventRecorder>,
    pub log: Arc<dyn LogSink>,
    /// The run's secret values: a module that writes engine
    /// text into an event or a stored object passes it through this set first.
    pub secrets: SecretSet,
    /// The controls of the run: `subscribe()` is the user's cancel.
    pub controls: Arc<TurnControls>,
    /// What a module may ask of the driver while a prompt is open.
    pub ctl: DriverCtl,
}

/// What a module may ask of the driver: cancel the prompt that is open ([`DriverCtl::interrupt`]), and read the process id of the
/// engine.
#[derive(Clone, Debug, Default)]
pub struct DriverCtl(Interrupt, Arc<std::sync::atomic::AtomicU32>);

impl DriverCtl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the `session/prompt` that is open now: the client writes `session/cancel` once, waits for the prompt's own answer (the
    /// engine gets agentd's 10 s grace), and the stage ends. The module is then asked at its `on_turn_end` with `TurnEnd.interrupted`
    /// set, whether the engine answered or ran out of grace (`raw.watched` says which), and answers with the prompt that follows (or a
    /// `Stop`). It is not the run's cancel: the user's `ham stop` stays final, wins when both are up, and one `session/cancel` is
    /// written for a prompt however many are asked. `false` when no prompt is open (between prompts, during a retry wait, a fresh
    /// session's setup): nothing was asked and nothing is queued, so a message goes the other way (next turn end).
    ///
    /// The module owns the rest of the policy: claiming its message before it asks, reaping the tool processes the engine left (the
    /// `EngineStopped` event does not cover a cancel, which leaves the engine running), and what to do when the grace runs out.
    pub fn interrupt(&self) -> bool {
        self.0.interrupt()
    }

    /// Whether a `session/prompt` is open now.
    pub fn prompt_open(&self) -> bool {
        self.0.prompt_open()
    }

    /// Records the process id of the engine.
    pub fn set_engine_pid(&self, pid: u32) {
        self.1.store(pid, std::sync::atomic::Ordering::SeqCst);
    }

    /// The process id of the engine, `None` before it started.
    pub fn engine_pid(&self) -> Option<u32> {
        Some(self.1.load(std::sync::atomic::Ordering::SeqCst)).filter(|pid| *pid != 0)
    }

    /// The cell the process id is kept in (0: none), shared by every copy of the control.
    pub fn engine_pid_cell(&self) -> Arc<std::sync::atomic::AtomicU32> {
        self.1.clone()
    }

    pub(crate) fn client_side(&self) -> Interrupt {
        self.0.clone()
    }
}

impl std::fmt::Debug for HookEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookEnv")
            .field("run_id", &self.run_id)
            .field("agent_id", &self.agent_id)
            .field("engine", &self.engine)
            .field("agent_data", &self.agent_data)
            .finish_non_exhaustive()
    }
}

impl HookEnv {
    /// The environment of one invocation.
    pub fn new(
        app: &App,
        req: &DirectInvocationRequest,
        rec: &Arc<EventRecorder>,
        controls: &Arc<TurnControls>,
    ) -> Self {
        Self {
            run_id: req.run_id.clone(),
            agent_id: harness_protocol::direct::DIRECT_AGENT_ID.to_string(),
            turn: req.turn,
            engine: req.engine.clone(),
            region: req.region.clone(),
            bucket: s3::split_uri(&req.request.uri)
                .map(|(b, _)| b.to_string())
                .unwrap_or_default(),
            internal_api_url: app
                .cfg
                .internal_api_endpoint
                .clone()
                .unwrap_or_else(|| req.internal_api_url.clone()),
            s3_endpoint: app.cfg.s3_endpoint.clone(),
            agent_data: app
                .cfg
                .data_root
                .join(&req.run_id)
                .join(harness_protocol::direct::DIRECT_AGENT_ID),
            cfg: Arc::new(app.cfg.clone()),
            creds: app.creds.clone(),
            rec: rec.clone(),
            log: app.log.clone(),
            secrets: SecretSet::default(),
            controls: controls.clone(),
            ctl: DriverCtl::new(),
        }
    }

    /// The environment with the run's secret set (`run_with` gives it the set the credential store keeps current).
    #[must_use]
    pub fn with_secrets(mut self, secrets: SecretSet) -> Self {
        self.secrets = secrets;
        self
    }
}

/// `on_restore`: the repository the run is about to restore. A module that seeds a run from an earlier run's delivered head names that
/// bundle here; the run restores `repo_uri` as it stands when every module has been asked.
#[derive(Debug)]
pub struct RestoreCtx<'a> {
    pub env: &'a HookEnv,
    /// The request the run fetched (`runs/<run>/input/request.json`), checked and parsed.
    pub request: &'a StartDirectRunRequest,
    pub repo_uri: &'a mut String,
}

/// `on_startup`: the run is about to start its engine. The engine's configuration directory is already root-owned and holds the rules; a
/// module that writes there writes as root, files read-only (0444) and directories 0555, so the engine can read and change nothing.
/// Nothing the module puts in `engine_env` or `session_cwd` is visible to the engine before it starts.
#[derive(Debug)]
pub struct StartupCtx<'a> {
    pub env: &'a HookEnv,
    pub request: &'a StartDirectRunRequest,
    /// The repository of the run (the work tree's root).
    pub repo: &'a Path,
    pub config_dir: &'a Path,
    pub data_dir: &'a Path,
    pub home: &'a Path,
    pub tmp: &'a Path,
    /// `(uid, gid)` the engine runs as, when agentd drops privileges; `None` when it runs as agentd's own user.
    pub engine_uid: Option<(u32, u32)>,
    /// The Landlock sandbox every process of the engine's family starts in; `None` in the `uid` mode.
    pub sandbox: Option<Sandbox>,
    /// The environment the engine will get. A change reaches the engine and every command listed through [`Self::launch_spec`] after it.
    pub engine_env: &'a mut BTreeMap<String, String>,
    /// The cargo `config.toml` the package proxy gave this run (crates.io replaced by the proxy's registry; no secret), as agentd wrote it for the
    /// engine; `None` when the run has no proxy. A module that runs cargo for the run (the report steps) uses this text and never the file the
    /// engine can change.
    pub cargo_config: Option<&'a str>,
    /// The pids of the container runtime's long-lived processes, read when the runtime was started and before the engine existed;
    /// empty when the run has no runtime or the launcher cannot name them. A module that stops processes of the engine's user leaves these,
    /// and everything below them, alone.
    pub runtime_pids: Vec<u32>,
    /// The directory of `session/new`: the repository's git top level unless a module names another.
    pub session_cwd: &'a mut Option<String>,
    /// Set by a module that sends follow-up prompts of its own (the goal loop): the harness's bounded nudge is off for the run, so two
    /// loops do not take turns. A module that leaves it alone changes nothing; a run whose module never set it (the module went off at startup,
    /// or is not registered) sends the nudge exactly as before.
    pub nudge_off: bool,
    argv: &'a [String],
    engine_path: &'a [PathBuf],
    mcp: Option<&'a EngineMcp>,
}

impl<'a> StartupCtx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        env: &'a HookEnv,
        request: &'a StartDirectRunRequest,
        repo: &'a Path,
        config_dir: &'a Path,
        data_dir: &'a Path,
        home: &'a Path,
        tmp: &'a Path,
        engine_uid: Option<(u32, u32)>,
        sandbox: Option<Sandbox>,
        engine_env: &'a mut BTreeMap<String, String>,
        session_cwd: &'a mut Option<String>,
        argv: &'a [String],
        engine_path: &'a [PathBuf],
        mcp: Option<&'a EngineMcp>,
        cargo_config: Option<&'a str>,
    ) -> Self {
        Self {
            env,
            request,
            repo,
            config_dir,
            data_dir,
            home,
            tmp,
            engine_uid,
            sandbox,
            engine_env,
            cargo_config,
            runtime_pids: Vec::new(),
            session_cwd,
            nudge_off: false,
            argv,
            engine_path,
            mcp,
        }
    }

    /// The `[mcp]` table of the engine's profile: how the engine learns of the harness's tool server (protocol 1.40.0). `None` for a profile that
    /// has no table, an engine the tool layer has not been wired to.
    pub fn mcp(&self) -> Option<&'a EngineMcp> {
        self.mcp
    }

    /// An owned copy of what a command run as the engine's user needs, with the environment scrubbed (see [`CleanLaunch`]). The tool server keeps
    /// it for the whole run: a tool starts its commands from it long after this call has returned.
    pub fn clean_launch(&self) -> CleanLaunch {
        CleanLaunch::new(
            self.engine_env,
            self.repo.to_path_buf(),
            self.engine_path.to_vec(),
            self.data_dir.to_path_buf(),
            self.engine_uid,
        )
        .with_sandbox(self.sandbox.clone())
    }

    /// A one-off command as the engine's user with a scrubbed environment ([`CleanLaunch`]): no `AWS_*` and nothing else the engine's own variables
    /// carry, the engine's search path, the work tree as the working directory. `stderr_name` is a file name in the data directory.
    pub fn clean_spec(&self, argv: Vec<String>, stderr_name: &str) -> LaunchSpec {
        self.clean_launch().spec(argv, stderr_name)
    }

    /// The engine's own launch command and arguments, as the run will start it.
    pub fn engine_argv(&self) -> &[String] {
        self.argv
    }

    /// A one-off command of the engine's binary (a listing, a check) as the engine would run it: the run's environment, working directory
    /// and user. `stderr_name` is a file name in the data directory. Run it with a bound (a module's call is cut off at
    /// [`Limits::startup`] whatever it does).
    pub fn launch_spec(&self, argv: Vec<String>, stderr_name: &str) -> LaunchSpec {
        LaunchSpec {
            argv,
            env: self.engine_env.clone(),
            cwd: self.repo.to_path_buf(),
            path: self.engine_path.to_vec(),
            stderr_log: self.data_dir.join(stderr_name),
            run_as: self.engine_uid,
            sandbox: self.sandbox.clone(),
        }
    }
}

/// What a command started on behalf of the engine (a tool of the harness's tool server) is given: the engine's user, the work tree as the working
/// directory, the engine's search path, and an environment that holds only the variables of the allow-list below. The engine's own environment
/// carries the AWS configuration of the run, the model pin and the research URL; a tool's child gets none of them. Owned and cheap to clone, so
/// that a tool can keep it for the run.
#[derive(Clone, Debug)]
pub struct CleanLaunch {
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    path: Vec<PathBuf>,
    data_dir: PathBuf,
    run_as: Option<(u32, u32)>,
    sandbox: Option<Sandbox>,
}

/// The variables of the engine's environment that a tool's child keeps: where it may write and the user's locale. Nothing else is copied, so no
/// `AWS_*` variable, no credential-shaped variable and no variable of the harness can reach a child of a tool.
pub const CLEAN_ENV_KEEP: [&str; 8] = [
    "HOME", "TMPDIR", "USER", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ",
];

impl CleanLaunch {
    pub fn new(
        engine_env: &BTreeMap<String, String>,
        cwd: PathBuf,
        path: Vec<PathBuf>,
        data_dir: PathBuf,
        run_as: Option<(u32, u32)>,
    ) -> Self {
        Self {
            env: engine_env
                .iter()
                .filter(|(k, _)| CLEAN_ENV_KEEP.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            cwd,
            path,
            data_dir,
            run_as,
            sandbox: None,
        }
    }

    /// The same launch in a Landlock sandbox: a child of a tool is confined as the engine is.
    pub fn with_sandbox(mut self, sandbox: Option<Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// The environment a child gets.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// The launch of `argv`; `stderr_name` is a file name in the data directory.
    pub fn spec(&self, argv: Vec<String>, stderr_name: &str) -> LaunchSpec {
        LaunchSpec {
            argv,
            env: self.env.clone(),
            cwd: self.cwd.clone(),
            path: self.path.clone(),
            stderr_log: self.data_dir.join(stderr_name),
            run_as: self.run_as,
            sandbox: self.sandbox.clone(),
        }
    }
}

/// One JSON-RPC message of the connection to the engine.
#[derive(Debug)]
pub struct Wire<'a> {
    /// 1 for the first message of the run, then one more for each, in both directions: no gap, no repeat.
    pub seq: u64,
    pub dir: Dir,
    pub msg: &'a Value,
    /// The message is from a session the run abandoned (a fresh session replaced it).
    pub stale: bool,
}

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    /// A fresh session replaced it.
    Replaced,
    /// The run ended in it.
    RunEnded,
}

/// A session exists and the engine took the model the run pinned.
#[derive(Debug)]
pub struct SessionStarted<'a> {
    pub session_id: &'a str,
    /// 1 for the first session of the run, 2 after a fresh session, and so on.
    pub number: u32,
    /// The `session/new` result as the engine sent it.
    pub response: &'a Value,
    pub cwd: &'a str,
    /// Why a fresh session was started (`harness`, `user`, `analyzer`); `None` for the first.
    pub trigger: Option<&'a str>,
}

#[derive(Debug)]
pub struct SessionEnded<'a> {
    pub session_id: &'a str,
    pub number: u32,
    pub why: SessionEnd,
}

/// A `session/prompt` is about to be written.
#[derive(Debug)]
pub struct PromptSent<'a> {
    /// 1 for the first prompt of the run, counted across sessions.
    pub no: u32,
    pub session_id: &'a str,
    /// The text, byte for byte.
    pub text: &'a str,
}

/// A `session/prompt` was answered or failed.
#[derive(Debug)]
pub struct PromptReplied<'a> {
    pub no: u32,
    pub session_id: &'a str,
    pub stop_reason: Option<&'a str>,
    pub usage: Option<Usage>,
    pub error: Option<&'a AcpError>,
}

/// The entry points of the retry and failover ladder, raised from the places that already record them as events.
#[derive(Debug)]
pub enum LadderEvent<'a> {
    /// A provider failure ended a try and another follows after `backoff_s` seconds.
    ProviderRetry {
        attempt: u32,
        of: u32,
        class: &'a str,
        backoff_s: u64,
        detail: &'a str,
    },
    /// The engine was switched to another route or model.
    Switch(&'a DirectSwitch),
    /// The engine stopped the run itself and one more try follows.
    EngineStop {
        hint: &'a str,
        attempt: u32,
        of: u32,
        backoff_s: u64,
        detail: &'a str,
    },
    /// Every route failed: a pass of waiting before the next.
    Wait {
        pass: u32,
        wait_s: u64,
        detail: &'a str,
    },
    /// Every route has been down for 15 minutes.
    Outage { waited_s: u64 },
    /// Every route has been down for the notice period; the run goes on waiting.
    OutageNotice { waited_s: u64 },
    /// A prompt succeeded after an outage notice.
    OutageRecovered { waited_s: u64 },
    /// A fact the recovery ladder tells its readers (`history_unusable`, ...).
    Notify { reason: &'a str, detail: &'a str },
    /// A silence ended a stage and the run resumes the session once.
    SilenceResume { detail: &'a str },
}

/// Why an engine process was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineStop {
    /// The run is over: the process was waited for and, if it was still there, its group was killed.
    Reaped,
    /// A fresh session needs a new process: the old group was killed and the same launch starts again.
    Restarted,
}

/// What the driver tells a module besides the wire.
#[derive(Debug)]
pub enum Event<'a> {
    SessionStarted(SessionStarted<'a>),
    SessionEnded(SessionEnded<'a>),
    PromptSent(PromptSent<'a>),
    PromptReplied(PromptReplied<'a>),
    Ladder(LadderEvent<'a>),
    /// agentd wrote `session/cancel` (the user's, a deadline's, a stall's): raised after the message was seen on the wire.
    Cancel {
        session_id: &'a str,
    },
    /// An engine process was stopped. Tool processes it started in their own session survive a group kill: this is the moment to reap them.
    EngineStopped(EngineStop),
    /// The invocation is over: `turn-complete` was posted and `result.json` written.
    RunEnded,
}

impl Event<'_> {
    /// A short word for the event, for logs and tests.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionStarted(_) => "session-started",
            Self::SessionEnded(_) => "session-ended",
            Self::PromptSent(_) => "prompt-sent",
            Self::PromptReplied(_) => "prompt-replied",
            Self::Ladder(_) => "ladder",
            Self::Cancel { .. } => "cancel",
            Self::EngineStopped(_) => "engine-stopped",
            Self::RunEnded => "run-ended",
        }
    }
}

/// A stage ended: one prompt with its retries, answered or not.
#[derive(Debug)]
pub struct TurnEnd<'a> {
    /// 1 for the first stop of the run, counting every stage.
    pub n: u32,
    pub session_id: Option<&'a str>,
    pub session: u32,
    /// How the stage ended, as the stop table reads it; `raw.message` is the engine's last message (known secrets removed).
    pub raw: &'a RawEnd,
    pub classified: &'a Classified,
    /// Files that differ from the base, when the tree could be measured.
    pub files_changed: Option<u32>,
    /// A module interrupted the prompt ([`DriverCtl::interrupt`]): the engine answered `cancelled`, or did not answer within the grace
    /// (`raw.watched` says which). The stage ended because of the module, not the user.
    pub interrupted: bool,
    /// What is left of the run's deadline.
    pub time_left: Duration,
    pub tool_calls: u64,
}

/// What a module says at a turn end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnVote {
    /// No opinion: the run goes on as the driver decides (today's behaviour).
    Done,
    /// Send this text as the next prompt, in the same session (an attempt of kind `instruction`), then ask again.
    Prompt(String),
    /// End the run now. It reads "stopped" with this detail (`stopped_other`, [`crate::direct::classify::stopped_by_module`]).
    Stop { detail: String },
    /// The engine is still working on its own (a loop it started: the goal loop) and the run must not end yet. The driver keeps the run open:
    /// it reads the engine's pipe for `until` (what it reads reaches the collector and the wire tap), applies the deadline, the silence limit
    /// and the user's cancel as it does for a prompt, and then asks the modules again (the last message of the run is the one the engine said
    /// last). A [`TurnVote::Prompt`] of any module replaces a `Wait` whichever module votes first, a [`TurnVote::Stop`] beats it, of two
    /// `Wait`s the shorter is kept, and at least [`MIN_WAIT`]. Only [`DirectHook::on_turn_end`] may wait: at the seal a `Wait` is read as
    /// [`TurnVote::Done`].
    Wait { until: Duration },
}

/// The shortest time a [`TurnVote::Wait`] holds the run for.
pub const MIN_WAIT: Duration = Duration::from_millis(10);

/// `on_shutdown`: the engine is gone and nothing else writes the work tree.
#[derive(Debug)]
pub struct Shutdown<'a> {
    pub env: &'a HookEnv,
    /// How the run ended, before the tree was measured.
    pub classified: &'a Classified,
}

/// `on_summary`: `summary.json` is built and not yet checked or uploaded. A module adds its own members to it (the skills a session was
/// given, `DirectSummary.skills`); it changes nothing the driver wrote.
#[derive(Debug)]
pub struct SummaryCtx<'a> {
    pub env: &'a HookEnv,
    pub classified: &'a Classified,
    pub summary: &'a mut DirectSummary,
}

/// `on_report`: the work is delivered. The tree is committed, its bundle is in the bucket and `summary.json` is written, so the label and the head
/// already stored there are durable if the session is lost during this step. The package proxy and the container runtime still run and the
/// flusher still posts the run's heartbeat, so a module can work on the head for a while. `turn.ended` and `turn-complete` come after. A module
/// that makes a report about the delivered head writes it to the run's bucket here. A refused create-only report write signals one conflict to
/// the driver; that replaces the final classification without rewriting the summary already stored. Called only when a head was delivered.
#[derive(Debug)]
pub struct Report<'a> {
    pub env: &'a HookEnv,
    /// How the run ended before the report step. Only a refused create-only report write can replace it afterwards.
    pub classified: &'a Classified,
    /// The commit `turn-complete` will report as `lastCommitSha`.
    pub head_sha: &'a str,
    /// The bundle of `refs/heads/harness/<run>/direct` as agentd wrote it, in agentd's own (root-owned) data directory.
    pub bundle: &'a Path,
    /// Where the same bundle is in the run's bucket.
    pub bundle_uri: &'a str,
    /// What is left of the run's deadline (zero when it is past).
    pub time_left: Duration,
    conflict: &'a std::sync::OnceLock<&'static str>,
    pin: &'a std::sync::OnceLock<harness_protocol::output_pins::OutputPin>,
}

/// What the modules' report step left for the driver: the first create-only conflict a module signalled, and the record of the report object
/// agentd stored.
#[derive(Debug, Default)]
pub struct ReportOutcome {
    pub conflict: Option<&'static str>,
    pub pin: Option<harness_protocol::output_pins::OutputPin>,
}

impl Report<'_> {
    /// Records what S3 answered to this module's own write of its report object: the SHA-256 of the bytes it sent and the version id.
    /// The driver sends it in `turn-complete`, so delivery reads that version of the report and no later one.
    pub(crate) fn stored(&self, pin: harness_protocol::output_pins::OutputPin) {
        let _ = self.pin.set(pin);
    }

    /// Signals that this module's create-only end-object write found the key already taken. The first such object is the hard conflict;
    /// the module records the one `upload-conflict` event itself.
    pub(crate) fn upload_conflict(&self, object: &'static str) {
        let _ = self.conflict.set(object);
    }
}

/// `on_finalize`: `turn-complete` is built and not yet posted.
#[derive(Debug)]
pub struct Finalize<'a> {
    pub env: &'a HookEnv,
    pub classified: &'a Classified,
    pub complete: &'a mut TurnCompleteRequest,
}

// ---- the trait ---------------------------------------------------------------------------------------------------------------

fn ready<T: Send + 'static>(v: T) -> BoxFuture<'static, T> {
    Box::pin(async move { v })
}

/// A module of the direct driver. Every method has a default that does nothing, so a module implements only what it needs.
pub trait DirectHook: Send + Sync {
    /// A short name for logs and error details (`transcript`).
    fn name(&self) -> &'static str;

    /// Before the repository is restored. An `Err` ends the run as "did not start".
    fn on_restore<'a, 'b: 'a>(
        &'a self,
        _ctx: &'a mut RestoreCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        ready(Ok(()))
    }

    /// Before the engine starts. An `Err`, a panic or a timeout ends the run as "did not start", before any model call.
    fn on_startup<'a, 'b: 'a>(
        &'a self,
        _ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        ready(Ok(()))
    }

    /// Every JSON-RPC message of the engine connection. Synchronous and hot: copy, queue, return.
    fn on_wire(&self, _wire: &Wire<'_>) {}

    /// The driver's own events. Synchronous: queue and return.
    fn on_event(&self, _event: &Event<'_>) {}

    /// Controls the internal API delivered with an events response, all kinds (the driver acts on `Cancel` and `Instruct` itself).
    fn on_controls(&self, _controls: &[PendingControl]) {}

    /// A prompt ended. Is the run done? See the voting rules in the module documentation.
    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        ready(TurnVote::Done)
    }

    /// Every other part of the driver says the run is over (the modules voted `Done`, the answer window and the automatic remedies
    /// have nothing more to do). The one atomic last question: "is anything pending? if not, the run is sealed". A `Prompt` continues the run.
    fn on_seal<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        ready(TurnVote::Done)
    }

    /// The engine is gone, before the tree is committed and uploaded. Stop what runs for the engine (a server), flush what depends on the
    /// wire being complete.
    fn on_shutdown<'a>(&'a self, _end: &'a Shutdown<'a>) -> BoxFuture<'a, ()> {
        ready(())
    }

    /// `summary.json` is built and not yet checked or uploaded: add the module's own members to it. Runs for EVERY ending, after
    /// [`Self::on_shutdown`] and before [`Self::on_finalize`]; bounded by [`Limits::finalize`] like finalize.
    fn on_summary<'a>(&'a self, _ctx: SummaryCtx<'a>) -> BoxFuture<'a, ()> {
        ready(())
    }

    /// The delivered head is committed, bundled and uploaded and `summary.json` is written, before `turn.ended` and `turn-complete`: a
    /// module makes a report about the head here (the clean-checkout test report). The package proxy, the container runtime and the
    /// flusher (the run's heartbeat) still run. The call
    /// is bounded by [`Limits::report`], a ceiling the driver enforces; a module keeps its own, shorter budget and, when it runs out,
    /// records that it did not finish. Ordinary report success or failure changes nothing about how the run ended. A module whose
    /// create-only report-object write is refused records its one event and signals the object through [`Report::upload_conflict`].
    fn on_report<'a>(&'a self, _report: &'a Report<'a>) -> BoxFuture<'a, ()> {
        ready(())
    }

    /// Just before `turn-complete` is posted: the last chance to flush and to add to the request. Delivery never waits longer than
    /// [`Limits::finalize`] for it.
    fn on_finalize<'a>(&'a self, _fin: Finalize<'a>) -> BoxFuture<'a, ()> {
        ready(())
    }

    /// The time this module needs in [`Self::on_report`] when it needs more than [`Limits::report`].
    /// `None`, the default, keeps the driver's own ceiling. The driver grants it only up to what is left of the run's deadline
    /// ([`report_ceiling`]); asking changes nothing else about the call.
    fn report_time(&self) -> Option<Duration> {
        None
    }
}

/// The ceiling the driver enforces on one module's `on_report`: its own [`Limits::report`], or what the module asks for ([`DirectHook::report_time`])
/// when that is more, and never more than `time_left`, what is left of the run's deadline. The driver's own ceiling is the floor, as it always was.
pub fn report_ceiling(limits: &Limits, asked: Option<Duration>, time_left: Duration) -> Duration {
    asked.map_or(limits.report, |a| limits.report.max(a.min(time_left)))
}

// ---- running the modules --------------------------------------------------------------------------------------------------------

/// How long the driver waits for an asynchronous hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// `on_startup`: past it the run does not start.
    pub startup: Duration,
    /// `on_restore`.
    pub restore: Duration,
    /// `on_turn_end` and `on_seal`: past it the module is switched off and counts as `Done`.
    pub decision: Duration,
    /// `on_shutdown`.
    pub shutdown: Duration,
    /// `on_finalize`: delivery is never delayed beyond it.
    pub finalize: Duration,
    /// `on_report`: the ceiling for a module that works on the delivered head while the heartbeat runs (480 s). A module budgets itself
    /// below it.
    pub report: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(60),
            restore: Duration::from_secs(60),
            decision: Duration::from_secs(120),
            shutdown: Duration::from_secs(15),
            finalize: Duration::from_secs(15),
            report: Duration::from_secs(480),
        }
    }
}

/// A future that turns a panic in a poll into an error.
struct CatchUnwind<F>(F);

impl<F: Future + Unpin> Future for CatchUnwind<F> {
    type Output = Result<F::Output, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| Pin::new(&mut self.0).poll(cx))) {
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
}

struct Slot {
    hook: Arc<dyn DirectHook>,
    disabled: AtomicBool,
}

struct Inner {
    slots: Vec<Slot>,
    /// Where the "module disabled" lines go; set once (by the first caller that has a log).
    log: std::sync::OnceLock<Arc<dyn LogSink>>,
    limits: Limits,
    wire_seq: AtomicU64,
}

impl Inner {
    fn disable(&self, slot: &Slot, why: &str) {
        if !slot.disabled.swap(true, Ordering::SeqCst)
            && let Some(l) = self.log.get()
        {
            l.line(&format!(
                "direct module {} disabled: {why}",
                slot.hook.name()
            ));
        }
    }

    fn live(&self) -> impl Iterator<Item = &Slot> {
        self.slots
            .iter()
            .filter(|s| !s.disabled.load(Ordering::SeqCst))
    }

    /// Calls a synchronous hook of every module that is still on; a panic switches that module off.
    fn each(&self, what: &'static str, f: impl Fn(&dyn DirectHook)) {
        for slot in self.live() {
            if catch_unwind(AssertUnwindSafe(|| f(slot.hook.as_ref()))).is_err() {
                self.disable(slot, &format!("panicked in {what}"));
            }
        }
    }

    /// Calls an asynchronous hook of one module with a time limit; the error says why it gave no answer (a panic while the future was
    /// made or polled, or no answer in time).
    async fn call<'a, T>(
        &self,
        what: &'static str,
        limit: Duration,
        make: impl FnOnce() -> BoxFuture<'a, T>,
    ) -> Result<T, String> {
        let fut =
            catch_unwind(AssertUnwindSafe(make)).map_err(|_| format!("panicked in {what}"))?;
        match tokio::time::timeout(limit, CatchUnwind(fut)).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(())) => Err(format!("panicked in {what}")),
            Err(_) => Err(format!("no answer within {} s in {what}", limit.as_secs())),
        }
    }
}

/// The modules of a run. Cheap to clone; every call on an empty set does nothing, awaits nothing and installs nothing.
#[derive(Clone)]
pub struct Hooks {
    inner: Arc<Inner>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self::none()
    }
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("modules", &self.names())
            .finish()
    }
}

impl Hooks {
    /// No module: the driver as it was before the seam.
    pub fn none() -> Self {
        Self::new(Vec::new())
    }

    /// These modules, asked in this order.
    pub fn new(hooks: Vec<Arc<dyn DirectHook>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots: hooks
                    .into_iter()
                    .map(|hook| Slot {
                        hook,
                        disabled: AtomicBool::new(false),
                    })
                    .collect(),
                log: std::sync::OnceLock::new(),
                limits: Limits::default(),
                wire_seq: AtomicU64::new(0),
            }),
        }
    }

    /// The modules of the product, in the order they are asked: transcript, goal, research, tools, attachments, engine_config, skills, continue_from,
    /// report, messages (`tests/direct_hooks_standard.rs` pins it). Each module directory offers `hook()`, which returns `None` when the module is
    /// off for the run (its setting says off, or the host lacks what it needs, such as the goal plugin folder). The order matters for
    /// votes: `messages` is last so that a user's pending message has priority over a prompt another module proposes, while a `Stop` of
    /// any earlier module is final and `messages` is not asked (it claims nothing).
    pub fn standard(env: &HookEnv) -> Self {
        Self::standard_with(env, crate::direct::goal::settings::GoalSettings::from_env())
    }

    /// As [`standard`](Self::standard) with these goal settings: the goal module is the one that depends on a folder of the host (the
    /// plugin directory), so a test that pins the whole order names a folder that has one. The order is the one of `standard`; there is
    /// only this list.
    pub fn standard_with(env: &HookEnv, goal: crate::direct::goal::settings::GoalSettings) -> Self {
        Self::standard_with_tools(
            env,
            goal,
            crate::direct::tools::settings::Settings::from_env(),
        )
    }

    /// As [`standard_with`](Self::standard_with) with these tool settings too: the kill switch of the tool server (`AGENTD_TOOLS_MODE`) is read from
    /// the environment, which a test cannot change, so the list takes it as a value. There is only this list.
    pub fn standard_with_tools(
        env: &HookEnv,
        goal: crate::direct::goal::settings::GoalSettings,
        tools: crate::direct::tools::settings::Settings,
    ) -> Self {
        let modules = [
            crate::direct::transcript::hook(env),
            crate::direct::goal::hook_with(env, goal),
            crate::direct::research::hook(env),
            crate::direct::tools::hook_for(env, tools),
            crate::direct::attachments::hook(env),
            crate::direct::engine_config::hook(env),
            crate::direct::skills::hook(env),
            crate::direct::continue_from::hook(env),
            crate::direct::report::hook(env),
            crate::direct::messages::hook(env),
        ];
        Self::new(modules.into_iter().flatten().collect()).with_log(env.log.clone())
    }

    /// Where the "module disabled" lines go. The first log given is kept; a set that already has one ignores a later call.
    pub fn with_log(self, log: Arc<dyn LogSink>) -> Self {
        let _ = self.inner.log.set(log);
        self
    }

    /// The limits of the asynchronous hooks. Builder methods apply before the set is shared; a shared set keeps its settings.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        if let Some(i) = Arc::get_mut(&mut self.inner) {
            i.limits = limits;
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.inner.slots.is_empty()
    }

    /// The names of the registered modules, in order.
    pub fn names(&self) -> Vec<&'static str> {
        self.inner.slots.iter().map(|s| s.hook.name()).collect()
    }

    /// The tap to hand to the ACP client; `None` for an empty set, so a run with no module installs nothing.
    pub fn tap(&self) -> Option<Arc<dyn WireTap>> {
        (!self.is_empty()).then(|| Arc::new(self.clone()) as Arc<dyn WireTap>)
    }

    /// Raises an event. A module that panics is switched off.
    pub fn event(&self, event: &Event<'_>) {
        self.inner.each("on_event", |h| h.on_event(event));
    }

    /// Shows a batch of controls to the modules.
    pub fn controls(&self, controls: &[PendingControl]) {
        self.inner.each("on_controls", |h| h.on_controls(controls));
    }

    /// Asks every module before the repository is restored. The first `Err` (a module's own words, or a panic or a timeout named with
    /// the module) ends the run as "did not start".
    pub async fn restore(&self, ctx: &mut RestoreCtx<'_>) -> Result<(), String> {
        let i = &self.inner;
        for slot in i.live() {
            let name = slot.hook.name();
            match i
                .call("on_restore", i.limits.restore, || {
                    slot.hook.on_restore(&mut *ctx)
                })
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(why)) => return Err(why),
                Err(why) => {
                    i.disable(slot, &why);
                    return Err(format!("module {name} {why}"));
                }
            }
        }
        Ok(())
    }

    /// Asks every module before the engine starts; see [`Self::restore`] for how a failure ends the run.
    pub async fn startup(&self, ctx: &mut StartupCtx<'_>) -> Result<(), String> {
        let i = &self.inner;
        for slot in i.live() {
            let name = slot.hook.name();
            match i
                .call("on_startup", i.limits.startup, || {
                    slot.hook.on_startup(&mut *ctx)
                })
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(why)) => return Err(why),
                Err(why) => {
                    i.disable(slot, &why);
                    return Err(format!("module {name} {why}"));
                }
            }
        }
        Ok(())
    }

    /// The vote of a turn end ([`DirectHook::on_turn_end`]).
    pub async fn turn_end(&self, end: &TurnEnd<'_>) -> TurnVote {
        self.vote(false, end).await
    }

    /// The vote of the seal ([`DirectHook::on_seal`]).
    pub async fn seal(&self, end: &TurnEnd<'_>) -> TurnVote {
        self.vote(true, end).await
    }

    async fn vote(&self, seal: bool, end: &TurnEnd<'_>) -> TurnVote {
        let i = &self.inner;
        let what = if seal { "on_seal" } else { "on_turn_end" };
        let mut best = TurnVote::Done;
        for slot in i.live() {
            let asked = i
                .call(what, i.limits.decision, || {
                    if seal {
                        slot.hook.on_seal(end)
                    } else {
                        slot.hook.on_turn_end(end)
                    }
                })
                .await;
            match asked {
                Ok(TurnVote::Done) => {}
                Ok(stop @ TurnVote::Stop { .. }) => return stop,
                Ok(prompt @ TurnVote::Prompt(_)) => best = prompt,
                // A wait gives way to a prompt, whichever module voted first; of two waits the shorter holds. At the seal it is no opinion.
                Ok(TurnVote::Wait { until }) if !seal => {
                    best = match best {
                        TurnVote::Wait { until: other } => TurnVote::Wait {
                            until: other.min(until),
                        },
                        prompt @ TurnVote::Prompt(_) => prompt,
                        _ => TurnVote::Wait { until },
                    };
                }
                Ok(TurnVote::Wait { .. }) => {}
                // A module that gives no answer counts as having no opinion.
                Err(why) => i.disable(slot, &why),
            }
        }
        match best {
            TurnVote::Wait { until } => TurnVote::Wait {
                until: until.max(MIN_WAIT),
            },
            other => other,
        }
    }

    /// Tells every module the engine is gone.
    pub async fn shutdown(&self, env: &HookEnv, classified: &Classified) {
        let i = &self.inner;
        let end = Shutdown { env, classified };
        for slot in i.live() {
            if let Err(why) = i
                .call("on_shutdown", i.limits.shutdown, || {
                    slot.hook.on_shutdown(&end)
                })
                .await
            {
                i.disable(slot, &why);
            }
        }
    }

    /// Lets every module add its members to the built `summary.json` before it is checked and uploaded; a module that panics or does not
    /// answer within [`Limits::finalize`] is switched off, as at finalize.
    pub async fn summary(
        &self,
        env: &HookEnv,
        classified: &Classified,
        summary: &mut DirectSummary,
    ) {
        let i = &self.inner;
        for slot in i.live() {
            if let Err(why) = i
                .call("on_summary", i.limits.finalize, || {
                    slot.hook.on_summary(SummaryCtx {
                        env,
                        classified,
                        summary: &mut *summary,
                    })
                })
                .await
            {
                i.disable(slot, &why);
            }
        }
    }

    /// Tells every module the head is delivered and lets it work on it, each within [`Limits::report`]. Returns the first create-only
    /// report-object conflict a module signals; the module already recorded its one event.
    pub async fn report(
        &self,
        env: &HookEnv,
        classified: &Classified,
        head_sha: &str,
        bundle: &Path,
        bundle_uri: &str,
        time_left: Duration,
    ) -> ReportOutcome {
        let i = &self.inner;
        let conflict = std::sync::OnceLock::new();
        let pin = std::sync::OnceLock::new();
        let report = Report {
            env,
            classified,
            head_sha,
            bundle,
            bundle_uri,
            time_left,
            conflict: &conflict,
            pin: &pin,
        };
        for slot in i.live() {
            if let Err(why) = i
                .call(
                    "on_report",
                    report_ceiling(&i.limits, slot.hook.report_time(), time_left),
                    || slot.hook.on_report(&report),
                )
                .await
            {
                i.disable(slot, &why);
            }
        }
        ReportOutcome {
            conflict: conflict.get().copied(),
            pin: pin.get().cloned(),
        }
    }

    /// Lets every module flush and change the `turn-complete` request before it is posted.
    pub async fn finalize(
        &self,
        env: &HookEnv,
        classified: &Classified,
        complete: &mut TurnCompleteRequest,
    ) {
        let i = &self.inner;
        for slot in i.live() {
            if let Err(why) = i
                .call("on_finalize", i.limits.finalize, || {
                    slot.hook.on_finalize(Finalize {
                        env,
                        classified,
                        complete: &mut *complete,
                    })
                })
                .await
            {
                i.disable(slot, &why);
            }
        }
    }
}

impl WireTap for Hooks {
    fn wire(&self, dir: Dir, msg: &Value, stale: bool) {
        let i = &self.inner;
        let seq = i.wire_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let wire = Wire {
            seq,
            dir,
            msg,
            stale,
        };
        i.each("on_wire", |h| h.on_wire(&wire));
        // agentd's own `session/cancel` is also an event, raised after the message was seen.
        if dir == Dir::ToEngine
            && msg.get("method").and_then(Value::as_str) == Some("session/cancel")
        {
            let session_id = msg
                .pointer("/params/sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.event(&Event::Cancel { session_id });
        }
    }
}
