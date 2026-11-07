//! agentd settings from the environment (defaults match the worker image layout).

use crate::packages::PackagesIndex;
use harness_protocol::direct::{
    INPUT_MARGIN_S, INPUT_MIN_WINDOW_S, MAX_AUTO_SESSIONS, MAX_INPUT_REQUESTS, MAX_INPUT_WAIT_S,
    MAX_USER_SESSIONS, OUTAGE_NOTICE_AFTER_S, REVIEW_GUARD_S,
};
use harness_protocol::runtime::RUNTIME_PORT;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Settings of the direct driver (`crate::direct`). The defaults are the contract's; the rest exist for tests and for the
/// kill switch of a launch setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectConfig {
    /// `AGENTD_DISABLED_TOOLS`: comma-separated tool names the engine's launch settings switch off. Empty (the default) leaves
    /// every tool on, sub-agents included, as in the core engine; `spawn_agent` is the kill switch.
    pub disabled_tools: Vec<String>,
    /// `AGENTD_DIRECT_EVENTS_EVERY_MS` (default 2000).
    pub events_every: Duration,
    /// `AGENTD_DIRECT_HEARTBEAT_EVERY_MS` (default 60000).
    pub heartbeat_every: Duration,
    /// `AGENTD_DIRECT_UPLOAD_PAUSE_MS` (default 2000): the first pause before an upload is tried again; it doubles (2, 4, 8 s).
    pub upload_retry_pause: Duration,
    /// `AGENTD_DIRECT_SNAPSHOT_EVERY_MS` (tests only): replaces the invocation's snapshot interval.
    pub snapshot_interval: Option<Duration>,
    /// `AGENTD_DIRECT_PROVIDER_RETRIES` (default 3, at most 6): how many times the prompt is retried on the same engine session
    /// after a provider failure (the first slice of provider failover). 0 switches it off; a bad value is the default.
    pub provider_retries: u32,
    /// `AGENTD_DIRECT_RETRY_PAUSE_MS` (default 5000): the first wait before a retry; it doubles (5, 10, 20 s).
    pub provider_retry_pause: Duration,
    /// `AGENTD_DIRECT_ENGINE_STOP_RESUMES` (default 1, at most 3; 0 allowed): how many times the prompt is resumed on the same route and
    /// model after the engine ended it `cancelled` with no cancel from agentd. Such a stop
    /// never switches a route or a model. A bad value is the default. It follows `AGENTD_DIRECT_PROVIDER_RETRIES=0`: with the retry off
    /// there is no resume either.
    pub engine_stop_resumes: u32,
    /// `AGENTD_DIRECT_FAILOVER` (default on; `0`, `off`, `false` or `no` switch it off): when the request carries failover routes, switch the
    /// engine to them after the resumes on the model are spent (the second slice of provider failover). Off: the routes are ignored.
    pub failover: bool,
    /// `AGENTD_DIRECT_FAILOVER_RETRIES` (default 1, at most 3): resumes on a fallback route after a transient failure.
    pub failover_retries: u32,
    /// `AGENTD_DIRECT_FAILOVER_DWELL_S` (default 120): how long after the model failed it may be tried again.
    pub failover_dwell: Duration,
    /// `AGENTD_DIRECT_FAILOVER_WAIT_S` (default 30): the first wait with every route down; it grows by half each time.
    pub failover_wait: Duration,
    /// `AGENTD_DIRECT_FAILOVER_WAIT_CAP_S` (default 300): the longest such wait.
    pub failover_wait_cap: Duration,
    /// `AGENTD_DIRECT_OUTAGE_NOTICE_S` (default 1800, 1 to 86400): after this long with every route down the run raises its outage notice once
    /// and goes on waiting and probing until its deadline.
    pub outage_notice: Duration,
    /// `AGENTD_DIRECT_FAILOVER_PROBE` (default on): send a small test request to each route while every route is down, instead of a
    /// resume that carries the whole conversation.
    pub failover_probe: bool,
    /// `AGENTD_DIRECT_INPUT_WAIT_S` (default 0 = never wait, at most 14400): how long a stopped run waits for the user to answer a
    /// question. The request's `inputWaitS` replaces it when the client sends one; with neither, a run
    /// ends at its first terminal stop exactly as before. A bad value is the default.
    pub input_wait: Duration,
    /// `AGENTD_DIRECT_INPUT_MAX_REQUESTS` (default 3, at most 8; 0 never asks): questions one run may raise.
    pub input_max_requests: u32,
    /// `AGENTD_DIRECT_INPUT_MARGIN_S` (default 900, at most 3600; 0 allowed): the window is clamped to the run's remaining life less this.
    pub input_margin: Duration,
    /// `AGENTD_DIRECT_INPUT_MIN_WINDOW_S` (default 300, 1 to 3600): no question opens when the clamped window is shorter than this.
    pub input_min_window: Duration,
    /// `AGENTD_DIRECT_AUTO_SESSIONS_MAX` (default 1, at most 3; 0 switches the automatic fresh session off): fresh sessions agentd starts on its own per run.
    pub auto_sessions_max: u32,
    /// `AGENTD_DIRECT_USER_SESSIONS_MAX` (default 3, at most 8; 0 refuses the user's fresh-session answer): fresh sessions the user may ask for per run.
    pub user_sessions_max: u32,
    /// `AGENTD_DIRECT_CARGO_JOBS` (default 2): `CARGO_BUILD_JOBS` of the engine's session. A whole number from 1 is taken, and above 64 it is 64;
    /// anything else (0, a negative number, text, an empty value) is the default.
    pub cargo_jobs: u32,
    /// `AGENTD_CARGO_TARGET_DIR`: where the engine's cargo keeps `target/`.
    pub cargo_target: crate::direct::cargo::TargetSetting,
    /// `AGENTD_DISK_LOW_MB` (default 512, whole MiB; 0 switches the check off): the root volume counts as short of room below this much free
    /// space. Anything that is not a whole number is the default.
    pub disk_low_mb: u64,
    /// `AGENTD_WORKSPACE_LIMIT_MB` (default 960 = the mount's 1024 MiB less a 64 MiB margin; 0 switches the check off): the checkout mount
    /// counts as full at this much use. The mount reports no use, so agentd measures the folder. Anything that is not a whole number
    /// is the default.
    pub workspace_limit_mb: u64,
    /// `AGENTD_DISK_CHECK_S` (default 60, 0 to 3600; 0 switches the pruning off): seconds between two looks at the free space of the root volume
    /// while a direct run is on.
    pub disk_check: Duration,
    /// `AGENTD_DISK_PRUNE_MB` (default 1536, whole MiB; 0 switches the pruning off): the cargo target folder is pruned when the root volume has
    /// less free space than this.
    pub disk_prune_mb: u64,
    /// `AGENTD_CARGO_KEEP_MIN` (default 20, 0 to 1440 minutes; 0 switches the pruning off): a cargo unit compiled in this long is never pruned.
    pub cargo_keep: Duration,
    /// `AGENTD_DIRECT_REVIEW_ROUND` (default on; `0`, `off`, `false` or `no` switch it off for every run): the review round. A run cannot
    /// switch on what the stage switched off; the request's `review` member can switch it off for one run.
    pub review_round: bool,
    /// `AGENTD_DIRECT_REVIEW_GUARD_S` (default 1200, 60 to 28800): the least time a run must have left for the review round to start. The
    /// request's `review.guardS` replaces it. A value outside the range, or one that is not a whole number, is the default.
    pub review_guard: Duration,
}

