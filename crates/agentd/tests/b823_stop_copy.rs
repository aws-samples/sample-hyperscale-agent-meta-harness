//! The copies of the work when something stops answering, and when agentd is asked to end.
//!
//! A session that was stopped from outside lived on for about a minute: it posted its events every two seconds and stored no copy, wrote no
//! log line and told the run nothing, although its timer was due. One look of the copy task that does not return (a work tree that stops
//! answering, a store that takes the upload and never answers) ended every later copy, because the task waited for it without a limit.
//!
//! These tests hold five things. A look whose upload gets no answer is said and given up, and the timer goes on. A git that hangs on the
//! work tree is said and ended, and the next look copies. A termination signal saves the newest work at once, also while a look is in
//! flight. The first process of its machine, which is what a session's microVM runs, runs on after that copy: a request to end cuts none
//! of its looks short, and a copy of a stop that failed is tried again soon. The time a copy names is before its tree was read, never
//! the time it was stored.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::snapshot::{
    COPY_GIT_LIMIT, COPY_LATE_AFTER, COPY_STORE_LIMIT, GiveUp, TREE_ANSWER_LIMIT, TreeAlive,
    cut_unix, late_after, store_limit,
};
use agentd::server::{Ending, ends_after_the_copy};
use direct_support::{Direct, Opts, RUN, checkout_of_copy, first_process_possible};
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::TurnOutcome;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the run is told when a look has run long and is reading the work tree.
const LATE_TREE: &str =
    "A copy of the work is taking long: reading the work tree has not ended. ham keeps trying.";
/// What the run is told when a look has run long and is storing its copy.
const LATE_STORE: &str =
    "A copy of the work is taking long: storing it has not ended. ham keeps trying.";
/// What the run is told when a copy was given up or failed.
const NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";
/// What the run is told when the copy that a request to end asked for could not be saved.
const STOP_NOT_SAVED: &str =
    "The session was asked to end, and a copy of the work could not be saved.";

/// Every version of `snapshot.json` in the bucket, oldest first: the fake store has them at once (the run's events reach the plane in batches).
fn records(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

/// The `savedCopy` members of the run's progress events, in order.
fn saved_copies(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .events()
        .into_iter()
        .filter(|(t, _)| *t == EventType::TurnProgress)
        .filter_map(|(_, v)| v.get("savedCopy").cloned())
        .collect()
}

/// The `copy_not_saved` notices of the run, in order.
fn copy_notices(d: &Direct) -> Vec<String> {
    d.aws
        .progress("notify")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["reason"] == "copy_not_saved")
        .map(|v| v["detail"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A checkout of the snapshot bundle the bucket holds now, on its `snapshot` branch: the copy alone when it holds the whole history (the
/// command the client prints), and on the bundle the run started from when the copy needs it, as every reader does.
fn clone_of_snapshot(d: &Direct, name: &str) -> PathBuf {
    checkout_of_copy(&d.aws, &d.dir, &keys::snapshot_bundle_key(RUN), name)
}

/// Waits until `seen` holds, at most `limit`; `false` when it never did.
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

/// Looks into the copy in the bucket each time a new one is there (its record is stored after its bundle, so a new record means both).
#[derive(Default)]
struct CopyWatch {
    seen: usize,
    clones: u32,
}

impl CopyWatch {
    /// Whether a copy that came since the last call holds the file `name`.
    fn new_copy_holds(&mut self, d: &Direct, name: &str) -> bool {
        let now = records(d).len();
        if now == self.seen {
            return false;
        }
        self.seen = now;
        self.clones += 1;
        clone_of_snapshot(d, &format!("copy-{}", self.clones))
            .join(name)
            .is_file()
    }
}

// ---- a look that does not return ------------------------------------------------------------------------------------------------

/// The store takes the upload of one copy's bundle and never answers it. Before, the copy task waited for that one look (up to 494 s in the
/// product: four requests of 120 s and their pauses), no later look ran and the run was told nothing until the look had failed.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_look_whose_upload_gets_no_answer_is_said_and_the_timer_goes_on() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "14".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
            c.direct.upload_retry_pause = Duration::from_millis(100);
            c.direct.copy_late = Some(Duration::from_millis(1_500));
            c.direct.copy_store_limit = Some(Duration::from_millis(3_500));
        })),
        ..Opts::default()
    })
    .await;
    // The first upload of a copy's bundle gets no answer for a minute; the store answers every PUT after it at once.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(60), 1);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let copies = saved_copies(&d);
    let notices = copy_notices(&d);
    assert!(
        copies.iter().any(|c| c.get("final").is_none()),
        "TRIGGER PRESENT: one upload got no answer and no copy was stored while the engine worked on for about 14 s with a 300 ms \
         timer: copies {copies:?}; notices {notices:?}"
    );
    // The run was told when the look was late. (On a build host under load the git work of that look can itself take longer than the late
    // limit of this test: the run is then told that reading the tree is slow.)
    assert!(
        notices
            .first()
            .is_some_and(|n| n == LATE_STORE || n == LATE_TREE),
        "{notices:?}"
    );
    // And it was told that the copy could not be saved when the same look was given up: the late sentence says that ham keeps trying, and
    // without this one the run never learns that the copy did not come.
    assert_eq!(
        notices.iter().filter(|n| *n == NOT_SAVED).count(),
        1,
        "TRIGGER PRESENT: a look was said to be late and was then given up, and the run was not told that its copy could not be saved: \
         {notices:?}"
    );
    assert!(
        notices
            .iter()
            .all(|n| n == LATE_STORE || n == LATE_TREE || n == NOT_SAVED),
        "{notices:?}"
    );
    let s = d.summary();
    assert!(
        s.snapshots.failures >= 1,
        "the look that was given up is counted: {:?}",
        s.snapshots
    );
    // The worker's log names the late look and the one that was given up.
    let log = d.log.lines().join("\n");
    assert!(log.contains("direct turn snapshot late: "), "{log}");
    assert!(
        log.contains("direct turn snapshot failed: not stored within"),
        "{log}"
    );
}

