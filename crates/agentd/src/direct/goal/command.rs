//! Evidence that a command is running. A model that starts the repository's full test run in the background can only wait for it with
//! tool calls (`sleep`, `tail`), and those calls are not polling in the sense of the brake inside a round. What tells the two apart is the
//! process table: processes of the engine's user that were not there when the session began, that used CPU since the last sample, and that
//! are older than the call that waits.
//!
//! The rules, each with the failure it answers:
//!
//! - **a baseline**: when a session begins, the engine's user's processes are listed (the engine, its helpers, a container runtime that is
//!   already up) with their sessions. Only a process that is not in the list, and not in a session of the list, can be a command, so the
//!   engine's own CPU, the CPU of a helper that lives as long as the engine, and what the engine runs without a session of its own never
//!   count. The list is taken again when a new session begins ([`CommandProbe::rebaseline`]).
//! - **per process, by start time**: a process is its pid and its start time, so a pid the kernel reused is another process. A process seen
//!   before counts by the change of its CPU ticks; a new one counts its ticks from zero; one that was born before the previous sample and is
//!   seen for the first time counts nothing (its files could not be read then); a process that left adds nothing and is never a negative
//!   number. The ticks of a process are its own (`utime + stime`) and those of the children it has waited for: a
//!   test run that starts a short-lived process per test keeps almost no CPU of its own, and the children are gone before any sample sees
//!   them, so their CPU is in the parent's counters. A child that lives across a sample and is waited for before the next is counted in its
//!   own counters and again in its parent's; a deeper tree counts a tick once more for each level (up to depth plus one in all). The rate below
//!   is therefore a rate of activity, not of CPU seconds, and it overstates a deep tree.
//! - **a rate, not a floor**: the interval is active when those changes add up to at least `cpu_pct` percent of one CPU over the time since the
//!   previous sample, rounded up (5 percent by default: 25 ticks in 5 seconds, 300 in a minute). A heartbeat of a daemon does not reach it; a
//!   compiler or a test run does.
//! - **a window**: a job does not use the CPU evenly. A test run waits for a timeout, a build is quiet while it links, a serial test
//!   run spends minutes in a test that waits for a network that is not there. Judged interval by interval, such a job read Idle in most of them
//!   and the time of those intervals counted against the brake (most of the samples of a busy test run). So a
//!   sample is also Active when the sessions that are still there used at least the same rate over the last `window` seconds
//!   ([`ProcProbe::with_window`]; the stage's `AGENTD_GOAL_COMMAND_WINDOW_S`, by default the longer of the user's stall window and 30 minutes): the interval answer or the
//!   window answer, whichever names the older session. The window starts at the first sample after the baseline (a young window is judged by
//!   the time it has), covers only sessions that have a process in the scan being judged (a command the model ran itself and that has ended
//!   lends nothing to the next call; a session missed by one scan keeps its history, one missed by two loses it), and is forgotten with the
//!   baseline. A job that works and then does nothing stays Active until its work leaves the window or the rate over the window falls below
//!   the threshold, then reads Idle: a job that hangs does not hold the run for ever.
//! - **by session**: the engine starts each command of the model in a session of its own, so a command the model runs itself is as old as its
//!   call, and a job it started with an earlier call is older than the polls that come after it. The CPU a job's children use belongs to the
//!   job's session whatever the age of the child (a script that starts a test run in step 7 is still the job of step 1). A session is as old
//!   as its oldest process that is still there.
//! - **four answers**: [`Command::Active`] with the age (below), [`Command::Idle`] and [`Command::Unknown`] (the first sample after a
//!   baseline has no interval; a process table or an uptime that cannot be read). Unknown never protects anything.
//! - **the age**: the sessions are sorted by age, oldest first, and their CPU is added until it reaches the rate. The age of the session where it
//!   does is the answer: the sessions at least that old used enough CPU together, and no younger session is needed. The watch compares it
//!   with the age of each call ([`watch`](crate::direct::goal::watch)): the CPU of a session younger than a call cannot be a wait of that call.
//! - **only the engine's user**: processes of other users, and zombies, are not looked at. A probe is refused for agentd's own user
//!   ([`ProcProbe::for_engine_user`]): it could not tell agentd's processes from the engine's. A run without a separate engine user (a developer
//!   machine, a test) has no evidence and the brake works as it did.
//! - **or the processes below the engine's**: in a Landlock sandbox the engine shares agentd's user, so the user cannot tell them apart, and the probe
//!   reads the process tree instead ([`ProcProbe::for_engine_family`]): the engine's process (its id from the driver control) and every process whose chain of parents reaches
//!   it. A process left to init by a parent that ended is not below the engine's any more and is not seen, the same blind spot as a program that changes session.
//!
//! What it cannot see, on purpose:
//!
//! - a command that waits (network, disk, a lock) and uses almost no CPU looks idle once its earlier work has left the window: a test run that
//!   waits for one timeout after another for longer than the window, with little work before it, is not told from a hung one;
//! - CPU that is in processes of another user (a container runtime) is not the engine user's;
//! - children that exit and are not waited for by a process of the engine's user (their parent left first) add their CPU to nobody the probe
//!   reads;
//! - a program that starts its workers with a session of its own (`setsid`) makes them as young as their start, and they are not seen as the
//!   job's;
//! - a job that was in the baseline (the engine started again while it ran) is not looked at.
//!
//! The evidence says a command used CPU, not that it will finish, that the call waits for it, or that its output is good: a CPU-bound job the
//! model started earlier protects a later call that has nothing to do with it, and the grace ([`watch`](crate::direct::goal::watch)) ends that.
//! The ages are read from `/proc/uptime` (the machine's clock, which counts a suspended machine), the calls from the supervisor's own clock
//! (which does not): a machine that was suspended between a call and the next sample can lend one interval to a foreground command.

