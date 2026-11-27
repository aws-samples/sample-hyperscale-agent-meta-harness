//! A commit that changes no file is work of the run, and a look of the copy task stores it.
//!
//! A look compares the tree of the work with the tree of the last copy. The engine's `.git` is no part of that tree, so a commit of
//! files that a copy already holds, an amended message and a rebase to the same tree left the tree as it was: the look ended before it
//! read the engine's head, and the commit was in no stored copy until a file changed again or the run ended. A session that died in
//! between kept the files and lost the commit and its message.
//!
//! The look now reads the engine's head also when the tree is unchanged and makes a copy when the head is another commit than the
//! one the session compares with: the engine's head of the last copy the session made, or the head the session started on when it
//! has made none. A copy that could not be stored is forgotten by the caller (`Snapshotter::forget_last`), and the next look then
//! copies whatever it finds, so the head a look compares with is that of the last stored copy. The rules these tests hold, each
//! with the breakage that fails it in the change's records:
//!
//! - the state belongs to the session, never to the process;
//! - a session that restored a copy makes none until a file or the head changes;
//! - a head that moved and came back before a look stores nothing; a head that moved back or sideways stores a copy that restores;
//! - an unborn head stores nothing and fails no look; a head that cannot be read, or whose commit cannot be read, stores nothing
//!   for the head, keeps the copies of the files and does not move what the next look compares with;
//! - a copy that was not stored does not make the next look skip the head;
//! - the copy of the end without a head stands on the newest head a look read.
//!
//! These tests drive a [`Snapshotter`] with the host's git and no engine. The whole turns are the `b904_` tests of
//! `b860_kept_commits_run.rs`; what a look at an unchanged tree may start is held by `b860_look_cost.rs`.
#![allow(clippy::unwrap_used)]

mod b860_thin_support;
mod common;
mod direct_support;

use agentd::direct::snapshot::{SnapshotMade, Snapshotter};
use b860_thin_support::{Lab, lab, needed, restored, stage_base, thin};
use common::{FIXTURE_DATE, git, python_calc, scratch};
use std::path::{Path, PathBuf};

/// Another committer date than the fixtures', one hour later: a commit made again with it has another id.
const LATER_DATE: &str = "@1767229200 +0000";

/// git in `dir` as the engine runs it, with [`LATER_DATE`] as the committer date of a commit it makes.
fn git_later(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", FIXTURE_DATE)
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", LATER_DATE)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The engine commits what its work tree holds, also when that is nothing new; the answer is its new head.
fn engine_commits(repo: &Path, message: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

/// The tree of the work as it is now, hashed by the engine's own git.
fn work_tree(repo: &Path) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["write-tree"])
}

/// The parent of the copy's commit `commit`, read in the private repository under `data`.
fn parent_of(data: &Path, commit: &str) -> String {
    git(
        data,
        &[
            "--git-dir",
            data.join("snapshot.git").to_str().unwrap(),
            "rev-parse",
            &format!("{commit}^"),
        ],
    )
}

/// What a person does with a downloaded copy, by the commands `ham pull --snapshot` prints: the repository the run started from
/// first, then the copy fetched into it and checked out. The answer is the checkout.
fn looked_at(l: &Lab, name: &str, copy: &Path) -> PathBuf {
    let dir = l.dir.join(format!("look-{name}"));
    let kept = l.dir.join(format!("look-{name}.bundle"));
    std::fs::copy(copy, &kept).unwrap();
    if needed(&kept).is_empty() {
        git(
            &l.dir,
            &[
                "clone",
                "-q",
                "-b",
                "snapshot",
                kept.to_str().unwrap(),
                dir.to_str().unwrap(),
            ],
        );
    } else {
        git(
            &l.dir,
            &[
                "clone",
                "-q",
                l.base_bundle.to_str().unwrap(),
                dir.to_str().unwrap(),
            ],
        );
        git(&dir, &["fetch", "-q", kept.to_str().unwrap(), "snapshot"]);
        git(&dir, &["checkout", "-q", "-B", "look", "FETCH_HEAD"]);
    }
    dir
}

