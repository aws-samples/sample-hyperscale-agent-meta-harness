//! A busy session is progress for the brake inside a round.
//!
//! The brake ends a run that makes tool calls for 20 minutes (at least 10 of them) with no file changed, or 30 calls in a row with no file changed
//! and no new text. The second signal is the CPU of the session outside agentd and the engine, read from `/proc/stat` and `/proc/<pid>/stat`: when
//! it was busy at `AGENTD_STALL_BUSY_PCT` percent of one CPU (50) over the stretch being judged, the stretch is progress and the counts start
//! again. A stop the brake still makes says whether the CPU was idle.
//!
//! The first half is the pure watch with a clock the tests move; then the reading of the process table over a `/proc` the tests write; then the
//! settings; then whole runs against the fake engine (the polls of `goal_command_polling`, 20 calls over 12 seconds, with a window of 10 seconds).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::busy::{BusyProbe, ProcBusy};
use agentd::direct::goal::machine::{Brake, Rule};
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::goal::watch::{HoldNote, Watch, WatchLimits};
use agentd::direct::hooks::{DirectHook, DriverCtl, Hooks};
use common::{plugin_folder, scratch};
use direct_support::{Direct, Opts};
use harness_protocol::direct::{DirectOutcome, GoalOptions, HARNESS_STOP_PREFIX};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn limits() -> WatchLimits {
    WatchLimits {
        calls: 30,
        window: Duration::from_secs(20 * 60),
        min_calls: 10,
        who: "Cline",
    }
}

struct Clock(Instant);

impl Clock {
    fn at(&self, s: u64) -> Instant {
        self.0 + Duration::from_secs(s)
    }
}

/// What the monitor of the module does, with the clock in seconds: every 5 s a reading of the session's CPU outside agentd and the engine and a
/// look at the tree (which never changes here), and a tool call every `poll_s` seconds from the first. `cpu_pct` is the share of one CPU the
/// session used: 100 clock ticks are one CPU for one second, so a share of p percent adds p ticks every second. Returns the second of the brake
/// and the brake, or `None` when the run went on to `minutes`. The notes the watch made are kept in `notes`.
fn poll(
    w: &mut Watch,
    c: &Clock,
    minutes: u64,
    poll_s: u64,
    cpu_pct: &dyn Fn(u64) -> u64,
    notes: &mut Vec<HoldNote>,
) -> Option<(u64, Brake)> {
    let mut ticks = 0u64;
    for s in (0..=minutes * 60).step_by(5) {
        ticks += cpu_pct(s) * 5;
        w.cpu(c.at(s), Some(ticks));
        if s > 0 && s % poll_s == 0 {
            w.tool_call(c.at(s));
        }
        let brake = w
            .wants_look(c.at(s), Duration::from_secs(5))
            .then(|| w.look(c.at(s), Some("t0".into())))
            .flatten();
        notes.extend(w.take_note());
        if let Some(b) = brake {
            return Some((s, b));
        }
    }
    None
}

fn watch(c: &Clock) -> Watch {
    Watch::new(limits(), c.at(0), Some("t0".into())).with_busy_pct(50)
}

fn busy_note(n: &HoldNote) -> Option<(u32, Duration)> {
    match n {
        HoldNote::Busy { pct, span } => Some((*pct, *span)),
        _ => None,
    }
}

// ---- the watch ------------------------------------------------------------------------------------------------------------------

#[test]
fn a_run_that_polls_a_busy_session_for_40_minutes_with_short_commands_is_not_stopped() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let mut notes = Vec::new();
    let got = poll(&mut w, &c, 40, 60, &|_| 150, &mut notes);
    assert!(got.is_none(), "{got:?}");
    // TRIGGER PRESENT: the unchanged tree and the calls would have ended the run at 20 minutes: 20 calls in the window, 10 needed
    assert!(w.calls_in_a_row() <= 20, "{}", w.calls_in_a_row());
    let busy: Vec<_> = notes.iter().filter_map(busy_note).collect();
    assert!(
        !busy.is_empty() && busy.len() <= 3,
        "one note for each window that was held back: {notes:?}"
    );
    assert_eq!(busy[0].0, 150, "{busy:?}");
    assert_eq!(busy[0].1, Duration::from_secs(20 * 60), "{busy:?}");
}