use crate::direct::goal::watch::Command;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Clock ticks per second of the CPU counters and start times in `/proc/<pid>/stat` (`USER_HZ`: 100 on every Linux the worker image runs on).
pub const CLOCK_TICKS_PER_S: u64 = 100;

/// Where evidence comes from. The real one reads `/proc`; a test names its own.
pub trait CommandProbe: Send + Sync + std::fmt::Debug {
    /// Takes the engine user's processes of this moment as the baseline: a new session began, and everything running belongs to it. The next
    /// [`sample`](Self::sample) has no interval to judge and answers [`Command::Unknown`].
    fn rebaseline(&self);
    /// What the processes outside the baseline did since the previous sample. Reads files: call it where blocking is allowed.
    fn sample(&self, now: Instant) -> Command;
}

/// The fields of `/proc/<pid>/stat` the probe reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub state: char,
    /// The parent process (0 for the first process of a machine).
    pub ppid: u32,
    /// The session the process belongs to.
    pub session: u32,
    /// `starttime`: clock ticks after boot. With the pid it tells a process from a later one that reused the pid.
    pub start: u64,
    /// `utime + stime`: the CPU the process itself used.
    pub ticks: u64,
    /// `cutime + cstime`: the CPU of the children the process has waited for (they are gone; their own `stat` is too).
    pub waited: u64,
}

/// Reads one line of `/proc/<pid>/stat`: `pid (comm) state ppid ...`. The command name is printed raw by the kernel (cut at 15 bytes, so a
/// long non-ASCII name can end in half a character) and may hold spaces and parentheses, so the fields start after the last `)`. `None` for
/// a line that is too short or whose numbers do not parse.
pub fn parse_stat(text: impl AsRef<[u8]>) -> Option<Stat> {
    let bytes = text.as_ref();
    let end = bytes.iter().rposition(|b| *b == b')')?;
    // Everything after the name is ASCII (letters and numbers).
    let rest = std::str::from_utf8(&bytes[end + 1..]).ok()?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    // state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime prio nice threads itreal starttime
    if f.len() < 20 {
        return None;
    }
    let n = |i: usize| f[i].parse::<u64>().ok();
    Some(Stat {
        state: f[0].chars().next()?,
        ppid: u32::try_from(n(1)?).ok()?,
        session: u32::try_from(n(3)?).ok()?,
        start: n(19)?,
        ticks: n(11)?.saturating_add(n(12)?),
        waited: n(13)?.saturating_add(n(14)?),
    })
}

