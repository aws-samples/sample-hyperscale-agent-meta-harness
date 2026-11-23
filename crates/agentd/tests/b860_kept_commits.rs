//! The commits a run made are packed once into a folder of the worker's private repository (the keep), so that the looks
//! after it read them from the worker's own disk and no longer from the engine's store on the session's storage.
//!
//! Root invariant 9 binds every case here: a keep never fails, delays, thins or loses a copy. These tests drive a [`Snapshotter`]
//! with the host's git and hold what a keep leaves on the disk and what the copies beside it are:
//! - a pack is kept after the second look in a row that found the same head of the engine, once, and a head that moved replaces it;
//! - the kept pack is no part of the private repository's own object store, so the clean-up of that store never counts or removes
//!   anything because of it, and a blob that the engine pruned is still where release 5.25 keeps it;
//! - a worker that starts again finds its pack and clears what a keep that was cut left behind, and nothing that is not its own;
//! - a copy made beside a kept pack is the copy the release makes: the same ref, tree, parents and history, restored in a repository
//!   that holds only the bundle the run started from, and no larger;
//! - every reader restores such a copy, and the copy of the end builds no pack.
//!
//! Every look that is meant to make a copy is given a changed tree, and every test first shows that a pack was kept: a test of the
//! copies beside a kept pack that could pass without a keep is no test. The tests that need to see or steer the git processes are
//! in `b860_kept_commits_git.rs`; the copy task around the keep is tested with whole turns in `b860_kept_commits_run.rs`.
#![allow(clippy::unwrap_used)]

mod b860_kept_support;
mod b860_support;
mod b860_thin_support;
mod common;
mod direct_support;

use agentd::direct::snapshot::{
    GiveUp, Keep, SnapshotMade, Snapshotter, TIDY_FREE_BYTES, TidyMarks, hash_bundle,
};
use b860_kept_support::{
    Seen, alive, changed_look, command_of, edit, engine_commits, fetched_into_the_base,
    fix_the_dates, incoming_folder, init_seen, keeps_started, kept_folder, kept_markers, kept_seen,
    limit, names, objects_of, private, second_process, short_disk, work_of,
};
use b860_support::hash_of;
use b860_thin_support::{Lab, lab, needed, noise, restored, thin, thin_on, thin_within};
use common::{git, python_calc, scratch};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// Whether the repository at `git_dir` holds the object `id` in its own store or behind its alternates.
fn holds(git_dir: &Path, id: &str) -> bool {
    std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "-e", id])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// The lines of the commit `commit` in the private repository `git_dir` without the two that carry a time: its tree, its parents
/// and its message.
fn commit_without_times(git_dir: &Path, commit: &str) -> Vec<String> {
    git(
        git_dir,
        &[
            "--git-dir",
            git_dir.to_str().unwrap(),
            "cat-file",
            "-p",
            commit,
        ],
    )
    .lines()
    .filter(|l| !l.starts_with("author ") && !l.starts_with("committer "))
    .map(str::to_string)
    .collect()
}

/// The whole text of the commit `commit` in the private repository `git_dir`.
fn commit_text(git_dir: &Path, commit: &str) -> String {
    git(
        git_dir,
        &[
            "--git-dir",
            git_dir.to_str().unwrap(),
            "cat-file",
            "-p",
            commit,
        ],
    )
}

/// A copy made beside a kept pack (`kept`) is the copy the release makes for the same state (`release`, made in a private
/// repository that never held a kept pack): the same ref, the same tree, parents and message, and the same commit; the same
/// commits it needs; the same files and the same history in a repository that holds only the bundle the run started from; and no
/// more bytes.
///
/// The two copies are one commit because the test that calls this fixed the dates of the worker's commits
/// ([`fix_the_dates`]). A copy's own commit is otherwise stamped with the second of the clock, the bytes of a bundle change with
/// the bytes of that commit, and two copies of one state made in two seconds differed by a byte in one comparison of sixty: the
/// sizes are compared only for copies that are the same commit, so that no comparison follows the clock.
fn assert_the_releases_copy(
    l: &Lab,
    what: &str,
    kept: (&Path, &SnapshotMade),
    release: (&Path, &SnapshotMade),
) {
    let (kept_git, kept_made) = kept;
    let (release_git, release_made) = release;
    assert_eq!(
        commit_without_times(kept_git, &kept_made.commit),
        commit_without_times(release_git, &release_made.commit),
        "{what}: the tree, the parents and the message of the copy's commit"
    );
    assert_eq!(
        commit_text(kept_git, &kept_made.commit),
        commit_text(release_git, &release_made.commit),
        "TRIGGER PRESENT: {what}: the two copies that are compared are not one commit: their dates follow the clock"
    );
    assert_eq!(kept_made.commit, release_made.commit, "{what}: the commit");
    assert_eq!(
        needed(&kept_made.bundle),
        needed(&release_made.bundle),
        "{what}: the commits the copy needs"
    );
    let mut fetched = Vec::new();
    for (arm, made) in [("kept", kept_made), ("release", release_made)] {
        let fresh = fetched_into_the_base(l, &format!("{what}-{arm}"), &made.bundle);
        assert_eq!(
            git(&fresh, &["rev-parse", "refs/heads/snapshot"]),
            made.commit,
            "{what}, {arm}: the ref of the copy"
        );
        fetched.push((
            git(&fresh, &["ls-tree", "-r", "refs/heads/snapshot"]),
            git(&fresh, &["rev-list", "refs/heads/snapshot^1"]),
        ));
    }
    assert_eq!(fetched[0].0, fetched[1].0, "{what}: the files of the copy");
    assert_eq!(
        fetched[0].1, fetched[1].1,
        "{what}: the history below the copy"
    );
    assert!(
        kept_made.bundle_bytes <= release_made.bundle_bytes,
        "TRIGGER PRESENT: {what}: the copy beside the kept pack has {} bytes, the release's copy {}",
        kept_made.bundle_bytes,
        release_made.bundle_bytes
    );
}

// ---- when a pack is kept ------------------------------------------------------------------------------------------------------------

/// The first look of a head keeps nothing; the second look in a row that finds the same head keeps one pack whose marker names
/// that head and which holds the run's commits; the looks after it keep nothing more, and a look at an unchanged tree keeps nothing.
#[tokio::test]
async fn b860_kept_the_runs_commits_are_packed_once_at_rest() {
    let l = lab("b860-kept-once");
    // An engine's store without a pack folder holds no pack: the keep reads that as no pack.
    let _ = std::fs::remove_dir(l.repo.join(".git/objects/pack"));
    let mut copies = thin(&l, "data").await;
    let head = engine_commits(&l.repo, 0, 40);
    let git_dir = private(&l, "data");

    changed_look(&mut copies, &l.repo, 1).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "the first look of a head keeps nothing"
    );
    assert_eq!(
        kept_markers(&git_dir),
        vec![],
        "no pack before the head was found twice"
    );

    changed_look(&mut copies, &l.repo, 2).await;
    let kept = copies.keep_commits(&limit()).await;
    let markers = kept_markers(&git_dir);
    assert!(
        matches!(kept, Keep::Kept { bytes } if bytes > 0),
        "TRIGGER PRESENT: no kept pack after the second look of the same head: {kept:?}; the private repository holds {} kept \
         packs",
        markers.len()
    );
    assert_eq!(markers.len(), 1, "one kept pack: {markers:?}");
    assert_eq!(markers[0].1, head, "the marker names the engine's head");
    let pack = markers[0].0.with_extension("pack");
    assert!(pack.is_file() && markers[0].0.with_extension("idx").is_file());
    assert_eq!(copies.kept_bytes(), std::fs::metadata(&pack).unwrap().len());
    let commits = objects_of(&l.repo, &markers[0].0)
        .iter()
        .filter(|(_, kind)| kind == "commit")
        .count();
    assert_eq!(
        commits, 40,
        "the pack holds the 40 commits of the run and none of the base"
    );
    assert_eq!(names(&incoming_folder(&l, "data")), Vec::<String>::new());

    let marker_written = std::fs::metadata(&markers[0].0)
        .unwrap()
        .modified()
        .unwrap();
    for round in 3..=5 {
        changed_look(&mut copies, &l.repo, round).await;
        assert_eq!(
            copies.keep_commits(&limit()).await,
            Keep::NotDue,
            "look {round} of the same head packs nothing again"
        );
    }
    assert!(
        copies.snapshot_until(&limit()).await.unwrap().is_none(),
        "an unchanged tree"
    );
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    let after = kept_markers(&git_dir);
    assert_eq!(after, markers, "the same one pack");
    assert_eq!(
        std::fs::metadata(&after[0].0).unwrap().modified().unwrap(),
        marker_written,
        "the marker was not written again"
    );

    // The engine removes its `.git`: the look that finds it gone removes the kept pack, and its copy holds the whole tree.
    std::fs::rename(
        l.repo.join(".git"),
        l.dir.join("the-engines-git-moved-away"),
    )
    .unwrap();
    let made = changed_look(&mut copies, &l.repo, 6).await;
    assert_eq!(
        kept_markers(&git_dir),
        vec![],
        "the pack of an engine whose .git is gone is removed"
    );
    assert_eq!(copies.kept_bytes(), 0);
    assert!(needed(&made.bundle).is_empty(), "a copy of the whole tree");
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
}

/// A head at a commit the run started from is followed by no keep: an engine that has made no commit has nothing above the base,
/// and the copy task takes no step of a keep for it, however many looks find that head.
#[tokio::test]
async fn b860_kept_a_head_at_the_base_is_followed_by_no_keep() {
    let l = lab("b860-kept-base");
    let mut copies = thin(&l, "data").await;
    for round in 1..=3 {
        changed_look(&mut copies, &l.repo, round).await;
        assert!(
            !copies.keep_is_due(),
            "TRIGGER PRESENT: a keep is due after look {round} of an engine that made no commit"
        );
        assert_eq!(
            copies.keep_commits(&limit()).await,
            Keep::NotDue,
            "look {round}"
        );
    }
    assert_eq!(
        names(&private(&l, "data").join("kept")),
        Vec::<String>::new(),
        "no folder of a kept pack is made"
    );
}

/// A repository whose run starts from a bundle of two branches, `main` and `side`: a copy above the base leaves out what either
/// reaches. The answer names the head of `side`.
fn lab_of_two_branches(label: &str) -> (Lab, String) {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    git(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("side.txt"), "the file of the side branch\n").unwrap();
    git(&repo, &["add", "side.txt"]);
    git(&repo, &["commit", "-q", "-m", "side"]);
    let side = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("main.txt"), "the file of the main branch\n").unwrap();
    git(&repo, &["add", "main.txt"]);
    git(&repo, &["commit", "-q", "-m", "main"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let base_bundle = dir.join("repo.bundle");
    git(
        &repo,
        &[
            "bundle",
            "create",
            "-q",
            base_bundle.to_str().unwrap(),
            "HEAD",
            "refs/heads/main",
            "refs/heads/side",
        ],
    );
    (
        Lab {
            dir,
            repo,
            base_bundle,
            base,
        },
        side,
    )
}

/// One kept pack at a time: a head that moved and came to rest replaces the pack, and a head back at a commit the run started
/// from leaves none. With several floors the pack holds what the run made above all of them, and every copy on the way restores.
#[tokio::test]
async fn b860_kept_a_head_that_moved_replaces_the_pack() {
    let (l, side) = lab_of_two_branches("b860-kept-moved");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 5);
    git(&l.repo, &["merge", "-q", "--no-edit", "side"]);
    let first = git(&l.repo, &["rev-parse", "HEAD"]);

    changed_look(&mut copies, &l.repo, 1).await;
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    changed_look(&mut copies, &l.repo, 2).await;
    let kept = copies.keep_commits(&limit()).await;
    assert!(
        matches!(kept, Keep::Kept { .. }),
        "TRIGGER PRESENT: no kept pack for the first head: {kept:?}"
    );
    let markers = kept_markers(&git_dir);
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].1, first);
    let held: Vec<String> = objects_of(&l.repo, &markers[0].0)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        held.contains(&first),
        "the pack holds the merge the run made"
    );
    let side_blob = git(&l.repo, &["rev-parse", &format!("{side}:side.txt")]);
    assert!(
        !held.contains(&side) && !held.contains(&side_blob) && !held.contains(&l.base),
        "the pack holds nothing that a commit the run started from reaches"
    );

    // The head moves: the look that finds it keeps nothing, and the pack of the head before stays until the new one stands.
    let second = engine_commits(&l.repo, 5, 3);
    let made = changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a moved head"
    );
    assert_eq!(
        kept_markers(&git_dir),
        markers,
        "the pack of the head before stays"
    );
    restored(&l, "moved", &made.bundle, &made.commit).await;
    let made = changed_look(&mut copies, &l.repo, 4).await;
    restored(&l, "at-rest-again", &made.bundle, &made.commit).await;
    assert!(matches!(
        copies.keep_commits(&limit()).await,
        Keep::Kept { .. }
    ));
    let markers = kept_markers(&git_dir);
    assert_eq!(
        markers.len(),
        1,
        "TRIGGER PRESENT: {} kept packs after the head moved and came to rest: {markers:?}",
        markers.len()
    );
    assert_eq!(markers[0].1, second);
    assert_eq!(
        names(&kept_folder(&l, "data"))
            .iter()
            .filter(|n| n.ends_with(".pack"))
            .count(),
        1,
        "the pack kept before is gone"
    );
    let made = changed_look(&mut copies, &l.repo, 5).await;
    let wt = restored(&l, "after-replace", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(5)
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("side.txt")).unwrap(),
        "the file of the side branch\n"
    );

    // The engine goes back to the commit the run started from: nothing of the run is above the floors any more.
    git(&l.repo, &["reset", "-q", "--hard", &l.base]);
    let made = changed_look(&mut copies, &l.repo, 6).await;
    assert_eq!(
        kept_markers(&git_dir),
        vec![],
        "TRIGGER PRESENT: the look that found the head at a commit the run started from left the pack of the head before"
    );
    assert_eq!(copies.kept_bytes(), 0);
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a moved head"
    );
    restored(&l, "reset", &made.bundle, &made.commit).await;
    let made = changed_look(&mut copies, &l.repo, 7).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a head at a commit the run started from is followed by no keep"
    );
    assert_eq!(kept_markers(&git_dir), vec![]);
    let wt = restored(&l, "at-the-floor", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(7)
    );
}

