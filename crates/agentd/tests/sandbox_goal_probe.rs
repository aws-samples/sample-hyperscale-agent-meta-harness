//! In a Landlock sandbox the engine shares agentd's user, so the evidence of a running command cannot be "the engine user's processes":
//! it is the processes below the engine's own. The tests read a fabricated `/proc`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::goal::command::{CommandProbe, ProcProbe};
use agentd::direct::goal::watch::Command;
use agentd::direct::goal::{Evidence, evidence_choice};
use common::scratch;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const BOOT: u64 = 5_000_000;
const ENGINE: u32 = 100;

fn uptime(root: &Path, ticks: u64) {
    std::fs::write(
        root.join("uptime"),
        format!("{}.{:02} 1.00\n", ticks / 100, ticks % 100),
    )
    .unwrap();
}

/// One process in the fake table: its parent, its session, the CPU it used and when it started (ticks after boot).
fn proc_(root: &Path, pid: u32, ppid: u32, session: u32, ticks: u64, start: u64) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime prio nice threads itreal starttime ...
    let stat = format!(
        "{pid} (x) S {ppid} {pid} {session} 0 -1 4194560 10 0 0 0 {ticks} 0 0 0 20 0 1 0 {start} 123456 77 18446744073709551615"
    );
    std::fs::write(dir.join("stat"), stat).unwrap();
}

fn gone(root: &Path, pid: u32) {
    std::fs::remove_dir_all(root.join(pid.to_string())).unwrap();
}

#[test]
fn the_user_wins_when_there_is_one_and_the_sandbox_decides_otherwise() {
    assert_eq!(evidence_choice(Some((7, 8)), false), Evidence::User(7));
    assert_eq!(evidence_choice(Some((7, 8)), true), Evidence::User(7));
    assert_eq!(evidence_choice(None, true), Evidence::Below);
    assert_eq!(evidence_choice(None, false), Evidence::Off);
}

fn fixture() -> (std::path::PathBuf, Arc<AtomicU32>, ProcProbe) {
    let root = scratch("family-proc").join("proc");
    std::fs::create_dir_all(&root).unwrap();
    uptime(&root, BOOT);
    proc_(&root, 1, 0, 1, 0, 0);
    proc_(&root, ENGINE, 1, ENGINE, 0, BOOT - 100_000);
    let cell = Arc::new(AtomicU32::new(ENGINE));
    // 50 percent of a CPU makes an interval active.
    let probe = ProcProbe::for_engine_family_in(&root, cell.clone(), 50);
    (root, cell, probe)
}

#[test]
fn a_job_below_the_engine_is_active_and_a_stranger_in_the_same_user_is_not_looked_at() {
    let (root, _cell, probe) = fixture();
    probe.rebaseline();
    let t0 = Instant::now();
    // The model starts a job in a session of its own, below the engine (through a shell), after the baseline.
    proc_(&root, 150, ENGINE, 150, 0, BOOT + 10);
    proc_(&root, 151, 150, 150, 0, BOOT + 10);
    uptime(&root, BOOT + 100);
    assert!(
        matches!(probe.sample(t0), Command::Unknown),
        "no interval yet"
    );
    // Ten seconds later the job used a whole CPU; a process nobody started under the engine used ten whole CPUs.
    proc_(&root, 150, ENGINE, 150, 400, BOOT + 10);
    proc_(&root, 151, 150, 150, 600, BOOT + 10);
    proc_(&root, 300, 1, 300, 10_000, BOOT + 5);
    uptime(&root, BOOT + 1100);
    let at = t0 + Duration::from_secs(10);
    assert!(matches!(probe.sample(at), Command::Active { .. }));
    // The job ends; only the stranger keeps burning CPU.
    gone(&root, 150);
    gone(&root, 151);
    proc_(&root, 300, 1, 300, 20_000, BOOT + 5);
    uptime(&root, BOOT + 2100);
    assert!(matches!(
        probe.sample(at + Duration::from_secs(10)),
        Command::Idle
    ));
}

#[test]
fn a_stranger_alone_never_makes_the_evidence_active() {
    let (root, _cell, probe) = fixture();
    probe.rebaseline();
    let t0 = Instant::now();
    proc_(&root, 300, 1, 300, 0, BOOT + 10);
    uptime(&root, BOOT + 100);
    let _ = probe.sample(t0);
    proc_(&root, 300, 1, 300, 5_000, BOOT + 10);
    uptime(&root, BOOT + 1100);
    assert!(matches!(
        probe.sample(t0 + Duration::from_secs(10)),
        Command::Idle
    ));
}

#[test]
fn no_engine_yet_reads_as_an_empty_table() {
    let (root, cell, probe) = fixture();
    cell.store(0, Ordering::SeqCst);
    probe.rebaseline();
    let t0 = Instant::now();
    proc_(&root, 150, ENGINE, 150, 0, BOOT + 10);
    uptime(&root, BOOT + 100);
    let _ = probe.sample(t0);
    proc_(&root, 150, ENGINE, 150, 5_000, BOOT + 10);
    uptime(&root, BOOT + 1100);
    assert!(matches!(
        probe.sample(t0 + Duration::from_secs(10)),
        Command::Idle
    ));
}

#[test]
fn a_loop_of_parents_in_a_changing_table_ends_the_walk() {
    let (root, _cell, probe) = fixture();
    probe.rebaseline();
    let t0 = Instant::now();
    proc_(&root, 400, 401, 400, 0, BOOT + 10);
    proc_(&root, 401, 400, 400, 0, BOOT + 10);
    uptime(&root, BOOT + 100);
    let _ = probe.sample(t0);
    proc_(&root, 400, 401, 400, 5_000, BOOT + 10);
    uptime(&root, BOOT + 1100);
    assert!(matches!(
        probe.sample(t0 + Duration::from_secs(10)),
        Command::Idle
    ));
}
