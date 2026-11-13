//! The kill path in the `landlock` mode. The `uid` mode finds a tool that scrubbed its environment by the
//! engine's user. In the `landlock` mode the engine shares agentd's user, so the tag in the environment is the only mark left, and
//! `env -i <program>` or a daemon that execs with an empty environment escapes the end-of-run reap and keeps its write access to the checkout.
//!
//! What every process of the engine's family has, and agentd's own children do not, is the seccomp filter the launcher installs (`sandbox-exec`):
//! the kernel keeps it through `fork`, `setsid`, a double fork and an `exec` with a scrubbed environment, and no process can take it off. The
//! count is in `/proc/<pid>/status` as `Seccomp_filters`. A process that has more filters than agentd was started inside the sandbox. The
//! reaper also follows the parent chain from the processes it knows, so a child of a family member counts where the field is missing.
//!
//! Two kinds of test, as in `direct_messages_reap.rs`: the scan on a fabricated `/proc`, and real processes (Linux).
//!
//! The reaper reads the machine's `/proc`, so a real-process test must own its machine. Beside other sandboxed processes (a service of the host that
//! carries more filters than the test, a neighbour's engine) the reap counts them as the family: it cannot stop what another user owns, so
//! `remaining` never empties, and it kills what the test's own user owns. The real-process test therefore runs again inside a private user, PID
//! and mount namespace (`unshare --user --map-root-user --pid --fork --mount-proc`), where `/proc` lists only the test's own processes. Where
//! the machine cannot make one (a container that refuses `unshare`, which hosts one run anyway) it runs on the machine's `/proc`, as before.
#![allow(clippy::unwrap_used)]

use agentd::direct::messages::reap::{Reaper, TAG_ENV};
use std::path::{Path, PathBuf};

mod common;

struct Proc<'a> {
    pid: u32,
    comm: &'a str,
    state: char,
    ppid: u32,
    uid: u32,
    environ: &'a [&'a str],
    /// The `Seccomp_filters:` line of `status`; `None` leaves the line out, as a kernel older than 5.9 does.
    filters: Option<u32>,
}

fn plant(root: &Path, p: &Proc<'_>) {
    let d = root.join(p.pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("stat"),
        format!(
            "{} ({}) {} {} 1 1 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {} 123456 100 18446744073709551615\n",
            p.pid,
            p.comm,
            p.state,
            p.ppid,
            1000 + p.pid
        ),
    )
    .unwrap();
    let filters = p.filters.map_or(String::new(), |n| {
        format!("Seccomp:\t2\nSeccomp_filters:\t{n}\n")
    });
    std::fs::write(
        d.join("status"),
        format!(
            "Name:\t{}\nState:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t0\t0\t0\t0\n{filters}",
            p.comm, p.state, p.pid, p.ppid, p.uid, p.uid, p.uid, p.uid
        ),
    )
    .unwrap();
    std::fs::write(d.join("environ"), p.environ.join("\0") + "\0").unwrap();
}

fn scratch_proc() -> PathBuf {
    let d = common::scratch("proc-sbx");
    std::fs::create_dir_all(&d).unwrap();
    d
}

const SELF: u32 = 100;

fn proc<'a>(
    pid: u32,
    comm: &'a str,
    ppid: u32,
    environ: &'a [&'a str],
    filters: Option<u32>,
) -> Proc<'a> {
    Proc {
        pid,
        comm,
        state: 'S',
        ppid,
        uid: 0,
        environ,
        filters,
    }
}