// ---- the kept pack is no part of the private repository's own store ---------------------------------------------------------------

/// A repository whose base tracks a file that an ignore rule of the engine matches once the session runs: only the index the
/// worker keeps still tracks it, so a copy that was read into a new index (the whole tree) lacks it.
fn lab_with_a_generated_file(label: &str) -> Lab {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    std::fs::create_dir_all(repo.join("gen")).unwrap();
    std::fs::write(
        repo.join("gen/keep.txt"),
        "generated, and tracked at the base\n",
    )
    .unwrap();
    git(&repo, &["add", "gen"]);
    git(&repo, &["commit", "-q", "-m", "base with a generated file"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let base_bundle = dir.join("repo.bundle");
    b860_thin_support::stage_base(&repo, &base_bundle);
    Lab {
        dir,
        repo,
        base_bundle,
        base,
    }
}

/// Ruling 8 of the lead: the pack kept before must not be what holds a blob of the tree. A look writes an uncommitted file's blob
/// loose into the private repository; the engine commits the file and the keep packs the blob; the private clean-up runs; the
/// engine rewrites its history onto another commit, leaves the file in the work tree and prunes the old history from its store; a
/// new keep replaces the old one. Release 5.25 still holds the blob loose in the private repository then, because nothing of that
/// repository ever held it in a pack. The looks that follow, one of them after another file changed, must write the tree of the
/// index the worker keeps, as the release does: a look that finds the blob in no store reads the whole tree again into a new
/// index, and its copy lacks the files that only the kept index tracks.
#[tokio::test]
async fn b860_kept_a_blob_the_engine_pruned_is_still_where_the_release_keeps_it() {
    let l = lab_with_a_generated_file("b860-kept-evict");
    let mut copies = thin(&l, "data").await;
    copies.set_tidy_marks(TidyMarks::AT_ONCE);
    let git_dir = private(&l, "data");
    std::fs::write(l.repo.join(".git/info/exclude"), "gen/\n").unwrap();

    // A look writes the blob of an uncommitted file loose into the private repository.
    let notes = "notes that are first in no commit, then in one, then in none again\n";
    std::fs::write(l.repo.join("notes.txt"), notes).unwrap();
    // The file is an hour older than the index that will name it, as a file of a real run is older than the look after it. git
    // then trusts what the index says of the file. A file written in the same second as the index is read again at every look,
    // and its blob is written again when no store holds it, which hides the loss this test is about.
    std::fs::File::options()
        .write(true)
        .open(l.repo.join("notes.txt"))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();
    let blob = git(&l.repo, &["hash-object", "notes.txt"]);
    let first = changed_look(&mut copies, &l.repo, 1).await;
    assert!(holds(&git_dir, &blob));
    assert!(
        !holds(&l.repo.join(".git"), &blob),
        "the engine's store does not hold it yet"
    );

    // The engine commits the file; the second look of that head keeps the run's commits, the blob among them.
    git(&l.repo, &["add", "notes.txt"]);
    git(&l.repo, &["commit", "-q", "-m", "notes"]);
    changed_look(&mut copies, &l.repo, 2).await;
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    changed_look(&mut copies, &l.repo, 3).await;
    let kept = copies.keep_commits(&limit()).await;
    assert!(
        matches!(kept, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {kept:?}"
    );
    let markers = kept_markers(&git_dir);
    assert!(
        objects_of(&l.repo, &markers[0].0)
            .iter()
            .any(|(id, _)| *id == blob),
        "TRIGGER PRESENT is not shown: the kept pack does not hold the blob"
    );

    // The private clean-up runs at the start of the next look.
    changed_look(&mut copies, &l.repo, 4).await;
    assert!(
        !holds(&git_dir, &first.commit),
        "TRIGGER PRESENT is not shown: the clean-up of the private repository did not run"
    );

    // The engine rewrites its history onto another commit, leaves the file in the work tree and prunes the old history.
    git(&l.repo, &["reset", "-q", &l.base]);
    std::fs::write(l.repo.join("other.txt"), "the engine's other work\n").unwrap();
    git(&l.repo, &["add", "other.txt"]);
    git(&l.repo, &["commit", "-q", "-m", "rewritten"]);
    git(&l.repo, &["reflog", "expire", "--expire=now", "--all"]);
    git(&l.repo, &["gc", "-q", "--prune=now"]);
    assert!(
        !holds(&l.repo.join(".git"), &blob),
        "TRIGGER PRESENT is not shown: the engine's store still holds the blob"
    );
    assert_eq!(
        std::fs::read_to_string(l.repo.join("notes.txt")).unwrap(),
        notes
    );

    // The new head comes to rest and a new keep replaces the old one.
    changed_look(&mut copies, &l.repo, 5).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a moved head"
    );
    changed_look(&mut copies, &l.repo, 6).await;
    let again = copies.keep_commits(&limit()).await;
    assert!(
        matches!(again, Keep::Kept { .. } | Keep::Nothing),
        "the keep of the new head: {again:?}"
    );
    assert!(
        kept_markers(&git_dir)
            .iter()
            .all(|(_, head)| *head != markers[0].1),
        "TRIGGER PRESENT is not shown: the pack of the head before is still kept"
    );

    // The looks that follow, after another file changed, write the tree of the index the worker keeps.
    for round in 7..=8 {
        let made = changed_look(&mut copies, &l.repo, round).await;
        assert!(
            !git_dir.join("index-whole").exists(),
            "TRIGGER PRESENT: look {round} could not write the tree of the index the worker keeps and read the whole tree again \
             into a new index: a blob that the index names is in no store"
        );
        let wt = restored(
            &l,
            &format!("after-the-prune-{round}"),
            &made.bundle,
            &made.commit,
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(wt.join("notes.txt")).unwrap(),
            notes
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("work.txt")).unwrap(),
            work_of(round)
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("gen/keep.txt")).unwrap(),
            "generated, and tracked at the base\n",
            "TRIGGER PRESENT: the copy of look {round} lacks the file that only the kept index tracks"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("other.txt")).unwrap(),
            "the engine's other work\n"
        );
    }
}

/// The clean-up of the private repository does not see the kept pack: with the marks of a clean-up at every look, `prune` and
/// `repack` run beside it and it stays, the repository counts no pack of its own (so the kept pack is no part of the 64 MiB mark),
/// and the copies made meanwhile restore.
#[tokio::test]
async fn b860_kept_the_clean_up_does_not_see_the_kept_pack() {
    let l = lab("b860-kept-tidy");
    let mut copies = thin(&l, "data").await;
    copies.set_tidy_marks(TidyMarks::AT_ONCE);
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 20);
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let kept = copies.keep_commits(&limit()).await;
    assert!(
        matches!(kept, Keep::Kept { .. }),
        "TRIGGER PRESENT: no kept pack: {kept:?}"
    );
    let markers = kept_markers(&git_dir);
    let files = names(&kept_folder(&l, "data"));
    assert!(
        !files.is_empty(),
        "the kept pack lies in its own folder of the private repository"
    );
    for round in 3..=6 {
        let made = changed_look(&mut copies, &l.repo, round).await;
        assert_eq!(
            copies.take_tidy_error(),
            None,
            "the clean-up before look {round}"
        );
        restored(&l, &format!("tidy-{round}"), &made.bundle, &made.commit).await;
    }
    assert_eq!(
        kept_markers(&git_dir),
        markers,
        "the kept pack stays through prune and repack"
    );
    assert_eq!(names(&kept_folder(&l, "data")), files);
    let counted = git(
        &l.repo,
        &[
            "--git-dir",
            git_dir.to_str().unwrap(),
            "count-objects",
            "-v",
        ],
    );
    assert!(
        counted.lines().any(|line| line == "size-pack: 0")
            && counted.lines().any(|line| line == "packs: 0"),
        "TRIGGER PRESENT: the clean-up counts the kept pack as a pack of the private repository:\n{counted}"
    );
}

// ---- a worker that starts again ---------------------------------------------------------------------------------------------------

/// Copies the files of the pack beside `marker` under the name `pack-<id>`, with `extensions`.
fn copy_of_the_pack(marker: &Path, id: &str, extensions: &[&str]) {
    for ext in extensions {
        let from = marker.with_extension(ext);
        if from.exists() {
            std::fs::copy(&from, marker.with_file_name(format!("pack-{id}.{ext}"))).unwrap();
        }
    }
}

/// Ruling 12 of the lead: what a worker finds when it opens a private repository that an earlier worker left. Its own pack with
/// its marker is found, and the next look at rest packs nothing again. What a keep that was cut left is cleared: a file in the
/// folder a keep writes into (the keep was cut before its marker stood), a pack with a marker of the worker but without its index
/// (cut after the marker stood), and a second pack with an older marker of the worker (cut while the pack before was replaced).
/// What does not carry the worker's own line is not the worker's to remove: a pack with another marker and a pack with none stay.
#[tokio::test]
async fn b860_kept_a_worker_that_starts_again_finds_its_pack() {
    let l = lab("b860-kept-again");
    let git_dir = private(&l, "data");
    let head = engine_commits(&l.repo, 0, 10);
    {
        let mut copies = thin(&l, "data").await;
        changed_look(&mut copies, &l.repo, 1).await;
        changed_look(&mut copies, &l.repo, 2).await;
        let kept = copies.keep_commits(&limit()).await;
        assert!(
            matches!(kept, Keep::Kept { .. }),
            "TRIGGER PRESENT: no kept pack: {kept:?}"
        );
    }
    let markers = kept_markers(&git_dir);
    assert_eq!(markers.len(), 1);
    let marker = markers[0].0.clone();
    let folder = kept_folder(&l, "data");
    assert_eq!(
        marker.parent().unwrap(),
        folder,
        "the kept pack lies in its own folder"
    );
    let own = names(&folder);

    // A keep that was cut before its marker stood: its files are still in the folder it writes into.
    let incoming = incoming_folder(&l, "data");
    std::fs::create_dir_all(&incoming).unwrap();
    std::fs::write(incoming.join("tmp_pack_Ab12Cd"), b"half a pack").unwrap();
    std::fs::write(
        incoming.join(format!("pack-{}.pack", "a".repeat(40))),
        b"a pack",
    )
    .unwrap();
    // A keep that was cut after its marker stood and before its pack was whole: a marker of the worker, a pack, no index.
    let cut = "b".repeat(40);
    copy_of_the_pack(&marker, &cut, &["keep", "pack"]);
    // A keep that was cut while the pack before was replaced: a second whole pack with an older marker of the worker.
    let older = "c".repeat(40);
    copy_of_the_pack(&marker, &older, &["keep", "pack", "idx", "rev"]);
    let older_marker = folder.join(format!("pack-{older}.keep"));
    std::fs::write(&older_marker, format!("agentd kept {}\n", "1".repeat(40))).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&older_marker)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();
    // Not the worker's: a pack with a marker that does not carry the worker's line, and a pack with no marker.
    let foreign = "d".repeat(40);
    copy_of_the_pack(&marker, &foreign, &["pack", "idx"]);
    std::fs::write(
        folder.join(format!("pack-{foreign}.keep")),
        "kept by someone else\n",
    )
    .unwrap();
    let bare = "e".repeat(40);
    copy_of_the_pack(&marker, &bare, &["pack", "idx"]);

    let mut copies = thin(&l, "data").await;
    assert_eq!(
        names(&incoming),
        Vec::<String>::new(),
        "what a cut keep left in its folder is cleared"
    );
    let mut left = own.clone();
    left.extend([
        format!("pack-{foreign}.idx"),
        format!("pack-{foreign}.keep"),
        format!("pack-{foreign}.pack"),
        format!("pack-{bare}.idx"),
        format!("pack-{bare}.pack"),
    ]);
    left.sort();
    assert_eq!(
        names(&folder),
        left,
        "the worker's own newest pack stays, its cut and its older pack go, and nothing else is touched"
    );
    assert_eq!(
        copies.kept_bytes(),
        std::fs::metadata(marker.with_extension("pack"))
            .unwrap()
            .len(),
        "the worker found its pack"
    );
    let written = std::fs::metadata(&marker).unwrap().modified().unwrap();

    // The head is where the pack was kept for: the looks of the new worker pack nothing again, and their copies restore.
    let made = changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    restored(&l, "again-1", &made.bundle, &made.commit).await;
    let made = changed_look(&mut copies, &l.repo, 4).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a pack is kept for this head: the second look in a row packs nothing again"
    );
    restored(&l, "again-2", &made.bundle, &made.commit).await;
    assert_eq!(
        kept_markers(&git_dir)
            .iter()
            .filter(|(_, h)| *h == head)
            .count(),
        1
    );
    assert_eq!(
        std::fs::metadata(&marker).unwrap().modified().unwrap(),
        written
    );
}

