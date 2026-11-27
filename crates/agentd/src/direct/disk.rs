//! The session's two disks.
//!
//! An AgentCore session has `/` (the root volume: the image layers, `/tmp`, `/var/lib/agentd`, the home folder; a few GiB free at the start)
//! and `/mnt/workspace` (managed session storage with the checkout: a 1 GiB hard limit that cannot be raised, and `df` and
//! `statvfs` report 0 used on it, so neither the model nor the harness sees the real use). When the mount fills, the
//! engine answers error -32603 on every turn, and route failover takes that for a provider fault: it cycles all model routes
//! and sends prompts with no new tool call. The routes are healthy.
//!
//! This module holds what is needed to tell the two cases apart without a model:
//!
//! * [`Reading`], what was measured, and [`DiskProbe`], who measures it: [`HostDisk`] reads the free space of the root volume with
//!   `statvfs` and measures the checkout mount by walking it (allocated blocks, a hard link once, no symlink followed, bounded by an entry
//!   count and a time, a walk cut short is a lower bound).
//! * [`Limits`], the two numbers a deployment can change (`AGENTD_DISK_LOW_MB`, `AGENTD_WORKSPACE_LIMIT_MB`; 0 switches a check off), and
//!   the rules built on them: [`Limits::full`] (what stops the run), [`Limits::root_low`] and [`Limits::workspace_high`] (what warns early).
//!   A number that was not measured is never read as full.
//! * [`DiskFull`], the cause with its numbers and the words that name it: the stop detail (which makes the run's label say the harness ended
//!   the run and why), the notice and the question for the user.
//! * [`DiskWatch`], one notice per crossing of a warning mark.
//!
//! The retry loop measures after a -32603 and at each prompt boundary (`crate::direct::retry`). Nothing here deletes anything.
//!
//! `statvfs` of the root volume can show room while the file system no longer takes a write (`Read-only file
//! system (os error 30)` with hundreds of MiB reported free), and the engine's bare `Internal error` then names nothing. The measurement
//! therefore also makes a test write ([`Reading::writable`]); a folder that refuses it is the cause by itself ([`DiskFull::unwritable`]).
//! [`DiskProbe::diagnose`] says what the machine reports for the log, and [`Limits::engine_fault`] is the cause of a run that ended because
//! the engine failed on every route while every route answered a test request (`crate::direct::local_fault`).
//!
//! One measurement of [`HostDisk`] has a time limit of its own ([`MEASURE_LIMIT`]): a file system that does not answer no longer
//! holds a prompt boundary or the check after an engine error, and what was not measured in time is never read as full. The test write is
//! made only in a folder that is the worker's own ([`test_write_with`]: a real folder of the worker's user that neither group nor others
//! may write, opened once without following a link at the last part of its path), under a name no other write gets ([`probe_name`]),
//! and the folders of a run that get
//! it are the run's own on both disks ([`run_write_dirs`]). Any other folder gets no test write and says nothing about the disk.
//!
//! The session's storage can refuse writes with "no space left on device" while its files are far below its limit. A stop whose
//! numbers show room therefore does not call the disk full ([`DiskFull::no_space_with_room`]): it says that the engine reported no space,
//! or that the worker's own write to the storage was refused (`crate::direct::storage_watch`), with the size of the files. The stop
//! decision is the one it was. [`names_no_space`] is how a tool result is read for the words, and [`DiskProbe::storage_used`] is the walk
//! alone, for the notice of a refusal.

use crate::BoxFuture;
use crate::logging::LogSink;
use harness_protocol::direct::HARNESS_STOP_PREFIX;
use std::collections::HashSet;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Free space on the root volume below which it counts as short of room (`AGENTD_DISK_LOW_MB`).
pub const DEFAULT_LOW_MB: u64 = 512;
/// The managed session storage holds this much, and it is not adjustable (Quotas for Amazon Bedrock AgentCore, "Session storage limits").
pub const MOUNT_QUOTA_MB: u64 = 1024;
/// Kept below the quota: the mount is called full a little before its last byte.
pub const WORKSPACE_MARGIN_MB: u64 = 64;
/// Use of the checkout mount at which it counts as full (`AGENTD_WORKSPACE_LIMIT_MB`).
pub const DEFAULT_WORKSPACE_LIMIT_MB: u64 = MOUNT_QUOTA_MB - WORKSPACE_MARGIN_MB;
/// The early notice for the workspace comes at this share of its limit.
pub const WARN_PERCENT: u64 = 90;
/// The largest value a size setting takes (16 TiB): a bigger number is this one.
pub const MAX_MB: u64 = 16 * 1024 * 1024;

/// A size setting in MiB: a whole number from 0 (0 switches that check off), above [`MAX_MB`] it is [`MAX_MB`]; unset, blank or anything
/// that is not a whole number is `default`.
pub fn parse_mb(value: Option<&str>, default: u64) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(default, |n| n.min(MAX_MB))
}

/// What was measured. A member that is `None` was not measured, and an unmeasured number is never read as full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    /// Free space of the root volume in MiB.
    pub root_free_mb: Option<u64>,
    /// Use of the checkout mount in MiB, from a walk of the folder.
    pub workspace_used_mb: Option<u64>,
    /// The walk was cut short (entries or time), so `workspace_used_mb` is a lower bound.
    pub workspace_partial: bool,
    /// A test write (a 4 KiB file created, synced and removed) in each folder of the worker's own that the measurement was given:
    /// `Some(true)` when every folder that was tried took it, `Some(false)` when one refused it (a read-only or full disk that `statvfs`
    /// still shows room on), `None` when no folder was tried.
    pub writable: Option<bool>,
}

/// Who measures the two disks. The real one is [`HostDisk`]; the tests give a scripted one.
pub trait DiskProbe: Send + Sync + std::fmt::Debug {
    fn measure(&self) -> BoxFuture<'_, Reading>;