/// A stand-in for a work tree that stops answering: a `git` that never returns from `add` while the file `mark` exists, in a folder that
/// leads the search path of `agentd serve`. Every other call, and every call once the mark is gone, is the real git.
fn hanging_git(dir: &Path) -> (PathBuf, PathBuf) {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("slow-tree-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mark = dir.join("the-tree-does-not-answer");
    let script = format!(
        "#!/bin/sh\nif [ -e '{mark}' ]; then for a in \"$@\"; do if [ \"$a\" = add ]; then exec sleep 600; fi; done; fi\nexec '{real}' \"$@\"\n",
        mark = mark.display()
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, mark)
}

/// The work tree stops answering: the `git add` of the next look never returns. Before, the look was waited for without a limit, so no
/// later look ran, nothing was logged and the run was told nothing for as long as the process lived.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_git_that_hangs_on_the_work_tree_is_said_and_ended_and_the_next_look_copies() {
    let scratch = common::scratch("slow-tree");
    let (bin, mark) = hanging_git(&scratch);
    let d = working_process(
        "300",
        "14",
        vec![
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("AGENTD_DIRECT_COPY_LATE_MS", "400".into()),
            ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "1500".into()),
        ],
    )
    .await;
    // From now on the tree does not answer, and it holds a file that no copy has.
    std::fs::write(&mark, "set\n").unwrap();
    std::fs::write(
        d.worktree().join("late.txt"),
        "written after the last copy\n",
    )
    .unwrap();
    // The look of the next tick hangs in `git add`. The run is told when that look is late.
    let told = wait_for(Duration::from_secs(6), || !copy_notices(&d).is_empty()).await;
    // The tree answers again (also for the end commit of a build that never gives a look up).
    std::fs::remove_file(&mark).unwrap();
    assert!(
        told,
        "TRIGGER PRESENT: a look hung on the work tree for 6 s and the run was told nothing; copies {:?}",
        records(&d)
    );
    assert_eq!(copy_notices(&d), vec![LATE_TREE.to_string()]);
    // The look is given up, its git is ended, and a later look stores the file while the engine still works.
    let mut watch = CopyWatch::default();
    let copied = wait_for(Duration::from_secs(8), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        copied,
        "no copy held the file after the tree answered again: {:?}",
        records(&d)
    );
    let log = d.process.as_ref().unwrap().log_text();
    assert!(
        log.contains("direct turn snapshot late: reading the work tree"),
        "{log}"
    );
    assert!(log.contains("direct turn snapshot failed:"), "{log}");
    // No `sleep` of the stand-in is left behind: the git that was given up was ended.
    let left = std::process::Command::new("pgrep")
        .args([
            "-P",
            &d.process.as_ref().unwrap().pid().to_string(),
            "sleep",
        ])
        .output()
        .unwrap();
    assert!(
        left.stdout.is_empty(),
        "a git that was given up still runs: {}",
        String::from_utf8_lossy(&left.stdout)
    );
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(d.summary().snapshots.failures >= 1);
}

/// The file system of the work tree stops answering while a look reads the tree: here the tree is moved away, so the question agentd asks
/// the file system fails. The look is given up at that question, not at the long limit of its git work (a minute here, an hour in the
/// product), so a tree that is gone costs seconds and a repository whose copies take long keeps its copies.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_look_is_given_up_when_the_file_system_of_the_tree_no_longer_answers() {
    let scratch = common::scratch("tree-gone");
    let (bin, mark) = hanging_git(&scratch);
    let d = working_process(
        "300",
        "40",
        vec![
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("AGENTD_DIRECT_COPY_LATE_MS", "400".into()),
            // The git work of a look has a minute: only the question to the file system can end this look sooner.
            ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "60000".into()),
        ],
    )
    .await;
    std::fs::write(&mark, "set\n").unwrap();
    std::fs::write(
        d.worktree().join("late.txt"),
        "written after the last copy\n",
    )
    .unwrap();
    // The look of the next tick hangs in `git add`. The file system still answers, so the run is told that the copy is taking long.
    let told = wait_for(Duration::from_secs(6), || !copy_notices(&d).is_empty()).await;
    assert!(told, "the look that hangs is said: {:?}", records(&d));
    assert_eq!(copy_notices(&d), vec![LATE_TREE.to_string()]);
    // Now the tree is gone.
    let away = d.worktree().with_file_name("repo-away");
    std::fs::rename(d.worktree(), &away).unwrap();
    let log = || d.process.as_ref().unwrap().log_text();
    let given_up = wait_for(Duration::from_secs(5), || {
        log().contains("direct turn snapshot failed:")
    })
    .await;
    // The tree is back and answers again.
    std::fs::rename(&away, d.worktree()).unwrap();
    std::fs::remove_file(&mark).unwrap();
    assert!(
        given_up,
        "TRIGGER PRESENT: the tree was gone for 5 s and the look that hung on it was not given up; log:\n{}",
        log()
    );
    assert!(
        log().contains("direct turn snapshot: the work tree"),
        "the log names the file system that did not answer:\n{}",
        log()
    );
    // A later look stores the file.
    let mut watch = CopyWatch::default();
    let copied = wait_for(Duration::from_secs(8), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        copied,
        "no copy held the file after the tree was back: {:?}; log:\n{}",
        records(&d),
        log()
    );
}