#[test]
fn a_process_with_more_seccomp_filters_than_agentd_is_the_engines_whatever_its_environment_and_parent()
 {
    let root = scratch_proc();
    let tag = format!("{TAG_ENV}=run-a");
    let tagged = [tag.as_str()];
    // agentd has one filter (the container's). The engine has two (the container's and the launcher's), and so has everything it started.
    plant(&root, &proc(SELF, "agentd", 1, &[], Some(1)));
    plant(&root, &proc(200, "engine", SELF, &tagged, Some(2)));
    plant(&root, &proc(201, "orphan", 1, &[], Some(2)));
    plant(&root, &proc(202, "child", 200, &[], Some(2)));
    // agentd's own helpers (a git, a kill) have agentd's count; so has everything else on the machine.
    plant(&root, &proc(203, "git", SELF, &[], Some(1)));
    plant(&root, &proc(301, "platform", 1, &[], Some(1)));
    plant(&root, &proc(302, "no-filters", 1, &[], Some(0)));
    let r = Reaper::with_root(root, SELF, "run-a", None).with_sandbox_marks();
    let mut pids: Vec<u32> = r.scan().iter().map(|p| p.pid).collect();
    pids.sort_unstable();
    assert_eq!(
        pids,
        [200, 201, 202],
        "the orphan with an empty environment is part of the family; agentd's helpers are not"
    );
}

#[test]
fn where_the_kernel_reports_no_filter_count_a_child_of_a_known_process_still_counts() {
    // Kernels before 5.9 have no `Seccomp_filters` line. The parent chain still finds what has not been orphaned.
    let root = scratch_proc();
    let tag = format!("{TAG_ENV}=run-a");
    let tagged = [tag.as_str()];
    plant(&root, &proc(SELF, "agentd", 1, &[], None));
    plant(&root, &proc(200, "engine", SELF, &tagged, None));
    plant(&root, &proc(202, "scrubbed-child", 200, &[], None));
    plant(&root, &proc(203, "scrubbed-grandchild", 202, &[], None));
    plant(&root, &proc(204, "git", SELF, &[], None));
    plant(&root, &proc(301, "platform", 1, &[], None));
    let r = Reaper::with_root(root, SELF, "run-a", None).with_sandbox_marks();
    let mut pids: Vec<u32> = r.scan().iter().map(|p| p.pid).collect();
    pids.sort_unstable();
    assert_eq!(pids, [200, 202, 203]);
}

#[test]
fn a_zombie_with_more_filters_is_not_killed() {
    let root = scratch_proc();
    plant(&root, &proc(SELF, "agentd", 1, &[], Some(1)));
    plant(
        &root,
        &Proc {
            state: 'Z',
            ..proc(201, "dead", 1, &[], Some(2))
        },
    );
    let r = Reaper::with_root(root, SELF, "run-a", None).with_sandbox_marks();
    assert!(r.scan().is_empty());
}

#[test]
fn the_marks_are_off_unless_the_run_is_sandboxed() {
    // The same machine as the first test, read by a reaper that was not told the run is sandboxed: only the tag counts, as before.
    let root = scratch_proc();
    let tag = format!("{TAG_ENV}=run-a");
    let tagged = [tag.as_str()];
    plant(&root, &proc(SELF, "agentd", 1, &[], Some(1)));
    plant(&root, &proc(200, "engine", SELF, &tagged, Some(2)));
    plant(&root, &proc(201, "orphan", 1, &[], Some(2)));
    plant(&root, &proc(202, "child", 200, &[], Some(2)));
    let plain = Reaper::with_root(root.clone(), SELF, "run-a", None);
    assert!(!plain.uses_sandbox_marks());
    let pids: Vec<u32> = plain.scan().iter().map(|p| p.pid).collect();
    assert_eq!(pids, [200]);
    let marked = Reaper::for_run("run-a", None, true);
    assert!(marked.uses_sandbox_marks());
    assert!(!Reaper::for_run("run-a", None, false).uses_sandbox_marks());
}