    /// What the machine says about its disks, for the log at the first engine error that names no cause: the result of each test write, the
    /// space and inodes of the folders' file systems with their read-only flag, the mounts that hold them with their options, and the tail
    /// of the kernel log. Best effort, bounded, nothing secret. Empty when the probe has nothing to add.
    fn diagnose(&self) -> BoxFuture<'_, String> {
        Box::pin(async { String::new() })
    }

    /// The size of the files on the session's storage in MiB, from the walk alone (no test write, no look at the root volume), and
    /// whether the walk was cut short (the number is then a lower bound). `(None, false)` when it was not measured, which is what a probe
    /// that has no walk answers.
    fn storage_used(&self) -> BoxFuture<'_, (Option<u64>, bool)> {
        Box::pin(async { (None, false) })
    }
}

/// The two numbers a deployment can change. 0 switches that check off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The root volume is short of room below this much free space.
    pub root_low_mb: u64,
    /// The checkout mount is full at this much use.
    pub workspace_limit_mb: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            root_low_mb: DEFAULT_LOW_MB,
            workspace_limit_mb: DEFAULT_WORKSPACE_LIMIT_MB,
        }
    }
}

impl Limits {
    /// The root volume has less free space than the low mark.
    pub fn root_low(&self, r: &Reading) -> bool {
        self.root_low_mb > 0 && r.root_free_mb.is_some_and(|free| free < self.root_low_mb)
    }

    /// The checkout mount is at [`WARN_PERCENT`] of its limit or more.
    pub fn workspace_high(&self, r: &Reading) -> bool {
        self.workspace_limit_mb > 0
            && r.workspace_used_mb.is_some_and(|used| {
                used.saturating_mul(100) >= self.workspace_limit_mb.saturating_mul(WARN_PERCENT)
            })
    }

    /// The cause when either disk is short: the root volume below its low mark, or the checkout mount at its limit. `None` when both have
    /// room and when what is short was not measured.
    pub fn full(&self, r: &Reading) -> Option<DiskFull> {
        self.full_with(r, None)
    }

    /// [`Limits::full`] with what the engine's error text said: a text that names the disk makes it the cause by itself, whatever the numbers
    /// show, and the numbers stay in the words. A test write that failed ([`Reading::writable`]) is the same: the disk is the cause by itself.
    pub fn full_with(&self, r: &Reading, signal: Option<Signal>) -> Option<DiskFull> {
        let full = self.build(r, signal);
        (full.root_short || full.workspace_short || full.unwritable || signal.is_some())
            .then_some(full)
    }

    /// The cause of a run that ended because the engine failed on every route while every route answered a test request
    /// (`crate::direct::local_fault`). Always a cause: the numbers of the reading go in the words, and when they or a test write name the disk
    /// the words say the disk instead.
    pub fn engine_fault(&self, r: &Reading) -> DiskFull {
        DiskFull {
            engine_fault: true,
            ..self.build(r, None)
        }
    }

    fn build(&self, r: &Reading, signal: Option<Signal>) -> DiskFull {
        DiskFull {
            root_free_mb: r.root_free_mb,
            root_low_mb: self.root_low_mb,
            workspace_used_mb: r.workspace_used_mb,
            workspace_limit_mb: self.workspace_limit_mb,
            workspace_partial: r.workspace_partial,
            root_short: self.root_low(r),
            workspace_short: self.workspace_limit_mb > 0
                && r.workspace_used_mb
                    .is_some_and(|used| used >= self.workspace_limit_mb),
            signal,
            unwritable: r.writable == Some(false),
            engine_fault: false,
            storage_refused: false,
            engine: String::new(),
        }
    }

    /// The mark at which the files count as nearly filling the storage: [`WARN_PERCENT`] of the stop mark, in MiB (864 by
    /// default). With the workspace check switched off the default stop mark decides, so the mark never is 0.
    pub fn storage_warn_mb(&self) -> u64 {
        let limit = if self.workspace_limit_mb == 0 {
            DEFAULT_WORKSPACE_LIMIT_MB
        } else {
            self.workspace_limit_mb
        };
        limit.saturating_mul(WARN_PERCENT) / 100
    }
}

/// The session disk is full: which of the two is short, and the numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskFull {
    pub root_free_mb: Option<u64>,
    pub root_low_mb: u64,
    pub workspace_used_mb: Option<u64>,
    pub workspace_limit_mb: u64,
    pub workspace_partial: bool,
    pub root_short: bool,
    pub workspace_short: bool,
    /// What the engine's error text said, when it named the disk.
    pub signal: Option<Signal>,
    /// A test write to a folder of the run failed ([`Reading::writable`]): the file system does not take writes, whatever it reports free.
    pub unwritable: bool,
    /// The engine failed on every route while every route answered a test request: a fault of the machine, not of a provider.
    pub engine_fault: bool,
    /// The worker's own test write to the session's storage, made beside this measurement, got "no space left on device"
    /// (`crate::direct::storage_watch`). The caller sets it; it changes the words only when both numbers show room
    /// ([`Self::no_space_with_room`]), never the decision to stop.
    pub storage_refused: bool,
    /// The profile name of the engine that ran, for the question of every stop. The caller sets it; a cause that was given
    /// none names no engine (the question of a stop with room reads "The engine", the two others "the engine").
    pub engine: String,
}

impl DiskFull {
    /// The numbers, a test write or the engine's text name the disk (and not only the engine's failure on every route).
    fn disk_cause(&self) -> bool {
        self.root_short || self.workspace_short || self.unwritable || self.signal.is_some()
    }

    /// The engine failed everywhere and nothing names the disk.
    fn only_engine(&self) -> bool {
        self.engine_fault && !self.disk_cause() && !self.no_space_with_room()
    }