/// Finding 4 of the review of this branch: a worker adopts the pack an earlier worker kept only after git's own check of the pack
/// has passed (`git index-pack --verify`, which reads every object of the pack). git copies packed bytes into a bundle without
/// reading them, so a pack with wrong bytes gave a stored copy that no reader could fetch. Here 48 bytes in the middle of the pack
/// are wrong when the worker starts again: the pack is removed, and the worker says so once; the copy of the next look is the
/// copy the release makes, it can be fetched into a repository that holds only the run's base, and it restores; the head is kept
/// again at its next rest, and a worker that starts on that pack adopts it.
#[tokio::test]
async fn b860_kept_a_pack_with_wrong_bytes_is_not_adopted() {
    let Some(dir) = second_process("b860_kept_a_pack_with_wrong_bytes_is_not_adopted") else {
        return;
    };
    fix_the_dates(&dir);
    let l = lab("b860-kept-damaged");
    let head = engine_commits(&l.repo, 0, 30);
    let (kept_git, release_git) = (private(&l, "data-kept"), private(&l, "data-release"));
    let mut release = thin(&l, "data-release").await;
    release.set_keep_commits(false);
    {
        let mut earlier = thin(&l, "data-kept").await;
        for round in 1..=2 {
            changed_look(&mut earlier, &l.repo, round).await;
            b860_kept_support::look(&mut release).await;
        }
        let kept = earlier.keep_commits(&limit()).await;
        assert!(
            matches!(kept, Keep::Kept { .. }),
            "TRIGGER PRESENT is not shown: no kept pack: {kept:?}"
        );
    }
    let markers = kept_markers(&kept_git);
    assert_eq!(markers.len(), 1);
    let pack = markers[0].0.with_extension("pack");
    let mut bytes = std::fs::read(&pack).unwrap();
    assert!(
        bytes.len() > 200,
        "the pack has a middle: {} bytes",
        bytes.len()
    );
    let middle = bytes.len() / 2;
    for byte in &mut bytes[middle - 24..middle + 24] {
        *byte = !*byte;
    }
    // The file of a pack cannot be written to: the wrong bytes take its place under its name.
    let wrong = pack.with_extension("wrong");
    std::fs::write(&wrong, &bytes).unwrap();
    std::fs::rename(&wrong, &pack).unwrap();

    let mut later = thin(&l, "data-kept").await;
    // The copies are numbered within the run: the new worker goes on where the earlier one stood.
    later.count_from(2);
    assert_eq!(
        (
            kept_markers(&kept_git),
            names(&kept_folder(&l, "data-kept")),
            later.kept_bytes()
        ),
        (vec![], Vec::<String>::new(), 0),
        "TRIGGER PRESENT: the worker adopted a kept pack with wrong bytes"
    );
    let said = later.take_kept_note();
    assert!(
        said.as_deref()
            .is_some_and(|why| why.contains("git's check")),
        "the worker says why the pack is gone: {said:?}"
    );
    assert_eq!(later.take_kept_note(), None, "it is said once");

    let a = changed_look(&mut later, &l.repo, 3).await;
    let b = b860_kept_support::look(&mut release).await;
    assert_the_releases_copy(
        &l,
        "after-the-wrong-bytes",
        (&kept_git, &a),
        (&release_git, &b),
    );
    let wt = restored(&l, "after-the-wrong-bytes", &a.bundle, &a.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );

    // The head is kept again at its next rest, and a worker that starts on that pack adopts it: git's check passes.
    assert_eq!(
        later.keep_commits(&limit()).await,
        Keep::NotDue,
        "the first look of the new worker"
    );
    changed_look(&mut later, &l.repo, 4).await;
    let keep = later.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "the head is kept again: {keep:?}"
    );
    drop(later);
    let mut again = thin(&l, "data-kept").await;
    assert_eq!(
        again.take_kept_note(),
        None,
        "a whole pack passes git's check"
    );
    assert!(again.kept_bytes() > 0, "the worker adopted its whole pack");
    assert_eq!(
        kept_markers(&kept_git)
            .iter()
            .map(|(_, h)| h.clone())
            .collect::<Vec<_>>(),
        vec![head]
    );
}

/// Copies every file of the folder of a kept pack into `to`; the answer is the name that the files of the one pack there share.
fn saved_pack(folder: &Path, to: &Path) -> String {
    let mut name = None;
    for file in names(folder) {
        std::fs::copy(folder.join(&file), to.join(&file)).unwrap();
        if let Some(pack) = file.strip_suffix(".pack") {
            name = Some(pack.to_string());
        }
    }
    name.expect("one pack in the folder of the kept pack")
}

/// Ruling 14 of the lead, the cut between the renames that install a kept pack. A keep renames the marker, the pack, its reverse
/// index and last its index into the folder of the kept pack, with no wait between them, and then removes the pack kept before:
/// only a worker that is killed can stop between two of them. What such a worker leaves is planted here from the files of two
/// real keeps, for each place it can stop, and the worker that starts on it clears it: the files of the pack that is not whole
/// are gone from the folder of the kept pack and from the folder the keep wrote into, the pack kept before is the worker's kept
/// pack again, the copy of the next look restores, and the head is kept at its next rest. Killed after the last rename and
/// before the pack kept before was removed, the worker finds two whole packs: the newer one is its kept pack, and the older goes.
#[tokio::test]
async fn b860_kept_a_keep_cut_between_its_renames_is_cleared_at_the_start() {
    let l = lab("b860-kept-renames");
    let git_dir = private(&l, "data");
    let folder = kept_folder(&l, "data");
    let incoming = incoming_folder(&l, "data");
    let saved = l.dir.join("saved-packs");
    std::fs::create_dir_all(&saved).unwrap();

    // Two real keeps: the pack of the first head, then the pack of the second head, which replaces it.
    let first = engine_commits(&l.repo, 0, 10);
    let mut round = 0;
    let (before, second, after) = {
        let mut copies = thin(&l, "data").await;
        for _ in 0..2 {
            round += 1;
            changed_look(&mut copies, &l.repo, round).await;
        }
        let kept = copies.keep_commits(&limit()).await;
        assert!(
            matches!(kept, Keep::Kept { .. }),
            "TRIGGER PRESENT is not shown: no kept pack for the first head: {kept:?}"
        );
        let before = saved_pack(&folder, &saved);
        let second = engine_commits(&l.repo, 10, 5);
        for _ in 0..2 {
            round += 1;
            changed_look(&mut copies, &l.repo, round).await;
        }
        let kept = copies.keep_commits(&limit()).await;
        assert!(
            matches!(kept, Keep::Kept { .. }),
            "TRIGGER PRESENT is not shown: no kept pack for the second head: {kept:?}"
        );
        (before, second, saved_pack(&folder, &saved))
    };
    assert_ne!(before, after, "two packs");
    let files_of = |name: &str| -> Vec<String> {
        ["idx", "keep", "pack", "rev"]
            .iter()
            .map(|ext| format!("{name}.{ext}"))
            .filter(|file| saved.join(file).exists())
            .collect()
    };

    // The order of the renames: the marker, the pack, its reverse index, its index. `renamed` of them were made before the cut.
    let order = ["keep", "pack", "rev", "idx"];
    for renamed in 1..=4_usize {
        for dir in [&folder, &incoming] {
            std::fs::create_dir_all(dir).unwrap();
            for name in names(dir) {
                std::fs::remove_file(dir.join(name)).unwrap();
            }
        }
        for file in files_of(&before) {
            std::fs::copy(saved.join(&file), folder.join(&file)).unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(folder.join(format!("{before}.keep")))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3_600))
            .unwrap();
        for (at, ext) in order.iter().enumerate() {
            let file = format!("{after}.{ext}");
            if saved.join(&file).exists() {
                let to = if at < renamed { &folder } else { &incoming };
                std::fs::copy(saved.join(&file), to.join(&file)).unwrap();
            }
        }

        let mut copies = thin(&l, "data").await;
        copies.count_from(round);
        assert_eq!(
            names(&incoming),
            Vec::<String>::new(),
            "cut after {renamed} renames: the folder the keep wrote into is cleared"
        );
        let (whole, head) = if renamed == 4 {
            (&after, &second)
        } else {
            (&before, &first)
        };
        assert_eq!(
            names(&folder),
            files_of(whole),
            "cut after {renamed} renames: the one whole pack with the worker's newest marker stays, and nothing of the other"
        );
        assert_eq!(
            kept_markers(&git_dir)
                .iter()
                .map(|(_, h)| h.clone())
                .collect::<Vec<_>>(),
            vec![head.clone()],
            "cut after {renamed} renames"
        );
        assert_eq!(
            copies.kept_bytes(),
            std::fs::metadata(folder.join(format!("{whole}.pack")))
                .unwrap()
                .len(),
            "cut after {renamed} renames: the worker's kept pack"
        );

        round += 1;
        let made = changed_look(&mut copies, &l.repo, round).await;
        let wt = restored(
            &l,
            &format!("renames-{renamed}"),
            &made.bundle,
            &made.commit,
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(wt.join("work.txt")).unwrap(),
            work_of(round)
        );
        assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
        round += 1;
        changed_look(&mut copies, &l.repo, round).await;
        let keep = copies.keep_commits(&limit()).await;
        if renamed == 4 {
            assert_eq!(keep, Keep::NotDue, "the pack of this head stands");
        } else {
            assert!(
                matches!(keep, Keep::Kept { .. }),
                "cut after {renamed} renames: the head is kept at its next rest: {keep:?}"
            );
        }
        assert_eq!(
            kept_markers(&git_dir)
                .iter()
                .map(|(_, h)| h.clone())
                .collect::<Vec<_>>(),
            vec![second.clone()],
            "cut after {renamed} renames: one kept pack, of the engine's head"
        );
        assert_eq!(names(&folder), files_of(&after));
    }
}

// ---- the copy beside a kept pack is the release's copy ----------------------------------------------------------------------------

/// What the engine's store holds of the run's commits when the comparison starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Store {
    /// Every object of the run is loose.
    Loose,
    /// The engine packed the run's first commits; the last ones are loose.
    Tail,
    /// The engine packed every commit of the run.
    Packed,
}

