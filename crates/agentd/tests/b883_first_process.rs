//! A session keeps no ended process entry. In a session's microVM `agentd serve` is process 1, and every process whose parent
//! ended before it is handed to process 1. Nobody waited for those, so one ended entry per commit of the engine stayed until the
//! session ended. `agentd serve` as process 1 now starts the worker as its one child and itself only collects ended processes, passes
//! SIGTERM and SIGINT on and ends with the worker's exit code (`agentd::first`).
//!
//! The tests that need process 1 start their program as the first process of a process-id namespace
//! (`direct_support::first_process_possible`); on a machine that cannot make one they print `SKIPPED:` and hold nothing, and with
//! `HAMH_TEST_REQUIRE_PROCESS_1=1` they fail there. The fixtures use `sh`, `sleep`, `setsid`, `trap`, `kill -s <NAME> <pid>` and files
//! as barriers only.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::first::{Collected, Ended, Watched, supervise_with};
use direct_support::{
    as_first_process, child_of, ended_entries_with_parent, first_process_possible, record_skip_in,
    with_coverage_profile,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The program that runs the loop of the first process around any program (`src/bin/first_process_rig.rs`).
const RIG: &str = env!("CARGO_BIN_EXE_agentd-first-process-rig");

/// `true` on a machine that cannot make the namespace, after the line that says so.
fn skipped() -> bool {
    if first_process_possible() {
        return false;
    }
    eprintln!(
        "SKIPPED: this machine cannot start a process as the first one of a process-id namespace (unshare --user --pid --fork)"
    );
    true
}

/// Waits until `seen` holds, at most `limit`; `false` when it never did.
fn wait_until(limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while !seen() {
        if began.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn alive(pid: u32) -> bool {
    // Number 0 names no process: to `kill` it is the caller's own process group, which always answers.
    pid != 0
        && std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

fn send(signal: &str, pid: u32) {
    // To `kill`, number 0 is the caller's own process group: this test, and whatever shares its group.
    assert_ne!(
        pid, 0,
        "number 0 names no process: {signal} would go to the test's own process group"
    );
    let status = std::process::Command::new("kill")
        .args(["-s", signal, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -s {signal} {pid}: {status}");
}

/// The ended entries with parent `parent` that are left: the same process is an ended entry in two samples one second apart.
fn left_with_parent(parent: u32) -> Vec<(u32, String)> {
    let first = ended_entries_with_parent(parent);
    std::thread::sleep(Duration::from_secs(1));
    let second = ended_entries_with_parent(parent);
    first.into_iter().filter(|e| second.contains(e)).collect()
}

/// The loop of the first process around a shell fixture: `agentd-first-process-rig -- sh -c <text>`, as process 1 of a namespace of its
/// own or as an ordinary process. The fixture works in `dir` and uses files there as barriers.
struct Rig {
    child: std::process::Child,
    /// Process 1 of the namespace, as this machine numbers it (the rig itself when it is an ordinary process).
    first: u32,
    in_namespace: bool,
    dir: PathBuf,
}

impl Rig {
    fn start_program(label: &str, as_first: bool, program: &[&str]) -> Self {
        let dir = common::scratch(label);
        let mut command = if as_first {
            as_first_process(Path::new(RIG))
        } else {
            std::process::Command::new(RIG)
        };
        let mut child = command
            .arg("--")
            .args(program)
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::File::create(dir.join("out.log")).unwrap())
            .stderr(std::fs::File::create(dir.join("first.log")).unwrap())
            .spawn()
            .unwrap();
        let first = if as_first {
            // The first child of the program that made the namespace. A namespace whose worker ends at once may be gone before it
            // can be read: it then has no first process to name (0).
            let outer = child.id();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let found = std::fs::read_to_string(format!("/proc/{outer}/task/{outer}/children"))
                    .ok()
                    .and_then(|c| c.split_whitespace().next().and_then(|p| p.parse().ok()));
                if let Some(pid) = found {
                    break pid;
                }
                if child.try_wait().unwrap().is_some() {
                    break 0;
                }
                assert!(
                    Instant::now() < deadline,
                    "process 1 under the namespace program {outer}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            child.id()
        };
        Self {
            child,
            first,
            in_namespace: as_first,
            dir,
        }
    }

    /// The fixture `text` as the worker of process 1.
    fn start(label: &str, text: &str) -> Self {
        Self::start_program(label, true, &["sh", "-c", text])
    }

    /// The worker, as this machine numbers it.
    fn worker(&self) -> u32 {
        child_of(self.first)
    }

    fn has(&self, file: &str) -> bool {
        self.dir.join(file).exists()
    }

    fn put(&self, file: &str) {
        std::fs::write(self.dir.join(file), "").unwrap();
    }

    /// What process 1 and the fixture wrote to standard error.
    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("first.log")).unwrap_or_default()
    }

    /// The exit code of the namespace once it has ended, at most `limit` from now (`None` inside: a signal ended the program that made
    /// it); `None` when it still runs.
    fn ended_within(&mut self, limit: Duration) -> Option<Option<i32>> {
        let began = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // The namespace is gone: the number of its process 1 names nothing of this rig any more.
                self.first = 0;
                return Some(status.code());
            }
            if began.elapsed() > limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The process that the drop of this rig ends with KILL: process 1 of the namespace, and only while the program that made the
    /// namespace still runs. That program waits for process 1, so the number stays taken until it has ended. Afterwards the number
    /// names no process of this rig, the machine may give it to any other process, and the rig forgets it.
    fn first_to_end(&mut self) -> Option<u32> {
        if !self.in_namespace {
            return None;
        }
        self.first_to_signal()
    }

    /// The process that a signal of a test goes to: process 1 of the namespace (the rig itself, where it runs as an ordinary
    /// process), and only while the program that the test started still runs, the guard of [`Rig::first_to_end`]. Once that program
    /// has ended, the number names no process of this rig: 0, which `ended_within` leaves, is the caller's own process group to
    /// `kill`, and a number whose end nobody saw may by now be another process of the machine.
    fn first_to_signal(&mut self) -> Option<u32> {
        if self.first == 0 {
            return None;
        }
        if !matches!(self.child.try_wait(), Ok(None)) {
            self.first = 0;
            return None;
        }
        Some(self.first)
    }

    /// Sends process 1 `signal` by its name with the `kill` program, as a container runtime or a person would. A test that sends
    /// after the end of its namespace fails here, before `kill` is started.
    fn signal(&mut self, signal: &str) {
        let Some(first) = self.first_to_signal() else {
            panic!(
                "no process 1 to send {signal} to: the program that made the namespace has ended; log:\n{}",
                self.log()
            );
        };
        send(signal, first);
    }

    /// The last line of process 1: (ended processes collected, passes of the loop, stop signals passed on).
    fn last_line(&self) -> (u64, u64, u64) {
        counts_of_last_line(&self.log())
    }
}

/// The counts in the last line of process 1 in `log`: (ended processes collected, passes of the loop, stop signals passed on).
fn counts_of_last_line(log: &str) -> (u64, u64, u64) {
    let line = log
        .lines()
        .rfind(|l| l.contains("the worker ended"))
        .unwrap_or_else(|| panic!("process 1 wrote no last line; log:\n{log}"));
    let number_before = |mark: &str| -> u64 {
        let head = &line[..line
            .find(mark)
            .unwrap_or_else(|| panic!("{mark:?} in {line:?}"))];
        head.split_whitespace().last().unwrap().parse().unwrap()
    };
    (
        number_before(" ended processes were collected"),
        number_before(" passes;"),
        number_before(" stop signals were passed on"),
    )
}

impl Drop for Rig {
    fn drop(&mut self) {
        // The first process of a namespace takes every process of the namespace with it.
        if let Some(first) = self.first_to_end() {
            let _ = std::process::Command::new("kill")
                .args(["-s", "KILL", &first.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The state letter of process `pid` in `/proc/<pid>/stat` (`Z`: it has ended and nobody has waited for it); `None` when it is gone.
fn state_of(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..].trim_start().chars().next()
}

/// The rig ends its namespace with KILL to the number of process 1, which it read once at the start. When the program that made the
/// namespace has ended, that number names no process of the rig any more, and the machine gives it to the next process that needs
/// one: a compiler of another build, another test of the same run. The rig then names no process to end. Three shapes: the test saw
/// the end, the namespace ended and nobody looked, and a namespace that still runs, which the drop must still end.
#[test]
fn b883_the_rig_names_no_first_process_once_its_namespace_has_ended() {
    if skipped() {
        return;
    }
    const WAITS: &str = "while [ ! -e stop ]; do sleep 0.05; done\nexit 0\n";
    // The test saw the end.
    let mut seen = Rig::start("b883-rig-seen", WAITS);
    assert_ne!(
        seen.first, 0,
        "TRIGGER PRESENT: the rig read the number of process 1 while the namespace ran"
    );
    seen.put("stop");
    assert_eq!(
        seen.ended_within(Duration::from_secs(20)),
        Some(Some(0)),
        "log:\n{}",
        seen.log()
    );
    assert_eq!(
        seen.first, 0,
        "the number is forgotten once the test saw the namespace end"
    );
    assert_eq!(seen.first_to_end(), None);

    // The namespace ended and the test did not look.
    let mut unseen = Rig::start("b883-rig-unseen", WAITS);
    assert_ne!(
        unseen.first, 0,
        "TRIGGER PRESENT: the rig read the number of process 1 while the namespace ran"
    );
    let outer = unseen.child.id();
    unseen.put("stop");
    assert!(
        wait_until(Duration::from_secs(20), || state_of(outer) == Some('Z')),
        "TRIGGER PRESENT: the program that made the namespace has ended and nobody has waited for it; log:\n{}",
        unseen.log()
    );
    assert_eq!(
        unseen.first_to_end(),
        None,
        "a namespace that has ended names no process to end"
    );
    assert_eq!(unseen.first, 0);

    // A namespace that still runs: the drop ends it.
    let mut running = Rig::start("b883-rig-running", WAITS);
    let first = running.first;
    assert_ne!(first, 0);
    assert_eq!(running.first_to_end(), Some(first));
    assert!(alive(first));
    drop(running);
    assert!(
        wait_until(Duration::from_secs(10), || !alive(first)),
        "the drop of a rig whose namespace runs ends process 1"
    );
}

/// A test sends its signals to the number of process 1, which the rig read once at the start. Once the program that made the
/// namespace has ended, that number names no process of the rig. After the test saw the end it is 0, and to `kill` number 0 is the
/// caller's own process group: the test itself. Where nobody looked, the machine may have given the number to another process.
/// The rig then names no process to signal. Four shapes: the test saw the end, the namespace ended and nobody looked, a namespace
/// that still runs, which the signal must reach, and a rig that runs as an ordinary process.
#[test]
fn b883_the_rig_names_no_process_to_signal_once_its_namespace_has_ended() {
    if skipped() {
        return;
    }
    const WAITS: &str =
        "kill -s USR1 1\n: > started\nwhile [ ! -e stop ]; do sleep 0.05; done\nexit 0\n";
    // The test saw the end.
    let mut seen = Rig::start("b883-signal-seen", WAITS);
    assert_ne!(
        seen.first, 0,
        "TRIGGER PRESENT: the rig read the number of process 1 while the namespace ran"
    );
    seen.put("stop");
    assert_eq!(
        seen.ended_within(Duration::from_secs(20)),
        Some(Some(0)),
        "log:\n{}",
        seen.log()
    );
    assert_eq!(
        seen.first, 0,
        "TRIGGER PRESENT: the number that a send would use is 0 once the test saw the namespace end"
    );
    assert_eq!(
        seen.first_to_signal(),
        None,
        "number 0 is the test's own process group to kill, and no process 1"
    );

    // The namespace ended and the test did not look.
    let mut unseen = Rig::start("b883-signal-unseen", WAITS);
    assert_ne!(
        unseen.first, 0,
        "TRIGGER PRESENT: the rig read the number of process 1 while the namespace ran"
    );
    let outer = unseen.child.id();
    unseen.put("stop");
    assert!(
        wait_until(Duration::from_secs(20), || state_of(outer) == Some('Z')),
        "TRIGGER PRESENT: the program that made the namespace has ended and nobody has waited for it; log:\n{}",
        unseen.log()
    );
    assert_eq!(
        unseen.first_to_signal(),
        None,
        "a namespace that has ended names no process to signal"
    );
    assert_eq!(unseen.first, 0);

    // A namespace that still runs: the signal reaches process 1, which passes it on to the worker.
    let mut running = Rig::start("b883-signal-running", WAITS);
    assert!(
        wait_until(Duration::from_secs(20), || running.has("started")),
        "{}",
        running.log()
    );
    let first = running.first;
    assert_ne!(first, 0);
    assert_eq!(running.first_to_signal(), Some(first));
    running.signal("TERM");
    assert_eq!(
        running.ended_within(Duration::from_secs(20)),
        Some(Some(143)),
        "the signal reached process 1 and, through it, the worker; log:\n{}",
        running.log()
    );
    assert_eq!(running.first_to_signal(), None);

    // A rig that runs as an ordinary process: the number is its own, for as long as it runs.
    let mut ordinary = Rig::start_program(
        "b883-signal-ordinary",
        false,
        &[
            "sh",
            "-c",
            "while [ ! -e stop ]; do sleep 0.05; done\nexit 0\n",
        ],
    );
    let own = ordinary.child.id();
    assert_eq!(ordinary.first_to_signal(), Some(own));
    ordinary.put("stop");
    assert_eq!(
        ordinary.ended_within(Duration::from_secs(20)),
        Some(Some(0)),
        "log:\n{}",
        ordinary.log()
    );
    assert_eq!(ordinary.first_to_signal(), None);
}

/// To `kill`, number 0 is the caller's own process group, which always answers. The rig leaves 0 where a process has ended, so a
/// look at number 0 finds no process.
#[test]
fn b883_the_rig_finds_no_process_alive_under_number_0() {
    assert!(!alive(0), "number 0 names no process of a rig");
}

/// A send to number 0 would go to the test's own process group. It fails the test before `kill` is started. The signal of this
/// test is 0, which sends nothing: where the test fails, no process was signalled.
#[test]
#[should_panic(expected = "number 0 names no process")]
fn b883_the_rig_sends_no_signal_to_number_0() {
    send("0", 0);
}

/// The processor time process `pid` has used, in clock ticks (user and system), and its private memory in kB (`None` where this
/// machine does not let the test read it).
fn cost_of(pid: u32) -> (u64, Option<u64>) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let after = stat.rfind(')').map_or("", |i| &stat[i + 1..]);
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After the name: state is field 0, so user time (field 14 of the line) is 11 and system time 12.
    let ticks = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let private = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .map(|text| {
            text.lines()
                .filter(|l| l.starts_with("Private_Clean:") || l.starts_with("Private_Dirty:"))
                .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .sum()
        });
    (ticks(11) + ticks(12), private)
}

// ---- the loop, around a fixture ------------------------------------------------------------------------------------------------------

/// 2,000 detached processes are handed to process 1 while the worker lives on, and all of them end within a moment. Each one is left
/// by a short shell of its own, so that it is handed over and not collected by the fixture's loop. Process 1 collects every one without
/// another end to wake it, in a number of passes that follows from the loop. Then a stream of handed-over processes that does not stop:
/// a stop signal is passed on while it runs.
#[test]
fn b883_a_burst_of_handed_over_processes_is_collected_without_another_end_to_wake_process_1() {
    if skipped() {
        return;
    }
    const BURST: u64 = 2_000;
    let mut rig = Rig::start(
        "b883-burst",
        "yes | head -n 2000 | while read x; do sh -c 'setsid sleep 0.2 & exit 0'; done\n\
         : > burst-done\n\
         while [ ! -e stop ]; do sleep 0.1; done\n\
         exit 0\n",
    );
    assert!(
        wait_until(Duration::from_secs(300), || rig.has("burst-done")),
        "the fixture handed its processes over; log:\n{}",
        rig.log()
    );
    std::thread::sleep(Duration::from_secs(2));
    let left = left_with_parent(rig.first);
    let (ticks, private) = cost_of(rig.first);
    rig.put("stop");
    let code = rig.ended_within(Duration::from_secs(30));
    assert_eq!(code, Some(Some(0)), "log:\n{}", rig.log());
    let (collected, passes, passed_on) = rig.last_line();
    assert!(
        collected >= BURST,
        "TRIGGER PRESENT: process 1 collected {collected} ended processes, and {BURST} were handed to it; log:\n{}",
        rig.log()
    );
    eprintln!(
        "TRIGGER PRESENT: process 1 collected {collected} handed-over processes ({BURST} were left behind)"
    );
    assert!(
        left.is_empty(),
        "ended entries left with process 1 as their parent two seconds after the burst: {left:?}"
    );
    // An ended child costs at most three passes: the one that collects it, the one that finds nothing after it, and one for its notice
    // when that comes late. One more pass is the first look, and one collects the worker.
    let bound = 3 * (collected + 1) + passed_on + 1;
    assert!(
        passes <= bound,
        "process 1 made {passes} passes for {collected} ended processes; the loop allows {bound}"
    );
    eprintln!(
        "COST first-process entries={collected} passes={passes} cpu_ticks={ticks} private_kb={}",
        private.map_or("unreadable".to_string(), |kb| kb.to_string())
    );

    // The first namespace has ended: its rig goes here and not at the end of the test, after the thousands of processes of the stream.
    drop(rig);

    // A stream without end: the stop signal gets its turn although an ended process is always waiting.
    let mut rig = Rig::start(
        "b883-stream",
        "trap ': > got-term' TERM\n\
         kill -s USR1 1\n\
         ( while [ ! -e stop ]; do sh -c 'setsid sleep 0.05 & exit 0'; echo x >> handed; done ) &\n\
         while [ ! -e stop ]; do sleep 0.1; done\n\
         wait\n\
         exit 0\n",
    );
    let handed = |rig: &Rig| {
        std::fs::read_to_string(rig.dir.join("handed"))
            .map(|t| t.lines().count())
            .unwrap_or(0)
    };
    assert!(
        wait_until(Duration::from_secs(120), || handed(&rig) >= 200),
        "the stream runs; log:\n{}",
        rig.log()
    );
    let before = handed(&rig);
    rig.signal("TERM");
    assert!(
        wait_until(Duration::from_secs(20), || rig.has("got-term")),
        "the stop signal reached the worker while ended processes kept coming; log:\n{}",
        rig.log()
    );
    assert!(!rig.has("stop"), "the stream was not stopped by the test");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        handed(&rig) > before,
        "TRIGGER PRESENT: the stream still ran when the signal was passed on ({before} then {})",
        handed(&rig)
    );
    rig.put("stop");
    assert_eq!(
        rig.ended_within(Duration::from_secs(30)),
        Some(Some(0)),
        "log:\n{}",
        rig.log()
    );
    let (_, _, passed_on) = rig.last_line();
    assert_eq!(passed_on, 1, "log:\n{}", rig.log());
}

/// Process 1 ends with the exit code of the worker.
#[test]
fn b883_process_1_ends_with_the_workers_exit_code() {
    if skipped() {
        return;
    }
    let mut rig = Rig::start("b883-exit7", "exit 7\n");
    assert_eq!(
        rig.ended_within(Duration::from_secs(20)),
        Some(Some(7)),
        "log:\n{}",
        rig.log()
    );
    assert!(
        rig.log().contains("the worker ended (exit 7)"),
        "{}",
        rig.log()
    );
}

/// A worker that a signal ended: process 1 ends at once with 128 plus the signal's number, and does not wait for a process that the
/// worker left behind and that still runs.
#[test]
fn b883_process_1_ends_with_128_plus_the_signal_and_does_not_wait_for_a_helper_left_behind() {
    if skipped() {
        return;
    }
    let mut rig = Rig::start(
        "b883-killed",
        "setsid sleep 600 &\n: > started\nsleep 600\n",
    );
    assert!(
        wait_until(Duration::from_secs(20), || rig.has("started")),
        "{}",
        rig.log()
    );
    let worker = rig.worker();
    assert_ne!(
        worker, rig.first,
        "TRIGGER PRESENT: the worker is a child of process 1"
    );
    send("KILL", worker);
    assert_eq!(
        rig.ended_within(Duration::from_secs(5)),
        Some(Some(137)),
        "log:\n{}",
        rig.log()
    );
    assert!(
        rig.log().contains("the worker ended (signal 9)"),
        "{}",
        rig.log()
    );
}

/// A stop signal that reaches process 1 before the worker has said that it is ready is held, and passed on once at that notice. Passed
/// on at once it would end the fixture's shell by the default action (exit 143).
#[test]
fn b883_a_stop_signal_before_the_worker_is_ready_is_held_and_passed_on_once() {
    if skipped() {
        return;
    }
    let mut rig = Rig::start(
        "b883-barrier",
        ": > started\n\
         while [ ! -e go ]; do sleep 0.05; done\n\
         trap ': > got-term' TERM\n\
         kill -s USR1 1\n\
         while [ ! -e got-term ]; do sleep 0.05; done\n\
         exit 9\n",
    );
    assert!(
        wait_until(Duration::from_secs(20), || rig.has("started")),
        "{}",
        rig.log()
    );
    let worker = rig.worker();
    rig.signal("TERM");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        alive(rig.first) && alive(worker),
        "TRIGGER PRESENT: process 1 and the worker both live while the signal is held; log:\n{}",
        rig.log()
    );
    assert!(
        !rig.has("got-term"),
        "nothing was passed on before the ready notice"
    );
    assert!(
        rig.log()
            .contains("agentd (process 1): SIGTERM held until the worker is ready"),
        "{}",
        rig.log()
    );
    rig.put("go");
    assert_eq!(
        rig.ended_within(Duration::from_secs(20)),
        Some(Some(9)),
        "log:\n{}",
        rig.log()
    );
    assert!(rig.has("got-term"));
    let (_, _, passed_on) = rig.last_line();
    assert_eq!(passed_on, 1, "passed on once; log:\n{}", rig.log());
}

/// The known limit of the ready notice, held so that a change of it is seen: any root process of the machine can send it. Sent before
/// the worker watches the stop signals, it opens the barrier early, and a stop signal then ends the worker by the default action. No
/// program of the image and no code of the worker sends it.
#[test]
fn b883_a_ready_notice_from_another_root_process_opens_the_barrier_early() {
    if skipped() {
        return;
    }
    let mut rig = Rig::start(
        "b883-early",
        ": > started\nwhile [ ! -e go ]; do sleep 0.05; done\nexit 9\n",
    );
    assert!(
        wait_until(Duration::from_secs(20), || rig.has("started")),
        "{}",
        rig.log()
    );
    rig.signal("USR1");
    std::thread::sleep(Duration::from_millis(300));
    rig.signal("TERM");
    assert_eq!(
        rig.ended_within(Duration::from_secs(20)),
        Some(Some(143)),
        "log:\n{}",
        rig.log()
    );
    assert!(
        rig.log().contains("the worker ended (signal 15)"),
        "{}",
        rig.log()
    );
}

/// Only SIGTERM and SIGINT are passed on. Any other signal has no watcher in process 1, and the kernel drops it there.
#[test]
fn b883_only_term_and_int_are_passed_on() {
    if skipped() {
        return;
    }
    let mut rig = Rig::start(
        "b883-others",
        "trap ': > got-term' TERM\n\
         trap ': > got-int' INT\n\
         trap ': > got-hup' HUP\n\
         trap ': > got-quit' QUIT\n\
         trap ': > got-usr2' USR2\n\
         kill -s USR1 1\n\
         : > started\n\
         while [ ! -e got-term ]; do sleep 0.05; done\n\
         exit 5\n",
    );
    assert!(
        wait_until(Duration::from_secs(20), || rig.has("started")),
        "{}",
        rig.log()
    );
    let worker = rig.worker();
    for signal in ["HUP", "QUIT", "USR2"] {
        rig.signal(signal);
    }
    std::thread::sleep(Duration::from_millis(700));
    assert!(alive(rig.first) && alive(worker), "{}", rig.log());
    for file in ["got-hup", "got-quit", "got-usr2", "got-int", "got-term"] {
        assert!(
            !rig.has(file),
            "{file}: a signal that is not passed on reached the worker"
        );
    }
    rig.signal("INT");
    assert!(
        wait_until(Duration::from_secs(10), || rig.has("got-int")),
        "SIGINT is passed on; log:\n{}",
        rig.log()
    );
    assert!(!rig.has("got-term"));
    rig.signal("TERM");
    assert_eq!(
        rig.ended_within(Duration::from_secs(20)),
        Some(Some(5)),
        "log:\n{}",
        rig.log()
    );
    assert!(rig.has("got-term"));
    let (_, _, passed_on) = rig.last_line();
    assert_eq!(passed_on, 2, "log:\n{}", rig.log());
}

/// A worker that cannot be started: process 1 writes one line and ends with 1, as `agentd serve` does when its port cannot be bound.
#[test]
fn b883_a_worker_that_cannot_be_started_ends_process_1_with_1() {
    let missing = common::scratch("b883-missing").join("no-such-program");
    let missing = missing.to_str().unwrap();
    let mut ordinary = Rig::start_program("b883-nostart", false, &[missing]);
    assert_eq!(
        ordinary.ended_within(Duration::from_secs(20)),
        Some(Some(1)),
        "log:\n{}",
        ordinary.log()
    );
    assert!(
        ordinary
            .log()
            .contains("agentd (process 1): the worker could not be started ("),
        "{}",
        ordinary.log()
    );
    assert!(
        ordinary.log().contains("); process 1 ends\n"),
        "{}",
        ordinary.log()
    );
    if skipped() {
        return;
    }
    let mut first = Rig::start_program("b883-nostart-1", true, &[missing]);
    assert_eq!(
        first.ended_within(Duration::from_secs(20)),
        Some(Some(1)),
        "log:\n{}",
        first.log()
    );
    assert!(
        first
            .log()
            .contains("agentd (process 1): the worker could not be started ("),
        "{}",
        first.log()
    );
}

/// The loop itself, with its two kernel calls as closures: an ended process is always waiting, and a stop signal must be passed on
/// all the same. A loop that collects until nothing is ready never gives the signal its turn, whatever the speed of the machine.
#[tokio::test]
async fn b883_a_stop_signal_is_passed_on_while_ended_processes_keep_coming() {
    const LOOKS: u64 = 10_000_000;
    let signals = Watched::new().unwrap();
    let passed = Arc::new(AtomicBool::new(false));
    let looks = Arc::new(AtomicU64::new(0));
    // The signal goes to this test's own process, where the watchers above are in place.
    let sender = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        send("TERM", std::process::id());
    });
    let (seen, counted) = (passed.clone(), looks.clone());
    let code = supervise_with(
        move || {
            if seen.load(Ordering::SeqCst) {
                return Collected::Worker(Ended::Exit(0));
            }
            let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
            assert!(
                n <= LOOKS,
                "no turn was given to the stop signals in {LOOKS} looks that each found an ended process"
            );
            Collected::Other
        },
        |signal| {
            assert_eq!(signal, rustix::process::Signal::TERM);
            passed.store(true, Ordering::SeqCst);
        },
        signals,
        true,
    )
    .await;
    sender.join().unwrap();
    assert_eq!(
        code, 0,
        "the loop ends with the worker's exit code once the signal was passed on"
    );
    let n = looks.load(Ordering::SeqCst);
    assert!(
        n > 0,
        "TRIGGER PRESENT: ended processes were found before the signal came"
    );
    eprintln!(
        "TRIGGER PRESENT: {n} looks found an ended process before the stop signal was passed on"
    );
}

// ---- the real `agentd serve` -----------------------------------------------------------------------------------------------------------

use agentd::first::WORKER_FLAG;
use direct_support::{Direct, Opts, RUN, checkout_of_copy};
use harness_protocol::direct::s3 as keys;

/// Every version of `snapshot.json` in the bucket, oldest first.
fn records(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

/// Whether the copy in the bucket now holds the file `name`, looked at once per new copy.
#[derive(Default)]
struct CopyWatch {
    seen: usize,
    clones: u32,
}

impl CopyWatch {
    fn new_copy_holds(&mut self, d: &Direct, name: &str) -> bool {
        let now = records(d).len();
        if now == self.seen {
            return false;
        }
        self.seen = now;
        self.clones += 1;
        checkout_of_copy(
            &d.aws,
            &d.dir,
            &keys::snapshot_bundle_key(RUN),
            &format!("copy-{}", self.clones),
        )
        .join(name)
        .is_file()
    }
}

async fn wait_for(limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while !seen() {
        if began.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// The arguments of process `pid`.
fn arguments_of(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

/// The argument that process 1 gives its worker changes nothing in a process that process 1 did not start: asked to end, it saves the
/// copy and ends with the signal's exit code, as every `agentd serve` outside a session's machine does.
#[tokio::test(flavor = "multi_thread")]
async fn b883_the_worker_argument_outside_process_1_changes_nothing() {
    let mut d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "40".into())],
        agentd_env: vec![
            ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "3600000".to_string()),
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
        ],
        serves_itself: true,
        // The program ends by itself after the signal, and the test waits for that.
        writes_profile: true,
        ..Opts::default()
    })
    .await;
    let pid = d.process.as_ref().unwrap().pid();
    assert!(
        arguments_of(pid).iter().any(|a| a == WORKER_FLAG),
        "TRIGGER PRESENT: the process was started with {WORKER_FLAG}: {:?}",
        arguments_of(pid)
    );
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(30), || !records(&d).is_empty()).await,
        "the copy after the engine's first tool call"
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    std::fs::write(d.worktree().join("late.txt"), "written before the signal\n").unwrap();
    d.process.as_ref().unwrap().signal("TERM");
    let stored = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        stored,
        "the copy of the stop; log:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    let began = Instant::now();
    let code = loop {
        if let Some(code) = d.process.as_mut().unwrap().ended() {
            break Some(code);
        }
        if began.elapsed() > Duration::from_secs(20) {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let log = d.process.as_ref().unwrap().log_text();
    assert_eq!(code, Some(Some(143)), "log:\n{log}");
    assert!(
        log.contains("termination signal SIGTERM: agentd ends"),
        "{log}"
    );
    assert!(!log.contains("behind the first process"), "{log}");
    assert!(!log.contains("agentd (process 1)"), "{log}");
}

/// SIGINT that comes before the ready notice is held as SIGTERM is, and passed on once at the notice. The loop runs in this test's own
/// process, where its watchers are in place, and both signals are sent to that process.
#[tokio::test]
async fn b883_an_interrupt_before_the_ready_notice_is_held_and_passed_on_once() {
    let signals = Watched::new().unwrap();
    let notice_sent = Arc::new(AtomicBool::new(false));
    let passed = Arc::new(AtomicU64::new(0));
    let told = notice_sent.clone();
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        send("INT", std::process::id());
        std::thread::sleep(Duration::from_millis(300));
        told.store(true, Ordering::SeqCst);
        send("USR1", std::process::id());
    });
    let (counted, seen) = (passed.clone(), passed.clone());
    let code = supervise_with(
        move || {
            if seen.load(Ordering::SeqCst) > 0 {
                Collected::Worker(Ended::Exit(0))
            } else {
                Collected::Nothing
            }
        },
        move |signal| {
            assert_eq!(signal, rustix::process::Signal::INT);
            assert!(
                notice_sent.load(Ordering::SeqCst),
                "TRIGGER PRESENT: the signal came 300 ms before the ready notice and was passed on before it"
            );
            counted.fetch_add(1, Ordering::SeqCst);
        },
        signals,
        false,
    )
    .await;
    sender.join().unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        passed.load(Ordering::SeqCst),
        1,
        "the held signal is passed on once"
    );
}

