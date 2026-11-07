//! The kill path of the messages module: the processes an engine's tools
//! start survive a kill of the engine's process group, because a shell tool runs in its own session and a command that forks
//! and exits leaves its child to init. A `/now` that was not acknowledged, and the end of every engine, must leave none of them
//! behind: they would keep writing into the work tree after the next prompt or after the end commit.
//!
//! Two kinds of test. The scan is read from a fabricated `/proc` tree, so every case of the parsing runs anywhere. The kill runs real
//! processes (a shell that starts a tool in its own session, a command that forks and exits, a loop that forks as fast as it can)
//! and proves they are gone; it needs Linux, as the microVM does.
#![allow(clippy::unwrap_used)]

use agentd::direct::messages::reap::{Reaper, TAG_ENV};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod common;

// ---- the scan, on a fabricated /proc ----------------------------------------------------------------------------------------------

struct Proc<'a> {
    pid: u32,
    comm: &'a str,
    state: char,
    ppid: u32,
    uid: u32,
    environ: &'a [&'a str],
}

/// Writes the three files of `/proc/<pid>` the scan reads, in the format of the kernel.
fn plant(root: &Path, p: &Proc<'_>) {
    let d = root.join(p.pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime prio nice threads itreal start ...
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
    std::fs::write(
        d.join("status"),
        format!(
            "Name:\t{}\nState:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t0\t0\t0\t0\n",
            p.comm, p.state, p.pid, p.ppid, p.uid, p.uid, p.uid, p.uid
        ),
    )
    .unwrap();
    std::fs::write(d.join("environ"), p.environ.join("\0") + "\0").unwrap();
}

fn tag_line(tag: &str) -> String {
    format!("{TAG_ENV}={tag}")
}

fn scratch_proc() -> PathBuf {
    let d = common::scratch("proc");
    std::fs::create_dir_all(&d).unwrap();
    d
}

const SELF: u32 = 100;

#[test]
fn the_scan_finds_the_engine_and_every_process_that_carries_the_tag_wherever_it_sits_in_the_tree() {
    let root = scratch_proc();
    let tag = tag_line("run-a");
    let other = tag_line("run-b");
    // agentd itself (it started the engine), the engine, its tool, a tool that left its parent (reparented to init), and unrelated ones.
    plant(
        &root,
        &Proc {
            pid: SELF,
            comm: "agentd",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &["PATH=/usr/bin"],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 200,
            comm: "cline",
            state: 'S',
            ppid: SELF,
            uid: 0,
            environ: &[&tag, "PATH=/bin"],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 201,
            comm: "bash",
            state: 'S',
            ppid: 200,
            uid: 0,
            environ: &[&tag],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 202,
            comm: "sleep",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &["X=1", &tag],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 300,
            comm: "other-run",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &[&other],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 301,
            comm: "unrelated",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &["PATH=/bin"],
        },
    );
    let r = Reaper::with_root(root, SELF, "run-a", None);
    let mut pids: Vec<u32> = r.scan().iter().map(|p| p.pid).collect();
    pids.sort_unstable();
    assert_eq!(pids, [200, 201, 202]);
    assert_eq!(
        r.engine_pids(),
        [200],
        "the engine is the tagged child of agentd"
    );
}

#[test]
fn the_scan_never_lists_agentd_itself_even_if_it_carries_the_tag_and_skips_zombies() {
    let root = scratch_proc();
    let tag = tag_line("run-a");
    plant(
        &root,
        &Proc {
            pid: SELF,
            comm: "agentd",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &[&tag],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 200,
            comm: "gone",
            state: 'Z',
            ppid: 1,
            uid: 0,
            environ: &[],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 201,
            comm: "tool",
            state: 'R',
            ppid: 1,
            uid: 0,
            environ: &[&tag],
        },
    );
    let r = Reaper::with_root(root, SELF, "run-a", None);
    let pids: Vec<u32> = r.scan().iter().map(|p| p.pid).collect();
    assert_eq!(pids, [201]);
}

#[test]
fn the_engine_user_is_swept_whole_when_the_engine_runs_as_another_user() {
    // In the microVM the engine runs as its own user; everything that user owns is the engine's, tag or not.
    let root = scratch_proc();
    plant(
        &root,
        &Proc {
            pid: SELF,
            comm: "agentd",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &[],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 200,
            comm: "cline",
            state: 'S',
            ppid: SELF,
            uid: 1001,
            environ: &[],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 201,
            comm: "scrubbed",
            state: 'S',
            ppid: 1,
            uid: 1001,
            environ: &["ONLY=this"],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 202,
            comm: "agentd-helper",
            state: 'S',
            ppid: SELF,
            uid: 0,
            environ: &[],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 203,
            comm: "dead",
            state: 'Z',
            ppid: 1,
            uid: 1001,
            environ: &[],
        },
    );
    let r = Reaper::with_root(root, SELF, "run-a", Some(1001));
    let mut pids: Vec<u32> = r.scan().iter().map(|p| p.pid).collect();
    pids.sort_unstable();
    assert_eq!(pids, [200, 201]);
}

#[test]
fn a_command_name_with_spaces_and_parentheses_is_read_correctly() {
    let root = scratch_proc();
    let tag = tag_line("run-a");
    plant(
        &root,
        &Proc {
            pid: SELF,
            comm: "agentd",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &[],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 200,
            comm: "evil) S 1 (x",
            state: 'S',
            ppid: SELF,
            uid: 0,
            environ: &[&tag],
        },
    );
    let r = Reaper::with_root(root, SELF, "run-a", None);
    let found = r.scan();
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].pid, found[0].ppid, found[0].state),
        (200, SELF, 'S')
    );
    assert_eq!(r.engine_pids(), [200]);
}

