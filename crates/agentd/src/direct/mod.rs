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
pub mod delivery_word;
pub mod disk;
pub mod dox;
pub mod driver;
pub mod earlier_records;
pub mod engine_config;
pub mod engine_stop;
pub mod failover;
pub mod flow;
pub mod fresh;
pub mod git_settings;
pub mod goal;
pub mod hooks;
pub mod input;
pub mod local_fault;
pub mod marker;
pub mod messages;
pub mod observe;
pub mod placement;
pub mod preflight;
pub mod probe;
pub mod prune;
pub mod recorder;
pub mod records;
pub mod recover;
pub mod report;
pub mod request;
pub mod research;
pub mod retry;
pub mod review;
pub mod rules_place;
pub mod skills;
pub mod slice;
pub mod snapshot;
pub mod storage_watch;
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
use crate::direct::snapshot::{
    BundleForm, COPY_LATE_AFTER, COPY_SPACING_CAP, COPY_SPACING_MAX, Cadence, CopyClock, CopyTimer,
    GitBudget, GitSource, GiveUp, STOP_COPY_BUDGET, STOP_COPY_RETRY, SnapshotMade, Snapshotter,
    TREE_ANSWER_LIMIT, TREE_ASK_EVERY, TreeAlive, cut_unix, failed_look_counts, late_after,
    snapshot_json, store_limit,
};
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
use crate::events::{EventRecorder, HttpEventSink, TurnControls, spawn_flusher_ending};
use crate::logging::LogSink;
use crate::s3::{PutNew, S3Objects};
use crate::sandbox::{
    HelperCheck, Identity, IdentityPreflight, LandlockEnforcer, Mode, Policy, Preflight, Sandbox,
    route_of_this_process, word_of_user,
};
use crate::secrets::SecretSet;
use crate::server::{App, EndingWatch};
use crate::time::{format_rfc3339, now_unix};
use crate::turn::{reap_engine, summary_of, turn_ended_payload, write_result};
use crate::workspace::{self, GitState, Hidden, Worktree};
use creds_fn::CredsFn;
use harness_core::engine_profile::{
    fixtures_dir, hidden_paths_union, load_profile, load_profiles, resolve_launch,
};
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::bundle_header::{self, HEADER_BYTES_MAX};
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
    /// The block on how the run delivers that was put before the first prompt of this session, as written, for the summary; `None`
    /// when the engine was told nothing about the delivery, which includes a session that ended before its first prompt was written.
    delivery_note: Option<String>,
    base: Option<String>,
    hidden: Option<Hidden>,
    git_state: Option<GitState>,
    /// The watch of the session's storage, when the session has one. The end sequence tells it what its own write of the git
    /// settings met.
    storage: Option<storage_watch::StorageWatch>,
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
    /// The copies of the work of this session (the private repository and where its copies go); `None` before the checkout exists
    /// and when the private repository could not be made.
    copies: Option<Copies>,
    /// Why the private repository of the copies could not be made. The run was told when it happened, and the end of the turn
    /// names it, because such a session saves no copy at all.
    copies_error: Option<String>,
    /// The first bytes of the bundle the run started from, when this invocation downloaded that bundle (as its restore object, or
    /// as the base of a copy that needs it). The copies of the session leave out what its heads hold ([`Copies::open`]).
    base_header: Option<Vec<u8>>,
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
    // A quiet beat interval leaves a beat in the run's events, and `turn.started` says how often. The interval is the heartbeat's,
    // and never shorter than a minute.
    rec.beat_when_quiet(app.cfg.direct.heartbeat_every.max(MIN_BEAT_EVERY));
    // The flusher also gives the service notice when agentd is asked to end while the engine works.
    let flusher = spawn_flusher_ending(
        rec.clone(),
        sink.clone(),
        app.cfg.direct.events_every,
        app.cfg.direct.heartbeat_every,
        req.turn,
        app.log.clone(),
        controls.clone(),
        Some(app.ending.clone()),
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

/// The first `max` bytes of the file at `path`, or fewer when the file is shorter; nothing when it cannot be read. For the header
/// of a bundle that this invocation downloaded.
fn file_prefix(path: &Path, max: u64) -> Vec<u8> {
    use std::io::Read as _;
    let mut prefix = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(max).read_to_end(&mut prefix);
    }
    prefix
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
    // The number of the session (the turn of the invocation), and how often the session writes an event at least while it lives
    // (a beat for a quiet heartbeat interval).
    started["session"] = serde_json::json!(req.turn);
    if let Some(s) = rec.beat_s() {
        started["beatS"] = serde_json::json!(s);
    }
    rec.record_urgent(EventType::TurnStarted, started);
}