/// A look that finds no child at all although the worker was never collected ends process 1 with 1 and one line; the loop does not go
/// on waiting for nothing. The real look cannot say this while the worker lives, so the loop is fed its own.
#[tokio::test]
async fn b883_process_1_ends_with_1_when_it_has_no_child_to_wait_for() {
    let signals = Watched::new().unwrap();
    let code = supervise_with(
        || Collected::NoChild,
        |signal| panic!("no stop signal came, and {signal:?} was passed on"),
        signals,
        true,
    )
    .await;
    assert_eq!(code, 1);
}

/// The output of a passing test is not shown, so without the record a round could not tell that such a test did not run.
#[test]
fn b883_a_first_process_test_that_skips_leaves_its_line_in_the_record() {
    let record = common::scratch("b883-skip-record").join("skips.txt");
    record_skip_in(&record);
    record_skip_in(&record);
    let text = std::fs::read_to_string(&record).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one line for each skip: {text:?}");
    for line in lines {
        assert_eq!(
            line,
            "SKIPPED: b883_a_first_process_test_that_skips_leaves_its_line_in_the_record: this machine cannot start a process as the first one of a process-id namespace"
        );
    }
}

/// The rig program without a program to run says how it is called and ends with 2.
#[test]
fn b883_the_rig_program_without_a_program_to_run_says_how_it_is_called() {
    let cases: [&[&str]; 3] = [&[], &["sh"], &["--"]];
    for arguments in cases {
        let output = std::process::Command::new(RIG)
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("usage: agentd-first-process-rig -- <program> [args]"),
            "{arguments:?}: {output:?}"
        );
    }
}