/// The real user id of a process from the bytes of `/proc/<pid>/status`: its `Name:` line is raw bytes too, so the file is read as lines of
/// bytes and only the `Uid:` line is read as text.
fn uid_of(status: impl AsRef<[u8]>) -> Option<u32> {
    status
        .as_ref()
        .split(|b| *b == b'\n')
        .find_map(|l| l.strip_prefix(b"Uid:"))
        .and_then(|l| std::str::from_utf8(l).ok())?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The machine's uptime in clock ticks, from the text of `/proc/uptime` (`12345.67 98765.43`).
fn uptime_ticks(text: &str) -> Option<u64> {
    let first = text.split_whitespace().next()?;
    let (whole, frac) = first.split_once('.').unwrap_or((first, ""));
    let mut cs = frac.chars().take(2).collect::<String>();
    while cs.len() < 2 {
        cs.push('0');
    }
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(CLOCK_TICKS_PER_S)?
        .checked_add(cs.parse::<u64>().ok()?)
}

/// A process: its pid and its start time.
type Key = (u32, u64);

/// One process of the engine's user at a sample.
#[derive(Clone, Copy, Debug)]
struct Proc {
    key: Key,
    session: u32,
    ticks: u64,
    waited: u64,
}

impl Proc {
    /// Everything the process and the children it waited for have used.
    fn total(&self) -> u64 {
        self.ticks.saturating_add(self.waited)
    }
}

/// What a sample read: the machine's uptime before the listing began, and the processes of the engine's user.
struct Scan {
    up: u64,
    procs: Vec<Proc>,
}

/// What was there when the session began.
#[derive(Debug, Default)]
struct Baseline {
    keys: BTreeSet<Key>,
    sessions: BTreeSet<u32>,
}

impl Baseline {
    fn of(procs: &[Proc]) -> Self {
        Self {
            keys: procs.iter().map(|p| p.key).collect(),
            sessions: procs.iter().map(|p| p.session).collect(),
        }
    }

    fn holds(&self, p: &Proc) -> bool {
        self.keys.contains(&p.key) || self.sessions.contains(&p.session)
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// The processes of the engine's user when the session began; `None` until the first sample or the first baseline.
    baseline: Option<Baseline>,
    /// The ticks of every process outside the baseline at the previous sample.
    prev: BTreeMap<Key, u64>,
    /// The time of the previous sample and the uptime when its listing began; `None` right after a baseline.
    at: Option<(Instant, u64)>,
    /// The time of the first sample after the baseline: where the window begins while it is younger than its length.
    since: Option<Instant>,
    /// What each session that is outside the baseline used in the samples of the window.
    tracks: BTreeMap<u32, Track>,
}

/// The most intervals kept for one session: a window of 4 hours sampled every 5 s holds 2,880. A probe that is asked more often than that
/// (a test) forgets the oldest.
const MAX_TRACK: usize = 4096;

/// What one session used in the samples of the window.
#[derive(Debug, Default)]
struct Track {
    /// The end of each sample interval and the ticks the session used in it, oldest first.
    used: VecDeque<(Instant, u64)>,
    /// How many scans in a row found no process of the session.
    missed: u8,
}

/// Which processes of the table are the engine's.
#[derive(Clone, Debug)]
enum Select {
    /// Every process of this user (the microVM: the engine has a user of its own).
    Uid(u32),
    /// The engine's process and every process below it (a Landlock sandbox: the engine shares agentd's user, so the user cannot tell them). The
    /// engine's process id is read at each sample; 0 is no engine yet.
    Below(Arc<AtomicU32>),
}

/// The process table of one engine: by its user, or by the process tree below it.
#[derive(Debug)]
pub struct ProcProbe {
    root: PathBuf,
    select: Select,
    cpu_pct: u64,
    /// How far back the window reaches; zero is no window (every interval alone).
    window: Duration,
    inner: Mutex<Inner>,
}

impl ProcProbe {
    /// The real `/proc` for the processes of `uid`, or `None` when `uid` is agentd's own user or agentd's user cannot be read.
    pub fn for_engine_user(uid: u32, cpu_pct: u32) -> Option<Self> {
        Self::for_engine_user_in(Path::new("/proc"), uid, cpu_pct)
    }

    /// As [`for_engine_user`](Self::for_engine_user) over another `/proc` tree (a test's) for the processes of `uid`. agentd's own user is
    /// always read from the real `/proc`.
    pub fn for_engine_user_in(root: &Path, uid: u32, cpu_pct: u32) -> Option<Self> {
        let own = uid_of(std::fs::read("/proc/self/status").ok()?)?;
        (own != uid).then(|| Self::with_root(root.to_path_buf(), uid, cpu_pct))
    }

    /// A probe over another `/proc` tree (a test's), for the processes of `uid`. `cpu_pct` is the share of one CPU, in percent, that makes an
    /// interval active (1 to 100).
    pub fn with_root(root: PathBuf, uid: u32, cpu_pct: u32) -> Self {
        Self::selecting(root, Select::Uid(uid), cpu_pct)
    }

    /// A probe over the processes below the engine's (the process whose id `engine_pid` holds), over the real `/proc`. It is for a run whose
    /// engine shares agentd's user, where [`for_engine_user`](Self::for_engine_user) is refused.
    pub fn for_engine_family(engine_pid: Arc<AtomicU32>, cpu_pct: u32) -> Self {
        Self::for_engine_family_in(Path::new("/proc"), engine_pid, cpu_pct)
    }

    /// As [`for_engine_family`](Self::for_engine_family) over another `/proc` tree (a test's). The tree is walked from each process up through
    /// the parents; a process that was left to init by a parent that ended is not below the engine any more, and is not looked at.
    pub fn for_engine_family_in(root: &Path, engine_pid: Arc<AtomicU32>, cpu_pct: u32) -> Self {
        Self::selecting(root.to_path_buf(), Select::Below(engine_pid), cpu_pct)
    }

    fn selecting(root: PathBuf, select: Select, cpu_pct: u32) -> Self {
        Self {
            root,
            select,
            cpu_pct: u64::from(cpu_pct.clamp(1, 100)),
            window: Duration::ZERO,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The probe with a window of `window`: a sample is also Active when the sessions that are still there used the rate over the last
    /// `window`. Zero is no window, which is what [`with_root`](Self::with_root) gives.
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// The ticks that `cpu_pct` percent of one CPU is over `span`, rounded up, at least one.
    fn needed(&self, span: Duration) -> u64 {
        let ms = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        ms.saturating_mul(CLOCK_TICKS_PER_S)
            .saturating_mul(self.cpu_pct)
            .div_ceil(100_000)
            .max(1)
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The uptime, then every process of the engine's user that is not a zombie, with its session and CPU ticks. The uptime is read first: a
    /// process born after it is in the listing. `None` when the uptime or the table cannot be read; a process that leaves between the listing
    /// and the read is skipped.
    fn scan(&self) -> Option<Scan> {
        let up = uptime_ticks(&std::fs::read_to_string(self.root.join("uptime")).ok()?)?;
        let entries = std::fs::read_dir(&self.root).ok()?;
        let pids: Vec<u32> = entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .collect();
        let procs = match &self.select {
            Select::Uid(_) => pids.iter().filter_map(|pid| self.read(*pid)).collect(),
            Select::Below(engine) => self.below(engine.load(Ordering::SeqCst), &pids),
        };
        Some(Scan { up, procs })
    }

    /// The engine's process and every non-zombie process whose chain of parents reaches it. The chain is followed at most a few hundred
    /// steps, so a table that changed while it was read cannot loop.
    fn below(&self, engine: u32, pids: &[u32]) -> Vec<Proc> {
        if engine == 0 {
            return Vec::new();
        }
        let table: BTreeMap<u32, Stat> = pids
            .iter()
            .filter_map(|pid| {
                let stat =
                    parse_stat(std::fs::read(self.root.join(pid.to_string()).join("stat")).ok()?)?;
                Some((*pid, stat))
            })
            .collect();
        let belongs = |mut pid: u32| {
            for _ in 0..512 {
                if pid == engine {
                    return true;
                }
                match table.get(&pid) {
                    Some(stat) if stat.ppid != 0 && stat.ppid != pid => pid = stat.ppid,
                    _ => return false,
                }
            }
            false
        };
        table
            .iter()
            .filter(|(pid, stat)| stat.state != 'Z' && stat.state != 'X' && belongs(**pid))
            .map(|(pid, stat)| Proc {
                key: (*pid, stat.start),
                session: stat.session,
                ticks: stat.ticks,
                waited: stat.waited,
            })
            .collect()
    }

    fn read(&self, pid: u32) -> Option<Proc> {
        let Select::Uid(want) = &self.select else {
            return None;
        };
        let dir = self.root.join(pid.to_string());
        let stat = parse_stat(std::fs::read(dir.join("stat")).ok()?)?;
        if stat.state == 'Z' || stat.state == 'X' {
            return None;
        }
        let uid = uid_of(std::fs::read(dir.join("status")).ok()?)?;
        (uid == *want).then_some(Proc {
            key: (pid, stat.start),
            session: stat.session,
            ticks: stat.ticks,
            waited: stat.waited,
        })
    }
}

impl CommandProbe for ProcProbe {
    fn rebaseline(&self) {
        let Some(scan) = self.scan() else { return };
        let mut g = self.inner();
        g.baseline = Some(Baseline::of(&scan.procs));
        g.prev.clear();
        g.at = None;
        g.since = None;
        g.tracks.clear();
    }

    fn sample(&self, now: Instant) -> Command {
        let Some(scan) = self.scan() else {
            return Command::Unknown;
        };
        let mut guard = self.inner();
        let g = &mut *guard;
        let Some(baseline) = g.baseline.take() else {
            g.baseline = Some(Baseline::of(&scan.procs));
            g.prev.clear();
            g.tracks.clear();
            g.at = Some((now, scan.up));
            g.since = Some(now);
            return Command::Unknown;
        };
        let before = g.at.replace((now, scan.up));
        if before.is_none() {
            g.since = Some(now);
        }
        let mut next = BTreeMap::new();
        // the processes outside the baseline, each with the CPU it and its waited-for children used in this interval
        let mut found: Vec<(Proc, u64)> = Vec::new();
        for p in scan.procs.iter().filter(|p| !baseline.holds(p)) {
            let used = match (g.prev.get(&p.key), before) {
                (Some(then), _) => p.total().saturating_sub(*then),
                // new: born since the previous sample, or seen for the first time though it is older, which counts nothing
                (None, Some((_, up))) if p.key.1 >= up => p.total(),
                (None, _) => 0,
            };
            next.insert(p.key, p.total());
            found.push((*p, used));
        }
        g.baseline = Some(baseline);
        g.prev = next;
        let Some((before, _)) = before else {
            return Command::Unknown;
        };
        let sessions = by_session(&found);
        let interval = youngest_needed(
            sessions.values().copied(),
            self.needed(now.saturating_duration_since(before)),
        );
        let windowed = self.windowed(g, now, &sessions);
        let start = match (interval, windowed) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match start {
            Some(start) => Command::Active {
                age: Duration::from_millis(
                    scan.up
                        .saturating_sub(start)
                        .saturating_mul(1000 / CLOCK_TICKS_PER_S),
                ),
            },
            None => Command::Idle,
        }
    }
}

impl ProcProbe {
    /// The window answer of a sample: the start of the session where the CPU the sessions that are there now used over the window reaches the
    /// rate over the window, oldest first; `None` when they do not, when there is no window or when the window has no time yet. `sessions` are the
    /// sessions with a process in this scan (the start of the oldest and what they used in this interval).
    fn windowed(
        &self,
        g: &mut Inner,
        now: Instant,
        sessions: &BTreeMap<u32, (u64, u64)>,
    ) -> Option<u64> {
        if self.window.is_zero() {
            return None;
        }
        for (sid, (_, used)) in sessions {
            let t = g.tracks.entry(*sid).or_default();
            t.missed = 0;
            t.used.push_back((now, *used));
            if t.used.len() > MAX_TRACK {
                t.used.pop_front();
            }
        }
        // a session that two scans in a row did not find is over; one that one scan missed (a process left between the listing and the read)
        // keeps its history
        g.tracks.retain(|sid, t| {
            if sessions.contains_key(sid) {
                return true;
            }
            t.missed = t.missed.saturating_add(1);
            t.missed < 2
        });
        let window = self.window;
        for t in g.tracks.values_mut() {
            while t
                .used
                .front()
                .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= window)
            {
                t.used.pop_front();
            }
        }
        let span = window.min(now.saturating_duration_since(g.since?));
        if span.is_zero() {
            return None;
        }
        let tracks = &g.tracks;
        youngest_needed(
            sessions.iter().map(|(sid, (start, _))| {
                let used = tracks.get(sid).map_or(0, |t| {
                    t.used
                        .iter()
                        .filter(|(at, _)| now.saturating_duration_since(*at) < span)
                        .fold(0u64, |sum, (_, u)| sum.saturating_add(*u))
                });
                (*start, used)
            }),
            self.needed(span),
        )
    }
}

/// The sessions of the processes outside the baseline: for each, the start of its oldest process that is still there and the CPU the processes
/// used in this interval.
fn by_session(found: &[(Proc, u64)]) -> BTreeMap<u32, (u64, u64)> {
    let mut sessions: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    for (p, used) in found {
        let e = sessions.entry(p.session).or_insert((p.key.1, 0));
        e.0 = e.0.min(p.key.1);
        e.1 = e.1.saturating_add(*used);
    }
    sessions
}

/// The start of the session where the CPU used reaches `needed` ticks when the sessions are taken oldest first; `None` when all together do
/// not reach it. Each item is the start of a session (its oldest process that is still there) and the ticks it used.
fn youngest_needed(sessions: impl IntoIterator<Item = (u64, u64)>, needed: u64) -> Option<u64> {
    let mut by_age: Vec<(u64, u64)> = sessions.into_iter().filter(|(_, used)| *used > 0).collect();
    by_age.sort_unstable();
    let mut sum = 0u64;
    by_age.into_iter().find_map(|(start, used)| {
        sum = sum.saturating_add(used);
        (sum >= needed).then_some(start)
    })
}
