//! The sweep that ends an engine must leave the session's container runtime alone.
//!
//! The messages module's reaper stops every process of the engine's user when the engine ends. The runtime's front socket runs as that
//! user, and so does Podman's service (the namespace's root maps to the engine's uid), so the sweep killed both before the test report
//! started. The socket file stayed on disk with nobody behind it, a connect to it was refused at once, and every Testcontainers step of
//! the report ended "the session's container runtime did not answer". `Reaper::protect_pid` names a process whose pid and start time agentd
//! read right after the runtime was started; that process and everything below it is out of every scan and every kill.
//!
//! The selection runs on a fabricated `/proc` tree (every case of the uid sweep, anywhere) and on real processes (a scan of the real `/proc`
//! that kills nothing, and a kill that selects by the run's tag only, so it can never touch a process of this machine that the test did not
//! start).
#![allow(clippy::unwrap_used)]

use agentd::direct::messages::reap::{Reaper, Report, TAG_ENV, killed_list, reaped_line};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;

const SELF: u32 = 100;
const ENGINE_UID: u32 = 901;
/// A host id inside the namespace's private range (the container's own users).
const NS_UID: u32 = 200_070;

struct Proc<'a> {
    pid: u32,
    comm: &'a str,
    ppid: u32,
    uid: u32,
    start: u64,
    environ: &'a [&'a str],
}

fn plant(root: &Path, p: &Proc<'_>) {
    let d = root.join(p.pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("stat"),
        format!(
            "{} ({}) S {} 1 1 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {} 123456 100 18446744073709551615\n",
            p.pid, p.comm, p.ppid, p.start
        ),
    )
    .unwrap();
    std::fs::write(
        d.join("status"),
        format!(
            "Name:\t{}\nState:\tS\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t0\t0\t0\t0\n",
            p.comm, p.pid, p.ppid, p.uid, p.uid, p.uid, p.uid
        ),
    )
    .unwrap();
    std::fs::write(d.join("environ"), p.environ.join("\0") + "\0").unwrap();
}

fn proc<'a>(pid: u32, comm: &'a str, ppid: u32, uid: u32) -> Proc<'a> {
    Proc {
        pid,
        comm,
        ppid,
        uid,
        start: 1000 + u64::from(pid),
        environ: &["PATH=/usr/bin"],
    }
}

fn tree() -> PathBuf {
    let root = common::scratch("b765-proc");
    std::fs::create_dir_all(&root).unwrap();
    plant(&root, &proc(SELF, "agentd", 1, 0));
    // The runtime, as `ham-containers up` leaves it: the front (the engine's user, a session of its own) and the idle supervisor (root).
    plant(&root, &proc(300, "python3.12", 1, ENGINE_UID));
    plant(&root, &proc(301, "python3.12", 1, 0));
    // After the first request the supervisor has become the launcher; the namespace's init, Podman's service and conmon run as the namespace's
    // root, which is the engine's uid on the host; a container's own processes run as a user of the private range. A worker of the front.
    plant(&root, &proc(302, "python3.12", 301, ENGINE_UID));
    plant(&root, &proc(303, "podman", 302, ENGINE_UID));
    plant(&root, &proc(304, "conmon", 302, ENGINE_UID));
    plant(&root, &proc(305, "postgres", 304, NS_UID));
    plant(&root, &proc(306, "python3.12", 300, ENGINE_UID));
    // What the sweep is for: tools of the engine's user, and one that carries the run's tag.
    plant(&root, &proc(400, "sleep", 1, ENGINE_UID));
    plant(
        &root,
        &Proc {
            environ: &["AGENTD_ENGINE_TAG=run-a"],
            ..proc(401, "node", 1, ENGINE_UID)
        },
    );
    // Not the engine's: a root process.
    plant(&root, &proc(500, "sshd", 1, 0));
    root
}

fn reaper(root: &Path) -> Reaper {
    Reaper::with_root(root.to_path_buf(), SELF, "run-a", Some(ENGINE_UID))
}

fn pids(r: &Reaper) -> Vec<u32> {
    r.scan().iter().map(|p| p.pid).collect()
}

#[test]
fn b765_trigger_present_the_uid_sweep_puts_the_runtime_front_in_the_kill_set() {
    // The cause, on the layer that fails: with no protection every process of the engine's user is in the set, the front and Podman's
    // service included.
    let r = reaper(&tree());
    let all = pids(&r);
    for runtime in [300, 302, 303, 304, 306] {
        assert!(
            all.contains(&runtime),
            "TRIGGER PRESENT: {runtime} is in {all:?}"
        );
    }
}

#[test]
fn b765_the_uid_sweep_spares_the_protected_runtime_tree_and_still_finds_the_tools() {
    let r = reaper(&tree());
    assert!(r.protect_pid(300), "the front is readable");
    assert!(r.protect_pid(301), "the supervisor is readable");
    // Everything below the supervisor and below the front is out: the launcher's children, Podman, conmon, the front's worker; the container's
    // own process was never in the set (not the engine's uid).
    assert_eq!(pids(&r), vec![400, 401]);
}