#[test]
fn with_a_user_to_sweep_the_marks_change_nothing() {
    // The microVM: the engine has its own user, and what it owns is swept whole. The filter count and the parent chain add nothing there.
    let root = scratch_proc();
    let tag = format!("{TAG_ENV}=run-a");
    let tagged = [tag.as_str()];
    plant(&root, &proc(SELF, "agentd", 1, &[], Some(1)));
    plant(&root, &proc(200, "engine", SELF, &tagged, Some(2)));
    plant(
        &root,
        &Proc {
            uid: 1001,
            ..proc(201, "scrubbed", 1, &[], Some(0))
        },
    );
    plant(
        &root,
        &proc(202, "root-orphan-with-filters", 1, &[], Some(2)),
    );
    plant(&root, &proc(203, "child-of-tagged", 200, &[], Some(2)));
    let r = Reaper::with_root(root, SELF, "run-a", Some(1001)).with_sandbox_marks();
    assert!(!r.uses_sandbox_marks());
    let mut pids: Vec<u32> = r.scan().iter().map(|p| p.pid).collect();
    pids.sort_unstable();
    assert_eq!(pids, [200, 201]);
}

#[cfg(target_os = "linux")]
mod real_processes {
    use super::*;
    use agentd::sandbox::ipc;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CHILD: &str = "HAMH_REAP_SANDBOXED_CHILD";
    /// Set for the run inside the private namespace.
    const PRIVATE: &str = "HAMH_REAP_SANDBOXED_PRIVATE";
    const TEST: &str = "real_processes::a_tool_that_scrubs_its_environment_and_is_left_to_init_is_gone_after_the_reap";
    /// The processes in a private namespace are the test, the stand-in engine, its tool and their shells; a number past this means the
    /// machine's `/proc` is showing.
    const PRIVATE_PROC_LIMIT: usize = 20;