// ---- agentd is asked to end ---------------------------------------------------------------------------------------------------------

/// A real `agentd serve` whose engine has edited its files and is silent for `silence_s` seconds; the copies its tool calls asked for are
/// in the bucket. The timer is `every_ms`; `more` is added to the environment of the process.
async fn working_process(
    every_ms: &str,
    silence_s: &str,
    more: Vec<(&'static str, String)>,
) -> Direct {
    let mut agentd_env = vec![
        ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", every_ms.to_string()),
        ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
    ];
    agentd_env.extend(more);
    let d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", silence_s.into())],
        agentd_env,
        ..Opts::default()
    })
    .await;
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(30), || !records(&d).is_empty()).await,
        "the copy after the engine's first tool call"
    );
    // The engine's edits are two tool calls: the copy the second one asks for is stored before the test goes on.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    d
}

/// Waits until the process has ended and returns its exit code; `None` when it still runs after `limit`.
async fn ended_within(d: &mut Direct, limit: Duration) -> Option<Option<i32>> {
    let began = Instant::now();
    loop {
        if let Some(code) = d.process.as_mut().unwrap().ended() {
            return Some(code);
        }
        if began.elapsed() > limit {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// agentd is asked to end while the engine works (the timer is an hour away and no tool call ends, so nothing else asks for a copy). Before,
/// agentd had no handler: the signal ended it at once where it is not the first process of its machine, and was ignored where it is, and in
/// both cases the work written since the last copy was in no copy.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_termination_signal_saves_the_newest_work_at_once() {
    let mut d = working_process("3600000", "40", Vec::new()).await;
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    std::fs::write(
        d.worktree().join("late.txt"),
        "written after the last copy and before the signal\n",
    )
    .unwrap();
    let asked = Instant::now();
    d.process.as_ref().unwrap().signal("TERM");
    let stored = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    let took = asked.elapsed();
    assert!(
        stored,
        "TRIGGER PRESENT: no copy after the termination signal holds the file written before it; the process ended with {:?}; log:\n{}",
        d.process.as_mut().unwrap().ended(),
        d.process.as_ref().unwrap().log_text()
    );
    assert!(
        took < Duration::from_secs(20),
        "the copy came {took:?} after the signal"
    );
    let last = records(&d).pop().unwrap();
    assert!(
        last.get("final").is_none(),
        "the engine had not stopped: this is no copy of the end: {last}"
    );
    // The process is not the first one of its machine here, so it ends as the signal asks, after the copy: 128 + 15.
    assert_eq!(
        ended_within(&mut d, Duration::from_secs(20)).await,
        Some(Some(143)),
        "log:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    let log = d.process.as_ref().unwrap().log_text();
    assert!(
        log.contains("termination signal SIGTERM: a copy of the work is saved first"),
        "{log}"
    );
    assert!(
        log.contains("direct turn session ending: a copy of the work was stored"),
        "{log}"
    );
}

/// The signal comes while a look is in flight: its tree was read before the last file was written, and its upload takes three seconds.
/// The copy that follows the signal must hold that last file all the same: a look in flight is no reason to skip it.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_termination_signal_during_a_look_in_flight_still_saves_the_newest_work() {
    let mut d = working_process("300", "40", Vec::new()).await;
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    // The next upload of a copy's bundle is answered after 3 s: the look that reads `mid.txt` is in flight for that long.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(3), 1);
    std::fs::write(d.worktree().join("mid.txt"), "read by the look in flight\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    std::fs::write(
        d.worktree().join("late.txt"),
        "written while that look stored its copy\n",
    )
    .unwrap();
    d.process.as_ref().unwrap().signal("TERM");
    let stored = wait_for(Duration::from_secs(25), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        stored,
        "TRIGGER PRESENT: the copy in the bucket does not hold the file written before the signal; records {:?}; the process ended \
         with {:?}; log:\n{}",
        records(&d),
        d.process.as_mut().unwrap().ended(),
        d.process.as_ref().unwrap().log_text()
    );
    assert_eq!(
        ended_within(&mut d, Duration::from_secs(20)).await,
        Some(Some(143))
    );
}

/// agentd is asked to end while the git of the stop copy hangs on the work tree: no copy can be made in the time the copy has. The run
/// is told so in a sentence of its own, which names the request and promises no further try: this process ends after the copy. Before,
/// the run read "ham keeps trying" from a worker that ended two event periods later.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_stop_copy_that_fails_tells_the_run_that_the_session_was_asked_to_end() {
    let scratch = common::scratch("stop-slow-tree");
    let (bin, mark) = hanging_git(&scratch);
    let mut d = working_process(
        "3600000",
        "40",
        vec![
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "3000".into()),
        ],
    )
    .await;
    let before = records(&d).len();
    std::fs::write(&mark, "set\n").unwrap();
    std::fs::write(d.worktree().join("late.txt"), "written before the signal\n").unwrap();
    d.process.as_ref().unwrap().signal("TERM");
    let told = wait_for(Duration::from_secs(12), || !copy_notices(&d).is_empty()).await;
    let code = ended_within(&mut d, Duration::from_secs(20)).await;
    std::fs::remove_file(&mark).unwrap();
    let log = d.process.as_ref().unwrap().log_text();
    assert!(
        told,
        "the run is told that the copy of the stop failed; log:\n{log}"
    );
    assert_eq!(
        copy_notices(&d),
        vec![STOP_NOT_SAVED.to_string()],
        "TRIGGER PRESENT: the copy of a stop failed and the run was not told that the session was asked to end; log:\n{log}"
    );
    assert!(
        log.contains("direct turn session ending: no copy of the work could be saved"),
        "{log}"
    );
    assert_eq!(code, Some(Some(143)), "log:\n{log}");
    assert_eq!(records(&d).len(), before, "no copy could be made");
}