// ---- `agentd serve` as process 1 ------------------------------------------------------------------------------------------------------

use direct_support::{http, leaving_git};
use harness_protocol::runtime::TurnOutcome;

/// The last number of the `NSpid` line of process `pid`: its process id in its own namespace.
fn own_pid_of(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("NSpid:"))?;
    line.split_whitespace().last()?.parse().ok()
}

/// The children of process `parent`, whichever of its threads started them.
fn children_of(parent: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|list| {
            list.split_whitespace()
                .filter_map(|p| p.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The measured shape of the defect, with the real `agentd serve` as process 1: every git call of the worker leaves a detached process
/// behind, the copies of the work go on every 300 ms, and the engine edits its files. Before, each of those processes stayed as an ended
/// entry with process 1 as its parent until the machine ended. The copies must still hold the work: the worker's own git calls keep
/// their exit status.
#[tokio::test(flavor = "multi_thread")]
async fn b883_ended_processes_handed_to_process_1_are_collected_while_the_copies_go_on() {
    if skipped() {
        return;
    }
    let scratch = common::scratch("b883-leaving");
    let (bin, calls) = leaving_git(&scratch);
    let d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "6".into())],
        agentd_env: vec![
            ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "300".to_string()),
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ],
        first_process: true,
        ..Opts::default()
    })
    .await;
    let agentd = d.process.as_ref().unwrap();
    assert_eq!(
        agentd.own_pid(),
        Some(1),
        "agentd serve is process 1 of its namespace"
    );
    let first = agentd.pid();
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    let resp = d.wait_result().await;
    let log = agentd.log_text();
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\nlog:\n{log}"
    );
    let git_calls = std::fs::read_to_string(&calls)
        .map(|t| t.lines().count())
        .unwrap_or(0);
    assert!(
        git_calls >= 5,
        "TRIGGER PRESENT: the git that leaves a process behind was called {git_calls} times"
    );
    eprintln!(
        "TRIGGER PRESENT: {git_calls} git calls of the worker each left a detached process behind"
    );
    // The worker's own git calls gave their exit status: a stored copy holds what the engine wrote.
    assert!(!records(&d).is_empty(), "copies were stored; log:\n{log}");
    let copy = checkout_of_copy(&d.aws, &d.dir, &keys::snapshot_bundle_key(RUN), "b883-copy");
    let calc = std::fs::read_to_string(copy.join("src/calc.py")).unwrap_or_default();
    assert!(
        calc.contains("percent_change"),
        "the stored copy holds the engine's edit: {calc:?}"
    );
    let none_left = wait_for(Duration::from_secs(10), || {
        ended_entries_with_parent(first).is_empty()
    })
    .await;
    assert!(
        none_left,
        "ended entries with process 1 as their parent, ten seconds after the turn ({git_calls} git calls): {:?}",
        ended_entries_with_parent(first)
    );
    assert!(agentd.alive(), "process 1 runs on; log:\n{log}");
}

