//! The first process of a session's machine.
//!
//! In a session's microVM `agentd serve` is process 1, and the kernel hands every process whose parent ended before it to process 1.
//! Nobody waited for those: each stayed in the process table as an ended entry until the session ended (one `git <defunct>` per commit
//! of the engine). The worker cannot wait for them itself: a wait for any child would take the exit status of its own children (the
//! engine, git, every command), see `direct/messages/reap.rs`.
//!
//! So `agentd serve`, when it is process 1, takes the first-process role ([`takes_the_role`], [`split`]): it starts the worker, a second
//! `agentd serve` with the argument [`WORKER_FLAG`], as its one child and does three things only. It collects every ended child, it
//! passes SIGTERM and SIGINT on to the worker, and it ends with the worker's exit code. On any other machine `agentd serve` is one
//! process and none of this runs.
//!
//! The rules of the loop ([`supervise_with`]):
//! - The signals are watched before the worker is started, so no notice of an ended child and no stop signal can be lost.
//! - One pass collects at most one entry and then gives the stop signals their turn; while a pass collected something the loop goes
//!   round without sleeping, and when nothing is ready it sleeps until a notice. A stream of ended processes that never stops cannot
//!   keep a stop signal from being passed on, and there is no batch size and no timer.
//! - The look is a wait for ANY child (`rustix::process::wait`). `waitpid(None, ..)` of that crate waits only for the caller's own
//!   process group and would miss exactly the detached entries.
//! - Only SIGTERM and SIGINT are passed on, to the worker's pid only, from the thread that collects: a pid that was collected is never
//!   signalled. Any other signal has no watcher, and the kernel drops it for process 1.
//! - A stop signal that comes before the worker has said it is ready (SIGUSR1 to its parent, [`say_ready`]) is held and passed on once
//!   at that notice: passed on earlier it would end the worker by the default action.
//! - Process 1 uses no command of the runtime and never waits on the worker's handle: the one wait is the look above.
//! - A line of process 1 is one write of the whole line, after it has acted. It writes no line for a signal it passed on: the worker's
//!   own line records that.

use crate::logging::LogSink;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, WaitStatus, getppid, kill_process, wait};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::signal::unix::{SignalKind, signal};

/// The argument of the worker that process 1 starts. An argument and no variable: a variable would reach the engine's commands.
pub const WORKER_FLAG: &str = "--behind-first-process";

fn has_flag(args: &[String]) -> bool {
    args.iter().any(|a| a == WORKER_FLAG)
}

/// `agentd serve` takes the first-process role: it is process 1 and no process 1 started it.
pub fn takes_the_role(pid: u32, args: &[String]) -> bool {
    pid == 1 && !has_flag(args)
}

/// The worker counts as behind process 1 only with the argument and with process 1 as its parent.
pub fn is_behind_first(args: &[String], parent: Option<u32>) -> bool {
    has_flag(args) && parent == Some(1)
}

/// The process id of this process's parent; `None` when the parent is outside this process's namespace.
pub fn parent_pid() -> Option<u32> {
    getppid().and_then(|p| u32::try_from(p.as_raw_nonzero().get()).ok())
}

static BEHIND_FIRST: AtomicBool = AtomicBool::new(false);

/// `agentd serve` found that it is the worker behind process 1 ([`is_behind_first`]); set once, before anything is started.
pub fn note_behind_first() {
    BEHIND_FIRST.store(true, Ordering::SeqCst);
}

/// Whether this process is the worker behind process 1.
pub fn behind_first() -> bool {
    BEHIND_FIRST.load(Ordering::SeqCst)
}

/// The worker's command: this program (`std::env::current_exe()`, so the worker's name stays `agentd`), the arguments as given
/// (`serve` first) and [`WORKER_FLAG`] last. The three streams, the environment and the directory are inherited.
pub fn worker_command(args: &[String]) -> std::io::Result<std::process::Command> {
    // nosemgrep: current-exe -- process 1 starts the program it already runs once more as its worker, a path to run and not a trust or access decision
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.args(args).arg(WORKER_FLAG);
    Ok(command)
}

/// How the worker ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// It exited with this code.
    Exit(u8),
    /// A signal with this number ended it.
    Signal(u8),
}

impl Ended {
    /// From the two readings of a wait status: an exit code is kept (its low eight bits), a signal is named, anything else is exit 1.
    pub fn of(exit: Option<i32>, signal: Option<i32>) -> Self {
        match (exit, signal) {
            (Some(code), _) => Self::Exit(u8::try_from(code & 0xff).unwrap_or(1)),
            (None, Some(n)) => Self::Signal(u8::try_from(n).unwrap_or(u8::MAX)),
            (None, None) => Self::Exit(1),
        }
    }