    /// Both numbers were measured, neither is short, and either the engine's text says no space left or the worker's own write
    /// to the session's storage was refused. The disk is then not full, and the words say what was seen: a refusal with room.
    pub fn no_space_with_room(&self) -> bool {
        self.root_free_mb.is_some()
            && self.workspace_used_mb.is_some()
            && !self.root_short
            && !self.workspace_short
            && (self.signal == Some(Signal::NoSpace) || self.storage_refused)
    }

    /// The engine's own text says no space left (the first of the two cases of a stop with room).
    fn engine_said_no_space(&self) -> bool {
        self.signal == Some(Signal::NoSpace)
    }

    /// `158 MiB of 1024 MiB, root 5343 MiB free`, the numbers of a stop with room: the size of the files against what the storage
    /// holds (not the stop mark), `at least` for a walk cut short.
    fn numbers_with_room(&self) -> String {
        let at_least = if self.workspace_partial {
            "at least "
        } else {
            ""
        };
        format!(
            "{at_least}{} MiB of {MOUNT_QUOTA_MB} MiB, root {} MiB free",
            self.workspace_used_mb.unwrap_or_default(),
            self.root_free_mb.unwrap_or_default()
        )
    }

    /// The read-only words: the engine's text said so, or a test write failed and no text said full.
    fn read_only(&self) -> bool {
        match self.signal {
            Some(Signal::ReadOnly) => true,
            Some(Signal::NoSpace) => false,
            None => self.unwritable,
        }
    }

    /// The title of the question: `The session disk is read-only or full` when the engine's text said read-only or a test write failed,
    /// `The engine failed on every route` when nothing names the disk, else `The session disk is full`.
    pub fn title(&self) -> &'static str {
        if self.no_space_with_room() {
            if self.engine_said_no_space() {
                TITLE_ENGINE_NO_SPACE
            } else {
                TITLE_STORAGE_REFUSED
            }
        } else if self.read_only() {
            TITLE_READ_ONLY
        } else if self.only_engine() {
            TITLE_ENGINE_FAULT
        } else {
            TITLE
        }
    }

    /// The cause, as the run's label and notices name it. A text or a test write that says read-only names both causes, because the
    /// measurement may show room.
    pub fn cause(&self) -> &'static str {
        if self.no_space_with_room() {
            if self.engine_said_no_space() {
                "the engine reported no space left on device"
            } else {
                "the session's storage refused a write"
            }
        } else if self.read_only() {
            "the session disk is read-only or full"
        } else if self.only_engine() {
            "the engine failed on every route while every route answered a test request"
        } else {
            "the session disk is full"
        }
    }

    /// The word of the `notify` event and of the question's reason: `engine_fault` when nothing names the disk, else `disk_full`.
    pub fn reason_word(&self) -> &'static str {
        if self.only_engine() {
            "engine_fault"
        } else {
            "disk_full"
        }
    }

    /// `The engine's error says: read-only file system. A test write to a folder of the run failed. ` for what named the disk, else nothing.
    fn says(&self) -> String {
        // The cause of a stop with room says it already.
        if self.no_space_with_room() {
            return String::new();
        }
        let mut out = self.signal.map_or_else(String::new, |signal| {
            format!("The engine's error says: {}. ", signal.word())
        });
        if self.unwritable {
            out.push_str("A test write to a folder of the run failed. ");
        }
        out
    }

    /// The last sentence of the stop and the notice: what more prompts will do.
    fn closing(&self) -> &'static str {
        if self.no_space_with_room() {
            "More prompts can fail the same way while the storage refuses writes. The refusal can end by itself."
        } else if self.only_engine() {
            "Another try on any route would fail the same way."
        } else {
            "More prompts would fail the same way until space is freed."
        }
    }

    /// The numbers: `workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free`. The mark is the stop mark, not the mount's
    /// capacity, which is larger. What was not measured is said so, a walk cut short reads `at least`, and a check that is switched off has
    /// no mark to name.
    pub fn numbers(&self) -> String {
        let workspace = match (self.workspace_used_mb, self.workspace_partial) {
            (None, _) => "workspace use unknown".to_string(),
            (Some(used), partial) => {
                let used = if partial {
                    format!("at least {used}")
                } else {
                    used.to_string()
                };
                if self.workspace_limit_mb == 0 {
                    format!("workspace {used} MiB")
                } else {
                    format!(
                        "workspace {used} MiB against a stop mark of {} MiB",
                        self.workspace_limit_mb
                    )
                }
            }
        };
        let root = self.root_free_mb.map_or_else(
            || "root free space unknown".to_string(),
            |free| format!("root {free} MiB free"),
        );
        format!("{workspace}, {root}")
    }

    /// The cause and its numbers on one line: `the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB
    /// free`. It is the error line of the question and the start of the stop detail.
    pub fn line(&self) -> String {
        // A stop with room names what was seen and the size of the files against the storage's own size.
        if self.no_space_with_room() {
            return if self.engine_said_no_space() {
                format!(
                    "{}, but the files add up to {}",
                    self.cause(),
                    self.numbers_with_room()
                )
            } else {
                format!(
                    "{} while its files add up to {}",
                    self.cause(),
                    self.numbers_with_room()
                )
            };
        }
        if self.only_engine() {
            return format!("{}, {}", self.cause(), self.numbers_short());
        }
        format!("{}, {}", self.cause(), self.numbers())
    }

    /// The numbers of a run that ended on the engine's failure alone, short enough for the label to carry them whole (the label cuts the
    /// reason at 120 characters): `workspace 40 MiB used, root 5700 MiB free`. What was not measured is said so.
    fn numbers_short(&self) -> String {
        let workspace = self.workspace_used_mb.map_or_else(
            || "workspace use unknown".to_string(),
            |used| {
                let at_least = if self.workspace_partial {
                    "at least "
                } else {
                    ""
                };
                format!("workspace {at_least}{used} MiB used")
            },
        );
        let root = self.root_free_mb.map_or_else(
            || "root free space unknown".to_string(),
            |free| format!("root {free} MiB free"),
        );
        format!("{workspace}, {root}")
    }

    /// The detail of the stop: it begins with [`HARNESS_STOP_PREFIX`], so the run's label says the harness ended the run and why (the first
    /// sentence is the reason).
    pub fn detail(&self) -> String {
        format!(
            "{HARNESS_STOP_PREFIX} {}. {}{}",
            self.line(),
            self.says(),
            self.closing()
        )
    }

    /// The notice of the run's events: the cause, the numbers and what the user can do.
    pub fn notice(&self) -> String {
        if self.no_space_with_room() {
            let own = if self.engine_said_no_space() && self.storage_refused {
                "ham's own write to the session's storage was refused too. "
            } else {
                ""
            };
            let line = self.line();
            return format!(
                "The {}. Prompts can fail while the storage refuses writes. {own}{}",
                line.trim_start_matches("the "),
                self.question()
            );
        }
        let fails = if self.only_engine() {
            "Prompts fail the same way on every route."
        } else {
            "Prompts fail until space is freed."
        };
        format!(
            "The {}. {fails} {}",
            self.line().trim_start_matches("the "),
            self.says() + &self.question()
        )
    }

    /// What the user can do, for the question of the card and the end of the notice.
    pub fn question(&self) -> String {
        if self.no_space_with_room() {
            crate::engine::direct_words::storage_refused_question(&self.engine)
        } else if self.only_engine() {
            crate::engine::direct_words::engine_fault_question(&self.engine)
        } else {
            crate::engine::direct_words::disk_full_question(&self.engine)
        }
    }
}