/// The copy `copy` restores, by the product's own restore and by the commands a person is given: the checkout stands on the copy's
/// commit, whose parent is `head` and whose tree is `tree`, and nothing differs from it.
async fn restores_on(l: &Lab, name: &str, copy: &SnapshotMade, head: &str, tree: &str) {
    let got = restored(l, name, &copy.bundle, &copy.commit).await;
    assert_eq!(
        git(&got, &["rev-parse", "HEAD^"]),
        head,
        "{name}: the copy stands on the engine's head"
    );
    assert_eq!(
        git(&got, &["rev-parse", "HEAD^{tree}"]),
        tree,
        "{name}: the copy holds the tree of the work"
    );
    let seen = looked_at(l, name, &copy.bundle);
    assert_eq!(git(&seen, &["rev-parse", "HEAD"]), copy.commit, "{name}");
    assert_eq!(git(&seen, &["rev-parse", "HEAD^"]), head, "{name}");
    assert_eq!(git(&seen, &["status", "--porcelain"]), "", "{name}");
}

/// A look that must make a copy because the engine's head moved: the copy, or the failure that names the gap.
async fn copy_of_the_head(s: &mut Snapshotter, what: &str) -> SnapshotMade {
    s.snapshot().await.unwrap().unwrap_or_else(|| {
        panic!(
            "TRIGGER PRESENT: the look after {what} stored nothing: the engine's head is in no copy until a file changes or the \
             run ends"
        )
    })
}

#[tokio::test]
async fn b904_a_commit_of_saved_files_is_in_the_copy_of_the_next_look() {
    let l = lab("b904-commit");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let first = s.snapshot().await.unwrap().expect("the tree changed");
    let tree = work_tree(&l.repo);
    // The engine commits the files that copy holds: no file changes.
    let head = engine_commits(&l.repo, "engine: the saved files");
    assert_eq!(work_tree(&l.repo), tree, "the commit changed no file");
    let copy = copy_of_the_head(&mut s, "a commit of saved files").await;
    assert_eq!((first.count, copy.count), (1, 2));
    assert!(!copy.final_copy);
    assert_eq!(needed(&copy.bundle), vec![l.base.clone()]);
    restores_on(&l, "commit", &copy, &head, &tree).await;
    let got = l.dir.join("look-commit");
    assert_eq!(
        git(&got, &["log", "-1", "--format=%s", "HEAD^"]),
        "engine: the saved files",
        "the copy holds the commit's message"
    );
    // The head is where that copy found it: the looks after it store nothing.
    for n in 0..3 {
        assert_eq!(
            s.snapshot().await.unwrap(),
            None,
            "look {n} after the copy of the head made a copy"
        );
    }
}

#[tokio::test]
async fn b904_an_amended_message_is_in_the_copy_of_the_next_look() {
    let l = lab("b904-amend");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let first_head = engine_commits(&l.repo, "engine: first message");
    let first = s.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(parent_of(&l.dir.join("data"), &first.commit), first_head);
    let tree = work_tree(&l.repo);
    git(
        &l.repo,
        &["commit", "-q", "--amend", "-m", "engine: second message"],
    );
    let head = git(&l.repo, &["rev-parse", "HEAD"]);
    assert_ne!(head, first_head, "the amended commit is another commit");
    assert_eq!(work_tree(&l.repo), tree, "the amend changed no file");
    let copy = copy_of_the_head(&mut s, "an amended message").await;
    restores_on(&l, "amend", &copy, &head, &tree).await;
    let got = l.dir.join("look-amend");
    let subjects = git(&got, &["log", "--format=%s", "HEAD"]);
    assert!(
        subjects.lines().any(|s| s == "engine: second message"),
        "{subjects}"
    );
    assert!(
        !subjects.lines().any(|s| s == "engine: first message"),
        "the copy holds the history as the engine left it: {subjects}"
    );
    assert_eq!(s.snapshot().await.unwrap(), None);
}