/// How long git's own check of the pack beside `marker` takes: the mean of ten runs of the command a worker runs when it starts
/// again on a repository that holds a kept pack (`git index-pack --verify` with an object folder that holds nothing), with the
/// git behind the stand-in, which is the second entry of the search path.
fn check_cost(git_dir: &Path, marker: &Path) -> Duration {
    let path = std::env::var("PATH").unwrap_or_default();
    let behind = path.split_once(':').map_or(path.as_str(), |(_, rest)| rest);
    let began = Instant::now();
    for _ in 0..10 {
        let out = std::process::Command::new("git")
            .args(["index-pack", "--verify"])
            .arg(marker.with_extension("pack"))
            .env("PATH", behind)
            .env("GIT_DIR", git_dir)
            .env("GIT_OBJECT_DIRECTORY", git_dir.join("kept/incoming"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git's check of a whole pack: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    began.elapsed() / 10
}

/// Ruling 13 of the lead: on three repositories (the run's commits all loose, a loose tail behind a pack, all packed) the copies
/// made beside a kept pack are the copies the release makes for the same states, compared look by look with a private repository
/// of its own that never held a kept pack: the same ref, tree, parents, message and needed commits, the same files and history in a
/// repository that holds only the bundle the run started from (`git bundle verify`, a fetch, a connectivity check), restored by
/// the product's own restore, and no larger. The hash the look makes of its bundle is the hash of the bytes of that file.
/// Both copies of a comparison are made from commits with fixed dates ([`fix_the_dates`]).
#[tokio::test]
async fn b860_kept_the_copy_is_the_releases_copy() {
    let Some(dir) = second_process("b860_kept_the_copy_is_the_releases_copy") else {
        return;
    };
    fix_the_dates(&dir);
    for store in [Store::Loose, Store::Tail, Store::Packed] {
        let l = lab(&format!("b860-kept-same-{store:?}"));
        engine_commits(&l.repo, 0, 30);
        if store != Store::Loose {
            git(&l.repo, &["gc", "-q"]);
        }
        if store == Store::Tail {
            engine_commits(&l.repo, 30, 10);
        }
        let mut kept = thin(&l, "data-kept").await;
        let mut release = thin(&l, "data-release").await;
        release.set_keep_commits(false);
        let (kept_git, release_git) = (private(&l, "data-kept"), private(&l, "data-release"));
        let mut sizes = Vec::new();
        for round in 1..=4 {
            let mut a = changed_look(&mut kept, &l.repo, round).await;
            let keep = kept.keep_commits(&limit()).await;
            let b = b860_kept_support::look(&mut release).await;
            assert_eq!(
                release.keep_commits(&limit()).await,
                Keep::NotDue,
                "the switch is off"
            );
            assert_eq!(
                kept_markers(&release_git),
                vec![],
                "the release's repository never holds a kept pack"
            );
            match (round, store) {
                (2, Store::Loose | Store::Tail) => assert!(
                    matches!(keep, Keep::Kept { .. }),
                    "TRIGGER PRESENT is not shown ({store:?}): no pack was kept, so the copies compared are two copies of the \
                     release: {keep:?}"
                ),
                (2, Store::Packed) => assert_eq!(
                    keep,
                    Keep::Nothing,
                    "{store:?}: what the engine packed is read from the engine's pack, as in the release"
                ),
                _ => assert_eq!(keep, Keep::NotDue, "{store:?}, look {round}"),
            }
            let what = format!("{store:?}-look-{round}");
            assert_the_releases_copy(&l, &what, (&kept_git, &a), (&release_git, &b));
            let wt = restored(&l, &what, &a.bundle, &a.commit).await;
            assert_eq!(
                std::fs::read_to_string(wt.join("work.txt")).unwrap(),
                work_of(round)
            );
            // The hash the look makes is the hash of the bytes a store would send.
            hash_bundle(&mut a, &limit()).await.unwrap();
            let digest = a.digest.clone().unwrap();
            let (sha, bytes) = hash_of(&a.bundle);
            assert_eq!(
                (digest.sha256_hex, digest.bytes),
                (sha, bytes),
                "{what}: the hash of the bundle"
            );
            sizes.push(format!(
                "look {round}: {} bytes beside the kept pack, {} in the release",
                a.bundle_bytes, b.bundle_bytes
            ));
        }
        println!("{store:?}:\n{}", sizes.join("\n"));
        // What git's own check of the kept pack costs a worker that starts again on this repository. It is printed for the
        // record, and no test asserts a time.
        match kept_markers(&kept_git).first() {
            Some((marker, _)) => println!(
                "{store:?}: git's check of the kept pack of {} bytes takes {:?}",
                kept.kept_bytes(),
                check_cost(&kept_git, marker)
            ),
            None => println!("{store:?}: no pack is kept, so nothing is checked"),
        }
        // What the worker itself measures when it starts again on this repository: the time it writes into its line of the
        // first reading of the tree, with the load of the machine beside it. Printed for the record too.
        drop(kept);
        let began = Instant::now();
        let mut again = thin(&l, "data-kept").await;
        let start = began.elapsed();
        let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let load = load.split_whitespace().next().unwrap_or("?");
        match again.take_kept_check() {
            Some((bytes, took)) => println!(
                "{store:?}: a worker that starts on the kept pack pays {took:?} for git's check of {bytes} bytes ({:.0} ms a \
                 MiB) in a start of {start:?}; load {load}",
                took.as_secs_f64() * 1000.0 / (bytes as f64 / 1_048_576.0)
            ),
            None => println!(
                "{store:?}: a worker that starts without a kept pack checks nothing in a start of {start:?}; load {load}"
            ),
        }
    }
}

/// Ruling 14 of the lead: going back with a kept pack present is its own case. When the keep is switched off for a private
/// repository that holds a kept pack, the pack the worker kept is removed, the looks keep nothing, and their copies are the copies
/// of a repository that never held one. A worker that starts with the switch off on such a repository removes the pack too.
/// Both copies of a comparison are made from commits with fixed dates ([`fix_the_dates`]).
#[tokio::test]
async fn b860_kept_going_back_with_a_kept_pack_present_gives_the_releases_look() {
    let Some(dir) =
        second_process("b860_kept_going_back_with_a_kept_pack_present_gives_the_releases_look")
    else {
        return;
    };
    fix_the_dates(&dir);
    let l = lab("b860-kept-back");
    engine_commits(&l.repo, 0, 20);
    let mut back = thin(&l, "data-back").await;
    let mut release = thin(&l, "data-release").await;
    release.set_keep_commits(false);
    let (back_git, release_git) = (private(&l, "data-back"), private(&l, "data-release"));
    for round in 1..=2 {
        changed_look(&mut back, &l.repo, round).await;
        b860_kept_support::look(&mut release).await;
    }
    let kept = back.keep_commits(&limit()).await;
    assert!(
        matches!(kept, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no kept pack to go back from: {kept:?}"
    );
    assert_eq!(kept_markers(&back_git).len(), 1);

    back.set_keep_commits(false);
    assert_eq!(
        kept_markers(&back_git),
        vec![],
        "the pack the worker kept is removed with the switch"
    );
    assert_eq!(back.kept_bytes(), 0);
    for round in 3..=4 {
        let a = changed_look(&mut back, &l.repo, round).await;
        assert_eq!(
            back.keep_commits(&limit()).await,
            Keep::NotDue,
            "the switch is off"
        );
        let b = b860_kept_support::look(&mut release).await;
        assert_the_releases_copy(
            &l,
            &format!("back-look-{round}"),
            (&back_git, &a),
            (&release_git, &b),
        );
    }
    assert_eq!(kept_markers(&back_git), vec![]);

    // A worker that starts with the switch off finds the pack an earlier worker kept, and removes it.
    {
        let mut earlier = thin(&l, "data-earlier").await;
        changed_look(&mut earlier, &l.repo, 5).await;
        changed_look(&mut earlier, &l.repo, 6).await;
        assert!(matches!(
            earlier.keep_commits(&limit()).await,
            Keep::Kept { .. }
        ));
    }
    let earlier_git = private(&l, "data-earlier");
    assert_eq!(kept_markers(&earlier_git).len(), 1);
    let mut later = thin(&l, "data-earlier").await;
    later.set_keep_commits(false);
    // The copies are numbered within the run: the new worker goes on where the repository it is compared with stands.
    later.count_from(4);
    assert_eq!(kept_markers(&earlier_git), vec![]);
    let a = changed_look(&mut later, &l.repo, 7).await;
    let b = b860_kept_support::look(&mut release).await;
    assert_the_releases_copy(
        &l,
        "back-new-worker",
        (&earlier_git, &a),
        (&release_git, &b),
    );
}

// ---- the comparisons do not follow the clock ---------------------------------------------------------------------------------------

/// The second the clock shows now.
fn second_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Finding 1 of the review of this branch: no comparison of a copy's size with the release's copy follows the clock. A copy's own
/// commit carries the second it was made in unless both of its dates are named, and the bytes of a bundle change with the bytes
/// of that commit: of sixty bundles whose commits differed only by their second, one was a byte larger. Here the two copies of
/// one state are made in two different seconds on purpose, in two private repositories: they are one commit, and they have the
/// same bytes. git has no setting that moves the time of a commit, so the test waits until the clock shows the next second, one
/// second at the most, as `b892_fixture_dates.rs` does.
#[tokio::test]
async fn b860_kept_two_copies_of_one_state_are_one_commit_in_any_second() {
    let Some(dir) =
        second_process("b860_kept_two_copies_of_one_state_are_one_commit_in_any_second")
    else {
        return;
    };
    fix_the_dates(&dir);
    let l = lab("b860-kept-dates");
    engine_commits(&l.repo, 0, 5);
    let mut one = thin(&l, "data-one").await;
    let mut two = thin(&l, "data-two").await;
    two.set_keep_commits(false);
    let (one_git, two_git) = (private(&l, "data-one"), private(&l, "data-two"));
    b860_kept_support::edit(&l.repo, 1);
    let a = b860_kept_support::look(&mut one).await;
    let made = second_now();
    while second_now() <= made {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        second_now() > made,
        "TRIGGER PRESENT is not shown: the clock still shows the second the first copy was made in"
    );
    let b = b860_kept_support::look(&mut two).await;
    assert_eq!(
        commit_text(&one_git, &a.commit),
        commit_text(&two_git, &b.commit),
        "TRIGGER PRESENT: the copy made in a later second is another commit: a comparison of the two copies' sizes follows the \
         clock"
    );
    assert_eq!(a.commit, b.commit);
    assert_eq!(
        a.bundle_bytes, b.bundle_bytes,
        "one commit on one history: the same bytes"
    );
    assert_the_releases_copy(&l, "two-seconds", (&one_git, &a), (&two_git, &b));
}

// ---- the disk --------------------------------------------------------------------------------------------------------------------

/// Root invariant 9 wins over the keep (finding 3 of the review of this branch): a kept pack does not stand on a disk that is
/// short for the copy. The keep's own free-space rule, the mark of the clean-up and twice the last bundle, is asked at every look
/// while a pack is kept, before the bundle is written. Here the disk falls under the mark after the keep: the look that follows
/// removes the pack, makes the copy the release makes for that state, and the copy restores. The stand-in for git writes down
/// what the folder of the kept pack holds when it is asked for the bundle, and that list is empty: a pack that is removed after
/// the bundle was written fails here (ruling 23 (c) of the lead). The head whose pack the disk took is not kept again while it
/// stands (ruling 23 (b)). With room again, a head that moved and came to rest is kept.
#[tokio::test]
async fn b860_kept_a_disk_that_runs_short_after_the_keep_loses_the_pack_and_no_copy() {
    let Some(dir) = second_process(
        "b860_kept_a_disk_that_runs_short_after_the_keep_loses_the_pack_and_no_copy",
    ) else {
        return;
    };
    fix_the_dates(&dir);
    let l = lab("b860-kept-short");
    engine_commits(&l.repo, 0, 20);
    let mut kept = thin(&l, "data-kept").await;
    let mut release = thin(&l, "data-release").await;
    release.set_keep_commits(false);
    let (kept_git, release_git) = (private(&l, "data-kept"), private(&l, "data-release"));
    for round in 1..=2 {
        changed_look(&mut kept, &l.repo, round).await;
        b860_kept_support::look(&mut release).await;
    }
    let keep = kept.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {keep:?}"
    );
    assert_eq!(kept_markers(&kept_git).len(), 1);

    // The disk of the private repositories falls under the mark: both see the same disk.
    let short = TidyMarks {
        free_bytes: u64::MAX,
        ..TidyMarks::RUN
    };
    kept.set_tidy_marks(short);
    release.set_tidy_marks(short);
    let from = kept_seen(&dir, &kept_git).len();
    let a = changed_look(&mut kept, &l.repo, 3).await;
    let bundles: Vec<Seen> = kept_seen(&dir, &kept_git)
        .split_off(from)
        .into_iter()
        .filter(|c| command_of(&c.call) == "bundle")
        .collect();
    assert_eq!(
        bundles.len(),
        1,
        "TRIGGER PRESENT is not shown: the look on the short disk asked git for one bundle: {bundles:?}"
    );
    assert_eq!(
        bundles[0].pack,
        Vec::<String>::new(),
        "TRIGGER PRESENT: the kept pack was on the short disk when git was asked for the bundle"
    );
    let b = b860_kept_support::look(&mut release).await;
    assert_eq!(
        (
            kept_markers(&kept_git),
            names(&kept_folder(&l, "data-kept")),
            kept.kept_bytes()
        ),
        (vec![], Vec::<String>::new(), 0),
        "TRIGGER PRESENT: the kept pack stands on a disk that has less free than the keep's own rule asks"
    );
    assert_the_releases_copy(&l, "short-disk", (&kept_git, &a), (&release_git, &b));
    let wt = restored(&l, "short-disk", &a.bundle, &a.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );

    // The removal leaves the head marked as tried: the same head is not kept again, so no pack is written and removed look after
    // look. (The miss that names the disk is held in `b860_kept_a_keep_starts_only_with_room_for_its_pack`.)
    assert_eq!(
        kept.keep_commits(&limit()).await,
        Keep::NotDue,
        "the head whose pack the short disk took is not kept again"
    );
    let a = changed_look(&mut kept, &l.repo, 4).await;
    let b = b860_kept_support::look(&mut release).await;
    assert_eq!(
        kept.keep_commits(&limit()).await,
        Keep::NotDue,
        "one try for this head"
    );
    assert_eq!(names(&kept_folder(&l, "data-kept")), Vec::<String>::new());
    assert_the_releases_copy(&l, "short-disk-next", (&kept_git, &a), (&release_git, &b));

    // With room again, a head that moved and came to rest is kept.
    kept.set_tidy_marks(TidyMarks::RUN);
    release.set_tidy_marks(TidyMarks::RUN);
    let head = engine_commits(&l.repo, 20, 1);
    for round in 5..=6 {
        let a = changed_look(&mut kept, &l.repo, round).await;
        let b = b860_kept_support::look(&mut release).await;
        assert_the_releases_copy(
            &l,
            &format!("room-again-{round}"),
            (&kept_git, &a),
            (&release_git, &b),
        );
    }
    let keep = kept.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "with room again: {keep:?}"
    );
    assert_eq!(
        kept_markers(&kept_git)
            .iter()
            .map(|(_, h)| h.clone())
            .collect::<Vec<_>>(),
        vec![head]
    );
}

/// The git calls agentd made in the private repository `git_dir` from call `from` on that found the folder of the kept pack, or the
/// folder a keep writes into, holding a file when they started.
fn calls_beside_a_pack(dir: &Path, git_dir: &Path, from: usize) -> (Vec<Seen>, Vec<Seen>) {
    let calls = kept_seen(dir, git_dir).split_off(from);
    let beside = calls
        .iter()
        .filter(|c| !c.pack.is_empty() || !c.incoming.is_empty())
        .cloned()
        .collect();
    (calls, beside)
}

/// A file in the folder a keep writes into, as the git of a keep that was cut makes it after the guard's sweep.
fn left_by_a_cut_keep(l: &Lab, data: &str) {
    let folder = incoming_folder(l, data);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("tmp_pack_left"),
        "what the git of a cut keep wrote after the sweep\n",
    )
    .unwrap();
}

/// Ruling 23 (a) of the lead, the major finding of the second review (its try T6): root invariant 9 wins over the keep before a look
/// writes anything. The short-disk question is the first thing a look does. Here a pack is kept, the disk falls under the keep's
/// rule, and three looks fail in the step that reads the tree, as they do on a full disk, where `git add` has no room for the blob
/// it writes. Every git call of those looks, the clean-up's first one included, finds the folder of the kept pack empty: the bytes
/// of the pack are free before the tree is read. A file that the git of a cut keep made after the guard's sweep is gone as early
/// (ruling 23 (f)). The look that can read the tree again stores a copy that restores, and the head whose pack the disk took is
/// not kept again.
#[tokio::test]
async fn b860_kept_a_short_disk_takes_the_pack_before_a_look_reads_the_tree() {
    let Some(dir) =
        second_process("b860_kept_a_short_disk_takes_the_pack_before_a_look_reads_the_tree")
    else {
        return;
    };
    let l = lab("b860-kept-short-look");
    engine_commits(&l.repo, 0, 5);
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {keep:?}"
    );
    left_by_a_cut_keep(&l, "data");

    copies.set_tidy_marks(short_disk());
    std::fs::write(dir.join("add-fail"), "").unwrap();
    edit(&l.repo, 3);
    for n in 1..=3 {
        let from = kept_seen(&dir, &git_dir).len();
        let made = copies.snapshot_until(&limit()).await;
        let (calls, beside) = calls_beside_a_pack(&dir, &git_dir, from);
        assert!(
            made.is_err() && calls.iter().any(|c| command_of(&c.call) == "add"),
            "TRIGGER PRESENT is not shown: look {n} did not fail in the step that reads the tree: {:?}; {calls:?}",
            made.as_ref().map(Option::is_some)
        );
        assert!(
            beside.is_empty(),
            "TRIGGER PRESENT: look {n} on a short disk started git while the kept pack, or what a cut keep left, was on the \
             disk: {beside:?}"
        );
        assert_eq!(
            (
                copies.kept_bytes(),
                names(&kept_folder(&l, "data")),
                names(&incoming_folder(&l, "data"))
            ),
            (0, Vec::<String>::new(), Vec::<String>::new()),
            "after look {n}"
        );
    }

    std::fs::remove_file(dir.join("add-fail")).unwrap();
    let made = b860_kept_support::look(&mut copies).await;
    let wt = restored(&l, "after-the-failed-looks", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "the head whose pack the short disk took is not kept again"
    );
}