/// The title of the question a full disk raises.
pub const TITLE: &str = "The session disk is full";

/// The title of the question when the engine's text says the disk is read-only or a test write failed.
pub const TITLE_READ_ONLY: &str = "The session disk is read-only or full";

/// The title of the question when the engine failed on every route and nothing names the disk.
pub const TITLE_ENGINE_FAULT: &str = "The engine failed on every route";

/// The title of the question when the engine's text says no space left and the numbers show room.
pub const TITLE_ENGINE_NO_SPACE: &str = "The engine reported no space left on device";

/// The title of the question when the worker's own write to the session's storage was refused and the numbers show room.
pub const TITLE_STORAGE_REFUSED: &str = "The session's storage refused a write";

/// What the engine's own error text says about the disk: the managed session storage turns read-only once it is
/// full, so the engine's -32603 answers carried `Read-only file system (os error 30)` and not only `No space left on device`. The text
/// names the disk by itself; the measurement stays the second signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// `No space left on device`, `os error 28`, `ENOSPC`.
    NoSpace,
    /// `Read-only file system`, `os error 30`, `EROFS`.
    ReadOnly,
}

impl Signal {
    /// The words of the text, as the notices quote them.
    pub fn word(self) -> &'static str {
        match self {
            Self::NoSpace => "no space left on device",
            Self::ReadOnly => "read-only file system",
        }
    }
}

/// True when `needle` stands in `haystack` as a whole word: no letter, digit or underscore touches either end (`os error 28` is not in `os
/// error 280`, and `erofs` is not in `erofs_utils`).
fn has_word(haystack: &str, needle: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    haystack.match_indices(needle).any(|(at, _)| {
        haystack[..at].chars().next_back().is_none_or(|c| !word(c))
            && haystack[at + needle.len()..]
                .chars()
                .next()
                .is_none_or(|c| !word(c))
    })
}

/// What `text` says about the disk, in any case: no space left (`No space left on device`, `ENOSPC`, `os error 28`) or a read-only file
/// system (`Read-only file system`, `EROFS`, `os error 30`). A text that names both is called full, because a mount that is read-only
/// because it is full is full.
pub fn signal_of(text: &str) -> Option<Signal> {
    let t = text.to_lowercase();
    if t.contains("no space left on device")
        || has_word(&t, "enospc")
        || has_word(&t, "os error 28")
    {
        return Some(Signal::NoSpace);
    }
    (t.contains("read-only file system") || has_word(&t, "erofs") || has_word(&t, "os error 30"))
        .then_some(Signal::ReadOnly)
}

/// How many texts [`names_no_space`] has read in this process.
static NO_SPACE_SEARCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many texts this process has searched with [`names_no_space`].
pub fn no_space_searches() -> u64 {
    NO_SPACE_SEARCHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether `text` names that no space is left on a device (the words, `ENOSPC`, `os error 28`): how the result of a tool call is
/// read. A text that only quotes the words reads the same, which is why the words alone never bring a notice: they make the worker test
/// the storage itself (`crate::direct::storage_watch`).
pub fn names_no_space(text: &str) -> bool {
    NO_SPACE_SEARCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The answer of `signal_of(text) == Some(Signal::NoSpace)` with one ASCII-lowered copy and three searches, and without the
    // search for the read-only words: this runs on every final tool result, on up to 256 KiB. The needles are ASCII and hold no
    // letter that a character outside ASCII lowers to, so the ASCII-lowered text finds what the fully lowered text finds.
    let t = text.to_ascii_lowercase();
    t.contains("no space left on device") || has_word(&t, "enospc") || has_word(&t, "os error 28")
}

/// [`signal_of`] for the message and the data of the engine's -32603; no other error is read (another code is another kind of failure).
pub fn signal_of_error(err: &crate::acp::AcpError) -> Option<Signal> {
    let crate::acp::AcpError::Rpc {
        code: -32603,
        message,
        data,
    } = err
    else {
        return None;
    };
    let mut text = message.clone();
    if let Some(data) = data {
        text.push(' ');
        text.push_str(&data.to_string());
    }
    signal_of(&text)
}

/// The probe and the limits a run measures with.
#[derive(Clone, Debug)]
pub struct DiskCheck {
    pub probe: Arc<dyn DiskProbe>,
    pub limits: Limits,
}

impl DiskCheck {
    pub fn new(probe: Arc<dyn DiskProbe>, limits: Limits) -> Self {
        Self { probe, limits }
    }
}

/// Which warnings are on, so that each crossing of a mark is announced once: a new notice only after the number came back across the mark.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskWatch {
    root_low: bool,
    workspace_high: bool,
}

