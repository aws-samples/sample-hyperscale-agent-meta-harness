//! The direct driver: one prompt, unchanged, to one engine session in one microVM.
//!
//! `POST /invocations` with `kind: "direct"` lands in [`run`]. The startup sequence of section 2.5 (credentials and events, the
//! request, the repository, the engine's configuration and the user's rules, the credential socket, the rules preflight, the
//! engine) runs first; then [`driver::run_direct_prompt`] sends ONE `session/prompt`; then the end sequence of section 2.7
//! commits what the tree holds, bundles it, writes and uploads `summary.json`, records `turn.ended`, posts
//! `/internal/turn-complete` and writes `result.json`. The harness records how the engine stopped ([`classify`]) and never
//! judges the work.
//!
//! Modules: [`classify`] (the stop table), [`collector`] (the ACP handler), [`driver`] (the wire sequence, the watchdogs and the
//! optional nudge), [`preflight`] and [`rules_place`] (the user's rules), [`request`] (`request.json`), [`snapshot`] (the
//! periodic safety copy) and [`summary`] (`summary.json`). [`hooks`] is the seam for the pieces that watch or steer a run and live in
//! their own directories: [`transcript`], [`messages`], [`goal`], [`research`], [`skills`] and [`continue_from`] (`AGENTS.md` of this
//! directory).

pub mod attachments;
pub mod cargo;
pub mod classify;
pub mod collector;
pub mod compat;
pub mod context;
pub mod continue_from;
pub mod creds_fn;
pub mod disk;
pub mod dox;
pub mod driver;
pub mod engine_config;
pub mod engine_stop;
pub mod failover;
pub mod flow;
pub mod fresh;
pub mod goal;
pub mod hooks;
pub mod input;
pub mod local_fault;
pub mod messages;
pub mod observe;
pub mod placement;
pub mod preflight;
pub mod probe;
pub mod prune;
pub mod recorder;
pub mod records;
pub mod report;
pub mod request;
pub mod research;
pub mod retry;
pub mod review;
pub mod rules_place;
pub mod skills;
pub mod slice;
pub mod snapshot;
pub mod summary;
pub mod tools;
pub mod transcript;
pub mod waiting;

use crate::BoxFuture;
use crate::acp::{AcpClient, AcpError, Activity, Watched};
use crate::creds::RefreshContext;
use crate::creds_socket::{Callers, serve_credentials_for};
use crate::direct::classify::{
    Classified, RawEnd, TreeFacts, classify, not_replaced_text, refine, scrub_detail,
    upload_conflict_payload, upload_conflict_text,
};
use crate::direct::collector::DirectCollector;
use crate::direct::context::ContextDir;
use crate::direct::driver::{
    AuditSink, GitProbe, InputWiring, NoProbe, Nudge, PromptPlan, Respawn, TreeProbe,
    run_direct_stages,
};
use crate::direct::failover::{FailoverPlan, Route, effective_limits};
use crate::direct::hooks::{EngineStop, Event, HookEnv, Hooks, RestoreCtx, StartupCtx};
use crate::direct::probe::BedrockProbe;
use crate::direct::request::{rule_refs, rules_digest, rules_manifest};
use crate::direct::retry::RetryPolicy;
use crate::direct::review::{GitRepo, ReviewPlan};
use crate::direct::snapshot::{GitSource, Snapshotter, snapshot_json};
use crate::direct::summary::{
    InputNote, SummaryExtras, SummaryInputs, build_summary_with, nudge_note, reclassify,
};
use crate::emit::{
    EmitContext, acp_model_value, aws_env, emit_aws_config, emit_engine_for, emit_profile,
};
use crate::engine::user_settings::{ReasoningLaunch, ReasoningSource, check_reasoning_launch};
use crate::engine::{
    EngineUsers, LaunchSpec, SystemUsers, chown_tree, kill_group, resolve_engine_identity,
    resolve_program, spawn_captured,
};
use crate::events::{EventRecorder, HttpEventSink, TurnControls, spawn_flusher_with};
use crate::logging::LogSink;
use crate::s3::{PutNew, S3Objects};
use crate::sandbox::{
    HelperCheck, Identity, IdentityPreflight, LandlockEnforcer, Mode, Policy, Preflight, Sandbox,
    route_of_this_process, word_of_user,
};
use crate::secrets::SecretSet;
use crate::server::App;
use crate::time::{format_rfc3339, now_unix};
use crate::turn::{reap_engine, summary_of, turn_ended_payload, write_result};
use crate::workspace::{self, GitState, Hidden, Worktree};
use creds_fn::CredsFn;
use harness_core::engine_profile::{
    fixtures_dir, hidden_paths_union, load_profile, load_profiles, resolve_launch,
};
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::common::{Engine, Transport};
use harness_protocol::direct::DirectInvocationRequest;
use harness_protocol::direct::{
    DEFAULT_SNAPSHOT_EVERY_S, DIRECT_AGENT_ID, DIRECT_TURN, DirectAttempt, DirectFailoverNote,
    DirectFlow, DirectFreshSession, DirectGit, DirectIntervention, DirectNudge, DirectOutcome,
    DirectPackages, DirectRuleRef, DirectRulesPreflight, DirectSnapshots, DirectSwitch,
    DirectTiming, NeedsInputModel, s3,
};
use harness_protocol::envelope::EventType;
use harness_protocol::output_pins::{OutputPin, OutputPins};
use harness_protocol::runtime::{
    InvocationResponse, PackagesToken, RepoSource, RulesManifest, TurnOutcome, turn_result_path,
};
use harness_protocol::store::CredsBinding;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// `Ham-Role` of the harness commit and `role` of the events.
pub const DIRECT_ROLE: &str = "direct";
/// An engine that ignores `session/cancel` is stopped after this long.
pub const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// How long past the deadline a hung startup step is waited for.
pub const SETUP_GUARD: Duration = Duration::from_secs(60);
/// The bundle, the summary and the snapshot uploads are tried again this many times.
pub const UPLOAD_RETRIES: u32 = 3;
/// The end-of-run objects agentd writes once (`If-None-Match: *`), named below `runs/<run>/agents/direct/` as the conflict label
/// names them.
pub const BUNDLE_OBJECT: &str = "turns/1/bundle";
pub const SUMMARY_OBJECT: &str = "turns/1/summary.json";
pub const TEST_REPORT_OBJECT: &str = "turns/1/test-report.json";

// ---- the package proxy seam --------------------------------------------------------------------------------------------------

/// What agentd hands the loopback package proxy.
// Debug is written by hand below: the token must never print.
pub struct ProxyStart {
    /// `<domain>-<owner>.d.codeartifact.<region>.amazonaws.com`.
    pub host: String,
    pub repository: String,
    /// The CodeArtifact token, as the wire type that carries it (`expose` is the one way to its text; the buffer is wiped when
    /// this struct is dropped). The proxy keeps its own `Secret` per credentialed upstream.
    pub token: PackagesToken,
    pub expires_at: Option<u64>,
    pub goproxy: Option<String>,
    pub log: Arc<dyn LogSink>,
}

impl std::fmt::Debug for ProxyStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyStart")
            .field("host", &self.host)
            .field("repository", &self.repository)
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("goproxy", &self.goproxy)
            .finish_non_exhaustive()
    }
}

/// A running proxy, as the driver uses it.
pub trait ProxyRun: Send {
    /// The environment variables the engine's package tools read.
    fn env(&self) -> BTreeMap<String, String>;
    /// The Maven settings file (a mirror of `*` to the proxy; no secret).
    fn maven_settings_xml(&self) -> String;
    /// The cargo `config.toml` (crates.io replaced by the proxy's registry; no secret).
    fn cargo_config_toml(&self) -> String;
    fn stats(&self) -> DirectPackages;
    fn stop(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// Starts the proxy; a seam so tests run a fake.
pub trait ProxyLauncher: Send + Sync {
    fn start(&self, start: ProxyStart) -> BoxFuture<'_, Result<Box<dyn ProxyRun>, String>>;
}

/// What a run needs from outside the process.
pub struct Deps {
    pub proxy: Arc<dyn ProxyLauncher>,
    /// The session's rootless container runtime (the seam below); a run goes on without it.
    pub containers: Arc<dyn ContainersLauncher>,
    /// The modules of the run (the module seam, [`hooks`]). `None` registers the product's own ([`Hooks::standard`]); tests put fake
    /// modules here.
    pub hooks: Option<Hooks>,
    /// Asked before a run in the `landlock` mode fetches or places anything: can this kernel give the Landlock domain.
    pub preflight: Arc<dyn Preflight>,
    /// Asked before a run on the nobody route (the `landlock` mode's default identity) fetches or places anything: can this machine make the user and IPC
    /// namespaces, and does the helper work.
    pub identity: Arc<dyn IdentityPreflight>,
    /// Who agentd is and who the engine user is: asked at step 1c, before anything is fetched or placed, so that a root agentd that cannot
    /// resolve the engine user ends the run and never starts the engine as root.
    pub engine_users: Arc<dyn EngineUsers>,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

impl Default for Deps {
    fn default() -> Self {
        Self {
            proxy: Arc::new(PkgProxyLauncher),
            containers: Arc::new(ShellContainersLauncher::from_env()),
            hooks: None,
            preflight: Arc::new(LandlockEnforcer),
            identity: Arc::new(HelperCheck),
            engine_users: Arc::new(SystemUsers),
        }
    }
}

struct ProxyLog(Arc<dyn LogSink>);

impl pkgproxy::Log for ProxyLog {
    fn line(&self, text: &str) {
        self.0.line(text);
    }
}

/// The real proxy (crate `pkgproxy`).
#[derive(Debug)]
pub struct PkgProxyLauncher;

struct PkgProxyRun(pkgproxy::Handle);

impl ProxyRun for PkgProxyRun {
    fn env(&self) -> BTreeMap<String, String> {
        self.0.env()
    }
    fn maven_settings_xml(&self) -> String {
        self.0.maven_settings_xml()
    }
    fn cargo_config_toml(&self) -> String {
        self.0.cargo_config_toml()
    }
    fn stats(&self) -> DirectPackages {
        let s = self.0.snapshot();
        DirectPackages {
            enabled: true,
            requests: s.requests,
            bytes: s.bytes,
            by_prefix: s.by_prefix,
            statuses: s.statuses,
            refused: s.refused,
        }
    }
    fn stop(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move { self.0.stop().await })
    }
}

impl ProxyLauncher for PkgProxyLauncher {
    fn start(&self, start: ProxyStart) -> BoxFuture<'_, Result<Box<dyn ProxyRun>, String>> {
        Box::pin(async move {
            // Port 3128, else any free port (`Handle::env` reports the real one).
            for port in [3128u16, 0] {
                let mut upstreams = pkgproxy::codeartifact_upstreams(
                    &start.host,
                    &start.repository,
                    pkgproxy::Secret::new(start.token.expose().to_string()),
                );
                if let Some(url) = &start.goproxy {
                    upstreams.push(pkgproxy::go_upstream(url));
                }
                let cfg = pkgproxy::Config {
                    listen: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    upstreams,
                    expires_at_unix: start.expires_at,
                    max_connections: 64,
                    request_timeout: Duration::from_secs(900),
                    idle_timeout: Duration::from_secs(60),
                    max_redirects: 5,
                    log: Arc::new(ProxyLog(start.log.clone())),
                };
                match pkgproxy::start(cfg).await {
                    Ok(h) => return Ok(Box::new(PkgProxyRun(h)) as Box<dyn ProxyRun>),
                    Err(e) if port != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {}
                    Err(e) => return Err(format!("package proxy: {e}")),
                }
            }
            Err("package proxy: no port".to_string())
        })
    }
}

// ---- the container runtime seam ------------------------------------------------------------------------------------------------

/// Where the engine finds the session's Docker endpoint (the front of the rootless runtime that `ham-containers up` starts; it is
/// also the default path Testcontainers and the `docker` command probe).
const DOCKER_SOCKET_URL: &str = "unix:///var/run/docker.sock";
/// `ham-containers`, the helper of the worker image that starts and stops the runtime.
const CONTAINERS_BIN: &str = "/opt/harness/bin/ham-containers";
/// `up` returns once the runtime is launched in the background; this is its limit.
const CONTAINERS_UP_TIMEOUT: Duration = Duration::from_secs(30);
/// `down` limit; an override can shorten it, never lengthen it.
const CONTAINERS_DOWN_TIMEOUT: Duration = Duration::from_secs(15);
/// The tail of what `up` or `down` printed that reaches the log.
const CONTAINERS_LOG_TAIL: usize = 2048;
/// How long the pipes of a finished `up` are read after it exited: a daemon it left behind may hold them open for good.
const CONTAINERS_PIPE_GRACE: Duration = Duration::from_millis(300);

/// What agentd hands the launcher of the session's container runtime.
#[derive(Debug)]
pub struct ContainersStart {
    /// The engine user (`AGENTD_ENGINE_USER`): the runtime's daemon runs as this user, never as root.
    pub user: String,
    pub log: Arc<dyn LogSink>,
}

/// A running container runtime, as the driver uses it.
pub trait ContainersRun: Send {
    /// The environment variables the engine's tests read to reach it.
    fn env(&self) -> BTreeMap<String, String>;
    /// The pids of the runtime's long-lived processes (the front socket's process and the supervisor), read right after the runtime was
    /// started and before the engine exists. The engine-stop sweep leaves them, and everything below them, alone. Empty when the
    /// launcher cannot name them.
    fn processes(&self) -> Vec<u32> {
        Vec::new()
    }
    fn stop(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// Starts the runtime; a seam so tests run a fake. A failure is never fatal to the run.
pub trait ContainersLauncher: Send + Sync {
    fn start(
        &self,
        start: ContainersStart,
    ) -> BoxFuture<'_, Result<Box<dyn ContainersRun>, String>>;
}

/// The four variables of a session with a container runtime: the Docker and Podman clients and Testcontainers find the endpoint,
/// Testcontainers connects to published ports on the loopback address and does not start its reaper container (the runtime has
/// no registry to pull it from, and the session ends with its containers).
pub fn containers_env() -> BTreeMap<String, String> {
    [
        ("CONTAINER_HOST", DOCKER_SOCKET_URL),
        ("DOCKER_HOST", DOCKER_SOCKET_URL),
        ("TESTCONTAINERS_HOST_OVERRIDE", "127.0.0.1"),
        ("TESTCONTAINERS_RYUK_DISABLED", "true"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The real launcher: `ham-containers up --user <user>` as agentd's own user (root), `ham-containers down` at the end.
#[derive(Debug, Clone)]
pub struct ShellContainersLauncher {
    bin: PathBuf,
    up_timeout: Duration,
    down_timeout: Duration,
}

impl ShellContainersLauncher {
    pub fn new(bin: impl Into<PathBuf>, up_timeout: Duration, down_timeout: Duration) -> Self {
        Self {
            bin: bin.into(),
            up_timeout,
            down_timeout,
        }
    }

    /// `AGENTD_CONTAINERS_BIN` and `AGENTD_CONTAINERS_TIMEOUT_S`, else the image's helper and the 30 s / 15 s limits.
    pub fn from_env() -> Self {
        let (up, down) =
            Self::timeouts_from(std::env::var("AGENTD_CONTAINERS_TIMEOUT_S").ok().as_deref());
        Self::new(
            Self::bin_from(std::env::var("AGENTD_CONTAINERS_BIN").ok().as_deref()),
            up,
            down,
        )
    }

    pub fn bin_from(value: Option<&str>) -> PathBuf {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(CONTAINERS_BIN),
        }
    }

    /// `(up, down)`: a positive whole number of seconds sets `up` and shortens `down` to match; anything else keeps the defaults.
    pub fn timeouts_from(value: Option<&str>) -> (Duration, Duration) {
        match value
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
        {
            Some(n) => (
                Duration::from_secs(n),
                Duration::from_secs(n).min(CONTAINERS_DOWN_TIMEOUT),
            ),
            None => (CONTAINERS_UP_TIMEOUT, CONTAINERS_DOWN_TIMEOUT),
        }
    }
}

struct ShellContainersRun {
    bin: PathBuf,
    down_timeout: Duration,
    log: Arc<dyn LogSink>,
    /// The pids `ham-containers up` named.
    processes: Vec<u32>,
}

/// The line `ham-containers up` prints to name the runtime's long-lived processes.
const CONTAINERS_PIDS_LINE: &str = "CONTAINERS PIDS ";

/// The pids the line `CONTAINERS PIDS front=<pid> supervisor=<pid> [holder=<pid>]` of `up`'s output names. The line is read from the
/// start of a line; a member that is not a number, and pid 0 and pid 1, are left out; no such line gives none.
fn runtime_pids(stdout: &str) -> Vec<u32> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(CONTAINERS_PIDS_LINE))
        .map(|rest| {
            rest.split_whitespace()
                .filter_map(|member| member.split_once('='))
                .filter(|(name, _)| matches!(*name, "front" | "supervisor" | "holder"))
                .filter_map(|(_, pid)| pid.parse::<u32>().ok())
                .filter(|pid| *pid > 1)
                .collect()
        })
        .unwrap_or_default()
}

/// How a helper command ended.
enum Ended {
    /// It exited; `ok` is a zero status, `output` the tail of what it printed (standard output first).
    Exited {
        ok: bool,
        status: String,
        stdout: String,
        stderr: String,
    },
    TimedOut,
    Failed(String),
}

/// Keeps the last [`CONTAINERS_LOG_TAIL`] bytes a pipe delivers, until it closes.
async fn collect_tail<R>(mut pipe: R, into: Arc<Mutex<Vec<u8>>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 4096];
    while let Ok(n) = pipe.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        let mut buf = into.lock().unwrap_or_else(|p| p.into_inner());
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > CONTAINERS_LOG_TAIL {
            let cut = buf.len() - CONTAINERS_LOG_TAIL;
            buf.drain(..cut);
        }
    }
}

/// Runs `bin args...` with a limit. The child is killed when the limit passes or this future is dropped. The pipes are read after
/// the exit for a moment only: `ham-containers up` leaves daemons behind, and any of them may hold a pipe open.
async fn run_helper(bin: &Path, args: &[&str], limit: Duration) -> Ended {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Ended::Failed(format!("cannot run {}: {e}", bin.display())),
    };
    let (out, err) = (
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    );
    let readers: Vec<tokio::task::JoinHandle<()>> = [
        child
            .stdout
            .take()
            .map(|p| tokio::spawn(collect_tail(p, out.clone()))),
        child
            .stderr
            .take()
            .map(|p| tokio::spawn(collect_tail(p, err.clone()))),
    ]
    .into_iter()
    .flatten()
    .collect();
    let waited = tokio::time::timeout(limit, child.wait()).await;
    let ended = match waited {
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            None
        }
        Ok(Err(e)) => {
            for r in &readers {
                r.abort();
            }
            return Ended::Failed(format!("cannot wait for {}: {e}", bin.display()));
        }
        Ok(Ok(status)) => Some(status),
    };
    for r in readers {
        let abort = r.abort_handle();
        if tokio::time::timeout(CONTAINERS_PIPE_GRACE, r)
            .await
            .is_err()
        {
            abort.abort();
        }
    }
    let text = |m: &Arc<Mutex<Vec<u8>>>| {
        String::from_utf8_lossy(&m.lock().unwrap_or_else(|p| p.into_inner())).into_owned()
    };
    match ended {
        None => Ended::TimedOut,
        Some(status) => Ended::Exited {
            ok: status.success(),
            status: status.to_string(),
            stdout: text(&out),
            stderr: text(&err),
        },
    }
}

