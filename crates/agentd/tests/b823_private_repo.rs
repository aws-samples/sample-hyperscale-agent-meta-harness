//! The private repository of the copies (`<agent_data>/snapshot.git`) keeps what its last copy needs, not every version of every
//! file the session changed. An hour of copies of a tree whose large files change before each copy must not fill the worker's disk.
//! These tests run git and no engine; `b823_copies.rs` tests what a copy holds.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::snapshot::Snapshotter;
use common::{git, python_calc, scratch};
use std::path::{Path, PathBuf};

/// The copies of one hour at the 60 s timer.
const HOUR_OF_COPIES: u64 = 60;
/// The size of the file the engine writes again before each copy: bytes that do not compress, as a build output or a data file.
const CHANGED_BYTES: usize = 256 * 1024;
/// What the private repository may hold after any number of copies: the version of the changed file its last copy needs and the one before
/// it (the next look removes that one before it hashes the tree), with room for git's own small objects (trees, commits, the other files).
/// Every version it keeps beyond that adds `CHANGED_BYTES`.
const HELD_AT_MOST: u64 = 4 * CHANGED_BYTES as u64;

/// `n` bytes that do not compress, different for every `seed` (xorshift64).
fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = (seed + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(n + 8);
    while out.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(n);
    out
}

/// The bytes of every file under `dir`.
fn bytes_under(dir: &Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let t = e.file_type().unwrap();
        if t.is_dir() {
            total += bytes_under(&e.path());
        } else if t.is_file() {
            total += e.metadata().unwrap().len();
        }
    }
    total
}

/// The bytes of the private repository's object store (loose objects and packs).
fn held(data: &Path) -> u64 {
    bytes_under(&data.join("snapshot.git/objects"))
}

async fn setup(label: &str) -> (PathBuf, PathBuf, PathBuf, Snapshotter) {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let snap = Snapshotter::new(&data, &repo, &base).await.unwrap();
    std::fs::create_dir_all(repo.join("out")).unwrap();
    (dir, repo, data, snap)
}

/// The engine's work before copy `n`: the large file written again and one small file.
fn work(repo: &Path, n: u64) {
    std::fs::write(repo.join("out/data.bin"), noise(n, CHANGED_BYTES)).unwrap();
    std::fs::write(repo.join("notes.txt"), format!("round {n}\n")).unwrap();
}

/// A clone of `bundle` on its `snapshot` branch: the command the client prints (`git clone -b snapshot <file> <dir>`).
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