impl DiskWatch {
    /// The notices a new reading brings: one for each mark it crossed since the last reading that measured that disk. A disk the reading did
    /// not measure keeps its state.
    pub fn crossings(&mut self, limits: &Limits, r: &Reading) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(free) = r.root_free_mb {
            let low = limits.root_low(r);
            if low && !self.root_low {
                out.push(format!(
                    "The session's root volume is running low: {free} MiB free, below the {} MiB mark. Nothing is deleted.",
                    limits.root_low_mb
                ));
            }
            self.root_low = low;
        }
        if let Some(used) = r.workspace_used_mb {
            let high = limits.workspace_high(r);
            if high && !self.workspace_high {
                out.push(format!(
                    "The workspace folder is nearly full: {used} MiB used against a stop mark of {} MiB, over {WARN_PERCENT} percent of it. Nothing is deleted.",
                    limits.workspace_limit_mb
                ));
            }
            self.workspace_high = high;
        }
        out
    }
}

/// A session whose first reading of the workspace folder is above this many MiB says the note about its storage. It is the
/// largest restored size at which the measurement rounds saw no refused write on the session's storage (repositories of 60, 100 and
/// 200 MiB). The counts are in the user guide ("When the session's storage refuses writes"), not here. No setting changes the mark:
/// it moves, as a code change, when a measurement gives it new neighbours.
pub const NOTE_ABOVE_MB: u64 = 200;
/// The smallest restored size at which the measurement rounds saw a refused write on the session's storage (a repository of
/// 300 MiB; the larger measured sizes, 600 and 920 MiB, had refusals too). The note's first row names it beside [`NOTE_ABOVE_MB`].
pub const REFUSALS_FROM_MB: u64 = 300;
/// The reason of the note's events (`turn.progress`, kind `notify`), one event for each row.
pub const NOTE_REASON: &str = "disk_note";
/// The note's second row. It holds no number: the flag is named on every stage, with its condition in words.
pub const NOTE_COMPUTE_ROW: &str = "A new run with --compute instances, where the stage has such machines, keeps the workspace folder on the machine's own disk. That machine runs no containers.";

/// The note's first row for a first reading of `used` MiB; `at_least` for a walk that was cut short, whose number is a lower bound.
pub fn note_size_row(used: u64, at_least: bool) -> String {
    let bound = if at_least { "at least " } else { "" };
    format!(
        "The workspace folder holds {bound}{used} MiB. In measurements this storage at times refused writes from {REFUSALS_FROM_MB} MiB up and none up to {NOTE_ABOVE_MB} MiB. The run goes on."
    )
}

/// A sum of bytes in whole MiB, rounded down: how the walk's sum becomes the number of a [`Reading`].
pub fn whole_mib(bytes: u64) -> u64 {
    bytes >> 20
}

/// What a session's first reading says in the log (`direct turn disk reading: ...`): the size of the workspace folder, `at least`
/// for a walk that was cut short and `not measured` when it was not read. The root volume's free space is not in the line: it
/// differs between two runs of the same code, and the worker's golden streams hold the log line for line.
pub fn reading_words(r: &Reading) -> String {
    match (r.workspace_used_mb, r.workspace_partial) {
        (Some(used), true) => format!("workspace at least {used} MiB used"),
        (Some(used), false) => format!("workspace {used} MiB used"),
        (None, _) => "workspace not measured".to_string(),
    }
}

/// Whether the session's first reading was taken. The note about a large workspace folder is said with that reading and
/// never later: its mark comes from restored repositories, and the first reading of a session is the restored work before the
/// first prompt. The state lives beside [`DiskWatch`] for the worker's session, so a fresh conversation of the engine inside the
/// session says nothing again, and a later session of the same run, which restores the work again, says the note again.
#[derive(Clone, Copy, Debug, Default)]
pub struct FolderNote {
    read: bool,
}

impl FolderNote {
    /// `None` for every reading after the session's first. For the first one: the rows of the note, which are two when the workspace
    /// check is on (`workspace_limit_mb` above 0) and the folder was measured above [`NOTE_ABOVE_MB`], and none otherwise. A first
    /// reading that did not measure the folder brings no row, and no later reading does. It makes no reading of its own.
    pub fn first_reading(&mut self, limits: &Limits, r: &Reading) -> Option<Vec<String>> {
        if std::mem::replace(&mut self.read, true) {
            return None;
        }
        let rows = match r.workspace_used_mb {
            Some(used) if limits.workspace_limit_mb > 0 && used > NOTE_ABOVE_MB => vec![
                note_size_row(used, r.workspace_partial),
                NOTE_COMPUTE_ROW.to_string(),
            ],
            _ => Vec::new(),
        };
        Some(rows)
    }
}

/// How far a walk of the checkout may go before it reports what it has as a lower bound.
#[derive(Clone, Copy, Debug)]
pub struct WalkBudget {
    /// The most directory entries it reads.
    pub entries: usize,
    /// The longest it runs.
    pub time: Duration,
}

impl Default for WalkBudget {
    fn default() -> Self {
        Self {
            entries: 500_000,
            time: Duration::from_secs(5),
        }
    }
}

/// How long one measurement of [`HostDisk`], and the folders' part of its report, may take: the walk's own budget
/// ([`WalkBudget`], 5 s) and as long again for the read of the free space and the test writes. A measurement that has not answered by
/// then is given up: its reading says that nothing was measured, which is never read as full, and the run goes on as after a
/// measurement that failed. While that measurement is still out, the next one answers at once in the same way and starts no second
/// read. It is no setting; `crates/agentd/AGENTS.md` ("Session disks", part h) states it for maintainers.
pub const MEASURE_LIMIT: Duration = Duration::from_secs(10);