    /// The exit code process 1 ends with: the worker's own, or 128 plus the signal's number as a shell reports it. Process 1 cannot end
    /// itself by a signal.
    pub fn code(self) -> u8 {
        match self {
            Self::Exit(code) => code,
            Self::Signal(n) => n.saturating_add(128),
        }
    }
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exit(code) => write!(f, "exit {code}"),
            Self::Signal(n) => write!(f, "signal {n}"),
        }
    }
}

/// The exit code of process 1 for the worker's wait status.
pub fn exit_code_of(status: WaitStatus) -> u8 {
    Ended::of(status.exit_status(), status.terminating_signal()).code()
}

/// What one look for an ended child gave. The real look is `rustix::process::wait(WaitOptions::NOHANG)`; a test feeds its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collected {
    /// No child has ended.
    Nothing,
    /// An ended process that is not the worker was collected.
    Other,
    /// The worker ended.
    Worker(Ended),
    /// There is no child at all, which cannot happen while the worker was never collected.
    NoChild,
}

/// The four notices process 1 sleeps on: a child ended, the two stop signals, and the worker's ready notice.
#[derive(Debug)]
pub struct Watched {
    child: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    ready: tokio::signal::unix::Signal,
}

impl Watched {
    /// Starts to watch SIGCHLD, SIGTERM, SIGINT and SIGUSR1. Inside a runtime, and before the worker is started.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            child: signal(SignalKind::child())?,
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            ready: signal(SignalKind::user_defined1())?,
        })
    }
}

/// One line of process 1: built whole, with its newline, and written with one call, so that it is never split and never lands inside
/// a line of the worker in pieces.
pub fn say(text: &str) {
    let line = format!("agentd (process 1): {text}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
}

fn name_of(signal: Signal) -> &'static str {
    if signal == Signal::TERM {
        "SIGTERM"
    } else {
        "SIGINT"
    }
}

/// The loop of process 1, with its two kernel calls as parameters: `collect` is one look for an ended child, `pass_on` sends a stop
/// signal to the worker. `ready`: whether the worker has already said that it watches the stop signals. Returns the exit code of
/// process 1.
pub async fn supervise_with(
    mut collect: impl FnMut() -> Collected,
    mut pass_on: impl FnMut(Signal),
    mut signals: Watched,
    mut ready: bool,
) -> u8 {
    let (mut collected, mut passes, mut passed_on) = (0_u64, 0_u64, 0_u64);
    let (mut held_term, mut held_int) = (false, false);
    loop {
        passes += 1;
        let more = match collect() {
            Collected::Worker(ended) => {
                say(&format!(
                    "the worker ended ({ended}); {collected} ended processes were collected in {passes} passes; {passed_on} stop signals were passed on; process 1 ends"
                ));
                return ended.code();
            }
            Collected::Other => {
                collected += 1;
                true
            }
            Collected::Nothing => false,
            Collected::NoChild => {
                say(
                    "process 1 has no child to wait for although the worker was never collected; process 1 ends",
                );
                return 1;
            }
        };
        // The stop signals come first; with `more` the pass does not sleep, and `yield_now` lets the runtime read the signals that came
        // meanwhile before this task goes on.
        let stop = tokio::select! {
            biased;
            _ = signals.term.recv() => Some(Signal::TERM),
            _ = signals.int.recv() => Some(Signal::INT),
            _ = signals.ready.recv() => {
                ready = true;
                None
            }
            _ = signals.child.recv() => None,
            () = tokio::task::yield_now(), if more => None,
        };
        match stop {
            Some(signal) if ready => {
                pass_on(signal);
                passed_on += 1;
            }
            Some(signal) => {
                if signal == Signal::TERM {
                    held_term = true;
                } else {
                    held_int = true;
                }
                say(&format!(
                    "{} held until the worker is ready",
                    name_of(signal)
                ));
            }
            None => {}
        }
        if ready {
            for (held, signal) in [(&mut held_term, Signal::TERM), (&mut held_int, Signal::INT)] {
                if std::mem::take(held) {
                    pass_on(signal);
                    passed_on += 1;
                }
            }
        }
    }
}

