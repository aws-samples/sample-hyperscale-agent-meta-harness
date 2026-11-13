//! The evidence that a command is running, smoothed over a window. The probe of
//! `direct_goal_command.rs` judged every sample interval alone: a job whose CPU comes in bursts (a test run that waits for a timeout, a build
//! that is quiet while it links) read Idle in most intervals, and the time of those intervals counted against the brake. Now a sample is also
//! Active when the sessions that are still there used at least the rate (5 percent of one CPU by default) over the last `window` seconds. The
//! tests build a `/proc` of their own, as `direct_goal_command.rs` does; the clock is the file `uptime`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::goal::command::{CommandProbe, ProcProbe};
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::goal::watch::Command;
use common::scratch;
use harness_protocol::direct::GoalOptions;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ENGINE_UID: u32 = 4242;
/// The uptime of the fake machine, in clock ticks, when a test begins: 10,000 seconds.
const BOOT: u64 = 1_000_000;

struct P {
    pid: u32,
    session: u32,
    start: u64,
    utime: u64,
    cutime: u64,
}

impl P {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            session: pid,
            start: 1000 + u64::from(pid),
            utime: 0,
            cutime: 0,
        }
    }
    fn born(mut self, s: u64) -> Self {
        self.start = BOOT + s * 100;
        self
    }
    fn session(mut self, sid: u32) -> Self {
        self.session = sid;
        self
    }
    fn cpu(mut self, utime: u64) -> Self {
        self.utime = utime;
        self
    }
    fn waited(mut self, cutime: u64) -> Self {
        self.cutime = cutime;
        self
    }
    fn write(&self, root: &Path) {
        let dir = root.join(self.pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let stat = format!(
            "{} (x) S 1 {} {} 0 -1 4194560 10 0 0 0 {} 0 {} 0 20 0 1 0 {} 123456 77 18446744073709551615",
            self.pid, self.pid, self.session, self.utime, self.cutime, self.start
        );
        std::fs::write(dir.join("stat"), stat).unwrap();
        std::fs::write(
            dir.join("status"),
            format!("Name:\tx\nUid:\t{0}\t{0}\t{0}\t{0}\n", ENGINE_UID),
        )
        .unwrap();
    }
}

struct Rig {
    root: PathBuf,
    p: ProcProbe,
    t0: Instant,
}

impl Rig {
    /// A probe whose window is `window_s` seconds (0 is no window: every interval alone).
    fn new(tag: &str, window_s: u64) -> Self {
        let root = scratch(tag).join("proc");
        std::fs::create_dir_all(&root).unwrap();
        let p = ProcProbe::with_root(root.clone(), ENGINE_UID, 5)
            .with_window(Duration::from_secs(window_s));
        let r = Rig {
            root,
            p,
            t0: Instant::now(),
        };
        r.clock(0);
        r
    }

    fn clock(&self, s: u64) {
        let ticks = BOOT + s * 100;
        std::fs::write(
            self.root.join("uptime"),
            format!("{}.{:02} 99.99\n", ticks / 100, ticks % 100),
        )
        .unwrap();
    }

    fn sample(&self, s: u64) -> Command {
        self.clock(s);
        self.p.sample(self.t0 + Duration::from_secs(s))
    }