/// The real probe: `statvfs` of the root volume's folder (the data root, `/var/lib/agentd`), a walk of the checkout mount and a test write in
/// each folder it is given that is the worker's own: the data root, the workspace root and the folders added with
/// [`HostDisk::with_write_dirs`]. One measurement takes [`MEASURE_LIMIT`] at the most.
#[derive(Debug)]
pub struct HostDisk {
    root: PathBuf,
    workspace: PathBuf,
    budget: WalkBudget,
    write_dirs: Vec<PathBuf>,
    /// A measurement's read is out: started and not ended yet.
    out: Arc<AtomicBool>,
    /// Where a measurement that was given up at its limit says so, in one line ([`HostDisk::with_log`]).
    log: Option<Arc<dyn LogSink>>,
}

/// Says that the read of a measurement has ended, when the read ends or is given up before it began.
struct ReadOut(Arc<AtomicBool>);

impl Drop for ReadOut {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl HostDisk {
    pub fn new(root: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self::with_budget(root, workspace, WalkBudget::default())
    }

    pub fn with_budget(
        root: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        budget: WalkBudget,
    ) -> Self {
        let (root, workspace) = (root.into(), workspace.into());
        Self {
            write_dirs: vec![root.clone(), workspace.clone()],
            root,
            workspace,
            budget,
            out: Arc::new(AtomicBool::new(false)),
            log: None,
        }
    }

    /// The log of the worker. A measurement that is given up at [`MEASURE_LIMIT`] writes one line there, so that a maintainer can read
    /// that the disk was not measured; the measurements that answer at once while that read is still out write none.
    #[must_use]
    pub fn with_log(mut self, log: Arc<dyn LogSink>) -> Self {
        self.log = Some(log);
        self
    }

    /// More folders to make the test write in (a session gives [`run_write_dirs`]). A folder that does not exist or is not the worker's
    /// own is not tried, and one that is listed twice is tried once.
    #[must_use]
    pub fn with_write_dirs(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        for dir in dirs {
            if !self.write_dirs.contains(&dir) {
                self.write_dirs.push(dir);
            }
        }
        self
    }
}

impl DiskProbe for HostDisk {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        let (root, workspace, budget) = (self.root.clone(), self.workspace.clone(), self.budget);
        let write_dirs = self.write_dirs.clone();
        let out = self.out.clone();
        let log = self.log.clone();
        Box::pin(async move {
            // One read at a time. A read that was given up at the limit may still wait for its file system, and a second one
            // would wait for the same.
            if out.swap(true, Ordering::SeqCst) {
                return Reading::default();
            }
            let ended = ReadOut(out);
            // The walk reads many directory entries and the test write waits for the disk: not on an async worker.
            let read = tokio::task::spawn_blocking(move || {
                let _ended = ended;
                read(&root, &workspace, budget, &write_dirs)
            });
            // A file system that does not answer does not hold the caller longer than the limit. Nothing was measured then.
            match tokio::time::timeout(MEASURE_LIMIT, read).await {
                Ok(done) => done.unwrap_or_default(),
                Err(_) => {
                    if let Some(log) = &log {
                        log.line(&format!(
                            "direct turn disk not measured: the measurement gave no answer within {} s",
                            MEASURE_LIMIT.as_secs()
                        ));
                    }
                    Reading::default()
                }
            }
        })
    }

    fn storage_used(&self) -> BoxFuture<'_, (Option<u64>, bool)> {
        let (workspace, budget) = (self.workspace.clone(), self.budget);
        Box::pin(async move {
            // The walk reads many directory entries: not on an async worker.
            tokio::task::spawn_blocking(move || match used_bytes(&workspace, budget) {
                Some((bytes, complete)) => (Some(whole_mib(bytes)), !complete),
                None => (None, false),
            })
            .await
            .unwrap_or((None, false))
        })
    }

    fn diagnose(&self) -> BoxFuture<'_, String> {
        let (root, workspace) = (self.root.clone(), self.workspace.clone());
        let write_dirs = self.write_dirs.clone();
        Box::pin(async move {
            let report =
                tokio::task::spawn_blocking(move || host_report(&root, &workspace, &write_dirs));
            // The folders' part has the limit of a measurement, and the report says when it was given up.
            let mut text = match tokio::time::timeout(MEASURE_LIMIT, report).await {
                Ok(done) => done.unwrap_or_default(),
                Err(_) => format!(
                    "the folders gave no answer within {} s\n",
                    MEASURE_LIMIT.as_secs()
                ),
            };
            text.push_str(&dmesg_tail().await);
            bounded(text, DIAGNOSE_MAX_CHARS)
        })
    }
}

fn read(root: &Path, workspace: &Path, budget: WalkBudget, write_dirs: &[PathBuf]) -> Reading {
    let (workspace_used_mb, workspace_partial) = match used_bytes(workspace, budget) {
        Some((bytes, complete)) => (Some(whole_mib(bytes)), !complete),
        None => (None, false),
    };
    Reading {
        root_free_mb: free_mb(root),
        workspace_used_mb,
        workspace_partial,
        writable: writable(write_dirs),
    }
}

/// The size of the test write.
const PROBE_BYTES: usize = 4096;

/// The most the diagnostic text holds.
const DIAGNOSE_MAX_CHARS: usize = 4096;

/// The first part of the name of a test write's file; 32 hexadecimal digits follow ([`probe_name`]).
pub const PROBE_PREFIX: &str = ".agentd-write-probe-";