/// The whole role: builds a runtime of one thread, watches the signals, starts the worker and runs [`supervise_with`] with the kernel's
/// calls. Returns the exit code of this process: the worker's, or 1 when the signals cannot be watched or the worker cannot be started.
pub fn split(mut worker: std::process::Command) -> u8 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            say(&format!("signals cannot be watched ({e}); process 1 ends"));
            return 1;
        }
    };
    runtime.block_on(async move {
        let signals = match Watched::new() {
            Ok(signals) => signals,
            Err(e) => {
                say(&format!(
                    "signals cannot be watched ({e}); process 1 ends"
                ));
                return 1;
            }
        };
        // The handle is kept and never waited on: the one wait of this process is the look below.
        let child = match worker.spawn() {
            Ok(child) => child,
            Err(e) => {
                say(&format!(
                    "the worker could not be started ({e}); process 1 ends"
                ));
                return 1;
            }
        };
        let worker_pid = Pid::from_child(&child);
        say(&format!(
            "the worker is process {}; ended processes that are handed to process 1 are collected here",
            child.id()
        ));
        let collect = move || {
            loop {
                match wait(WaitOptions::NOHANG) {
                    Ok(None) => return Collected::Nothing,
                    Ok(Some((pid, status))) if pid == worker_pid => {
                        return Collected::Worker(Ended::of(
                            status.exit_status(),
                            status.terminating_signal(),
                        ));
                    }
                    Ok(Some(_)) => return Collected::Other,
                    Err(e) if e == Errno::INTR => {}
                    Err(_) => return Collected::NoChild,
                }
            }
        };
        // A worker that has already ended is collected by the next look; the error of a signal to it is nothing to act on.
        let pass_on = move |signal: Signal| {
            let _ = kill_process(worker_pid, signal);
        };
        supervise_with(collect, pass_on, signals, false).await
    })
}

/// The worker's side of the ready notice: SIGUSR1 to process 1, once the worker watches its own stop signals. Only the worker behind
/// process 1 sends it.
pub fn say_ready(log: &dyn LogSink) {
    if !behind_first() {
        return;
    }
    if let Err(e) = kill_process(Pid::INIT, Signal::USR1) {
        log.line(&format!(
            "the first process could not be told that agentd watches the termination signals ({e}): it holds a request to end"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn b883_only_process_1_without_the_worker_argument_takes_the_role() {
        assert!(takes_the_role(1, &args(&["serve"])));
        assert!(takes_the_role(
            1,
            &args(&["serve", "--listen", "127.0.0.1:8080"])
        ));
        assert!(!takes_the_role(1, &args(&["serve", WORKER_FLAG])));
        assert!(!takes_the_role(7, &args(&["serve"])));
        assert!(!takes_the_role(7, &args(&["serve", WORKER_FLAG])));
    }

    #[test]
    fn b883_the_argument_counts_only_below_process_1() {
        assert!(is_behind_first(&args(&["serve", WORKER_FLAG]), Some(1)));
        assert!(!is_behind_first(
            &args(&["serve", WORKER_FLAG]),
            Some(4_242)
        ));
        assert!(!is_behind_first(&args(&["serve", WORKER_FLAG]), None));
        assert!(!is_behind_first(&args(&["serve"]), Some(1)));
    }

    #[test]
    fn b883_the_worker_command_repeats_the_arguments_and_adds_the_argument_last() {
        let given = args(&["serve", "--listen", "127.0.0.1:8080"]);
        let command = worker_command(&given).expect("this program is known");
        let got: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            args(&["serve", "--listen", "127.0.0.1:8080", WORKER_FLAG])
        );
        assert_eq!(
            command.get_program(),
            std::env::current_exe()
                .expect("this program is known")
                .as_os_str()
        );
        assert_eq!(
            command.get_envs().count(),
            0,
            "the environment is inherited as it is"
        );
    }

    #[test]
    fn b883_an_exit_code_is_kept_and_a_signal_is_128_plus_its_number() {
        assert_eq!(Ended::of(Some(0), None).code(), 0);
        assert_eq!(Ended::of(Some(7), None).code(), 7);
        assert_eq!(Ended::of(Some(143), None).code(), 143);
        assert_eq!(Ended::of(None, Some(9)).code(), 137);
        assert_eq!(Ended::of(None, Some(15)).code(), 143);
        assert_eq!(Ended::of(None, None).code(), 1);
        assert_eq!(Ended::of(Some(7), None).to_string(), "exit 7");
        assert_eq!(Ended::of(None, Some(9)).to_string(), "signal 9");
        // The same through a real wait status: a child of this test that exits 7, and one that a signal ends.
        let mut exits = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("sh starts");
        let (_, status) =
            rustix::process::waitpid(Some(Pid::from_child(&exits)), WaitOptions::empty())
                .expect("the child is waited for")
                .expect("it ended");
        assert_eq!(exit_code_of(status), 7);
        // The wait above collected the child, so the standard handle has nothing left to wait for.
        let _ = exits.wait();
        let mut sleeps = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("sleep starts");
        let pid = Pid::from_child(&sleeps);
        kill_process(pid, Signal::KILL).expect("the child is signalled");
        let (_, status) = rustix::process::waitpid(Some(pid), WaitOptions::empty())
            .expect("the child is waited for")
            .expect("it ended");
        assert_eq!(exit_code_of(status), 137);
        let _ = sleeps.wait();
    }
}