/// Process 1 ends with the worker's exit code when the worker is the real `agentd serve`: a port that is taken ends the worker with 1
/// and its line about the port, and process 1 with it.
#[tokio::test(flavor = "multi_thread")]
async fn b883_process_1_ends_with_the_exit_code_of_a_worker_whose_port_is_taken() {
    if skipped() {
        return;
    }
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let dir = common::scratch("b883-port");
    let log_file = dir.join("agentd.log");
    let mut command = as_first_process(Path::new(env!("CARGO_BIN_EXE_agentd")));
    command
        .args(["serve", "--listen", &addr])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("AGENTD_DATA_ROOT", dir.join("data"))
        .env("AGENTD_WORKSPACE_ROOT", dir.join("workspace"))
        .env("AGENTD_RUNTIME_DIR", dir.join("run"))
        .stdin(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log_file).unwrap());
    // Process 1 and its worker both end by themselves here, so in a coverage run both write their profile: the lines of the role in
    // `main`, and of the worker that finds itself behind process 1 and says that it is ready.
    with_coverage_profile(&mut command);
    let mut child = command.spawn().unwrap();
    let began = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if began.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "process 1 did not end; log:\n{}",
                std::fs::read_to_string(&log_file).unwrap_or_default()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let log = std::fs::read_to_string(&log_file).unwrap_or_default();
    assert_eq!(code, Some(1), "log:\n{log}");
    assert!(log.contains(&format!("agentd: bind {addr}:")), "{log}");
    assert!(
        log.contains("agentd (process 1): the worker ended (exit 1)"),
        "TRIGGER PRESENT: process 1 collected the worker and ended with its code; log:\n{log}"
    );
    drop(taken);
}