/// What a test write came to.
#[derive(Debug)]
pub enum TestWrite {
    /// The folder took the file: made, written, synced and removed.
    Took,
    /// The file system refused a step of the write (the create, the write, the sync or the remove), as a read-only or full one does.
    Refused(std::io::Error),
    /// No write was made, and nothing is said about the disk: the folder is not one the worker alone writes (a link or a file in its
    /// place, a path that names nothing, another user's folder, or one that group or others may write), no name could be made for the
    /// file, or the name was taken.
    NotTried(&'static str),
}

/// Where a test looks in at a test write ([`test_write_with`]). The product gives nothing to run there.
#[derive(Clone, Copy, Debug)]
pub enum WriteStep<'a> {
    /// The folder has passed the rule and the file is not touched yet.
    Checked,
    /// The file stands under this name: made, written and synced, and not removed yet.
    Made(&'a str),
}

/// A name for the file of one test write: [`PROBE_PREFIX`] and 128 bits of the system's random source as 32 hexadecimal digits. No
/// other write gets the name, in this run or in another, and it cannot be made ahead.
pub fn probe_name() -> std::io::Result<String> {
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut name = String::with_capacity(PROBE_PREFIX.len() + 32);
    name.push_str(PROBE_PREFIX);
    for b in bytes {
        name.push_str(&format!("{b:02x}"));
    }
    Ok(name)
}

/// The test write: a 4 KiB file made, synced and removed in `dir`, under a name of its own ([`probe_name`]). A read-only
/// or full file system refuses one of the steps, whatever `statvfs` reports free.
pub fn test_write_in(dir: &Path) -> TestWrite {
    match probe_name() {
        Ok(name) => test_write_with(dir, &name, &|_| {}),
        Err(_) => TestWrite::NotTried("no name could be made for the file"),
    }
}

/// [`test_write_in`] for a caller that only asks whether the folder took the write: an error for a write that was refused and for one
/// that was not tried.
pub fn test_write(dir: &Path) -> std::io::Result<()> {
    match test_write_in(dir) {
        TestWrite::Took => Ok(()),
        TestWrite::Refused(e) => Err(e),
        TestWrite::NotTried(why) => Err(std::io::Error::other(why)),
    }
}

/// The test write of [`test_write_in`] under the name `name`. The folder is opened once without following a link at the last part of
/// its path (a link earlier in the path is followed, and the rule holds for the folder that was opened, whatever its path names). It is checked
/// through that handle: a real folder of this process's user that neither group nor others may write, as the folder of the storage
/// watch's own write is (`crate::direct::storage_watch::test_write_with`). A folder that fails the rule is not written to, because
/// somebody else could then decide what the write meets. The file is made new through the handle, for this user alone and without
/// following a link, and it is removed through the handle, so the write stays in the folder that was checked whatever its path comes
/// to name afterwards. `look` runs at the two steps of [`WriteStep`]: the product gives nothing to run, and a test looks in there.
pub fn test_write_with(dir: &Path, name: &str, look: &dyn Fn(WriteStep<'_>)) -> TestWrite {
    use rustix::fs::{AtFlags, Mode, OFlags};
    use rustix::io::Errno;
    let refused = |e: Errno| TestWrite::Refused(std::io::Error::from(e));
    let folder = match rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(folder) => folder,
        // A link or a file in the folder's place.
        Err(e) if e == Errno::LOOP || e == Errno::NOTDIR => {
            return TestWrite::NotTried("the folder is not a folder");
        }
        Err(e) if e == Errno::NOENT => return TestWrite::NotTried("the folder does not exist"),
        Err(e) if e == Errno::ACCESS || e == Errno::PERM => {
            return TestWrite::NotTried("the folder is not the worker's own");
        }
        Err(e) => return refused(e),
    };
    let stat = match rustix::fs::fstat(&folder) {
        Ok(stat) => stat,
        Err(e) => return refused(e),
    };
    if stat.st_uid != rustix::process::geteuid().as_raw()
        || Mode::from_raw_mode(stat.st_mode).intersects(Mode::WGRP | Mode::WOTH)
    {
        return TestWrite::NotTried("the folder is not the worker's own");
    }
    look(WriteStep::Checked);
    // The file is made new in the folder that was checked, and no link is followed: a name that is taken is left as it is.
    let fd = match rustix::fs::openat(
        &folder,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) {
        Ok(fd) => fd,
        Err(e) if e == Errno::EXIST => return TestWrite::NotTried("the name of the file is taken"),
        Err(e) => return refused(e),
    };
    let mut f = std::fs::File::from(fd);
    let wrote = f.write_all(&[0u8; PROBE_BYTES]).and_then(|()| f.sync_all());
    if wrote.is_ok() {
        look(WriteStep::Made(name));
    }
    drop(f);
    let removed =
        rustix::fs::unlinkat(&folder, name, AtFlags::empty()).map_err(std::io::Error::from);
    // What the write met comes first: a refused write whose file could not be removed either is a refused write.
    match wrote.and(removed) {
        Ok(()) => TestWrite::Took,
        Err(e) => TestWrite::Refused(e),
    }
}

/// The folders of a run that get the test write besides the two roots: the run's own folder on the root volume (`agent_data`)
/// and the run's own folder on the session's storage (`<workspace root>/<run>`). The worker makes both and hands neither to the
/// engine's user. The folders the engine's user writes (its state folder, the parent of the checkout) are inside them, on the same two
/// file systems, and get no test write.
pub fn run_write_dirs(agent_data: &Path, workspace_root: &Path, run: &str) -> [PathBuf; 2] {
    [agent_data.to_path_buf(), workspace_root.join(run)]
}

/// Takes the write bits of group and others off a folder of [`run_write_dirs`]. The worker makes these folders with the mode
/// its umask leaves. Under a umask that leaves a write bit for the group the rule of [`test_write_with`] would pass the folder by, and
/// the disk rule would lose its test write there without a word. The folder is opened once without following a link at the last part
/// of its path, and only a real folder of this process's user is changed. No other bit of the mode moves, so nobody gains an access.
/// `true` when the mode was changed.
pub fn keep_run_dir_own(dir: &Path) -> bool {
    use rustix::fs::{Mode, OFlags};
    let Ok(folder) = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return false;
    };
    let Ok(stat) = rustix::fs::fstat(&folder) else {
        return false;
    };
    let mode = Mode::from_raw_mode(stat.st_mode);
    let open = Mode::WGRP | Mode::WOTH;
    if stat.st_uid != rustix::process::geteuid().as_raw() || !mode.intersects(open) {
        return false;
    }
    rustix::fs::fchmod(&folder, mode.difference(open)).is_ok()
}