    /// The command that runs `test` of `exe` again in a user, PID and mount namespace of its own, with a fresh `/proc`: the test is process 1
    /// there, and no process of the machine is in view.
    pub(super) fn private_namespace_argv(unshare: &str, exe: &Path, test: &str) -> Vec<String> {
        [
            unshare,
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--mount-proc",
            "--",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .chain([
            exe.display().to_string(),
            "--exact".to_string(),
            test.to_string(),
            "--nocapture".to_string(),
        ])
        .collect()
    }

    /// Runs `test` again in a private namespace and returns what it printed. `None` when this machine cannot make the namespace (no
    /// `unshare`, or a kernel or container that refuses it): a probe of the same command line with `true` decides before the test starts.
    pub(super) fn rerun_in_private_namespace(
        unshare: &str,
        test: &str,
    ) -> Option<std::process::Output> {
        let probe = Command::new(unshare)
            .args(private_namespace_argv(unshare, Path::new("true"), "x")[1..7].iter())
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !probe.success() {
            return None;
        }
        let exe = std::env::current_exe().unwrap();
        let argv = private_namespace_argv(unshare, &exe, test);
        Command::new(&argv[0])
            .args(&argv[1..])
            .env(PRIVATE, "1")
            .stdin(Stdio::null())
            .output()
            .ok()
    }

    /// The number of processes `/proc` lists.
    fn visible_processes() -> usize {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.parse::<u32>().is_ok())
            })
            .count()
    }

    /// Alive means a process that is not a zombie.
    fn alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.trim_start().chars().next())
                .is_some_and(|c| c != 'Z' && c != 'X'),
            Err(_) => false,
        }
    }

    /// A stand-in tool script: a background program that runs with an empty environment, started from a subshell so that its parent ends and
    /// it is left to init. `$1` is the file that receives its pid. The script is a file, so no command line carries more than its path.
    fn tool_script(dir: &Path) -> PathBuf {
        let p = dir.join("tool.sh");
        std::fs::write(
            &p,
            "#!/bin/sh\n( env -i /bin/sleep 600 & echo $! > \"$1\" )\nexit 0\n",
        )
        .unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    #[test]
    fn a_tool_that_scrubs_its_environment_and_is_left_to_init_is_gone_after_the_reap() {
        if std::env::var(CHILD).is_ok() {
            // The stand-in engine: it installs the launcher's filter, starts the tool and stays alive until it is killed.
            ipc::install().unwrap();
            let script = std::env::var("HAMH_REAP_SANDBOXED_SCRIPT").unwrap();
            let pidfile = std::env::var("HAMH_REAP_SANDBOXED_PIDFILE").unwrap();
            let status = Command::new(script).arg(pidfile).status().unwrap();
            assert!(status.success());
            std::thread::sleep(Duration::from_secs(600));
            return;
        }
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        if !status.contains("Seccomp_filters:") {
            eprintln!("NOT RUN: this kernel reports no Seccomp_filters (before Linux 5.9)");
            return;
        }
        if std::env::var(PRIVATE).is_err() {
            // The reap reads the machine's /proc: run where the test's own processes are the only ones in view.
            if let Some(out) = rerun_in_private_namespace("unshare", TEST) {
                assert!(
                    out.status.success(),
                    "the run in the private namespace failed:\n{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                return;
            }
            eprintln!(
                "NOT ISOLATED: no private PID namespace here, the reap reads the machine's /proc"
            );
        } else {
            // TRIGGER PRESENT: the test is process 1 and /proc lists a handful of processes, not the machine's.
            assert_eq!(
                std::process::id(),
                1,
                "the test is not process 1 of a namespace"
            );
            let n = visible_processes();
            assert!(
                n < PRIVATE_PROC_LIMIT,
                "/proc lists {n} processes: the namespace has no /proc of its own"
            );
        }
        let dir = common::scratch("reap-sbx");
        let script = tool_script(&dir);
        let pidfile = dir.join("tool.pid");
        let tag = Reaper::new_tag();
        let mut engine = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env(TAG_ENV, &tag)
            .env("HAMH_REAP_SANDBOXED_SCRIPT", &script)
            .env("HAMH_REAP_SANDBOXED_PIDFILE", &pidfile)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(20);
        let tool: u32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < end,
                "the stand-in engine never started its tool"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        // TRIGGER PRESENT: the tool runs, carries no tag, and has another parent than the engine.
        assert!(alive(tool), "the tool is running");
        let environ = std::fs::read(format!("/proc/{tool}/environ")).unwrap();
        assert!(environ.is_empty(), "the tool's environment is empty");
        let stat = std::fs::read_to_string(format!("/proc/{tool}/stat")).unwrap();
        let ppid: u32 = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(ppid, engine.id(), "the tool was left to another parent");

        let report = Reaper::new(&tag, None)
            .with_sandbox_marks()
            .reap_blocking(false, Duration::from_secs(10));
        let end = Instant::now() + Duration::from_secs(5);
        while alive(tool) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        let still = alive(tool);
        // Whatever the verdict, nothing of this test stays behind.
        Command::new("kill")
            .args(["-KILL", &tool.to_string()])
            .status()
            .unwrap();
        let _ = engine.kill();
        let _ = engine.wait();
        assert!(
            !still,
            "the tool with an empty environment survived the reap: {report:?}"
        );
        assert!(report.remaining.is_empty(), "{report:?}");
    }
}

#[cfg(target_os = "linux")]
mod private_namespace {
    use super::real_processes::{private_namespace_argv, rerun_in_private_namespace};
    use std::path::Path;

    #[test]
    fn the_private_namespace_command_names_user_pid_and_mount_namespaces_and_a_fresh_proc() {
        let argv = private_namespace_argv("unshare", Path::new("/t/exe"), "a::b");
        assert_eq!(argv[0], "unshare");
        for flag in [
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--mount-proc",
        ] {
            assert!(argv.iter().any(|a| a == flag), "{flag} missing: {argv:?}");
        }
        let split = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            argv[split + 1..],
            ["/t/exe", "--exact", "a::b", "--nocapture"]
        );
    }

    #[test]
    fn a_machine_that_cannot_make_the_namespace_gets_none_and_runs_the_scenario_in_place() {
        // No such program, and a program that refuses (`false` exits 1 for any arguments).
        assert!(rerun_in_private_namespace("ham-no-such-unshare-program", "x").is_none());
        assert!(rerun_in_private_namespace("false", "x").is_none());
    }
}