/// The base commit of the run's own work, settled once in step 4 before anything reads it, with the one commit the harness may make on
/// the restored head first. A first request gets the DOX starter. A follow-up gets the commit that takes the
/// earlier request's lifecycle records out of the work tree when the stored request asks for it
/// (`removeEarlierRecords: true`, which only the service writes); a follow-up without that member starts on the head it restored.
/// Returns the base and, for a follow-up, the commit the records step computes for the seed, whether or not this invocation made it.
async fn settle_base(
    app: &App,
    wt: &Worktree,
    start: &harness_protocol::api::v1::StartDirectRunRequest,
    folder: &str,
    agent_data: &Path,
    origin: &str,
    fresh: bool,
) -> Result<(String, Option<String>), String> {
    if let Some(from) = &start.continue_from {
        let outcome = earlier_records::settle(
            wt,
            &earlier_records::Request {
                head_sha: from.head_sha.as_deref(),
                remove: harness_protocol::direct::removes_earlier_records(
                    Some(from),
                    start.remove_earlier_records,
                ),
                folder,
                agent_data,
            },
            origin,
            fresh,
        )
        .await
        .map_err(|e| format!("worktree: {e}"))?;
        if let Some(note) = &outcome.settled.note {
            app.log
                .line(&format!("direct turn earlier records: {note}"));
        }
        return Ok((outcome.settled.base, outcome.commit));
    }
    let starter = dox::settle(
        wt,
        &dox::Request {
            skills: &start.skills,
            prompt: &start.prompt,
            continue_run: start.continue_from.is_some(),
            agent_data,
        },
        origin,
        fresh,
    )
    .await
    .map_err(|e| format!("worktree: {e}"))?;
    if let Some(note) = &starter.note {
        app.log.line(&format!("direct turn DOX starter: {note}"));
    }
    Ok((starter.base, None))
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
    // The answer of this download names the version of the object that was read, when the bucket keeps versions.
    let restored_version = s3_objects
        .get_to_file_with_version(&repo_uri, &creds, &bundle_file)
        .await
        .map_err(|e| format!("worktree: {e}"))?;
    // A copy of the work may hold what the run changed and need what the bundle the run started from holds; its header says
    // so. The base is then fetched too and the restore takes it first. A restore object that names no commit it needs is restored
    // as before, with no read of the base.
    let base_uri = s3::split_uri(&req.request.uri)
        .map(|(bucket, _)| s3::uri(bucket, &s3::repo_bundle_key(run)));
    let restores_base = base_uri.as_deref() == Some(repo_uri.as_str());
    let restore_header = file_prefix(&bundle_file, HEADER_BYTES_MAX);
    let restore_needs = bundle_header::needs(&restore_header)
        .map(|commits| !commits.is_empty())
        .unwrap_or(false);
    let base_file = agent_data.join("base.bundle");
    let mut base_for_restore: Option<&Path> = None;
    // A checkout that an earlier invocation of the run left on this machine is returned as it is (`prepare_from_bundle_state`):
    // nothing is restored, so the base is not downloaded for it, and a bucket that cannot give the base ends no setup that needs
    // no restore.
    let checkout_left = workspace::checkout_left(&cfg.workspace_root, run, DIRECT_AGENT_ID);
    if restore_needs {
        if restores_base {
            return Err(
                "worktree: the run's base bundle needs commits it does not hold".to_string(),
            );
        }
        if !checkout_left {
            let Some(base_uri) = base_uri.as_deref() else {
                return Err(
                    "worktree: the copy to restore needs the run's base bundle: the request names no bucket"
                        .to_string(),
                );
            };
            s3_objects
                .get_to_file(base_uri, &creds, &base_file)
                .await
                .map_err(|e| {
                    format!("worktree: the copy to restore needs the run's base bundle: {e}")
                })?;
            base_for_restore = Some(&base_file);
        }
    }
    // The header of the base, when this invocation holds the base as a file: what the copies of this session leave out is read from
    // it ([`Copies::open`]); a session that holds no such file asks the bucket for the first bytes of the object.
    st.base_header = if restores_base {
        Some(restore_header)
    } else if base_for_restore.is_some() {
        Some(file_prefix(&base_file, HEADER_BYTES_MAX))
    } else {
        None
    };
    let (wt, fresh) = workspace::prepare_from_bundle_state(
        &cfg.workspace_root,
        run,
        DIRECT_AGENT_ID,
        &bundle_file,
        base_for_restore,
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
    // The storage says which commit this session starts on, before any engine work and before the starter commit of the harness: the
    // commit the service named for a later session (the restore check above passed), the commit of the checkout for the first. A read of
    // this storage after a stop takes it as the proof that the session restored that commit. The number is the copy's own when the commit
    // is a copy agentd made, else 0. A marker that cannot be written is logged and the session goes on.
    let started_on = restored
        .as_ref()
        .map_or_else(|| origin.clone(), |r| r.restore_head.clone());
    let started_count = workspace::git_with_env(&wt.repo, &["log", "-1", "--format=%s"], &[])
        .await
        .ok()
        .and_then(|subject| copy_number(&subject))
        .unwrap_or(0);
    // The session's two disks: measured at each prompt boundary and, fresh, after a -32603. Both settings at 0 switch
    // the measurement off.
    let disk_check = (cfg.direct.disk_low_mb > 0 || cfg.direct.workspace_limit_mb > 0).then(|| {
        disk::DiskCheck::new(
            // The test write goes to the run's own folder, the checkout's parent on the managed storage and the
            // engine's state folder, besides the two roots.
            Arc::new(
                disk::HostDisk::new(cfg.data_root.clone(), cfg.workspace_root.clone())
                    .with_write_dirs([agent_data.clone(), wt.root.clone(), data_dir.clone()]),
            ),
            disk::Limits {
                root_low_mb: cfg.direct.disk_low_mb,
                workspace_limit_mb: cfg.direct.workspace_limit_mb,
            },
        )
    });
    // The watch of the session's storage is built where the disk check is, so a deployment that switches both checks off has
    // neither. Its test write goes to the run's own folder on the storage, which agentd makes and the engine cannot write. It says
    // when the storage refuses a write while the files leave room; it stops nothing and tells the engine nothing. It is built before
    // the session's first marker is written, because every write of the marker goes through it: the marker is a write the worker
    // makes on the session's storage anyway, and its result counts like a test write's.
    let storage = disk_check.as_ref().map(|check| {
        storage_watch::StorageWatch::new(storage_watch::WatchSetup {
            // A test gives its own writes; the product never does.
            writes: match app.storage_writes.get() {
                Some(own) => own.clone(),
                None => Arc::new(storage_watch::HostWrites),
            },
            probe: check.probe.clone(),
            rec: rec.clone(),
            log: app.log.clone(),
            run_id: run.to_string(),
            session: req.turn,
            run_dir: cfg.workspace_root.join(run),
            limits: check.limits,
            test_refusal: cfg.direct.storage_test_refusal,
        })
    });
    st.storage = storage.clone();
    let start_marker = harness_protocol::direct::CopyMarker {
        schema_version: marker::SCHEMA_VERSION,
        session: req.turn,
        count: started_count,
        head_sha: started_on,
    };
    // A later session that restored a saved copy of an earlier session, or the final bundle of one, hands the watch the
    // copy it started from with the marker of its start, so that a notice can name it when this session had stored no copy of
    // its own before its first refusal. Without a watch nothing is read.
    let start_marked = match (&storage, &restored) {
        (Some(_), Some(r)) => started_from(
            app,
            &s3_objects,
            &creds,
            run,
            r,
            &repo_uri,
            restored_version,
        )
        .await
        .map_or(
            storage_watch::Marked::Start,
            storage_watch::Marked::StartFrom,
        ),
        _ => storage_watch::Marked::Start,
    };
    if let Err(e) = write_marker(
        storage.as_ref(),
        &cfg.workspace_root,
        run,
        &start_marker,
        start_marked,
    )
    .await
    {
        app.log
            .line(&format!("direct turn start marker: {}", scrub_detail(&e)));
    }
    // The one commit the harness may make before the base is read ([`settle_base`]): the DOX starter of a first request, or the
    // commit that takes the earlier request's lifecycle records out of the work tree of a follow-up. Either is made before the base is read, so
    // it is not the run's own change. A later slice finds the commit in the history it restored and measures from it. Best effort: a refusal
    // leaves the tree as it was and the run goes on. The head the checkout had before is kept for the `continue_from` tripwire, which accepts
    // the seed and exactly the commit the harness computes for it.
    let found_head = workspace::head(&wt)
        .await
        .map_err(|e| format!("worktree: {e}"))?;
    let (base, earlier_records_commit) =
        settle_base(app, &wt, &start, folder, &agent_data, &origin, fresh).await?;
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
        command_limit_ms: crate::engine::direct::run_limit(&req.engine, &user_settings)
            .map(crate::engine::user_settings::CommandSeconds::millis),
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
    let mut env = crate::engine::launch::base_env(crate::engine::launch::BaseEnv {
        home: &home,
        tmp: &tmp,
        // Git is told that the run's repository and temp folder are safe to work in.
        repo: &wt.repo,
        engine: &req.engine,
        profile_env: &launch.env,
        built_in: built_in.env,
        settings: &user_settings,
        aws: aws_env(&ectx, &aws_config),
        env_remove: &launch.env_remove,
    });
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
            // What the repository step found and what it computes for the seed of a follow-up, for the `continue_from` tripwire.
            ctx.found_head = Some(found_head.clone());
            ctx.earlier_records_commit
                .clone_from(&earlier_records_commit);
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

        // The private repository of the copies is made for every session, before the engine starts: it is seeded from the base,
        // which agentd can read only while `.git` is the checkout's own, and an engine may move or remove `.git` at once.
        match Copies::open(
            app,
            req,
            &agent_data,
            &wt,
            &base,
            st.base_header.take(),
            st.snapshots.clone(),
            rec,
            storage.clone(),
        )
        .await
        {
            Ok(copies) => st.copies = Some(copies),
            Err(e) => {
                copies_off(app, &st.snapshots, rec, &e);
                st.copies_error = Some(e);
            }
        }

        // The bound of one wait this launch carries, as the engine will read it (a module may have lowered it).
        let guard_wait = crate::engine::direct::guard_wait_bound(&req.engine, &env);
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
            engine: req.engine.clone(),
            log: app.log.clone(),
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
        // The copies of the work. The private repository is made before the engine starts (above), the copy of the end is made in
        // `finish`, and the task below looks at the tree on the timer and when a tool call ended.
        let tree_wake = Arc::new(tokio::sync::Notify::new());
        // The session's two disks and the watch of its storage were built before the session's first marker, above.
        // The task that makes the watch's test writes starts here and ends after the stage loop.
        let storage_task = storage.as_ref().map(storage_watch::StorageWatch::start);
        let mut collector = DirectCollector::new(rec.clone())
            .with_engine(&req.engine)
            .with_command_limit(
                crate::engine::direct::run_limit(&req.engine, &user_settings)
                    .map(crate::engine::user_settings::CommandSeconds::millis),
            )
            .with_wait_bound(guard_wait)
            .with_secret_set(st.secrets.clone())
            .with_hooks(hooks.clone())
            .with_tree_wake(tree_wake.clone());
        if let Some(watch) = &storage {
            collector = collector.with_storage_watch(watch.clone());
        }
        let snapshots = st
            .copies
            .as_ref()
            .and_then(|copies| start_snapshots(app, req, copies.clone(), tree_wake));
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
        // What the engine is told about the delivery: the delivery of the stored request's target, unless the user switched the
        // block off (`deliveryNote: false`) or the invocation carries a member that differs from that target.
        let delivery = delivery_word::for_run(req.delivery.as_ref(), &start);
        if req.delivery.is_some() && delivery.is_none() && start.delivery_note != Some(false) {
            app.log.line(
                "direct turn delivery: the invocation's member differs from the stored request's target; the engine is told nothing about the delivery",
            );
        }
        // The block as the engine reads it, for the summary: the run's records say what the engine was told. It becomes a member of the
        // summary only once the first prompt was written (after the stage loop, below).
        let delivery_note = delivery_word::block(delivery.as_ref());
        let plan = PromptPlan {
            cwd: top,
            engine: req.engine.clone(),
            model: req.model.id.clone(),
            // The user's text as typed; a run in the earlier records folder gets one line before it (COMPAT-CAH-DATA). The
            // harness's word on how the run delivers comes first, in every session of the run; a run that switched the block off
            // gets nothing before its text.
            prompt: delivery_word::with_block(
                delivery.as_ref(),
                &match &st.records_line {
                    Some(line) => format!("{line}\n\n{first}"),
                    None => first.to_string(),
                },
            ),
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
                let retry = match &disk_check {
                    Some(check) => retry.with_disk(check.clone()),
                    None => retry,
                };
                match &storage {
                    Some(watch) => retry.with_storage(watch.clone()),
                    None => retry,
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
            delivery,
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
        // A test write that the last tool result asked for is given the time one write may take; then the task ends.
        if let (Some(watch), Some(task)) = (&storage, storage_task) {
            let _ = tokio::time::timeout(TREE_ANSWER_LIMIT, watch.idle()).await;
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
        // The engine's runs that no answered prompt reported (a prompt that ended in an error, the goal plugin's own rounds).
        let outside = hooks.usage_outside_prompts();
        if let Some(u) = outside {
            app.log.line(&format!(
                "direct usage outside the answered prompts: input {} output {} cache read {} cache write {}",
                u.input_tokens, u.output_tokens, u.cache_read_input_tokens, u.cache_write_input_tokens
            ));
        }
        st.usage = match (end.usage, outside) {
            (Some(a), Some(b)) => Some(summary::add_usage(a, b)),
            (a, b) => a.or(b),
        };
        st.tool_calls = collector.tool_calls();
        st.nudges_sent = end.nudges_sent;
        st.max_silent_gap_s = end.max_silent_gap_s;
        st.prompt_s = end.prompt_started.map(|t| t.elapsed().as_secs());
        // The summary records the block only when the first prompt of the session was written. An engine that failed in the stages
        // before it (`initialize`, `session/new`, the model pin) read nothing, and the records of the run must not say it was told.
        st.delivery_note = delivery_note.filter(|_| end.prompt_started.is_some());
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
    /// The engine's profile name: which sign of the launch to look for ([`Respawn::confirm`]).
    engine: String,
    log: Arc<dyn LogSink>,
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

    fn confirm(&mut self) -> BoxFuture<'_, Option<String>> {
        use crate::engine::config::{CONFIRM_WAIT, Confirm, confirm_launch, limit_not_confirmed};
        Box::pin(async move {
            match confirm_launch(&self.engine, &self.spec.env, &self.capture, CONFIRM_WAIT).await {
                Confirm::NotAsked => None,
                Confirm::Confirmed => {
                    let line = crate::engine::config::ready_line(&self.engine, &self.spec.env);
                    self.log.line(&format!(
                        "direct turn command limit confirmed by the engine: {} ms",
                        line.as_deref()
                            .and_then(|l| l.rsplit(' ').next())
                            .unwrap_or("?")
                    ));
                    None
                }
                Confirm::Missing { headline } => {
                    self.log.line(&format!(
                        "direct turn command limit not confirmed by the engine: {}",
                        headline.as_deref().unwrap_or("its log holds no line")
                    ));
                    limit_not_confirmed(&self.engine, &self.spec.env)
                }
            }
        })
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

/// The tail of the engine's error output as the stop digest carries it: the last `max` bytes of the log ([`tail_of_file`]) without the
/// lines the harness's own plugin writes, which are not the engine's words. `None` when nothing else is left.
pub fn engine_error_tail(path: &Path, max: usize) -> Option<String> {
    let tail = tail_of_file(path, max)?;
    let rest = crate::engine::without_lines_starting(
        tail.as_bytes(),
        crate::engine::config::HARNESS_LINE_PREFIX,
    );
    let text = String::from_utf8_lossy(&rest).trim().to_string();
    (!text.is_empty()).then_some(text)
}

impl input::DigestSource for RunSources {
    fn gather(&self) -> BoxFuture<'_, input::DigestSources> {
        Box::pin(async move {
            let mut src = input::DigestSources {
                task: self.task.clone(),
                secrets: self.secrets.snapshot(),
                stderr_tail: engine_error_tail(&self.stderr_log, 3 * 1024),
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
    delivery: Option<harness_protocol::direct::DirectDelivery>,
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
        delivery,
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

/// The copies of the work that a session keeps in the bucket: agentd's private repository under one lock, where a copy goes, and what
/// the run is told. The copy task ([`start_snapshots`]) and the copy of the end ([`Copies::save_final`]) use the same handle, so they never
/// run at once and they count in one place.
#[derive(Clone)]
struct Copies {
    snapshotter: Arc<tokio::sync::Mutex<Snapshotter>>,
    creds: Arc<crate::creds::CredStore>,
    log: Arc<dyn crate::logging::LogSink>,
    region: String,
    endpoint: Option<String>,
    bucket: String,
    run: String,
    /// Slice n keeps its snapshots under its own turn, so a later slice never overwrites the one an earlier slice's restore names.
    turn: u32,
    pause: Duration,
    stats: Arc<Mutex<DirectSnapshots>>,
    rec: Arc<EventRecorder>,
    /// A copy failed and none has been stored since: the run was told once.
    failing: Arc<std::sync::atomic::AtomicBool>,
    /// A look was late and no copy has been stored since: the run was told once. It has a flag of its own, so that the look that is
    /// then given up still says that the copy could not be saved.
    late_said: Arc<std::sync::atomic::AtomicBool>,
    /// Whether this agentd ends once the copy that a request to end asked for is saved (`server::ends_after_the_copy`). The first
    /// process of its machine runs on: a request to end cuts none of its looks short, and a copy of a stop that failed is tried again.
    ends_after_copy: bool,
    /// The longest the copy of the end may take (`final_copy_budget`).
    final_budget: Duration,
    /// The number of the newest copy the run saved before this session; the session's own copies go on counting from it.
    before: u32,
    /// The limits of a look of the copy task.
    limits: CopyLimits,
    /// What the look in progress is doing ([`STEP_TREE`], [`STEP_STORE`]), for the words of a look that is late.
    step: Arc<std::sync::atomic::AtomicU8>,
    /// Asks the file system of the work tree whether it answers, before a look and while its git works.
    tree: TreeAlive,
    /// The root of the session's storage, where the marker of the copies is written (`marker::write`).
    marker_root: PathBuf,
    /// The marker could not be written after a copy and that was logged; it is logged again only after a marker was written.
    marker_failing: Arc<std::sync::atomic::AtomicBool>,
    /// The watch of the session's storage, when the session has one: the marker of each stored copy is written through it,
    /// with the copy and the version the bucket gave its bundle.
    storage: Option<storage_watch::StorageWatch>,
    /// The log line of a look whose keep has not ended; it is written with what came of the keep
    /// ([`Copies::keep_between_looks`]).
    keep_line: Arc<Mutex<Option<KeepLine>>>,
}

/// Writes `marker` as the marker of `run`: through the watch of the session's storage when the session has one
/// (`storage_watch::StorageWatch::marker_write_for`, which reads the result as it reads a test write's), else as before the watch
/// (`marker::write`). The error has the words `marker::write` gives it, for the caller's log line.
async fn write_marker(
    storage: Option<&storage_watch::StorageWatch>,
    workspace_root: &Path,
    run: &str,
    marker: &harness_protocol::direct::CopyMarker,
    marked: storage_watch::Marked,
) -> Result<(), String> {
    let Some(watch) = storage else {
        return marker::write(workspace_root, run, marker).await;
    };
    let bytes = serde_json::to_vec(marker).map_err(|e| format!("marker: {e}"))?;
    watch.marker_write_for(bytes, marked).await.map_err(|e| {
        if e.errno.is_none() && e.text.starts_with("the storage did not answer") {
            format!("marker: {}", e.text)
        } else {
            format!("marker: {e}")
        }
    })
}

/// The limits of one look of the copy task. No look is waited for without them: a session that was stopped from outside lived on
/// for a minute and stored no copy, because one look never returned.
#[derive(Clone, Copy, Debug)]
struct CopyLimits {
    /// A look that has run this long without storing its copy is said to the run (`snapshot::COPY_LATE_AFTER`, `snapshot::late_after`).
    late: Duration,
    /// While the git of a look works, the file system of the work tree is asked this often whether it still answers
    /// (`snapshot::TREE_ASK_EVERY`; a test that sets the late limit sets this with it).
    ask_every: Duration,
    /// What a test set for the git work of one look; `None` is `snapshot::git_limit`, which follows the last stored copy
    /// (`snapshot::GitBudget`). The git work of a look, and its file calls on the work tree, are given up after it whatever the file
    /// system answers.
    git: Option<Duration>,
    /// What a test set for storing one copy; `None` is `snapshot::store_limit` of the bundle's size.
    store: Option<Duration>,
    /// What the copy has that is saved when agentd is asked to end (`snapshot::STOP_COPY_BUDGET`), in a process that ends after it.
    stop: Duration,
}

/// The copy task is between two looks.
const STEP_IDLE: u8 = 0;
/// The look in progress reads the work tree and makes its copy with git.
const STEP_TREE: u8 = 1;
/// The look in progress stores its copy.
const STEP_STORE: u8 = 2;

/// The times one look ends at.
#[derive(Clone, Debug)]
struct LookBound {
    /// The git work of the look and its file calls on the work tree.
    git: GiveUp,
    /// The whole of the look, once a process that ends after the copy was asked to end.
    stop: GiveUp,
    /// When the git work of the look has run for its own limit.
    git_at: Instant,
    /// agentd was asked to end before this look ended: what the run is told when it fails is said for the request ([`Copies::stop_done`]).
    asked: Arc<std::sync::atomic::AtomicBool>,
    /// The look was given up because the file system of the work tree did not answer.
    silent: Arc<std::sync::atomic::AtomicBool>,
    /// The git work of the look ended within its limit; what follows is the store of the copy.
    git_ended: Arc<std::sync::atomic::AtomicBool>,
    /// When the git work of the look began: after the file system of the tree answered and the copy before it had ended. Unset for a look
    /// that never got that far.
    git_started_at: Arc<std::sync::OnceLock<std::time::Instant>>,
    /// When that git work ended.
    git_ended_at: Arc<std::sync::OnceLock<std::time::Instant>>,
    /// What started the look, for the worker's log line of the look.
    by: LookBy,
    /// What the look did so far, for that line ([`LookNote`]).
    note: Arc<Mutex<LookNote>>,
}

impl LookBound {
    /// A look that starts now, whose git work has `git`.
    fn new(git: Duration) -> Self {
        let git_at = Instant::now() + git;
        Self {
            git: GiveUp::at(Some(git_at)),
            stop: GiveUp::at(None),
            git_at,
            asked: Arc::default(),
            silent: Arc::default(),
            git_ended: Arc::default(),
            git_started_at: Arc::default(),
            git_ended_at: Arc::default(),
            by: LookBy::Timer,
            note: Arc::default(),
        }
    }

    /// Changes what the worker's log line of this look will say.
    fn note(&self, change: impl FnOnce(&mut LookNote)) {
        change(&mut self.note.lock().unwrap_or_else(|p| p.into_inner()));
    }

    /// The worker's log line of this look up to its store ([`save_line`]), with what started the look and what it found of the
    /// engine's head.
    fn line(&self, stored: bool) -> KeepLine {
        let note = self.note.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let by = match self.by {
            _ if note.for_a_stop => "stop",
            LookBy::Timer => "timer",
            LookBy::Tool => "tool",
            LookBy::Retry => "retry",
        };
        save_line(&note, by, note.steps.engine.word(), stored)
    }

    /// The git work of this look starts now.
    fn git_starts(&self) {
        let _ = self.git_started_at.set(std::time::Instant::now());
    }

    /// The git work of this look has ended and made a copy: the look goes on to store it.
    fn git_has_ended(&self) {
        let _ = self.git_ended_at.set(std::time::Instant::now());
        self.git_ended
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// What the spacing counts of this look, which started at `started`, asked when the look has failed (`snapshot::failed_look_counts`):
    /// its git work, to its end or to now when the git was given up, and nothing for a look that never started git (the tree did not
    /// answer, or the copy before it had not ended).
    fn counted(&self, started: std::time::Instant) -> (std::time::Instant, std::time::Instant) {
        failed_look_counts(
            started,
            self.git_started_at.get().copied(),
            self.git_ended_at.get().copied(),
            std::time::Instant::now(),
        )
    }

    /// agentd was asked to end. With `end_by` (a process that ends after the copy) nothing of this look runs after that time; without
    /// (the first process of its machine, which runs on) the look keeps its own limits.
    fn asked_to_end(&self, end_by: Option<Instant>) {
        self.asked.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(at) = end_by {
            self.git.no_later_than(at);
            self.stop.no_later_than(at);
        }
    }

    /// Whether agentd was asked to end before this look ended.
    fn was_asked_to_end(&self) -> bool {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The file system of the work tree did not answer: the git work of this look ends now.
    fn tree_is_silent(&self) {
        self.silent.store(true, std::sync::atomic::Ordering::SeqCst);
        self.git.no_later_than(Instant::now());
    }

    /// Whether the git work of this look, which failed, ran for its own limit: no silent tree ended it, and it had not ended when the
    /// limit passed (`snapshot::git_limit_gave_up`). A look whose copy could not be stored after its git had ended did not.
    fn git_limit_reached(&self) -> bool {
        crate::direct::snapshot::git_limit_gave_up(
            self.silent.load(std::sync::atomic::Ordering::SeqCst),
            self.git_ended.load(std::sync::atomic::Ordering::SeqCst),
            Instant::now() >= self.git_at,
        )
    }
}

/// The worker's save line of a look of the copy task, and of a copy of the end, up to its store: `stored`, or `failed`
/// with the step that failed as `why`, what started the save as `by`, what it found of the engine's head as `engine`, the wall time up
/// to now and the times of its steps in milliseconds. What came of the keep, the bytes of the bundle and the bytes of the kept pack
/// follow it ([`KeepLine::text`]).
fn save_line(note: &LookNote, by: &str, engine: &str, stored: bool) -> KeepLine {
    let outcome = if stored {
        "stored".to_string()
    } else {
        format!("failed why={}", note.step)
    };
    KeepLine {
        head: format!(
            "direct turn copy look n={} {outcome} by={by} engine={engine} ms total={} tidy={} tree={} head={} commit={} bundle={} \
             hash={} store={}",
            note.n,
            note.began.map_or(0, |began| began.elapsed().as_millis()),
            note.steps.tidy.as_millis(),
            note.steps.tree.as_millis(),
            note.steps.head.as_millis(),
            note.steps.commit.as_millis(),
            note.steps.bundle.as_millis(),
            note.hash.as_millis(),
            note.store.as_millis(),
        ),
        bytes: note.bytes,
        kept: note.kept,
    }
}

/// What started a look of the copy task, for the worker's log line of the look. A look that a request to end asked for is
/// told by the request itself ([`LookBound::was_asked_to_end`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LookBy {
    /// The timer of the copies.
    Timer,
    /// A tool call that ended.
    Tool,
    /// The timer while the copy of a stop that could not be saved is tried again.
    Retry,
}

/// What the worker's log line of a look says, gathered while the look runs ([`LookBound::line`]).
#[derive(Clone, Debug, Default)]
struct LookNote {
    /// When the look began.
    began: Option<std::time::Instant>,
    /// agentd had been asked to end when the look began: the look is the one of a stop.
    for_a_stop: bool,
    /// What the look was doing last: `tree` (the question to the file system), `lock` (the wait for the private repository),
    /// `git`, `hash` or `store`. A look that failed names it as why.
    step: &'static str,
    /// The number of the copy the look made; 0 before it has made one.
    n: u32,
    /// The times of the steps of its git work, and what it found of the engine's head.
    steps: crate::direct::snapshot::LookSteps,
    /// The hash of the bundle.
    hash: Duration,
    /// The store of the copy.
    store: Duration,
    /// The bytes of the bundle; 0 before the look has made one.
    bytes: u64,
    /// The bytes of the kept pack when the git work of the look had ended.
    kept: u64,
    /// When the store of a copy of the end began, for the line of one that was given up in its store; looks leave it unset.
    store_began: Option<std::time::Instant>,
}

/// The worker's log line of one look that stored a copy or failed, without what came of the keep that may follow the look:
/// one line per such look, and none for a look at an unchanged tree.
#[derive(Clone, Debug)]
struct KeepLine {
    /// The line up to the time of the store ([`LookBound::line`]).
    head: String,
    /// The bytes of the bundle.
    bytes: u64,
    /// The bytes of the kept pack when the look ended.
    kept: u64,
}

impl KeepLine {
    /// The whole line. `keep` is `0` when no keep followed the look, the milliseconds the keep took when it ran to its end, `ended`
    /// when what started the next look cut it, and `missed` when it could not be made; `kept` is the bytes of the kept pack then.
    fn text(&self, keep: &str, kept: u64) -> String {
        format!("{} keep={keep} bytes={} kept={kept}", self.head, self.bytes)
    }
}

/// The line of a look whose keep is in flight. It is written when the keep ends ([`Self::done`]); when the keep is dropped
/// first (the next look is due, a tool call ended, agentd was asked to end, the copy task is over), it is written here, and says
/// that the keep was ended, or that none started.
struct KeepEnding {
    log: Arc<dyn crate::logging::LogSink>,
    line: Option<KeepLine>,
    /// The keep has the private repository and may have started its git.
    started: bool,
}

impl KeepEnding {
    fn done(&mut self, keep: &str, kept: u64) {
        if let Some(line) = self.line.take() {
            self.log.line(&line.text(keep, kept));
        }
    }
}

impl Drop for KeepEnding {
    fn drop(&mut self) {
        if let Some(line) = self.line.take() {
            self.log
                .line(&line.text(if self.started { "ended" } else { "0" }, line.kept));
        }
    }
}

/// A request to end that the copy task has taken up.
#[derive(Clone, Copy, Debug)]
struct Stop {
    /// The number of the request (`server::Ending`).
    n: u64,
    /// When the time its copy has in a process that ends after the copy is over.
    at: Instant,
}

/// What a look at the tree did.
enum Looked {
    /// The tree had not changed.
    Same,
    /// A copy was stored: the size of its bundle, and when git had made it (the uploads came after).
    Stored {
        bytes: u64,
        git_ended: std::time::Instant,
    },
    /// A copy could not be made or stored.
    Failed,
}

/// The sentence the run is told when a copy cannot be saved, once per streak of failures.
pub const COPY_NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";

/// The sentence the run is told when a look has run for the late limit and still reads the work tree, once until a copy is stored
/// again. The look goes on; when it is given up after all, the run is told that too ([`COPY_NOT_SAVED`]).
pub const COPY_LATE_TREE: &str =
    "A copy of the work is taking long: reading the work tree has not ended. ham keeps trying.";

/// The sentence the run is told when a look has run for the late limit and still stores its copy.
pub const COPY_LATE_STORE: &str =
    "A copy of the work is taking long: storing it has not ended. ham keeps trying.";

/// The sentence the run is told when the copy of the end cannot be saved and no earlier failure was said: nothing tries again
/// after the end, so it promises nothing.
pub const END_COPY_NOT_SAVED: &str = "The last copy of the work could not be saved.";

/// The sentence the run is told when the copy that a request to end asked for cannot be saved, once for each request and whatever
/// was said before. The session is ending, so it promises no further try: a process that ends after the copy makes none, and the first
/// process of its machine tries again only for as long as the machine lives.
pub const STOP_COPY_NOT_SAVED: &str =
    "The session was asked to end, and a copy of the work could not be saved.";

/// The sentence the run is told when a session cannot make the private repository of its copies: it saves none, so nothing is
/// promised but the delivery.
pub const COPIES_OFF: &str = "Copies of the work cannot be saved in this session. The work is kept only if the run delivers it.";

/// The private repository of the copies could not be made: counted as a failed copy, logged, and the run is told (`COPIES_OFF`).
fn copies_off(app: &App, stats: &Mutex<DirectSnapshots>, rec: &EventRecorder, e: &str) {
    stats.lock().unwrap_or_else(|p| p.into_inner()).failures += 1;
    app.log
        .line(&format!("direct turn snapshots off: {}", scrub_detail(e)));
    rec.record(
        EventType::TurnProgress,
        serde_json::json!({
            "turn": DIRECT_TURN,
            "kind": "notify",
            "reason": "copy_not_saved",
            "outageMinutes": null,
            "detail": COPIES_OFF,
        }),
    );
}

/// The least time between two beats of a quiet session ([`EventRecorder::beat_when_quiet`]). The product's heartbeat interval is
/// this minute; a shorter heartbeat interval (a test's) does not fill the run's events with beats.
pub const MIN_BEAT_EVERY: Duration = Duration::from_secs(60);

/// How often the record of an earlier session's newest copy is asked for before the count goes on without it.
const COPIES_BEFORE_TRIES: u32 = 3;

/// The number `n` of a commit whose subject is `harness: snapshot <n>`: a copy agentd made while an engine worked.
fn copy_number(subject: &str) -> Option<u32> {
    subject
        .trim()
        .strip_prefix("harness: snapshot ")?
        .parse()
        .ok()
}

/// The most bytes of the record of a copy that are read for the copy a session started from. A record is a few hundred.
const STARTED_FROM_RECORD_BYTES: u64 = 4096;

/// How long the start of a session waits for what names the copy it started from: the time the watch of the storage waits
/// for an answer (`snapshot::TREE_ANSWER_LIMIT`). A start from a saved copy reads that copy's record; a start from a final bundle
/// reads the record and then the first bytes of the saved copy, with one limit for both reads. These reads are the one place
/// where naming the copy can hold the start of a session.
const STARTED_FROM_RECORD_LIMIT: Duration = crate::direct::snapshot::TREE_ANSWER_LIMIT;

/// The copy a later session started from, as a notice of the storage watch names it (`storage_watch::Marked::StartFrom`).
/// The object the session restored (`repo_uri`) is one of two objects of an earlier session `k` of the run:
///
/// * its newest saved copy (`turns/<k>/snapshot.bundle`), as after a lost session. The version is the one the bucket answered
///   for the restore's own download (`version`), and the copy's own record (`turns/<k>/snapshot.json`) must name the commit the
///   session restored;
/// * its final bundle (`turns/<k>/bundle`), as after a session that ended in order. That session saved its copy of the end
///   before it wrote the bundle (root invariant 9), from the same commit. The record must name the commit the session
///   restored. Then the first bytes of the saved copy are read once: the version is the one the bucket answers for that read,
///   and the header in those bytes must name the same commit, which is what the download route requires of the version.
///
/// The record gives the copy's number and its time, which is from before the copy's tree was read; the commit's own
/// date is later. Anything else is `None`, and the log says why: a bucket that keeps no versions answers none, a session that
/// ended between the two uploads of a copy left the record of the copy before, and a session whose copy of the end was not
/// stored left the record of another commit beside its bundle. The files read back from a stopped session are no numbered copy
/// of the run: they are `None` without a line and without a read. The reads are waited for [`STARTED_FROM_RECORD_LIMIT`] at
/// the most, one limit for both; the session starts whatever they come to.
///
/// A copy that was found is said in one line of the log for each invocation of the session, in the form of the line of the
/// first reading of the tree: the copy's number, the session that saved it and the time the reads took. The reads are made at
/// the start of every later session, so the log of any such session shows that the worker's own reads worked and what they
/// cost the start. They are made at every start, so a later session that is invoked a second time writes the line again: two
/// such lines in the log of one session mean two invocations. A session that names no copy has the line that says why, and
/// not this one.
async fn started_from(
    app: &App,
    s3_objects: &S3Objects,
    creds: &harness_protocol::api::internal::ProcessCredentials,
    run: &str,
    restored: &slice::Restored,
    repo_uri: &str,
    version: Option<String>,
) -> Option<storage_watch::CopyRef> {
    const CANNOT: &str = "a notice of a refused write cannot name it";
    let (bucket, key) = s3::split_uri(repo_uri)?;
    // The newest saved copy of an earlier session (`false`), or the final bundle of one (`true`).
    let (session, final_bundle) = (1..restored.turn).rev().find_map(|k| {
        if key == s3::snapshot_bundle_key_t(run, k) {
            Some((k, false))
        } else if key == s3::bundle_key_t(run, k) {
            Some((k, true))
        } else {
            None
        }
    })?;
    let line = |text: String| app.log.line(&format!("direct turn storage: {text}"));
    if !final_bundle && version.is_none() {
        line(format!(
            "the bucket answered no version for the copy this session started from; {CANNOT}"
        ));
        return None;
    }
    // The copy the reads are about, in the log's words.
    let copy_words = if final_bundle {
        format!("the newest saved copy of session {session}")
    } else {
        "the copy this session started from".to_string()
    };
    let no_answer = |what: &str| {
        format!(
            "{what} got no answer within {} s; {CANNOT}",
            STARTED_FROM_RECORD_LIMIT.as_secs()
        )
    };
    let reads_began = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + STARTED_FROM_RECORD_LIMIT;
    let uri = s3::uri(bucket, &s3::snapshot_json_key_t(run, session));
    let read = tokio::time::timeout_at(
        deadline,
        s3_objects.get_prefix(&uri, creds, STARTED_FROM_RECORD_BYTES),
    )
    .await;
    let record = match read {
        Ok(Ok(bytes)) => serde_json::from_slice::<serde_json::Value>(&bytes).ok(),
        Err(_) => {
            line(no_answer(&format!("the record of {copy_words}")));
            return None;
        }
        Ok(Err(e)) => {
            line(format!(
                "the record of {copy_words} could not be read; {CANNOT}: {}",
                scrub_detail(&e)
            ));
            return None;
        }
    };
    let named = record.as_ref().and_then(|r| {
        let count = u32::try_from(r.get("count")?.as_u64()?).ok()?;
        let at = r.get("at")?.as_str()?;
        (r.get("headSha")?.as_str()? == restored.restore_head
            && count >= 1
            && harness_protocol::common::parse_utc_unix(at).is_some())
        .then(|| (count, at.to_string()))
    });
    let Some((count, at)) = named else {
        line(if final_bundle {
            format!(
                "the record of session {session} does not name the commit this session started on; a notice of a refused write names no copy"
            )
        } else {
            format!(
                "the record of session {session} does not name the copy this session started from; {CANNOT}"
            )
        });
        return None;
    };
    let version_id = if final_bundle {
        // The saved copy was not downloaded. Its first bytes are read for the version the bucket holds of it and for its header.
        let uri = s3::uri(bucket, &s3::snapshot_bundle_key_t(run, session));
        let read = tokio::time::timeout_at(
            deadline,
            s3_objects.get_prefix_with_version(&uri, creds, bundle_header::HEADER_BYTES),
        )
        .await;
        match read {
            Ok(Ok((bytes, Some(version_id)))) => {
                if bundle_header::bundle_head(&bytes).ok().as_deref()
                    != Some(restored.restore_head.as_str())
                {
                    line(format!(
                        "{copy_words} is not the commit this session started on; {CANNOT}"
                    ));
                    return None;
                }
                version_id
            }
            Ok(Ok((_, None))) => {
                line(format!(
                    "the bucket answered no version for {copy_words}; {CANNOT}"
                ));
                return None;
            }
            Err(_) => {
                line(no_answer(&copy_words));
                return None;
            }
            Ok(Err(e)) => {
                line(format!(
                    "{copy_words} could not be read; {CANNOT}: {}",
                    scrub_detail(&e)
                ));
                return None;
            }
        }
    } else {
        version?
    };
    line(format!(
        "this session started from copy {count} of session {session}; {} took {} ms",
        if final_bundle {
            "the two reads"
        } else {
            "the read"
        },
        reads_began.elapsed().as_millis()
    ));
    Some(storage_watch::CopyRef {
        session,
        count,
        at,
        head_sha: restored.restore_head.clone(),
        version_id,
    })
}

/// The number of the newest copy the run saved before the session of `req` (0 for a run that saved none). The copies of a run are
/// numbered within the run, so that a number names one copy: a session after the first reads the record of the newest earlier session
/// that left one (`turns/<k>/snapshot.json`, in the worker's own prefix) and goes on from its `count`. A session that ended between its two
/// uploads leaves the bundle of its newest copy without that copy's record; the bundle is what this session restored, so the larger of the
/// record's number and the restored commit's counts ([`copies_counted`]). A session that left no record answers 404, or 403 where the role may
/// not list the bucket, and the session before it is asked. A record that cannot be read after [`COPIES_BEFORE_TRIES`] tries leaves the
/// number of the restored commit, and without one the count starts again: the session goes on either way, and the client tells two copies
/// of one number apart by their commits.
async fn copies_before(
    app: &App,
    req: &DirectInvocationRequest,
    agent_data: &Path,
    repo: &Path,
    bucket: &str,
) -> u32 {
    let restored = workspace::git_with_env(repo, &["log", "-1", "--format=%s"], &[])
        .await
        .ok()
        .and_then(|subject| copy_number(&subject));
    let s3_objects = S3Objects::new(req.region.clone(), app.cfg.s3_endpoint.clone());
    let dest = agent_data.join("earlier-snapshot.json");
    let mut recorded: Option<u32> = None;
    'sessions: for k in (1..req.turn).rev() {
        let uri = s3::uri(bucket, &s3::snapshot_json_key_t(&req.run_id, k));
        for attempt in 1..=COPIES_BEFORE_TRIES {
            let got = match app.creds.current().await {
                Ok(c) => s3_objects.get_to_file(&uri, &c, &dest).await,
                Err(e) => Err(e.to_string()),
            };
            match got {
                Ok(()) => {
                    recorded = std::fs::read(&dest)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                        .and_then(|v| v.get("count")?.as_u64())
                        .and_then(|n| u32::try_from(n).ok());
                    if recorded.is_some() {
                        break 'sessions;
                    }
                    // A record that is not one: the session before it is asked.
                    continue 'sessions;
                }
                // That session left no record: the one before it is asked. The worker's role may list the bucket only under its own
                // prefixes, and S3 answers 403 for an absent key to a caller that may not list it, so a 403 is a missing record too.
                Err(e) if e.contains(" returned 404") || e.contains(" returned 403") => {
                    continue 'sessions;
                }
                Err(e) if attempt == COPIES_BEFORE_TRIES => {
                    app.log.line(&format!(
                        "direct turn copy count: the record of session {k} could not be read: {}",
                        scrub_detail(&e)
                    ));
                    break 'sessions;
                }
                Err(_) => tokio::time::sleep(app.cfg.direct.upload_retry_pause).await,
            }
        }
    }
    copies_counted(recorded, restored)
}

/// The number the copies of a session go on from: the larger of the number the newest record of an earlier session names
/// (`recorded`) and the number of the commit the session restored (`restored`, `None` for a commit that is no copy). A session can
/// leave bundles without their records (it ended between the two uploads of one copy, or the upload of an earlier copy failed and it
/// ended before the record of a later one), so the restored commit can be any number ahead of the record, and no number names two copies.
pub fn copies_counted(recorded: Option<u32>, restored: Option<u32>) -> u32 {
    recorded.unwrap_or(0).max(restored.unwrap_or(0))
}

/// How often the first bytes of the bundle the run started from are asked for before the copies of the session are whole.
const BASE_HEADER_TRIES: u32 = 3;

/// The commits the copies of this session leave out: the heads that the bundle the run started from names in its own header
/// (`runs/<run>/input/repo.bundle`), of which the private repository of the copies can read each. Nothing is taken from the checkout
/// and nothing from the invocation's `baseSha`: a checkout that an earlier invocation left stands at a commit of the engine, which the
/// base object does not hold, and a copy above such a commit could not be restored from the base.
///
/// `header` is the first bytes of that bundle when this invocation downloaded it. Without them the bucket is asked for the first
/// [`HEADER_BYTES_MAX`] bytes of the object, [`BASE_HEADER_TRIES`] times with the upload pause. The error says why there is no floor:
/// the header could not be read, it is no bundle header that ends inside those bytes, it names a commit the bundle itself needs, or
/// it names no commit this repository can read.
async fn floors_of_the_base(
    app: &App,
    req: &DirectInvocationRequest,
    bucket: &str,
    header: Option<Vec<u8>>,
    snapshotter: &Snapshotter,
) -> Result<Vec<String>, String> {
    let header = match header {
        Some(bytes) => bytes,
        None => {
            let uri = s3::uri(bucket, &s3::repo_bundle_key(&req.run_id));
            let s3_objects = S3Objects::new(req.region.clone(), app.cfg.s3_endpoint.clone());
            let mut got = Err("not asked".to_string());
            for attempt in 1..=BASE_HEADER_TRIES {
                got = match app.creds.current().await {
                    Ok(c) => s3_objects.get_prefix(&uri, &c, HEADER_BYTES_MAX).await,
                    Err(e) => Err(e.to_string()),
                };
                if got.is_ok() || attempt == BASE_HEADER_TRIES {
                    break;
                }
                tokio::time::sleep(app.cfg.direct.upload_retry_pause).await;
            }
            got.map_err(|e| format!("the header of the run's base bundle could not be read: {e}"))?
        }
    };
    let heads = bundle_header::heads(&header)
        .map_err(|e| format!("the header of the run's base bundle: {e}"))?;
    let mut floors = Vec::new();
    for head in heads {
        if snapshotter.has_commit(&head).await {
            floors.push(head);
        }
    }
    if floors.is_empty() {
        return Err(
            "the run's base bundle names no commit that the private repository of the copies can read"
                .to_string(),
        );
    }
    Ok(floors)
}

impl Copies {
    /// Makes the private repository of the session's copies for the work tree at `wt`; the error says why it cannot be made
    /// ([`copies_off`] tells the run).
    #[allow(clippy::too_many_arguments)]
    async fn open(
        app: &App,
        req: &DirectInvocationRequest,
        agent_data: &Path,
        wt: &Worktree,
        base: &str,
        base_header: Option<Vec<u8>>,
        stats: Arc<Mutex<DirectSnapshots>>,
        rec: &Arc<EventRecorder>,
        storage: Option<storage_watch::StorageWatch>,
    ) -> Result<Self, String> {
        let mut snapshotter = Snapshotter::new(agent_data, &wt.repo, base).await?;
        // The tree was read once while the repository was made, before the engine starts, so that the first look of the copy
        // task does not read every file in front of a tool call. A reading that was given up fails nothing: that look reads the files.
        // What git's check of a kept pack cost the start of the turn is said in the same line.
        let check = snapshotter
            .take_kept_check()
            .map(|(bytes, took)| {
                format!(
                    "; git's check of the kept pack of {bytes} bytes took {} ms",
                    took.as_millis()
                )
            })
            .unwrap_or_default();
        match snapshotter.take_prime_note() {
            Some(Ok(took)) => app.log.line(&format!(
                "direct turn copies: the first reading of the tree took {} ms{check}",
                took.as_millis()
            )),
            Some(Err(e)) => app.log.line(&format!(
                "direct turn copies: the first reading of the tree was given up: {}{check}",
                scrub_detail(&e)
            )),
            None => {}
        }
        // The pack an earlier worker kept is adopted only after git's own check of it has passed. One that did not pass
        // is gone, and this is the line that says so.
        if let Some(why) = snapshotter.take_kept_note() {
            app.log
                .line(&format!("direct turn copy keep: {}", scrub_detail(&why)));
        }
        let bucket = s3::split_uri(&req.request.uri)
            .map(|(b, _)| b.to_string())
            .unwrap_or_default();
        // A copy leaves out what the bundle the run started from holds, when the form of the copies says so and that bundle's
        // own header names commits this repository can read. With no such commit, for any reason, the copies of the session are whole.
        let form = app.cfg.direct.copy_form;
        snapshotter.set_form(form);
        snapshotter.set_keep_commits(app.cfg.direct.keep_commits);
        if form == BundleForm::AboveFloor {
            match floors_of_the_base(app, req, &bucket, base_header, &snapshotter).await {
                Ok(floors) => snapshotter.set_floors(floors),
                Err(why) => app.log.line(&format!(
                    "direct turn copies are whole in this session: {}",
                    scrub_detail(&why)
                )),
            }
        }
        // The copies of a run are numbered within the run, so a session after the first goes on from the newest earlier copy.
        let before = if req.turn > 1 {
            copies_before(app, req, agent_data, &wt.repo, &bucket).await
        } else {
            0
        };
        snapshotter.count_from(before);
        Ok(Self {
            snapshotter: Arc::new(tokio::sync::Mutex::new(snapshotter)),
            creds: app.creds.clone(),
            log: app.log.clone(),
            region: req.region.clone(),
            endpoint: app.cfg.s3_endpoint.clone(),
            bucket,
            run: req.run_id.clone(),
            turn: req.turn,
            pause: app.cfg.direct.upload_retry_pause,
            stats,
            rec: rec.clone(),
            failing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            late_said: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ends_after_copy: crate::server::ends_after_the_copy(
                std::process::id(),
                crate::first::behind_first(),
            ),
            final_budget: app.cfg.direct.final_copy_budget,
            before,
            limits: CopyLimits {
                late: app.cfg.direct.copy_late.unwrap_or(COPY_LATE_AFTER),
                ask_every: app.cfg.direct.copy_late.unwrap_or(TREE_ASK_EVERY),
                git: app.cfg.direct.copy_git_limit,
                store: app.cfg.direct.copy_store_limit,
                stop: app.cfg.direct.stop_copy_budget.unwrap_or(STOP_COPY_BUDGET),
            },
            step: Arc::new(std::sync::atomic::AtomicU8::new(STEP_IDLE)),
            tree: TreeAlive::new(&wt.repo),
            marker_root: app.cfg.workspace_root.clone(),
            marker_failing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            storage,
            keep_line: Arc::default(),
        })
    }

    /// Uploads `made` (the bundle, then the record, so a reader that sees the record finds the bundle), counts it and tells the run: one
    /// `turn.progress` event with a `savedCopy` member and no `kind`, which a client that does not know the member ignores.
    ///
    /// `request` is the whole-time limit of one request; `None` is the client's own. A look sets the limit of its store, and the first
    /// try of the copy of the end sets the copy's budget. A request also ends when nothing of it moved for `s3::NO_PROGRESS`.
    async fn store(&self, made: &SnapshotMade, request: Option<Duration>) -> Result<(), String> {
        let s3_objects = S3Objects::new(self.region.clone(), self.endpoint.clone());
        let s3_objects = match request {
            Some(limit) => s3_objects.with_request_timeout(limit),
            None => s3_objects,
        };
        let c = self.creds.current().await.map_err(|e| e.to_string())?;
        let bundle_uri = s3::uri(
            &self.bucket,
            &s3::snapshot_bundle_key_t(&self.run, self.turn),
        );
        // The bundle is hashed once. A look has hashed it as part of its own work (`snapshot::hash_bundle`); the copy of the
        // end comes without a hash, and it is made here.
        let digest = match &made.digest {
            Some(digest) => digest.clone(),
            None => crate::s3::digest_file(&made.bundle).await?,
        };
        // The version the bucket gave this bundle is in the answer of the upload that is read anyway. The watch of the storage
        // keeps it with the copy, so that a notice can name this copy after a newer one replaced it at the same key.
        let version_id = s3_objects
            .put_file_retrying_with_version(
                &bundle_uri,
                &c,
                &made.bundle,
                &digest,
                UPLOAD_RETRIES,
                self.pause,
            )
            .await?;
        // The time the copy names is from before its tree was read, not the time it was stored.
        let at = format_rfc3339(cut_unix(made.read_unix));
        // The bundle is in the bucket, so the storage now stands at this copy: the marker says so before the record is uploaded. A
        // bundle that was not stored never reaches this line, and a marker that cannot be written does not fail the copy.
        self.mark(made, &at, version_id).await;
        let json_path = made.bundle.with_file_name("snapshot.json");
        std::fs::write(&json_path, snapshot_json(made, &at)).map_err(|e| e.to_string())?;
        let json_uri = s3::uri(&self.bucket, &s3::snapshot_json_key_t(&self.run, self.turn));
        upload_file(&s3_objects, &json_uri, &c, &json_path, self.pause).await?;
        {
            let mut s = self.stats.lock().unwrap_or_else(|p| p.into_inner());
            // The summary counts the copies of this session; the record and the event carry the number within the run.
            s.count = made.count.saturating_sub(self.before);
            s.last_at = Some(at.clone());
        }
        self.failing
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.late_said
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let mut saved = serde_json::json!({
            "at": at,
            "count": made.count,
            "headSha": made.commit,
            "session": self.turn,
        });
        if made.final_copy {
            saved["final"] = serde_json::json!(true);
        }
        self.rec.record(
            EventType::TurnProgress,
            serde_json::json!({"turn": DIRECT_TURN, "savedCopy": saved}),
        );
        Ok(())
    }

    /// Writes the marker of the storage for the copy `made`, whose bundle is stored (`marker::write`). What it could not do is
    /// logged once until a marker is written again; the copy is not affected. The write goes through the watch of the
    /// session's storage when the session has one, with the copy as a notice would name it: its time `at` and the version
    /// `version_id` the bucket gave its bundle. A copy without a version cannot be named, and the log says so.
    async fn mark(&self, made: &SnapshotMade, at: &str, version_id: Option<String>) {
        let marker = harness_protocol::direct::CopyMarker {
            schema_version: marker::SCHEMA_VERSION,
            session: self.turn,
            count: made.count,
            head_sha: made.commit.clone(),
        };
        let marked = match version_id {
            Some(version_id) => storage_watch::Marked::Copy(storage_watch::CopyRef {
                session: self.turn,
                count: made.count,
                at: at.to_string(),
                head_sha: made.commit.clone(),
                version_id,
            }),
            None => {
                if self.storage.is_some() {
                    self.log.line(&format!(
                        "direct turn storage: the bucket gave copy {} no version; a notice of a refused write cannot name it",
                        made.count
                    ));
                }
                storage_watch::Marked::Unnamed
            }
        };
        match write_marker(
            self.storage.as_ref(),
            &self.marker_root,
            &self.run,
            &marker,
            marked,
        )
        .await
        {
            Ok(()) => self
                .marker_failing
                .store(false, std::sync::atomic::Ordering::SeqCst),
            Err(e) => {
                if !self
                    .marker_failing
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    self.log
                        .line(&format!("direct turn copy marker: {}", scrub_detail(&e)));
                }
            }
        }
    }

    /// What the clean-up of the private repository after a look could not do is logged; the copy itself is not affected.
    /// And why a copy holds the whole tree when it does because its tree could not be staged (`Snapshotter::take_whole_note`):
    /// the run is told that such a copy was stored, so the log is where its difference is said.
    fn log_tidy(&self, snapshotter: &mut Snapshotter) {
        if let Some(e) = snapshotter.take_tidy_error() {
            self.log.line(&format!(
                "direct turn copy repository clean-up failed: {}",
                scrub_detail(&e)
            ));
        }
        if let Some(why) = snapshotter.take_whole_note() {
            self.log.line(&format!(
                "direct turn copy of the whole tree: {}",
                scrub_detail(&why)
            ));
        }
    }

    /// A look of the copy task failed: it is counted and logged, and the run is told ([`COPY_NOT_SAVED`]) when it is the first failure
    /// since a copy was stored. When agentd was asked to end before the look ended, nothing is said here: the copy of that request says
    /// what came of it ([`Copies::stop_done`]).
    fn look_failed(&self, bound: &LookBound, e: &str) -> Looked {
        self.count_failed("snapshot", e);
        // The look's own line: which step failed, and what the steps before it took.
        let line = bound.line(false);
        self.log.line(&line.text("0", line.kept));
        if !bound.was_asked_to_end() {
            self.tell_failed(COPY_NOT_SAVED);
        }
        Looked::Failed
    }

    /// A copy failed: it is counted and logged. The run is not told here.
    fn count_failed(&self, what: &str, e: &str) {
        self.stats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .failures += 1;
        self.log
            .line(&format!("direct turn {what} failed: {}", scrub_detail(e)));
    }

    /// The run is told (`sentence`) that a copy could not be saved, when it is the first failure since a copy was stored.
    fn tell_failed(&self, sentence: &str) {
        if !self.failing.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.notify(sentence);
        }
    }

    /// One notice about the copies of the work (`reason` `copy_not_saved`).
    fn notify(&self, sentence: &str) {
        self.rec.record(
            EventType::TurnProgress,
            serde_json::json!({
                "turn": DIRECT_TURN,
                "kind": "notify",
                "reason": "copy_not_saved",
                "outageMinutes": null,
                "detail": sentence,
            }),
        );
    }

    /// One look of the copy task: a copy when the tree changed since the last one. Every part of it ends within `bound`: the wait
    /// for the private repository, the git work and the file calls on the work tree at the time `bound.git` names, storing the copy
    /// within `snapshot::store_limit`, and all of it at the time `bound.stop` names once a process that ends after the copy was asked to
    /// end. A part that is given up is a failed look: counted, logged, and said to the run ([`Copies::look_failed`]).
    async fn look(&self, bound: &LookBound) -> Looked {
        self.step
            .store(STEP_TREE, std::sync::atomic::Ordering::SeqCst);
        let looked = self.look_within(bound).await;
        self.step
            .store(STEP_IDLE, std::sync::atomic::Ordering::SeqCst);
        looked
    }

    async fn look_within(&self, bound: &LookBound) -> Looked {
        // The line of the look before this one, when the keep that was to follow it never started.
        self.keep_line_now("0");
        bound.note(|n| {
            n.began = Some(std::time::Instant::now());
            n.for_a_stop = bound.was_asked_to_end();
            n.step = "tree";
        });
        // A tree whose file system does not answer is named as that within seconds, before any git is started on it.
        if let Err(e) = self.tree.answers(TREE_ANSWER_LIMIT).await {
            bound.tree_is_silent();
            return self.look_failed(bound, &e);
        }
        bound.note(|n| n.step = "lock");
        let Some(mut snapshotter) = bound.git.run(self.snapshotter.lock()).await else {
            return self.look_failed(bound, "the copy before it had not ended");
        };
        // A keep that was in flight was cut by what started this look (`Snapshotter::keep_cut`).
        snapshotter.keep_cut(bound.by == LookBy::Timer && !bound.was_asked_to_end());
        bound.note(|n| n.step = "git");
        // What the spacing counts of a look that fails starts here: a look that never got this far ran no git.
        bound.git_starts();
        let looked = snapshotter.snapshot_until(&bound.git).await;
        self.log_tidy(&mut snapshotter);
        let (steps, kept) = (snapshotter.take_look_steps(), snapshotter.kept_bytes());
        bound.note(|n| {
            n.steps = steps;
            n.kept = kept;
        });
        let mut made = match looked {
            Ok(Some(made)) => made,
            Ok(None) => return Looked::Same,
            Err(e) => return self.look_failed(bound, &e),
        };
        bound.note(|n| {
            n.n = made.count;
            n.bytes = made.bundle_bytes;
            n.step = "hash";
        });
        let hash_began = std::time::Instant::now();
        // The hash of the bundle is processor time of the copy, so it is part of the look's work: made within the look's limit
        // and before that work ends, where the spacing counts it. The upload sends the bundle under this hash and does not hash again.
        if let Err(e) = crate::direct::snapshot::hash_bundle(&mut made, &bound.git).await {
            // The tree has no stored copy: the next look copies it again, changed or not.
            snapshotter.forget_last();
            return self.look_failed(bound, &e);
        }
        bound.note(|n| {
            n.hash = hash_began.elapsed();
            n.step = "store";
        });
        // What the spacing counts ends here: the uploads cost the engine nothing, and one that hangs must not hold the next look back.
        let git_ended = std::time::Instant::now();
        bound.git_has_ended();
        self.step
            .store(STEP_STORE, std::sync::atomic::Ordering::SeqCst);
        let limit = self
            .limits
            .store
            .unwrap_or_else(|| store_limit(made.bundle_bytes));
        let stored = bound
            .stop
            .run(tokio::time::timeout(limit, self.store(&made, Some(limit))))
            .await;
        bound.note(|n| n.store = git_ended.elapsed());
        let e = match stored {
            Some(Ok(Ok(()))) => {
                self.stored_line(bound, &snapshotter);
                return Looked::Stored {
                    bytes: made.bundle_bytes,
                    git_ended,
                };
            }
            Some(Ok(Err(e))) => e,
            Some(Err(_)) => format!("not stored within {} s", limit.as_secs_f32()),
            None => "not stored in the time agentd had left".to_string(),
        };
        // The tree has no stored copy yet: the next look copies it again, changed or not.
        snapshotter.forget_last();
        self.look_failed(bound, &e)
    }

    /// The look in progress has run for the late limit and has not stored its copy: it is logged with what the look is doing, and
    /// the run is told, once until a copy is stored again and not after a failure was said. Nothing is counted: the look goes on and may
    /// still store its copy, which the run then sees as the copy's `savedCopy` event. The late notice has a flag of its own
    /// (`late_said`): when the look is given up after all, the run is told that the copy could not be saved.
    ///
    /// The line states the worker's own numbers: how long the look had taken when its late timer fired (`taken`), the
    /// limit that was applied, the limit that was set, and what the last look that stored a copy took when the limit was made (`limit`).
    fn late(&self, taken: Duration, limit: LateLimit) {
        let (what, sentence) = match self.step.load(std::sync::atomic::Ordering::SeqCst) {
            STEP_STORE => ("storing the copy", COPY_LATE_STORE),
            _ => ("reading the work tree", COPY_LATE_TREE),
        };
        let last = limit.last.map_or_else(
            || "no copy was stored before".to_string(),
            |took| format!("the last stored copy took {:.3} s", took.as_secs_f64()),
        );
        self.log.line(&format!(
            "direct turn snapshot late: {what} has taken {:.3} s so far; its limit was {:.3} s (the set limit is {:.3} s, {last})",
            taken.as_secs_f64(),
            limit.after.as_secs_f64(),
            self.limits.late.as_secs_f64()
        ));
        if !self.failing.load(std::sync::atomic::Ordering::SeqCst)
            && !self
                .late_said
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.notify(sentence);
        }
    }

    /// Whether the look in progress reads the work tree (its git works).
    fn reads_tree(&self) -> bool {
        self.step.load(std::sync::atomic::Ordering::SeqCst) == STEP_TREE
    }

    /// The git of the look in progress works, and the file system of the work tree is asked whether it still answers. When it does
    /// not, the look is given up now: its git is ended, the look fails, and the next tick asks the file system first.
    async fn tree_check(&self, bound: &LookBound) {
        if let Err(e) = self.tree.answers(TREE_ANSWER_LIMIT).await {
            self.log.line(&format!(
                "direct turn snapshot: {}; the look in progress is given up",
                scrub_detail(&e)
            ));
            bound.tree_is_silent();
        }
    }

    /// The limits of a look that starts now: its git work has what a test set, else what `git` gives it (`snapshot::git_limit`).
    fn bound(&self, git: &GitBudget) -> LookBound {
        LookBound::new(self.limits.git.unwrap_or_else(|| git.limit()))
    }

    /// agentd was asked to end, request `n`: logged before the copy is made. `bound` is the look that runs or starts now. In a
    /// process that ends after the copy, nothing of it runs after the time the copy has (`CopyLimits::stop`), counted from now, but
    /// for `snapshot::TREE_ANSWER_LIMIT` at the most while a question to the tree is under way then ([`watched_look`]); the first
    /// process of its machine runs on, so a look of it keeps its own limits: a copy that needs longer than that time is still stored.
    fn stop_asked(&self, n: u64, bound: &LookBound) -> Stop {
        let at = Instant::now() + self.limits.stop;
        self.log
            .line("direct turn session ending: a copy of the work is saved at once");
        bound.asked_to_end(self.ends_after_copy.then_some(at));
        Stop { n, at }
    }

    /// The worker's log line of a look that stored its copy. When a keep follows the look (the timer started the look,
    /// agentd was not asked to end while it ran, and the keep is due, `Snapshotter::keep_is_due`), the line waits for what comes of
    /// the keep ([`Self::keep_between_looks`]); else it is written now.
    fn stored_line(&self, bound: &LookBound, snapshotter: &Snapshotter) {
        let line = bound.line(true);
        if bound.by == LookBy::Timer && !bound.was_asked_to_end() && snapshotter.keep_is_due() {
            *self.keep_line.lock().unwrap_or_else(|p| p.into_inner()) = Some(line);
        } else {
            self.log.line(&line.text("0", line.kept));
        }
    }

    /// Writes the line of a look that still waits for its keep, with `keep` as what came of the keep.
    fn keep_line_now(&self, keep: &str) {
        let line = self
            .keep_line
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(line) = line {
            self.log.line(&line.text(keep, line.kept));
        }
    }

    /// The keep of the run's commits (`Snapshotter::keep_commits`), as an arm of the copy task's wait for its next look
    /// ([`start_snapshots`]). It never ends that wait. When the look before it asked for a keep, the keep runs here, after that
    /// look has stored its copy and after the task has counted the look: nothing of a keep is part of a look, of its limits, of
    /// its late notice or of the spacing of the looks. The tick of the timer, a tool call's wake and a request to end are arms of
    /// the same wait, as in release 5.25: the one that comes drops this future, which ends the keep's git with its group and
    /// removes what it wrote, and the look it starts takes the private repository at once. The task yields once before the keep
    /// takes the repository, so an event that is already there (a wake that came during the look) is taken first and no git of a
    /// keep is started. The copy of the end begins after the copy task was aborted, which drops a keep in flight the same way.
    ///
    /// What came of the keep goes into the look's log line; why a keep was missed is logged once, in a line of its own.
    async fn keep_between_looks(&self) -> std::convert::Infallible {
        let line = self
            .keep_line
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(line) = line {
            let mut ending = KeepEnding {
                log: self.log.clone(),
                line: Some(line),
                started: false,
            };
            tokio::task::yield_now().await;
            let mut snapshotter = self.snapshotter.lock().await;
            ending.started = true;
            let began = std::time::Instant::now();
            let give_up = GiveUp::at(Some(Instant::now() + crate::direct::snapshot::KEEP_LIMIT));
            let keep = snapshotter.keep_commits(&give_up).await;
            let took = began.elapsed().as_millis().max(1).to_string();
            let word = match &keep {
                crate::direct::snapshot::Keep::Kept { .. }
                | crate::direct::snapshot::Keep::Nothing => took.as_str(),
                crate::direct::snapshot::Keep::NotDue => "0",
                crate::direct::snapshot::Keep::Missed(why) => {
                    self.log.line(&format!(
                        "direct turn copy keep missed: {}",
                        scrub_detail(why)
                    ));
                    "missed"
                }
            };
            ending.done(word, snapshotter.kept_bytes());
        }
        std::future::pending().await
    }

    /// What the copy that a request to end asked for came to: logged, and when no copy could be saved the run is told
    /// ([`STOP_COPY_NOT_SAVED`]), once for the request and whatever was said before; no later failure of the streak is said after it.
    fn stop_done(&self, looked: &Looked) {
        self.log.line(match looked {
            Looked::Stored { .. } => "direct turn session ending: a copy of the work was stored",
            Looked::Same => {
                "direct turn session ending: the work had not changed since its last copy"
            }
            Looked::Failed => "direct turn session ending: no copy of the work could be saved",
        });
        if matches!(looked, Looked::Failed) {
            self.failing
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.notify(STOP_COPY_NOT_SAVED);
        }
    }

    /// The copy of the end, made when the engine and everything it started have stopped and the copy task is over: the final
    /// `head` when the end commit was made, else the tree as it is (no git call reads the engine's repository then). It is tried a second
    /// time as the tree when the head cannot be copied. `Some(text)` is what the turn's error says when no copy could be stored.
    ///
    /// The whole of it (the wait for a copy in flight, git, both uploads) has `final_budget`: the delivery that follows must not wait on
    /// a copy that does not come. Giving up drops the work in flight: git is killed with its future and an upload is abandoned. An
    /// upload whose bytes were sent can still be stored after that, so the words are "not confirmed"; a bundle that went up without its
    /// record is served by the route from the bundle's own commit once it is old enough.
    ///
    /// This is the first try. One request of it has the whole budget as its whole-time limit: an upload that moves is not cut,
    /// however large its bundle. Before, a request had a quarter of the budget (30 s of the default 120 s), which a bundle of 812 MB
    /// meets only at 27 MB a second. A request that gets no answer ends when nothing moved for `s3::NO_PROGRESS` (the same 30 s), so
    /// it still leaves room for the tries after it: three fit with their pauses (30 + 2 + 30 + 4 + 30 s). A failure is counted and
    /// logged; the run is told by [`Copies::save_final_again`] or [`Copies::end_copy_not_saved`], once it is known that no later try
    /// stored the copy.
    async fn save_final(&self, head: Option<&str>) -> Option<String> {
        self.save_final_within(head, Some(self.final_budget)).await
    }

    /// The copy of the end once more, for a first try that said `first`: the session's own bundle went up after it, so the
    /// bucket answers. A request has the client's own limit this time, and the try has a budget of the same length as the first. `None`
    /// when the copy is stored now; else what the turn's error says, and the run is told.
    async fn save_final_again(&self, head: Option<&str>, first: &str) -> Option<String> {
        let said = self.save_final_within(head, None).await;
        match &said {
            None => self.log.line(&format!(
                "direct turn final copy stored at the second try; the first said: {}",
                scrub_detail(first)
            )),
            Some(_) => self.end_copy_not_saved(),
        }
        said
    }

    /// The end state of the work has no copy and nothing tries again: the run is told, unless an earlier failure was said already.
    fn end_copy_not_saved(&self) {
        self.tell_failed(END_COPY_NOT_SAVED);
    }

    /// A try of the copy of the end within `final_budget`. Each try that stored a copy or failed writes the worker's save line
    /// ([`Self::end_line`]), also a try that was given up at the budget, with the step it was in as `why`.
    async fn save_final_within(
        &self,
        head: Option<&str>,
        request: Option<Duration>,
    ) -> Option<String> {
        let note = Mutex::new(LookNote {
            began: Some(std::time::Instant::now()),
            step: "lock",
            ..LookNote::default()
        });
        match tokio::time::timeout(
            self.final_budget,
            self.save_final_unbounded(head, request, &note),
        )
        .await
        {
            Ok(said) => said,
            Err(_) => {
                let e = format!("not confirmed within {} s", self.final_budget.as_secs_f32());
                self.count_failed("final copy", &e);
                {
                    let mut n = note.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(began) = n.store_began {
                        n.store = began.elapsed();
                    }
                }
                self.end_line(&note, false);
                Some(format!("final copy: {e}"))
            }
        }
    }

    /// The worker's save line of a try of the copy of the end, as a look writes its own ([`save_line`]): `by=final`, the save
    /// whose record says `"final": true`, and `engine=unread`, because the copy of the end reads no head of the engine. Its store hashes
    /// the bundle, so `hash` is 0, and no keep follows it. A try that failed also has the line that names its error
    /// (`direct turn final copy failed: ...`, [`Self::count_failed`]).
    fn end_line(&self, note: &Mutex<LookNote>, stored: bool) {
        let note = note.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let line = save_line(&note, "final", "unread", stored);
        self.log.line(&line.text("0", line.kept));
    }

    async fn save_final_unbounded(
        &self,
        head: Option<&str>,
        request: Option<Duration>,
        note: &Mutex<LookNote>,
    ) -> Option<String> {
        fn set(note: &Mutex<LookNote>, change: impl FnOnce(&mut LookNote)) {
            change(&mut note.lock().unwrap_or_else(|p| p.into_inner()));
        }
        // The line of the last look, when the copy task ended before the keep that was to follow it started.
        self.keep_line_now("0");
        let mut snapshotter = self.snapshotter.lock().await;
        set(note, |n| n.step = "git");
        let made = match snapshotter.final_copy(head).await {
            Ok(made) => Ok(made),
            Err(e) if head.is_some() => {
                self.log_tidy(&mut snapshotter);
                self.log.line(&format!(
                    "direct turn final copy of the head failed, copying the tree: {}",
                    scrub_detail(&e)
                ));
                snapshotter.final_copy(None).await
            }
            Err(e) => Err(e),
        };
        self.log_tidy(&mut snapshotter);
        let (steps, kept) = (snapshotter.take_look_steps(), snapshotter.kept_bytes());
        set(note, |n| {
            n.steps = steps;
            n.kept = kept;
        });
        let stored = match made {
            Ok(made) => {
                let store_began = std::time::Instant::now();
                set(note, |n| {
                    n.n = made.count;
                    n.bytes = made.bundle_bytes;
                    n.step = "store";
                    n.store_began = Some(store_began);
                });
                let stored = self.store(&made, request).await;
                set(note, |n| n.store = store_began.elapsed());
                stored
            }
            Err(e) => Err(e),
        };
        match stored {
            Ok(()) => {
                self.end_line(note, true);
                None
            }
            Err(e) => {
                self.count_failed("final copy", &e);
                self.end_line(note, false);
                Some(format!("final copy: {}", scrub_detail(&e)))
            }
        }
    }
}

/// The timer of the copy task: what a test set, else the invocation's `snapshotEveryS` (`DEFAULT_SNAPSHOT_EVERY_S`); `None` when the
/// invocation switched the copies off (0).
fn copy_timer(app: &App, req: &DirectInvocationRequest) -> Option<Duration> {
    match app.cfg.direct.snapshot_interval {
        Some(d) => Some(d),
        None => match req.snapshot_every_s.unwrap_or(DEFAULT_SNAPSHOT_EVERY_S) {
            0 => None,
            s => Some(Duration::from_secs(u64::from(s))),
        },
    }
}

/// Why the copy task looks at the tree.
#[derive(Clone, Copy)]
enum Why {
    Timer,
    ToolCall,
    /// agentd was asked to end: the number of the request.
    Ending(u64),
}

/// The copy clock after a look that ran under `bound`.
fn note_look(
    clock: &mut CopyClock,
    started: std::time::Instant,
    bound: &LookBound,
    looked: &Looked,
) {
    match looked {
        Looked::Stored { bytes, git_ended } => clock.looked(started, *git_ended, Some(*bytes)),
        Looked::Same => clock.looked(started, std::time::Instant::now(), None),
        // A look that failed counts with the time its git work took, as a look that stored a copy does. The store that failed
        // is not counted, so a bucket that does not answer holds no look back; neither is the wait of a look that never started git.
        Looked::Failed => {
            let (from, to) = bound.counted(started);
            clock.failed(from, to);
        }
    }
}

/// The git budget after a look that started at `started` under `bound`: what the git work of a stored copy took, a look that its own git
/// limit gave up, or a git work that ended in time.
fn note_git(git: &mut GitBudget, started: std::time::Instant, bound: &LookBound, looked: &Looked) {
    match looked {
        Looked::Stored { git_ended, .. } => {
            git.stored(git_ended.saturating_duration_since(started));
        }
        Looked::Failed if bound.git_limit_reached() => git.gave_up(),
        Looked::Same | Looked::Failed => git.ended(),
    }
}

/// The late limit of one look and what it came from: `after` is `snapshot::late_after` of the set limit and of `last`,
/// what the last look that stored a copy took when the limit was made. The worker's late line states both ([`Copies::late`]).
#[derive(Clone, Copy)]
struct LateLimit {
    after: Duration,
    last: Option<Duration>,
}

impl LateLimit {
    /// The late limit of a look that starts now, made from the set limit `set` and from what the last look that stored a copy took
    /// (`last`). Every look makes its own: the look that follows a look which a request to end overtook too, after the copy
    /// that look may have stored.
    fn made(set: Duration, last: Option<Duration>) -> Self {
        Self {
            after: late_after(set, last),
            last,
        }
    }
}

/// One look of the copy task, watched while it runs. The run is told when the look has run for `limit.after` ([`Copies::late`],
/// `snapshot::late_after`). While its git works, the file system of the work tree is asked every `snapshot::TREE_ASK_EVERY` whether it
/// still answers, from the first period on and whatever the late limit, and the look is given up when it does not
/// ([`Copies::tree_check`]). The question is waited for, `snapshot::TREE_ANSWER_LIMIT` at the most, and the late line, a request to end
/// and the end of the look wait with it. A request to end that comes while the look runs is taken up ([`Copies::stop_asked`]): `stop`
/// then holds it. Only in a process that ends after the copy does it bring the end of the look forward.
async fn watched_look(
    copies: &Copies,
    bound: &LookBound,
    limit: LateLimit,
    ending: &mut EndingWatch,
    stop: &mut Option<Stop>,
) -> Looked {
    // The clock of the late line starts before the late timer is armed: the time the line states is never below the limit.
    let began = Instant::now();
    let look = copies.look(bound);
    tokio::pin!(look);
    let late = tokio::time::sleep(limit.after);
    tokio::pin!(late);
    let mut said = false;
    let period = copies.limits.ask_every;
    let mut check = tokio::time::interval_at(Instant::now() + period, period);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // The arms are polled in this order: the end of the look, its late line, a request to end, the question to the tree; at a
        // late limit equal to the period of the question the late line comes first. The question's arm is on from the first pass and
        // asks only while the look reads the tree: a condition of the arm is read before the look is first polled, when it reads nothing
        // yet, so the arm stayed off until the late timer or a request to end woke the loop. The arm waits for the answer, for
        // `snapshot::TREE_ANSWER_LIMIT` at the most, and the late line, a request to end and the end of the look wait with it.
        tokio::select! {
            biased;
            looked = &mut look => return looked,
            () = &mut late, if !said => {
                said = true;
                copies.late(began.elapsed(), limit);
            }
            n = ending.asked(), if stop.is_none() => {
                *stop = Some(copies.stop_asked(n, bound));
            }
            _ = check.tick() => {
                if copies.reads_tree() {
                    copies.tree_check(bound).await;
                }
            }
        }
    }
}

/// Starts the copy task: it looks at the tree every `snapshotEveryS` seconds, counted from the start of one look to the start of
/// the next, and when a tool call ended ([`Cadence`]), and stores a copy when the tree changed. The looks take a tenth of the time at the
/// most (`Cadence::delay` has the formula). After a copy that failed the timer is `snapshot::COPY_RETRY` when that is shorter.
/// With the copies switched off (0) it looks on no timer and after no tool call; the copy of the end is made all the same.
///
/// When agentd is asked to end (`server::Ending`) the task saves a copy at once, with no floor and no spacing, whatever the timer: a look
/// that is in flight ends first, because its copy may be all but stored, and one more look follows it, because the look in flight read
/// the tree before the request came; that look makes its late limit from the last stored copy, as every look does ([`LateLimit::made`]).
/// In a process that ends after the copy, both end within `snapshot::STOP_COPY_BUDGET` of the request, later by
/// `snapshot::TREE_ANSWER_LIMIT` at the most for each question to the tree that is under way when the request comes or when that
/// time is up, because a request to end and the end of a look wait for the question ([`watched_look`]):
/// what is not stored by then is lost with the process. The first process of its machine runs on, so there the request cuts nothing
/// short: the look in flight keeps its own limits, the look that follows it has the limits of any look, and when it could not be saved
/// the task looks again every `snapshot::STOP_COPY_RETRY` until a copy is stored. A look that a request to end asked for leaves the tick
/// of the timer where it is (`snapshot::CopyTimer`). Then the task goes on as before: while the process lives, the timer keeps saving
/// copies.
///
/// No look is waited for without a limit ([`Copies::look`], [`watched_look`]): a look that does not return is given up, said to the run,
/// and the next tick looks again. The git work of a look has `snapshot::git_limit`, which follows the last stored copy.
///
/// Between two looks the task keeps the run's commits, when the look before it asks for that
/// ([`Copies::keep_between_looks`]). The keep is one more arm of the wait for the next look and never ends that wait: the tick of
/// the timer, a tool call's wake and a request to end are taken as before, and each of them drops the keep, which ends its git. A
/// keep never stands between a look and its copy, and a look counts nothing of it.
fn start_snapshots(
    app: &App,
    req: &DirectInvocationRequest,
    copies: Copies,
    wake: Arc<tokio::sync::Notify>,
) -> Option<SnapshotTask> {
    let every = copy_timer(app, req);
    let mut cadence = Cadence::new(every);
    if let Some(floor) = app.cfg.direct.snapshot_floor {
        cadence = cadence.with_floor(floor);
    }
    if let Some(retry) = app.cfg.direct.snapshot_retry {
        cadence = cadence.with_retry(retry);
    }
    if let Some(test_timer) = app.cfg.direct.snapshot_interval {
        // A timer that a test set keeps the cap the spacing had while the default timer was one minute: five of its periods.
        cadence = cadence.with_space_max((test_timer * COPY_SPACING_CAP).min(COPY_SPACING_MAX));
    }
    let mut ending = app.ending.watch();
    Some(SnapshotTask(tokio::spawn(async move {
        // The cadence counts in real time (`std`); the sleeps are tokio's.
        let mut clock = CopyClock::default();
        let mut timer = CopyTimer::new(every, Instant::now());
        // What the last look that stored a copy took, git and uploads: a look is late against it. It is read on the clock of the
        // runtime (`look_started`), the clock that times the late limit and the limits that give a look up: the late rule
        // rests on one clock, and a test that pauses that clock drives all of it.
        let mut last_took: Option<Duration> = None;
        // What the git work of the last stored copy took: the git work of a look is given up against it.
        let mut git = GitBudget::default();
        // The copy that a request to end asked for could not be saved and this process runs on: the task looks again soon.
        let mut trying_again = false;
        loop {
            // The timer, a tool call that ended while the bundles are small, or a request to end.
            let tick = async {
                match timer.next() {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let mut why = tokio::select! {
                () = tick => Why::Timer,
                () = wake.notified(), if cadence.wakes_now(&clock, trying_again) => Why::ToolCall,
                n = ending.asked() => Why::Ending(n),
                // The keep of the run's commits runs while the task waits. It never ends the wait: what does drops it.
                never = copies.keep_between_looks() => match never {},
            };
            // A look that a request to end asked for has no floor and no spacing, and neither has a look that tries its copy again: only
            // the retry tick and a request to end start one then, no tool call (`Cadence::wakes_now`).
            if !matches!(why, Why::Ending(_)) && !trying_again {
                let wait = cadence.delay(
                    &clock,
                    matches!(why, Why::ToolCall),
                    std::time::Instant::now(),
                );
                if !wait.is_zero() {
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        n = ending.asked() => why = Why::Ending(n),
                    }
                }
            }
            let mut started = std::time::Instant::now();
            let asked_first = matches!(why, Why::Ending(_));
            // A look that a request to end asked for leaves the tick that was due where it is.
            let mut look_started = Instant::now();
            timer.look_starts(look_started, asked_first);
            // The bound of the look that ran last: what the copy clock counts of a look that failed ends with it.
            let mut bound = copies.bound(&git);
            bound.by = match why {
                Why::ToolCall => LookBy::Tool,
                Why::Timer if trying_again => LookBy::Retry,
                Why::Timer | Why::Ending(_) => LookBy::Timer,
            };
            let late = LateLimit::made(copies.limits.late, last_took);
            let mut stop = match why {
                Why::Ending(n) => Some(copies.stop_asked(n, &bound)),
                Why::Timer | Why::ToolCall => None,
            };
            let mut looked = watched_look(&copies, &bound, late, &mut ending, &mut stop).await;
            note_git(&mut git, started, &bound, &looked);
            if matches!(looked, Looked::Stored { .. }) {
                last_took = Some(look_started.elapsed());
            }
            if let (false, Some(asked)) = (asked_first, stop)
                && (!copies.ends_after_copy || Instant::now() < asked.at)
            {
                // The request to end came while that look ran: what was written after it had read the tree is in no copy yet.
                note_look(&mut clock, started, &bound, &looked);
                started = std::time::Instant::now();
                look_started = Instant::now();
                bound = copies.bound(&git);
                bound.asked_to_end(copies.ends_after_copy.then_some(asked.at));
                let late = LateLimit::made(copies.limits.late, last_took);
                looked = watched_look(&copies, &bound, late, &mut ending, &mut stop).await;
                note_git(&mut git, started, &bound, &looked);
                if matches!(looked, Looked::Stored { .. }) {
                    last_took = Some(look_started.elapsed());
                }
            }
            if let Some(asked) = stop {
                copies.stop_done(&looked);
                ending.saved(asked.n);
                // A process that ends after the copy tries nothing more; the first process of its machine runs on.
                trying_again |= !copies.ends_after_copy && matches!(looked, Looked::Failed);
            }
            note_look(&mut clock, started, &bound, &looked);
            // The tick after a look that failed comes after the retry time at the latest, counted from its start, whatever the
            // timer of the run (`Cadence::period`); a tick of the timer that comes sooner stays.
            if matches!(looked, Looked::Failed)
                && let Some(retry) = cadence.period(true)
            {
                timer.try_again(look_started, retry);
            }
            if trying_again {
                if matches!(looked, Looked::Failed) {
                    timer.try_again(Instant::now(), STOP_COPY_RETRY);
                } else {
                    trying_again = false;
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

/// How the end sequence's error starts when the git settings were not put back (`git_settings::put_back`).
const SETTINGS_NOT_BACK: &str = "restore git settings: ";

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
    let mut no_git: Option<git_settings::NotPutBack> = None;
    // The copy of the end was stored before a wait on the storage.
    let mut early_copy_stored = false;
    if let Some(wt) = &st.wt {
        if let Some(h) = st.hidden.take()
            && let Err(e) = workspace::restore_hidden(wt, h)
        {
            errors.push(format!("restore hidden paths: {e}"));
        }
        // The files are written with the host's calls. With the watch's test switch on, the first one is answered once as the
        // storage was seen to answer.
        // A test gives its own calls and its own repeat (`App::settings_write`); the product never does.
        let (calls, repeat): (
            Arc<dyn crate::checked_write::FileCalls>,
            git_settings::Repeat,
        ) = match (app.settings_write.get(), &st.storage) {
            (Some((calls, repeat)), _) => (calls.clone(), *repeat),
            (None, Some(watch)) => (watch.settings_calls(), git_settings::Repeat::default()),
            (None, None) => (
                Arc::new(crate::checked_write::Host),
                git_settings::Repeat::default(),
            ),
        };
        // Root invariant 9: a refused write is repeated for minutes, and the session can end in that wait. The copy of the end
        // is saved before it, as the tree is: no git call reads the engine's repository for that copy. When the write works in the end,
        // the commit step below saves the copy of the end again, with the head.
        if let Some(g) = &st.git_state
            && let Err(e) = git_settings::put_back(
                g,
                wt,
                calls,
                repeat,
                &app.log,
                st.storage.as_ref(),
                async || {
                    if let Some(copies) = &st.copies {
                        let said = copies.save_final(None).await;
                        app.log.line(&match &said {
                            None => git_settings::COPIED_BEFORE_WAIT.to_string(),
                            Some(e) => format!(
                                "{}: {}",
                                git_settings::NOT_COPIED_BEFORE_WAIT,
                                scrub_detail(e)
                            ),
                        });
                        early_copy_stored = said.is_none();
                    }
                },
            )
            .await
        {
            errors.push(format!("{SETTINGS_NOT_BACK}{e}"));
            no_git = Some(e);
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
    // Whether the copy of the end was tried in the commit step below.
    let mut final_copy_tried = false;
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
    if let Some(wt) = st.wt.as_ref().filter(|_| no_git.is_none()) {
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
                    no_git = Some(git_settings::NotPutBack::Other(e));
                }
                None
            }
        };
        git.committed_by_harness = head.is_some() && head != before;
        // The copy of the end, before the session's own bundle is written and before anything that can refuse a delivery. Without a
        // head (the commit failed) it is the tree as it is.
        // What it says goes into the turn's errors here, once the second try (below) has had its turn.
        let final_copy_at = errors.len();
        let final_head = head.clone();
        let mut final_copy_said: Option<String> = None;
        if let Some(copies) = &st.copies {
            final_copy_said = copies.save_final(head.as_deref()).await;
            final_copy_tried = true;
        }
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
        // The first try of the copy of the end was not stored. When the session's own bundle went up after it the bucket answers,
        // so the copy is tried once more; without that sign nothing more is tried and the end sequence goes on. Either way the run is
        // told when its end state has no copy.
        if let (Some(copies), Some(first)) = (&st.copies, final_copy_said) {
            let said = if git.bundle_uri.is_some() {
                copies.save_final_again(final_head.as_deref(), &first).await
            } else {
                copies.end_copy_not_saved();
                Some(first)
            };
            if let Some(e) = said {
                errors.insert(final_copy_at.min(errors.len()), e);
            }
        }
    }

    // A checkout agentd runs no git in still has its end state copied: the tree as it is, hashed in agentd's own repository, which
    // has its own configuration and reads nothing but files from the checkout. When the engine removed or replaced `.git` the copy holds
    // the whole tree, without the history it can no longer read. No session bundle follows it, so there is one try.
    // When that copy was stored before the wait on the storage, it holds this tree: nothing has written it since.
    if !final_copy_tried
        && !early_copy_stored
        && let Some(copies) = &st.copies
        && let Some(e) = copies.save_final(None).await
    {
        copies.end_copy_not_saved();
        errors.push(e);
    }
    // A session that could not make the private repository saved no copy at all: the end of the turn says so.
    if st.copies.is_none()
        && let Some(e) = &st.copies_error
    {
        errors.push(format!("final copy: {}", scrub_detail(e)));
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
    if let Some(why) = &no_git {
        if classified.outcome == DirectOutcome::ReportedDone {
            classified = classify::no_git(why, run);
        } else if matches!(why, git_settings::NotPutBack::Refused { .. })
            && let Some(said) = errors.iter_mut().find(|e| e.starts_with(SETTINGS_NOT_BACK))
        {
            // The engine's label stays, so the harness's detail with the command of the newest copy is not said. The error
            // names the command itself, once.
            *said = format!(
                "{said}; ham pull {run} --snapshot downloads the newest copy, if one was saved"
            );
        }
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
    // What the harness told the engine about the delivery: its own block, as it was written before the first prompt.
    summary.delivery_note = st.delivery_note.clone();
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