#[test]
fn the_same_polls_of_an_idle_session_are_stopped_at_20_minutes_as_before_and_the_label_says_the_cpu_was_idle()
 {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let mut notes = Vec::new();
    let (s, b) = poll(&mut w, &c, 40, 60, &|_| 0, &mut notes).expect("stopped");
    assert!((1200..=1205).contains(&s), "{s}");
    assert_eq!(b.rule, Rule::RoundNoProgress);
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 20 minutes: no file changed and 20 of 20 tool calls counted as no progress. CPU idle."
        )
    );
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&b.detail)),
        "Cline stopped: the harness ended the run (Cline made no progress in 20 minutes: no file changed and 20 of 20 tool calls counted as no progress)"
    );
    assert!(notes.iter().all(|n| busy_note(n).is_none()), "{notes:?}");
}

#[test]
fn a_session_that_was_busy_but_below_the_share_is_named_with_its_percent() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let (s, b) = poll(&mut w, &c, 40, 60, &|_| 30, &mut Vec::new()).expect("stopped");
    assert!((1200..=1205).contains(&s), "{s}");
    assert!(
        b.detail
            .contains("tool calls counted as no progress. CPU busy at 30 percent."),
        "{}",
        b.detail
    );
}

#[test]
fn a_build_that_ended_early_in_the_window_does_not_excuse_the_rest() {
    // 10 minutes at 60 percent, then nothing: over the 20 minutes of the window that is 30 percent.
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let (s, b) = poll(
        &mut w,
        &c,
        40,
        60,
        &|s| if s <= 600 { 60 } else { 0 },
        &mut Vec::new(),
    )
    .expect("stopped");
    assert!((1200..=1205).contains(&s), "{s}");
    assert!(b.detail.contains("CPU busy at 30 percent."), "{}", b.detail);
}

#[test]
fn with_the_setting_at_zero_the_signal_is_off_and_the_text_is_what_it_was() {
    let c = Clock(Instant::now());
    let mut w = Watch::new(limits(), c.at(0), Some("t0".into())).with_busy_pct(0);
    let mut notes = Vec::new();
    // TRIGGER PRESENT: the same busy session that the first test lets through
    let (s, b) = poll(&mut w, &c, 40, 60, &|_| 150, &mut notes).expect("stopped");
    assert!((1200..=1205).contains(&s), "{s}");
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 20 minutes: no file changed and 20 of 20 tool calls counted as no progress."
        )
    );
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn a_session_whose_cpu_cannot_be_read_stops_as_before_with_no_words_about_it() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let mut got = None;
    for s in (0..=2400u64).step_by(5) {
        w.cpu(c.at(s), None);
        if s > 0 && s % 60 == 0 {
            w.tool_call(c.at(s));
        }
        if w.wants_look(c.at(s), Duration::from_secs(5)) {
            got = got.or(w.look(c.at(s), Some("t0".into())));
        }
    }
    let b = got.expect("stopped");
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 20 minutes: no file changed and 20 of 20 tool calls counted as no progress."
        )
    );
}

#[test]
fn thirty_calls_in_a_row_in_a_busy_session_are_progress_and_in_an_idle_one_they_are_not() {
    // a call every 10 s: 30 calls in 5 minutes, before the window
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    assert!(poll(&mut w, &c, 30, 10, &|_| 80, &mut Vec::new()).is_none());
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let (s, b) = poll(&mut w, &c, 30, 10, &|_| 0, &mut Vec::new()).expect("stopped");
    assert!((300..=305).contains(&s), "{s}");
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared. CPU idle."
        )
    );
}

#[test]
fn a_burst_of_calls_shorter_than_the_least_stretch_is_not_excused_by_the_cpu() {
    // 30 calls in 15 seconds while the session is busy: too short a stretch to judge the CPU, so the brake works as it did
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let mut ticks = 0u64;
    for s in (0..=20u64).step_by(5) {
        ticks += 500;
        w.cpu(c.at(s), Some(ticks));
    }
    for i in 0..30u64 {
        w.tool_call(c.at(i / 2));
    }
    let b = w
        .look(c.at(20), Some("t0".into()))
        .expect("30 calls in a row");
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared."
        )
    );
}

