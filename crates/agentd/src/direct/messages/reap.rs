//! The kill path of the module: every process an engine's tools started, found and stopped.
//!
//! A group kill of the engine does not reach them. A prompt sent after that would run beside them, and the end commit would race their
//! writes.
//!
//! Who belongs to the engine is read from `/proc`, by marks that do not depend on how the tree of processes looks:
//!
//! - **the run's tag**: agentd puts `AGENTD_ENGINE_TAG=<random>` in the engine's environment ([`TAG_ENV`]); every descendant inherits it,
//!   whatever session or parent it ended up with. A tool that scrubs its environment escapes this mark, which is why the others exist.
//! - **the engine's user**: in the microVM the engine runs as its own user (`AGENTD_ENGINE_USER`), and everything that user owns is the
//!   engine's. A run whose engine shares agentd's user (the Instances machine, a developer machine, a test) has no such mark.
//!
//! Where the engine shares agentd's user (no second user to sweep, the `landlock` mode), two marks stand in for it.
//! They are switched on by [`Reaper::with_sandbox_marks`] for a sandboxed run and for nothing else, because both read the
//! machine and not the run: the Instances machine hosts one run, and a test machine may host several.
//!
//! - **the launcher's seccomp filter**: `agentd sandbox-exec` installs a filter on the engine, and the kernel keeps it through `fork`, `setsid`, a
//!   double fork and an `exec` with a scrubbed environment; no process can take it off. `/proc/<pid>/status` counts the filters
//!   (`Seccomp_filters`, Linux 5.9 and later), so a process with more of them than agentd was started inside the sandbox, wherever it sits in the
//!   tree. agentd's own children (a `git`, a `kill`) have agentd's count.
//! - **the parent chain**: a child of a process of the family is in the family, so a tool that scrubbed its environment and has not been
//!   orphaned is found where the kernel reports no filter count.
//!
//! agentd does not make itself a subreaper for this: an orphan would become its child and stay a zombie after the kill, because agentd's
//! `wait` belongs to the runtime that started its children, and a `waitpid` of its own would take their exit status.
//!
//! A reap repeats scan and kill until a scan finds nothing, so a loop that forks while it is being killed is caught on the next round;
//! zombies are not killed (they are dead; their parent has not collected them). The kill itself is `kill -KILL <pids>`, an argument-list
//! subprocess like the group kill of the driver.
//!
//! The reap says when it cannot vouch for what it did ([`Report::unverified`]: `/proc` could not be read, so an empty family is not
//! known to be empty), only ends after two empty scans in a row, and keeps the engine by identity (the pid and start time agentd marked,
//! [`Reaper::mark_engine`]) and not by parentage: when agentd is the init of its machine, tools the engine left reparent to agentd too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The environment variable that marks a process as the engine's or a descendant of it.
pub const TAG_ENV: &str = "AGENTD_ENGINE_TAG";

/// One process of the engine's family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub state: char,
    /// `starttime` of `/proc/<pid>/stat` (clock ticks after boot): with the pid it tells a process from a later one that reused the pid.
    pub start: u64,
    /// The process carries the run's tag.
    pub tagged: bool,
    pub name: String,
    /// The seccomp filters the process has (`Seccomp_filters` of `/proc/<pid>/status`, Linux 5.9 and later); `None` where the kernel does not
    /// say.
    pub filters: Option<u32>,
}

/// What one reap did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Every pid a kill was sent to, in the order it was sent.
    pub killed: Vec<u32>,
    /// `<pid> <name>` of each of them, in the same order: what a log line says was stopped.
    pub killed_names: Vec<String>,
    pub rounds: u32,
    /// Pids of the family still running when the bound passed. Empty is the goal.
    pub remaining: Vec<u32>,
    /// `/proc` could not be read: an empty `remaining` then does not mean the family is gone.
    pub unverified: bool,
}

/// `name` as a log line may carry it: a plain printable ASCII character stays, every other (a newline, a tab, a control character, a character
/// outside ASCII, a backslash) is written as its escape (`\n`, `\u{e9}`). The name of a process is text the engine's tools choose, and the log is
/// read line by line.
fn loggable(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if (c.is_ascii_graphic() || c == ' ') && c != '\\' {
            out.push(c);
        } else {
            out.extend(c.escape_default());
        }
    }
    out
}