impl DirectConfig {
    pub fn from_vars(get: &impl Fn(&str) -> Option<String>) -> Self {
        let ms = |key: &str, default: u64| {
            Duration::from_millis(
                get(key)
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|v| *v > 0)
                    .unwrap_or(default),
            )
        };
        let secs = |key: &str, default: u64| {
            Duration::from_secs(
                get(key)
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .filter(|v| *v > 0)
                    .unwrap_or(default),
            )
        };
        // Whole seconds in `min..=max`: above the maximum is the maximum, a value below the minimum or one that is not a whole
        // number is the default.
        let bounded_secs = |key: &str, default: u64, min: u64, max: u64| {
            Duration::from_secs(
                get(key)
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .filter(|n| *n >= min)
                    .map_or(default, |n| n.min(max)),
            )
        };
        // A count from 0 (off) to `max`: above the maximum is the maximum, a value that is not a whole number is the default.
        let bounded_count = |key: &str, default: u32, max: u32| {
            get(key)
                .and_then(|v| v.trim().parse::<u32>().ok())
                .map_or(default, |n| n.min(max))
        };
        // A switch that is on unless it says otherwise.
        let on = |key: &str| {
            !get(key).is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "off" | "false" | "no"
                )
            })
        };
        Self {
            disabled_tools: get("AGENTD_DISABLED_TOOLS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|t| {
                    !t.is_empty()
                        && t.bytes()
                            .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
                })
                .map(str::to_string)
                .collect(),
            events_every: ms("AGENTD_DIRECT_EVENTS_EVERY_MS", 2_000),
            heartbeat_every: ms("AGENTD_DIRECT_HEARTBEAT_EVERY_MS", 60_000),
            upload_retry_pause: ms("AGENTD_DIRECT_UPLOAD_PAUSE_MS", 2_000),
            snapshot_interval: get("AGENTD_DIRECT_SNAPSHOT_EVERY_MS")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .map(Duration::from_millis),
            provider_retries: get("AGENTD_DIRECT_PROVIDER_RETRIES")
                .and_then(|v| v.trim().parse::<u32>().ok())
                .map_or(3, |n| n.min(crate::direct::retry::MAX_CONFIGURED_RETRIES)),
            provider_retry_pause: ms("AGENTD_DIRECT_RETRY_PAUSE_MS", 5_000),
            engine_stop_resumes: get("AGENTD_DIRECT_ENGINE_STOP_RESUMES")
                .and_then(|v| v.trim().parse::<u32>().ok())
                .map_or(1, |n| n.min(3)),
            failover: on("AGENTD_DIRECT_FAILOVER"),
            failover_retries: get("AGENTD_DIRECT_FAILOVER_RETRIES")
                .and_then(|v| v.trim().parse::<u32>().ok())
                .map_or(1, |n| n.min(3)),
            failover_dwell: secs("AGENTD_DIRECT_FAILOVER_DWELL_S", 120),
            failover_wait: secs("AGENTD_DIRECT_FAILOVER_WAIT_S", 30),
            failover_wait_cap: secs("AGENTD_DIRECT_FAILOVER_WAIT_CAP_S", 300),
            outage_notice: bounded_secs(
                "AGENTD_DIRECT_OUTAGE_NOTICE_S",
                OUTAGE_NOTICE_AFTER_S,
                1,
                86_400,
            ),
            failover_probe: on("AGENTD_DIRECT_FAILOVER_PROBE"),
            input_wait: bounded_secs(
                "AGENTD_DIRECT_INPUT_WAIT_S",
                0,
                0,
                u64::from(MAX_INPUT_WAIT_S),
            ),
            input_max_requests: bounded_count(
                "AGENTD_DIRECT_INPUT_MAX_REQUESTS",
                MAX_INPUT_REQUESTS,
                8,
            ),
            input_margin: bounded_secs("AGENTD_DIRECT_INPUT_MARGIN_S", INPUT_MARGIN_S, 0, 3_600),
            input_min_window: bounded_secs(
                "AGENTD_DIRECT_INPUT_MIN_WINDOW_S",
                INPUT_MIN_WINDOW_S,
                1,
                3_600,
            ),
            auto_sessions_max: bounded_count(
                "AGENTD_DIRECT_AUTO_SESSIONS_MAX",
                MAX_AUTO_SESSIONS,
                3,
            ),
            user_sessions_max: bounded_count(
                "AGENTD_DIRECT_USER_SESSIONS_MAX",
                MAX_USER_SESSIONS,
                8,
            ),
            cargo_jobs: get("AGENTD_DIRECT_CARGO_JOBS")
                .and_then(|v| v.trim().parse::<u32>().ok())
                .filter(|n| *n >= 1)
                .map_or(crate::direct::cargo::DEFAULT_JOBS, |n| {
                    n.min(crate::direct::cargo::MAX_JOBS)
                }),
            cargo_target: crate::direct::cargo::TargetSetting::parse(
                get("AGENTD_CARGO_TARGET_DIR").as_deref(),
            ),
            disk_low_mb: crate::direct::disk::parse_mb(
                get("AGENTD_DISK_LOW_MB").as_deref(),
                crate::direct::disk::DEFAULT_LOW_MB,
            ),
            workspace_limit_mb: crate::direct::disk::parse_mb(
                get("AGENTD_WORKSPACE_LIMIT_MB").as_deref(),
                crate::direct::disk::DEFAULT_WORKSPACE_LIMIT_MB,
            ),
            disk_check: bounded_secs("AGENTD_DISK_CHECK_S", 60, 0, 3_600),
            disk_prune_mb: crate::direct::disk::parse_mb(
                get("AGENTD_DISK_PRUNE_MB").as_deref(),
                1536,
            ),
            cargo_keep: Duration::from_secs(
                u64::from(bounded_count("AGENTD_CARGO_KEEP_MIN", 20, 1_440)) * 60,
            ),
            review_round: on("AGENTD_DIRECT_REVIEW_ROUND"),
            review_guard: Duration::from_secs(
                get("AGENTD_DIRECT_REVIEW_GUARD_S")
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    .filter(|n| REVIEW_GUARD_S.contains(*n))
                    .map_or(u64::from(REVIEW_GUARD_S.default), u64::from),
            ),
        }
    }
}

