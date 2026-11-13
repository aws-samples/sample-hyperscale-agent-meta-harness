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

use crate::BoxFuture;
use harness_protocol::direct::HARNESS_STOP_PREFIX;
use std::collections::HashSet;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// A test write (a 4 KiB file created, synced and removed) in each folder the run writes to: `Some(true)` when every folder took it,
    /// `Some(false)` when one did not (a read-only or full disk that `statvfs` still shows room on), `None` when no folder was tried.
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
        }
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
}

impl DiskFull {
    /// The numbers, a test write or the engine's text name the disk (and not only the engine's failure on every route).
    fn disk_cause(&self) -> bool {
        self.root_short || self.workspace_short || self.unwritable || self.signal.is_some()
    }

    /// The engine failed everywhere and nothing names the disk.
    fn only_engine(&self) -> bool {
        self.engine_fault && !self.disk_cause()
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
        if self.read_only() {
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
        if self.read_only() {
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
        if self.only_engine() {
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
        if self.only_engine() {
            crate::engine::direct_words::engine_fault_question().to_string()
        } else {
            crate::engine::direct_words::disk_full_question().to_string()
        }
    }
}

/// The title of the question a full disk raises.
pub const TITLE: &str = "The session disk is full";

/// The title of the question when the engine's text says the disk is read-only or a test write failed.
pub const TITLE_READ_ONLY: &str = "The session disk is read-only or full";

/// The title of the question when the engine failed on every route and nothing names the disk.
pub const TITLE_ENGINE_FAULT: &str = "The engine failed on every route";

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

/// The real probe: `statvfs` of the root volume's folder (the data root, `/var/lib/agentd`), a walk of the checkout mount and a test write in
/// each folder the run writes to: the data root, the workspace root and the folders added with [`HostDisk::with_write_dirs`].
#[derive(Debug)]
pub struct HostDisk {
    root: PathBuf,
    workspace: PathBuf,
    budget: WalkBudget,
    write_dirs: Vec<PathBuf>,
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
        }
    }

    /// More folders to make the test write in (the run's own folder, the checkout's parent, the engine's state folder). A folder that does
    /// not exist is not tried, and one that is listed twice is tried once.
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
        Box::pin(async move {
            // The walk reads many directory entries and the test write waits for the disk: not on an async worker.
            tokio::task::spawn_blocking(move || read(&root, &workspace, budget, &write_dirs))
                .await
                .unwrap_or_default()
        })
    }

    fn diagnose(&self) -> BoxFuture<'_, String> {
        let (root, workspace) = (self.root.clone(), self.workspace.clone());
        let write_dirs = self.write_dirs.clone();
        Box::pin(async move {
            let mut text =
                tokio::task::spawn_blocking(move || host_report(&root, &workspace, &write_dirs))
                    .await
                    .unwrap_or_default();
            text.push_str(&dmesg_tail().await);
            bounded(text, DIAGNOSE_MAX_CHARS)
        })
    }
}

fn read(root: &Path, workspace: &Path, budget: WalkBudget, write_dirs: &[PathBuf]) -> Reading {
    let (workspace_used_mb, workspace_partial) = match used_bytes(workspace, budget) {
        Some((bytes, complete)) => (Some(bytes >> 20), !complete),
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

/// The test write: a 4 KiB file created, synced and removed in `dir`. A read-only or full file system refuses one of the three, whatever
/// `statvfs` reports free.
pub fn test_write(dir: &Path) -> std::io::Result<()> {
    let file = dir.join(format!(".agentd-write-probe-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&file)?;
    let wrote = f.write_all(&[0u8; PROBE_BYTES]).and_then(|()| f.sync_all());
    drop(f);
    let removed = std::fs::remove_file(&file);
    wrote.and(removed)
}

/// `Some(false)` when a folder that exists refuses the test write, `Some(true)` when every existing folder took it, `None` when no folder
/// exists.
fn writable(dirs: &[PathBuf]) -> Option<bool> {
    let mut tried = false;
    for dir in dirs.iter().filter(|d| d.is_dir()) {
        tried = true;
        if test_write(dir).is_err() {
            return Some(false);
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
        match test_write(dir) {
            Ok(()) => out.push_str(&format!("write test {}: ok\n", dir.display())),
            Err(e) => out.push_str(&format!("write test {}: failed: {e}\n", dir.display())),
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