/// The copy of the end on a short disk, with the session's final head (`with_head`) or without one: a pack is kept, the disk falls
/// under the keep's rule, and the copy of the end is made. Every git call of that copy finds the folder of the kept pack and the
/// folder a keep writes into empty, and the copy restores.
async fn the_copy_of_the_end_on_a_short_disk(dir: &Path, with_head: bool) {
    let l = lab(if with_head {
        "b860-kept-short-end-head"
    } else {
        "b860-kept-short-end-tree"
    });
    engine_commits(&l.repo, 0, 5);
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {keep:?}"
    );
    left_by_a_cut_keep(&l, "data");

    copies.set_tidy_marks(short_disk());
    edit(&l.repo, 3);
    let head = with_head.then(|| {
        git(&l.repo, &["add", "-A"]);
        git(&l.repo, &["commit", "-q", "-m", "the work of the session"]);
        git(&l.repo, &["rev-parse", "HEAD"])
    });
    let from = kept_seen(dir, &git_dir).len();
    let end = copies.final_copy(head.as_deref()).await.unwrap();
    let (calls, beside) = calls_beside_a_pack(dir, &git_dir, from);
    assert!(
        end.final_copy && calls.iter().any(|c| command_of(&c.call) == "bundle"),
        "TRIGGER PRESENT is not shown: the copy of the end made no bundle: {calls:?}"
    );
    assert!(
        beside.is_empty(),
        "TRIGGER PRESENT: the copy of the end on a short disk started git while the kept pack, or what a cut keep left, was on \
         the disk: {beside:?}"
    );
    assert_eq!(
        (
            copies.kept_bytes(),
            names(&kept_folder(&l, "data")),
            names(&incoming_folder(&l, "data"))
        ),
        (0, Vec::<String>::new(), Vec::<String>::new())
    );
    if let Some(head) = &head {
        assert_eq!(&end.commit, head);
    }
    let wt = restored(&l, "the-end", &end.bundle, &end.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
}

/// Ruling 23 (a) of the lead: the short-disk question is also the first thing the copy of the end does. With the session's final
/// head, the kept pack is gone before the first git call of that copy ([`the_copy_of_the_end_on_a_short_disk`]).
#[tokio::test]
async fn b860_kept_a_short_disk_takes_the_pack_before_the_copy_of_the_end_with_a_head() {
    let Some(dir) = second_process(
        "b860_kept_a_short_disk_takes_the_pack_before_the_copy_of_the_end_with_a_head",
    ) else {
        return;
    };
    the_copy_of_the_end_on_a_short_disk(&dir, true).await;
}

/// Ruling 23 (a) of the lead: the same for the copy of the end without a head, which is the tree as it is
/// ([`the_copy_of_the_end_on_a_short_disk`]).
#[tokio::test]
async fn b860_kept_a_short_disk_takes_the_pack_before_the_copy_of_the_end_without_a_head() {
    let Some(dir) = second_process(
        "b860_kept_a_short_disk_takes_the_pack_before_the_copy_of_the_end_without_a_head",
    ) else {
        return;
    };
    the_copy_of_the_end_on_a_short_disk(&dir, false).await;
}

/// Ruling 23 (b) of the lead, the second review's try T5 as it ran: the marks are set by hand to what a disk shows that has room
/// before each keep and is short after it, and the engine's head never moves. The removal on the short disk leaves the head
/// marked as tried, so six looks at that head cost one keep and one removal, where they cost six of each: the keep never costs
/// more work than the release's look in a loop. A head that moved and came to rest is kept when the disk has room.
#[tokio::test]
async fn b860_kept_a_pack_the_short_disk_took_is_not_kept_again_for_the_same_head() {
    let Some(dir) =
        second_process("b860_kept_a_pack_the_short_disk_took_is_not_kept_again_for_the_same_head")
    else {
        return;
    };
    let l = lab("b860-kept-once");
    engine_commits(&l.repo, 1, 5);
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let (mut keeps, mut removals) = (0, 0);
    for round in 3..9u32 {
        copies.set_tidy_marks(TidyMarks::RUN);
        if matches!(copies.keep_commits(&limit()).await, Keep::Kept { .. }) {
            keeps += 1;
        }
        copies.set_tidy_marks(short_disk());
        let before = copies.kept_bytes();
        let made = changed_look(&mut copies, &l.repo, round).await;
        if before > 0 && copies.kept_bytes() == 0 {
            removals += 1;
        }
        let wt = restored(&l, &format!("once-{round}"), &made.bundle, &made.commit).await;
        assert_eq!(
            std::fs::read_to_string(wt.join("work.txt")).unwrap(),
            work_of(round)
        );
    }
    assert_eq!(
        (keeps, removals, keeps_started(&dir)),
        (1, 1, 1),
        "TRIGGER PRESENT: six looks at one head of the engine, on a disk that is short after each keep: the keeps, the removals \
         and the times the keep's git was started"
    );

    copies.set_tidy_marks(TidyMarks::RUN);
    let head = engine_commits(&l.repo, 6, 1);
    changed_look(&mut copies, &l.repo, 9).await;
    changed_look(&mut copies, &l.repo, 10).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "a head that moved and came to rest, with room: {keep:?}"
    );
    assert_eq!(
        kept_markers(&git_dir)
            .iter()
            .map(|(_, h)| h.clone())
            .collect::<Vec<_>>(),
        vec![head]
    );
}

/// Ruling 23 (b) of the lead: a keep starts only when the disk has room for the keep's rule and for the pack it is about to write,
/// which is about as large as the last bundle. The free bytes are named by the test, as a disk shows them: what the pack holds is
/// taken off. With room between the rule and the rule plus the pack, six looks at one head start no keep at all: the one try for
/// the head is a miss that names the disk. With room for both, a head that moved and came to rest is kept once, and no look
/// removes the pack. A look clears what a cut keep left in the folder a keep writes into, also on a disk with room.
#[tokio::test]
async fn b860_kept_a_keep_starts_only_with_room_for_its_pack() {
    let Some(dir) = second_process("b860_kept_a_keep_starts_only_with_room_for_its_pack") else {
        return;
    };
    let l = lab("b860-kept-room");
    engine_commits(&l.repo, 0, 5);
    let mut copies = thin(&l, "data").await;
    changed_look(&mut copies, &l.repo, 1).await;
    let made = changed_look(&mut copies, &l.repo, 2).await;
    let rule = TidyMarks::RUN.free_bytes + 2 * made.bundle_bytes;
    let disk = rule + made.bundle_bytes / 8;
    let mut misses = Vec::new();
    for round in 3..9u32 {
        copies.set_keep_free_bytes(Some(disk - copies.kept_bytes()));
        if let Keep::Missed(why) = copies.keep_commits(&limit()).await {
            misses.push(why);
        }
        copies.set_keep_free_bytes(Some(disk - copies.kept_bytes()));
        changed_look(&mut copies, &l.repo, round).await;
    }
    assert_eq!(
        (keeps_started(&dir), names(&kept_folder(&l, "data"))),
        (0, Vec::<String>::new()),
        "TRIGGER PRESENT: the keep's git was started on a disk that has room for the keep's rule and none for the pack"
    );
    assert!(
        misses.len() == 1 && misses[0].contains("free"),
        "one try for the head, a miss that names the disk: {misses:?}"
    );

    engine_commits(&l.repo, 5, 1);
    changed_look(&mut copies, &l.repo, 9).await;
    let made = changed_look(&mut copies, &l.repo, 10).await;
    let disk = TidyMarks::RUN.free_bytes + 4 * made.bundle_bytes;
    copies.set_keep_free_bytes(Some(disk));
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "with room for the rule and for the pack: {keep:?}"
    );
    let files = names(&kept_folder(&l, "data"));
    for round in 11..17u32 {
        left_by_a_cut_keep(&l, "data");
        copies.set_keep_free_bytes(Some(disk - copies.kept_bytes()));
        changed_look(&mut copies, &l.repo, round).await;
        assert_eq!(
            (
                names(&kept_folder(&l, "data")),
                names(&incoming_folder(&l, "data"))
            ),
            (files.clone(), Vec::<String>::new()),
            "look {round}: the pack stands, and what a cut keep left is gone"
        );
        assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    }
    assert_eq!(keeps_started(&dir), 1, "one keep and no removal");
}