#[test]
fn the_least_stretch_follows_the_builder() {
    let c = Clock(Instant::now());
    let mut w = watch(&c).with_busy_min_span(Duration::from_secs(10));
    let mut ticks = 0u64;
    for s in (0..=20u64).step_by(5) {
        ticks += 500;
        w.cpu(c.at(s), Some(ticks));
    }
    for i in 0..30u64 {
        w.tool_call(c.at(i / 2));
    }
    assert!(
        w.look(c.at(20), Some("t0".into())).is_none(),
        "20 s is enough for a least stretch of 10 s: 100 percent is busy"
    );
}

#[test]
fn a_busy_session_with_no_file_changed_still_ends_at_the_grace_of_a_running_command() {
    let c = Clock(Instant::now());
    let mut w = watch(&c).with_command_grace(Duration::from_secs(2 * 3600));
    let (s, b) = poll(&mut w, &c, 4 * 60, 60, &|_| 120, &mut Vec::new()).expect("stopped");
    assert!((7200..=7220).contains(&s), "{s}");
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 2 hours: a command was running, but no file changed. CPU busy at 120 percent."
        )
    );
}

#[test]
fn a_change_of_the_tree_starts_the_stretch_over() {
    // An idle session, a call a minute, and the tree changes at 600 s: the brake comes 20 minutes after the change, not after the start.
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let mut got = None;
    for s in (0..=2400u64).step_by(5) {
        w.cpu(c.at(s), Some(0));
        if s > 0 && s % 60 == 0 {
            w.tool_call(c.at(s));
        }
        let fp = if s < 600 { "t0" } else { "t1" };
        if got.is_none() && w.wants_look(c.at(s), Duration::from_secs(5)) {
            got = w.look(c.at(s), Some(fp.into())).map(|b| (s, b));
        }
    }
    let (s, b) = got.expect("stopped");
    assert!((1800..=1805).contains(&s), "{s}");
    assert!(b.detail.contains("CPU idle."), "{}", b.detail);
}

// ---- the reading of the process table ------------------------------------------------------------------------------------------

const AGENTD: u32 = 4242;
const ENGINE: u32 = 4343;

/// One line of `/proc/<pid>/stat`: the CPU of the process is `utime` and `stime`, `start` its start time.
fn pstat(pid: u32, utime: u64, stime: u64, start: u64) -> String {
    format!(
        "{pid} (proc) S 1 {pid} {pid} 0 -1 4194560 100 0 0 0 {utime} {stime} 0 0 20 0 5 0 {start} 1000 100 18446744073709551615\n"
    )
}

/// As [`pstat`] with the CPU of the children the process has waited for (`cutime` and `cstime`).
fn pstat_children(
    pid: u32,
    utime: u64,
    stime: u64,
    cutime: u64,
    cstime: u64,
    start: u64,
) -> String {
    format!(
        "{pid} (proc) S 1 {pid} {pid} 0 -1 4194560 100 0 0 0 {utime} {stime} {cutime} {cstime} 20 0 5 0 {start} 1000 100 18446744073709551615\n"
    )
}

fn proc_stat(user: u64, nice: u64, system: u64, idle: u64) -> String {
    format!(
        "cpu  {user} {nice} {system} {idle} 7 0 3 0 0 0\ncpu0 1 1 1 1 0 0 0 0 0 0\nintr 1\nctxt 2\n"
    )
}

struct FakeProc {
    root: std::path::PathBuf,
}

impl FakeProc {
    fn new(label: &str) -> Self {
        let root = scratch(label);
        Self { root }
    }
    fn total(&self, user: u64, nice: u64, system: u64, idle: u64) {
        std::fs::write(self.root.join("stat"), proc_stat(user, nice, system, idle)).unwrap();
    }
    fn process(&self, pid: u32, utime: u64, stime: u64, start: u64) {
        let d = self.root.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("stat"), pstat(pid, utime, stime, start)).unwrap();
    }
    fn probe(&self, engine: &Arc<AtomicU32>) -> ProcBusy {
        ProcBusy::new(self.root.clone(), AGENTD, engine.clone())
    }
}