/// The same rule for the rig of the real `agentd serve` (`direct_support::Process`): once process 1 has ended, its drop names no
/// process to end. Here the worker is ended from outside, so process 1 collects it and ends with 128 plus the signal's number.
#[tokio::test(flavor = "multi_thread")]
async fn b883_the_rig_of_agentd_serve_names_no_process_1_once_it_has_ended() {
    if skipped() {
        return;
    }
    let mut d = Direct::start_process(Opts {
        scenario: "writes_files",
        first_process: true,
        // Process 1 ends by itself once its worker has ended, and the test waits for that.
        writes_profile: true,
        ..Opts::default()
    })
    .await;
    let agentd = d.process.as_mut().unwrap();
    let first = agentd.pid();
    assert_eq!(
        agentd.first_to_end(),
        Some(first),
        "TRIGGER PRESENT: while process 1 runs, the drop would end it"
    );
    send("KILL", agentd.worker_pid());
    let began = Instant::now();
    let code = loop {
        if let Some(code) = agentd.ended() {
            break code;
        }
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "process 1 ends once its worker has ended; log:\n{}",
            agentd.log_text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(code, Some(137), "log:\n{}", agentd.log_text());
    assert_eq!(
        agentd.first_to_end(),
        None,
        "a process 1 that has ended names no process to end"
    );
}

/// The stop of a session, with the worker behind process 1: the signal goes to process 1, which passes it on. The worker saves the copy
/// at once and runs on, as process 1 did when it served itself; it still answers, and a second request saves a copy again.
#[tokio::test(flavor = "multi_thread")]
async fn b883_the_worker_behind_process_1_saves_the_stop_copy_and_runs_on() {
    if skipped() {
        return;
    }
    let d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "90".into())],
        agentd_env: vec![
            ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "3600000".to_string()),
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
        ],
        first_process: true,
        ..Opts::default()
    })
    .await;
    let agentd = d.process.as_ref().unwrap();
    assert_eq!(
        agentd.own_pid(),
        Some(1),
        "agentd serve is process 1 of its namespace"
    );
    let worker = agentd.worker_pid();
    let arguments = arguments_of(worker);
    assert!(
        arguments.first().is_some_and(|a| a.ends_with("/agentd"))
            && arguments.get(1).map(String::as_str) == Some("serve")
            && arguments.last().map(String::as_str) == Some(WORKER_FLAG),
        "TRIGGER PRESENT: the first child of process 1 is the worker, a second agentd serve with {WORKER_FLAG}: {arguments:?}"
    );
    assert_eq!(
        own_pid_of(worker),
        Some(2),
        "the worker is not process 1 of its namespace"
    );
    assert!(
        agentd.log_text().contains(
            "agentd (process 1): the worker is process 2; ended processes that are handed to process 1 are collected here\n"
        ),
        "{}",
        agentd.log_text()
    );
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(60), || !records(&d).is_empty()).await,
        "the copy after the engine's first tool call; log:\n{}",
        agentd.log_text()
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    std::fs::write(d.worktree().join("late.txt"), "written before the signal\n").unwrap();
    agentd.signal("TERM");
    let asked = Instant::now();
    // The hop through process 1: from the signal to the worker's own line about it (an upper bound: the log is read every millisecond).
    let mut hop = None;
    while hop.is_none() && asked.elapsed() < Duration::from_secs(20) {
        if agentd.log_text().contains("termination signal SIGTERM:") {
            hop = Some(asked.elapsed());
        } else {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    let stored = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        stored,
        "the worker saved no copy when process 1 was asked to end; log:\n{}",
        agentd.log_text()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        agentd.alive() && alive(worker),
        "process 1 and the worker both run on after the copy; log:\n{}",
        agentd.log_text()
    );
    let (ping, _) = http("GET", &format!("{}/ping", d.base), None).await;
    assert_eq!(ping, 200, "the worker still answers after the copy");
    std::fs::write(
        d.worktree().join("fourth.txt"),
        "written before the second signal\n",
    )
    .unwrap();
    agentd.signal("TERM");
    let again = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "fourth.txt")
    })
    .await;
    let log = agentd.log_text();
    assert!(again, "a second request saves a copy again; log:\n{log}");
    assert!(
        log.contains(
            "termination signal SIGTERM: agentd works behind the first process of its machine and runs on until the machine ends"
        ),
        "{log}"
    );
    assert!(!log.contains("held until the worker is ready"), "{log}");
    eprintln!(
        "COST stop_hop_ms={}",
        hop.map_or("unseen".to_string(), |h| h.as_millis().to_string())
    );
}

/// What the engine is started with does not depend on the role: its environment and its open-files limit are the same behind process 1
/// as under a process 1 that serves itself. (The argument of the worker is no variable, and the worker alone lowers the limit.)
#[tokio::test(flavor = "multi_thread")]
async fn b883_the_engine_sees_the_same_environment_and_open_files_limit_behind_process_1() {
    if skipped() {
        return;
    }
    async fn facts(serves_itself: bool) -> (Vec<String>, String) {
        let d = Direct::start_process(Opts {
            scenario: "writes_files",
            engine_env: vec![("FAKE_ACP_SILENCE_S", "4".into())],
            first_process: true,
            serves_itself,
            ..Opts::default()
        })
        .await;
        let agentd = d.process.as_ref().unwrap();
        let parent = if serves_itself {
            agentd.pid()
        } else {
            agentd.worker_pid()
        };
        let (code, text) = d.post().await;
        assert_eq!(code, 200, "{text}");
        // The engine: the child of the serving agentd that runs the fake engine.
        let mut engine = None;
        let found = wait_for(Duration::from_secs(30), || {
            engine = children_of(parent).into_iter().find(|p| {
                arguments_of(*p)
                    .iter()
                    .any(|a| a.contains("fake-acp-engine"))
            });
            // The probe is whole once its last part is written.
            engine.is_some() && d.engine_probe().contains("--- cline dir")
        })
        .await;
        assert!(
            found,
            "the engine runs below process {parent}; log:\n{}",
            agentd.log_text()
        );
        let limits = std::fs::read_to_string(format!("/proc/{}/limits", engine.unwrap()))
            .unwrap_or_default();
        let open_files = limits
            .lines()
            .find(|l| l.starts_with("Max open files"))
            .unwrap_or_default()
            .to_string();
        let probe = d.engine_probe();
        let env = probe
            .split("--- env\n")
            .nth(1)
            .and_then(|rest| rest.split("--- rules\n").next())
            .unwrap_or_default();
        // The values that name this run's own folders, ports and tag are made equal; every other value is compared as it is.
        let own = [
            (d.dir.display().to_string(), "<run folder>"),
            (
                d.app.cfg.runtime_dir.display().to_string(),
                "<runtime folder>",
            ),
            (d.aws.base.clone(), "<store>"),
        ];
        let lines = env
            .lines()
            .map(|l| {
                if l.starts_with("AGENTD_ENGINE_TAG=") {
                    return "AGENTD_ENGINE_TAG=<the run's tag>".to_string();
                }
                own.iter()
                    .fold(l.to_string(), |line, (from, to)| line.replace(from, to))
            })
            .collect();
        let resp = d.wait_result().await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
        (lines, open_files)
    }
    let (itself_env, itself_files) = facts(true).await;
    let (behind_env, behind_files) = facts(false).await;
    assert!(
        itself_env.len() > 5 && itself_files.starts_with("Max open files"),
        "TRIGGER PRESENT: the engine's environment and limit were read: {} variables, {itself_files:?}",
        itself_env.len()
    );
    assert_eq!(
        behind_files, itself_files,
        "the open-files limit of the engine"
    );
    assert_eq!(behind_env, itself_env, "the environment of the engine");
    assert!(
        !behind_env
            .iter()
            .any(|l| l.contains("behind-first-process")),
        "the worker's argument reaches no variable of the engine"
    );
}

// ---- the sweep of a turn, behind a first process ---------------------------------------------------------------------------------------

use agentd::direct::messages::reap::{Reaper, TAG_ENV};

/// One process of a planted `/proc` tree.
struct Planted<'a> {
    pid: u32,
    name: &'a str,
    parent: u32,
    uid: u32,
    state: char,
    tagged: bool,
    /// The `Seccomp_filters` line of its status; `None` leaves the line out.
    filters: Option<u32>,
}

fn planted(pid: u32, name: &str, parent: u32, uid: u32) -> Planted<'_> {
    Planted {
        pid,
        name,
        parent,
        uid,
        state: 'S',
        tagged: false,
        filters: None,
    }
}

fn plant(root: &Path, p: &Planted<'_>) {
    let d = root.join(p.pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("stat"),
        format!(
            "{} ({}) {} {} 1 1 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {} 123456 100 18446744073709551615\n",
            p.pid,
            p.name,
            p.state,
            p.parent,
            1000 + u64::from(p.pid)
        ),
    )
    .unwrap();
    let filters = p
        .filters
        .map(|n| format!("Seccomp_filters:\t{n}\n"))
        .unwrap_or_default();
    std::fs::write(
        d.join("status"),
        format!(
            "Name:\t{}\nState:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{u}\t{u}\t{u}\t{u}\nGid:\t0\t0\t0\t0\n{filters}",
            p.name,
            p.state,
            p.pid,
            p.parent,
            u = p.uid
        ),
    )
    .unwrap();
    let environ = if p.tagged {
        format!("PATH=/usr/bin\0{TAG_ENV}=run-a\0")
    } else {
        "PATH=/usr/bin\0".to_string()
    };
    std::fs::write(d.join("environ"), environ).unwrap();
}

/// The process ids the sweep selects.
fn selected(r: &Reaper) -> Vec<u32> {
    r.scan().iter().map(|p| p.pid).collect()
}

