//! The copies of the work that agentd keeps in the bucket. The copy of a session's end (`Snapshotter::final_copy`) and when the copy
//! task looks at the tree (`Cadence`). These tests run git and no engine; `b823_copies_run.rs` runs whole turns.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::snapshot::{
    COPY_FLOOR, COPY_SPACING, Cadence, CopyClock, Snapshotter, WAKE_MAX_BUNDLE_BYTES, snapshot_json,
};
use common::{git, python_calc, scratch};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

async fn setup(label: &str) -> (PathBuf, PathBuf, String, Snapshotter) {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let snap = Snapshotter::new(&data, &repo, &base).await.unwrap();
    (dir, repo, base, snap)
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
async fn b823_the_final_copy_is_the_final_head_itself_not_an_older_snapshot() {
    let (dir, repo, base, mut snap) = setup("b823-final-head").await;
    // The engine works: one periodic copy, then seven more commits, as in the run that lost them.
    std::fs::write(repo.join("src/a.py"), "a = 1\n").unwrap();
    let periodic = snap.snapshot().await.unwrap().expect("the tree changed");
    assert!(!periodic.final_copy);
    for n in 0..7 {
        std::fs::write(repo.join(format!("src/review{n}.py")), format!("n = {n}\n")).unwrap();
        commit_all(&repo, &format!("review fix {n}"));
    }
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let made = snap.final_copy(Some(&head)).await.unwrap();
    assert!(made.final_copy);
    assert_eq!(made.commit, head, "the copy's commit is the final head");
    assert_eq!(made.base, base);
    assert_eq!(made.count, periodic.count + 1);

    let clone = clone_of(&dir, &made.bundle, "clone");
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&clone, &["rev-list", "--count", &format!("{base}..HEAD")]),
        "7"
    );
    assert!(clone.join("src/review6.py").is_file());
    // The record says that this is the copy of the end; a periodic record has no such member.
    let record: serde_json::Value =
        serde_json::from_str(&snapshot_json(&made, "2026-11-12T00:47:40Z")).unwrap();
    assert_eq!(record["final"], true);
    assert_eq!(record["headSha"], head.as_str());
    let periodic_record: serde_json::Value =
        serde_json::from_str(&snapshot_json(&periodic, "2026-11-12T00:40:22Z")).unwrap();
    assert!(periodic_record.get("final").is_none(), "{periodic_record}");
    // The engine's repository has no snapshot ref.
    assert!(git(&repo, &["branch", "--list", "snapshot"]).is_empty());
}

#[tokio::test]
async fn b823_a_final_copy_is_written_for_a_tree_that_never_changed_and_for_one_that_went_back() {
    let (dir, repo, base, mut snap) = setup("b823-final-same").await;
    // Nothing changed: the copy is the base, so the newest record says what the end state is.
    let made = snap.final_copy(Some(&base)).await.unwrap();
    assert!(made.final_copy);
    assert_eq!(made.commit, base);
    assert_eq!(made.count, 1);
    assert_eq!(
        git(&clone_of(&dir, &made.bundle, "c1"), &["rev-parse", "HEAD"]),
        base
    );

    // A tree that changed, was copied, and went back to its start: the final copy is the start again, not the copy in between.
    std::fs::write(repo.join("src/tmp.py"), "x = 1\n").unwrap();
    let between = snap.snapshot().await.unwrap().expect("the tree changed");
    std::fs::remove_file(repo.join("src/tmp.py")).unwrap();
    let end = snap.final_copy(Some(&base)).await.unwrap();
    assert_eq!(end.commit, base);
    assert_eq!(end.count, between.count + 1);
    let clone = clone_of(&dir, &end.bundle, "c2");
    assert!(!clone.join("src/tmp.py").exists());
}

