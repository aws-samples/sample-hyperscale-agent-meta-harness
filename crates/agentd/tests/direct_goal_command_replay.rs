//! A measured trace through the real probe. The probe reads it from a fake `/proc` that the test rewrites at every fifth second, as the module
//! samples it.
//!
//! The run of 2026-10-23 was ended by the brake because the probe read such a job as Idle in most samples. These tests say what it reads now.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::goal::command::{CommandProbe, ProcProbe};
use agentd::direct::goal::watch::Command;
use common::{repo_root, scratch};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

const ENGINE_UID: u32 = 4242;

fn rows() -> Vec<Value> {
    let path = repo_root().join("fixtures/replays/scripts-suite-serial/trace-5s.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    doc["rows"].as_array().unwrap().clone()
}

fn write_proc(root: &Path, pid: u64, start: u64, session: u64, own: u64, waited: u64) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("stat"),
        format!(
            "{pid} (x) S 1 {pid} {session} 0 -1 4194560 10 0 0 0 {own} 0 {waited} 0 20 0 1 0 {start} 123456 77 18446744073709551615"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("status"),
        format!("Name:\tx\nUid:\t{0}\t{0}\t{0}\t{0}\n", ENGINE_UID),
    )
    .unwrap();
}

fn set_uptime(root: &Path, ticks: u64) {
    std::fs::write(
        root.join("uptime"),
        format!("{}.{:02} 99.99\n", ticks / 100, ticks % 100),
    )
    .unwrap();
}

/// What a probe with `window` reads at each sample of the trace after the baseline: (second, answer).
fn replay(tag: &str, window: Duration) -> Vec<(u64, Command)> {
    let root = scratch(tag).join("proc");
    std::fs::create_dir_all(&root).unwrap();
    let probe = ProcProbe::with_root(root.clone(), ENGINE_UID, 5).with_window(window);
    let t0 = Instant::now();
    let rows = rows();
    // the engine's own process, there before the job: the baseline
    write_proc(&root, 90, 1, 90, 0, 0);
    let first = &rows[0];
    assert!(
        first["p"].as_array().unwrap().is_empty(),
        "the trace begins before the job"
    );
    set_uptime(&root, first["up"].as_u64().unwrap());
    assert_eq!(probe.sample(t0), Command::Unknown, "the baseline");
    let mut seen: Vec<u64> = Vec::new();
    let mut out = Vec::new();
    for row in &rows[1..] {
        let t = row["t"].as_u64().unwrap();
        set_uptime(&root, row["up"].as_u64().unwrap());
        let mut now: Vec<u64> = Vec::new();
        for p in row["p"].as_array().unwrap() {
            let n = |i: usize| p[i].as_u64().unwrap();
            write_proc(&root, n(0), n(1), n(2), n(3), n(4));
            now.push(n(0));
        }
        for gone in seen.iter().filter(|pid| !now.contains(pid)) {
            std::fs::remove_dir_all(root.join(gone.to_string())).unwrap();
        }
        seen = now;
        out.push((t, probe.sample(t0 + Duration::from_secs(t))));
    }
    out
}

fn share(answers: &[(u64, Command)], from: u64, to: u64) -> f64 {
    let of: Vec<&(u64, Command)> = answers
        .iter()
        .filter(|(t, _)| (from..to).contains(t))
        .collect();
    let active = of
        .iter()
        .filter(|(_, c)| matches!(c, Command::Active { .. }))
        .count();
    f64::from(u32::try_from(active).unwrap()) / f64::from(u32::try_from(of.len()).unwrap())
}

#[test]
fn judged_one_interval_at_a_time_the_suite_reads_idle_for_all_but_its_busy_start_even_with_its_children_counted()
 {
    // Without the ticks of the exited children the probe saw 2 percent of this trace; with them it sees the busy start (second 5 to 214, 90
    // percent of the samples), and the minutes of waiting that follow read Idle: 22 percent of the 900 s.
    let a = replay("replay-intervals", Duration::ZERO);
    let busy = share(&a, 5, 215);
    let all = share(&a, 5, 900);
    assert!(
        busy > 0.8,
        "children counted, the busy start is seen: {busy}"
    );
    assert!(all < 0.3, "the waiting is not: {all}");
}

#[test]
fn with_the_window_the_suite_reads_active_from_its_start_while_its_work_is_in_the_window() {
    let a = replay("replay-window-30", Duration::from_secs(30 * 60));
    // the first samples (the first interval has no history) and every sample after: the job is Active for the 900 s of the trace
    assert!(share(&a, 5, 900) > 0.97, "{}", share(&a, 5, 900));
    for (t, c) in &a {
        if let Command::Active { age } = c {
            assert!(
                age.as_secs() <= *t,
                "the job began after the baseline, so it is never older than the trace: {age:?} at {t}"
            );
        }
    }
}

#[test]
fn a_window_shorter_than_the_wait_releases_the_suite_once_its_work_has_left_it() {
    // TRIGGER PRESENT: the same job reads Active up to the end of its work (second 214) in the test above; here the window is 5 minutes
    let a = replay("replay-window-5", Duration::from_secs(300));
    assert!(share(&a, 5, 214) > 0.95, "{}", share(&a, 5, 214));
    assert!(
        share(&a, 560, 900) == 0.0,
        "no CPU in the window of the last 5 minutes, and none of the quiet seconds is held: {}",
        share(&a, 560, 900)
    );
}