/// The tail of what a helper printed, one log line per non-empty line.
fn log_output(log: &Arc<dyn LogSink>, label: &str, stdout: &str, stderr: &str) {
    let joined = format!("{stdout}{stderr}");
    let tail = if joined.len() > CONTAINERS_LOG_TAIL {
        let mut from = joined.len() - CONTAINERS_LOG_TAIL;
        while !joined.is_char_boundary(from) {
            from += 1;
        }
        &joined[from..]
    } else {
        &joined
    };
    for line in tail.lines().map(str::trim).filter(|l| !l.is_empty()) {
        log.line(&format!("direct turn containers {label}: {line}"));
    }
}

fn first_line(stderr: &str, stdout: &str) -> Option<String> {
    [stderr, stdout]
        .into_iter()
        .find_map(|s| s.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(str::to_string)
}

impl ContainersLauncher for ShellContainersLauncher {
    fn start(
        &self,
        start: ContainersStart,
    ) -> BoxFuture<'_, Result<Box<dyn ContainersRun>, String>> {
        Box::pin(async move {
            if !self.bin.is_file() {
                return Err("containers: ham-containers is not installed".to_string());
            }
            match run_helper(&self.bin, &["up", "--user", &start.user], self.up_timeout).await {
                Ended::Exited {
                    ok,
                    status,
                    stdout,
                    stderr,
                } => {
                    log_output(&start.log, "up", &stdout, &stderr);
                    if ok {
                        let processes = runtime_pids(&stdout);
                        Ok(Box::new(ShellContainersRun {
                            bin: self.bin.clone(),
                            down_timeout: self.down_timeout,
                            log: start.log,
                            processes,
                        }) as Box<dyn ContainersRun>)
                    } else {
                        Err(format!(
                            "containers: {}",
                            first_line(&stderr, &stdout).unwrap_or_else(|| format!(
                                "ham-containers up exited with {status}"
                            ))
                        ))
                    }
                }
                Ended::TimedOut => Err(format!(
                    "containers: ham-containers up did not finish in {:?}",
                    self.up_timeout
                )),
                Ended::Failed(e) => Err(format!("containers: {e}")),
            }
        })
    }
}

impl ContainersRun for ShellContainersRun {
    fn env(&self) -> BTreeMap<String, String> {
        containers_env()
    }

    fn processes(&self) -> Vec<u32> {
        self.processes.clone()
    }

    fn stop(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            match run_helper(&self.bin, &["down"], self.down_timeout).await {
                Ended::Exited {
                    ok,
                    status,
                    stdout,
                    stderr,
                } => {
                    log_output(&self.log, "stop", &stdout, &stderr);
                    if !ok {
                        self.log
                            .line(&format!("direct turn containers stop failed: {status}"));
                    }
                }
                Ended::TimedOut => self.log.line(&format!(
                    "direct turn containers stop failed: did not finish in {:?}",
                    self.down_timeout
                )),
                Ended::Failed(e) => self
                    .log
                    .line(&format!("direct turn containers stop failed: {e}")),
            }
        })
    }
}

// ---- the run -------------------------------------------------------------------------------------------------------------------

/// What the startup learned and the end sequence needs.
#[derive(Default)]
struct State {
    agent_data: PathBuf,
    request_sha: String,
    prompt_bytes: u64,
    nudge_max: Option<u32>,
    /// A module that runs its own follow-up loop (the goal loop) switched the request's nudge off.
    nudge_off_by_loop: bool,
    rules: Vec<DirectRuleRef>,
    manifest: Option<RulesManifest>,
    digest: Option<String>,
    rules_preflight: Option<DirectRulesPreflight>,
    engine_name: String,
    engine_version: Option<String>,
    engine_binary_sha256: Option<String>,
    reasoning_effort: Option<String>,
    reasoning_source: Option<String>,
    /// Who the engine ran as: `nobody`, `agentd` or the engine user's name; the run summary records it.
    engine_identity: Option<String>,
    wt: Option<Worktree>,
    /// The records folder the run chose when it started (COMPAT-CAH-DATA: one records folder per run); the locator in it is read when it is needed.
    records_folder: Option<&'static str>,
    /// The first line of the first prompt, when the run works in the earlier records folder.
    records_line: Option<String>,
    base: Option<String>,
    hidden: Option<Hidden>,
    git_state: Option<GitState>,
    quarantined: Vec<String>,
    proxy: Option<Box<dyn ProxyRun>>,
    containers: Option<Box<dyn ContainersRun>>,
    started_recorded: bool,
    pinned: Option<String>,
    usage: Option<harness_protocol::common::Usage>,
    tool_calls: u64,
    nudges_sent: u32,
    max_silent_gap_s: Option<u32>,
    prompt_s: Option<u64>,
    exit_note: Option<String>,
    snapshots: Arc<Mutex<DirectSnapshots>>,
    /// Every try of the prompt, when the run retried after a provider failure.
    attempts: Vec<DirectAttempt>,
    /// Every confirmed switch of route or model (failover).
    switches: Vec<DirectSwitch>,
    final_model: Option<String>,
    failover: Option<DirectFailoverNote>,
    outage_s: Option<u64>,
    /// Every question the run asked its user.
    interventions: Vec<DirectIntervention>,
    /// Every fresh session of the run.
    fresh_sessions: Vec<DirectFreshSession>,
    /// How the last stop followed a wait: a sentence that leads the detail.
    input_note: Option<InputNote>,
    /// The facts of the last attempt when the run ended on the engine's own stop.
    engine_stop: Option<engine_stop::StopReport>,
    /// The user's answer channel, handed back by the driver: what arrives during the end sequence is acknowledged as ignored.
    late_answers: Option<waiting::LateAnswers>,
    /// A module of the seam ended the run (`TurnVote::Stop`): its words.
    hook_stop: Option<String>,
    /// The last stage ended on error -32603 while the session disk was short: the cause with its numbers. The end is then the
    /// harness's own stop that names the disk ([`classify::with_disk`]).
    disk_full: Option<disk::DiskFull>,
    /// What the review round did, or why it did not run.
    review: Option<harness_protocol::direct::DirectReview>,
    /// The run's secret values: the invocation's ([`secret_values`]) and every secret key, session token and nonce a credential refresh
    /// mints after it (the credential store adds them, `CredStore::watch_secrets`). The collector, the recorder, the digest sources and the
    /// end sequence redact with what it holds at the moment they redact; nothing is removed from it.
    secrets: SecretSet,
}

/// Runs the invocation's turn and returns the result it wrote. The caller (the server) holds the turn slot and the `/ping`
/// busy guard until this returns.
pub async fn run(app: &App, req: DirectInvocationRequest) -> InvocationResponse {
    run_with(app, req, &Deps::default()).await
}