/// ` (<pid> <name>,...)` for the first processes a reap stopped, so a log line says what the engine left; nothing for none. A name is
/// written with [`loggable`], so no name can end the line or start another.
pub fn killed_list(names: &[String]) -> String {
    const SHOWN: usize = 8;
    if names.is_empty() {
        return String::new();
    }
    let shown = names
        .iter()
        .take(SHOWN)
        .map(|n| loggable(n))
        .collect::<Vec<_>>()
        .join(", ");
    let more = names.len().saturating_sub(SHOWN);
    if more > 0 {
        format!(" ({shown}, and {more} more)")
    } else {
        format!(" ({shown})")
    }
}

/// The log line of a reap at the end of an engine: how many processes it stopped, which, and how many remain.
pub fn reaped_line(report: &Report) -> String {
    format!(
        "reaped {} process(es) the engine left{}, {} remain",
        report.killed.len(),
        killed_list(&report.killed_names),
        report.remaining.len()
    )
}

/// The user whose every process counts as the engine's: the engine's user when both it and agentd's own are known and differ. A sweep by
/// user that could include agentd itself (same user) or whose own user is unknown is not done.
pub fn sweep_uid(own: Option<u32>, engine: Option<u32>) -> Option<u32> {
    match (own, engine) {
        (Some(o), Some(e)) if o != e => Some(e),
        _ => None,
    }
}

/// Finds and stops the processes of one engine family.
#[derive(Clone, Debug)]
pub struct Reaper {
    root: PathBuf,
    self_pid: u32,
    tag: String,
    /// The engine's user, when it is not agentd's own.
    uid: Option<u32>,
    /// The engine agentd marked: its pid and the start time that tells it from a later process with the same pid.
    engine: Arc<Mutex<Option<(u32, u64)>>>,
    /// The run is sandboxed and shares agentd's user: the launcher's seccomp filter and the parent chain mark the family too.
    sandboxed: bool,
    /// Processes the reap never touches, with everything below them: `(pid, start time)` of the container runtime's front and
    /// supervisor. The start time tells the process from a later one that reused the pid.
    protected: Arc<Mutex<Vec<(u32, u64)>>>,
}