#[test]
fn b765_a_protected_process_that_carries_the_run_tag_is_still_spared() {
    let root = tree();
    plant(
        &root,
        &Proc {
            environ: &["AGENTD_ENGINE_TAG=run-a"],
            ..proc(307, "podman", 302, ENGINE_UID)
        },
    );
    plant(
        &root,
        &Proc {
            environ: &["AGENTD_ENGINE_TAG=run-a"],
            ..proc(300, "python3.12", 1, ENGINE_UID)
        },
    );
    let r = reaper(&root);
    assert!(r.protect_pid(300) && r.protect_pid(301));
    let found = pids(&r);
    assert!(!found.contains(&300) && !found.contains(&307), "{found:?}");
    assert!(found.contains(&401), "{found:?}");
}

#[test]
fn b765_a_reused_pid_with_another_start_time_is_not_protected_and_nor_are_its_children() {
    let root = tree();
    let r = reaper(&root);
    assert!(r.protect_pid(300));
    // The front died and the kernel gave its pid to a tool of the engine, which has a child.
    plant(
        &root,
        &Proc {
            start: 987_654,
            ..proc(300, "sleep", 1, ENGINE_UID)
        },
    );
    plant(&root, &proc(310, "sleep", 300, ENGINE_UID));
    let found = pids(&r);
    assert!(
        found.contains(&300),
        "a reused pid is a tool again: {found:?}"
    );
    assert!(found.contains(&310), "{found:?}");
}

#[test]
fn b765_a_pid_that_cannot_be_read_cannot_be_protected() {
    let r = reaper(&tree());
    assert!(!r.protect_pid(999), "no such process");
    assert!(!r.protect_pid(0));
    assert_eq!(
        pids(&r),
        vec![300, 302, 303, 304, 306, 400, 401],
        "nothing was shielded"
    );
}

#[test]
fn b765_agentd_itself_is_never_marked() {
    // Agentd is outside every scan already; a mark on it would report a protection that does nothing.
    let r = reaper(&tree());
    assert!(!r.protect_pid(SELF));
}

#[test]
fn b765_the_supervisor_keeps_its_protection_when_it_execs_the_launcher() {
    let root = tree();
    let r = reaper(&root);
    assert!(r.protect_pid(301));
    // `exec` keeps the pid and the start time; the name changes and a child appears.
    plant(&root, &proc(301, "ham-containers", 1, 0));
    plant(&root, &proc(320, "unshare", 301, ENGINE_UID));
    let found = pids(&r);
    assert!(!found.contains(&320) && !found.contains(&302), "{found:?}");
}

#[test]
fn b765_a_protected_root_that_has_died_protects_nothing_below_it() {
    let root = tree();
    let r = reaper(&root);
    assert!(r.protect_pid(300));
    // The front is gone; its worker was handed to init.
    std::fs::remove_dir_all(root.join("300")).unwrap();
    plant(&root, &proc(306, "python3.12", 1, ENGINE_UID));
    let found = pids(&r);
    assert!(found.contains(&306), "{found:?}");
}

// ---- real processes ---------------------------------------------------------------------------------------------------------------

struct Child1(Child);

impl Drop for Child1 {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(program: &str, args: &[&str], tag: Option<&str>) -> Child1 {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(t) = tag {
        cmd.env(TAG_ENV, t);
    }
    Child1(cmd.spawn().unwrap())
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

fn children_of(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(p) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        if rest
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
            == Some(pid)
        {
            out.push(p);
        }
    }
    out
}

fn wait_for<F: Fn() -> bool>(f: F) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn my_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|l| l.split_whitespace().next()?.parse().ok())
        .unwrap()
}

#[test]
fn b765_a_scan_of_the_real_proc_by_the_engine_users_uid_finds_a_tool_but_not_the_runtime_tree() {
    // Reads the real /proc and kills nothing: the uid is this test's own, so the set holds every process of this user.
    let front = spawn("sh", &["-c", "sleep 300 & wait"], None);
    let front_pid = front.0.id();
    assert!(
        wait_for(|| !children_of(front_pid).is_empty()),
        "the worker started"
    );
    let worker = children_of(front_pid)[0];
    let tool = spawn("sleep", &["300"], None);
    let tool_pid = tool.0.id();
    let r = Reaper::with_root(
        PathBuf::from("/proc"),
        std::process::id(),
        "b765-real-scan",
        Some(my_uid()),
    );
    let before = pids(&r);
    assert!(
        before.contains(&front_pid) && before.contains(&worker) && before.contains(&tool_pid),
        "TRIGGER PRESENT: {front_pid} {worker} {tool_pid} in {before:?}"
    );
    assert!(r.protect_pid(front_pid));
    let after = pids(&r);
    assert!(!after.contains(&front_pid), "{after:?}");
    assert!(!after.contains(&worker), "{after:?}");
    assert!(after.contains(&tool_pid), "{after:?}");
}