/// A `git` that writes where it was started, the private repository it was given and its arguments to `calls`, and then is the real git.
fn recording_git(dir: &Path) -> (PathBuf, PathBuf) {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("recording-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = dir.join("git-calls.tsv");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\t%s\\t%s\\n' \"$(pwd -P)\" \"${{GIT_DIR:-}}\" \"$*\" >> '{calls}'\nexec '{real}' \"$@\"\n",
        calls = calls.display()
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, calls)
}

/// The git calls of a look that work on the tree are started outside it, and git changes to the tree itself (`-C`). Starting a process in
/// a directory waits until the new process is there; in a tree that stopped answering that wait cannot be ended, and it held the copy
/// task inside its own step, where no limit of the look and no late notice could run. Before, `git add -A`, `git write-tree` and the read
/// of the engine's head were started in the work tree.
#[tokio::test(flavor = "multi_thread")]
async fn b823_the_git_of_a_look_is_started_outside_the_work_tree() {
    let scratch = common::scratch("git-outside");
    let (bin, calls) = recording_git(&scratch);
    let d = working_process(
        "300",
        "6",
        vec![("PATH", format!("{}:/usr/bin:/bin", bin.display()))],
    )
    .await;
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    // The tree as agentd names it, and where it is (the folder a process started in it reports).
    let tree = d.worktree().display().to_string();
    let tree_is = std::fs::canonicalize(d.worktree())
        .unwrap()
        .display()
        .to_string();
    let text = std::fs::read_to_string(&calls).unwrap();
    // (where it was started, its private repository, its arguments) of every git call of agentd.
    let rows: Vec<(&str, &str, &str)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            Some((f.next()?, f.next()?, f.next()?))
        })
        .collect();
    // The calls of the copies: the ones that name the private repository of the copies.
    let of_copies = |verb: &str| -> Vec<&(&str, &str, &str)> {
        rows.iter()
            .filter(|(_, git_dir, args)| git_dir.ends_with("/snapshot.git") && args.ends_with(verb))
            .collect()
    };
    for verb in [" add -A", " write-tree"] {
        let found = of_copies(verb);
        assert!(!found.is_empty(), "the copies ran git{verb}: {text}");
        for (started_in, _, args) in found {
            assert!(
                !started_in.starts_with(&tree_is),
                "TRIGGER PRESENT: git{verb} of a copy was started in the work tree ({started_in}): {args}"
            );
            assert!(
                args.starts_with(&format!("-C {tree} ")),
                "git{verb} of a copy changes to the work tree itself: {args}"
            );
        }
    }
    // The read of the engine's head, which a look makes in the engine's repository, is started outside the tree too.
    let heads: Vec<_> = rows
        .iter()
        .filter(|(started_in, _, args)| {
            *started_in == "/"
                && args.starts_with(&format!("-C {tree} "))
                && args.ends_with(" rev-parse HEAD")
        })
        .collect();
    assert!(
        !heads.is_empty(),
        "TRIGGER PRESENT: no read of the engine's head was started outside the work tree: {text}"
    );
}

/// When the session's bundle file of the private repository was written last; `None` while there is none.
fn bundle_written(d: &Direct) -> Option<std::time::SystemTime> {
    std::fs::metadata(d.agent_data().join("snapshot.git/snapshot.bundle"))
        .and_then(|m| m.modified())
        .ok()
}