#[test]
fn a_tag_must_match_whole_not_as_a_prefix_or_a_suffix_of_another() {
    let root = scratch_proc();
    plant(
        &root,
        &Proc {
            pid: SELF,
            comm: "agentd",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &[],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 200,
            comm: "a",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &[&tag_line("run-a-2")],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 201,
            comm: "b",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &[&format!("X{TAG_ENV}=run-a")],
        },
    );
    plant(
        &root,
        &Proc {
            pid: 202,
            comm: "c",
            state: 'S',
            ppid: 1,
            uid: 0,
            environ: &[&tag_line("run-a")],
        },
    );
    let r = Reaper::with_root(root, SELF, "run-a", None);
    let pids: Vec<u32> = r.scan().iter().map(|p| p.pid).collect();
    assert_eq!(pids, [202]);
}

#[test]
fn a_process_that_vanishes_while_it_is_read_is_not_an_error() {
    let root = scratch_proc();
    // a numeric directory with no files, a file where a directory is expected, a name that is not a pid
    std::fs::create_dir_all(root.join("500")).unwrap();
    std::fs::write(root.join("501"), "x").unwrap();
    std::fs::create_dir_all(root.join("self")).unwrap();
    let r = Reaper::with_root(root, SELF, "run-a", None);
    assert!(r.scan().is_empty());
}

#[test]
fn the_reaper_has_a_tag_that_is_new_for_every_run() {
    let a = Reaper::new_tag();
    let b = Reaper::new_tag();
    assert_ne!(a, b);
    assert!(
        a.len() >= 16 && a.chars().all(|c| c.is_ascii_alphanumeric()),
        "{a}"
    );
}

