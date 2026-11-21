//! A save of the work must not fail on its own lock file, and must not change what it copies because of one.
//!
//! The measurement of the save's cost met this on a bench run and reproduced it with plain git: `git add -A` on a large file that is
//! rewritten in place ends with a bus error and leaves `index.lock` in the private repository of the copies. The look went on to the copy
//! of the whole tree, which is made for a repository whose history cannot be read any more, said that its copy was stored and left the
//! lock where it was. The next look could not stage at all ("Unable to create '.../index.lock': File exists"), and when its copy of the
//! whole tree failed too, the run was told that a copy of the work could not be saved. Lock files were removed only after a look that
//! had ended in an error.
//!
//! These tests hold three things. A lock that a git which has ended left behind does not send a look to the copy of the whole tree: the
//! lock is removed and the tree is staged again in the index agentd keeps, so the copy holds what that index tracks. A save whose
//! `git add` died stores its copy, and the save after it does not meet the lock. An index that cannot be used at all still gives the copy
//! of the whole tree, as before.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::snapshot::{GiveUp, Snapshotter};
use common::{git, python_calc, scratch};
use direct_support::{Direct, Opts, RUN, checkout_of_copy};
use harness_protocol::direct::s3 as keys;
use harness_protocol::runtime::TurnOutcome;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the run is told when a copy was given up or failed.
const NOT_SAVED: &str = "A copy of the work could not be saved; ham keeps trying.";

/// A repository whose base tracks `build/out.txt` while an ignore rule matches `build/` now, and the private repository of its copies.
/// Only the index agentd keeps, which is seeded with the base, still tracks that file: a copy of the whole tree leaves it out.
async fn setup(label: &str) -> (PathBuf, PathBuf, PathBuf, Snapshotter) {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    std::fs::create_dir_all(repo.join("build")).unwrap();
    std::fs::write(repo.join("build/out.txt"), "tracked at the base\n").unwrap();
    git(&repo, &["add", "-f", "build/out.txt"]);
    git(
        &repo,
        &["commit", "-q", "-m", "a build file the base tracks"],
    );
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::create_dir_all(repo.join(".git/info")).unwrap();
    let mut exclude = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(repo.join(".git/info/exclude"))
        .unwrap();
    writeln!(exclude, "build/").unwrap();
    drop(exclude);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let snap = Snapshotter::new(&data, &repo, &base).await.unwrap();
    (dir, repo, data, snap)
}