impl Reaper {
    /// The real `/proc`, for this process. `engine_uid` is the user the engine runs as; it is used only when it differs from agentd's
    /// own (the kill would otherwise take agentd and everything else of the same user).
    pub fn new(tag: &str, engine_uid: Option<u32>) -> Self {
        let own = own_uid(Path::new("/proc"));
        Self {
            root: PathBuf::from("/proc"),
            self_pid: std::process::id(),
            tag: tag.to_string(),
            uid: sweep_uid(own, engine_uid),
            engine: Arc::new(Mutex::new(None)),
            sandboxed: false,
            protected: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A reaper over another `/proc` tree (a test's), for `self_pid`; `uid` is taken as given.
    pub fn with_root(root: PathBuf, self_pid: u32, tag: &str, uid: Option<u32>) -> Self {
        Self {
            root,
            self_pid,
            tag: tag.to_string(),
            uid,
            engine: Arc::new(Mutex::new(None)),
            sandboxed: false,
            protected: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The reaper of one run: [`Self::new`], with the sandbox marks when the run is sandboxed (the `landlock` mode).
    pub fn for_run(tag: &str, engine_uid: Option<u32>, sandboxed: bool) -> Self {
        let reaper = Self::new(tag, engine_uid);
        if sandboxed {
            reaper.with_sandbox_marks()
        } else {
            reaper
        }
    }

    /// Also counts as the engine's what has more seccomp filters than agentd and what descends from the family (see the module text). For a run
    /// in the `landlock` mode, where the engine has no user of its own; it does nothing when there is a user to sweep.
    #[must_use]
    pub fn with_sandbox_marks(mut self) -> Self {
        self.sandboxed = true;
        self
    }

    /// Keeps the process `pid` and everything below it out of every scan and reap from now on. The start time is read now, so the
    /// mark names this process and no later one with the same pid. `false` when the process cannot be read: nothing is protected then.
    pub fn protect_pid(&self, pid: u32) -> bool {
        // Pid 0 is no process and pid 1 is the machine's init: neither is ever a runtime to protect. Agentd is outside every scan already, so a mark on
        // it would report a protection that does nothing.
        let Some(p) = (pid > 1 && pid != self.self_pid)
            .then(|| self.read(pid))
            .flatten()
        else {
            return false;
        };
        if p.state == 'Z' || p.state == 'X' {
            return false;
        }
        let mut marks = self.protected.lock().unwrap_or_else(|e| e.into_inner());
        if !marks.contains(&(p.pid, p.start)) {
            marks.push((p.pid, p.start));
        }
        true
    }

    /// The processes the reap leaves alone: every protected process that is still the one that was marked (same start time), and everything
    /// below it, to any depth. A protected process that is gone, or whose pid another process now holds, shields nothing: what was below it
    /// is the engine's again. A process of the namespace that Podman runs in is a descendant of the supervisor on the host, whatever
    /// user id it has inside, so the walk by parent needs no list of the runtime's programs.
    fn shielded(&self, all: &[Proc]) -> std::collections::BTreeSet<u32> {
        let marks: Vec<(u32, u64)> = self
            .protected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut shield: std::collections::BTreeSet<u32> = all
            .iter()
            .filter(|p| marks.contains(&(p.pid, p.start)))
            .map(|p| p.pid)
            .collect();
        if shield.is_empty() {
            return shield;
        }
        loop {
            let before = shield.len();
            for p in all {
                if shield.contains(&p.ppid) {
                    shield.insert(p.pid);
                }
            }
            if shield.len() == before {
                return shield;
            }
        }
    }

    /// Whether the filter count and the parent chain mark the family: a sandboxed run with no user of its own to sweep.
    pub fn uses_sandbox_marks(&self) -> bool {
        self.sandboxed && self.uid.is_none()
    }

    /// A tag no other run has: 128 bits from the process's random source, in hex.
    pub fn new_tag() -> String {
        crate::time::new_ulid().to_lowercase()
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The processes of the family now: tagged, or owned by the engine's user; never agentd, never a zombie. A `/proc` that cannot be
    /// read gives none, which is why [`Self::scan_checked`] exists.
    pub fn scan(&self) -> Vec<Proc> {
        self.scan_checked().unwrap_or_default()
    }

    /// As [`Self::scan`], and `None` when the directory of processes could not be read at all.
    pub fn scan_checked(&self) -> Option<Vec<Proc>> {
        let dir = std::fs::read_dir(&self.root).ok()?;
        let all: Vec<Proc> = dir
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| *pid != self.self_pid)
            .filter_map(|pid| self.read(pid))
            .filter(|p| p.state != 'Z' && p.state != 'X')
            .collect();
        // With a second user to sweep nothing else is needed (and nothing changes there). A sandboxed run without one has the filter count and the
        // parent chain in its place.
        let shares_user = self.uses_sandbox_marks();
        let own_filters = if shares_user {
            self.read(self.self_pid).and_then(|p| p.filters)
        } else {
            None
        };
        let marked = |p: &Proc| {
            p.tagged
                || self.uid == Some(p.uid)
                || matches!((p.filters, own_filters), (Some(n), Some(own)) if n > own)
        };
        let mut family: std::collections::BTreeSet<u32> =
            all.iter().filter(|p| marked(p)).map(|p| p.pid).collect();
        if shares_user {
            // Children of the family, to any depth (a process list is short, so the walk repeats until nothing is added).
            loop {
                let before = family.len();
                for p in &all {
                    if family.contains(&p.ppid) {
                        family.insert(p.pid);
                    }
                }
                if family.len() == before {
                    break;
                }
            }
        }
        // The container runtime: its front runs as the engine's user, and so does Podman's service, so the user sweep would stop them with
        // the engine and the report that follows would find a socket file with nobody behind it. They win over every mark, the run's tag included.
        for pid in self.shielded(&all) {
            family.remove(&pid);
        }
        let mut out: Vec<Proc> = all
            .into_iter()
            .filter(|p| family.contains(&p.pid))
            .collect();
        out.sort_by_key(|p| p.pid);
        Some(out)
    }

    /// Marks the engine: the family member agentd started, by pid and start time. Called when a session starts, so a restarted engine is
    /// the one marked. With several candidates (survivors of an earlier engine that agentd now parents) the newest is the engine.
    pub fn mark_engine(&self) {
        let candidates: Vec<Proc> = self
            .scan()
            .into_iter()
            .filter(|p| p.tagged && p.ppid == self.self_pid)
            .collect();
        let mut mark = self.engine.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((pid, start)) = *mark
            && candidates.iter().any(|p| p.pid == pid && p.start == start)
        {
            return;
        }
        if let Some(newest) = candidates.iter().max_by_key(|p| p.start) {
            *mark = Some((newest.pid, newest.start));
        }
    }

    /// Marks `pid` as the engine (its start time is read now).
    pub fn mark_engine_pid(&self, pid: u32) {
        if let Some(p) = self.read(pid) {
            *self.engine.lock().unwrap_or_else(|p| p.into_inner()) = Some((p.pid, p.start));
        }
    }

    /// Whether `p` is the engine itself: the marked process, or when none is marked a child of agentd.
    fn is_engine(&self, p: &Proc) -> bool {
        match *self.engine.lock().unwrap_or_else(|p| p.into_inner()) {
            Some((pid, start)) => p.pid == pid && p.start == start,
            None => p.ppid == self.self_pid,
        }
    }

    /// The engine itself: the member of the family agentd marked, else the members agentd started.
    pub fn engine_pids(&self) -> Vec<u32> {
        self.scan()
            .iter()
            .filter(|p| self.is_engine(p))
            .map(|p| p.pid)
            .collect()
    }

    fn read(&self, pid: u32) -> Option<Proc> {
        let dir = self.root.join(pid.to_string());
        let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
        // `pid (comm) state ppid ...`: the command name may hold spaces and parentheses, so the fields start after the last ')'.
        let (head, rest) = stat.rsplit_once(')')?;
        let name = head.split_once('(').map(|(_, n)| n.to_string())?;
        let mut f = rest.split_whitespace();
        let state = f.next()?.chars().next()?;
        let ppid: u32 = f.next()?.parse().ok()?;
        // after ppid: pgrp session tty tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime prio nice threads itreal starttime
        let start: u64 = f.nth(17)?.parse().ok()?;
        let status = std::fs::read_to_string(dir.join("status")).ok()?;
        let uid = uid_of(&status)?;
        let tagged = std::fs::read(dir.join("environ"))
            .map(|b| has_tag(&b, &self.tag))
            .unwrap_or(false);
        Some(Proc {
            pid,
            ppid,
            uid,
            state,
            start,
            tagged,
            name,
            filters: filters_of(&status),
        })
    }

    /// Whether `p` is still the process that was scanned (the pid was not reused).
    fn same(&self, p: &Proc) -> bool {
        self.read(p.pid)
            .is_some_and(|now| now.start == p.start && now.state != 'Z' && now.state != 'X')
    }

    /// Kills the family and waits until two scans in a row find none of it, for at most `bound`. With `keep_engine` the engine stays (a
    /// cancel the engine answered: its tools go, it does not). Blocking: the caller runs it where blocking is allowed.
    pub fn reap_blocking(&self, keep_engine: bool, bound: Duration) -> Report {
        let end = Instant::now() + bound;
        let mut report = Report::default();
        let mut empty_scans = 0;
        loop {
            let Some(all) = self.scan_checked() else {
                // Nothing can be said about a family whose directory cannot be read.
                report.unverified = true;
                return report;
            };
            let family: Vec<Proc> = all
                .into_iter()
                .filter(|p| !(keep_engine && self.is_engine(p)))
                .collect();
            if family.is_empty() {
                // A process started between the scan and the kill shows on the next one; two empty scans in a row end it.
                empty_scans += 1;
                if empty_scans >= 2 {
                    return report;
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            empty_scans = 0;
            if Instant::now() >= end {
                report.remaining = family.iter().map(|p| p.pid).collect();
                return report;
            }
            // The pid is checked against its start time again just before the kill, so a pid the kernel reused is not hit.
            let victims: Vec<&Proc> = family.iter().filter(|p| self.same(p)).collect();
            if !victims.is_empty() {
                let pids: Vec<u32> = victims.iter().map(|p| p.pid).collect();
                kill(&pids);
                report
                    .killed_names
                    .extend(victims.iter().map(|p| format!("{} {}", p.pid, p.name)));
                report.killed.extend(pids);
            }
            report.rounds += 1;
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

/// `kill -KILL <pids>`; a pid that is gone already is not an error.
fn kill(pids: &[u32]) {
    let mut cmd = Command::new("kill");
    cmd.arg("-KILL").arg("--");
    for p in pids {
        cmd.arg(p.to_string());
    }
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The user id of a process from the text of `/proc/<pid>/status` (the real one).
fn uid_of(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The number of seccomp filters of a process from the text of `/proc/<pid>/status` (`Seccomp_filters:`, Linux 5.9 and later).
fn filters_of(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Seccomp_filters:"))?
        .trim()
        .parse()
        .ok()
}

fn own_uid(root: &Path) -> Option<u32> {
    uid_of(&std::fs::read_to_string(root.join("self/status")).ok()?)
}

/// Whether the NUL-separated environment holds exactly `AGENTD_ENGINE_TAG=<tag>`.
fn has_tag(environ: &[u8], tag: &str) -> bool {
    let want = format!("{TAG_ENV}={tag}");
    environ.split(|b| *b == 0).any(|e| e == want.as_bytes())
}