#[test]
fn the_busy_time_is_what_user_nice_and_system_gained_less_agentd_and_the_engine() {
    let p = FakeProc::new("busy-basic");
    let engine = Arc::new(AtomicU32::new(ENGINE));
    p.total(1000, 100, 500, 90_000);
    p.process(AGENTD, 100, 50, 10);
    p.process(ENGINE, 200, 100, 20);
    let probe = p.probe(&engine);
    assert_eq!(probe.read(), Some(0), "the first reading is the origin");
    // 1000 more ticks of user, 100 of nice, 400 of system; idle and iowait do not count; agentd used 120 of them and the engine 280
    p.total(2000, 200, 900, 95_000);
    p.process(AGENTD, 160, 110, 10);
    p.process(ENGINE, 340, 240, 20);
    assert_eq!(probe.read(), Some(1500 - 120 - 280));
    // nothing more happened: the reading stays
    assert_eq!(probe.read(), Some(1100));
    // the build of the model: 600 ticks in processes of neither
    p.total(2400, 200, 1100, 95_500);
    assert_eq!(probe.read(), Some(1100 + 600));
}

#[test]
fn the_engines_ticks_come_off_the_total_and_a_restarted_engine_is_followed() {
    let p = FakeProc::new("busy-engine");
    let engine = Arc::new(AtomicU32::new(ENGINE));
    p.total(1000, 0, 0, 0);
    p.process(AGENTD, 0, 0, 10);
    p.process(ENGINE, 500, 0, 20);
    let probe = p.probe(&engine);
    assert_eq!(probe.read(), Some(0));
    // the engine used 100, a build used 300
    p.total(1400, 0, 0, 0);
    p.process(ENGINE, 600, 0, 20);
    assert_eq!(probe.read(), Some(300));
    // the engine is replaced by a new process, which has used 40 ticks since it began; a build used 200
    engine.store(ENGINE + 1, Ordering::SeqCst);
    p.process(ENGINE + 1, 40, 0, 900);
    p.total(1640, 0, 0, 0);
    assert_eq!(probe.read(), Some(300 + 200));
    // no engine for a moment (the pid is 0): nothing is taken off
    engine.store(0, Ordering::SeqCst);
    p.total(1740, 0, 0, 0);
    assert_eq!(probe.read(), Some(300 + 200 + 100));
}

#[test]
fn the_children_agentd_waited_for_are_agentds_own_work_and_the_engines_are_not() {
    // agentd runs git for the snapshots and the fingerprint of the tree: their CPU (the children it waited for) is agentd's. The children the
    // engine waited for are its commands, which is what the brake wants to see.
    let p = FakeProc::new("busy-children");
    let engine = Arc::new(AtomicU32::new(ENGINE));
    p.total(1000, 0, 0, 0);
    let write = |agentd: (u64, u64, u64, u64), engine_c: (u64, u64, u64, u64)| {
        for (pid, (u, s, cu, cs)) in [(AGENTD, agentd), (ENGINE, engine_c)] {
            let d = p.root.join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("stat"), pstat_children(pid, u, s, cu, cs, 10)).unwrap();
        }
    };
    write((10, 0, 0, 0), (50, 0, 0, 0));
    let probe = p.probe(&engine);
    assert_eq!(probe.read(), Some(0));
    // 400 more ticks in all: agentd 20 itself and 80 in git children, the engine 30 itself and 100 in commands it waited for
    p.total(1400, 0, 0, 0);
    write((30, 0, 60, 20), (80, 0, 70, 30));
    assert_eq!(probe.read(), Some(400 - 20 - 80 - 30));
}

#[test]
fn a_counter_that_goes_back_never_makes_the_reading_go_back() {
    let p = FakeProc::new("busy-back");
    let engine = Arc::new(AtomicU32::new(0));
    p.total(5000, 0, 0, 0);
    p.process(AGENTD, 100, 0, 10);
    let probe = p.probe(&engine);
    assert_eq!(probe.read(), Some(0));
    p.total(5200, 0, 0, 0);
    p.process(AGENTD, 150, 0, 10);
    assert_eq!(probe.read(), Some(150));
    // agentd's counter is read a moment after the total: it has gained more than the total did
    p.total(5210, 0, 0, 0);
    p.process(AGENTD, 190, 0, 10);
    assert_eq!(probe.read(), Some(150), "saturates at zero");
}