#[tokio::test]
async fn b823_without_a_head_the_final_copy_is_the_tree_and_runs_no_git_in_the_engines_repository()
{
    let (dir, repo, base, mut snap) = setup("b823-final-tree").await;
    std::fs::write(repo.join("src/a.py"), "a = 1\n").unwrap();
    let head = commit_all(&repo, "engine: a");
    std::fs::write(repo.join("src/b.py"), "b = 1\n").unwrap();
    let periodic = snap.snapshot().await.unwrap().expect("the tree changed");
    // The engine goes on, then leaves a repository agentd may not run git in: its HEAD is no ref any more. A periodic copy then sits on
    // the last head agentd read, as the copy of the end does.
    std::fs::write(repo.join("src/c.py"), "c = 1\n").unwrap();
    std::fs::write(repo.join(".git/HEAD"), "not a ref\n").unwrap();
    let mid = snap
        .snapshot()
        .await
        .unwrap()
        .expect("TRIGGER: the tree changed");
    assert_eq!(
        git(&clone_of(&dir, &mid.bundle, "mid"), &["rev-parse", "HEAD^"]),
        head
    );
    std::fs::write(repo.join("src/c.py"), "c = 2\n").unwrap();

    let made = snap.final_copy(None).await.unwrap();
    assert!(made.final_copy);
    assert_eq!(made.count, periodic.count + 2);
    let clone = clone_of(&dir, &made.bundle, "clone");
    assert!(
        clone.join("src/c.py").is_file(),
        "the last change is in the copy"
    );
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD^"]),
        head,
        "the copy sits on the last head agentd read"
    );
    assert_eq!(
        git(&clone, &["rev-list", "--count", &format!("{base}..HEAD")]),
        "2"
    );
}

#[tokio::test]
async fn b823_lock_files_a_killed_copy_left_do_not_stop_the_final_copy() {
    let (dir, repo, _base, mut snap) = setup("b823-final-locks").await;
    std::fs::write(repo.join("src/a.py"), "a = 1\n").unwrap();
    snap.snapshot().await.unwrap().expect("the tree changed");
    std::fs::write(repo.join("src/b.py"), "b = 1\n").unwrap();
    let head = commit_all(&repo, "engine: b");
    // What `git add`, `git update-ref` and `git bundle create` leave when they are killed in the middle.
    let git_dir = dir.join("agent-data/snapshot.git");
    for lock in [
        "index.lock",
        "refs/heads/snapshot.lock",
        "snapshot.bundle.lock",
        "HEAD.lock",
    ] {
        std::fs::write(git_dir.join(lock), b"").unwrap();
    }
    let made = snap.final_copy(Some(&head)).await.unwrap();
    assert_eq!(made.commit, head);
    assert!(
        clone_of(&dir, &made.bundle, "clone")
            .join("src/b.py")
            .is_file()
    );
    // And the tree copy, which needs the index.
    std::fs::write(git_dir.join("index.lock"), b"").unwrap();
    std::fs::write(repo.join("src/c.py"), "c = 1\n").unwrap();
    let tree = snap.final_copy(None).await.unwrap();
    assert!(
        clone_of(&dir, &tree.bundle, "clone2")
            .join("src/c.py")
            .is_file()
    );
}

#[tokio::test]
async fn b823_a_copy_that_was_not_stored_is_made_again_at_the_next_look() {
    let (_dir, repo, _base, mut snap) = setup("b823-forget").await;
    std::fs::write(repo.join("src/a.py"), "a = 1\n").unwrap();
    let first = snap.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(
        snap.snapshot().await.unwrap(),
        None,
        "an unchanged tree is not copied twice"
    );
    // The upload of the first copy failed: the same tree is copied again.
    snap.forget_last();
    let again = snap
        .snapshot()
        .await
        .unwrap()
        .expect("the tree has no stored copy");
    assert_eq!(again.count, first.count + 1);
    assert_eq!(snap.snapshot().await.unwrap(), None);
}

// ---- when the copy task looks at the tree ------------------------------------------------------------------------------------------

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn b823_the_constants_of_the_cadence_are_the_ones_the_pages_state() {
    assert_eq!(harness_protocol::direct::DEFAULT_SNAPSHOT_EVERY_S, 60);
    assert_eq!(COPY_FLOOR, s(15));
    assert_eq!(COPY_SPACING, 9);
    assert_eq!(WAKE_MAX_BUNDLE_BYTES, 8 * 1024 * 1024);
}

#[test]
fn b823_a_wake_waits_for_the_floor_and_the_timer_does_not() {
    let c = Cadence::new(Some(s(60)));
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    // Nothing was looked at yet: a wake looks at once.
    assert_eq!(c.delay(&clock, true, t0), Duration::ZERO);
    // A copy started at t0 and took 0.1 s. 4 s after the start a wake waits for the rest of the floor; the timer does not wait.
    clock.looked(t0, t0 + ms(100), Some(200_000));
    assert_eq!(c.delay(&clock, true, t0 + s(4)), s(11));
    assert_eq!(c.delay(&clock, false, t0 + s(4)), Duration::ZERO);
    // 20 s after the start the floor is over.
    assert_eq!(c.delay(&clock, true, t0 + s(20)), Duration::ZERO);
}