/// [`run`] with the outside world given.
pub async fn run_with(app: &App, req: DirectInvocationRequest, deps: &Deps) -> InvocationResponse {
    let received = Instant::now();
    let started_unix = now_unix();
    app.log.line(&format!(
        "direct turn started: run={} agent={DIRECT_AGENT_ID} engine={} model={}",
        req.run_id, req.engine, req.model.id
    ));
    let api_url = app
        .cfg
        .internal_api_endpoint
        .clone()
        .unwrap_or_else(|| req.internal_api_url.clone());
    app.creds
        .install(
            RefreshContext {
                run_id: req.run_id.clone(),
                agent_id: DIRECT_AGENT_ID.to_string(),
                runtime_session_id: req.runtime_session_id.clone(),
                internal_api_url: api_url.clone(),
                region: req.region.clone(),
            },
            req.credentials.clone(),
            req.nonce.clone(),
        )
        .await;
    // The run's secret set, watched before the store's first `current()` of the run (the request fetch in `start_and_prompt`): a refresh
    // there, or at any later use, adds what it mints before anything can print it.
    let secrets = SecretSet::new(secret_values(&req));
    app.creds.watch_secrets(secrets.clone()).await;
    let rec = EventRecorder::new(&req.run_id, DIRECT_AGENT_ID);
    // The run's completion key is derived from the first nonce the invocation carries; worker-api requires it on events and turn-complete.
    let sink = Arc::new(
        HttpEventSink::new(app.creds.clone(), api_url, req.region.clone())
            .with_completion_key(CredsBinding::completion_key(&req.nonce)),
    );
    // Controls from the internal API: a Cancel stops the running prompt (`ham stop`).
    let (controls, _) = TurnControls::new();
    // The modules of the run, made before anything runs; with none registered this is an empty set that every call below skips.
    let hook_env = HookEnv::new(app, &req, &rec, &controls).with_secrets(secrets.clone());
    let hooks = match &deps.hooks {
        Some(h) => h.clone().with_log(app.log.clone()),
        None => Hooks::standard(&hook_env),
    };
    controls.install_hooks(&hooks);
    let flusher = spawn_flusher_with(
        rec.clone(),
        sink.clone(),
        app.cfg.direct.events_every,
        app.cfg.direct.heartbeat_every,
        req.turn,
        app.log.clone(),
        controls.clone(),
    );
    let mut st = State {
        engine_name: req.engine.clone(),
        secrets,
        ..State::default()
    };
    // The prompt phase ends at the deadline by itself; this guard is for a startup step that hangs (a git, a download), and it
    // leaves time for the engine to be stopped. The engine and the snapshot task stop when their futures are dropped.
    let guard = received + Duration::from_secs(u64::from(req.timeout_s)) + SETUP_GUARD;
    let started = tokio::time::timeout_at(
        guard,
        start_and_prompt(
            app, &req, deps, &rec, &controls, &hooks, &hook_env, &mut st, received,
        ),
    )
    .await;
    let setup_failed = |e: String| RawEnd {
        watched: Watched::Done(Err(AcpError::Protocol(e.clone()))),
        cancel_sent: false,
        message: String::new(),
        setup_error: Some(e),
    };
    let raw = match started {
        Ok(Ok(raw)) => raw,
        Ok(Err(e)) => setup_failed(e),
        Err(_) => setup_failed("the run did not finish starting before its deadline".to_string()),
    };
    finish(
        app,
        &req,
        st,
        raw,
        &rec,
        &sink,
        flusher,
        received,
        started_unix,
        &hooks,
        &hook_env,
    )
    .await
}

