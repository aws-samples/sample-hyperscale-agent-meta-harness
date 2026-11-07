//! The open-files limit of agentd and of everything it starts.
//!
//! Many programs close every descriptor number from 3 up to the open-files limit before they run another program: old package tools,
//! `fakeroot`, some JVMs, pseudo-terminal libraries, any C code that loops `close(3..OPEN_MAX)`. A container runtime may hand a session a limit
//! of about a billion, and that loop then takes minutes for each program started, so a test suite that starts programs seems to hang. The
//! limit is a property the children inherit, so `agentd serve` lowers its own soft limit once, before it starts anything. The hard limit is
//! not touched: a program that needs more descriptors may raise its own soft limit again, up to the hard one.
//!
//! The stage variable `AGENTD_OPEN_FILES_CAP` names the cap (default 65,536, enough for any build or test run) or switches the step off
//! (`0`, `off`, `false`, `no`). The soft limit is only ever lowered: a limit at or below the cap, or one the cap would raise, stays as it is.
//! A call that fails is a log line and never a failure to start. [`cap_nofile`] is the pure decision, [`apply`] reads and sets the limit
//! through [`Limits`], and [`KernelLimits`] is this process's own.

use crate::logging::LogSink;

/// The stage variable.
pub const VAR: &str = "AGENTD_OPEN_FILES_CAP";
/// The cap when the variable is unset.
pub const DEFAULT_CAP: u64 = 65_536;
/// The smallest cap the variable may name.
pub const MIN_CAP: u64 = 256;
/// The largest cap the variable may name.
pub const MAX_CAP: u64 = 4_194_304;
/// A limit with no bound, as the pure function sees it.
pub const UNLIMITED: u64 = u64::MAX;

/// The longest part of a rejected value that goes into the log.
const SHOWN_CHARS: usize = 32;

/// What the stage variable says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    /// Leave the session's limit as it is.
    Off,
    /// Lower the soft limit to this when it is above it.
    Cap(u64),
    /// Not `off` and not a number in range: a config error, and the default cap applies. Holds the value as it will be logged.
    Invalid(String),
}

impl Setting {
    /// The setting for the variable's value (`None`: not set). Unset or blank is the default cap.
    pub fn from_value(value: Option<&str>) -> Self {
        let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Self::Cap(DEFAULT_CAP);
        };
        if matches!(
            raw.to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        ) {
            return Self::Off;
        }
        match raw.parse::<u64>() {
            Ok(n) if (MIN_CAP..=MAX_CAP).contains(&n) => Self::Cap(n),
            _ => Self::Invalid(shown(raw)),
        }
    }
}

/// A value for the log: printable characters only, cut at [`SHOWN_CHARS`].
fn shown(raw: &str) -> String {
    raw.chars()
        .take(SHOWN_CHARS)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The new soft limit, or `None` when the soft limit stays as it is. Only lowers: a soft limit at or below the cap is left alone, the new
/// value is the cap or the hard limit when that is lower, a cap of 0 changes nothing, and a result of 0 is never returned. `u64::MAX`
/// stands for "no limit".
pub fn cap_nofile(soft: u64, hard: u64, cap: u64) -> Option<u64> {
    if cap == 0 || soft <= cap {
        return None;
    }
    let new = cap.min(hard);
    (new > 0 && new < soft).then_some(new)
}

/// The two numbers of the open-files limit and the means to lower the soft one.
pub trait Limits {
    /// `(soft, hard)`; [`UNLIMITED`] for a limit with no bound.
    fn soft_hard(&self) -> (u64, u64);
    /// Sets the soft limit and passes the hard one back as it is.
    fn set_soft(&self, soft: u64, hard: u64) -> Result<(), String>;
}

/// The open-files limit of this process.
#[derive(Clone, Copy, Debug, Default)]
pub struct KernelLimits;

impl Limits for KernelLimits {
    fn soft_hard(&self) -> (u64, u64) {
        use rustix::process::{Resource, getrlimit};
        let l = getrlimit(Resource::Nofile);
        (
            l.current.unwrap_or(UNLIMITED),
            l.maximum.unwrap_or(UNLIMITED),
        )
    }

    fn set_soft(&self, soft: u64, hard: u64) -> Result<(), String> {
        use rustix::process::{Resource, Rlimit, setrlimit};
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(soft),
                maximum: (hard != UNLIMITED).then_some(hard),
            },
        )
        .map_err(|e| e.to_string())
    }
}

/// What [`apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The soft limit went from `from` to `to`.
    Lowered { from: u64, to: u64, hard: u64 },
    /// The soft limit is at or below the cap (or no new value was possible).
    Left { soft: u64, hard: u64 },
    /// The step is switched off.
    Off { soft: u64, hard: u64 },
    /// The call failed or the limit read back differently; the limit is as it was found, or as the kernel reports it now.
    Failed { soft: u64, hard: u64, why: String },
}

fn words(limit: u64) -> String {
    if limit == UNLIMITED {
        "unlimited".to_string()
    } else {
        limit.to_string()
    }
}

/// Applies the setting to `limits` and logs what it did: one line, and before it a config error line for a bad value. Never fails.
pub fn apply(setting: &Setting, limits: &dyn Limits, log: &dyn LogSink) -> Outcome {
    let (soft, hard) = limits.soft_hard();
    let cap = match setting {
        Setting::Off => {
            log.line(&format!(
                "open files limit left alone: {VAR} is off, soft {}, hard {}",
                words(soft),
                words(hard)
            ));
            return Outcome::Off { soft, hard };
        }
        Setting::Cap(n) => *n,
        Setting::Invalid(value) => {
            log.line(&format!(
                "config error: {VAR}={value:?} is not off and not a number from {MIN_CAP} to {MAX_CAP}; the default {DEFAULT_CAP} applies"
            ));
            DEFAULT_CAP
        }
    };
    let Some(new) = cap_nofile(soft, hard, cap) else {
        log.line(&format!(
            "open files limit left alone: soft {} is at or below the cap {cap}, hard {}",
            words(soft),
            words(hard)
        ));
        return Outcome::Left { soft, hard };
    };
    let failed = |why: String, now: (u64, u64)| {
        log.line(&format!(
            "open files limit not lowered: {why}, soft {}, hard {}",
            words(now.0),
            words(now.1)
        ));
        Outcome::Failed {
            soft: now.0,
            hard: now.1,
            why,
        }
    };
    if let Err(why) = limits.set_soft(new, hard) {
        return failed(format!("setrlimit failed: {why}"), (soft, hard));
    }
    let now = limits.soft_hard();
    if now.0 != new {
        return failed(
            format!("the soft limit reads {} after it was set", words(now.0)),
            now,
        );
    }
    log.line(&format!(
        "open files limit lowered: soft {} to {}, hard {}, cap {cap}",
        words(soft),
        words(new),
        words(hard)
    ));
    Outcome::Lowered {
        from: soft,
        to: new,
        hard,
    }
}