#[tokio::test]
async fn b904_a_rebase_to_the_same_tree_is_in_the_copy_of_the_next_look() {
    let l = lab("b904-rebase");
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/one.py"), "one = 1\n").unwrap();
    engine_commits(&l.repo, "engine: commit one");
    std::fs::write(l.repo.join("src/two.py"), "two = 2\n").unwrap();
    let before = engine_commits(&l.repo, "engine: commit two");
    s.snapshot().await.unwrap().expect("the tree changed");
    let tree = work_tree(&l.repo);
    git_later(&l.repo, &["rebase", "-q", "--force-rebase", &l.base]);
    let head = git(&l.repo, &["rev-parse", "HEAD"]);
    assert_ne!(head, before, "the rebase made the commits again");
    assert_eq!(work_tree(&l.repo), tree, "the rebase changed no file");
    let copy = copy_of_the_head(&mut s, "a rebase to the same tree").await;
    restores_on(&l, "rebase", &copy, &head, &tree).await;
    let got = l.dir.join("look-rebase");
    assert_eq!(
        git(
            &got,
            &["rev-list", "--count", &format!("{}..HEAD^", l.base)]
        ),
        "2",
        "the copy holds the two commits the rebase made"
    );
    assert!(
        std::process::Command::new("git")
            .current_dir(&got)
            .args(["cat-file", "-e", &format!("{before}^{{commit}}")])
            .output()
            .is_ok_and(|o| !o.status.success()),
        "the copy holds no commit from before the rebase"
    );
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// The state belongs to the session: two sessions in one process each compare with their own head.
#[tokio::test]
async fn b904_two_sessions_in_one_process_each_compare_with_their_own_head() {
    let a = lab("b904-two-a");
    let b = lab("b904-two-b");
    let mut sa = thin(&a, "data").await;
    let mut sb = thin(&b, "data").await;
    assert_eq!(sa.snapshot().await.unwrap(), None);
    // The engine of session b commits; the engine of session a does nothing.
    let hb = engine_commits(&b.repo, "engine b: a commit without a file");
    let cb = copy_of_the_head(&mut sb, "a commit in session b").await;
    assert_eq!(parent_of(&b.dir.join("data"), &cb.commit), hb);
    assert_eq!(
        sa.snapshot().await.unwrap(),
        None,
        "TRIGGER PRESENT: session a made a copy because the head of session b moved"
    );
    // And the other way round.
    let ha = engine_commits(&a.repo, "engine a: a commit without a file");
    assert_eq!(
        sb.snapshot().await.unwrap(),
        None,
        "TRIGGER PRESENT: session b made a copy because the head of session a moved"
    );
    let ca = copy_of_the_head(&mut sa, "a commit in session a").await;
    assert_eq!(parent_of(&a.dir.join("data"), &ca.commit), ha);
    assert_eq!((ca.count, cb.count), (1, 1), "each session counts its own");
    assert_eq!(sa.snapshot().await.unwrap(), None);
    assert_eq!(sb.snapshot().await.unwrap(), None);
}

/// A session that restored a copy stands on that copy's commit: that is the head it started on, and it makes no copy until a
/// file or the head changes.
#[tokio::test]
async fn b904_a_restored_session_makes_no_copy_until_a_file_or_the_head_changes() {
    let l = lab("b904-restored");
    let mut first = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let copy = first.snapshot().await.unwrap().expect("the tree changed");
    let got = restored(&l, "session-two", &copy.bundle, &copy.commit).await;
    assert_ne!(
        copy.commit, l.base,
        "the session starts on another commit than the run's base"
    );
    // The next session, in a new machine, on the repository restored from that copy.
    let again = Lab {
        dir: l.dir.clone(),
        repo: got.clone(),
        base_bundle: l.base_bundle.clone(),
        base: l.base.clone(),
    };
    let mut next = thin(&again, "data-two").await;
    for n in 0..3 {
        assert_eq!(
            next.snapshot().await.unwrap(),
            None,
            "TRIGGER PRESENT: look {n} of a session that restored a copy made a copy with nothing new in it"
        );
    }
    // The engine commits without a file change: the head moved, and the copy stands on it.
    let tree = work_tree(&got);
    let head = engine_commits(&got, "engine: after the restore");
    let second = copy_of_the_head(&mut next, "a commit in a restored session").await;
    assert_eq!(parent_of(&l.dir.join("data-two"), &second.commit), head);
    assert_eq!(git(&got, &["rev-parse", &format!("{head}^")]), copy.commit);
    restores_on(&again, "restored", &second, &head, &tree).await;
    assert_eq!(next.snapshot().await.unwrap(), None);
    // A file change is a copy as before.
    std::fs::write(got.join("src/b.py"), "b = 2\n").unwrap();
    next.snapshot().await.unwrap().expect("a file changed");
}

/// A head that moved and came back before a look is the head the session compares with: nothing is stored.
#[tokio::test]
async fn b904_a_head_that_moved_and_came_back_before_a_look_stores_nothing() {
    let l = lab("b904-back");
    let mut s = thin(&l, "data").await;
    // From the head the session started on, and back.
    engine_commits(&l.repo, "engine: made and taken back");
    git(&l.repo, &["reset", "-q", "--soft", &l.base]);
    assert_eq!(
        s.snapshot().await.unwrap(),
        None,
        "TRIGGER PRESENT: a head that is where the session started stored a copy"
    );
    // From the head of a stored copy, and back.
    let kept = engine_commits(&l.repo, "engine: kept");
    let copy = copy_of_the_head(&mut s, "a commit without a file").await;
    assert_eq!(parent_of(&l.dir.join("data"), &copy.commit), kept);
    engine_commits(&l.repo, "engine: made and taken back again");
    git(&l.repo, &["reset", "-q", "--soft", &kept]);
    assert_eq!(
        s.snapshot().await.unwrap(),
        None,
        "TRIGGER PRESENT: a head that is where the last copy found it stored a copy"
    );
    // Twenty commits between two looks are one copy.
    let mut head = String::new();
    for n in 0..20 {
        head = engine_commits(&l.repo, &format!("engine: commit {n} of twenty"));
    }
    let twenty = copy_of_the_head(&mut s, "twenty commits without a file").await;
    assert_eq!(parent_of(&l.dir.join("data"), &twenty.commit), head);
    assert_eq!(twenty.count, copy.count + 1);
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// A head that moved back below the last copy's head, to the run's base and below it, is another head: its copy is stored, and
/// it restores.
#[tokio::test]
async fn b904_a_head_that_moved_back_stores_a_copy_that_restores() {
    // A base with a parent, so that the head can go below the run's base.
    let dir = scratch("b904-moved-back");
    let repo = python_calc(&dir);
    std::fs::write(repo.join("src/base2.py"), "base = 2\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base, second commit"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let below = git(&repo, &["rev-parse", "HEAD^"]);
    let base_bundle = dir.join("repo.bundle");
    stage_base(&repo, &base_bundle);
    let l = Lab {
        dir,
        repo,
        base_bundle,
        base,
    };
    let mut s = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/one.py"), "one = 1\n").unwrap();
    let one = engine_commits(&l.repo, "engine: commit one");
    std::fs::write(l.repo.join("src/two.py"), "two = 2\n").unwrap();
    engine_commits(&l.repo, "engine: commit two");
    s.snapshot().await.unwrap().expect("the tree changed");
    let tree = work_tree(&l.repo);
    // Below the last copy's head, to the run's base, and below the base: the files stay, the head is another each time.
    for (name, to) in [("to-one", &one), ("to-base", &l.base), ("below", &below)] {
        git(&l.repo, &["reset", "-q", "--soft", to]);
        assert_eq!(
            work_tree(&l.repo),
            tree,
            "{name}: the reset changed no file"
        );
        let copy = copy_of_the_head(&mut s, "a reset that changed no file").await;
        assert_eq!(parent_of(&l.dir.join("data"), &copy.commit), *to, "{name}");
        restores_on(&l, name, &copy, to, &tree).await;
        assert_eq!(s.snapshot().await.unwrap(), None, "{name}");
    }
}

/// A head that names no commit yet stores nothing and fails no look; the first commit on it is another head.
#[tokio::test]
async fn b904_an_unborn_head_stores_nothing_and_fails_no_look() {
    let l = lab("b904-unborn");
    let mut s = thin(&l, "data").await;
    let tree = work_tree(&l.repo);
    git(&l.repo, &["checkout", "-q", "--orphan", "fresh"]);
    assert!(
        std::process::Command::new("git")
            .current_dir(&l.repo)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .is_ok_and(|o| !o.status.success()),
        "TRIGGER PRESENT is not shown: the head names a commit"
    );
    for n in 0..2 {
        assert_eq!(
            s.snapshot().await,
            Ok(None),
            "TRIGGER PRESENT: look {n} at a head that names no commit failed or stored a copy"
        );
    }
    let head = engine_commits(&l.repo, "engine: the first commit of a new branch");
    let copy = copy_of_the_head(&mut s, "the first commit of a new branch").await;
    assert_eq!(parent_of(&l.dir.join("data"), &copy.commit), head);
    restores_on(&l, "unborn", &copy, &head, &tree).await;
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// A session that STARTS on a branch with no commit. The start's read of the engine's head names no commit, so the session
/// has no head to compare with and starts on the tree of the run's base. Its first looks are quiet and fail nothing; the first
/// commit of the branch is in the next look's copy, and that copy restores.
#[tokio::test]
async fn b904_a_session_that_starts_on_a_branch_with_no_commit_stores_the_first_commit() {
    let l = lab("b904-start-unborn");
    let tree = work_tree(&l.repo);
    git(&l.repo, &["checkout", "-q", "--orphan", "fresh"]);
    assert!(
        std::process::Command::new("git")
            .current_dir(&l.repo)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .is_ok_and(|o| !o.status.success()),
        "TRIGGER PRESENT is not shown: the head names a commit when the session starts"
    );
    let mut s = thin(&l, "data").await;
    for n in 0..2 {
        assert_eq!(
            s.snapshot().await,
            Ok(None),
            "TRIGGER PRESENT: look {n} of a session that started on a head that names no commit failed or stored a copy"
        );
    }
    let head = engine_commits(
        &l.repo,
        "engine: the first commit of the branch the session started on",
    );
    let copy = copy_of_the_head(
        &mut s,
        "the first commit of the branch the session started on",
    )
    .await;
    assert_eq!(parent_of(&l.dir.join("data"), &copy.commit), head);
    restores_on(&l, "start-unborn", &copy, &head, &tree).await;
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// A head that cannot be read stores nothing for the head and fails no look, the copies of the files go on, and the head the
/// next look compares with stays: once the head can be read again, the commit that was made meanwhile is stored.
#[tokio::test]
async fn b904_a_head_that_cannot_be_read_keeps_the_file_copies_and_what_the_next_look_compares_with()
 {
    let l = lab("b904-unread");
    let mut s = thin(&l, "data").await;
    let made = engine_commits(&l.repo, "engine: made before the head could not be read");
    // The file of the branch the head names holds no commit id: the repository is one, and git cannot read its head.
    let head_file = l.repo.join(".git/refs/heads/main");
    let named = std::fs::read_to_string(&head_file).unwrap();
    assert_eq!(named.trim(), made, "the branch's own file names the head");
    std::fs::write(&head_file, "this names no commit\n").unwrap();
    let asked = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(&l.repo)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    assert!(
        asked(&["rev-parse", "--git-dir"]) && !asked(&["rev-parse", "HEAD"]),
        "TRIGGER PRESENT is not shown: git reads the head, or the folder is no repository any more"
    );
    for n in 0..2 {
        assert_eq!(
            s.snapshot().await,
            Ok(None),
            "TRIGGER PRESENT: look {n} at a head that cannot be read failed or stored a copy"
        );
    }
    // A file changes: its copy is made as before, on the head the session knows.
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let files =
        s.snapshot().await.unwrap().expect(
            "TRIGGER PRESENT: a changed file was not copied while the head could not be read",
        );
    assert_eq!(parent_of(&l.dir.join("data"), &files.commit), l.base);
    assert_eq!(s.snapshot().await, Ok(None));
    // The head can be read again: it is another than the one the session compares with, and its copy is stored.
    std::fs::write(&head_file, named).unwrap();
    let tree = work_tree(&l.repo);
    let copy = copy_of_the_head(&mut s, "a commit made while the head could not be read").await;
    assert_eq!(parent_of(&l.dir.join("data"), &copy.commit), made);
    restores_on(&l, "unread", &copy, &made, &tree).await;
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// A head that names a commit which no object store holds is nothing a copy can stand on: git answers with the id, and the
/// private repository cannot read the commit. With the tree as the last copy has it the look stores nothing and fails nothing;
/// the copies of changed files go on; and the head the next look compares with stays, so the commit that was made before is
/// stored once its head can be read again.
#[tokio::test]
async fn b904_a_moved_head_whose_commit_cannot_be_read_stores_nothing_for_the_head() {
    let l = lab("b904-no-object");
    let mut s = thin(&l, "data").await;
    let made = engine_commits(
        &l.repo,
        "engine: made before the branch named a commit that is not there",
    );
    let branch = l.repo.join(".git/refs/heads/main");
    let named = std::fs::read_to_string(&branch).unwrap();
    assert_eq!(named.trim(), made, "the branch's own file names the head");
    let absent = "1234567890abcdef1234567890abcdef12345678";
    std::fs::write(&branch, format!("{absent}\n")).unwrap();
    assert_eq!(
        git(&l.repo, &["rev-parse", "HEAD"]),
        absent,
        "TRIGGER PRESENT is not shown: git does not answer with the id the branch names"
    );
    for n in 0..3 {
        assert_eq!(
            s.snapshot().await,
            Ok(None),
            "TRIGGER PRESENT: look {n} at a head whose commit cannot be read failed or stored a copy with nothing new in it"
        );
    }
    // A file changes: its copy is made as before, on the head the session knows.
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let files = s.snapshot().await.unwrap().expect(
        "TRIGGER PRESENT: a changed file was not copied while the head's commit could not be read",
    );
    assert_eq!(parent_of(&l.dir.join("data"), &files.commit), l.base);
    assert_eq!(s.snapshot().await, Ok(None));
    // The branch names its commit again: it is another than the one the session compares with, and its copy is stored.
    std::fs::write(&branch, named).unwrap();
    let tree = work_tree(&l.repo);
    let copy = copy_of_the_head(&mut s, "a commit whose head could not be read for a while").await;
    assert_eq!(parent_of(&l.dir.join("data"), &copy.commit), made);
    restores_on(&l, "no-object", &copy, &made, &tree).await;
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// While `.git` is not the repository's own directory the worker runs no git in the engine's repository, so a look reads no head
/// there and a commit stores nothing.
#[tokio::test]
async fn b904_a_look_reads_no_head_while_the_git_folder_is_not_the_repositorys_own() {
    let l = lab("b904-not-own");
    let mut s = thin(&l, "data").await;
    s.set_git_dir_reader(std::sync::Arc::new(|_| false));
    engine_commits(&l.repo, "engine: a commit the worker may not read");
    assert_eq!(
        s.snapshot().await,
        Ok(None),
        "TRIGGER PRESENT: a look read the engine's head through a git folder that is not the repository's own"
    );
}

/// A copy that could not be stored is forgotten by the copy task: the next look does not skip the head that copy was made for.
#[tokio::test]
async fn b904_a_copy_that_was_not_stored_does_not_make_the_next_look_skip_the_head() {
    let l = lab("b904-not-stored");
    let mut s = thin(&l, "data").await;
    let tree = work_tree(&l.repo);
    let head = engine_commits(&l.repo, "engine: a commit whose copy is refused");
    let refused = copy_of_the_head(&mut s, "a commit without a file").await;
    // What the copy task does when the bucket refused the copy.
    s.forget_last();
    let again = s.snapshot().await.unwrap().expect(
        "TRIGGER PRESENT: the look after a copy that was not stored stored nothing: the engine's head is in no stored copy",
    );
    assert_eq!(again.count, refused.count + 1);
    assert_eq!(parent_of(&l.dir.join("data"), &again.commit), head);
    restores_on(&l, "not-stored", &again, &head, &tree).await;
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// The head a session compares with is that of its last STORED copy. A copy of a moved head is refused, and the copy task forgets
/// it; then the commit that head names cannot be read for a while, and the look that follows stores the files without it. Once
/// the commit can be read again, no stored copy holds it: the look stores it.
#[tokio::test]
async fn b904_a_copy_that_was_not_stored_leaves_the_head_the_session_compares_with() {
    let l = lab("b904-refused-then-unreadable");
    let mut s = thin(&l, "data").await;
    let tree = work_tree(&l.repo);
    let head = engine_commits(&l.repo, "engine: a commit whose copy is refused");
    copy_of_the_head(&mut s, "a commit without a file").await;
    // What the copy task does when the bucket refused the copy.
    s.forget_last();
    // The commit's own object is out of the engine's store: git still answers with its id, and nothing can read it.
    let object = l
        .repo
        .join(".git/objects")
        .join(&head[..2])
        .join(&head[2..]);
    let away = object.with_extension("away");
    std::fs::rename(&object, &away).unwrap();
    assert_eq!(git(&l.repo, &["rev-parse", "HEAD"]), head);
    let files = s.snapshot().await.unwrap().expect(
        "the look after a copy that was not stored copies the files, with or without the head",
    );
    assert_ne!(
        std::process::Command::new("git")
            .current_dir(l.dir.join("data"))
            .args(["--git-dir", "snapshot.git", "rev-parse", "--verify", "-q"])
            .arg(format!("{}^", files.commit))
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default(),
        head,
        "TRIGGER PRESENT is not shown: the copy made while the commit could not be read stands on it"
    );
    // The commit can be read again. No stored copy holds it.
    std::fs::rename(&away, &object).unwrap();
    let copy = s.snapshot().await.unwrap().expect(
        "TRIGGER PRESENT: the copy of the engine's head was never stored, and the look that can read the head again stored nothing: \
         the commit is in no stored copy",
    );
    assert_eq!(parent_of(&l.dir.join("data"), &copy.commit), head);
    restores_on(&l, "refused-then-unreadable", &copy, &head, &tree).await;
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// A copy that is made without a head (the head cannot be read, a file changed) leaves the head the session compares with: when
/// the head can be read again and is the one the session knows, the look stores nothing.
#[tokio::test]
async fn b904_a_copy_made_without_a_head_leaves_the_head_the_session_compares_with() {
    let l = lab("b904-no-head-copy");
    let mut s = thin(&l, "data").await;
    let branch = l.repo.join(".git/refs/heads/main");
    let named = std::fs::read_to_string(&branch).unwrap();
    std::fs::write(&branch, "this names no commit\n").unwrap();
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    let files = s
        .snapshot()
        .await
        .unwrap()
        .expect("a changed file is copied while the head cannot be read");
    assert_eq!(parent_of(&l.dir.join("data"), &files.commit), l.base);
    std::fs::write(&branch, named).unwrap();
    assert_eq!(
        git(&l.repo, &["rev-parse", "HEAD"]),
        l.base,
        "the head is the one the session started on"
    );
    for n in 0..2 {
        assert_eq!(
            s.snapshot().await,
            Ok(None),
            "TRIGGER PRESENT: look {n} at the head the session knows stored a copy with nothing new in it"
        );
    }
}

/// A look that fails after it read a moved head does not skip that head: the next look stores it.
#[tokio::test]
async fn b904_a_look_that_failed_after_it_read_the_head_does_not_skip_the_head() {
    let l = lab("b904-failed-look");
    let mut s = thin(&l, "data").await;
    let tree = work_tree(&l.repo);
    let head = engine_commits(&l.repo, "engine: a commit whose first look fails");
    // The bundle of a copy cannot be written: a folder stands where its file goes.
    let bundle = l.dir.join("data/snapshot.git/snapshot.bundle");
    std::fs::create_dir(&bundle).unwrap();
    let failed = s.snapshot().await;
    assert!(
        failed.is_err(),
        "TRIGGER PRESENT is not shown: the look whose bundle cannot be written did not fail: {failed:?}"
    );
    std::fs::remove_dir(&bundle).unwrap();
    let copy = s.snapshot().await.unwrap().expect(
        "TRIGGER PRESENT: a look failed after it read the engine's moved head, and the look after it stored nothing: the commit \
         is in no copy",
    );
    assert_eq!(parent_of(&l.dir.join("data"), &copy.commit), head);
    restores_on(&l, "failed-look", &copy, &head, &tree).await;
    assert_eq!(s.snapshot().await.unwrap(), None);
}

/// A session that restored a copy and made none of its own: its copy of the end, given no head, stands on the commit the session
/// started on, which every look read, and so holds the commits of the sessions before it. It stood on the run's base before.
#[tokio::test]
async fn b904_the_copy_of_the_end_of_a_restored_session_that_made_no_copy_stands_on_the_head_it_started_on()
 {
    let l = lab("b904-end-restored");
    let mut first = thin(&l, "data").await;
    std::fs::write(l.repo.join("src/a.py"), "a = 1\n").unwrap();
    engine_commits(&l.repo, "engine: the work of the first session");
    let copy = first.snapshot().await.unwrap().expect("the tree changed");
    let got = restored(&l, "end-session-two", &copy.bundle, &copy.commit).await;
    let again = Lab {
        dir: l.dir.clone(),
        repo: got.clone(),
        base_bundle: l.base_bundle.clone(),
        base: l.base.clone(),
    };
    let mut next = thin(&again, "data-two").await;
    assert_eq!(next.snapshot().await.unwrap(), None);
    let tree = work_tree(&got);
    let end = next.final_copy(None).await.unwrap();
    assert!(end.final_copy);
    assert_eq!(
        parent_of(&l.dir.join("data-two"), &end.commit),
        copy.commit,
        "TRIGGER PRESENT: the copy of the end of a session that restored a copy does not stand on that copy: it lacks the commits \
         of the sessions before it"
    );
    restores_on(&again, "end-restored", &end, &copy.commit, &tree).await;
    let seen = l.dir.join("look-end-restored");
    assert!(
        git(&seen, &["log", "--format=%s", "HEAD"])
            .lines()
            .any(|s| s == "engine: the work of the first session"),
        "the copy of the end holds the first session's commit"
    );
}

/// The copy of the end that is given no head stands on the newest head a look read.
#[tokio::test]
async fn b904_the_copy_of_the_end_without_a_head_stands_on_the_newest_head_a_look_read() {
    let l = lab("b904-end");
    let mut s = thin(&l, "data").await;
    let data = l.dir.join("data");
    let tree = work_tree(&l.repo);
    engine_commits(&l.repo, "engine: first commit without a file");
    let _ = s.snapshot().await.unwrap();
    let head = engine_commits(&l.repo, "engine: second commit without a file");
    let _ = s.snapshot().await.unwrap();
    // The end commit failed, or the worker may run no git in the checkout: the copy of the end is given no head.
    let end = s.final_copy(None).await.unwrap();
    assert!(end.final_copy);
    assert_eq!(
        parent_of(&data, &end.commit),
        head,
        "TRIGGER PRESENT: the copy of the end does not stand on the engine's newest commit: that commit is in no copy"
    );
    restores_on(&l, "end", &end, &head, &tree).await;
}