// ---- the kill, on real processes ----------------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod live {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    /// A stand-in engine: a shell that starts `sleep` the way Cline starts a shell tool, one in a session of its own (`setsid`), one that
    /// forks and exits so that its parent is left to init (a subshell), and one plain child; then it becomes a process that stays
    /// (`exec sleep`): the engine does not end when its tools do. Only `sleep`, `setsid` and the shell's own `&`, `( )` and `exec`.
    fn engine(tag: &str) -> Child {
        let script = "setsid sleep 600 &\n\
                      ( sleep 601 & )\n\
                      sleep 602 &\n\
                      exec sleep 1000\n";
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .env(TAG_ENV, tag)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    /// The `sleep` processes of the family other than the engine itself, once there are `want` of them: the tools. They are found by the
    /// run's tag, the mark the reaper uses; the stand-in does not write their pids anywhere.
    fn tools_of(tag: &str, engine_pid: u32, want: usize) -> Vec<u32> {
        let r = Reaper::new(tag, None);
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let v: Vec<u32> = r
                .scan()
                .into_iter()
                .filter(|p| p.name == "sleep" && p.pid != engine_pid)
                .map(|p| p.pid)
                .collect();
            if v.len() >= want {
                return v;
            }
            assert!(Instant::now() < end, "the stand-in engine started {v:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Alive means a process that is not a zombie (a zombie is dead; its parent has not collected it).
    fn alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.trim_start().chars().next())
                .is_some_and(|c| c != 'Z' && c != 'X'),
            Err(_) => false,
        }
    }

    fn all_gone(pids: &[u32], within: Duration) -> bool {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if pids.iter().all(|p| !alive(*p)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        pids.iter().all(|p| !alive(*p))
    }

    #[test]
    fn a_tool_in_its_own_session_and_a_child_left_to_init_are_gone_after_the_reap_and_the_engine_is_not()
     {
        let tag = Reaper::new_tag();
        let mut engine = engine(&tag);
        let tools = tools_of(&tag, engine.id(), 3);
        // TRIGGER PRESENT: the tools exist, one of them in a session of its own and one orphaned (its parent is not the engine).
        assert!(tools.iter().all(|p| alive(*p)), "{tools:?}");
        let engine_pid = engine.id();
        let parents: Vec<u32> = tools
            .iter()
            .map(|p| {
                let s = std::fs::read_to_string(format!("/proc/{p}/stat")).unwrap();
                s.rsplit_once(')')
                    .unwrap()
                    .1
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        assert!(
            parents.iter().any(|pp| *pp != engine_pid),
            "an orphaned tool: {parents:?}"
        );
        // A group kill of the engine misses the tool in its own session: the reason for this module.
        Command::new("kill")
            .args(["-KILL", "--", &format!("-{engine_pid}")])
            .status()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            tools.iter().any(|p| alive(*p)),
            "TRIGGER ABSENT: a process-group kill took every tool, so the test proves nothing"
        );
        let _ = engine.wait();

        let r = Reaper::new(&tag, None);
        let report = r.reap_blocking(false, Duration::from_secs(10));
        assert!(report.remaining.is_empty(), "{report:?}");
        assert!(all_gone(&tools, Duration::from_secs(5)), "{tools:?}");
    }

    #[test]
    fn keeping_the_engine_kills_only_what_the_engine_started() {
        let tag = Reaper::new_tag();
        let mut engine = engine(&tag);
        let tools = tools_of(&tag, engine.id(), 3);
        let r = Reaper::new(&tag, None);
        assert_eq!(r.engine_pids(), [engine.id()]);
        let report = r.reap_blocking(true, Duration::from_secs(10));
        assert!(report.remaining.is_empty(), "{report:?}");
        assert!(all_gone(&tools, Duration::from_secs(5)), "{tools:?}");
        assert!(alive(engine.id()), "the engine was kept");
        // and then the engine itself goes
        let report = r.reap_blocking(false, Duration::from_secs(10));
        assert!(report.killed.contains(&engine.id()), "{report:?}");
        let _ = engine.wait();
        assert!(!alive(engine.id()));
    }

    #[test]
    fn a_process_of_another_run_or_without_a_tag_is_left_alone() {
        let mine = Reaper::new_tag();
        let theirs = Reaper::new_tag();
        let mut e1 = engine(&mine);
        let mut e2 = engine(&theirs);
        let t1 = tools_of(&mine, e1.id(), 3);
        let t2 = tools_of(&theirs, e2.id(), 3);
        let report = Reaper::new(&mine, None).reap_blocking(false, Duration::from_secs(10));
        assert!(report.remaining.is_empty(), "{report:?}");
        assert!(all_gone(&t1, Duration::from_secs(5)));
        assert!(
            t2.iter().all(|p| alive(*p)),
            "the other run's tools were killed: {t2:?}"
        );
        assert!(alive(e2.id()));
        let _ = e1.wait();
        Reaper::new(&theirs, None).reap_blocking(false, Duration::from_secs(10));
        let _ = e2.wait();
    }

    #[test]
    fn a_parent_that_keeps_starting_children_as_fast_as_it_can_does_not_outrun_the_reap() {
        let tag = Reaper::new_tag();
        // A tagged parent, and a thread that starts a tagged `sleep` every few milliseconds for as long as the parent lives: children
        // appear between the reaper's scan and its kill, as they do from a loop that forks.
        let mut parent = Command::new("sleep")
            .arg("1000")
            .env(TAG_ENV, &tag)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let parent_pid = parent.id();
        let spawner = {
            let tag = tag.clone();
            std::thread::spawn(move || {
                let mut kids = Vec::new();
                while alive(parent_pid) {
                    if let Ok(k) = Command::new("sleep")
                        .arg("600")
                        .env(TAG_ENV, &tag)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        kids.push(k);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                kids
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        let r = Reaper::new(&tag, None);
        assert!(
            r.scan().len() > 5,
            "TRIGGER ABSENT: the parent started too few children"
        );
        let report = r.reap_blocking(false, Duration::from_secs(10));
        assert!(report.remaining.is_empty(), "{report:?}");
        assert!(report.rounds >= 1);
        let mut kids = spawner.join().unwrap();
        // the test is the parent of everything here: collect the dead
        let _ = parent.wait();
        for k in &mut kids {
            let _ = k.wait();
        }
        // nothing carries the tag any more
        assert!(Reaper::new(&tag, None).scan().is_empty());
    }

    #[test]
    fn a_reap_with_nothing_to_kill_returns_at_once() {
        let tag = Reaper::new_tag();
        let t = Instant::now();
        let report = Reaper::new(&tag, None).reap_blocking(false, Duration::from_secs(10));
        assert!(
            report.killed.is_empty() && report.remaining.is_empty(),
            "{report:?}"
        );
        assert!(t.elapsed() < Duration::from_secs(2));
    }
}