#[test]
fn a_process_table_that_cannot_be_read_gives_no_reading() {
    let p = FakeProc::new("busy-missing");
    let engine = Arc::new(AtomicU32::new(ENGINE));
    let probe = p.probe(&engine);
    assert_eq!(probe.read(), None, "no stat file");
    p.total(1000, 0, 0, 0);
    assert_eq!(
        probe.read(),
        None,
        "no stat file of agentd: its own CPU is not known"
    );
    p.process(AGENTD, 10, 0, 10);
    assert_eq!(
        probe.read(),
        Some(0),
        "an engine that is not in the table counts as none"
    );
    std::fs::write(p.root.join("stat"), "garbage\n").unwrap();
    assert_eq!(probe.read(), None);
}

#[test]
fn the_engine_pid_is_shared_through_the_driver_control() {
    let ctl = DriverCtl::new();
    assert_eq!(ctl.engine_pid(), None);
    let copy = ctl.clone();
    ctl.set_engine_pid(4343);
    assert_eq!(
        copy.engine_pid(),
        Some(4343),
        "a copy of the control sees it"
    );
    assert_eq!(copy.engine_pid_cell().load(Ordering::SeqCst), 4343);
    copy.set_engine_pid(4344);
    assert_eq!(ctl.engine_pid(), Some(4344));
}

// ---- the settings -----------------------------------------------------------------------------------------------------------

fn settings_from(pairs: &[(&str, &str)]) -> GoalSettings {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

#[test]
fn the_share_defaults_to_50_percent_of_one_cpu_and_zero_switches_the_signal_off() {
    let s = settings_from(&[]);
    assert_eq!(s.busy_pct, 50);
    assert!(s.busy_signal());
    let s = settings_from(&[("AGENTD_STALL_BUSY_PCT", "75")]);
    assert_eq!(s.busy_pct, 75);
    let s = settings_from(&[("AGENTD_STALL_BUSY_PCT", "0")]);
    assert_eq!(s.busy_pct, 0);
    assert!(!s.busy_signal());
    for bad in ["", "half", "-5", "1.5", "50%"] {
        assert_eq!(
            settings_from(&[("AGENTD_STALL_BUSY_PCT", bad)]).busy_pct,
            50,
            "{bad:?} is the default"
        );
    }
    assert_eq!(
        settings_from(&[("AGENTD_STALL_BUSY_PCT", "999999")]).busy_pct,
        1000
    );
}

#[test]
fn the_users_switch_for_running_commands_turns_the_cpu_signal_off_too() {
    // run.command_grace_minutes 0 (the request's commandGraceS 0) and the stage's AGENTD_GOAL_COMMAND=off both stop the look at running commands
    let s = settings_from(&[("AGENTD_GOAL_COMMAND", "off")]);
    assert!(!s.busy_signal());
    let s = settings_from(&[]).with_request(&GoalOptions {
        enabled: true,
        round_calls: None,
        round_window_s: None,
        round_min_calls: None,
        command_grace_s: Some(0),
        final_message: None,
        new_calls: None,
        outside_checkout_note: None,
    });
    assert!(!s.busy_signal());
    // the limits of the brake keep their meaning
    let s = settings_from(&[]);
    let l = s.watch_limits("Cline");
    assert_eq!(
        (l.calls, l.window, l.min_calls),
        (30, Duration::from_secs(1200), 10)
    );
}

// ---- whole runs ---------------------------------------------------------------------------------------------------------------

/// The CPU of the session as a test decides it: `pct` percent of one CPU since the probe was made.
#[derive(Debug)]
struct Cpu {
    began: Instant,
    pct: Mutex<u64>,
    reads: AtomicU32,
}

impl Cpu {
    fn new(pct: u64) -> Arc<Self> {
        Arc::new(Self {
            began: Instant::now(),
            pct: Mutex::new(pct),
            reads: AtomicU32::new(0),
        })
    }
}

impl BusyProbe for Cpu {
    fn read(&self) -> Option<u64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let ms = u64::try_from(self.began.elapsed().as_millis()).unwrap();
        Some(ms * *self.pct.lock().unwrap() / 1000)
    }
}