fn mkdirs(dirs: &[&Path]) -> Result<(), String> {
    for d in dirs {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    Ok(())
}

/// SHA-256 of a file, streamed (the engine binary is large); `None` when there is no such file.
fn sha256_file(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// The secret values of the invocation: the session credentials, the refresh nonce and the package token. The engine is given
/// session credentials by design, so a model can print them; none may leave in a message, an error or an event. They seed the run's
/// [`SecretSet`]; what a credential refresh mints later joins them there (`CredStore::watch_secrets`).
fn secret_values(req: &DirectInvocationRequest) -> Vec<String> {
    let mut v = vec![
        req.credentials.secret_access_key.clone(),
        req.credentials.session_token.clone(),
        req.nonce.clone(),
        // The completion key made from the nonce: it stays out of every message, error and event like the nonce does.
        CredsBinding::completion_key(&req.nonce),
    ];
    v.extend(req.packages_token.as_ref().map(|t| t.expose().to_string()));
    v
}

/// `turn.started`, once: with the rules digest as soon as it is known, else (a run that failed earlier) without one just ahead of
/// `turn.ended`.
fn record_started(
    rec: &EventRecorder,
    req: &DirectInvocationRequest,
    st: &mut State,
    digest: Option<&str>,
) {
    if st.started_recorded {
        return;
    }
    st.started_recorded = true;
    let mut started = serde_json::json!({
        "turn": DIRECT_TURN,
        "engine": req.engine,
        "model": req.model.id,
        "role": DIRECT_ROLE,
    });
    if let Some(d) = digest {
        started["rulesDigest"] = serde_json::json!(d);
    }
    rec.record_urgent(EventType::TurnStarted, started);
}

/// Steps 2 to 13 of the startup sequence. An `Err` is a failure before the prompt (`stopped_setup_failed`).
#[allow(clippy::too_many_arguments)]
async fn start_and_prompt(
    app: &App,
    req: &DirectInvocationRequest,
    deps: &Deps,
    rec: &Arc<EventRecorder>,
    controls: &Arc<TurnControls>,
    hooks: &Hooks,
    hook_env: &HookEnv,
    st: &mut State,
    received: Instant,
) -> Result<RawEnd, String> {
    let cfg = &app.cfg;
    let run = req.run_id.as_str();
    let agent_data = cfg.data_root.join(run).join(DIRECT_AGENT_ID);
    st.agent_data = agent_data.clone();
    // 1. how the engine is kept away from agentd's secrets: the deployment's mode, checked before anything is fetched or placed. A
    // mode that is not one, a conflict with the engine user, or a kernel that cannot give the Landlock domain ends the run here: no engine starts.
    let sandbox_mode = cfg
        .engine_sandbox
        .resolve(cfg.engine_user.as_deref())
        .map_err(|why| format!("engine sandbox: {why}"))?;
    // 1b. who the engine runs as: the nobody route by default in the `landlock` mode, agentd's own user when the deployment says so. A word that
    // is not one, a machine that cannot make the namespaces and a helper that does not work end the run here as well.
    let identity = if sandbox_mode == Mode::Landlock {
        cfg.engine_identity
            .resolve()
            .map_err(|why| format!("engine sandbox: {why}"))?
    } else {
        Identity::Agentd
    };
    if sandbox_mode == Mode::Landlock {
        deps.preflight
            .check()
            .map_err(|e| format!("engine sandbox: {e}"))?;
        if identity == Identity::Nobody {
            deps.identity
                .check(&cfg.identity_helper)
                .map_err(|e| format!("engine sandbox: {e}"))?;
        }
    }
    st.engine_identity = Some(match sandbox_mode {
        Mode::Landlock => identity.word().to_string(),
        Mode::Uid => cfg
            .engine_user
            .as_deref()
            .map_or_else(|| Identity::Agentd.word().to_string(), word_of_user),
    });
    // 1c. who the engine runs as under the uid split: resolved once, here, before anything is fetched or placed. A root agentd that cannot
    // resolve the named engine user ends the run, and the engine never starts as root; no user named, or an agentd known not to be root, is a start
    // without the split as before.
    let owner = resolve_engine_identity(deps.engine_users.as_ref(), cfg.engine_user.as_deref())
        .await
        .map_err(|why| {
            app.log
                .line(&format!("direct turn engine user refused: {why}"));
            format!("engine user: {why}")
        })?;
    mkdirs(&[&agent_data.join("input")])?;

    // 2. the request: fetched, checked against its hash, parsed, its rules checked again
    let creds = app.creds.current().await.map_err(|e| e.to_string())?;
    let s3_objects = S3Objects::new(req.region.clone(), cfg.s3_endpoint.clone());
    let request_file = agent_data.join("input/request.json");
    s3_objects
        .get_to_file(&req.request.uri, &creds, &request_file)
        .await
        .map_err(|e| format!("request.json: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&request_file, std::fs::Permissions::from_mode(0o600));
    }
    let bytes = std::fs::read(&request_file).map_err(|e| format!("request.json: {e}"))?;
    let start = request::parse_request(&bytes, &req.request.sha256)?;
    request::check_rules(&start.rules)?;
    request::check_limits_member(start.limits.as_ref())?;
    // The user's engine settings (protocol 1.39.0): checked again here, before anything is placed, and typed; a request with none has none.
    let user_settings = request::engine_settings(&start, &req.engine)?;
    // The user's review member (protocol 1.44.0): checked again with the one check `ham` and control-api call, before anything is placed.
    review::check_member(start.review.as_ref())?;
    st.request_sha = req.request.sha256.clone();
    st.prompt_bytes = start.prompt.len() as u64;
    st.nudge_max = start.nudge.as_ref().map(|n| n.max_nudges);
    st.rules = rule_refs(&start.rules);
    if !start.rules.is_empty() {
        let manifest = rules_manifest(&start.rules);
        let digest = rules_digest(&manifest);
        app.log.line(&format!(
            "direct turn rules manifest: {} entries, digest {digest}",
            manifest.entries.len()
        ));
        st.manifest = Some(manifest);
        st.digest = Some(digest);
    }
    let digest = st.digest.clone();
    record_started(rec, req, st, digest.as_deref());

    let profile = load_profile(&cfg.profiles_dir.join(format!("{}.toml", req.engine)))
        .map_err(|e| format!("engine profile {}: {e}", req.engine))?;
    st.engine_version = Some(profile.version.clone());
    // The engine binary is large: it is hashed on a blocking thread, not on an async worker.
    let binary = resolve_program(&profile.binary.path, &cfg.engine_path);
    st.engine_binary_sha256 = tokio::task::spawn_blocking(move || sha256_file(&binary))
        .await
        .ok()
        .flatten();

    // 3. directories
    let (data_dir, config_dir, home, tmp) = (
        agent_data.join("data"),
        agent_data.join("config"),
        agent_data.join("home"),
        agent_data.join("tmp"),
    );
    mkdirs(&[&data_dir, &config_dir, &home, &tmp])?;
    // Where the harness's files for the engine go (`crate::engine::layout`): the config directory itself, or a root-owned directory beside it.
    let layout = crate::engine::layout::config_layout(&req.engine, &config_dir);
    layout
        .prepare()
        .map_err(|e| format!("engine config root: {e}"))?;

    // 4. the repository, on branch harness/<run>/direct
    let RepoSource::S3Bundle { uri: repo_uri } = &req.repo else {
        return Err("repo: a direct run restores an S3 bundle".to_string());
    };
    // A module of the seam may name another bundle (a run seeded from an earlier run's delivered head).
    let mut repo_uri = repo_uri.clone();
    hooks
        .restore(&mut RestoreCtx {
            env: hook_env,
            request: &start,
            repo_uri: &mut repo_uri,
        })
        .await?;
    let bundle_file = agent_data.join("input.bundle");
    s3_objects
        .get_to_file(&repo_uri, &creds, &bundle_file)
        .await
        .map_err(|e| format!("worktree: {e}"))?;
    let (wt, fresh) = workspace::prepare_from_bundle_state(
        &cfg.workspace_root,
        run,
        DIRECT_AGENT_ID,
        &bundle_file,
    )
    .await
    .map_err(|e| format!("worktree: {e}"))?;
    st.wt = Some(wt.clone());
    // 4a. the records folder of the run (COMPAT-CAH-DATA): chosen once, from the repository as restored. State in both `.ham` and `.cah` is refused here,
    // before any model call; a repository whose state is in `.cah` is worked there, and the first line of the first prompt says so.
    let folder = records::choose(&wt.repo).map_err(|why| format!("records: {why}"))?;
    st.records_folder = Some(folder);
    st.records_line = records::prompt_line(folder);
    // The engine commits in this repository with no git configuration of its own.
    workspace::set_repo_identity(&wt)
        .await
        .map_err(|e| format!("worktree: {e}"))?;
    // The user's build member (protocol 1.48.0): `target/` is listed unless `excludeTargetDir` is false.
    let build = start.build.unwrap_or_default();
    workspace::exclude_generated_with(&wt, build.exclude_target_dir())
        .map_err(|e| format!("worktree: {e}"))?;
    // A slice after the first starts on the work an earlier slice left and measures everything (the changed files, the no-changes label,
    // the review round, the summary) against the run's original base, or against the DOX starter commit made on it; the first slice's
    // base is the head it restored, or the DOX starter commit made on it.
    let restored = slice::Restored::of(req);
    let origin = match &restored {
        Some(r) => {
            slice::check_restored(&wt.repo, r).await?;
            r.base_sha.clone()
        }
        None => workspace::head(&wt)
            .await
            .map_err(|e| format!("worktree: {e}"))?,
    };
    // The DOX starter is written and committed before the base is read, so the engine loads it from its first request and the starter is
    // not the run's own change. A later slice finds the commit in the history it restored and measures from it. Best effort: a refusal leaves the
    // tree as it was and the run goes on.
    let starter = dox::settle(
        &wt,
        &dox::Request {
            skills: &start.skills,
            prompt: &start.prompt,
            continue_run: start.continue_from.is_some(),
            agent_data: &agent_data,
        },
        &origin,
        fresh,
    )
    .await
    .map_err(|e| format!("worktree: {e}"))?;
    if let Some(note) = &starter.note {
        app.log.line(&format!("direct turn DOX starter: {note}"));
    }
    let base = starter.base;
    st.base = Some(base.clone());
    st.git_state = Some(GitState::snapshot(&wt).map_err(|e| format!("worktree: {e}"))?);

    // 5. repository hooks, plugins and agents set aside
    let every = load_profiles(&cfg.profiles_dir).unwrap_or_else(|_| vec![profile.clone()]);
    let hidden =
        workspace::hide_paths(&wt, &hidden_paths_union(&every)).map_err(|e| e.to_string())?;
    st.quarantined = hidden.quarantined();
    if !st.quarantined.is_empty() {
        app.log.line(&format!(
            "direct turn quarantined: {}",
            st.quarantined.join(",")
        ));
    }
    st.hidden = Some(hidden);

    // The files agentd places for the engine are given to it as they are written (step 8 gives it the rest).

    // 6. the package proxy, when the run carries a token and the stage names its index
    let mut proxy_env = BTreeMap::new();
    // The cargo config the proxy gave the run, for the modules that run cargo themselves (the report steps).
    let mut cargo_config_text: Option<String> = None;
    if let (Some(token), Some(ix)) = (&req.packages_token, &cfg.packages) {
        let launcher = deps.proxy.clone();
        let proxy_start = ProxyStart {
            host: crate::packages::repository_host(ix, &req.region),
            repository: ix.repository.clone(),
            token: token.clone(),
            expires_at: req.packages_token_expires_at,
            goproxy: cfg.goproxy.clone(),
            log: app.log.clone(),
        };
        // Its own task: a start that panics ends as "no packages", the same as one that returns an error.
        let started = tokio::spawn(async move { launcher.start(proxy_start).await })
            .await
            .unwrap_or_else(|_| Err("package proxy: start failed unexpectedly".to_string()));
        match started {
            Ok(p) => {
                proxy_env = p.env();
                let m2 = home.join(".m2");
                mkdirs(&[&m2])?;
                let settings = m2.join("settings.xml");
                std::fs::write(&settings, p.maven_settings_xml())
                    .map_err(|e| format!("maven settings: {e}"))?;
                // Maven reads `${user.home}/.m2/settings.xml`, and Java's user.home is the password database's (`/home/agent`), not
                // `$HOME` (this run's data directory): the file above would be ignored, and builds would reach for Maven Central. Maven
                // 3.9 reads its arguments from MAVEN_ARGS, so the engine's Maven (and the one the wrapper downloads) names the file.
                proxy_env.insert(
                    "MAVEN_ARGS".to_string(),
                    format!("--settings {}", settings.display()),
                );
                // Cargo: the proxy's `config.toml` in a cargo home of the engine's home replaces crates.io by the proxy's registry. A write
                // that fails is logged and the run goes on: the other tools still reach their sources.
                let cargo_text = p.cargo_config_toml();
                match cargo::place_config(&cargo::home_dir(&home), &cargo_text, owner) {
                    Ok(vars) => proxy_env.extend(vars),
                    Err(e) => app
                        .log
                        .line(&format!("direct turn cargo config not placed: {e}")),
                }
                cargo_config_text = Some(cargo_text);
                st.proxy = Some(p);
            }
            // The run goes on without packages; the engine's installs fail where it sees them.
            Err(e) => app
                .log
                .line(&format!("direct turn packages not available: {e}")),
        }
    } else {
        app.log
            .line("direct turn package proxy not started: no token or no index");
    }

    // 6b. the container runtime, for an engine user other than root: `ham-containers up` launches the session's rootless runtime
    // in the background and the engine's tests reach it through four variables. A runtime that is missing or fails to start is
    // logged and the run goes on without it; the engine then finds no Docker, as before.
    let mut containers_env = BTreeMap::new();
    let mut runtime_pids: Vec<u32> = Vec::new();
    match cfg.engine_user.as_deref() {
        Some(user) if !user.is_empty() && user != "root" => {
            let launcher = deps.containers.clone();
            let containers_start = ContainersStart {
                user: user.to_string(),
                log: app.log.clone(),
            };
            // Its own task: a start that panics ends as "not available", the same as one that returns an error.
            let started = tokio::spawn(async move { launcher.start(containers_start).await })
                .await
                .unwrap_or_else(|_| Err("start failed unexpectedly".to_string()));
            match started {
                Ok(c) => {
                    containers_env = c.env();
                    runtime_pids = c.processes();
                    st.containers = Some(c);
                }
                Err(e) => app
                    .log
                    .line(&format!("direct turn containers not available: {e}")),
            }
        }
        _ => app
            .log
            .line("direct turn containers not started: no engine user"),
    }

    // 7. the engine's configuration
    let socket = cfg.runtime_dir.join("creds.sock");
    let model_region = req.model.region.clone();
    // Failover: the routes after the user's model. The engine's provider settings carry one output limit and one
    // window for the whole run and do not change them on a switch (measured 2026-10-15), so with routes both are the smallest of the
    // routes: no switch can send a limit a route does not allow.
    let routes: Vec<Route> = if cfg.direct.failover && !req.failover.is_empty() {
        std::iter::once(&req.model)
            .chain(req.failover.iter())
            .map(Route::from)
            .collect()
    } else {
        Vec::new()
    };
    let (max_output_tokens, context_tokens) = if routes.len() > 1 {
        effective_limits(&routes)
    } else {
        (req.model.max_output_tokens, req.model.context_tokens)
    };
    let ectx = EmitContext {
        engine: req.engine.clone(),
        model: req.model.id.clone(),
        region: model_region.clone(),
        data_dir: data_dir.clone(),
        config_dir: config_dir.clone(),
        agentd_exe: cfg.agentd_exe.clone(),
        socket: socket.clone(),
        max_output_tokens: Some(max_output_tokens),
        context_tokens: Some(context_tokens),
        compaction: user_settings.compaction_block(),
    };
    let aws_config = emit_aws_config(&ectx).map_err(|e| format!("aws config: {e}"))?;
    let built_in =
        emit_engine_for(&ectx, &profile.version).map_err(|e| format!("engine config: {e}"))?;
    let vars: BTreeMap<&str, String> = [
        ("workspace", wt.repo.display().to_string()),
        ("dataDir", data_dir.display().to_string()),
        ("configDir", config_dir.display().to_string()),
        ("model", req.model.id.clone()),
        ("region", model_region.clone()),
        (
            "fixtures",
            fixtures_dir(&cfg.fixtures_root, &profile.name)
                .display()
                .to_string(),
        ),
    ]
    .into();
    emit_profile(&profile, &cfg.templates_root, &vars)
        .map_err(|e| format!("profile config emitters: {e}"))?;
    let mut launch = resolve_launch(&profile, Transport::Acp, &vars)
        .map_err(|e| format!("engine launch: {e}"))?;
    // The arguments only a direct run adds (the profile is shared with the old line, which keeps its own argv). The reasoning level
    // is the user's own, else the level of the model spec; `off` and a spec with no level add none.
    let reasoning = if crate::engine::direct::has_reasoning_flag(&req.engine) {
        user_settings.reasoning_launch(req.model.default_reasoning_effort.as_deref())
    } else {
        ReasoningLaunch::NONE
    };
    launch.argv = crate::engine::direct::direct_launch_argv(
        &req.engine,
        std::mem::take(&mut launch.argv),
        reasoning.effort,
    );
    check_reasoning_launch(&launch.argv, &reasoning)?;
    if let Some(source) = reasoning.source {
        app.log.line(&format!(
            "direct reasoning: {} ({})",
            reasoning.effort.flag().unwrap_or("off"),
            match source {
                ReasoningSource::Default => "the model's default",
                ReasoningSource::Explicit => "set in your engine settings",
                ReasoningSource::Off => "switched off in your engine settings",
            }
        ));
    }
    st.reasoning_effort = reasoning.effort.flag().map(str::to_string);
    st.reasoning_source = reasoning.source.map(|s| s.word().to_string());
    let mut env: BTreeMap<String, String> = [
        ("HOME", home.display().to_string()),
        ("TMPDIR", tmp.display().to_string()),
        ("USER", "agent".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    env.extend(launch.env.clone());
    env.extend(built_in.env);
    env.extend(crate::engine::direct::launch_env(
        &req.engine,
        &user_settings,
    ));
    env.extend(aws_env(&ectx, &aws_config));
    for k in launch.env_remove.iter().map(String::as_str).chain([
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
    ]) {
        env.remove(k);
    }
    env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    // mypy 2.x keeps its incremental cache in SQLite databases under ./.mypy_cache, which in a session is the repository on the session
    // storage (/mnt/workspace): it crashed there and ran once its cache moved to /tmp. The engine's
    // temp directory is on the VM's local disk.
    env.insert(
        "MYPY_CACHE_DIR".into(),
        tmp.join("mypy-cache").display().to_string(),
    );
    // The package tools reach the proxy; no offline switch is set, the engine installs what its tests need.
    env.extend(proxy_env);
    // The build switches for a small session hold nothing of the network, so cargo gets them with or without a proxy.
    env.extend(cargo::build_env_with(
        cfg.direct.cargo_jobs,
        &start.build.unwrap_or_default(),
    ));
    // The target folder is on the root volume, outside the checkout: the checkout is on the session's managed storage, which holds
    // 1 GiB and reports no use, and a cargo `target/` there fills it unseen. A folder that cannot be made safely is logged and the run goes on
    // with cargo's own default, as it does without a cargo config.
    if let Some(dir) = cargo::target_dir(&cfg.direct.cargo_target, &agent_data) {
        match cargo::place_target_dir(&dir, owner) {
            Ok(vars) => env.extend(vars),
            Err(e) => app
                .log
                .line(&format!("direct turn cargo target folder not placed: {e}")),
        }
    }
    env.extend(containers_env);
    if let Some(k) = &launch.model_env {
        env.insert(k.clone(), acp_model_value(&req.engine, &req.model.id));
    }
    // The `landlock` mode: the engine is agentd's own user, so a Java program takes `user.home` from a password database that names a folder no
    // grant covers, and `java.io.tmpdir` from `/tmp`. The run's own home and temp folder are named for it.
    if sandbox_mode == Mode::Landlock {
        let names = crate::sandbox::tool_env(&env, &home, &tmp);
        env.extend(names);
    }

    // 8. the engine user owns what it writes
    if let Some(o) = owner {
        chown_tree(&[&wt.root, &data_dir, &config_dir, &home, &tmp], o).await;
        workspace::set_engine_uid(&wt, o.0);
    }

    // 8b. the Landlock sandbox of the engine's family: the folders the engine writes, the configuration it reads, and agentd's own
    // state declared private, so a layout that would put one inside the other is refused here and not discovered by the engine.
    let sandbox = if sandbox_mode == Mode::Landlock {
        let endpoint = std::env::var("AWS_EC2_METADATA_SERVICE_ENDPOINT").ok();
        // The engine's own configuration directory: read only for an engine that writes nothing there, read and write for one that keeps state in it.
        let mut policy = crate::engine::direct::grant_config_dir(
            Policy::for_config(cfg, endpoint.as_deref())
                .with_read_write(&wt.repo)
                .with_read_write(&data_dir)
                .with_read_write(&home)
                .with_read_write(&tmp),
            &req.engine,
            &config_dir,
        )
        // The root of the harness's files when it is not the config directory (an engine that keeps state there): read only, like the rest.
        .with_read_only(&layout.root)
        // What the modules place for the engine to read (the attachments and the research repositories) is below this folder; it
        // exists only when a module made it.
        .with_optional_read_only(&agent_data.join(context::CONTEXT_DIR))
        .with_private(&agent_data.join("input"))
        .with_private(&agent_data.join("input.bundle"))
        // The rest of agentd's own state: the end sequence's files, its private repositories, and the credential socket's folder. None may
        // sit under a folder the engine is granted, the shared scratch directories included.
        .with_private(&agent_data.join("turns"))
        .with_private(&agent_data.join("snapshot.git"))
        .with_private(&agent_data.join("facts.git"))
        .with_private(&agent_data.join("probe.git"))
        .with_private(&cfg.runtime_dir);
        if let Some(dir) =
            cargo::target_dir(&cfg.direct.cargo_target, &agent_data).filter(|d| d.is_dir())
        {
            policy = policy.with_read_write(&dir);
        }
        // The shared scratch directories come last: one that holds a path of agentd's or of the run is left out and logged.
        let (policy, left_out) = policy.with_shared_tmp();
        // The nobody route: agentd's own namespaces go into the policy, so the launcher can check that it was started in other ones.
        let policy = if identity == Identity::Nobody {
            policy.with_nobody(route_of_this_process().map_err(|e| format!("engine sandbox: {e}"))?)
        } else {
            policy
        };
        policy
            .validate()
            .map_err(|e| format!("engine sandbox: {e}"))?;
        app.log.line(&format!(
            "direct turn engine sandbox: landlock, identity {}, {} writable folder(s), {} denied port(s)",
            identity.word(),
            policy.read_write.len(),
            policy.deny_connect.len()
        ));
        if !left_out.is_empty() {
            app.log.line(&format!(
                "direct turn engine sandbox: not granted because a folder of agentd or of the run is inside: {}",
                left_out
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Some(Sandbox {
            launcher: cfg.agentd_exe.clone(),
            policy: Arc::new(policy),
            identity_helper: (identity == Identity::Nobody).then(|| cfg.identity_helper.clone()),
        })
    } else {
        None
    };

    // 9. the user's rules and the launch settings, after the chown so that root owns them. Where they go, who may write there and which
    // switches the launch must carry are the engine's strategy (`crate::engine::layout`); the same call serves every engine.
    let placed = rules_place::place_rules(&layout.rules, &start.rules)?;
    app.log.line(&format!(
        "direct turn rules placed: {} file(s)",
        placed.len()
    ));
    let locked = crate::engine::layout::lock_settings(
        &req.engine,
        &layout,
        &user_settings.disabled_tools(&cfg.direct.disabled_tools),
        &mut env,
    )
    .map_err(|e| format!("engine settings: {e}"))?;
    let (root_var, root_value) = crate::engine::layout::config_root_env(&layout);
    env.insert(root_var.to_string(), root_value);
    // Telemetry and self-update are off for every engine, and a launch that lacks a switch of its engine does not start.
    crate::engine::layout::check_kill_switches(&req.engine, &env, locked.as_deref())?;
    // The place of the harness's files belongs to root from here: a root-owned rules/ or locked/ inside a directory the engine owns could be
    // renamed away and replaced (review of 2026-10-13).
    let given = crate::engine::layout::protect(&layout, owner).await;
    // The owner is read back from the directory itself, so the line says what the engine will find, not what was meant.
    let owner_uid = std::fs::metadata(&config_dir).map_or_else(
        |_| "unknown".to_string(),
        |m| std::os::unix::fs::MetadataExt::uid(&m).to_string(),
    );
    app.log.line(&format!(
        "direct turn config directory protected: given_to_root={given} owner_uid={owner_uid}"
    ));

    // 10. the credential socket
    let server = serve_credentials_for(
        &socket,
        app.creds.clone(),
        owner,
        Callers::for_mode(sandbox_mode),
    )
    .await
    .map_err(|e| format!("credential socket: {e}"))?;
    let result = async {
        // The modules of the seam, after the credential socket is up and before anything is asked of the engine: they may add to its
        // environment, name the session directory, write under its configuration directory and refuse the run.
        let mut session_cwd: Option<String> = None;
        let mut loop_runs = false;
        if !hooks.is_empty() {
            let mut ctx = StartupCtx::new(
                hook_env,
                &start,
                &wt.repo,
                &config_dir,
                &data_dir,
                &home,
                &tmp,
                owner,
                sandbox.clone(),
                &mut env,
                &mut session_cwd,
                &launch.argv,
                &cfg.engine_path,
                profile.mcp.as_ref(),
                cargo_config_text.as_deref(),
            );
            // The runtime's processes were named when it started, before the engine existed: the engine cannot have chosen them.
            ctx.runtime_pids.clone_from(&runtime_pids);
            hooks.startup(&mut ctx).await?;
            loop_runs = ctx.nudge_off;
        }
        // The engine's last word on its environment, now that the modules added theirs and the stage's kill switches are known. A refusal is a
        // line and the run goes on without what it would have added (as a tools module that cannot register goes on without tools).
        if let Err(e) = crate::engine::config::finalize_env(
            &req.engine,
            &profile.version,
            profile.mcp.as_ref(),
            &cfg.templates_root,
            &user_settings.disabled_tools(&cfg.direct.disabled_tools),
            &mut env,
        ) {
            app.log
                .line(&format!("direct turn engine environment not finished: {e}"));
        }
        // 12. the rules preflight, before any model call
        let mut preflight = DirectRulesPreflight {
            ran: false,
            ms: None,
        };
        if !placed.is_empty()
            && crate::engine::layout::placement_check(&req.engine)
                == crate::engine::layout::PlacementCheck::Files
        {
            // The engine cannot list its rules: the files on the disk are the check.
            placement::verify_rules_placed(&req.engine, &layout, &placed, &wt.repo)?;
            app.log.line(&format!(
                "direct turn rules placement verified on the disk: {} file(s)",
                placed.len()
            ));
        } else if !placed.is_empty()
            && let Some(argv) = crate::engine::direct::rules_listing_argv(
                &req.engine,
                launch.argv.first().map_or("", String::as_str),
                &config_dir,
            )
        {
            let t = std::time::Instant::now();
            let spec = LaunchSpec {
                argv,
                env: env.clone(),
                cwd: wt.repo.clone(),
                path: cfg.engine_path.clone(),
                stderr_log: data_dir.join("rules-listing-stderr.log"),
                run_as: owner,
                sandbox: sandbox.clone(),
            };
            let listing = preflight::run_listing(&spec).await;
            preflight.ran = true;
            preflight.ms = Some(u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
            st.rules_preflight = Some(preflight);
            preflight::check_rules_listing(&placed, &listing?)?;
        }
        st.rules_preflight = Some(preflight);

        // The session's cwd is the git top level of the worktree; the nudge, when asked for, and the review round need a probe of the tree.
        let top = workspace::toplevel(&wt)
            .await
            .map_err(|e| format!("worktree: {e}"))?;
        let top = session_cwd.unwrap_or(top);
        // The goal loop sends the follow-up prompts itself, so while it runs the request's nudge is off; with the loop off, or with no
        // loop module, the nudge is what it always was.
        st.nudge_off_by_loop = loop_runs && start.nudge.as_ref().is_some_and(|n| n.max_nudges > 0);
        if st.nudge_off_by_loop {
            app.log
                .line("direct turn nudge off: the goal loop sends the follow-up prompts");
        }
        let nudge = start
            .nudge
            .as_ref()
            .filter(|n| n.max_nudges > 0 && !loop_runs)
            .map(|n| Nudge {
                text: n.text.clone(),
                max: n.max_nudges,
            });
        // The review round: the request's member laid over the stage's settings. A round that can run needs the probe too: it
        // reads the work tree before and after the round to say whether the round changed a file.
        let review_plan = ReviewPlan::new(
            start.review.as_ref(),
            cfg.direct.review_round,
            cfg.direct.review_guard,
            &base,
            Arc::new(GitRepo::new(&wt.repo, &base)),
        );
        let probe: Box<dyn TreeProbe> = if nudge.is_some() || review_plan.is_on() {
            Box::new(GitProbe::open(&agent_data, &wt.repo).await)
        } else {
            Box::new(NoProbe)
        };
        let silence = Duration::from_secs(u64::from(
            req.provider_silence_s
                .filter(|s| *s > 0)
                .unwrap_or(crate::acp::DEFAULT_PROVIDER_SILENCE_S),
        ));
        // The deadline counts from the moment the invocation was accepted, whatever setup cost.
        let deadline = received + Duration::from_secs(u64::from(req.timeout_s));

        // 11. the engine
        let spec = LaunchSpec {
            argv: launch.argv.clone(),
            env,
            cwd: wt.repo.clone(),
            path: cfg.engine_path.clone(),
            stderr_log: data_dir.join("engine-stderr.log"),
            run_as: owner,
            sandbox: sandbox.clone(),
        };
        app.log.line("direct turn engine starting");
        let (mut child, stderr_capture) =
            spawn_captured(&spec).map_err(|e| format!("engine start: {e}"))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            kill_group(&mut child).await;
            return Err("engine start: no stdio".to_string());
        };
        // The brake inside a round leaves the engine's own CPU out of the session's; a fresh session's engine is told in `respawn`.
        hook_env.ctl.set_engine_pid(child.id().unwrap_or(0));
        // The engine process of the run, restartable: a fresh session on a dead or hung engine starts the same launch again.
        let audit = audit_sink(app, req, &agent_data);
        let mut host = EngineHost {
            spec,
            child,
            capture: stderr_capture,
            audit: audit.clone(),
            hooks: hooks.clone(),
            pid: hook_env.ctl.engine_pid_cell(),
        };
        let activity = Activity::new();
        // A tool call that goes silent is bounded for the engines that ask for it (the default is none for the first engine).
        let tool_idle = crate::engine::watchdog::tool_idle(&req.engine, cfg.direct.tool_idle_s);
        activity.set_tool_idle(tool_idle);
        if let Some(limit) = tool_idle {
            app.log.line(&format!(
                "direct turn tool-idle limit: {} s",
                limit.as_secs()
            ));
        }
        let mut client = AcpClient::new(stdout, stdin)
            .with_prompt_guard(crate::engine::config::prompt_guard(&req.engine))
            .with_cancel(controls.subscribe(), CANCEL_GRACE)
            .with_activity(activity.clone());
        if let Some(tap) = hooks.tap() {
            client = client
                .with_tap(tap)
                .with_interrupt(hook_env.ctl.client_side(), CANCEL_GRACE);
        }
        let mut collector = DirectCollector::new(rec.clone())
            .with_engine(&req.engine)
            .with_secret_set(st.secrets.clone())
            .with_hooks(hooks.clone());
        let snapshots =
            start_snapshots(app, req, &agent_data, &wt, &base, st.snapshots.clone()).await;
        // Stale cargo build output is pruned while a prompt runs: the look stops with the stage loop, below.
        let pruning = start_prune(app, &agent_data, rec);
        // The first prompt of a slice after the first is the harness's: the task, the work tree's facts and the earlier session's last message.
        let slice_prompt = match &restored {
            Some(r) => Some(
                slice::first_prompt(
                    &agent_data,
                    &wt,
                    &base,
                    &start.prompt,
                    r,
                    st.secrets.snapshot(),
                )
                .await,
            ),
            None => None,
        };
        let first = slice_prompt.as_deref().unwrap_or(&start.prompt);
        let plan = PromptPlan {
            cwd: top,
            engine: req.engine.clone(),
            model: req.model.id.clone(),
            // The user's text as typed; a run in the earlier records folder gets one line before it (COMPAT-CAH-DATA).
            prompt: match &st.records_line {
                Some(line) => format!("{line}\n\n{first}"),
                None => first.to_string(),
            },
            nudge,
            cancel: Some(controls.subscribe()),
            // What is left of the deadline now that everything before the prompt is done.
            timeout: deadline.saturating_duration_since(Instant::now()),
            silence,
            retry: {
                let retry = RetryPolicy::standard(
                    cfg.direct.provider_retries,
                    cfg.direct.provider_retry_pause,
                )
                .with_log(app.log.clone());
                // The session's two disks: measured at each prompt boundary and, fresh, after a -32603. Both settings at 0 switch
                // the measurement off.
                if cfg.direct.disk_low_mb > 0 || cfg.direct.workspace_limit_mb > 0 {
                    retry.with_disk(disk::DiskCheck::new(
                        // The test write goes to the run's own folder, the checkout's parent on the managed storage and the
                        // engine's state folder, besides the two roots.
                        Arc::new(
                            disk::HostDisk::new(cfg.data_root.clone(), cfg.workspace_root.clone())
                                .with_write_dirs([
                                    agent_data.clone(),
                                    wt.root.clone(),
                                    data_dir.clone(),
                                ]),
                        ),
                        disk::Limits {
                            root_low_mb: cfg.direct.disk_low_mb,
                            workspace_limit_mb: cfg.direct.workspace_limit_mb,
                        },
                    ))
                } else {
                    retry
                }
            },
            // The retry setting at 0 means one prompt, as before the failover slices: the engine-stop resume follows it.
            engine_stop_resumes: if cfg.direct.provider_retries == 0 {
                0
            } else {
                cfg.direct.engine_stop_resumes
            },
            failover: (routes.len() > 1).then(|| {
                let mut f = FailoverPlan::new(routes.clone());
                f.fallback_retries = cfg.direct.failover_retries;
                f.return_dwell = cfg.direct.failover_dwell;
                f.wait_base = cfg.direct.failover_wait;
                f.wait_cap = cfg.direct.failover_wait_cap;
                f.notice_after = cfg.direct.outage_notice;
                if cfg.direct.failover_probe {
                    let creds = app.creds.clone();
                    let creds_fn: CredsFn = Arc::new(move || {
                        let creds = creds.clone();
                        Box::pin(async move { creds.current().await.map_err(|e| e.to_string()) })
                    });
                    f = f.with_probe(Arc::new(BedrockProbe::new(creds_fn)));
                }
                f
            }),
        };
        // The stage loop: the automatic remedies of the recovery ladder always run (one fresh session for a history the
        // provider refuses or a dead engine, one resume of a silence); a run that was given a wait also asks its user what to do when it
        // would otherwise end, and is the run of today until a stop that cannot be mended.
        let input = input_wiring(
            app,
            req,
            controls,
            rec,
            &agent_data,
            &wt,
            &base,
            &start.prompt,
            &routes,
            audit,
            &st.quarantined,
            &st.secrets,
            review_plan,
            start.limits.unwrap_or_default(),
        )
        .await;
        let end = run_direct_stages(
            &mut client,
            &mut collector,
            &activity,
            &plan,
            probe.as_ref(),
            input,
            &mut host,
        )
        .await;
        // The snapshot task stops before the engine is reaped and the tree is committed: no git of its own may overlap them.
        if let Some(mut h) = snapshots {
            h.0.abort();
            let _ = (&mut h.0).await;
        }
        if let Some(task) = pruning {
            task.stop().await;
        }
        drop(client); // closes stdin: ACP engines exit on EOF
        let grace = if end.raw.setup_error.is_some() {
            0
        } else {
            u64::from(launch.cancel.kill_after_s)
        };
        let exit = reap_engine(&mut host.child, grace).await;
        hooks.event(&Event::EngineStopped(EngineStop::Reaped));
        if matches!(
            end.raw.watched,
            Watched::Done(Err(AcpError::Closed | AcpError::Io(_)))
        ) {
            st.exit_note = crate::turn::closed_note(&exit, &host.capture);
        }
        st.pinned = end.pinned;
        st.usage = end.usage;
        st.tool_calls = collector.tool_calls();
        st.nudges_sent = end.nudges_sent;
        st.max_silent_gap_s = end.max_silent_gap_s;
        st.prompt_s = end.prompt_started.map(|t| t.elapsed().as_secs());
        st.attempts = end.attempts;
        st.switches = end.switches;
        st.final_model = end.final_model;
        st.failover = end.failover;
        st.outage_s = end.outage_s;
        st.interventions = end.interventions;
        st.fresh_sessions = end.fresh_sessions;
        st.input_note = end.input_note;
        st.engine_stop = end.engine_stop;
        st.late_answers = end.late_answers;
        st.hook_stop = end.hook_stop;
        st.disk_full = end.disk_full;
        st.review = end.review;
        Ok(end.raw)
    }
    .await;
    server.abort();
    let _ = std::fs::remove_file(&socket);
    result
}

// ---- the answer window --------------------------------------------------------------------------------------------

/// The engine process of a run. A fresh session on a dead or hung engine stops it and starts the same launch again.
struct EngineHost {
    spec: LaunchSpec,
    child: tokio::process::Child,
    capture: crate::engine::StderrCapture,
    /// Where the digest of every stop goes.
    audit: Arc<dyn AuditSink>,
    /// The modules of the run: told when the process is stopped and started again.
    hooks: Hooks,
    /// The process id of the engine for the modules (`DriverCtl::engine_pid_cell`): set again when the process is started again.
    pid: Arc<std::sync::atomic::AtomicU32>,
}

impl Respawn for EngineHost {
    type R = tokio::process::ChildStdout;
    type W = tokio::process::ChildStdin;

    fn respawn(&mut self) -> BoxFuture<'_, Result<(Self::R, Self::W), String>> {
        Box::pin(async move {
            // The old process and everything its tools started.
            kill_group(&mut self.child).await;
            let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
            self.hooks
                .event(&Event::EngineStopped(EngineStop::Restarted));
            let (mut child, capture) =
                spawn_captured(&self.spec).map_err(|e| format!("engine start: {e}"))?;
            let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                kill_group(&mut child).await;
                return Err("engine start: no stdio".to_string());
            };
            self.pid
                .store(child.id().unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
            self.child = child;
            self.capture = capture;
            Ok((stdout, stdin))
        })
    }

    fn audit(&self) -> Option<Arc<dyn AuditSink>> {
        Some(self.audit.clone())
    }
}

/// The harness facts a digest is made of that the driver does not hold itself: the git half comes from the private repository
/// ([`GitSource`], never `git` in the engine's repository), the rest from the engine's error output; the driver adds the attempts, the
/// tool notes and the messages.
struct RunSources {
    git: Arc<GitSource>,
    /// The paths moved aside at launch (repository hooks, plugins, agents): their absence is the harness's, not a change of the work.
    hidden: Vec<String>,
    task: String,
    /// The run's secret set; a digest is redacted with what it holds when the digest is gathered.
    secrets: SecretSet,
    stderr_log: PathBuf,
}

/// True when `path` is one of the paths agentd moved aside at launch, or is under one of them.
fn is_hidden(hidden: &[String], path: &str) -> bool {
    hidden.iter().any(|h| {
        path == h
            || path
                .strip_prefix(h.as_str())
                .is_some_and(|r| r.starts_with('/'))
    })
}

/// The work tree as the user will receive it: what [`GitSource`] measures, less the paths agentd moved aside at launch (they are put
/// back before the end commit, so their absence now is not a change; counting them would make an untouched tree look touched and
/// hide a stop that changed no file).
struct VisibleTree {
    git: Arc<GitSource>,
    hidden: Vec<String>,
}

impl input::TreeSource for VisibleTree {
    fn changed_files(&self, limit: usize) -> BoxFuture<'_, Option<input::ChangedFiles>> {
        Box::pin(async move {
            let all = self
                .git
                .changed_files(limit.max(snapshot::MAX_FACT_ITEMS))
                .await?;
            let kept: Vec<String> = all
                .paths
                .iter()
                .filter(|p| !is_hidden(&self.hidden, p))
                .cloned()
                .collect();
            let removed = u32::try_from(all.paths.len() - kept.len()).unwrap_or(0);
            Some(input::ChangedFiles {
                total: all.total.saturating_sub(removed),
                paths: kept.into_iter().take(limit).collect(),
            })
        })
    }
}

/// The last `max` bytes of a regular file the engine can write to, read without following a link and without blocking: a link or a
/// FIFO the engine planted there is not read.
fn tail_of_file(path: &Path, max: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let len = meta.len();
    let max_u64 = u64::try_from(max).unwrap_or(u64::MAX);
    file.seek(SeekFrom::Start(len.saturating_sub(max_u64)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(max_u64).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

impl input::DigestSource for RunSources {
    fn gather(&self) -> BoxFuture<'_, input::DigestSources> {
        Box::pin(async move {
            let mut src = input::DigestSources {
                task: self.task.clone(),
                secrets: self.secrets.snapshot(),
                stderr_tail: tail_of_file(&self.stderr_log, 3 * 1024),
                ..input::DigestSources::default()
            };
            self.git.facts().await.fill(&mut src);
            // A path agentd moved aside at launch is put back before the delivered tree is committed: it is no change of the work.
            let before = src.changed_files.len();
            src.changed_files.retain(|p| !is_hidden(&self.hidden, p));
            let removed = u32::try_from(before - src.changed_files.len()).unwrap_or(0);
            src.files_total = src.files_total.saturating_sub(removed);
            if src.changed_files.is_empty() && src.files_total == 0 {
                src.diff_stat = None;
            }
            src
        })
    }
}

/// The audit copies under the run's `agents/direct/turns/1/` prefix: written to a file in agentd's own directory first, then uploaded
/// with the same retries as the bundle.
struct S3Audit {
    s3: S3Objects,
    creds: Arc<crate::creds::CredStore>,
    bucket: String,
    run: String,
    turn: u32,
    dir: PathBuf,
    pause: Duration,
    log: Arc<dyn LogSink>,
}

/// The S3 key of an audit copy: `runs/<run>/agents/direct/turns/1/<file>` (`stop-digest-<n>.json`, `fresh-session-<n>.md`).
pub fn audit_key(run_id: &str, file: &str) -> String {
    audit_key_t(run_id, DIRECT_TURN, file)
}

/// [`audit_key`] for slice `turn`.
pub fn audit_key_t(run_id: &str, turn: u32, file: &str) -> String {
    format!("{}{file}", s3::turn_prefix_t(run_id, turn))
}

impl AuditSink for S3Audit {
    fn put<'a>(&'a self, file: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let done = async {
                std::fs::create_dir_all(&self.dir)
                    .map_err(|e| format!("{}: {e}", self.dir.display()))?;
                let path = self.dir.join(file);
                std::fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
                let creds = self.creds.current().await.map_err(|e| e.to_string())?;
                let uri = s3::uri(&self.bucket, &audit_key_t(&self.run, self.turn, file));
                upload_file(&self.s3, &uri, &creds, &path, self.pause).await
            }
            .await;
            // An audit copy is never worth the run: the failure is logged and the caller goes on.
            if let Err(e) = &done {
                self.log.line(&format!(
                    "direct turn audit copy {file} not uploaded: {}",
                    scrub_detail(e)
                ));
            }
            done
        })
    }
}

/// The sink of the audit copies of a run (the digest of every stop, the digest of a fresh session).
fn audit_sink(app: &App, req: &DirectInvocationRequest, agent_data: &Path) -> Arc<dyn AuditSink> {
    Arc::new(S3Audit {
        s3: S3Objects::new(req.region.clone(), app.cfg.s3_endpoint.clone()),
        creds: app.creds.clone(),
        bucket: s3::split_uri(&req.request.uri)
            .map(|(b, _)| b.to_string())
            .unwrap_or_default(),
        run: req.run_id.clone(),
        turn: req.turn,
        dir: agent_data.join("audit"),
        pause: app.cfg.direct.upload_retry_pause,
        log: app.log.clone(),
    })
}

/// The wiring of the stage loop, or `None` when the run is the run of today: the instruction channel already taken, or a part that cannot
/// be made (logged). With no wait asked for (the request's `inputWaitS`, else `AGENTD_DIRECT_INPUT_WAIT_S`, default 0) or no question
/// allowed, the policy waits zero: the loop never asks, but the automatic remedies of the recovery ladder still run (one fresh session for
/// a history the provider refuses or a dead engine, one resume of a silence), as the whole-agent tests of package A5 require.
#[allow(clippy::too_many_arguments)]
async fn input_wiring(
    app: &App,
    req: &DirectInvocationRequest,
    controls: &Arc<TurnControls>,
    rec: &Arc<EventRecorder>,
    agent_data: &Path,
    wt: &Worktree,
    base: &str,
    task: &str,
    routes: &[Route],
    audit: Arc<dyn AuditSink>,
    hidden: &[String],
    secrets: &SecretSet,
    review: ReviewPlan,
    limits: harness_protocol::direct::RunLimits,
) -> Option<InputWiring> {
    let d = &app.cfg.direct;
    let wait = req
        .input_wait_s
        .map_or(d.input_wait, |s| Duration::from_secs(u64::from(s)));
    // The request's `limits` member replaces the stage's counts for this run; a member left out keeps the stage's.
    let max_requests = limits.questions.unwrap_or(d.input_max_requests);
    let asks = !wait.is_zero() && max_requests > 0;
    let instructions = controls.take_instructions()?;
    let context = match ContextDir::create(agent_data) {
        Ok(c) => c,
        Err(e) => {
            app.log
                .line(&format!("direct turn stage loop off: {}", scrub_detail(&e)));
            return None;
        }
    };
    let git = match GitSource::open(agent_data, &wt.repo, base).await {
        Ok(g) => Arc::new(g),
        Err(e) => {
            app.log
                .line(&format!("direct turn stage loop off: {}", scrub_detail(&e)));
            return None;
        }
    };
    if asks {
        app.log.line(&format!(
            "direct turn answer window on: wait {} s, {} question(s) at most",
            wait.as_secs(),
            max_requests
        ));
    } else {
        app.log
            .line("direct turn automatic recovery on, no answer window");
    }
    Some(InputWiring {
        policy: input::InputPolicy {
            wait: if asks { wait } else { Duration::ZERO },
            max_requests: d.input_max_requests,
            margin: d.input_margin,
            min_window: d.input_min_window,
            max_auto_sessions: d.auto_sessions_max,
            max_user_sessions: d.user_sessions_max,
        }
        .with_limits(&limits),
        instructions,
        cancel: controls.subscribe(),
        tree: Arc::new(VisibleTree {
            git: git.clone(),
            hidden: hidden.to_vec(),
        }),
        sources: Arc::new(RunSources {
            hidden: hidden.to_vec(),
            git,
            task: task.to_string(),
            secrets: secrets.clone(),
            stderr_log: agent_data.join("data").join("engine-stderr.log"),
        }),
        context,
        task: task.to_string(),
        engine_label: crate::engine::direct::engine_label(&req.engine).to_string(),
        routes: if routes.len() > 1 {
            routes
                .iter()
                .take(harness_protocol::direct::MAX_NEEDS_INPUT_MODELS)
                .map(|r| NeedsInputModel {
                    id: r.id.clone(),
                    name: r.id.clone(),
                    context_tokens: r.context_tokens,
                })
                .collect()
        } else {
            Vec::new()
        },
        rec: rec.clone(),
        audit: Some(audit),
        log: Some(app.log.clone()),
        review: Some(review),
    })
}

// ---- the prune of stale cargo output -------------------------------------------------------------------------------------

/// Starts the look that prunes the cargo target folder while a prompt runs ([`prune`]); `None` when a setting switches it off or cargo keeps its
/// target folder in the checkout (`AGENTD_CARGO_TARGET_DIR=checkout`: that folder is on the session's managed storage, which `statvfs` of the
/// root volume does not describe). Each prune that removes something is one `notify` event with the reason `disk_pruned` and one log line.
fn start_prune(app: &App, agent_data: &Path, rec: &Arc<EventRecorder>) -> Option<prune::PruneTask> {
    let cfg = &app.cfg;
    let Some(settings) = prune::PruneSettings::from_config(&cfg.direct) else {
        app.log
            .line("direct turn disk prune off: a setting switches it off");
        return None;
    };
    let Some(target) = cargo::target_dir(&cfg.direct.cargo_target, agent_data) else {
        app.log
            .line("direct turn disk prune off: cargo keeps its target folder in the checkout");
        return None;
    };
    let (log, rec) = (app.log.clone(), rec.clone());
    Some(prune::start(
        settings,
        target,
        Arc::new(prune::StatvfsFree::new(cfg.data_root.clone())),
        Arc::new(move |pruned, free_after| {
            let text = prune::notice(pruned, free_after);
            log.line(&format!("direct turn disk pruned: {text}"));
            rec.record(
                EventType::TurnProgress,
                serde_json::json!({
                    "turn": DIRECT_TURN,
                    "kind": "notify",
                    "reason": "disk_pruned",
                    "outageMinutes": null,
                    "detail": text,
                }),
            );
        }),
    ))
}

// ---- the snapshot loop ----------------------------------------------------------------------------------------------------------

/// The snapshot task; dropping it (an early return, the safety timeout) stops it.
struct SnapshotTask(tokio::task::JoinHandle<()>);

impl Drop for SnapshotTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts the periodic snapshot task; `None` when snapshots are off or the snapshot repository could not be made.
async fn start_snapshots(
    app: &App,
    req: &DirectInvocationRequest,
    agent_data: &Path,
    wt: &Worktree,
    base: &str,
    stats: Arc<Mutex<DirectSnapshots>>,
) -> Option<SnapshotTask> {
    let every = match app.cfg.direct.snapshot_interval {
        Some(d) => d,
        None => match req.snapshot_every_s.unwrap_or(DEFAULT_SNAPSHOT_EVERY_S) {
            0 => return None,
            s => Duration::from_secs(u64::from(s)),
        },
    };
    let mut snapshotter = match Snapshotter::new(agent_data, &wt.repo, base).await {
        Ok(s) => s,
        Err(e) => {
            app.log.line(&format!("direct turn snapshots off: {e}"));
            return None;
        }
    };
    let creds = app.creds.clone();
    let log = app.log.clone();
    let s3_objects = S3Objects::new(req.region.clone(), app.cfg.s3_endpoint.clone());
    let (bucket, run) = (
        s3::split_uri(&req.request.uri)
            .map(|(b, _)| b.to_string())
            .unwrap_or_default(),
        req.run_id.clone(),
    );
    // Slice n keeps its snapshots under its own turn, so a later slice never overwrites the one an earlier slice's restore names.
    let turn = req.turn;
    let pause = app.cfg.direct.upload_retry_pause;
    Some(SnapshotTask(tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let outcome = async {
                let Some(made) = snapshotter.snapshot().await? else {
                    return Ok::<_, String>(None);
                };
                let c = creds.current().await.map_err(|e| e.to_string())?;
                let bundle_uri = s3::uri(&bucket, &s3::snapshot_bundle_key_t(&run, turn));
                upload_file(&s3_objects, &bundle_uri, &c, &made.bundle, pause).await?;
                let at = format_rfc3339(now_unix());
                let json_path = made.bundle.with_file_name("snapshot.json");
                std::fs::write(&json_path, snapshot_json(&made, &at)).map_err(|e| e.to_string())?;
                let json_uri = s3::uri(&bucket, &s3::snapshot_json_key_t(&run, turn));
                upload_file(&s3_objects, &json_uri, &c, &json_path, pause).await?;
                Ok(Some((made.count, at)))
            }
            .await;
            let mut s = stats.lock().unwrap_or_else(|p| p.into_inner());
            match outcome {
                Ok(Some((count, at))) => {
                    s.count = count;
                    s.last_at = Some(at);
                }
                Ok(None) => {}
                Err(e) => {
                    s.failures += 1;
                    log.line(&format!(
                        "direct turn snapshot failed: {}",
                        scrub_detail(&e)
                    ));
                }
            }
        }
    })))
}

/// PUTs `file` to `uri` create-only (`If-None-Match: *`, [`S3Objects::put_file_new_retrying`]), trying again [`UPLOAD_RETRIES`] times
/// with a pause that doubles when the request fails. A 412 on the first try is a key that another writer took (`PutNew::Exists`),
/// answered at once. A 412 after a try whose answer was lost is checked against the checksum agentd sent: agentd's own object counts as
/// stored, another one is `PutNew::Exists` (the end sequence then records the conflict and writes its own object over it).
async fn upload_new(
    s3_objects: &S3Objects,
    uri: &str,
    creds: &harness_protocol::api::internal::ProcessCredentials,
    file: &Path,
    pause: Duration,
) -> Result<(PutNew, Option<OutputPin>), String> {
    s3_objects
        .put_file_new_retrying_receipt(uri, creds, file, UPLOAD_RETRIES, pause)
        .await
}

/// PUTs `file` to `uri`, trying again [`UPLOAD_RETRIES`] times with a pause that doubles. For the objects that are rewritten or
/// numbered by design (the snapshot, the stop digests, the audit copies) and for the object that replaces a foreign one at an
/// end-of-run key; the bundle and `summary.json` use [`upload_new`] first.
async fn upload_file(
    s3_objects: &S3Objects,
    uri: &str,
    creds: &harness_protocol::api::internal::ProcessCredentials,
    file: &Path,
    pause: Duration,
) -> Result<(), String> {
    s3_objects
        .put_file_retrying(uri, creds, file, UPLOAD_RETRIES, pause)
        .await
}

/// [`upload_file`] that returns the record of the write that stored the object: the SHA-256 of the bytes and the version id of that
/// write's answer. The end-of-run objects that replace a foreign one use it.
async fn upload_file_receipt(
    s3_objects: &S3Objects,
    uri: &str,
    creds: &harness_protocol::api::internal::ProcessCredentials,
    file: &Path,
    pause: Duration,
) -> Result<OutputPin, String> {
    s3_objects
        .put_file_retrying_receipt(uri, creds, file, UPLOAD_RETRIES, pause)
        .await
}

/// The turn's `error`: the stop's detail (a stop that is not "reported done"), the upload failure and what the end sequence could
/// not do, on one scrubbed line without the session's secrets; `None` when there is nothing to say.
fn compose_error(
    classified: &Classified,
    upload_error: &Option<String>,
    errors: &[String],
    secrets: &SecretSet,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if classified.outcome != DirectOutcome::ReportedDone
        && let Some(d) = &classified.detail
    {
        parts.push(d.clone());
    }
    parts.extend(upload_error.clone());
    parts.extend(errors.iter().cloned());
    (!parts.is_empty()).then(|| scrub_detail(&secrets.redact(&parts.join("; "))))
}

fn outcome_word(o: TurnOutcome) -> &'static str {
    match o {
        TurnOutcome::Completed => "completed",
        TurnOutcome::Failed => "failed",
        TurnOutcome::Cancelled => "cancelled",
        TurnOutcome::TimedOut => "timed_out",
    }
}

/// Whether the modules' report step is asked at the end of a run that ended as `outcome`. A run the user cancelled and a run that ran into
/// its deadline are wound up without it: the report can take minutes, and a user who sent `ham cancel` waits for the run to end.
/// Every other end, the engine's own stops included, is asked.
pub fn report_step_applies(outcome: DirectOutcome) -> bool {
    !matches!(
        outcome,
        DirectOutcome::StoppedByUser | DirectOutcome::StoppedTimeout
    )
}

/// Why agentd runs no git in the checkout: the reason of an [`workspace::WsError::Untrusted`], else the text of the error that stopped the restore.
fn untrusted_reason(e: &workspace::WsError) -> String {
    match e {
        workspace::WsError::Untrusted(why) => why.clone(),
        other => other.to_string(),
    }
}

/// The end sequence and the result.
#[allow(clippy::too_many_arguments)]
async fn finish(
    app: &App,
    req: &DirectInvocationRequest,
    mut st: State,
    mut raw: RawEnd,
    rec: &Arc<EventRecorder>,
    sink: &Arc<HttpEventSink>,
    flusher: crate::events::Flusher,
    received: Instant,
    started_unix: i64,
    hooks: &Hooks,
    hook_env: &HookEnv,
) -> InvocationResponse {
    let run = req.run_id.as_str();
    // Nothing the engine said may carry the session's secrets out: the invocation's and every one a refresh minted since (the set the
    // credential store grew for the whole run), read at each redaction.
    let secrets = st.secrets.clone();
    raw.message = secrets.redact(&raw.message);
    let mut classified: Classified = classify(&raw);
    // A prompt that ended on error -32603 while the session disk was short ended on the disk, not on the provider.
    classified = classify::with_disk(classified, st.disk_full.as_ref());
    // The engine's own stop is worded with the facts of its last attempt (the loop guard or the mistake limit), not as a cut stream.
    if let Some(report) = &st.engine_stop {
        classified = classify::refine_engine_stop(classified, report);
    }
    if matches!(
        classified.outcome,
        DirectOutcome::StoppedEngineDied | DirectOutcome::StoppedSetupFailed
    ) && let Some(note) = &st.exit_note
    {
        let base = classified.detail.take().unwrap_or_default();
        classified.detail = Some(scrub_detail(&format!("{base}; {note}")));
    }
    // A module of the seam ended the run: the row is "stopped" with its words, whatever the last prompt said.
    if let Some(detail) = &st.hook_stop {
        classified = classify::stopped_by_module(detail);
    }
    classified.detail = classified.detail.take().map(|d| secrets.redact(&d));
    let mut errors: Vec<String> = Vec::new();
    // The engine is gone and nothing writes the tree any more: the modules stop what ran for it, whatever the ending was.
    hooks.shutdown(hook_env, &classified).await;

    // 2. hidden paths and git settings back
    // A repository whose settings could not be put back (or whose `.git` is not its own directory any more) gets no git call of agentd's: the
    // engine's settings could name a program, and it would start in agentd's `git add` with agentd's rights.
    let mut untrusted: Option<String> = None;
    if let Some(wt) = &st.wt {
        if let Some(h) = st.hidden.take()
            && let Err(e) = workspace::restore_hidden(wt, h)
        {
            errors.push(format!("restore hidden paths: {e}"));
        }
        if let Some(g) = &st.git_state
            && let Err(e) = g.restore(wt)
        {
            errors.push(format!("restore git settings: {e}"));
            app.log
                .line(&format!("direct turn repository not trusted: {e}"));
            untrusted = Some(untrusted_reason(&e));
        }
    }

    // 3 and 4. commit what the tree holds, bundle it, upload it
    let mut git = DirectGit {
        base_sha: st.base.clone(),
        head_sha: None,
        committed_by_harness: false,
        files_changed: 0,
        bundle_uri: None,
        bundle_bytes: None,
    };
    let mut upload_error: Option<String> = None;
    let mut tree_files: Option<u32> = None;
    // The flow the run declared; stays `None` when the checkout cannot be read (untrusted), as the summary leaves the member out.
    let mut flow: Option<DirectFlow> = None;
    // An end-of-run object that was in the bucket before agentd's create-only write (S3's 412, or a retry's HEAD showing another writer's
    // checksum): the run ends as a conflict, named by the object. agentd records it, then writes its own object over the foreign one; if
    // that fails the final error says so (not_replaced_text).
    let mut conflict: Option<&'static str> = None;
    // What S3 answered to agentd's own write of each end-of-run object (the SHA-256 of the bytes and the version id). They go to the
    // service in `turn-complete`, which the engine cannot send, so delivery reads exactly these versions and not whatever is at the key later.
    let mut bundle_pin: Option<OutputPin> = None;
    let mut summary_pin: Option<OutputPin> = None;
    let creds = app.creds.current().await;
    let s3_objects = S3Objects::new(req.region.clone(), app.cfg.s3_endpoint.clone());
    let bucket = s3::split_uri(&req.request.uri)
        .map(|(b, _)| b.to_string())
        .unwrap_or_default();
    let pause = app.cfg.direct.upload_retry_pause;
    if let Some(wt) = st.wt.as_ref().filter(|_| untrusted.is_none()) {
        let before = workspace::head(wt).await.ok();
        // Read before the harness makes its own commit below: the final task commit is the one the engine left last.
        flow = Some(flow::of_run(wt, before.as_deref(), st.base.as_deref(), &raw.message).await);
        let author = workspace::TurnAuthor {
            role: DIRECT_ROLE,
            agent: DIRECT_AGENT_ID,
            task: None,
        };
        let committed = match &st.git_state {
            Some(state) => workspace::commit_turn_verified(wt, req.turn, &author, state).await,
            None => workspace::commit_turn_as(wt, req.turn, &author).await,
        };
        let head = match committed {
            Ok(sha) => Some(sha),
            Err(e) => {
                errors.push(format!("commit: {e}"));
                if matches!(e, workspace::WsError::Untrusted(_)) {
                    untrusted = Some(untrusted_reason(&e));
                }
                None
            }
        };
        git.committed_by_harness = head.is_some() && head != before;
        if let Some(base) = &st.base
            && let Ok(set) = workspace::changed_between(wt, base).await
        {
            git.files_changed = u32::try_from(set.len()).unwrap_or(u32::MAX);
            // Measured only against a head the commit step produced: after a failed commit HEAD can lag the tree, and a count of
            // zero would then name a stop by a fact that is not true.
            if head.is_some() {
                tree_files = Some(git.files_changed);
            }
        }
        // Under agentd's own (root-owned) data directory, never under the worktree root the engine user owns: a `turns` link the
        // engine planted there would make root write the bundle where the model chose.
        let file = st
            .agent_data
            .join("turns")
            .join(DIRECT_TURN.to_string())
            .join("bundle");
        match workspace::write_bundle(wt, &file).await {
            Ok(()) => {
                let uri = s3::uri(&bucket, &s3::bundle_key_t(run, req.turn));
                let put = match &creds {
                    Ok(c) => upload_new(&s3_objects, &uri, c, &file, pause).await,
                    Err(e) => Err(e.to_string()),
                };
                match put {
                    Ok((PutNew::Stored, pin)) => {
                        bundle_pin = pin;
                        git.head_sha = head;
                        git.bundle_uri = Some(uri);
                        git.bundle_bytes = std::fs::metadata(&file).ok().map(|m| m.len());
                    }
                    // Another writer's object is under the one key worker-api accepts. agentd records the conflict, then writes its
                    // own bundle over it (the versioned bucket keeps the foreign object as an earlier version). The run stays a
                    // conflict; the head is delivered only when agentd's own bundle is the object under the key.
                    Ok((PutNew::Exists, _)) => {
                        conflict = Some(BUNDLE_OBJECT);
                        let text = upload_conflict_text(BUNDLE_OBJECT);
                        upload_error = Some(format!("upload: {text}"));
                        rec.record(
                            EventType::TurnProgress,
                            upload_conflict_payload(BUNDLE_OBJECT),
                        );
                        app.log
                            .line(&format!("direct turn upload conflict: {text}"));
                        let replaced = match &creds {
                            Ok(c) => upload_file_receipt(&s3_objects, &uri, c, &file, pause).await,
                            Err(e) => Err(e.to_string()),
                        };
                        match replaced {
                            Ok(pin) => {
                                bundle_pin = Some(pin);
                                app.log.line(
                                    "direct turn bundle replaced: agentd's own bundle was written over the object that was there",
                                );
                                git.head_sha = head;
                                git.bundle_uri = Some(uri);
                                git.bundle_bytes = std::fs::metadata(&file).ok().map(|m| m.len());
                            }
                            Err(e) => {
                                app.log.line(&format!(
                                    "direct turn bundle not replaced: {}",
                                    scrub_detail(&e)
                                ));
                                upload_error =
                                    Some(format!("upload: {text}; {}", not_replaced_text(&e)));
                            }
                        }
                    }
                    Err(e) => upload_error = Some(format!("upload: {}", scrub_detail(&e))),
                }
            }
            Err(e) => errors.push(format!("bundle: {e}")),
        }
    }

    // Row 16: the engine's end_turn with a message is "reported done" only when the delivered tree differs from the base.
    classified = refine(
        classified,
        TreeFacts {
            files_changed: tree_files,
        },
    );
    // A repository agentd would not run git in: a done the harness cannot deliver is not one it claims. Any other end keeps its label, and the
    // error text carries the reason (the same rule as a summary that cannot be written).
    if let Some(why) = &untrusted
        && classified.outcome == DirectOutcome::ReportedDone
    {
        classified = classify::repository_not_trusted(why);
    }
    // A bundle that was there before agentd's write: the run ends on row 14 with the harness's sentence naming the object, whatever
    // the engine's last prompt said; the summary built below carries it.
    if let Some(object) = conflict {
        classified = classify::upload_conflict(object);
    }

    // Facts to record beside the label: the last entry of the run log when it is red. Read from the work tree the engine
    // left, without following a link; nothing the engine wrote reaches the summary unless it is a plain token or a cleaned command.
    let observations: Vec<harness_protocol::direct::DirectObservation> = match st.wt.clone() {
        Some(wt) => {
            // The records the run chose when it started (COMPAT-CAH-DATA); a run that stopped before the choice reads the tree as it is.
            let chosen = st.records_folder;
            tokio::task::spawn_blocking(move || match chosen {
                Some(folder) => observe::last_log_observation_for(
                    &wt.repo,
                    &records::Records::read_in(&wt.repo, folder),
                ),
                None => observe::last_log_observation(&wt.repo),
            })
            .await
            .ok()
            .flatten()
            .into_iter()
            .map(|mut o| {
                o.text = scrub_detail(&secrets.redact(&o.text));
                o.command = o.command.map(|c| scrub_detail(&secrets.redact(&c)));
                o
            })
            .filter(harness_protocol::direct::DirectObservation::is_valid)
            .collect()
        }
        None => Vec::new(),
    };

    // The package proxy's counters are read now; the proxy and the container runtime go on running until the report step below is over.
    let proxy = st.proxy.take();
    let packages = proxy
        .as_ref()
        .map_or_else(DirectPackages::default, |p| p.stats());
    let containers = st.containers.take();
    let snapshots = st
        .snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();

    // 5. summary.json
    let now = now_unix();
    let wall_s = received.elapsed().as_secs();
    let stop_reason = classify::stop_reason(&raw);
    let error_text = compose_error(&classified, &upload_error, &errors, &secrets);
    let inputs = SummaryInputs {
        run_id: run.to_string(),
        classified: classified.clone(),
        stop_reason: stop_reason.clone(),
        message: Some(raw.message.clone()),
        model: req.model.clone(),
        pinned: st.pinned.clone(),
        engine_name: st.engine_name.clone(),
        engine_version: st.engine_version.clone(),
        engine_binary_sha256: st.engine_binary_sha256.clone(),
        reasoning_effort: st.reasoning_effort.clone(),
        reasoning_source: st.reasoning_source.clone(),
        engine_identity: st.engine_identity.clone(),
        rules: st.rules.clone(),
        rules_preflight: st.rules_preflight.unwrap_or(DirectRulesPreflight {
            ran: false,
            ms: None,
        }),
        request_sha256: st.request_sha.clone(),
        prompt_bytes: st.prompt_bytes,
        nudge: if st.nudge_max.is_some() {
            DirectNudge {
                // Asked for and not used: the goal loop ran in its place.
                honored: !st.nudge_off_by_loop && nudge_note(st.nudge_max).honored,
                ..nudge_note(st.nudge_max)
            }
        } else {
            DirectNudge {
                requested: false,
                honored: false,
            }
        },
        usage: st.usage,
        tool_calls: st.tool_calls,
        timing: DirectTiming {
            started_at: format_rfc3339(started_unix),
            ended_at: format_rfc3339(now),
            wall_s,
            prompt_s: st.prompt_s,
            max_silent_gap_s: st.max_silent_gap_s,
        },
        git: git.clone(),
        snapshots,
        packages,
        error: error_text.clone(),
        attempts: st.attempts.clone(),
        switches: st.switches.clone(),
        final_model: st.final_model.clone(),
        failover: st.failover.clone(),
        outage_s: st.outage_s,
    };
    let mut summary = build_summary_with(
        inputs,
        SummaryExtras {
            interventions: st.interventions.clone(),
            fresh_sessions: st.fresh_sessions.clone(),
            input_notes: st.input_note.into_iter().collect(),
            observations,
        },
    );
    // The summary names the slice it belongs to (turn n of the run).
    summary.turn = req.turn;
    // What the review round did, or why it did not run: a fact the driver recorded, not a part of the classification.
    summary.review = st.review.clone();
    // The flow the run declared: a fact read from the commits, never a part of the classification.
    summary.flow = flow;
    // The modules add their members to the summary; it is checked and written after.
    hooks.summary(hook_env, &classified, &mut summary).await;
    summary::fit_summary(&mut summary);
    if let Err(e) = summary.validate() {
        app.log.line(&format!("direct turn summary invalid: {e}"));
    }
    let summary_file = st
        .agent_data
        .join("turns")
        .join(DIRECT_TURN.to_string())
        .join("summary.json");
    let wrote = summary_file
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::write(
                &summary_file,
                serde_json::to_vec_pretty(&summary).unwrap_or_default(),
            )
        });
    match wrote {
        Ok(()) => {
            let uri = s3::uri(&bucket, &s3::summary_key_t(run, req.turn));
            let put = match &creds {
                Ok(c) => upload_new(&s3_objects, &uri, c, &summary_file, pause).await,
                Err(e) => Err(e.to_string()),
            };
            match put {
                Ok((PutNew::Stored, pin)) => summary_pin = pin,
                // agentd's summary cannot be stored: the key holds another writer's object. The run is reclassified as a conflict
                // (turn.ended, turn-complete, `result.json`, the log), the conflict is recorded, and agentd writes its own summary over
                // the foreign one, reclassified the same way so that what run-session reads names the conflict. The versioned bucket
                // keeps the foreign object as an earlier version.
                Ok((PutNew::Exists, _)) => {
                    conflict = Some(SUMMARY_OBJECT);
                    classified = classify::upload_conflict(SUMMARY_OBJECT);
                    rec.record(
                        EventType::TurnProgress,
                        upload_conflict_payload(SUMMARY_OBJECT),
                    );
                    upload_error.get_or_insert_with(|| {
                        format!("upload: {}", upload_conflict_text(SUMMARY_OBJECT))
                    });
                    app.log.line(&format!(
                        "direct turn summary upload conflict: {}",
                        upload_conflict_text(SUMMARY_OBJECT)
                    ));
                    reclassify(
                        &mut summary,
                        &classified,
                        compose_error(&classified, &upload_error, &errors, &secrets),
                    );
                    let replaced = match (
                        &creds,
                        std::fs::write(
                            &summary_file,
                            serde_json::to_vec_pretty(&summary).unwrap_or_default(),
                        ),
                    ) {
                        (Ok(c), Ok(())) => {
                            upload_file_receipt(&s3_objects, &uri, c, &summary_file, pause).await
                        }
                        (Err(e), _) => Err(e.to_string()),
                        (_, Err(e)) => Err(format!("{}: {e}", summary_file.display())),
                    };
                    match replaced {
                        Ok(pin) => {
                            summary_pin = Some(pin);
                            app.log.line(
                                "direct turn summary replaced: agentd's own summary was written over the object that was there",
                            );
                        }
                        Err(e) => {
                            app.log.line(&format!(
                                "direct turn summary not replaced: {}",
                                scrub_detail(&e)
                            ));
                            let text = upload_conflict_text(SUMMARY_OBJECT);
                            upload_error =
                                Some(format!("upload: {text}; {}", not_replaced_text(&e)));
                        }
                    }
                }
                Err(e) => {
                    app.log.line(&format!(
                        "direct turn summary not uploaded: {}",
                        scrub_detail(&e)
                    ));
                    upload_error.get_or_insert_with(|| format!("upload: {}", scrub_detail(&e)));
                }
            }
        }
        Err(e) => {
            app.log
                .line(&format!("direct turn summary not written: {e}"));
            // The harness owns the end state of the run: a done it could not record is not one it claims. An end that was not a
            // done keeps its label and carries the failure in its error text.
            let why = scrub_detail(&e.to_string());
            upload_error.get_or_insert_with(|| format!("write: summary.json: {why}"));
            if classified.outcome == DirectOutcome::ReportedDone {
                classified = classify::summary_not_written(&why);
            }
        }
    }

    // The record of the report object agentd stores in the step below, if it stores one.
    let mut report_pin: Option<OutputPin> = None;

    // The modules' report step: the head is delivered (committed, bundled, uploaded) and `summary.json` is in the bucket, so the
    // pre-report label and the head are durable whatever happens next. The package proxy and the container runtime still run and the
    // flusher goes on with the heartbeat, so a module can work on the head for a while (the clean-checkout test report). Bounded by
    // `Limits::report`; ordinary report results change nothing, while a refused create-only report write replaces the final
    // classification after the module records its conflict event. After an earlier conflict the run already ends stopped_other and a
    // failed replacement may have left a foreign bundle or summary under the key, so the step is skipped.
    if conflict.is_some() {
        app.log
            .line("direct turn report step skipped: an object of the run already existed");
    } else if report_step_applies(classified.outcome)
        && let (Some(head), Some(uri)) = (git.head_sha.as_deref(), git.bundle_uri.as_deref())
    {
        let bundle = st
            .agent_data
            .join("turns")
            .join(DIRECT_TURN.to_string())
            .join("bundle");
        let time_left =
            Duration::from_secs(u64::from(req.timeout_s)).saturating_sub(received.elapsed());
        let reported = hooks
            .report(hook_env, &classified, head, &bundle, uri, time_left)
            .await;
        report_pin = reported.pin;
        if let Some(object) = reported.conflict {
            // The report module already recorded the one conflict event. Make the refused create-only write the hard final result,
            // without rewriting the summary that was made durable before the report step. The classification detail is the final
            // error's single source for this conflict; bundle and summary upload errors keep their existing composition.
            classified = classify::upload_conflict(object);
        }
    }
    // The proxy and the container runtime stop once the engine is gone, the tree is delivered and the report step is over; best effort,
    // bounded by their launchers.
    if let Some(p) = proxy {
        p.stop().await;
    }
    if let Some(c) = containers {
        c.stop().await;
    }

    // 6. turn.ended, then the final flush
    record_started(rec, req, &mut st, None);
    let word = outcome_word(classified.turn_outcome);
    let mut ended = turn_ended_payload(
        req.turn,
        word,
        stop_reason.as_deref(),
        git.head_sha.as_deref(),
    );
    ended["directOutcome"] = serde_json::json!(classified.outcome.as_str());
    let end_engine = Engine::new(&req.engine).unwrap_or(Engine::DEFAULT);
    let end_label = classified
        .outcome
        .label_for_engine(end_engine, classified.detail.as_deref());
    ended["label"] = serde_json::json!(end_label);
    rec.record(EventType::TurnEnded, ended);
    // An answer that reached the run after its last stop is told that the run is over, in the events of this final flush.
    if let Some(late) = st.late_answers.as_mut() {
        late.drain();
    }
    let _pending = flusher.stop().await;
    app.log.line(&format!(
        "direct turn ended: run={run} outcome={} label={:?}",
        classified.outcome.as_str(),
        end_label
    ));

    // 7. turn-complete (its error also names a summary upload that failed)
    let error_text = compose_error(&classified, &upload_error, &errors, &secrets);
    let resp = InvocationResponse {
        summary: summary_of(&raw.message),
        run_id: run.to_string(),
        agent_id: DIRECT_AGENT_ID.to_string(),
        turn: req.turn,
        outcome: classified.turn_outcome,
        stop_reason: stop_reason.clone(),
        last_commit_sha: git.head_sha.clone(),
        bundle_uri: git.bundle_uri.clone(),
        usage: st.usage,
        error: error_text.clone(),
        quarantined: st.quarantined.clone(),
        reverted: Vec::new(),
        refused: Vec::new(),
        system_prompt_sha256: None,
        rules_digest: st.digest.clone(),
        rules_manifest: st.manifest.clone(),
        fault_hint: classified.fault_hint.clone(),
        max_silent_gap_s: st.max_silent_gap_s,
    };
    let mut complete: TurnCompleteRequest = crate::turn::turn_complete_request(&resp);
    complete.finish_reason = stop_reason.unwrap_or_else(|| word.to_string());
    // The modules' last word, before turn-complete: a flush, fields they add to the request. Bounded; delivery never waits longer.
    hooks.finalize(hook_env, &classified, &mut complete).await;
    // A slice that is not the last and ended on its prompt deadline hands the run over; the service starts the next slice.
    if slice::handoff(req, classified.outcome) {
        complete.handoff = Some(true);
    }
    // The records of the objects agentd stored. A run whose Agent item requires them reads nothing else.
    let pins = OutputPins {
        bundle: bundle_pin,
        summary: summary_pin,
        test_report: report_pin,
    };
    if !pins.is_empty() {
        complete.output_pins = Some(pins);
    }
    for attempt in 0..=UPLOAD_RETRIES {
        match sink.turn_complete(&complete).await {
            Ok(()) => break,
            Err(e) if attempt == UPLOAD_RETRIES => {
                app.log.line(&format!(
                    "direct turn turn-complete failed: {}",
                    scrub_detail(&e)
                ));
            }
            Err(_) => tokio::time::sleep(pause * 2u32.pow(attempt)).await,
        }
    }

    // 8. result.json, by rename
    let path = turn_result_path(&app.cfg.data_root, run, DIRECT_AGENT_ID, req.turn);
    if let Err(e) = write_result(&path, &resp) {
        app.log
            .line(&format!("direct turn result not written: {e}"));
    }
    hooks.event(&Event::RunEnded);
    resp
}