/// Ruling 23 (e) of the lead: git's check of a kept pack at a worker's start that does not end. The stand-in for git holds the
/// check without end; the worker gives it up at its limit and ends its git. The pack is removed, and a mark in the private
/// repository switches the keep off for the rest of the run: the worker says so once, no later look keeps again, on the same head
/// or on a head that moved, and a worker that starts on that repository afterwards keeps none either and runs no check. A pack
/// that cannot be checked within the limit is never kept twice. The copies restore as before.
#[tokio::test]
async fn b860_kept_a_check_that_does_not_end_is_given_up_and_the_run_keeps_no_pack_again() {
    let Some(dir) = second_process(
        "b860_kept_a_check_that_does_not_end_is_given_up_and_the_run_keeps_no_pack_again",
    ) else {
        return;
    };
    let l = lab("b860-kept-check-held");
    engine_commits(&l.repo, 0, 5);
    let git_dir = private(&l, "data");
    {
        let mut earlier = thin(&l, "data").await;
        changed_look(&mut earlier, &l.repo, 1).await;
        changed_look(&mut earlier, &l.repo, 2).await;
        let keep = earlier.keep_commits(&limit()).await;
        assert!(
            matches!(keep, Keep::Kept { .. }),
            "TRIGGER PRESENT is not shown: no pack was kept: {keep:?}"
        );
    }
    let checks = |dir: &Path| {
        kept_seen(dir, &git_dir)
            .iter()
            .filter(|c| command_of(&c.call) == "index-pack")
            .count()
    };
    assert_eq!(checks(&dir), 0);

    std::fs::write(dir.join("check-hold"), "").unwrap();
    let within = Duration::from_secs(3);
    let began = Instant::now();
    let mut later = thin_within(&l, "data", within).await;
    let took = began.elapsed();
    later.count_from(2);
    assert!(
        dir.join("check-running").exists() && took >= within,
        "TRIGGER PRESENT is not shown: git's check of the kept pack was not held to the worker's limit: the start took {took:?}"
    );
    let pid: u32 = std::fs::read_to_string(dir.join("check-running"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let ended = Instant::now();
    while alive(pid) && ended.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid), "the git of a check that was given up is ended");
    std::fs::remove_file(dir.join("check-hold")).unwrap();
    assert_eq!(
        (
            kept_markers(&git_dir),
            names(&kept_folder(&l, "data")),
            later.kept_bytes()
        ),
        (vec![], Vec::<String>::new(), 0),
        "TRIGGER PRESENT: the pack whose check was given up still stands"
    );
    let said = later.take_kept_note();
    assert!(
        said.as_deref()
            .is_some_and(|why| why.contains("did not end") && why.contains("no pack is kept")),
        "TRIGGER PRESENT: the worker does not say that the check did not end and that the run keeps no pack again: {said:?}"
    );
    assert_eq!(later.take_kept_note(), None, "it is said once");
    assert!(
        git_dir.join("kept/off").is_file(),
        "the mark in the private repository"
    );

    // No later look keeps again: the same head twice, then a head that moved and came to rest.
    changed_look(&mut later, &l.repo, 3).await;
    let made = changed_look(&mut later, &l.repo, 4).await;
    assert!(!later.keep_is_due(), "the keep is off for this run");
    assert_eq!(later.keep_commits(&limit()).await, Keep::NotDue);
    let wt = restored(&l, "after-the-check", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(4)
    );
    engine_commits(&l.repo, 5, 1);
    changed_look(&mut later, &l.repo, 5).await;
    changed_look(&mut later, &l.repo, 6).await;
    assert_eq!(later.keep_commits(&limit()).await, Keep::NotDue);
    drop(later);

    // A worker that starts on this repository afterwards: the mark stands, nothing is checked, and no pack is kept.
    let mut again = thin(&l, "data").await;
    again.count_from(6);
    assert_eq!(again.take_kept_note(), None);
    changed_look(&mut again, &l.repo, 7).await;
    changed_look(&mut again, &l.repo, 8).await;
    assert_eq!(again.keep_commits(&limit()).await, Keep::NotDue);
    assert_eq!(
        (
            keeps_started(&dir),
            checks(&dir),
            names(&kept_folder(&l, "data"))
        ),
        (1, 1, Vec::<String>::new()),
        "the keep's git ran once in the run, and git's check once"
    );
}

// ---- ruling 25 of the lead: a worker's start, the engine's packs, the question before the bundle, the mark -------------------------

/// A worker that kept a pack of five loose commits of the engine and has ended: the private repository under `data` holds the pack.
/// The answer is the engine's head.
async fn left_with_a_kept_pack(l: &Lab, data: &str) -> String {
    let head = engine_commits(&l.repo, 1, 5);
    let mut copies = thin(l, data).await;
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {keep:?}"
    );
    head
}

/// Ruling 25 (c) of the lead, finding 3 of the third review: the question before the bundle, the third place of ruling 23 (a). A
/// look asks the disk when it starts, writes the files that changed into the private repository, and asks once more before its
/// bundle; only that question sees a disk that ran short during the look. Here the disk has room when the look starts and is
/// short when the look asks again: the git calls that read the tree find the kept pack, and the call that writes the bundle finds
/// its folder empty. The copy restores, and the head whose pack the disk took is not kept again.
#[tokio::test]
async fn b860_kept_a_disk_that_runs_short_during_a_look_loses_the_pack_before_the_bundle() {
    let Some(dir) = second_process(
        "b860_kept_a_disk_that_runs_short_during_a_look_loses_the_pack_before_the_bundle",
    ) else {
        return;
    };
    let l = lab("b860-kept-short-bundle");
    engine_commits(&l.repo, 1, 5);
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {keep:?}"
    );

    copies.set_keep_free_answers(vec![u64::MAX, 0]);
    edit(&l.repo, 3);
    let from = kept_seen(&dir, &git_dir).len();
    let made = b860_kept_support::look(&mut copies).await;
    let calls = kept_seen(&dir, &git_dir).split_off(from);
    let of = |command: &str| -> Vec<&Seen> {
        calls
            .iter()
            .filter(|c| command_of(&c.call) == command)
            .collect()
    };
    assert!(
        !of("add").is_empty() && of("add").iter().all(|c| !c.pack.is_empty()),
        "TRIGGER PRESENT is not shown: the kept pack did not stand while the look read the tree: {calls:?}"
    );
    assert!(
        !of("bundle").is_empty() && of("bundle").iter().all(|c| c.pack.is_empty()),
        "TRIGGER PRESENT: the disk ran short during the look, and the kept pack stood when the bundle was asked for: {:?}",
        of("bundle")
    );
    assert_eq!(
        (copies.kept_bytes(), names(&kept_folder(&l, "data"))),
        (0, Vec::<String>::new())
    );
    let wt = restored(&l, "short-during-the-look", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    copies.set_keep_free_bytes(None);
    changed_look(&mut copies, &l.repo, 4).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "the head whose pack the short disk took is not kept again"
    );
}

/// Ruling 25 (c) of the lead, finding 3 of the third review: a worker that starts on a private repository that holds the mark of a
/// run that keeps no pack any more while a pack still stands beside the mark. A worker does not make that state itself: it
/// removes the pack before it writes the mark. The start removes the pack, runs no check of it and says nothing, and no later
/// look keeps again. The copies restore.
#[tokio::test]
async fn b860_kept_a_worker_that_starts_on_the_mark_leaves_no_pack_beside_it() {
    let Some(dir) =
        second_process("b860_kept_a_worker_that_starts_on_the_mark_leaves_no_pack_beside_it")
    else {
        return;
    };
    let l = lab("b860-kept-mark-and-pack");
    left_with_a_kept_pack(&l, "data").await;
    let git_dir = private(&l, "data");
    std::fs::write(
        git_dir.join("kept/off"),
        "the mark of a run that keeps no pack any more\n",
    )
    .unwrap();
    assert!(
        !names(&kept_folder(&l, "data")).is_empty(),
        "TRIGGER PRESENT is not shown: no pack stands beside the mark"
    );
    let from = kept_seen(&dir, &git_dir).len();

    let mut copies = thin(&l, "data").await;
    copies.count_from(2);
    assert_eq!(
        (
            kept_markers(&git_dir),
            names(&kept_folder(&l, "data")),
            copies.kept_bytes()
        ),
        (vec![], Vec::<String>::new(), 0),
        "TRIGGER PRESENT: a worker that started on the mark left the pack beside it"
    );
    let calls = kept_seen(&dir, &git_dir).split_off(from);
    assert!(
        !calls.iter().any(|c| command_of(&c.call) == "index-pack"),
        "git's check ran for a pack beside the mark"
    );
    assert_eq!(copies.take_kept_note(), None);
    changed_look(&mut copies, &l.repo, 3).await;
    let made = changed_look(&mut copies, &l.repo, 4).await;
    assert!(!copies.keep_is_due(), "the keep is off for this run");
    assert_eq!(copies.keep_commits(&limit()).await, Keep::NotDue);
    assert_eq!(keeps_started(&dir), 1);
    let wt = restored(&l, "on-the-mark", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(4)
    );
}

/// The bytes of every file under `dir`.
fn bytes_under(dir: &Path) -> u64 {
    let mut sum = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.is_dir() => sum += bytes_under(&path),
                Ok(m) => sum += m.len(),
                Err(_) => {}
            }
        }
    }
    sum
}

/// Ruling 25 (a) of the lead, finding 1 of the third review: root invariant 9 wins over the keep at a worker's start
/// too. A worker kept a pack and ended. The tree then changed by 3 MiB that no compression makes smaller, so the first reading of
/// the worker that starts next writes them into the private repository. That worker starts on a disk that is short of the keep's
/// rule. The question to the disk is the first thing the start does: `git init`, the first write of a start, and every git call of
/// the start in the private repository find the folder of the kept pack empty, so the start writes nothing while a pack stands
/// (the try read 3,146,906 bytes). No check is paid for a pack that goes: git's check does not run. The first copy restores, and
/// the head whose pack the disk took is not kept again while it stands, also with room.
#[tokio::test]
async fn b860_kept_a_short_disk_takes_the_pack_before_a_worker_that_starts_writes() {
    let Some(dir) =
        second_process("b860_kept_a_short_disk_takes_the_pack_before_a_worker_that_starts_writes")
    else {
        return;
    };
    let l = lab("b860-kept-short-start");
    left_with_a_kept_pack(&l, "data").await;
    let git_dir = private(&l, "data");
    let big = noise(7, 3 << 20);
    std::fs::write(l.repo.join("big.bin"), &big).unwrap();
    let objects = git_dir.join("objects");
    let (before, from, inits) = (
        bytes_under(&objects),
        kept_seen(&dir, &git_dir).len(),
        init_seen(&dir, &git_dir).len(),
    );
    assert!(
        !names(&kept_folder(&l, "data")).is_empty(),
        "TRIGGER PRESENT is not shown: no kept pack stands when the worker starts"
    );

    let mut copies = thin_on(&l, "data", TIDY_FREE_BYTES - 1).await;
    copies.count_from(2);
    let wrote = bytes_under(&objects) - before;
    let (calls, beside) = calls_beside_a_pack(&dir, &git_dir, from);
    let first = init_seen(&dir, &git_dir).split_off(inits);
    assert!(
        wrote >= 3 << 20 && calls.iter().any(|c| command_of(&c.call) == "add"),
        "TRIGGER PRESENT is not shown: the first reading of the start did not write the changed file: {wrote} bytes"
    );
    assert!(
        beside.is_empty(),
        "TRIGGER PRESENT: a worker that started on a short disk wrote {wrote} bytes into the private repository while the kept \
         pack stood; its git calls beside the pack: {:?}",
        beside
            .iter()
            .map(|c| command_of(&c.call))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        first,
        vec![Vec::<String>::new()],
        "TRIGGER PRESENT: `git init`, the first write of a start, found the kept pack on a short disk"
    );
    assert!(
        !calls.iter().any(|c| command_of(&c.call) == "index-pack")
            && copies.take_kept_check().is_none()
            && copies.take_kept_note().is_none(),
        "a check was paid, or a line said, for a pack that goes"
    );
    assert_eq!(
        (copies.kept_bytes(), names(&kept_folder(&l, "data"))),
        (0, Vec::<String>::new())
    );
    println!(
        "B860 START on a short disk beside a kept pack: the start wrote {wrote} bytes into the private repository, 0 of them \
         while the pack stood"
    );

    copies.set_keep_free_bytes(None);
    changed_look(&mut copies, &l.repo, 3).await;
    let made = changed_look(&mut copies, &l.repo, 4).await;
    let wt = restored(&l, "after-the-short-start", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(4)
    );
    assert!(std::fs::read(wt.join("big.bin")).unwrap() == big);
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "the head whose pack the short disk took at the start is not kept again"
    );
}

