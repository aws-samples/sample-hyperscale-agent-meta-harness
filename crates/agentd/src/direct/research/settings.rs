//! The settings of the research module. Every one is an `AGENTD_RESEARCH_*` environment variable
//! (`config.rs` is not edited, `src/direct/AGENTS.md`); a value that cannot be used keeps the default, so a typo never switches the
//! module off or removes a cap. The numbers are the abuse and storage bounds of the design, not budgets (spend is pre-approved).

use harness_protocol::direct::{
    LIMIT_RESEARCH_FILE_BYTES, LIMIT_RESEARCH_RUN_BYTES, LIMIT_RESEARCH_UNPACKED_BYTES, LimitRange,
    RunLimits,
};
use std::str::FromStr;
use std::time::Duration;

/// `AGENTD_RESEARCH_MODE`: `off`, `false`, `0` or `no` (any case) switches the module off for the run; anything else leaves it on.
/// The run's own switch (`ham run --no-research`, the start request's research member) is read by the module itself; this one is
/// for the image and for tests.
pub const ENV_MODE: &str = "AGENTD_RESEARCH_MODE";

/// What the module needs to know that is not in the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    /// The loopback port of the API. 3129 by default (never 3128, the package proxy's); when it is taken the module asks for any free
    /// port. 0 asks for any free port at once.
    pub port: u16,
    /// A stored page is at most this many bytes (512 KiB).
    pub max_page_bytes: u64,
    /// A stored repository archive is at most this many bytes, compressed (64 MiB).
    pub max_archive_bytes: u64,
    /// A repository unpacks to at most this many bytes (200 MiB).
    pub max_unpacked_bytes: u64,
    /// A repository holds at most this many files (20,000).
    pub max_files: u32,
    /// One unpacked file is at most this many bytes (1 MiB).
    pub max_file_bytes: u64,
    /// All the research material of a run is at most this many bytes (300 MiB).
    pub max_run_bytes: u64,
    /// Free space that must stay on the disk after an unpack (256 MiB).
    pub min_free_bytes: u64,
    /// How long one call may block a caller before it answers "pending" (90 s).
    pub wait: Duration,
    /// How often a pending repository job is asked about (1 s).
    pub poll_every: Duration,
    /// How many times a call to the route is tried again after a transport error, a 5xx or a throttle (3).
    pub retries: u32,
    /// The pause before the first try again; it doubles (500 ms).
    pub retry_pause: Duration,
    /// How many calls the API serves at once; the next gets `limit_rate` (8).
    pub max_inflight: usize,
    /// How long one call to the route may take (120 s).
    pub call_timeout: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 3129,
            max_page_bytes: 512 * 1024,
            max_archive_bytes: 64 * 1024 * 1024,
            max_unpacked_bytes: 200 * 1024 * 1024,
            max_files: 20_000,
            max_file_bytes: 1024 * 1024,
            max_run_bytes: 300 * 1024 * 1024,
            min_free_bytes: 256 * 1024 * 1024,
            wait: Duration::from_secs(90),
            poll_every: Duration::from_secs(1),
            retries: 3,
            retry_pause: Duration::from_millis(500),
            max_inflight: 8,
            call_timeout: Duration::from_secs(120),
        }
    }
}

fn number<T: FromStr>(get: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<T> {
    get(name)?.trim().parse().ok()
}

/// A number that must be above zero to mean anything (a cap of zero is a typo, not a cap).
fn positive<T: FromStr + PartialOrd + Default>(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
) -> Option<T> {
    number::<T>(get, name).filter(|v| *v > T::default())
}

impl Settings {
    /// These settings with the research budgets of the request's `limits` member laid over them: `researchFileBytes`,
    /// `researchUnpackedBytes` and `researchRunBytes` replace the stage's three byte limits for this run; a member left out keeps the stage's. The
    /// request was checked by `harness_protocol::direct::check_limits`; each value is cut to its bounds again here, so a number that was not
    /// checked cannot get past them. The free-space rule ([`Settings::min_free_bytes`]) is not touched.
    pub fn with_limits(&self, l: &RunLimits) -> Self {
        let mut s = self.clone();
        let cut = |n: u64, r: LimitRange| n.clamp(r.min, r.max);
        if let Some(n) = l.research_file_bytes {
            s.max_file_bytes = cut(n, LIMIT_RESEARCH_FILE_BYTES);
        }
        if let Some(n) = l.research_unpacked_bytes {
            s.max_unpacked_bytes = cut(n, LIMIT_RESEARCH_UNPACKED_BYTES);
        }
        if let Some(n) = l.research_run_bytes {
            s.max_run_bytes = cut(n, LIMIT_RESEARCH_RUN_BYTES);
        }
        s
    }

    /// The settings of the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(&|k| std::env::var(k).ok())
    }

    /// The settings `get` names (a test passes a map).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        let wait = number::<u64>(get, "AGENTD_RESEARCH_WAIT_MS")
            .map(Duration::from_millis)
            .or_else(|| number::<u64>(get, "AGENTD_RESEARCH_WAIT_S").map(Duration::from_secs))
            .unwrap_or(d.wait);
        Self {
            enabled: !get(ENV_MODE).is_some_and(|m| {
                matches!(
                    m.trim().to_ascii_lowercase().as_str(),
                    "off" | "false" | "0" | "no"
                )
            }),
            port: number(get, "AGENTD_RESEARCH_PORT").unwrap_or(d.port),
            max_page_bytes: positive(get, "AGENTD_RESEARCH_MAX_PAGE_BYTES")
                .unwrap_or(d.max_page_bytes),
            max_archive_bytes: positive(get, "AGENTD_RESEARCH_MAX_ARCHIVE_BYTES")
                .unwrap_or(d.max_archive_bytes),
            max_unpacked_bytes: positive(get, "AGENTD_RESEARCH_MAX_UNPACKED_BYTES")
                .unwrap_or(d.max_unpacked_bytes),
            max_files: positive(get, "AGENTD_RESEARCH_MAX_FILES").unwrap_or(d.max_files),
            max_file_bytes: positive(get, "AGENTD_RESEARCH_MAX_FILE_BYTES")
                .unwrap_or(d.max_file_bytes),
            max_run_bytes: positive(get, "AGENTD_RESEARCH_MAX_RUN_BYTES")
                .unwrap_or(d.max_run_bytes),
            min_free_bytes: number(get, "AGENTD_RESEARCH_MIN_FREE_BYTES")
                .unwrap_or(d.min_free_bytes),
            wait,
            poll_every: positive::<u64>(get, "AGENTD_RESEARCH_POLL_MS")
                .map_or(d.poll_every, Duration::from_millis),
            retries: number(get, "AGENTD_RESEARCH_RETRIES").unwrap_or(d.retries),
            retry_pause: positive::<u64>(get, "AGENTD_RESEARCH_RETRY_PAUSE_MS")
                .map_or(d.retry_pause, Duration::from_millis),
            max_inflight: positive(get, "AGENTD_RESEARCH_MAX_INFLIGHT").unwrap_or(d.max_inflight),
            call_timeout: positive::<u64>(get, "AGENTD_RESEARCH_CALL_TIMEOUT_S")
                .map_or(d.call_timeout, Duration::from_secs),
        }
    }
}