fn settings(extra: &[(&str, &str)]) -> GoalSettings {
    let mut m: BTreeMap<String, String> = [
        (
            "AGENTD_GOAL_PLUGIN_DIR",
            plugin_folder().display().to_string(),
        ),
        ("AGENTD_GOAL_POLL_MS", "20".to_string()),
        ("AGENTD_GOAL_SETTLE_MS", "150".to_string()),
        ("AGENTD_GOAL_DEAD_GRACE_MS", "150".to_string()),
        ("AGENTD_GOAL_KICK_AFTER_S", "1".to_string()),
        ("AGENTD_GOAL_DRAIN_S", "3".to_string()),
        ("AGENTD_GOAL_CLEARED_CONFIRM_MS", "200".to_string()),
        ("AGENTD_GOAL_FALLBACK_QUIET_S", "1".to_string()),
        ("AGENTD_GOAL_CLAIM_WAIT_MS", "300".to_string()),
        ("AGENTD_GOAL_EVENTS_EVERY_S", "0".to_string()),
        ("AGENTD_GOAL_LOOK_EVERY_MS", "50".to_string()),
        ("AGENTD_GOAL_ROUND_WINDOW_S", "10".to_string()),
        ("AGENTD_GOAL_ROUND_MIN_CALLS", "3".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in extra {
        m.insert((*k).to_string(), (*v).to_string());
    }
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

async fn start(scenario: &'static str) -> Direct {
    Direct::start(Opts {
        scenario,
        timeout_s: 90,
        ..Opts::default()
    })
    .await
}

async fn run(d: &Direct, s: GoalSettings, cpu: Arc<Cpu>) -> InvocationResponse {
    let goal = GoalHook::for_engine_with_busy("cline", s, cpu)
        .expect("the plugin folder is in the repository");
    let deps = Deps {
        hooks: Some(Hooks::new(vec![goal as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn holds(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["phase"] == "hold" && v.get("busyPct").is_some())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn polling_a_busy_session_for_longer_than_the_window_does_not_end_the_run() {
    let d = start("goal_command_polling").await;
    let cpu = Cpu::new(150);
    let t0 = Instant::now();
    let resp = run(&d, settings(&[]), cpu.clone()).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:#?}",
        d.engine_trace(),
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    // TRIGGER PRESENT: 20 polls over 12 seconds with no file changed until the end: more than the window of 10 s and more than its 3 calls
    assert!(s.tool_calls >= 20, "{}", s.tool_calls);
    assert!(t0.elapsed() > Duration::from_secs(10), "{:?}", t0.elapsed());
    assert!(
        cpu.reads.load(Ordering::SeqCst) >= 3,
        "the CPU was read while the polls came"
    );
    let held = holds(&d);
    assert!(!held.is_empty(), "the hold is told to the run's events");
    assert!(held[0]["busyPct"].as_u64().unwrap() >= 100, "{held:?}");
    assert!(
        held[0]["text"].as_str().unwrap().contains("session CPU"),
        "{held:?}"
    );
    assert!(d.worktree().join("notes/result.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_polls_of_an_idle_session_end_the_run_and_the_label_says_the_cpu_was_idle() {
    let d = start("goal_command_polling_brake").await;
    let cpu = Cpu::new(0);
    let resp = run(&d, settings(&[]), cpu).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(
        s.label
            .starts_with("Cline stopped: the harness ended the run (Cline made no progress in "),
        "{}",
        s.label
    );
    assert!(
        s.label.contains("in 10 seconds: no file changed and "),
        "{}",
        s.label
    );
    assert!(
        s.detail.as_deref().is_some_and(|d| d.contains("CPU idle.")),
        "{:?}",
        s.detail
    );
    assert!(holds(&d).is_empty());
    assert!(!d.worktree().join("notes/result.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_share_at_zero_a_busy_session_is_stopped_with_the_old_words() {
    let d = start("goal_command_polling_brake").await;
    let cpu = Cpu::new(150);
    let resp = run(&d, settings(&[("AGENTD_STALL_BUSY_PCT", "0")]), cpu.clone()).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(
        s.label.contains("in 10 seconds: no file changed and ")
            && !s.detail.as_deref().unwrap_or("").contains("CPU"),
        "{}",
        s.label
    );
    assert_eq!(
        cpu.reads.load(Ordering::SeqCst),
        0,
        "the signal off reads nothing"
    );
}