/// A clone of `bundle` on its `snapshot` branch.
fn clone_of(dir: &Path, bundle: &Path, name: &str) -> PathBuf {
    let clone = dir.join(name);
    git(
        dir,
        &[
            "clone",
            "-q",
            "-b",
            "snapshot",
            bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    clone
}

/// The lock files of the private repository that are there now.
fn locks_left(private: &Path) -> Vec<&'static str> {
    [
        "index.lock",
        "index-whole.lock",
        "HEAD.lock",
        "config.lock",
        "refs/heads/snapshot.lock",
        "snapshot.bundle.lock",
    ]
    .into_iter()
    .filter(|name| private.join(name).exists())
    .collect()
}

/// The private repository holds the lock of a `git add` that has ended (what a git that died in the middle leaves). Before, the look
/// went to the copy of the whole tree: it stored a copy without the file that only the kept index tracks, left the lock, and every look
/// after it hashed the whole tree again.
#[tokio::test]
async fn b860_a_lock_that_a_dead_git_left_does_not_fail_a_look_or_change_its_copy() {
    let (dir, repo, data, mut snap) = setup("b860-lock").await;
    let private = data.join("snapshot.git");
    std::fs::write(private.join("index.lock"), b"").unwrap();
    std::fs::write(repo.join("notes.txt"), "work after the lock was left\n").unwrap();
    let made = snap
        .snapshot_until(&GiveUp::at(None))
        .await
        .expect("the look ends without an error")
        .expect("the tree changed");
    assert_eq!(
        locks_left(&private),
        Vec::<&str>::new(),
        "TRIGGER PRESENT: the look stored a copy and left the lock of a git that has ended"
    );
    assert!(
        !private.join("index-whole").exists(),
        "TRIGGER PRESENT: a lock that no living process holds sent the look to the copy of the whole tree"
    );
    let clone = clone_of(&dir, &made.bundle, "clone");
    assert!(
        clone.join("build/out.txt").is_file(),
        "TRIGGER PRESENT: the copy lacks a file the base tracks: it was not made in the index agentd keeps"
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
        "work after the lock was left\n"
    );
    // The look after it: nothing changed, so no copy, and still no copy of the whole tree.
    assert_eq!(snap.snapshot_until(&GiveUp::at(None)).await, Ok(None));
    assert!(!private.join("index-whole").exists());
    // And the one after a change stores it.
    std::fs::write(repo.join("notes.txt"), "more work\n").unwrap();
    let next = snap
        .snapshot_until(&GiveUp::at(None))
        .await
        .unwrap()
        .expect("the tree changed");
    let clone = clone_of(&dir, &next.bundle, "clone-next");
    assert_eq!(
        std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
        "more work\n"
    );
    assert!(clone.join("build/out.txt").is_file());
}

/// The index agentd keeps cannot be read at all (not a lock: staging fails again after the locks are gone). The copy is then the whole
/// tree, as before this change: a look that can make a copy never makes none. No lock is left either way.
#[tokio::test]
async fn b860_an_index_that_cannot_be_used_still_gives_the_copy_of_the_whole_tree() {
    let (dir, repo, data, mut snap) = setup("b860-bad-index").await;
    let private = data.join("snapshot.git");
    std::fs::write(private.join("index"), b"this is no index file\n").unwrap();
    std::fs::write(repo.join("notes.txt"), "work\n").unwrap();
    let made = snap
        .snapshot_until(&GiveUp::at(None))
        .await
        .expect("the look ends without an error")
        .expect("the tree changed");
    let clone = clone_of(&dir, &made.bundle, "clone");
    assert_eq!(
        std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
        "work\n"
    );
    assert!(clone.join("src/calc.py").is_file());
    assert_eq!(locks_left(&private), Vec::<&str>::new());
    // The copy lacks the file that only the base tracks, and the run is told that a copy was stored: the worker's log is where the
    // difference is said, once.
    assert!(!clone.join("build/out.txt").exists());
    let note = snap.take_whole_note().expect(
        "TRIGGER PRESENT: the copy holds the whole tree after two failed stagings, and nothing says so",
    );
    assert!(
        note.contains("could not be staged") && note.contains("whole tree"),
        "{note}"
    );
    assert_eq!(snap.take_whole_note(), None, "the note is said once");
    // A look whose staging goes through says nothing.
    std::fs::remove_file(private.join("index")).unwrap();
    std::fs::write(repo.join("notes.txt"), "more work\n").unwrap();
    snap.snapshot_until(&GiveUp::at(None))
        .await
        .expect("the look ends without an error")
        .expect("the tree changed");
    assert_eq!(snap.take_whole_note(), None);
}

// ---- a whole turn: the save whose git add dies, and the save after it ------------------------------------------------------------

/// Every version of `snapshot.json` in the bucket, oldest first.
fn records(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
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

/// A checkout of the snapshot bundle the bucket holds now, on its `snapshot` branch. A copy of a whole turn holds what the run
/// changed since it started, so it is put together with the bundle the run started from, the way
/// every reader does it; cloned alone, git refuses it for the commits it needs.
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

/// A stand-in for what the measurement met, in a folder that leads the search path of `agentd serve`. While the file `die` is there, the
/// next `git add` into the index the copies keep takes the mark away, leaves that index's lock and ends on a bus error, as the git that
/// read a file while it was rewritten in place did. While the file `whole` is there, no `git add` into the index of a copy of the whole
/// tree works. Every other call is the real git.
fn dying_git(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("dying-git-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let die = dir.join("the-next-add-dies");
    let whole = dir.join("the-whole-tree-is-not-staged");
    let script = format!(
        r#"#!/bin/sh
add=0
for a in "$@"; do if [ "$a" = add ]; then add=1; fi; done
if [ "$add" = 1 ]; then
  case "$GIT_INDEX_FILE" in
    */snapshot.git/index)
      if mv '{die}' '{die}.done' 2>/dev/null; then
        : > "$GIT_DIR/index.lock"
        kill -s BUS $$
      fi ;;
    */snapshot.git/index-whole)
      if [ -e '{whole}' ]; then echo 'fatal: stand-in: the whole tree is not staged' >&2; exit 128; fi ;;
  esac
fi
exec '{real}' "$@"
"#,
        die = die.display(),
        whole = whole.display(),
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, die, whole)
}

/// A real `agentd serve` process whose engine wrote its files and now works on for `silence_s` seconds without a tool call, on a timer of
/// 300 ms; the copy of the engine's edits is stored.
async fn working_process(silence_s: &str, more: Vec<(&'static str, String)>) -> Direct {
    let mut agentd_env = vec![
        ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "300".to_string()),
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

/// The sequence the measurement met. The `git add` of one save dies and leaves its lock. Before, that save said "stored" (it had copied
/// the whole tree) and left the lock; the save after it could not stage, and when its copy of the whole tree failed too the run was told
/// that a copy of the work could not be saved, with an error that names the lock.
#[tokio::test(flavor = "multi_thread")]
async fn b860_a_save_whose_git_add_died_stores_its_copy_and_the_save_after_it_does_not_meet_the_lock()
 {
    let scratch = common::scratch("b860-died-add");
    let (bin, die, whole) = dying_git(&scratch);
    let d = working_process(
        "25",
        vec![("PATH", format!("{}:/usr/bin:/bin", bin.display()))],
    )
    .await;
    let lock = d.agent_data().join("snapshot.git/index.lock");
    let died = PathBuf::from(format!("{}.done", die.display()));
    let mut watch = CopyWatch::default();

    // The first save: its git add dies and leaves the lock.
    std::fs::write(&die, "set\n").unwrap();
    std::fs::write(
        d.worktree().join("first.txt"),
        "written before the save whose git add dies\n",
    )
    .unwrap();
    let first_stored = wait_for(Duration::from_secs(10), || {
        watch.new_copy_holds(&d, "first.txt")
    })
    .await;
    assert!(
        died.exists(),
        "TRIGGER PRESENT is not shown: the git add of a save did not die (the stand-in was not reached)"
    );
    assert!(
        first_stored,
        "the save whose git add died stored no copy: {:?}",
        records(&d)
    );
    let lock_left = lock.exists();

    // The save after it. A copy of the whole tree cannot be made now, as on the bench, where the same file ended that git too.
    std::fs::write(&whole, "set\n").unwrap();
    std::fs::write(
        d.worktree().join("second.txt"),
        "written before the save after it\n",
    )
    .unwrap();
    let second_stored = wait_for(Duration::from_secs(10), || {
        watch.new_copy_holds(&d, "second.txt")
    })
    .await;
    let notices = copy_notices(&d);
    let log = d.process.as_ref().unwrap().log_text();
    let failed: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("direct turn snapshot failed"))
        .collect();
    assert!(
        !notices.iter().any(|n| n == NOT_SAVED) && failed.is_empty(),
        "TRIGGER PRESENT: the save after one whose git add died failed on the lock that git left, and the run was told: notices \
         {notices:?}; log {failed:?}"
    );
    assert!(
        !lock_left,
        "TRIGGER PRESENT: the save whose git add died said its copy was stored and left the lock of that git"
    );
    assert!(second_stored, "{:?}", records(&d));
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d.summary().snapshots.failures,
        0,
        "no save of this run failed: {:?}",
        d.summary().snapshots
    );
    assert!(!lock.exists());
}