/// A termination signal while a turn is in its end sequence (here: while the copy of the end is stored). No copy task works then, so the
/// signal asks for no copy; the log says that a turn runs, not that none works. A process that is not the first of its machine ends at
/// once, as it did before agentd had a handler.
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_signal_in_the_end_sequence_is_logged_as_a_turn_outside_its_prompt_phase() {
    let mut d = working_process("3600000", "8", Vec::new()).await;
    // The copies the engine's tool calls asked for are stored by now; the engine is silent for some seconds more.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // The copy of the end is the next copy: its bundle goes up for 6 s.
    let before = bundle_written(&d);
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(6), 1);
    let storing = wait_for(Duration::from_secs(30), || {
        let now = bundle_written(&d);
        now.is_some() && now != before
    })
    .await;
    assert!(
        storing,
        "the engine stopped and the copy of the end is on its way"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    d.process.as_ref().unwrap().signal("TERM");
    let code = ended_within(&mut d, Duration::from_secs(10)).await;
    let log = d.process.as_ref().unwrap().log_text();
    assert_eq!(code, Some(Some(143)), "log:\n{log}");
    assert!(
        log.contains("termination signal SIGTERM: a turn runs outside its prompt phase"),
        "TRIGGER PRESENT: a signal in the end sequence of a turn was logged as if no turn worked; log:\n{log}"
    );
    assert!(
        !log.contains("no turn is working"),
        "a turn was working; log:\n{log}"
    );
}

// ---- agentd is the first process of its machine ---------------------------------------------------------------------------------------

/// [`working_process`] with `agentd serve` as process 1 of a process-id namespace of its own, which is what a session's microVM runs:
/// the kernel gives such a process no signal it has no handler for, and agentd runs on after the copy of a stop. `None` on a machine that
/// cannot make such a namespace; the test then says so and holds nothing.
async fn working_first_process(
    every_ms: &str,
    silence_s: &str,
    more: Vec<(&'static str, String)>,
) -> Option<Direct> {
    if !first_process_possible() {
        eprintln!(
            "SKIPPED: this machine cannot start a process as the first one of a process-id namespace (unshare --user --pid --fork)"
        );
        return None;
    }
    let mut agentd_env = vec![
        ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", every_ms.to_string()),
        ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
    ];
    agentd_env.extend(more);
    let d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", silence_s.into())],
        agentd_env,
        first_process: true,
        ..Opts::default()
    })
    .await;
    assert_eq!(
        d.process.as_ref().unwrap().own_pid(),
        Some(1),
        "TRIGGER PRESENT: agentd must be process 1 of its own namespace for this test"
    );
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(60), || !records(&d).is_empty()).await,
        "the copy after the engine's first tool call; log:\n{}",
        d.process.as_ref().unwrap().log_text()
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    Some(d)
}

/// A look reads `mid.txt`, writes its bundle and starts the upload, which the store answers after `put_s` seconds. Returns once that look
/// is storing (its bundle file is new), at most `limit` later.
async fn look_in_flight_storing(d: &Direct, put_s: u64, limit: Duration) {
    let before = bundle_written(d);
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(put_s), 1);
    std::fs::write(d.worktree().join("mid.txt"), "read by the look in flight\n").unwrap();
    let storing = wait_for(limit, || {
        let now = bundle_written(d);
        now.is_some() && now != before
    })
    .await;
    assert!(storing, "the look that reads mid.txt wrote its bundle");
    // The bundle file is in place before the upload starts: the look gets a moment to reach the upload.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Process 1 is asked to end while the engine works. The copy is saved at once, agentd runs on, the timer saves a later change, and a
/// second request saves a copy again. (A session's microVM runs agentd as its first process; before this test only the rule
/// `ends_after_the_copy(1)` was held.)
#[tokio::test(flavor = "multi_thread")]
async fn b823_the_first_process_saves_a_copy_when_asked_to_end_and_runs_on_and_the_timer_goes_on() {
    let Some(d) = working_first_process("4000", "90", Vec::new()).await else {
        return;
    };
    let agentd = d.process.as_ref().unwrap();
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    std::fs::write(d.worktree().join("late.txt"), "written before the signal\n").unwrap();
    agentd.signal("TERM");
    let stored = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    assert!(
        stored,
        "TRIGGER PRESENT: process 1 saved no copy when it was asked to end; log:\n{}",
        agentd.log_text()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        agentd.alive(),
        "process 1 runs on after the copy; log:\n{}",
        agentd.log_text()
    );
    // A change after the stop copy: only the timer can save it.
    std::fs::write(
        d.worktree().join("third.txt"),
        "written after the stop copy\n",
    )
    .unwrap();
    let by_timer = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "third.txt")
    })
    .await;
    assert!(
        by_timer,
        "the timer keeps saving copies after the stop copy; log:\n{}",
        agentd.log_text()
    );
    // A second request, with newer work.
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
        log.contains("direct turn session ending: a copy of the work was stored"),
        "{log}"
    );
    assert!(
        log.contains(
            "agentd is the first process of its machine and runs on until the machine ends"
        ),
        "{log}"
    );
    assert!(copy_notices(&d).is_empty(), "{:?}", copy_notices(&d));
}