/// The sweep at the end of a turn selects the engine's family and nothing else when the worker is not process 1: process 1 (the first
/// process, a root `agentd` with the worker's marks), the worker itself and an ended entry that waits for process 1 are never selected,
/// under the mark of the engine's user, and when no engine is marked and a tagged process that an earlier engine left behind has
/// process 1 as its parent. The same under the marks of a sandboxed run is held by
/// `b883_the_sweep_of_a_sandboxed_turn_leaves_process_1_and_the_worker_alone` of `direct_messages_reap_sandboxed.rs`: a test file
/// that reads those marks runs in the `seccomp-children` group of `.config/nextest.toml`, one test at a time, and this file does not.
#[test]
fn b883_the_sweep_of_a_turn_leaves_process_1_and_the_worker_alone() {
    const FIRST: u32 = 1;
    const WORKER: u32 = 7;
    const ENGINE_UID: u32 = 901;

    // The mark of the engine's user.
    let root = common::scratch("b883-proc-user");
    plant(
        &root,
        &Planted {
            filters: Some(0),
            ..planted(FIRST, "agentd", 0, 0)
        },
    );
    plant(
        &root,
        &Planted {
            filters: Some(0),
            ..planted(WORKER, "agentd", FIRST, 0)
        },
    );
    plant(
        &root,
        &Planted {
            tagged: true,
            ..planted(44, "cline", WORKER, ENGINE_UID)
        },
    );
    plant(
        &root,
        &Planted {
            tagged: true,
            ..planted(45, "node", 44, ENGINE_UID)
        },
    );
    // A tool that the engine left to process 1, and an ended entry that process 1 has not collected yet.
    plant(&root, &planted(46, "sleep", FIRST, ENGINE_UID));
    plant(
        &root,
        &Planted {
            state: 'Z',
            ..planted(60, "git", FIRST, ENGINE_UID)
        },
    );
    plant(&root, &planted(500, "sshd", FIRST, 0));
    let r = Reaper::with_root(root.clone(), WORKER, "run-a", Some(ENGINE_UID));
    assert_eq!(
        selected(&r),
        vec![44, 45, 46],
        "TRIGGER PRESENT: the engine's family is selected"
    );
    r.mark_engine();
    assert_eq!(
        r.engine_pids(),
        vec![44],
        "the engine is the worker's child"
    );
    assert!(!r.protect_pid(FIRST), "process 1 is no runtime to protect");

    // No engine is marked, and a tagged process that an earlier engine left behind has process 1 as its parent. It is selected for the
    // sweep and is not taken for the engine: only a child of the worker is.
    let root = common::scratch("b883-proc-leftover");
    plant(&root, &planted(FIRST, "agentd", 0, 0));
    plant(&root, &planted(WORKER, "agentd", FIRST, 0));
    plant(
        &root,
        &Planted {
            tagged: true,
            ..planted(46, "node", FIRST, ENGINE_UID)
        },
    );
    let r = Reaper::with_root(root.clone(), WORKER, "run-a", Some(ENGINE_UID));
    assert_eq!(selected(&r), vec![46]);
    assert_eq!(
        r.engine_pids(),
        Vec::<u32>::new(),
        "a process below process 1 is not the engine"
    );
    r.mark_engine();
    assert_eq!(
        r.engine_pids(),
        Vec::<u32>::new(),
        "and it is no candidate for the mark"
    );
}

// ---- what the first process costs the work in front ----------------------------------------------------------------------------------

use harness_protocol::direct::TestReport;

/// What one turn of the cost scenario took, in milliseconds, and what it gave process 1 to do.
#[derive(Clone, Copy)]
struct Cost {
    /// From the start of the program to its first `/ping` 200.
    start: f64,
    /// From the invocation until agentd is idle again: twenty tool calls with their copies, the end of the turn and the report.
    turn: f64,
    /// The one long command of the turn: the report's `sleep 2`, as the worker timed it.
    long_command: f64,
    /// The calls of the worker's git in the turn. Each one left a detached process behind, which was handed to process 1.
    git_calls: u64,
    /// What became of those processes. With the role: the ended processes that process 1 collected, from its last line. Without it:
    /// the ended entries that stayed with process 1 as their parent.
    handed_over: u64,
}

/// One turn of the scenario `first_process_cost` (twenty edits, the last a test command that sleeps two seconds) under `agentd serve` as
/// process 1 of a namespace: with the first-process role (`role`), or serving itself as.
///
/// The worker's git is the stand-in of [`leaving_git`] in both arms: every call leaves a detached process behind, so each copy of the
/// turn hands ended processes to process 1. With the role process 1 collects them while the turn runs, and that is the work whose cost
/// the test measures. Without it they stay as ended entries. Both are counted after the measured time: `TRIGGER PRESENT` for a turn
/// is that at least one process per edit was handed over, and that process 1 collected every one (with the role) or kept every one
/// as an ended entry (without it).
async fn cost_of_a_turn(role: bool, tool_calls: u64) -> Cost {
    let (bin, calls) = leaving_git(&common::scratch("b883-cost-git"));
    let mut d = Direct::start_process(Opts {
        scenario: "first_process_cost",
        agentd_env: vec![
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ],
        first_process: true,
        serves_itself: !role,
        // The report runs only with enough of the run's time left.
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let agentd = d.process.as_ref().unwrap();
    assert_eq!(
        agentd.own_pid(),
        Some(1),
        "agentd serve is process 1 of its namespace"
    );
    if role {
        let worker = arguments_of(agentd.worker_pid());
        assert!(
            worker.last().map(String::as_str) == Some(WORKER_FLAG),
            "TRIGGER PRESENT: with the role, the first child of process 1 is the worker: {worker:?}"
        );
    } else {
        assert!(
            arguments_of(agentd.pid()).iter().any(|a| a == WORKER_FLAG)
                && !agentd.log_text().contains("agentd (process 1)"),
            "TRIGGER PRESENT: without the role, process 1 serves itself; log:\n{}",
            agentd.log_text()
        );
    }
    let start = agentd.start_took.as_secs_f64() * 1_000.0;
    // The turn: from the invocation until agentd is idle again, which is after the report of the delivered tree. The end is looked for
    // every millisecond. The rig's usual waits look every 50 ms, and on that grid two turns of the same length differ by nothing or by
    // 50 ms, which is neither the noise of the turn nor a cost of the role.
    let close = Duration::from_millis(1);
    let began = Instant::now();
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    let resp = d.wait_result_every(close).await;
    d.wait_healthy_every(close).await;
    let turn = began.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\nlog:\n{}",
        agentd.log_text()
    );
    assert!(
        d.worktree().join("cost/file-19.txt").is_file(),
        "the engine's twenty edits were carried out"
    );
    let report: TestReport = serde_json::from_slice(
        &d.aws
            .get(&keys::test_report_key(RUN))
            .expect("the report of the turn"),
    )
    .unwrap();
    let long_command = report
        .steps
        .first()
        .unwrap_or_else(|| panic!("the report ran its command: {report:?}"))
        .duration_ms as f64;
    assert!(
        long_command >= 2_000.0,
        "TRIGGER PRESENT: the turn ran one command of two seconds: {long_command} ms"
    );

    // What the turn gave process 1 to do. The measured time is over.
    let git_calls = std::fs::read_to_string(&calls)
        .map(|t| t.lines().count() as u64)
        .unwrap_or(0);
    assert!(
        git_calls >= tool_calls,
        "TRIGGER PRESENT: the worker's git ran {git_calls} times in a turn of {tool_calls} edits, and each call leaves a process behind"
    );
    let agentd = d.process.as_mut().unwrap();
    let first = agentd.pid();
    let handed_over = if role {
        // A process that a git call left has ended 0.2 s after that call. Process 1 collects each one. Then the worker is ended from
        // outside, so that process 1 ends too and writes how many it collected.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            wait_for(Duration::from_secs(10), || {
                ended_entries_with_parent(first).is_empty()
            })
            .await,
            "with the role no ended entry stays with process 1 as its parent: {:?}",
            ended_entries_with_parent(first)
        );
        send("KILL", agentd.worker_pid());
        let asked = Instant::now();
        while agentd.ended().is_none() {
            assert!(
                asked.elapsed() < Duration::from_secs(20),
                "process 1 ends once its worker has ended; log:\n{}",
                agentd.log_text()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        counts_of_last_line(&agentd.log_text()).0
    } else {
        // Process 1 serves itself and waits for no process but its own: every process that was handed to it stays as an ended entry.
        wait_for(Duration::from_secs(10), || {
            ended_entries_with_parent(first).len() as u64 >= git_calls
        })
        .await;
        ended_entries_with_parent(first).len() as u64
    };
    assert!(
        handed_over >= git_calls,
        "TRIGGER PRESENT: {git_calls} git calls each left a process behind for process 1, and {handed_over} {}; log:\n{}",
        if role {
            "ended processes were collected by process 1"
        } else {
            "stayed as ended entries of process 1"
        },
        agentd.log_text()
    );
    Cost {
        start,
        turn,
        long_command,
        git_calls,
        handed_over,
    }
}