/// `Some(false)` when a folder refuses the test write, `Some(true)` when every folder that was tried took it, `None` when no folder was
/// tried (none exists, or none is the worker's own).
fn writable(dirs: &[PathBuf]) -> Option<bool> {
    let mut tried = false;
    for dir in dirs {
        match test_write_in(dir) {
            TestWrite::Took => tried = true,
            TestWrite::Refused(_) => return Some(false),
            TestWrite::NotTried(_) => {}
        }
    }
    tried.then_some(true)
}

/// A text cut to at most `max` characters (control characters become spaces so that it stays on the lines it was given).
fn bounded(text: String, max: usize) -> String {
    text.chars()
        .map(|c| if c == '\n' || !c.is_control() { c } else { ' ' })
        .take(max)
        .collect()
}

/// The part of the diagnostic the process can read itself: each test write, the space and inodes of each folder's file system with its
/// read-only flag, and the mounts that hold them with their options.
fn host_report(root: &Path, workspace: &Path, write_dirs: &[PathBuf]) -> String {
    let mut out = String::new();
    for dir in write_dirs.iter().filter(|d| d.is_dir()) {
        match test_write_in(dir) {
            TestWrite::Took => out.push_str(&format!("write test {}: ok\n", dir.display())),
            TestWrite::Refused(e) => {
                out.push_str(&format!("write test {}: failed: {e}\n", dir.display()));
            }
            TestWrite::NotTried(why) => {
                out.push_str(&format!(
                    "write test {}: not tried ({why})\n",
                    dir.display()
                ));
            }
        }
    }
    let mut paths: Vec<&Path> = vec![Path::new("/"), root, workspace];
    paths.extend(write_dirs.iter().map(PathBuf::as_path));
    let mut seen = HashSet::new();
    for path in paths.into_iter().filter(|p| seen.insert(p.to_path_buf())) {
        if let Ok(s) = rustix::fs::statvfs(path) {
            let mib = |blocks: u64| blocks.saturating_mul(s.f_frsize) >> 20;
            out.push_str(&format!(
                "df {}: {} MiB, {} MiB free, inodes {} of {} free, {}\n",
                path.display(),
                mib(s.f_blocks),
                mib(s.f_bavail),
                s.f_ffree,
                s.f_files,
                if s.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY) {
                    "mounted read-only"
                } else {
                    "mounted read-write"
                }
            ));
        }
    }
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        let table: Vec<(&str, &str, &str)> = mounts
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let (_, point, kind, options) = (f.next()?, f.next()?, f.next()?, f.next()?);
                Some((point, kind, options))
            })
            .collect();
        let mut shown = HashSet::new();
        for dir in [Path::new("/"), root, workspace]
            .into_iter()
            .chain(write_dirs.iter().map(PathBuf::as_path))
        {
            let best = table
                .iter()
                .filter(|(point, _, _)| dir.starts_with(point))
                .max_by_key(|(point, _, _)| point.len());
            if let Some((point, kind, options)) = best
                && shown.insert(*point)
            {
                let flag = options.split(',').any(|o| o == "ro");
                out.push_str(&format!(
                    "mount {point} {kind} {}{}\n",
                    options.chars().take(160).collect::<String>(),
                    if flag { " (read-only)" } else { "" }
                ));
            }
        }
    }
    out
}

/// The last lines of the kernel log, if the process may read it: `dmesg` with a short wait, 12 lines of at most 200 characters.
async fn dmesg_tail() -> String {
    let run = tokio::process::Command::new("dmesg")
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(3), run).await {
        Ok(Ok(out)) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let lines: Vec<&str> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(12)..];
            let mut s = String::from("dmesg tail:\n");
            for l in tail {
                s.extend(l.chars().take(200));
                s.push('\n');
            }
            s
        }
        Ok(Ok(out)) => format!(
            "dmesg: not readable ({})\n",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("no reason given")
                .chars()
                .take(120)
                .collect::<String>()
        ),
        Ok(Err(e)) => format!("dmesg: not available ({e})\n"),
        Err(_) => "dmesg: no answer in 3 s\n".to_string(),
    }
}

/// Free space in MiB that an unprivileged writer can use on the filesystem of `path`.
pub fn free_mb(path: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(path).ok()?;
    Some(s.f_bavail.saturating_mul(s.f_frsize) >> 20)
}

/// The space `dir` holds: the allocated blocks of everything in it, each hard link once, no symlink followed, nothing on another
/// filesystem. The second member is false when the budget ended the walk, and the number is then a lower bound. `None` when `dir` is not a
/// readable directory.
fn used_bytes(dir: &Path, budget: WalkBudget) -> Option<(u64, bool)> {
    let top = std::fs::metadata(dir)
        .ok()
        .filter(std::fs::Metadata::is_dir)?;
    std::fs::read_dir(dir).ok()?;
    let began = Instant::now();
    let device = top.dev();
    let mut bytes = top.blocks().saturating_mul(512);
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut entries = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(folder) = stack.pop() {
        let Ok(listing) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in listing.flatten() {
            entries += 1;
            if entries > budget.entries
                || (entries.is_multiple_of(64) && began.elapsed() > budget.time)
            {
                return Some((bytes, false));
            }
            // The entry's own metadata: a link is described, not followed.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.dev() != device {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
                continue;
            }
            bytes = bytes.saturating_add(meta.blocks().saturating_mul(512));
        }
    }
    Some((bytes, true))
}