/// Process 1 is asked to end while a look is in flight whose upload is answered about 5 s after it began. The time who asked waits for
/// the copy is 1.5 s here (30 s in the product); the timer is 20 s. Process 1 runs on, so nothing ends at that time: the look in flight
/// keeps its own limits and stores its copy, and one more look follows it at once for what was written after it had read the tree.
/// Before, the request cut the look in flight at that time, no look followed, and the next copy waited for the next tick of the timer: a
/// copy that needed longer than the budget was thrown away although the machine lived on.
#[tokio::test(flavor = "multi_thread")]
async fn b823_the_first_process_lets_the_look_in_flight_store_its_copy_and_copies_once_more() {
    let Some(d) = working_first_process(
        "20000",
        "120",
        vec![("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "1500".into())],
    )
    .await
    else {
        return;
    };
    let agentd = d.process.as_ref().unwrap();
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    // The look of the next tick reads `mid.txt`; its upload is answered 5 s after it began.
    look_in_flight_storing(&d, 5, Duration::from_secs(40)).await;
    std::fs::write(
        d.worktree().join("late.txt"),
        "written while that look stored its copy\n",
    )
    .unwrap();
    let asked = Instant::now();
    agentd.signal("TERM");
    // The upload in flight is answered about 5 s after it began: its copy is in the bucket well before the next tick, 20 s after the look
    // began.
    let mid = wait_for(Duration::from_secs(11), || {
        watch.new_copy_holds(&d, "mid.txt")
    })
    .await;
    let mid_took = asked.elapsed();
    assert!(
        mid,
        "TRIGGER PRESENT: process 1 runs on, and the copy that was in flight when it was asked to end was not stored {mid_took:?} after \
         the request; log:\n{}",
        agentd.log_text()
    );
    // The look that follows it stores what was written before the request.
    let late = clone_of_snapshot(&d, "after-the-look-in-flight")
        .join("late.txt")
        .is_file()
        || wait_for(Duration::from_secs(8), || {
            watch.new_copy_holds(&d, "late.txt")
        })
        .await;
    let late_took = asked.elapsed();
    let log = agentd.log_text();
    assert!(
        late,
        "TRIGGER PRESENT: no look followed the one in flight; the file written before the request was in no copy {late_took:?} after \
         it; log:\n{log}"
    );
    assert!(
        late_took < Duration::from_secs(16),
        "the copy came from the look that follows, not from the next tick: {late_took:?}"
    );
    assert!(agentd.alive(), "process 1 runs on; log:\n{log}");
    assert!(
        !log.contains("not stored in the time agentd had left"),
        "the look in flight kept its own limits; log:\n{log}"
    );
    assert!(
        log.contains("direct turn session ending: a copy of the work was stored"),
        "{log}"
    );
    assert!(copy_notices(&d).is_empty(), "{:?}", copy_notices(&d));
}

/// Process 1 is asked to end and the copy of the stop cannot be stored (the store takes its upload and does not answer). The timer is an
/// hour away. The run is told that the session was asked to end and that the copy could not be saved, and the copy task looks again a few
/// seconds later, because the process runs on: the store answers then, and the copy holds the work. Before, the next try was the next
/// tick of the timer, which a request to end had moved a whole period away.
#[tokio::test(flavor = "multi_thread")]
async fn b823_the_first_process_tries_a_stop_copy_that_failed_again_soon() {
    let Some(d) = working_first_process(
        "3600000",
        "120",
        vec![("AGENTD_DIRECT_COPY_STORE_LIMIT_MS", "1500".into())],
    )
    .await
    else {
        return;
    };
    let agentd = d.process.as_ref().unwrap();
    let mut watch = CopyWatch {
        seen: records(&d).len(),
        clones: 0,
    };
    // The next upload of a copy's bundle gets no answer for a minute; the store answers every PUT after it at once.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(60), 1);
    std::fs::write(d.worktree().join("late.txt"), "written before the signal\n").unwrap();
    let asked = Instant::now();
    agentd.signal("TERM");
    let stored = wait_for(Duration::from_secs(20), || {
        watch.new_copy_holds(&d, "late.txt")
    })
    .await;
    let took = asked.elapsed();
    let log = agentd.log_text();
    assert!(
        log.contains("direct turn session ending: no copy of the work could be saved"),
        "the copy of the stop failed first; log:\n{log}"
    );
    assert!(
        stored,
        "TRIGGER PRESENT: the copy of a stop failed, process 1 ran on, and no copy held the work {took:?} after the request (the timer \
         is an hour away); log:\n{log}"
    );
    assert_eq!(copy_notices(&d), vec![STOP_NOT_SAVED.to_string()], "{log}");
    assert!(agentd.alive(), "process 1 runs on; log:\n{log}");
}

/// A process that is not the first of its machine ends after the copy of a stop, so nothing of a look runs after the time that copy
/// has: a look in flight that needs longer is given up, and what it had read is lost with the process, as it was before agentd had a
/// handler. (The first process keeps such a look: `b823_the_first_process_lets_the_look_in_flight_store_its_copy_and_copies_once_more`.)
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_process_that_ends_after_the_copy_gives_a_look_in_flight_up_at_the_stop_budget() {
    let mut d = working_process(
        "300",
        "60",
        vec![("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "1500".into())],
    )
    .await;
    look_in_flight_storing(&d, 8, Duration::from_secs(30)).await;
    d.process.as_ref().unwrap().signal("TERM");
    let code = ended_within(&mut d, Duration::from_secs(30)).await;
    let log = d.process.as_ref().unwrap().log_text();
    assert_eq!(code, Some(Some(143)), "log:\n{log}");
    assert!(
        log.contains("not stored in the time agentd had left"),
        "the look in flight was given up at the stop budget; log:\n{log}"
    );
    assert!(
        log.contains("direct turn session ending: no copy of the work could be saved"),
        "log:\n{log}"
    );
}

// ---- the time a copy names ------------------------------------------------------------------------------------------------------------

/// The time in a copy's record is what the client prints as "changes after <time> are not in it". Before, it was the second in which the
/// bundle had been stored: a file written after the tree was read, in that second or before it, was not in the copy although the line
/// said so (seen live: a file of 08:20:41 missing from the copy "saved 08:20:41").
#[tokio::test(flavor = "multi_thread")]
async fn b823_a_copy_names_a_time_before_its_tree_was_read_not_the_time_it_was_stored() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "6".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
        })),
        ..Opts::default()
    })
    .await;
    // The first upload of a copy's bundle takes 2 s: the tree of that copy was read at least 2 s before its record is in the bucket.
    d.aws
        .delay_next_puts("snapshot.bundle", Duration::from_secs(2), 1);
    let watcher = async {
        let seen = wait_for(Duration::from_secs(60), || !records(&d).is_empty()).await;
        (seen, agentd::time::now_unix())
    };
    let ((_, resp), (seen, seen_unix)) = tokio::join!(d.invoke(), watcher);
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(seen, "a first copy");
    let first = records(&d).remove(0);
    let at = agentd::time::parse_rfc3339(first["at"].as_str().unwrap()).unwrap();
    assert!(
        seen_unix - at >= 2,
        "TRIGGER PRESENT: the copy names {} and its record was in the bucket at {seen_unix}, {} s later, but its bundle alone took 2 s to \
         store: the time it names is not from before its tree was read",
        first["at"],
        seen_unix - at
    );
    // The event of that copy carries the same time.
    assert_eq!(saved_copies(&d)[0]["at"], first["at"]);
}

