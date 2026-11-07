//! The CPU of the session outside agentd and the engine.
//!
//! The brake inside a round ([`watch`](crate::direct::goal::watch)) ends a run that makes tool calls and changes no file. A run that waits for a
//! cold build of tens of minutes on two vCPUs with `sleep 60; tail build.log` looks like that, and such a run can be ended
//! that way. The evidence of a running command ([`command`](crate::direct::goal::command)) watches the processes of the engine's user by session
//! and age; this is the second signal and needs none of that: how much CPU the machine used that is neither agentd's nor the engine's own.
//!
//! The reading is a counter of clock ticks (100 a second, the unit of `/proc`) that only goes up, so the watch can take the CPU used over any
//! stretch from two readings. It is the busy time of the machine, `user + nice + system` of the first line of `/proc/stat`, less what agentd's
//! own process, the children it waited for (git) and the engine's own process used (`utime + stime` of `/proc/<pid>/stat`, and `cutime + cstime`
//! for agentd). What the engine runs for the model (a build, a test run,
//! a package install, `rg`) is a process of its own and stays in the count, and so does any helper process of the engine's. The idle, iowait,
//! irq and steal times never count. Only the machine's total is read, so no CPU is counted twice.
//!
//! The engine's process id comes from the driver control ([`DriverCtl::engine_pid_cell`](crate::direct::hooks::DriverCtl)): the engine is started
//! again for a fresh session, and the pid changes with it. A restarted engine's ticks are taken off from its first reading on, at most what the machine
//! used in that interval.

use crate::direct::goal::command::parse_stat;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Who reads the CPU of the session. The real one is [`ProcBusy`]; a test names its own.
pub trait BusyProbe: Send + Sync + std::fmt::Debug {
    /// The CPU the session used outside agentd and the engine, as a counter of clock ticks that never goes down; its first value is 0 (the
    /// origin), so the counter shows what was used since the first reading. `None` when `/proc` could not be read: no evidence.
    fn read(&self) -> Option<u64>;
}

/// `user + nice + system` of the first line of the text of `/proc/stat` (`cpu  100 5 50 1000 ...`). `None` for a text that does not start with
/// that line or whose numbers do not parse.
pub fn busy_ticks(stat: &str) -> Option<u64> {
    let mut fields = stat.lines().next()?.strip_prefix("cpu")?.split_whitespace();
    let mut next = || fields.next()?.parse::<u64>().ok();
    let (user, nice, system) = (next()?, next()?, next()?);
    Some(user.saturating_add(nice).saturating_add(system))
}

/// A process: its pid, its start time and what it has used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Own {
    pid: u32,
    start: u64,
    ticks: u64,
    /// The CPU of the children the process has waited for.
    waited: u64,
}

#[derive(Debug, Default)]
struct State {
    /// The totals at the previous reading; `None` before the first.
    total: Option<u64>,
    agentd: u64,
    engine: Option<Own>,
    /// What has been used since the first reading.
    used: u64,
}

/// The real probe, over a `/proc` tree (the real one in the product, a written one in a test).
#[derive(Debug)]
pub struct ProcBusy {
    root: PathBuf,
    agentd: u32,
    engine: Arc<AtomicU32>,
    state: Mutex<State>,
}

impl ProcBusy {
    /// `agentd` is agentd's own process id; `engine` holds the engine's (0: there is none), see the driver control.
    pub fn new(root: PathBuf, agentd: u32, engine: Arc<AtomicU32>) -> Self {
        Self {
            root,
            agentd,
            engine,
            state: Mutex::new(State::default()),
        }
    }

    fn own(&self, pid: u32) -> Option<Own> {
        let stat = parse_stat(std::fs::read(self.root.join(pid.to_string()).join("stat")).ok()?)?;
        Some(Own {
            pid,
            start: stat.start,
            ticks: stat.ticks,
            waited: stat.waited,
        })
    }
}

impl BusyProbe for ProcBusy {
    fn read(&self) -> Option<u64> {
        let total = busy_ticks(&std::fs::read_to_string(self.root.join("stat")).ok()?)?;
        // agentd's own CPU must be known: without it the count could not be told from agentd's work.
        // What agentd used itself and in the processes it started and waited for (git, for the snapshots and the fingerprint of the tree).
        let agentd = self.own(self.agentd)?;
        let agentd = agentd.ticks.saturating_add(agentd.waited);
        let engine = match self.engine.load(Ordering::SeqCst) {
            0 => None,
            pid => self.own(pid),
        };
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(before) = g.total else {
            g.total = Some(total);
            g.agentd = agentd;
            g.engine = engine;
            return Some(g.used);
        };
        let gained = total.saturating_sub(before);
        let by_agentd = agentd.saturating_sub(g.agentd);
        let by_engine = match (engine, g.engine) {
            (Some(now), Some(then)) if now.pid == then.pid && now.start == then.start => {
                now.ticks.saturating_sub(then.ticks)
            }
            // a process that is new since the previous reading has used everything it has used so far in this interval, at most
            (Some(now), _) => now.ticks,
            (None, _) => 0,
        };
        g.used = g
            .used
            .saturating_add(gained.saturating_sub(by_agentd.saturating_add(by_engine)));
        g.total = Some(total);
        g.agentd = agentd;
        g.engine = engine;
        Some(g.used)
    }
}
