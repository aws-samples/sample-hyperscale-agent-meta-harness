//! Evidence that a command is running: the processes of the engine's user in `/proc`, less the ones that were there when the session
//! began (the engine, its helpers), and whether they used CPU since the last sample. A command counts as one the model waits for only if its
//! session is older than the call: the probe reports how old the sessions are that used the CPU (`Command::Active { age }`), and the watch
//! compares that age with the age of each tool call (`direct_goal_watch.rs`). The tests build a `/proc` of their own in a scratch directory,
//! so the rules (baseline, deltas per process and start time, the rate, other users, zombies, sessions and their ages, an unreadable root) are
//! exact. The fake machine's clock is the file `uptime`, which each sample writes first.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::goal::command::{CommandProbe, ProcProbe, parse_stat};
use agentd::direct::goal::watch::Command;
use common::scratch;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ENGINE_UID: u32 = 4242;

/// The uptime of the fake machine, in clock ticks (10 ms), when a test begins: 10,000 seconds. A process that `born(s)` started `s` seconds
/// after that; one that is not `born` started long before.
const BOOT: u64 = 1_000_000;

/// One process of the fake `/proc`: `stat` with the fields the probe reads at their places and `status` with the user.
struct P {
    pid: u32,
    uid: u32,
    state: char,
    start: u64,
    /// The session the process belongs to; by default its own (a session leader).
    session: u32,
    utime: u64,
    stime: u64,
    cutime: u64,
    cstime: u64,
    /// The command name as the kernel prints it in `stat`: raw bytes, cut at 15 bytes, so a long non-ASCII name can end in half a character.
    comm: Vec<u8>,
    /// The same name on the `Name:` line of `status`.
    name: Vec<u8>,
}

impl P {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            uid: ENGINE_UID,
            state: 'S',
            start: 1000 + u64::from(pid),
            session: pid,
            utime: 0,
            stime: 0,
            cutime: 0,
            cstime: 0,
            comm: b"a b) c".to_vec(),
            name: b"x".to_vec(),
        }
    }
    fn cpu(mut self, utime: u64) -> Self {
        self.utime = utime;
        self
    }
    /// Started `s` seconds after the test began.
    fn born(mut self, s: u64) -> Self {
        self.start = BOOT + s * 100;
        self
    }
    fn session(mut self, sid: u32) -> Self {
        self.session = sid;
        self
    }
    fn write(&self, root: &Path) {
        let dir = root.join(self.pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime prio nice threads itreal starttime ...
        let mut stat = format!("{} (", self.pid).into_bytes();
        stat.extend_from_slice(&self.comm);
        stat.extend_from_slice(
            format!(
                ") {} 1 {} {} 0 -1 4194560 10 0 0 0 {} {} {} {} 20 0 1 0 {} 123456 77 18446744073709551615",
                self.state,
                self.pid,
                self.session,
                self.utime,
                self.stime,
                self.cutime,
                self.cstime,
                self.start
            )
            .as_bytes(),
        );
        std::fs::write(dir.join("stat"), stat).unwrap();
        let mut status = b"Name:\t".to_vec();
        status.extend_from_slice(&self.name);
        status.extend_from_slice(
            format!(
                "\nUmask:\t0022\nUid:\t{0}\t{0}\t{0}\t{0}\nGid:\t1\t1\t1\t1\n",
                self.uid
            )
            .as_bytes(),
        );
        std::fs::write(dir.join("status"), status).unwrap();
    }
}

/// A fake `/proc`, the probe over it, and the clock of both.
struct Rig {
    root: PathBuf,
    p: ProcProbe,
    t0: Instant,
}

impl Rig {
    fn new(tag: &str) -> Self {
        Self::with_pct(tag, 5)
    }

    fn with_pct(tag: &str, pct: u32) -> Self {
        let root = scratch(tag).join("proc");
        std::fs::create_dir_all(&root).unwrap();
        let p = ProcProbe::with_root(root.clone(), ENGINE_UID, pct);
        let r = Rig {
            root,
            p,
            t0: Instant::now(),
        };
        r.clock(0);
        r
    }

    /// The machine's uptime, `s` seconds into the test.
    fn clock(&self, s: u64) {
        let ticks = BOOT + s * 100;
        std::fs::write(
            self.root.join("uptime"),
            format!("{}.{:02} 99.99\n", ticks / 100, ticks % 100),
        )
        .unwrap();
    }