// ---- the small parts ----------------------------------------------------------------------------------------------------------------

/// Whether `took` is `secs` seconds: the paused clock of a test ends a sleep at its time, rounded up to the next millisecond.
fn took_secs(took: Duration, secs: u64) -> bool {
    took >= Duration::from_secs(secs)
        && took < Duration::from_secs(secs) + Duration::from_millis(50)
}

#[test]
fn b823_a_copy_names_the_second_before_its_tree_was_read() {
    // A look that began to read the tree in second 100 names second 99: all of second 99 was over before the reading began.
    assert_eq!(cut_unix(100), 99);
}

#[test]
fn b823_the_limits_of_a_look_follow_the_last_copy_and_the_size_of_the_copy() {
    // The git work of a look never has more than an hour (`b823_copy_rules` holds the rule that gives it less).
    assert_eq!(COPY_GIT_LIMIT, Duration::from_secs(3_600));
    // A look is late after twenty seconds, and for a repository whose copies take long after three times its last stored copy.
    assert_eq!(late_after(COPY_LATE_AFTER, None), Duration::from_secs(20));
    assert_eq!(
        late_after(COPY_LATE_AFTER, Some(Duration::from_secs(1))),
        Duration::from_secs(20)
    );
    assert_eq!(
        late_after(COPY_LATE_AFTER, Some(Duration::from_secs(40))),
        Duration::from_secs(120)
    );
    // Thirty seconds to store a copy, and a second more for each mebibyte of its bundle.
    assert_eq!(store_limit(0), COPY_STORE_LIMIT);
    assert_eq!(store_limit(4_709), Duration::from_secs(30));
    assert_eq!(store_limit(8 * 1024 * 1024), Duration::from_secs(38));
    assert_eq!(store_limit(1024 * 1024 * 1024), Duration::from_secs(1_054));
}

#[tokio::test(start_paused = true)]
async fn b823_work_is_given_up_at_the_time_named_and_the_time_only_moves_forward() {
    let began = tokio::time::Instant::now();
    // No time: nothing is given up.
    let never = GiveUp::at(None);
    assert_eq!(
        never
            .run(tokio::time::sleep(Duration::from_secs(3_600)))
            .await,
        Some(())
    );
    // A time: work that ends before it answers, work that does not is given up at that time.
    let at = tokio::time::Instant::now() + Duration::from_secs(10);
    let give_up = GiveUp::at(Some(at));
    assert_eq!(give_up.run(async { 7 }).await, Some(7));
    assert_eq!(
        give_up
            .run(tokio::time::sleep(Duration::from_secs(60)))
            .await,
        None
    );
    assert!(took_secs(began.elapsed(), 3_610), "{:?}", began.elapsed());
    // The time is brought forward while the work runs (agentd was asked to end), never back.
    let give_up = GiveUp::at(Some(tokio::time::Instant::now() + Duration::from_secs(300)));
    let sooner = tokio::time::Instant::now() + Duration::from_secs(30);
    let work = give_up.run(tokio::time::sleep(Duration::from_secs(200)));
    let moved = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        give_up.no_later_than(sooner);
        give_up.no_later_than(sooner + Duration::from_secs(100));
    };
    let (ended, ()) = tokio::join!(work, moved);
    assert_eq!(ended, None);
    assert!(
        took_secs(began.elapsed(), 3_640),
        "given up 30 s after the look began, not 300 s: {:?}",
        began.elapsed()
    );
    // A time named for a look that had none.
    let give_up = GiveUp::at(None);
    give_up.no_later_than(tokio::time::Instant::now() + Duration::from_secs(1));
    assert_eq!(
        give_up
            .run(tokio::time::sleep(Duration::from_secs(2)))
            .await,
        None
    );
}