impl Default for DirectConfig {
    fn default() -> Self {
        Self::from_vars(&|_| None)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// `AGENTD_LISTEN`, default `0.0.0.0:8080`.
    pub listen: SocketAddr,
    /// `AGENTD_PROFILES_DIR`: `engine/profiles/*.toml`.
    pub profiles_dir: PathBuf,
    /// `AGENTD_FIXTURES_ROOT`: repository root holding `fixtures/engines/<name>` (the `{fixtures}` value).
    pub fixtures_root: PathBuf,
    /// `AGENTD_TEMPLATES_ROOT`: root that profile `configEmitters` templates are relative to.
    pub templates_root: PathBuf,
    /// `AGENTD_WORKSPACE_ROOT`: session storage (`/mnt/workspace`).
    pub workspace_root: PathBuf,
    /// `AGENTD_DATA_ROOT`: local disk for engine HOME, data dirs and configs.
    pub data_root: PathBuf,
    /// `AGENTD_RUNTIME_DIR`: the credential socket.
    pub runtime_dir: PathBuf,
    /// `AGENTD_ENGINE_PATH`: colon-separated directories for engine binaries, plus the Rust toolchain's bin directory of the worker image
    /// (`AGENTD_RUST_BIN`, default [`DEFAULT_RUST_BIN`], empty switches it off), placed before the system directories.
    pub engine_path: Vec<PathBuf>,
    /// `AGENTD_EXE`: this binary, as the engine's `credential_process` calls it.
    pub agentd_exe: PathBuf,
    /// `AGENTD_ENGINE_USER` (default `agent`; empty disables): uid split when running as root.
    pub engine_user: Option<String>,
    /// `AGENTD_ENGINE_SANDBOX`: how the engine is kept out of agentd's secrets. Unset or any other value
    /// is kept as a refusal, and no engine starts.
    pub engine_sandbox: crate::sandbox::Setting,
    /// `AGENTD_ENGINE_IDENTITY`: who the engine runs as in the `landlock` mode. A value that is neither
    /// is kept as a refusal and the run ends. The `uid` mode does not read it.
    pub engine_identity: crate::sandbox::IdentitySetting,
    /// `AGENTD_NOBODY_EXEC` (default `/opt/harness/bin/ham-nobody-exec`): the helper that starts the engine on the nobody route.
    pub identity_helper: PathBuf,
    /// `AGENTD_S3_ENDPOINT`: path-style S3 endpoint (tests only).
    pub s3_endpoint: Option<String>,
    /// `AGENTD_PACKAGES_*`: the CodeArtifact repository the engine installs from.
    pub packages: Option<PackagesIndex>,
    /// `AGENTD_INTERNAL_API_ENDPOINT`: replaces the invocation's internal API URL (tests only).
    pub internal_api_endpoint: Option<String>,
    /// `AGENTD_GOPROXY` (non-blank): the stage's Go module source, which the package proxy serves to the engine's `go`
    /// tool. The runtimes stack names it on every worker runtime.
    pub goproxy: Option<String>,
    /// The direct driver's settings (`crate::direct`).
    pub direct: DirectConfig,
}

/// `AGENTD_OPEN_FILES_CAP` (default 65536; `0`, `off`, `false` or `no` leave the session's open-files limit as it is; a number from 256 to
/// 4194304 is the cap; anything else is kept as a config error and the default applies): the soft limit that `agentd serve` lowers itself to
/// when it is above, so every process it starts inherits the lower one (`crate::nofile`).
pub fn open_files_cap(get: &impl Fn(&str) -> Option<String>) -> crate::nofile::Setting {
    crate::nofile::Setting::from_value(get(crate::nofile::VAR).as_deref())
}

/// Where the worker image installs the Rust toolchain's programs (`images/tools/lang/install.sh` puts it below `lang/rust`).
pub const DEFAULT_RUST_BIN: &str = "/opt/harness/gate-tools/lang/rust/bin";

/// `path` with the Rust toolchain's bin directory in it once: before the first system directory, so the pinned toolchain wins over
/// anything in `/usr/bin`, or at the end when the list has none. `rust_bin` is `AGENTD_RUST_BIN`: unset is the image's directory, empty
/// leaves the list as it is.
fn with_rust_bin(mut path: Vec<PathBuf>, rust_bin: Option<String>) -> Vec<PathBuf> {
    let dir = PathBuf::from(rust_bin.as_deref().map_or(DEFAULT_RUST_BIN, str::trim));
    if dir.as_os_str().is_empty() || path.contains(&dir) {
        return path;
    }
    let at = path
        .iter()
        .position(|p| {
            ["/usr/local/bin", "/usr/bin", "/bin"]
                .iter()
                .any(|s| p == Path::new(s))
        })
        .unwrap_or(path.len());
    path.insert(at, dir);
    path
}

impl Config {
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Self {
        let path = |key: &str, default: &str| {
            PathBuf::from(get(key).unwrap_or_else(|| default.to_string()))
        };
        Self {
            listen: get("AGENTD_LISTEN")
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], RUNTIME_PORT))),
            profiles_dir: path("AGENTD_PROFILES_DIR", "/opt/harness/engine/profiles"),
            fixtures_root: path("AGENTD_FIXTURES_ROOT", "/opt/harness"),
            templates_root: path("AGENTD_TEMPLATES_ROOT", "/opt/harness"),
            workspace_root: path("AGENTD_WORKSPACE_ROOT", "/mnt/workspace"),
            data_root: path("AGENTD_DATA_ROOT", "/var/lib/agentd"),
            runtime_dir: path("AGENTD_RUNTIME_DIR", "/run/agentd"),
            engine_path: with_rust_bin(
                get("AGENTD_ENGINE_PATH")
                    .unwrap_or_else(|| "/opt/harness/bin:/usr/local/bin:/usr/bin:/bin".to_string())
                    .split(':')
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect(),
                get("AGENTD_RUST_BIN"),
            ),
            agentd_exe: path("AGENTD_EXE", "/opt/harness/bin/agentd"),
            engine_user: match get("AGENTD_ENGINE_USER") {
                Some(u) if u.is_empty() => None,
                Some(u) => Some(u),
                None => Some("agent".to_string()),
            },
            engine_sandbox: crate::sandbox::Setting::from_value(
                get(crate::sandbox::ENV_VAR).as_deref(),
            ),
            engine_identity: crate::sandbox::IdentitySetting::from_value(
                get(crate::sandbox::IDENTITY_VAR).as_deref(),
            ),
            identity_helper: path("AGENTD_NOBODY_EXEC", "/opt/harness/bin/ham-nobody-exec"),
            s3_endpoint: get("AGENTD_S3_ENDPOINT").filter(|s| !s.is_empty()),
            packages: PackagesIndex::from_vars(&get),
            internal_api_endpoint: get("AGENTD_INTERNAL_API_ENDPOINT").filter(|s| !s.is_empty()),
            goproxy: get("AGENTD_GOPROXY").filter(|s| !s.trim().is_empty()),
            direct: DirectConfig::from_vars(&get),
        }
    }

    /// From the process environment; `AGENTD_EXE` defaults to this executable.
    pub fn from_env() -> Self {
        let mut cfg = Self::from_vars(|k| std::env::var(k).ok());
        if std::env::var_os("AGENTD_EXE").is_none()
            // nosemgrep: current-exe -- AGENTD_EXE defaults to this executable path when unset. The path only goes into the credential_process line and is never trusted for a decision.
            && let Ok(exe) = std::env::current_exe()
        {
            cfg.agentd_exe = exe;
        }
        cfg
    }
}
