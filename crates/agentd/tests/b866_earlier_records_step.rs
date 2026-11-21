//! The step that takes an earlier request's lifecycle records out of a follow-up's work tree, called by itself
//! (`agentd::direct::earlier_records::settle`) on a checkout the test made as the clone of a bundle leaves it.
//!
//! A whole run always starts from a fresh clone, which holds tracked files only and the head's content. The states in which the step must
//! leave a checkout alone (a file below the records that git does not track, a record whose file differs from the head, a checkout an
//! earlier invocation left, a checkout on another branch) cannot be reached through a run, so they are made here. Each case first shows
//! that the same seed in a clean checkout does get the commit: the trigger of the case.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::earlier_records::{self, MAX_ENTRIES, Outcome, Request, SUBJECT};
use agentd::workspace::Worktree;
use common::git;
use std::path::{Path, PathBuf};

const BRANCH: &str = "harness/01M4XTESTRUNDIRECT00000001/direct";
/// The identity the worker gives the repository (`workspace::AGENT_AUTHOR`): the engine's commits carry it.
const ENGINE: (&str, &str) = ("ham agent", "agent@harness.invalid");
const CARD: &str = "# Goal\n\n1. `python3 -m pytest -q` -> exit 0\n";

fn commit_as_engine(repo: &Path, message: &str) {
    for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", message]] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", ENGINE.0)
            .env("GIT_AUTHOR_EMAIL", ENGINE.1)
            .env("GIT_COMMITTER_NAME", ENGINE.0)
            .env("GIT_COMMITTER_EMAIL", ENGINE.1)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn write(repo: &Path, rel: &str, text: &str) {
    let path = repo.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A seed with the records an engine wrote (and `more` files under the red records), and a checkout of it as the clone of a bundle
/// leaves one: on the run's branch, at the seed. Returns the checkout, the seed and the run's private data folder.
struct Made {
    wt: Worktree,
    seed: String,
    agent_data: PathBuf,
}

fn made(label: &str, more: usize) -> Made {
    let dir = common::scratch(label);
    let source = common::python_calc(&dir);
    write(&source, ".ham/lifecycle/GOAL.md", CARD);
    write(&source, ".ham/research/01-note.md", "# Note\n");
    write(&source, ".ham/context/INDEX.md", "# Index\n");
    for i in 0..more {
        write(&source, &format!(".ham/lifecycle/red/{i:04}.txt"), "red\n");
    }
    commit_as_engine(&source, "the earlier run");
    let seed = git(&source, &["rev-parse", "HEAD"]);
    let root = dir.join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    let repo = root.join("repo");
    git(
        &dir,
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    git(&repo, &["checkout", "-q", "-B", BRANCH]);
    let agent_data = dir.join("agent-data");
    std::fs::create_dir_all(&agent_data).unwrap();
    Made {
        wt: Worktree {
            root,
            repo,
            branch: BRANCH.to_string(),
        },
        seed,
        agent_data,
    }
}

async fn settle(m: &Made, fresh: bool) -> Outcome {
    earlier_records::settle(
        &m.wt,
        &Request {
            head_sha: Some(&m.seed),
            remove: true,
            folder: ".ham",
            agent_data: &m.agent_data,
        },
        &m.seed,
        fresh,
    )
    .await
    .expect("no undo failed")
}

/// The same seed in a clean checkout gets the commit: without this, a case below would pass for any reason.
async fn trigger(label: &str, more: usize) -> String {
    let m = made(label, more);
    let out = settle(&m, true).await;
    let commit = out.commit.clone().expect("the step computes a commit");
    assert_eq!(out.settled.base, commit, "{out:?}");
    assert_eq!(git(&m.wt.repo, &["rev-parse", "HEAD"]), commit);
    assert_eq!(
        git(&m.wt.repo, &["log", "-1", "--format=%s|%P"]),
        format!("{SUBJECT}|{}", m.seed)
    );
    assert!(!m.wt.repo.join(".ham/lifecycle").exists());
    assert!(m.wt.repo.join(".ham/context/INDEX.md").is_file());
    assert_eq!(git(&m.wt.repo, &["status", "--porcelain"]), "");
    commit
}

/// Nothing was made in the checkout: the head is the seed, the card is in the work tree and the index is the head's.
fn assert_left_alone(m: &Made, out: &Outcome, why: &str) {
    assert_eq!(out.settled.base, m.seed, "the base is the origin: {out:?}");
    let note = out.settled.note.as_deref().unwrap_or_default();
    assert!(
        note.starts_with("not removed:") && note.contains(why),
        "the line for the log says why: {note}"
    );
    assert_eq!(git(&m.wt.repo, &["rev-parse", "HEAD"]), m.seed);
    assert_eq!(
        std::fs::read_to_string(m.wt.repo.join(".ham/research/01-note.md")).unwrap(),
        "# Note\n"
    );
    assert!(m.wt.repo.join(".ham/lifecycle/GOAL.md").is_file());
    assert!(
        git(
            &m.wt.repo,
            &["diff-index", "--cached", "--name-only", "HEAD", "--"]
        )
        .is_empty(),
        "the index is the head's"
    );
}

#[test]
fn b866_step_the_subject_is_the_one_the_runs_see() {
    assert_eq!(
        SUBJECT,
        "harness: earlier request's records removed from the work tree"
    );
    assert_eq!(MAX_ENTRIES, 1000);
}

#[tokio::test]
async fn b866_step_an_untracked_file_below_the_records_leaves_the_checkout_as_it_was() {
    let made_in_a_clean_checkout = trigger("b866-untracked-trigger", 0).await;
    let m = made("b866-untracked", 0);
    write(&m.wt.repo, ".ham/research/stray.md", "not tracked\n");
    let out = settle(&m, true).await;
    assert_left_alone(&m, &out, "is not a file of the head");
    assert_eq!(
        out.commit.as_deref().map(str::len),
        Some(made_in_a_clean_checkout.len()),
        "the commit the step computes for the seed is still named: {out:?}"
    );
    assert_eq!(
        std::fs::read_to_string(m.wt.repo.join(".ham/research/stray.md")).unwrap(),
        "not tracked\n"
    );
    assert_eq!(
        git(&m.wt.repo, &["status", "--porcelain"]),
        "?? .ham/research/stray.md"
    );
}

#[tokio::test]
async fn b866_step_a_record_that_differs_from_the_head_leaves_the_checkout_as_it_was() {
    trigger("b866-differs-trigger", 0).await;
    let m = made("b866-differs", 0);
    write(
        &m.wt.repo,
        ".ham/lifecycle/GOAL.md",
        "edited in the checkout\n",
    );
    let out = settle(&m, true).await;
    assert_left_alone(&m, &out, "differs from the head");
    assert_eq!(
        std::fs::read_to_string(m.wt.repo.join(".ham/lifecycle/GOAL.md")).unwrap(),
        "edited in the checkout\n",
        "the edit is not lost"
    );
}

#[tokio::test]
async fn b866_step_more_than_a_thousand_entries_are_left_alone() {
    // 998 red records beside the card and the note are 1,000 entries: handled. One more is left alone.
    trigger("b866-thousand-trigger", MAX_ENTRIES - 2).await;
    let m = made("b866-thousand", MAX_ENTRIES - 1);
    let out = settle(&m, true).await;
    assert_left_alone(&m, &out, "more than 1000 entries");
    assert_eq!(out.commit, None, "the step computes no commit: {out:?}");
    assert_eq!(git(&m.wt.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn b866_step_a_checkout_an_earlier_invocation_left_at_the_seed_gets_no_commit() {
    let computed = trigger("b866-reused-trigger", 0).await;
    let m = made("b866-reused", 0);
    let out = settle(&m, false).await;
    assert_left_alone(&m, &out, "the checkout was made by an earlier invocation");
    assert_eq!(
        out.commit.as_deref().map(str::len),
        Some(computed.len()),
        "{out:?}"
    );
    assert_eq!(git(&m.wt.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn b866_step_a_checkout_on_another_branch_is_left_alone() {
    trigger("b866-branch-trigger", 0).await;
    let m = made("b866-branch", 0);
    git(&m.wt.repo, &["checkout", "-q", "-B", "other"]);
    let out = settle(&m, true).await;
    assert_left_alone(&m, &out, "the checkout is not as the bundle left it");
}

#[tokio::test]
async fn b866_step_a_request_that_keeps_the_records_or_names_no_seed_computes_nothing() {
    let m = made("b866-keeps", 0);
    let kept = earlier_records::settle(
        &m.wt,
        &Request {
            head_sha: Some(&m.seed),
            remove: false,
            folder: ".ham",
            agent_data: &m.agent_data,
        },
        &m.seed,
        true,
    )
    .await
    .unwrap();
    assert_left_alone(&m, &kept, "removeEarlierRecords is not true");
    assert_eq!(kept.commit, None);
    let no_seed = earlier_records::settle(
        &m.wt,
        &Request {
            head_sha: None,
            remove: true,
            folder: ".ham",
            agent_data: &m.agent_data,
        },
        &m.seed,
        true,
    )
    .await
    .unwrap();
    assert_left_alone(&m, &no_seed, "names no seed commit");
    assert_eq!(no_seed.commit, None);
}

/// The step takes only the entries of the removed files out of the checkout's index. What git knows about every other tracked file (its
/// size and times) stays, so the next `git status` does not read every tracked file of the repository again.
#[tokio::test]
async fn b866_step_the_index_keeps_what_git_knows_about_the_files_that_stay() {
    let known = |repo: &Path| {
        git(
            repo,
            &["ls-files", "--debug", "--", ".ham/context/INDEX.md"],
        )
    };
    let m = made("b866-index-stat", 0);
    let before = known(&m.wt.repo);
    assert!(
        before.contains("size: 8") && !before.contains("mtime: 0:0"),
        "TRIGGER PRESENT: the clone's index knows the file that stays: {before}"
    );
    let out = settle(&m, true).await;
    assert_eq!(
        Some(out.settled.base.as_str()),
        out.commit.as_deref(),
        "TRIGGER PRESENT: the step made the commit: {out:?}"
    );
    assert_eq!(
        known(&m.wt.repo),
        before,
        "the index entry of a file that stays is as the clone left it"
    );
    assert_eq!(git(&m.wt.repo, &["status", "--porcelain"]), "");
    assert!(
        git(
            &m.wt.repo,
            &["ls-files", "--", ".ham/lifecycle", ".ham/research"]
        )
        .is_empty(),
        "the entries of the removed records are out of the index"
    );
}

/// The line for the log says how long the step took, in seconds: the cost of the step at the start of a run is a number in every log.
#[tokio::test]
async fn b866_step_the_log_line_says_how_long_the_step_took() {
    fn seconds(note: &str) -> f64 {
        let (_, tail) = note
            .rsplit_once("; the step took ")
            .unwrap_or_else(|| panic!("no seconds in: {note}"));
        let number = tail
            .strip_suffix(" s")
            .unwrap_or_else(|| panic!("no unit in: {note}"));
        assert_eq!(
            number.split_once('.').map(|(_, d)| d.len()),
            Some(2),
            "two decimals: {note}"
        );
        number.parse().unwrap()
    }
    // A step that made the commit, and a step that left the checkout alone.
    let m = made("b866-seconds", 0);
    let out = settle(&m, true).await;
    let note = out.settled.note.clone().unwrap();
    assert!(note.starts_with("removed 2 of 2 entries"), "{note}");
    let took = seconds(&note);
    assert!((0.0..600.0).contains(&took), "{note}");
    let m = made("b866-seconds-kept", 0);
    let out = settle(&m, false).await;
    let note = out.settled.note.clone().unwrap();
    assert!(note.starts_with("not removed:"), "{note}");
    assert!(seconds(&note) >= 0.0);
}

/// A seed whose records hold an executable file, a link and a note two folders deep, and a checkout of it as the clone of a bundle
/// leaves one.
fn made_with_a_link_and_a_mode(label: &str) -> Made {
    use std::os::unix::fs::PermissionsExt;
    let dir = common::scratch(label);
    let source = common::python_calc(&dir);
    write(&source, ".ham/lifecycle/GOAL.md", CARD);
    write(
        &source,
        ".ham/lifecycle/red/01-run.sh",
        "#!/bin/sh\nexit 1\n",
    );
    std::fs::set_permissions(
        source.join(".ham/lifecycle/red/01-run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("../GOAL.md", source.join(".ham/lifecycle/red/card")).unwrap();
    write(&source, ".ham/research/deep/er/01-note.md", "# Note\n");
    write(&source, ".ham/context/INDEX.md", "# Index\n");
    commit_as_engine(&source, "the earlier run");
    let seed = git(&source, &["rev-parse", "HEAD"]);
    let root = dir.join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    let repo = root.join("repo");
    git(
        &dir,
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    git(&repo, &["checkout", "-q", "-B", BRANCH]);
    let agent_data = dir.join("agent-data");
    std::fs::create_dir_all(&agent_data).unwrap();
    Made {
        wt: Worktree {
            root,
            repo,
            branch: BRANCH.to_string(),
        },
        seed,
        agent_data,
    }
}

fn walk_state(repo: &Path, rel: &str, out: &mut Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    let path = repo.join(rel);
    let meta = std::fs::symlink_metadata(&path).unwrap();
    if meta.file_type().is_symlink() {
        out.push(format!(
            "link {rel} -> {}",
            std::fs::read_link(&path).unwrap().display()
        ));
    } else if meta.is_dir() {
        out.push(format!("dir {rel}"));
        let mut names: Vec<String> = std::fs::read_dir(&path)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        for name in names {
            walk_state(repo, &format!("{rel}/{name}"), out);
        }
    } else {
        out.push(format!(
            "file {rel} {:o} {:?}",
            meta.permissions().mode() & 0o777,
            std::fs::read_to_string(&path).unwrap()
        ));
    }
}

/// Everything of the checkout the step may touch: each path below `.ham` with its kind, mode and content, the index and the head.
fn undo_state(repo: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_state(repo, ".ham", &mut out);
    out.push(format!("index {}", git(repo, &["ls-files", "-s"])));
    out.push(format!("head {}", git(repo, &["rev-parse", "HEAD"])));
    out.push(format!(
        "status {:?}",
        git(repo, &["status", "--porcelain"])
    ));
    out
}

/// The undo. The branch cannot be moved (another git holds the lock of its ref), which the step finds only at its last stage, after
/// the files left the work tree and the index was read. Everything is put back as it was: every file with its content and mode, the
/// link, the folders, the index and the head, and the line for the log says so.
#[tokio::test]
async fn b866_step_a_branch_that_cannot_be_moved_puts_the_tree_back() {
    // TRIGGER PRESENT: a seed of the same shape in a checkout whose branch can be moved gets the commit, and all four records leave
    // the tree.
    let free = made_with_a_link_and_a_mode("b866-undo-trigger");
    let out = settle(&free, true).await;
    let made_there = out.commit.clone().expect("the step computes a commit");
    assert_eq!(git(&free.wt.repo, &["rev-parse", "HEAD"]), made_there);
    assert!(!free.wt.repo.join(".ham/lifecycle").exists());
    assert!(!free.wt.repo.join(".ham/research").exists());

    let m = made_with_a_link_and_a_mode("b866-undo");
    let before = undo_state(&m.wt.repo);
    assert!(
        before
            .iter()
            .any(|l| l.starts_with("link .ham/lifecycle/red/card -> ../GOAL.md"))
            && before
                .iter()
                .any(|l| l.starts_with("file .ham/lifecycle/red/01-run.sh 755 ")),
        "TRIGGER PRESENT: a link and an executable file are among the records: {before:#?}"
    );
    // Another git holds the lock of the branch's ref: `update-ref` gives up after its short wait.
    let lock =
        m.wt.repo
            .join(".git/refs/heads")
            .join(format!("{BRANCH}.lock"));
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "").unwrap();
    let out = settle(&m, true).await;
    std::fs::remove_file(&lock).unwrap();
    let note = out.settled.note.clone().unwrap_or_default();
    assert!(
        note.starts_with("not removed: tree put back:"),
        "the line for the log says that the tree was put back: {note}"
    );
    assert_eq!(out.settled.base, m.seed, "the base is the origin: {out:?}");
    let commit = out
        .commit
        .clone()
        .expect("the commit the step computes for the seed is still named");
    assert_eq!(git(&m.wt.repo, &["rev-parse", "HEAD"]), m.seed);
    assert_eq!(
        undo_state(&m.wt.repo),
        before,
        "every file, link, mode, folder and the index are as before"
    );
    assert!(
        git(
            &m.wt.repo,
            &["diff-index", "--cached", "--name-only", "HEAD", "--"]
        )
        .is_empty(),
        "the index is the head's"
    );
    // The lock is gone: the same checkout is still one the step can be made in.
    let again = settle(&m, true).await;
    assert_eq!(again.settled.base, commit, "{again:?}");
    assert_eq!(git(&m.wt.repo, &["rev-parse", "HEAD"]), commit);
}

#[tokio::test]
async fn b866_step_the_private_index_is_gone_after_the_step() {
    let m = made("b866-index", 0);
    settle(&m, true).await;
    let left: Vec<_> = std::fs::read_dir(&m.agent_data)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "the private index is removed: {left:?}");
}