/// One more pack in the engine's store that holds one blob no commit names: what a fetch or a tool of the engine can leave.
fn one_more_engine_pack(l: &Lab, n: u32) {
    use std::io::Write;
    let file = l.dir.join(format!("other-{n}.bin"));
    std::fs::write(&file, noise(u64::from(n) + 77, 4_096)).unwrap();
    let id = git(&l.repo, &["hash-object", "-w", file.to_str().unwrap()]);
    let mut child = std::process::Command::new("git")
        .current_dir(&l.repo)
        .args(["pack-objects", "-q", ".git/objects/pack/pack"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", id.trim()).as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// Ruling 25 (b) of the lead, finding 2 of the third review: no keep-and-remove cycle when the engine's packs change.
/// The engine's head rests on five loose commits, a pack is kept, and one more pack appears in the engine's store before each of
/// six looks. The first of them removes the kept pack, because the engine's packs are no longer those its marker names, and the
/// head stays marked as tried: with the first keep, the six looks cost one keep and one removal, where they cost seven and six,
/// and the keep's git is started once. The keep never costs more work than the release's look in a loop. Every copy restores. A
/// head that moved and came to rest is kept again.
#[tokio::test]
async fn b860_kept_a_pack_the_engines_packs_overtook_is_not_kept_again_for_the_same_head() {
    let Some(dir) = second_process(
        "b860_kept_a_pack_the_engines_packs_overtook_is_not_kept_again_for_the_same_head",
    ) else {
        return;
    };
    let l = lab("b860-kept-packs-once");
    engine_commits(&l.repo, 1, 5);
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let first = copies.keep_commits(&limit()).await;
    assert!(
        matches!(first, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {first:?}"
    );
    let (mut keeps, mut removals) = (1, 0);
    for round in 3..9u32 {
        one_more_engine_pack(&l, round);
        let before = copies.kept_bytes();
        let made = changed_look(&mut copies, &l.repo, round).await;
        if before > 0 && copies.kept_bytes() == 0 {
            removals += 1;
        }
        if matches!(copies.keep_commits(&limit()).await, Keep::Kept { .. }) {
            keeps += 1;
        }
        let wt = restored(&l, &format!("packs-{round}"), &made.bundle, &made.commit).await;
        assert_eq!(
            std::fs::read_to_string(wt.join("work.txt")).unwrap(),
            work_of(round)
        );
    }
    assert_eq!(
        (keeps, removals, keeps_started(&dir)),
        (1, 1, 1),
        "TRIGGER PRESENT: six looks at one head of the engine, one more pack in the engine's store before each: the keeps, the \
         removals and the times the keep's git was started"
    );

    let head = engine_commits(&l.repo, 6, 1);
    changed_look(&mut copies, &l.repo, 9).await;
    changed_look(&mut copies, &l.repo, 10).await;
    let keep = copies.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "a head that moved and came to rest: {keep:?}"
    );
    assert_eq!(
        kept_markers(&git_dir)
            .iter()
            .map(|(_, h)| h.clone())
            .collect::<Vec<_>>(),
        vec![head]
    );
}

// ---- ruling 27 of the lead: the mark over several workers, the clearing of the mark, the rule in one place -------------------------

/// Ruling 27 (a) of the lead, finding 1 of the fourth review (its sequence R3): the mark of a head that was tried holds for one
/// worker. It is in the worker's memory: a worker that starts takes it from the pack that stands, and a worker that starts on a
/// private repository without a pack has none. The engine's head rests on five loose commits over five workers of one run,
/// five looks each. In the first three the engine's packs change once, before the third look (R3): each keeps the head once and
/// removes that pack once. The fourth sees no change of the engine's packs: one keep, no removal, and its pack stands when it
/// ends. The fifth starts on that pack and keeps nothing; the engine's packs change before its third look, and it removes the
/// pack once. A head that rests costs each worker one keep and one removal at the most, the keep's git is started once for each
/// worker that started on no pack, and every copy restores.
#[tokio::test]
async fn b860_kept_a_head_at_rest_costs_each_worker_one_keep_and_one_removal_at_the_most() {
    let Some(dir) = second_process(
        "b860_kept_a_head_at_rest_costs_each_worker_one_keep_and_one_removal_at_the_most",
    ) else {
        return;
    };
    let l = lab("b860-kept-workers");
    engine_commits(&l.repo, 1, 5);
    let mut round = 0u32;
    let mut workers = Vec::new();
    for worker in 1..=5u32 {
        let mut copies = thin(&l, "data").await;
        copies.count_from(round);
        let on_a_pack = copies.kept_bytes() > 0;
        let (mut keeps, mut removals) = (0u32, 0u32);
        for step in 1..=5u32 {
            if step == 3 && worker != 4 {
                one_more_engine_pack(&l, worker * 10 + step);
            }
            round += 1;
            let before = copies.kept_bytes();
            let made = changed_look(&mut copies, &l.repo, round).await;
            if before > 0 && copies.kept_bytes() == 0 {
                removals += 1;
            }
            if matches!(copies.keep_commits(&limit()).await, Keep::Kept { .. }) {
                keeps += 1;
            }
            let wt = restored(&l, &format!("workers-{round}"), &made.bundle, &made.commit).await;
            assert_eq!(
                std::fs::read_to_string(wt.join("work.txt")).unwrap(),
                work_of(round)
            );
        }
        workers.push((on_a_pack, keeps, removals));
    }
    println!(
        "B860 WORKERS one head at rest over five workers, {round} looks, every copy restored: (started on a pack, keeps, \
         removals) of each worker: {workers:?}; the keep's git was started {} times",
        keeps_started(&dir)
    );
    assert_eq!(
        workers,
        vec![
            (false, 1, 1),
            (false, 1, 1),
            (false, 1, 1),
            (false, 1, 0),
            (true, 0, 1)
        ],
        "TRIGGER PRESENT: one head of the engine at rest over five workers: whether each started on a kept pack, its keeps and \
         its removals"
    );
    assert_eq!(
        keeps_started(&dir),
        4,
        "the keep's git is started once for each worker that started on no pack"
    );
}

/// Ruling 27 (b) of the lead, finding 2 of the fourth review: the clearing of the mark. A pack is kept at a head. The engine then
/// goes back to the commit the run started from: nothing of the run is above the floors, the next look removes the pack, and the
/// mark of the head that was tried is cleared with it. The engine returns to the same commit and rests there for two looks: the
/// head is kept again. With the mark left in place that head would not be kept again by this worker until it moved elsewhere.
/// Every copy on the way restores.
#[tokio::test]
async fn b860_kept_a_head_that_went_back_to_a_floor_and_returned_is_kept_again() {
    let l = lab("b860-kept-floor-and-back");
    let head = engine_commits(&l.repo, 1, 5);
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let first = copies.keep_commits(&limit()).await;
    assert!(
        matches!(first, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {first:?}"
    );

    // The engine goes back to the commit the run started from: nothing of the run is above the floors any more.
    git(&l.repo, &["reset", "-q", "--hard", &l.base]);
    let made = changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(
        (copies.kept_bytes(), kept_markers(&git_dir)),
        (0, vec![]),
        "TRIGGER PRESENT is not shown: the look at a head that is back at a floor left the kept pack"
    );
    let wt = restored(&l, "back-at-the-floor", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a head at a floor is followed by no keep"
    );

    // The engine returns to the commit the pack was kept at, and rests there.
    git(&l.repo, &["reset", "-q", "--hard", &head]);
    changed_look(&mut copies, &l.repo, 4).await;
    let made = changed_look(&mut copies, &l.repo, 5).await;
    let again = copies.keep_commits(&limit()).await;
    assert!(
        matches!(again, Keep::Kept { .. }),
        "TRIGGER PRESENT: the head went back to a floor, returned to the commit it was kept at and rested for two looks, and \
         is not kept again: {again:?}"
    );
    assert_eq!(
        kept_markers(&git_dir)
            .iter()
            .map(|(_, h)| h.clone())
            .collect::<Vec<_>>(),
        vec![head]
    );
    let wt = restored(&l, "returned-from-the-floor", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(5)
    );
}

/// Whether the keep's free-space rule reads a disk as short at each place that asks it, on a disk that has `under` bytes less
/// free than the rule asks for at that place: a worker that starts on a kept pack (the rule is `TIDY_FREE_BYTES`: the last
/// bundle is 0 at a start), a keep before it starts (`TIDY_FREE_BYTES`, twice the last bundle, and the size of the last bundle
/// more for the pack), and a look beside a kept pack (`TIDY_FREE_BYTES` and twice the last bundle). Short is: the start removed
/// the pack and git checked nothing; the keep is a miss that names the disk; the look removed the pack. Not short is: the pack
/// stays, git checked it and the worker adopted it; a pack is kept; the pack stays. Each place has a repository of its own, so
/// the answer of one place does not follow from another's.
async fn short_at_each_place(dir: &Path, label: &str, under: u64) -> [bool; 3] {
    // A worker that starts on a kept pack.
    let l = lab(&format!("{label}-start"));
    left_with_a_kept_pack(&l, "data").await;
    let git_dir = private(&l, "data");
    let files = names(&kept_folder(&l, "data"));
    assert!(
        !files.is_empty(),
        "TRIGGER PRESENT is not shown: no kept pack stands when the worker starts"
    );
    let from = kept_seen(dir, &git_dir).len();
    let mut started = thin_on(&l, "data", TIDY_FREE_BYTES - under).await;
    let checked = kept_seen(dir, &git_dir)
        .split_off(from)
        .iter()
        .any(|c| command_of(&c.call) == "index-pack");
    let left = names(&kept_folder(&l, "data"));
    let (bytes, check) = (started.kept_bytes(), started.take_kept_check());
    let start = left.is_empty();
    assert!(
        if start {
            bytes == 0 && !checked && check.is_none()
        } else {
            left == files && bytes > 0 && checked && check.is_some()
        },
        "a worker's start {under} bytes under the rule: the folder of the kept pack holds {left:?}, the worker holds {bytes} \
         bytes of it, git's check ran: {checked}, and the worker measured it: {check:?}"
    );
    drop(started);

    // A keep before it starts.
    let l = lab(&format!("{label}-keep"));
    engine_commits(&l.repo, 1, 5);
    let mut copies = thin(&l, "data").await;
    changed_look(&mut copies, &l.repo, 1).await;
    let made = changed_look(&mut copies, &l.repo, 2).await;
    copies.set_keep_free_bytes(Some(TIDY_FREE_BYTES + 3 * made.bundle_bytes - under));
    let keep = match copies.keep_commits(&limit()).await {
        Keep::Missed(why) if why.contains("free") => true,
        Keep::Kept { .. } => false,
        other => panic!("a keep {under} bytes under the rule and its pack: {other:?}"),
    };
    drop(copies);

    // A look beside a kept pack.
    let l = lab(&format!("{label}-look"));
    engine_commits(&l.repo, 1, 5);
    let mut copies = thin(&l, "data").await;
    changed_look(&mut copies, &l.repo, 1).await;
    let made = changed_look(&mut copies, &l.repo, 2).await;
    let kept = copies.keep_commits(&limit()).await;
    assert!(
        matches!(kept, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {kept:?}"
    );
    let files = names(&kept_folder(&l, "data"));
    copies.set_keep_free_bytes(Some(TIDY_FREE_BYTES + 2 * made.bundle_bytes - under));
    let made = changed_look(&mut copies, &l.repo, 3).await;
    let left = names(&kept_folder(&l, "data"));
    let look = left.is_empty();
    assert!(
        if look {
            copies.kept_bytes() == 0
        } else {
            left == files && copies.kept_bytes() > 0
        },
        "a look {under} bytes under the rule: the folder of the kept pack holds {left:?}, the worker holds {} bytes of it",
        copies.kept_bytes()
    );
    let wt = restored(&l, "at-the-boundary", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    [start, keep, look]
}

/// Ruling 27 (c) of the lead, finding 3 of the fourth review: one rule, one place. The bytes of the keep's free-space rule and
/// its comparison are one function (`Snapshotter::keep_rule_short`) that a worker's start, a keep and a look all ask, and the
/// boundary is pinned to the byte at each of them. This side: a disk with exactly the rule's bytes free is short at no place.
/// The pack an earlier worker kept stays at a start and git checks it, a keep is started, and a look leaves the kept pack.
#[tokio::test]
async fn b860_kept_a_disk_with_exactly_the_rules_bytes_free_is_short_at_no_place() {
    let Some(dir) =
        second_process("b860_kept_a_disk_with_exactly_the_rules_bytes_free_is_short_at_no_place")
    else {
        return;
    };
    assert_eq!(
        short_at_each_place(&dir, "b860-kept-at-the-rule", 0).await,
        [false, false, false],
        "TRIGGER PRESENT: a disk with exactly the rule's bytes free was read as short: at a worker's start, by a keep before \
         it starts, by a look beside a kept pack"
    );
}

/// Ruling 27 (c) of the lead, the other side of the boundary: a disk with one byte less free than the rule asks for is short at
/// every place. The pack an earlier worker kept is gone at a start and git checks nothing, a keep is a miss that names the
/// disk, and a look removes the kept pack. The copy of that look restores.
#[tokio::test]
async fn b860_kept_a_disk_one_byte_under_the_rule_is_short_at_every_place() {
    let Some(dir) =
        second_process("b860_kept_a_disk_one_byte_under_the_rule_is_short_at_every_place")
    else {
        return;
    };
    assert_eq!(
        short_at_each_place(&dir, "b860-kept-under-the-rule", 1).await,
        [true, true, true],
        "TRIGGER PRESENT: a disk with one byte less free than the rule asks for was not read as short: at a worker's start, \
         by a keep before it starts, by a look beside a kept pack"
    );
}

// ---- the readers of a copy ---------------------------------------------------------------------------------------------------------

/// Every reader restores a copy that was made beside a kept pack, after the run committed 50 times: the restore of a later session
/// (the product's own restore, the base first), the copy of the end, which builds no pack and reads none, and the read of a
/// stopped session's storage, which has a private repository of its own in another place.
#[tokio::test]
async fn b860_kept_every_reader_restores_a_copy_made_beside_a_kept_pack() {
    let l = lab("b860-kept-readers");
    let mut copies = thin(&l, "data").await;
    let git_dir = private(&l, "data");
    engine_commits(&l.repo, 0, 50);
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;
    let kept = copies.keep_commits(&limit()).await;
    assert!(
        matches!(kept, Keep::Kept { .. }),
        "TRIGGER PRESENT: no kept pack: {kept:?}"
    );
    let markers = kept_markers(&git_dir);
    let files = names(&kept_folder(&l, "data"));

    // A later session's restore of a copy made beside the pack.
    let made = changed_look(&mut copies, &l.repo, 3).await;
    assert_eq!(
        needed(&made.bundle),
        vec![l.base.clone()],
        "the copy is above the run's base"
    );
    let wt = restored(&l, "later-session", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    assert!(
        wt.join("run/file0049.txt").is_file(),
        "the run's last commit is in the copy"
    );

    // The read of a stopped session's storage: the tree and the engine's history as the storage holds them.
    let read = l.dir.join("read-back");
    std::fs::create_dir_all(&read).unwrap();
    let mut reader = Snapshotter::for_read_back(&read, &l.repo, &l.base)
        .await
        .unwrap();
    let back = reader
        .read_back(&l.base, 1, &GiveUp::at(None))
        .await
        .unwrap();
    let wt = restored(&l, "read-back", &back.bundle, &back.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    assert_eq!(
        kept_markers(&read.join("snapshot.git")),
        vec![],
        "a read keeps no pack"
    );

    // The copy of the end at the session's final head: it builds no pack, and it restores.
    git(&l.repo, &["add", "-A"]);
    git(&l.repo, &["commit", "-q", "-m", "the work of the session"]);
    let head = git(&l.repo, &["rev-parse", "HEAD"]);
    let end = copies.final_copy(Some(&head)).await.unwrap();
    assert!(end.final_copy);
    assert_eq!(end.commit, head);
    assert_eq!(needed(&end.bundle), vec![l.base.clone()]);
    let wt = restored(&l, "the-end", &end.bundle, &end.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );
    assert_eq!(
        kept_markers(&git_dir),
        markers,
        "the copy of the end keeps nothing and removes nothing"
    );
    assert_eq!(names(&kept_folder(&l, "data")), files);
    assert_eq!(names(&incoming_folder(&l, "data")), Vec::<String>::new());
}

// ---- a storage that answers a keep slowly ------------------------------------------------------------------------------------------

/// A file call of a keep on the session's storage that has not returned when the keep is cut fails no look. A keep reads the names
/// of the engine's packs, a folder on the storage of the work tree. When the storage answers slowly and the keep is cut meanwhile
/// (a tool call ended, agentd was asked to end), the look that follows at once must make its copy: the copy of a stop must not be
/// lost to a read that a keep left behind. Once the storage answers again, a keep is made.
#[tokio::test(flavor = "multi_thread")]
async fn b860_kept_a_read_of_the_keep_that_has_not_returned_fails_no_look() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let l = lab("b860-kept-slow-read");
    let mut copies = thin(&l, "data").await;
    engine_commits(&l.repo, 0, 10);
    changed_look(&mut copies, &l.repo, 1).await;
    changed_look(&mut copies, &l.repo, 2).await;

    // The names of the engine's packs come only when the test lets them.
    let answers = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let (gate, count) = (answers.clone(), asked.clone());
    copies.set_engine_packs_reader(Arc::new(move |_: &Path| {
        count.fetch_add(1, Ordering::SeqCst);
        while !gate.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok("the packs of the test".to_string())
    }));

    // The keep asks for the names and is cut while the answer is outstanding.
    let far = limit();
    {
        let keep = copies.keep_commits(&far);
        tokio::pin!(keep);
        let began = std::time::Instant::now();
        loop {
            tokio::select! {
                out = &mut keep => panic!("TRIGGER PRESENT is not shown: the keep ended while its read was held: {out:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {
                    if asked.load(Ordering::SeqCst) >= 1 {
                        break;
                    }
                    assert!(began.elapsed() < Duration::from_secs(30), "the keep never asked for the engine's packs");
                }
            }
        }
    }
    copies.keep_cut(false);

    // The look that follows at once makes its copy while that read is still outstanding.
    b860_kept_support::edit(&l.repo, 3);
    let made = copies
        .snapshot_until(&limit())
        .await
        .unwrap_or_else(|e| {
            panic!("TRIGGER PRESENT: the look after a keep whose read of the storage had not returned failed: {e}")
        })
        .expect("the tree changed, so the look makes a copy");
    let wt = restored(&l, "slow-read", &made.bundle, &made.commit).await;
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        work_of(3)
    );

    // A keep that would read the packs while that read is still outstanding is a miss at once: one read is outstanding at most.
    let again = copies.keep_commits(&limit()).await;
    assert!(
        matches!(&again, Keep::Missed(why) if why.contains("has not returned")),
        "a second read of the packs is refused at once: {again:?}"
    );
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "no second read was started"
    );

    // The storage answers, then stops again. The head moves and comes to rest, and this keep is given up while it waits.
    answers.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    answers.store(false, Ordering::SeqCst);
    engine_commits(&l.repo, 10, 1);
    changed_look(&mut copies, &l.repo, 4).await;
    assert_eq!(
        copies.keep_commits(&limit()).await,
        Keep::NotDue,
        "a moved head"
    );
    changed_look(&mut copies, &l.repo, 5).await;
    let short = GiveUp::at(Some(
        tokio::time::Instant::now() + Duration::from_millis(300),
    ));
    let given_up = copies.keep_commits(&short).await;
    assert!(
        matches!(&given_up, Keep::Missed(why) if why.contains("did not answer")),
        "a keep that is given up while the storage is silent is a miss: {given_up:?}"
    );
    let made = changed_look(&mut copies, &l.repo, 6).await;
    restored(&l, "slow-read-given-up", &made.bundle, &made.commit).await;

    // The storage answers again: a head that moved and came to rest keeps its pack.
    answers.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    engine_commits(&l.repo, 11, 1);
    changed_look(&mut copies, &l.repo, 7).await;
    changed_look(&mut copies, &l.repo, 8).await;
    let kept = copies.keep_commits(&limit()).await;
    assert!(
        matches!(kept, Keep::Kept { .. }),
        "the keep once the storage answers: {kept:?}"
    );

    // A read that ends in a panic is a miss, and it is not outstanding afterwards.
    copies.set_engine_packs_reader(Arc::new(|_: &Path| -> Result<String, String> {
        panic!("the reader of the test panics")
    }));
    engine_commits(&l.repo, 12, 1);
    changed_look(&mut copies, &l.repo, 9).await;
    changed_look(&mut copies, &l.repo, 10).await;
    let panicked = copies.keep_commits(&limit()).await;
    assert!(matches!(panicked, Keep::Missed(_)), "{panicked:?}");
    copies.set_engine_packs_reader(Arc::new(|_: &Path| Ok("the packs of the test".to_string())));
    engine_commits(&l.repo, 13, 1);
    changed_look(&mut copies, &l.repo, 11).await;
    changed_look(&mut copies, &l.repo, 12).await;
    assert!(matches!(
        copies.keep_commits(&limit()).await,
        Keep::Kept { .. }
    ));
}

// ---- the engine packs its own commits ----------------------------------------------------------------------------------------------

/// The copy does not grow when the engine packs its own commits under a kept pack. The run rewrites one line of a long text file in
/// each of 20 commits. Loose, each version of the file is whole, and the kept pack holds them whole. Once the engine packs its
/// commits (`git gc`), its pack holds all but one version as a small difference, and the release's copy shrinks with it. A bundle
/// that read the kept pack before the engine's pack would take the whole versions: the copy would be many times the release's, and
/// a copy above 8 MiB starts no copy after a tool call any more. Which of two packs git reads first differs between git versions:
/// one reads the newer file first, another the object folder that was named first. So the look that finds other packs in the
/// engine's store than the keep found reads no kept pack and removes it. Here the storage's clock is also an hour behind the
/// worker's, so that a git that reads the newer pack first would read the kept pack.
/// A head that then moves and comes to rest keeps a pack of what is in no pack: the new commit alone.
/// Both copies of a comparison are made from commits with fixed dates ([`fix_the_dates`]).
#[tokio::test]
async fn b860_kept_an_engine_repack_under_a_kept_pack_does_not_grow_the_copy() {
    let Some(dir) =
        second_process("b860_kept_an_engine_repack_under_a_kept_pack_does_not_grow_the_copy")
    else {
        return;
    };
    fix_the_dates(&dir);
    let l = lab("b860-kept-repack");
    std::fs::create_dir_all(l.repo.join("run")).unwrap();
    for round in 0..20u32 {
        let text: String = (0..4_000u32)
            .map(|n| {
                if n == round * 100 {
                    format!("line {n} of the long file, changed in round {round}\n")
                } else {
                    format!("line {n} of the long file\n")
                }
            })
            .collect();
        std::fs::write(l.repo.join("run/long.txt"), text).unwrap();
        git(&l.repo, &["add", "run"]);
        git(&l.repo, &["commit", "-q", "-m", &format!("round {round}")]);
    }
    let mut kept = thin(&l, "data-kept").await;
    let mut release = thin(&l, "data-release").await;
    release.set_keep_commits(false);
    let (kept_git, release_git) = (private(&l, "data-kept"), private(&l, "data-release"));
    let mut loose_bytes = 0;
    for round in 1..=2 {
        changed_look(&mut kept, &l.repo, round).await;
        loose_bytes = b860_kept_support::look(&mut release).await.bundle_bytes;
    }
    let keep = kept.keep_commits(&limit()).await;
    assert!(
        matches!(keep, Keep::Kept { .. }),
        "TRIGGER PRESENT is not shown: no pack was kept: {keep:?}"
    );

    // The engine packs its commits, and its storage says that the new pack is an hour old.
    git(&l.repo, &["gc", "-q"]);
    let engine_packs = l.repo.join(".git/objects/pack");
    for name in names(&engine_packs) {
        // The files of a pack cannot be written to; their owner may set their time.
        std::fs::File::open(engine_packs.join(&name))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3_600))
            .unwrap();
    }
    // The next look of each repository copies the same tree.
    b860_kept_support::edit(&l.repo, 3);
    let b = b860_kept_support::look(&mut release).await;
    let a = b860_kept_support::look(&mut kept).await;
    println!(
        "the release's copy: {loose_bytes} bytes while the run's commits were loose, {} after the engine packed them; beside the \
         kept pack: {}",
        b.bundle_bytes, a.bundle_bytes
    );
    assert!(
        b.bundle_bytes * 2 < loose_bytes,
        "TRIGGER PRESENT is not shown: the engine's pack did not make the release's copy smaller: {} bytes after it, {loose_bytes} \
         before",
        b.bundle_bytes
    );
    assert_eq!(
        a.commit, b.commit,
        "TRIGGER PRESENT: the two copies that are compared are not one commit: their dates follow the clock"
    );
    assert!(
        a.bundle_bytes <= b.bundle_bytes,
        "TRIGGER PRESENT: after the engine packed its commits the copy beside the kept pack has {} bytes and the release's copy {}",
        a.bundle_bytes,
        b.bundle_bytes
    );
    assert_eq!(
        kept_markers(&kept_git),
        vec![],
        "TRIGGER PRESENT: the look that found the engine's new pack left the kept pack that this pack overtook"
    );
    assert_the_releases_copy(
        &l,
        "after-the-engine-packed",
        (&kept_git, &a),
        (&release_git, &b),
    );

    // The head moves and comes to rest: the new keep holds what is in no pack, the one new commit, and replaces the pack before.
    let head = engine_commits(&l.repo, 100, 1);
    for round in 4..=5 {
        let a = changed_look(&mut kept, &l.repo, round).await;
        let b = b860_kept_support::look(&mut release).await;
        assert_the_releases_copy(
            &l,
            &format!("after-the-engine-packed-{round}"),
            (&kept_git, &a),
            (&release_git, &b),
        );
    }
    assert!(matches!(
        kept.keep_commits(&limit()).await,
        Keep::Kept { .. }
    ));
    let markers = kept_markers(&kept_git);
    assert_eq!(
        markers.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>(),
        vec![head]
    );
    let held = objects_of(&l.repo, &markers[0].0);
    assert_eq!(
        held.iter().filter(|(_, kind)| kind == "commit").count(),
        1,
        "the pack holds the one commit that the engine has in no pack: {held:?}"
    );
}