    /// A sample `s` seconds into the test.
    fn sample(&self, s: u64) -> Command {
        self.clock(s);
        self.p.sample(self.t0 + Duration::from_secs(s))
    }

    /// A sample at an instant that is no whole second.
    fn sample_ms(&self, ms: u64) -> Command {
        self.clock(ms / 1000);
        self.p.sample(self.t0 + Duration::from_millis(ms))
    }
}

fn active(age_s: u64) -> Command {
    Command::Active {
        age: Duration::from_secs(age_s),
    }
}

fn is_active(c: Command) -> bool {
    matches!(c, Command::Active { .. })
}

#[test]
fn stat_is_read_after_a_name_that_has_spaces_and_parentheses() {
    let s = parse_stat(
        "77 (we(ird) na me) R 1 77 78 0 -1 4194560 10 0 0 0 31 12 4 3 20 0 1 0 987654 123456 77 99",
    )
    .unwrap();
    assert_eq!(
        (s.state, s.session, s.start, s.ticks),
        ('R', 78, 987_654, 31 + 12),
        "the session is the fourth field after the name, and the ticks are the process's own (user and system)"
    );
    assert!(
        parse_stat("77 (x) R 1 2").is_none(),
        "a short line is no process"
    );
    assert!(parse_stat("garbage").is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn the_real_proc_of_this_process_parses_and_its_cpu_ticks_rise_with_work() {
    let read = || parse_stat(std::fs::read("/proc/self/stat").unwrap()).unwrap();
    let before = read();
    let t0 = Instant::now();
    let mut x = 0u64;
    while t0.elapsed() < Duration::from_millis(400) {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
    }
    assert_ne!(x, 1);
    let after = read();
    assert_eq!(
        before.start, after.start,
        "the start time identifies the process"
    );
    assert_eq!(before.session, after.session);
    assert!(
        after.ticks > before.ticks,
        "{} -> {}",
        before.ticks,
        after.ticks
    );
}

#[test]
fn the_first_sample_takes_the_baseline_and_says_nothing() {
    let r = Rig::new("probe-baseline");
    P::new(10).cpu(500).write(&r.root);
    assert_eq!(r.sample(0), Command::Unknown);
    P::new(10).cpu(900).write(&r.root);
    assert_eq!(
        r.sample(5),
        Command::Idle,
        "a process of the baseline never counts, however much CPU it uses"
    );
}

#[test]
fn a_new_process_that_uses_a_twentieth_of_a_core_or_more_makes_the_interval_active() {
    let r = Rig::new("probe-active");
    P::new(10).write(&r.root);
    r.sample(0);
    // 5 seconds at 5 percent of one core is 25 ticks at 100 Hz
    P::new(11).born(1).cpu(24).write(&r.root);
    assert_eq!(r.sample(5), Command::Idle, "24 ticks in 5 s");
    P::new(11).born(1).cpu(24 + 25).write(&r.root);
    assert!(is_active(r.sample(10)), "25 more ticks in the next 5 s");
}

#[test]
fn a_new_process_counts_from_zero_and_a_known_one_by_its_change() {
    let r = Rig::new("probe-delta");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(11).born(2).cpu(100).write(&r.root);
    assert!(
        is_active(r.sample(5)),
        "it was born in this interval and has used 100 ticks"
    );
    // the same process, 10 ticks more: the 100 it had are not counted again
    P::new(11).born(2).cpu(110).write(&r.root);
    assert_eq!(r.sample(10), Command::Idle);
}

// Changed the test that stood here (`ticks_of_waited_for_children_do_not_count`). It pinned that the CPU of children a process has waited for
// (cutime and cstime) is left out, so that a foreground command's shell would not look busy. A foreground command's shell is a session of its
// own that is as young as its call, so the age rule keeps that call counted whichever counter the ticks are in (the tests of the watch and
// `direct_goal_command_run.rs`).

#[test]
fn the_cpu_of_children_a_process_waited_for_counts_once_they_are_gone() {
    let r = Rig::new("probe-children");
    P::new(10).write(&r.root);
    r.sample(0);
    let mut job = P::new(12).born(1);
    job.cutime = 400;
    job.cstime = 100;
    job.write(&r.root);
    assert_eq!(
        r.sample(5),
        active(4),
        "a new process whose waited-for children used 500 ticks in this interval; its session is 4 s old"
    );
    assert_eq!(
        r.sample(10),
        Command::Idle,
        "the same numbers again: nothing new was used"
    );
    job.cutime = 400 + 30;
    job.cstime = 100 + 20;
    job.write(&r.root);
    assert!(
        is_active(r.sample(15)),
        "50 more ticks in children that have exited since the last sample"
    );
    job.utime = 24;
    job.write(&r.root);
    assert_eq!(
        r.sample(20),
        Command::Idle,
        "24 own ticks in 5 s are below the 25 of the rate, as before"
    );
}

#[test]
fn the_system_time_of_waited_for_children_counts_too() {
    // A test run that starts a process per test spends much of its CPU in the kernel: the children's system time (`cstime`) is part of it.
    let r = Rig::new("probe-children-sys");
    P::new(10).write(&r.root);
    r.sample(0);
    let mut job = P::new(12).born(1);
    job.cstime = 500;
    job.write(&r.root);
    assert_eq!(
        r.sample(5),
        active(4),
        "500 ticks of system time in children that have exited, none of user time"
    );
}

#[test]
fn a_job_that_runs_one_short_child_after_the_other_is_active_in_every_interval() {
    // The shape of a serial test run: the python process that runs the tests waits for a child, the child is gone before a sample sees it, and
    // the next one starts. The parent's own ticks stay at a handful.
    let r = Rig::new("probe-short-children");
    P::new(10).write(&r.root);
    r.sample(0);
    let mut runner = P::new(20).born(1);
    for (i, s) in (5..=60).step_by(5).enumerate() {
        runner.utime = 1 + u64::try_from(i).unwrap();
        runner.cutime = 400 * (u64::try_from(i).unwrap() + 1);
        runner.write(&r.root);
        assert!(
            is_active(r.sample(s)),
            "at second {s}: 400 ticks of exited children in 5 s (80 percent of a CPU)"
        );
    }
}

#[test]
fn a_reused_pid_is_another_process() {
    let r = Rig::new("probe-reuse");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(11).born(1).cpu(500).write(&r.root);
    assert!(is_active(r.sample(5)));
    // pid 11 again, started later, with 30 ticks: not the 500 it is compared with, and not a negative number
    P::new(11).born(7).cpu(30).write(&r.root);
    assert!(
        is_active(r.sample(10)),
        "a new process with 30 ticks in 5 s"
    );
}

#[test]
fn processes_that_left_add_nothing_and_a_negative_change_is_not_a_command() {
    let r = Rig::new("probe-gone");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(11).born(1).cpu(200).write(&r.root);
    assert!(is_active(r.sample(5)));
    std::fs::remove_dir_all(r.root.join("11")).unwrap();
    assert_eq!(r.sample(10), Command::Idle);
}

#[test]
fn other_users_and_dead_processes_do_not_count() {
    let r = Rig::new("probe-others");
    P::new(10).write(&r.root);
    r.sample(0);
    let mut other = P::new(20).born(1).cpu(900);
    other.uid = 0;
    other.write(&r.root);
    let mut zombie = P::new(21).born(1).cpu(900);
    zombie.state = 'Z';
    zombie.write(&r.root);
    std::fs::create_dir_all(r.root.join("self")).unwrap();
    std::fs::write(r.root.join("cpuinfo"), "x").unwrap();
    assert_eq!(r.sample(5), Command::Idle);
}

#[test]
fn a_root_that_cannot_be_read_is_unknown() {
    let p = ProcProbe::with_root(
        PathBuf::from("/nonexistent/proc-for-the-test"),
        ENGINE_UID,
        5,
    );
    assert_eq!(p.sample(Instant::now()), Command::Unknown);
}

#[test]
fn a_machine_whose_uptime_cannot_be_read_gives_no_evidence() {
    // Without the uptime a process has no age, and a command that cannot be told from the model's own work protects nothing.
    let r = Rig::new("probe-no-uptime");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(11).born(1).cpu(900).write(&r.root);
    std::fs::remove_file(r.root.join("uptime")).unwrap();
    assert_eq!(r.p.sample(r.t0 + Duration::from_secs(5)), Command::Unknown);
    // the failed sample changed nothing: the process is new to the next one, and is counted from zero
    assert!(is_active(r.sample(10)));
}

#[test]
fn a_new_baseline_takes_in_the_processes_of_the_moment() {
    let r = Rig::new("probe-rebaseline");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(11).born(1).cpu(300).write(&r.root);
    assert!(is_active(r.sample(5)));
    // the engine started again (a new session): everything there now belongs to it
    r.p.rebaseline();
    assert_eq!(r.sample(10), Command::Unknown);
    P::new(11).born(1).cpu(900).write(&r.root);
    assert_eq!(r.sample(15), Command::Idle);
    P::new(13).born(16).cpu(300).write(&r.root);
    assert!(is_active(r.sample(20)));
}

#[test]
fn the_rate_follows_the_length_of_the_interval() {
    let r = Rig::new("probe-rate");
    P::new(10).write(&r.root);
    r.sample(0);
    // a quiet minute: 5 percent of 60 s is 300 ticks
    P::new(11).born(1).cpu(299).write(&r.root);
    assert_eq!(r.sample(60), Command::Idle);
    P::new(11).born(1).cpu(299 + 300).write(&r.root);
    assert!(is_active(r.sample(120)));
}

#[test]
fn a_share_that_is_no_whole_number_of_ticks_is_rounded_up() {
    // 1 percent of 5.5 s is 5.5 ticks: 5 are not enough, 6 are
    let r = Rig::with_pct("probe-ceil", 1);
    P::new(10).write(&r.root);
    r.sample_ms(0);
    P::new(11).born(1).cpu(5).write(&r.root);
    assert_eq!(r.sample_ms(5500), Command::Idle, "5 ticks in 5.5 s at 1%");
    P::new(11).born(1).cpu(5 + 6).write(&r.root);
    assert!(
        is_active(r.sample_ms(11000)),
        "6 ticks in the next 5.5 s at 1%"
    );
}

#[test]
fn a_process_whose_stat_name_is_not_valid_utf8_is_still_seen() {
    // The kernel prints the command name raw and cuts it at 15 bytes: a long Cyrillic name ends in half a character, and so does any
    // longer non-ASCII name. A text read of `stat` fails on it and the process, a running command, would be skipped.
    let r = Rig::new("probe-comm-bytes");
    P::new(10).write(&r.root);
    r.sample(0);
    let mut weird = P::new(11).born(1).cpu(500);
    weird.comm = b"\xd0".to_vec();
    weird.write(&r.root);
    let mut weirder = P::new(12).born(1).cpu(500);
    weirder.comm = b"te st\xff\xfe)(".to_vec();
    weirder.write(&r.root);
    assert!(
        is_active(r.sample(5)),
        "busy processes with names that are not text are commands"
    );
}

#[test]
fn a_process_whose_status_name_is_not_valid_utf8_is_still_seen_and_still_read_for_its_user() {
    // `status` prints the same name raw on its `Name:` line; its `Uid:` line has to be read all the same.
    let r = Rig::new("probe-name-bytes");
    P::new(10).write(&r.root);
    r.sample(0);
    let mut mine = P::new(11).born(1).cpu(500);
    mine.name = b"\xd0\xd1".to_vec();
    mine.write(&r.root);
    let mut other = P::new(12).born(1).cpu(900);
    other.name = b"\xd0".to_vec();
    other.uid = 0;
    other.write(&r.root);
    assert!(is_active(r.sample(5)), "the engine user's");
    // only the other user's process is busy now
    let mut other = P::new(12).born(1).cpu(1900);
    other.name = b"\xd0".to_vec();
    other.uid = 0;
    other.write(&r.root);
    assert_eq!(
        r.sample(10),
        Command::Idle,
        "another user's process does not count, whatever its name"
    );
}

// ---- how old the processes are that used the CPU (the review of the watch: a foreground command is not a command the model waits for) ----
//
// Cline starts each command of the model as a child of its own, in a session of its own (the shell is a fresh one for each call). So a
// foreground command is as old as its call, a job the model started earlier and polls is older than the polls, and the CPU the job's children
// use belongs to the job's session whatever the age of the child.

#[test]
fn a_job_has_the_age_of_its_session_and_the_children_that_burn_the_cpu_share_it() {
    // The leader is a script that uses almost no CPU; its child is a test run that started minutes later, in the same session.
    let r = Rig::new("probe-job-age");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(1).write(&r.root);
    P::new(21).session(20).born(100).cpu(2000).write(&r.root);
    assert_eq!(
        r.sample(105),
        active(104),
        "the session began at second 1, 104 s before the sample"
    );
}

#[test]
fn a_foreground_command_is_as_young_as_its_own_session() {
    let r = Rig::new("probe-foreground-age");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(30).born(100).cpu(900).write(&r.root);
    assert_eq!(r.sample(105), active(5), "started 5 s before the sample");
}

#[test]
fn the_age_is_that_of_the_session_the_cpu_threshold_is_reached_in_oldest_first() {
    // An old session that used almost nothing does not lend its age to a young session's CPU...
    let r = Rig::new("probe-oldest-first");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(1).write(&r.root);
    P::new(30).born(100).cpu(900).write(&r.root);
    assert_eq!(r.sample(105), active(5));
    // ... and an old session that used enough on its own gives its age, whatever the young one does.
    P::new(20).born(1).cpu(1 + 100).write(&r.root);
    P::new(30).born(100).cpu(900 + 900).write(&r.root);
    assert_eq!(r.sample(110), active(109));
    // The CPU of the sessions is added, oldest first: 15 ticks of an old session and 15 of a young one reach the 25 of a 5 s interval at the
    // young one, so the age is the young one's.
    P::new(20).born(1).cpu(1 + 100 + 15).write(&r.root);
    P::new(30).born(100).cpu(900 + 900 + 15).write(&r.root);
    assert_eq!(r.sample(115), active(15));
}

#[test]
fn a_session_whose_leader_is_gone_has_the_age_of_its_oldest_member() {
    // `nohup script &` from a shell that ends: the shell was the leader, the script and its children are the session.
    let r = Rig::new("probe-leaderless");
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(21).session(20).born(10).cpu(1).write(&r.root);
    P::new(22).session(20).born(50).cpu(2000).write(&r.root);
    assert_eq!(r.sample(60), active(50));
}

#[test]
fn what_runs_in_the_engines_own_session_is_not_a_command() {
    // A helper the engine starts without a session of its own (a language server, a watcher) is in the session of the engine, which is in
    // the baseline. However much CPU it uses, it is not a command the model waits for.
    let r = Rig::new("probe-engine-session");
    P::new(10).session(10).write(&r.root);
    r.sample(0);
    P::new(11).session(10).born(20).cpu(5000).write(&r.root);
    assert_eq!(r.sample(25), Command::Idle);
    // and a session of its own is a command
    P::new(12).born(26).cpu(500).write(&r.root);
    assert_eq!(r.sample(30), active(4));
}

#[test]
fn a_process_first_seen_that_was_born_before_the_previous_sample_counts_nothing() {
    // A process the previous sample did not see (its files could not be read just then) is not new: the CPU it used in its whole life is
    // not what it used since the last sample.
    let r = Rig::new("probe-first-seen");
    P::new(10).write(&r.root);
    r.sample(0);
    assert_eq!(r.sample(5), Command::Idle);
    P::new(11).born(2).cpu(900).write(&r.root);
    assert_eq!(
        r.sample(10),
        Command::Idle,
        "born at second 2, before the sample at 5, and seen for the first time at 10"
    );
    P::new(11).born(2).cpu(900 + 30).write(&r.root);
    assert!(
        is_active(r.sample(15)),
        "from now on it counts by its change"
    );
}

#[test]
fn a_probe_for_the_agents_own_user_is_refused() {
    // it could not tell agentd's processes from the engine's
    let own: u32 = std::fs::read_to_string("/proc/self/status")
        .map(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|l| l.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0)
        })
        .unwrap_or(0);
    assert!(ProcProbe::for_engine_user(own, 5).is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn a_real_busy_process_of_this_user_is_seen_running() {
    // The real `/proc` of this machine, for the user this test runs as: a process that is started after the baseline, in a session of its
    // own (as every command of the engine is), and spins is a command. Other tests run beside this one as the same user, which can only add
    // activity, so the test asserts the active answer and nothing about idle.
    let own: u32 = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap();
    let p = ProcProbe::with_root(PathBuf::from("/proc"), own, 5);
    let t0 = Instant::now();
    assert_eq!(p.sample(t0), Command::Unknown);
    let mut child = std::process::Command::new("setsid")
        .args(["sh", "-c", "while :; do :; done"])
        .spawn()
        .unwrap();
    let mut seen = Command::Idle;
    for _ in 0..10 {
        std::thread::sleep(Duration::from_secs(1));
        seen = p.sample(Instant::now());
        if is_active(seen) {
            break;
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        is_active(seen),
        "a loop that uses a whole CPU for seconds is a running command: {seen:?}"
    );
    if let Command::Active { age } = seen {
        assert!(
            age < Duration::from_secs(30),
            "the loop began seconds ago: {age:?}"
        );
    }
}