    fn gone(&self, pid: u32) {
        std::fs::remove_dir_all(self.root.join(pid.to_string())).unwrap();
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

// ---- the window ----------------------------------------------------------------------------------------------------------------------

#[test]
fn a_job_that_worked_hard_stays_active_through_a_quiet_stretch_while_its_rate_over_the_window_holds()
 {
    // 60 CPU seconds in the first five, then nothing: 5 percent of the 600 s window is 30 CPU seconds.
    let r = Rig::new("window-burst", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert_eq!(
        r.sample(5),
        active(4),
        "the burst itself: the interval rule"
    );
    assert_eq!(
        r.sample(10),
        active(9),
        "nothing new in this interval, 6000 ticks in the 10 s of the window against 50"
    );
    assert_eq!(
        r.sample(300),
        active(299),
        "300 s later: 6000 ticks against the 1500 that 5 percent of 300 s needs"
    );
    assert!(
        is_active(r.sample(600)),
        "the whole window is 600 s: 6000 against 3000"
    );
}

#[test]
fn a_job_that_stops_using_the_cpu_is_idle_once_its_work_has_left_the_window() {
    // TRIGGER PRESENT: the job is Active for the first ten minutes (the test above); the hung job of the brief is one that then does nothing.
    let r = Rig::new("window-hang", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert!(is_active(r.sample(5)));
    assert!(is_active(r.sample(600)));
    assert_eq!(
        r.sample(610),
        Command::Idle,
        "the burst ended more than 600 s ago: no CPU in the window"
    );
    assert_eq!(r.sample(900), Command::Idle);
}

#[test]
fn the_rate_decays_as_the_window_grows_a_short_blip_does_not_hold_for_long() {
    // one CPU second at the start: the interval rule passes (100 ticks in 5 s), the window rule only while 100 ticks are 5 percent of the span
    let r = Rig::new("window-blip", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(100).write(&r.root);
    assert!(is_active(r.sample(5)));
    assert!(is_active(r.sample(10)), "100 ticks against 50");
    assert!(is_active(r.sample(20)), "100 ticks against 100");
    assert_eq!(r.sample(25), Command::Idle, "100 ticks against 125");
}

#[test]
fn a_daemon_that_uses_a_fraction_of_a_percent_is_never_active() {
    let r = Rig::new("window-daemon", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    let mut ticks = 0;
    for s in (5..=900).step_by(5) {
        ticks += 1; // 0.2 percent of one CPU
        P::new(30).born(1).cpu(ticks).write(&r.root);
        assert_eq!(r.sample(s), Command::Idle, "at second {s}");
    }
}

#[test]
fn without_a_window_every_interval_is_judged_alone() {
    // the behaviour of the first version, kept when the window is 0
    let r = Rig::new("window-off", 0);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert!(is_active(r.sample(5)));
    assert_eq!(r.sample(10), Command::Idle);
}

#[test]
fn the_first_interval_after_a_baseline_is_judged_by_its_own_rate() {
    // the window has no history yet: a job that starts hot is seen at once, not after it has built up a rate over the window
    let r = Rig::new("window-start", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(25).write(&r.root);
    assert!(is_active(r.sample(5)), "25 ticks in 5 s");
}

// ---- only what is still there -----------------------------------------------------------------------------------------------------------

#[test]
fn the_cpu_of_a_session_that_has_ended_does_not_count_for_the_next_call() {
    // The commands the model runs itself: each is a session of its own that ends. Their CPU must not stay in the window and make a later
    // call look like a wait for an old job.
    let r = Rig::new("window-ended", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert!(is_active(r.sample(5)));
    r.gone(20);
    P::new(30).born(100).cpu(5).write(&r.root);
    assert_eq!(
        r.sample(105),
        Command::Idle,
        "the session that used the CPU is gone; the one that is there used almost nothing"
    );
    assert_eq!(r.sample(110), Command::Idle);
}

#[test]
fn a_session_is_still_counted_when_one_scan_missed_it() {
    // a scan that cannot read a process (it left between the listing and the read) must not wipe the history of a job
    let r = Rig::new("window-missed", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert!(is_active(r.sample(5)));
    r.gone(20);
    assert_eq!(r.sample(10), Command::Idle, "not there: nothing to hold");
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert!(
        is_active(r.sample(15)),
        "back at the next scan, with its history"
    );
}

#[test]
fn a_session_that_stays_away_for_two_scans_loses_its_history() {
    let r = Rig::new("window-away", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert!(is_active(r.sample(5)));
    r.gone(20);
    assert_eq!(r.sample(10), Command::Idle);
    assert_eq!(r.sample(15), Command::Idle);
    // the same session number comes back as a new session with a little CPU: it does not inherit the old one
    P::new(20).born(16).cpu(3).write(&r.root);
    assert_eq!(r.sample(20), Command::Idle);
}

#[test]
fn a_new_baseline_forgets_the_window() {
    let r = Rig::new("window-rebaseline", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert!(is_active(r.sample(5)));
    r.p.rebaseline();
    assert_eq!(r.sample(10), Command::Unknown);
    assert_eq!(
        r.sample(15),
        Command::Idle,
        "everything there at the baseline belongs to the engine"
    );
}

// ---- which session the answer is about --------------------------------------------------------------------------------------------------

#[test]
fn the_age_is_the_older_of_the_interval_answer_and_the_window_answer() {
    // An old job worked earlier (window); right now a command the model started itself is the only thing that uses the CPU (interval).
    let r = Rig::new("window-age", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    assert!(is_active(r.sample(5)));
    P::new(20).born(1).cpu(6000).write(&r.root);
    P::new(30).born(100).cpu(900).write(&r.root);
    assert_eq!(
        r.sample(105),
        active(104),
        "the old job's age, not the 5 s of the command that uses the CPU now"
    );
}

#[test]
fn the_cpu_of_the_children_of_a_job_counts_in_the_window_too() {
    let r = Rig::new("window-children", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    // the job's own ticks stay at a handful; its exited children used 6000
    P::new(20).born(1).cpu(2).waited(6000).write(&r.root);
    assert!(is_active(r.sample(5)));
    P::new(20).born(1).cpu(2).waited(6000).write(&r.root);
    assert_eq!(r.sample(300), active(299));
}

#[test]
fn two_overlapping_jobs_then_one_exits_and_the_other_hangs() {
    // Two background jobs, each in a session of its own. One ends; the other goes on to wait without using the CPU. The one that is left
    // answers with its own age while its work is in the window, and the hold ends once the work has left it.
    let r = Rig::new("overlap", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(6000).write(&r.root);
    P::new(30).born(2).cpu(6000).write(&r.root);
    assert_eq!(r.sample(5), active(4), "the older of the two answers");
    r.gone(20);
    P::new(30).born(2).cpu(6000).write(&r.root);
    assert_eq!(
        r.sample(10),
        active(8),
        "the job that is left answers with its own age"
    );
    assert_eq!(r.sample(300), active(298));
    assert!(is_active(r.sample(600)), "its work is still in the window");
    assert_eq!(
        r.sample(610),
        Command::Idle,
        "its work has left the window and it uses no CPU: it hangs"
    );
}

#[test]
fn a_job_with_several_processes_has_the_cpu_of_all_of_them() {
    let r = Rig::new("window-members", 600);
    P::new(10).write(&r.root);
    r.sample(0);
    P::new(20).born(1).cpu(1).write(&r.root);
    P::new(21).session(20).born(2).cpu(3000).write(&r.root);
    P::new(22).session(20).born(3).cpu(3000).write(&r.root);
    assert_eq!(r.sample(5), active(4));
    assert_eq!(r.sample(200), active(199));
}

// ---- the window is a stage setting, the longer of the user's stall window and 30 minutes by default -----------------------------------------------------------

fn from(pairs: &[(&str, &str)]) -> GoalSettings {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

#[test]
fn the_window_is_thirty_minutes_or_the_stall_window_when_that_is_longer_unless_the_stage_names_one()
{
    assert_eq!(from(&[]).command_window(), Duration::from_secs(1800));
    assert_eq!(
        from(&[("AGENTD_GOAL_ROUND_WINDOW_S", "600")]).command_window(),
        Duration::from_secs(1800),
        "a shorter stall window does not shorten the hold"
    );
    assert_eq!(
        from(&[("AGENTD_GOAL_ROUND_WINDOW_S", "3600")]).command_window(),
        Duration::from_secs(3600)
    );
    assert_eq!(
        from(&[("AGENTD_GOAL_COMMAND_WINDOW_S", "1800")]).command_window(),
        Duration::from_secs(1800)
    );
    assert_eq!(
        from(&[
            ("AGENTD_GOAL_ROUND_WINDOW_S", "600"),
            ("AGENTD_GOAL_COMMAND_WINDOW_S", "1800")
        ])
        .command_window(),
        Duration::from_secs(1800),
        "the stage's own number wins over the stall window"
    );
}

#[test]
fn the_stage_window_is_cut_to_its_bounds_and_zero_switches_it_off() {
    assert_eq!(
        from(&[("AGENTD_GOAL_COMMAND_WINDOW_S", "1")]).command_window(),
        Duration::from_secs(5)
    );
    assert_eq!(
        from(&[("AGENTD_GOAL_COMMAND_WINDOW_S", "99999")]).command_window(),
        Duration::from_secs(14_400)
    );
    assert_eq!(
        from(&[("AGENTD_GOAL_COMMAND_WINDOW_S", "0")]).command_window(),
        Duration::ZERO
    );
    assert_eq!(
        from(&[("AGENTD_GOAL_COMMAND_WINDOW_S", "ten minutes")]).command_window(),
        Duration::from_secs(1800),
        "a value that does not parse is the default"
    );
}

#[test]
fn a_stall_window_in_the_request_moves_the_command_window_with_it() {
    let run = from(&[]).with_request(&GoalOptions {
        round_window_s: Some(3600),
        ..GoalOptions::switch(true)
    });
    assert_eq!(run.command_window(), Duration::from_secs(3600));
    let pinned = from(&[("AGENTD_GOAL_COMMAND_WINDOW_S", "900")]).with_request(&GoalOptions {
        round_window_s: Some(3600),
        ..GoalOptions::switch(true)
    });
    assert_eq!(pinned.command_window(), Duration::from_secs(900));
}