#[tokio::test(start_paused = true)]
async fn b823_who_asks_agentd_to_end_waits_for_the_copy_and_for_nothing_when_no_turn_works() {
    let ending = std::sync::Arc::new(Ending::default());
    // No copy task watches: there is no work to copy and nothing to wait for.
    assert!(!ending.watched());
    let n = ending.ask();
    assert_eq!(n, 1);
    let began = tokio::time::Instant::now();
    assert!(ending.saved(n, Duration::from_secs(30)).await);
    assert_eq!(began.elapsed(), Duration::ZERO);

    // A copy task watches: it is told, and who asked waits until the copy is saved.
    let mut watch = ending.watch();
    assert!(ending.watched());
    let n = ending.ask();
    assert_eq!(watch.asked().await, n);
    let waiting = ending.saved(n, Duration::from_secs(30));
    let copying = async {
        tokio::time::sleep(Duration::from_secs(4)).await;
        watch.saved(n);
    };
    let began = tokio::time::Instant::now();
    let (saved, ()) = tokio::join!(waiting, copying);
    assert!(saved);
    assert!(took_secs(began.elapsed(), 4), "{:?}", began.elapsed());

    // A copy that does not come in time: who asked goes on at the limit.
    let n = ending.ask();
    let began = tokio::time::Instant::now();
    assert!(!ending.saved(n, Duration::from_secs(30)).await);
    assert!(took_secs(began.elapsed(), 30), "{:?}", began.elapsed());

    // The prompt phase ends while someone waits (the copy task is dropped): the wait is over at once.
    let n = ending.ask();
    let waiting = ending.saved(n, Duration::from_secs(30));
    let ended = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(watch);
    };
    let began = tokio::time::Instant::now();
    let (released, ()) = tokio::join!(waiting, ended);
    assert!(released);
    assert!(took_secs(began.elapsed(), 2), "{:?}", began.elapsed());
    assert!(!ending.watched());
}

#[test]
fn b823_only_the_first_process_of_its_machine_runs_on_after_the_copy() {
    // Process 1 ignored the signal while agentd had no handler; it still runs on, and the timer keeps saving copies.
    assert!(!ends_after_the_copy(1));
    // Any other process was ended by the signal; it still ends, after the copy.
    assert!(ends_after_the_copy(2));
    assert!(ends_after_the_copy(std::process::id()));
}

#[tokio::test(flavor = "multi_thread")]
async fn b823_a_tree_that_is_there_answers_and_one_that_is_gone_is_named() {
    let dir = common::scratch("tree-alive");
    let tree = dir.join("repo");
    std::fs::create_dir_all(&tree).unwrap();
    let alive = TreeAlive::new(&tree);
    assert_eq!(alive.answers(TREE_ANSWER_LIMIT).await, Ok(()));
    // Asked again: no earlier question is outstanding.
    assert_eq!(alive.answers(TREE_ANSWER_LIMIT).await, Ok(()));
    std::fs::rename(&tree, dir.join("repo-away")).unwrap();
    let gone = alive.answers(TREE_ANSWER_LIMIT).await.unwrap_err();
    assert!(gone.starts_with("the work tree: "), "{gone}");
    std::fs::rename(dir.join("repo-away"), &tree).unwrap();
    assert_eq!(alive.answers(TREE_ANSWER_LIMIT).await, Ok(()));
}

#[test]
fn b823_the_settings_that_only_tests_set_are_read_in_milliseconds() {
    use agentd::config::DirectConfig;
    let from = |pairs: &[(&str, &str)]| {
        DirectConfig::from_vars(&|k: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_string())
        })
    };
    // Unset, as in the product: the constants hold.
    let d = from(&[]);
    assert_eq!(d.copy_late, None);
    assert_eq!(d.copy_git_limit, None);
    assert_eq!(d.copy_store_limit, None);
    assert_eq!(d.stop_copy_budget, None);
    let d = from(&[
        ("AGENTD_DIRECT_COPY_LATE_MS", "400"),
        ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "1500"),
        ("AGENTD_DIRECT_COPY_STORE_LIMIT_MS", " 2500 "),
        ("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", "5000"),
    ]);
    assert_eq!(d.copy_late, Some(Duration::from_millis(400)));
    assert_eq!(d.copy_git_limit, Some(Duration::from_millis(1_500)));
    assert_eq!(d.copy_store_limit, Some(Duration::from_millis(2_500)));
    assert_eq!(d.stop_copy_budget, Some(Duration::from_secs(5)));
    // Zero and a value that is no number leave the constant: a limit of nothing would give every look up.
    let d = from(&[
        ("AGENTD_DIRECT_COPY_LATE_MS", "0"),
        ("AGENTD_DIRECT_COPY_GIT_LIMIT_MS", "soon"),
        ("AGENTD_DIRECT_COPY_STORE_LIMIT_MS", "-1"),
        ("AGENTD_DIRECT_STOP_COPY_BUDGET_MS", ""),
    ]);
    assert_eq!(d.copy_late, None);
    assert_eq!(d.copy_git_limit, None);
    assert_eq!(d.copy_store_limit, None);
    assert_eq!(d.stop_copy_budget, None);
}