/// One measure of a turn: its name, how it is read, and whether the test holds a bound for it.
type Measure = (&'static str, fn(&Cost) -> f64, bool);

fn turn_of(c: &Cost) -> f64 {
    c.turn
}

fn long_command_of(c: &Cost) -> f64 {
    c.long_command
}

fn start_of(c: &Cost) -> f64 {
    c.start
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// The step in which a run reports a measure: the smallest difference between two of its values that are not the same; 0 when all are
/// the same. The long command is reported in whole milliseconds, and on a quiet machine two turns often report the same number.
fn step_of(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let step = sorted
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|difference| *difference > 0.0)
        .fold(f64::INFINITY, f64::min);
    if step.is_finite() { step } else { 0.0 }
}

/// How many times the noise of a run a cost must be before the test fails: the usual rule that a difference is real once it is larger
/// than three times the typical noise of the measurement.
const NOISE_FACTOR: f64 = 3.0;

/// The cost test of every build: the same turn with the first-process role and without it, in the same kind of namespace. The
/// turn gives process 1 its work: every git call of the worker hands it an ended process ([`cost_of_a_turn`]).
///
/// A round is four turns: a pair of two turns without the role, and a pair of one turn without it and one with it. The rounds take
/// turns in which pair runs first and which arm of the second pair runs first, so that neither a machine that warms up nor one that
/// slows down favours an arm. Ten rounds give ten differences "with the role less without it" and ten differences between two turns
/// without it. The cost is the median of the first ten. The noise is the median of the absolute values of the second ten, measured in
/// the same run, and never less than the step in which the run reports the measure ([`step_of`]): a measure cannot show a cost that
/// is smaller than its own step. The test fails when the role costs the turn, or its long command, more than [`NOISE_FACTOR`] times
/// that noise; no number of milliseconds is written here. The time to start is printed and not held: the role is one more start of
/// the program, once per session.
///
/// The noise of a run grows with whatever else the machine does, and the bound with it. In a run of many tests this test therefore
/// gets every test thread (a row of `.config/nextest.toml`): no other test of the run works beside the turns it times.
#[tokio::test(flavor = "multi_thread")]
async fn b883_cost_a_turn_behind_process_1_against_process_1_serving_itself() {
    if skipped() {
        return;
    }
    const PAIRS: usize = 10;
    let scenario = common::repo_root()
        .join("fixtures/engines/fake-acp/transcripts/direct/first_process_cost/turn-1.jsonl");
    let tool_calls = std::fs::read_to_string(&scenario)
        .unwrap()
        .matches("\"sessionUpdate\": \"tool_call\"")
        .count() as u64;
    assert!(
        tool_calls >= 20,
        "TRIGGER PRESENT: the scenario has {tool_calls} tool calls"
    );
    // (without the role, without the role) and (without the role, with the role).
    let (mut quiet, mut measured) = (Vec::new(), Vec::new());
    for round in 0..PAIRS {
        if round % 2 == 0 {
            let a = cost_of_a_turn(false, tool_calls).await;
            let b = cost_of_a_turn(false, tool_calls).await;
            quiet.push((a, b));
            let off = cost_of_a_turn(false, tool_calls).await;
            let on = cost_of_a_turn(true, tool_calls).await;
            measured.push((off, on));
        } else {
            let on = cost_of_a_turn(true, tool_calls).await;
            let off = cost_of_a_turn(false, tool_calls).await;
            measured.push((off, on));
            let a = cost_of_a_turn(false, tool_calls).await;
            let b = cost_of_a_turn(false, tool_calls).await;
            quiet.push((a, b));
        }
    }
    let with_role: Vec<&Cost> = measured.iter().map(|(_, on)| on).collect();
    let without: Vec<&Cost> = measured
        .iter()
        .map(|(off, _)| off)
        .chain(quiet.iter().flat_map(|(a, b)| [a, b]))
        .collect();
    let least =
        |turns: &[&Cost], of: fn(&Cost) -> u64| turns.iter().map(|c| of(c)).min().unwrap_or(0);
    eprintln!(
        "TRIGGER PRESENT: every one of the {} turns handed ended processes to process 1, one for each call of the worker's git (at least {} calls in a turn); with the role process 1 collected them (at least {} in a turn), without it they stayed as ended entries (at least {} in a turn)",
        with_role.len() + without.len(),
        least(&with_role, |c| c.git_calls).min(least(&without, |c| c.git_calls)),
        least(&with_role, |c| c.handed_over),
        least(&without, |c| c.handed_over),
    );
    let mut held = Vec::new();
    let measures: [Measure; 3] = [
        ("turn_ms", turn_of, true),
        ("long_command_ms", long_command_of, true),
        ("start_ms", start_of, false),
    ];
    for (name, of, bound) in measures {
        let values: Vec<f64> = quiet
            .iter()
            .chain(&measured)
            .flat_map(|(a, b)| [of(a), of(b)])
            .collect();
        let step = step_of(&values);
        let between_quiet: Vec<f64> = quiet.iter().map(|(a, b)| (of(a) - of(b)).abs()).collect();
        let with_less_without: Vec<f64> =
            measured.iter().map(|(off, on)| of(on) - of(off)).collect();
        // The ten differences of each kind, for whoever reads a run that failed or came close.
        let listed = |values: &[f64]| {
            values
                .iter()
                .map(|v| format!("{v:.1}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        eprintln!(
            "COST-PAIRS {name} with_less_without=[{}] between_two_without=[{}]",
            listed(&with_less_without),
            listed(&between_quiet)
        );
        // The noise is never taken as less than the step in which the measure is reported: a time reported in whole milliseconds
        // cannot show noise below one step.
        let noise = median(between_quiet).max(step);
        let more = median(with_less_without);
        let limit = NOISE_FACTOR * noise;
        eprintln!(
            "COST {name} off={:.0} on={:.0} more={more:.1} noise={noise:.1} limit={limit:.1} step={step:.3} n={PAIRS}",
            median(measured.iter().map(|(off, _)| of(off)).collect()),
            median(measured.iter().map(|(_, on)| of(on)).collect()),
        );
        if bound && more > limit {
            held.push(format!(
                "{name}: the role costs {more:.1} ms in the median of {PAIRS} pairs, more than {NOISE_FACTOR} times the noise of this run ({noise:.1} ms: the median difference between two turns without the role, or the step of the measure when that is larger)"
            ));
        }
    }
    assert!(held.is_empty(), "{held:?}");
}

// ---- the uploads of a turn of the cost scenario do not wait for an acknowledgement ------------------------------------------

use direct_support::{Arrival, BUCKET, FakeAws};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long a step of the test of the uploads may take before the test gives up. It ends a test that hangs; nothing is judged by it.
const WAIT_LIVENESS: Duration = Duration::from_secs(60);
/// The requests of each control client that are judged, after [`WAIT_WARM_UPS`] that are not.
const WAIT_ROUNDS: usize = 15;
const WAIT_WARM_UPS: usize = 3;
/// Two thirds of the judged requests of a control client: how many must say the same for the cause to count as shown.
const WAIT_NEED: usize = (2 * WAIT_ROUNDS).div_ceil(3);
/// The cause is present in a run when the client with Nagle's algorithm on takes at least this many times as long as the client with
/// no-delay.
const WAIT_FACTOR: u32 = 10;
/// The turns of the cost scenario the test looks at. The first half run as the cost test's turns do, the second half with the worker
/// on one processor; each half has turns with the first-process role and without it.
const WAIT_TURNS: usize = 6;
/// Requests that waited in this many of the turns fail the test. A wait in one turn only can be a pause of the machine and is
/// printed; a wait in a second turn is the wait.
const WAIT_TURNS_NEED: usize = 2;

/// A client of the test's own on one kept connection of the stand-in. It sends a PUT as two writes: the head and, once the stand-in
/// has read the head, the body.
struct TwoWrites {
    stream: tokio::net::TcpStream,
    host: String,
    name: &'static str,
    sent: usize,
}

impl TwoWrites {
    async fn open(aws: &FakeAws, no_delay: bool, name: &'static str) -> Self {
        let host = aws.base.trim_start_matches("http://").to_string();
        let stream = tokio::net::TcpStream::connect(&host).await.unwrap();
        stream.set_nodelay(no_delay).unwrap();
        assert_eq!(stream.nodelay().unwrap(), no_delay);
        Self {
            stream,
            host,
            name,
            sent: 0,
        }
    }

    /// One request. How long the first byte of the body came after the head, as the stand-in read them.
    async fn put(&mut self, aws: &FakeAws, body: &[u8]) -> Duration {
        let path = format!("/{BUCKET}/b965-control/{}/{}", self.name, self.sent);
        self.sent += 1;
        // The head is about as long as a signed head of the worker's.
        let head = format!(
            "PUT {path} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\nx-b965-fill: {}\r\n\r\n",
            self.host,
            body.len(),
            "a".repeat(600)
        );
        let before = aws.heads_seen();
        self.stream.write_all(head.as_bytes()).await.unwrap();
        tokio::time::timeout(WAIT_LIVENESS, aws.head_seen(before))
            .await
            .expect("the stand-in did not read the head");
        self.stream.write_all(body).await.unwrap();
        let mut answer = Vec::new();
        let mut chunk = [0_u8; 512];
        while !answer.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::time::timeout(WAIT_LIVENESS, self.stream.read(&mut chunk))
                .await
                .expect("no answer of the stand-in")
                .unwrap();
            assert!(n > 0, "the stand-in closed the connection of {}", self.name);
            answer.extend_from_slice(&chunk[..n]);
        }
        assert!(
            answer.starts_with(b"HTTP/1.1 200 "),
            "{}",
            String::from_utf8_lossy(&answer)
        );
        let seen = aws
            .arrivals()
            .into_iter()
            .rev()
            .find(|a| a.path == path)
            .unwrap_or_else(|| panic!("the stand-in noted no request {path}"));
        assert_eq!(
            (seen.with_head, seen.body),
            (0, body.len()),
            "the head came alone and the body whole: {seen:?}"
        );
        seen.body_began_after
    }
}

fn median_of(of: &[Duration]) -> Duration {
    let mut sorted = of.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
}

fn micros_of(of: &[Duration]) -> Vec<u128> {
    of.iter().map(Duration::as_micros).collect()
}

/// One turn of the cost scenario, started as [`cost_of_a_turn`] starts it, against a stand-in that holds its acknowledgements back:
/// every request of the worker as its bytes came in. With `one_processor` the worker runs on one processor of the machine.
async fn arrivals_of_a_cost_turn(role: bool, one_processor: bool) -> Vec<Arrival> {
    let (bin, _) = leaving_git(&common::scratch("b965-wait-git"));
    let d = Direct::start_process(Opts {
        scenario: "first_process_cost",
        agentd_env: vec![
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ],
        first_process: true,
        serves_itself: !role,
        timeout_s: 3600,
        holds_acks: true,
        one_processor,
        ..Opts::default()
    })
    .await;
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    let resp = d.wait_result().await;
    d.wait_healthy().await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\nlog:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    assert_eq!(
        d.aws.stand_in_faults(),
        (0, 0),
        "the stand-in asked the kernel to hold every acknowledgement back and cut every request out of the bytes it read"
    );
    d.aws.arrivals()
}

/// The hardening of the cost test above. The cost test times a turn from the invocation to the idle worker, and the end of a
/// turn is a row of uploads. Until two of them waited for the acknowledgement of their head in most turns: a turn's time then
/// sat on levels one such wait apart, each median of the cost test landed on a level by the split of its ten values, and its pass or
/// its failure said nothing about the first-process role. This test fails when the requests of such a turn wait again, whatever
/// client of the worker sends them.
///
/// It runs [`WAIT_TURNS`] turns of the cost scenario, started as the cost test starts them, half of them with the role, each against
/// a stand-in that holds its acknowledgements back and notes how the bytes of every request came in
/// (`FakeAws::start_holding_acks`). Against that stand-in a client with Nagle's algorithm on waits for the acknowledgement of its
/// head when it sends a request as two writes on a kept connection, and a client with no-delay does not. Two clients of the test's
/// own show both in the same run (`TRIGGER PRESENT`), and half of the median of the first parts a request that waited from one that
/// did not: no time is typed in.
///
/// Each turn must have sent the two uploads that waited on a kept connection. Requests that waited in
/// [`WAIT_TURNS_NEED`] of the turns fail the test with a text that names the wait and the requests; a wait in one turn only is
/// printed. The cost test itself is as it was.
///
/// Whether a worker without no-delay waits in a turn is a race inside the worker, and its outcome differs from machine to machine
/// and with what else the machine does: an upload leaves as two writes only when the first piece of its file is not read yet at the
/// moment the head is written; when the piece is there, head and body leave together and nothing can wait. So the test looks at
/// two kinds of turn. The first half of its turns run as the cost test's do, and the test gets every test thread of a run, as the
/// cost test does (a row of `.config/nextest.toml`): those turns meet what the turns of the cost test meet on the same machine. In
/// the second half the worker runs on one processor (`Opts::one_processor`). Measured without no-delay: on one quiet machine the
/// uploads waited in 19 of 20 turns of the first kind and in 9 of 10 of the second; on another quiet machine in 1 of 80 turns of
/// the first kind and in every turn of the second; on that second machine crowded, in every turn of both kinds. It is no test of
/// the option itself: `tests/b970_no_delay.rs` holds that on every machine.
#[tokio::test(flavor = "multi_thread")]
async fn b965_the_uploads_of_a_cost_turn_do_not_wait_for_the_acknowledgement_of_their_head() {
    if skipped() {
        return;
    }
    // The two control clients, in turns, against a stand-in of the kind the turns below get.
    let aws = FakeAws::start_holding_acks().await;
    let body = vec![b'{'; 176];
    let mut nagle = TwoWrites::open(&aws, false, "nagle").await;
    let mut no_delay = TwoWrites::open(&aws, true, "no-delay").await;
    for _ in 0..WAIT_WARM_UPS {
        nagle.put(&aws, &body).await;
        no_delay.put(&aws, &body).await;
    }
    let (mut n, mut d) = (Vec::new(), Vec::new());
    for round in 0..WAIT_ROUNDS {
        if round % 2 == 0 {
            n.push(nagle.put(&aws, &body).await);
            d.push(no_delay.put(&aws, &body).await);
        } else {
            d.push(no_delay.put(&aws, &body).await);
            n.push(nagle.put(&aws, &body).await);
        }
    }
    assert_eq!(aws.stand_in_faults(), (0, 0));
    let half = median_of(&n) / 2;
    let n_waited = n.iter().filter(|took| **took >= half).count();
    let d_waited = d.iter().filter(|took| **took >= half).count();
    eprintln!(
        "B965 control body_after_head_us nagle={:?} no_delay={:?} half_of_nagle_median_us={} waited nagle={n_waited} no_delay={d_waited} of {WAIT_ROUNDS}",
        micros_of(&n),
        micros_of(&d),
        half.as_micros(),
    );
    // TRIGGER PRESENT: on this machine, in this run, against this kind of stand-in, a client that keeps Nagle's algorithm on waits
    // for the acknowledgement of its head and a client with no-delay does not.
    assert!(
        median_of(&n) >= median_of(&d) * WAIT_FACTOR
            && n_waited >= WAIT_NEED
            && d_waited <= WAIT_ROUNDS - WAIT_NEED,
        "UPLOAD WAIT TRIGGER ABSENT: a client with Nagle's algorithm on did not wait for the acknowledgement of its head in this \
         run (median {:?}, {n_waited} of {WAIT_ROUNDS} requests waited) where a client with no-delay took {:?} in the median \
         ({d_waited} waited): this run can say nothing about the uploads of a turn",
        median_of(&n),
        median_of(&d),
    );

    // The turns: without the role, with it, with it, without it, and so on.
    let mut waits: std::collections::BTreeMap<String, Vec<(usize, Duration)>> =
        std::collections::BTreeMap::new();
    let mut turns_with_a_wait = Vec::new();
    for turn in 0..WAIT_TURNS {
        let role = turn % 4 == 1 || turn % 4 == 2;
        // The first half as the cost test starts a turn, the second half with the worker on one processor.
        let one_processor = turn >= WAIT_TURNS / 2;
        let arrivals = arrivals_of_a_cost_turn(role, one_processor).await;
        // A request can show the wait when it has a body and came on a kept connection.
        let judged: Vec<&Arrival> = arrivals
            .iter()
            .filter(|a| a.body > 0 && a.nth >= 1)
            .collect();
        let alone: Vec<&&Arrival> = judged.iter().filter(|a| a.with_head == 0).collect();
        let named = |end: &str| {
            judged
                .iter()
                .filter(|a| a.method == "PUT" && a.path.ends_with(end))
                .count()
        };
        // TRIGGER PRESENT: the turn sent the two uploads that waited, the record of the copy of the end and the
        // session's summary, each on a kept connection, where a client with Nagle's algorithm on waits. Whether a request came as a
        // head without its body says nothing here: with no-delay the two writes follow each other so closely that one read of the
        // stand-in often brings both.
        assert!(
            named("/snapshot.json") >= 1 && named("/summary.json") >= 1,
            "UPLOAD WAIT TRIGGER ABSENT: turn {turn} (role {role}) sent {} requests with a body on a kept connection, {} PUT of \
             snapshot.json and {} of summary.json among them: {:?}",
            judged.len(),
            named("/snapshot.json"),
            named("/summary.json"),
            judged
                .iter()
                .map(|a| format!(
                    "{} {} nth={} with_head={}",
                    a.method, a.path, a.nth, a.with_head
                ))
                .collect::<Vec<_>>()
        );
        let mut waited_here = std::collections::BTreeMap::new();
        for a in &alone {
            if a.body_began_after >= half {
                // One entry a kind and turn: the longest.
                let kind = format!("{} {}", a.method, a.path);
                let longest: &mut Duration = waited_here.entry(kind).or_default();
                *longest = (*longest).max(a.body_began_after);
            }
        }
        eprintln!(
            "B965 turn={turn} role={role} requests={} with_a_body_on_a_kept_connection={} head_alone={} waited={:?} longest_body_after_head_us={} one_processor={one_processor}",
            arrivals.len(),
            judged.len(),
            alone.len(),
            waited_here
                .iter()
                .map(|(k, v)| format!("{k} {}us", v.as_micros()))
                .collect::<Vec<_>>(),
            alone
                .iter()
                .map(|a| a.body_began_after.as_micros())
                .max()
                .unwrap_or(0),
        );
        if !waited_here.is_empty() {
            turns_with_a_wait.push(turn);
        }
        for (kind, took) in waited_here {
            waits.entry(kind).or_default().push((turn, took));
        }
    }
    let waited: Vec<String> = waits
        .iter()
        .map(|(kind, turns)| {
            format!(
                "{kind} in {} of {WAIT_TURNS} turns ({})",
                turns.len(),
                turns
                    .iter()
                    .map(|(turn, took)| format!("turn {turn}: {took:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    eprintln!(
        "B965 turns_with_a_waiting_request={turns_with_a_wait:?} of {WAIT_TURNS} (the test fails from {WAIT_TURNS_NEED}) waited={waited:?}"
    );
    assert!(
        turns_with_a_wait.len() < WAIT_TURNS_NEED,
        "UPLOAD WAIT: in {} of {WAIT_TURNS} turns of the cost scenario requests of the worker waited for the acknowledgement of \
         their head: {waited:?}. In this run a client with Nagle's algorithm on took {:?} from its head to the first byte of its \
         body and a client with no-delay {:?}. While the requests of a turn wait like this, the turn times of the cost test \
         (b883_cost_a_turn_behind_process_1_against_process_1_serving_itself) sit on levels one such wait apart, and neither a \
         pass nor a failure of that test says anything about the first-process role.",
        turns_with_a_wait.len(),
        median_of(&n),
        median_of(&d),
    );
}