fn commit_all(repo: &Path, message: &str) -> String {
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            "user.name=engine",
            "-c",
            "user.email=engine@example.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
    git(repo, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn b823_an_hour_of_copies_keeps_what_the_last_copy_needs_and_no_more() {
    let (dir, repo, data, mut snap) = setup("b823-repo-hour").await;
    let mut last = None;
    for n in 0..HOUR_OF_COPIES {
        work(&repo, n);
        last = Some(snap.snapshot().await.unwrap().expect("the tree changed"));
    }
    let bytes = held(&data);
    assert!(
        bytes < HELD_AT_MOST,
        "TRIGGER PRESENT: after {HOUR_OF_COPIES} copies the private repository holds {bytes} bytes of objects; one version of the \
         changed file is {CHANGED_BYTES}"
    );
    // The copy still holds the work: the last bundle clones to the last state.
    let made = last.unwrap();
    let clone = clone_of(&dir, &made.bundle, "clone");
    assert_eq!(
        std::fs::read(clone.join("out/data.bin")).unwrap(),
        noise(HOUR_OF_COPIES - 1, CHANGED_BYTES)
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
        format!("round {}\n", HOUR_OF_COPIES - 1)
    );
    // And the next copy is made on what is left: a change of the small file only.
    std::fs::write(repo.join("notes.txt"), "one more\n").unwrap();
    let next = snap.snapshot().await.unwrap().expect("the tree changed");
    let clone = clone_of(&dir, &next.bundle, "clone-next");
    assert_eq!(
        std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
        "one more\n"
    );
    assert_eq!(
        std::fs::read(clone.join("out/data.bin")).unwrap(),
        noise(HOUR_OF_COPIES - 1, CHANGED_BYTES)
    );
    // A tree that did not change makes no copy, as before.
    assert!(snap.snapshot().await.unwrap().is_none());
}

#[tokio::test]
async fn b823_the_copy_of_the_final_head_leaves_no_older_version_behind() {
    let (dir, repo, data, mut snap) = setup("b823-repo-end-head").await;
    for n in 0..20 {
        work(&repo, n);
        snap.snapshot().await.unwrap().expect("the tree changed");
    }
    let head = commit_all(&repo, "the engine's work");
    let made = snap.final_copy(Some(&head)).await.unwrap();
    let bytes = held(&data);
    assert!(
        bytes < HELD_AT_MOST,
        "TRIGGER PRESENT: after 20 copies and the copy of the end the private repository holds {bytes} bytes of objects"
    );
    let clone = clone_of(&dir, &made.bundle, "clone");
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read(clone.join("out/data.bin")).unwrap(),
        noise(19, CHANGED_BYTES)
    );
}

#[tokio::test]
async fn b823_the_copy_of_the_end_as_the_tree_leaves_no_older_version_behind() {
    let (dir, repo, data, mut snap) = setup("b823-repo-end-tree").await;
    for n in 0..20 {
        work(&repo, n);
        snap.snapshot().await.unwrap().expect("the tree changed");
    }
    work(&repo, 20);
    let made = snap.final_copy(None).await.unwrap();
    let bytes = held(&data);
    assert!(
        bytes < HELD_AT_MOST,
        "TRIGGER PRESENT: after 21 copies the private repository holds {bytes} bytes of objects"
    );
    let clone = clone_of(&dir, &made.bundle, "clone");
    assert_eq!(
        std::fs::read(clone.join("out/data.bin")).unwrap(),
        noise(20, CHANGED_BYTES)
    );
}

#[tokio::test]
async fn b823_files_that_git_stores_in_packs_are_bounded_too() {
    let (dir, repo, data, mut snap) = setup("b823-repo-packs").await;
    // git writes a file above its big-file threshold (512 MiB by default) into a pack of its own instead of a loose object. A lower
    // threshold in the private repository's own config makes the test's file such a file.
    let config = data.join("snapshot.git/config");
    git(
        &dir,
        &[
            "config",
            "-f",
            config.to_str().unwrap(),
            "core.bigFileThreshold",
            "64k",
        ],
    );
    let mut last = None;
    for n in 0..20 {
        work(&repo, n);
        last = Some(snap.snapshot().await.unwrap().expect("the tree changed"));
    }
    assert!(
        std::fs::read_dir(data.join("snapshot.git/objects/pack"))
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".pack")),
        "TRIGGER PRESENT: git wrote the large file into a pack"
    );
    let bytes = held(&data);
    assert!(
        bytes < HELD_AT_MOST,
        "after 20 copies of a file git packs the private repository holds {bytes} bytes of objects"
    );
    let clone = clone_of(&dir, &last.unwrap().bundle, "clone");
    assert_eq!(
        std::fs::read(clone.join("out/data.bin")).unwrap(),
        noise(19, CHANGED_BYTES)
    );
}

#[tokio::test]
async fn b823_after_the_engine_removed_git_the_copies_are_bounded_too() {
    let (dir, repo, data, mut snap) = setup("b823-repo-no-git").await;
    work(&repo, 0);
    snap.snapshot().await.unwrap().expect("the tree changed");
    std::fs::rename(repo.join(".git"), dir.join("gone.git")).unwrap();
    let mut last = None;
    for n in 1..20 {
        work(&repo, n);
        last = Some(snap.snapshot().await.unwrap().expect("the tree changed"));
    }
    let bytes = held(&data);
    assert!(
        bytes < HELD_AT_MOST,
        "TRIGGER PRESENT: after 20 copies of a checkout without its .git the private repository holds {bytes} bytes of objects"
    );
    let clone = clone_of(&dir, &last.unwrap().bundle, "clone");
    assert_eq!(
        std::fs::read(clone.join("out/data.bin")).unwrap(),
        noise(19, CHANGED_BYTES)
    );
    assert!(
        clone.join("src/calc.py").is_file(),
        "the whole tree is in the copy"
    );
    let end = snap.final_copy(None).await.unwrap();
    assert!(held(&data) < HELD_AT_MOST);
    let clone = clone_of(&dir, &end.bundle, "clone-end");
    assert!(clone.join("src/calc.py").is_file());
}