#[test]
fn b823_copying_takes_at_most_a_tenth_of_the_time() {
    let c = Cadence::new(Some(s(60)));
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    // The last copy took 3 s: the next look starts 27 s after its end, for the timer and for a wake.
    clock.looked(t0, t0 + s(3), Some(52_000_000));
    assert_eq!(c.delay(&clock, false, t0 + s(8)), s(22));
    assert_eq!(c.delay(&clock, true, t0 + s(8)), s(22));
    assert_eq!(c.delay(&clock, false, t0 + s(30)), Duration::ZERO);
    // A copy of 20 s is followed by 180 s without one, whatever the interval says.
    clock.looked(t0, t0 + s(20), Some(900_000_000));
    assert_eq!(c.delay(&clock, false, t0 + s(80)), s(120));
    // A look that found nothing to copy is short, so it delays nothing worth counting.
    clock.looked(t0 + s(300), t0 + s(300) + ms(20), None);
    assert_eq!(c.delay(&clock, false, t0 + s(301)), Duration::ZERO);
}

#[test]
fn b823_a_tool_call_wakes_the_copy_only_while_the_bundle_is_small() {
    let c = Cadence::new(Some(s(60)));
    let t0 = Instant::now();
    let mut clock = CopyClock::default();
    assert!(c.wakes(&clock), "before the first copy");
    clock.looked(t0, t0 + ms(50), Some(WAKE_MAX_BUNDLE_BYTES));
    assert!(c.wakes(&clock));
    clock.looked(t0, t0 + ms(50), Some(WAKE_MAX_BUNDLE_BYTES + 1));
    assert!(!c.wakes(&clock), "a long history stays on the timer");
    // A look that made no copy keeps the size of the last bundle.
    clock.looked(t0 + s(60), t0 + s(60) + ms(20), None);
    assert!(!c.wakes(&clock));
    // With the timer off there is no copy while the engine works, so nothing wakes one either.
    assert!(!Cadence::new(None).wakes(&CopyClock::default()));
}

// Review 2: the engine removed or replaced `.git`. agentd then makes no end commit and no session bundle, and the copies are the only
// thing that keeps the work; the private repository can read neither the engine's commits nor the base any more.

/// The files of the clone at `clone` that a test checks, as text.
fn read(clone: &Path, file: &str) -> String {
    std::fs::read_to_string(clone.join(file)).unwrap()
}

#[tokio::test]
async fn b823_after_the_engine_removed_git_the_copy_of_the_end_holds_the_whole_tree() {
    let (dir, repo, _base, mut snap) = setup("b823-no-git-end").await;
    std::fs::write(repo.join("notes.txt"), "before\n").unwrap();
    snap.snapshot().await.unwrap().expect("the tree changed");
    let calc = read(&repo, "src/calc.py");
    // The engine moves .git away (for agentd the same as removing it) and works on.
    std::fs::rename(repo.join(".git"), dir.join("gone.git")).unwrap();
    std::fs::write(repo.join("notes.txt"), "after the move\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new work\n").unwrap();
    let made = snap
        .final_copy(None)
        .await
        .expect("TRIGGER PRESENT: the tree of a checkout without its .git was not copied");
    assert!(made.final_copy);
    let clone = clone_of(&dir, &made.bundle, "clone");
    assert_eq!(read(&clone, "notes.txt"), "after the move\n");
    assert_eq!(read(&clone, "new.txt"), "new work\n");
    assert_eq!(
        read(&clone, "src/calc.py"),
        calc,
        "a file the engine did not touch is in the copy too"
    );
}

#[tokio::test]
async fn b823_after_the_engine_removed_git_a_copy_while_it_works_holds_the_whole_tree() {
    let (dir, repo, _base, mut snap) = setup("b823-no-git-look").await;
    std::fs::write(repo.join("notes.txt"), "before\n").unwrap();
    snap.snapshot().await.unwrap().expect("the tree changed");
    std::fs::rename(repo.join(".git"), dir.join("gone.git")).unwrap();
    std::fs::write(repo.join("notes.txt"), "after the move\n").unwrap();
    let made = snap
        .snapshot()
        .await
        .expect("TRIGGER PRESENT: a look at a checkout without its .git made no copy")
        .expect("the tree changed");
    assert!(!made.final_copy);
    let clone = clone_of(&dir, &made.bundle, "clone");
    assert_eq!(read(&clone, "notes.txt"), "after the move\n");
    assert!(
        clone.join("src/calc.py").is_file(),
        "the whole tree is in the copy"
    );
    // Nothing changed since: the next look makes no copy.
    assert!(snap.snapshot().await.unwrap().is_none());
}