#[test]
fn b765_the_kill_stops_a_tool_and_leaves_the_protected_runtime_and_its_worker_running() {
    // The family is selected by the run's tag only (no uid), so nothing the test did not start can be hit.
    let tag = format!("b765-kill-{}", std::process::id());
    let front = spawn("sh", &["-c", "sleep 300 & wait"], Some(&tag));
    let front_pid = front.0.id();
    assert!(
        wait_for(|| !children_of(front_pid).is_empty()),
        "the worker started"
    );
    let worker = children_of(front_pid)[0];
    let tool = spawn("sleep", &["300"], Some(&tag));
    let tool_pid = tool.0.id();
    let r = Reaper::with_root(PathBuf::from("/proc"), std::process::id(), &tag, None);
    let family = pids(&r);
    assert!(
        family.contains(&front_pid) && family.contains(&worker) && family.contains(&tool_pid),
        "TRIGGER PRESENT: the tag finds all three: {family:?}"
    );
    assert!(r.protect_pid(front_pid));
    let report = r.reap_blocking(false, Duration::from_secs(5));
    assert!(report.remaining.is_empty(), "{report:?}");
    assert!(wait_for(|| !alive(tool_pid)), "the tool is stopped");
    assert!(alive(front_pid), "the protected runtime process was killed");
    assert!(alive(worker), "the protected runtime's worker was killed");
    assert!(!report.killed.contains(&front_pid) && !report.killed.contains(&worker));
}

// ---- the log line of a reap -------------------------------------------------------------------------------------------------------

fn names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{} sleep", 500 + i)).collect()
}

#[test]
fn b765_the_reap_line_names_what_it_stopped() {
    assert_eq!(killed_list(&[]), "");
    assert_eq!(killed_list(&names(2)), " (500 sleep, 501 sleep)");
    let report = Report {
        killed: vec![500, 501],
        killed_names: names(2),
        remaining: vec![9],
        ..Report::default()
    };
    assert_eq!(
        reaped_line(&report),
        "reaped 2 process(es) the engine left (500 sleep, 501 sleep), 1 remain"
    );
    assert_eq!(
        reaped_line(&Report::default()),
        "reaped 0 process(es) the engine left, 0 remain"
    );
}

#[test]
fn b765_the_reap_line_shows_the_first_eight_names_and_counts_the_rest() {
    let list = killed_list(&names(11));
    assert!(list.starts_with(" (500 sleep, 501 sleep"), "{list}");
    assert!(list.contains("507 sleep, and 3 more)"), "{list}");
    assert!(!list.contains("508 sleep"), "{list}");
}

#[test]
fn b765_a_zombie_cannot_be_protected() {
    let root = tree();
    // A zombie keeps its pid and its start time and runs nothing; what it left below it went to init.
    std::fs::write(
        root.join("300/stat"),
        format!("300 (python3.12) Z 1 1 1 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {} 123456 100 18446744073709551615\n", 1300),
    )
    .unwrap();
    let r = reaper(&root);
    assert!(!r.protect_pid(300));
}

// ---- the log line carries no control character (review finding F2) -----------------------------------------------------------------

/// The path of a program on `PATH`.
fn program_path(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .unwrap()
}

#[test]
fn b765_a_process_name_with_a_newline_cannot_start_a_line_of_the_log() {
    // The name of a process is text the engine chooses (a tool can set it), so the log line escapes whatever is not a plain printable character.
    let report = Report {
        killed: vec![31, 32, 33, 34],
        killed_names: vec![
            "31 x\ndirect turn r".to_string(),
            "32 tab\there\r".to_string(),
            "33 caf\u{e9}".to_string(),
            "34 back\\slash".to_string(),
        ],
        ..Report::default()
    };
    let line = reaped_line(&report);
    assert!(!line.chars().any(char::is_control), "{line:?}");
    assert_eq!(
        line,
        "reaped 4 process(es) the engine left (31 x\\ndirect turn r, 32 tab\\there\\r, 33 caf\\u{e9}, 34 back\\\\slash), 0 remain"
    );
}

#[test]
fn b765_a_real_process_with_a_newline_in_its_name_is_named_on_one_line() {
    // The kernel takes the name of a process from the name its program was started by, and /proc/<pid>/stat prints it as it is. A link whose
    // name holds a newline therefore makes a tool that carries one, the way a prctl of the tool would.
    let dir = common::scratch("b765-newline-name");
    let link = dir.join("x\nforged line");
    std::os::unix::fs::symlink(program_path("sleep"), &link).unwrap();
    let tag = format!("b765-newline-{}", std::process::id());
    let child = Child1(
        Command::new(&link)
            .arg("300")
            .env(TAG_ENV, &tag)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id();
    let r = Reaper::with_root(PathBuf::from("/proc"), std::process::id(), &tag, None);
    assert!(
        wait_for(|| pids(&r).contains(&pid)),
        "the tagged tool is in the family"
    );
    let report = r.reap_blocking(false, Duration::from_secs(5));
    assert!(
        report.killed_names.iter().any(|n| n.contains('\n')),
        "TRIGGER PRESENT: the name the reap read holds a newline: {:?}",
        report.killed_names
    );
    let line = reaped_line(&report);
    assert!(!line.chars().any(char::is_control), "{line:?}");
    assert!(line.contains("x\\nforged line"), "{line}");
}
